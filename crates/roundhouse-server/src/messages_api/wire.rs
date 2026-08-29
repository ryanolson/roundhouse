// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Anthropic Messages wire vocabulary, serve side.
//!
//! Pure translation, no state, mirroring
//! [`responses_api::wire`](crate::responses_api): what a request's JSON means
//! as canonical [`Item`]s, and which session it names. The endpoint and its
//! follower live in the parent module; the frames going the other way live in
//! [`emit`](super::emit).
//!
//! **The request type is open where the client's is closed, and that asymmetry
//! is deliberate.** `CreateMessageParams` carries `additionalProperties: false`
//! upstream, which is why the *dispatch* client whitelists every field it
//! sends. A server reading the same shape must do the opposite: Claude Code
//! 2.1.247 posts the *beta* property set — `context_management`, a `thinking`
//! object with a `display` field, `output_config` — and the beta schema grows
//! faster than any pin (evidence doc §5.5 ¶4). Refusing an unknown property
//! would make every roundhouse release a race against the client's next one.
//! What is refused instead is a *content shape* that cannot be represented at
//! all, because that is the failure a client can act on and the one that would
//! otherwise corrupt a session's prefix.
//!
//! **Canonicalization is the prefix contract.** The result is compared against
//! the session's stored items, so it must be a function of the request alone:
//! the same conversation must canonicalize the same way on every turn, on every
//! node, or the check fails and the session forks to a cold generation —
//! silently, because every turn would still answer. That is why nothing here
//! reads a header, a clock, or a config, and why the attribution pseudo-header
//! block Claude Code prepends to `system` is stored as ordinary prefix with no
//! special case: the block is stable per conversation (§4.4, confirmed at
//! 2.1.247), so its stability is the client's to keep, and a server that
//! stripped it would be guessing at which parts of a system prompt matter.

use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::Value;

use roundhouse_core::ids::TurnId;
use roundhouse_core::item::{Item, ItemContent, Role};

use roundhouse_fleet::anthropic_messages::wire::ContentBlock;

use crate::http::ApiError;

/// The header Claude Code names its session with.
///
/// Confirmed live on every inference request at 2.1.247 (§5.5 ¶2): a fresh UUID
/// per invocation unless `CLAUDE_CODE_SESSION_ID` is set, stable across
/// `--continue`. It is read first because it is the clean seam — no body
/// parsing, and the value is the session id rather than something the session
/// id has to be dug out of.
pub const SESSION_HEADER: &str = "x-claude-code-session-id";

/// The header a Task-tool subagent identifies itself with.
///
/// A subagent runs inside the parent's process and inherits the parent's
/// session id, so without this the two interleave their turns on one log — and
/// because neither one's resent history contains the other's items, every
/// alternating turn diverges and forks. Read here rather than guessed at from
/// the body, and treated as *part of the name* rather than as a reason to open
/// an anonymous session: a subagent is a conversation of its own that a later
/// turn of the same subagent should continue.
pub const AGENT_HEADER: &str = "x-claude-code-agent-id";

/// The namespace every session name this surface derives lives in.
///
/// **Cross-dialect continuation is not a feature** (M11.1 review, F6). A
/// Messages client names its conversation with a header or a `metadata.user_id`
/// and a Responses client names its own with `prompt_cache_key`; both are
/// arbitrary client-chosen strings, and
/// [`ControlPlane::qualify`](crate::control_config::ControlPlane) puts them in
/// one namespace per principal. Two clients of one principal that happen to
/// choose the same string are then not two conversations but one contested one
/// — and since their histories were never going to agree, *every* alternating
/// turn looks like an edited resend and forks, dropping the control store's
/// overlay, intent, steer and binding records for the generation it leaves
/// behind each time.
///
/// A prefix rather than a second namespace argument on `qualify`, because this
/// is a fact about *this dialect's* names and not about the principal: the
/// Responses surface's keys are unchanged, so no session minted before this
/// existed moves. The shared `turn_id_for` deliberately stays shared — a turn
/// id is a content hash and two dialects hashing one conversation differently
/// would each be idempotent alone and neither across a chained deployment that
/// serves one and dispatches the other.
const DIALECT_NAMESPACE: &str = "anthropic_messages";

/// The separator in the *older* `metadata.user_id` shape.
///
/// `user_<hex>_account_<uuid>_session_<uuid>`. Neither hex nor a UUID contains
/// an underscore, so the marker occurs at most once and taking the first split
/// is unambiguous.
const USER_ID_SESSION_MARKER: &str = "_session_";

// ---------------------------------------------------------------------------
// The request
// ---------------------------------------------------------------------------

/// The part of a Messages request this surface reads.
///
/// Everything else — `tools`, `tool_choice`, `thinking`, `temperature`,
/// `context_management`, `output_config`, and whatever the next beta adds — is
/// accepted and ignored, for the reason the module doc gives.
#[derive(Debug, Default, Deserialize)]
pub struct CreateMessageParams {
    /// What the client believes it is talking to.
    ///
    /// Recorded as the turn's declared baseline and echoed in the response's
    /// `model`, never routed on — the same treatment `ResponsesRequest::model`
    /// gets, and for the same reason: it is the counterfactual the savings
    /// figure is priced against, which only the client can name.
    #[serde(default)]
    pub model: Option<String>,
    /// A string or a list of blocks, resolved at canonicalization.
    ///
    /// Held as raw JSON rather than as a typed either-or so a shape that is
    /// neither can be named in the refusal instead of surfacing as a serde
    /// "data did not match any variant" from somewhere inside the parse.
    #[serde(default)]
    pub system: Option<Value>,
    #[serde(default)]
    pub messages: Vec<InputMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub metadata: Option<Metadata>,
    /// The ceiling the client will let this answer grow to.
    ///
    /// Required by the upstream schema, and — since M11.1's F1 — actually
    /// honoured: `create_message` narrows it into
    /// [`TurnInput::output_token_cap`](crate::engine::TurnInput) and it reaches
    /// the provider as the dispatch's own `max_tokens`. It is emphatically not
    /// a routing input: v1 still chooses its target by policy, and this
    /// constrains what the chosen target may *produce*, nothing about which one
    /// is chosen.
    ///
    /// `u64` rather than `u32` because it is the client's number and parsing is
    /// not the place to refuse one — a value too large for the wire is narrowed
    /// where it is read, with the reasoning written there.
    #[serde(default)]
    pub max_tokens: Option<u64>,
}

/// One turn of the resent conversation.
///
/// Both fields are optional so that a missing one is a refusal naming *which*
/// field, rather than a parse failure naming the whole body. The Messages API
/// requires both; a client that omits one has made a mistake worth telling it
/// about precisely.
#[derive(Debug, Deserialize)]
pub struct InputMessage {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub user_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Session naming
// ---------------------------------------------------------------------------

/// Which session this request names, or `None` for an anonymous one.
///
/// Plan R5's order exactly: the header, then the session component of
/// `metadata.user_id` in both shapes it has shipped in, then the whole
/// `user_id`, then nothing.
///
/// **The fallbacks are not defensive padding; each answers a real client.** The
/// header is absent from v2.1.42 and present at 2.1.247, so a deployment whose
/// users have not updated is served by the second rung. `user_id` changed shape
/// between those versions — an underscore-delimited string became a JSON object
/// string (§5.5 ¶1) — and `claude-code-router`'s `_session_` split, which the
/// evidence cites, does not parse the newer one; reading both is what keeps one
/// client session on one roundhouse session across a client upgrade. The whole
/// string is kept when neither shape parses because a name we do not recognise
/// is still a name, and hashing it into an anonymous session would throw away a
/// warm prefix for no gain.
///
/// The last rung exists for a bare `curl`, not for Claude Code: every version
/// read sends `user_id` on every request, so the product path never reaches it.
/// It is `None` rather than a 4xx because a client with no session is asking for
/// one turn, and answering it costs nothing.
///
/// Every rung that *does* name something is scoped by
/// [`DIALECT_NAMESPACE`] and by the calling agent, so a name is only ever
/// shared with another turn of the same dialect and the same agent. See
/// [`scoped`].
pub fn session_key(headers: &HeaderMap, params: &CreateMessageParams) -> Option<String> {
    let agent = headers
        .get(AGENT_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(named) = headers
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(scoped(named, agent));
    }
    params
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.user_id.as_deref())
        .map(str::trim)
        .filter(|user_id| !user_id.is_empty())
        .map(|user_id| scoped(&session_component(user_id), agent))
}

/// A client-chosen name, in the namespace it is allowed to collide inside.
///
/// Two dimensions, and neither is decoration:
///
/// **The dialect**, for the reason [`DIALECT_NAMESPACE`] gives — a Messages
/// session id and a Responses `prompt_cache_key` that read the same are not the
/// same conversation.
///
/// **The agent**, because the Task tool runs a subagent inside the parent's own
/// process, and the client-surface evidence has it inheriting the parent's
/// session id. Two agents appending to one log interleave two conversations
/// neither of them can then resend, so each turn diverges from what the other
/// left and forks. Joining the agent id makes them siblings: the parent keeps
/// `…/{session}` and each subagent gets `…/{session}/agent/{id}`, which is one
/// conversation each and a name a later turn of the same subagent reaches
/// again.
///
/// The parent's own name is deliberately *not* re-spelled when the header is
/// absent, so a deployment whose clients never send it sees exactly the names
/// it saw before — and the subagent's name keeps the parent's session id as a
/// visible prefix, which is what makes the relationship readable in a store
/// listing rather than only in this function.
///
/// `#` is avoided on purpose: [`Conversations`](crate::conversations) spells a
/// fork generation `{key}#g{n}`, and a client-chosen name that could mint a
/// string of that shape would let one conversation address another's
/// generation.
fn scoped(session: &str, agent: Option<&str>) -> String {
    match agent {
        Some(agent) => format!("{DIALECT_NAMESPACE}/{session}/agent/{agent}"),
        None => format!("{DIALECT_NAMESPACE}/{session}"),
    }
}

/// The session component of a `metadata.user_id`, in either shipped shape.
///
/// Never `None`: an unrecognised shape yields the whole string, which is the
/// third rung of R5's order. Trimmed at each rung because a value that differs
/// only in whitespace between two turns would bind two sessions to one
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
    // when reached through [`session_key`], which trims before it calls. A
    // whitespace-only difference between two turns of one conversation would
    // otherwise bind them to two sessions and lose the warm prefix — the exact
    // failure the whole prefix-admission design exists to avoid, arriving
    // through the one rung nobody thinks about.
    user_id.trim().to_string()
}

// ---------------------------------------------------------------------------
// Canonicalizing a resent conversation
// ---------------------------------------------------------------------------

/// Convert `system` and `messages` into canonical items.
pub fn canonicalize(params: &CreateMessageParams) -> Result<Vec<Item>, ApiError> {
    let mut items = Vec::with_capacity(params.messages.len() + 1);
    if let Some(system) = &params.system {
        system_items(system, &mut items)?;
    }
    for message in &params.messages {
        message_items(message, &mut items)?;
    }
    mark_turn_configuration(&mut items);
    Ok(items)
}

/// The leading run of system items is *turn configuration*; every system item
/// after it is conversation history.
///
/// **This is where the M11.1 review's F7 ruling is pinned, and it is pinned by
/// position — once, here — rather than re-derived by every later reader.** The
/// leading run is what the client rebuilds from its own environment on each
/// invocation (the date, the cwd, the git branch, whichever betas are on
/// today), and admitting it strictly forks an ordinary `--continue` the first
/// time any of that moves. An *interior* `{"role":"system"}` message — the
/// `mid-conversation-system-2026-04-07` beta's, which the shipping client sends
/// on every request — is something that happened in the conversation at a
/// position both sides agree on, so it stays history and is admitted strictly
/// like any other item.
///
/// The distinction is carried as [`Role::Developer`] rather than as a side
/// table or an index, because a run of identically-shaped system items is not
/// splittable by anything downstream: the session fold, the prefix check and
/// the judge's brief would each have to guess where configuration ended, and
/// the first one that guessed differently would fork every session. Developer
/// is the role roundhouse already has for instructions-as-configuration (the
/// Responses surface maps a `developer` message to it), so this is a
/// vocabulary reuse and not an invention — and it means the Messages dialect
/// never produces a Developer item that is *not* configuration, which is what
/// makes [`is_turn_configuration`](roundhouse_core::session::is_turn_configuration)
/// total.
///
/// Both sources of a leading system item are covered on purpose: the top-level
/// `system` field, and — for a client that sends none — a `{"role":"system"}`
/// message sitting at position zero. "Leading" is about where the item is, not
/// about which field it arrived in.
fn mark_turn_configuration(items: &mut [Item]) {
    for item in items {
        if item.role != Role::System {
            break;
        }
        item.role = Role::Developer;
    }
}

/// This conversation's turn id.
///
/// Delegates to the Responses surface's function rather than restating FNV over
/// `Item::render` here. One answer to "what is this conversation's turn id" is
/// the whole point: the id is what makes a client's retry replay instead of
/// generating and billing a second answer, and two dialects that hashed
/// differently would each be idempotent alone and neither across a client that
/// switched. The function's natural home is `roundhouse-core` beside
/// `Item::render`, and moving it there is the right change the day a third
/// dialect wants it; a wrapper is what keeps that a one-line move instead of a
/// hunt for copies.
pub fn turn_id_for(items: &[Item]) -> TurnId {
    crate::responses_api::turn_id_for(items)
}

/// `system`, as one item per block.
///
/// A string becomes one item; a list becomes one item per block, in order. The
/// per-block split is what R5 means by "ordinary stored prefix": Claude Code
/// sends three blocks, of which block 0 is the attribution pseudo-header and
/// blocks 1-2 carry the cache breakpoints (§5.5 ¶5), and folding them into one
/// item would make the whole system prompt a single unit whose hash changes
/// whenever any part of it does. One item per block is also what lets the
/// prefix check say *how much* of the system prompt still agrees.
///
/// An empty string is skipped, exactly as `instructions` is on the Responses
/// surface: "no system prompt" and "an empty system prompt" are the same
/// request, and storing an empty item for one of them would put two clients
/// that meant the same thing on two sessions. An empty *block* is kept, because
/// a block is positional — the client counts on its index, and dropping it
/// would renumber everything after it.
fn system_items(system: &Value, items: &mut Vec<Item>) -> Result<(), ApiError> {
    match system {
        Value::String(text) => {
            if !text.is_empty() {
                items.push(Item::system_text(text.clone()));
            }
            Ok(())
        }
        Value::Array(blocks) => {
            for block in blocks {
                items.push(block_item(Role::System, block)?);
            }
            Ok(())
        }
        other => Err(ApiError::unprocessable(format!(
            "`system` must be a string or a list of content blocks, not {}",
            json_shape(other)
        ))),
    }
}

/// One message, as one item per block.
fn message_items(message: &InputMessage, items: &mut Vec<Item>) -> Result<(), ApiError> {
    // Three roles, and the third one is not in the base schema — it is in the
    // shipping client. `InputMessage.role` is `user | assistant` on the pinned
    // spec, and the first reading of this function refused everything else on
    // the reasoning that a system prompt travels in the top-level `system`
    // field. The 2.1.251 capture falsified that in the strongest possible way:
    // the client sends `anthropic-beta: mid-conversation-system-2026-04-07` and,
    // under it, a `{"role":"system"}` message on *every* request — so the strict
    // reading would have refused the entire current client line with a 422 and
    // no turn would ever have been served.
    //
    // Widened by name rather than by a catch-all mapping the unknown onto
    // something: a role we do not recognise stored as `User` is a prefix that
    // silently never matches again, whereas a refusal naming the role is a
    // sentence somebody can act on and a signal that the next beta moved.
    let role = match message.role.as_deref() {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        Some("system") => Role::System,
        Some(other) => {
            return Err(ApiError::unprocessable(format!(
                "message role `{other}` is not a role the Messages API has"
            )));
        }
        None => return Err(ApiError::unprocessable("a message needs a `role`")),
    };
    match message.content.as_ref() {
        Some(Value::String(text)) => {
            items.push(Item {
                role,
                content: ItemContent::Text { text: text.clone() },
                response_id: None,
            });
            Ok(())
        }
        Some(Value::Array(blocks)) => {
            for block in blocks {
                items.push(block_item(role, block)?);
            }
            Ok(())
        }
        Some(other) => Err(ApiError::unprocessable(format!(
            "a message needs `content` as a string or a list of content blocks, not {}",
            json_shape(other)
        ))),
        None => Err(ApiError::unprocessable("a message needs `content`")),
    }
}

/// One content block as a canonical item.
///
/// `role` is the message's own, and two block types override it. A `tool_use`
/// is an assistant item and a `tool_result` is a tool item *wherever they
/// appear*, because Anthropic's wire wraps tool results in a user message as a
/// transport convention rather than as a claim about who produced them —
/// unwrapping it here is what makes the log's role vocabulary the same one the
/// Responses surface writes, so the validate loop and the prompt renderer do
/// not have to know which dialect a session was opened on.
///
/// **Everything unrecognised becomes [`ItemContent::Opaque`] rather than a
/// refusal**, which is plan R5's opaque-first ruling. The alternative — refuse
/// what we do not model — was tried by `claudius` on the dispatch side and
/// rejects six of the spec's twelve response blocks today; on the serve side it
/// would be worse, because the blocks in question are images, documents and
/// server-tool results that a client resends on *every* turn. One refusal
/// remains, and it is the shape that genuinely cannot be stored: a block with
/// no string `type` is not a block the API can have sent, and giving it one
/// would be inventing the identity the opaque variant is keyed on.
///
/// Three fields are dropped and each loss is real. A `cache_control` breakpoint
/// is a caching directive rather than content, and roundhouse places its own
/// breakpoints from the segment boundaries it knows — keeping the client's
/// would let it name a prefix boundary in a prompt it does not assemble. A
/// `tool_result`'s `is_error` flag has no home on [`ItemContent::ToolResult`]
/// and adding one would change a shape every stored record is written in, so
/// the validate loop's failure detection reads the output text alone, as it
/// does for every other dialect. And a typed block's `extra` map — an unknown
/// field on an otherwise-known block — is not carried, which costs nothing for
/// prefix admission (canonicalization is deterministic either way) and would
/// cost fidelity only if this surface re-emitted a client's history, which it
/// does not.
fn block_item(role: Role, block: &Value) -> Result<Item, ApiError> {
    // Checked here rather than left to serde, so the refusal is this module's
    // sentence and not a message about internally tagged enums. A block is an
    // object with a `type` on every version of this API; anything else is a
    // client bug, and telling it which of the two rules it broke is the whole
    // value of refusing instead of storing something meaningless.
    if !block.is_object() {
        return Err(ApiError::unprocessable(format!(
            "a content block must be an object with a `type`, not {}",
            json_shape(block)
        )));
    }
    let parsed: ContentBlock = serde_json::from_value(block.clone()).map_err(|error| {
        ApiError::unprocessable(format!("a content block could not be read: {error}"))
    })?;
    let (role, content) = match parsed {
        ContentBlock::Text { text, .. } => (role, ItemContent::Text { text }),
        ContentBlock::ToolUse {
            id, name, input, ..
        } => (
            Role::Assistant,
            ItemContent::ToolCall {
                call_id: id,
                name,
                // The canonical item carries arguments as a string because the
                // Responses wire does; `Value::to_string` over a `BTreeMap`
                // gives one key order for any given object, so a body a chained
                // Relay alphabetized canonicalizes to the same string as the
                // one the client sent.
                arguments: input.to_string(),
            },
        ),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => (
            Role::Tool,
            ItemContent::ToolResult {
                call_id: tool_use_id,
                output: tool_result_output(&content),
            },
        ),
        ContentBlock::Thinking {
            thinking,
            signature,
            ..
        } => (
            role,
            ItemContent::Thinking {
                thinking,
                signature,
            },
        ),
        ContentBlock::RedactedThinking { data, .. } => {
            (role, ItemContent::RedactedThinking { data })
        }
        ContentBlock::Opaque(value) => {
            let block_type = value
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ApiError::unprocessable(
                        "a content block needs a string `type`; this one has none, so there \
                         is no identity to store it under",
                    )
                })?
                .to_string();
            (
                role,
                ItemContent::Opaque {
                    block_type,
                    block: value,
                },
            )
        }
    };
    Ok(Item {
        role,
        content,
        response_id: None,
    })
}

/// A tool result's content as one string.
///
/// The wire form is a string or a list of blocks. The structured form is kept
/// as its own JSON encoding rather than flattened to the text inside it — the
/// same choice `responses_api::wire::output_text` makes, and for the same
/// reason: flattening discards what a tool returned and makes two different
/// outputs canonicalize identically. A missing `content` becomes the empty
/// string rather than the literal `null`, which is what a tool that returned
/// nothing means and what a model reading the rendered prompt should see.
fn tool_result_output(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// What kind of JSON this is, for a refusal a client can act on.
///
/// The shape and not the value: a refusal quoting a whole system prompt back
/// would put the request body into a log line, and the client already knows
/// what it sent.
fn json_shape(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    /// What a *leading* system block canonicalizes to.
    ///
    /// Spelled out here rather than reached for as `Item::system_text`, because
    /// the difference is the F7 ruling: the leading run is turn configuration
    /// and carries [`Role::Developer`], while an interior system message stays
    /// a `System` item of the conversation. A test that could not tell the two
    /// apart would pass whichever way the boundary moved.
    fn developer_text(text: &str) -> Item {
        Item {
            role: Role::Developer,
            content: ItemContent::Text { text: text.into() },
            response_id: None,
        }
    }

    fn params(body: Value) -> CreateMessageParams {
        serde_json::from_value(body).expect("the fixture is a well-formed request")
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("a header name"),
                value.parse().expect("a header value"),
            );
        }
        headers
    }

    /// The 2.1.247 body, canonicalized.
    ///
    /// Modelled on the live capture (§5.5): three system blocks with the
    /// attribution pseudo-header first, a user turn, the assistant's reply
    /// replayed verbatim with its thinking block, and a second user turn. The
    /// attribution block is an ordinary `System` item like the other two — R5's
    /// ruling, asserted here so a future "helpfully" special-cased strip is a
    /// failing test rather than a session that forks on every client upgrade.
    #[test]
    fn the_live_client_body_canonicalizes_block_by_block() {
        let items = canonicalize(&params(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 32000,
            "system": [
                { "type": "text",
                  "text": "x-anthropic-billing-header: cc_version=2.1.247.3b2; cc_entrypoint=cli;" },
                { "type": "text", "text": "You are Claude Code.",
                  "cache_control": { "type": "ephemeral" } },
                { "type": "text", "text": "<system-reminder>…</system-reminder>",
                  "cache_control": { "type": "ephemeral", "ttl": "1h" } },
            ],
            "messages": [
                { "role": "user", "content": "list the crates" },
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "they want the workspace members",
                      "signature": "ErUBCkYIBRgCKk" },
                    { "type": "text", "text": "There are six." },
                ]},
                { "role": "user", "content": [{ "type": "text", "text": "and the tests?" }] },
            ],
        })))
        .expect("the shape the live client sends must canonicalize");

        assert_eq!(
            items,
            vec![
                developer_text(
                    "x-anthropic-billing-header: cc_version=2.1.247.3b2; cc_entrypoint=cli;"
                ),
                developer_text("You are Claude Code."),
                developer_text("<system-reminder>…</system-reminder>"),
                Item::user_text("list the crates"),
                Item {
                    role: Role::Assistant,
                    content: ItemContent::Thinking {
                        thinking: "they want the workspace members".into(),
                        signature: "ErUBCkYIBRgCKk".into(),
                    },
                    response_id: None,
                },
                Item {
                    role: Role::Assistant,
                    content: ItemContent::Text {
                        text: "There are six.".into()
                    },
                    response_id: None,
                },
                Item::user_text("and the tests?"),
            ],
            "the attribution block is ordinary prefix — turn configuration, per the \
             leading run (F7) — and cache_control leaves no trace"
        );
    }

    /// A `system` string and a `system` block list of the same text agree on
    /// the text but not on the item count.
    #[test]
    fn a_system_string_is_one_item_and_an_empty_one_is_none() {
        assert_eq!(
            canonicalize(&params(json!({ "system": "be brief", "messages": [] }))).unwrap(),
            vec![developer_text("be brief")]
        );
        assert_eq!(
            canonicalize(&params(json!({ "system": "", "messages": [] }))).unwrap(),
            Vec::<Item>::new(),
            "an empty system prompt and no system prompt are one request"
        );
        assert_eq!(
            canonicalize(&params(json!({ "messages": [] }))).unwrap(),
            Vec::<Item>::new()
        );
        // A block list keeps an empty block: its index is positional.
        assert_eq!(
            canonicalize(&params(json!({
                "system": [{ "type": "text", "text": "" }, { "type": "text", "text": "b" }],
                "messages": [],
            })))
            .unwrap(),
            vec![developer_text(""), developer_text("b")]
        );
    }

    /// The two tool shapes land on the roles the log uses everywhere else.
    ///
    /// Anthropic wraps a tool result in a *user* message. Storing it under
    /// `Role::User` would make the same exchange read differently depending on
    /// which dialect opened the session, and the validate loop reads both.
    #[test]
    fn tool_blocks_are_unwrapped_onto_the_logs_own_roles() {
        let items = canonicalize(&params(json!({
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "toolu_1", "name": "Grep",
                      "input": { "pattern": "fn main", "path": "src" } },
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1",
                      "content": "3 matches", "is_error": false },
                ]},
            ],
        })))
        .unwrap();

        assert_eq!(
            items,
            vec![
                Item {
                    role: Role::Assistant,
                    content: ItemContent::ToolCall {
                        call_id: "toolu_1".into(),
                        name: "Grep".into(),
                        // Key-sorted by `serde_json`'s `BTreeMap`, which is what
                        // makes a Relay-alphabetized resend canonicalize alike.
                        arguments: r#"{"path":"src","pattern":"fn main"}"#.into(),
                    },
                    response_id: None,
                },
                Item {
                    role: Role::Tool,
                    content: ItemContent::ToolResult {
                        call_id: "toolu_1".into(),
                        output: "3 matches".into(),
                    },
                    response_id: None,
                },
            ]
        );
    }

    /// A structured tool result keeps its JSON; an absent one is empty.
    #[test]
    fn a_structured_tool_result_keeps_its_encoding() {
        let items = canonicalize(&params(json!({
            "messages": [{ "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "t1",
                  "content": [{ "type": "text", "text": "hi" }] },
                { "type": "tool_result", "tool_use_id": "t2" },
            ]}],
        })))
        .unwrap();
        assert_eq!(
            items[0].content,
            ItemContent::ToolResult {
                call_id: "t1".into(),
                output: r#"[{"text":"hi","type":"text"}]"#.into(),
            }
        );
        assert_eq!(
            items[1].content,
            ItemContent::ToolResult {
                call_id: "t2".into(),
                output: String::new(),
            },
            "a tool that returned nothing must not render as the literal `null`"
        );
    }

    /// **Everything roundhouse does not model rides through verbatim.**
    ///
    /// The blocks named are the eight the pinned spec lists as response content
    /// beyond the four typed ones, plus the two request-side shapes a client
    /// resends constantly. Each must reach an `Opaque` item carrying its own
    /// type and its own JSON — refusing any of them would make a conversation
    /// containing an image unservable, and flattening one would fork the
    /// session the day the flattening changed.
    #[test]
    fn unmodelled_blocks_become_opaque_items_carrying_their_own_json() {
        for block_type in [
            "image",
            "document",
            "server_tool_use",
            "web_search_tool_result",
            "web_fetch_tool_result",
            "code_execution_tool_result",
            "bash_code_execution_tool_result",
            "text_editor_code_execution_tool_result",
            "tool_search_tool_result",
            "container_upload",
            "search_result",
        ] {
            let block = json!({ "type": block_type, "id": "srvtoolu_1", "payload": { "b": 1 } });
            let items = canonicalize(&params(json!({
                "messages": [{ "role": "assistant", "content": [block.clone()] }],
            })))
            .unwrap_or_else(|error| panic!("`{block_type}` must not be refused: {error:?}"));
            assert_eq!(
                items,
                vec![Item {
                    role: Role::Assistant,
                    content: ItemContent::Opaque {
                        block_type: block_type.to_string(),
                        block,
                    },
                    response_id: None,
                }]
            );
        }
    }

    /// The one content shape that cannot be stored is refused, by name.
    ///
    /// A block with no string `type` has no identity to key an opaque item on,
    /// and the refusal has to say so: this is the Responses surface's discipline
    /// — name the shape you would not take — applied to the one case
    /// opaque-first does not cover.
    #[test]
    fn a_block_with_no_type_is_refused_and_the_refusal_says_what_was_wrong() {
        for block in [
            json!({ "text": "no type here" }),
            json!("bare string"),
            json!(7),
        ] {
            let error = canonicalize(&params(json!({
                "messages": [{ "role": "user", "content": [block.clone()] }],
            })))
            .expect_err(&format!(
                "{block} has no block identity and must be refused"
            ));
            let rendered = format!("{error:?}");
            assert!(
                rendered.contains("`type`"),
                "the refusal must name what was missing: {rendered}"
            );
        }
    }

    /// The refusals for a malformed request name the field.
    #[test]
    fn malformed_requests_are_refused_by_field() {
        let cases = [
            (json!({ "system": 7, "messages": [] }), "system"),
            (json!({ "messages": [{ "content": "hi" }] }), "role"),
            (
                json!({ "messages": [{ "role": "narrator", "content": "hi" }] }),
                "narrator",
            ),
            (json!({ "messages": [{ "role": "user" }] }), "content"),
            (
                json!({ "messages": [{ "role": "user", "content": 7 }] }),
                "content",
            ),
        ];
        for (body, named) in cases {
            let error =
                canonicalize(&params(body.clone())).expect_err(&format!("{body} must be refused"));
            let rendered = format!("{error:?}");
            assert!(
                rendered.contains(named),
                "the refusal for {body} does not name `{named}`: {rendered}"
            );
        }
    }

    /// **A `system` message mid-conversation is the shipping client's shape.**
    ///
    /// M11.1's core stage ruled `role: "system"` a client that had confused two
    /// dialects, on the reading that a Messages request carries its system
    /// prompt in the top-level field. The fresh 2.1.251 capture falsifies that:
    /// the client sends `anthropic-beta: mid-conversation-system-2026-04-07`
    /// and, under it, a `{"role":"system"}` message between the first user turn
    /// and the assistant's reply — on *every* request, first turn included. The
    /// old refusal would have 422'd the entire current client line.
    ///
    /// The second half is the one that would have been missed by reading alone.
    /// The client sends that message's content as a **one-block list on the
    /// first turn and as a bare string on the resend** — verified byte-identical
    /// text across the two captured bodies — so a canonicalization whose result
    /// depended on the container would disagree with itself at item 1 of turn
    /// two, fork the session, and lose the warm prefix on every second turn
    /// forever, silently, while every turn still answered.
    #[test]
    fn a_mid_conversation_system_message_is_the_same_item_in_both_shapes() {
        let text = "Available agent types for the Agent tool:\n- claude: …";
        let as_blocks = canonicalize(&params(json!({
            "messages": [
                { "role": "user", "content": "say hi" },
                { "role": "system", "content": [
                    { "type": "text", "text": text, "cache_control": { "type": "ephemeral" } }
                ]},
            ],
        })))
        .expect("the shipping client's mid-conversation system message must be served");
        let as_string = canonicalize(&params(json!({
            "messages": [
                { "role": "user", "content": "say hi" },
                { "role": "system", "content": text },
            ],
        })))
        .expect("and so must the resend's flattened spelling of it");

        assert_eq!(
            as_blocks,
            vec![Item::user_text("say hi"), Item::system_text(text)],
            "a mid-conversation system message is a system item, not a refusal"
        );
        assert_eq!(
            as_blocks, as_string,
            "the block and string spellings of one message must be one prefix"
        );
        assert_eq!(
            turn_id_for(&as_blocks),
            turn_id_for(&as_string),
            "and therefore one turn id, or the resend is billed as a new turn"
        );
    }

    /// A role neither the API nor its betas have is still refused.
    ///
    /// The control on the arm above: widening for `system` must not have
    /// widened to anything a client cares to send, because an unrecognised role
    /// stored as some default is a prefix that will never match again.
    #[test]
    fn a_role_no_beta_has_added_is_still_refused_by_name() {
        let error = canonicalize(&params(json!({
            "messages": [{ "role": "developer", "content": "hi" }],
        })))
        .expect_err("`developer` is a Responses-dialect role");
        assert!(
            format!("{error:?}").contains("developer"),
            "the refusal must name the role it refused: {error:?}"
        );
    }

    /// Properties the beta schema adds are accepted and ignored.
    ///
    /// The exact property set the 2.1.247 capture carried (§5.5 ¶4). A closed
    /// request type would 4xx every turn of the shipping client, and the failure
    /// would arrive as "roundhouse is broken" rather than as "roundhouse is one
    /// beta behind".
    #[test]
    fn the_beta_property_surface_is_accepted_and_ignored() {
        let items = canonicalize(&params(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 32000,
            "messages": [{ "role": "user", "content": "hi" }],
            "context_management": { "edits": [{ "type": "clear_thinking_20251015", "keep": "all" }] },
            "thinking": { "type": "enabled", "budget_tokens": 10000, "display": "compact" },
            "output_config": { "format": "text" },
            "tools": [{ "name": "Grep", "input_schema": { "type": "object" } }],
            "tool_choice": { "type": "auto" },
            "temperature": 1,
            "service_tier": "standard",
            "mcp_servers": [],
            "an_unknown_field_from_the_next_beta": true,
        })))
        .expect("an unknown property must not refuse the turn");
        assert_eq!(items, vec![Item::user_text("hi")]);
    }

    /// **Where turn configuration ends is decided by position, once** (F7).
    ///
    /// The four shapes that matter, and each is a different way to get the
    /// boundary wrong:
    ///
    /// - `system` blocks followed by messages: the run is the blocks.
    /// - A `{"role":"system"}` message *after* a user message: history, because
    ///   it happened at a position both sides agree on. Folding it into the
    ///   configuration would take it out of what prefix admission compares and
    ///   silently shorten every later claim.
    /// - A `{"role":"system"}` message at position zero with no top-level
    ///   `system`: configuration, because "leading" is about where an item sits
    ///   and not about which field carried it.
    /// - `system` blocks *and* an interior system message: the run stops at the
    ///   first user message and does not resume.
    #[test]
    fn the_leading_system_run_is_configuration_and_an_interior_one_is_history() {
        let leading_only = canonicalize(&params(json!({
            "system": [{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }],
            "messages": [
                { "role": "user", "content": "hello" },
                { "role": "system", "content": "a reminder" },
            ],
        })))
        .unwrap();
        assert_eq!(
            leading_only
                .iter()
                .map(|item| item.role)
                .collect::<Vec<_>>(),
            vec![
                Role::Developer,
                Role::Developer,
                Role::User,
                // Interior: history, and admitted strictly like any other item.
                Role::System,
            ],
        );

        // No `system` field at all: the first message is still the leading run.
        let message_only = canonicalize(&params(json!({
            "messages": [
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": "hello" },
            ],
        })))
        .unwrap();
        assert_eq!(
            message_only
                .iter()
                .map(|item| item.role)
                .collect::<Vec<_>>(),
            vec![Role::Developer, Role::User],
        );

        // And a conversation that opens with a user message has no
        // configuration at all — not "the first system item it can find".
        let none = canonicalize(&params(json!({
            "messages": [
                { "role": "user", "content": "hello" },
                { "role": "system", "content": "a reminder" },
            ],
        })))
        .unwrap();
        assert_eq!(
            none.iter().map(|item| item.role).collect::<Vec<_>>(),
            vec![Role::User, Role::System],
        );
    }

    /// **The header wins, then either `user_id` shape, then the whole string.**
    ///
    /// One test for the whole order because the order *is* the ruling, and the
    /// interesting failures are precedence failures: a reader that prefers
    /// `user_id` binds a subagent's turns to its parent's session, and a reader
    /// that handles only one `user_id` shape re-keys every session the day a
    /// user upgrades their client.
    #[test]
    fn the_session_key_follows_r5s_order() {
        let live_user_id = r#"{"device_id":"a1b2","account_uuid":"","session_id":"11111111-2222-3333-4444-555555555555"}"#;
        let with_metadata =
            |user_id: &str| params(json!({ "messages": [], "metadata": { "user_id": user_id } }));

        // 1. The header, even when `user_id` names a different session.
        assert_eq!(
            session_key(
                &headers(&[(SESSION_HEADER, "header-session")]),
                &with_metadata(live_user_id)
            )
            .as_deref(),
            Some("anthropic_messages/header-session")
        );
        // 2a. The 2.1.247 JSON-object shape.
        assert_eq!(
            session_key(&HeaderMap::new(), &with_metadata(live_user_id)).as_deref(),
            Some("anthropic_messages/11111111-2222-3333-4444-555555555555")
        );
        // 2b. The older underscore shape, which the JSON parse does not reach.
        assert_eq!(
            session_key(
                &HeaderMap::new(),
                &with_metadata("user_9f3a_account_c0ffee_session_deadbeef")
            )
            .as_deref(),
            Some("anthropic_messages/deadbeef")
        );
        // 3. A shape neither rung recognises is still a name.
        assert_eq!(
            session_key(&HeaderMap::new(), &with_metadata("just-a-name")).as_deref(),
            Some("anthropic_messages/just-a-name")
        );
        // 4. Nothing at all.
        assert_eq!(
            session_key(&HeaderMap::new(), &params(json!({ "messages": [] }))),
            None
        );
    }

    /// The degenerate `user_id` shapes each fall through to the next rung.
    #[test]
    fn a_user_id_that_names_no_session_falls_through_to_itself() {
        // JSON, but not an object.
        assert_eq!(session_component("[1,2]"), "[1,2]");
        assert_eq!(session_component("42"), "42");
        // An object with no `session_id`, and one whose `session_id` is blank.
        let no_session = r#"{"device_id":"a1b2"}"#;
        assert_eq!(session_component(no_session), no_session);
        let blank = r#"{"session_id":"   "}"#;
        assert_eq!(session_component(blank), blank);
        // The marker with nothing after it.
        assert_eq!(
            session_component("user_9f3a_session_"),
            "user_9f3a_session_"
        );
        // Whitespace never distinguishes two turns of one conversation.
        assert_eq!(session_component("  padded  "), "padded");
        assert_eq!(session_component(r#"{"session_id":"  abc  "}"#), "abc");
    }

    /// An empty header value is absent, not a session named "".
    #[test]
    fn a_blank_session_header_falls_through_to_the_body() {
        assert_eq!(
            session_key(
                &headers(&[(SESSION_HEADER, "   ")]),
                &params(json!({ "messages": [], "metadata": { "user_id": "from-body" } }))
            )
            .as_deref(),
            Some("anthropic_messages/from-body")
        );
        assert_eq!(
            session_key(
                &headers(&[(SESSION_HEADER, "")]),
                &params(json!({ "messages": [] }))
            ),
            None
        );
    }

    /// **Every derived name carries its dialect, and its agent when there is
    /// one** (M11.1 review, F6).
    ///
    /// Two collisions this closes, both of which forked a session on *every*
    /// alternating turn rather than once: a Responses `prompt_cache_key` that
    /// reads the same as a Messages session id under one principal, and a
    /// Task-tool subagent that inherits its parent's session id. Neither is an
    /// edited conversation; both were two conversations sharing a log, which is
    /// the one thing prefix admission can never reconcile.
    ///
    /// The last assertion is the one that keeps this cheap: with no agent
    /// header the parent's name gains nothing beyond the dialect, so a
    /// deployment whose clients never send it is unaffected.
    #[test]
    fn a_derived_name_carries_its_dialect_and_its_agent() {
        let body = params(json!({ "messages": [] }));

        assert_eq!(
            session_key(&headers(&[(SESSION_HEADER, "s1")]), &body).as_deref(),
            Some("anthropic_messages/s1"),
        );
        assert_eq!(
            session_key(
                &headers(&[(SESSION_HEADER, "s1"), (AGENT_HEADER, "agent-7")]),
                &body
            )
            .as_deref(),
            Some("anthropic_messages/s1/agent/agent-7"),
        );
        // The agent joins a `user_id`-derived name too: the rung a name came
        // from is not a reason to scope it differently.
        assert_eq!(
            session_key(
                &headers(&[(AGENT_HEADER, "agent-7")]),
                &params(json!({ "messages": [], "metadata": { "user_id": "from-body" } }))
            )
            .as_deref(),
            Some("anthropic_messages/from-body/agent/agent-7"),
        );
        // A blank agent header is absent, not an agent named "": otherwise a
        // client that sent the header empty would get a session of its own that
        // no later turn could name again.
        assert_eq!(
            session_key(
                &headers(&[(SESSION_HEADER, "s1"), (AGENT_HEADER, "  ")]),
                &body
            )
            .as_deref(),
            Some("anthropic_messages/s1"),
        );
        // A name a Responses client could choose can no longer reach a Messages
        // session, whatever it spells.
        assert_ne!(
            session_key(&headers(&[(SESSION_HEADER, "shared")]), &body).as_deref(),
            Some("shared"),
        );
    }

    /// **A conversation's turn id is the same one the Responses surface mints.**
    ///
    /// Not a coincidence to be re-derived: the id is what deduplicates a
    /// client's retry onto the answer it already paid for, and two dialects
    /// hashing differently would each be idempotent alone and neither across a
    /// client that switched — or across a chained roundhouse, which is exactly
    /// the supported topology.
    #[test]
    fn the_turn_id_is_the_conversation_and_agrees_with_the_other_dialect() {
        let items = canonicalize(&params(json!({
            "system": "be brief",
            "messages": [{ "role": "user", "content": "hello" }],
        })))
        .unwrap();
        assert_eq!(turn_id_for(&items), turn_id_for(&items));
        // One hash function over one canonical vocabulary, reached from the
        // other module. What is *not* claimed is that the two dialects
        // canonicalize a request the same way — this one maps a leading system
        // run to `Developer` (F7) and the other maps `instructions` to
        // `System`, and since F6 puts their sessions in different namespaces a
        // turn id was never going to deduplicate across them anyway. What must
        // stay single is the function: two spellings of FNV over `Item::render`
        // would each be idempotent alone and neither across a chained
        // roundhouse, which serves one surface and dispatches the other.
        assert_eq!(
            turn_id_for(&items),
            crate::responses_api::turn_id_for(&[
                developer_text("be brief"),
                Item::user_text("hello"),
            ])
        );
        assert_ne!(
            turn_id_for(&items),
            turn_id_for(&canonicalize(&params(json!({ "messages": [] }))).unwrap())
        );
    }

    /// **A Relay-alphabetized resend canonicalizes to the very same items.**
    ///
    /// Synergy ruling S3's first chain hazard, instantiated for this surface: a
    /// chained NeMo Relay re-serializes every intercepted body through an
    /// alphabetizing `serde_json::Map`, so turn two of a chained conversation
    /// arrives with its object keys reordered. If that changed a single
    /// canonical item, prefix admission would fork the session — cold prefix,
    /// full price, and every turn still answering.
    ///
    /// The fixtures are *bytes*, not values, because that is the only place the
    /// difference exists: `serde_json`'s default map is a `BTreeMap`, so two
    /// documents differing only in key order parse to one value and serialize
    /// alike. That is the property being asserted, and it is a property of a
    /// design decision rather than a tautology — `ItemContent::Opaque` holds a
    /// parsed value precisely so this holds, and the day someone stores a
    /// block's raw bytes instead (for byte-exact re-emission, which is a real
    /// thing to want) this test is what says what it costs.
    #[test]
    fn a_re_encoded_body_canonicalizes_identically() {
        let sent = r#"{
            "model": "claude-sonnet-4-5",
            "messages": [{ "role": "assistant", "content": [
                { "type": "tool_use", "id": "toolu_1", "name": "Grep",
                  "input": { "pattern": "fn main", "path": "src", "case": false } },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png",
                                               "data": "AAAA" } }
            ]}]
        }"#;
        // What a chained Relay hands on: the same document, every object's keys
        // alphabetized and the whitespace gone.
        let relayed = r#"{"messages":[{"content":[{"id":"toolu_1","input":{"case":false,"path":"src","pattern":"fn main"},"name":"Grep","type":"tool_use"},{"source":{"data":"AAAA","media_type":"image/png","type":"base64"},"type":"image"}],"role":"assistant"}],"model":"claude-sonnet-4-5"}"#;

        assert_ne!(sent, relayed, "the fixtures must differ as bytes");
        let of = |body: &str| {
            canonicalize(&params(
                serde_json::from_str(body).expect("the fixture is JSON"),
            ))
            .expect("both bodies canonicalize")
        };
        assert_eq!(of(sent), of(relayed));
        assert_eq!(
            turn_id_for(&of(sent)),
            turn_id_for(&of(relayed)),
            "and the turn id must agree, or a chained retry is billed twice"
        );
    }
}
