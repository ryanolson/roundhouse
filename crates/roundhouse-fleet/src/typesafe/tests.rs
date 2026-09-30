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

fn tier() -> ChoiceQuestion {
    ChoiceQuestion {
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

/// The one-question batch, which is what every assertion about a single
/// question's guards is written against.
fn one() -> BTreeMap<String, ChoiceQuestion> {
    BTreeMap::from([("tier".to_string(), tier())])
}

fn request() -> SystemOneRequest {
    SystemOneRequest {
        model: "jev-1.12".into(),
        state: STATE.into(),
        questions: one(),
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
        "`choice` is the one question type this client asks: {body}"
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
        let reply = SystemOneClient::reply(&envelope(valid(), USAGE), &one()).unwrap();
        assert_eq!(
            reply.usage,
            Some(SystemOneUsage {
                input_tokens: 312,
                output_tokens: 48
            })
        );
        let answers = reply.answers.expect("the published shape is usable");
        assert_eq!(answers.len(), 1, "one question asked, one answer back");
        let answer = &answers["tier"];
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
        let reply = SystemOneClient::reply(raw.as_bytes(), &one()).unwrap();
        assert!(reply.answers.is_ok(), "{:?}", reply.answers);
        assert_eq!(
            reply.usage.unwrap().input_tokens,
            312,
            "an unknown usage axis must not discard the two that are known"
        );
    }

    /// A `model` too extreme for `f64` is metadata `serde_json` cannot even
    /// hold as a `Value`, and must be unknown rather than take the answers
    /// and the accounting down with it -- the same leniency `usage` and
    /// `answers` already get.
    #[test]
    fn a_model_too_extreme_for_f64_is_unknown_rather_than_failing_the_envelope() {
        let raw = format!(
            r#"{{"model":1e400,"answers":{{"tier":{}}},"usage":{USAGE}}}"#,
            valid()
        );
        let reply = SystemOneClient::reply(raw.as_bytes(), &one())
            .expect("a malformed model must not fail the whole envelope");
        assert_eq!(reply.reported_model, None);
        assert_eq!(
            reply.usage,
            Some(SystemOneUsage {
                input_tokens: 312,
                output_tokens: 48
            }),
            "the accounting must survive a model that could not be read"
        );
        assert!(reply.answers.is_ok(), "{:?}", reply.answers);
    }

    /// A `usage` axis `serde_json` cannot even hold as a `Value` -- here, an
    /// unrelated extra key carrying a number too extreme for `f64` -- must
    /// not fail the whole envelope either.
    #[test]
    fn a_usage_block_carrying_an_unreadable_extra_key_does_not_fail_the_envelope() {
        let raw = format!(
            r#"{{"model":"jev-1.12","answers":{{"tier":{}}},"usage":{{"input_tokens":900,"output_tokens":4,"x":1e400}}}}"#,
            valid()
        );
        let reply = SystemOneClient::reply(raw.as_bytes(), &one())
            .expect("a malformed usage block must not fail the whole envelope");
        assert_eq!(
            reply.usage,
            Some(SystemOneUsage {
                input_tokens: 900,
                output_tokens: 4
            }),
            "WireUsage does not deny unknown fields, so an extra key -- even \
             one serde_json cannot represent as a Value -- is ignored rather \
             than failing the usage block it sits beside"
        );
        assert!(reply.answers.is_ok(), "{:?}", reply.answers);
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
            let reply = SystemOneClient::reply(&envelope(answer, USAGE), &one())
                .unwrap_or_else(|error| panic!("{why}: the envelope parses: {error}"));
            assert_eq!(reply.answers, Err(expected), "{why}: {reply:?}");
        }

        // No answer at all under the key we asked under.
        let empty =
            br#"{"model":"jev-1.12","answers":{},"usage":{"input_tokens":1,"output_tokens":1}}"#;
        assert_eq!(
            SystemOneClient::reply(empty, &one()).unwrap().answers,
            Err(SignalError::MissingAnswer)
        );
    }

    /// Why `signal`'s `is_finite` guard has no fixture in the table above:
    /// JSON carries no NaN or infinity literal, so the wire cannot reach it
    /// directly. A number that overflows `f64` instead fails to parse at all,
    /// and `answers` is held unparsed for exactly this reason: the failure is
    /// confined to the answer that carried it, and the usage beside it still
    /// arrives.
    #[test]
    fn a_probability_that_overflows_f64_fails_only_the_answers() {
        let overflowing = r#"{"type":"choice","choice":"capable","probabilities":{"capable":1e400,"efficient":0.15},"confidence":0.8}"#;
        let reply = SystemOneClient::reply(&envelope(overflowing, USAGE), &one())
            .expect("the envelope parses; only the answer is unusable");
        assert_eq!(
            reply.usage,
            Some(SystemOneUsage {
                input_tokens: 312,
                output_tokens: 48
            }),
            "a number one answer cannot carry must not discard the usage beside it"
        );
        assert_eq!(
            reply.answers,
            Err(SignalError::MalformedAnswers),
            "the whole batch fails to parse as a `Value`, not just the one answer"
        );
    }

    /// An explicit `null` for `answers` reads the same as an absent field --
    /// `serde` maps JSON `null` to `None` for the envelope's `Option<Box<RawValue>>`
    /// before any batch parsing runs -- so it is an empty batch, not a
    /// wrong-shaped one: the reported usage still arrives, and the missing
    /// key is what the answers come back refused over.
    #[test]
    fn a_null_answers_block_keeps_the_usage_and_fails_only_the_answers() {
        let raw = br#"{"answers":null,"usage":{"input_tokens":900,"output_tokens":4}}"#;
        let reply = SystemOneClient::reply(raw, &one()).expect("the envelope parses");
        assert_eq!(
            reply.usage,
            Some(SystemOneUsage {
                input_tokens: 900,
                output_tokens: 4
            })
        );
        assert_eq!(
            reply.answers,
            Err(SignalError::MissingAnswer),
            "an empty batch, not a malformed one: no answer arrived under any \
             key that was asked under"
        );
    }

    /// A non-object `answers` -- a string, here -- is the same wrong-shaped
    /// batch as `null`, and is refused the same way rather than taken apart.
    #[test]
    fn a_non_object_answers_block_keeps_the_usage_and_fails_only_the_answers() {
        let raw = br#"{"answers":"not a batch","usage":{"input_tokens":900,"output_tokens":4}}"#;
        let reply = SystemOneClient::reply(raw, &one()).expect("the envelope parses");
        assert_eq!(
            reply.usage,
            Some(SystemOneUsage {
                input_tokens: 900,
                output_tokens: 4
            })
        );
        assert_eq!(reply.answers, Err(SignalError::MalformedAnswers));
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
            let reply = SystemOneClient::reply(&envelope(answer, USAGE), &one()).unwrap();
            assert_eq!(reply.answers, Err(SignalError::NotAChoice), "{why}");
        }
    }

    /// **The spend survives the rejection.** The middle outcome, and the one a
    /// flat `Result` would lose: the service answered, charged for it, and said
    /// something unusable.
    #[test]
    fn usage_is_retained_when_the_signal_is_unusable() {
        let unusable = r#"{"type":"choice","choice":"capable","probabilities":{"capable":0.4,"efficient":0.2},"confidence":0.8}"#;
        let reply = SystemOneClient::reply(&envelope(unusable, USAGE), &one()).unwrap();
        assert_eq!(reply.answers, Err(SignalError::SumIsNotOne));
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
            let reply = SystemOneClient::reply(&raw, &one())
                .unwrap_or_else(|error| panic!("{why}: the envelope must still parse: {error}"));
            assert_eq!(
                reply.usage, None,
                "{why}: a partly reported spend is unknown accounting, and a \
                 zero here books a billed call as free"
            );
            assert!(
                reply.answers.is_ok(),
                "{why}: an unpriceable call still answered, and discarding the \
                 signal would lose the one thing that did arrive: {:?}",
                reply.answers
            );
        }
    }

    /// A body that is not an envelope has no accounting to preserve, so it is
    /// the error arm rather than a reply with an empty answer.
    #[test]
    fn a_body_that_is_not_an_envelope_is_an_error() {
        assert_eq!(
            SystemOneClient::reply(b"<html>502</html>", &one()),
            Err(SystemOneError::Malformed)
        );
    }

    /// Rounding is not a malformed distribution.
    #[test]
    fn a_rounded_distribution_is_within_tolerance() {
        let rounded = r#"{"type":"choice","choice":"capable","probabilities":{"capable":0.5005,"efficient":0.4996},"confidence":0.5}"#;
        assert!(
            SystemOneClient::reply(&envelope(rounded, USAGE), &one())
                .unwrap()
                .answers
                .is_ok(),
            "rounding is not a malformed distribution"
        );
    }
}

/// Many questions in one request, and the join between them and their answers.
mod batch {
    use super::*;

    /// Two questions that differ on every axis the join could confuse: the key,
    /// the instructions, the option names and the option *count*.
    ///
    /// The keys are chosen so `z_complexity` sorts **after** `a_tier`. That is
    /// load-bearing rather than decorative: a client that carried only the
    /// first question of a map would satisfy every assertion below about
    /// `a_tier`, so the question that is omitted, made unanswerable or
    /// malformed is always the second one.
    fn questions() -> BTreeMap<String, ChoiceQuestion> {
        BTreeMap::from([
            ("a_tier".to_string(), tier()),
            (
                "z_complexity".to_string(),
                ChoiceQuestion {
                    instructions: "How involved is the work this turn asks for?".into(),
                    criteria: BTreeMap::from([
                        ("low".to_string(), "One edit in one file".to_string()),
                        ("medium".to_string(), "A few files, one seam".to_string()),
                        ("high".to_string(), "A change across modules".to_string()),
                    ]),
                },
            ),
        ])
    }

    fn request() -> SystemOneRequest {
        SystemOneRequest {
            model: "jev-1.12".into(),
            state: STATE.into(),
            questions: questions(),
        }
    }

    const USAGE: &str = r#"{"input_tokens":312,"output_tokens":48}"#;
    const TIER_OK: &str = r#"{"type":"choice","choice":"capable","probabilities":{"capable":0.85,"efficient":0.15},"confidence":0.82}"#;
    const COMPLEXITY_OK: &str = r#"{"type":"choice","choice":"high","probabilities":{"low":0.1,"medium":0.3,"high":0.6},"confidence":0.71}"#;

    fn envelope(answers: &str) -> Vec<u8> {
        format!(r#"{{"model":"jev-1.12","answers":{answers},"usage":{USAGE}}}"#).into_bytes()
    }

    fn billed() -> Option<SystemOneUsage> {
        Some(SystemOneUsage {
            input_tokens: 312,
            output_tokens: 48,
        })
    }

    /// Every question is serialized under its own id, with its own
    /// instructions and its own options, in **one** body.
    #[test]
    fn every_question_is_serialized_under_its_own_id() {
        let body = SystemOneClient::body(&request());

        let asked = body["questions"]
            .as_object()
            .unwrap_or_else(|| panic!("a `questions` map: {body}"));
        assert_eq!(
            asked.len(),
            2,
            "both questions travel in one request, which is the whole point of \
             the map: {body}"
        );
        assert_eq!(body["questions"]["a_tier"]["type"], "choice");
        assert_eq!(body["questions"]["z_complexity"]["type"], "choice");
        assert_eq!(
            body["questions"]["z_complexity"]["instructions"],
            "How involved is the work this turn asks for?",
            "each question carries its own instructions: {body}"
        );
        assert_eq!(
            body["questions"]["z_complexity"]["criteria"]["high"],
            "A change across modules"
        );
        // The option *sets* are per question and never pooled: a body that
        // merged them would give each question five options.
        assert_eq!(
            body["questions"]["a_tier"]["criteria"]
                .as_object()
                .map(|map| map.len()),
            Some(2),
            "{body}"
        );
        assert_eq!(
            body["questions"]["z_complexity"]["criteria"]
                .as_object()
                .map(|map| map.len()),
            Some(3),
            "{body}"
        );
        // One state for the batch, not one per question — the saving the batch
        // exists for.
        assert_eq!(body["state"], STATE);

        // Byte-stable for identical inputs, so a recorded request can be
        // compared against a replayed one. The mechanism is the workspace's
        // `serde_json` pin rather than anything here — `preserve_order` is off,
        // so every `Value` renders in sorted key order — which is why the
        // assertion is that two builds agree and not that some order is the
        // right one.
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            serde_json::to_string(&SystemOneClient::body(&request())).unwrap()
        );
        // The keys really are sorted, which is the property that claim rests
        // on: `z_complexity` was inserted second and prints second, and would
        // still print second from a map that had been built the other way.
        assert_eq!(
            asked.keys().collect::<Vec<_>>(),
            ["a_tier", "z_complexity"],
            "{body}"
        );
    }

    /// Each answer is filed under the id its question was asked under, whatever
    /// order the service chose to print them in.
    #[test]
    fn answers_are_filed_under_their_own_ids_whatever_order_they_arrive_in() {
        // `z_complexity` first on the wire, which is neither the request's
        // order nor the sorted one.
        let raw = envelope(&format!(
            r#"{{"z_complexity":{COMPLEXITY_OK},"a_tier":{TIER_OK}}}"#
        ));
        let reply = SystemOneClient::reply(&raw, &questions()).unwrap();

        let answers = reply
            .answers
            .unwrap_or_else(|error| panic!("both answers are well formed: {error}"));
        assert_eq!(answers.len(), 2, "one answer per question: {answers:?}");
        assert_eq!(answers["a_tier"].choice, "capable");
        assert_eq!(answers["z_complexity"].choice, "high");
        // The values, not just the ids: an implementation that paired answers
        // to questions by position rather than by id would swap these two, and
        // the distributions are the only thing that shows it.
        assert_eq!(answers["z_complexity"].probabilities["medium"], 0.3);
        assert_eq!(answers["z_complexity"].confidence, 0.71);
        assert_eq!(answers["a_tier"].probabilities["efficient"], 0.15);
        assert_eq!(answers["a_tier"].confidence, 0.82);
    }

    /// Each answer is validated against **its own** question's options.
    ///
    /// The failure this rules out is a client that checks every answer against
    /// the union of every question's options, which would accept a complexity
    /// answer given in tiers.
    #[test]
    fn an_answer_is_checked_against_the_options_its_own_question_offered() {
        let crossed = envelope(&format!(
            r#"{{"a_tier":{TIER_OK},"z_complexity":{TIER_OK}}}"#
        ));
        let reply = SystemOneClient::reply(&crossed, &questions()).unwrap();
        assert_eq!(
            reply.answers,
            Err(SignalError::OptionsDisagree),
            "a complexity answer given over the tier options is an answer to a \
             different question: {reply:?}"
        );

        // The control: the same two answers, each over its own options, is the
        // usable reply — so the rejection above is about the crossing and not
        // about the second question being rejected on sight.
        assert!(
            SystemOneClient::reply(
                &envelope(&format!(
                    r#"{{"a_tier":{TIER_OK},"z_complexity":{COMPLEXITY_OK}}}"#
                )),
                &questions(),
            )
            .unwrap()
            .answers
            .is_ok()
        );
    }

    /// A reply that is not exactly the questions asked is refused **whole**,
    /// and the spend survives every one of those refusals.
    ///
    /// All-or-nothing because a caller asks for the features it needs to decide
    /// with: four answers out of five is not four-fifths of a decision.
    #[test]
    fn a_reply_that_is_not_the_questions_asked_is_refused_whole_with_its_usage() {
        for (why, answers, expected) in [
            (
                "no answer for the second question",
                format!(r#"{{"a_tier":{TIER_OK}}}"#),
                SignalError::MissingAnswer,
            ),
            (
                "an answer under an id nothing asked",
                format!(
                    r#"{{"a_tier":{TIER_OK},"z_complexity":{COMPLEXITY_OK},"unasked":{TIER_OK}}}"#
                ),
                SignalError::UnexpectedAnswer,
            ),
            (
                // Both directions wrong at once, which pins the order: the
                // missing id is roundhouse's own string and the unasked one is
                // the service's, so the one that can be named leads.
                "one id missing and another unasked",
                format!(r#"{{"a_tier":{TIER_OK},"c_other":{COMPLEXITY_OK}}}"#),
                SignalError::MissingAnswer,
            ),
            (
                "one malformed distribution among well-formed ones",
                format!(
                    r#"{{"a_tier":{TIER_OK},"z_complexity":{{"type":"choice","choice":"high","probabilities":{{"low":0.1,"medium":0.3,"high":0.2}},"confidence":0.71}}}}"#
                ),
                SignalError::SumIsNotOne,
            ),
            (
                "one answer of the wrong question type among well-formed ones",
                format!(
                    r#"{{"a_tier":{TIER_OK},"z_complexity":{{"type":"score","score":2.0,"choice":"high","probabilities":{{"low":0.1,"medium":0.3,"high":0.6}},"confidence":0.71}}}}"#
                ),
                SignalError::NotAChoice,
            ),
        ] {
            let reply = SystemOneClient::reply(&envelope(&answers), &questions())
                .unwrap_or_else(|error| panic!("{why}: the envelope parses: {error}"));
            assert_eq!(
                reply.answers,
                Err(expected),
                "{why}: one unusable answer makes the batch unusable, and no \
                 caller should have to decide whether a partial set is enough"
            );
            assert_eq!(
                reply.usage,
                billed(),
                "{why}: the service scored and charged for the whole batch, and \
                 a rejected signal must not settle that at zero"
            );
        }
    }
}
