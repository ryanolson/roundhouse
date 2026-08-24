// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! ATIF v1.7 trajectories, by cold replay.
//!
//! # Attribution
//!
//! The twelve wire structs below are ported field-for-field from **NVIDIA NeMo
//! Relay** (Apache-2.0), rev `1a548124`, file
//! `crates/core/src/observability/atif.rs`. The field names, the serde
//! attributes and the `ATIF-v1.7` schema string are theirs; the doc comments are
//! rewritten and the exporter is not ported at all — Relay's accumulates events
//! in memory as a runtime plugin, and ours replays a durable log.
//!
//! A port rather than a dependency, and the reason is structural: ATIF is not in
//! `nemo-relay-types`. It lives in `crates/core`, the heavy crate — a runtime, a
//! plugin registry, a subscriber bus — none of which an emitter needs and all of
//! which it would have to build. Everything roundhouse emits that *is* in the
//! types crate is taken from the types crate; see [`atof`](crate::atof) and
//! [`summary`](crate::summary), which import Relay's own definitions.
//!
//! A port is a fork unless something makes drift visible, so
//! [`tests::the_ported_field_names_match_relays`] pins every field name of every
//! struct against a literal list transcribed from that revision. A field renamed
//! or dropped upstream then arrives here as a failing assertion naming the
//! struct, rather than as a consumer quietly failing to parse our export.
//!
//! # How a session becomes a trajectory
//!
//! One walk of [`SessionReplay`], and two kinds of step:
//!
//! - what the client said that turn, as `user` or `system` steps;
//! - what the deployment answered, as **exactly one `agent` step per dispatched
//!   turn** — the step that carries `tool_calls`, `observation`, `metrics` and
//!   the routing facts.
//!
//! That is a deliberate reading of "one step per turn", and it is worth being
//! explicit about. [`AtifStep`] has no field for a prompt: `message` is the
//! step's own content and `source` says whose it is. So a turn rendered as a
//! single `agent` step has nowhere to put what the client asked, and every
//! trajectory would silently omit the user's half of the conversation. The
//! reading here is also upstream's: Relay's own exporter maps LLM Start to a
//! `user` step and LLM End to an `agent` step, one call producing two. What
//! stays literally true of a turn is the part the rule was about — one
//! metrics-bearing step, one routing decision, one dispatch.
//!
//! **Observations arrive a turn late, and that is not a defect.** The deployment
//! emits a tool call at the end of one turn; the client runs the tool and
//! returns the result as input to the next. So a step's `observation` is
//! correlated forward by `tool_call_id` across the turn boundary, which is what
//! puts a call and its result on one step rather than on two.
//!
//! **A steered turn is an ordinary tool call here.** See the crate
//! documentation: that divergence is documented rather than reconciled, and the
//! `steered` flag in the step's routing facts is what makes it findable.

use std::collections::BTreeMap;

use roundhouse_core::event::{Accounting, SessionEvent};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use crate::replay::{SessionReplay, TurnRecord, spoken_input};
use crate::wire::{rfc3339, route_facts, route_schema};

/// The ATIF schema version every trajectory this crate produces is stamped with.
///
/// `crates/core/src/observability/atif.rs:55` at rev `1a548124`, unmoved between
/// that revision and the two before it. Downstream consumers gate on it.
pub const ATIF_SCHEMA_VERSION: &str = "ATIF-v1.7";

// ---------------------------------------------------------------------------
// The ported wire types
// ---------------------------------------------------------------------------
//
// `PartialEq` is derived here and not upstream. It changes no serialized byte
// and it is what lets a test assert that two exports of one log are equal
// structurally as well as textually.

/// Information about the agent that produced the trajectory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifAgentInfo {
    /// Human-readable agent name.
    pub name: String,
    /// Agent version string.
    pub version: String,
    /// Default LLM model name used by the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// Tool definitions available to the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_definitions: Option<Vec<Json>>,
    /// Extra metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Json>,
}

/// A single step in an ATIF trajectory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifStep {
    /// 1-based ordinal step ID.
    pub step_id: usize,
    /// Source of the step: `"system"`, `"user"`, or `"agent"`.
    pub source: String,
    /// The message content (string or array of content parts).
    pub message: Json,
    /// ISO 8601 timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// LLM model name, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// Qualitative or quantitative measure of reasoning effort.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<Json>,
    /// The agent's explicit internal reasoning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Tool calls made by the agent in this step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<AtifToolCall>>,
    /// Observation (tool results) for this step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<AtifObservation>,
    /// Token usage and cost metrics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<AtifMetrics>,
    /// Number of LLM calls represented by this step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_call_count: Option<u64>,
    /// Whether this step was copied from a previous trajectory for context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_copied_context: Option<bool>,
    /// Extra metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Json>,
}

/// Token usage and cost metrics for a single step.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AtifMetrics {
    /// Number of prompt tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    /// Number of completion tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    /// Number of cached tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// Cost in USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Token IDs for prompt (input) tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_token_ids: Option<Vec<u64>>,
    /// Token IDs for completion (response) tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_token_ids: Option<Vec<u64>>,
    /// Log probability assigned to each generated token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<Vec<f64>>,
    /// Other metrics (e.g. reasoning_tokens, cache_creation_input_tokens).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Json>,
}

/// Aggregate statistics for the entire trajectory.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AtifFinalMetrics {
    /// Sum of all prompt tokens across all steps, including cached tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_prompt_tokens: Option<u64>,
    /// Sum of all completion tokens across all steps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_completion_tokens: Option<u64>,
    /// Sum of all cached tokens across all steps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cached_tokens: Option<u64>,
    /// Total real monetary cost for the entire trajectory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    /// Total number of steps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_steps: Option<u64>,
    /// Custom aggregate metrics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Json>,
}

/// A tool call made by the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifToolCall {
    /// Correlation ID linking this call to its observation result.
    pub tool_call_id: String,
    /// Name of the tool/function called.
    pub function_name: String,
    /// Arguments passed to the tool.
    pub arguments: Json,
    /// Provider or host-specific metadata for this tool call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Json>,
}

/// Observation results from tool execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifObservation {
    /// List of observation results (one per tool call).
    pub results: Vec<AtifObservationResult>,
}

/// A single observation result from a tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifObservationResult {
    /// Correlation ID linking to the originating tool call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_call_id: Option<String>,
    /// The tool's output content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Json>,
    /// References to delegated subagent trajectories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_trajectory_ref: Option<Vec<AtifSubagentTrajectoryRef>>,
    /// Provider or host-specific metadata for this observation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Json>,
}

/// Reference to a delegated subagent trajectory.
///
/// Nothing here produces one — roundhouse serves one conversation per session
/// and a sub-agent is a session of its own — but the type is ported because a
/// consumer deserializing our export against its own ATIF model needs the shape
/// to exist, and because the field list is what the drift test pins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifSubagentTrajectoryRef {
    /// Embedded trajectory identifier, resolved against `subagent_trajectories`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trajectory_id: Option<String>,
    /// Run identity for debug/search/display correlation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Extra metadata about the subagent execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Json>,
}

/// Lineage node identifying a callable within an ATIF step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifAncestry {
    /// Unique identifier for the callable node (scope UUID).
    pub function_id: String,
    /// Human-readable name of the callable node.
    pub function_name: String,
    /// Optional parent callable identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Optional parent callable name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_name: Option<String>,
}

/// Invocation timing and correlation metadata for one execution occurrence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifInvocationInfo {
    /// Invocation start timestamp in Unix epoch seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_timestamp: Option<f64>,
    /// Invocation end timestamp in Unix epoch seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_timestamp: Option<f64>,
    /// Stable invocation identifier for correlation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
    /// Terminal status of the invocation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Runtime or framework label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framework: Option<String>,
}

/// Lineage payload serialized into ATIF `Step.extra`.
///
/// **Roundhouse's routing facts sit beside this rather than inside it**, and the
/// type is why: `ancestry` is required and describes a callable graph a plugin
/// runtime has and a turn engine does not. So `AtifStep.extra` is an object with
/// a `data_schema` key and a payload keyed by that schema's name — the same
/// shape the NeMo-Agent-Toolkit converter itself writes when it lifts a context
/// scope's `data_schema` into a step — and an `AtifStepExtra` may or may not be
/// another key in the same object depending on the producer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifStepExtra {
    /// Step-level callable lineage.
    pub ancestry: AtifAncestry,
    /// Step-level invocation timing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invocation: Option<AtifInvocationInfo>,
    /// Full unwrapped LLM request payload for request-level fidelity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_request: Option<Json>,
    /// Full raw LLM response payload for response-level fidelity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_response: Option<Json>,
    /// Legacy event payload field retained for source compatibility.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_payload: Option<Json>,
    /// Per-tool callable lineage, aligned with `tool_calls`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_ancestry: Vec<AtifAncestry>,
    /// Per-tool invocation timing, aligned with `tool_calls`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_invocations: Option<Vec<AtifInvocationInfo>>,
}

/// A complete ATIF trajectory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifTrajectory {
    /// Schema version (e.g., `"ATIF-v1.7"`).
    pub schema_version: String,
    /// Unique session identifier.
    pub session_id: String,
    /// Canonical per-trajectory-document identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trajectory_id: Option<String>,
    /// Information about the agent.
    pub agent: AtifAgentInfo,
    /// Ordered list of trajectory steps.
    pub steps: Vec<AtifStep>,
    /// Custom information, design notes, or explanations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Aggregate metrics for the entire trajectory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_metrics: Option<AtifFinalMetrics>,
    /// Reference to the continuation trajectory file if continued elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continued_trajectory_ref: Option<String>,
    /// Embedded subagent trajectories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_trajectories: Option<Vec<AtifTrajectory>>,
    /// Extra metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Json>,
}

// ---------------------------------------------------------------------------
// The producer
// ---------------------------------------------------------------------------

/// What every trajectory this crate produces says about itself.
const NOTES: &str = "Produced by cold replay of roundhouse's durable session log. \
     A turn steered by roundhouse's validate loop appears here as an ordinary \
     tool call; `extra[\"roundhouse/route\"].steered` marks which.";

/// One session's trajectory.
pub fn trajectory(events: &[SessionEvent]) -> AtifTrajectory {
    from_replay(&SessionReplay::of(events))
}

/// The same, from a replay a caller already has.
///
/// Public because the three producers are usually wanted together — a route that
/// serves both the trajectory and the summaries should walk the log once — and
/// because it is the seam that makes this function's purity obvious: there is no
/// input but the replay.
pub fn from_replay(replay: &SessionReplay) -> AtifTrajectory {
    let observations = observations_by_call(replay);

    let mut steps: Vec<AtifStep> = Vec::new();
    for turn in &replay.turns {
        for item in &turn.input {
            if let Some((source, text)) = spoken_input(item) {
                steps.push(input_step(steps.len() + 1, source, text, turn));
            }
        }
        if turn.is_publishable() {
            steps.push(agent_step(steps.len() + 1, turn, &observations));
        }
    }

    let final_metrics = final_metrics(&steps);
    AtifTrajectory {
        schema_version: ATIF_SCHEMA_VERSION.to_string(),
        session_id: replay.session_id.as_str().to_string(),
        // The session's own deterministic uuid, so two exports of one log name
        // one document and a consumer can deduplicate them.
        trajectory_id: Some(
            crate::ids::derive(&replay.session_id, &crate::ids::Name::session()).to_string(),
        ),
        agent: agent_info(replay),
        steps,
        notes: Some(NOTES.to_string()),
        final_metrics: Some(final_metrics),
        continued_trajectory_ref: None,
        subagent_trajectories: None,
        extra: None,
    }
}

/// Who produced this trajectory, and under what deployment facts.
///
/// `model_name` is deliberately absent. ATIF calls it "the default LLM model
/// name used by the agent", and roundhouse has no such thing: choosing a model
/// per turn is the entire product, so naming one here would report the last
/// routing decision as a property of the agent. Every step carries the model
/// that actually served it.
fn agent_info(replay: &SessionReplay) -> AtifAgentInfo {
    AtifAgentInfo {
        name: crate::PRODUCER.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        model_name: None,
        // Roundhouse does not own the tool catalog: an agent brings its own
        // tools and roundhouse never sees their definitions, only the calls.
        tool_definitions: None,
        extra: Some(json!({
            "data_schema": route_schema(),
            crate::ROUTE_SCHEMA_NAME: {
                "model_policy": replay.model_policy,
                "principal": replay.principal,
                "arm": replay.arm,
                "turns": replay.turns.len(),
            },
        })),
    }
}

/// A step for something the client said.
fn input_step(step_id: usize, source: &str, text: &str, turn: &TurnRecord) -> AtifStep {
    AtifStep {
        step_id,
        source: source.to_string(),
        message: Json::String(text.to_string()),
        timestamp: Some(rfc3339(turn.started_at_ms)),
        model_name: None,
        reasoning_effort: None,
        reasoning_content: None,
        tool_calls: None,
        observation: None,
        // No metrics, and that is the invariant the "one step per turn" reading
        // preserves: exactly one step per turn carries tokens, so a consumer
        // summing `metrics` over steps cannot double-count a dispatch.
        metrics: None,
        llm_call_count: None,
        is_copied_context: None,
        extra: None,
    }
}

/// The one metrics-bearing step of a dispatched turn.
fn agent_step(
    step_id: usize,
    turn: &TurnRecord,
    observations: &BTreeMap<String, String>,
) -> AtifStep {
    let tool_calls = tool_calls(turn);
    let observation = observation(&tool_calls, observations);
    AtifStep {
        step_id,
        source: "agent".to_string(),
        message: Json::String(agent_message(turn)),
        timestamp: Some(rfc3339(turn.ended_at_ms.unwrap_or(turn.started_at_ms))),
        model_name: turn.decision().map(|d| d.chosen.model().to_string()),
        // Roundhouse records how many reasoning tokens a turn spent, which is
        // not what this field is: `reasoning_effort` is the *request's* setting
        // and roundhouse does not set one. The count rides `metrics.extra`,
        // where it is a measurement rather than a claim about configuration.
        reasoning_effort: None,
        // The log stores no reasoning text. Providers that emit it emit it as
        // output the client never sees, and roundhouse does not retain it —
        // there is nothing here to publish, which is different from choosing
        // not to.
        reasoning_content: None,
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        observation,
        metrics: Some(metrics(turn)),
        // One dispatch per turn by construction: a turn that was re-dispatched
        // is a second `TurnStarted` and therefore a second step.
        llm_call_count: Some(1),
        is_copied_context: None,
        extra: Some(json!({
            "data_schema": route_schema(),
            crate::ROUTE_SCHEMA_NAME: route_facts(turn),
        })),
    }
}

/// What the client received, however it was committed.
///
/// The streamed text is preferred over the assistant item because it is what
/// actually went down the wire; the item is the same string, committed so a
/// successor can resume from it. A turn that answered with a tool call and no
/// prose has an empty message, which is the honest rendering and the one
/// upstream's own converter produces.
fn agent_message(turn: &TurnRecord) -> String {
    if !turn.text.is_empty() {
        return turn.text.clone();
    }
    turn.output
        .iter()
        .map(|item| item.spoken_text())
        .collect::<Vec<_>>()
        .concat()
}

fn tool_calls(turn: &TurnRecord) -> Vec<AtifToolCall> {
    turn.output
        .iter()
        .filter_map(|item| match &item.content {
            roundhouse_core::item::ItemContent::ToolCall {
                call_id,
                name,
                arguments,
            } => Some(AtifToolCall {
                tool_call_id: call_id.clone(),
                function_name: name.clone(),
                // Arguments are a string on the wire and an object in ATIF.
                // A payload that does not parse is published as the string it
                // is rather than dropped: what the agent was actually handed is
                // the fact worth keeping, and a tool that takes a bare string is
                // not an error.
                arguments: serde_json::from_str(arguments)
                    .unwrap_or_else(|_| Json::String(arguments.clone())),
                extra: None,
            }),
            _ => None,
        })
        .collect()
}

/// The results for this step's calls, if any came back.
fn observation(
    calls: &[AtifToolCall],
    observations: &BTreeMap<String, String>,
) -> Option<AtifObservation> {
    let results: Vec<AtifObservationResult> = calls
        .iter()
        .filter_map(|call| {
            observations
                .get(&call.tool_call_id)
                .map(|output| AtifObservationResult {
                    source_call_id: Some(call.tool_call_id.clone()),
                    content: Some(Json::String(output.clone())),
                    subagent_trajectory_ref: None,
                    extra: None,
                })
        })
        .collect();
    (!results.is_empty()).then_some(AtifObservation { results })
}

/// Every tool result in the session, keyed by the call it answers.
///
/// First result wins. A client that returned two results for one call id has
/// contradicted itself, and taking the later one would let a resend rewrite what
/// an earlier step observed.
fn observations_by_call(replay: &SessionReplay) -> BTreeMap<String, String> {
    let mut by_call = BTreeMap::new();
    for (call_id, output) in replay.tool_results() {
        by_call.entry(call_id).or_insert(output);
    }
    by_call
}

/// One turn's tokens and, where it is a measured fact, its cost.
///
/// **`cost_usd` is published only where it is measured money.** ATIF has one
/// cost field and no way to say how sure of it a producer is, so an estimate put
/// there is indistinguishable from a bill — and roundhouse knows exactly three
/// ways for a turn's cost to be less than measured: a forwarded subscription
/// seat this deployment holds no rate card for, a provider that reported no
/// usage so the tokens are our tokenizer's, and a hosted turn whose log predates
/// the recorded rate card. Each withholds the field and names itself in `extra`,
/// so a reader sees an absence with a reason rather than a number to trust.
///
/// A local dispatch publishes `0.0`, which is a measured zero: our own fleet
/// bills nothing. What it *would* have cost hosted is the optimization summary's
/// `baseline_cost`, which is where a counterfactual belongs.
fn metrics(turn: &TurnRecord) -> AtifMetrics {
    let usage = &turn.usage;
    let (cost_usd, basis) = cost(turn);
    AtifMetrics {
        prompt_tokens: Some(usage.input_tokens),
        completion_tokens: Some(usage.output_tokens),
        cached_tokens: Some(usage.cached_input_tokens),
        cost_usd,
        prompt_token_ids: None,
        completion_token_ids: None,
        logprobs: None,
        extra: Some(json!({
            // Both are components of the two counts above rather than additions
            // to them, exactly as roundhouse's own `Usage` documents.
            "reasoning_tokens": usage.reasoning_tokens,
            "uncached_input_tokens": usage.uncached_input_tokens(),
            "accounting": match usage.accounting {
                Accounting::Reported => "reported",
                Accounting::Estimated => "estimated",
            },
            "cost_basis": basis,
        })),
    }
}

/// What this turn cost, and on what basis — or why there is no number.
fn cost(turn: &TurnRecord) -> (Option<f64>, &'static str) {
    let Some(decision) = turn.decision() else {
        return (None, "not_routed");
    };
    if decision.chosen.is_local() {
        return (Some(0.0), "local_dispatch_bills_nothing");
    }
    if !decision.billing.is_billable() {
        return (None, "seat_forwarded_no_rate_card");
    }
    if turn.usage.accounting == Accounting::Estimated {
        return (None, "usage_estimated_by_roundhouse");
    }
    match decision.rate_card {
        Some(card) => (Some(card.price(&turn.usage)), "provider_rate_card"),
        None => (None, "no_rate_card_recorded"),
    }
}

/// The trajectory's totals, summed over the steps that carry metrics.
///
/// `total_cost_usd` sums only the steps that published a cost, and says how many
/// did not. A total that silently treated a withheld cost as zero would read as
/// a cheap session rather than as an incompletely priced one — which is the
/// same failure the dashboard's coverage figures exist to prevent.
fn final_metrics(steps: &[AtifStep]) -> AtifFinalMetrics {
    let mut totals = AtifFinalMetrics {
        total_steps: Some(steps.len() as u64),
        ..AtifFinalMetrics::default()
    };
    let mut prompt = 0u64;
    let mut completion = 0u64;
    let mut cached = 0u64;
    let mut cost = 0.0f64;
    let mut unpriced = 0u64;
    for metrics in steps.iter().filter_map(|step| step.metrics.as_ref()) {
        prompt += metrics.prompt_tokens.unwrap_or(0);
        completion += metrics.completion_tokens.unwrap_or(0);
        cached += metrics.cached_tokens.unwrap_or(0);
        match metrics.cost_usd {
            Some(step_cost) => cost += step_cost,
            None => unpriced += 1,
        }
    }
    totals.total_prompt_tokens = Some(prompt);
    totals.total_completion_tokens = Some(completion);
    totals.total_cached_tokens = Some(cached);
    totals.total_cost_usd = Some(cost);
    totals.extra = Some(json!({ "unpriced_steps": unpriced }));
    totals
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{self, Log};
    use roundhouse_core::event::IncompleteReason;

    /// Every field name of every ported struct, transcribed from
    /// `crates/core/src/observability/atif.rs` at rev `1a548124`.
    ///
    /// A port is a fork unless drift is visible. This is what makes it visible:
    /// a field renamed upstream, or dropped here, fails as a named struct rather
    /// than as a consumer that cannot parse our export six months later.
    ///
    /// Read off a fully-populated instance, because `skip_serializing_if` hides
    /// an absent field — a test over a default value would pin an empty object
    /// and pass forever.
    #[test]
    fn the_ported_field_names_match_relays() {
        let ancestry = AtifAncestry {
            function_id: "f".into(),
            function_name: "n".into(),
            parent_id: Some("p".into()),
            parent_name: Some("pn".into()),
        };
        let invocation = AtifInvocationInfo {
            start_timestamp: Some(1.0),
            end_timestamp: Some(2.0),
            invocation_id: Some("i".into()),
            status: Some("ok".into()),
            framework: Some("roundhouse".into()),
        };
        let subagent = AtifSubagentTrajectoryRef {
            trajectory_id: Some("t".into()),
            session_id: Some("s".into()),
            extra: Some(json!({})),
        };
        let observation_result = AtifObservationResult {
            source_call_id: Some("c".into()),
            content: Some(json!("out")),
            subagent_trajectory_ref: Some(vec![subagent.clone()]),
            extra: Some(json!({})),
        };
        let tool_call = AtifToolCall {
            tool_call_id: "c".into(),
            function_name: "grep".into(),
            arguments: json!({}),
            extra: Some(json!({})),
        };
        let metrics = AtifMetrics {
            prompt_tokens: Some(1),
            completion_tokens: Some(1),
            cached_tokens: Some(1),
            cost_usd: Some(1.0),
            prompt_token_ids: Some(vec![1]),
            completion_token_ids: Some(vec![1]),
            logprobs: Some(vec![1.0]),
            extra: Some(json!({})),
        };
        let step = AtifStep {
            step_id: 1,
            source: "agent".into(),
            message: json!(""),
            timestamp: Some("t".into()),
            model_name: Some("m".into()),
            reasoning_effort: Some(json!("low")),
            reasoning_content: Some("r".into()),
            tool_calls: Some(vec![tool_call.clone()]),
            observation: Some(AtifObservation {
                results: vec![observation_result.clone()],
            }),
            metrics: Some(metrics.clone()),
            llm_call_count: Some(1),
            is_copied_context: Some(false),
            extra: Some(json!({})),
        };
        let agent = AtifAgentInfo {
            name: "roundhouse".into(),
            version: "0".into(),
            model_name: Some("m".into()),
            tool_definitions: Some(vec![json!({})]),
            extra: Some(json!({})),
        };
        let final_metrics = AtifFinalMetrics {
            total_prompt_tokens: Some(1),
            total_completion_tokens: Some(1),
            total_cached_tokens: Some(1),
            total_cost_usd: Some(1.0),
            total_steps: Some(1),
            extra: Some(json!({})),
        };
        let step_extra = AtifStepExtra {
            ancestry: ancestry.clone(),
            invocation: Some(invocation.clone()),
            llm_request: Some(json!({})),
            llm_response: Some(json!({})),
            event_payload: Some(json!({})),
            tool_ancestry: vec![ancestry.clone()],
            tool_invocations: Some(vec![invocation.clone()]),
        };
        let trajectory = AtifTrajectory {
            schema_version: ATIF_SCHEMA_VERSION.into(),
            session_id: "s".into(),
            trajectory_id: Some("t".into()),
            agent: agent.clone(),
            steps: vec![step.clone()],
            notes: Some("n".into()),
            final_metrics: Some(final_metrics.clone()),
            continued_trajectory_ref: Some("c".into()),
            subagent_trajectories: Some(Vec::new()),
            extra: Some(json!({})),
        };

        let expected: [(&str, Vec<&str>); 12] = [
            (
                "AtifTrajectory",
                vec![
                    "schema_version",
                    "session_id",
                    "trajectory_id",
                    "agent",
                    "steps",
                    "notes",
                    "final_metrics",
                    "continued_trajectory_ref",
                    "subagent_trajectories",
                    "extra",
                ],
            ),
            (
                "AtifAgentInfo",
                vec!["name", "version", "model_name", "tool_definitions", "extra"],
            ),
            (
                "AtifStep",
                vec![
                    "step_id",
                    "source",
                    "message",
                    "timestamp",
                    "model_name",
                    "reasoning_effort",
                    "reasoning_content",
                    "tool_calls",
                    "observation",
                    "metrics",
                    "llm_call_count",
                    "is_copied_context",
                    "extra",
                ],
            ),
            (
                "AtifMetrics",
                vec![
                    "prompt_tokens",
                    "completion_tokens",
                    "cached_tokens",
                    "cost_usd",
                    "prompt_token_ids",
                    "completion_token_ids",
                    "logprobs",
                    "extra",
                ],
            ),
            (
                "AtifFinalMetrics",
                vec![
                    "total_prompt_tokens",
                    "total_completion_tokens",
                    "total_cached_tokens",
                    "total_cost_usd",
                    "total_steps",
                    "extra",
                ],
            ),
            (
                "AtifToolCall",
                vec!["tool_call_id", "function_name", "arguments", "extra"],
            ),
            ("AtifObservation", vec!["results"]),
            (
                "AtifObservationResult",
                vec![
                    "source_call_id",
                    "content",
                    "subagent_trajectory_ref",
                    "extra",
                ],
            ),
            (
                "AtifSubagentTrajectoryRef",
                vec!["trajectory_id", "session_id", "extra"],
            ),
            (
                "AtifAncestry",
                vec!["function_id", "function_name", "parent_id", "parent_name"],
            ),
            (
                "AtifInvocationInfo",
                vec![
                    "start_timestamp",
                    "end_timestamp",
                    "invocation_id",
                    "status",
                    "framework",
                ],
            ),
            (
                "AtifStepExtra",
                vec![
                    "ancestry",
                    "invocation",
                    "llm_request",
                    "llm_response",
                    "event_payload",
                    "tool_ancestry",
                    "tool_invocations",
                ],
            ),
        ];

        let actual: Vec<(&str, Json)> = vec![
            ("AtifTrajectory", serde_json::to_value(&trajectory).unwrap()),
            ("AtifAgentInfo", serde_json::to_value(&agent).unwrap()),
            ("AtifStep", serde_json::to_value(&step).unwrap()),
            ("AtifMetrics", serde_json::to_value(&metrics).unwrap()),
            (
                "AtifFinalMetrics",
                serde_json::to_value(&final_metrics).unwrap(),
            ),
            ("AtifToolCall", serde_json::to_value(&tool_call).unwrap()),
            (
                "AtifObservation",
                serde_json::to_value(AtifObservation {
                    results: vec![observation_result.clone()],
                })
                .unwrap(),
            ),
            (
                "AtifObservationResult",
                serde_json::to_value(&observation_result).unwrap(),
            ),
            (
                "AtifSubagentTrajectoryRef",
                serde_json::to_value(&subagent).unwrap(),
            ),
            ("AtifAncestry", serde_json::to_value(&ancestry).unwrap()),
            (
                "AtifInvocationInfo",
                serde_json::to_value(&invocation).unwrap(),
            ),
            ("AtifStepExtra", serde_json::to_value(&step_extra).unwrap()),
        ];

        for ((name, want), (same_name, value)) in expected.iter().zip(actual.iter()) {
            assert_eq!(name, same_name, "the two tables must stay aligned");
            let mut got: Vec<&str> = value
                .as_object()
                .unwrap_or_else(|| panic!("{name} must serialize as an object"))
                .keys()
                .map(String::as_str)
                .collect();
            let mut want = want.clone();
            got.sort_unstable();
            want.sort_unstable();
            assert_eq!(got, want, "`{name}` has drifted from Relay's field list");
        }
        assert_eq!(expected.len(), 12, "twelve structs, per the port");
    }

    #[test]
    fn the_schema_version_is_the_one_consumers_gate_on() {
        assert_eq!(ATIF_SCHEMA_VERSION, "ATIF-v1.7");
        let traj = trajectory(&[]);
        assert_eq!(traj.schema_version, "ATIF-v1.7");
    }

    #[test]
    fn a_turn_produces_one_metrics_bearing_step() {
        let mut log = Log::new("acme/ada/main");
        log.created(None);
        log.turn(
            "t1",
            "r1",
            fixtures::frontier("anthropic", "claude"),
            fixtures::usage(10_000, 8_000, 500),
        );

        let traj = trajectory(log.events());
        assert_eq!(traj.steps.len(), 2, "the client's message, then the answer");
        assert_eq!(traj.steps[0].source, "user");
        assert!(traj.steps[0].metrics.is_none());
        assert_eq!(traj.steps[1].source, "agent");
        assert_eq!(traj.steps[1].step_id, 2, "step ids are 1-based and dense");
        let metrics = traj.steps[1].metrics.as_ref().expect("the agent step");
        assert_eq!(metrics.prompt_tokens, Some(10_000));
        assert_eq!(metrics.cached_tokens, Some(8_000));
        assert_eq!(metrics.completion_tokens, Some(500));
        assert_eq!(
            traj.steps
                .iter()
                .filter(|step| step.metrics.is_some())
                .count(),
            1,
            "one dispatch, one priced step: a consumer summing metrics over \
             steps must not be able to double-count a turn"
        );
        assert_eq!(
            traj.final_metrics.as_ref().unwrap().total_prompt_tokens,
            Some(10_000)
        );
    }

    #[test]
    fn the_routing_facts_ride_a_data_schema_tagged_object_beside_the_step() {
        let mut log = Log::new("s1");
        log.created(None);
        log.routed_turn(
            "t1",
            "r1",
            fixtures::decision(
                fixtures::local("llama"),
                vec![fixtures::candidate(
                    fixtures::frontier("anthropic", "claude"),
                    0.05,
                )],
            ),
            fixtures::usage(1_000, 0, 100),
        );

        let traj = trajectory(log.events());
        let extra = traj.steps[1].extra.as_ref().expect("routing facts");
        assert_eq!(extra["data_schema"]["name"], "roundhouse/route");
        assert_eq!(extra["data_schema"]["version"], "1");
        let facts = &extra["roundhouse/route"];
        assert_eq!(facts["serving_mode"], "local");
        assert_eq!(facts["response_id"], "r1");
        assert_eq!(facts["quoted_frontier_alternative_usd"], 0.05);
        assert_eq!(facts["considered"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_tool_call_and_its_result_land_on_one_step() {
        let mut log = Log::new("s1");
        log.created(None);
        log.tool_call_turn("t1", "r1", "call_1", "grep", r#"{"q":"x"}"#);
        log.tool_result_turn("t2", "r2", "call_1", "3 matches");

        let traj = trajectory(log.events());
        let calling = traj
            .steps
            .iter()
            .find(|step| step.tool_calls.is_some())
            .expect("the step that made the call");
        let calls = calling.tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].tool_call_id, "call_1");
        assert_eq!(calls[0].function_name, "grep");
        assert_eq!(calls[0].arguments, json!({"q": "x"}));
        let results = &calling
            .observation
            .as_ref()
            .expect("the result arrives a turn later, on the calling step")
            .results;
        assert_eq!(results[0].source_call_id.as_deref(), Some("call_1"));
        assert_eq!(results[0].content, Some(json!("3 matches")));

        // The result is an observation and never a user message: it is the
        // output of a call the agent made, and emitting it as one would put
        // every tool run into the conversation twice.
        assert!(
            !traj
                .steps
                .iter()
                .any(|step| step.message == json!("3 matches")),
            "a tool result must not appear as a spoken step"
        );
    }

    #[test]
    fn a_cost_that_is_not_measured_is_withheld_with_its_reason() {
        // Estimated usage on a hosted turn: priced by the dashboard as
        // `billed_estimated_usd`, and refused here, because ATIF has one cost
        // field and no way to say how sure of it we are.
        let mut log = Log::new("s1");
        log.created(None);
        log.turn(
            "t1",
            "r1",
            fixtures::frontier("anthropic", "claude"),
            fixtures::estimated(fixtures::usage(10_000, 0, 100)),
        );
        let traj = trajectory(log.events());
        let metrics = traj.steps[1].metrics.as_ref().unwrap();
        assert_eq!(metrics.cost_usd, None);
        assert_eq!(
            metrics.extra.as_ref().unwrap()["cost_basis"],
            "usage_estimated_by_roundhouse"
        );
        assert_eq!(
            traj.final_metrics.as_ref().unwrap().extra.as_ref().unwrap()["unpriced_steps"],
            1,
            "and the total says how much of itself is missing"
        );

        // CONTROL: the identical turn with reported usage is priced from the
        // card the decision recorded, so the assertion above is about the
        // provenance and not about a rate card having gone missing.
        let mut reported = Log::new("s2");
        reported.created(None);
        reported.turn(
            "t1",
            "r1",
            fixtures::frontier("anthropic", "claude"),
            fixtures::usage(10_000, 0, 100),
        );
        let priced = trajectory(reported.events());
        let metrics = priced.steps[1].metrics.as_ref().unwrap();
        let expected = 10_000.0 * 3.75e-6 + 100.0 * 15.0e-6;
        assert!((metrics.cost_usd.unwrap() - expected).abs() < 1e-12);
        assert_eq!(
            metrics.extra.as_ref().unwrap()["cost_basis"],
            "provider_rate_card"
        );
    }

    #[test]
    fn a_turn_that_never_reached_a_provider_keeps_its_prompt_and_gains_no_step() {
        let mut log = Log::new("s1");
        log.created(None);
        log.refused_turn("t1", "r1", IncompleteReason::PolicyRefused);

        let traj = trajectory(log.events());
        assert_eq!(
            traj.steps.len(),
            1,
            "what the client asked is still what the client asked"
        );
        assert_eq!(traj.steps[0].source, "user");
        assert_eq!(
            traj.final_metrics.as_ref().unwrap().total_prompt_tokens,
            Some(0)
        );
    }

    #[test]
    fn two_exports_of_one_log_are_byte_identical() {
        let mut log = Log::new("acme/ada/main");
        log.created(None);
        log.tool_call_turn("t1", "r1", "call_1", "grep", r#"{"q":"x"}"#);
        log.tool_result_turn("t2", "r2", "call_1", "3 matches");

        let first = serde_json::to_string(&trajectory(log.events())).unwrap();
        let second = serde_json::to_string(&trajectory(log.events())).unwrap();
        assert_eq!(
            first, second,
            "a document produced by cold replay must not depend on when it was \
             produced -- an id from a clock or a random source would make two \
             exports of one finished session undiffable"
        );
        assert!(
            !first.contains("\"uuid\""),
            "and the trajectory carries no per-event ids at all: {first}"
        );
    }

    #[test]
    fn our_trajectory_round_trips_through_our_own_structs() {
        let mut log = Log::new("acme/ada/main");
        log.created(None);
        log.turn(
            "t1",
            "r1",
            fixtures::frontier("anthropic", "claude"),
            fixtures::usage(1_000, 0, 50),
        );

        let produced = trajectory(log.events());
        let json = serde_json::to_string(&produced).unwrap();
        let parsed: AtifTrajectory = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, produced, "the port has to be able to read itself");
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }
}
