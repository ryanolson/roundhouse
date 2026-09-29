// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Labels: the lineage name a request claims, before qualification.
//!
//! **Moved, not rewritten.** Every value here is the one the server derived
//! before this crate existed — the Messages rungs from
//! `messages_api::wire::session_key`, `scoped` and `session_component`, the
//! Responses rungs from `RequestContext::from_request` and `conversation_key`.
//! A label is the key the store holds a conversation's log under, so a label
//! that changed spelling in the move would not fail anything: it would open a
//! cold session for every live conversation on the first request after the
//! deploy, and every turn would still answer. The server's golden capture
//! (`every_label_matches_the_golden_capture`) is what holds the move to that.

use http::HeaderMap;
use serde_json::Value;

use crate::{CLAUDE_AGENT_HEADER, CLAUDE_SESSION_HEADER, MESSAGES_DIALECT_NAMESPACE};
use crate::{RequestView, Surface};

/// Codex's root identity: every sub-agent of one root sends the same value.
const CODEX_SESSION_HEADER: &str = "session-id";

/// Codex's per-thread identity, and so its lineage name.
const CODEX_THREAD_HEADER: &str = "thread-id";

/// The separator in the *older* `metadata.user_id` shape.
///
/// `user_<hex>_account_<uuid>_session_<uuid>`. Neither hex nor a UUID contains
/// an underscore, so the marker occurs at most once and taking the first split
/// is unambiguous.
const USER_ID_SESSION_MARKER: &str = "_session_";

/// Which rung of which surface named the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelSource {
    CodexThread,
    CodexSession,
    PromptCacheKey,
    ClaudeSession,
    ClaudeUserId,
}

/// A named lineage, unqualified: the server prefixes the principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    pub source: LabelSource,
    /// Whether `x-claude-code-agent-id` scoped the name to one sub-agent.
    pub agent_scoped: bool,
}

/// A request's claim to a lineage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Labeled {
    Named(Label),
    /// A Messages request that names nothing. The server mints
    /// `anonymous_key`, because that reads the process id and the clock.
    Anonymous,
}

/// Why a Responses request names no lineage.
///
/// The texts are the server's existing 422 bodies, verbatim: the server maps
/// each variant to `ApiError::unprocessable(error.to_string())`, and a client
/// that parses the refusal must see what it saw before the move.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    #[error(
        "a `thread-id`, `session-id`, or `prompt_cache_key` is required to name the conversation"
    )]
    Unnamed,
    #[error("`{0}` must be a non-empty ASCII header")]
    InvalidHeader(&'static str),
}

/// The lineage a request claims.
///
/// **Messages** never fails: its rungs are [`messages_label`]'s, and a request
/// that names nothing is [`Labeled::Anonymous`] — asking for one turn, which
/// costs nothing to answer.
///
/// **Responses**: `thread-id`, else `session-id`, else a non-empty
/// `prompt_cache_key`, each taken as sent. A blank or non-ASCII `session-id`
/// is refused first and `thread-id` second, whatever else the request carries,
/// because that is the order the server checked them in and a client sending
/// two bad headers gets the same refusal it got before. A request that names
/// nothing at all is [`LabelError::Unnamed`]: the prefix fingerprint can be
/// shared by unrelated conversations, so it may stand in for a cache hint but
/// never for the identity of an append-only history.
///
/// **What the caller keeps.** The server's adapter still refuses a bad
/// `x-codex-window-id`, and it refused that *before* it refused an unnamed
/// request. So an adapter that wants the old precedence exactly checks the
/// window header when this returns [`LabelError::Unnamed`] (and when it
/// returns a label) — never before calling this, which would move it ahead of
/// the two header refusals here.
pub fn label(view: &RequestView<'_>) -> Result<Labeled, LabelError> {
    match view.surface {
        Surface::AnthropicMessages => {
            Ok(match messages_rung(view.headers, view.metadata_user_id) {
                Some(label) => Labeled::Named(label),
                None => Labeled::Anonymous,
            })
        }
        Surface::OpenAiResponses => responses_rung(view.headers, view.prompt_cache_key),
    }
}

/// The Messages rung of [`label`], byte for byte the server's old
/// `session_key` without its request type.
///
/// R5's order: the session header, else `metadata.user_id`, each trimmed and a
/// blank one treated as absent. The header is read first because it is the
/// clean seam, and `user_id` second because every version read sends it on
/// every request, in one of two shapes [`session_component`] knows. The last
/// rung — nothing — exists for a bare `curl`, not for Claude Code.
///
/// Every rung that does name something is scoped by the dialect and by the
/// calling agent, so a name is only ever shared with another turn of the same
/// dialect and the same agent. See [`scoped`].
pub fn messages_label(headers: &HeaderMap, metadata_user_id: Option<&str>) -> Option<String> {
    messages_rung(headers, metadata_user_id).map(|label| label.name)
}

/// The session component of a `metadata.user_id`, in either shipped shape.
///
/// Never empty-handed: an unrecognised shape yields the whole string, which is
/// the third rung of R5's order. Trimmed at each rung because a value that
/// differs only in whitespace between two turns would bind two sessions to one
/// conversation.
pub fn session_component(user_id: &str) -> String {
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

/// The client's root identity: Codex `session-id`, or Claude Code's session
/// header. Never a KV key — all sub-agents of one root share it — and never a
/// label on its own for Codex, whose threads are the lineages.
///
/// Lenient where [`label`] is strict: a malformed header reads as absent,
/// because this is an observation and the refusal belongs to `label`. Claude's
/// value is trimmed as its label rung trims it, Codex's is taken as sent as
/// its label rung takes it, so the session and the label agree on spelling.
/// `metadata.user_id` is not read: it is a label rung, and a session that
/// appeared only when the header was missing would be two answers to one
/// question depending on the client version.
pub fn client_session(view: &RequestView<'_>) -> Option<String> {
    match view.surface {
        Surface::AnthropicMessages => trimmed_header(view.headers, CLAUDE_SESSION_HEADER),
        Surface::OpenAiResponses => strict_header(view.headers, CODEX_SESSION_HEADER)
            .ok()
            .flatten(),
    }
}

fn messages_rung(headers: &HeaderMap, metadata_user_id: Option<&str>) -> Option<Label> {
    let agent = trimmed_header(headers, CLAUDE_AGENT_HEADER);
    let agent_scoped = agent.is_some();
    if let Some(named) = trimmed_header(headers, CLAUDE_SESSION_HEADER) {
        return Some(Label {
            name: scoped(&named, agent.as_deref()),
            source: LabelSource::ClaudeSession,
            agent_scoped,
        });
    }
    metadata_user_id
        .map(str::trim)
        .filter(|user_id| !user_id.is_empty())
        .map(|user_id| Label {
            name: scoped(&session_component(user_id), agent.as_deref()),
            source: LabelSource::ClaudeUserId,
            agent_scoped,
        })
}

fn responses_rung(headers: &HeaderMap, cache_key: Option<&str>) -> Result<Labeled, LabelError> {
    // Both read before either is used, so a bad `thread-id` is refused even
    // when `session-id` alone would have named the request — the order and
    // the strictness the server had.
    let session = strict_header(headers, CODEX_SESSION_HEADER)?;
    let thread = strict_header(headers, CODEX_THREAD_HEADER)?;
    let named = |name: String, source| {
        Ok(Labeled::Named(Label {
            name,
            source,
            agent_scoped: false,
        }))
    };
    if let Some(thread) = thread {
        return named(thread, LabelSource::CodexThread);
    }
    if let Some(session) = session {
        return named(session, LabelSource::CodexSession);
    }
    // Empty, not blank: a whitespace key was a name before the move and stays
    // one, because a body field is not a header and was never trimmed.
    match cache_key.filter(|key| !key.is_empty()) {
        Some(key) => named(key.to_owned(), LabelSource::PromptCacheKey),
        None => Err(LabelError::Unnamed),
    }
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
/// own process with the parent's session id. Joining the agent id makes them
/// siblings: the parent keeps `…/{session}` and each subagent gets
/// `…/{session}/agent/{id}`, one conversation each.
///
/// The parent's own name is deliberately *not* re-spelled when the header is
/// absent, so a deployment whose clients never send it sees exactly the names
/// it saw before. `#` is avoided on purpose: the server spells a fork
/// generation `{key}#g{n}`, and a client-chosen name that could mint that
/// shape would let one conversation address another's generation.
fn scoped(session: &str, agent: Option<&str>) -> String {
    match agent {
        Some(agent) => format!("{MESSAGES_DIALECT_NAMESPACE}/{session}/agent/{agent}"),
        None => format!("{MESSAGES_DIALECT_NAMESPACE}/{session}"),
    }
}

/// A header as the Messages rungs read it: visible ASCII, trimmed, and a blank
/// value the same as none.
fn trimmed_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// A header as the Responses rungs read it: absent is `None`, and present but
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
