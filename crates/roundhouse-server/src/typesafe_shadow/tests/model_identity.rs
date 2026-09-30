// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The service-reported model identity, distinct from the request alias, as
//! the public OpenAPI schema's `SystemOneResponse.model` documents it.
//!
//! `roundhouse_fleet::typesafe::Envelope` declares no `model` field, so the
//! reported identity never reaches `SystemOneReply`. Asserted against the
//! durable record's own JSON serialization rather than a typed accessor,
//! because the field does not exist yet: the failure must be a runtime one, so
//! the fix inherits a red assertion rather than a type to invent blind.

use super::*;

/// Depth-first search for the first `key` in a JSON value, keeping the
/// assertions below anchored to the specific field the contract names
/// (`reported_model`) rather than to any coincidental substring elsewhere in
/// the record.
fn find_json_key<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|v| find_json_key(v, key))),
        serde_json::Value::Array(items) => items.iter().find_map(|v| find_json_key(v, key)),
        _ => None,
    }
}

/// The control: the requested identity is this deployment's own
/// configuration, durable on the intent before any HTTP happens, unaffected
/// by whatever the service reports back.
#[tokio::test]
async fn the_requested_model_identity_is_already_durable_on_the_intent() {
    let (addr, _up) = upstream(ANSWER).await;
    let credential = credential();
    let shadow = shadow(addr, config(), RecordingLedger::granting(1.0));

    let projection = shadow.projection(&capture(), &[], &[]).expect("it fits");
    let prepared = shadow
        .prepare(call(&credential), &projection, Some(&[frontier()]))
        .expect("prepared");

    assert_eq!(
        prepared.intent.identity.model,
        config().model,
        "the requested identity is durable the moment the intent is written"
    );
}

/// The service answers under a model different from the one requested. That
/// identity must survive into the durable `ClassificationRecord` under the
/// key `reported_model`.
#[tokio::test]
async fn the_service_reported_model_identity_must_survive_into_the_durable_record() {
    const REPORTED_MODEL: &str = "jev-1.12-canary-2026w38";
    let answer = format!(
        r#"{{"model":"{REPORTED_MODEL}","answers":{{
  "intent":{{"type":"choice","choice":"implement","probabilities":{{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05}},"confidence":0.82}},
  "complexity":{{"type":"choice","choice":"involved","probabilities":{{"trivial":0.1,"routine":0.2,"involved":0.5,"deep":0.1,"unknown":0.1}},"confidence":0.61}},
  "context_dependence":{{"type":"choice","choice":"recent","probabilities":{{"self_contained":0.2,"recent":0.5,"deep":0.2,"unknown":0.1}},"confidence":0.55}}
}},"usage":{{"input_tokens":312,"output_tokens":48}}}}"#
    );
    let (addr, up) = upstream(Box::leak(answer.into_boxed_str())).await;
    let credential = credential();
    assert_ne!(config().model, REPORTED_MODEL);

    let record = classify(
        &shadow(addr, config(), RecordingLedger::granting(1.0)),
        &credential,
        Some(&[frontier()]),
    )
    .await
    .expect("prepared");

    assert_eq!(up.count(), 1, "the call must actually have been made");
    assert!(record.outcome.classification().is_some());

    let parsed: serde_json::Value =
        serde_json::to_value(&record).expect("a durable record serializes to JSON");
    let reported = find_json_key(&parsed, "reported_model");
    assert_eq!(
        reported,
        Some(&serde_json::Value::String(REPORTED_MODEL.to_string())),
        "the service-reported model identity must survive under the key \
         `reported_model`; found {reported:?} in:\n{parsed}"
    );
}

/// Missing model metadata invents no identity and must never be silently
/// replaced with the request alias -- the two are different facts. Usage
/// stays intact.
#[tokio::test]
async fn missing_model_metadata_invents_no_identity_and_keeps_reported_usage() {
    let (addr, up) = upstream(ANSWER_NO_MODEL_FIELD).await;
    let credential = credential();

    let record = classify(
        &shadow(addr, config(), RecordingLedger::granting(1.0)),
        &credential,
        Some(&[frontier()]),
    )
    .await
    .expect("prepared");

    assert_eq!(up.count(), 1);
    let classification = record
        .outcome
        .classification()
        .expect("the taxonomy answer is independent of the model field");
    assert_eq!(classification.intent.value, TurnIntent::Implement);
    assert_eq!(
        record.outcome.committed_usd(),
        Some(REPORTED_USD),
        "usage must be unaffected by an upstream that reports no model \
         identity at all"
    );

    let parsed: serde_json::Value =
        serde_json::to_value(&record).expect("a durable record serializes to JSON");
    assert_ne!(
        find_json_key(&parsed, "reported_model"),
        Some(&serde_json::Value::String(config().model)),
        "a missing reported identity must never be silently replaced with the \
         requested alias: {parsed}"
    );
}

/// A `model` field of the wrong JSON shape must not fail the whole envelope,
/// or the usage riding beside it would be discarded along with it.
#[tokio::test]
async fn malformed_model_metadata_does_not_discard_reported_usage() {
    const ANSWER_MALFORMED_MODEL: &str = r#"{"model":42,"answers":{
  "intent":{"type":"choice","choice":"implement","probabilities":{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05},"confidence":0.82},
  "complexity":{"type":"choice","choice":"involved","probabilities":{"trivial":0.1,"routine":0.2,"involved":0.5,"deep":0.1,"unknown":0.1},"confidence":0.61},
  "context_dependence":{"type":"choice","choice":"recent","probabilities":{"self_contained":0.2,"recent":0.5,"deep":0.2,"unknown":0.1},"confidence":0.55}
},"usage":{"input_tokens":312,"output_tokens":48}}"#;
    let (addr, up) = upstream(ANSWER_MALFORMED_MODEL).await;
    let credential = credential();

    let record = classify(
        &shadow(addr, config(), RecordingLedger::granting(1.0)),
        &credential,
        Some(&[frontier()]),
    )
    .await
    .expect("prepared");

    assert_eq!(up.count(), 1);
    assert!(
        record.outcome.classification().is_some(),
        "a wrong-shaped model field must not fail the whole envelope: {:?}",
        record.outcome
    );
    assert_eq!(
        record.outcome.committed_usd(),
        Some(REPORTED_USD),
        "reported usage must survive a malformed model field, not just a \
         missing one"
    );
}

/// An upstream that echoes back the credential it was called with -- standing
/// in for a service bug or a hostile response -- must never see that value
/// land in the durable record, however it is echoed.
#[tokio::test]
async fn an_echoed_synthetic_credential_never_reaches_the_durable_record() {
    let answer = format!(
        r#"{{"model":"{KEY}","answers":{{
  "intent":{{"type":"choice","choice":"implement","probabilities":{{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05}},"confidence":0.82}},
  "complexity":{{"type":"choice","choice":"involved","probabilities":{{"trivial":0.1,"routine":0.2,"involved":0.5,"deep":0.1,"unknown":0.1}},"confidence":0.61}},
  "context_dependence":{{"type":"choice","choice":"recent","probabilities":{{"self_contained":0.2,"recent":0.5,"deep":0.2,"unknown":0.1}},"confidence":0.55}}
}},"usage":{{"input_tokens":312,"output_tokens":48}}}}"#
    );
    let (addr, up) = upstream(Box::leak(answer.into_boxed_str())).await;
    let credential = credential();

    let record = classify(
        &shadow(addr, config(), RecordingLedger::granting(1.0)),
        &credential,
        Some(&[frontier()]),
    )
    .await
    .expect("prepared");

    assert_eq!(up.count(), 1);
    assert!(record.outcome.classification().is_some());
    assert_eq!(
        record.outcome.committed_usd(),
        Some(REPORTED_USD),
        "usage must survive an echoed credential the same as any other reply"
    );

    let serialized = serde_json::to_string(&record).expect("a durable record serializes");
    assert!(
        !serialized.contains(KEY),
        "an upstream-echoed credential must never reach the durable record: \
         {serialized}"
    );
}

/// A complete answer set with no top-level `model` key, standing in for a
/// service that omits the field entirely. Everything else matches [`ANSWER`].
const ANSWER_NO_MODEL_FIELD: &str = r#"{"answers":{
  "intent":{"type":"choice","choice":"implement","probabilities":{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05},"confidence":0.82},
  "complexity":{"type":"choice","choice":"involved","probabilities":{"trivial":0.1,"routine":0.2,"involved":0.5,"deep":0.1,"unknown":0.1},"confidence":0.61},
  "context_dependence":{"type":"choice","choice":"recent","probabilities":{"self_contained":0.2,"recent":0.5,"deep":0.2,"unknown":0.1},"confidence":0.55}
},"usage":{"input_tokens":312,"output_tokens":48}}"#;
