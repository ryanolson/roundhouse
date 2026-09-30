// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Session and sequence identity: one request in, five facts out.
//!
//! - **The client**, exactly or as declared ([`detect_client`]).
//! - **The session**, the client's root identity ([`claude_session`],
//!   [`CodexHeaders::session`]). Never a KV key: every sub-agent of one Codex
//!   root shares it.
//! - **The label**, the lineage name before qualification ([`messages_label`]
//!   for Claude Messages, [`CodexHeaders::label`] for Responses, whose
//!   precedence is [`codex_conversation`] for a caller holding plain fields). Its values
//!   are today's, byte for byte — `messages_label` is the Messages rung the
//!   server's `session_key` used to be, and the Codex reader is the label half
//!   of `RequestContext::from_request`.
//! - **The client's own compaction signals** ([`claude_signals`],
//!   [`codex_signals`], [`content_marker`]), read only as exact literals.
//! - **The keyed digests** ([`TipKeyer`], [`SequenceDigester`]) built on the
//!   unkeyed chain in [`roundhouse_core::item::chain`]. The values they produce
//!   are plain types in [`roundhouse_core::sequence`], because core's records
//!   store them and core cannot depend on this crate.
//!
//! # One API per client, not one over both
//!
//! Each client names its session in its own headers and body fields, and the
//! two derivations share no rung. So each has its own entry point taking only
//! what it reads — `messages_label(headers, user_id)` cannot fail and has no
//! cache key to ignore; `CodexHeaders::read` refuses exactly the three
//! headers Codex sends — rather than one function over a union of both
//! surfaces' inputs, whose unused half is a field a caller can fill wrongly
//! and whose impossible answers (an anonymous Responses request, a refused
//! Messages one) every caller would have to write an arm for.
//!
//! # Pure, and why that is the boundary
//!
//! No store, no network, no clock, no environment, and no async in the API.
//! The server passes the deployment secret as a value and keeps what reads the
//! process or the clock — `anonymous_key` and `ControlPlane::qualify`. A
//! function here is therefore the same answer on every node for the same
//! request, which is the property a digest compared across nodes needs and the
//! one a stray `now_ms()` would silently break.
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

pub use detect::{
    AttributionBlock, DeclaredClient, Detection, attribution_block, detect_client,
    without_attribution_block,
};
pub use keyed::{
    SequenceDigester, SequenceKey, TipKeyer, ToolsDigest, new_anchor, prefix_fingerprint,
    tools_digest,
};
pub use label::{CodexHeaders, LabelError, claude_session, codex_conversation, messages_label};
pub use signals::{
    ClientSignals, CodexWindow, ContentMarker, RequestPurpose, claude_signals, codex_signals,
    content_marker,
};

/// The header Claude Code names its session with.
///
/// Confirmed live on every inference request at 2.1.247: a fresh UUID per
/// invocation unless `CLAUDE_CODE_SESSION_ID` is set, stable across
/// `--continue`. It is read first because it is the clean seam — no body
/// parsing, and the value is the session id rather than something the session
/// id has to be dug out of.
pub(crate) const CLAUDE_SESSION_HEADER: &str = "x-claude-code-session-id";

/// The header a Task-tool subagent identifies itself with.
///
/// A subagent runs inside the parent's process and inherits the parent's
/// session id, so without this the two interleave their turns on one log — and
/// because neither one's resent history contains the other's items, every
/// alternating turn diverges and forks. Treated as *part of the name* rather
/// than as a reason to open an anonymous session: a subagent is a conversation
/// of its own that a later turn of the same subagent should continue.
pub(crate) const CLAUDE_AGENT_HEADER: &str = "x-claude-code-agent-id";

/// Which wire a request arrived on — an input to the sequence digest, and
/// nothing else.
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
    /// `"anthropic_messages"` is deliberately independent of
    /// `MESSAGES_DIALECT_NAMESPACE`, though they read the same: this one is a
    /// digest input, and tying it to a label namespace would re-key every
    /// sequence the day that namespace is renamed.
    pub fn wire_name(self) -> &'static str {
        match self {
            Surface::AnthropicMessages => "anthropic_messages",
            Surface::OpenAiResponses => "openai_responses",
        }
    }
}

/// A header as visible ASCII, or `None` — the lenient read every observation
/// in this crate shares. The strict, refusing read is Codex's alone
/// ([`CodexHeaders::read`]).
pub(crate) fn ascii_header<'a>(headers: &'a http::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}
