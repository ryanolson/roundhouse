// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use http::{HeaderMap, HeaderValue};
use roundhouse_core::item::Item;

use crate::{
    Label, LabelError, LabelSource, Labeled, RequestView, Surface, client_session, label,
    messages_label,
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
