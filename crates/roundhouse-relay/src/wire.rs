// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What all three documents say about a routing decision, said once.
//!
//! A routing decision reaches a consumer three ways — as the `data` of an ATOF
//! context scope, as a `data_schema`-tagged object in an ATIF step's `extra`,
//! and as part of the typed payload on an optimization contribution. They are
//! one fact in three envelopes, so the projection lives here and the three
//! producers embed it. Three hand-written spellings would drift, and a consumer
//! correlating a trajectory step against the event stream it came from would
//! find the same decision described two ways.
//!
//! **Money is deliberately absent from [`route_facts`].** What a turn cost is
//! the optimization summary's subject and is priced through
//! `roundhouse_core::metrics`; what the *router* quoted at decision time is a
//! fact about the decision and is here. Mixing them would put a second, cheaper
//! answer to "what did this turn cost" into two documents that are not the one
//! anybody reconciles against a bill.

use chrono::{DateTime, Utc};
use nemo_relay_types::api::event::DataSchema;
use serde_json::{Value, json};

use roundhouse_core::metrics::{ModelKey, ServingMode};

use crate::replay::TurnRecord;
use crate::{ROUTE_SCHEMA_NAME, ROUTE_SCHEMA_VERSION};

/// A log timestamp as ATOF carries it.
///
/// Saturating rather than fallible: `at_ms` is a `u64` of milliseconds since the
/// epoch and only a value some 292 million years out cannot be represented, so
/// the failure this would report is a corrupt log rather than a late one. A
/// producer of a report about the past that refused to render a session because
/// one timestamp was absurd would go dark exactly where an operator needs it.
pub(crate) fn timestamp(at_ms: u64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(at_ms as i64).unwrap_or_default()
}

/// The same instant as the ISO 8601 string ATIF steps carry.
pub(crate) fn rfc3339(at_ms: u64) -> String {
    timestamp(at_ms).to_rfc3339()
}

/// The `{name, version}` pair every roundhouse-authored payload is tagged with.
pub fn route_schema() -> DataSchema {
    DataSchema {
        name: ROUTE_SCHEMA_NAME.to_string(),
        version: ROUTE_SCHEMA_VERSION.to_string(),
    }
}

/// One routing decision, as a consumer of any of the three formats sees it.
///
/// Every field is read off the decision the log recorded, never re-derived from
/// the live deployment: a document produced by cold replay must describe the
/// constraints that were in force when the turn ran, and a policy or a catalog
/// an operator has edited since would otherwise rewrite history.
///
/// `considered` carries the losing options because without them the document
/// cannot answer "was that the right call?" after the fact — the same reason
/// `DecisionRecord` carries them.
pub fn route_facts(turn: &TurnRecord) -> Value {
    let Some(decision) = turn.decision() else {
        return Value::Null;
    };
    let considered: Vec<Value> = decision
        .considered
        .iter()
        .map(|candidate| {
            json!({
                "target": candidate.target,
                "expected_cost_usd": candidate.expected_cost_usd,
                "expected_ttft_ms": candidate.expected_ttft_ms,
                "expected_prefill_tokens": candidate.expected_prefill_tokens,
                "quality_prior": candidate.quality_prior,
            })
        })
        .collect();
    json!({
        "session_seq": turn.started_seq,
        "turn_id": turn.turn_id.as_str(),
        "response_id": turn.response_id.as_str(),
        "chosen": decision.chosen,
        "serving_mode": serving_mode(turn),
        "policy": decision.policy,
        "rationale": decision.rationale,
        "turn_policy_digest": decision.turn_policy_digest,
        "budget_state": decision.budget_state,
        "payer": decision.payer,
        "billing": decision.billing,
        "isl_tokens": decision.isl_tokens,
        "expected_prefill_tokens": decision.expected_prefill_tokens,
        "expected_cost_usd": decision.expected_cost_usd,
        "quoted_frontier_alternative_usd": decision.quoted_frontier_alternative_usd(),
        "withheld_providers": decision.withheld_providers,
        "considered": considered,
        // Not a routing fact but the one thing a reader of *this* document
        // cannot otherwise recover: the crate documentation says a steered turn
        // publishes as an ordinary tool call, and this is what lets somebody
        // holding both the trajectory and our log find which ones those were.
        "steered": turn.steered,
    })
}

/// `"local"` or `"frontier"`, in the metrics surface's own vocabulary.
///
/// Through [`ModelKey`] rather than a local `match`, so the two documents and
/// the dashboard cannot end up spelling one axis two ways.
fn serving_mode(turn: &TurnRecord) -> &'static str {
    match turn.decision() {
        Some(decision) => ModelKey::from_target(&decision.chosen).mode.as_str(),
        None => ServingMode::Local.as_str(),
    }
}
