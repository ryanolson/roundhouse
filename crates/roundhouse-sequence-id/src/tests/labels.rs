// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use http::{HeaderMap, HeaderValue};
use roundhouse_core::item::Item;

use crate::{
    CLAUDE_AGENT_HEADER, CLAUDE_SESSION_HEADER, Label, LabelError, LabelSource, Labeled,
    RequestView, Surface, client_session, label, messages_label, session_component,
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

pub(super) fn view<'a>(
    surface: Surface,
    headers: &'a HeaderMap,
    prompt_cache_key: Option<&'a str>,
) -> RequestView<'a> {
    RequestView {
        surface,
        headers,
        items: &[],
        tools: None,
        metadata_user_id: None,
        prompt_cache_key,
    }
}

fn named(name: &str, source: LabelSource) -> Result<Labeled, LabelError> {
    Ok(Labeled::Named(Label {
        name: name.to_owned(),
        source,
        agent_scoped: false,
    }))
}

#[test]
fn codex_header_precedence_is_thread_then_session_then_cache_key() {
    let all = headers(&[("session-id", "root"), ("thread-id", "thread-2")]);
    let session_only = headers(&[("session-id", "root")]);
    let thread_only = headers(&[("thread-id", "thread-2")]);
    let none = HeaderMap::new();
    let key = Some("cache-key");
    let responses = |h, k| label(&view(Surface::OpenAiResponses, h, k));

    assert_eq!(
        responses(&all, key),
        named("thread-2", LabelSource::CodexThread)
    );
    assert_eq!(
        responses(&thread_only, key),
        named("thread-2", LabelSource::CodexThread)
    );
    assert_eq!(
        responses(&session_only, key),
        named("root", LabelSource::CodexSession)
    );
    assert_eq!(
        responses(&none, key),
        named("cache-key", LabelSource::PromptCacheKey)
    );

    // Taken as sent: a value is never trimmed on this surface, because the
    // server never trimmed it and a trimmed label is another conversation's key.
    let spaced = headers(&[("thread-id", " t ")]);
    assert_eq!(
        responses(&spaced, None),
        named(" t ", LabelSource::CodexThread)
    );
    assert_eq!(
        responses(&none, Some(" ")),
        named(" ", LabelSource::PromptCacheKey)
    );
}

#[test]
fn no_name_is_an_unnamed_error() {
    let none = HeaderMap::new();
    let items = [Item::system_text("be brief"), Item::user_text("hi")];
    for key in [None, Some("")] {
        // Items do not help: a content fingerprint may stand in for a cache
        // hint, never for the identity of an append-only history.
        let view = RequestView {
            items: &items,
            ..view(Surface::OpenAiResponses, &none, key)
        };
        assert_eq!(label(&view), Err(LabelError::Unnamed), "{key:?}");
    }
    assert_eq!(
        LabelError::Unnamed.to_string(),
        "a `thread-id`, `session-id`, or `prompt_cache_key` is required to name the conversation"
    );
}

#[test]
fn a_blank_or_non_ascii_responses_header_is_the_same_422() {
    let non_ascii = HeaderValue::from_bytes("café".as_bytes()).unwrap();
    for name in ["session-id", "thread-id"] {
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
            let got = label(&view(Surface::OpenAiResponses, case, Some("cache-key")));
            assert_eq!(got, Err(LabelError::InvalidHeader(name)), "{case:?}");
            assert_eq!(
                got.unwrap_err().to_string(),
                format!("`{name}` must be a non-empty ASCII header")
            );
        }
    }
    // A bad `thread-id` is refused even when `session-id` alone would name
    // the request, and two bad headers are refused in the server's order.
    let bad_thread = headers(&[("session-id", "root"), ("thread-id", " ")]);
    assert_eq!(
        label(&view(Surface::OpenAiResponses, &bad_thread, None)),
        Err(LabelError::InvalidHeader("thread-id"))
    );
    let both = headers(&[
        ("session-id", ""),
        ("thread-id", ""),
        ("x-codex-window-id", ""),
    ]);
    assert_eq!(
        label(&view(Surface::OpenAiResponses, &both, None)),
        Err(LabelError::InvalidHeader("session-id"))
    );
}

/// The window header's 422 stays in the server's adapter (plan §3.1), so
/// `label` must not raise it — or the adapter's own check would be dead and
/// its ordering against `Unnamed` would be decided here, silently.
#[test]
fn the_window_header_is_the_adapters_refusal_not_the_labels() {
    let blank_window = headers(&[("thread-id", "t"), ("x-codex-window-id", " ")]);
    assert_eq!(
        label(&view(Surface::OpenAiResponses, &blank_window, None)),
        named("t", LabelSource::CodexThread)
    );
    let unnamed_blank_window = headers(&[("x-codex-window-id", " ")]);
    assert_eq!(
        label(&view(Surface::OpenAiResponses, &unnamed_blank_window, None)),
        Err(LabelError::Unnamed)
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
        let view = RequestView {
            metadata_user_id: user_id,
            ..view(Surface::AnthropicMessages, case, Some("cache-key"))
        };
        assert_eq!(label(&view), Ok(Labeled::Anonymous), "{case:?}");
        assert_eq!(messages_label(case, user_id), None);
    }
}

#[test]
fn the_messages_label_carries_its_source_and_its_agent() {
    let scoped = headers(&[
        ("x-claude-code-session-id", " s1 "),
        ("x-claude-code-agent-id", "a1"),
    ]);
    let agent = RequestView {
        metadata_user_id: Some(r#"{"session_id":"other"}"#),
        ..view(Surface::AnthropicMessages, &scoped, None)
    };
    assert_eq!(
        label(&agent),
        Ok(Labeled::Named(Label {
            name: "anthropic_messages/s1/agent/a1".into(),
            source: LabelSource::ClaudeSession,
            agent_scoped: true,
        }))
    );
    let bare = HeaderMap::new();
    let legacy = RequestView {
        metadata_user_id: Some("user_ab_account_cd_session_s2"),
        ..view(Surface::AnthropicMessages, &bare, None)
    };
    assert_eq!(
        label(&legacy),
        Ok(Labeled::Named(Label {
            name: "anthropic_messages/s2".into(),
            source: LabelSource::ClaudeUserId,
            agent_scoped: false,
        }))
    );
    assert_eq!(
        client_session(&legacy),
        None,
        "user_id is a label rung, not the session"
    );
    assert_eq!(client_session(&agent).as_deref(), Some("s1"));
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
        let view = RequestView {
            metadata_user_id: Some(user_id),
            ..view(Surface::AnthropicMessages, &none, None)
        };
        assert_eq!(
            label(&view),
            Ok(Labeled::Named(Label {
                name: format!("anthropic_messages/{session}"),
                source: LabelSource::ClaudeUserId,
                agent_scoped: false,
            })),
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
