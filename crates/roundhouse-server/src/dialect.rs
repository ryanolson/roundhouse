// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! How this deployment's clients spell a tool call.
//!
//! A deployment stores one canonical conversation and serves it to whatever
//! agent is in front of it. Most of that conversation is dialect-free — a
//! question is a question — but a *tool call* is not: the same call is spelled
//! one way by an agent that names MCP tools with a separate `namespace` field
//! and another way by an agent that flattens the namespace into the tool's own
//! name. Each surface stores what its own client spells — see the two
//! addenda below, which replace the paragraph that used to stand here claiming
//! one neutral stored form rendered outward.
//!
//! **Why that direction and not the other.** Storing the namespace would make
//! two spellings of one call two different stored items, and there is no seam
//! that could ever reconcile them: the prefix check would disagree with itself
//! the first time a client changed dialect, and every steered session would
//! silently fork onto a cold generation. A neutral stored name is the one form
//! both spellings can be mapped onto, which is what buys a second agent surface
//! later without forking the sessions the first one wrote.
//!
//! **What the wire layer does and does not already do for that** (F10, review;
//! pinned by `responses_api::wire`'s
//! `a_flat_spelling_is_a_different_canonical_call_until_the_wire_learns_to_split_it`).
//! `canonical_item` ignores a *separate* `namespace` field and the item `id` on
//! the way in, which is exactly what makes [`ClientDialect::CodexResponses`]'s
//! own resend round-trip to the bare stored name — the property the whole
//! steering choreography rests on. It does **not** split a namespace folded
//! into `name` itself, and nothing here should be read as claiming it does: a
//! flat `mcp__roundhouse__fetch_steer` with no `namespace` key canonicalizes to
//! that whole string and would fork the session on the next turn. So the day a
//! flat variant lands, its arm owes canonicalization the *reverse* mapping —
//! splitting the flat name back apart on the way in — as well as the rendering
//! on the way out. Keeping the namespace out of the log makes that
//! reconciliation possible; it does not make it already true, and an earlier
//! draft of this paragraph claimed it did.
//!
//! *(Superseded in part by the R-M1 addendum below: the flat variant landed and
//! owes canonicalization no reverse mapping. Splitting would move the `turn_id`
//! of every stored tool-using session, and the two spellings never have to be
//! reconciled because a session is written by one client.)*
//!
//! **A replay renders today's namespace.** The dialect is read when a frame is
//! built, not when the item was committed, so replaying an old response after
//! an operator renamed the namespace renders the *new* name. That is the same
//! class of edge as a rate-card change re-pricing a snapshot, and it is
//! deliberately not solved by stamping the namespace into the item — see the
//! paragraph above for what that would cost. It is harmless in the direction
//! that matters: a client that dispatches the renamed call reaches the same
//! server, and a client holding the old name is holding a call it already ran.
//!
//! # Addendum (2026-09-02): R-M0 — the first paragraph is half right
//!
//! Everything above was written before a wire could be read for it. M12's R-M0
//! read one, from codex's own types at the Cargo pin `6344a65` and from the M9
//! suite against a real binary, and the answer splits three ways.
//!
//! **Confirmed: the log stores a bare name, and the namespace is a separate
//! wire field.** Codex advertises an MCP server to the model as a single
//! `namespace` object and lists each tool inside it under its *bare* name
//! (`core/src/tools/handlers/mcp.rs:388-393` building
//! `ToolSpec::Namespace { name: callable_namespace, .. }`;
//! `tools/src/responses_api.rs:117-123` renaming each tool to
//! `tool_name.name`), where `callable_namespace` is `mcp__<server>`
//! (`codex-mcp/src/tools.rs:139-146`, `:228-234`) — this constant. So the call
//! comes back as `{"name":"status","namespace":"mcp__roundhouse"}`
//! (`protocol/src/models.rs:910-928`), dispatch is an exact
//! `ToolName { name, namespace }` lookup (`core/src/tools/router.rs:154-170`),
//! and a flat spelling would resolve against nothing
//! (`core/src/tools/registry.rs:828`) — which is what
//! [`ClientDialect::CodexResponses`]'s own doc already says, now with the line
//! numbers behind it. `codex_e2e.rs`'s
//! `the_delimiter_a_skill_spells_is_the_one_the_real_binary_namespaces_with`
//! read the same shape off a live binary, and
//! `responses_api::wire`'s `r_m0_a_codex_mcp_call_arrives_bare_with_a_separate_namespace`
//! now pins it end to end, built from `codex_protocol`'s own `ResponseItem`
//! rather than from a fixture this repo typed.
//!
//! **Wrong: "the spelling is applied on the way out, from here."** Nothing
//! applies it. `ControlPlane::client_dialect` has no caller outside its own
//! unit tests, because M10.0 (T4) deleted the outbound `function_call`
//! projection along with the synthetic steer it existed for. This enum is a
//! *description* of what a client sends, not a renderer of what we send, and
//! the paragraph on replay below describes a rendering that does not happen —
//! it is about a hazard that would return the day something renders again, not
//! one that is live.
//!
//! **The cost of the confirmed half, which the first paragraph did not
//! reckon.** Storing the bare name is right for prefix admission and wrong for
//! everything that has to *recognise* one of our own calls: the log holds
//! `status`, and `roundhouse_core::validate::is_control_call` matches the flat
//! `mcp__roundhouse__` prefix, so it has never fired on this surface. Control
//! traffic an agent makes because our own generated skill told it to is folded
//! into `task_exchanges` as work on the task — the exact failure G04 was
//! written to close. `roundhouse-core`'s
//! `a_control_call_as_the_responses_wire_stores_it_is_recognised` is that
//! finding — it landed red and `#[ignore]`d, with its live control beside it,
//! and R-M1 below is where the ignore came off. Nothing in *this file* changed
//! behaviour.
//!
//! # Addendum (2026-09-02): R-M1 — one enum, two surfaces, and what it is for
//!
//! R-M0 left this type describing a rendering nothing performs. M12 gives it
//! the job it can honestly hold: **it says how the log of a session written by
//! that client spells one of roundhouse's own control tools**, which is the one
//! question anything downstream actually asks of a stored tool name.
//!
//! - [`ClientDialect::CodexResponses`] — the namespace travels in its own wire
//!   field, canonicalization drops it, and the log holds `status`.
//! - [`ClientDialect::ClaudeMessages`] — Claude Code flattens the registration
//!   into every tool name it declares and every `tool_use` it emits
//!   (`mcp__roundhouse__status`, captured at 2.1.257 in
//!   `tests/fixtures/claude-2.1.257-mcp-turn-2-toolresult.json`), so the log
//!   holds that whole string.
//!
//! [`ClientDialect::stored_call_name`] is that statement, and each surface's
//! wire module is pinned to it by a test rather than by a reader's eye.
//!
//! **The dialect is a property of the surface, not of the deployment.** The
//! Messages handler names [`ClientDialect::claude_messages`] as a constant of
//! itself, exactly as it names `WireProtocol::AnthropicMessages` for the
//! toolbox it forwards. It must never be read from the deployment-wide
//! `mcp_namespace` the control plane compiles: one deployment serves both
//! clients at once, and a single configured answer would be wrong for one of
//! them on every turn.
//!
//! **Why the Messages surface stores flat rather than splitting.** Nothing
//! renders a tool call outbound (R-M0), so the only consumer of a stored tool
//! name is `roundhouse_core::validate::is_control_call` — which wants the flat
//! prefix. Splitting on the way in would move the `turn_id` of every stored
//! tool-using session, and the cross-dialect resumption the original paragraph
//! guarded against is not a scenario this product has: a session is written by
//! one client from its first turn to its last.
//!
//! **What R-M1 could not do as ruled, recorded because the next reader will
//! ask.** The ruling asked for a Responses-side recognizer built from "a bare
//! name plus the deployment's configured namespace". There is no such
//! recognizer: the namespace is not in the stored record, so a configured
//! value has nothing to be compared against. The fix that landed matches a bare
//! name against `roundhouse_core::validate::CONTROL_TOOL_NAMES` instead, and
//! that function's doc states what the substitution costs.

/// The namespace an unconfigured deployment renders, and the name a client's
/// MCP registration is expected to use.
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
/// half-done. The core definition carries the note about what an operator
/// renaming [`ClientDialect::namespace`] gets instead.
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
    CodexResponses { namespace: String },
    /// Claude Code over the Anthropic Messages API: the namespace is folded
    /// into the tool's own name and there is no second field.
    ///
    /// Also not a preference, and read off the client rather than chosen: the
    /// registration names the server, and every tool the client then declares
    /// (`tools[].name`), calls (`tool_use.name`) and permits
    /// (`--allowedTools`) is `mcp__<server>__<tool>` — one string, everywhere.
    /// A `namespace` field on this wire would be a field the API has no place
    /// for.
    ClaudeMessages { namespace: String },
}

impl ClientDialect {
    /// What a client of the Anthropic Messages surface writes in.
    ///
    /// A constructor rather than a literal at the call site so the namespace
    /// has one origin: [`DEFAULT_MCP_NAMESPACE`], which is also what
    /// `topham`'s generated `--mcp-config` names the server and what the
    /// validate fold recognises. Three spellings of one name is how a
    /// registration stops matching the tool it registers.
    pub fn claude_messages() -> Self {
        Self::ClaudeMessages {
            namespace: DEFAULT_MCP_NAMESPACE.to_string(),
        }
    }

    /// The namespace this dialect's tools live under, however it spells them.
    pub fn namespace(&self) -> &str {
        match self {
            Self::CodexResponses { namespace } | Self::ClaudeMessages { namespace } => namespace,
        }
    }

    /// How `tool` appears in the log of a session written by this client.
    ///
    /// The one behaviour this type has, and the reason it is not documentation.
    /// `roundhouse_core::validate::is_control_call` has to recognise whatever
    /// comes back, and *both* of these are things it must say yes to — which is
    /// exactly what the per-arm tests below assert, rather than trusting two
    /// modules to keep agreeing by hand.
    pub fn stored_call_name(&self, tool: &str) -> String {
        match self {
            // The namespace rides its own wire field and canonicalization drops
            // it, so what reaches the log is the bare name and nothing else.
            Self::CodexResponses { .. } => tool.to_string(),
            Self::ClaudeMessages { namespace } => format!(
                "{namespace}{delimiter}{tool}",
                delimiter = roundhouse_core::validate::CONTROL_TOOL_DELIMITER
            ),
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
        Self::CodexResponses {
            namespace: DEFAULT_MCP_NAMESPACE.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use roundhouse_core::validate::{CONTROL_TOOL_NAMES, is_control_call};

    /// The whole contract this type carries: whatever a client's dialect leaves
    /// in the log, the fold that has to spot roundhouse's own traffic spots it.
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
                    is_control_call(&stored),
                    "`{stored}` is what {dialect:?} leaves in the log for `{tool}`"
                );
            }
        }
    }

    /// The two arms really are two spellings, not one with a different label.
    ///
    /// Worth asserting on its own: a `stored_call_name` that had quietly become
    /// the same string on both arms would pass the test above and would mean
    /// one of the two surfaces had stopped matching its own client.
    #[test]
    fn the_two_surfaces_spell_one_call_two_ways() {
        assert_eq!(
            ClientDialect::default().stored_call_name("status"),
            "status"
        );
        assert_eq!(
            ClientDialect::claude_messages().stored_call_name("status"),
            "mcp__roundhouse__status"
        );
        assert_eq!(
            ClientDialect::claude_messages().namespace(),
            ClientDialect::default().namespace(),
            "one deployment, one server name: the surfaces differ in how they \
             spell it, never in what it is"
        );
    }

    /// An operator's renamed namespace reaches the flat spelling, and reaches
    /// the bare one by definition — there is nothing to rename in `status`.
    ///
    /// The asymmetry is the point, and it is why `is_control_call`'s bare arm
    /// cannot consult a configured namespace: the record it reads never carried
    /// one.
    #[test]
    fn a_renamed_namespace_moves_the_flat_spelling_and_cannot_move_the_bare_one() {
        let renamed = ClientDialect::ClaudeMessages {
            namespace: "mcp__yard".to_string(),
        };
        assert_eq!(renamed.stored_call_name("status"), "mcp__yard__status");
        assert!(
            !is_control_call(&renamed.stored_call_name("status")),
            "a renamed deployment loses the flat exemption, exactly as \
             `CONTROL_TOOL_NAMESPACE` says it does"
        );

        let renamed_responses = ClientDialect::CodexResponses {
            namespace: "mcp__yard".to_string(),
        };
        assert_eq!(renamed_responses.stored_call_name("status"), "status");
        assert!(
            is_control_call(&renamed_responses.stored_call_name("status")),
            "the bare spelling never carried the namespace, so renaming it \
             costs the bare arm nothing"
        );
    }
}
