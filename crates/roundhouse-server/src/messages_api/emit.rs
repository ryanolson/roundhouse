// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Turning one turn's log entries into Messages-dialect frames.
//!
//! Pure, and that is the point: the follower that tails the store lives in the
//! parent module, and everything here is a function of the entries it is handed.
//! The Responses surface's equivalent is spread across `concerns`, `emitted` and
//! `project` on a follower that cannot be built without a store and an engine,
//! so its narrowing is only reachable through a socket. This surface has a
//! stricter client — Claude Code *throws* on four distinct ordering mistakes
//! (`research/claude-code-client-surface.md` §3.3) where a Responses client
//! merely drops a frame — so the sequencing is worth asserting directly.
//!
//! **What the client enforces, and how the shape here makes it unreachable.**
//! A `content_block_delta` at an index with no prior `content_block_start`
//! raises `RangeError("Content block not found")`; a `content_block_stop`
//! before any `message_start` raises `Error("Message not found")`; a
//! `text_delta` applied to a `tool_use` block raises `Error("Content block is
//! not a text block")`. Rather than check for those,
//! [`MessageEmission::opening`] is called by *every* arm that emits anything, so
//! the prelude precedes everything by construction, and a block is opened by
//! whatever fills it — so a delta can only ever land on a block of its own kind.
//! There is no ordering for the client to reject. The cost is two fields; the
//! alternative is a family of ordering tests that can only ever sample the
//! orderings someone thought of.
//!
//! **The content model is a real block sequence since M11.2, where M11.1 had one
//! text block at index 0.** That single block was right while the log could only
//! hold one text stream per response; a dispatched turn now commits its own tool
//! calls as items, and a turn that speaks, calls, speaks and calls again is four
//! blocks whose *order* the client resends as history. So blocks are allocated
//! as content arrives and closed before the next one opens.
//!
//! Opening them lazily is a reversal, and the reason is a fork rather than a
//! tidiness: an eagerly opened block 0 puts an empty text block ahead of the
//! call on the ordinary agent turn — a model that is calling a tool usually says
//! nothing first — and the client's resend does not contain one, so the stored
//! history and the claim diverge at that item and the session forks. What the
//! eager open bought is kept where it belongs: a stream that reaches
//! `message_stop` having completed *no* content block is one of the two
//! conditions that make Claude Code re-issue the whole turn non-streaming
//! (§3.6), so [`MessageEmission::stopped`] emits an empty text block for a turn
//! that produced nothing at all.
//!
//! **Usage crosses an axis change here and it is the most dangerous line in the
//! file** — the exact mirror of the note at the top of the dispatch decoder.
//! Anthropic's three input counters are disjoint; roundhouse's
//! [`Usage`](roundhouse_core::event::Usage) nests cached and written input
//! *inside* `input_tokens`. Emitting the stored total as this wire's
//! `input_tokens` would report a 200 000-token cached prompt three times over,
//! and would do it in the direction that flatters the dashboard. See
//! [`wire_usage`].

use axum::response::sse::Event;
use serde_json::Value;

use roundhouse_core::event::{IncompleteReason, SessionEventKind, Usage};
use roundhouse_core::ids::{ResponseId, TurnId};
use roundhouse_core::item::{Item, ItemContent};

use roundhouse_fleet::anthropic_messages::wire::{
    ApiError as WireError, BlockDelta, ContentBlock, Extra, MESSAGE_TYPE, Message,
    MessageDeltaBody, StopReason, StreamEvent, Usage as WireUsage,
};

/// The first content block index, which is the only one a prose turn uses.
///
/// Kept as a name rather than as a literal `0` because the tests that assert the
/// prose shape are asserting *the first block*, not the number zero — and since
/// M11.2 the numbers after it are handed out by
/// [`MessageEmission::open_text_block`] rather than fixed.
pub const FIRST_BLOCK_INDEX: u64 = 0;

/// What `message_start` reports as the output count.
///
/// One, never zero and never the real figure. The real figure is not known when
/// the prelude goes out, and the client's merge takes `output_tokens` with `??`
/// rather than the `> 0` guard it applies to the input counters (§3.4), so the
/// final `message_delta` overwrites whatever is here — as long as this frame is
/// not the *last* one to carry a count. `1` is what the upstream API itself
/// sends in the prelude, and it is the honest floor: a stream that dies before
/// its `message_delta` then bills as one token rather than as a free turn, and
/// a free turn is the one accounting mistake that reads as a saving.
const PRELUDE_OUTPUT_TOKENS: u64 = 1;

/// The role every message this surface emits carries.
const ASSISTANT: &str = "assistant";

// ---------------------------------------------------------------------------
// Error vocabulary
// ---------------------------------------------------------------------------

/// The one mid-stream error type Claude Code retries.
///
/// **A deliberate vocabulary borrow, not a description.** §3.2: an `event:
/// error` builds an `APIError` with `status = undefined`, and the client's
/// retry predicate short-circuits on a missing status unless the serialised
/// body contains the literal `"type":"overloaded_error"` — with the one
/// exception of an API-key (not subscription-OAuth) client whose *initial*
/// response headers set `x-should-retry`. So on this wire "overloaded" is not a
/// claim about upstream load; it is the only spelling of "try this again", and
/// a truthful `api_error` for a transient failure means the agent's loop ends
/// on a fault a retry would have cleared. Plan R5 rules it that way.
pub const OVERLOADED_ERROR: &str = "overloaded_error";
/// A failure this surface does not want retried and cannot name better.
pub const API_ERROR: &str = "api_error";
/// The turn was refused by the control plane, not by an upstream.
pub const PERMISSION_ERROR: &str = "permission_error";
/// The project's budget is spent — a ceiling, not a fault.
pub const RATE_LIMIT_ERROR: &str = "rate_limit_error";

/// How a terminal log event ends this dialect's stream.
///
/// Two shapes, and the split is the same one the Responses surface makes
/// between `response.incomplete` and `response.failed`: a truncated answer is
/// still an answer and ends with the ordinary terminal pair, whereas a turn
/// that was never served has no stop reason to report and ends with an error
/// event. Reporting a refusal as `stop_reason: refusal` would be worse than
/// wrong — that value means *the model* declined, and an operator reading it
/// would go looking at the model instead of at the policy file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    /// `message_delta` carrying this stop reason, then `message_stop`.
    Stopped(StopReason),
    /// An `error` event, and nothing after it.
    Failed {
        kind: &'static str,
        message: &'static str,
    },
}

/// How this dialect ends a turn the log recorded as incomplete.
///
/// Exhaustive by name so a seventh [`IncompleteReason`] is a compile error
/// here: the enum's own doc calls adding a variant a one-way door, and a
/// catch-all at this site would quietly send the seventh out as whatever the
/// default happened to be — which is a *retryability* decision, i.e. the one
/// thing this table is for.
pub fn terminal_for(reason: &IncompleteReason) -> Terminal {
    match reason {
        // The model stopped short. This is the one incomplete reason that is
        // genuinely an answer, and the wire has a value for it.
        IncompleteReason::MaxOutputTokens => Terminal::Stopped(StopReason::MaxTokens),
        // Nothing was dispatched and nothing an agent does changes that. Named
        // `permission_error` rather than `api_error` because the remedy is an
        // operator widening a policy, and the error type is the only part of
        // this a client surfaces prominently.
        IncompleteReason::PolicyRefused => Terminal::Failed {
            kind: PERMISSION_ERROR,
            message: "no target this key may use was admissible for this turn",
        },
        // A ceiling that clears on its own, which is what `rate_limit_error`
        // means to every reader of this wire — but *not* retryable mid-stream,
        // because the window is minutes to hours and an immediate retry is
        // refused again. The distinction the Responses surface draws with two
        // English messages is drawn here with two error types.
        IncompleteReason::BudgetExhausted => Terminal::Failed {
            kind: RATE_LIMIT_ERROR,
            message: "this project's budget is spent and it is configured to refuse rather \
                      than serve locally",
        },
        // Both transient, both worth retrying, and therefore both spelled
        // `overloaded_error` — see that constant for why the honest spelling
        // would end the agent's loop.
        IncompleteReason::UpstreamError => Terminal::Failed {
            kind: OVERLOADED_ERROR,
            message: "the upstream this turn was routed to failed before the answer was \
                      complete",
        },
        IncompleteReason::OwnerLost => Terminal::Failed {
            kind: OVERLOADED_ERROR,
            message: "the process generating this turn lost its lease; the partial answer is \
                      durable and a retry resumes from it",
        },
        // Nobody is reading this frame — the client hung up, which is what the
        // reason means. It is emitted anyway because the alternative is a
        // stream that ends without a terminal frame, and *that* is the shape
        // that costs a second full-price non-streaming turn (§3.6) if the
        // client turns out to still be there.
        IncompleteReason::ClientCancelled => Terminal::Failed {
            kind: API_ERROR,
            message: "this turn was cancelled",
        },
    }
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// One SSE frame: the `event:` name and the payload that goes on `data:`.
///
/// The payload is held as the typed [`StreamEvent`] the *dispatch* client
/// parses, rather than as loose JSON. That is the shared-vocabulary decision
/// paying for itself twice: a frame this surface can build is a frame our own
/// Anthropic client can decode, which
/// `every_frame_this_surface_emits_parses_back_through_the_dispatch_decoder`
/// asserts directly — and it is a standing guard on the chained topology, where
/// roundhouse's output really is another roundhouse's input.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    name: &'static str,
    event: StreamEvent,
}

impl Frame {
    /// The `event:` line.
    ///
    /// Load-bearing rather than decorative on this wire, unlike the Responses
    /// surface where the client reads the type out of the JSON and ignores the
    /// line. Claude Code dispatches on the *name* (§3.2) and silently drops a
    /// frame that has none — the stream then ends with nothing consumed, and
    /// the turn is re-issued non-streaming at full price.
    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn event(&self) -> &StreamEvent {
        &self.event
    }

    /// The payload, as it goes on the wire.
    ///
    /// The `expect` is unreachable and is written out rather than hidden behind
    /// a fallback: [`StreamEvent`] serializes strings, integers, and
    /// `serde_json` values under string keys, none of which can fail. A
    /// fallback would turn an impossible bug into a malformed frame that the
    /// client drops in silence, which is strictly harder to find.
    pub fn data(&self) -> Value {
        serde_json::to_value(&self.event).expect("a StreamEvent has no unserializable shape")
    }

    pub fn into_sse(self) -> Event {
        Event::default()
            .event(self.name)
            .data(self.data().to_string())
    }
}

/// The `event:` name for a payload, so the two can never disagree.
///
/// Both halves of a frame come from one value. The dispatch decoder refuses a
/// frame whose `event:` line and payload `type` disagree — "one of the two is
/// then lying about what frame this is, and picking either would be guessing
/// with the turn's accounting" — and a builder that took the name as an
/// argument is exactly how they come to disagree.
fn frame(event: StreamEvent) -> Frame {
    let name = match &event {
        StreamEvent::MessageStart { .. } => "message_start",
        StreamEvent::ContentBlockStart { .. } => "content_block_start",
        StreamEvent::ContentBlockDelta { .. } => "content_block_delta",
        StreamEvent::ContentBlockStop { .. } => "content_block_stop",
        StreamEvent::MessageDelta { .. } => "message_delta",
        StreamEvent::MessageStop { .. } => "message_stop",
        StreamEvent::Ping { .. } => "ping",
        StreamEvent::Error { .. } => "error",
    };
    Frame { name, event }
}

/// Anthropic's disjoint input counters, from roundhouse's nested ones.
///
/// **The forward half of the inversion the dispatch decoder documents as its
/// most dangerous line, and it is dangerous in the same direction.** There,
/// `input_tokens` had to become `fresh + read + written` or a cached prompt
/// would be under-reported by exactly the amount that was cached. Here the same
/// identity runs backwards: `fresh` is what is left of the stored total once
/// the two components are taken out. Passing `usage.input_tokens` straight
/// through would report the cached tokens twice — once as `input_tokens` and
/// again as `cache_read_input_tokens` — and a client that sums the three, which
/// is exactly what Anthropic's billing semantics tell it to do, would bill a
/// warm turn as if it were nearly two cold ones.
///
/// Saturating rather than checked: the identity holds for every `Usage` the
/// engine writes, and a log entry that violated it (a component larger than the
/// total it belongs to) is corrupt in a way this frame cannot repair. Reporting
/// zero fresh input for such an entry is the reading that under-claims rather
/// than the one that inflates.
///
/// `output_tokens` is an argument rather than read off `usage`, because the
/// prelude reports [`PRELUDE_OUTPUT_TOKENS`] while the terminal reports the
/// measured figure, and both are otherwise the same projection.
fn wire_usage(usage: &Usage, output_tokens: u64) -> WireUsage {
    let read = usage.cached_input_tokens;
    let written = usage.cache_write_tokens;
    WireUsage {
        input_tokens: usage
            .input_tokens
            .saturating_sub(read)
            .saturating_sub(written),
        cache_read_input_tokens: read,
        cache_creation_input_tokens: written,
        output_tokens,
        // The 5m/1h split behind `extended-cache-ttl-2025-04-11`. Roundhouse's
        // `Usage` does not carry the lifetime a write was made under, so the
        // breakdown would have to be invented; absent is the reading that says
        // "not measured" rather than "measured as zero on both".
        cache_creation: None,
        extra: Extra::new(),
    }
}

/// The prelude: the `Message` skeleton and the input-side accounting.
///
/// `model` is what the *client* asked for, not what the router chose. Reporting
/// the route would put roundhouse's routing decisions into the user's
/// transcript, which is the same product decision plan R5 makes when it
/// declines to serve `/v1/models`: the router's choices stay invisible to the
/// agent's user, and the savings dashboard is where they are read. The turn's
/// real target is on the `Routed` log entry, joined by the response id below.
///
/// `id` is roundhouse's own [`ResponseId`], not a synthesized `msg_…`. Nothing
/// in the client parses the shape of this string, and a client's log and this
/// deployment's log naming one response by one id is worth more than cosmetic
/// resemblance to the upstream API.
fn message_start(response_id: &ResponseId, model: &str, admitted: &Usage) -> Frame {
    frame(StreamEvent::MessageStart {
        message: Message {
            kind: MESSAGE_TYPE.to_string(),
            id: Some(response_id.to_string()),
            role: Some(ASSISTANT.to_string()),
            model: Some(model.to_string()),
            // Empty by definition: the content arrives as blocks.
            content: Vec::new(),
            stop_reason: None,
            stop_sequence: None,
            usage: wire_usage(admitted, PRELUDE_OUTPUT_TOKENS),
            extra: Extra::new(),
        },
        extra: Extra::new(),
    })
}

fn content_block_start(index: u64) -> Frame {
    frame(StreamEvent::ContentBlockStart {
        index,
        content_block: ContentBlock::text(""),
        extra: Extra::new(),
    })
}

fn content_block_delta(index: u64, text: &str) -> Frame {
    frame(StreamEvent::ContentBlockDelta {
        index,
        delta: BlockDelta::TextDelta {
            text: text.to_string(),
            extra: Extra::new(),
        },
        extra: Extra::new(),
    })
}

/// A `tool_use` block opening, with the empty `input` the real API sends.
///
/// `input: {}` rather than the arguments, and it is not a placeholder we could
/// helpfully fill: the client's accumulator *overwrites* this field with what it
/// parses from the block's accumulated `input_json_delta` fragments at
/// `content_block_stop`, so arguments put here are discarded. The empty object
/// is what the upstream sends and what a strict reader expects; see
/// [`MessageEmission::tool_block`].
fn tool_block_start(index: u64, call_id: &str, name: &str) -> Frame {
    frame(StreamEvent::ContentBlockStart {
        index,
        content_block: ContentBlock::ToolUse {
            id: call_id.to_string(),
            name: name.to_string(),
            input: Value::Object(serde_json::Map::new()),
            cache_control: None,
            extra: Extra::new(),
        },
        extra: Extra::new(),
    })
}

/// The whole of a tool call's arguments, as the one fragment this block has.
fn tool_block_delta(index: u64, arguments: &str) -> Frame {
    frame(StreamEvent::ContentBlockDelta {
        index,
        delta: BlockDelta::InputJsonDelta {
            partial_json: arguments.to_string(),
            extra: Extra::new(),
        },
        extra: Extra::new(),
    })
}

fn content_block_stop(index: u64) -> Frame {
    frame(StreamEvent::ContentBlockStop {
        index,
        extra: Extra::new(),
    })
}

/// The terminal metadata: why generation stopped, and what it cost.
///
/// **The `usage` object is omitted entirely when there is no output to report,
/// and that is the whole reason this function has a branch.** §3.4: the client
/// merges `output_tokens` with `??`, not with the `> 0` guard it applies to the
/// input counters, so an explicit `"output_tokens": 0` here *overwrites* a
/// non-zero accumulated count and the turn bills as free. Omitting the whole
/// object is the documented-safe path — the merge begins `if(!q) return {...A}`
/// — and it is safer than omitting the one field, because a `usage` object that
/// carries only input counts is a shape nothing on either side has ever had to
/// think about.
///
/// The input counters ride along whenever they are non-zero, which is not
/// duplication of the prelude but a correction of it: the cache split is
/// measured by the dispatch that answers the turn, so at prelude time it is not
/// yet known and goes out as zero. The `> 0` guard makes a later, larger value
/// win and makes a zero a no-op, so this is the one frame in which the number
/// this whole product exists to maximize can reach the client at all.
fn message_delta(stop_reason: StopReason, usage: &Usage) -> Frame {
    frame(StreamEvent::MessageDelta {
        delta: MessageDeltaBody {
            stop_reason: Some(stop_reason),
            ..Default::default()
        },
        usage: (usage.output_tokens > 0).then(|| wire_usage(usage, usage.output_tokens)),
        extra: Extra::new(),
    })
}

fn message_stop() -> Frame {
    frame(StreamEvent::MessageStop {
        extra: Extra::new(),
    })
}

/// A keepalive, as an event with a payload rather than as an SSE comment.
///
/// Both satisfy Claude Code's 300-second byte watchdog, which counts every byte
/// relayed including comment lines (§3.5), and the client skips a `ping` event
/// explicitly (§3.2) so neither costs anything to parse. The tie is broken by
/// the chained topology: NeMo Relay's SSE re-encoder discards frames with no
/// `data:` line, so a bare comment keeps a direct connection alive and lets a
/// chained one die silently at exactly the 300-second mark — the failure that
/// looks like an upstream hang and is not one. One shape that survives both
/// topologies beats two shapes chosen per topology.
pub fn keepalive() -> Frame {
    frame(StreamEvent::Ping {
        extra: Extra::new(),
    })
}

fn error_frame(kind: &str, message: &str) -> Frame {
    frame(StreamEvent::Error {
        error: WireError {
            kind: kind.to_string(),
            message: message.to_string(),
            extra: Extra::new(),
        },
        extra: Extra::new(),
    })
}

// ---------------------------------------------------------------------------
// The non-streaming projection
// ---------------------------------------------------------------------------

/// The complete `Message` for a finished turn, for a `stream: false` request.
///
/// Served genuinely rather than refused, because Claude Code's auth and quota
/// probes are one-token non-streaming creates (§3.6) and a surface that 500s or
/// 422s on them fails before the first turn.
///
/// **`content` is the caller's blocks since M11.2, not a string this function
/// wraps.** It used to take the answer's text and make one text block of it,
/// which was true while a turn had exactly one block and became a silent
/// truncation the moment a turn could also call tools. The non-streaming path
/// reassembles the same blocks the streaming path emitted, and hands them here:
/// two projections of one turn that disagreed about its content would be two
/// answers to a question a client is entitled to ask either way. An empty answer
/// is still one empty text block, for the same reason — that is what the stream
/// sends.
pub fn message_body(
    response_id: &ResponseId,
    model: &str,
    content: Vec<ContentBlock>,
    stop_reason: StopReason,
    usage: &Usage,
) -> Message {
    Message {
        kind: MESSAGE_TYPE.to_string(),
        id: Some(response_id.to_string()),
        role: Some(ASSISTANT.to_string()),
        model: Some(model.to_string()),
        content,
        stop_reason: Some(stop_reason),
        stop_sequence: None,
        usage: wire_usage(usage, usage.output_tokens),
        extra: Extra::new(),
    }
}

// ---------------------------------------------------------------------------
// The streaming projection
// ---------------------------------------------------------------------------

/// What one log entry does to this stream.
///
/// Deliberately without the Responses follower's `bound` on the deduplicated
/// arm: that number is the sequence of the entry the follower read, and this
/// projection never sees a sequence. Keeping cursors out of it is what lets the
/// whole ordering contract be asserted from a list of entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Continue,
    /// This turn was answered before; the follower replays that response's
    /// entries through this same projection.
    Deduplicated {
        response_id: ResponseId,
    },
    /// Terminal. Nothing may follow in either direction.
    End,
}

/// Projects one turn's log entries into this dialect's frames.
///
/// Holds the narrowing as well as the rendering — which entry belongs to this
/// turn at all — where the Responses surface splits the two across `concerns`
/// and `project`. One predicate rather than two for the reason that surface's
/// own `emitted` gives about duplicated narrowings: with the condition written
/// twice, neither copy is load-bearing and a test that deletes one stays green.
pub struct MessageEmission {
    turn_id: TurnId,
    model: String,
    /// What this request admitted, reported in the prelude. The cache split is
    /// not known yet and is corrected by the terminal `message_delta`.
    admitted: Usage,
    response_id: Option<ResponseId>,
    /// Whether the prelude has gone out.
    started: bool,
    /// Whether any text has gone out.
    ///
    /// Not "whether a block is open" — [`Self::open_text`] answers that. This
    /// one decides whether a committed assistant item is *new* text or the same
    /// text a dispatched turn already streamed as deltas, which is the same
    /// distinction the Responses follower's `item_open` draws and the reason an
    /// interjection-seam answer reaches the client at all.
    streamed: bool,
    /// The next content block index to hand out.
    ///
    /// **Real since M11.2, where M11.1 had the constant zero.** A turn that
    /// speaks and then calls two tools is four blocks, and the client's
    /// accumulator keys every frame on this number: a delta at an index with no
    /// prior start is `RangeError("Content block not found")`, which is a thrown
    /// exception rather than a dropped frame, i.e. a lost turn. Monotonic and
    /// never reused, which is the property that makes that impossible here.
    next_index: u64,
    /// The index of the open text block, when one is open.
    ///
    /// Text blocks are opened lazily by their first delta and closed by the next
    /// tool call or by the terminal — a deliberate reversal of M11.1, where the
    /// prelude opened block 0 eagerly. The reason is that an eager empty text
    /// block is a block the *client* then resends as content: on a turn whose
    /// whole answer is a tool call — the ordinary agent turn — the stored
    /// history would hold an empty text item the resend does not, and the
    /// session would fork on the very next turn. The property the eager open
    /// bought (never reaching `message_stop` with no completed block, §3.6,
    /// which costs a second full-price non-streaming turn) is kept by
    /// [`Self::stopped`]'s empty-block fallback instead.
    open_text: Option<u64>,
    /// Whether this turn put a tool call on the wire.
    ///
    /// Read only by [`Self::completion_stop_reason`], and there it is the
    /// stronger evidence: see that function for why the emitted content
    /// out-ranks the provider's own word.
    called_a_tool: bool,
    /// Reported at the terminal instead of what the log booked.
    ///
    /// The interjection seam's substitution: a turn answered at the seam books
    /// the judge's usage, which is not what the turn contributed to the
    /// context. Computing it needs the engine's tokenizer, so the follower
    /// computes it and this holds the answer — see
    /// `Engine::context_contribution` for why the two part company.
    reported: Option<Usage>,
}

impl MessageEmission {
    pub fn new(turn_id: TurnId, model: impl Into<String>, admitted: Usage) -> Self {
        Self {
            turn_id,
            model: model.into(),
            admitted,
            response_id: None,
            started: false,
            streamed: false,
            next_index: 0,
            open_text: None,
            called_a_tool: false,
            reported: None,
        }
    }

    pub fn response_id(&self) -> Option<&ResponseId> {
        self.response_id.as_ref()
    }

    /// Report `usage` at the terminal rather than what the log booked.
    pub fn report_instead(&mut self, usage: Usage) {
        self.reported = Some(usage);
    }

    /// Queue what one log entry becomes on the wire.
    ///
    /// Exhaustive by kind, like the Responses follower's `concerns`, and for
    /// the same reason: an entry is claimed by its identity, and a catch-all
    /// would make a thirteenth event kind silently unclaimed rather than a
    /// compile error at the one site that decides what a client sees.
    pub fn project(&mut self, kind: &SessionEventKind) -> (Vec<Frame>, Step) {
        match kind {
            SessionEventKind::TurnStarted {
                turn_id,
                response_id,
            } if *turn_id == self.turn_id => {
                self.response_id = Some(response_id.clone());
                (self.opening(), Step::Continue)
            }
            SessionEventKind::TurnDeduplicated {
                turn_id,
                response_id,
            } if *turn_id == self.turn_id => {
                // The response id is adopted here as well as handed to the
                // follower, so that a replay whose `TurnStarted` is somehow not
                // reached still claims the response's deltas rather than
                // streaming nothing. No frames: the replay re-reads that
                // response's own entries through this same projection, and
                // announcing the message twice is the one thing the client's
                // accumulator has no defence against.
                self.response_id = Some(response_id.clone());
                (
                    Vec::new(),
                    Step::Deduplicated {
                        response_id: response_id.clone(),
                    },
                )
            }
            SessionEventKind::OutputTextDelta { response_id, text } if self.claims(response_id) => {
                self.streamed = true;
                let mut frames = self.opening();
                let index = self.open_text_block(&mut frames);
                frames.push(content_block_delta(index, text));
                (frames, Step::Continue)
            }
            // Two shapes of committed item, and they are not variations of one
            // thing: a seam answer is text that never passed through a delta,
            // and a tool call is the turn's *other* content channel.
            SessionEventKind::ItemAppended { item } => match self.emitted(item) {
                // One delta carrying the whole thing, because the block model
                // here has no "here is a finished item" frame and does not need
                // one: a client assembles text from deltas either way, and a
                // single large delta is a shape the accumulator already handles.
                Some(Emitted::SeamText(text)) => {
                    let text = text.to_string();
                    self.streamed = true;
                    let mut frames = self.opening();
                    let index = self.open_text_block(&mut frames);
                    frames.push(content_block_delta(index, &text));
                    (frames, Step::Continue)
                }
                Some(Emitted::ToolCall {
                    call_id,
                    name,
                    arguments,
                }) => {
                    let (call_id, name, arguments) =
                        (call_id.to_string(), name.to_string(), arguments.to_string());
                    let mut frames = self.opening();
                    self.tool_block(&mut frames, &call_id, &name, &arguments);
                    (frames, Step::Continue)
                }
                None => (Vec::new(), Step::Continue),
            },
            SessionEventKind::ResponseCompleted {
                response_id,
                usage,
                stop_reason,
                ..
            } if self.claims(response_id) => {
                let reason = self.completion_stop_reason(stop_reason.as_deref());
                (self.stopped(reason, usage), Step::End)
            }
            SessionEventKind::ResponseIncomplete {
                response_id,
                reason,
                usage,
                ..
            } if self.claims(response_id) => {
                let frames = match terminal_for(reason) {
                    Terminal::Stopped(stop_reason) => self.stopped(stop_reason, usage),
                    Terminal::Failed { kind, message } => self.failed(kind, message),
                };
                (frames, Step::End)
            }
            // Everything else in the session's window, named rather than
            // defaulted. The validate loop's three kinds and the routing
            // decision belong to no response a client asked for: a side call is
            // money nobody requested and a verdict is a decision, and neither
            // is an answer to the turn being streamed.
            SessionEventKind::TurnStarted { .. }
            | SessionEventKind::TurnDeduplicated { .. }
            | SessionEventKind::OutputTextDelta { .. }
            | SessionEventKind::ResponseCompleted { .. }
            | SessionEventKind::ResponseIncomplete { .. }
            | SessionEventKind::SessionCreated { .. }
            | SessionEventKind::Routed { .. }
            | SessionEventKind::SideCallCompleted { .. }
            | SessionEventKind::SideCallAbandoned { .. }
            | SessionEventKind::ValidationDecided { .. }
            | SessionEventKind::Error { .. } => (Vec::new(), Step::Continue),
        }
    }

    /// End a stream whose turn is gone but whose response never terminated.
    ///
    /// Reachable directly because the follower calls it on that path, and
    /// because a refusal raised after the headers are out has nowhere else to
    /// go: a status code is no longer expressible.
    pub fn failed(&mut self, kind: &'static str, message: &str) -> Vec<Frame> {
        // No prelude is synthesized here, unlike every other arm. An error
        // event is legal on its own — the client throws at the SSE layer before
        // the accumulator sees anything (§3.2) — whereas a `message_start` for
        // a turn that produced nothing would be a message id naming a response
        // the log may not contain, which is the same thing the Responses
        // surface's `failed_frame` declines to invent.
        let mut frames = Vec::new();
        self.close_text_block(&mut frames);
        frames.push(error_frame(kind, message));
        frames
    }

    /// The prelude, at most once.
    ///
    /// Called by every arm that emits anything, which is what makes
    /// "`message_start` first" true by construction rather than by ordering
    /// discipline — a `content_block_stop` before one is `Error("Message not
    /// found")` in the client's accumulator (§3.3), and a throw mid-stream is a
    /// lost turn. The content block that used to ride along here is opened by
    /// whatever fills it instead; see [`Self::open_text`].
    fn opening(&mut self) -> Vec<Frame> {
        if self.started {
            return Vec::new();
        }
        let Some(response_id) = self.response_id.clone() else {
            // Nothing claims a frame before a response is named, so this is
            // unreachable from `project`. Returning empty rather than panicking
            // keeps it that way for a future caller too.
            return Vec::new();
        };
        self.started = true;
        vec![message_start(&response_id, &self.model, &self.admitted)]
    }

    /// The index of a text block that is open and accepting deltas, opening one
    /// if the last thing on the wire was a tool call or nothing at all.
    ///
    /// The allocator's read half. A `text_delta` aimed at a `tool_use` block is
    /// `Error("Content block is not a text block")` in the client's accumulator
    /// — the third of its three block-type throws — so text after a call has to
    /// be a *new* block rather than a resumption of the one before it.
    fn open_text_block(&mut self, frames: &mut Vec<Frame>) -> u64 {
        if let Some(index) = self.open_text {
            return index;
        }
        let index = self.next_index;
        self.next_index += 1;
        self.open_text = Some(index);
        frames.push(content_block_start(index));
        index
    }

    /// Close the open text block, if one is open.
    fn close_text_block(&mut self, frames: &mut Vec<Frame>) {
        if let Some(index) = self.open_text.take() {
            frames.push(content_block_stop(index));
        }
    }

    /// One completed tool call, as its own block.
    ///
    /// **Start, one `input_json_delta`, stop — never a start whose
    /// `content_block` already carries the arguments.** The client's accumulator
    /// applies `input_json_delta` fragments to the *partial JSON* it keeps per
    /// block and parses the result at `content_block_stop`; the `input` on a
    /// `tool_use` start frame is `{}` on the real API and is the field the
    /// accumulator overwrites. Emitting the arguments only on the start frame
    /// would therefore hand a strict reader a complete call and hand Claude Code
    /// an empty one — the failure that looks like a model calling tools with no
    /// arguments.
    ///
    /// One delta rather than fragments because the log holds the call whole: the
    /// dispatch decoder already reassembled it, and re-splitting it here would
    /// invent boundaries no upstream chose. A client that reassembles fragments
    /// handles one fragment as a special case of many.
    ///
    /// The block is closed immediately. A tool call is complete when it is
    /// committed — that is what `FrontierChunk::ToolCall` means — so there is
    /// nothing further that could arrive for this index.
    fn tool_block(&mut self, frames: &mut Vec<Frame>, call_id: &str, name: &str, arguments: &str) {
        self.close_text_block(frames);
        let index = self.next_index;
        self.next_index += 1;
        self.called_a_tool = true;
        frames.push(tool_block_start(index, call_id, name));
        frames.push(tool_block_delta(index, arguments));
        frames.push(content_block_stop(index));
    }

    /// The ordinary terminal: close the block, report, stop.
    fn stopped(&mut self, stop_reason: StopReason, usage: &Usage) -> Vec<Frame> {
        let mut frames = self.opening();
        if self.next_index == 0 {
            // **The empty-block fallback, and it is the whole reason blocks are
            // lazy rather than free.** A stream that reaches `message_stop`
            // having completed no content block is one of the two conditions
            // that make Claude Code re-issue the entire turn non-streaming
            // (§3.6), at full price. A turn that produced nothing — a refusal
            // the seam answered with silence, an upstream that streamed no
            // deltas — says so with an empty text block for free. Reached only
            // when *nothing* was emitted, so a tool-only turn does not get one.
            self.open_text_block(&mut frames);
        }
        self.close_text_block(&mut frames);
        // `unwrap_or` and not `expect`, for the reason the Responses follower
        // gives at the same seam: the emission and the completion land in one
        // append batch, so a completion with no emission before it is an
        // ordinary dispatched turn rather than an ordering bug.
        frames.push(message_delta(
            stop_reason,
            self.reported.as_ref().unwrap_or(usage),
        ));
        frames.push(message_stop());
        frames
    }

    /// Whether an entry naming this response belongs to this stream.
    fn claims(&self, response_id: &ResponseId) -> bool {
        self.response_id.as_ref() == Some(response_id)
    }

    /// What a committed item becomes on this wire, if anything.
    ///
    /// The provenance stamp is the first condition for both shapes, and it is
    /// what rules out the client's own resent history: canonicalization sets no
    /// stamp on anything a client sends, so a client cannot forge an answer or a
    /// call into this stream. A replay re-reads the whole log, so every item the
    /// session ever held passes through here.
    ///
    /// Beyond the stamp the two shapes ask opposite questions. Text is claimed
    /// only when it was *not* streamed — a dispatched turn puts its answer out
    /// as deltas and then commits the same text as an item, and claiming it here
    /// too would deliver the answer twice; what survives that filter is the
    /// interjection seam's answer, committed whole and never streamed. A tool
    /// call is claimed unconditionally, because a call has no delta path at all:
    /// the item *is* how it reaches this projection.
    ///
    /// The remaining variants are listed by name rather than caught by a
    /// wildcard because the default a wildcard would pick is the unsafe one —
    /// the argument `Item::spoken_text` makes for itself. A `ToolResult` is the
    /// client's own work coming back and is never something this deployment
    /// emitted; thinking and opaque blocks are stored so a resend round-trips,
    /// and re-emitting one would put a block in an answer the model did not
    /// produce this turn.
    ///
    /// `pub` because the follower asks it too, *before* projecting: a
    /// [`Emitted::SeamText`] is a turn answered at the interjection seam, whose
    /// reported usage is the engine's `context_contribution` rather than what
    /// the log booked, and only the follower holds an engine to ask. One
    /// function called twice rather than the condition written twice — the
    /// hazard the Responses follower's `emitted` doc names is a *duplicated*
    /// narrowing, where neither copy is load-bearing and deleting one keeps the
    /// suite green. Here there is one copy and two callers, so a change to it
    /// moves both answers together.
    pub fn emitted<'a>(&self, item: &'a Item) -> Option<Emitted<'a>> {
        let response_id = item.response_id.as_ref()?;
        if !self.claims(response_id) {
            return None;
        }
        match &item.content {
            ItemContent::Text { text } if !self.streamed && !text.is_empty() => {
                Some(Emitted::SeamText(text))
            }
            ItemContent::ToolCall {
                call_id,
                name,
                arguments,
            } => Some(Emitted::ToolCall {
                call_id,
                name,
                arguments,
            }),
            ItemContent::Text { .. }
            | ItemContent::ToolResult { .. }
            | ItemContent::Thinking { .. }
            | ItemContent::RedactedThinking { .. }
            | ItemContent::Opaque { .. } => None,
        }
    }

    /// Why a *completed* turn stopped, in this dialect's closed vocabulary.
    ///
    /// **Two sources, and the emitted content wins.** A message whose content
    /// carries `tool_use` blocks and whose `stop_reason` is anything but
    /// `tool_use` is a shape the real API never produces and a shape an agent's
    /// loop does not act on — it reads the reason to decide whether the turn is
    /// waiting on it or over. The provider's own word cannot be trusted to
    /// supply it, because roundhouse routes across dialects: a Claude Code turn
    /// answered by an OpenAI model arrives through a wire that has no such word
    /// at all, and forwarding its silence would hand the client a toolbox
    /// request labelled "finished". So a turn that emitted a call says so.
    ///
    /// Otherwise the provider's word is *translated*, never forwarded: this
    /// dialect's `stop_reason` is a closed set of seven, the neutral string is
    /// whatever the answering wire spells, and an unrecognised value here is a
    /// value the strict oracle — and a strict client — refuses.
    ///
    /// An unknown or absent reason becomes `end_turn`, which is the least-wrong
    /// of the available lies: the turn did complete, the wire has no "the
    /// provider said something we do not know" value, and inventing one is
    /// exactly what makes a parser reject a whole message. The honest record of
    /// what the provider actually said is in the log, on the terminal event.
    fn completion_stop_reason(&self, provider_word: Option<&str>) -> StopReason {
        if self.called_a_tool {
            return StopReason::ToolUse;
        }
        match provider_word {
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            // The Responses wire's spelling of the same fact, from
            // `incomplete_details.reason`. Translated rather than dropped
            // because this is M11.1's F1 reporting half arriving through the
            // other dialect: a turn cut off at the ceiling must not read as one
            // that finished.
            Some("max_output_tokens") => StopReason::MaxTokens,
            Some("stop_sequence") => StopReason::StopSequence,
            Some("refusal") => StopReason::Refusal,
            Some("pause_turn") => StopReason::PauseTurn,
            Some("model_context_window_exceeded") => StopReason::ModelContextWindowExceeded,
            Some(_) | None => StopReason::EndTurn,
        }
    }
}

/// What a committed item is, on this wire.
///
/// Two shapes rather than an `Option<&str>` because a tool call is not text with
/// a different tag: it opens its own block, it carries three fields, and it is
/// the one thing here that a *dispatched* turn commits. Keeping them one type is
/// what makes [`MessageEmission::emitted`] the single narrowing that both the
/// follower and [`MessageEmission::project`] read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Emitted<'a> {
    /// An answer committed whole, never streamed: the interjection seam's.
    SeamText(&'a str),
    /// A tool call this turn produced, for the client to run.
    ToolCall {
        call_id: &'a str,
        name: &'a str,
        arguments: &'a str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    use roundhouse_core::event::Accounting;
    use roundhouse_core::item::Role;
    use serde_json::json;

    fn turn() -> TurnId {
        TurnId::new("turn_1")
    }

    fn response() -> ResponseId {
        ResponseId::new("resp_1")
    }

    fn emission() -> MessageEmission {
        MessageEmission::new(
            turn(),
            "claude-sonnet-4-5",
            Usage {
                input_tokens: 900,
                ..Default::default()
            },
        )
    }

    fn started() -> MessageEmission {
        let mut emission = emission();
        emission.project(&SessionEventKind::TurnStarted {
            turn_id: turn(),
            response_id: response(),
        });
        emission
    }

    fn names(frames: &[Frame]) -> Vec<&'static str> {
        frames.iter().map(Frame::name).collect()
    }

    /// A whole successful turn, in the order the client's parser demands.
    ///
    /// The sequence is the contract: `message_start` first, exactly one
    /// start/stop pair around the deltas at one index, `message_delta` then
    /// `message_stop` last. §3.3 turns three of those into thrown exceptions
    /// rather than into dropped frames, so this is the test the whole module's
    /// shape exists to make true.
    #[test]
    fn a_dispatched_turn_streams_the_sequence_the_client_enforces() {
        let mut emission = emission();
        let mut frames = Vec::new();
        let mut steps = Vec::new();
        for kind in [
            SessionEventKind::TurnStarted {
                turn_id: turn(),
                response_id: response(),
            },
            SessionEventKind::OutputTextDelta {
                response_id: response(),
                text: "he".into(),
            },
            SessionEventKind::OutputTextDelta {
                response_id: response(),
                text: "llo".into(),
            },
            SessionEventKind::ResponseCompleted {
                response_id: response(),
                usage: Usage {
                    input_tokens: 900,
                    output_tokens: 12,
                    ..Default::default()
                },
                provider_reported_cost_usd: None,
                stop_reason: None,
            },
        ] {
            let (batch, step) = emission.project(&kind);
            frames.extend(batch);
            steps.push(step);
        }

        assert_eq!(
            names(&frames),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(
            steps.last(),
            Some(&Step::End),
            "the completion is terminal and nothing may follow it"
        );

        // Index discipline: every block event names the one block.
        for frame in &frames {
            let index = match frame.event() {
                StreamEvent::ContentBlockStart { index, .. }
                | StreamEvent::ContentBlockDelta { index, .. }
                | StreamEvent::ContentBlockStop { index, .. } => Some(*index),
                _ => None,
            };
            if let Some(index) = index {
                assert_eq!(index, FIRST_BLOCK_INDEX);
            }
        }
    }

    /// **Nothing is emitted before `message_start`, whatever arrives first.**
    ///
    /// The client raises `RangeError("Content block not found")` on a delta at
    /// an unopened index and `Error("Message not found")` on a stop before any
    /// message, and a throw mid-stream loses the turn. Every entry that can
    /// carry text is fed to a fresh emission with the `TurnStarted` withheld,
    /// and each must still open the stream properly.
    #[test]
    fn every_frame_emitting_entry_opens_the_stream_first() {
        let delta = SessionEventKind::OutputTextDelta {
            response_id: response(),
            text: "hi".into(),
        };
        let committed = SessionEventKind::ItemAppended {
            item: Item::assistant_text("guidance", response()),
        };
        let completed = SessionEventKind::ResponseCompleted {
            response_id: response(),
            usage: Usage::default(),
            provider_reported_cost_usd: None,
            stop_reason: None,
        };

        for first in [delta, committed, completed] {
            // The dedup path is the only way a response is named without a
            // `TurnStarted` for this turn having been projected.
            let mut emission = emission();
            emission.project(&SessionEventKind::TurnDeduplicated {
                turn_id: turn(),
                response_id: response(),
            });
            let (frames, _) = emission.project(&first);
            assert_eq!(
                names(&frames)[..2],
                ["message_start", "content_block_start"],
                "an entry that emits anything must open the stream first: {frames:#?}"
            );
        }
    }

    /// **A `message_delta` never writes an explicit zero output count.**
    ///
    /// §3.4: the client merges `output_tokens` with `??`, so `0` on the wire
    /// overwrites a non-zero accumulated value and the turn bills as free — the
    /// one accounting error that reads as a saving. The control is the
    /// non-zero case, which must carry the count: a builder that omitted
    /// `usage` unconditionally would satisfy the claim above and report
    /// nothing at all.
    #[test]
    fn the_terminal_delta_omits_usage_rather_than_reporting_a_zero_output() {
        let silent = message_delta(StopReason::EndTurn, &Usage::default());
        assert_eq!(
            silent.data()["usage"],
            Value::Null,
            "a zero output count must not reach the wire in any form"
        );

        let spoken = message_delta(
            StopReason::EndTurn,
            &Usage {
                output_tokens: 12,
                ..Default::default()
            },
        );
        assert_eq!(spoken.data()["usage"]["output_tokens"], json!(12));
    }

    /// **The input axes are converted, not forwarded.**
    ///
    /// Roundhouse nests cached and written input inside `input_tokens`;
    /// Anthropic's three counters are disjoint. The fixture is the one the
    /// dispatch decoder's own tests use — a 9 512-token prompt of which 9 000
    /// were read from cache and 500 written — so the two directions are pinned
    /// against one arithmetic. Forwarding `input_tokens` unconverted would
    /// report 9 512 + 9 000 + 500 to a client that sums them.
    #[test]
    fn the_prelude_reports_anthropics_disjoint_input_counters() {
        let usage = Usage {
            input_tokens: 9_512,
            cached_input_tokens: 9_000,
            cache_write_tokens: 500,
            output_tokens: 64,
            reasoning_tokens: 0,
            accounting: Accounting::Reported,
        };
        let data = message_start(&response(), "claude-opus-4-5", &usage).data();
        let reported = &data["message"]["usage"];
        assert_eq!(reported["input_tokens"], json!(12));
        assert_eq!(reported["cache_read_input_tokens"], json!(9_000));
        assert_eq!(reported["cache_creation_input_tokens"], json!(500));
        assert_eq!(
            reported["input_tokens"].as_u64().unwrap()
                + reported["cache_read_input_tokens"].as_u64().unwrap()
                + reported["cache_creation_input_tokens"].as_u64().unwrap(),
            usage.input_tokens,
            "the three disjoint counters must sum back to the stored total"
        );
        assert_eq!(
            reported["output_tokens"],
            json!(PRELUDE_OUTPUT_TOKENS),
            "the prelude must never carry the real output count"
        );
        assert_ne!(reported["output_tokens"], json!(0));

        // A log entry that violates the nesting invariant under-claims rather
        // than wrapping: `saturating_sub` is the direction that cannot inflate.
        let corrupt = Usage {
            input_tokens: 10,
            cached_input_tokens: 40,
            ..Default::default()
        };
        assert_eq!(
            message_start(&response(), "m", &corrupt).data()["message"]["usage"]["input_tokens"],
            json!(0)
        );
    }

    /// The prelude names the model the client asked for.
    #[test]
    fn the_prelude_echoes_the_requested_model_and_the_roundhouse_response_id() {
        let data = message_start(&response(), "claude-sonnet-4-5", &Usage::default()).data();
        assert_eq!(data["message"]["model"], json!("claude-sonnet-4-5"));
        assert_eq!(data["message"]["id"], json!("resp_1"));
        assert_eq!(data["message"]["role"], json!("assistant"));
        assert_eq!(
            data["message"]["content"],
            json!([]),
            "the prelude's content is empty by definition"
        );
    }

    /// Each frame's `event:` line and its payload `type` are one value.
    ///
    /// The dispatch decoder refuses a frame whose two names disagree, and
    /// Claude Code dispatches on the line while ignoring the payload's type —
    /// so a disagreement is a frame that is either refused or silently
    /// mis-routed depending on who reads it.
    #[test]
    fn every_frames_event_name_matches_its_payload_type() {
        let mut emission = emission();
        let (mut frames, _) = emission.project(&SessionEventKind::TurnStarted {
            turn_id: turn(),
            response_id: response(),
        });
        frames.extend([
            content_block_delta(FIRST_BLOCK_INDEX, "hi"),
            tool_block_start(1, "toolu_01", "Grep"),
            tool_block_delta(1, r#"{"pattern":"fn main"}"#),
            keepalive(),
            error_frame(OVERLOADED_ERROR, "try again"),
        ]);
        frames.extend(emission.stopped(StopReason::MaxTokens, &Usage::default()));
        assert!(
            frames.iter().any(|frame| frame.name() == "message_start"),
            "the fixture must cover every builder, the prelude included"
        );

        for frame in frames {
            let data = frame.data();
            assert_eq!(
                data["type"],
                json!(frame.name()),
                "the SSE name and the payload type disagree: {data}"
            );
        }
    }

    /// **What this surface emits, our own Anthropic client can read.**
    ///
    /// The shared-vocabulary claim, asserted rather than assumed. It is not
    /// only tidiness: the chained topology puts a roundhouse in front of a
    /// roundhouse, so this really is a round trip somebody runs.
    #[test]
    fn every_frame_this_surface_emits_parses_back_through_the_dispatch_decoder() {
        let mut emission = emission();
        let mut frames = Vec::new();
        for kind in [
            SessionEventKind::TurnStarted {
                turn_id: turn(),
                response_id: response(),
            },
            SessionEventKind::OutputTextDelta {
                response_id: response(),
                text: "hello".into(),
            },
            SessionEventKind::ResponseCompleted {
                response_id: response(),
                usage: Usage {
                    input_tokens: 900,
                    cached_input_tokens: 800,
                    output_tokens: 12,
                    ..Default::default()
                },
                provider_reported_cost_usd: None,
                stop_reason: None,
            },
        ] {
            let (batch, _) = emission.project(&kind);
            frames.extend(batch);
        }
        frames.push(keepalive());
        frames.push(error_frame(OVERLOADED_ERROR, "overloaded"));

        for frame in frames {
            let data = frame.data();
            let decoded: StreamEvent = serde_json::from_value(data.clone())
                .unwrap_or_else(|error| panic!("our own client cannot read {data}: {error}"));
            assert_eq!(&decoded, frame.event());
        }
    }

    /// Every incomplete reason ends the stream, and the retryable ones say so.
    ///
    /// The table is asserted as a whole rather than reason by reason because
    /// the property that matters is a partition: exactly the two transient
    /// reasons spell `overloaded_error`, which §3.2 makes the only mid-stream
    /// wording a subscription-OAuth client retries. A refusal wearing that
    /// spelling would loop the agent forever on a policy nobody is going to
    /// change; a transient fault without it ends the loop on a fault a retry
    /// would clear.
    #[test]
    fn only_the_transient_incomplete_reasons_are_spelled_retryable() {
        let retryable = [IncompleteReason::UpstreamError, IncompleteReason::OwnerLost];
        let terminal = [
            IncompleteReason::PolicyRefused,
            IncompleteReason::BudgetExhausted,
            IncompleteReason::ClientCancelled,
        ];

        // The messages are not restated here. Copying them would only assert
        // that a string equals itself, and the property under test is the
        // partition, not the prose.
        for reason in &retryable {
            match terminal_for(reason) {
                Terminal::Failed { kind, message } => {
                    assert_eq!(
                        kind, OVERLOADED_ERROR,
                        "{reason:?} is transient and must be spelled retryable"
                    );
                    assert!(!message.is_empty(), "{reason:?} must say what happened");
                }
                other => panic!("{reason:?} is not a stop reason: {other:?}"),
            }
        }
        for reason in &terminal {
            match terminal_for(reason) {
                Terminal::Failed { kind, .. } => assert_ne!(
                    kind, OVERLOADED_ERROR,
                    "{reason:?} must not be spelled as retryable"
                ),
                other => panic!("{reason:?} is not an answer: {other:?}"),
            }
        }
        // And the four error types are four, not one wearing four names: a
        // table that collapsed to `api_error` would satisfy the partition above
        // while telling every reader the same nothing.
        assert_eq!(
            [
                OVERLOADED_ERROR,
                API_ERROR,
                PERMISSION_ERROR,
                RATE_LIMIT_ERROR
            ]
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
            4
        );
        assert_eq!(
            terminal_for(&IncompleteReason::MaxOutputTokens),
            Terminal::Stopped(StopReason::MaxTokens),
            "a truncated answer is still an answer and ends with the ordinary pair"
        );
    }

    /// A refusal ends the stream with an error event, not with a stop reason.
    ///
    /// **And the block is closed only if one is open.** Since M11.2 blocks are
    /// opened by their content rather than by the prelude, so a refusal that
    /// arrives before any text has no block to close — and a
    /// `content_block_stop` at an index the client never saw started is the
    /// `RangeError` its accumulator throws on. Both shapes are asserted here
    /// because a `failed` that always emitted a stop would pass the first and a
    /// `failed` that never did would pass the second.
    #[test]
    fn a_refused_turn_ends_with_an_error_event_after_its_block_is_closed() {
        let refusal = SessionEventKind::ResponseIncomplete {
            response_id: response(),
            reason: IncompleteReason::PolicyRefused,
            usage: Usage::default(),
            terminal_attempt: None,
        };

        let mut silent = started();
        let (frames, step) = silent.project(&refusal);
        assert_eq!(step, Step::End);
        assert_eq!(
            names(&frames),
            ["error"],
            "no block was opened, so there is none to close"
        );

        let mut emission = started();
        emission.project(&SessionEventKind::OutputTextDelta {
            response_id: response(),
            text: "here is what I foun".into(),
        });
        let (frames, step) = emission.project(&refusal);
        assert_eq!(step, Step::End);
        assert_eq!(names(&frames), ["content_block_stop", "error"]);
        let body = frames[1].data();
        assert_eq!(body["error"]["type"], json!(PERMISSION_ERROR));
        assert_ne!(
            body["error"]["type"],
            json!(OVERLOADED_ERROR),
            "a policy refusal retried forever is an agent stuck forever"
        );
    }

    /// A turn that dies before it is named emits an error alone.
    #[test]
    fn a_turn_that_never_started_fails_without_inventing_a_message() {
        let mut emission = emission();
        let frames = emission.failed(API_ERROR, "the turn ended without terminating its response");
        assert_eq!(names(&frames), ["error"]);
        assert_eq!(emission.response_id(), None);
    }

    /// A seam answer reaches the client; a dispatched turn's own commit does
    /// not arrive twice.
    ///
    /// Both halves in one test because either alone is satisfiable by the wrong
    /// implementation: always projecting a committed item duplicates every
    /// dispatched answer, and never projecting one leaves a halted turn
    /// streaming nothing at all — which is what the Responses surface shipped
    /// before its own `emitted` arm existed.
    #[test]
    fn a_committed_answer_is_streamed_only_when_no_delta_carried_it() {
        let mut seam = started();
        let (frames, step) = seam.project(&SessionEventKind::ItemAppended {
            item: Item::assistant_text("let me redirect you", response()),
        });
        assert_eq!(step, Step::Continue);
        assert_eq!(
            names(&frames),
            ["content_block_start", "content_block_delta"]
        );
        assert_eq!(
            frames[1].data()["delta"]["text"],
            json!("let me redirect you")
        );

        let mut dispatched = started();
        dispatched.project(&SessionEventKind::OutputTextDelta {
            response_id: response(),
            text: "the answer".into(),
        });
        let (frames, _) = dispatched.project(&SessionEventKind::ItemAppended {
            item: Item::assistant_text("the answer", response()),
        });
        assert!(
            frames.is_empty(),
            "the streamed answer must not arrive a second time as a commit"
        );
    }

    /// A client's own resent history is never projected as an answer.
    ///
    /// Canonicalization stamps nothing, so an unstamped item is by construction
    /// something the client sent — including the thinking blocks M11.1 added,
    /// which are assistant-role text-bearing items and the shape most likely to
    /// be mistaken for output.
    #[test]
    fn the_clients_own_items_are_never_projected() {
        let mut emission = started();
        for item in [
            Item::user_text("what is the answer"),
            Item {
                role: Role::Assistant,
                content: ItemContent::Text {
                    text: "an answer the client resent".into(),
                },
                response_id: None,
            },
            Item {
                role: Role::Assistant,
                content: ItemContent::Thinking {
                    thinking: "the user probably wants X".into(),
                    signature: "sig".into(),
                },
                response_id: Some(response()),
            },
            Item {
                role: Role::Assistant,
                content: ItemContent::Opaque {
                    block_type: "image".into(),
                    block: json!({ "type": "image" }),
                },
                response_id: Some(response()),
            },
        ] {
            let (frames, step) = emission.project(&SessionEventKind::ItemAppended { item });
            assert!(frames.is_empty(), "projected something it does not own");
            assert_eq!(step, Step::Continue);
        }
    }

    /// Entries belonging to another turn or another response are not claimed.
    #[test]
    fn entries_from_other_turns_and_other_responses_are_not_claimed() {
        let mut emission = emission();
        let (frames, step) = emission.project(&SessionEventKind::TurnStarted {
            turn_id: TurnId::new("turn_other"),
            response_id: ResponseId::new("resp_other"),
        });
        assert!(frames.is_empty());
        assert_eq!(step, Step::Continue);
        assert_eq!(emission.response_id(), None);

        let mut emission = started();
        let (frames, _) = emission.project(&SessionEventKind::OutputTextDelta {
            response_id: ResponseId::new("resp_other"),
            text: "another turn's answer".into(),
        });
        assert!(
            frames.is_empty(),
            "another response's text reached the wire"
        );
    }

    /// A deduplicated turn yields the replay signal and no frames.
    #[test]
    fn a_deduplicated_turn_announces_the_replay_without_emitting_a_message() {
        let mut emission = emission();
        let (frames, step) = emission.project(&SessionEventKind::TurnDeduplicated {
            turn_id: turn(),
            response_id: response(),
        });
        assert!(
            frames.is_empty(),
            "the replay emits the message; announcing it twice has no recovery"
        );
        assert_eq!(
            step,
            Step::Deduplicated {
                response_id: response()
            }
        );
    }

    /// A turn that produced no text still completes one content block.
    ///
    /// "Stream completed with `message_start` but no content blocks completed"
    /// is one of the two conditions that make Claude Code re-issue the entire
    /// turn non-streaming (§3.6) — one malformed stream, one extra full-price
    /// answer. The terminal's empty-block fallback is what buys this now that
    /// the prelude no longer opens a block eagerly.
    #[test]
    fn an_empty_answer_still_completes_a_content_block() {
        let mut emission = started();
        let (frames, _) = emission.project(&SessionEventKind::ResponseCompleted {
            response_id: response(),
            usage: Usage::default(),
            provider_reported_cost_usd: None,
            stop_reason: None,
        });
        assert_eq!(
            names(&frames),
            [
                "content_block_start",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(
            frames[0].data()["content_block"],
            json!({ "type": "text", "text": "" }),
            "the block that stands in for silence is an empty text block"
        );
    }

    /// **A tool-only turn gets no empty text block, and that is the fork this
    /// milestone's lazy blocks exist to avoid.**
    ///
    /// The eager block-0 open of M11.1 would put `{"type":"text","text":""}`
    /// ahead of the call in every agent turn — the ordinary shape, since a model
    /// that is calling a tool usually says nothing first. The client resends the
    /// content it was given; if it drops that empty block (and nothing in the
    /// API's own traffic ever contains one) the resent history no longer matches
    /// the stored items, and the session forks on its very next turn while every
    /// turn still answers. So: exactly one block, and it is the call.
    #[test]
    fn a_turn_that_only_calls_a_tool_emits_no_text_block() {
        let mut emission = started();
        let (frames, step) = emission.project(&SessionEventKind::ItemAppended {
            item: Item {
                response_id: Some(response()),
                ..Item::tool_call("toolu_01", "Grep", r#"{"pattern":"fn main"}"#)
            },
        });
        assert_eq!(step, Step::Continue);
        assert_eq!(
            names(&frames),
            [
                "content_block_start",
                "content_block_delta",
                "content_block_stop"
            ]
        );
        assert_eq!(frames[0].data()["index"], json!(0));
        assert_eq!(
            frames[0].data()["content_block"],
            json!({ "type": "tool_use", "id": "toolu_01", "name": "Grep", "input": {} }),
            "the start frame carries an empty input; the client overwrites it \
             from the fragments"
        );
        assert_eq!(
            frames[1].data()["delta"],
            json!({ "type": "input_json_delta", "partial_json": r#"{"pattern":"fn main"}"# })
        );

        let (terminal, step) = emission.project(&SessionEventKind::ResponseCompleted {
            response_id: response(),
            usage: Usage {
                output_tokens: 9,
                ..Default::default()
            },
            provider_reported_cost_usd: None,
            stop_reason: Some("tool_use".into()),
        });
        assert_eq!(step, Step::End);
        assert_eq!(
            names(&terminal),
            ["message_delta", "message_stop"],
            "a turn that completed a block gets no stand-in empty one"
        );
        assert_eq!(
            terminal[0].data()["delta"]["stop_reason"],
            json!("tool_use")
        );
    }

    /// **Text and calls interleave by index, in the order the log holds them.**
    ///
    /// The whole content model of M11.2 in one sequence: text opens block 0, the
    /// call closes it and takes block 1, and the text after the call is a *new*
    /// block rather than a resumption — a `text_delta` aimed at a `tool_use`
    /// block is the third of the client accumulator's block-type throws, and a
    /// throw mid-stream is a lost turn.
    #[test]
    fn text_and_tool_calls_interleave_as_separate_indexed_blocks() {
        let mut emission = started();
        let mut frames = Vec::new();
        for kind in [
            SessionEventKind::OutputTextDelta {
                response_id: response(),
                text: "let me look".into(),
            },
            SessionEventKind::ItemAppended {
                item: Item {
                    response_id: Some(response()),
                    ..Item::tool_call("toolu_01", "Grep", r#"{"q":"x"}"#)
                },
            },
            SessionEventKind::OutputTextDelta {
                response_id: response(),
                text: " and also".into(),
            },
            SessionEventKind::ItemAppended {
                item: Item {
                    response_id: Some(response()),
                    ..Item::tool_call("toolu_02", "Read", r#"{"path":"/a"}"#)
                },
            },
            SessionEventKind::ResponseCompleted {
                response_id: response(),
                usage: Usage::default(),
                provider_reported_cost_usd: None,
                stop_reason: Some("tool_use".into()),
            },
        ] {
            let (batch, _) = emission.project(&kind);
            frames.extend(batch);
        }

        // Every block event, as (name, index) — the shape the client's
        // accumulator is a state machine over.
        let indexed: Vec<(&str, u64)> = frames
            .iter()
            .filter_map(|frame| match frame.event() {
                StreamEvent::ContentBlockStart { index, .. }
                | StreamEvent::ContentBlockDelta { index, .. }
                | StreamEvent::ContentBlockStop { index, .. } => Some((frame.name(), *index)),
                _ => None,
            })
            .collect();
        assert_eq!(
            indexed,
            [
                ("content_block_start", 0),
                ("content_block_delta", 0),
                ("content_block_stop", 0),
                ("content_block_start", 1),
                ("content_block_delta", 1),
                ("content_block_stop", 1),
                ("content_block_start", 2),
                ("content_block_delta", 2),
                ("content_block_stop", 2),
                ("content_block_start", 3),
                ("content_block_delta", 3),
                ("content_block_stop", 3),
            ]
        );

        // Every index is started before it is used and stopped after, and no
        // index is reused — the three orderings the client throws on.
        let mut open: Option<u64> = None;
        let mut seen = std::collections::BTreeSet::new();
        for (name, index) in &indexed {
            match *name {
                "content_block_start" => {
                    assert_eq!(open, None, "block {index} opened over an open one");
                    assert!(seen.insert(*index), "index {index} was reused");
                    open = Some(*index);
                }
                "content_block_delta" => assert_eq!(open, Some(*index)),
                _ => {
                    assert_eq!(open, Some(*index));
                    open = None;
                }
            }
        }
        assert_eq!(open, None, "a block was left open at the terminal");
    }

    /// **The stop reason a client is told is translated, never forwarded — and
    /// emitted tool calls out-rank whatever the provider said.**
    ///
    /// The table matters because this dialect's `stop_reason` is a closed set of
    /// seven and the value arriving from the fold is whatever the *answering*
    /// wire spells: routing a Claude Code turn to an OpenAI model brings
    /// `max_output_tokens` back, and a turn a Responses-wire provider answered
    /// with tool calls brings back nothing at all. Forwarding either verbatim
    /// gives a strict reader a value it refuses and an agent a turn it will not
    /// act on.
    #[test]
    fn the_stop_reason_is_translated_and_a_tool_call_overrides_it() {
        let emission = started();
        for (word, expected) in [
            (Some("end_turn"), StopReason::EndTurn),
            (Some("tool_use"), StopReason::ToolUse),
            (Some("max_tokens"), StopReason::MaxTokens),
            // The Responses wire's own spelling of the same fact.
            (Some("max_output_tokens"), StopReason::MaxTokens),
            (Some("stop_sequence"), StopReason::StopSequence),
            (Some("refusal"), StopReason::Refusal),
            (Some("pause_turn"), StopReason::PauseTurn),
            (
                Some("model_context_window_exceeded"),
                StopReason::ModelContextWindowExceeded,
            ),
            // Neither a value this build knows nor one the wire has.
            (Some("guardrail_intervened"), StopReason::EndTurn),
            (None, StopReason::EndTurn),
        ] {
            assert_eq!(
                emission.completion_stop_reason(word),
                expected,
                "{word:?} was mistranslated"
            );
        }

        // And the override, which is what makes a cross-dialect tool turn
        // usable: the same two inputs that gave `end_turn` above give
        // `tool_use` once a call has gone out.
        let mut called = started();
        called.project(&SessionEventKind::ItemAppended {
            item: Item {
                response_id: Some(response()),
                ..Item::tool_call("toolu_01", "Grep", "{}")
            },
        });
        assert_eq!(called.completion_stop_reason(None), StopReason::ToolUse);
        assert_eq!(
            called.completion_stop_reason(Some("end_turn")),
            StopReason::ToolUse,
            "a message carrying tool_use blocks and saying end_turn is a shape \
             the API never sends and an agent never acts on"
        );
    }

    /// The seam's substituted usage displaces what the log booked.
    #[test]
    fn a_reported_usage_override_reaches_the_terminal_delta() {
        let mut emission = started();
        emission.report_instead(Usage {
            input_tokens: 40,
            output_tokens: 7,
            ..Default::default()
        });
        let (frames, _) = emission.project(&SessionEventKind::ResponseCompleted {
            response_id: response(),
            usage: Usage {
                input_tokens: 5_000,
                output_tokens: 900,
                ..Default::default()
            },
            provider_reported_cost_usd: None,
            stop_reason: None,
        });
        let delta = frames
            .iter()
            .find(|frame| frame.name() == "message_delta")
            .expect("the terminal pair carries a delta");
        assert_eq!(delta.data()["usage"]["output_tokens"], json!(7));
        assert_eq!(delta.data()["usage"]["input_tokens"], json!(40));
    }

    /// The keepalive is an event with a payload, not a bare comment.
    #[test]
    fn the_keepalive_carries_a_data_payload() {
        let ping = keepalive();
        assert_eq!(ping.name(), "ping");
        assert_eq!(
            ping.data(),
            json!({ "type": "ping" }),
            "a chained Relay's re-encoder drops frames with no `data:` line"
        );
    }

    /// **The keepalive's over-the-wire SSE framing carries the `event:` line
    /// and a `data:` line, not the bare comment [`axum::response::sse::KeepAlive`]
    /// would emit.**
    ///
    /// [`the_keepalive_carries_a_data_payload`] asserts on [`Frame::data`], which
    /// is the payload this module *would* put on the wire if it went out as a
    /// named event — but that assertion is reachable whether or not
    /// [`Frame::into_sse`] actually names the event or attaches that payload as
    /// `data:`. Only inspecting the rendered [`axum::response::sse::Event`] can
    /// tell a real `event: ping\ndata: …` frame apart from
    /// `Event::default().comment("keepalive")`, which satisfies Claude Code's
    /// byte watchdog identically (§3.5) and is silently discarded by a chained
    /// NeMo Relay's re-encoder — the module doc's own reason for choosing the
    /// event shape in the first place.
    ///
    /// `Event`'s fields are private; its `#[derive(Debug)]` renders the exact
    /// wire bytes through `bytes::BytesMut`'s ASCII-escaping `Debug` (ordinary
    /// printable bytes, `event: ping` and `data: ` included, come through
    /// unescaped), which is the only way from outside `axum::response::sse` to
    /// see what `into_sse()` produced without standing up a real socket.
    #[test]
    fn the_keepalive_survives_into_sse_as_a_named_event_with_data() {
        let rendered = format!("{:?}", keepalive().into_sse());
        assert!(
            rendered.contains("event: ping"),
            "Claude Code dispatches on the event: line and silently drops a \
             frame with none: {rendered}"
        );
        assert!(
            rendered.contains("data: "),
            "a chained Relay's re-encoder drops frames with no data: line: {rendered}"
        );

        // The control: a bare SSE comment — the shape this test exists to rule
        // out — passes Frame::data() scrutiny trivially (there is no Frame to
        // build one from) but must fail *this* assertion, or the two shapes
        // are indistinguishable to every test in this module and the claim
        // above is untested rather than tested.
        let comment_only = format!("{:?}", Event::default().comment("keepalive"));
        assert!(
            !comment_only.contains("data: "),
            "the control must actually be a bare comment: {comment_only}"
        );
    }

    /// The non-streaming body is a complete `Message`, block and all.
    #[test]
    fn the_non_streaming_body_is_a_complete_message() {
        let body = message_body(
            &response(),
            "claude-sonnet-4-5",
            vec![ContentBlock::text("hello")],
            StopReason::EndTurn,
            &Usage {
                input_tokens: 900,
                cached_input_tokens: 800,
                output_tokens: 12,
                ..Default::default()
            },
        );
        let data = serde_json::to_value(&body).expect("a Message serializes");
        assert_eq!(data["type"], json!("message"));
        assert_eq!(data["role"], json!("assistant"));
        assert_eq!(data["stop_reason"], json!("end_turn"));
        assert_eq!(
            data["content"],
            json!([{ "type": "text", "text": "hello" }])
        );
        assert_eq!(data["usage"]["input_tokens"], json!(100));
        assert_eq!(data["usage"]["cache_read_input_tokens"], json!(800));
        assert_eq!(data["usage"]["output_tokens"], json!(12));

        // An empty answer still carries its block, so the two projections of one
        // turn cannot disagree about whether there was content.
        let empty = message_body(
            &response(),
            "m",
            vec![ContentBlock::text("")],
            StopReason::EndTurn,
            &Usage::default(),
        );
        assert_eq!(
            serde_json::to_value(&empty).unwrap()["content"],
            json!([{ "type": "text", "text": "" }])
        );

        // And a tool-using turn's blocks reach `content` in order, which is the
        // whole reason this takes blocks rather than a string: the streaming
        // path emits them and this path must not flatten them away.
        let calling = message_body(
            &response(),
            "m",
            vec![
                ContentBlock::text("let me look"),
                ContentBlock::ToolUse {
                    id: "toolu_01".into(),
                    name: "Grep".into(),
                    input: json!({ "pattern": "fn main" }),
                    cache_control: None,
                    extra: Extra::new(),
                },
            ],
            StopReason::ToolUse,
            &Usage::default(),
        );
        let data = serde_json::to_value(&calling).unwrap();
        assert_eq!(data["stop_reason"], json!("tool_use"));
        assert_eq!(
            data["content"],
            json!([
                { "type": "text", "text": "let me look" },
                { "type": "tool_use", "id": "toolu_01", "name": "Grep",
                  "input": { "pattern": "fn main" } },
            ])
        );
    }
}
