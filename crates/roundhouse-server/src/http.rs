// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! HTTP/SSE transport.
//!
//! The event log *is* the streaming bus. A handler never holds a channel to the
//! engine: it records the session's sequence number, spawns the turn, and then
//! tails [`SessionStore::read_events`] exactly as a reconnecting client would.
//! That is what collapses SSE resumption, reconnect replay, and the audit trail
//! into one mechanism instead of three that have to be kept in agreement — a
//! stream fed from an in-process channel would diverge from the log the moment
//! a turn outlived the connection that started it, and there would be no way to
//! tell which of the two was right.
//!
//! Reads take no lease. A lease exists to make writes single-writer; a reader
//! that took one would evict the very engine it is watching.
//!
//! Following the log is a [`POLL_INTERVAL`] poll over batches of
//! [`READ_BATCH`]. That is the in-process placeholder for a store-side notify,
//! which arrives with the Redis backend; putting a subscription method on
//! [`SessionStore`] now would shape the trait around a backend that does not
//! exist yet, and [`MemoryStore`](roundhouse_core::store::MemoryStore) could
//! not honor it.
//!
//! What a second transport needs is `pub(crate)` rather than private:
//! [`responses_api`](crate::responses_api) follows the same log and answers the
//! same pre-stream failures, and a second copy of the cursor or of the error
//! vocabulary would be a second thing to keep in agreement with this one.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use roundhouse_core::context::Tokenizer;
use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::item::{Item, Role};
use roundhouse_core::store::{SessionStore, StoreError};

use crate::engine::Engine;

/// How long to wait before re-reading a log that had nothing new.
///
/// Short enough that a client cannot perceive the added latency on a token
/// delta, long enough that an idle stream is not a busy loop against the store.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Events read per store round trip.
pub(crate) const READ_BATCH: usize = 256;

/// How long a settled stream waits for its turn task to unwind.
///
/// The terminal event has already closed the response, so this only bounds how
/// long we will hold the connection open to learn *why* a turn failed. A task
/// that takes longer than this has stopped being the client's problem.
const SETTLE_GRACE: Duration = Duration::from_millis(500);

/// Engine and store handles shared by every handler.
///
/// `Clone` is written out rather than derived: deriving would demand `S: Clone`
/// of a store that is only ever shared behind an [`Arc`].
struct Transport<S: SessionStore, T: Tokenizer + Clone> {
    engine: Arc<Engine<S, T>>,
    store: Arc<S>,
}

impl<S: SessionStore, T: Tokenizer + Clone> Clone for Transport<S, T> {
    fn clone(&self) -> Self {
        Self {
            engine: Arc::clone(&self.engine),
            store: Arc::clone(&self.store),
        }
    }
}

/// The transport's routes.
///
/// The store is passed alongside the engine rather than borrowed out of it: the
/// streaming endpoints only read, and reading through the engine would suggest
/// a coupling to turn execution that deliberately does not exist.
pub fn router<S, T>(engine: Arc<Engine<S, T>>, store: Arc<S>) -> Router
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/v1/sessions", post(create_session::<S, T>))
        .route(
            "/v1/sessions/{session_id}/responses",
            post(create_response::<S, T>),
        )
        .route(
            "/v1/sessions/{session_id}/events",
            get(session_events::<S, T>),
        )
        .with_state(Transport { engine, store })
}

// ---------------------------------------------------------------------------
// Request and reply bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct CreateSessionBody {
    /// Adopt a client-supplied id instead of minting one.
    ///
    /// A client that generated its own id before its first request can retry
    /// that request after a network failure without risking two sessions.
    session_id: Option<SessionId>,
}

#[derive(Debug, Serialize)]
struct CreateSessionReply {
    session_id: SessionId,
    created: bool,
}

#[derive(Debug, Deserialize)]
struct CreateResponseBody {
    turn_id: TurnId,
    input: Vec<InputItem>,
}

#[derive(Debug, Deserialize)]
struct InputItem {
    role: Role,
    text: String,
}

impl InputItem {
    /// Convert to the canonical item shape, refusing roles the transport cannot
    /// yet round-trip.
    ///
    /// Assistant items belong to the engine, which stamps them with the
    /// response that produced them; tool items need a call id the wire format
    /// does not carry yet. Accepting either here would let a client write
    /// history the log has no way to attribute.
    fn into_item(self) -> Result<Item, ApiError> {
        match self.role {
            Role::User => Ok(Item::user_text(self.text)),
            Role::System => Ok(Item::system_text(self.text)),
            other => Err(ApiError::unprocessable(format!(
                "role `{}` is not accepted as input",
                other.as_str()
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors that happen before anything is streamed
// ---------------------------------------------------------------------------

/// A request that never reached the log.
///
/// Once a turn is admitted its failures are log events, and the stream carries
/// them; this covers only what can go wrong first — a body we cannot read, a
/// session that does not exist, a store that is down.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn not_found(session_id: &SessionId) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "session_not_found",
            message: format!("session `{session_id}` not found"),
        }
    }

    pub(crate) fn unprocessable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "invalid_request",
            message: message.into(),
        }
    }

    pub(crate) fn internal(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "code": self.code, "message": self.message } });
        (self.status, axum::Json(body)).into_response()
    }
}

/// Classify a store failure raised before streaming began.
pub(crate) fn store_error(session_id: &SessionId, error: StoreError) -> ApiError {
    match error {
        StoreError::SessionNotFound(_) => ApiError::not_found(session_id),
        other => ApiError::internal("store_error", other.to_string()),
    }
}

/// Parse a JSON request body.
///
/// Done by hand rather than through [`axum::Json`] so that a missing or wrong
/// `Content-Type` is not a separate failure mode from malformed JSON: both are
/// the client sending something this endpoint cannot read, and both are 422.
pub(crate) fn parse_body<B: serde::de::DeserializeOwned>(body: &Bytes) -> Result<B, ApiError> {
    serde_json::from_slice(body)
        .map_err(|error| ApiError::unprocessable(format!("malformed request body: {error}")))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /v1/sessions`
///
/// The reply reports `created` rather than distinguishing the two outcomes by
/// status code: adopting an id that already exists is a successful retry, not a
/// creation, and a client that retried cannot tell those apart from a 201.
async fn create_session<S, T>(
    State(state): State<Transport<S, T>>,
    body: Bytes,
) -> Result<Response, ApiError>
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    let request: CreateSessionBody = if body.is_empty() {
        CreateSessionBody::default()
    } else {
        parse_body(&body)?
    };

    let session_id = request.session_id.unwrap_or_else(SessionId::generate);
    let created = state
        .engine
        .create_session(&session_id)
        .await
        .map_err(|error| ApiError::internal("engine_error", error.to_string()))?;

    Ok(axum::Json(CreateSessionReply {
        session_id,
        created,
    })
    .into_response())
}

/// `POST /v1/sessions/{session_id}/responses`
///
/// The body is validated before the session is looked up, because it is a pure
/// function of the request and needs no store round trip to reject.
async fn create_response<S, T>(
    State(state): State<Transport<S, T>>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError>
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    let session_id = SessionId::new(session_id);
    let request: CreateResponseBody = parse_body(&body)?;
    let input = request
        .input
        .into_iter()
        .map(InputItem::into_item)
        .collect::<Result<Vec<_>, _>>()?;

    // Read before the spawn. An event appended between this read and the start
    // of the turn would fall outside the stream's window and be lost to this
    // client, which is the one gap the log cannot repair after the fact.
    let start = state
        .store
        .last_seq(&session_id)
        .await
        .map_err(|error| store_error(&session_id, error))?;

    let turn_id = request.turn_id;
    let turn = tokio::spawn({
        let engine = Arc::clone(&state.engine);
        let session_id = session_id.clone();
        let turn_id = turn_id.clone();
        // Flattened to `Result<(), String>` at the spawn boundary: the stream
        // needs to know only that the turn failed and how that reads, so it
        // does not depend on the engine's result or error shape.
        async move {
            engine
                .run_turn(&session_id, turn_id, input)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
    });

    let follower = ResponseFollower {
        tail: LogTail::new(Arc::clone(&state.store), session_id, start),
        turn_id,
        response_id: None,
        turn,
        queued: VecDeque::new(),
        phase: Phase::Tailing,
    };

    Ok(Sse::new(follower.into_stream())
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// `GET /v1/sessions/{session_id}/events`
async fn session_events<S, T>(
    State(state): State<Transport<S, T>>,
    Path(session_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, ApiError>
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    let session_id = SessionId::new(session_id);
    let cursor = resume_cursor(&params, &headers)?;

    // Existence probe. A stream opened on a session that does not exist would
    // otherwise poll an error forever instead of answering the client.
    state
        .store
        .last_seq(&session_id)
        .await
        .map_err(|error| store_error(&session_id, error))?;

    let follower = Follower {
        tail: LogTail::new(Arc::clone(&state.store), session_id, cursor),
        queued: VecDeque::new(),
        closed: false,
    };

    Ok(Sse::new(follower.into_stream())
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// Resolve where a replay starts, exclusive.
///
/// `starting_after` wins over `Last-Event-ID` because the query parameter is an
/// explicit choice made by this request, while the header is whatever the
/// browser remembered from the connection before. A cursor that does not parse
/// is refused rather than defaulted to zero: silently restarting from the
/// beginning would hand the client an entire session it did not ask for, and it
/// would look like a successful resume.
fn resume_cursor(params: &HashMap<String, String>, headers: &HeaderMap) -> Result<u64, ApiError> {
    let supplied = match params.get("starting_after") {
        Some(value) => Some(("starting_after", value.clone())),
        None => match headers.get("last-event-id") {
            Some(value) => {
                let value = value
                    .to_str()
                    .map_err(|_| ApiError::unprocessable("`Last-Event-ID` must be ASCII"))?;
                Some(("Last-Event-ID", value.to_string()))
            }
            None => None,
        },
    };

    match supplied {
        None => Ok(0),
        Some((field, value)) => value.parse().map_err(|_| {
            ApiError::unprocessable(format!(
                "`{field}` must be a sequence number, got `{value}`"
            ))
        }),
    }
}

// ---------------------------------------------------------------------------
// Following the log
// ---------------------------------------------------------------------------

/// A read cursor over one session's log.
pub(crate) struct LogTail<S: SessionStore> {
    store: Arc<S>,
    session_id: SessionId,
    cursor: u64,
}

impl<S: SessionStore> LogTail<S> {
    pub(crate) fn new(store: Arc<S>, session_id: SessionId, after_seq: u64) -> Self {
        Self {
            store,
            session_id,
            cursor: after_seq,
        }
    }

    /// One batch after `after_seq`, leaving the follow cursor alone.
    pub(crate) async fn read(&self, after_seq: u64) -> Result<Vec<SessionEvent>, StoreError> {
        self.store
            .read_events(&self.session_id, after_seq, READ_BATCH)
            .await
    }

    /// Everything appended since the last call, advancing the follow cursor.
    pub(crate) async fn drain(&mut self) -> Result<Vec<SessionEvent>, StoreError> {
        let events = self.read(self.cursor).await?;
        if let Some(last) = events.last() {
            self.cursor = last.seq;
        }
        Ok(events)
    }
}

/// Replays from a cursor and then follows the log indefinitely.
///
/// Nothing but a store failure ends this stream; the client closes it. That is
/// the point of the endpoint — a client holds one connection across turns it
/// did not itself start.
struct Follower<S: SessionStore> {
    tail: LogTail<S>,
    queued: VecDeque<Event>,
    closed: bool,
}

impl<S: SessionStore> Follower<S> {
    async fn next_frame(&mut self) -> Option<Event> {
        loop {
            if let Some(frame) = self.queued.pop_front() {
                return Some(frame);
            }
            if self.closed {
                return None;
            }
            match self.tail.drain().await {
                Ok(events) if events.is_empty() => tokio::time::sleep(POLL_INTERVAL).await,
                Ok(events) => self.queued.extend(events.iter().map(frame_for)),
                Err(error) => {
                    self.queued.push_back(error_frame(&error.to_string()));
                    self.closed = true;
                }
            }
        }
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

/// Where a [`ResponseFollower`] is in its life.
///
/// The follower is a linear machine — it tails, then it either replays a
/// deduplicated response or settles a fresh one, then it is done — and holding
/// that as one value keeps every transition an assignment to one field instead
/// of an interaction between flags.
enum Phase {
    /// Following new appends, waiting for this turn's terminal event.
    Tailing,
    /// The turn was deduplicated onto an earlier response; its log entries are
    /// being re-read in batches. `bound` is the sequence number of the
    /// `turn_deduplicated` event, past which nothing can belong to the replay.
    Replaying {
        response_id: ResponseId,
        cursor: u64,
        bound: u64,
    },
    /// The terminal frame is queued; only the turn's own outcome is still owed.
    Settling,
    /// Nothing more will be queued; the stream ends when the queue empties.
    Done,
}

/// Streams the response one `POST .../responses` asked for.
///
/// The turn runs in a task this follower never aborts. Dropping the handle
/// detaches rather than cancels, which is what a durable log demands: a client
/// that hangs up must not take the turn down with it, because the turn is
/// already admitted and a client that reconnects is owed its outcome.
struct ResponseFollower<S: SessionStore> {
    tail: LogTail<S>,
    turn_id: TurnId,
    /// Set by the `turn_started` naming this request's turn.
    response_id: Option<ResponseId>,
    turn: JoinHandle<Result<(), String>>,
    queued: VecDeque<Event>,
    phase: Phase,
}

impl<S: SessionStore> ResponseFollower<S> {
    async fn next_frame(&mut self) -> Option<Event> {
        loop {
            if let Some(frame) = self.queued.pop_front() {
                return Some(frame);
            }
            match &self.phase {
                Phase::Tailing => self.tail_once().await,
                Phase::Replaying { .. } => self.replay_once().await,
                // Only once the terminal frame is out the door: the client is
                // waiting on it, and it must not be held back to learn why a
                // turn the log has already closed went the way it did.
                Phase::Settling => {
                    self.phase = Phase::Done;
                    self.collect_outcome().await;
                }
                Phase::Done => return None,
            }
        }
    }

    /// One tailing step: read new appends, or notice the turn is gone.
    async fn tail_once(&mut self) {
        // Observed before the drain, so that finished-then-empty proves the log
        // is fully read: the task's appends happen-before its completion, and a
        // check made after the drain could miss events appended in between —
        // closing the stream with the terminal frame still unread.
        let finished = self.turn.is_finished();
        match self.tail.drain().await {
            Ok(events) if events.is_empty() => {
                if finished {
                    self.close_without_terminal().await;
                } else {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
            Ok(events) => self.absorb(events),
            Err(error) => {
                self.queued.push_back(error_frame(&error.to_string()));
                self.phase = Phase::Done;
            }
        }
    }

    /// Queue a batch, stopping at whichever event ends this request's stream.
    ///
    /// Every event in the session's window goes out, not just this response's:
    /// the log is the bus, and a client watching one turn is entitled to see
    /// what else the session did while it waited.
    fn absorb(&mut self, events: Vec<SessionEvent>) {
        for event in events {
            self.queued.push_back(frame_for(&event));

            if self.response_id.is_none() {
                match &event.kind {
                    SessionEventKind::TurnStarted {
                        turn_id,
                        response_id,
                    } if *turn_id == self.turn_id => {
                        self.response_id = Some(response_id.clone());
                    }
                    SessionEventKind::TurnDeduplicated {
                        turn_id,
                        response_id,
                    } if *turn_id == self.turn_id => {
                        self.phase = Phase::Replaying {
                            response_id: response_id.clone(),
                            cursor: 0,
                            bound: event.seq,
                        };
                        return;
                    }
                    _ => {}
                }
            }

            if event.is_terminal() && event.response_id() == self.response_id.as_ref() {
                self.phase = Phase::Settling;
                return;
            }
        }
    }

    /// One replay step: deliver a batch of the deduplicated response's entries.
    ///
    /// The frames keep their original sequence numbers, because they are the
    /// log entries this client missed rather than new ones; renumbering them
    /// would advertise sequence numbers the session does not contain. The read
    /// is bounded by the `turn_deduplicated` event and paced one batch per
    /// frame demand, exactly like tailing — a whole-log sweep inside a single
    /// poll would buffer an entire session before the first byte went out.
    /// Earlier retries' `turn_deduplicated` markers carry this response's id
    /// too, and are excluded by kind: they announce a replay, they are not part
    /// of the response, and forwarding them would push the stream's end past
    /// the terminal event.
    async fn replay_once(&mut self) {
        let Phase::Replaying {
            response_id,
            cursor,
            bound,
        } = &self.phase
        else {
            return;
        };
        let (response_id, cursor, bound) = (response_id.clone(), *cursor, *bound);

        match self.tail.read(cursor).await {
            Ok(events) if events.is_empty() => self.phase = Phase::Done,
            Ok(events) => {
                let last_seq = events.last().map_or(cursor, |event| event.seq);
                self.queued.extend(
                    events
                        .iter()
                        .filter(|event| {
                            event.seq < bound
                                && event.response_id() == Some(&response_id)
                                && !matches!(event.kind, SessionEventKind::TurnDeduplicated { .. })
                        })
                        .map(frame_for),
                );
                if last_seq + 1 >= bound {
                    self.phase = Phase::Done;
                } else {
                    self.phase = Phase::Replaying {
                        response_id,
                        cursor: last_seq,
                        bound,
                    };
                }
            }
            Err(error) => {
                self.queued.push_back(error_frame(&error.to_string()));
                self.phase = Phase::Done;
            }
        }
    }

    /// Append the turn's own outcome after its response has already terminated.
    ///
    /// The terminal event is the close signal — the engine's settle seam
    /// guarantees one for every admitted turn — so the task is consulted only to
    /// name a failure, and only for as long as it takes to unwind. Past that the
    /// response is closed either way and holding the connection open buys the
    /// client nothing.
    async fn collect_outcome(&mut self) {
        match tokio::time::timeout(SETTLE_GRACE, &mut self.turn).await {
            Ok(Ok(Err(message))) => self.queued.push_back(error_frame(&message)),
            Ok(Err(join)) => self
                .queued
                .push_back(error_frame(&format!("turn task failed: {join}"))),
            Ok(Ok(Ok(()))) | Err(_) => {}
        }
    }

    /// Close a stream whose turn is gone but whose terminal event never came.
    ///
    /// Two ways here: the engine returned before writing a `turn_started` for
    /// this turn id (a lease it could not take, a session that vanished under
    /// it), or an admitted turn died without settling — fenced by a successor,
    /// so even its `mark_incomplete` was refused. Either way no terminal event
    /// is coming and nothing else would ever close this stream, so the turn's
    /// own outcome is the only answer left to give.
    async fn close_without_terminal(&mut self) {
        self.phase = Phase::Done;
        let message = match (&mut self.turn).await {
            Ok(Err(message)) => message,
            Err(join) => format!("turn task failed: {join}"),
            Ok(Ok(())) => "the turn ended without terminating its response".to_string(),
        };
        self.queued.push_back(error_frame(&message));
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

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Render one log entry as an SSE frame.
///
/// The frame's `id` is the entry's sequence number, which is exactly what a
/// client hands back as `Last-Event-ID`. Its name is the `type` tag serde
/// already writes into the data, read back out of the serialized form so the
/// two can never drift apart as event kinds are added.
fn frame_for(event: &SessionEvent) -> Event {
    let id = event.seq.to_string();
    match serde_json::to_value(event) {
        Ok(value) => {
            let name = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("message")
                .to_string();
            Event::default().id(id).event(name).data(value.to_string())
        }
        // An entry that will not encode still occupies its sequence number, so
        // it is reported in place rather than skipped: a hole here would break
        // the contiguity a resuming client's cursor depends on.
        Err(error) => Event::default()
            .id(id)
            .event("error")
            .data(error_payload(&format!(
                "event {} could not be encoded: {error}",
                event.seq
            ))),
    }
}

/// An out-of-band failure, in the log's own error vocabulary.
///
/// Deliberately carries no `id`: it is not a log entry, and giving it one would
/// advance a client's resumption cursor to a sequence number the session does
/// not have.
fn error_frame(message: &str) -> Event {
    Event::default().event("error").data(error_payload(message))
}

fn error_payload(message: &str) -> String {
    json!({ "type": "error", "message": message }).to_string()
}
