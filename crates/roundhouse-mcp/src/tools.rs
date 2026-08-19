// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The tool list, its schemas, and the one function that turns a named call
//! into a typed one.
//!
//! # Why the list is short, and why it is pinned
//!
//! Every listed tool costs tokens in the client's context on *every* turn, and
//! the cost is paid by the tenant whether or not the tool is ever called. Eight
//! is the whole surface and a ninth needs an argument, not a pull request.
//!
//! The list is also a stable interface in a stricter sense than most: a client
//! caches it, and Codex sends the tool set as part of the prompt, so a name or
//! a schema that moves invalidates prompt caches across every session in a
//! deployment at once. [`descriptors`] is therefore golden-pinned by test —
//! names *and* schema shape — and the pin is what a change to it has to argue
//! with.
//!
//! # Schemas by hand
//!
//! The schemas below are written out rather than derived. A derive would make
//! the wire contract a shadow of whatever the Rust type happens to be this
//! week — renaming a field, adding a `#[serde(flatten)]`, or changing an
//! `Option` would silently republish the contract — and it would put the
//! description strings, which are the part a model actually reads, somewhere
//! other than beside the schema they describe. [`descriptors_match_their_request_types`]
//! is what keeps the hand-written half honest: every declared property has to
//! deserialize into the request type it claims to describe.
//!
//! # Dispatch is not transport
//!
//! [`dispatch`] lives here and not in [`crate::transport`] on purpose. Turning
//! `("prefer", {…})` into `PreferRequest` and back into a [`ToolOutcome`] is
//! surface semantics — it is where an unknown tool and a malformed argument
//! become the errors an agent sees — and keeping it out of the transport file
//! is what makes that file swappable without moving a test.

use serde::Deserialize;
use serde_json::{Value, json};

use roundhouse_core::control::Principal;

use crate::surface::{ControlSurface, SurfaceError, ToolOutcome};

/// Every tool this surface serves, in the order it lists them.
pub const TOOL_NAMES: [&str; 8] = [
    "status",
    "init_session",
    "declare_intent",
    "prefer",
    "set_quality_floor",
    "fetch_steer",
    "report_outcome",
    "explain_last_route",
];

/// One entry of the tool list.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDescriptor {
    pub name: &'static str,
    /// What the model is told the tool does.
    ///
    /// Written for a reader that will act on it without asking a follow-up
    /// question, because it cannot ask one.
    pub description: &'static str,
    /// JSON Schema for the arguments object.
    pub input_schema: Value,
}

/// A named call with its arguments, before it has a type.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub name: String,
    /// The raw arguments object. `Value::Null` for a call that sent none,
    /// which every tool with only optional fields accepts.
    pub arguments: Value,
}

/// The conversation property, repeated by every session-scoped tool.
///
/// One function rather than a copied literal: the description is the sentence
/// that tells a model it may omit the field, and eight slightly different
/// spellings of it is how a model learns that the field means eight things.
fn conversation_property() -> Value {
    json!({
        "type": "string",
        "description": "The conversation this concerns, as the client's own prompt_cache_key. Omit it and the most recent conversation on this key is used."
    })
}

/// The tool list, exactly as it goes on the wire.
///
/// Stable across calls: this is a `const`-shaped function with no state, no
/// clock and no capability detection in it. Capability-dependent listing is
/// M6's business — its action map is what decides whether a client can be
/// steered at all — and a list that varied per client would break the prompt
/// cache of every session that saw two variants.
pub fn descriptors() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: "status",
            // Not "costs nothing": answering this reads the conversation's log,
            // and a description that told a model the call was free would be
            // inviting the loop the surface then has to absorb.
            description: "What this key may be routed to right now: the effective policy fingerprint, the admissible model names, budget remaining, and any steer awaiting an answer. Changes nothing; it reads this conversation's log to answer, so it is cheap between turns and not free in a loop.",
            input_schema: json!({
                "type": "object",
                "properties": { "conversation": conversation_property() },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "init_session",
            // States what M5 does — mint, record, and ask the client to keep
            // the token — and stops there. The read side that turns a kept
            // token into a resolved conversation is M7's (see
            // `ControlStore::binding_in_log`), and a description written in the
            // present tense about it would have every agent's context asserting
            // a correlation this deployment does not perform.
            description: "Mint an id identifying this conversation to roundhouse, which records it. Call it once at the start of a session and keep the output in the conversation, unsummarized: the id travelling back in the history you resend is what lets a later turn be matched to this conversation.",
            input_schema: json!({
                "type": "object",
                "properties": { "conversation": conversation_property() },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "declare_intent",
            description: "State what you are trying to do and how you will know you are done. Changes no routing; it is what lets a review name a divergence from your goal instead of guessing at one.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "goal": { "type": "string", "description": "What you are trying to accomplish, in one sentence." },
                    "plan_steps": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "The steps you expect to take, if you know them."
                    },
                    "done_when": { "type": "string", "description": "The observable condition that means the goal is met." },
                    "conversation": conversation_property()
                },
                "required": ["goal", "done_when"],
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "prefer",
            description: "Ask for local models, hosted models, or neither, for a while. This can only narrow what you are already allowed: if you ask for more than this key's policy permits, or for a side of the fleet it has nothing on, the answer says narrowed: true and your routing is left as it was.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["local", "frontier", "auto"],
                        "description": "local: keep turns on this deployment's own models. frontier: keep turns on hosted models. auto: release any preference you set earlier."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["turn", "session"],
                        "description": "turn: the next turn only. session: until you replace it, or until `turns` runs out."
                    },
                    "turns": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "How many turns the preference lasts. Only meaningful with scope=session; with scope=turn the only accepted value is 1."
                    },
                    "reason": { "type": "string", "description": "Why. Required, and recorded: a routing change nobody explained cannot be audited." },
                    "conversation": conversation_property()
                },
                "required": ["mode", "scope", "reason"],
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "set_quality_floor",
            description: "Raise the minimum model quality your turns may be routed to, for a number of turns. Narrowing only: a floor below this key's own is reported as narrowed: true rather than applied, and so is one that would leave nothing routable.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "floor": {
                        "type": "number",
                        "minimum": 0.0,
                        "maximum": 1.0,
                        "description": "Lowest acceptable model quality, on a 0.0 to 1.0 scale."
                    },
                    "turns": { "type": "integer", "minimum": 1, "description": "How many turns the floor lasts." },
                    "reason": { "type": "string", "description": "Why. Required, and recorded." },
                    "conversation": conversation_property()
                },
                "required": ["floor", "turns", "reason"],
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "fetch_steer",
            description: "Read the correction a roundhouse tool call named. Returns exactly what was written when the call was emitted; calling it twice returns the same bytes and does no work.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "steer_id": { "type": "string", "description": "The id the tool call named." }
                },
                "required": ["steer_id"],
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "report_outcome",
            description: "Say what you did about a steer. Advisory: not reporting is never an error and never blocks a turn.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "steer_id": { "type": "string", "description": "The steer you are reporting on." },
                    "outcome": {
                        "type": "string",
                        "enum": ["applied", "rejected", "not_applicable"],
                        "description": "applied: you changed course. rejected: you considered it and did not. not_applicable: it did not describe what you were doing."
                    },
                    "note": { "type": "string", "description": "Anything worth recording alongside." }
                },
                "required": ["steer_id", "outcome"],
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "explain_last_route",
            description: "Why your last turn went where it went: the model chosen, the reason, what else was considered, the budget situation, and the policy fingerprint in force.",
            input_schema: json!({
                "type": "object",
                "properties": { "conversation": conversation_property() },
                "additionalProperties": false
            }),
        },
    ]
}

/// Route one named call to its typed handler.
///
/// Never returns `Err`: a refusal is a [`ToolOutcome`] with `is_error` set,
/// because that is what MCP asks for and because a JSON-RPC error mid-turn
/// reads to a client as a broken connection rather than as a tool that said no.
/// The [`SurfaceError`] variants are still the vocabulary — they are rendered
/// here, in one place, so every tool refuses in the same words.
pub async fn dispatch(
    surface: &dyn ControlSurface,
    principal: &Principal,
    call: ToolCall,
) -> ToolOutcome {
    match dispatch_inner(surface, principal, call).await {
        Ok(outcome) => outcome,
        Err(error) => ToolOutcome::refused(&error),
    }
}

async fn dispatch_inner(
    surface: &dyn ControlSurface,
    principal: &Principal,
    call: ToolCall,
) -> Result<ToolOutcome, SurfaceError> {
    // An absent arguments object and an empty one mean the same thing, and a
    // client is free to send either. Normalizing here rather than in eight
    // `#[serde(default)]`-shaped workarounds keeps the request types describing
    // the schema rather than the client's habits.
    let arguments = match call.arguments {
        Value::Null => json!({}),
        other => other,
    };
    match call.name.as_str() {
        "status" => {
            surface
                .status(principal, decode("status", arguments)?)
                .await
        }
        "init_session" => {
            surface
                .init_session(principal, decode("init_session", arguments)?)
                .await
        }
        "declare_intent" => {
            surface
                .declare_intent(principal, decode("declare_intent", arguments)?)
                .await
        }
        "prefer" => {
            surface
                .prefer(principal, decode("prefer", arguments)?)
                .await
        }
        "set_quality_floor" => {
            surface
                .set_quality_floor(principal, decode("set_quality_floor", arguments)?)
                .await
        }
        "fetch_steer" => {
            surface
                .fetch_steer(principal, decode("fetch_steer", arguments)?)
                .await
        }
        "report_outcome" => {
            surface
                .report_outcome(principal, decode("report_outcome", arguments)?)
                .await
        }
        "explain_last_route" => {
            surface
                .explain_last_route(principal, decode("explain_last_route", arguments)?)
                .await
        }
        unknown => Err(SurfaceError::UnknownTool(unknown.to_string())),
    }
}

/// Deserialize a tool's arguments, keeping serde's message.
///
/// Serde names the offending field — "missing field `reason`", "unknown field
/// `resaon`" — and that name is the entire content of the mistake. Replacing it
/// with a generic "invalid arguments" is how an agent ends up retrying the same
/// call with the same typo.
fn decode<T: for<'de> Deserialize<'de>>(
    tool: &'static str,
    arguments: Value,
) -> Result<T, SurfaceError> {
    serde_json::from_value(arguments).map_err(|error| SurfaceError::BadArguments {
        tool,
        detail: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_names_and_the_descriptors_are_one_list() {
        // Two spellings of the tool set exist because one is a `const` a
        // matcher can be checked against and the other carries the schemas.
        // This is what stops them drifting.
        let declared: Vec<&str> = descriptors().iter().map(|tool| tool.name).collect();
        assert_eq!(declared, TOOL_NAMES.to_vec());
    }

    #[test]
    fn descriptors_match_their_request_types() {
        // The hand-written schemas' keeper. Every declared property has to be a
        // field the request type accepts, or a model reads a schema promising
        // an argument the surface would refuse as unknown.
        //
        // Checked by construction rather than by reflection: `deny_unknown_fields`
        // on every request type means a probe object built from the schema's
        // own property names either deserializes or names the property that
        // does not exist.
        for tool in descriptors() {
            let properties = tool.input_schema["properties"]
                .as_object()
                .expect("every schema declares an object of properties")
                .clone();
            let probe: serde_json::Map<String, Value> = properties
                .iter()
                .map(|(name, schema)| (name.clone(), sample_for(schema)))
                .collect();
            let call = ToolCall {
                name: tool.name.to_string(),
                arguments: Value::Object(probe),
            };
            let decoded = decode_probe(&call);
            assert!(
                decoded.is_ok(),
                "`{}`'s schema declares a property its request type refuses: {}",
                tool.name,
                decoded.unwrap_err()
            );
        }
    }

    /// A value of the type a schema fragment declares.
    fn sample_for(schema: &Value) -> Value {
        if let Some(choices) = schema["enum"].as_array() {
            return choices[0].clone();
        }
        match schema["type"].as_str() {
            Some("string") => json!("x"),
            Some("integer") => json!(1),
            Some("number") => json!(0.5),
            Some("array") => json!(["x"]),
            other => panic!("no sample for schema type {other:?}"),
        }
    }

    /// Decode a probe against the request type the tool dispatches to.
    ///
    /// The match is spelled out rather than shared with [`dispatch_inner`]
    /// because the point of the test is that these eight pairings are right,
    /// and a helper both sides called would test the helper.
    fn decode_probe(call: &ToolCall) -> Result<(), SurfaceError> {
        use crate::surface::*;
        let args = call.arguments.clone();
        match call.name.as_str() {
            "status" => decode::<StatusRequest>("status", args).map(drop),
            "init_session" => decode::<InitSessionRequest>("init_session", args).map(drop),
            "declare_intent" => decode::<DeclareIntentRequest>("declare_intent", args).map(drop),
            "prefer" => decode::<PreferRequest>("prefer", args).map(drop),
            "set_quality_floor" => {
                decode::<SetQualityFloorRequest>("set_quality_floor", args).map(drop)
            }
            "fetch_steer" => decode::<FetchSteerRequest>("fetch_steer", args).map(drop),
            "report_outcome" => decode::<ReportOutcomeRequest>("report_outcome", args).map(drop),
            "explain_last_route" => {
                decode::<ExplainLastRouteRequest>("explain_last_route", args).map(drop)
            }
            other => panic!("no request type paired with `{other}`"),
        }
    }

    #[test]
    fn a_required_property_is_one_the_request_type_also_requires() {
        // The other direction: a schema that marks a field optional while the
        // type requires it produces a tool a model calls correctly and the
        // surface refuses.
        for tool in descriptors() {
            let declared: Vec<&str> = tool.input_schema["required"]
                .as_array()
                .map(|entries| entries.iter().map(|e| e.as_str().unwrap()).collect())
                .unwrap_or_default();
            let properties = tool.input_schema["properties"].as_object().unwrap();
            for name in &declared {
                assert!(
                    properties.contains_key(*name),
                    "`{}` requires `{name}`, which it does not declare",
                    tool.name
                );
            }
            // Everything the type requires must be listed: an empty object has
            // to fail for exactly the tools with required fields.
            let empty = ToolCall {
                name: tool.name.to_string(),
                arguments: json!({}),
            };
            assert_eq!(
                decode_probe(&empty).is_err(),
                !declared.is_empty(),
                "`{}`'s required list and its request type disagree about the empty object",
                tool.name
            );
        }
    }
}
