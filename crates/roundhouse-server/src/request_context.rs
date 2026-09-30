// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Client identity and a content fingerprint are independent cache inputs.

use axum::http::HeaderMap;
use roundhouse_core::item::{Item, Role};
use roundhouse_sequence_id::{CodexHeaders, codex_conversation};
use sha2::{Digest, Sha256};

use crate::http::ApiError;

/// Request metadata carried independently of Roundhouse's internal log identity.
///
/// **The fields are the only copy.** [`conversation_key`](Self::conversation_key)
/// is computed from them on every call, through the crate's one statement of
/// Codex's precedence, rather than stored beside them: a stored name next to
/// public fields is two sources of truth, and the day a caller changed one the
/// log would be keyed by the other.
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub prompt_cache_key: String,
    pub prefix_fingerprint: String,
    pub window_id: Option<String>,
}

impl RequestContext {
    /// The Responses surface's adapter over `roundhouse_sequence_id`'s Codex
    /// reader.
    ///
    /// The name is the crate's; what stays here is the content fingerprint,
    /// which names nothing.
    ///
    /// **The refusal order is the one this function always had, and it is the
    /// order of the two `?` below**: `read` refuses a bad `session-id`, then
    /// a bad `thread-id`, then a bad window, and only then can `label` refuse
    /// an unnamed request — because a client that sends two bad headers is told
    /// about the first one, and moving the derivation must not change which.
    /// The golden capture's combined-invalid cases pin the header order;
    /// `an_unnamed_request_with_a_bad_window_is_refused_for_the_window` pins
    /// the window ahead of the missing name, which the capture has no case for.
    pub(crate) fn from_request(
        headers: &HeaderMap,
        cache_key: Option<&str>,
        items: &[Item],
    ) -> Result<Self, ApiError> {
        let codex = CodexHeaders::read(headers)?;
        // Only the refusal is wanted here: the name itself is recomputed from
        // the fields by `conversation_key`, so there is no second copy of it.
        // A fingerprint can be shared by unrelated conversations and cannot
        // supply the missing identity of an append-only history — which is
        // why `label` refuses rather than falling back to it.
        codex.label(cache_key)?;
        let prefix_fingerprint = prefix_fingerprint(items);
        let cache_key = cache_key.filter(|key| !key.is_empty());
        Ok(Self {
            session_id: codex.session().map(str::to_owned),
            thread_id: codex.thread().map(str::to_owned),
            prompt_cache_key: cache_key.unwrap_or(&prefix_fingerprint).to_owned(),
            prefix_fingerprint,
            window_id: codex.window().map(str::to_owned),
        })
    }

    /// The lineage this request names, from the public fields alone.
    ///
    /// `prompt_cache_key` stands in for the body's key: `from_request` fills
    /// it with the client's key whenever that key is non-empty, and it is only
    /// the rung reached when neither header named the request — in which case
    /// `from_request` succeeded only because the client's key was non-empty.
    /// So on every context `from_request` builds this is exactly the name
    /// `CodexHeaders::label` chose. `prompt_cache_key` is never empty there
    /// (a client key or a 64-hex fingerprint), so the fallback is reached only
    /// by a hand-emptied context, and then returns that same empty key.
    pub(crate) fn conversation_key(&self) -> &str {
        codex_conversation(
            self.thread_id.as_deref(),
            self.session_id.as_deref(),
            Some(&self.prompt_cache_key),
        )
        .unwrap_or(&self.prompt_cache_key)
    }
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
    /// header, so they pin the naming headers' refusals ahead of the window
    /// but not the window ahead of `Unnamed`: an adapter that checked the
    /// window only once a name was found passes the whole capture. This is the
    /// case it misses.
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
        // With no header to outrank it, the blank key is the conversation's
        // name, as it was before the move: a body field was never trimmed.
        let named_by_blank = RequestContext::from_request(&HeaderMap::new(), Some(" "), &items)
            .expect("a blank key names a conversation");
        assert_eq!(named_by_blank.conversation_key(), " ");
    }

    /// **The key follows the public fields, because there is no other copy.**
    /// Before, the key was a private string stored beside public
    /// `thread_id`/`session_id` fields, so a context whose `thread_id` was
    /// cleared was still keyed by the thread (M1 review, R1). Clearing a rung
    /// now falls through to the next one, as `from_request` would have.
    #[test]
    fn the_conversation_key_follows_the_public_fields() {
        let items = [Item::user_text("first")];
        let mut headers = HeaderMap::new();
        headers.insert("session-id", "s".parse().unwrap());
        headers.insert("thread-id", "t".parse().unwrap());
        let mut context = RequestContext::from_request(&headers, Some("k"), &items).unwrap();
        assert_eq!(context.conversation_key(), "t");
        context.thread_id = None;
        assert_eq!(context.conversation_key(), "s");
        context.session_id = None;
        assert_eq!(context.conversation_key(), "k");
        // And a field set is a field keyed by: no hidden name outranks it.
        context.thread_id = Some("other".into());
        assert_eq!(context.conversation_key(), "other");
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
