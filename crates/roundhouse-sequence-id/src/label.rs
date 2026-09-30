// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Labels: the lineage name a request claims, before qualification.
//!
//! **Moved, not rewritten.** Every value here is the one the server derived
//! before this crate existed — the Messages rungs from the server's Messages
//! wire module (`session_key`, `scoped` and `session_component`), the
//! Responses rungs from `RequestContext::from_request` and `conversation_key`.
//! A label is the key the store holds a conversation's log under, so a label
//! that changed spelling in the move would not fail anything: it would open a
//! cold session for every live conversation on the first request after the
//! deploy, and every turn would still answer. The server's golden capture
//! (`every_label_matches_the_golden_capture`) is what holds the move to that.

use http::HeaderMap;
use roundhouse_core::ids::MESSAGES_DIALECT_NAMESPACE;
use serde_json::Value;

use crate::{CLAUDE_AGENT_HEADER, CLAUDE_SESSION_HEADER, ascii_header};

/// Codex's root identity: every sub-agent of one root sends the same value.
const CODEX_SESSION_HEADER: &str = "session-id";

/// Codex's per-thread identity, and so its lineage name.
const CODEX_THREAD_HEADER: &str = "thread-id";

/// Codex's context window, `{thread_id}:{window_number}`.
pub(crate) const CODEX_WINDOW_HEADER: &str = "x-codex-window-id";

/// The separator in the *older* `metadata.user_id` shape.
///
/// `user_<hex>_account_<uuid>_session_<uuid>`. Neither hex nor a UUID contains
/// an underscore, so the marker occurs at most once and taking the first split
/// is unambiguous.
const USER_ID_SESSION_MARKER: &str = "_session_";

/// Why a Responses request names no lineage.
///
/// The texts are the server's existing 422 bodies, verbatim: the server maps
/// each variant to a 422 carrying `error.to_string()`, and a client that
/// parses the refusal must see what it saw before the move.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    #[error(
        "a `thread-id`, `session-id`, or `prompt_cache_key` is required to name the conversation"
    )]
    Unnamed,
    #[error("`{0}` must be a non-empty ASCII header")]
    InvalidHeader(&'static str),
}

/// The Messages label, byte for byte the server's old `session_key` without
/// its request type — and the whole of this surface's label API.
///
/// R5's order: the session header, else `metadata.user_id`, each trimmed and a
/// blank one treated as absent. The header is read first because it is the
/// clean seam, and `user_id` second because every version read sends it on
/// every request, in one of two shapes `session_component` knows. The last
/// rung — nothing, `None` — exists for a bare `curl`, not for Claude Code: the
/// server mints `anonymous_key` for it, because that reads the process id and
/// the clock. It never fails, so it returns no `Result` for a caller to map.
///
/// **The fallbacks are not defensive padding; each answers a real client.** The
/// header is absent from v2.1.42 and present at 2.1.247, so a deployment whose
/// users have not updated is served by the second rung. `user_id` changed shape
/// between those versions — an underscore-delimited string became a JSON object
/// string (2.1.247 capture) — and `claude-code-router`'s `_session_` split
/// does not parse the newer one; reading both is what keeps one
/// client session on one roundhouse session across a client upgrade. The whole
/// string is kept as a name when neither shape parses because a name we do not
/// recognise is still a name, and hashing it into an anonymous session would
/// throw away a warm prefix for no gain.
///
/// The last rung is `None` rather than a 4xx because a client with no session
/// is asking for one turn, and answering it costs nothing; refusing it would
/// turn a bare `curl` into an error for no protection anyone needs.
///
/// Every rung that does name something is scoped by the dialect and by the
/// calling agent, so a name is only ever shared with another turn of the same
/// dialect and the same agent. See `scoped`.
pub fn messages_label(headers: &HeaderMap, metadata_user_id: Option<&str>) -> Option<String> {
    let agent = trimmed_header(headers, CLAUDE_AGENT_HEADER);
    let session = trimmed_header(headers, CLAUDE_SESSION_HEADER).or_else(|| {
        metadata_user_id
            .map(str::trim)
            .filter(|user_id| !user_id.is_empty())
            .map(session_component)
    })?;
    Some(scoped(&session, agent.as_deref()))
}

/// Claude Code's root identity: its session header, trimmed as its label rung
/// trims it, so the session and the label agree on spelling.
///
/// `metadata.user_id` is not read: it is a label rung, and a session that
/// appeared only when the header was missing would be two answers to one
/// question depending on the client version.
pub fn claude_session(headers: &HeaderMap) -> Option<String> {
    trimmed_header(headers, CLAUDE_SESSION_HEADER)
}

/// The session component of a `metadata.user_id`, in either shipped shape.
///
/// Never empty-handed: an unrecognised shape yields the whole string, which is
/// the third rung of R5's order. Trimmed at each rung because a value that
/// differs only in whitespace between two turns would bind two sessions to one
/// conversation.
pub(crate) fn session_component(user_id: &str) -> String {
    // The 2.1.247 shape: `user_id` is itself a JSON-encoded object.
    if let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(user_id)
        && let Some(session_id) = fields.get("session_id").and_then(Value::as_str)
        && !session_id.trim().is_empty()
    {
        return session_id.trim().to_string();
    }
    // The pre-2.1.247 shape.
    if let Some((_, session_id)) = user_id.split_once(USER_ID_SESSION_MARKER)
        && !session_id.trim().is_empty()
    {
        return session_id.trim().to_string();
    }
    // Trimmed here too, so this function is right on its own rather than only
    // when reached through `messages_label`, which trims before it calls. A
    // whitespace-only difference between two turns of one conversation would
    // otherwise bind them to two sessions and lose the warm prefix — the exact
    // failure the whole prefix-admission design exists to avoid, arriving
    // through the one rung nobody thinks about.
    user_id.trim().to_string()
}

/// The three identity headers a Codex request carries, each validated as it
/// is read.
///
/// **Reading is where Responses refuses**, and in the order the server always
/// refused in: a blank or non-ASCII `session-id`, then `thread-id`, then
/// `x-codex-window-id`, whatever else the request carries — a client that
/// sends two bad headers is told about the first one, and the move must not
/// change which. The window names nothing, but it is read here rather than by
/// the server so there is one strict header reader and one 422 text, not a
/// second copy of each that could drift. A value that passes is kept as sent,
/// untrimmed, because it always was and a trimmed name is another
/// conversation's key.
///
/// Holding what it validated is the point of the type: a caller cannot label
/// a request without having read its headers first, so an unnamed request is
/// refused after every bad header and never before one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexHeaders {
    session: Option<String>,
    thread: Option<String>,
    window: Option<String>,
}

impl CodexHeaders {
    pub fn read(headers: &HeaderMap) -> Result<Self, LabelError> {
        let session = strict_header(headers, CODEX_SESSION_HEADER)?;
        let thread = strict_header(headers, CODEX_THREAD_HEADER)?;
        let window = strict_header(headers, CODEX_WINDOW_HEADER)?;
        Ok(Self {
            session,
            thread,
            window,
        })
    }

    /// `session-id`: Codex's root identity. Never a KV key — all sub-agents of
    /// one root share it — and never the label while a thread is named.
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// `thread-id`: when present, the label.
    pub fn thread(&self) -> Option<&str> {
        self.thread.as_deref()
    }

    /// `x-codex-window-id`, as sent. Parsed, leniently, by [`codex_signals`](crate::codex_signals).
    pub fn window(&self) -> Option<&str> {
        self.window.as_deref()
    }

    /// The lineage this request names: `thread-id`, else `session-id`, else a
    /// non-empty `prompt_cache_key`, each taken as sent.
    ///
    /// A request that names nothing is [`LabelError::Unnamed`]: a content
    /// fingerprint can be shared by unrelated conversations, so it may stand
    /// in for a cache hint but never for the identity of an append-only
    /// history.
    pub fn label(&self, prompt_cache_key: Option<&str>) -> Result<String, LabelError> {
        codex_conversation(self.thread(), self.session(), prompt_cache_key)
            .map(str::to_owned)
            .ok_or(LabelError::Unnamed)
    }
}

/// Codex's lineage precedence, and the only statement of it: `thread`, else
/// `session`, else a non-empty `cache_key`, each returned as given.
///
/// Pure and unvalidated — [`CodexHeaders::read`] is where a header is refused
/// — so that a caller holding the three values as plain fields (the server's
/// `RequestContext`) can derive its key from those fields on every call
/// instead of storing a copy that could disagree with them, while still
/// using the one precedence [`CodexHeaders::label`] uses.
pub fn codex_conversation<'a>(
    thread: Option<&'a str>,
    session: Option<&'a str>,
    cache_key: Option<&'a str>,
) -> Option<&'a str> {
    // Empty, not blank: a whitespace key was a name before the move and stays
    // one, because a body field is not a header and was never trimmed.
    thread
        .or(session)
        .or(cache_key.filter(|key| !key.is_empty()))
}

/// A client-chosen name, in the namespace it is allowed to collide inside.
///
/// Two dimensions, and neither is decoration:
///
/// **The dialect**, because a Messages session id and a Responses
/// `prompt_cache_key` that read the same are not the same conversation, and
/// `ControlPlane::qualify` puts both in one namespace per principal — two
/// clients choosing one string would otherwise fork each other on every
/// alternating turn.
///
/// **The agent**, because the Task tool runs a subagent inside the parent's
/// own process with the parent's session id. Two agents appending to one log
/// interleave two conversations neither of them can then resend, so each turn
/// diverges from what the other left and forks. Joining the agent id makes them
/// siblings: the parent keeps `…/{session}` and each subagent gets
/// `…/{session}/agent/{id}`, one conversation each and a name a later turn of
/// the same subagent reaches again.
///
/// The parent's own name is deliberately *not* re-spelled when the header is
/// absent, so a deployment whose clients never send it sees exactly the names
/// it saw before — and the subagent's name keeps the parent's session id as a
/// visible prefix, which is what makes the relationship readable in a store
/// listing rather than only in this function. `#` is avoided on purpose: the
/// server spells a fork generation `{key}#g{n}`, and a client-chosen name that
/// could mint that shape would let one conversation address another's
/// generation.
fn scoped(session: &str, agent: Option<&str>) -> String {
    match agent {
        Some(agent) => format!("{MESSAGES_DIALECT_NAMESPACE}/{session}/agent/{agent}"),
        None => format!("{MESSAGES_DIALECT_NAMESPACE}/{session}"),
    }
}

/// A header as the Messages rungs read it: visible ASCII, trimmed, and a blank
/// value the same as none.
fn trimmed_header(headers: &HeaderMap, name: &str) -> Option<String> {
    ascii_header(headers, name)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// A header as Codex's reader takes it: absent is `None`, and present but
/// blank or not visible ASCII is a refusal naming the header. The value is
/// kept as sent, untrimmed, because it was before.
fn strict_header(headers: &HeaderMap, name: &'static str) -> Result<Option<String>, LabelError> {
    headers
        .get(name)
        .map(|value| {
            value
                .to_str()
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .ok_or(LabelError::InvalidHeader(name))
        })
        .transpose()
}
