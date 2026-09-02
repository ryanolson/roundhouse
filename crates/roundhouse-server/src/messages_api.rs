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
//! # Two submodules are public, and one is not
//!
//! [`wire`] and [`emit`] are, so the conformance oracle can drive parse,
//! canonicalization and the frame projection directly from `tests/` rather than
//! only over a socket — the same reason the dispatch side's wire module is
//! public, and the reason [`http::ApiError`](crate::http::ApiError) is public as
//! a value.
//!
//! `follower` is not. It holds the cursor, the poll, the keepalive and the fold
//! that answers a non-streaming request from the same frames a stream would
//! have carried, and it came out of this file in M12 review F11 for size — but
//! the seam it came out along is real: what is left here decides whether a turn
//! may run and on whose behalf, and nothing outside this surface follows one.
//! Its own tests come with it, because the state they read (`idle_polls`, the
//! queue, the phase) is private to it and always was.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequest, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::Sse;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

use roundhouse_core::context::Tokenizer;
use roundhouse_core::event::Usage;
use roundhouse_core::now_ms;
use roundhouse_core::store::SessionStore;

use roundhouse_fleet::WireProtocol;

use crate::control_config::{AuthError, PlaneSource};
use crate::conversations::Conversations;
use crate::engine::{Engine, TurnInput};
use crate::http::{ApiError, POLL_INTERVAL, parse_body, refuse_over_fair_use, store_error};
use crate::responses_api::{API_PREFIX, bind_prefix};

pub mod emit;
// Private, unlike its two siblings: nothing outside this surface follows a
// Messages turn, and the seam exists to keep the handler's file readable rather
// than to offer anything.
mod follower;
pub mod wire;

use emit::MessageEmission;
use follower::{MessagesFollower, complete_message};
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
///
/// `pub` since M11.2b, for the reason [`API_PREFIX`] is read rather than
/// retyped in `codex_launch`: [`crate::claude_launch`] renders the URL a
/// launched client will assemble, and a second `"messages"` literal there would
/// agree with this one today and part company on the edit that moved the route
/// — silently, since the launcher's own tests would still pass.
pub const MESSAGES_PATH: &str = "messages";

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

/// The largest request body this surface will buffer.
///
/// **The platform's own number, and it is nearly sixteen times axum's.** The
/// Messages and Token Counting APIs both cap a request at 32 MB and answer a
/// larger one with `413 request_too_large` (`platform.claude.com/docs/en/api/errors`,
/// quoted in `research/claude-code-client-surface.md` §3.6). Axum's `Bytes`
/// extractor applies its own undisclosed 2 MiB default unless a route says
/// otherwise, and 2 MiB is not a hypothetical ceiling for this dialect: an
/// agentic client resends its entire history on every turn, this suite's own
/// captured two-turn fixture is already 90 KB, and a single pasted screenshot
/// is a megabyte of base64 in one block. A client that crossed the default got
/// a turn refused for a limit nobody chose, in a plain-text envelope its parser
/// cannot read (M11.1 review, F3).
///
/// Matching the platform rather than inventing a number is the whole point: a
/// proxy that refuses what the upstream would have served is a proxy that
/// changes the answer, and one that accepts what the upstream refuses just
/// moves the refusal somewhere the client cannot act on it.
///
/// **What this is also the size of, stated because raising it made it bigger.**
/// Axum runs extractors before the handler body, so the bytes are buffered
/// before [`create_message`]'s admission check runs — this number is therefore
/// what an *unauthenticated* caller can make this process hold, and it went
/// from 2 MiB to 32 MB with the ceiling. Unchanged in kind (the ordering
/// predates this constant) and matched to the upstream, which refuses at the
/// same size in front of its own API; narrowing it would need admission to be
/// a `FromRequestParts` extractor ahead of the body, which is a change to every
/// surface rather than to this line.
///
/// `pub(crate)` because [`responses_api`](crate::responses_api) takes the same
/// number: the two surfaces front the same log through the same buffering
/// extractor, and two limits would mean one client's history was servable and
/// the other's was not for no reason either client could see. One constant with
/// one citation, read twice.
pub(crate) const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

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
        // On the router and not on each route, so a third route cannot be added
        // under a limit nobody chose — which is exactly how the 2 MiB default
        // got here. See [`MAX_REQUEST_BYTES`] for the number and
        // [`RequestBody`] for what a request over it is answered with.
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(Messages {
            engine,
            store,
            planes,
            conversations,
        })
}

/// The raw request body, refused in *this dialect's* envelope.
///
/// **A newtype around [`Bytes`] rather than `Bytes` itself, and the difference
/// is the only thing it exists for.** `Bytes`'s own rejection is rendered by
/// axum, inside `FromRequest`, before a handler body — and therefore before
/// [`MessagesError::into_response`] — ever runs: a plain-text `413` reading
/// "Failed to buffer the request body: length limit exceeded", with no
/// `"type":"error"`, no `error.type` from Anthropic's vocabulary and no
/// `roundhouse_code`. Claude Code branches on that vocabulary, so a refusal
/// outside it is a refusal it cannot classify; and `error_kind`'s own
/// `PAYLOAD_TOO_LARGE => "request_too_large"` row was unreachable in production
/// for exactly as long as nothing routed a 413 through this type (M11.1
/// review, F3).
///
/// Every rejection is translated, not just the large one: whatever else can go
/// wrong buffering a body (a connection that dies mid-request) is roundhouse's
/// refusal to render too, and axum's status for it — a 400 — is kept, because
/// this is a change of envelope and never of decision.
struct RequestBody(Bytes);

impl<S> FromRequest<S> for RequestBody
where
    S: Send + Sync,
{
    type Rejection = MessagesError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Bytes::from_request(request, state).await {
            Ok(body) => Ok(Self(body)),
            Err(rejection) => {
                let status = rejection.status();
                Err(if status == StatusCode::PAYLOAD_TOO_LARGE {
                    // Our own sentence rather than axum's, because this is the
                    // one rejection a client can act on: the number is what it
                    // has to get under, and "length limit exceeded" does not
                    // say what the limit is.
                    ApiError::refused(
                        status,
                        "request_too_large",
                        format!(
                            "request body exceeds the {MAX_REQUEST_BYTES}-byte limit \
                             this endpoint accepts"
                        ),
                    )
                    .into()
                } else {
                    ApiError::refused(status, "unreadable_body", rejection.body_text()).into()
                })
            }
        }
    }
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
    RequestBody(body): RequestBody,
) -> Result<Response, MessagesError>
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    let plane = state.planes.plane(now_ms());
    let admission = plane.turn_admission(&headers)?;
    refuse_over_fair_use(&*state.engine, &admission).await?;
    let mut params: CreateMessageParams = parse_body(&body)?;

    let claimed = canonicalize(&params)?;
    let turn_id = turn_id_for(&claimed);
    // Read off `params` before the toolbox is taken out of it below, and
    // *including* that toolbox: on this dialect it is the largest part of the
    // request — 79% of a measured Claude Code turn's bytes — and the number this
    // reports is what `message_start` tells the client it admitted (M11.2a's
    // F4).
    let admitted_input_tokens = state.engine.admitted_input_tokens(
        &claimed,
        params.tools.as_ref(),
        params.tool_choice.as_ref(),
    );
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

    // **What the client will let this answer grow to**, threaded into the turn
    // and out to the dispatch as [`FrontierQuote::output_token_cap`]. Until
    // M11.1's F1 it was parsed here and read nowhere, so every dispatch carried
    // this deployment's *pricing estimate* as its ceiling — 256 tokens by
    // default — and every real answer was truncated mid-sentence while the
    // client's own `max_tokens: 64000` said otherwise.
    //
    // Three narrowings, each of which is a request this surface must not turn
    // into an upstream 400 of our own making:
    //
    // - `0` is treated as absent. The Messages schema's minimum is 1, so a zero
    //   is a client mistake, and forwarding it would refuse a turn we could
    //   otherwise serve. Ignoring it costs a ceiling nobody meant.
    // - A value past `u32::MAX` saturates rather than wrapping, because
    //   `as u32` on a `u64` here would turn "as much as you can" into a small
    //   number chosen by arithmetic.
    // - Nothing is clamped *down* to any model's real maximum. That belongs to
    //   the provider, which knows its own models and answers with a message
    //   naming the limit; a ceiling this file invented would refuse a request
    //   the upstream would have served.
    let output_token_cap = params
        .max_tokens
        .filter(|declared| *declared > 0)
        .map(|declared| u32::try_from(declared).unwrap_or(u32::MAX));

    // **The client's toolbox, on its way to the model.** Taken rather than
    // cloned: the definitions are the largest thing in a real Claude Code
    // request — twenty-four schemas, several kilobytes — and nothing below reads
    // them again. Read here for the first time in M11.2; before it they were
    // parsed by nothing, so an agentic client's whole request reached the model
    // as a transcript with no tools attached and the turn could only answer in
    // prose.
    let tools = params.tools.take();
    let tool_choice = params.tool_choice.take();

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
                        output_token_cap,
                        // **Verbatim, and not canonicalized.** Unlike the
                        // messages above — which become log items and so must
                        // pass through one canonical form — the tool
                        // definitions are a property of *this request*, not of
                        // the conversation: they are re-declared on every turn
                        // by the client and are never replayed out of the log.
                        // So there is nothing for a canonical form to buy here,
                        // and one would cost the same thing it costs everywhere
                        // else on this path: the parts of a schema this build
                        // does not model, dropped where nobody sees it.
                        tools,
                        tool_choice,
                        // **And the dialect they are written in, which this is
                        // the only layer that knows** (M11.2a, F1). Routing
                        // picks a target on price with no read of the surface
                        // that accepted the turn, and a catalog may mix
                        // dialects — the shipped example does — so without this
                        // stamp an Anthropic-shaped tool array reaches a
                        // Responses upstream and 400s every tool-using turn.
                        tools_dialect: Some(WireProtocol::AnthropicMessages),
                        // **And the sibling stamp, which is deliberately not
                        // a field here** (M12, R-M1). The *client* dialect of
                        // this surface — how a client of it spells a call to
                        // one of roundhouse's own MCP tools — is
                        // [`ClientDialect::claude_messages`], named for the
                        // surface rather than read from the deployment-wide
                        // `mcp_namespace`: one deployment serves both clients
                        // at once, so a single configured answer would be wrong
                        // for one of them on every turn. It is not carried on
                        // `TurnInput` because nothing downstream *renders* a
                        // tool call any more (R-M0 looked); what reads it is
                        // canonicalization's contract, pinned by
                        // `wire::tests::a_control_call_keeps_the_flat_name_the_client_spells`.
                    },
                    &admission,
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
    });

    let follower = MessagesFollower::new(
        Arc::clone(&state.store),
        session_id,
        start,
        Arc::clone(&state.conversations),
        admission.principal.clone(),
        Arc::clone(&state.engine),
        admitted_input_tokens,
        MessageEmission::new(
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
    );

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
    RequestBody(body): RequestBody,
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
        // Tools included, and that is what makes this answer usable at all
        // (M11.2a's F4): the client asks this question *because* it is about to
        // send its whole toolbox, which on a real Claude Code turn is 79% of the
        // request. An answer counting only the messages would have told an agent
        // its context was a fifth as full as it was — and it would have
        // disagreed with the `input_tokens` the very next `message_start`
        // reported for the same body, which is the drift one function exists to
        // prevent.
        "input_tokens": state.engine.admitted_input_tokens(
            &claimed,
            params.tools.as_ref(),
            params.tool_choice.as_ref(),
        ),
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
