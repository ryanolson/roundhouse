// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Relay-format surface: three reads over one session's log.
//!
//! | route | document |
//! |---|---|
//! | `GET /v1/sessions/{id}/atof` | the ATOF event stream, as NDJSON |
//! | `GET /v1/sessions/{id}/trajectory` | one ATIF v1.7 trajectory |
//! | `GET /v1/sessions/{id}/optimization` | one `LlmOptimizationSummary` per turn |
//!
//! **Reads only, through the store, with no lease and no engine.** A lease exists
//! to make writes single-writer, and a reader that took one would evict the very
//! engine it is describing. Nothing here consults the turn engine, the routing
//! policy or the fleet: each document is a function of the events the store
//! returns plus the rate card, which is why the same request against a replica
//! that never served the session answers identically.
//!
//! **No state of its own.** `roundhouse_relay`'s producers are pure, so there is
//! nothing to cache and nothing to keep in agreement with anything. The one piece
//! of configuration is [`MetricsConfig`] — the rate card and the declared
//! correlaries — held here rather than read off the engine for the reason
//! [`metrics_api`](crate::metrics_api) gives: repricing history under a corrected
//! card must not mean touching the thing that serves traffic.
//!
//! # Authorization is the namespace check, and it runs first
//!
//! These routes take a session id as input and hand back the raw log projected
//! three ways — items, routing decisions, prices — so
//! [`in_namespace`](crate::http::in_namespace) *is* their authorization, exactly
//! as it is for `GET /v1/sessions/{id}/events`. It is applied **before the store
//! is touched**, so the refusal is the same whether the session exists or not: a
//! namespaced session id is `{project}/{user}/{whatever the client called its
//! conversation}` and eminently guessable, and a refusal that leaked existence
//! would be an oracle over other tenants' sessions.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use roundhouse_core::event::SessionEvent;
use roundhouse_core::ids::SessionId;
use roundhouse_core::metrics::MetricsConfig;
use roundhouse_core::now_ms;
use roundhouse_core::store::SessionStore;
use roundhouse_relay::{atif, atof, summary};

use crate::control_config::PlaneSource;
use crate::http::{ApiError, READ_BATCH, in_namespace, store_error};

/// The store and the rate card, shared by the three handlers.
///
/// `Clone` is written out rather than derived: deriving would demand `S: Clone`
/// of a store that is only ever shared behind an [`Arc`].
struct RelayState<S: SessionStore> {
    store: Arc<S>,
    pricing: Arc<MetricsConfig>,
    /// Re-resolved per request, so a key revoked over the admin plane stops
    /// reading a tenant's trajectories at the same moment it stops being able to
    /// spend on them.
    planes: Arc<dyn PlaneSource>,
}

impl<S: SessionStore> Clone for RelayState<S> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            pricing: Arc::clone(&self.pricing),
            planes: Arc::clone(&self.planes),
        }
    }
}

/// Mount the three Relay-format reads, gated by a control plane.
///
/// One constructor with a required plane, for the reason
/// [`http::router`](crate::http::router) gives: who may read a tenant's
/// trajectory is not a detail a call site should be able to leave out.
pub fn relay_router<S, P>(planes: Arc<P>, store: Arc<S>, pricing: Arc<MetricsConfig>) -> Router
where
    S: SessionStore,
    P: PlaneSource,
{
    let planes: Arc<dyn PlaneSource> = planes;
    Router::new()
        .route("/v1/sessions/{session_id}/atof", get(atof_stream::<S>))
        .route("/v1/sessions/{session_id}/trajectory", get(trajectory::<S>))
        .route(
            "/v1/sessions/{session_id}/optimization",
            get(optimization::<S>),
        )
        .with_state(RelayState {
            store,
            pricing,
            planes,
        })
}

/// `GET /v1/sessions/{session_id}/atof`
///
/// NDJSON rather than a JSON array, because that is how ATOF is stored and what
/// the NeMo-Agent-Toolkit converter's reader expects. A consumer streams it line
/// by line; an array would have to be buffered whole before the first event
/// could be handled.
async fn atof_stream<S>(
    State(state): State<RelayState<S>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError>
where
    S: SessionStore,
{
    let events = authorized_replay(&state, session_id, &headers).await?;
    let body = atof::ndjson(&atof::events(&events));
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/x-ndjson"),
            // A session is append-only, so a cached document is a truncated
            // one — and the next turn is the interesting part.
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response())
}

/// `GET /v1/sessions/{session_id}/trajectory`
async fn trajectory<S>(
    State(state): State<RelayState<S>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError>
where
    S: SessionStore,
{
    let events = authorized_replay(&state, session_id, &headers).await?;
    json(&atif::trajectory(&events))
}

/// `GET /v1/sessions/{session_id}/optimization`
///
/// An array, one entry per dispatched turn, in log order.
/// `LlmOptimizationSummary` is per-call in Relay's model, so a session is a list
/// of them rather than one summed document — and summing them here would produce
/// a figure `/v1/metrics` already publishes, computed a second way.
async fn optimization<S>(
    State(state): State<RelayState<S>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError>
where
    S: SessionStore,
{
    let events = authorized_replay(&state, session_id, &headers).await?;
    json(&summary::for_session(&events, &state.pricing))
}

/// Resolve the caller, refuse a session outside its namespace, and read the log.
///
/// The one place the three handlers share, so that "authorize, then replay"
/// cannot be got in the wrong order on one of them — which is the whole of the
/// gating, and the failure would be silent.
async fn authorized_replay<S>(
    state: &RelayState<S>,
    session_id: String,
    headers: &HeaderMap,
) -> Result<Vec<SessionEvent>, ApiError>
where
    S: SessionStore,
{
    let session_id = SessionId::new(session_id);
    let plane = state.planes.plane(now_ms()).await;
    let principal = plane.turn_principal(headers)?;
    in_namespace(&plane, &principal, &session_id)?;
    read_all(&*state.store, &session_id).await
}

/// Every event in one session, in order.
///
/// A loop, because [`SessionStore::read_events`] returns at most `limit` and a
/// single call would silently truncate any session longer than one batch — a
/// trajectory that stopped at turn forty with no error is worse than one that
/// failed. The cursor is the last event's own `seq`, and a batch that fails to
/// advance it ends the read rather than spinning: a store that returned the same
/// page forever is a bug in the store, and hanging a request on it would hide
/// that.
async fn read_all<S>(store: &S, session_id: &SessionId) -> Result<Vec<SessionEvent>, ApiError>
where
    S: SessionStore,
{
    let mut events: Vec<SessionEvent> = Vec::new();
    let mut cursor = 0u64;
    loop {
        let batch = store
            .read_events(session_id, cursor, READ_BATCH)
            .await
            .map_err(|error| store_error(session_id, error))?;
        let Some(last) = batch.last() else {
            return Ok(events);
        };
        if last.seq <= cursor {
            return Ok(events);
        }
        cursor = last.seq;
        let short = batch.len() < READ_BATCH;
        events.extend(batch);
        if short {
            return Ok(events);
        }
    }
}

/// One document, as JSON.
fn json<T: serde::Serialize>(document: &T) -> Result<Response, ApiError> {
    let body = serde_json::to_vec(document)
        .map_err(|error| ApiError::internal("relay_encode_failed", error.to_string()))?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response())
}
