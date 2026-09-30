// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The label a request names, captured before the derivation moves.
//!
//! Both surfaces used to turn a request into a conversation label in two
//! separate places: the Messages wire module's `session_key` for Claude
//! Messages and [`RequestContext::from_request`] plus
//! [`RequestContext::conversation_key`] for Responses. The derivation has since moved into the
//! `roundhouse-sequence-id` crate, and "the move changed no label" was only a
//! claim until something had recorded what the labels were. This module
//! recorded them *first*, from the pre-move code, into
//! `tests/fixtures/golden-labels.json`; the same test keeps asserting the
//! file, now through [`claimed_label`] and `from_request` — the handlers' own
//! calls into the crate — so the move is provably behavior-preserving and
//! every later change to a label is a reviewed diff of that file rather than a
//! silent re-binding of live sessions to new conversations.
//!
//! **What is pinned.** Every Claude Messages body under `tests/fixtures/`,
//! once with no headers and once under each captured header set (so the
//! `x-claude-code-session-id` rung and the `metadata.user_id` rung are both
//! exercised on real bytes), plus one body with its metadata removed for the
//! anonymous `None`, which no captured body reaches; and a hand-built
//! Responses matrix covering each precedence rung, blank and non-ASCII values
//! of each header, the no-name 422, and combined-invalid requests, which pin
//! the order the checks run in (`session-id`, `thread-id`, then the window)
//! because the first refusal is the one a client sees. The Codex turn-metadata
//! thread fallback is not here: it is read in `responses_api`, outside
//! `from_request`, and does not feed the label.
//!
//! **Case count is part of the capture.** The test fails if a case is added or
//! removed without re-blessing, and separately if the matrix stops being the
//! size the capture claims, so a fixture that quietly loses a request cannot
//! shrink the coverage while the file keeps agreeing with itself.
//!
//! **Re-blessing.** Run this module's test with `ROUNDHOUSE_BLESS_GOLDEN_LABELS=1`
//! set and it rewrites the file from the current derivation instead of reading
//! it: `ROUNDHOUSE_BLESS_GOLDEN_LABELS=1 timeout 900 cargo test -p
//! roundhouse-server --lib label_golden`. Do that only for a change that is
//! meant to move a label, and read the diff before committing it.
//!
//! This is a unit test inside the crate, not an integration test, because
//! `RequestContext::from_request` and `conversation_key` are `pub(crate)`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::response::IntoResponse;
use roundhouse_core::item::Item;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::RequestContext;
use crate::http::ApiError;
use crate::messages_api::claimed_label;
use crate::messages_api::wire::CreateMessageParams;

const BLESS_ENV: &str = "ROUNDHOUSE_BLESS_GOLDEN_LABELS";

/// The Messages request bodies. `claude-2.1.257-mcp-wire.json` is deliberately
/// absent: it holds MCP requests, not Messages bodies.
const BODIES: [(&str, &str); 7] = [
    (
        "claude-2.1.251-turn-1",
        include_str!("../../tests/fixtures/claude-2.1.251-turn-1.json"),
    ),
    (
        "claude-2.1.251-turn-2-continue",
        include_str!("../../tests/fixtures/claude-2.1.251-turn-2-continue.json"),
    ),
    (
        "claude-2.1.257-turn-1",
        include_str!("../../tests/fixtures/claude-2.1.257-turn-1.json"),
    ),
    (
        "claude-2.1.257-turn-2-continue",
        include_str!("../../tests/fixtures/claude-2.1.257-turn-2-continue.json"),
    ),
    (
        "claude-2.1.257-turn-3-continue",
        include_str!("../../tests/fixtures/claude-2.1.257-turn-3-continue.json"),
    ),
    (
        "claude-2.1.257-mcp-turn-1",
        include_str!("../../tests/fixtures/claude-2.1.257-mcp-turn-1.json"),
    ),
    (
        "claude-2.1.257-mcp-turn-2-toolresult",
        include_str!("../../tests/fixtures/claude-2.1.257-mcp-turn-2-toolresult.json"),
    ),
];

/// The captured header sets: two requests each.
const HEADER_CAPTURES: [(&str, &str); 3] = [
    (
        "2.1.251",
        include_str!("../../tests/fixtures/claude-2.1.251-headers.json"),
    ),
    (
        "2.1.257",
        include_str!("../../tests/fixtures/claude-2.1.257-headers.json"),
    ),
    (
        "2.1.257-mcp",
        include_str!("../../tests/fixtures/claude-2.1.257-mcp-headers.json"),
    ),
];

const REQUESTS_PER_CAPTURE: usize = 2;
/// Every fixture body carries `metadata.user_id`, so none reaches the anonymous
/// rung; one body with the metadata removed does.
const ANONYMOUS_CASES: usize = 1;
const MESSAGES_CASES: usize =
    BODIES.len() * (1 + HEADER_CAPTURES.len() * REQUESTS_PER_CAPTURE) + ANONYMOUS_CASES;
const RESPONSES_CASES_AT_LEAST: usize = 12;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Golden {
    messages: Vec<MessageCase>,
    responses: Vec<ResponseCase>,
}

/// `label` is `None` for the anonymous request, which is a real answer and not
/// a missing one.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct MessageCase {
    id: String,
    label: Option<String>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct ResponseCase {
    id: String,
    outcome: Outcome,
}

/// An error is recorded as the status and body a client would receive, not as a
/// `Debug` rendering, so the capture pins what is observable and survives a
/// change to the error type's private fields.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum Outcome {
    Ok {
        conversation_key: String,
        session_id: Option<String>,
        thread_id: Option<String>,
        window_id: Option<String>,
        prompt_cache_key: String,
    },
    Err {
        status: u16,
        body: Value,
    },
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden-labels.json")
}

/// Every header of a captured request, verbatim (the redacted `x-api-key`
/// placeholder included: it is a valid value and no rung reads it).
fn header_sets() -> Vec<(String, HeaderMap)> {
    let mut sets = Vec::new();
    for (capture, raw) in HEADER_CAPTURES {
        let requests: Vec<Value> = serde_json::from_str(raw).expect("a header capture is JSON");
        assert_eq!(
            requests.len(),
            REQUESTS_PER_CAPTURE,
            "header capture {capture} must hold {REQUESTS_PER_CAPTURE} requests"
        );
        for (index, request) in requests.iter().enumerate() {
            let mut headers = HeaderMap::new();
            for (name, value) in request["headers"].as_object().expect("a headers object") {
                headers.insert(
                    HeaderName::from_bytes(name.as_bytes()).expect("a captured header name"),
                    HeaderValue::from_str(value.as_str().expect("a captured header value"))
                        .expect("a captured header value"),
                );
            }
            sets.push((format!("headers-{capture}#{index}"), headers));
        }
    }
    sets
}

fn messages_cases() -> Vec<MessageCase> {
    let mut cases = Vec::new();
    let sets = header_sets();
    for (body_id, body) in BODIES {
        let params: CreateMessageParams =
            serde_json::from_str(body).expect("a captured Messages body");
        let anonymous = ("no-headers".to_owned(), HeaderMap::new());
        for (headers_id, headers) in std::iter::once(&anonymous).chain(&sets) {
            cases.push(MessageCase {
                id: format!("messages/{body_id}/{headers_id}"),
                label: handler_label(headers, &params),
            });
        }
    }
    let (body_id, body) = BODIES[2];
    let mut anonymous_body: Value = serde_json::from_str(body).expect("a captured Messages body");
    assert!(
        anonymous_body
            .as_object_mut()
            .expect("a body is an object")
            .remove("metadata")
            .is_some(),
        "the fixture must carry the metadata this case removes"
    );
    cases.push(MessageCase {
        id: format!("messages/{body_id}-without-metadata/no-headers"),
        label: handler_label(
            &HeaderMap::new(),
            &serde_json::from_value(anonymous_body).expect("a Messages body"),
        ),
    });
    cases
}

/// The handler's label for one request, through the handler's own glue.
fn handler_label(headers: &HeaderMap, params: &CreateMessageParams) -> Option<String> {
    claimed_label(headers, params)
}

/// One Responses request: raw header bytes (so a non-ASCII value can be built)
/// and the body's `prompt_cache_key`.
struct Request {
    id: &'static str,
    headers: &'static [(&'static str, &'static [u8])],
    cache_key: Option<&'static str>,
}

const fn request(
    id: &'static str,
    headers: &'static [(&'static str, &'static [u8])],
    cache_key: Option<&'static str>,
) -> Request {
    Request {
        id,
        headers,
        cache_key,
    }
}

/// "café" in UTF-8: a legal header byte string that is not ASCII.
const NON_ASCII: &[u8] = "caf\u{e9}".as_bytes();

const RESPONSES: &[Request] = &[
    // Each rung alone, then each pair, then all three: the winner is the
    // highest rung present.
    request("thread-id-alone", &[("thread-id", b"thread-a")], None),
    request("session-id-alone", &[("session-id", b"session-a")], None),
    request("prompt-cache-key-alone", &[], Some("cache-a")),
    request(
        "thread-id-beats-session-id",
        &[("thread-id", b"thread-a"), ("session-id", b"session-a")],
        None,
    ),
    request(
        "thread-id-beats-prompt-cache-key",
        &[("thread-id", b"thread-a")],
        Some("cache-a"),
    ),
    request(
        "session-id-beats-prompt-cache-key",
        &[("session-id", b"session-a")],
        Some("cache-a"),
    ),
    request(
        "all-three-rungs",
        &[("thread-id", b"thread-a"), ("session-id", b"session-a")],
        Some("cache-a"),
    ),
    request(
        "window-id-is-not-a-name",
        &[("x-codex-window-id", b"window-a")],
        Some("cache-a"),
    ),
    // The value is kept as sent: nothing trims a name that is not blank.
    request(
        "padded-thread-id-is-not-trimmed",
        &[("thread-id", b"  thread-a  ")],
        None,
    ),
    // No name at all.
    request("no-name", &[], None),
    request("no-name-empty-prompt-cache-key", &[], Some("")),
    request(
        "no-name-window-id-only",
        &[("x-codex-window-id", b"w")],
        None,
    ),
    // Blank and non-ASCII values of each header. A blank header is refused
    // even when a lower rung could have named the conversation.
    request("blank-thread-id", &[("thread-id", b"   ")], Some("cache-a")),
    request("empty-thread-id", &[("thread-id", b"")], Some("cache-a")),
    request(
        "blank-session-id",
        &[("session-id", b"   ")],
        Some("cache-a"),
    ),
    request(
        "blank-session-id-beside-valid-thread-id",
        &[("session-id", b" "), ("thread-id", b"thread-a")],
        None,
    ),
    request(
        "blank-window-id",
        &[("thread-id", b"thread-a"), ("x-codex-window-id", b" ")],
        None,
    ),
    request(
        "non-ascii-thread-id",
        &[("thread-id", NON_ASCII)],
        Some("cache-a"),
    ),
    request(
        "non-ascii-session-id",
        &[("session-id", NON_ASCII)],
        Some("cache-a"),
    ),
    request(
        "non-ascii-window-id",
        &[("thread-id", b"thread-a"), ("x-codex-window-id", NON_ASCII)],
        None,
    ),
    // Several refusals at once: the first one reached is the one reported.
    request(
        "combined-invalid-session-thread-window",
        &[
            ("session-id", b" "),
            ("thread-id", b" "),
            ("x-codex-window-id", b" "),
        ],
        None,
    ),
    request(
        "combined-invalid-thread-and-window",
        &[("thread-id", b" "), ("x-codex-window-id", NON_ASCII)],
        None,
    ),
];

async fn error_outcome(error: ApiError) -> Outcome {
    let response = error.into_response();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("an error body");
    Outcome::Err {
        status,
        body: serde_json::from_slice(&bytes).expect("an error body is JSON"),
    }
}

async fn responses_cases() -> Vec<ResponseCase> {
    let items = [Item::system_text("system"), Item::user_text("first")];
    let mut cases = Vec::new();
    for request in RESPONSES {
        let mut headers = HeaderMap::new();
        for (name, value) in request.headers {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_bytes(value).expect("a representable header value"),
            );
        }
        let outcome = match RequestContext::from_request(&headers, request.cache_key, &items) {
            Ok(context) => Outcome::Ok {
                conversation_key: context.conversation_key().to_owned(),
                session_id: context.session_id.clone(),
                thread_id: context.thread_id.clone(),
                window_id: context.window_id.clone(),
                prompt_cache_key: context.prompt_cache_key.clone(),
            },
            Err(error) => error_outcome(error).await,
        };
        cases.push(ResponseCase {
            id: format!("responses/{}", request.id),
            outcome,
        });
    }
    cases
}

async fn capture() -> Golden {
    let golden = Golden {
        messages: messages_cases(),
        responses: responses_cases().await,
    };
    assert_eq!(
        golden.messages.len(),
        MESSAGES_CASES,
        "the Messages matrix is every body under every header set, and none"
    );
    assert!(
        golden.responses.len() >= RESPONSES_CASES_AT_LEAST,
        "the Responses matrix must hold at least {RESPONSES_CASES_AT_LEAST} cases"
    );
    let mut ids: Vec<_> = golden
        .messages
        .iter()
        .map(|case| &case.id)
        .chain(golden.responses.iter().map(|case| &case.id))
        .collect();
    ids.sort();
    let total = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), total, "a case id must name exactly one case");
    golden
}

/// The cases that differ between the golden file and the derivation now, keyed
/// by case id.
///
/// **By id, not by position.** The first version zipped the two lists, which
/// reads every case after an insertion or a removal as changed: adding one
/// Responses case would report the whole tail of the matrix as a moved label,
/// burying the one real difference under a screenful of false ones and
/// teaching the reader to re-bless without looking — the one habit this file
/// exists to prevent. Added, removed and changed are separate findings because
/// they mean different things: a removed id is coverage that quietly shrank, an
/// added id is coverage not yet blessed, and only a changed one is a label that
/// moved.
fn diff_by_id<'a, T: PartialEq + std::fmt::Debug + 'a>(
    golden: impl IntoIterator<Item = (&'a str, &'a T)>,
    current: impl IntoIterator<Item = (&'a str, &'a T)>,
) -> Vec<String> {
    let golden: BTreeMap<&str, &T> = golden.into_iter().collect();
    let current: BTreeMap<&str, &T> = current.into_iter().collect();
    let mut differences = Vec::new();
    for id in golden.keys().filter(|id| !current.contains_key(*id)) {
        differences.push(format!("{id}\n  removed: golden has it, now does not"));
    }
    for id in current.keys().filter(|id| !golden.contains_key(*id)) {
        differences.push(format!("{id}\n  added: now has it, golden does not"));
    }
    for (id, was) in &golden {
        if let Some(now) = current.get(id).filter(|now| *now != was) {
            differences.push(format!("{id}\n  golden {was:?}\n  now    {now:?}"));
        }
    }
    differences
}

#[tokio::test]
async fn every_label_matches_the_golden_capture() {
    let current = capture().await;
    let path = golden_path();
    if std::env::var_os(BLESS_ENV).is_some_and(|value| value == "1") {
        let mut text = serde_json::to_string_pretty(&current).expect("the capture serializes");
        text.push('\n');
        std::fs::write(&path, text).expect("write the golden capture");
        return;
    }
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{} is unreadable ({error}); create it with {BLESS_ENV}=1",
            path.display()
        )
    });
    let golden: Golden = serde_json::from_str(&text).expect("the golden capture parses");

    let mut differences = Vec::new();
    if golden.messages.len() != current.messages.len() {
        differences.push(format!(
            "Messages case count: golden {}, now {}",
            golden.messages.len(),
            current.messages.len()
        ));
    }
    if golden.responses.len() != current.responses.len() {
        differences.push(format!(
            "Responses case count: golden {}, now {}",
            golden.responses.len(),
            current.responses.len()
        ));
    }
    differences.extend(diff_by_id(
        golden.messages.iter().map(|case| (case.id.as_str(), case)),
        current.messages.iter().map(|case| (case.id.as_str(), case)),
    ));
    differences.extend(diff_by_id(
        golden.responses.iter().map(|case| (case.id.as_str(), case)),
        current
            .responses
            .iter()
            .map(|case| (case.id.as_str(), case)),
    ));
    assert!(
        differences.is_empty(),
        "a label moved; if that was the point, re-bless with {BLESS_ENV}=1:\n{}",
        differences.join("\n")
    );
}

/// Removing one case from the middle names that case and nothing else. Under
/// the positional zip this reported every later case as changed, so the
/// assertion is on the exact list, not on its being non-empty.
#[test]
fn a_removed_case_is_reported_by_id_and_does_not_cascade() {
    fn by_id(cases: &[MessageCase]) -> Vec<(&str, &MessageCase)> {
        cases.iter().map(|case| (case.id.as_str(), case)).collect()
    }

    let case = |id: &str, label: &str| MessageCase {
        id: id.to_owned(),
        label: Some(label.to_owned()),
    };
    let golden = [
        case("a", "1"),
        case("b", "2"),
        case("c", "3"),
        case("d", "4"),
    ];

    let without_b = [case("a", "1"), case("c", "3"), case("d", "4")];
    let differences = diff_by_id(by_id(&golden), by_id(&without_b));
    assert_eq!(differences.len(), 1, "{differences:#?}");
    assert!(
        differences[0].starts_with("b\n  removed"),
        "{differences:#?}"
    );

    let with_new = [
        case("a", "1"),
        case("b", "2"),
        case("b2", "9"),
        case("c", "3"),
        case("d", "4"),
    ];
    let differences = diff_by_id(by_id(&golden), by_id(&with_new));
    assert_eq!(differences.len(), 1, "{differences:#?}");
    assert!(
        differences[0].starts_with("b2\n  added"),
        "{differences:#?}"
    );

    let moved = [
        case("a", "1"),
        case("b", "2"),
        case("c", "X"),
        case("d", "4"),
    ];
    let differences = diff_by_id(by_id(&golden), by_id(&moved));
    assert_eq!(differences.len(), 1, "{differences:#?}");
    assert!(
        differences[0].starts_with("c\n  golden"),
        "{differences:#?}"
    );

    // Control: identical lists differ nowhere, so the three findings above are
    // the edits and not noise in the comparison.
    assert!(diff_by_id(by_id(&golden), by_id(&golden)).is_empty());
}
