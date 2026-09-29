// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which client sent a request, and how sure that answer is.
//!
//! **Exact or declared, and only one thing is exact.** Claude Code puts an
//! attribution block at canonical item 0 whose full shape no other client has
//! a reason to produce; a header is a claim any client can make by copying a
//! string. Routing and the dispatch-projection strip act only on [`Confidence::Exact`],
//! so a detection that guessed would change what a model is sent.

use http::HeaderMap;
use roundhouse_core::item::{Item, ItemContent, Role};

use crate::RequestView;

/// The attribution block's fixed head, through the `=` of its first field.
const BLOCK_HEAD: &str = "x-anthropic-billing-header: cc_version=";

/// The separator between the block's two fields, both included in full.
const BLOCK_ENTRYPOINT: &str = "; cc_entrypoint=";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Client {
    ClaudeCode { version: Option<String> },
    Codex,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// The exact attribution block at item 0 (Claude Code only).
    Exact,
    /// A client header alone: `user-agent`, `x-app`, `originator`, or an
    /// `x-codex-*` header. Any client can send one.
    Declared,
    NoSignal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub client: Client,
    pub confidence: Confidence,
}

/// Claude Code's per-request attribution block, as parsed from item 0.
///
/// **Never a label.** The fingerprint is 12 bits of three characters of the
/// first typed prompt, so two unrelated sessions collide at 1 in 4,096 per
/// pair — enough to tell a client, nowhere near enough to tell a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributionBlock<'a> {
    pub version: &'a str,
    pub fingerprint: &'a str,
    pub entrypoint: &'a str,
}

/// Which client sent this request.
///
/// The block wins outright: it is exact, and its version is the client's own.
/// Headers only declare, and two declarations that name different clients
/// cancel to [`Client::Unknown`] with no signal — a request claiming to be
/// both is either a proxy stacking headers or a client imitating another, and
/// in neither case is one of the two claims the one to believe.
pub fn detect_client(view: &RequestView<'_>) -> Detection {
    if let Some(block) = attribution_block(view.items) {
        return Detection {
            client: Client::ClaudeCode {
                version: Some(block.version.to_owned()),
            },
            confidence: Confidence::Exact,
        };
    }
    let claude = declared_claude(view.headers);
    let codex = declared_codex(view.headers);
    let (client, confidence) = match (claude, codex) {
        (Some(version), false) => (Client::ClaudeCode { version }, Confidence::Declared),
        (None, true) => (Client::Codex, Confidence::Declared),
        (Some(_), true) | (None, false) => (Client::Unknown, Confidence::NoSignal),
    };
    Detection { client, confidence }
}

/// The attribution block, when item 0 is exactly one.
///
/// Exactly means item 0 is a `Developer` text item — the leading system run,
/// as canonicalization marks it — matching, in full,
/// `^x-anthropic-billing-header: cc_version=([0-9]+\.[0-9]+\.[0-9]+)\.([0-9a-f]{3}); cc_entrypoint=([a-z0-9_-]+);$`.
/// A block with any extra field is not one, so a client that adds a field
/// fails toward "no strip" rather than toward stripping an item that now
/// carries something a model should see. Parsed by hand rather than with a
/// regex engine: the grammar is four literal pieces and three character
/// classes, and a dependency for that would be the largest thing in the crate.
pub fn attribution_block(items: &[Item]) -> Option<AttributionBlock<'_>> {
    let Item {
        role: Role::Developer,
        content: ItemContent::Text { text },
        ..
    } = items.first()?
    else {
        return None;
    };
    let rest = text.strip_prefix(BLOCK_HEAD)?;
    let (stamp, entrypoint) = rest.split_once(BLOCK_ENTRYPOINT)?;
    let entrypoint = entrypoint.strip_suffix(';')?;
    let (version, fingerprint) = stamp.rsplit_once('.')?;
    let dotted = version.split('.').collect::<Vec<_>>();
    let is_version = dotted.len() == 3 && dotted.iter().all(|part| all_of(part, is_digit));
    let is_fingerprint = fingerprint.len() == 3 && all_of(fingerprint, is_lower_hex);
    let is_entrypoint = all_of(entrypoint, is_entrypoint_char);
    (is_version && is_fingerprint && is_entrypoint).then_some(AttributionBlock {
        version,
        fingerprint,
        entrypoint,
    })
}

/// `items` without the attribution block, for the dispatch projection only.
///
/// Returns `items` unchanged unless item 0 is the exact block. Canonical
/// items, admission, and the chain never call this: the block is an ordinary
/// stored item, and the chain must name what the client sent.
pub fn without_attribution_block(items: &[Item]) -> &[Item] {
    match attribution_block(items) {
        Some(_) => &items[1..],
        None => items,
    }
}

/// `Some(version)` when a header declares Claude Code; the version is read
/// from `user-agent: claude-cli/<version> …` when that is what declared it.
fn declared_claude(headers: &HeaderMap) -> Option<Option<String>> {
    let from_agent = header(headers, "user-agent")
        .and_then(|agent| agent.strip_prefix("claude-cli/"))
        .map(|rest| {
            let version = rest.split(' ').next().unwrap_or_default();
            (!version.is_empty()).then(|| version.to_owned())
        });
    let from_app = header(headers, "x-app") == Some("cli");
    match from_agent {
        Some(version) => Some(version),
        None => from_app.then_some(None),
    }
}

/// Whether a header declares Codex: its `originator` (default `codex_cli_rs`,
/// which also heads its `user-agent`), or any `x-codex-*` header.
fn declared_codex(headers: &HeaderMap) -> bool {
    header(headers, "originator").is_some_and(|value| value.starts_with("codex_"))
        || header(headers, "user-agent").is_some_and(|value| value.starts_with("codex_"))
        || headers
            .keys()
            .any(|name| name.as_str().starts_with("x-codex-"))
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn all_of(text: &str, class: fn(u8) -> bool) -> bool {
    !text.is_empty() && text.bytes().all(class)
}

fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

fn is_lower_hex(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'f')
}

fn is_entrypoint_char(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-')
}
