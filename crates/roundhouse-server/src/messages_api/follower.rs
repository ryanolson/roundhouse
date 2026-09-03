// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Following one turn's log as a Messages response, streamed or whole.
//!
//! Split out of [`messages_api`](super) (M12 review F11) once that file had
//! passed 1 500 lines. The seam is the one the module doc already drew: the
//! handler decides *whether* a turn may run and on whose behalf, and everything
//! here decides what the client is told while it does. What makes it a module
//! rather than a region is that the two halves are read for different reasons —
//! a question about admission, budgets or refusal codes never needs the cursor,
//! and a question about frame order never needs the auth.
//!
//! **The projection is in neither.** [`MessageEmission`] holds narrowing,
//! ordering, the usage inversion and the terminal table; this file holds the
//! cursor, the poll, the keepalive and the fold that turns the same frames into
//! one `Message`. That is why the streaming and non-streaming answers cannot
//! disagree: there is one projection with two renderings, and both are here.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::sse::Event;
use axum::response::{IntoResponse, Response};
use futures::Stream;
use serde_json::Value;
use tokio::task::JoinHandle;

use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::Principal;
use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_core::ids::SessionId;
use roundhouse_core::store::SessionStore;

use roundhouse_fleet::anthropic_messages::wire::{
    ApiError as WireError, BlockDelta, ContentBlock, Extra as WireExtra, Message, StopReason,
    StreamEvent,
};

use crate::conversations::Conversations;
use crate::engine::Engine;
use crate::http::{ApiError, LogTail, POLL_INTERVAL};

use super::emit::{self, Emitted, Frame, MessageEmission, Step, keepalive};
use super::{MessagesError, keepalive_due};

/// Where a [`MessagesFollower`] is in its life.
///
/// The Responses follower's three phases exactly, and `Copy` for the same
/// reason: the response being replayed is on the emission, so a phase is
/// nothing but cursors.
#[derive(Clone, Copy)]
enum Phase {
    Tailing,
    /// The turn was deduplicated onto an earlier response, whose entries are
    /// being re-read in batches. `bound` is the sequence of the
    /// `turn_deduplicated` event, past which nothing can belong to the replay.
    Replaying {
        cursor: u64,
        bound: u64,
    },
    Done,
}

/// Streams one turn as a Messages API response.
///
/// The turn runs in a task this follower never aborts, for the reason
/// [`http`](crate::http) gives: dropping the handle detaches rather than
/// cancels, and a client that hangs up must not take down a turn the log has
/// already admitted.
///
/// **The projection is not here.** [`MessageEmission`] holds all of it —
/// narrowing, ordering, the usage inversion, the terminal table — and this type
/// holds only the cursor, the poll and the keepalive. That split is what lets
/// the whole ordering contract be asserted from a list of log entries with no
/// store and no engine, which is the difference between testing the sequence
/// Claude Code throws on and hoping it is right.
pub(super) struct MessagesFollower<S: SessionStore, T: Tokenizer + Clone> {
    tail: LogTail<S>,
    /// Where a tool call this turn emits is recorded, so the MCP surface can
    /// find the conversation it came from (M12, R-M2).
    ///
    /// Carried on the follower rather than reached for at the handler, because
    /// the id is not knowable until the model produces it: the binding can only
    /// be written by whatever is watching the log as the call is committed, and
    /// that is this type.
    conversations: Arc<Conversations>,
    /// Whose calls these are. Held for the binding above and for nothing else —
    /// a tool-use id is unique on its own, and this is the check that keeps one
    /// tenant's id from resolving in another tenant's hands.
    principal: Principal,
    /// Only ever asked what this deployment's tokenizer makes of an item; the
    /// turn itself runs in the task below.
    engine: Arc<Engine<S, T>>,
    admitted_input_tokens: u64,
    emission: MessageEmission,
    turn: JoinHandle<Result<(), String>>,
    queued: VecDeque<Frame>,
    /// Consecutive empty polls, for the keepalive below.
    idle_polls: u32,
    phase: Phase,
}

impl<S: SessionStore, T: Tokenizer + Clone + Send + Sync + 'static> MessagesFollower<S, T> {
    /// One turn, followed from `after_seq`.
    ///
    /// A constructor rather than a struct literal at the handler, for the one
    /// field a literal could get wrong: `session_id` is both what the tail
    /// follows and what an MCP tool-use binding is written against, and the
    /// handler spelled it twice. Taken once here, the two cannot part company —
    /// and there is now only one of it to part company with, the binding being
    /// written from [`LogTail::session_id`] rather than from a copy this type
    /// kept beside the tail (M12 review, F11).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        store: Arc<S>,
        session_id: SessionId,
        after_seq: u64,
        conversations: Arc<Conversations>,
        principal: Principal,
        engine: Arc<Engine<S, T>>,
        admitted_input_tokens: u64,
        emission: MessageEmission,
        turn: JoinHandle<Result<(), String>>,
    ) -> Self {
        Self {
            tail: LogTail::new(store, session_id, after_seq),
            conversations,
            principal,
            engine,
            admitted_input_tokens,
            emission,
            turn,
            queued: VecDeque::new(),
            idle_polls: 0,
            phase: Phase::Tailing,
        }
    }

    async fn next_frame(&mut self) -> Option<Frame> {
        loop {
            if let Some(frame) = self.queued.pop_front() {
                return Some(frame);
            }
            match self.phase {
                Phase::Tailing => self.tail_once().await,
                Phase::Replaying { .. } => self.replay_once().await,
                Phase::Done => return None,
            }
        }
    }

    /// One tailing step: read new appends, or notice the turn is gone.
    async fn tail_once(&mut self) {
        // Observed before the drain, so that finished-then-empty proves the log
        // is fully read; see the same check in `http`.
        let finished = self.turn.is_finished();
        match self.tail.drain().await {
            Ok(events) if events.is_empty() => {
                if finished {
                    self.fail_without_terminal().await;
                } else {
                    self.idle().await;
                }
            }
            Ok(events) => {
                self.idle_polls = 0;
                self.consume(&events, None).await;
            }
            Err(error) => self.fail(&error.to_string()),
        }
    }

    /// One replay step: project a batch of the deduplicated response's entries.
    ///
    /// Bounded by the `turn_deduplicated` event and paced one batch per frame
    /// demand, exactly like tailing. Earlier retries' markers carry this
    /// response's id too and are excluded by kind: they announce a replay rather
    /// than belonging to one, and projecting them would restart it.
    async fn replay_once(&mut self) {
        let Phase::Replaying { cursor, bound } = self.phase else {
            return;
        };
        match self.tail.read(cursor).await {
            Ok(events) if events.is_empty() => self.phase = Phase::Done,
            Ok(events) => {
                let last_seq = events.last().map_or(cursor, |event| event.seq);
                self.consume(&events, Some(bound)).await;
                // `consume` sets `Done` on a terminal frame and `Replaying` on a
                // nested dedup; neither should be overwritten here.
                if matches!(self.phase, Phase::Replaying { .. }) {
                    self.phase = if last_seq + 1 >= bound {
                        Phase::Done
                    } else {
                        Phase::Replaying {
                            cursor: last_seq,
                            bound,
                        }
                    };
                }
            }
            Err(error) => self.fail(&error.to_string()),
        }
    }

    /// Project a batch, stopping at the first terminal or dedup step.
    ///
    /// One function for both phases because the *narrowing* is the emission's,
    /// not the phase's: what differs between tailing and replaying is only the
    /// `bound` and the marker exclusion, which are two lines rather than a
    /// second copy of the loop.
    async fn consume(&mut self, events: &[SessionEvent], bound: Option<u64>) {
        for event in events {
            if let Some(bound) = bound
                && (event.seq >= bound
                    || matches!(event.kind, SessionEventKind::TurnDeduplicated { .. }))
            {
                continue;
            }
            // Asked before the projection, because only this type holds an
            // engine to ask with. An item the emission would stream as a seam
            // answer was committed whole rather than dispatched, so what the log
            // booked for it is the judge's usage and not what this turn
            // contributed to the context — see `Engine::context_contribution`.
            //
            // Narrowed to the seam answer specifically, not to "anything the
            // emission claims": a tool call is claimed too and is the product of
            // an ordinary *dispatched* turn, whose booked usage is exactly what
            // the client should be told. Substituting a context contribution
            // there would replace a provider's measured counts with our
            // tokenizer's estimate on the most ordinary turn an agent takes.
            if let SessionEventKind::ItemAppended { item } = &event.kind {
                match self.emission.emitted(item) {
                    Some(Emitted::SeamText(_)) => {
                        let contribution = self
                            .engine
                            .context_contribution(self.admitted_input_tokens, item);
                        self.emission.report_instead(contribution);
                    }
                    // **R-M2's binding, written here and nowhere else.** This
                    // is the one moment both halves of the correlation are in
                    // one place: the id the client is about to be handed, and
                    // the session it was emitted into. Claude Code quotes it
                    // back on the `tools/call` it makes for this block
                    // (`_meta["claudecode/toolUseId"]`), and without the
                    // binding that call falls back to the principal's most
                    // recent conversation — which, for an agent running
                    // subagents, is a coin toss between logs.
                    //
                    // Written from the *emitted* narrowing rather than from
                    // every `ToolCall` item in the window, so the binding is
                    // for a call this response actually announced to this
                    // client and not for one a concurrent replay walked past.
                    //
                    // **Awaited inline, and on a durable deployment that is a
                    // round trip in the projection loop** (M14.1). Not spawned
                    // off, because the ordering is load-bearing: the client
                    // cannot call the tool until it has been handed this
                    // block, so the binding must be in the store *before* the
                    // frame carrying the id leaves — a write racing the answer
                    // to it is the pre-R-M2 guess with a stopwatch on it. One
                    // `SET`-shaped write per tool call the deployment emits is
                    // the price, and it is paid on the leg that then waits for
                    // a model.
                    Some(Emitted::ToolCall { call_id, .. }) => {
                        let call_id = call_id.to_string();
                        self.conversations
                            .bind_call(&self.principal, &call_id, self.tail.session_id().clone())
                            .await;
                    }
                    None => {}
                }
            }
            let (frames, step) = self.emission.project(&event.kind);
            self.queued.extend(frames);
            match step {
                Step::Continue => {}
                Step::Deduplicated { .. } => {
                    self.phase = Phase::Replaying {
                        cursor: 0,
                        bound: event.seq,
                    };
                    return;
                }
                Step::End => {
                    self.phase = Phase::Done;
                    return;
                }
            }
        }
    }

    /// Wait out one poll, emitting a `ping` if the stream has gone quiet.
    ///
    /// Counted in polls rather than timed off a clock so the keepalive is a
    /// function of the same loop everything else here is, and so a test can
    /// reach it without waiting. See
    /// [`KEEPALIVE_INTERVAL`](super::KEEPALIVE_INTERVAL) for the cadence and
    /// [`emit::keepalive`] for why it is an event rather than an SSE comment.
    async fn idle(&mut self) {
        self.idle_polls += 1;
        if keepalive_due(self.idle_polls) {
            self.idle_polls = 0;
            self.queued.push_back(keepalive());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    /// End a stream whose turn is gone but whose response never terminated.
    ///
    /// Two ways here, both from [`http`](crate::http)'s account: the engine
    /// returned before writing a `turn_started` for this turn id, or an admitted
    /// turn died without settling. Either way no terminal event is coming, and
    /// the turn's own outcome is the only answer left to give.
    async fn fail_without_terminal(&mut self) {
        let message = match (&mut self.turn).await {
            Ok(Err(message)) => message,
            Err(join) => format!("turn task failed: {join}"),
            Ok(Ok(())) => "the turn ended without terminating its response".to_string(),
        };
        self.fail(&message);
    }

    /// Terminate with an `error` event.
    ///
    /// `api_error` and not `overloaded_error`: this is a turn whose task is
    /// gone, so the same request will not be answered by trying again — and
    /// `overloaded_error` is the one string that makes Claude Code retry
    /// regardless of anything else (§3.2). Spelling a permanent fault retryable
    /// is how an agent spends its whole retry budget on a turn that can never
    /// succeed.
    fn fail(&mut self, message: &str) {
        // Leaked into the frame deliberately: the message is this deployment's
        // own — a store error or a turn task's failure — and the alternative is
        // a client-side report with nothing in it to correlate against the log.
        self.queued
            .extend(self.emission.failed(emit::API_ERROR, message));
        self.phase = Phase::Done;
    }

    pub(super) fn into_stream(
        self,
    ) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
        futures::stream::unfold(self, |mut follower| async move {
            follower
                .next_frame()
                .await
                .map(|frame| (Ok(frame.into_sse()), follower))
        })
    }
}

// ---------------------------------------------------------------------------
// The non-streaming answer
// ---------------------------------------------------------------------------

/// Content blocks, reassembled from the block frames a stream would have sent.
///
/// **The client's own accumulator, on our side of the wire.** Since M11.2 a turn
/// is not one text block: a tool-using answer interleaves text and `tool_use`
/// blocks, and a non-streaming caller is owed the same `content` array a
/// streaming one assembles. Concatenating every `text_delta` into a single block
/// — what this path did while there was only ever one block — would drop every
/// tool call on the floor, and the client would read a turn that asked for
/// nothing as a turn that answered.
///
/// One block open at a time, and that is a property of the emitter rather than
/// an assumption about the wire: [`MessageEmission`] closes each block before
/// opening the next, so there is no interleaving to track. A frame sequence that
/// violated it would build the blocks in a different order, which is a shape
/// only our own emitter could produce and one its own ordering tests forbid.
#[derive(Debug, Default)]
struct BlockAccumulator {
    done: Vec<ContentBlock>,
    open: Option<OpenBlock>,
}

/// A block being filled, in the two shapes this surface emits.
#[derive(Debug)]
enum OpenBlock {
    Text(String),
    /// A tool call, whose arguments arrive as JSON *fragments* — see
    /// [`BlockAccumulator::close`] for what is done with a fragment run that
    /// does not parse.
    Tool {
        id: String,
        name: String,
        partial_json: String,
    },
}

impl BlockAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn open(&mut self, block: &ContentBlock) {
        // Any previous block is closed by its own `content_block_stop`; this is
        // belt and braces for a frame sequence that skipped one, and it keeps
        // the content it had rather than discarding it.
        self.close();
        self.open = match block {
            ContentBlock::Text { text, .. } => Some(OpenBlock::Text(text.clone())),
            ContentBlock::ToolUse { id, name, .. } => Some(OpenBlock::Tool {
                id: id.clone(),
                name: name.clone(),
                partial_json: String::new(),
            }),
            // Nothing else is emitted by this surface. Named rather than
            // wildcarded, so the day it emits a `thinking` block this line is a
            // compile error at the one site that decides what a non-streaming
            // client sees — a wildcard would silently drop it instead, and
            // dropping is the unsafe default here. Ignored rather than refused
            // for now: this fold reads our own frames, so an unknown block is a
            // bug in the emitter, and a refusal would turn it into a 500 for the
            // client instead of a red test for us.
            ContentBlock::ToolResult { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. }
            | ContentBlock::Opaque(_) => None,
        };
    }

    fn push(&mut self, delta: &BlockDelta) {
        match (&mut self.open, delta) {
            (Some(OpenBlock::Text(text)), BlockDelta::TextDelta { text: chunk, .. }) => {
                text.push_str(chunk);
            }
            (
                Some(OpenBlock::Tool { partial_json, .. }),
                BlockDelta::InputJsonDelta {
                    partial_json: fragment,
                    ..
                },
            ) => partial_json.push_str(fragment),
            // A delta whose type does not match its block is exactly what the
            // client's accumulator *throws* on, and it cannot happen against our
            // own emitter. Dropped here for the same reason the unknown block is.
            _ => {}
        }
    }

    fn close(&mut self) {
        let Some(open) = self.open.take() else {
            return;
        };
        self.done.push(match open {
            OpenBlock::Text(text) => ContentBlock::text(text),
            OpenBlock::Tool {
                id,
                name,
                partial_json,
            } => ContentBlock::ToolUse {
                id,
                name,
                // **Parsed here and only here.** `input` is a JSON *value* on
                // this wire while the log holds the arguments as the byte string
                // the model produced — the same asymmetry the streaming path
                // resolves by sending fragments and letting the client parse.
                // An unparseable run becomes `{}` rather than failing the turn:
                // the arguments came out of a decoder that already reassembled
                // them, so this is unreachable short of a corrupt log, and a
                // whole turn refused over one malformed call is a worse answer
                // than a call the client refuses for itself.
                input: serde_json::from_str(&partial_json)
                    .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
                cache_control: None,
                extra: WireExtra::new(),
            },
        });
    }

    fn finish(mut self) -> Vec<ContentBlock> {
        self.close();
        self.done
    }
}

/// Run the follower to the end and answer one complete `Message`.
///
/// **Assembled from the frames the streaming path would have emitted, not from
/// a second reading of the log.** Claude Code's auth and quota probes are
/// one-token `stream`-less creates (§3.6) and its streaming fallback re-issues a
/// whole turn this way, so this path is reached with the *same* turn a stream
/// would have carried — and two projections that could disagree about it would
/// be two answers to one question, discovered by a client that got a different
/// message depending on which it asked for. Folding the frames makes them one
/// projection with two renderings.
///
/// A stream that ended in an `error` event becomes a status code rather than a
/// 200 carrying an error body: nothing has been written yet on this path, so the
/// status is still expressible, and it is what the client's recovery reads.
pub(super) async fn complete_message<S, T>(
    mut follower: MessagesFollower<S, T>,
) -> Result<Response, MessagesError>
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    let mut message: Option<Message> = None;
    let mut content = BlockAccumulator::new();
    let mut stop_reason = StopReason::EndTurn;
    let mut usage = None;
    while let Some(frame) = follower.next_frame().await {
        match frame.event() {
            StreamEvent::MessageStart { message: start, .. } => message = Some(start.clone()),
            StreamEvent::ContentBlockStart { content_block, .. } => content.open(content_block),
            StreamEvent::ContentBlockDelta { delta, .. } => content.push(delta),
            StreamEvent::ContentBlockStop { .. } => content.close(),
            StreamEvent::MessageDelta {
                delta,
                usage: reported,
                ..
            } => {
                if let Some(reason) = &delta.stop_reason {
                    stop_reason = reason.clone();
                }
                // NOT a field-by-field merge like the client's own
                // accumulator (§3.4: a `> 0` guard per input-side counter,
                // `??` on output_tokens -- both documented on `message_delta`
                // above). This replaces the whole `Usage` object wholesale
                // when the terminal frame carries one, and leaves the
                // prelude's standing when it carries none.
                //
                // The two land on identical numbers today only because
                // `emit::message_delta` never reports a partial terminal
                // count: whenever `reported` is `Some`, every field in it is
                // the complete, freshly measured total for the whole turn,
                // not a delta to reconcile against the prelude. They would
                // disagree on exactly one shape the review pinned: a terminal
                // delta whose fresh input count (input minus cache read minus
                // cache write) is genuinely zero. The client's `> 0` guard
                // reads that zero as "not reported" and keeps the prelude's
                // inflated, pre-cache-split count; this wholesale replace has
                // no such guard and adopts the correct zero instead.
                //
                // Unreachable from our own dispatch today: a turn is only
                // ever completed -- dispatched or seam-answered -- with
                // content that was never itself part of an earlier cache read
                // or write, so the terminal `Usage` this fold ever sees
                // always has a nonzero fresh remainder. A dispatch path that
                // could report an all-cached turn would need this to become a
                // real field-by-field merge.
                if reported.is_some() {
                    usage = reported.clone();
                }
            }
            StreamEvent::Error { error, .. } => {
                return Err(MessagesError(mid_stream_failure(error)));
            }
            StreamEvent::MessageStop { .. } | StreamEvent::Ping { .. } => {}
        }
    }

    let Some(mut message) = message else {
        // No `message_start` at all: the turn never began, and there is nothing
        // to report a stop reason for. The streaming path answers this with an
        // error event; here the status code is still available and says more.
        return Err(MessagesError(ApiError::internal(
            "turn_never_started",
            "the turn produced no response to answer with",
        )));
    };
    message.content = content.finish();
    message.stop_reason = Some(stop_reason);
    if let Some(usage) = usage {
        message.usage = usage;
    }
    Ok(axum::Json(message).into_response())
}

/// A mid-stream failure, as a pre-stream status code.
///
/// The emission's error vocabulary read backwards. It exists because the
/// non-streaming path can still answer with a status, and a client that got
/// `200 {"type":"error"}` would have to parse a success body to find a failure —
/// which the SDK does not do off the streaming path.
fn mid_stream_failure(error: &WireError) -> ApiError {
    match error.kind.as_str() {
        emit::RATE_LIMIT_ERROR => ApiError::refused(
            StatusCode::TOO_MANY_REQUESTS,
            "budget_exhausted",
            error.message.clone(),
        ),
        emit::PERMISSION_ERROR => ApiError::refused(
            StatusCode::FORBIDDEN,
            "policy_refused",
            error.message.clone(),
        ),
        // `overloaded_error` included: mid-stream it is the only spelling the
        // client retries, and off the stream a 503 is the status that says the
        // same thing to the same retry predicate — which
        // `error_kind` then spells back as `overloaded_error`, so the two
        // renderings of one failure agree without either knowing about the
        // other.
        emit::OVERLOADED_ERROR => ApiError::refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_unavailable",
            error.message.clone(),
        ),
        _ => ApiError::internal("turn_failed", error.message.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // F9's fixtures only: a real `Engine` and a real `SessionId`/`TurnId` pair
    // to build a real `MessagesFollower` from.
    use roundhouse_core::context::ByteTokenizer;
    use roundhouse_core::event::Usage;
    use roundhouse_core::ids::{ResponseId, TurnId};
    use roundhouse_core::item::Item;
    use roundhouse_core::routing::AffinityPolicy;
    use roundhouse_core::store::MemoryStore;
    use roundhouse_fleet::{EchoFrontierClient, StaticFrontierCatalog};

    use crate::engine::{EchoLocalExecutor, EngineConfig};

    // -------------------------------------------------------------------
    // F9 (M11.1 thermo-nuclear review)
    // -------------------------------------------------------------------
    //
    // The claim: the guard above and the fix stage's two SSE-framing tests
    // all stop short of `idle()` (messages_api.rs:723-730) itself -- the
    // async method that actually decides, on every quiet poll, whether to
    // push a keepalive. Neither drives it: the guard above calls
    // `keepalive_due` directly with no `MessagesFollower` in sight, and
    // nothing in `messages_api_surface.rs` waits out (or fakes) the 15 real
    // seconds of silence the branch needs, because every fixture there
    // answers through a synchronous echo/scripted client with no stall. So
    // deleting the `if` line below, inverting its comparison, or moving the
    // `idle_polls = 0` reset before the check would all ship with the whole
    // workspace green -- `messages_api_surface.rs` cannot even name
    // `idle_polls`, `keepalive_due`, or `fn idle`, all private to this
    // module, so no test outside this file could ever have noticed.
    //
    // `bare_follower` builds a real `MessagesFollower` and the two tests
    // below call the real `.idle()` -- not a re-implementation of its logic
    // -- so a mutation to that method is what they are sensitive to, not a
    // mutation to `keepalive_due` (already guarded above) or to
    // `keepalive()`'s payload (already guarded by the fix stage's framing
    // tests). `idle_polls` is seeded directly to 599 in the second test
    // rather than reached by calling `.idle()` 599 times in a row: the only
    // per-call side effect of a not-yet-due poll besides the sleep is the
    // increment, so seeding reproduces the state 599 real calls would leave
    // without paying 599 * 25 ms for it -- which also shows that reaching
    // this branch does not actually require the wait the module doc's "so a
    // test can reach it without waiting" comment reserves for the pure
    // predicate alone.

    /// A follower over disposable fixtures nothing here reads: `.idle()`
    /// touches only `idle_polls` and `queued`, so `tail` and `engine` exist
    /// only because the struct's fields must all be initialized.
    fn bare_follower() -> MessagesFollower<MemoryStore, ByteTokenizer> {
        let store = Arc::new(MemoryStore::new());
        // Through `new` rather than a struct literal, so the fixture is built
        // the way the handler builds one: a literal here could set a
        // `session_id` the tail does not follow, which is exactly the state the
        // constructor exists to make unreachable.
        MessagesFollower::new(
            Arc::clone(&store),
            SessionId::new("f9-fixture"),
            0,
            Arc::new(Conversations::new()),
            Principal::new("acme", "ada"),
            Arc::new(Engine::new(
                store,
                ByteTokenizer,
                Arc::new(EchoLocalExecutor::new("local")),
                StaticFrontierCatalog::new(vec![]),
                Arc::new(EchoFrontierClient::new("frontier")),
                Arc::new(AffinityPolicy::new()),
                EngineConfig::default(),
            )),
            0,
            MessageEmission::new(TurnId::new("turn_f9"), "claude-test", Usage::default()),
            // Never finishes, so a mutated `idle()` reading
            // `self.turn.is_finished()` (it does not, today) could not
            // accidentally explain a passing test by routing through
            // `fail_without_terminal` instead.
            tokio::spawn(std::future::pending::<Result<(), String>>()),
        )
    }

    /// F11's other half: the id an MCP binding is written against is the one
    /// the tail follows, because there is only one of it.
    ///
    /// The follower used to keep its own copy of the `SessionId` beside the
    /// tail's, set from the same constructor argument — a duplicate that could
    /// only ever go wrong, never right. `LogTail::session_id` is where it is
    /// read from now, so a binding pointing at a log this turn never touched is
    /// no longer a state this type can be put in. This test is what keeps the
    /// accessor honest: it drives the real `consume` path that writes the
    /// binding, so a mutation that binds some other session — or stops binding
    /// at all — goes red here rather than in an agent's subagent picking up
    /// another conversation's answer (M12, R-M2).
    #[tokio::test]
    async fn the_binding_names_the_session_the_tail_follows() {
        let mut follower = bare_follower();
        let response = ResponseId::new("resp_f11");
        let followed = follower.tail.session_id().clone();
        let mut call = Item::tool_call("call_f11", "mcp__roundhouse__status", "{}");
        call.response_id = Some(response.clone());

        follower
            .consume(
                &[
                    SessionEvent {
                        seq: 1,
                        session_id: followed.clone(),
                        at_ms: 0,
                        kind: SessionEventKind::TurnStarted {
                            turn_id: TurnId::new("turn_f9"),
                            response_id: response.clone(),
                        },
                    },
                    SessionEvent {
                        seq: 2,
                        session_id: followed.clone(),
                        at_ms: 0,
                        kind: SessionEventKind::ItemAppended { item: call },
                    },
                ],
                None,
            )
            .await;

        assert_eq!(
            follower
                .conversations
                .session_of_call(&follower.principal, "call_f11")
                .await
                .unwrap(),
            Some(followed),
            "the call this response announced must resolve to the log it was \
             emitted into"
        );
    }

    /// CONTROL, kept live. Pins today's real, verified `idle()` behavior on a
    /// single not-yet-due poll: proves the fixture above is sound (it
    /// constructs and the method runs) and that an ordinary quiet poll does
    /// not queue anything, without relying on the 600-poll boundary the
    /// probe below is actually about. On its own this does not close F9's
    /// gap -- a mutated `idle()` that deletes the `if` block outright, or
    /// pins `idle_polls` at some other constant, still increments once here
    /// and still queues nothing, so this assertion cannot tell "no keepalive
    /// mechanism exists" from "the mechanism has not fired yet."
    #[tokio::test]
    async fn f9_control_idle_does_not_queue_a_keepalive_before_the_window() {
        let mut follower = bare_follower();
        follower.idle().await;
        assert_eq!(follower.idle_polls, 1, "one quiet poll, one increment");
        assert!(
            follower.queued.is_empty(),
            "a single quiet poll is nowhere near the 15 s window and must not queue anything: \
             {:?}",
            follower.queued
        );
    }

    /// PROBE: F9, fixed here. Seeds the boundary directly (see the module
    /// comment above for why that is equivalent to 599 real quiet polls) and
    /// calls the real `.idle()` once more, which must be the 600th and
    /// therefore due.
    ///
    /// The gap this closes was established by direct reading/grep alone (a
    /// claim about the *absence* of any reference to `idle_polls`,
    /// `keepalive_due`, or `fn idle` outside this module), and this probe's
    /// own correctness — that it compiles against the real signatures below
    /// and actually exercises `idle()` rather than a re-implementation of it
    /// — was left unconfirmed by a disk-exhaustion incident that stopped the
    /// thermo-nuclear review from running cargo again. Confirmed now:
    /// `cargo test -p roundhouse-server --lib messages_api -j 2` compiles and
    /// passes this test and its control together, with neither ignored.
    #[tokio::test]
    async fn f9_idle_fires_the_keepalive_exactly_at_the_window_boundary() {
        let mut follower = bare_follower();
        follower.idle_polls = 599;
        follower.idle().await;
        assert_eq!(
            follower.idle_polls, 0,
            "the 600th consecutive empty poll must reset the counter"
        );
        assert_eq!(
            follower.queued.len(),
            1,
            "and must queue exactly one keepalive frame: {:?}",
            follower.queued
        );
        assert_eq!(
            follower.queued.front(),
            Some(&keepalive()),
            "the queued frame must be the real ping payload"
        );
    }
}
