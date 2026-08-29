// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Anthropic Messages API surface.
//!
//! A sibling of [`responses_api`](crate::responses_api) over the same log: same
//! engine, same store, same admission, same fair-use refusal, same
//! log-tailing follower. It exists because Claude Code's native dialect is this
//! one, and an agent that has to be modified before it can be routed is not
//! transparently hooked up — which is the word the product definition puts the
//! most weight on.
//!
//! Two things are genuinely different from the Responses surface, and both are
//! about the client rather than about us.
//!
//! **The session has no field to name itself in.** A Messages request body has
//! no `prompt_cache_key`; what it has is an `x-claude-code-session-id` header
//! (live at 2.1.247) and a `metadata.user_id` that has carried the session id in
//! two different spellings across the versions in the wild. [`wire::session_key`]
//! resolves them in the order plan R5 fixes, and everything downstream — the
//! namespace qualification, the `Conversations` binding, the fork on prefix
//! disagreement — is what the Responses surface already does with the key a
//! client hands it, through the one function both call
//! ([`responses_api::bind_prefix`](crate::responses_api)).
//!
//! **The client's parser is strict where the other one is forgiving.** Claude
//! Code dispatches SSE frames on the `event:` name and *silently drops* a frame
//! without one; its accumulator throws on four distinct ordering mistakes; and
//! its usage merge takes `output_tokens` with `??`, so an explicit zero in the
//! terminal frame bills a turn as free. A malformed stream is not a visible
//! failure but a second, non-streaming request for the same turn — one
//! full-price answer for one framing mistake. [`emit`] is shaped so those
//! mistakes are unreachable rather than merely untested.
//!
//! # What this surface costs when it is wrong, stated once
//!
//! Every other transport in this crate fails loudly. This one has a client that
//! recovers from a malformed stream by **re-issuing the whole turn without
//! streaming** (client surface §3.6), so a framing mistake shows up as a
//! deployment that works and costs twice, not as a deployment that is broken.
//! That is the reason the two submodules are pure and separately tested, the
//! reason the strict oracle in `tests/common/anthropic.rs` exists at all, and
//! the reason the keepalive here is a `ping` *event* rather than the SSE comment
//! [`axum::response::sse::KeepAlive`] would emit: a comment satisfies the
//! client's 300-second byte watchdog and is then discarded by a chained NeMo
//! Relay's re-encoder, which drops frames with no `data:` line. One shape that
//! survives both topologies beats two shapes chosen per topology.
//!
//! # Both submodules are public
//!
//! So the conformance oracle can drive parse, canonicalization and the frame
//! projection directly from `tests/` rather than only over a socket — the same
//! reason the dispatch side's wire module is public, and the reason
//! [`http::ApiError`](crate::http::ApiError) is public as a value.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures::Stream;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use roundhouse_core::context::Tokenizer;
use roundhouse_core::event::{SessionEvent, SessionEventKind, Usage};
use roundhouse_core::now_ms;
use roundhouse_core::store::SessionStore;

use roundhouse_fleet::anthropic_messages::wire::{
    ApiError as WireError, BlockDelta, ContentBlock, Message, StopReason, StreamEvent,
};

use crate::control_config::{AuthError, PlaneSource};
use crate::conversations::Conversations;
use crate::engine::{Engine, TurnInput};
use crate::http::{
    ApiError, LogTail, POLL_INTERVAL, parse_body, refuse_over_fair_use, store_error,
};
use crate::responses_api::{API_PREFIX, bind_prefix};

pub mod emit;
pub mod wire;

use emit::{Frame, MessageEmission, Step, keepalive};
use wire::{CreateMessageParams, canonicalize, session_key, turn_id_for};

/// The path the client posts a turn to.
///
/// Under the same `{API_PREFIX}` as the Responses surface, and that sharing is a
/// ruling rather than a convenience (plan R5). Claude Code appends `/v1/messages`
/// to a bare-origin `ANTHROPIC_BASE_URL`, so the version segment is not ours to
/// choose; and `codex_launch::deployment_root` already strips `API_PREFIX` off a
/// deployment's `base_url` to find the MCP mount, so a second prefix here would
/// give one deployment two roots and hand the generated Codex config a bogus
/// one. The coupling is inherited deliberately.
///
/// **The `?beta=true` the client appends is not part of this.** Axum routes on
/// the path alone, so the query is ignored — which is a claim tested rather than
/// assumed, because a router that matched the full URL would 404 every request
/// the shipping client makes.
const MESSAGES_PATH: &str = "messages";

/// The token-count estimate path.
const COUNT_TOKENS_PATH: &str = "messages/count_tokens";

/// How long a silent stream waits before emitting a `ping`.
///
/// The ceiling is the client's: it aborts a stream that relays no bytes for 300
/// seconds (§3.5), and v2.1.42 logs a `tengu_streaming_stall` at 30. Fifteen
/// seconds puts two keepalives inside the shorter of those two windows, which
/// makes a stalled *upstream* visible in our own logs as a run of pings rather
/// than as a client-side abort with nothing on our side to correlate it to.
/// No source establishes a required cadence, only the ceiling (Dive C open
/// question 4), so this is chosen against the *stall log* rather than against
/// the abort — the abort is the failure we must never reach, not the one to
/// aim at.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Serial number for anonymous session names, per process.
static ANONYMOUS: AtomicU64 = AtomicU64::new(0);

/// Whether this many consecutive empty polls is a quiet-enough stream.
///
/// A free function so the cadence is checkable without a store, an engine and a
/// fifteen-second wait. The alternative was to leave the arithmetic inline and
/// have no test of it at all: the only path that reaches it is a turn slower
/// than [`KEEPALIVE_INTERVAL`], which nothing in the suite is, so an edit that
/// made this fire on every poll — flooding a chained Relay — or on none — losing
/// the client at the 300-second watchdog — would go out green either way.
fn keepalive_due(idle_polls: u32) -> bool {
    u128::from(idle_polls) * POLL_INTERVAL.as_millis() >= KEEPALIVE_INTERVAL.as_millis()
}

// ---------------------------------------------------------------------------
// The router
// ---------------------------------------------------------------------------

/// Engine and store handles, plus this node's cache-key bindings.
///
/// The same four things [`responses_api`](crate::responses_api)'s state holds
/// and for the same reasons: a [`PlaneSource`] rather than a compiled plane so a
/// revoked key stops working here too, and a shared [`Conversations`] so an
/// agent that narrowed the routing of a conversation over MCP and a turn that
/// then arrives on it reach one session id, generation and all.
///
/// `Clone` written out rather than derived, because deriving would demand
/// `S: Clone` of a store only ever shared behind an [`Arc`].
struct Messages<S: SessionStore, T: Tokenizer + Clone> {
    engine: Arc<Engine<S, T>>,
    store: Arc<S>,
    planes: Arc<dyn PlaneSource>,
    conversations: Arc<Conversations>,
}

impl<S: SessionStore, T: Tokenizer + Clone> Clone for Messages<S, T> {
    fn clone(&self) -> Self {
        Self {
            engine: Arc::clone(&self.engine),
            store: Arc::clone(&self.store),
            planes: Arc::clone(&self.planes),
            conversations: Arc::clone(&self.conversations),
        }
    }
}

/// The Messages surface's two routes, gated by a control plane.
///
/// Shaped exactly like [`responses_router`](crate::responses_api::responses_router)
/// — same four arguments, same `Arc<dyn PlaneSource>` erasure at the boundary
/// rather than in the parameter, so `Arc<ControlPlane>` and
/// `Arc<ControlDirectory>` both coerce at the call site.
///
/// **`/v1/models` is deliberately absent** (plan R5). Model discovery is opt-in
/// client-side and filters for ids containing `claude` or `anthropic`; serving
/// it would put roundhouse's routing choices into the user's `/model` picker,
/// which is a product decision deferred rather than made. The refusal a client
/// gets is the router's own 404, which is what "not served" should look like —
/// there is nothing here to keep in agreement with a catalog.
pub fn messages_router<S, T, P>(
    planes: Arc<P>,
    engine: Arc<Engine<S, T>>,
    store: Arc<S>,
    conversations: Arc<Conversations>,
) -> Router
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
    P: PlaneSource,
{
    let planes: Arc<dyn PlaneSource> = planes;
    Router::new()
        .route(
            &format!("{API_PREFIX}/{MESSAGES_PATH}"),
            post(create_message::<S, T>),
        )
        .route(
            &format!("{API_PREFIX}/{COUNT_TOKENS_PATH}"),
            post(count_tokens::<S, T>),
        )
        .with_state(Messages {
            engine,
            store,
            planes,
            conversations,
        })
}

// ---------------------------------------------------------------------------
// POST /v1/messages
// ---------------------------------------------------------------------------

/// `POST /v1/messages`
///
/// The order is the Responses surface's, and every step of it is load-bearing
/// in the same way: admission before the body is read, so an unauthenticated
/// request costs a hash lookup and cannot name a session; the fair-use refusal
/// immediately after, because it is the last point at which a status code is
/// still expressible and it is *before* any grant is opened; then everything
/// that is a pure function of the request, so a malformed one costs no round
/// trip and creates no session.
///
/// **The one difference is the session name**, and it is where this dialect
/// earns its own handler: there is no `prompt_cache_key` to require, so a
/// missing name is an *anonymous* turn rather than a 4xx. Claude Code sends
/// `metadata.user_id` on every request of every version read, so the product
/// path never reaches that arm; it exists so a bare `curl` gets a served turn.
async fn create_message<S, T>(
    State(state): State<Messages<S, T>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, MessagesError>
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    let plane = state.planes.plane(now_ms());
    let admission = plane.turn_admission(&headers)?;
    refuse_over_fair_use(&*state.engine, &admission).await?;
    let params: CreateMessageParams = parse_body(&body)?;

    let claimed = canonicalize(&params)?;
    let turn_id = turn_id_for(&claimed);
    let admitted_input_tokens = state.engine.admitted_input_tokens(&claimed);
    // Resolved before `bind` consumes it, and named here rather than inside the
    // bind so the anonymous arm is visible at the site that decides what a turn
    // belongs to rather than buried in a helper.
    let cache_key = session_key(&headers, &params).unwrap_or_else(anonymous_key);
    let (session_id, input) = bind_prefix(
        &state.engine,
        &state.store,
        &state.conversations,
        &plane,
        &admission.principal,
        &cache_key,
        claimed,
    )
    .await?;

    // Read before the spawn, for the reason `http` gives: an event appended
    // between this read and the start of the turn would fall outside the
    // stream's window and be lost to this client.
    let start = state
        .store
        .last_seq(&session_id)
        .await
        .map_err(|error| store_error(&session_id, error))?;

    // Echoed in `message_start.model` and recorded as the turn's declared
    // baseline; never routed on. Empty is treated as absent for the reason the
    // Responses surface gives: a client that sent `""` named nothing, and
    // recording it would put a baseline in the log no catalog can resolve.
    let declared_baseline = params
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string);

    let turn = tokio::spawn({
        let engine = Arc::clone(&state.engine);
        let session_id = session_id.clone();
        let turn_id = turn_id.clone();
        let admission = admission.clone();
        let declared_baseline = declared_baseline.clone();
        async move {
            engine
                .run_turn(
                    &session_id,
                    turn_id,
                    TurnInput {
                        items: input,
                        declared_baseline,
                    },
                    &admission,
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
    });

    let follower = MessagesFollower {
        tail: LogTail::new(Arc::clone(&state.store), session_id, start),
        engine: Arc::clone(&state.engine),
        admitted_input_tokens,
        emission: MessageEmission::new(
            turn_id,
            // What the client asked for, or — when it named nothing — what it
            // will read as "whatever answered". An empty `model` in a
            // `message_start` is a field the client's accumulator carries
            // verbatim into its transcript, so a placeholder that says where the
            // name came from beats an empty string that says nothing.
            declared_baseline.unwrap_or_else(|| UNDECLARED_MODEL.to_string()),
            Usage {
                input_tokens: admitted_input_tokens,
                ..Default::default()
            },
        ),
        turn,
        queued: VecDeque::new(),
        idle_polls: 0,
        phase: Phase::Tailing,
    };

    if params.stream {
        return Ok(Sse::new(follower.into_stream()).into_response());
    }
    complete_message(follower).await
}

/// What `message_start.model` says when the client named nothing.
///
/// A client of this API always names a model — it is `required` on the pinned
/// schema — so this is reached only by a hand-rolled request. Named rather than
/// empty because the value ends up in a transcript.
const UNDECLARED_MODEL: &str = "roundhouse-routed";

/// A fresh session name for a request that carried none.
///
/// Collision-free by construction rather than by hope: the process id separates
/// two roundhouses sharing a store, the millisecond separates two runs of one
/// process, and the counter separates two requests inside one millisecond. It
/// then goes through the same [`ControlPlane::qualify`](crate::control_config::ControlPlane)
/// as any client-chosen key, so it also cannot collide across tenants.
///
/// Deliberately *not* derived from the request's content. A content-derived name
/// would give two anonymous callers who happened to send the same body one
/// shared session log — which within a namespace is one tenant reading their own
/// data, but is also a conversation neither of them asked to join. Fresh per
/// request is the reading that costs a cold prefix and surprises nobody.
fn anonymous_key() -> String {
    format!(
        "anonymous-{}-{}-{}",
        std::process::id(),
        now_ms(),
        ANONYMOUS.fetch_add(1, Ordering::Relaxed)
    )
}

// ---------------------------------------------------------------------------
// POST /v1/messages/count_tokens
// ---------------------------------------------------------------------------

/// `POST /v1/messages/count_tokens`
///
/// **An estimate, and it says so.** The number is this deployment's own
/// tokenizer counting [`Item::render`]ed items — the same function
/// [`Engine::admitted_input_tokens`] prices a turn with, so it is exactly what
/// roundhouse will bill and admit — and it is *not* what the model this turn
/// gets routed to would count. Two vocabularies are in play at once: the
/// client's question is "how much of Anthropic's context will this fill", and a
/// turn routed to a local model is measured by that model's tokenizer instead
/// (Dive D §5). The number is therefore a planning aid, never a quota.
///
/// **Served rather than refused, and that is a cost decision.** When
/// `count_tokens` fails, Claude Code falls back to a real one-token `create`
/// against the routed model (§2.4) — so refusing this endpoint does not save
/// the estimate's cost, it converts it into a dispatch. For the same reason the
/// fair-use ceiling is *not* applied here: refusing a rate-limited agent's
/// estimate would push it into the path that spends money, which is the
/// opposite of what a ceiling is for. Authentication still applies, because
/// tokenizing an arbitrary body is work a stranger may not ask this process to
/// do.
async fn count_tokens<S, T>(
    State(state): State<Messages<S, T>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, MessagesError>
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    let plane = state.planes.plane(now_ms());
    plane.turn_admission(&headers)?;
    let params: CreateMessageParams = parse_body(&body)?;
    let claimed = canonicalize(&params)?;
    Ok(axum::Json(json!({
        "input_tokens": state.engine.admitted_input_tokens(&claimed),
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Pre-stream refusals, in Anthropic's envelope
// ---------------------------------------------------------------------------

/// An [`ApiError`] rendered the way this dialect's client parses one.
///
/// A newtype rather than a second error type: every refusal on this path is
/// raised by machinery shared with the other surfaces — the key table, the
/// fair-use ledger, the body parser, the canonicalizer — and a parallel
/// vocabulary would mean each of those refusals existed twice, with the second
/// copy free to drift. What changes here is the *envelope*, not the decision.
///
/// **§3.7's rule is about not wrapping somebody else's error, and it is
/// respected by construction here**: nothing on this path forwards an upstream
/// body. Every error this renders is roundhouse's own, so putting it in
/// Anthropic's shape is translation rather than encapsulation — and the retry
/// vocabulary below is chosen so the client's own recovery logic reads it
/// correctly, which is the whole point of the rule.
#[derive(Debug)]
struct MessagesError(ApiError);

impl From<ApiError> for MessagesError {
    fn from(error: ApiError) -> Self {
        Self(error)
    }
}

/// The auth vocabulary, through the shared conversion rather than around it.
///
/// [`AuthError`] already knows its own status and code — one table, read by
/// every surface — so this hop exists only to put the result in this module's
/// newtype. A direct `AuthError` → envelope mapping here would be a second
/// place where a revoked key's status is decided.
impl From<AuthError> for MessagesError {
    fn from(error: AuthError) -> Self {
        Self(error.into())
    }
}

/// The error `type` a status maps to.
///
/// Anthropic's published vocabulary, chosen for what the *client* does with each
/// value rather than for descriptive accuracy (§2.5's retry predicate):
///
/// - `rate_limit_error` on 429 — under subscription OAuth this is not retried by
///   the backoff at all but routed to the rate-limit UI, which reads
///   `retry-after`; that is why the header below exists.
/// - `overloaded_error` **only** on 503, where a retry is what we want. It is the
///   one spelling the client treats as retryable regardless of status, so
///   putting it on anything else would make a permanent refusal loop.
/// - `api_error` for every other 5xx: a fault of ours, retried by the client's
///   `status >= 500` rule without needing the overload spelling.
/// - `invalid_request_error` for the 4xx shapes, which is where a 422 from the
///   canonicalizer lands. Not retried, correctly: no amount of waiting fixes a
///   body.
fn error_kind(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::NOT_FOUND => "not_found_error",
        StatusCode::PAYLOAD_TOO_LARGE => "request_too_large",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        StatusCode::SERVICE_UNAVAILABLE => emit::OVERLOADED_ERROR,
        other if other.is_server_error() => emit::API_ERROR,
        // Every remaining 4xx, and anything else that reaches here. A 2xx or 3xx
        // cannot: this type is only constructed from an `ApiError`, whose every
        // constructor names a failure status.
        _ => "invalid_request_error",
    }
}

impl IntoResponse for MessagesError {
    fn into_response(self) -> Response {
        let status = self.0.status();
        let mut error = json!({
            "type": error_kind(status),
            "message": self.0.message(),
            // Roundhouse's own refusal code, beside the wire vocabulary rather
            // than in place of it. The `type` is what the client branches on and
            // is drawn from Anthropic's small closed set; the code is what an
            // operator greps for and distinguishes the dozen refusals that share
            // one status. Under its own key so neither can ever be read as the
            // other.
            "roundhouse_code": self.0.code(),
        });
        // The fair-use refusal's machine-readable fields — `resets_at`, the
        // window, the scope — travel through unchanged. They are the only part
        // of a refusal an *agent* rather than a person acts on, and dropping
        // them at the envelope would leave this dialect's clients guessing at a
        // time the other dialect's are told.
        if let (Some(Value::Object(fields)), Some(error)) = (self.0.detail(), error.as_object_mut())
        {
            for (name, value) in fields {
                error.entry(name.clone()).or_insert(value.clone());
            }
        }
        let body = json!({ "type": "error", "error": error });

        let mut response = (status, axum::Json(body)).into_response();
        // **Where the client actually reads a retry time.** Its backoff takes
        // `retry-after` in seconds when present (§2.5's `Dp`), and under
        // subscription OAuth the 429 path sleeps on the same header, defaulting
        // to *thirty minutes* when it is absent. So a fair-use window of two
        // minutes reported only in the body is a two-minute ceiling the client
        // waits half an hour on. The seconds come from `resets_at`, which
        // `refuse_over_fair_use` already rounded *up* from the millisecond
        // floor — rounding here as well would be a second convention.
        if let Some(seconds) = retry_after_seconds(&self.0)
            && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
        {
            response.headers_mut().insert("retry-after", value);
        }
        response
    }
}

/// The `retry-after` a refusal carries, in whole seconds.
///
/// Read off the same `resets_at` the body reports rather than recomputed, so
/// header and body can never name two different times — which is the failure a
/// client would resolve by trusting the header and being refused again.
fn retry_after_seconds(error: &ApiError) -> Option<u64> {
    let resets_at = error.detail()?.get("resets_at")?.as_u64()?;
    Some(seconds_until(resets_at, now_ms()))
}

/// Whole seconds from `now_ms` until the unix second `resets_at`.
///
/// Split out because it is arithmetic with a direction, and the direction is the
/// only thing about it that can be wrong. `resets_at` was rounded *up* to the
/// next whole second by `refuse_over_fair_use`, from a millisecond figure that
/// is a floor; the current time must therefore be rounded *down*, so the wait
/// this reports is never shorter than the real one. Rounding both ends up loses
/// as much as a second, and a client that waits a second too few is refused
/// again for no reason but our own arithmetic — one wasted round trip per
/// refusal, on the path a rate-limited agent takes most often.
///
/// Saturating on the other side: a ceiling that has already cleared is not a
/// wait, and a past reset is an ordinary race rather than a fault.
fn seconds_until(resets_at: u64, now_ms: u64) -> u64 {
    resets_at.saturating_sub(now_ms / 1_000)
}

// ---------------------------------------------------------------------------
// Following the log as a Message
// ---------------------------------------------------------------------------

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
struct MessagesFollower<S: SessionStore, T: Tokenizer + Clone> {
    tail: LogTail<S>,
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
                self.consume(&events, None);
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
                self.consume(&events, Some(bound));
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
    fn consume(&mut self, events: &[SessionEvent], bound: Option<u64>) {
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
            if let SessionEventKind::ItemAppended { item } = &event.kind
                && self.emission.seam_answer(item).is_some()
            {
                let contribution = self
                    .engine
                    .context_contribution(self.admitted_input_tokens, item);
                self.emission.report_instead(contribution);
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
    /// reach it without waiting. See [`KEEPALIVE_INTERVAL`] for the cadence and
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

    fn into_stream(self) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
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
async fn complete_message<S, T>(
    mut follower: MessagesFollower<S, T>,
) -> Result<Response, MessagesError>
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    let mut message: Option<Message> = None;
    let mut text = String::new();
    let mut stop_reason = StopReason::EndTurn;
    let mut usage = None;
    while let Some(frame) = follower.next_frame().await {
        match frame.event() {
            StreamEvent::MessageStart { message: start, .. } => message = Some(start.clone()),
            StreamEvent::ContentBlockDelta { delta, .. } => {
                if let BlockDelta::TextDelta { text: chunk, .. } = delta {
                    text.push_str(chunk);
                }
            }
            StreamEvent::MessageDelta {
                delta,
                usage: reported,
                ..
            } => {
                if let Some(reason) = &delta.stop_reason {
                    stop_reason = reason.clone();
                }
                // Merged the way the client's own accumulator merges it (§3.4):
                // the terminal frame's counts replace the prelude's, and its
                // absence leaves the prelude's standing. Reproducing the merge
                // here rather than reading the log again is what keeps the two
                // renderings one projection.
                if reported.is_some() {
                    usage = reported.clone();
                }
            }
            StreamEvent::Error { error, .. } => {
                return Err(MessagesError(mid_stream_failure(error)));
            }
            StreamEvent::ContentBlockStart { .. }
            | StreamEvent::ContentBlockStop { .. }
            | StreamEvent::MessageStop { .. }
            | StreamEvent::Ping { .. } => {}
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
    message.content = vec![ContentBlock::text(text)];
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

    #[test]
    fn the_two_routes_sit_under_the_prefix_the_client_appends() {
        // Claude Code posts to `{base_url}/v1/messages`, so the segment is the
        // client's to choose and ours to match. Concatenated rather than joined
        // for the reason `API_PREFIX`'s own test gives: a doubled or missing
        // slash serves a path nobody requests.
        assert_eq!(format!("{API_PREFIX}/{MESSAGES_PATH}"), "/v1/messages");
        assert_eq!(
            format!("{API_PREFIX}/{COUNT_TOKENS_PATH}"),
            "/v1/messages/count_tokens"
        );
    }

    #[test]
    fn every_refusal_status_maps_to_a_type_the_client_knows() {
        // The vocabulary is small and closed on the client's side, and the two
        // entries that matter are the ones a wrong answer makes expensive:
        // `overloaded_error` anywhere but 503 is an infinite retry, and a 429
        // spelled anything but `rate_limit_error` is a ceiling the rate-limit UI
        // never shows.
        for (status, expected) in [
            (StatusCode::BAD_REQUEST, "invalid_request_error"),
            (StatusCode::UNAUTHORIZED, "authentication_error"),
            (StatusCode::FORBIDDEN, "permission_error"),
            (StatusCode::NOT_FOUND, "not_found_error"),
            (StatusCode::CONFLICT, "invalid_request_error"),
            (StatusCode::PAYLOAD_TOO_LARGE, "request_too_large"),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_request_error"),
            (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
            (StatusCode::INTERNAL_SERVER_ERROR, "api_error"),
            (StatusCode::NOT_IMPLEMENTED, "api_error"),
            (StatusCode::SERVICE_UNAVAILABLE, "overloaded_error"),
        ] {
            assert_eq!(error_kind(status), expected, "for {status}");
        }
    }

    #[test]
    fn only_the_service_unavailable_refusal_is_spelled_retryable() {
        // The partition, not the table: `overloaded_error` is the one string
        // Claude Code retries on regardless of status (§3.2), so exactly one
        // pre-stream refusal may carry it.
        let overloaded: Vec<u16> = (400u16..600)
            .filter_map(|code| StatusCode::from_u16(code).ok())
            .filter(|status| error_kind(*status) == emit::OVERLOADED_ERROR)
            .map(|status| status.as_u16())
            .collect();
        assert_eq!(overloaded, vec![503]);
    }

    #[test]
    fn a_retry_time_never_rounds_a_client_into_a_second_refusal() {
        // The refusal's own convention: `resets_at` is `retry_at_ms` rounded up
        // to the next whole second. Take a clock that is *not* on a second
        // boundary, because that is the only case where the two roundings
        // differ — and a test at 10 000 ms would have passed for both.
        let now_ms: u64 = 10_500;
        let retry_at_ms: u64 = 12_000;
        let resets_at = retry_at_ms.div_ceil(1_000);
        assert_eq!(
            seconds_until(resets_at, now_ms),
            2,
            "1.5 s of window left rounds up to a 2 s wait"
        );

        // The failure this direction prevents, made concrete: rounding the
        // current time up as well reports one second, and the client's retry
        // lands 500 ms before the window has room and is refused again.
        assert_eq!(resets_at.saturating_sub(now_ms.div_ceil(1_000)), 1);

        // A whole-second boundary is not stretched by a second.
        assert_eq!(seconds_until(12, 10_000), 2);
        // And a window that has already cleared is no wait at all.
        assert_eq!(seconds_until(9, 10_000), 0);
    }

    #[test]
    fn the_keepalive_fires_inside_the_windows_at_both_ends() {
        // Not on the first quiet poll: a ping every 25 ms is a stream of frames
        // a chained Relay re-encodes for nothing.
        assert!(!keepalive_due(1));
        // And well inside the client's 300-second abort — with room to spare
        // against v2.1.42's 30-second stall log, which is the shorter of the two
        // and the one that makes a stalled upstream visible rather than fatal.
        let polls = (1..).find(|polls| keepalive_due(*polls)).expect("it fires");
        let after = POLL_INTERVAL * polls;
        assert!(
            after <= Duration::from_secs(30),
            "the first keepalive lands after {after:?}, past the stall threshold"
        );
        assert!(
            after >= Duration::from_secs(1),
            "and not so early that an ordinary turn is peppered with pings: {after:?}"
        );
    }

    #[test]
    fn an_anonymous_name_is_fresh_on_every_call() {
        let first = anonymous_key();
        let second = anonymous_key();
        assert_ne!(
            first, second,
            "two anonymous turns inside one millisecond must not share a session"
        );
        assert!(first.starts_with("anonymous-"), "{first}");
    }
}
