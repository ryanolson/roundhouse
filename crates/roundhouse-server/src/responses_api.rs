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
use axum::extract::State;
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

use crate::control_config::{ControlPlane, PlaneSource};
use crate::conversations::Conversations;
use crate::engine::{Engine, TurnInput};
use crate::http::{
    ApiError, LogTail, POLL_INTERVAL, READ_BATCH, parse_body, refuse_over_fair_use, store_error,
};

mod wire;
use wire::{
    canonicalize, completed_frame, created_frame, delta_frame, failed_frame, incomplete_frame,
    item_added_frame, item_done_frame,
};

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
/// Everything else a client sends — `tools`, `tool_choice`,
/// `parallel_tool_calls`, `reasoning`, `text`, `include`, `store`,
/// `client_metadata` — is accepted and ignored. Ignoring rather than rejecting
/// is the point of a compatibility surface: v1 chooses its target by routing
/// policy rather than by requested model and runs no tool loop, and a client
/// that had to strip fields before talking to us would not be a client of the
/// same API.
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
    let admitted_input_tokens = state.engine.admitted_input_tokens(&claimed);
    let (session_id, input) = state
        .bind(&plane, &admission.principal, cache_key, claimed)
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
        claimed: Vec<Item>,
    ) -> Result<(SessionId, Vec<Item>), ApiError> {
        bind_prefix(
            &self.engine,
            &self.store,
            &self.conversations,
            plane,
            principal,
            cache_key,
            claimed,
        )
        .await
    }
}

/// Resolve a cache key to the session holding its history, and to the part of
/// `claimed` that session does not have yet.
///
/// The read is unleased and therefore a snapshot: a second request on the same
/// cache key arriving before the first has appended would compute its delta
/// against a prefix that is about to grow. Serializing turns within a
/// conversation is the client's job — these APIs have no other way to order
/// them, since a turn's input is defined by the one before it — and the
/// engine's per-session gate keeps the log itself consistent regardless.
///
/// **A free function, `pub(crate)`, because prefix admission is what the two
/// dialects share and not what distinguishes them.**
/// [`messages_api`](crate::messages_api) resolves a *different* session name —
/// a header or `metadata.user_id` rather than `prompt_cache_key` — and then
/// asks exactly this question of it. A second copy would have been a second
/// answer to "does the client's history still agree with ours", and the two
/// would agree only until one of them learned something: the fork rule, the
/// stamp-blind comparison in [`same_item`], and the retry-shaped empty suffix
/// are each a decision that has to hold for a conversation *whichever* dialect
/// it was opened on — a chained Relay serves one and dispatches the other.
///
/// The client's key is namespaced by [`ControlPlane::qualify`] rather than by a
/// convention spelled here, because the id this mints is the id the native
/// surface's namespace check will later be asked about: minting and checking
/// are one function pair, and two spellings of the convention is how a
/// namespace stops being one. The plane is the handler's snapshot rather than a
/// fresh read: a session id minted under one compiled plane and checked under
/// another is a session created and immediately unreachable.
pub(crate) async fn bind_prefix<S, T>(
    engine: &Engine<S, T>,
    store: &S,
    conversations: &Conversations,
    plane: &ControlPlane,
    principal: &Principal,
    cache_key: &str,
    claimed: Vec<Item>,
) -> Result<(SessionId, Vec<Item>), ApiError>
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    // Computed once and used for both the fork counter and the session id, so
    // the two cannot key on different strings. See [`Conversations`].
    let key = plane.qualify(principal, cache_key);
    let session_id = conversations.bind(principal, &key);
    create_session(engine, &session_id).await?;
    if let Some(delta) = suffix_after(&stored_items(store, &session_id).await?, &claimed) {
        return Ok((session_id, delta));
    }

    // The client's history disagrees with what we stored — it edited or
    // compacted the conversation, so what it is asking for is not a
    // continuation of this session and appending the difference would produce a
    // conversation neither side believes in. It gets a fresh internal session,
    // which is empty and so agrees trivially; no second check is needed.
    //
    // The honest cost: the new session starts with no history, so the routing
    // ledger no longer knows any provider is warm for it and the next turn is
    // priced cold. That is the conservative direction — a ledger that claimed a
    // warm prefix for a conversation that just changed shape would be claiming a
    // cache hit nobody can serve.
    let session_id = conversations.fork(principal, &key);
    create_session(engine, &session_id).await?;
    Ok((session_id, claimed))
}

async fn create_session<S, T>(engine: &Engine<S, T>, session_id: &SessionId) -> Result<(), ApiError>
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    engine
        .create_session(session_id)
        .await
        .map(|_| ())
        .map_err(|error| ApiError::internal("engine_error", error.to_string()))
}

/// The session's committed conversation, projected from the log.
///
/// A projection rather than a [`Session`](roundhouse_core::session::Session):
/// opening one takes the lease, and a read that took the lease would evict the
/// turn it is about to start.
async fn stored_items<S: SessionStore>(
    store: &S,
    session_id: &SessionId,
) -> Result<Vec<Item>, ApiError> {
    let mut items = Vec::new();
    let mut cursor = 0u64;
    loop {
        let batch = store
            .read_events(session_id, cursor, READ_BATCH)
            .await
            .map_err(|error| store_error(session_id, error))?;
        let Some(last) = batch.last() else { break };
        cursor = last.seq;
        items.extend(batch.into_iter().filter_map(|event| match event.kind {
            SessionEventKind::ItemAppended { item } => Some(item),
            _ => None,
        }));
    }
    Ok(items)
}

/// The part of `claimed` that `stored` does not already contain.
///
/// `None` when the two disagree anywhere they overlap. A `claimed` shorter than
/// `stored` is not a disagreement but the ordinary retry: the client is
/// re-sending a turn whose answer we already appended and it never saw, and the
/// empty suffix it yields is exactly right — the turn id will deduplicate it
/// onto the response that answer belongs to.
fn suffix_after(stored: &[Item], claimed: &[Item]) -> Option<Vec<Item>> {
    let overlap = stored.len().min(claimed.len());
    stored[..overlap]
        .iter()
        .zip(&claimed[..overlap])
        .all(|(stored, claimed)| same_item(stored, claimed))
        .then(|| claimed[overlap..].to_vec())
}

/// Item equality as this surface sees it: role and content, never the response
/// stamp.
///
/// Assistant history comes back as the model's own words with no id attached —
/// the client has no field to put one in — so comparing stamps would fail the
/// prefix check on every turn after the first.
fn same_item(stored: &Item, claimed: &Item) -> bool {
    stored.role == claimed.role && stored.content == claimed.content
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
    text: String,
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
    /// **A second arm used to live here and is gone (M10.0, T4).** A `ToolCall`
    /// bearing this response's id was projected as two `function_call` frames —
    /// outcome B, the synthetic call an agent dispatched over MCP. No seam
    /// produces one now, so the arm was unreachable rather than merely unused,
    /// and an unreachable arm in the *one* predicate that decides what goes on
    /// the wire is the kind of thing a later reader restores by accident. What
    /// it means today is stronger and is asserted as such: an item carrying a
    /// tool call can only have come from the client, so it is never projected —
    /// see `a_clients_own_tool_call_is_not_projected_as_an_emitted_one`.
    fn emitted<'a>(&'a self, item: &'a Item) -> Option<&'a str> {
        let response_id = item.response_id.as_ref()?;
        if self.response_id.as_ref() != Some(response_id) {
            return None;
        }
        match &item.content {
            ItemContent::Text { text } if !self.item_open && !text.is_empty() => Some(text),
            // The three M11.1 variants join the tool shapes here rather than
            // getting arms of their own, and the reason is the same for all
            // five: this dialect has no frame for them. A Responses client
            // asked for `response.output_text`, and a thinking block relayed as
            // one would put reasoning in the answer; relayed as anything else
            // it would be an item type the client drops in silence. They can
            // only reach a session through the Messages surface's
            // canonicalization, and that surface is where they go back out.
            ItemContent::Text { .. }
            | ItemContent::ToolCall { .. }
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
                if !self.item_open {
                    self.item_open = true;
                    self.queued.push_back(item_added_frame());
                }
                self.text.push_str(text);
                self.queued.push_back(delta_frame(text));
                Step::Continue
            }
            SessionEventKind::ResponseCompleted {
                response_id, usage, ..
            } => {
                if self.item_open {
                    self.queued.push_back(item_done_frame(&self.text));
                }
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
                // Built before either is queued, because the borrow is on this
                // follower and the queue needs it back mutably. The pair is
                // built together for a second reason too: a client announced one
                // item and handed another has no way to reconcile them. One
                // `emitted` call decides both, for the reason that predicate's
                // own doc gives: a narrowing written twice has neither copy
                // load-bearing, and a test that removed one would stay green.
                let (frames, contribution) = match self.emitted(item) {
                    Some(text) => (
                        Some([item_added_frame(), item_done_frame(text)]),
                        Some(
                            self.engine
                                .context_contribution(self.admitted_input_tokens, item),
                        ),
                    ),
                    None => (None, None),
                };
                if contribution.is_some() {
                    self.context_contribution = contribution;
                }
                self.queued.extend(frames.into_iter().flatten());
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
    use roundhouse_core::item::Role;

    use super::*;

    fn user(text: &str) -> Item {
        Item::user_text(text)
    }

    fn assistant(text: &str) -> Item {
        Item::assistant_text(text, ResponseId::new("resp_1"))
    }

    #[test]
    fn a_grown_history_yields_only_what_the_session_lacks() {
        let stored = vec![user("hello"), assistant("hi")];
        let claimed = vec![
            user("hello"),
            // The client's copy carries no response stamp; ours does.
            Item {
                role: Role::Assistant,
                content: ItemContent::Text { text: "hi".into() },
                response_id: None,
            },
            user("again"),
        ];
        assert_eq!(
            suffix_after(&stored, &claimed),
            Some(vec![user("again")]),
            "a stamped assistant item must still match the client's copy of it"
        );
    }

    #[test]
    fn a_retry_of_an_answered_turn_yields_nothing_to_append() {
        let stored = vec![user("hello"), assistant("hi")];
        // The retry predates the answer, because the client never saw it.
        assert_eq!(suffix_after(&stored, &[user("hello")]), Some(Vec::new()));
    }

    #[test]
    fn an_edited_history_is_refused_rather_than_appended() {
        let stored = vec![user("hello"), assistant("hi")];
        assert_eq!(suffix_after(&stored, &[user("goodbye")]), None);
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
