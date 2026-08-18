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

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

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
use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::store::SessionStore;

use crate::control_config::ControlPlane;
use crate::engine::Engine;
use crate::http::{ApiError, LogTail, POLL_INTERVAL, READ_BATCH, parse_body, store_error};

mod wire;
use wire::{
    canonicalize, completed_frame, created_frame, delta_frame, failed_frame, incomplete_frame,
    item_added_frame, item_done_frame, turn_id_for,
};

/// Engine and store handles, plus this node's cache-key bindings.
///
/// `Clone` is written out rather than derived, for the same reason as in
/// [`http`](crate::http): deriving would demand `S: Clone` of a store that is
/// only ever shared behind an [`Arc`].
struct Compat<S: SessionStore, T: Tokenizer + Clone> {
    engine: Arc<Engine<S, T>>,
    store: Arc<S>,
    /// Who may drive this surface, and under what session namespace.
    plane: Arc<ControlPlane>,
    /// How many times each *namespaced* cache key's history has failed the
    /// prefix check.
    ///
    /// Keyed by the whole namespaced string — `{project}/{user}/{cache_key}`
    /// where there is a namespace, the bare cache key where there is not —
    /// rather than by the cache key the client sent. Two tenants both naming a
    /// conversation `main` own separate logs, and a shared fork counter would
    /// let an edited history in one of them cold-start the other: the second
    /// tenant's next request would compute a session id at a generation it
    /// never forked to, find it empty, and lose its warm prefix. One string
    /// rather than a `(Principal, key)` tuple because the same string is the
    /// session id's stem, so the counter and the id cannot be keyed on
    /// different things.
    ///
    /// Node-local, like the turn gates in [`Engine`] and for the same reason:
    /// this is process state standing in for a durable mapping the Redis store
    /// will own. Until then a client that reconnects to a different node keeps
    /// its cache key and loses only its generation, which re-derives on the
    /// first request that disagrees with the log.
    generations: Arc<Mutex<HashMap<String, u32>>>,
}

impl<S: SessionStore, T: Tokenizer + Clone> Clone for Compat<S, T> {
    fn clone(&self) -> Self {
        Self {
            engine: Arc::clone(&self.engine),
            store: Arc::clone(&self.store),
            plane: Arc::clone(&self.plane),
            generations: Arc::clone(&self.generations),
        }
    }
}

/// The compatibility surface's route, gated by a control plane.
///
/// Separate from [`http::router`](crate::http::router) rather than folded into
/// it: the two speak different vocabularies over the same log, and merging them
/// into one `Router` is the composition root's decision to make. One
/// constructor with a required plane, for the reason given there.
pub fn responses_router<S, T>(
    plane: Arc<ControlPlane>,
    engine: Arc<Engine<S, T>>,
    store: Arc<S>,
) -> Router
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/v1/responses", post(create_response::<S, T>))
        .with_state(Compat {
            engine,
            store,
            plane,
            generations: Arc::new(Mutex::new(HashMap::new())),
        })
}

// ---------------------------------------------------------------------------
// The request
// ---------------------------------------------------------------------------

/// The part of a Responses request this surface reads.
///
/// Everything else a client sends — `model`, `tools`, `tool_choice`,
/// `parallel_tool_calls`, `reasoning`, `text`, `include`, `store`,
/// `client_metadata` — is accepted and ignored. Ignoring rather than rejecting
/// is the point of a compatibility surface: v1 chooses its target by routing
/// policy rather than by requested model and runs no tool loop, and a client
/// that had to strip fields before talking to us would not be a client of the
/// same API.
#[derive(Debug, Deserialize)]
struct ResponsesRequest {
    /// The system prompt, sent whole on every turn.
    #[serde(default)]
    instructions: String,
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
    let principal = state.plane.turn_principal(&headers)?;
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
    let (session_id, input) = state.bind(&principal, cache_key, claimed).await?;

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
        let principal = principal.clone();
        async move {
            engine
                .run_turn(&session_id, turn_id, input, &principal)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
    });

    let follower = ResponsesFollower {
        tail: LogTail::new(Arc::clone(&state.store), session_id, start),
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

impl<S: SessionStore, T: Tokenizer + Clone> Compat<S, T> {
    /// Resolve a cache key to the session holding its history, and to the part
    /// of `claimed` that session does not have yet.
    ///
    /// The read is unleased and therefore a snapshot: a second request on the
    /// same cache key arriving before the first has appended would compute its
    /// delta against a prefix that is about to grow. Serializing turns within a
    /// conversation is the client's job — this API has no other way to order
    /// them, since a turn's input is defined by the one before it — and the
    /// engine's per-session gate keeps the log itself consistent regardless.
    async fn bind(
        &self,
        principal: &Principal,
        cache_key: &str,
        claimed: Vec<Item>,
    ) -> Result<(SessionId, Vec<Item>), ApiError> {
        // Computed once and used for both the fork counter and the session id,
        // so the two cannot key on different strings. See `generations`.
        let key = self.namespaced_key(principal, cache_key);
        let session_id = bound_session(&key, self.generation(&key));
        self.create(&session_id).await?;
        if let Some(delta) = suffix_after(&self.stored_items(&session_id).await?, &claimed) {
            return Ok((session_id, delta));
        }

        // The client's history disagrees with what we stored — it edited or
        // compacted the conversation, so what it is asking for is not a
        // continuation of this session and appending the difference would
        // produce a conversation neither side believes in. It gets a fresh
        // internal session, which is empty and so agrees trivially; no second
        // check is needed.
        //
        // The honest cost: the new session starts with no history, so the
        // routing ledger no longer knows any provider is warm for it and the
        // next turn is priced cold. That is the conservative direction — a
        // ledger that claimed a warm prefix for a conversation that just
        // changed shape would be claiming a cache hit nobody can serve.
        let session_id = bound_session(&key, self.next_generation(&key));
        self.create(&session_id).await?;
        Ok((session_id, claimed))
    }

    /// The client's cache key inside its caller's namespace.
    ///
    /// A cache key is chosen by the client and nothing stops two of them
    /// choosing `main`. Before namespacing, both got the session called `main`:
    /// one log, one lease, one warm prefix, and each tenant's conversation
    /// visible in the other's prompt.
    ///
    /// Deferred to [`ControlPlane::qualify`] rather than spelled here, because
    /// the id this mints is the id the native surface's namespace check will
    /// later be asked about: minting and checking are one function pair, and
    /// two spellings of the convention is how a namespace stops being one. The
    /// prefix it produces is unambiguous because a project or user id may not
    /// contain `/` — the config's slug rule is what buys that, and it is why
    /// the rule is at the config boundary rather than here.
    fn namespaced_key(&self, principal: &Principal, cache_key: &str) -> String {
        self.plane.qualify(principal, cache_key)
    }

    /// The generation this namespaced key is currently bound to.
    ///
    /// Read on every request, not just after a fork: a rebound key stays
    /// rebound, and starting each request from generation zero would compare
    /// every later turn against the history the client already abandoned and
    /// fork again, one dead session per turn.
    fn generation(&self, key: &str) -> u32 {
        self.generations
            .lock()
            .expect("generation map poisoned")
            .get(key)
            .copied()
            .unwrap_or(0)
    }

    fn next_generation(&self, key: &str) -> u32 {
        let mut generations = self.generations.lock().expect("generation map poisoned");
        let generation = generations.entry(key.to_string()).or_insert(0);
        *generation += 1;
        *generation
    }

    async fn create(&self, session_id: &SessionId) -> Result<(), ApiError> {
        self.engine
            .create_session(session_id)
            .await
            .map(|_| ())
            .map_err(|error| ApiError::internal("engine_error", error.to_string()))
    }

    /// The session's committed conversation, projected from the log.
    ///
    /// A projection rather than a [`Session`](roundhouse_core::session::Session):
    /// opening one takes the lease, and a read that took the lease would evict
    /// the turn it is about to start.
    async fn stored_items(&self, session_id: &SessionId) -> Result<Vec<Item>, ApiError> {
        let mut items = Vec::new();
        let mut cursor = 0u64;
        loop {
            let batch = self
                .store
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
}

/// This node's session id for a namespaced cache key at a given generation.
///
/// Generation zero is the key verbatim, so a session survives a process
/// restart that loses the generation map: the common case is a conversation
/// that never forked, and it re-binds to the same log.
fn bound_session(key: &str, generation: u32) -> SessionId {
    match generation {
        0 => SessionId::new(key),
        n => SessionId::new(format!("{key}#g{n}")),
    }
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
struct ResponsesFollower<S: SessionStore> {
    tail: LogTail<S>,
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

impl<S: SessionStore> ResponsesFollower<S> {
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
            SessionEventKind::SessionCreated { .. }
            | SessionEventKind::ItemAppended { .. }
            | SessionEventKind::Routed { .. }
            | SessionEventKind::Error { .. } => false,
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
            SessionEventKind::ResponseCompleted { response_id, usage } => {
                if self.item_open {
                    self.queued.push_back(item_done_frame(&self.text));
                }
                self.queued.push_back(completed_frame(response_id, usage));
                Step::End
            }
            SessionEventKind::ResponseIncomplete {
                response_id,
                reason,
                ..
            } => {
                self.queued.push_back(incomplete_frame(response_id, reason));
                Step::End
            }
            SessionEventKind::SessionCreated { .. }
            | SessionEventKind::ItemAppended { .. }
            | SessionEventKind::Routed { .. }
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
    use roundhouse_core::item::{ItemContent, Role};

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
}
