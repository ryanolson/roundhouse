// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Session and sequence identity: one request in, five facts out.
//!
//! - **The client**, exactly or as declared ([`detect_client`]).
//! - **The session**, the client's root identity ([`client_session`]). Never a
//!   KV key: every sub-agent of one Codex root shares it.
//! - **The label**, the lineage name before qualification ([`label`]). Its
//!   values are today's, byte for byte — [`messages_label`] is the Messages
//!   rung that `messages_api::wire::session_key` used to be, and the Responses
//!   rungs are the label half of `RequestContext::from_request`.
//! - **The client's own compaction signals** ([`client_signals`],
//!   [`content_marker`]), read only as exact literals.
//! - **The keyed digests** ([`TipKeyer`], [`SequenceDigester`]) built on the
//!   unkeyed chain in [`roundhouse_core::item::chain`].
//!
//! # Pure, and why that is the boundary
//!
//! No store, no network, no clock, no environment, and no async in the API.
//! The server passes the deployment secret as a value and keeps what reads the
//! process or the clock — `anonymous_key`, `ControlPlane::qualify`, and the
//! `x-codex-window-id` refusal of its `RequestContext` adapter. A function here
//! is therefore the same answer on every node for the same request, which is
//! the property a digest compared across nodes needs and the one a stray
//! `now_ms()` would silently break.
//!
//! # Exact or nothing
//!
//! Every marker this crate reads is a literal quoted from the client's source,
//! and a near match is no match. A client that changes a sentence then fails
//! toward "no signal" — an ordinary fresh sequence, visible in a count — rather
//! than toward a guessed compaction that frees KV a live lineage still needs.

mod detect;
mod keyed;
mod label;
mod signals;

#[cfg(test)]
mod tests;

pub use roundhouse_core::ids::MESSAGES_DIALECT_NAMESPACE;

pub use detect::{
    AttributionBlock, Client, Confidence, Detection, attribution_block, detect_client,
    without_attribution_block,
};
pub use keyed::{
    Anchor, SequenceDigest, SequenceDigester, SequenceKey, TipKey, TipKeyer, ToolsDigest,
    new_anchor, prefix_fingerprint, tools_digest,
};
pub use label::{
    Label, LabelError, LabelSource, Labeled, client_session, label, messages_label,
    session_component,
};
pub use signals::{
    ClientSignals, CodexWindow, CompactionKind, ContentMarker, RequestPurpose, client_signals,
    content_marker,
};

use roundhouse_core::item::Item;

/// The header Claude Code names its session with.
///
/// Confirmed live on every inference request at 2.1.247: a fresh UUID per
/// invocation unless `CLAUDE_CODE_SESSION_ID` is set, stable across
/// `--continue`. It is read first because it is the clean seam — no body
/// parsing, and the value is the session id rather than something the session
/// id has to be dug out of.
pub const CLAUDE_SESSION_HEADER: &str = "x-claude-code-session-id";

/// The header a Task-tool subagent identifies itself with.
///
/// A subagent runs inside the parent's process and inherits the parent's
/// session id, so without this the two interleave their turns on one log — and
/// because neither one's resent history contains the other's items, every
/// alternating turn diverges and forks. Treated as *part of the name* rather
/// than as a reason to open an anonymous session: a subagent is a conversation
/// of its own that a later turn of the same subagent should continue.
pub const CLAUDE_AGENT_HEADER: &str = "x-claude-code-agent-id";

/// Which wire a request arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Surface {
    AnthropicMessages,
    OpenAiResponses,
}

impl Surface {
    /// The spelling that enters the sequence digest.
    ///
    /// Fixed strings rather than `Debug`, because the digest is compared
    /// across releases and a renamed variant must not re-key every sequence.
    pub fn wire_name(self) -> &'static str {
        match self {
            Surface::AnthropicMessages => "anthropic_messages",
            Surface::OpenAiResponses => "openai_responses",
        }
    }
}

/// What a handler already holds once it has canonicalized a request.
///
/// Borrowed, so building one costs nothing and the crate never owns request
/// data it could be tempted to keep.
pub struct RequestView<'a> {
    pub surface: Surface,
    pub headers: &'a http::HeaderMap,
    /// Canonical items, as admission sees them.
    pub items: &'a [Item],
    /// Declared tools, as sent.
    pub tools: Option<&'a serde_json::Value>,
    /// Messages only.
    pub metadata_user_id: Option<&'a str>,
    /// Responses only.
    pub prompt_cache_key: Option<&'a str>,
}
