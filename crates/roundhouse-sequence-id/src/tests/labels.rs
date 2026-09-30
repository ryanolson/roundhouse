// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use http::{HeaderMap, HeaderValue};

use crate::label::session_component;
use crate::{
    CLAUDE_AGENT_HEADER, CLAUDE_SESSION_HEADER, CodexHeaders, LabelError, claude_session,
    codex_conversation, messages_label,
};

pub(super) fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            *name,
            HeaderValue::from_str(value).expect("test header value"),
        );
    }
    map
}

/// The Codex label as the server derives it: read, then label.
pub(super) fn codex_label(
    headers: &HeaderMap,
    cache_key: Option<&str>,
) -> Result<String, LabelError> {
    CodexHeaders::read(headers)?.label(cache_key)
}

fn named(name: &str) -> Result<String, LabelError> {
    Ok(name.to_owned())
}

#[test]
fn codex_header_precedence_is_thread_then_session_then_cache_key() {
    let all = headers(&[("session-id", "root"), ("thread-id", "thread-2")]);
    let session_only = headers(&[("session-id", "root")]);
    let thread_only = headers(&[("thread-id", "thread-2")]);
    let none = HeaderMap::new();
    let key = Some("cache-key");

    assert_eq!(codex_label(&all, key), named("thread-2"));
    assert_eq!(codex_label(&thread_only, key), named("thread-2"));
    assert_eq!(codex_label(&session_only, key), named("root"));
    assert_eq!(codex_label(&none, key), named("cache-key"));

    // Taken as sent: a value is never trimmed on this surface, because the
    // server never trimmed it and a trimmed label is another conversation's key.
    let spaced = headers(&[("thread-id", " t ")]);
    assert_eq!(codex_label(&spaced, None), named(" t "));
    assert_eq!(codex_label(&none, Some(" ")), named(" "));

    // What was read is what is reported: the thread and the session are the
    // values the label chose between, as sent.
    // The pure precedence the server's `RequestContext` keys by, over plain
    // values: the same rungs, and an empty key is no name while a blank one is.
    assert_eq!(
        codex_conversation(Some("t"), Some("s"), Some("k")),
        Some("t")
    );
    assert_eq!(codex_conversation(None, Some("s"), Some("k")), Some("s"));
    assert_eq!(codex_conversation(None, None, Some("k")), Some("k"));
    assert_eq!(codex_conversation(None, None, Some(" ")), Some(" "));
    assert_eq!(codex_conversation(None, None, Some("")), None);
    assert_eq!(codex_conversation(None, None, None), None);

    let read = CodexHeaders::read(&all).unwrap();
    assert_eq!(
        (read.session(), read.thread(), read.window()),
        (Some("root"), Some("thread-2"), None)
    );
    // The server records the window from this getter alone, so a reader that
    // validated the header but dropped it would pass every refusal test.
    let windowed = CodexHeaders::read(&headers(&[("x-codex-window-id", "t:1")])).unwrap();
    assert_eq!(windowed.window(), Some("t:1"));
}

#[test]
fn no_name_is_an_unnamed_error() {
    let none = HeaderMap::new();
    for key in [None, Some("")] {
        // Nothing but the cache key can stand in: a content fingerprint may be
        // a cache hint, never the identity of an append-only history — which is
        // why the label takes no items at all.
        assert_eq!(codex_label(&none, key), Err(LabelError::Unnamed), "{key:?}");
    }
    // A window names nothing, even a valid one.
    let window = headers(&[("x-codex-window-id", "t:3")]);
    assert_eq!(codex_label(&window, None), Err(LabelError::Unnamed));
    assert_eq!(
        LabelError::Unnamed.to_string(),
        "a `thread-id`, `session-id`, or `prompt_cache_key` is required to name the conversation"
    );
}

#[test]
fn a_blank_or_non_ascii_responses_header_is_the_same_422() {
    let non_ascii = HeaderValue::from_bytes("café".as_bytes()).unwrap();
    for name in ["session-id", "thread-id", "x-codex-window-id"] {
        let mut cases: Vec<HeaderMap> = ["", "   ", "\t"]
            .iter()
            .map(|blank| headers(&[(name, *blank)]))
            .collect();
        let mut bad = HeaderMap::new();
        bad.insert(name, non_ascii.clone());
        cases.push(bad);
        for case in &cases {
            // A usable cache key does not rescue a bad header: the server
            // refused before it looked for a name, and still does.
            let got = codex_label(case, Some("cache-key"));
            assert_eq!(got, Err(LabelError::InvalidHeader(name)), "{case:?}");
            assert_eq!(
                got.unwrap_err().to_string(),
                format!("`{name}` must be a non-empty ASCII header")
            );
        }
    }
    // A bad `thread-id` is refused even when `session-id` alone would name
    // the request, and a bad window even when a thread does.
    let bad_thread = headers(&[("session-id", "root"), ("thread-id", " ")]);
    assert_eq!(
        codex_label(&bad_thread, None),
        Err(LabelError::InvalidHeader("thread-id"))
    );
    let bad_window = headers(&[("thread-id", "t"), ("x-codex-window-id", " ")]);
    assert_eq!(
        codex_label(&bad_window, None),
        Err(LabelError::InvalidHeader("x-codex-window-id"))
    );
}

/// **The refusal order is the server's**: `session-id`, then `thread-id`, then
/// the window, then no name — so a client sending several bad headers is told
/// about the one it was always told about first.
#[test]
fn codex_refusals_come_in_the_servers_order() {
    let all_bad = headers(&[
        ("session-id", ""),
        ("thread-id", ""),
        ("x-codex-window-id", ""),
    ]);
    assert_eq!(
        codex_label(&all_bad, None),
        Err(LabelError::InvalidHeader("session-id"))
    );
    let thread_and_window = headers(&[("thread-id", ""), ("x-codex-window-id", "")]);
    assert_eq!(
        codex_label(&thread_and_window, None),
        Err(LabelError::InvalidHeader("thread-id"))
    );
    // A bad window outranks a missing name: `read` refuses it before `label`
    // is reachable at all.
    let unnamed_blank_window = headers(&[("x-codex-window-id", " ")]);
    assert_eq!(
        codex_label(&unnamed_blank_window, None),
        Err(LabelError::InvalidHeader("x-codex-window-id"))
    );
}

#[test]
fn a_messages_request_with_no_name_is_anonymous() {
    let none = HeaderMap::new();
    let blank = headers(&[
        ("x-claude-code-session-id", "  "),
        ("x-claude-code-agent-id", "a"),
    ]);
    // Codex's names on this surface name nothing: a surface reads its own.
    let foreign = headers(&[("session-id", "root"), ("thread-id", "t")]);
    for (case, user_id) in [(&none, None), (&blank, Some("   ")), (&foreign, None)] {
        assert_eq!(messages_label(case, user_id), None, "{case:?}");
    }
}

#[test]
fn the_messages_label_prefers_the_header_and_scopes_its_agent() {
    let scoped = headers(&[
        ("x-claude-code-session-id", " s1 "),
        ("x-claude-code-agent-id", "a1"),
    ]);
    assert_eq!(
        messages_label(&scoped, Some(r#"{"session_id":"other"}"#)).as_deref(),
        Some("anthropic_messages/s1/agent/a1")
    );
    let bare = HeaderMap::new();
    let legacy = Some("user_ab_account_cd_session_s2");
    assert_eq!(
        messages_label(&bare, legacy).as_deref(),
        Some("anthropic_messages/s2")
    );
    assert_eq!(
        claude_session(&bare),
        None,
        "user_id is a label rung, not the session"
    );
    assert_eq!(claude_session(&scoped).as_deref(), Some("s1"));
}

#[test]
fn every_fixture_body_is_named_by_its_user_id() {
    use super::fixtures::{BODIES, body, user_id};
    let none = HeaderMap::new();
    for (name, _) in BODIES {
        let body = body(name);
        let user_id = user_id(&body).expect("every captured body carries metadata.user_id");
        let session = serde_json::from_str::<serde_json::Value>(user_id).unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            messages_label(&none, Some(user_id)),
            Some(format!("anthropic_messages/{session}")),
            "{name}"
        );
    }
}

// ---------------------------------------------------------------------------
// Moved from the server's `messages_api::wire`, names kept. Each asserted
// today's `session_key`; `messages_label` is that function without the
// server's request type, so the assertions are the same and only the call
// changed — which is the point of moving them rather than rewriting them.
// ---------------------------------------------------------------------------

/// The engine reads a session's dialect off its key, and this is where the
/// two meet (M12 review, F8).
///
/// Asserted over every key shape [`messages_label`] can mint — the header
/// form, the `user_id` form, and the header form with an agent tail — rather
/// than over one sample, because the fold's correctness rests on *all* of them
/// being recognisable and a sampled test would pass while one shape quietly
/// resolved to the Responses recognizer. Here rather than in core because the
/// join is between two crates: this one mints the keys and core's
/// `ControlCallDialect::of_session_key` reads them, and the one namespace
/// constant is what they now share instead of two spellings and a hope.
#[test]
fn the_session_key_this_surface_mints_folds_under_the_messages_dialect() {
    use roundhouse_core::validate::ControlCallDialect;

    let keys = [
        messages_label(&headers(&[(CLAUDE_SESSION_HEADER, "header-session")]), None),
        messages_label(
            &headers(&[
                (CLAUDE_SESSION_HEADER, "s1"),
                (CLAUDE_AGENT_HEADER, "agent-7"),
            ]),
            None,
        ),
        messages_label(
            &HeaderMap::new(),
            Some("user_9f3a_account_c0ffee_session_deadbeef"),
        ),
    ];
    for key in keys {
        let key = key.expect("each of these names a session");
        assert_eq!(
            ControlCallDialect::of_session_key(&key),
            ControlCallDialect::ClaudeMessages,
            "`{key}` is a key this surface mints, so the fold must read it \
             as the Messages surface"
        );
    }
}

/// **The header wins, then either `user_id` shape, then the whole string.**
///
/// One test for the whole order because the order *is* the ruling, and the
/// interesting failures are precedence failures: a reader that prefers
/// `user_id` binds a subagent's turns to its parent's session, and a reader
/// that handles only one `user_id` shape re-keys every session the day a
/// user upgrades their client.
#[test]
fn the_session_key_follows_r5s_order() {
    let live_user_id = r#"{"device_id":"a1b2","account_uuid":"","session_id":"11111111-2222-3333-4444-555555555555"}"#;

    // 1. The header, even when `user_id` names a different session.
    assert_eq!(
        messages_label(
            &headers(&[(CLAUDE_SESSION_HEADER, "header-session")]),
            Some(live_user_id)
        )
        .as_deref(),
        Some("anthropic_messages/header-session")
    );
    // 2a. The 2.1.247 JSON-object shape.
    assert_eq!(
        messages_label(&HeaderMap::new(), Some(live_user_id)).as_deref(),
        Some("anthropic_messages/11111111-2222-3333-4444-555555555555")
    );
    // 2b. The older underscore shape, which the JSON parse does not reach.
    assert_eq!(
        messages_label(
            &HeaderMap::new(),
            Some("user_9f3a_account_c0ffee_session_deadbeef")
        )
        .as_deref(),
        Some("anthropic_messages/deadbeef")
    );
    // 3. A shape neither rung recognises is still a name.
    assert_eq!(
        messages_label(&HeaderMap::new(), Some("just-a-name")).as_deref(),
        Some("anthropic_messages/just-a-name")
    );
    // 4. Nothing at all.
    assert_eq!(messages_label(&HeaderMap::new(), None), None);
}

/// The degenerate `user_id` shapes each fall through to the next rung.
#[test]
fn a_user_id_that_names_no_session_falls_through_to_itself() {
    // JSON, but not an object.
    assert_eq!(session_component("[1,2]"), "[1,2]");
    assert_eq!(session_component("42"), "42");
    // An object with no `session_id`, and one whose `session_id` is blank.
    let no_session = r#"{"device_id":"a1b2"}"#;
    assert_eq!(session_component(no_session), no_session);
    let blank = r#"{"session_id":"   "}"#;
    assert_eq!(session_component(blank), blank);
    // The marker with nothing after it.
    assert_eq!(
        session_component("user_9f3a_session_"),
        "user_9f3a_session_"
    );
    // Whitespace never distinguishes two turns of one conversation.
    assert_eq!(session_component("  padded  "), "padded");
    assert_eq!(session_component(r#"{"session_id":"  abc  "}"#), "abc");
}

/// An empty header value is absent, not a session named "".
#[test]
fn a_blank_session_header_falls_through_to_the_body() {
    assert_eq!(
        messages_label(
            &headers(&[(CLAUDE_SESSION_HEADER, "   ")]),
            Some("from-body")
        )
        .as_deref(),
        Some("anthropic_messages/from-body")
    );
    assert_eq!(
        messages_label(&headers(&[(CLAUDE_SESSION_HEADER, "")]), None),
        None
    );
}

/// **Every derived name carries its dialect, and its agent when there is
/// one** (M11.1 review, F6).
///
/// Two collisions this closes, both of which forked a session on *every*
/// alternating turn rather than once: a Responses `prompt_cache_key` that
/// reads the same as a Messages session id under one principal, and a
/// Task-tool subagent that inherits its parent's session id. Neither is an
/// edited conversation; both were two conversations sharing a log, which is
/// the one thing prefix admission can never reconcile.
///
/// The last assertion is the one that keeps this cheap: with no agent
/// header the parent's name gains nothing beyond the dialect, so a
/// deployment whose clients never send it is unaffected.
#[test]
fn a_derived_name_carries_its_dialect_and_its_agent() {
    assert_eq!(
        messages_label(&headers(&[(CLAUDE_SESSION_HEADER, "s1")]), None).as_deref(),
        Some("anthropic_messages/s1"),
    );
    assert_eq!(
        messages_label(
            &headers(&[
                (CLAUDE_SESSION_HEADER, "s1"),
                (CLAUDE_AGENT_HEADER, "agent-7")
            ]),
            None
        )
        .as_deref(),
        Some("anthropic_messages/s1/agent/agent-7"),
    );
    // The agent joins a `user_id`-derived name too: the rung a name came
    // from is not a reason to scope it differently.
    assert_eq!(
        messages_label(
            &headers(&[(CLAUDE_AGENT_HEADER, "agent-7")]),
            Some("from-body")
        )
        .as_deref(),
        Some("anthropic_messages/from-body/agent/agent-7"),
    );
    // A blank agent header is absent, not an agent named "": otherwise a
    // client that sent the header empty would get a session of its own that
    // no later turn could name again.
    assert_eq!(
        messages_label(
            &headers(&[(CLAUDE_SESSION_HEADER, "s1"), (CLAUDE_AGENT_HEADER, "  ")]),
            None
        )
        .as_deref(),
        Some("anthropic_messages/s1"),
    );
    // A name a Responses client could choose can no longer reach a Messages
    // session, whatever it spells.
    assert_ne!(
        messages_label(&headers(&[(CLAUDE_SESSION_HEADER, "shared")]), None).as_deref(),
        Some("shared"),
    );
}

/// Mutation 2i: the pre-2.1.247 shape trims what follows `_session_`.
#[test]
fn the_legacy_user_id_shape_is_trimmed_after_its_marker() {
    assert_eq!(session_component("user_9f3a_account_c1_session_ s1"), "s1");
    assert_eq!(
        session_component("user_9f3a_account_c1_session_  s1  "),
        "s1"
    );
}
