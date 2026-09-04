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
//! namespace in its own wire field and the log keeps it there, beside a bare
//! `status`. Asking one dialect-blind function to accept both is what F8
//! found: a Messages client's own tool literally named `status` was swallowed
//! by an exemption written for the *other* wire, and dropped from the task view
//! along with roundhouse's own chatter.
//!
//! **Since M17 (R-N9) the Responses arm reads the stored namespace first and
//! the bare name only as a fallback.** The log carries the field now, so a call
//! that names `mcp__roundhouse` is exactly ours and one that names another
//! server is exactly not. The bare arm stays for records written before the
//! field existed, which can never gain one — recovering it means guessing which
//! bare `status` was ours, and rewriting the log would move the turn id of
//! every conversation holding a control call.

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
/// client that sends the namespace in its own wire field leaves a bare
/// `status` in `name`, with the server it went to in the record's own
/// `namespace` field since M17 — so the name alone still identifies nothing,
/// and the list is what the field is checked *against* once it says the call is
/// ours. For a record written before M17 the field is absent and the list is
/// all there is; see [`ControlCallDialect::CodexResponses`] for what that
/// costs and why the arm cannot be removed.
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
    /// field, and since M17 the log keeps it there — a bare `status` in `name`
    /// with `mcp__roundhouse` beside it. A record written before M17 has the
    /// bare name and nothing else, which is the case the recogniser's fallback
    /// arm exists for and the one the remaining exemption pays for.
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

    /// Whether the call `name`, under `namespace`, as this dialect's client
    /// spells it, is one of ours.
    fn recognises(self, name: &str, namespace: Option<&str>) -> bool {
        match self {
            // The flat spelling carries whose server it is, so it is exact —
            // and the namespace argument is ignored here rather than consulted,
            // because on this surface it is `None` by construction. Claude Code
            // folds the registration into every tool name it declares, so the
            // wire has no separate field for canonicalization to read; a `Some`
            // arriving here would be a record no Messages client can write.
            Self::ClaudeMessages => is_flat_control_call(name),
            // **The field first, the bare name second** (M17, R-N9).
            //
            // Codex sends the namespace beside the name, and since M17 the log
            // keeps it, so a call that says `mcp__roundhouse` is exactly ours
            // and a call that names any *other* server is exactly not — which
            // closes the collision this arm used to pay for, where a third
            // party's MCP tool called `status` was exempted from the task view
            // along with roundhouse's own chatter.
            //
            // `None` is the arm that cannot be removed. Records written before
            // M17 carry no namespace and never will: the field could only be
            // recovered by guessing which bare `status` was ours, which is the
            // ambiguity the change exists to remove, and a rewrite would move
            // the turn id of every conversation holding a control call. So a
            // `None` falls back to name-alone recognition and keeps the old
            // trade — an under-count of a call or two, against G04's
            // over-count of roundhouse's own chatter, which fired steers at an
            // agent that had done nothing wrong and is the louder error. The
            // exposure was one name of eight and is now zero for every record
            // written after the change.
            Self::CodexResponses => match namespace {
                Some(CONTROL_TOOL_NAMESPACE) => CONTROL_TOOL_NAMES.contains(&name),
                Some(_) => false,
                None => CONTROL_TOOL_NAMES.contains(&name),
            },
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
///
/// Takes the stored `namespace` as well as the stored name since M17 (R-N9),
/// because on the Responses surface the name alone can no longer answer the
/// question for a record that has one: `status` under `mcp__other` is the
/// client's own tool and `status` under `mcp__roundhouse` is ours, and the
/// caller holding the record is the only one that can tell this function
/// which. `None` means the record does not say — a pre-M17 log, or a plain
/// function tool that sends no namespace at all — and is what the bare-name
/// fallback exists for.
pub fn is_control_call_on(
    name: &str,
    namespace: Option<&str>,
    dialect: ControlCallDialect,
) -> bool {
    dialect.recognises(name, namespace)
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
        .filter(|exchange| {
            !is_control_call_on(&exchange.name, exchange.namespace.as_deref(), dialect)
        })
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

    /// A call a *post-M17* Responses log holds: the name and the namespace its
    /// client sent beside it.
    fn namespaced_call(call_id: &str, name: &str, namespace: &str) -> Item {
        Item::namespaced_tool_call(call_id, name, Some(namespace.to_string()), "{}")
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
            None,
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
    /// one flat string. `responses_api::wire::canonical_item` used to drop the
    /// namespace, so the log stored `status` alone; M17 carries it beside the
    /// name instead, without moving any turn id (the render leaves it out), and
    /// this test still exercises the arm that reads such a record — because
    /// every record written before M17 is one. That is pinned a crate
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
            is_control_call_on(stored, None, ControlCallDialect::CodexResponses),
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
                is_control_call_on(tool, None, ControlCallDialect::CodexResponses),
                "the bare spelling of `{tool}` in a record written before M17"
            );
            assert!(
                is_control_call_on(
                    &flat_control_call_name(tool),
                    None,
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
                    !is_control_call_on(theirs, None, dialect),
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
            None,
            ControlCallDialect::ClaudeMessages
        ));
        assert!(is_control_call_on(
            &flat,
            None,
            ControlCallDialect::ClaudeMessages
        ));

        assert!(is_control_call_on(
            "status",
            None,
            ControlCallDialect::CodexResponses
        ));
        assert!(!is_control_call_on(
            &flat,
            None,
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

    /// The bare arm's price, pinned rather than left to be discovered — and
    /// **narrowed by M17 to the records that still pay it**.
    ///
    /// Somebody else's MCP server offering a tool named `status` arrives over
    /// the Responses wire as the bare string `status`, exactly as ours did
    /// before M17, and is exempted from the task view with it. Nothing in such
    /// a record can tell the two apart — the namespace that could have was the
    /// field canonicalization dropped.
    ///
    /// **The day the log kept a namespace, this was to be the test to delete,
    /// and deleting it would have been wrong** (R-N9). A record written before
    /// M17 carries no namespace and never will: recovering one means guessing
    /// which bare `status` was ours, which is the ambiguity the change exists
    /// to remove, and rewriting the log would move the turn id of every
    /// conversation holding a control call. So the bare arm stays as the
    /// fallback for a `None`, this test narrows to that arm, and
    /// `a_third_partys_status_under_another_namespace_is_the_agents_work` is
    /// the half that proves the price is no longer paid by anything written
    /// after the change. Still scoped to the one surface that pays it — F8's
    /// correction — with the Messages half below proving the scoping is real.
    #[test]
    fn a_third_partys_bare_status_tool_is_exempted_with_ours_on_the_responses_wire_only() {
        // Both calls stored with no namespace: a log written before M17.
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

    /// **R-N9: a third party's `status`, under a third party's namespace, is
    /// the agent's own work — and ours, under ours, is still not.**
    ///
    /// The half the test above could not assert and the reason the exemption
    /// narrowed rather than stayed. The realistic collision was one name of
    /// eight (`status` is a common MCP tool name; the other seven are
    /// distinctive enough that a clash would be a coincidence), and for every
    /// record written after M17 it is now zero: the stored record itself says
    /// which server the call went to.
    ///
    /// The three shapes are asserted together on purpose. `Some("mcp__other")`
    /// is a *different* answer from `None`, not a stricter one — a fold that
    /// simply ignored the field would put the first call back in the exemption
    /// and this would go red, while a fold that required a namespace outright
    /// would drop the pre-M17 record on the floor and the test above would.
    #[test]
    fn a_third_partys_status_under_another_namespace_is_the_agents_work() {
        let items = vec![
            namespaced_call("c1", "status", "mcp__other"),
            namespaced_call("c2", "status", CONTROL_TOOL_NAMESPACE),
            call("c3", "grep"),
        ];
        assert_eq!(
            task_names(&items, ControlCallDialect::CodexResponses),
            vec!["status", "grep"],
            "the `status` that survives is the one under somebody else's \
             server: it is the agent working on its task, and folding it out \
             with ours is the under-count the carried namespace closes"
        );

        // And the same call under our own namespace is ours whichever tool it
        // names, walked over the whole list so a name added to
        // `CONTROL_TOOL_NAMES` and not to the surface goes red here.
        for tool in CONTROL_TOOL_NAMES {
            assert!(
                is_control_call_on(
                    tool,
                    Some(CONTROL_TOOL_NAMESPACE),
                    ControlCallDialect::CodexResponses
                ),
                "`{tool}` under `{CONTROL_TOOL_NAMESPACE}` is ours"
            );
            assert!(
                !is_control_call_on(tool, Some("mcp__other"), ControlCallDialect::CodexResponses),
                "`{tool}` under somebody else's server is theirs"
            );
        }
    }

    /// The Messages arm did not move, and a `Some` cannot reach it.
    ///
    /// R-N9 left that surface alone because there is nothing there to read:
    /// Claude Code folds the registration into every tool name it declares, so
    /// canonicalization has no separate field to store and the record's
    /// namespace is `None` by construction. Pinned anyway, because "the
    /// recogniser now takes a namespace" is exactly the change that invites a
    /// later reader to start consulting it on both arms — and doing so on this
    /// one would make a flat `mcp__roundhouse__status` stop being ours the day
    /// something upstream started filling the field in.
    #[test]
    fn the_messages_arm_reads_the_flat_name_and_not_the_field() {
        let flat = flat_control_call_name("status");
        for namespace in [None, Some(CONTROL_TOOL_NAMESPACE), Some("mcp__other")] {
            assert!(
                is_control_call_on(&flat, namespace, ControlCallDialect::ClaudeMessages),
                "`{flat}` is ours on the Messages surface whatever a namespace \
                 field says ({namespace:?})"
            );
            assert!(
                !is_control_call_on("status", namespace, ControlCallDialect::ClaudeMessages),
                "a bare `status` is the client's own tool on the Messages \
                 surface, and no namespace field changes that ({namespace:?})"
            );
        }
    }
}
