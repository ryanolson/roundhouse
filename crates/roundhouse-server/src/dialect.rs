// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! How this deployment's clients spell a tool call.
//!
//! Most of a conversation is dialect-free — a question is a question — but a
//! *tool call* is not: an agent that names MCP tools with a separate
//! `namespace` field and one that folds the namespace into the tool's own name
//! spell the same call two ways. The contract is one paragraph:
//!
//! - **Each wire module stores the name its own client sent, verbatim.**
//!   Neither reads this type at run time; [`ClientDialect::stored_call_name`]
//!   is the *statement* of what each one stores, and a test per surface asserts
//!   the canonical form equals it.
//! - **[`ClientDialect::CodexResponses`]** — the namespace travels in its own
//!   wire field, and the log holds it there: a bare `status` in the name, with
//!   the server beside it in the record's own `namespace` (M17, R-N6). This
//!   type answers what the *name* is, which is why its arm did not move.
//!   **[`ClientDialect::ClaudeMessages`]** — Claude Code flattens the
//!   registration into every tool name it declares, calls and permits, so the
//!   log holds `mcp__roundhouse__status`.
//! - **`roundhouse_core::validate::is_control_call_on` must accept each
//!   surface's spelling and only that one.** It is the sole consumer of a
//!   stored tool name, and it takes the surface as a parameter for the reason
//!   the two bullets above give: a bare name is ours on one wire and the
//!   client's own on the other.
//! - **This type is never rendered outbound; the stored *name* is** — on both
//!   surfaces. The sentence this replaces said "nothing renders a tool call
//!   outbound", which was true of [`ClientDialect`] (no projection reads it,
//!   and the steer is assistant text rather than a synthetic call) and false of
//!   the log: `responses_api::wire::function_call_item` and
//!   `messages_api::emit` both put a stored call back on the wire for the
//!   client to run. The distinction matters because it is what makes the
//!   carried namespace reachable at all — the projection re-emits the field the
//!   client sent, so an MCP call resolves against codex's exact
//!   `ToolName { name, namespace }` lookup instead of arriving bare (M17,
//!   R-N10). What still holds is the half the sentence was written for: no
//!   *name* is composed on the way out, so there is no replay hazard from a
//!   spelling applied at projection time.
//! - **The namespace is `mcp__roundhouse` by construction**, shared by both
//!   launchers' registrations, the signage and the fold. There is no
//!   per-deployment rename; the control plane refuses a config that asks for
//!   one.
//!
//! The history behind that — R-M0's reading of codex at pin `6344a65`, why the
//! original "neutral stored name rendered outward" paragraph was half right,
//! and what the bare-name recognizer costs — is in
//! `agent-docs/PLAN-anthropic-messages.md`, addendum "M12 — the MCP control
//! surface for Claude Code" and its "What the implementation settled" section.
//! It is recorded there rather than here because that is where dated addenda
//! belong; this module states the code as it is.

/// The namespace this deployment serves under, and the name a client's MCP
/// registration has to use.
///
/// `mcp__<server-name>` is Codex's own construction (`core/src/tools/…`
/// builds the namespace object from the server's configured name), so this
/// constant is the server name `roundhouse` under that rule rather than a
/// string this crate invented.
///
/// Re-exported from `roundhouse-core` rather than spelled here (G04): the
/// validate loop's signal fold has to *recognise* a call under this namespace
/// as roundhouse's own control traffic, it lives a crate below this one, and
/// two literals that must agree in two crates is a rename that goes silently
/// half-done.
pub const DEFAULT_MCP_NAMESPACE: &str = roundhouse_core::validate::CONTROL_TOOL_NAMESPACE;

/// What every client of this deployment puts in front of an MCP server's
/// configured name to build the namespace it dispatches a tool call under.
///
/// A fact about the *clients*, and about both of them: codex builds a
/// `mcp__{server}` namespace object and Claude Code builds a flat
/// `mcp__{server}__{tool}` name (`research/claude-code-client-surface.md`
/// §5.8). Here rather than in either launcher because the two generators would
/// otherwise each carry a copy, and the thing they derive from it — the server
/// name a registration must use — has to be one answer or the namespace stops
/// being one.
pub const MCP_NAMESPACE_PREFIX: &str = "mcp__";

/// The server name a client's MCP registration has to use for a call to come
/// back under [`DEFAULT_MCP_NAMESPACE`].
///
/// Derived rather than written, so renaming the namespace renames what both
/// launchers register in the same edit. The `expect` is unreachable for any
/// namespace the constant can hold and is a louder failure than generating a
/// registration whose every control call resolves against nothing.
pub fn mcp_server_name() -> &'static str {
    DEFAULT_MCP_NAMESPACE
        .strip_prefix(MCP_NAMESPACE_PREFIX)
        .expect("the MCP namespace is `mcp__` plus the server's registered name")
}

/// How a client of one of this deployment's surfaces spells a call to an MCP
/// tool — and therefore how that call is spelled in the log of a session that
/// client wrote.
///
/// An enum rather than a bare `String` because a second agent surface does not
/// add a *namespace* to one spelling, it adds a different spelling. Making that
/// a variant means the day a third arrives the compiler names every site that
/// has to decide, instead of a reader having to find an `if` nobody wrote.
///
/// With one exception the compiler cannot name, and it is the one F10 found:
/// neither wire module reads this type at run time — each canonicalizes what
/// its client sent, verbatim — so adding a variant here does not make an input
/// path stop compiling. What holds the two together is a test per surface
/// asserting the canonical form equals [`Self::stored_call_name`]; those are
/// what go red if an arm and its wire drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientDialect {
    /// Codex over the OpenAI Responses API: an MCP tool is named by a bare
    /// `name` plus a separate `namespace` field.
    ///
    /// The separate field is not a preference. Codex dispatches on an exact
    /// `HashMap` lookup of `ToolName { name, namespace }` and nothing in its
    /// tree splits a flat `mcp__server__tool` back apart, so a call whose
    /// namespace is folded into its name resolves against nothing and comes
    /// back to the model as `unsupported call: …`.
    CodexResponses,
    /// Claude Code over the Anthropic Messages API: the namespace is folded
    /// into the tool's own name and there is no second field.
    ///
    /// Also not a preference, and read off the client rather than chosen: the
    /// registration names the server, and every tool the client then declares
    /// (`tools[].name`), calls (`tool_use.name`) and permits
    /// (`--allowedTools`) is `mcp__<server>__<tool>` — one string, everywhere.
    /// A `namespace` field on this wire would be a field the API has no place
    /// for.
    ClaudeMessages,
}

impl ClientDialect {
    /// What a client of the Anthropic Messages surface writes in.
    ///
    /// A named constructor rather than the bare variant so the call sites read
    /// as "the surface this is", and so that the day a Messages arm needs
    /// carrying something the compiler names one place to add it.
    pub fn claude_messages() -> Self {
        Self::ClaudeMessages
    }

    /// How `tool` appears in the log of a session written by this client.
    ///
    /// The one behaviour this type has, and the reason it is not documentation.
    /// `roundhouse_core::validate::is_control_call_on` has to recognise
    /// whatever comes back *under this same surface* — which is exactly what
    /// the per-arm tests below assert, rather than trusting two modules to keep
    /// agreeing by hand.
    pub fn stored_call_name(&self, tool: &str) -> String {
        match self {
            // The namespace rides its own wire field, so the *name* that
            // reaches the log is the bare one. Since M17 the record also
            // carries the namespace beside it — which is a fact about the
            // record, not about the name, and is why this arm is unchanged: a
            // caller asking "what is this tool called in that log" still gets
            // `status`.
            Self::CodexResponses => tool.to_string(),
            // The one renderer, shared with `codex_launch::skills` and the
            // Claude signage (M12 review, F7): three `format!` calls composing
            // the same two constants could drift in silence, and the drift
            // shows up as a skill naming a tool the client cannot resolve.
            Self::ClaudeMessages => roundhouse_core::validate::flat_control_call_name(tool),
        }
    }
}

impl Default for ClientDialect {
    /// What a deployment that has named no namespace renders.
    ///
    /// A real value rather than an absence: every client of this surface is a
    /// Responses client today, so "unconfigured" has one honest answer, and an
    /// `Option` here would put a `None` case on the projection whose only
    /// possible behavior would be to emit a call that cannot resolve.
    fn default() -> Self {
        Self::CodexResponses
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use roundhouse_core::validate::{CONTROL_TOOL_NAMES, ControlCallDialect, is_control_call_on};

    /// The surface each arm describes, as the fold's own recognizer names it.
    ///
    /// One function rather than a `match` at each assertion, because the pairing
    /// *is* the contract: this type says what a surface stores and the fold's
    /// dialect says what that surface's spelling is, and the two enums are only
    /// useful while they mean the same thing.
    fn recognizer(dialect: &ClientDialect) -> ControlCallDialect {
        match dialect {
            ClientDialect::CodexResponses => ControlCallDialect::CodexResponses,
            ClientDialect::ClaudeMessages => ControlCallDialect::ClaudeMessages,
        }
    }

    /// The whole contract this type carries: whatever a client's dialect leaves
    /// in the log, the fold that has to spot roundhouse's own traffic spots it
    /// *on that surface*.
    ///
    /// Walked over every tool and both arms rather than sampled, because the
    /// two arms fail in opposite directions and each failure is silent. A flat
    /// name the classifier missed would count our own chatter as the agent's
    /// work (G04); a bare name it missed would do the same on the other
    /// surface, which is precisely the state R-M0 found and M12 closed.
    #[test]
    fn every_spelling_a_dialect_stores_is_recognised_as_our_own_control_traffic() {
        for dialect in [ClientDialect::default(), ClientDialect::claude_messages()] {
            for tool in CONTROL_TOOL_NAMES {
                let stored = dialect.stored_call_name(tool);
                assert!(
                    is_control_call_on(&stored, None, recognizer(&dialect)),
                    "`{stored}` is what {dialect:?} leaves in the log for `{tool}`"
                );
            }
        }
    }

    /// The two arms really are two spellings, and each is the *other* surface's
    /// blind spot (M12 review, F8).
    ///
    /// Worth asserting on its own: a `stored_call_name` that had quietly become
    /// the same string on both arms would pass the test above and would mean
    /// one of the two surfaces had stopped matching its own client. The
    /// negatives are the half F8 added — a recognizer that accepted both
    /// spellings everywhere passes the positives and still folds a Messages
    /// client's own `status` tool out of the task view.
    #[test]
    fn the_two_surfaces_spell_one_call_two_ways() {
        let bare = ClientDialect::default().stored_call_name("status");
        let flat = ClientDialect::claude_messages().stored_call_name("status");
        assert_eq!(bare, "status");
        assert_eq!(flat, "mcp__roundhouse__status");

        assert!(!is_control_call_on(
            &bare,
            None,
            ControlCallDialect::ClaudeMessages
        ));
        assert!(!is_control_call_on(
            &flat,
            None,
            ControlCallDialect::CodexResponses
        ));
    }

    /// F3 (M12 thermo-nuclear review): the module doc states the current
    /// contract, not the addenda that got it there.
    ///
    /// It landed red at 139 `//!` lines — an original paragraph marked half
    /// right, a replay paragraph kept while saying it described a rendering
    /// that does not happen, a "Superseded in part" aside, and two dated
    /// addenda — and came live when the history moved to
    /// `agent-docs/PLAN-anthropic-messages.md`, which is where CLAUDE.md
    /// assigns dated addenda and which already carried this same R-M0
    /// archaeology with the codex `6344a65` citations.
    ///
    /// A budget rather than an exact count, and set well above the contract's
    /// own length: the failure being guarded against is the doc growing back
    /// into a changelog, not a paragraph gaining a sentence.
    #[test]
    fn module_doc_states_the_current_contract_not_its_history() {
        let doc_lines = include_str!("dialect.rs")
            .lines()
            .filter(|line| line.starts_with("//!"))
            .count();
        assert!(
            doc_lines <= 60,
            "module doc is {doc_lines} `//!` lines; CLAUDE.md assigns dated \
             addenda to agent-docs and says module docs describe the code as \
             it is, not the addenda that got it there"
        );
    }
}
