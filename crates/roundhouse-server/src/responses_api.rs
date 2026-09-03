// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The OpenAI Responses API surface.
//!
//! One endpoint, `POST /v1/responses`, streaming `response.*` events. It is a
//! second framing of the log [`http`](crate::http) already serves, not a second
//! engine: the handler records the session's sequence number, spawns the turn,
//! and tails the log exactly as that transport does.
//!
//! Two properties of this API decide everything else here. A client re-sends the
//! *whole* conversation on every turn — `previous_response_id` is a websocket
//! feature, so an HTTP client has nowhere to keep a cursor — and it names the
//! conversation with `prompt_cache_key`, which is its own session id. (A
//! configured deployment resolves that name inside the caller's namespace
//! rather than taking it verbatim — see [`Compat::namespaced_key`] — because a
//! name the client chooses is a name two clients can choose.) Against an
//! append-only log the resent history is not input: it is a claim about what the
//! session already contains. The handler checks that claim as a prefix and
//! admits only the suffix, which is what keeps one client session on one
//! Roundhouse session, and therefore on one accumulated warm prefix, instead of
//! re-appending the conversation every turn and never matching anything.
//!
//! The turn id is a content hash of that whole canonicalized conversation, which
//! is what makes this API's own retry behavior — re-POSTing after a 5xx or a
//! stream that died mid-answer — idempotent: the same conversation is the same
//! turn id, and the engine replays the response it already produced rather than
//! generating and billing a second one.
//!
//! Only `response.*` frames go out here. The log's native kinds — routing
//! decisions, item appends, the trailing error frame that names why a turn
//! failed — belong to the other transport; a client of this one is a model
//! client, it drops what it does not recognize, and the terminal event closes
//! the body before anything could follow it anyway.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures::Stream;
use serde::Deserialize;
use serde_json::Value;
use tokio::task::JoinHandle;

use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::Principal;
use roundhouse_core::event::{IncompleteReason, SessionEvent, SessionEventKind, Usage};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::item::{Item, ItemContent};
use roundhouse_core::now_ms;
use roundhouse_core::store::SessionStore;

use roundhouse_fleet::WireProtocol;

use crate::control_config::{ControlPlane, PlaneSource};
use crate::conversations::Conversations;
use crate::engine::{Engine, TurnInput};
use crate::http::{
    ApiError, LogTail, POLL_INTERVAL, parse_body, refuse_over_fair_use, store_error,
};
use crate::messages_api::MAX_REQUEST_BYTES;
use crate::prefix_admission::bind_prefix;

mod wire;
use wire::{
    call_added_frame, call_arguments_delta_frame, call_done_frame, canonicalize, completed_frame,
    created_frame, delta_frame, failed_frame, incomplete_frame, item_added_frame, item_done_frame,
    message_item_id,
};

/// What a committed item becomes on this wire, if anything.
///
/// Two shapes and not an `Option<&str>`, because the second one is not text: a
/// tool call is three fields, three frames, and — unlike a seam answer — the
/// product of a turn that really did dispatch. Making them one type is what
/// keeps [`Follower::emitted`] the single narrowing that both `concerns` and
/// `project` read, which is the property that doc insists on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Emitted<'a> {
    /// An answer committed whole rather than streamed: the interjection seam's.
    SeamText(&'a str),
    /// A call this turn's model asked the client to run.
    ToolCall {
        call_id: &'a str,
        name: &'a str,
        arguments: &'a str,
    },
}

/// The one answer to "what is this conversation's turn id".
///
/// Re-exported rather than left private because
/// [`messages_api`](crate::messages_api) mints turn ids too, and a second FNV
/// over `Item::render` would be a second answer: the id is what deduplicates a
/// client's retry onto the response it already paid for, so two dialects that
/// hashed differently would each be idempotent alone and neither across a
/// client that switched — or across the chained topology, where a roundhouse
/// serving this surface is a roundhouse dispatching the other.
///
/// It lives here because this is where it was written and because the pinned
/// hash literal that guards it lives beside it. Its natural home is
/// `roundhouse-core` beside `Item::render`, and the day a third dialect wants
/// it, moving it there — with the pin — is the change to make.
pub(crate) use wire::turn_id_for;

/// Engine and store handles, plus this node's cache-key bindings.
///
/// `Clone` is written out rather than derived, for the same reason as in
/// [`http`](crate::http): deriving would demand `S: Clone` of a store that is
/// only ever shared behind an [`Arc`].
struct Compat<S: SessionStore, T: Tokenizer + Clone> {
    engine: Arc<Engine<S, T>>,
    store: Arc<S>,
    /// Who may drive this surface, and under what session namespace.
    ///
    /// A [`PlaneSource`] rather than the compiled plane, for the reason
    /// [`http`](crate::http)'s transport holds one: a revoked key has to stop
    /// working on the surface it is being used on, and a router that captured
    /// one plane at mount time would never see the revocation.
    planes: Arc<dyn PlaneSource>,
    /// Which session this node binds a client's cache key to.
    ///
    /// Shared with the MCP control surface rather than owned here, which is why
    /// it is a constructor argument: an agent that narrows the routing of
    /// conversation `main` and a turn that then arrives on `main` have to reach
    /// one session id, generation and all. See [`Conversations`].
    conversations: Arc<Conversations>,
}

impl<S: SessionStore, T: Tokenizer + Clone> Clone for Compat<S, T> {
    fn clone(&self) -> Self {
        Self {
            engine: Arc::clone(&self.engine),
            store: Arc::clone(&self.store),
            planes: Arc::clone(&self.planes),
            conversations: Arc::clone(&self.conversations),
        }
    }
}

/// The version segment every route on this surface is served under.
///
/// A constant for the same reason [`MCP_MOUNT_PATH`](crate::mcp_api::MCP_MOUNT_PATH)
/// is one, and F14 is the miss that earned it: two unrelated places have to
/// agree on this string and only one of them is in this file. The route below
/// is where it is *served*; [`codex_launch::mcp_endpoint`](crate::codex_launch)
/// strips it off a deployment's `base_url` to recover the root the MCP surface
/// is mounted at, because `base_url` is defined as where this deployment serves
/// the Responses API. A literal in each would make a version rung — `/v2` — a
/// change that compiles, serves turns perfectly, and hands the generated config
/// an MCP url with a bogus version segment on it. The client then starts, times
/// out on MCP, and runs every turn with every steer silently unresolvable.
///
/// Deliberately *not* shared with the admin, metrics, and session routes, which
/// spell their own `/v1` in their own files: those are a separate versioning
/// surface with no coupling to `base_url`, and one constant across all of them
/// would claim a site-wide policy nobody has decided on.
pub const API_PREFIX: &str = "/v1";

/// The compatibility surface's route, gated by a control plane.
///
/// Separate from [`http::router`](crate::http::router) rather than folded into
/// it: the two speak different vocabularies over the same log, and merging them
/// into one `Router` is the composition root's decision to make. One
/// constructor with a required plane source, for the reason given there.
///
/// `conversations` is required for the same reason it is, and it is
/// supplied rather than minted here because the MCP surface reads the same
/// table: a router that made its own would be a second answer to "which session
/// is `main`?", and the two would agree only until a client edited its history.
///
/// Generic over the source and stored as `Arc<dyn PlaneSource>`, rather than
/// taking the trait object directly. `Arc<ControlPlane>` and
/// `Arc<ControlDirectory>` are both accepted and both unsize at the call site;
/// a parameter typed `Arc<dyn PlaneSource>` would not accept either through the
/// `Arc::clone(&plane)` a caller naturally writes, because the clone's own
/// return type is inferred before any coercion could apply.
pub fn responses_router<S, T, P>(
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
            &format!("{API_PREFIX}/responses"),
            post(create_response::<S, T>),
        )
        // The same 32 MB ceiling the Messages surface takes from the platform,
        // for the same reason and against the same axum default: a `Bytes`
        // extractor with no layer caps every request at an undisclosed 2 MiB,
        // and an agentic client resending its history crosses that long before
        // any provider would refuse it (M11.1 review, F3). This surface keeps
        // axum's own plain-text refusal shape — its clients read a status, not
        // a dialect-specific error envelope — so only the limit moves here.
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(Compat {
            engine,
            store,
            planes,
            conversations,
        })
}

// ---------------------------------------------------------------------------
// The request
// ---------------------------------------------------------------------------

/// The part of a Responses request this surface reads.
///
/// Everything else a client sends — `parallel_tool_calls`, `reasoning`, `text`,
/// `include`, `store`, `client_metadata` — is accepted and ignored. Ignoring
/// rather than rejecting is the point of a compatibility surface: v1 chooses its
/// target by routing policy rather than by requested model, and a client that
/// had to strip fields before talking to us would not be a client of the same
/// API. Roundhouse still runs no tool *itself*: the client does, which is why
/// `tools` is forwarded rather than acted on.
///
/// **`tools` and `tool_choice` left that list in M11.2**, and their absence from
/// it had a cost worth naming: a codex client's whole toolbox was parsed by
/// nothing, so every turn reached the model with no tools declared and could
/// only answer in prose — on the surface whose clients are agents.
///
/// **`model` was on that list until M10 and is now accepted, *recorded*, and
/// still never routed on.** The change is one word and it is the one word that
/// matters: nothing below reads it to pick a target, and the router cannot —
/// it never receives it. What it becomes is the turn's *declared baseline*, the
/// name the savings figure prices its counterfactual against, which is a
/// question only the client can answer and which ignoring the field threw away.
#[derive(Debug, Deserialize)]
struct ResponsesRequest {
    /// The system prompt, sent whole on every turn.
    #[serde(default)]
    instructions: String,
    /// What the client believes it is talking to.
    ///
    /// Recorded verbatim on the decision and read only by pricing. A client
    /// that names nothing is not a client that named the default: the
    /// counterfactual is inferred for that turn, and the log says which.
    #[serde(default)]
    model: Option<String>,
    /// The conversation so far, this turn's new items included.
    ///
    /// Held as raw JSON so an unsupported item type can be named in the refusal
    /// rather than reported as a shape mismatch somewhere inside serde.
    #[serde(default)]
    input: Vec<Value>,
    #[serde(default)]
    stream: bool,
    prompt_cache_key: Option<String>,
    /// What the client's own process can run.
    ///
    /// **New in M11.2, and until then this surface parsed no tools at all** —
    /// so a codex client's whole toolbox was accepted, ignored, and never
    /// reached the upstream, which is why a turn served through here could only
    /// answer in prose. Threaded to
    /// [`FrontierQuote::tools`](roundhouse_fleet::FrontierQuote) and nothing
    /// else; roundhouse still runs no tool itself.
    ///
    /// Raw JSON, forwarded verbatim, for the reason the Messages surface's twin
    /// field gives: roundhouse defines none of these schemas, so a typed
    /// projection could only lose what it did not model — a freeform tool's
    /// grammar, a server-tool type, whatever the next API version adds.
    #[serde(default)]
    tools: Option<Value>,
    /// How the client wants the model to choose among [`Self::tools`]. A string
    /// on some requests and an object on others, hence `Value`.
    #[serde(default)]
    tool_choice: Option<Value>,
}

/// `POST /v1/responses`
///
/// Validation order is deliberate: everything that is a pure function of the
/// request is decided before the store is touched, so a malformed request costs
/// no round trip and creates no session.
async fn create_response<S, T>(
    State(state): State<Compat<S, T>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError>
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    // Before the body is even read: an unauthenticated request must cost this
    // process a hash lookup and nothing else, and must not be able to name a
    // session — which is what parsing the body would let it do.
    //
    // The admission rather than the principal alone: one lookup answers both
    // who pays and what may be routed to, and the policy is fixed here for the
    // whole turn rather than re-read during it.
    //
    // One snapshot for the whole request: the namespace a cache key is
    // qualified into and the dialect the reply is rendered in are read off it
    // too, and two of them could disagree across a refresh.
    let plane = state.planes.plane(now_ms());
    let admission = plane.turn_admission(&headers)?;
    // Immediately after the key lookup and before anything is parsed, bound or
    // granted. A rolling fair-use window is the one refusal an *agent* rather
    // than an operator acts on, so it has to arrive as a status code with a
    // retry time — which this is the last point in the request able to produce,
    // since everything below spawns the turn and answers with a stream. See
    // `http::refuse_over_fair_use`.
    refuse_over_fair_use(&*state.engine, &admission).await?;
    let request: ResponsesRequest = parse_body(&body)?;
    if !request.stream {
        return Err(ApiError::unprocessable(
            "only streaming is implemented; set `stream` to true",
        ));
    }
    // Required, not defaulted: it is the session identity, and minting one here
    // would silently give every request its own conversation — the exact
    // failure this surface exists to avoid, and invisible from the client side
    // because every turn would still answer.
    let cache_key = request
        .prompt_cache_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            ApiError::unprocessable("`prompt_cache_key` is required: it names the session")
        })?;

    let claimed = canonicalize(&request.instructions, &request.input)?;
    let turn_id = turn_id_for(&claimed);
    // Before `bind` consumes it, and off the *claimed* conversation rather than
    // off the session's items afterwards: the two are equal by construction —
    // `bind` admits only the suffix that makes them so — and reading it here
    // needs no second trip to the store. This is the number the wire reports if
    // the interjection seam answers this turn; see
    // [`Engine::context_contribution`] for why the wire cannot just forward what
    // the log books.
    // Borrowed rather than moved, because the same declarations are handed to
    // the turn below: they are part of the request's size (M11.2a's F4) and
    // reporting an input count that omitted them would understate the largest
    // part of an agentic client's request.
    let admitted_input_tokens = state.engine.admitted_input_tokens(
        &claimed,
        request.tools.as_ref(),
        request.tool_choice.as_ref(),
    );
    let (session_id, input) = state
        .bind(
            &plane,
            &admission.principal,
            cache_key,
            codex_thread_id(&headers).as_deref(),
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

    let turn = tokio::spawn({
        let engine = Arc::clone(&state.engine);
        let session_id = session_id.clone();
        let turn_id = turn_id.clone();
        let admission = admission.clone();
        // Empty is treated as absent. A client that sends `"model": ""` has
        // named nothing, and recording the empty string would put a baseline
        // in the log that no catalog can resolve and no reader can act on.
        let declared_baseline = request.model.filter(|model| !model.trim().is_empty());
        // Moved out of the request here, next to the baseline, because both are
        // properties of *this* request rather than of the conversation the log
        // holds — nothing replays them, and nothing downstream reads them again.
        let (tools, tool_choice) = (request.tools, request.tool_choice);
        async move {
            engine
                .run_turn(
                    &session_id,
                    turn_id,
                    TurnInput {
                        items: input,
                        declared_baseline,
                        // This surface reads no ceiling off the request. The
                        // field exists because the Messages surface has one to
                        // pass (M11.1, F1); `max_output_tokens` on a Responses
                        // request stays accepted-and-ignored like the rest of
                        // the fields this compatibility surface does not read,
                        // and honouring it is a separate decision with its own
                        // test.
                        output_token_cap: None,
                        // The tools are *not* in that category any more, and
                        // the asymmetry is deliberate: a ceiling the client
                        // declared changes how much of an answer it gets, while
                        // tools it declared change whether an agentic turn can
                        // be answered at all. Dropping them made every codex
                        // turn a prose turn.
                        tools,
                        tool_choice,
                        // The mirror of the Messages surface's stamp: this
                        // surface accepted Responses-shaped tools, so that is
                        // what it declares, and a turn routed to an
                        // `anthropic_messages` catalog entry is restated by the
                        // dispatch client rather than posted as-is (M11.2a, F1).
                        tools_dialect: Some(WireProtocol::OpenAiResponses),
                    },
                    &admission,
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
    });

    let follower = ResponsesFollower {
        tail: LogTail::new(Arc::clone(&state.store), session_id, start),
        engine: Arc::clone(&state.engine),
        admitted_input_tokens,
        context_contribution: None,
        turn_id,
        response_id: None,
        turn,
        queued: VecDeque::new(),
        item_open: false,
        text: String::new(),
        message_index: 0,
        phase: Phase::Tailing,
    };

    Ok(Sse::new(follower.into_stream())
        .keep_alive(KeepAlive::default())
        .into_response())
}

// ---------------------------------------------------------------------------
// Binding a cache key to a session
// ---------------------------------------------------------------------------

impl<S: SessionStore, T: Tokenizer + Clone + Send + Sync + 'static> Compat<S, T> {
    async fn bind(
        &self,
        plane: &ControlPlane,
        principal: &Principal,
        cache_key: &str,
        thread_id: Option<&str>,
        claimed: Vec<Item>,
    ) -> Result<(SessionId, Vec<Item>), ApiError> {
        let (session_id, delta) = bind_prefix(
            &self.engine,
            &self.store,
            &self.conversations,
            plane,
            principal,
            cache_key,
            claimed,
        )
        .await?;
        // R-M9 (M12.1 review, F2): the one moment the client's thread and the
        // session it is in are both in hand. `bind_prefix` has just decided
        // which session this turn's history belongs to — including the fork,
        // which is what makes recording it here rather than before the call
        // load-bearing — and the request that carried the history also carried
        // the thread it came from.
        //
        // Here rather than inside `bind_prefix`, which the Messages surface
        // shares: a thread id is a *codex* correlator arriving on a codex
        // header, and the other dialect's clients correlate by the tool-use id
        // this deployment emitted. Threading an always-`None` argument through
        // the shared prefix-admission function would put one dialect's
        // vocabulary in the one place both dialects have to agree.
        if let Some(thread_id) = thread_id {
            self.conversations
                .bind_thread(principal, thread_id, session_id.clone())
                .await;
        }
        Ok((session_id, delta))
    }
}

/// The header codex carries its per-turn metadata in.
const CODEX_TURN_METADATA_HEADER: &str = "x-codex-turn-metadata";

/// The largest turn-metadata header this surface will parse.
///
/// Untrusted input: anything may set this header, so the work of parsing it is
/// work an unauthenticated-shaped request could ask for repeatedly. Real ones
/// are well under this — codex omits its unbounded tool inventory from the
/// header form precisely to keep it bounded
/// (`core/src/responses_metadata.rs::compatibility_headers` @ `6344a65`) — and
/// what a client loses by exceeding it is one exact correlation, falling back
/// to the R-M7 name path and then to `latest`.
const MAX_TURN_METADATA_BYTES: usize = 16 * 1024;

/// The longest thread id worth remembering. Codex's are UUIDs; this is a bound
/// on what a caller can make this node store as a map key, not a format check.
const MAX_THREAD_ID_BYTES: usize = 256;

/// The thread this turn belongs to, as codex declares it.
///
/// **Why a header rather than the body's `prompt_cache_key`** (M12.1 review,
/// F2, R-M9). At the pinned oracle (`6344a65`) the cache key is
/// `responses_metadata.session_id` (`core/src/client.rs`'s `prompt_cache_key`),
/// and every subagent of an agent family shares the root's — `AgentControl`'s
/// own comment says so (`core/src/agent/control.rs:104-110`), and
/// `core/src/session/session.rs:671-676` is where a non-root source takes it.
/// The per-thread id rides the turn separately: `THREAD_ID_KEY` puts
/// `self.thread_id` into the `x-codex-turn-metadata` payload
/// (`core/src/responses_metadata.rs:281`, built from the per-session
/// `TurnMetadataState` at `core/src/session/turn_context.rs:618-622`). So this
/// header is the only thing on the wire that tells a subagent's turn from its
/// parent's, and it is exactly what `_meta.threadId` will later quote back.
///
/// **Read leniently and used only as a lookup key.** Absent, non-UTF-8,
/// oversized, not JSON, no `thread_id`, or a `thread_id` that is not a
/// non-empty bounded string all mean the same thing — bind nothing — because
/// this is an optimization over a fall-back that already works, and a turn
/// must never be refused over metadata it did not have to send. It is never a
/// tenancy claim and never part of a session id: the binding it makes is
/// partitioned by the principal the *bearer key* resolved to, so a forged
/// header can only name a thread inside its own sender's namespace.
fn codex_thread_id(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(CODEX_TURN_METADATA_HEADER)?.to_str().ok()?;
    if raw.len() > MAX_TURN_METADATA_BYTES {
        return None;
    }
    let metadata: Value = serde_json::from_str(raw).ok()?;
    let thread_id = metadata.get("thread_id")?.as_str()?;
    (!thread_id.is_empty() && thread_id.len() <= MAX_THREAD_ID_BYTES).then(|| thread_id.to_string())
}

// ---------------------------------------------------------------------------
// Following the log as a response
// ---------------------------------------------------------------------------

/// Where a [`ResponsesFollower`] is in its life.
///
/// `Copy`, unlike its counterpart in [`http`](crate::http): the response being
/// replayed is already on the follower, so a phase is nothing but cursors.
#[derive(Clone, Copy)]
enum Phase {
    /// Following new appends, waiting for this turn's terminal event.
    Tailing,
    /// The turn was deduplicated onto an earlier response, whose entries are
    /// being re-read in batches. `bound` is the sequence number of the
    /// `turn_deduplicated` event, past which nothing can belong to the replay.
    Replaying { cursor: u64, bound: u64 },
    /// Nothing more will be queued; the stream ends when the queue empties.
    Done,
}

/// What one log entry does to this stream.
enum Step {
    Continue,
    Deduplicated {
        response_id: ResponseId,
        bound: u64,
    },
    /// The response terminated. Nothing may follow, in either direction: the
    /// terminal frame is what tells the client the answer is whole, and this
    /// API's failure frames are only reported by a client if the body then ends.
    End,
}

/// Streams one turn as a Responses API response.
///
/// The turn runs in a task this follower never aborts, for the reason
/// [`http`](crate::http) gives: dropping the handle detaches rather than
/// cancels, and a client that hangs up must not take down a turn the log has
/// already admitted.
struct ResponsesFollower<S: SessionStore, T: Tokenizer + Clone> {
    tail: LogTail<S>,
    /// Only ever asked what this deployment's tokenizer makes of an item; the
    /// turn itself runs in the task below. Held rather than a cloned tokenizer
    /// so the two estimates a seam answer reports come from the same pair of
    /// functions [`Engine::plan`] prices a dispatched turn with.
    engine: Arc<Engine<S, T>>,
    /// The input this request admitted, as [`Engine::admitted_input_tokens`]
    /// counts it. Computed once in the handler because it is a fact about the
    /// request, not about any frame.
    admitted_input_tokens: u64,
    /// Set when this response answered at the interjection seam instead of
    /// dispatching, and then reported in place of what the log booked.
    ///
    /// Carried on the follower rather than recomputed at the completion,
    /// because the item it is derived from arrives in an earlier event and the
    /// completion carries no trace of it. `None` means an ordinary dispatched
    /// turn, whose booked usage *is* its context contribution — see
    /// [`Engine::context_contribution`] for why the two part company only here.
    context_contribution: Option<Usage>,
    turn_id: TurnId,
    /// Set by the `turn_started` naming this request's turn, or by the response
    /// a `turn_deduplicated` points at.
    response_id: Option<ResponseId>,
    turn: JoinHandle<Result<(), String>>,
    queued: VecDeque<Event>,
    /// Whether `response.output_item.added` has gone out.
    ///
    /// The item is announced by the first delta rather than by the turn's start,
    /// so a turn that produces no text at all emits no item events instead of an
    /// empty message the client would have to interpret.
    item_open: bool,
    /// Deltas so far, which `response.output_item.done` repeats in full.
    ///
    /// Cleared when a message item is closed, because a response can have more
    /// than one: a turn that speaks, calls a tool and speaks again produces two,
    /// and a `done` repeating the whole turn's prose would hand the client the
    /// first run twice.
    text: String,
    /// How many message items this response has closed, which is what names the
    /// next one. See [`message_item_id`].
    message_index: usize,
    phase: Phase,
}

impl<S: SessionStore, T: Tokenizer + Clone + Send + Sync + 'static> ResponsesFollower<S, T> {
    async fn next_frame(&mut self) -> Option<Event> {
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
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
            Ok(events) => {
                for event in &events {
                    if !self.concerns(event) {
                        continue;
                    }
                    match self.project(event) {
                        Step::Continue => {}
                        Step::Deduplicated { response_id, bound } => {
                            self.response_id = Some(response_id);
                            self.phase = Phase::Replaying { cursor: 0, bound };
                            return;
                        }
                        Step::End => {
                            self.phase = Phase::Done;
                            return;
                        }
                    }
                }
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
                let mut ended = false;
                for event in &events {
                    if event.seq >= bound
                        || matches!(event.kind, SessionEventKind::TurnDeduplicated { .. })
                        || !self.concerns(event)
                    {
                        continue;
                    }
                    if matches!(self.project(event), Step::End) {
                        ended = true;
                        break;
                    }
                }
                self.phase = if ended || last_seq + 1 >= bound {
                    Phase::Done
                } else {
                    Phase::Replaying {
                        cursor: last_seq,
                        bound,
                    }
                };
            }
            Err(error) => self.fail(&error.to_string()),
        }
    }

    /// Whether an entry belongs to the response this request is streaming.
    ///
    /// Everything else in the session's window is dropped rather than forwarded:
    /// this surface has no vocabulary for a session, only for one response.
    /// Exhaustive by kind — an entry is claimed by its identity, and matching
    /// `response_id()` against `None` before this turn started would claim every
    /// session-level entry in the log.
    fn concerns(&self, event: &SessionEvent) -> bool {
        match &event.kind {
            SessionEventKind::TurnStarted { turn_id, .. }
            | SessionEventKind::TurnDeduplicated { turn_id, .. } => *turn_id == self.turn_id,
            SessionEventKind::OutputTextDelta { response_id, .. }
            | SessionEventKind::ResponseCompleted { response_id, .. }
            | SessionEventKind::ResponseIncomplete { response_id, .. } => {
                self.response_id.as_ref() == Some(response_id)
            }
            // Claimed exactly when there is something to project, asked
            // through the one predicate `project` renders from. See
            // [`Self::emitted`].
            SessionEventKind::ItemAppended { item } => self.emitted(item).is_some(),
            // The validate loop's three kinds belong to no response — they
            // answer `None` from `response_id()` — so no stream claims them.
            // That is what keeps this deployment's own bookkeeping off a
            // client's wire: a side call is money nobody asked us to spend and
            // a verdict is a decision, and neither is an answer to the turn
            // being streamed.
            SessionEventKind::SessionCreated { .. }
            | SessionEventKind::Routed { .. }
            | SessionEventKind::SideCallCompleted { .. }
            | SessionEventKind::SideCallAbandoned { .. }
            | SessionEventKind::ValidationDecided { .. }
            | SessionEventKind::Error { .. } => false,
        }
    }

    /// What *this response* emitted as an item, if this entry is one.
    ///
    /// One predicate rather than a condition in [`Self::concerns`] and a
    /// matching one in [`Self::project`]. The first draft had both, and the
    /// duplication was not merely untidy: with the narrowing written twice,
    /// neither copy was load-bearing on its own, so a test that removed one
    /// stayed green and the narrowness claim was unfalsifiable. Asking once
    /// makes "claimed" and "projected" the same answer by construction —
    /// there is no entry this stream claims and then silently drops.
    ///
    /// **Provenance is the question both arms ask**, and it is a real choice
    /// rather than a formality: a replay re-reads the whole log, so every item
    /// this session ever emitted passes through here, and only this response's
    /// may go out. An item a client sent never has a stamp at all, because
    /// canonicalization sets `None` on everything on the input path.
    ///
    /// Beyond provenance there is one further condition, and it is the whole of
    /// the narrowing: `item_open` must be false. An ordinary dispatched turn
    /// puts its answer on the wire through the delta path and *then* commits the
    /// same text as an item, so claiming it here too would emit a second
    /// `response.output_item.done` for one message — the answer arriving twice.
    /// `item_open` is true exactly when a delta has already announced that item,
    /// so the only message this arm ever claims is one no delta preceded: a
    /// completion from the interjection seam, whose text is committed whole and
    /// never streamed. Before this arm existed a halted turn streamed `created`
    /// then `completed` with no text at all — the correction sat in the log and
    /// the agent, whose loop the halt is meant to end, was handed an empty
    /// answer.
    ///
    /// **A second arm used to live here, went away in M10.0's T4, and is back
    /// for a different reason.** It was removed because the only `ToolCall` a
    /// response could stamp was the *synthetic* one an interjection emitted, and
    /// no seam produced one any more — an unreachable arm in the one predicate
    /// that decides what goes on the wire. What is stamped now is not synthetic:
    /// since M11.2 a dispatched turn's own tool calls are committed as items as
    /// the model produces them, so this arm is the ordinary agentic turn and the
    /// alternative to having it is a codex client whose tools never fire. A
    /// client's *own* tool call is still never projected — it carries no stamp,
    /// which is the first condition below and what
    /// `a_clients_own_tool_call_is_not_projected_as_an_emitted_one` asserts.
    fn emitted<'a>(&self, item: &'a Item) -> Option<Emitted<'a>> {
        let response_id = item.response_id.as_ref()?;
        if self.response_id.as_ref() != Some(response_id) {
            return None;
        }
        match &item.content {
            ItemContent::Text { text } if !self.item_open && !text.is_empty() => {
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
            // The three M11.1 variants join `ToolResult` here rather than
            // getting arms of their own, and the reason is the same for all
            // four: this dialect has no frame for them. A Responses client
            // asked for `response.output_text`, and a thinking block relayed as
            // one would put reasoning in the answer; relayed as anything else
            // it would be an item type the client drops in silence. A
            // `ToolResult` is the client's own work coming back, never
            // something this deployment emitted. They can only reach a session
            // through the Messages surface's canonicalization, and that surface
            // is where they go back out.
            ItemContent::Text { .. }
            | ItemContent::ToolResult { .. }
            | ItemContent::Thinking { .. }
            | ItemContent::RedactedThinking { .. }
            | ItemContent::Opaque { .. } => None,
        }
    }

    /// Queue what one log entry becomes on the wire.
    fn project(&mut self, event: &SessionEvent) -> Step {
        match &event.kind {
            SessionEventKind::TurnStarted { response_id, .. } => {
                self.response_id = Some(response_id.clone());
                self.queued.push_back(created_frame(response_id));
                Step::Continue
            }
            SessionEventKind::TurnDeduplicated { response_id, .. } => Step::Deduplicated {
                response_id: response_id.clone(),
                bound: event.seq,
            },
            SessionEventKind::OutputTextDelta { text, .. } => {
                // The item is announced before its first delta and never after:
                // text for an item the client was never told about has nowhere
                // to go, and clients treat that as a protocol violation rather
                // than as something to recover from.
                let id = message_item_id(self.message_index);
                if !self.item_open {
                    self.item_open = true;
                    self.queued.push_back(item_added_frame(&id));
                }
                self.text.push_str(text);
                self.queued.push_back(delta_frame(&id, text));
                Step::Continue
            }
            SessionEventKind::ResponseCompleted {
                response_id, usage, ..
            } => {
                self.close_message_item();
                // The log's number unless this response answered at the seam.
                // `unwrap_or` and not `expect`: the emission and the completion
                // land in one append batch, so a completion with no emission
                // before it is an ordinary dispatched turn and not an ordering
                // bug — and if the batch ever did arrive out of order, falling
                // back to what the log booked is the answer that still balances.
                let usage = self.context_contribution.as_ref().unwrap_or(usage);
                self.queued.push_back(completed_frame(response_id, usage));
                Step::End
            }
            // A refusal is not a truncated answer. This dialect has two
            // terminal shapes for a response that produced none — `incomplete`
            // means the model stopped short, `failed` means the request was
            // not served — and a client that read `response.incomplete` for a
            // refusal would report a model that ran out of room. The log's own
            // vocabulary is finer than the wire's here, so this is the one
            // place a reason is translated rather than forwarded.
            //
            // Two reasons land on `failed`, and they are the two under which
            // nothing was dispatched at all. They keep separate messages
            // because the remedies are opposite: a policy refusal is answered
            // by an operator widening a policy and never by retrying, and a
            // budget refusal is answered by an admin raising a limit — or by
            // waiting for the window to roll — after which the identical
            // request succeeds. Every remaining reason really is an attempt
            // that stopped, and forwards unchanged.
            SessionEventKind::ResponseIncomplete {
                response_id,
                reason:
                    reason @ (IncompleteReason::PolicyRefused | IncompleteReason::BudgetExhausted),
                // Nothing was dispatched, so there is nothing to report; and
                // this dialect's `failed` frame has no place to put usage even
                // when there is some. Bound by name so a field added here
                // cannot be dropped without someone reading this line.
                usage: _,
                ..
            } => {
                let message = match reason {
                    IncompleteReason::BudgetExhausted => {
                        "this project's budget is spent and it is configured to refuse rather \
                         than serve locally"
                    }
                    _ => "no target this key may use was admissible for this turn",
                };
                self.queued
                    .push_back(failed_frame(Some(response_id), message));
                Step::End
            }
            SessionEventKind::ResponseIncomplete {
                response_id,
                reason,
                // Deliberately not forwarded: `response.incomplete` carries no
                // usage in this dialect, and the log is where the accounting
                // for a truncated turn is read from.
                usage: _,
                ..
            } => {
                self.queued.push_back(incomplete_frame(response_id, reason));
                Step::End
            }
            // An answer this response produced at the interjection seam, which
            // `concerns` has already narrowed to exactly that. Two frames, both
            // carrying the whole message: it was committed whole rather than
            // streamed, so there are no deltas for a client to assemble it from
            // and anything not in these two frames is not on the wire.
            //
            // `item_open` is deliberately untouched. It tracks the *streamed*
            // message, whose `done` the completion below emits; a seam answer
            // produces no deltas and so leaves it false, which is what makes the
            // four-frame sequence four frames rather than five with an empty
            // message on the end.
            //
            // **Both seam answers take this path since M10.0**, and that is the
            // point of T5: a steer and a halt are one shape now — assistant text,
            // nothing dispatched, the judge's usage booked — so the usage
            // substitution below covers the steer by riding the seam the halt
            // already rode, rather than by a second arm that could drift from it.
            SessionEventKind::ItemAppended { item } => {
                // One `emitted` call decides everything this arm does, for the
                // reason that predicate's own doc gives: a narrowing written
                // twice has neither copy load-bearing, and a test that removed
                // one would stay green.
                match self.emitted(item) {
                    // An answer this response produced at the interjection seam.
                    // Two frames, both carrying the whole message: it was
                    // committed whole rather than streamed, so there are no
                    // deltas for a client to assemble it from and anything not
                    // in these two frames is not on the wire.
                    //
                    // **Both seam answers take this path since M10.0**, and that
                    // is the point of T5: a steer and a halt are one shape now —
                    // assistant text, nothing dispatched, the judge's usage
                    // booked — so the usage substitution below covers the steer
                    // by riding the seam the halt already rode, rather than by a
                    // second arm that could drift from it.
                    Some(Emitted::SeamText(text)) => {
                        // Built before either is queued, because the borrow is on
                        // this follower and the queue needs it back mutably. The
                        // pair is built together for a second reason too: a
                        // client announced one item and handed another has no way
                        // to reconcile them.
                        let contribution = self
                            .engine
                            .context_contribution(self.admitted_input_tokens, item);
                        let id = message_item_id(self.message_index);
                        // The id is spent whether or not a delta ever used it, so
                        // a later message item of the same response cannot be
                        // handed the same one.
                        self.message_index += 1;
                        self.context_contribution = Some(contribution);
                        self.queued.push_back(item_added_frame(&id));
                        self.queued.push_back(item_done_frame(&id, text));
                    }
                    // **A dispatched turn's own call, and deliberately no
                    // context substitution.** The seam answer above replaces the
                    // reported usage because its turn dispatched nothing and the
                    // log booked the judge's side call instead; this turn *did*
                    // dispatch, and what the log booked is the provider's own
                    // measured counts — exactly what the client should be told.
                    //
                    // **The open message item is closed first, and the order is
                    // the contract rather than a courtesy.** The log holds this
                    // turn's items in the order the model produced them — text,
                    // then the call — and a client rebuilds its history from the
                    // items it was handed, in the order it was handed them.
                    // Emitting the call while the message is still open would
                    // hand back `[call, message]`; the client would resend that,
                    // and the prefix check would disagree with the log at the
                    // first item, so every tool-using session forks on its second
                    // turn while every turn still answers.
                    //
                    // Three frames because the pinned codex parser reads the
                    // whole call off `output_item.done` and explicitly ignores
                    // `function_call_arguments.delta`
                    // (`codex-api/src/sse/responses.rs` @ `6344a65`, the
                    // unhandled arm), so `done` is what makes the call real; the
                    // `added` announcement and the argument delta are sent
                    // because a *streaming* consumer of this dialect renders from
                    // them, and sending only what one parser reads is how a
                    // surface stops working for the next client.
                    Some(Emitted::ToolCall {
                        call_id,
                        name,
                        arguments,
                    }) => {
                        let call = [
                            call_added_frame(call_id, name),
                            call_arguments_delta_frame(call_id, arguments),
                            call_done_frame(call_id, name, arguments),
                        ];
                        self.close_message_item();
                        self.queued.extend(call);
                    }
                    None => {}
                }
                Step::Continue
            }
            SessionEventKind::SessionCreated { .. }
            | SessionEventKind::Routed { .. }
            | SessionEventKind::SideCallCompleted { .. }
            | SessionEventKind::SideCallAbandoned { .. }
            | SessionEventKind::ValidationDecided { .. }
            | SessionEventKind::Error { .. } => Step::Continue,
        }
    }

    /// Close the open message item, if there is one.
    ///
    /// **Called from two places and it is the same act in both**: a tool call
    /// arriving mid-answer, and the completion. A message item is closed by
    /// repeating its whole text in a `response.output_item.done`, and the text
    /// buffer is emptied with it — a response can have more than one message
    /// item since M11.2, and a second `done` repeating the first run as well
    /// would hand the client the answer's opening twice.
    fn close_message_item(&mut self) {
        if !self.item_open {
            return;
        }
        let id = message_item_id(self.message_index);
        let text = std::mem::take(&mut self.text);
        self.item_open = false;
        self.message_index += 1;
        self.queued.push_back(item_done_frame(&id, &text));
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

    fn fail(&mut self, message: &str) {
        self.queued
            .push_back(failed_frame(self.response_id.as_ref(), message));
        self.phase = Phase::Done;
    }

    fn into_stream(self) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
        futures::stream::unfold(self, |mut follower| async move {
            follower
                .next_frame()
                .await
                .map(|frame| (Ok(frame), follower))
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn turn_metadata(raw: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            CODEX_TURN_METADATA_HEADER,
            HeaderValue::from_str(raw).expect("a header value fixture"),
        );
        headers
    }

    /// R-M9 (M12.1 review, F2): the ingest reads the turn's *own* thread out of
    /// codex's header, and reads nothing else out of it.
    ///
    /// The first assertion is the one with teeth. The payload carries
    /// `session_id` — the whole agent family's, and the string that becomes
    /// `prompt_cache_key` — right beside `thread_id`, so a reader that took
    /// the wrong field would bind every subagent to its parent and F2 would be
    /// re-openable with everything green.
    #[test]
    fn the_turn_metadata_header_yields_this_turns_own_thread_and_only_that() {
        let headers = turn_metadata(
            &serde_json::json!({
                "session_id": "shared-family-session",
                "thread_id": "this-agents-thread",
                "turn_id": "t1",
            })
            .to_string(),
        );
        assert_eq!(
            codex_thread_id(&headers).as_deref(),
            Some("this-agents-thread"),
            "the per-thread field, never the family-wide `session_id` sitting \
             next to it"
        );
    }

    /// The same header, read leniently: everything malformed binds nothing,
    /// and nothing refuses a turn.
    ///
    /// **Untrusted input, and the cost of getting it wrong is asymmetric.**
    /// The binding is an optimization over a fall-back that already works, so
    /// a header this function cannot make sense of must cost the client its
    /// exactness and never its turn. Each case below is a way an attacker — or
    /// a future codex — could send one.
    #[test]
    fn a_turn_metadata_header_this_surface_cannot_read_binds_nothing_and_refuses_nothing() {
        assert_eq!(codex_thread_id(&HeaderMap::new()), None, "absent");
        assert_eq!(codex_thread_id(&turn_metadata("not json")), None);
        assert_eq!(
            codex_thread_id(&turn_metadata(r#"{"session_id":"s"}"#)),
            None,
            "a payload with no `thread_id` at all"
        );
        assert_eq!(
            codex_thread_id(&turn_metadata(r#"{"thread_id":42}"#)),
            None,
            "a `thread_id` that is not a string"
        );
        assert_eq!(
            codex_thread_id(&turn_metadata(r#"{"thread_id":""}"#)),
            None,
            "an empty one, which would otherwise be a name every client shares"
        );
        assert_eq!(
            codex_thread_id(&turn_metadata(
                &serde_json::json!({ "thread_id": "t".repeat(MAX_THREAD_ID_BYTES + 1) })
                    .to_string()
            )),
            None,
            "and one longer than this node will store as a map key"
        );

        // The oversized-payload arm, which is the one that bounds *work* rather
        // than storage: a caller must not be able to make this surface parse
        // an arbitrarily large JSON document per turn.
        let bloat = serde_json::json!({
            "thread_id": "real-thread",
            "workspaces": "w".repeat(MAX_TURN_METADATA_BYTES),
        })
        .to_string();
        assert!(bloat.len() > MAX_TURN_METADATA_BYTES);
        assert_eq!(
            codex_thread_id(&turn_metadata(&bloat)),
            None,
            "over the cap the header is not parsed at all, even though a valid \
             `thread_id` is in there — the client loses one exact correlation \
             and keeps its turn"
        );
    }

    #[test]
    fn the_api_prefix_is_shaped_the_way_its_two_consumers_read_it() {
        // Both sides concatenate rather than join: this file writes
        // `{API_PREFIX}/responses`, and `codex_launch::mcp_endpoint` strips the
        // same string off the tail of a deployment's `base_url`. A missing
        // leading slash would build `v1/responses`, and a trailing one
        // `/v1//responses` — each of which serves and strips a path the other
        // side does not, which is F14's failure mode reached by a different
        // route.
        assert!(API_PREFIX.starts_with('/'), "{API_PREFIX}");
        assert!(!API_PREFIX.ends_with('/'), "{API_PREFIX}");
    }
}
