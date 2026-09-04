// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Turning a Responses SSE body into [`FrontierChunk`]s.
//!
//! Split from [`super`] because it answers a different question. That file is
//! about *authentication and the request*; this one is about a byte stream that
//! arrives in arbitrary pieces and has to become a sequence of durable deltas.
//!
//! **Dispatch is on the payload's own `type`, never on the `event:` line.** The
//! two are meant to agree and a proxy is free to drop the `event:` line
//! entirely — it is optional in the SSE grammar — while the JSON body is what
//! the provider actually documents. Reading the line would make roundhouse's
//! accounting depend on a field intermediaries treat as decoration.
//!
//! **A stream that ends without `response.completed` yields no `Done`, and that
//! is deliberate.** The engine already stands an estimate in for a provider
//! that reported nothing, and marks it estimated
//! (`Engine::estimated_usage` → [`Accounting`]). Synthesizing a zero-token
//! `Done` here would fold as *zero tokens for zero dollars*, which is
//! indistinguishable from a saving — the exact failure
//! [`crate::usage`] exists to prevent.
//!
//! **A tool call is read off `response.output_item.done`, and the argument
//! deltas are ignored — which is the opposite of the Anthropic decoder next
//! door, and the divergence is the wire's, not a preference.** The conformance
//! oracle is the pinned `codex` tree (`codex-api/src/sse/responses.rs` @
//! `6344a65`), and it settles this: `response.function_call_arguments.delta` and
//! `.done` are both listed in that parser's explicitly-unhandled arm, while the
//! finished `function_call` item — `call_id`, `name`, and `arguments` as a
//! *complete* JSON string — arrives on `response.output_item.done`. So this wire
//! hands over the whole call in one frame and needs no accumulator, where the
//! Messages wire only ever hands over fragments and needs one. Reading the
//! deltas here as well would be a second, independently-wrong path to the same
//! value: the two would have to agree, and nothing would check that they did.
//!
//! [`Accounting`]: roundhouse_core::event::Accounting

use std::collections::VecDeque;

use serde_json::Value;

use crate::frontier::{FrontierChunk, FrontierError};

/// How much of a single SSE event this will buffer before giving up.
///
/// A bound rather than trust: the body is a remote party's, and an upstream (or
/// something pretending to be one) that never sends the blank line separating
/// events would otherwise grow this buffer until the process died. Generous
/// enough that no real frame comes close — a `response.completed` payload is a
/// few hundred bytes and the largest text delta a provider emits is far below
/// this.
const MAX_EVENT_BYTES: usize = 1 << 20;

/// Assembles SSE events out of arbitrary byte runs and decodes the ones that
/// carry output or accounting.
#[derive(Default)]
pub(super) struct SseDecoder {
    /// Bytes received and not yet consumed by a complete event.
    buffer: String,
    /// Decoded chunks waiting to be yielded, in arrival order. A queue because
    /// one `feed` may complete several events.
    pending: VecDeque<FrontierChunk>,
    /// Set by the frame that ends a response. Nothing after it is read: a
    /// provider that kept talking past `response.completed` has already told us
    /// what the turn cost.
    finished: bool,
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
    /// Takes `&[u8]` and appends lossily rather than requiring valid UTF-8 per
    /// call: a chunk boundary lands mid-codepoint routinely, and an error there
    /// would fail turns at random on any prompt containing a non-ASCII
    /// character. Buffering the raw bytes and decoding at the event boundary is
    /// the alternative, and it costs a second copy of every delta for a
    /// difference nothing observes — the payload is JSON, whose parser rejects
    /// a truncated escape anyway.
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Result<(), FrontierError> {
        if self.finished {
            return Ok(());
        }
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        self.drain()
    }

    /// The body ended. Decode a final event that arrived without its blank
    /// line, which is what a well-behaved server sends before closing.
    pub(super) fn eof(&mut self) -> Result<(), FrontierError> {
        if self.finished {
            return Ok(());
        }
        let tail = std::mem::take(&mut self.buffer);
        self.decode_event(&tail)
    }

    /// Consume every complete event in the buffer.
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

    /// Decode one event block: every `data:` line joined, parsed as JSON,
    /// dispatched on its own `type`.
    fn decode_event(&mut self, event: &str) -> Result<(), FrontierError> {
        let mut data = String::new();
        for line in lines(event) {
            // A comment (`: keep-alive`) and every non-`data` field are skipped
            // rather than refused: the SSE grammar lets a server send both, and
            // a proxy that inserts heartbeats must not fail a turn.
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start());
            }
        }
        let data = data.trim();
        // `[DONE]` is the Chat Completions sentinel. The Responses API does not
        // send it, but a gateway that normalizes between the two may, and
        // treating it as JSON would fail an otherwise complete turn.
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        let payload: Value = serde_json::from_str(data).map_err(|source| {
            FrontierError::Upstream(format!("the upstream sent an unparseable event: {source}"))
        })?;
        self.dispatch(&payload)
    }

    fn dispatch(&mut self, payload: &Value) -> Result<(), FrontierError> {
        match payload.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                if let Some(delta) = payload.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    self.pending
                        .push_back(FrontierChunk::OutputText(delta.to_string()));
                }
                Ok(())
            }
            // **The whole tool call, in one frame.** See the module doc: on this
            // wire the terminal item carries the complete `arguments` string, so
            // there is nothing to assemble and the argument deltas are noise.
            //
            // `response.output_item.added` is *not* read, though it carries the
            // same item type: at `added` the arguments are `""` and the call is
            // a placeholder. Emitting there would hand the client a call with no
            // arguments and then no way to correct it, since this enum has no
            // amendment channel — one chunk per completed call is the contract.
            Some("response.output_item.done") => {
                if let Some(call) = function_call(payload.get("item")) {
                    self.pending.push_back(call);
                }
                Ok(())
            }
            Some("response.completed") => {
                self.finished = true;
                // A completion with no usage object is not an error: it is an
                // unaccounted call, which the engine marks as estimated. What
                // is *not* done here is inventing zeros — see the module doc.
                if let Some(usage) = payload.pointer("/response/usage") {
                    self.pending
                        .push_back(usage_chunk(usage, payload.pointer("/response")));
                }
                Ok(())
            }
            // The upstream's own terminal failures. Reported as an upstream
            // error rather than a short stream, because a turn that ended
            // because the provider refused it must not look to the engine like
            // a turn that simply produced little.
            Some("response.failed") => {
                self.finished = true;
                Err(FrontierError::Upstream(format!(
                    "the upstream failed the response: {}",
                    error_message(payload.pointer("/response/error"))
                )))
            }
            Some("error") => {
                self.finished = true;
                Err(FrontierError::Upstream(format!(
                    "the upstream sent an error frame: {}",
                    error_message(Some(payload))
                )))
            }
            // Everything else on the Responses wire — `response.created`,
            // item lifecycle, reasoning summaries — is a frame this client has
            // no use for. Skipped rather than refused: a provider adding a
            // frame type must not break a deployment that already works.
            _ => Ok(()),
        }
    }
}

/// Where the blank line separating two events sits: the offset the event's own
/// text ends at, and the offset the next event begins at.
///
/// **Not `find("\n\n")`.** SSE's line grammar accepts CR, LF *or* CRLF, so a
/// CRLF-framed body separates events with `0D 0A 0D 0A`, which contains no
/// `0A 0A` pair — a scan for the shorter form finds no boundary anywhere in such
/// a stream, hands the whole body to one `decode_event` at `eof`, and fails the
/// turn on the several `data:` payloads it then tries to parse as one. The
/// comment that used to sit here claimed the shorter form covered both, which
/// is how it survived: it is the kind of claim that reads as obviously true.
///
/// The twin of this function in `anthropic_messages::stream`, duplicated rather
/// than shared for the same reason `MAX_EVENT_BYTES` is: the two decoders are
/// deliberate mirrors, each meant to be readable alone.
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
/// a second one, so every ordinary CRLF-terminated line would read as a blank
/// line and split its own event.
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
/// `str::lines` splits on `\n` alone, which reads CR-only framing as one long
/// line whose `data:` field is buried mid-string — the frame is then dropped as
/// payload-less rather than refused. The empty strings a `\r\n` pair produces
/// here match no field prefix and cost nothing.
fn lines(event: &str) -> impl Iterator<Item = &str> {
    event.split(['\r', '\n'])
}

/// The tool call an output item describes, or `None` for every other item.
///
/// Shapes read straight from the conformance oracle's own type
/// (`codex-protocol`'s `ResponseItem::FunctionCall` @ `6344a65`): `call_id` and
/// `name` are required there, and `arguments` is a `String` carrying JSON —
/// which is why it is moved rather than parsed, and why
/// `FrontierChunk::ToolCall::arguments` is a `String` too.
///
/// **Every field is required here and a missing one drops the item**, which is
/// the same "nothing rather than something fabricated" rule the Anthropic
/// decoder applies to an unclosed tool block. A call with no `call_id` cannot be
/// paired with its result, and one with no `name` names no tool: a client handed
/// either would fail at a place that says nothing about where the value went
/// missing.
///
/// `namespace` — the oracle's optional MCP qualifier — is read **beside** the
/// name and never folded into it (M17, R-N6). Folding would put a dialect's
/// spelling into the log and make a namespaced call and a flat call to one tool
/// two different tools; dropping it, which is what this decoder did until M17,
/// left a model that asked for one of roundhouse's own MCP tools stored bare
/// and re-emitted with no `namespace` for codex's exact
/// `ToolName { name, namespace }` lookup to resolve. Optional here because it
/// is optional on the wire: a plain function tool sends no such field, and an
/// absent one means "this tool has no server", not "nobody said".
fn function_call(item: Option<&Value>) -> Option<FrontierChunk> {
    let item = item?;
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return None;
    }
    Some(FrontierChunk::ToolCall {
        id: item.get("call_id").and_then(Value::as_str)?.to_string(),
        name: item.get("name").and_then(Value::as_str)?.to_string(),
        // Absent reads as `None` rather than dropping the item, unlike the
        // three required fields above: a call with no server is a call, while a
        // call with no id cannot be paired and one with no name names nothing.
        namespace: item
            .get("namespace")
            .and_then(Value::as_str)
            .map(str::to_string),
        // `as_str` and not `to_string` on the value: this field is a JSON
        // *string* on the wire, so a `Value::String` renders with its quotes and
        // escapes if stringified whole, and the client would receive
        // `"{\"path\":\"/a\"}"` where the model sent `{"path":"/a"}`.
        arguments: item.get("arguments").and_then(Value::as_str)?.to_string(),
    })
}

/// The accounting frame, read out of a `usage` object.
///
/// Missing fields read as zero, which is right *here* and wrong in general: this
/// function only runs when the provider sent a `usage` object, so an absent
/// `reasoning_tokens` means a model that does not reason rather than an
/// unaccounted call. The unaccounted case never reaches this function at all.
fn usage_chunk(usage: &Value, response: Option<&Value>) -> FrontierChunk {
    let count = |value: Option<&Value>| value.and_then(Value::as_u64).unwrap_or(0);
    FrontierChunk::Done {
        input_tokens: count(usage.get("input_tokens")),
        cached_input_tokens: count(usage.pointer("/input_tokens_details/cached_tokens")),
        // Zero and not a read of `input_tokens_details.cache_write_tokens`,
        // because no such field exists on this wire: the Responses API bills a
        // cache write as ordinary uncached input and reports no separate count.
        // Zero here therefore means "nothing was written", which is true, rather
        // than "nobody said" — see `FrontierChunk::Done::cache_write_tokens`.
        cache_write_tokens: 0,
        output_tokens: count(usage.get("output_tokens")),
        reasoning_tokens: count(usage.pointer("/output_tokens_details/reasoning_tokens")),
        // `cost` is an OpenRouter extension to the Responses usage object;
        // OpenAI's own endpoint omits it. Absent stays `None` rather than
        // becoming zero, and the difference is the whole reason the field is an
        // `Option`: "this provider does not report prices" and "this call was
        // free" are the two readings a reconciliation view must never confuse,
        // and zero is the one that reads as a saving.
        //
        // `as_f64` and not a parse: a non-numeric `cost` is a provider saying
        // something this decoder does not understand, and `None` is the honest
        // answer to that -- refusing the whole stream over an accounting extra
        // would fail a turn that was served correctly.
        provider_reported_cost: usage.get("cost").and_then(Value::as_f64),
        // **`None` is the ordinary answer on this wire, and that is a fact
        // about the wire rather than a gap here.** The Responses API has no
        // stop-reason field for a turn that ended normally — `response.status`
        // is a lifecycle state ("completed"), not a reason, and putting it here
        // would spell a *state* in the field every other dialect spells a reason
        // in. The one place this wire does name a reason is
        // `incomplete_details.reason`, which is read.
        //
        // The consequence, stated because it is real: a tool-use turn on this
        // dialect reports no stop reason at all. The `ToolCall` chunk is the
        // signal instead, which is the honest reading — the provider announced a
        // call and never announced a reason — and it carries the same
        // information a synthesized `tool_use` would have, minus the
        // fabrication.
        //
        // A truncated turn arrives on this wire as a separate
        // `response.incomplete` event rather than as a `response.completed`
        // carrying details, and this decoder does not terminate on that event
        // (the oracle treats it as a stream error). Reading the field on the
        // completion frame is the cheap half; the incomplete frame is a named
        // gap, not a claim that one cannot happen.
        stop_reason: response
            .and_then(|response| response.pointer("/incomplete_details/reason"))
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// What an upstream error object says, or a stand-in naming the absence.
///
/// The caller redacts this before it reaches anyone — see
/// [`ForwardedCredential::redact`](roundhouse_core::control::ForwardedCredential::redact)
/// — so it may quote the upstream freely.
fn error_message(error: Option<&Value>) -> String {
    error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "the upstream named no reason".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the decoder over `pieces` and collect what it yields.
    fn decode(pieces: &[&str]) -> Result<Vec<FrontierChunk>, FrontierError> {
        let mut decoder = SseDecoder::default();
        let mut chunks = Vec::new();
        for piece in pieces {
            decoder.feed(piece.as_bytes())?;
            while let Some(chunk) = decoder.next_chunk() {
                chunks.push(chunk);
            }
        }
        decoder.eof()?;
        while let Some(chunk) = decoder.next_chunk() {
            chunks.push(chunk);
        }
        Ok(chunks)
    }

    const COMPLETED: &str = concat!(
        "event: response.completed\n",
        r#"data: {"type":"response.completed","response":{"usage":{"#,
        r#""input_tokens":120,"input_tokens_details":{"cached_tokens":100},"#,
        r#""output_tokens":30,"output_tokens_details":{"reasoning_tokens":12}}}}"#,
        "\n\n"
    );

    #[test]
    fn a_responses_stream_becomes_deltas_and_one_accounting_frame() {
        let chunks = decode(&[
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
            COMPLETED,
        ])
        .unwrap();

        assert_eq!(
            chunks,
            vec![
                FrontierChunk::OutputText("Hel".into()),
                FrontierChunk::OutputText("lo".into()),
                FrontierChunk::Done {
                    input_tokens: 120,
                    cached_input_tokens: 100,
                    cache_write_tokens: 0,
                    output_tokens: 30,
                    reasoning_tokens: 12,
                    // OpenAI's own Responses usage object carries no `cost`,
                    // and absent is `None` rather than zero — see
                    // `usage_chunk`.
                    provider_reported_cost: None,
                    // Nor a stop reason: this wire names one only when a turn
                    // ended early, and `COMPLETED` did not.
                    stop_reason: None,
                },
            ],
            "the cached count is the quantity the whole system exists to \
             maximize, and it only arrives if this reads the field"
        );
    }

    #[test]
    fn a_frame_split_across_reads_is_reassembled() {
        // The ordinary case on a real socket, and the one a naive
        // parse-per-read gets wrong: a chunk boundary anywhere, including
        // mid-JSON and mid-separator.
        let split = decode(&[
            "event: response.outp",
            "ut_text.delta\ndata: {\"type\":\"response.output_te",
            "xt.delta\",\"delta\":\"ok\"}\n",
            "\n",
            &COMPLETED[..40],
            &COMPLETED[40..],
        ])
        .unwrap();
        assert_eq!(split[0], FrontierChunk::OutputText("ok".into()));
        assert!(matches!(split[1], FrontierChunk::Done { .. }));

        // A multi-byte character straddling a read is text, not a failure.
        let text = "日本語";
        let frame =
            format!("data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{text}\"}}\n\n");
        let bytes = frame.as_bytes();
        let mut decoder = SseDecoder::default();
        decoder.feed(&bytes[..40]).unwrap();
        decoder.feed(&bytes[40..]).unwrap();
        assert_eq!(
            decoder.next_chunk(),
            Some(FrontierChunk::OutputText(text.into()))
        );
    }

    /// The same defect F5 found next door, in the file that decoder copied.
    ///
    /// No review finding named this one — the Anthropic decoder is where a CRLF
    /// stream was caught — but `drain` here carried the identical
    /// `find("\n\n")` scan under the identical comment claiming it handled CRLF,
    /// and the two files are deliberate mirrors. The consequence is the same and
    /// arrives on a wire roundhouse has been dispatching since M8: a legal
    /// CRLF-framed body has no `\n\n` byte pair anywhere, so no boundary is ever
    /// found, the whole body reaches `eof` as one event, and several `data:`
    /// lines joined by newlines fail `serde_json` — one error, no output, no
    /// accounting, for a turn the provider served correctly.
    #[test]
    fn crlf_and_cr_framed_events_are_split_by_drain_the_same_as_lf() {
        let delta = "event: response.output_text.delta\n\
                     data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n";
        for (framing, reframe) in [
            (
                "CRLF",
                (|s: &str| s.replace('\n', "\r\n")) as fn(&str) -> String,
            ),
            ("CR only", |s: &str| s.replace('\n', "\r")),
            ("CRLF line, LF blank", |s: &str| s.replace("\n\n", "\r\n\n")),
            ("LF line, CRLF blank", |s: &str| s.replace("\n\n", "\n\r\n")),
            // CONTROL: the LF framing every other test here uses, through the
            // same loop, so a scan that had stopped finding `\n\n` cannot pass.
            ("LF", |s: &str| s.to_string()),
        ] {
            let chunks = decode(&[&reframe(delta), &reframe(COMPLETED)])
                .unwrap_or_else(|error| panic!("{framing} framing must decode: {error}"));
            assert_eq!(
                chunks,
                vec![
                    FrontierChunk::OutputText("ok".into()),
                    FrontierChunk::Done {
                        input_tokens: 120,
                        cached_input_tokens: 100,
                        cache_write_tokens: 0,
                        output_tokens: 30,
                        reasoning_tokens: 12,
                        provider_reported_cost: None,
                        stop_reason: None,
                    },
                ],
                "{framing} framing"
            );
        }
    }

    #[test]
    fn a_stream_that_never_completes_yields_no_accounting_frame() {
        // The engine estimates for a provider that reported nothing and marks
        // the estimate. A synthesized zero-token `Done` here would fold as zero
        // tokens for zero dollars, which reads as a saving.
        let chunks = decode(&[
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"half an answ\"}\n\n",
        ])
        .unwrap();
        assert_eq!(
            chunks,
            vec![FrontierChunk::OutputText("half an answ".into())]
        );

        // And a completion with no usage object is the same case, not a zeroed
        // one.
        let unaccounted =
            decode(&["data: {\"type\":\"response.completed\",\"response\":{}}\n\n"]).unwrap();
        assert!(unaccounted.is_empty());
    }

    #[test]
    fn an_upstream_failure_is_an_error_and_not_a_short_stream() {
        // PROBE: both terminal failure shapes the Responses wire uses.
        for frame in [
            r#"data: {"type":"response.failed","response":{"error":{"message":"quota exceeded"}}}"#,
            r#"data: {"type":"error","message":"quota exceeded"}"#,
        ] {
            let error = decode(&[&format!("{frame}\n\n")]).expect_err("must be an error");
            assert!(
                matches!(&error, FrontierError::Upstream(message) if message.contains("quota exceeded")),
                "{error}"
            );
        }

        // CONTROL: an unparseable body is also an error rather than a silently
        // empty turn -- and it names what happened, because "the upstream sent
        // something we could not read" and "the model said nothing" are
        // different operational problems.
        let garbage = decode(&["data: not json at all\n\n"]).expect_err("must be an error");
        assert!(garbage.to_string().contains("unparseable"), "{garbage}");

        // CONTROL: a keep-alive comment and an unknown frame type are skipped,
        // so the strictness above is about failures and not about novelty.
        assert!(
            decode(&[": keep-alive\n\ndata: {\"type\":\"response.in_progress\"}\n\n"])
                .unwrap()
                .is_empty()
        );
    }

    /// **P3: an OpenRouter-shaped stream parses, keep-alives and all.**
    ///
    /// OpenRouter injects `: OPENROUTER PROCESSING` comment lines into the SSE
    /// body (`openrouter-api-surface.md` Q5.1, live 2026-08-24), and a
    /// line-oriented parser that tried to JSON-decode one would fail an
    /// otherwise perfect turn — at an interval that depends on how long the
    /// upstream took, so it would fail *the slow turns* and look like a
    /// timeout.
    ///
    /// Two placements, because they exercise different code. A comment as its
    /// own event block reaches `decode_event` with no `data:` line at all and
    /// is discarded by the empty-payload check; a comment *inside* an event
    /// block has to be skipped by the line loop while the block's real `data:`
    /// line still decodes. Only the first was covered before, by the
    /// `": keep-alive"` control in the test above.
    #[test]
    fn an_openrouter_shaped_stream_with_comment_keepalives_parses() {
        let chunks = decode(&[
            ": OPENROUTER PROCESSING\n\n",
            // Interleaved: the comment shares the block with the payload.
            concat!(
                ": OPENROUTER PROCESSING\n",
                r#"data: {"type":"response.output_text.delta","delta":"kimi"}"#,
                "\n\n",
            ),
            ": OPENROUTER PROCESSING\n\n",
            concat!(
                r#"data: {"type":"response.completed","response":{"usage":{"#,
                r#""input_tokens":1200,"input_tokens_details":{"cached_tokens":900},"#,
                r#""output_tokens":64,"output_tokens_details":{"reasoning_tokens":8},"#,
                // The OpenRouter extension: dollars beside the counts.
                r#""cost":0.00421,"cost_details":{"upstream_inference_cost":0.004}}}}"#,
                "\n\n",
            ),
        ])
        .expect("a keep-alive must never fail a turn");

        assert_eq!(
            chunks,
            vec![
                FrontierChunk::OutputText("kimi".into()),
                FrontierChunk::Done {
                    input_tokens: 1200,
                    cached_input_tokens: 900,
                    cache_write_tokens: 0,
                    output_tokens: 64,
                    reasoning_tokens: 8,
                    // The number the reconciliation rung will compare our
                    // token-priced figure against. Carried, never added to the
                    // counts beside it.
                    provider_reported_cost: Some(0.00421),
                    stop_reason: None,
                },
            ]
        );
    }

    /// The control that stops the assertion above being about `cost` existing
    /// rather than about it being *read*: the identical stream with the field
    /// absent decodes the identical counts and reports no price.
    ///
    /// Zero would be the tempting default and it is the one answer that is
    /// wrong in a way nobody sees — a provider that reports no price is not a
    /// provider that served the turn for free, and a reconciliation view fed
    /// zeroes would report perfect agreement with a bill it never read.
    #[test]
    fn a_usage_object_without_cost_reports_no_price_rather_than_a_free_turn() {
        let chunks = decode(&[COMPLETED]).unwrap();
        assert_eq!(
            chunks,
            vec![FrontierChunk::Done {
                input_tokens: 120,
                cached_input_tokens: 100,
                cache_write_tokens: 0,
                output_tokens: 30,
                reasoning_tokens: 12,
                provider_reported_cost: None,
                stop_reason: None,
            }]
        );
    }

    /// **The tool call, in the shape the conformance oracle builds it.**
    ///
    /// Copied structurally from codex's own test helper
    /// (`core/tests/common/responses.rs::ev_function_call` @ `6344a65`): a
    /// `response.output_item.done` whose item is a `function_call` with
    /// `call_id`, `name` and a complete `arguments` string. The `added` frame
    /// that precedes it in a real stream carries `"arguments": ""`, and the
    /// argument deltas between them are what the oracle's parser explicitly does
    /// not read — so both appear here, and neither may produce a chunk.
    #[test]
    fn a_function_call_is_read_once_from_the_done_item_and_not_from_its_deltas() {
        let chunks = decode(&[
            concat!(
                r#"data: {"type":"response.output_item.added","item":{"#,
                r#""type":"function_call","id":"fc_1","call_id":"call_1","#,
                r#""name":"shell","arguments":"","status":"in_progress"}}"#,
                "\n\n",
            ),
            concat!(
                r#"data: {"type":"response.function_call_arguments.delta","#,
                r#""item_id":"fc_1","delta":"{\"command\":"}"#,
                "\n\n",
            ),
            concat!(
                r#"data: {"type":"response.output_item.done","item":{"#,
                r#""type":"function_call","call_id":"call_1","name":"shell","#,
                r#""arguments":"{\"command\": [\"ls\", \"-l\"]}"}}"#,
                "\n\n",
            ),
            COMPLETED,
        ])
        .expect("a function-call turn must decode");

        assert_eq!(
            chunks,
            vec![
                FrontierChunk::ToolCall {
                    // `call_id`, not `id`: `id` names the *item*, `call_id`
                    // names the call, and only the second is what a
                    // `function_call_output` is paired on.
                    id: "call_1".into(),
                    name: "shell".into(),
                    namespace: None,
                    // The wire's own string, with its spacing, moved rather than
                    // parsed — see `function_call`.
                    arguments: r#"{"command": ["ls", "-l"]}"#.into(),
                },
                FrontierChunk::Done {
                    input_tokens: 120,
                    cached_input_tokens: 100,
                    cache_write_tokens: 0,
                    output_tokens: 30,
                    reasoning_tokens: 12,
                    provider_reported_cost: None,
                    // This wire names no reason for a turn that ended normally,
                    // and a tool-use turn is one of those: the call above is the
                    // signal, not a synthesized word.
                    stop_reason: None,
                },
            ],
            "exactly one call: the `added` placeholder and the argument delta \
             must not each produce one"
        );
    }

    /// **R-N6: an upstream's `namespace` reaches the chunk, and its absence
    /// reaches it as `None` rather than as a dropped call.**
    ///
    /// The gap this closes had no test because it had no field: a model asking
    /// for one of roundhouse's own MCP tools had its namespace dropped here,
    /// was stored bare, and was re-emitted to a codex client with no
    /// `namespace` for that client's exact `ToolName { name, namespace }`
    /// lookup to resolve — a round trip that never worked and that nothing went
    /// red about.
    ///
    /// The negative half is the one that would go wrong quietly. `namespace` is
    /// optional on this wire (a plain function tool sends none), so reading it
    /// with the same `?` the three required fields use would turn every
    /// non-MCP tool call into a silently dropped item — a turn that called a
    /// tool arriving as a turn that called nothing.
    #[test]
    fn a_namespaced_function_call_carries_its_namespace_and_a_bare_one_carries_none() {
        for (item, name, expected, why) in [
            (
                concat!(
                    r#"{"type":"function_call","call_id":"call_1","name":"status","#,
                    r#""namespace":"mcp__roundhouse","arguments":"{}"}"#,
                ),
                "status",
                Some("mcp__roundhouse"),
                "the model asked for an MCP tool and named the server it is on",
            ),
            (
                r#"{"type":"function_call","call_id":"call_1","name":"shell","arguments":"{}"}"#,
                "shell",
                None,
                "a plain function tool has no server and sends no field",
            ),
        ] {
            let done =
                format!("data: {{\"type\":\"response.output_item.done\",\"item\":{item}}}\n\n");
            let chunks =
                decode(&[&done, COMPLETED]).unwrap_or_else(|error| panic!("{why}: {error:?}"));

            assert_eq!(
                chunks.first(),
                Some(&FrontierChunk::ToolCall {
                    id: "call_1".into(),
                    name: name.into(),
                    namespace: expected.map(str::to_string),
                    arguments: "{}".into(),
                }),
                "{why}"
            );
        }
    }

    /// Output items that are not function calls, and function calls missing a
    /// field a client would need, both yield nothing — and neither fails the
    /// turn.
    ///
    /// A `message` item arrives on `response.output_item.done` on *every*
    /// ordinary turn, so treating an unrecognised item as an error would fail
    /// every turn this decoder has ever served. A call with no `call_id` cannot
    /// be paired with its result and one with no `name` names no tool; handing
    /// either to a client fails somewhere that says nothing about where the
    /// value went missing.
    #[test]
    fn an_output_item_that_is_not_a_usable_function_call_yields_nothing() {
        for (item, why) in [
            (
                r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello"}]}"#,
                "the ordinary assistant message every turn ends with",
            ),
            (
                r#"{"type":"web_search_call","status":"completed"}"#,
                "a server-side tool this build does not model",
            ),
            (
                r#"{"type":"function_call","name":"shell","arguments":"{}"}"#,
                "no call_id: nothing could pair the result",
            ),
            (
                r#"{"type":"function_call","call_id":"c","arguments":"{}"}"#,
                "no name: it names no tool",
            ),
            (
                r#"{"type":"function_call","call_id":"c","name":"shell"}"#,
                "no arguments at all",
            ),
            (
                r#"{"type":"function_call","call_id":"c","name":"shell","arguments":{"a":1}}"#,
                "arguments as an object rather than the wire's JSON string",
            ),
        ] {
            let chunks = decode(&[
                &format!("data: {{\"type\":\"response.output_item.done\",\"item\":{item}}}\n\n"),
                COMPLETED,
            ])
            .unwrap_or_else(|error| panic!("{why} must not fail the turn: {error}"));
            assert_eq!(chunks.len(), 1, "{why}: {chunks:?}");
            assert!(matches!(chunks[0], FrontierChunk::Done { .. }), "{why}");
        }
    }

    /// The one place this wire does name a reason, read.
    ///
    /// `incomplete_details.reason` is the Responses spelling of "the answer was
    /// cut off", and it is the same fact the Messages wire spells
    /// `stop_reason: max_tokens`. Without it a truncated turn is
    /// indistinguishable from a complete one everywhere downstream — the defect
    /// M11.1's F1 named on the other dialect.
    #[test]
    fn an_incomplete_reason_on_the_completion_frame_is_carried() {
        let chunks = decode(&[concat!(
            r#"data: {"type":"response.completed","response":{"#,
            r#""incomplete_details":{"reason":"max_output_tokens"},"#,
            r#""usage":{"input_tokens":10,"output_tokens":4}}}"#,
            "\n\n",
        )])
        .unwrap();
        assert!(
            matches!(
                &chunks[0],
                FrontierChunk::Done { stop_reason: Some(reason), .. }
                    if reason == "max_output_tokens"
            ),
            "{chunks:?}"
        );

        // CONTROL: the ordinary completion names none, so the assertion above is
        // about the field being *read* rather than about a constant.
        assert!(matches!(
            decode(&[COMPLETED]).unwrap()[0],
            FrontierChunk::Done {
                stop_reason: None,
                ..
            }
        ));
    }

    #[test]
    fn an_upstream_that_never_ends_an_event_is_abandoned_rather_than_buffered() {
        // The body is a remote party's. Without the bound this grows until the
        // process dies, which is a denial of service delivered by a provider.
        let mut decoder = SseDecoder::default();
        let filler = "x".repeat(64 * 1024);
        let error = loop {
            match decoder.feed(filler.as_bytes()) {
                Ok(()) => continue,
                Err(error) => break error,
            }
        };
        assert!(error.to_string().contains("no event boundary"), "{error}");

        // CONTROL: a payload comfortably under the bound is decoded, so the
        // limit is about an unterminated stream and not about size.
        let big = "y".repeat(4096);
        let chunks = decode(&[&format!(
            "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{big}\"}}\n\n"
        )])
        .unwrap();
        assert_eq!(chunks, vec![FrontierChunk::OutputText(big)]);
    }
}
