// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The text that tells a Claude Code model roundhouse's control tools exist,
//! and under what circumstances to reach for one.
//!
//! [`super`] emits what makes a client *reach* roundhouse; this emits what makes
//! the tools on the other end get *used*. It is the Claude analogue of
//! [`codex_launch::skills`](crate::codex_launch::skills) (M12, R-M4), and the
//! two differ in exactly two ways, both forced by the client rather than
//! chosen.
//!
//! # Why one appended block and not a directory
//!
//! Three places text can land in a Claude Code session, and the other two are
//! refused:
//!
//! - **`--append-system-prompt`** — an appended system block, which the client
//!   sends as ordinary Developer configuration. Loosely admitted by this
//!   deployment's own prefix admission, so a client that changes it between
//!   turns does not fork the conversation. This is what [`signage`] renders and
//!   what `topham` passes.
//! - **`$CLAUDE_CONFIG_DIR/skills`** — the shape `codex_launch::skills` writes
//!   for the other client, and wrong here twice over: the listing arrives as an
//!   *interior* system message that this surface admits strictly, so editing
//!   the signage forks every live session onto a cold generation; and owning
//!   `CLAUDE_CONFIG_DIR` to write into it relocates the file a forwarded login
//!   lives in, which evicts the login a `ForwardedClaudeLogin` launch exists to
//!   forward.
//! - **`CLAUDE.md`** — lands in the first *user* message, inside the operator's
//!   own repository. A launcher writing into a repository it was merely started
//!   in is a launcher that edits the operator's project, and the file is
//!   committed by the next `git add -A`.
//!
//! Hence: no file. The whole of this module is one string, and that is the same
//! rule [`super`]'s doc opens with.
//!
//! # Why this says *when* and never *what*
//!
//! `codex_launch::skills` quotes each descriptor's own sentence, because a
//! skill file is read on its own and the model may never have seen the tool
//! list. Here the opposite holds: the client's `--mcp-config` registration
//! makes it call `tools/list` on every session, and every descriptor's full
//! description then rides in `tools[]` on *every* request
//! (`research/claude-code-client-surface.md` §5.8: the MCP schemas flow into
//! the Messages toolbox verbatim). Restating them here would put each
//! description in the same context window twice, on every turn, for the whole
//! fleet — and would create a second copy to keep in step with the first.
//!
//! So this text carries the one thing a descriptor cannot: the *occasion*. A
//! description says what `prefer` does; nothing in the tool list says that "use
//! the local models" is a request to call it.

use roundhouse_mcp::tools::descriptors;

use crate::dialect::ClientDialect;

/// One tool, and the circumstance a model should recognise it by.
///
/// The name is a `&'static str` resolved against [`descriptors`] before any
/// text is rendered — a literal that does not resolve panics rather than
/// producing signage, which is `codex_launch::skills`'s rule for the same
/// reason: a name the client cannot resolve fails as a model that quietly
/// calls nothing.
struct Sign {
    tool: &'static str,
    /// When to reach for it, written as the condition and never as a summary
    /// of what the tool does — see the module doc.
    when: &'static str,
}

/// All eight, in the order a session meets them rather than in the order
/// [`descriptors`] lists them.
///
/// **Eight and not the three `codex_launch::skills` ships.** That module picks
/// three because each is a separate *file* competing for a model's attention,
/// and the four loop tools would be files inviting calls out of the loop that
/// gives them meaning. This is one block, read whole or not at all, so the
/// question is not which tools deserve a file but which a model would otherwise
/// call at the wrong moment — and for the loop tools that is answered by naming
/// the moment, which costs one line each.
const SIGNS: &[Sign] = &[
    Sign {
        tool: "status",
        when: "the user asks what this session may be routed to right now, which models are \
               admissible, or what budget is left",
    },
    Sign {
        tool: "explain_last_route",
        when: "the user asks why the previous turn went to the model it went to",
    },
    Sign {
        tool: "prefer",
        when: "the user asks to keep this session on this deployment's own local models, or on \
               hosted ones, or to drop a preference they set earlier",
    },
    Sign {
        tool: "set_quality_floor",
        when: "the user says the answers are not good enough and asks for a stronger model for a \
               while, or names a minimum quality to stay above",
    },
    Sign {
        tool: "declare_intent",
        when: "you are starting a piece of work worth reviewing, so a later review can name a \
               divergence from your goal instead of guessing at one",
    },
    Sign {
        tool: "init_session",
        when: "you want an id for this conversation you can carry in the history you resend; call \
               it once, and keep the answer unsummarized",
    },
    Sign {
        tool: "fetch_steer",
        when: "roundhouse corrected you earlier in this conversation and you no longer have the \
               message it arrived in",
    },
    Sign {
        tool: "report_outcome",
        when: "you have acted on such a correction, or considered it and decided not to",
    },
];

/// What every reader has to know before any of the eight makes sense.
const PREAMBLE: &str = "This session runs through roundhouse, which chooses the model for every \
                        turn. Naming a model in a request does not select one -- the field is \
                        recorded and ignored. What you can change is the set roundhouse chooses \
                        *from*, by calling the tools below. Their arguments and their answers are \
                        described in the tool list you already have; what follows is when to \
                        reach for each.";

/// The two things that are true of every call, said once.
///
/// Both are answers a model would otherwise mistake for failures and retry:
/// `narrowed: true` is a final answer that no argument can improve, and an
/// omitted `conversation` is the *correct* call rather than a missing field.
const CLOSING: &str = "Two things hold for all of them. The writing tools can only narrow: an \
                       answer carrying `narrowed: true` means the request asked for more than \
                       this key is already allowed and nothing changed -- that is a final answer, \
                       not something a retry improves. And the optional `conversation` argument \
                       is one to omit: leave it out and the call is matched to the conversation \
                       it was made from.";

/// The appended system block, as `topham launch` passes it.
///
/// Takes nothing, for `codex_launch::skills`'s reason: signage names tools and
/// never an address. The MCP server is registered once, on the same argv, and a
/// URL spelled here as well would be a second place the deployment's address
/// lives — parting company from the first on the next redeploy, silently,
/// because a stale address inside a prompt fails as a model that cannot call
/// anything rather than as a configuration error.
pub fn signage() -> String {
    let dialect = ClientDialect::claude_messages();
    let mut out = String::from(PREAMBLE);
    out.push_str("\n\n");
    for sign in SIGNS {
        // Resolved, not merely spelled: `descriptor` panics on a name this
        // deployment does not serve, which is what stops a rename in
        // `roundhouse-mcp` from shipping a block that sends the model to a tool
        // the client will answer `no such tool` for.
        let name = dialect.stored_call_name(descriptor(sign.tool).name);
        out.push_str(&format!("- `{name}`: use it when {}.\n", sign.when));
    }
    out.push('\n');
    out.push_str(CLOSING);
    out.push('\n');
    out
}

/// The descriptor a sign names, or a panic naming what is missing.
///
/// A panic and not an `Option` for the reason `codex_launch::skills` gives: the
/// degraded output — a block that silently omits one tool — is still a valid
/// block, is still read on every turn, and costs the fleet context forever.
/// `every_sign_names_a_tool_this_deployment_serves` is what turns a rename into
/// a red test here rather than a panic on somebody's deployment.
fn descriptor(tool: &'static str) -> roundhouse_mcp::tools::ToolDescriptor {
    descriptors()
        .into_iter()
        .find(|candidate| candidate.name == tool)
        .unwrap_or_else(|| {
            panic!(
                "`{tool}` is not in roundhouse-mcp's tool list, so signage pointing at it would \
                 send the model to a name this deployment cannot resolve"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    use roundhouse_mcp::tools::TOOL_NAMES;

    use crate::dialect::DEFAULT_MCP_NAMESPACE;

    /// The flat prefix a name in this text has to wear.
    fn prefix() -> String {
        format!(
            "{DEFAULT_MCP_NAMESPACE}{}",
            roundhouse_core::validate::CONTROL_TOOL_DELIMITER
        )
    }

    /// Every name the text spells is a tool this deployment serves — scanned
    /// out of the rendered text rather than read off [`SIGNS`].
    ///
    /// The table is not what a model reads; the text is. A hand-written tool
    /// name that crept into [`PREAMBLE`] or a `when` clause would be invisible
    /// to a check that only walked the table, and it is exactly the kind of
    /// name that gets typed once and never resolved.
    #[test]
    fn every_sign_names_a_tool_this_deployment_serves() {
        let prefix = prefix();
        let rendered = signage();
        let mut found = Vec::new();
        for occurrence in rendered.match_indices(&prefix) {
            let rest = &rendered[occurrence.0 + prefix.len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            assert!(
                TOOL_NAMES.contains(&name.as_str()),
                "the signage names `{prefix}{name}`, which is not in {TOOL_NAMES:?} -- the model \
                 would be sent to a name the client cannot resolve"
            );
            found.push(name);
        }
        found.sort();
        let mut expected: Vec<String> = TOOL_NAMES.iter().map(|name| name.to_string()).collect();
        expected.sort();
        assert_eq!(
            found, expected,
            "R-M4: all eight control tools are signed for, each exactly once"
        );
    }

    /// The flat spelling is the dialect's, not a literal typed here.
    ///
    /// The one thing this text has to get right about the *wire*: a Claude Code
    /// client permits, declares and calls an MCP tool by one flat string, and a
    /// block that spelled it any other way would name tools the client's
    /// `--allowedTools` never matches.
    #[test]
    fn the_names_are_spelled_the_way_this_surfaces_client_spells_them() {
        let rendered = signage();
        for tool in TOOL_NAMES {
            let flat = ClientDialect::claude_messages().stored_call_name(tool);
            assert!(
                rendered.contains(&flat),
                "`{flat}` is how this client names `{tool}`:\n{rendered}"
            );
        }
        // And the *bare* name never appears as a call on its own: a model told
        // to call `status` calls nothing, because that is not a tool its client
        // registered.
        assert!(
            !rendered.contains("`status`"),
            "a bare tool name in the text is one the client cannot resolve:\n{rendered}"
        );
    }

    /// R-M4's cost control: the block says *when*, and never repeats *what*.
    ///
    /// Every descriptor's own description already rides in `tools[]` on every
    /// request this client makes (§5.8), so a sentence duplicated here is paid
    /// for twice per turn, forever, and becomes a second copy to keep in step.
    /// Asserted on the descriptions themselves rather than on a length budget,
    /// because the failure is a copy-paste and not a size.
    #[test]
    fn the_signage_does_not_restate_the_descriptors_the_client_already_carries() {
        let rendered = signage();
        for descriptor in descriptors() {
            assert!(
                !rendered.contains(descriptor.description),
                "`{}`'s own description is in the appended block as well as in the toolbox the \
                 client sends on every request",
                descriptor.name
            );
        }
    }

    /// Nothing here is a secret, and nothing here can become one.
    ///
    /// [`signage`] takes no arguments at all, so this is a pin on that
    /// signature as much as on the text: the day somebody threads a launch into
    /// it to interpolate an address, this is what asks why a prompt needs one.
    #[test]
    fn the_signage_is_a_pure_function_of_the_tool_list() {
        assert_eq!(signage(), signage());
        assert!(!signage().contains("http"), "{}", signage());
    }
}
