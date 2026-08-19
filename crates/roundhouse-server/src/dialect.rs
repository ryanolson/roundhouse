// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! How this deployment's clients spell a tool call.
//!
//! A deployment stores one canonical conversation and serves it to whatever
//! agent is in front of it. Most of that conversation is dialect-free — a
//! question is a question — but a *tool call* is not: the same call is spelled
//! one way by an agent that names MCP tools with a separate `namespace` field
//! and another way by an agent that flattens the namespace into the tool's own
//! name. The log therefore stores the bare, neutral name
//! ([`Item::tool_call`](roundhouse_core::item::Item::tool_call)) and the
//! spelling is applied on the way out, from here.
//!
//! **Why that direction and not the other.** The wire layer's canonicalization
//! already ignores `namespace` and `id` on the way *in*
//! (`responses_api::wire::canonical_item`), so a namespaced resend and a flat
//! resend arrive as the same canonical item. Storing the namespace instead
//! would make those two spellings two different items: the prefix check would
//! disagree with itself the first time a client changed dialect, and every
//! steered session would silently fork onto a cold generation. Keeping the
//! namespace out of the log is what buys a second agent surface later without
//! forking the sessions the first one wrote.
//!
//! **A replay renders today's namespace.** The dialect is read when a frame is
//! built, not when the item was committed, so replaying an old response after
//! an operator renamed the namespace renders the *new* name. That is the same
//! class of edge as a rate-card change re-pricing a snapshot, and it is
//! deliberately not solved by stamping the namespace into the item — see the
//! paragraph above for what that would cost. It is harmless in the direction
//! that matters: a client that dispatches the renamed call reaches the same
//! server, and a client holding the old name is holding a call it already ran.

/// The namespace an unconfigured deployment renders, and the name a client's
/// MCP registration is expected to use.
///
/// `mcp__<server-name>` is Codex's own construction (`core/src/tools/…`
/// builds the namespace object from the server's configured name), so this
/// constant is the server name `roundhouse` under that rule rather than a
/// string this crate invented.
pub const DEFAULT_MCP_NAMESPACE: &str = "mcp__roundhouse";

/// The tool-call spelling this deployment emits.
///
/// One arm in v1, and it is an enum rather than a bare `String` on purpose: a
/// second agent surface does not add a *namespace* to the same rendering, it
/// adds a different rendering (a flat `mcp__roundhouse__fetch_steer` with no
/// namespace field at all). Making that a variant means the day it lands the
/// compiler names every site that has to decide, instead of a reader having to
/// find an `if` that was never written.
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
