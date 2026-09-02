// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Is this tool call the agent talking to *us* rather than working on its task?
//!
//! One question, one module. The names roundhouse's own MCP surface serves,
//! the namespace a client flattens them under, the renderer that composes the
//! flat spelling and the recognizer that takes it back apart all answer it, and
//! none of them has anything to do with pairing calls to their results — which
//! is the job of [`exchange`](super::exchange), where this used to live (M12
//! review, F1).
//!
//! **The recognizer is parameterised by the surface, and that is the whole of
//! its design** (M12 review, F8). Two clients spell one call two ways: Claude
//! Code folds the MCP registration's server name into every tool name it
//! declares and emits (`mcp__roundhouse__status`), while codex sends the
//! namespace in its own wire field and canonicalization drops it, leaving
//! `status` alone in the log. Asking one dialect-blind function to accept both
//! is what F8 found: a Messages client's own tool literally named `status` was
//! swallowed by an exemption written for the *other* wire, and dropped from the
//! task view along with roundhouse's own chatter.

use super::exchange::Exchange;

/// The namespace codex flattens roundhouse's own MCP tools under.
///
/// The definition, not a copy: `roundhouse-server`'s
/// `dialect::DEFAULT_MCP_NAMESPACE` re-exports this, and the server's rich doc
/// about what an operator may rename lives there. It is stated *here* because
/// the code that has to recognise a control call is below the server —
/// [`ToolSignals`](super::ToolSignals) and the trigger's signals are in this
/// crate and cannot see `roundhouse-server`. A second literal in this crate was
/// the alternative and it fails in the direction that costs most: a rename
/// would leave this classifier matching a name nothing emits, every control
/// call would go back to reading as agent trouble, and nothing would be red.
///
/// **There is no per-deployment rename**, and that is a decision rather than an
/// omission (M12 review, F2): the control plane's `mcp_namespace` knob is
/// refused, because both launchers' registrations, the signage and this
/// classifier have to name one server and a configured value reached none of
/// them.
pub const CONTROL_TOOL_NAMESPACE: &str = "mcp__roundhouse";

/// What a client puts between a namespace and a tool's own name.
///
/// `codex-mcp/src/mcp/mod.rs:78-81` @ `e363b08` builds the namespace as
/// `mcp{DELIMITER}{server}{DELIMITER}` and `core/src/tools/handlers/mcp.rs:53`
/// joins `{namespace}{DELIMITER}{name}`; [`CONTROL_TOOL_NAMESPACE`] is the
/// `mcp{DELIMITER}{server}` half without the trailing delimiter. Claude Code
/// composes the same two halves the same way
/// (`research/claude-code-client-surface.md` §5.8).
pub const CONTROL_TOOL_DELIMITER: &str = "__";

/// Roundhouse's own eight control tools, under the names their MCP surface
/// declares them by.
///
/// The definition, not a copy: `roundhouse_mcp::tools::TOOL_NAMES` re-exports
/// this, and that crate's `the_names_and_the_descriptors_are_one_list` is what
/// holds the served descriptors to it. Stated *here* for the same reason
/// [`CONTROL_TOOL_NAMESPACE`] is — the code that has to *recognise* a control
/// call lives a crate below the surface that serves one, and two literals that
/// must agree across a crate boundary is a rename that goes silently
/// half-done.
///
/// **Why a list of names is needed at all, and it is not a convenience.** A
/// client that folds the namespace into the tool's own name leaves the whole
/// `mcp__roundhouse__status` in the log, and the prefix alone identifies it. A
/// client that sends the namespace in its own wire field leaves only `status`
/// — the namespace is dropped at canonicalization on purpose (see
/// `responses_api::wire::canonical_item`) — and there is then *nothing* in the
/// stored record but the name. See [`ControlCallDialect::CodexResponses`] for
/// what that costs.
pub const CONTROL_TOOL_NAMES: [&str; 8] = [
    "status",
    "init_session",
    "declare_intent",
    "prefer",
    "set_quality_floor",
    "fetch_steer",
    "report_outcome",
    "explain_last_route",
];

/// The flat spelling of one of our tools: `mcp__roundhouse__<tool>`.
///
/// The one renderer (M12 review, F7). `codex_launch::skills` writes this into
/// every generated skill file, `claude_launch::signage` writes it into the
/// appended system prompt, and `dialect::ClientDialect::stored_call_name` says
/// it is what the Messages log holds — three sites that used to compose the
/// same two constants with three `format!` calls, none of which had a test
/// against the others. [`is_flat_control_call`] is this function's inverse and
/// sits beside it for the same reason.
pub fn flat_control_call_name(tool: &str) -> String {
    format!("{CONTROL_TOOL_NAMESPACE}{CONTROL_TOOL_DELIMITER}{tool}")
}

/// How the client that wrote a session's log spells a call to one of our tools.
///
/// **A parameter and not a constant, because the two surfaces disagree about
/// what a *bare* name means** (M12 review, F8). Recognising both spellings
/// everywhere reads as generous and is not: on the Messages surface the
/// namespace is never dropped, so a bare `status` in that log is provably not
/// ours, and accepting it silently exempted a client's own tool from every
/// signal the trigger computes while `turn_depth` went on counting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCallDialect {
    /// Codex over the OpenAI Responses API: the namespace rides its own wire
    /// field and canonicalization drops it, so the log holds `status`.
    CodexResponses,
    /// Claude Code over the Anthropic Messages API: the namespace is folded
    /// into the tool's own name, so the log holds `mcp__roundhouse__status`.
    ClaudeMessages,
}

impl ControlCallDialect {
    /// Which surface wrote the session under `session_key`.
    ///
    /// The Messages surface keys its sessions `…/anthropic_messages/<id>`
    /// (`messages_api::wire::session_key`) and nothing else does, so the key is
    /// the one place the client's identity survives into a crate that cannot
    /// see either wire module. Read here rather than at each fold so there is
    /// one seam to correct if a third surface ever keys itself differently.
    pub fn of_session_key(session_key: &str) -> Self {
        if session_key
            .split('/')
            .any(|segment| segment == MESSAGES_SESSION_SEGMENT)
        {
            Self::ClaudeMessages
        } else {
            Self::CodexResponses
        }
    }

    /// Whether `name`, as this dialect's client spells it, is one of ours.
    fn recognises(self, name: &str) -> bool {
        match self {
            // The flat spelling carries whose server it is, so it is exact.
            Self::ClaudeMessages => is_flat_control_call(name),
            // The bare spelling does not, so recognition is by name alone and a
            // third party's MCP tool called `status` is exempted with ours.
            // That under-counts an agent's work by a call or two; the failure
            // it replaces over-counted roundhouse's own chatter as the agent's
            // work and fired steers at an agent that had done nothing wrong,
            // which is the louder error and the one G04 names. Closing it
            // properly means keeping the namespace in the stored record, which
            // moves the canonical form of every already-stored tool-using
            // session — a decision above this function.
            Self::CodexResponses => CONTROL_TOOL_NAMES.contains(&name),
        }
    }
}

/// The session-key segment the Anthropic Messages surface stamps.
///
/// Spelled here rather than imported because the server names it a crate above;
/// `messages_api::wire`'s own suite is what holds the two together.
const MESSAGES_SESSION_SEGMENT: &str = "anthropic_messages";

/// Whether this call is the agent talking to *us* rather than working on its
/// task, as `dialect`'s client spells it.
pub fn is_control_call_on(name: &str, dialect: ControlCallDialect) -> bool {
    dialect.recognises(name)
}

/// The flat spelling alone: `mcp__roundhouse__<tool>`, any tool.
///
/// Separate from [`is_control_call_on`] because the two halves answer different
/// questions and only this one is exact. A flat name under our namespace is
/// ours whatever the tool is called — including a tool a later milestone adds
/// and this crate's list has not learned yet — while the bare half can only
/// ever check a name against a list.
///
/// Matched on the namespace *and* the delimiter together, never on the
/// namespace alone: a second MCP server called `roundhouse_extra` flattens to
/// `mcp__roundhouse_extra__…`, which a bare `starts_with` would swallow into
/// our own control traffic and quietly exempt somebody else's tools from every
/// signal in the trigger.
pub fn is_flat_control_call(name: &str) -> bool {
    name.strip_prefix(CONTROL_TOOL_NAMESPACE)
        .and_then(|rest| rest.strip_prefix(CONTROL_TOOL_DELIMITER))
        .is_some_and(|tool| !tool.is_empty())
}

/// The exchanges that are the agent working on its task, as `dialect`'s client
/// wrote them.
///
/// **Roundhouse's own control calls are dropped, not re-categorised** (G04).
/// Every signal in the trigger and every count in
/// [`ToolSignals`](super::ToolSignals) asks a question about what the *agent*
/// is doing, and an agent reading its own budget is not doing the task: a
/// fifth `ToolCategory` would still leave `status`, `explain_last_route`,
/// `prefer` and `set_quality_floor` inside the streaks, the windows and the
/// depth that the signals are computed over — which is how four calls made
/// because our own generated `rh-status` skill told the model to make them
/// bought a judge side-call the session did not need.
///
/// A `Vec<&Exchange>` rather than a filtered clone because the outputs are
/// whole tool results and this runs on the turn path; the borrowed view costs
/// one pointer per call and the clone would cost the transcript.
pub fn task_exchanges_on(exchanges: &[Exchange], dialect: ControlCallDialect) -> Vec<&Exchange> {
    exchanges
        .iter()
        .filter(|exchange| !is_control_call_on(&exchange.name, dialect))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::item::Item;
    use crate::validate::exchange::exchanges;

    fn call(call_id: &str, name: &str) -> Item {
        Item::tool_call(call_id, name, "{}")
    }

    fn task_names(items: &[Item], dialect: ControlCallDialect) -> Vec<String> {
        let exchanges = exchanges(items);
        task_exchanges_on(&exchanges, dialect)
            .iter()
            .map(|exchange| exchange.name.clone())
            .collect()
    }

    /// The classifier on the spelling every suite in this crate feeds it.
    ///
    /// **Written as the control for the test below while that one was
    /// `#[ignore]`d**, and kept now that both are live. Without it, "the
    /// exclusion is inert on the other surface" was indistinguishable from a
    /// classifier that recognised nothing at all — an unfalsifiable claim
    /// rather than a finding about one surface. It still earns its place: it is
    /// the half that holds if the bare arm is ever taken back out.
    #[test]
    fn a_flat_control_call_is_recognised_and_dropped_from_the_task_view() {
        let name = flat_control_call_name("status");
        assert!(is_control_call_on(
            &name,
            ControlCallDialect::ClaudeMessages
        ));

        let items = vec![call("c1", &name), call("c2", "grep")];
        assert_eq!(
            task_names(&items, ControlCallDialect::ClaudeMessages),
            vec!["grep"],
            "a flat-named control call is the agent talking to us, not working"
        );
    }

    /// R-M0 / R-M1 (M12): the same control call, spelled the way the
    /// *Responses* wire actually stores it, is recognised too.
    ///
    /// **The mechanism, and it is not a bug in the wire.** Codex presents an
    /// MCP server to the model as a `namespace` object and lists each tool
    /// under its **bare** name, so the model's call comes back as
    /// `{"name":"status","namespace":"mcp__roundhouse"}` — two wire fields, not
    /// one flat string. `responses_api::wire::canonical_item` deliberately
    /// drops the namespace (keeping it would fork every already-stored
    /// tool-using session), so the log stores `status`. That is pinned a crate
    /// above by `r_m0_a_codex_mcp_call_arrives_bare_with_a_separate_namespace`,
    /// built from codex's own `ResponseItem`, and the namespace half was read
    /// off a real binary by `codex_e2e.rs`'s
    /// `the_delimiter_a_skill_spells_is_the_one_the_real_binary_namespaces_with`.
    ///
    /// **What it cost.** Roundhouse's control tools landed in
    /// [`task_exchanges_on`] as ordinary work, so a session whose agent polls
    /// `status` accrued repeat-detection and no-progress signal against calls
    /// it made *because our own generated skill told it to* — the exact failure
    /// G04 was written to close.
    #[test]
    fn a_control_call_as_the_responses_wire_stores_it_is_recognised() {
        // What `canonicalize` writes to the log for a codex `status` call.
        let stored = "status";
        assert!(
            is_control_call_on(stored, ControlCallDialect::CodexResponses),
            "the agent asked roundhouse for its own status; the fold counted it \
             as work on the task"
        );

        let items = vec![call("c1", stored), call("c2", "grep")];
        assert_eq!(
            task_names(&items, ControlCallDialect::CodexResponses),
            vec!["grep"]
        );
    }

    /// Every control tool, in the spelling its own surface stores, and the near
    /// misses on each.
    ///
    /// The list is walked rather than sampled because the bare arm is an exact
    /// membership test: a tool added to [`CONTROL_TOOL_NAMES`] and *not* to the
    /// MCP surface — or the reverse — is a name one half of this deployment
    /// recognises and the other does not, and only walking the whole list makes
    /// that go red here rather than in a session's signal counts.
    #[test]
    fn both_spellings_of_every_control_tool_are_ours_and_the_near_misses_are_not() {
        for tool in CONTROL_TOOL_NAMES {
            assert!(
                is_control_call_on(tool, ControlCallDialect::CodexResponses),
                "the bare spelling of `{tool}`"
            );
            assert!(
                is_control_call_on(
                    &flat_control_call_name(tool),
                    ControlCallDialect::ClaudeMessages
                ),
                "the flat spelling of `{tool}`"
            );
        }

        // The near misses, which are what keep the exemption from being a
        // blanket one. A neighbouring server flattens to a name that merely
        // *starts* with ours; another server's bare tool is a bare name that is
        // not one of ours; and the namespace on its own names no tool at all.
        // Walked on *both* dialects rather than on a union recogniser, so a
        // near miss that only one surface rejects cannot pass by being caught
        // on the other.
        for theirs in [
            "mcp__roundhouse_extra__status",
            "mcp__other__query",
            "query",
            "grep",
            CONTROL_TOOL_NAMESPACE,
            "mcp__roundhouse__",
        ] {
            for dialect in [
                ControlCallDialect::ClaudeMessages,
                ControlCallDialect::CodexResponses,
            ] {
                assert!(
                    !is_control_call_on(theirs, dialect),
                    "`{theirs}` is not ours on {dialect:?}"
                );
            }
        }
    }

    /// M12 review F8: each surface accepts only the spelling its own client
    /// can produce, and the union accepts both.
    ///
    /// The asymmetry is the finding. A bare `status` on the Messages surface is
    /// provably *not* ours — Claude Code folds the namespace into every tool
    /// name it emits, so ours arrives flat — while on the Responses surface it
    /// is the only spelling ours can have. A dialect-blind classifier has to
    /// pick one of those to be wrong about, and picking "accept both" made it
    /// wrong on the surface M12 had just brought in.
    #[test]
    fn each_surface_accepts_only_the_spelling_its_own_client_produces() {
        let flat = flat_control_call_name("status");

        assert!(!is_control_call_on(
            "status",
            ControlCallDialect::ClaudeMessages
        ));
        assert!(is_control_call_on(
            &flat,
            ControlCallDialect::ClaudeMessages
        ));

        assert!(is_control_call_on(
            "status",
            ControlCallDialect::CodexResponses
        ));
        assert!(!is_control_call_on(
            &flat,
            ControlCallDialect::CodexResponses
        ));
    }

    /// The session key is what tells the fold which client wrote the log.
    ///
    /// Pinned on the shapes both surfaces actually mint — the Messages key is
    /// `<project>/<user>/anthropic_messages/<id>`, with an optional
    /// `/agent/<name>` tail — rather than on a bare `contains`, so a
    /// conversation a *Responses* client happened to name `anthropic_messages`
    /// does not get read as a surface it is not.
    #[test]
    fn the_session_key_names_the_surface_that_wrote_it() {
        for messages in [
            "anthropic_messages/s1",
            "acme/ada/anthropic_messages/c0cb70b6",
            "acme/ada/anthropic_messages/s1/agent/agent-7",
        ] {
            assert_eq!(
                ControlCallDialect::of_session_key(messages),
                ControlCallDialect::ClaudeMessages,
                "`{messages}` is a Messages session key"
            );
        }

        for responses in [
            "acme/ada/conv/my-conversation",
            "acme/ada/anthropic_messages_lookalike/s1",
            "acme/ada/not-anthropic_messages/s1",
            "s1",
        ] {
            assert_eq!(
                ControlCallDialect::of_session_key(responses),
                ControlCallDialect::CodexResponses,
                "`{responses}` is not a Messages session key"
            );
        }
    }

    /// The bare arm's price, pinned rather than left to be discovered.
    ///
    /// Somebody else's MCP server offering a tool named `status` arrives over
    /// the Responses wire as the bare string `status`, exactly as ours does,
    /// and is exempted from the task view with it. Nothing in the stored record
    /// can tell the two apart — the namespace that could have is the field
    /// canonicalization drops.
    ///
    /// Asserted as a *fact about the trade*, not as a behaviour anyone wants:
    /// the day the log keeps a namespace, this test is the one that should go
    /// red and be deleted. It is now scoped to the one surface that pays it —
    /// F8's correction — and the Messages half below is what proves the scoping
    /// is real.
    #[test]
    fn a_third_partys_bare_status_tool_is_exempted_with_ours_on_the_responses_wire_only() {
        let items = vec![call("c1", "status"), call("c2", "grep")];
        assert_eq!(
            task_names(&items, ControlCallDialect::CodexResponses),
            vec!["grep"],
            "a bare `status` is indistinguishable from ours once the wire has \
             dropped the namespace"
        );
        assert_eq!(
            task_names(&items, ControlCallDialect::ClaudeMessages),
            vec!["status", "grep"],
            "the Messages wire never drops the namespace, so a bare `status` \
             there is the client's own tool and is the agent's work"
        );

        // The control, and the reason the price is small: the *flat* spelling
        // still carries whose server it is, so a client that sends one is
        // classified exactly.
        let flat_items = vec![call("c1", "mcp__other__status"), call("c2", "grep")];
        assert_eq!(
            task_names(&flat_items, ControlCallDialect::ClaudeMessages),
            vec!["mcp__other__status", "grep"]
        );
    }
}
