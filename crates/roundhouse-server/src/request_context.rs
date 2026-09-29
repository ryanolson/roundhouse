// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Client identity and a content fingerprint are independent cache inputs.

use axum::http::HeaderMap;
use roundhouse_core::item::{Item, Role};
use roundhouse_sequence_id::{
    LabelError, LabelSource, Labeled, RequestView, Surface, client_session, label,
};
use sha2::{Digest, Sha256};

use crate::http::ApiError;

/// Request metadata carried independently of Roundhouse's internal log identity.
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub prompt_cache_key: String,
    pub prefix_fingerprint: String,
    pub window_id: Option<String>,
    /// The lineage name `roundhouse_sequence_id::label` gave, kept as given.
    ///
    /// Stored rather than re-derived from the fields above, because
    /// re-deriving it would be a second copy of the crate's precedence — the
    /// exact duplication the move exists to remove — and the day the two
    /// disagreed, the log would be keyed by one and reported by the other.
    label: String,
}

impl RequestContext {
    /// The Responses surface's adapter over `roundhouse_sequence_id::label`.
    ///
    /// The name is the crate's; what stays here is the `x-codex-window-id`
    /// refusal and the content fingerprint, neither of which names anything.
    ///
    /// **The refusal order is the one this function always had** — a bad
    /// `session-id`, then a bad `thread-id`, then a bad window, then no name —
    /// because a client that sends two bad headers is told about the first one,
    /// and moving the derivation must not change which. `label` checks the two
    /// naming headers and never looks at the window, so its header refusal
    /// returns first, the window is checked next, and only then does an
    /// unnamed request get its 422. Checking the window before calling `label`
    /// would report a blank window ahead of a blank `session-id`; checking it
    /// only on success would report "no name" ahead of a blank window. The
    /// golden capture's combined-invalid cases pin the first;
    /// `an_unnamed_request_with_a_bad_window_is_refused_for_the_window` pins
    /// the second, which the capture has no case for.
    pub(crate) fn from_request(
        headers: &HeaderMap,
        cache_key: Option<&str>,
        items: &[Item],
    ) -> Result<Self, ApiError> {
        let view = RequestView {
            surface: Surface::OpenAiResponses,
            headers,
            items,
            tools: None,
            metadata_user_id: None,
            prompt_cache_key: cache_key,
        };
        let labeled = match label(&view) {
            Err(invalid @ LabelError::InvalidHeader(_)) => return Err(label_refusal(invalid)),
            labeled => labeled,
        };
        let window_id = header(headers, "x-codex-window-id")?;
        // A fingerprint can be shared by unrelated conversations. It cannot
        // supply the missing identity of an append-only history — which is
        // why `label` refuses rather than falling back to it. `Anonymous` is a
        // Messages answer the crate never gives this surface; if it ever did,
        // it is still no name here, and refusing it keeps the one rule.
        let named = match labeled {
            Ok(Labeled::Named(named)) => named,
            Ok(Labeled::Anonymous) => return Err(label_refusal(LabelError::Unnamed)),
            Err(error) => return Err(label_refusal(error)),
        };
        // `label` reads `thread-id` before `session-id`, so a thread present is
        // the thread that named the request: its value is the name, as sent.
        let thread_id = (named.source == LabelSource::CodexThread).then(|| named.name.clone());
        // Lenient where `label` is strict, but `label` has already refused a
        // malformed `session-id`, so here the two read the same bytes.
        let session_id = client_session(&view);
        let prefix_fingerprint = prefix_fingerprint(items);
        let cache_key = cache_key.filter(|key| !key.is_empty());
        Ok(Self {
            session_id,
            thread_id,
            prompt_cache_key: cache_key.unwrap_or(&prefix_fingerprint).to_owned(),
            prefix_fingerprint,
            window_id,
            label: named.name,
        })
    }

    pub(crate) fn conversation_key(&self) -> &str {
        &self.label
    }
}

/// A label refusal, as the 422 this server has always sent for it.
///
/// One mapping for both surfaces. `LabelError`'s texts are this server's bodies
/// from before the move, verbatim, so a client that parses the refusal sees
/// what it saw; the status and code are chosen here, because an HTTP status is
/// the server's business and not the pure crate's.
pub(crate) fn label_refusal(error: LabelError) -> ApiError {
    ApiError::unprocessable(error.to_string())
}

/// The window header's check, the one refusal `label` does not make.
fn header(headers: &HeaderMap, name: &str) -> Result<Option<String>, ApiError> {
    headers
        .get(name)
        .map(|value| {
            value
                .to_str()
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    ApiError::unprocessable(format!("`{name}` must be a non-empty ASCII header"))
                })
        })
        .transpose()
}

/// Hash a typed, length-delimited prefix so role markers inside text cannot
/// create an ambiguous concatenation. Response IDs never affect the digest.
pub(crate) fn prefix_fingerprint(items: &[Item]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"roundhouse-prefix-v1\0");
    for item in items {
        if matches!(item.role, Role::System | Role::Developer | Role::User) {
            let encoded = serde_json::to_vec(&(&item.role, &item.content))
                .expect("canonical items serialize");
            hash.update((encoded.len() as u64).to_be_bytes());
            hash.update(encoded);
        }
        if item.role == Role::User {
            break;
        }
    }
    hex::encode(hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_is_content_based_but_does_not_supply_conversation_identity() {
        let items = vec![Item::system_text("system"), Item::user_text("first")];
        assert!(RequestContext::from_request(&HeaderMap::new(), None, &items).is_err());
        let mut headers = HeaderMap::new();
        headers.insert("session-id", "a".parse().unwrap());
        let a = RequestContext::from_request(&headers, None, &items).unwrap();
        headers.insert("session-id", "b".parse().unwrap());
        let b = RequestContext::from_request(&headers, None, &items).unwrap();
        assert_ne!(a.conversation_key(), b.conversation_key());
        assert_eq!(a.prompt_cache_key, b.prompt_cache_key);
        assert_eq!(a.prompt_cache_key.len(), 64);
        let mut extended = items.clone();
        extended.push(Item::user_text("later"));
        assert_eq!(prefix_fingerprint(&items), prefix_fingerprint(&extended));
        assert_ne!(
            prefix_fingerprint(&items),
            prefix_fingerprint(&[Item::system_text("changed"), Item::user_text("first")])
        );
    }

    /// **A bad window outranks a missing name**, as it did before the move.
    ///
    /// The golden capture's combined-invalid cases all carry a bad naming
    /// header, so they pin `label`'s refusals ahead of the window but not the
    /// window ahead of `Unnamed`: an adapter that checked the window only once
    /// a name was found passes the whole capture. This is the case it misses.
    #[tokio::test]
    async fn an_unnamed_request_with_a_bad_window_is_refused_for_the_window() {
        let items = [Item::user_text("first")];
        for window in [" ", ""] {
            let mut headers = HeaderMap::new();
            headers.insert("x-codex-window-id", window.parse().unwrap());
            for cache_key in [None, Some("")] {
                let error = RequestContext::from_request(&headers, cache_key, &items)
                    .expect_err("neither a name nor a usable window");
                let response = axum::response::IntoResponse::into_response(error);
                assert_eq!(
                    response.status(),
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY
                );
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(
                    body["error"]["message"],
                    "`x-codex-window-id` must be a non-empty ASCII header",
                    "window {window:?}, cache key {cache_key:?}"
                );
            }
        }
    }

    /// M1 refute, mutation 3c: an empty `prompt_cache_key` is no hint, so the
    /// cache key falls back to the fingerprint even when a header named the
    /// request; a whitespace key was a hint before the move and stays one.
    #[test]
    fn an_empty_cache_key_is_no_hint_and_a_blank_one_is() {
        let items = [Item::system_text("system"), Item::user_text("first")];
        let mut headers = HeaderMap::new();
        headers.insert("session-id", "a".parse().unwrap());
        let empty = RequestContext::from_request(&headers, Some(""), &items).unwrap();
        assert!(!empty.prompt_cache_key.is_empty());
        assert_eq!(empty.prompt_cache_key, empty.prefix_fingerprint);
        let blank = RequestContext::from_request(&headers, Some(" "), &items).unwrap();
        assert_eq!(blank.prompt_cache_key, " ");
    }

    #[test]
    fn explicit_cache_hint_is_independent_of_the_fingerprint() {
        let context = RequestContext::from_request(
            &HeaderMap::new(),
            Some("client-key"),
            &[Item::user_text("hello")],
        )
        .unwrap();
        assert_eq!(context.prompt_cache_key, "client-key");
        assert_ne!(context.prompt_cache_key, context.prefix_fingerprint);
    }
}

#[cfg(test)]
mod label_golden;
