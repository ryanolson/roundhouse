// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Turning a Messages SSE body into [`FrontierChunk`]s.
//!
//! Split from [`super`] for the same reason the Responses decoder is: that file
//! is about authentication and the request, this one is about a byte stream that
//! arrives in arbitrary pieces and has to become a sequence of durable deltas.
//! The structure deliberately mirrors `openai_responses::stream` — same buffer
//! bound, same `feed`/`eof`/`drain`, same lossy UTF-8, same queue of pending
//! chunks — so that the *differences* below are the only things a reader has to
//! hold in mind.
//!
//! **Dispatch is on the SSE `event:` line here, and on the payload `type` in the
//! Responses decoder, and both are right.** Anthropic names the event on the
//! `event:` line and documents it as the dispatch key; its own client dispatches
//! there and silently drops frames that lack one
//! (`research/claude-code-client-surface.md` §3.2). The payload's `type` is
//! meant to agree, so it is cross-checked when present and a disagreement fails
//! the stream: one of the two is then lying about what frame this is, and
//! picking either would be guessing with the turn's accounting. A frame with no
//! `event:` line at all falls back to the payload `type` rather than being
//! dropped — a generic SSE re-encoder in the path can strip the line, and unlike
//! Anthropic's client, this decoder loses nothing by being tolerant there.
//!
//! **The usage inversion is the single most dangerous line in this file.** On
//! the Anthropic wire `input_tokens` *excludes* cache reads and cache writes:
//! the three counters are disjoint. Roundhouse's [`Usage`] is OpenAI-shaped,
//! where `input_tokens` is the total and the cached count is a component of it.
//! Folding them without the conversion understates every cached turn's input by
//! exactly the amount that was cached — that is, it understates most by the
//! amount the whole product exists to maximize, and it understates it *silently*
//! and in the direction that reads as a saving.
//!
//! **A stream that dies before `message_stop` yields no `Done`.** Same rule as
//! the Responses decoder, and the same reason: the engine substitutes
//! `estimated_usage` and marks it, whereas a synthesized zero-token `Done` folds
//! as zero tokens for zero dollars — indistinguishable from a saving, which is
//! the one failure the metrics chapter is built against. A stream that *reaches*
//! `message_stop` having reported only one of the two halves of its bill is the
//! same case wearing a terminal frame, and is answered the same way — see
//! [`SseDecoder::emit_done`].
//!
//! **Tool blocks are the one thing this decoder assembles across frames**, and
//! since M11.2 the block lifecycle is therefore its business rather than an
//! accumulator's. A `tool_use` block opens on `content_block_start` carrying its
//! id and name, its arguments arrive as `input_json_delta` fragments that are
//! not JSON on their own, and the call is only knowable at that block's
//! `content_block_stop`. Four consequences, each a rule below: the index is the
//! discriminator, because a turn may interleave text and several tool blocks and
//! nothing else distinguishes their fragments; a block that never closes emits
//! nothing, the same discipline as the missing `Done` above and for the same
//! reason — half a set of arguments is not a smaller tool call, it is one no
//! consumer can parse; a block that *does* close over fragments that still do
//! not reassemble into JSON is answered the same way (F7, M11.2a review) — a
//! `content_block_stop` proves the provider stopped sending, not that what it
//! sent parses, and garbage is not a smaller call either, so nothing downstream
//! of this file (the log commit, both serve projections) ever holds a
//! `FrontierChunk::ToolCall` whose arguments do not parse; and the two lifecycle
//! frames are read *leniently*, so a malformed one loses the call it described
//! rather than failing a turn that had already been served.
//!
//! [`Usage`]: roundhouse_core::event::Usage

use std::collections::{BTreeMap, VecDeque};

use serde_json::Value;

use roundhouse_core::event::CacheReadSource;

use crate::frontier::{FrontierChunk, FrontierError};

use super::wire::{ApiError, BlockDelta, ContentBlock, Message, StreamEvent, Usage};

/// How much of a single SSE event this will buffer before giving up.
///
/// Same bound and same argument as the Responses decoder: the body is a remote
/// party's, and an upstream that never sends the blank line separating events
/// would otherwise grow this buffer until the process died. A `message_start`
/// payload is a few hundred bytes and the largest text delta is far below this.
const MAX_EVENT_BYTES: usize = 1 << 20;

/// How many tool blocks may be open at once before the stream is abandoned.
///
/// **The same argument as [`MAX_EVENT_BYTES`], applied to the one thing this
/// decoder accumulates *across* events.** That bound holds a single frame; a
/// tool block spans many, so an upstream (or something pretending to be one)
/// that opened a `content_block_start` per frame with a fresh index and never
/// closed any would grow this map until the process died — with every
/// individual frame comfortably legal.
///
/// Generous against real traffic: parallel tool use puts a handful of blocks in
/// flight, and the largest turn anyone has captured opens three. A limit no real
/// stream approaches is what makes crossing it evidence of a broken upstream
/// rather than of an ambitious turn.
const MAX_OPEN_TOOL_BLOCKS: usize = 64;

/// How much argument JSON one tool block may accumulate before the stream is
/// abandoned.
///
/// The other half of the bound above: a single open block whose
/// `input_json_delta` fragments never stop. Each fragment is small and legal, so
/// nothing else in this file catches it. One megabyte is far past any real tool
/// call — the largest argument a coding agent sends is a file edit, measured in
/// tens of kilobytes — and it is deliberately the same order as
/// [`MAX_EVENT_BYTES`] so the two limits read as one policy.
const MAX_TOOL_ARGUMENT_BYTES: usize = 1 << 20;

/// Input-side counts as they accumulate, in *Anthropic's* axes.
///
/// Held rather than emitted, because this dialect reports the input side on
/// `message_start` and the output side on the final `message_delta`, and
/// roundhouse's log takes one accounting record per call. Kept in the provider's
/// own disjoint axes and converted once, at the point the `Done` is built, so
/// there is exactly one place the conversion can be wrong.
#[derive(Debug, Default, Clone, Copy)]
struct InputSide {
    /// Tokens neither read from nor written to the cache.
    fresh: u64,
    read: u64,
    /// Whether any frame actually named the cache read, so a defaulted zero is
    /// not published as a cold prefix.
    read_source: CacheReadSource,
    written: u64,
}

impl InputSide {
    /// The prompt tokens this call was billed for, in roundhouse's axes.
    fn total(&self) -> u64 {
        self.fresh
            .saturating_add(self.read)
            .saturating_add(self.written)
    }

    /// Fold one `usage` object into the running input side.
    ///
    /// Each component is updated only when the frame reports a non-zero value,
    /// which is exactly the merge Anthropic's own client performs
    /// (`research/claude-code-client-surface.md` §3.4: a greater-than-zero guard
    /// on input and cache counts). The reason is that `message_delta` may repeat
    /// the object with only the output count filled in, and a naive overwrite
    /// would then retract everything `message_start` reported.
    fn fold(&mut self, usage: &Usage) {
        if usage.input_tokens > 0 {
            self.fresh = usage.input_tokens;
        }
        // The cache read carries provenance as well as a count: a frame that
        // names the field has spoken for it, a stated zero included. The `> 0`
        // condition is still here for the merge above's reason — a later frame
        // must not retract a read an earlier one reported — so an explicit zero
        // only lands while nothing positive has.
        if let Some(read) = usage.cache_read_input_tokens
            && (read > 0 || self.read == 0)
        {
            self.read = read;
            self.read_source = CacheReadSource::Provider;
        }
        if usage.cache_creation_input_tokens > 0 {
            self.written = usage.cache_creation_input_tokens;
        }
    }
}

/// A `tool_use` block that has opened and not yet closed.
///
/// Held rather than emitted, for the reason [`InputSide`] is held: the facts
/// arrive on three different frames and the consumer needs one value. The id and
/// the name come from `content_block_start`, the arguments from every
/// `input_json_delta` between there and `content_block_stop`, and only the stop
/// frame proves the arguments are complete.
#[derive(Debug, Clone)]
struct ToolBlock {
    id: String,
    name: String,
    /// The block's `input` as `content_block_start` carried it.
    ///
    /// `{}` on every streamed tool block the API documents — the object is
    /// filled in by the fragments that follow. Kept anyway, because an upstream
    /// or a proxy that sends a tool block whole (no fragments, a populated
    /// `input`) is a shape this decoder can read for free and would otherwise
    /// turn into a call with no arguments.
    seed: Value,
    /// Every `input_json_delta.partial_json` for this block, concatenated.
    ///
    /// A `String` and not a `Value`, because a fragment is not JSON: the first
    /// one is routinely `{"pat` and parsing it alone fails. Concatenation is the
    /// whole reconstruction, and it is also what keeps the bytes the ones the
    /// provider chose — see `FrontierChunk::ToolCall::arguments`.
    fragments: String,
}

impl ToolBlock {
    /// The completed call this block describes, or `None` if the reassembled
    /// arguments do not parse as JSON.
    ///
    /// The `{}` fallback is the wire's own answer and not an invention: a tool
    /// that takes no arguments streams a `content_block_start` with `input: {}`
    /// and no fragments at all, so the empty object is what the provider said.
    /// The alternative — an empty `arguments` string — is not valid JSON, and
    /// every consumer downstream would have to special-case it.
    ///
    /// **The parse gate is F7 (M11.2a thermo-nuclear review).** Before it, a
    /// block that closed over an unparseable concatenation — a connection that
    /// died one fragment early, still followed by a `content_block_stop` —
    /// reached `FrontierChunk::ToolCall` anyway, and the two serve projections
    /// each covered for it differently: `canonical_arguments` (`item.rs`) passes
    /// a non-JSON string through unchanged, so the log committed it; streaming
    /// forwarded those exact bytes as the block's one `input_json_delta`
    /// fragment and Claude Code's own accumulator threw closing it, while
    /// non-streaming silently substituted `{}` for the same call
    /// (`messages_api.rs`'s `BlockAccumulator::close`) — one stored item, one
    /// throw and one lie. This is the one seam that can tell "final and
    /// well-formed" apart from "final and garbage": downstream of this function
    /// neither serve path has any memory of whether the arguments it is holding
    /// came from a stream that actually finished. Dropping the call here —
    /// rather than downstream, and rather than repairing it — is this file's
    /// existing answer to a block whose fragments describe less than a full
    /// call, extended from "never closed" to "closed over the wrong bytes":
    /// nothing, rather than something no consumer can check.
    fn into_chunk(self) -> Option<FrontierChunk> {
        let arguments = match (self.fragments.is_empty(), self.seed) {
            (false, _) => self.fragments,
            // `input` defaults to `Value::Null` when the frame omitted it
            // entirely, which is "nobody said" rather than "no arguments" —
            // and on a block that also carried no fragments the two have the
            // same answer, because there is nothing else this call could take.
            (true, Value::Null) => "{}".to_string(),
            (true, seed) => seed.to_string(),
        };
        // The two `seed`-only arms above always produce valid JSON (`{}`, or a
        // `Value`'s own `to_string()`), so only the fragment-concatenation arm
        // can fail this — but gating uniformly, rather than only on that arm,
        // is what keeps this the one check every path through `arguments` has
        // to clear, present tense or future.
        if serde_json::from_str::<Value>(&arguments).is_err() {
            return None;
        }
        Some(FrontierChunk::ToolCall {
            id: self.id,
            name: self.name,
            // No such field on this wire, and no name to split. Anthropic's
            // `tool_use` block carries one flat `name`, and a client that
            // registered an MCP server folded the registration into it — so the
            // flat spelling *is* the namespace here, and manufacturing a `Some`
            // by splitting on `__` would invent a server boundary the provider
            // never drew (M17, R-N6).
            namespace: None,
            arguments,
        })
    }
}

/// Assembles SSE events out of arbitrary byte runs and decodes the ones that
/// carry output or accounting.
#[derive(Default)]
pub(super) struct SseDecoder {
    /// Bytes received and not yet consumed by a complete event.
    buffer: String,
    /// Decoded chunks waiting to be yielded, in arrival order.
    pending: VecDeque<FrontierChunk>,
    /// Set by `message_stop`. Nothing after it is read.
    finished: bool,
    /// Whether a `message_start` reported any input at all.
    ///
    /// The gate on emitting a `Done`. An upstream (or a proxy) that opened a
    /// stream without the prelude has told us nothing about what the prompt
    /// cost, and "nothing" must not be written down as "zero".
    saw_input: bool,
    /// Whether any `message_delta` reported an output count.
    ///
    /// The output-side twin of [`Self::saw_input`], and it exists for the same
    /// reason: `output_tokens` below is a `u64` that starts at Rust's zero, so
    /// without this flag "no frame ever said" and "the provider said zero" are
    /// the same value — and the first is an unaccounted turn while the second is
    /// a free one. On this dialect the two frames carry different halves of the
    /// bill, so each half needs its own answer to "did anyone actually say?".
    saw_output: bool,
    input: InputSide,
    /// Cumulative output count, from the last `message_delta` that reported one.
    output_tokens: u64,
    /// The last `stop_reason` any `message_delta` reported, verbatim.
    ///
    /// **Set only from a frame that named one**, which is the same
    /// non-retracting rule the counts above follow and it exists for the same
    /// failure: the wire sends an explicit `"stop_reason": null` on every
    /// non-final delta, so a plain assignment would let the last frame before
    /// `message_stop` erase what the frame that actually ended the turn said.
    ///
    /// A `String` and not a [`StopReason`](super::wire::StopReason), because
    /// what leaves this decoder is what the wire said — see
    /// `FrontierChunk::Done::stop_reason`. The typed parse still happens on
    /// the way in, so an eighth value arrives as `Other` and is carried rather
    /// than failing the frame.
    stop_reason: Option<String>,
    /// Tool blocks that have opened and not yet closed, by block index.
    ///
    /// **Keyed on the index because the index is the only discriminator.** A
    /// turn that says "I'll grep for that" and then calls two tools interleaves
    /// a text block and two `tool_use` blocks, and every `input_json_delta`
    /// names only its index — nothing in a fragment says which tool it belongs
    /// to. A decoder that kept one "current" block would splice the second
    /// call's arguments onto the first, producing one tool call with unparseable
    /// arguments and losing the other entirely.
    ///
    /// A `BTreeMap` rather than a `Vec` indexed by position: the indices are the
    /// provider's, this decoder does not get to assume they start at zero or
    /// arrive in order, and a sparse map cannot be made to allocate by a
    /// `content_block_start` claiming index 4 000 000 000.
    tool_blocks: BTreeMap<u64, ToolBlock>,
}

impl SseDecoder {
    /// Take the next decoded chunk, if one is ready.
    pub(super) fn next_chunk(&mut self) -> Option<FrontierChunk> {
        self.pending.pop_front()
    }

    /// Whether the terminal frame has been seen.
    pub(super) fn finished(&self) -> bool {
        self.finished
    }

    /// Add a run of bytes and decode whatever events it completed.
    ///
    /// Lossy UTF-8 for the same reason as the Responses decoder: a chunk
    /// boundary lands mid-codepoint routinely, and failing there would fail
    /// turns at random on any prompt containing a non-ASCII character.
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Result<(), FrontierError> {
        if self.finished {
            return Ok(());
        }
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        self.drain()
    }

    /// The body ended. Decode a final event that arrived without its blank line.
    pub(super) fn eof(&mut self) -> Result<(), FrontierError> {
        if self.finished {
            return Ok(());
        }
        let tail = std::mem::take(&mut self.buffer);
        self.decode_event(&tail)
    }

    fn drain(&mut self) -> Result<(), FrontierError> {
        while let Some((ends, next)) = event_boundary(&self.buffer) {
            let event = self.buffer[..ends].to_string();
            self.buffer.drain(..next);
            self.decode_event(&event)?;
            if self.finished {
                self.buffer.clear();
                return Ok(());
            }
        }
        if self.buffer.len() > MAX_EVENT_BYTES {
            return Err(FrontierError::Upstream(format!(
                "the upstream sent {} bytes with no event boundary, past the \
                 {MAX_EVENT_BYTES}-byte limit; abandoning the stream rather \
                 than buffering it",
                self.buffer.len()
            )));
        }
        Ok(())
    }

    /// Decode one event block: the `event:` name, every `data:` line joined.
    fn decode_event(&mut self, event: &str) -> Result<(), FrontierError> {
        let mut name: Option<&str> = None;
        let mut data = String::new();
        for line in lines(event) {
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start());
            } else if let Some(rest) = line.strip_prefix("event:") {
                name = Some(rest.trim());
            }
            // Comments (`: keep-alive`) and every other SSE field — `id:`,
            // `retry:` — are skipped rather than refused. A chained Relay
            // re-encoder drops `id:` lines entirely, so anything this decoder
            // read from one would work in a direct topology and vanish in a
            // chained one.
        }
        let data = data.trim();
        if data.is_empty() {
            // A frame with no payload carries nothing to account for, whatever
            // its name says. Notably that includes a bare `event: message_stop`
            // with no `data:` line: refusing to end the turn on it costs an
            // estimated usage record, while ending on it would mean emitting a
            // `Done` built from counts nobody confirmed.
            return Ok(());
        }
        let payload: Value = serde_json::from_str(data).map_err(|source| {
            FrontierError::Upstream(format!("the upstream sent an unparseable event: {source}"))
        })?;
        self.dispatch(name, payload)
    }

    fn dispatch(&mut self, name: Option<&str>, payload: Value) -> Result<(), FrontierError> {
        // Owned rather than borrowed out of `payload`, because the payload is
        // moved into the typed parse a few lines down and the cross-check has to
        // outlive it. One small allocation on a frame this client reads.
        let declared = payload
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);
        // The cross-check. The API's own contract is that the two agree
        // ("Each event uses an SSE event name … and includes the matching event
        // `type` in its data"), so a disagreement is not novelty to be tolerated
        // — it is two different claims about which frame this is, and this
        // decoder would have to guess which one the accounting belongs to.
        if let (Some(name), Some(declared)) = (name, declared.as_deref())
            && name != declared
        {
            return Err(FrontierError::Upstream(format!(
                "the upstream framed an event as `{name}` and typed its payload \
                 `{declared}`; refusing to guess which one the turn's accounting \
                 belongs to"
            )));
        }
        let Some(name) = name.or(declared.as_deref()) else {
            // Neither the frame nor the payload says what this is. Skipped, not
            // refused: an anonymous frame is what a keepalive from a rewriting
            // proxy looks like.
            return Ok(());
        };

        match name {
            "message_start" => {
                let StreamEvent::MessageStart { message, .. } = self.parse(name, payload)? else {
                    unreachable!("parsed as the name it was dispatched on")
                };
                self.fold_message_start(&message);
                Ok(())
            }
            // **Read leniently, and the asymmetry with `message_start` above is
            // deliberate.** A `message_start` this build cannot read means the
            // turn's input count is gone, which is an accounting hole worth
            // failing for. A `content_block_start` this build cannot read means
            // one tool call is lost — bad, but the turn was still served and its
            // bill is still knowable, and failing it would convert a frame we
            // merely did not understand into an unanswered request. So a parse
            // failure here drops the frame, which for a `tool_use` block means
            // no block opens, which by the rule below means nothing is emitted:
            // the same "nothing rather than something fabricated" answer this
            // decoder gives everywhere else.
            //
            // It also preserves a property the suite pins: novelty in a frame
            // this client does not otherwise read must not be able to fail a
            // deployment that already works.
            "content_block_start" => {
                let Ok(StreamEvent::ContentBlockStart {
                    index,
                    content_block,
                    ..
                }) = self.parse(name, payload)
                else {
                    return Ok(());
                };
                // Only `tool_use` is tracked. A text block needs no state — its
                // deltas are emitted as they arrive — and a thinking block is
                // consumed, so opening a record for either would be bookkeeping
                // nothing reads.
                // `name: tool` rather than binding `name`: the frame's own name
                // is in scope here, and one identifier meaning both the event
                // type and the tool being called is a line a reader has to
                // re-check.
                if let ContentBlock::ToolUse {
                    id,
                    name: tool,
                    input,
                    ..
                } = content_block
                {
                    // Checked before the insert and against blocks that are
                    // *open*, so a turn that legitimately calls a hundred tools
                    // one after another is unaffected — each closes before the
                    // next opens. What this refuses is a hundred at once and
                    // none of them ever closing.
                    if self.tool_blocks.len() >= MAX_OPEN_TOOL_BLOCKS
                        && !self.tool_blocks.contains_key(&index)
                    {
                        return Err(FrontierError::Upstream(format!(
                            "the upstream opened more than {MAX_OPEN_TOOL_BLOCKS} tool \
                             blocks without closing any; abandoning the stream rather \
                             than buffering them"
                        )));
                    }
                    // An index reopened before its stop replaces the block
                    // rather than merging with it: two starts on one index are
                    // the upstream contradicting itself, and appending the
                    // second call's fragments to the first would produce one
                    // call with arguments belonging to neither.
                    self.tool_blocks.insert(
                        index,
                        ToolBlock {
                            id,
                            name: tool,
                            seed: input,
                            fragments: String::new(),
                        },
                    );
                }
                Ok(())
            }
            // The frame that proves a tool call is complete, and the only place
            // one is emitted. Before it, the arguments are a prefix of a JSON
            // document; after it, they are the document.
            "content_block_stop" => {
                let Ok(StreamEvent::ContentBlockStop { index, .. }) = self.parse(name, payload)
                else {
                    return Ok(());
                };
                // F7: `into_chunk` answers `None` for a block whose reassembled
                // arguments do not parse, and that answer is "drop the call",
                // not "queue nothing and fail the turn" — the turn was already
                // served and every other frame in it still stands.
                if let Some(block) = self.tool_blocks.remove(&index)
                    && let Some(chunk) = block.into_chunk()
                {
                    self.pending.push_back(chunk);
                }
                Ok(())
            }
            "content_block_delta" => {
                let StreamEvent::ContentBlockDelta { index, delta, .. } =
                    self.parse(name, payload)?
                else {
                    unreachable!("parsed as the name it was dispatched on")
                };
                self.fold_delta(index, delta)
            }
            "message_delta" => {
                let StreamEvent::MessageDelta { delta, usage, .. } = self.parse(name, payload)?
                else {
                    unreachable!("parsed as the name it was dispatched on")
                };
                // **The reporting half of M11.1's F1.** Until M11.2 this arm
                // destructured `{ usage, .. }` and the stop reason went into the
                // `..`, which meant a turn cut off at the dispatch ceiling
                // (`max_tokens`) decoded to byte-identical chunks as one that
                // ended on its own — and `tool_use`, the signal that the turn is
                // waiting on the client rather than finished, could not be
                // spoken at all. Assigned only when the frame named one: the
                // wire sends an explicit `null` on every non-final delta, and a
                // plain assignment would let one of those erase the real answer.
                if let Some(reason) = delta.stop_reason {
                    self.stop_reason = Some(reason.as_wire().to_string());
                }
                if let Some(usage) = usage {
                    self.fold_message_delta(&usage);
                }
                Ok(())
            }
            "message_stop" => {
                self.finished = true;
                // Tool blocks still open at the terminal frame are *not* flushed
                // here, and the omission is the rule rather than a gap. Their
                // arguments are a prefix of a JSON document that the provider
                // stopped sending, so emitting one would hand the client a call
                // whose arguments do not parse — and a client that ran it anyway
                // would act on truncated input. The same answer this decoder
                // gives a stream that reports half its bill: nothing, rather than
                // something plausible nobody can check.
                self.emit_done();
                Ok(())
            }
            "error" => {
                self.finished = true;
                // The turn failed upstream, and it failed *after* the stream
                // opened — so this must reach the engine as an error rather than
                // as a short answer, exactly as `response.failed` does on the
                // Responses wire. A turn that ended because the provider refused
                // it must not look like a turn that simply produced little.
                //
                // **The asymmetry with a pre-stream 529 is deliberate.** The
                // same `overloaded_error` arriving as an HTTP status before any
                // bytes is retryable and fails over to another target; arriving
                // here it is terminal, because deltas have already been emitted
                // and handed downstream. Retrying from this point would have the
                // second target restate output the client has already seen, so
                // the turn's answer would contain the beginning twice. Refusing
                // the turn costs one dispatch; duplicating it corrupts the
                // transcript that the next turn's prefix admission is built on.
                let error = self.parse(name, payload).map(|event| match event {
                    StreamEvent::Error { error, .. } => error,
                    _ => unreachable!("parsed as the name it was dispatched on"),
                });
                Err(FrontierError::Upstream(format!(
                    "the upstream sent an error frame: {}",
                    describe(error)
                )))
            }
            // `ping` is a keepalive with a payload and nothing else, and
            // everything else on this wire is a frame this client has no use
            // for. Skipped rather than refused, and skipped *without parsing*:
            // an event type added upstream must not be able to fail a deployment
            // that already works, which is the openness R1 asks for and the
            // reason the `StreamEvent` enum needs no catch-all arm.
            //
            // The two block-lifecycle frames used to be here, on the argument
            // that index discipline was the *accumulator's* problem and not this
            // decoder's. That was true while the only thing crossing this seam
            // was prose. It stopped being true with `FrontierChunk::ToolCall`:
            // the index is now the only thing that says which call a fragment
            // belongs to, so the two frames moved into arms of their own above.
            _ => Ok(()),
        }
    }

    /// One frame, as the typed event it was dispatched as.
    ///
    /// Only reached for the four names whose *contents* this client reads. A
    /// shape error there is a real failure — an unreadable `message_start` means
    /// the turn's input count is gone — while the same strictness applied to
    /// `content_block_stop` would fail a turn over a field nothing looks at.
    fn parse(&self, name: &str, mut payload: Value) -> Result<StreamEvent, FrontierError> {
        if let Some(object) = payload.as_object_mut()
            && !object.contains_key("type")
        {
            // Dispatch got here on the `event:` line alone, so the payload has
            // no tag for serde to match on. Supplying the one the frame already
            // committed to is not a guess: the cross-check above proved they do
            // not disagree.
            object.insert("type".to_string(), Value::String(name.to_string()));
        }
        serde_json::from_value(payload).map_err(|source| {
            FrontierError::Upstream(format!(
                "the upstream sent a `{name}` this build could not read: {source}"
            ))
        })
    }

    fn fold_message_start(&mut self, message: &Message) {
        if !message.usage.reported_any_input() {
            // A prelude with no counts at all. Recorded as "nothing reported"
            // rather than as zeros, so that `message_stop` yields no `Done` and
            // the engine's estimate stands in — see `emit_done`.
            return;
        }
        self.saw_input = true;
        self.input.fold(&message.usage);
        // The 5m/1h split (`message.usage.cache_creation`) is parsed and not
        // carried: the ledger has one cache-write rate, so the total is what
        // pricing can use. Reading it here and dropping it would be the place
        // to change when it prices two.
    }

    fn fold_message_delta(&mut self, usage: &Usage) {
        // The output-side mirror of `fold_message_start`'s `reported_any_input`
        // gate, and a count rather than the presence of the `usage` object for
        // the same reason: a frame that carried an empty object told us nothing,
        // and `Usage`'s fields all default to zero, so presence would let a
        // proxy that stripped the counts but kept the braces book a real answer
        // at nothing.
        if usage.output_tokens > 0 {
            self.saw_output = true;
        }
        // Cumulative, per the API's own documentation, so a later frame can only
        // be greater than or equal to an earlier one. `max` rather than
        // assignment therefore loses nothing and makes a frame that omitted the
        // field unable to retract a count already reported — the failure
        // Anthropic's own client leaves open by merging this one field with `??`
        // (client surface §3.4).
        self.output_tokens = self.output_tokens.max(usage.output_tokens);
        // A `message_delta` may also restate the input side under some betas.
        // Folded with the same greater-than-zero guard the prelude uses, so a
        // restatement can correct a count but never erase one.
        if self.saw_input {
            self.input.fold(usage);
        }
    }

    /// One `content_block_delta`, routed by what it carries.
    ///
    /// `index` is used by exactly one arm and passed to all of them, because the
    /// alternative — reading it only where it is needed — would mean the caller
    /// deciding which deltas are indexed, and the caller is the frame dispatcher
    /// rather than the thing that knows what a delta means.
    fn fold_delta(&mut self, index: u64, delta: BlockDelta) -> Result<(), FrontierError> {
        match delta {
            BlockDelta::TextDelta { text, .. } => {
                if !text.is_empty() {
                    self.pending.push_back(FrontierChunk::OutputText(text));
                }
            }
            // **Consumed, never emitted, and never an error.** Thinking is not
            // spoken text: it is the model's scratch space, and appending it to
            // the answer would put reasoning into the client's transcript and
            // into the durable item the next turn resends. A signature is the
            // opaque attestation that lets a thinking block be resent at all.
            // Both are real frames on every extended-thinking turn, so refusing
            // them would fail the turns this product routes most.
            //
            // What that costs, stated because it is a real cost: the thinking
            // tokens are billed inside `output_tokens` and roundhouse's
            // `reasoning_tokens` stays zero on this dialect, so a thinking turn
            // reads as a verbose answer rather than as an expensive silence. The
            // count is not on the wire to be read — it is `output_tokens_details`
            // under a beta this build does not request — so the alternative
            // would be counting the characters we just declined to keep.
            BlockDelta::ThinkingDelta { .. } | BlockDelta::SignatureDelta { .. } => {}
            // **Accumulated, never emitted one at a time.** A `partial_json` is
            // a *prefix* of a JSON document — the first fragment of a real call
            // is routinely `{"pat` — so a chunk per fragment would hand every
            // consumer the same reassembly problem and let two of them disagree
            // about it. The concatenation becomes one `ToolCall` at this block's
            // `content_block_stop`.
            //
            // A fragment naming a block this decoder never saw open is dropped
            // rather than starting one: with no `content_block_start` there is
            // no id and no tool name, and a call with arguments but no name is
            // not something a client can run. Silent for the same reason the
            // unclosed block is — there is nothing here worth failing a served
            // turn over.
            BlockDelta::InputJsonDelta { partial_json, .. } => {
                if let Some(block) = self.tool_blocks.get_mut(&index) {
                    // Bounded for the reason `MAX_EVENT_BYTES` is, and this is
                    // the only accumulation in this file that spans frames: each
                    // fragment is small and legal, so an upstream that never
                    // stops sending them is caught by nothing else.
                    if block.fragments.len() + partial_json.len() > MAX_TOOL_ARGUMENT_BYTES {
                        return Err(FrontierError::Upstream(format!(
                            "the upstream sent more than {MAX_TOOL_ARGUMENT_BYTES} bytes of \
                             arguments for one tool block; abandoning the stream rather \
                             than buffering them"
                        )));
                    }
                    block.fragments.push_str(&partial_json);
                }
            }
            BlockDelta::Other(_) => {}
        }
        Ok(())
    }

    /// The one accounting frame this dialect's two usage events fold into.
    ///
    /// **Both halves or neither**, which is the F6 correction. A `Done` is the
    /// engine's signal that the provider reported this turn — it books it as
    /// `Accounting::Reported` unconditionally — so a `Done` assembled from one
    /// measured half and one defaulted zero is not a partial record, it is a
    /// fabricated one wearing the provider's authority. Zero output tokens on a
    /// hosted model prices at zero dollars, and a zero-dollar frontier turn is
    /// indistinguishable on the savings dashboard from a turn that was routed
    /// locally — the one failure the metrics chapter is built against.
    ///
    /// The cost of the stricter gate, stated because it is real: a stream whose
    /// final `message_delta` was stripped of its output count loses the *input*
    /// counts it did measure, and the engine estimates the whole turn instead of
    /// half of it. That trade is deliberate. An estimate is marked as an
    /// estimate everywhere it is read, so the accounting stays honest and merely
    /// less precise; the alternative writes a number nobody measured into the
    /// column the whole product is judged on, and marks it measured.
    fn emit_done(&mut self) {
        if !(self.saw_input && self.saw_output) {
            // A missing prelude, a prelude that reported nothing, or a final
            // `message_delta` that never carried an output count. The engine
            // substitutes its own estimate and marks it; a `Done` built here
            // would carry a zero on one axis, which folds as zero tokens for
            // zero dollars and reads as a saving.
            return;
        }
        self.pending.push_back(FrontierChunk::Done {
            // **The inversion.** `fresh + read + written`, because Anthropic's
            // three counters are disjoint and roundhouse's `input_tokens` is the
            // total. Sending `usage.input_tokens` straight through would report
            // a 200 000-token cached prompt as the 12 tokens that were new.
            input_tokens: self.input.total(),
            cached_input_tokens: self.input.read,
            cache_read_source: self.input.read_source,
            cache_write_tokens: self.input.written,
            output_tokens: self.output_tokens,
            // Thinking is billed as ordinary output here and reported by no
            // field this build reads — see `fold_delta`.
            reasoning_tokens: 0,
            // **Not read, rather than not there** — a distinction M10.3's
            // reconciliation reader needs, because it is only half true that
            // this wire carries no price. `api.anthropic.com`'s usage object
            // has no cost field at all. OpenRouter's `/messages` route speaks
            // the same dialect and *does* attach `cost` and `cost_details`,
            // which this build parses into `Usage::extra` and deliberately does
            // not fold: a provider-reported dollar figure is a second pricing
            // authority beside the catalog's rate card, and which of the two
            // wins is a ruling nobody has made yet. `None` and not zero either
            // way: "no price was read here" and "this call was free" are the
            // two readings a reconciliation view must never confuse.
            provider_reported_cost: None,
            // **Verbatim, including a value this build has never seen.** The
            // typed parse on the way in has an `Other(String)` arm precisely so
            // an eighth stop reason is carried rather than refused, and
            // `as_wire` gives that arm back its own spelling — so what reaches
            // the log is what the provider said, and the emit layers decide what
            // a client in *their* dialect is owed for it. `None` when no frame
            // named one, which on a stream that reached `message_stop` means a
            // proxy stripped the field: "nobody said" and "end_turn" are
            // different facts, and only the first is true here.
            stop_reason: self.stop_reason.clone(),
        });
    }
}

/// Where the blank line separating two events sits: the offset the event's own
/// text ends at, and the offset the next event begins at.
///
/// **Not `find("\n\n")`, and the difference is a whole turn.** SSE's line
/// grammar (`stream_format`) accepts CR, LF *or* CRLF as the terminator, so a
/// CRLF-framed body separates events with `0D 0A 0D 0A` — which contains no
/// `0A 0A` pair at all. A scan for the shorter form therefore finds no boundary
/// anywhere in such a stream: every event stays in the buffer until `eof`, which
/// hands the whole body to one `decode_event`, which joins several `data:` lines
/// into one string and fails the turn on `trailing characters at line 2`. Not a
/// truncated answer and not a mis-count — no output, no accounting, and no
/// failover, because the failure arrives after the stream opened.
///
/// The two offsets are returned separately rather than one plus a fixed width
/// because the width is not fixed: it is 2, 3 or 4 bytes depending on which
/// terminators the server chose, and mixing that up either eats the first byte
/// of the next event or leaves a stray one at the head of it.
///
/// Duplicated in `openai_responses::stream` rather than shared, exactly as
/// `MAX_EVENT_BYTES` is: the two decoders are deliberate mirrors down to their
/// buffering, and a shared module would be the first thing to join two files
/// whose whole value is that each can be read alone.
fn event_boundary(buffer: &str) -> Option<(usize, usize)> {
    let bytes = buffer.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' && *byte != b'\r' {
            continue;
        }
        // Both terminator bytes are ASCII, so this index is a char boundary
        // whatever multi-byte text surrounds it.
        let rest = &buffer[index..];
        let Some(first) = terminator(rest) else {
            continue;
        };
        if let Some(second) = terminator(&rest[first..]) {
            return Some((index, index + first + second));
        }
    }
    None
}

/// The length of a line terminator at the head of `rest`, if one is there.
///
/// **CRLF is tested first and that ordering is load-bearing.** Reading the `\r`
/// of a `\r\n` as a terminator on its own would make the `\n` after it look like
/// a second one, and every ordinary CRLF-framed line would then read as a blank
/// line — splitting each event into fragments that decode as nothing.
fn terminator(rest: &str) -> Option<usize> {
    if rest.starts_with("\r\n") {
        Some(2)
    } else if rest.starts_with('\n') || rest.starts_with('\r') {
        Some(1)
    } else {
        None
    }
}

/// The lines of one event block, on any of the three terminators SSE allows.
///
/// `str::lines` splits on `\n` alone (tolerating a trailing `\r`), which is
/// right for LF and CRLF framing and silently wrong for CR-only framing: the
/// whole event arrives as one "line" that begins with `event:` and carries the
/// `data:` payload inside it, so the frame is dropped as payload-less rather
/// than refused. Splitting on either byte reads all three the same way; the
/// empty strings a `\r\n` pair produces match no field prefix and cost nothing.
fn lines(event: &str) -> impl Iterator<Item = &str> {
    event.split(['\r', '\n'])
}

/// What an error frame said, or a stand-in naming the absence.
///
/// Takes the *parse result* rather than the error body, because a mid-stream
/// `error` whose payload this build cannot read is still a failed turn and must
/// not become a successful short one. The caller redacts this before it reaches
/// anyone, so it may quote the upstream freely.
fn describe(error: Result<ApiError, FrontierError>) -> String {
    match error {
        Ok(error) if error.message.is_empty() && error.kind.is_empty() => {
            "the upstream named no reason".to_string()
        }
        Ok(error) if error.message.is_empty() => error.kind,
        Ok(error) if error.kind.is_empty() => error.message,
        Ok(error) => format!("{}: {}", error.kind, error.message),
        Err(unreadable) => format!("(the error body was itself unreadable: {unreadable})"),
    }
}

#[cfg(test)]
mod tests;
