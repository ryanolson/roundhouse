// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Client identity and a content fingerprint are independent cache inputs.

use axum::http::HeaderMap;
use roundhouse_core::item::{Item, Role};
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
}

impl RequestContext {
    pub(crate) fn from_request(
        headers: &HeaderMap,
        cache_key: Option<&str>,
        items: &[Item],
    ) -> Result<Self, ApiError> {
        let session_id = header(headers, "session-id")?;
        let thread_id = header(headers, "thread-id")?;
        let window_id = header(headers, "x-codex-window-id")?;
        let prefix_fingerprint = prefix_fingerprint(items);
        let cache_key = cache_key.filter(|key| !key.is_empty());
        // A fingerprint can be shared by unrelated conversations. It cannot
        // supply the missing identity of an append-only history.
        if session_id.is_none() && thread_id.is_none() && cache_key.is_none() {
            return Err(ApiError::unprocessable(
                "a `thread-id`, `session-id`, or `prompt_cache_key` is required to name the conversation",
            ));
        }
        Ok(Self {
            session_id,
            thread_id,
            prompt_cache_key: cache_key.unwrap_or(&prefix_fingerprint).to_owned(),
            prefix_fingerprint,
            window_id,
        })
    }

    pub(crate) fn conversation_key(&self) -> &str {
        self.thread_id
            .as_deref()
            .or(self.session_id.as_deref())
            .unwrap_or(&self.prompt_cache_key)
    }
}

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
