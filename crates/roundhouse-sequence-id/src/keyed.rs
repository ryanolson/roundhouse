// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The keyed digests built on the unkeyed chain (plan §3.2).
//!
//! ```text
//! t   = SHA-256(tools.to_string())   or 32 zero bytes when none declared
//! k_P = HMAC-SHA256(K, "rh-prefix-scope-v1\0" || ns(P))
//! L_i = HMAC-SHA256(k_P, "rh-prefix-tip-v1\0" || t || c_i)[..16]
//! q   = lp(ns(P)) || lp(surface.wire_name()) || lp(label) || u32be(generation)
//! S   = HMAC-SHA256(K, "rh-sequence-v1\0" || q)[..16]
//! I   = HMAC-SHA256(K, "rh-invalidation-v1\0" || S_pred || (S_succ or 16 zero bytes))[..16]
//! F   = hex(SHA-256("rh-cache-hint-v1\0" || c_f))
//! ```
//!
//! **Every value has its own domain prefix**, so a digest never equals a tip
//! key and an invalidation id never equals either, whatever the inputs.
//!
//! **Tips are keyed per principal.** Two principals with the same content get
//! unrelated tips, so no lookup crosses a principal and the store holds nothing
//! a reader without the secret can test a guessed prompt against. `S` is the
//! one keyed value that leaves Roundhouse (as `x-dynamo-session-id`); a
//! deployment learns from it only which requests belong to one sequence, which
//! the content already shows.
//!
//! **Tools are not in the chain.** They enter only the tip key, so a tool
//! change makes a new tip the way it makes a new provider cache prefix. That
//! under-claims overlap, which is the safe direction.

use hmac::{Hmac, Mac};
use roundhouse_core::item::chain::{Chain, ChainValue};
use roundhouse_core::item::{Item, Role};
use sha2::{Digest, Sha256};

use crate::Surface;

type HmacSha256 = Hmac<Sha256>;

const SCOPE_DOMAIN: &[u8] = b"rh-prefix-scope-v1\0";
const TIP_DOMAIN: &[u8] = b"rh-prefix-tip-v1\0";
const SEQUENCE_DOMAIN: &[u8] = b"rh-sequence-v1\0";
const INVALIDATION_DOMAIN: &[u8] = b"rh-invalidation-v1\0";
const CACHE_HINT_DOMAIN: &[u8] = b"rh-cache-hint-v1\0";

/// `t`: the declared tools, as one digest.
pub struct ToolsDigest(pub [u8; 32]);

/// The tools digest of a request.
///
/// Over the parsed value's `to_string`, which is sorted-key compact JSON
/// because `serde_json`'s `preserve_order` is off workspace-wide — so a body a
/// chained Relay alphabetized digests like the one the client sent. A missing
/// field and a JSON `null` both mean "none declared" and are the zero digest;
/// an empty list is a declaration and digests as one, which at worst makes a
/// new tip where the provider would have shared a prefix — the safe direction.
pub fn tools_digest(tools: Option<&serde_json::Value>) -> ToolsDigest {
    match tools {
        None | Some(serde_json::Value::Null) => ToolsDigest([0; 32]),
        Some(tools) => ToolsDigest(Sha256::digest(tools.to_string().as_bytes()).into()),
    }
}

/// `L_i`: the stored key of one chain link, for one principal and one tool set.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TipKey(pub [u8; 16]);

/// A KV lineage family. A fork that resends a lineage's prompt shares it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Anchor(pub [u8; 16]);

/// Keys chain links for one principal: holds `k_P`, never `K`.
#[derive(Clone)]
pub struct TipKeyer {
    scope: HmacSha256,
}

impl TipKeyer {
    pub fn new(deployment_secret: &[u8], principal_namespace: &str) -> Self {
        let mut scope = keyed(deployment_secret);
        scope.update(SCOPE_DOMAIN);
        scope.update(principal_namespace.as_bytes());
        let scope_key = scope.finalize().into_bytes();
        Self {
            scope: keyed(&scope_key),
        }
    }

    pub fn tip_key(&self, tools: &ToolsDigest, link: &ChainValue) -> TipKey {
        let mut mac = self.scope.clone();
        mac.update(TIP_DOMAIN);
        mac.update(&tools.0);
        mac.update(link.as_bytes());
        TipKey(truncated(mac))
    }

    pub fn tip_keys(&self, chain: &Chain, tools: &ToolsDigest) -> Vec<TipKey> {
        chain
            .links()
            .iter()
            .map(|link| self.tip_key(tools, link))
            .collect()
    }
}

/// `L_{m-1}` of the first dispatched prompt; `None` for an empty prompt.
///
/// The last tip rather than an earlier one because only the last commits to
/// the whole prompt: two lineages share an anchor only when one resent the
/// other's entire first prompt, which is what a fork is. An earlier tip would
/// put every session that shares a system prompt in one family.
pub fn new_anchor(first_prompt_keys: &[TipKey]) -> Option<Anchor> {
    first_prompt_keys.last().map(|tip| Anchor(tip.0))
}

/// The four fields that name one sequence.
pub struct SequenceKey<'a> {
    /// The principal namespace `ControlPlane::qualify` prepends; empty in
    /// `Open` mode.
    pub namespace: &'a str,
    pub surface: Surface,
    /// The unqualified label, as [`label`](crate::label) returns it.
    pub label: &'a str,
    pub generation: u32,
}

/// `S`, sent as `x-dynamo-session-id`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SequenceDigest(pub [u8; 16]);

impl SequenceDigest {
    /// All 32 hex characters: the header value.
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    /// The first 8 hex characters: the most a log line may carry (§3.8).
    pub fn short(&self) -> String {
        hex(&self.0[..4])
    }
}

/// Digests sequence keys and invalidations under the deployment secret `K`.
#[derive(Clone)]
pub struct SequenceDigester {
    secret: HmacSha256,
}

impl SequenceDigester {
    pub fn new(deployment_secret: &[u8]) -> Self {
        Self {
            secret: keyed(deployment_secret),
        }
    }

    /// `S` for one sequence key.
    ///
    /// Every variable-length field is length-prefixed because a label comes
    /// from a header or from `metadata.user_id` JSON, and a separator byte
    /// could appear in the latter: `("a/b", "c")` and `("a", "b/c")` must not
    /// name one sequence.
    pub fn digest(&self, key: &SequenceKey<'_>) -> SequenceDigest {
        let mut mac = self.secret.clone();
        mac.update(SEQUENCE_DOMAIN);
        length_prefixed(&mut mac, key.namespace.as_bytes());
        length_prefixed(&mut mac, key.surface.wire_name().as_bytes());
        length_prefixed(&mut mac, key.label.as_bytes());
        mac.update(&key.generation.to_be_bytes());
        SequenceDigest(truncated(mac))
    }

    /// `I`: names one supersession, predecessor to successor (or to none).
    pub fn invalidation_id(
        &self,
        predecessor: &SequenceDigest,
        successor: Option<&SequenceDigest>,
    ) -> [u8; 16] {
        let mut mac = self.secret.clone();
        mac.update(INVALIDATION_DOMAIN);
        mac.update(&predecessor.0);
        mac.update(&successor.map_or([0; 16], |successor| successor.0));
        truncated(mac)
    }
}

/// `F`: the fallback `prompt_cache_key` (Q10), through the first `User` item,
/// or through the last item when there is none.
///
/// A one-way function of one link, so it cannot be extended to later links,
/// and unkeyed, so it works without a secret as the fingerprint it replaces
/// does. `chain` is the chain of `items`; if it is shorter than the position
/// it needs, the link is computed from `items` rather than guessed, because a
/// fingerprint of the wrong prefix is a cache hint that silently points at
/// another conversation's prefix.
pub fn prefix_fingerprint(chain: &Chain, items: &[Item]) -> String {
    let mut hash = Sha256::new();
    hash.update(CACHE_HINT_DOMAIN);
    let through = items
        .iter()
        .position(|item| item.role == Role::User)
        .or_else(|| items.len().checked_sub(1));
    if let Some(index) = through {
        match chain.link(index) {
            Some(link) => hash.update(link.as_bytes()),
            None => hash.update(Chain::over(&items[..=index]).links()[index].as_bytes()),
        }
    }
    hex(&hash.finalize())
}

fn keyed(key: &[u8]) -> HmacSha256 {
    HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length")
}

fn truncated(mac: HmacSha256) -> [u8; 16] {
    let full = mac.finalize().into_bytes();
    let mut out = [0; 16];
    out.copy_from_slice(&full[..16]);
    out
}

fn length_prefixed(mac: &mut HmacSha256, field: &[u8]) {
    let len = u32::try_from(field.len()).expect("a sequence key field is shorter than 4 GiB");
    mac.update(&len.to_be_bytes());
    mac.update(field);
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
