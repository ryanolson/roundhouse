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
///
/// With one exception the compiler cannot name, and it is the one F10 found:
/// `responses_api::wire::canonical_item` reads no dialect at all — it takes
/// whatever is in `name` verbatim — so adding a variant here does *not* make
/// the input path stop compiling. The module doc above says what that arm owes
/// canonicalization; the wire test named there is what goes red if it forgets.
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
