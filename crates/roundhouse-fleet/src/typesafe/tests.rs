// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Parse-and-build tests. What arrives at a socket is asserted in
//! `tests/typesafe_transport.rs`, against a loopback upstream.

use super::*;
use roundhouse_core::control::Secret;

/// A state string that appears nowhere else, so a scan that finds it found the
/// real thing.
const STATE: &str = "ZZZQQQ-transcript-the-user-asked-about-parser-commas";
const KEY: &str = "sk-ZZZQQQ-typesafe-deployment-key";

pub(super) fn question() -> ChoiceQuestion {
    ChoiceQuestion {
        key: "tier".into(),
        instructions: "Which kind of model should answer this?".into(),
        criteria: BTreeMap::from([
            ("capable".to_string(), "Hard, multi-step work".to_string()),
            (
                "efficient".to_string(),
                "Routine, checkable work".to_string(),
            ),
        ]),
    }
}

fn request() -> SystemOneRequest {
    SystemOneRequest {
        model: "jev-1.12".into(),
        state: STATE.into(),
        question: question(),
    }
}

/// The body is the schema `docs.typesafe.ai/api` publishes, field for field: a
/// `questions` map keyed by the caller's name, carrying a `choice` question
/// whose `criteria` are the options.
#[test]
fn the_body_is_one_keyed_choice_question_with_its_criteria() {
    let body = SystemOneClient::body(&request());

    assert_eq!(
        body["model"], "jev-1.12",
        "the pinned model, never a default"
    );
    assert_eq!(body["state"], STATE);
    let asked = &body["questions"]["tier"];
    assert_eq!(
        asked["type"], "choice",
        "one `choice` question is the whole of what this client asks: {body}"
    );
    assert_eq!(
        asked["instructions"],
        "Which kind of model should answer this?"
    );
    assert_eq!(asked["criteria"]["capable"], "Hard, multi-step work");
    assert_eq!(asked["criteria"]["efficient"], "Routine, checkable work");
    assert_eq!(
        asked["criteria"].as_object().map(|map| map.len()),
        Some(2),
        "the two tier options and nothing else: {body}"
    );
    // The control: the question is keyed by the caller's name, so an answer can
    // be found again. A body that inlined one anonymous question would pass
    // every assertion above except this one.
    assert!(
        body["questions"]
            .as_object()
            .is_some_and(|map| map.len() == 1 && map.contains_key("tier")),
        "exactly one question, under the key the answer comes back on: {body}"
    );
}

/// The prompt is a projection of somebody's session, and a dropped request must
/// not put it in a log.
#[test]
fn debug_elides_the_state_and_keeps_its_length() {
    let shown = format!("{:?}", request());
    assert!(
        !shown.contains(STATE),
        "a `Debug` that prints the state carries a transcript into every log \
         that catches one: {shown}"
    );
    assert!(
        shown.contains(&STATE.len().to_string()),
        "the length survives, because that is what a size bound is debugged \
         with: {shown}"
    );
    // The controls: the fields that are roundhouse's own words are still
    // readable, so the assertion above is about elision and not about an empty
    // `Debug`.
    assert!(shown.contains("jev-1.12") && shown.contains("tier"));
}

/// Neither the key nor the prompt may reach an error, on any arm.
#[test]
fn no_error_carries_a_credential_or_a_prompt() {
    let errors = [
        SystemOneError::Status { status: 422 },
        SystemOneError::Malformed,
        SystemOneError::DeadlineExceeded,
        SystemOneError::ResponseTooLarge { limit_bytes: 64 },
        SystemOneError::RequestTooLarge {
            limit_bytes: 64,
            actual_bytes: 4096,
        },
        SystemOneError::ForwardedCredentialRefused,
        SystemOneError::Transport {
            message: "connection refused".into(),
            timed_out: false,
        },
    ];
    for error in &errors {
        let shown = format!("{error:?} {error}");
        assert!(!shown.contains(KEY), "an error carried the key: {shown}");
        assert!(
            !shown.contains(STATE),
            "an error carried the prompt: {shown}"
        );
    }
    // The control that makes the `Status` arm mean something: a 422 body that
    // quotes both back has nowhere to land, because the variant holds no body.
    let status = SystemOneError::Status { status: 422 };
    assert!(!format!("{status:?}").contains(KEY));
}

/// A stored key becomes a bearer; nothing else authenticates.
#[test]
fn only_a_stored_deployment_key_authenticates() {
    let stored = TurnCredential::Stored(Secret::api_key(KEY).unwrap());
    let headers = SystemOneClient::headers(&stored).unwrap();
    assert_eq!(
        headers.get(AUTHORIZATION).unwrap().as_bytes(),
        format!("Bearer {KEY}").as_bytes()
    );
    assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "application/json");

    assert_eq!(
        SystemOneClient::headers(&TurnCredential::Absent).unwrap_err(),
        SystemOneError::Credential(
            TurnCredential::Absent
                .require_api_key(PROVIDER)
                .expect_err("Absent never yields a key")
        ),
        "no credential is a local refusal, never an unauthenticated request"
    );
}

/// D4. The header map is the one place the plaintext key lives in memory, and
/// `tracing` prints header maps. `HeaderValue`'s own `Debug` prints `Sensitive`
/// for a value marked so — which is the effect being asserted, not the flag.
#[test]
fn a_debug_of_the_headers_does_not_print_the_key() {
    let stored = TurnCredential::Stored(Secret::api_key(KEY).unwrap());
    let headers = SystemOneClient::headers(&stored).unwrap();
    let shown = format!("{headers:?}");
    assert!(
        !shown.contains(KEY),
        "a header map that prints the deployment key puts it in every log that \
         formats one: {shown}"
    );
    // The control: the map is not passing by being empty — the non-secret
    // header is still readable, and the authorization header is still present.
    assert!(shown.contains("application/json"), "{shown}");
    assert!(headers.contains_key(AUTHORIZATION));
}

mod signal {
    use super::*;

    fn envelope(answer: &str, usage: &str) -> Vec<u8> {
        format!(r#"{{"model":"jev-1.12","answers":{{"tier":{answer}}},"usage":{usage}}}"#)
            .into_bytes()
    }

    const USAGE: &str = r#"{"input_tokens":312,"output_tokens":48}"#;

    fn valid() -> &'static str {
        r#"{"type":"choice","choice":"capable","probabilities":{"capable":0.85,"efficient":0.15},"confidence":0.82}"#
    }

    /// The published example parses, and the service's own accounting comes
    /// with it. The control every rejection below is measured against.
    #[test]
    fn a_published_shaped_answer_parses_with_its_usage() {
        let reply = SystemOneClient::reply(&envelope(valid(), USAGE), &question()).unwrap();
        assert_eq!(
            reply.usage,
            Some(SystemOneUsage {
                input_tokens: 312,
                output_tokens: 48
            })
        );
        let answer = reply.answer.expect("the published shape is usable");
        assert_eq!(answer.choice, "capable");
        assert_eq!(answer.confidence, 0.82);
        assert_eq!(answer.probabilities["efficient"], 0.15);
    }

    /// Fields this build has never seen are ignored, not refused: the service
    /// is free to add one, and a client that broke on it would break a
    /// deployment nobody touched.
    #[test]
    fn unrelated_extra_fields_are_tolerated() {
        let generous = r#"{"type":"choice","choice":"capable","probabilities":{"capable":0.85,"efficient":0.15},"confidence":0.82,"rationale":"unseen","latency_ms":12}"#;
        let raw = format!(
            r#"{{"model":"jev-1.12","request_id":"req_1","answers":{{"tier":{generous}}},"usage":{{"input_tokens":312,"output_tokens":48,"cached":7}}}}"#
        );
        let reply = SystemOneClient::reply(raw.as_bytes(), &question()).unwrap();
        assert!(reply.answer.is_ok(), "{:?}", reply.answer);
        assert_eq!(
            reply.usage.unwrap().input_tokens,
            312,
            "an unknown usage axis must not discard the two that are known"
        );
    }

    /// Every way a distribution can be wrong, each rejected by its own name.
    ///
    /// These fixtures pin exactly one ordering constraint: case `options that
    /// are not the options we offered` also has an unoffered `choice`, so
    /// `OptionsDisagree` must be decided before `ChoiceNotOffered`. Every other
    /// case fires exactly one variant, so the rest of the order is free.
    #[test]
    fn a_malformed_distribution_is_rejected() {
        for (why, answer, expected) in [
            (
                "a probability outside 0..=1",
                r#"{"type":"choice","choice":"capable","probabilities":{"capable":1.4,"efficient":-0.4},"confidence":0.8}"#,
                SignalError::ProbabilityOutOfRange,
            ),
            (
                "a distribution that does not sum to one",
                r#"{"type":"choice","choice":"capable","probabilities":{"capable":0.4,"efficient":0.2},"confidence":0.8}"#,
                SignalError::SumIsNotOne,
            ),
            (
                "options that are not the options we offered",
                r#"{"type":"choice","choice":"balanced","probabilities":{"balanced":1.0},"confidence":0.9}"#,
                SignalError::OptionsDisagree,
            ),
            (
                "a choice naming no offered option",
                r#"{"type":"choice","choice":"balanced","probabilities":{"capable":0.85,"efficient":0.15},"confidence":0.8}"#,
                SignalError::ChoiceNotOffered,
            ),
            (
                "a confidence outside 0..=1",
                r#"{"type":"choice","choice":"capable","probabilities":{"capable":0.85,"efficient":0.15},"confidence":7.0}"#,
                SignalError::ConfidenceOutOfRange,
            ),
        ] {
            let reply = SystemOneClient::reply(&envelope(answer, USAGE), &question())
                .unwrap_or_else(|error| panic!("{why}: the envelope parses: {error}"));
            assert_eq!(reply.answer, Err(expected), "{why}: {reply:?}");
        }

        // No answer at all under the key we asked under.
        let empty =
            br#"{"model":"jev-1.12","answers":{},"usage":{"input_tokens":1,"output_tokens":1}}"#;
        assert_eq!(
            SystemOneClient::reply(empty, &question()).unwrap().answer,
            Err(SignalError::MissingAnswer)
        );
    }

    /// Why `signal`'s `is_finite` guard has no fixture in the table above:
    /// JSON carries no NaN or infinity literal, and a number that overflows
    /// `f64` fails the envelope rather than arriving as one. The guard states
    /// what the range means; this records that the wire cannot reach it.
    #[test]
    fn a_probability_that_overflows_f64_fails_the_envelope() {
        let overflowing = r#"{"type":"choice","choice":"capable","probabilities":{"capable":1e400,"efficient":0.15},"confidence":0.8}"#;
        assert_eq!(
            SystemOneClient::reply(&envelope(overflowing, USAGE), &question()),
            Err(SystemOneError::Malformed)
        );
    }

    /// D5. An answer of another question type is not a `choice`, whether or not
    /// it happens to carry the fields one has.
    ///
    /// Two shapes, because they reach the verdict by different routes and only
    /// the second makes the `type` check load-bearing: a faithful `noul`
    /// (`api.md`'s documented field is `noul`, not `probability`) is missing
    /// every `choice` field, while a `score` answer carrying all of them is
    /// rejected *only* on its declared type.
    #[test]
    fn an_answer_of_another_question_type_is_not_a_choice() {
        for (why, answer) in [
            (
                "a faithful noul answer",
                r#"{"type":"noul","noul":0.7,"confidence":0.8}"#,
            ),
            (
                "a score answer carrying every choice field",
                r#"{"type":"score","score":2.0,"choice":"capable","probabilities":{"capable":0.85,"efficient":0.15},"confidence":0.82}"#,
            ),
        ] {
            let reply = SystemOneClient::reply(&envelope(answer, USAGE), &question()).unwrap();
            assert_eq!(reply.answer, Err(SignalError::NotAChoice), "{why}");
        }
    }

    /// **The spend survives the rejection.** The middle outcome, and the one a
    /// flat `Result` would lose: the service answered, charged for it, and said
    /// something unusable.
    #[test]
    fn usage_is_retained_when_the_signal_is_unusable() {
        let unusable = r#"{"type":"choice","choice":"capable","probabilities":{"capable":0.4,"efficient":0.2},"confidence":0.8}"#;
        let reply = SystemOneClient::reply(&envelope(unusable, USAGE), &question()).unwrap();
        assert_eq!(reply.answer, Err(SignalError::SumIsNotOne));
        assert_eq!(
            reply.usage,
            Some(SystemOneUsage {
                input_tokens: 312,
                output_tokens: 48
            }),
            "a call that was billed for must not settle at zero because its \
             answer was unusable"
        );
    }

    /// D1. Accounting that is not fully reported is **unknown**, never a zero.
    ///
    /// The absent case passed before this existed, through the envelope-level
    /// `Option`. The partial and malformed cases are the ones that matter: a
    /// per-field default turns silence about the input axis into a reported
    /// zero, which books a billed call as free. In every case the *answer* must
    /// survive — unknown cost is not a reason to discard a usable signal.
    #[test]
    fn usage_that_is_not_fully_reported_is_unknown_rather_than_zero() {
        for (why, usage) in [
            ("an absent usage object", None),
            (
                "a usage object missing the input axis",
                Some(r#"{"output_tokens":48}"#),
            ),
            (
                "a usage object missing the output axis",
                Some(r#"{"input_tokens":312}"#),
            ),
            ("an empty usage object", Some("{}")),
            ("a usage field that is not an object", Some(r#""billed""#)),
            (
                "a usage axis that is not a number",
                Some(r#"{"input_tokens":"lots","output_tokens":48}"#),
            ),
        ] {
            let raw = match usage {
                Some(usage) => envelope(valid(), usage),
                None => format!(r#"{{"model":"jev-1.12","answers":{{"tier":{}}}}}"#, valid())
                    .into_bytes(),
            };
            let reply = SystemOneClient::reply(&raw, &question())
                .unwrap_or_else(|error| panic!("{why}: the envelope must still parse: {error}"));
            assert_eq!(
                reply.usage, None,
                "{why}: a partly reported spend is unknown accounting, and a \
                 zero here books a billed call as free"
            );
            assert!(
                reply.answer.is_ok(),
                "{why}: an unpriceable call still answered, and discarding the \
                 signal would lose the one thing that did arrive: {:?}",
                reply.answer
            );
        }
    }

    /// A body that is not an envelope has no accounting to preserve, so it is
    /// the error arm rather than a reply with an empty answer.
    #[test]
    fn a_body_that_is_not_an_envelope_is_an_error() {
        assert_eq!(
            SystemOneClient::reply(b"<html>502</html>", &question()),
            Err(SystemOneError::Malformed)
        );
    }

    /// Rounding is not a malformed distribution.
    #[test]
    fn a_rounded_distribution_is_within_tolerance() {
        let rounded = r#"{"type":"choice","choice":"capable","probabilities":{"capable":0.5005,"efficient":0.4996},"confidence":0.5}"#;
        assert!(
            SystemOneClient::reply(&envelope(rounded, USAGE), &question())
                .unwrap()
                .answer
                .is_ok(),
            "rounding is not a malformed distribution"
        );
    }
}
