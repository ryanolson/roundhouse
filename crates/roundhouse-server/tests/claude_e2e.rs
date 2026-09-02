// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
#![cfg(feature = "e2e-claude")]

//! M11.2b of `PLAN-agentic-control-plane.md`: a **real `claude` binary** driving
//! a real roundhouse over a real socket, on the Direct topology.
//!
//! [`codex_e2e`](../codex_e2e.rs)'s sibling, and deliberately its mirror: the
//! same discipline, the same evidence posture, the other client and the other
//! dialect. Every other Messages suite in this crate doubles the client — the
//! best of them, `messages_api_surface.rs`, replays request bodies captured from
//! the real binary, which is a very good double and still a double: a fixture
//! cannot decide to resend something else. This one doubles nothing on the
//! client side. It spawns `claude -p`, hands it exactly the environment
//! [`roundhouse_server::claude_launch`] generates, and lets the real client
//! decide what to send, which tool to run, and what to send back.
//!
//! **The rig is not here; the claims are** (M12 review F11). The deployment,
//! the two topologies, the child command and everything that spawns a process
//! live in [`common::claude_rig`](common/claude_rig.rs); this file scripts the
//! upstream, drives that rig, and says what must be true of what came back.
//! Three consecutive milestones had each added to one file, and the seam was
//! already named — so the split is along it rather than at a line number.
//! What did *not* move is the structural guards that read source: they now scan
//! the module, because a guard living beside the code it guards is a file
//! checking itself.
//!
//! # What the suite is the closure of
//!
//! **Kinds of test, not a count of them.** An earlier draft of this doc opened
//! with a number and then listed the tests by name, which is a second spelling
//! of what `#[ignore]` already says — and it was wrong within one review round,
//! because a later round added one more and nothing made the prose follow
//! (M11.2b review F10). What is durable is the *taxonomy*: some
//! tests need the real `claude` binary, some need the real `nemo-relay` binary
//! as well, one needs Relay alone, and the rest need no binary at all because
//! what they catch is this harness lying to itself. Each is gated by an
//! `#[ignore]` reason that names which, and
//! [`the_module_doc_does_not_enumerate_a_count_of_real_binary_tests`] keeps the
//! prose from growing a number back.
//!
//! The real-binary tests that need `claude`, on the Direct topology:
//!
//! - `a_real_claude_binary_completes_a_prose_turn_through_roundhouse` — that
//!   [`claude_launch`](roundhouse_server::claude_launch)'s environment map is
//!   one a real client actually hooks up with. Every unit test of that module
//!   proves the map *says* the right thing; only a spawned binary proves it
//!   reads it.
//! - `a_tool_using_turn_is_run_by_the_real_client_and_its_result_rejoins_the_session`
//!   — the M11.2a tool loop's first real-binary evidence. M11.2a proved the
//!   surface can *emit* interleaved `tool_use` blocks a strict oracle accepts
//!   and that a hand-built `tool_result` resend is prefix-admitted; the half it
//!   could not reach is that a real agent runs the call and that what it then
//!   resends is what our log holds. The `Read` here is executed by the client,
//!   against a file this test wrote, and the file's contents come back on the
//!   wire.
//! - `a_continued_run_extends_the_session_rather_than_forking_it` — where R-A
//!   meets the client rather than a fixture. 2.1.257 appends a trailing
//!   `role: "system"` message carrying `<total_tokens>N tokens left</total_tokens>`
//!   after the new user turn of every `--continue`, and `wire::canonicalize`
//!   drops it. Three runs, so both containers the notice arrives in are
//!   exercised; what is asserted is that the notice is on the wire and never
//!   becomes a log item. The *fork* the rule prevents is deliberately not
//!   asserted here — this client's counter does not move over a run of this
//!   rig, so a fork caused by it is not producible — and the test's own doc
//!   says so, and says where that claim is pinned instead.
//! - `the_seat_chain_a_launched_client_presents` — the `M11-SEAT-EVIDENCE`
//!   block. Printed, and asserted only on its *shape*: which header carried
//!   which kind of value. What a reader should conclude about a credential chain
//!   is not a thing a fixture should decide, and the subscription-login half of
//!   the chain cannot be observed here at all — see "The half this cannot
//!   observe" below.
//!
//! …the Chained ones drive the other deployment shape, through a real
//! `nemo-relay run --agent claude`:
//! [`a_chained_turn_reaches_roundhouse_with_the_turn_key_and_not_relays_own`]
//! (credential attribution and `?beta=true` survival across the hop) and
//! [`a_chained_continue_survives_relays_re_encode_without_forking`] (R7 hazard 1
//! against the real re-encoder). See "The chained topology" below.
//!
//! …the **closure** ones drive both topologies through a real `topham`, the
//! operator launcher, rather than through a launch this file constructed:
//! [`a_real_client_launched_through_topham_hooks_up_on_direct`] and
//! [`a_real_client_handed_to_relay_through_topham_hooks_up_chained`]. They are
//! gated on `ROUNDHOUSE_TEST_TOPHAM_BIN` on top of everything else, and what
//! they add over the tests above is the one link those cannot reach: that
//! something a person can run produces the map the rest of this file hands the
//! client directly.
//!
//! [`a_real_client_reaches_the_control_surface_through_the_turn`] is the same
//! kind and one rung further out (M12, R-M6): the launcher's generated argv
//! registers this deployment's own `/mcp` mount, and the client answers a
//! `tool_use` for a `mcp__roundhouse__*` tool by dispatching against it. It is
//! the only test in the file where **both** of this deployment's routers are
//! mounted on the socket the client talks to, and it is where the flat tool
//! name, the turn key on a second protocol, the tool-use-id correlation, the
//! prefix check and the validate fold's control-traffic exclusion are all one
//! run.
//!
//! …and the Relay-only ones need no `claude` at all: they drive `nemo-relay run
//! --dry-run` directly, which resolves configuration without spawning an agent.
//! See "Hazard 4, made detectable" at the bottom of this file.
//!
//! The rest need no binary, because what they catch is this harness lying to
//! itself: the constructed-command environment guard and its named limit, the
//! two-topology launch comparison, the fork probe's arithmetic, the root claim,
//! and the structural guards the M11.2b review left behind.
//!
//! # What is real here, and what is scripted
//!
//! Real: the `claude` binary, the HTTP transport, the Messages surface, the
//! MCP control surface beside it, the control directory and its minted turn
//! key, the session log, the prefix check, the tool the client chose to run,
//! and [`ClaudeEnv`] — the launcher's output is consumed verbatim rather than
//! re-spelled.
//!
//! Scripted: the frontier. An in-process [`FrontierClient`] answers every
//! dispatch, so this test decides when a `tool_use` block is emitted rather than
//! asking a model to decide. That is the only way the tool test is a test: a
//! real upstream would make it partly an assertion about a model's willingness
//! to call a tool.
//!
//! **The scripted upstream always terminates.** [`ScriptedTurns`] answers a
//! queue of turns and then prose forever, rather than repeating its last answer:
//! an upstream that replied `tool_use` to every dispatch would drive the real
//! client around its loop until [`CHILD_DEADLINE`] killed it, and the failure
//! would read as "the client hung" rather than "the script never ended".
//!
//! # How to run it
//!
//! ```text
//! timeout 300 cargo test -p roundhouse-server --features e2e-claude \
//!     --test claude_e2e -- --include-ignored --test-threads=1 --nocapture
//! ```
//!
//! `--features e2e-claude` compiles the file at all; `--include-ignored` opts
//! into spawning processes. The chained tests additionally need
//! `ROUNDHOUSE_TEST_RELAY_BIN`, and the closure tests
//! `ROUNDHOUSE_TEST_TOPHAM_BIN` naming a **freshly built** `target/debug/topham`
//! (`cargo build -p topham`) — freshly, because a stale one reports green for
//! code nobody compiled. `--test-threads=1` is not politeness: `claude
//! --continue` resolves "the most recent conversation" from the session store in
//! the run's isolated `HOME`, keyed by working directory, so two tests
//! interleaving their spawns would be two clients racing for one rollout. Once
//! opted in, a missing binary is a loud panic naming `ROUNDHOUSE_TEST_CLAUDE_BIN`
//! rather than a silent skip.
//!
//! **No network is needed**, and no credential exists to leak. The server binds
//! `127.0.0.1:0`, the frontier is in-process, and the child's environment is
//! *cleared* before it is built.
//!
//! # The isolation trap this suite is written around
//!
//! `agent-docs/research/claude-code-client-surface.md` §5.7: with
//! `CLAUDE_CODE_REMOTE=true` in the environment — which is exactly the
//! environment a Claude Code Remote container runs this repository's own
//! sessions in — the client presents the container's **managed OAuth token**,
//! because §1.3's API-key arm is guarded by `!CLAUDE_CODE_REMOTE` and the
//! sentinel therefore suppresses nothing. A real subscription seat would reach
//! this test rig, on a socket that records every header it is handed.
//!
//! Three things stand between that and a run of this file, and they are ordered
//! from most to least structural:
//!
//! 1. [`build_child_command`] calls `Command::env_clear()` and rebuilds the
//!    environment from [`ClaudeEnv`] plus five named isolation variables.
//!    `CLAUDE_CODE_REMOTE` is not one of them and cannot be inherited.
//! 2. [`the_childs_environment_is_the_generated_map_plus_the_isolation_vars`]
//!    asserts that key set with `==` on the constructed [`std::process::Command`],
//!    with no binary and no socket, so the guard runs on every compile of this
//!    file rather than only when somebody opts into spawning. **What it cannot
//!    see is step 1 itself**: `Command::get_envs()` reports only the explicit
//!    `env()` diff, identically whether or not `env_clear()` ran, so a dropped
//!    `env_clear()` leaves this guard green — it is a check on the generated map,
//!    not a check that the map is all the child gets. Guard 3 is what actually
//!    closes that gap.
//! 3. [`the_seat_chain_a_launched_client_presents`] reads the *wire*: an
//!    `authorization` header or an `x-claude-*-remote-*` header on a request that
//!    reached this deployment means the clear leaked and the run is contaminated.
//!    This is the only guard of the three that would catch a dropped
//!    `env_clear()` — guard 2's `==` still passes, because the ambient variable
//!    it would leak was never in the explicit diff to begin with.
//!
//! The run's `HOME` and `CLAUDE_CONFIG_DIR` live under the **system temp
//! directory** and never under `target/`, which is where the codex sibling puts
//! its `CODEX_HOME`. That difference is a fact about this client: Claude Code
//! discovers `CLAUDE.md`, a project `.claude/` settings directory (hooks
//! included) and the enclosing git repository by walking up from its working
//! directory, so a run rooted inside this checkout would launch with *this
//! repository's* instructions, hooks and git status as ambient context — the
//! same class of contamination the environment clear exists to prevent, arriving
//! by a different door.
//!
//! # The half this cannot observe
//!
//! [`ClaudeAuthKind::ForwardedClaudeLogin`] is not driven here. The codex
//! sibling could forge its half of that chain — 0.146.0 reads an unsigned
//! `auth.json` and never checks a JWT signature, so a hermetic seat is
//! constructible — and nothing equivalent is true of a `claude` subscription
//! login, which is a keychain/OAuth artifact this box does not hold and must not
//! be given one. So that arm stays a documented one-capture: §1.3 predicts the
//! login's bearer would ride `Authorization` while the turn key rides
//! [`TURN_KEY_HEADER`], which is precisely the pass-through shape
//! `control_config`'s own suites pin from the other side. What this file adds is
//! the negative that makes the prediction checkable: under `RoundhouseKey` the
//! real client presents **no** `Authorization` at all.
//!
//! # Version vigilance
//!
//! Written against, and verified against, `claude 2.1.257`. The version is
//! printed on every run and a mismatch prints a WARNING rather than failing, for
//! the reason the codex sibling gives: this is evidence about a *binary*, and a
//! green run against an unread version is the silent change of meaning
//! CLAUDE.md's vigilance rule exists to catch. Three 2.1.257-specific facts are
//! load-bearing and would move:
//!
//! - the trailing `<total_tokens>` notice exists at all, and is a `role:
//!   "system"` message rather than turn configuration (R-A; §5.7);
//! - `-p` with `--output-format json` prints one JSON document whose
//!   `session_id`, `result` and `num_turns` this file reads. An older line
//!   printed the same fields; a client that renamed one would fail here on the
//!   parse rather than on an assertion, which is the loud direction;
//! - `--allowedTools Read` is enough to let a print-mode run execute a `Read`
//!   against a file inside its own working directory without a permission
//!   prompt. A client that started prompting would surface as
//!   `permission_denials` in the result document, which
//!   [`ClaudeRun::assert_completed`] prints.
//!
//! # The chained topology
//!
//! The last two tests drive the *other* deployment shape: `nemo-relay run
//! --agent claude` between the client and roundhouse. They are gated on
//! `ROUNDHOUSE_TEST_RELAY_BIN` on top of everything above, and a missing Relay
//! under `--include-ignored` is the same loud panic naming the variable that a
//! missing client is.
//!
//! **Where "Relay evidence §x" points.** Every claim here or in the rig module
//! about what Relay
//! does is read from one document —
//! `agent-docs/research/nemo-relay-0.8.0-published-read.md`, whose 2026-09-01
//! addendum re-derived each of them against the 0.8.2 tarball this suite drives
//! — and is cited by section, never by file and line. M11.2b review F10: a
//! citation by file and line, copied into Rust, is a claim about a pinned tree
//! that nobody re-derives when the pin moves — and it had already been copied
//! into two files. One pointer per claim leaves one place to re-read, which is
//! what CLAUDE.md's synergy-vigilance rule asks for.
//!
//! **The chained launch is the Direct launch, wrapped** (R-D′). Relay overwrites
//! `ANTHROPIC_BASE_URL` with its own gateway and *merges* its
//! `x-nemo-relay-proxy-token` into `ANTHROPIC_CUSTOM_HEADERS` rather than
//! replacing the block, forwards headers it does not own untouched, and strips
//! its own credential before dispatch — so the same [`ClaudeEnv`] serves both
//! topologies and a chained turn keeps Direct's semantics exactly: the turn key
//! on [`TURN_KEY_HEADER`], the sentinel inert on `x-api-key`.
//! [`the_chained_child_is_relay_wrapping_the_very_same_launch`] holds the two
//! spellings against each other without spawning anything.
//!
//! **Three unit guards are cited rather than re-run**, because each is a
//! property of *our* code that one end-to-end sample could only agree with:
//! `wire`'s Relay-alphabetized-resend test (hazard 1), `emit`'s no-`data:`-frame
//! rule (hazard 2), and `messages_api`'s note on the `?beta=true` route. What the
//! chained tests add is the half no unit test can hold: that the artefact those
//! guards are written against is the artefact Relay actually produces.
//!
//! # The chained refusals — documented, and one now guarded
//!
//! Two of R-D's rulings happen inside Relay's process, where a live turn
//! through this file cannot watch them. They are refusals an operator must
//! honour, recorded here and in
//! [`claude_launch`](roundhouse_server::claude_launch)'s chained runbook:
//!
//! - **Hazard 4: set the upstream base URL and auth header in one config layer.**
//!   `replace_upstream_base_url` clears a configured `anthropic_auth_header`
//!   whenever the base URL is changed by a *different* layer
//!   (Relay evidence §A.5), so a deployment that names the base URL
//!   on the command line and the auth header in `config.toml` runs
//!   unauthenticated and finds out at roundhouse's 401. The reference wiring
//!   below sidesteps this by configuring no auth header at all — the turn key
//!   rides the client's own headers — which is *why* it is the reference.
//!   Unenforceable by roundhouse (the layering is Relay's), but the clearing
//!   is cheaply observable without a live turn:
//!   [`hazard_4_a_different_base_url_layer_clears_the_configured_auth_header`]
//!   drives `nemo-relay run --dry-run` directly, at the bottom of this file.
//! - **Hazard 5: a plugin's dispatch-override turn is key-authed only.** Relay's
//!   `effective_dispatch_request` strips provider credentials before redirecting
//!   a turn to an explicit target (Relay evidence §A.6), so a turn
//!   redirected by a plugin arrives carrying no forwarded seat whatever the
//!   client presented. No pass-through deployment may assume otherwise.
//!
//! **Resumption is not offered in band on this surface**, and R-D closes plan
//! open question 4 that way for this rung: Relay's SSE decoder ignores `id:`
//! lines outright (Relay evidence §A.3), so a cursor carried as an SSE
//! id does not survive the hop — and the Messages emitter carries none, so there
//! is nothing to lose today and a documented reason not to add one tomorrow.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use roundhouse_core::ids::SessionId;
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::now_ms;
use roundhouse_core::validate::{
    CONTROL_TOOL_NAMES, ControlCallDialect, exchanges, is_control_call_on, task_exchanges_on,
};
use roundhouse_server::claude_launch::{
    API_KEY_ENV, BASE_URL_ENV, CUSTOM_HEADERS_ENV, ClaudeLaunch, ROUNDHOUSE_API_KEY_SENTINEL,
};
// The variable the launch profiles below name as where the turn key is read
// from, read from the generator that defines it rather than spelled here: a
// profile that named a variable nothing exports resolves to a launch with no
// credential, which roundhouse admits and degrades to local-only.
use roundhouse_server::ClientDialect;
use roundhouse_server::codex_launch::DEFAULT_KEY_ENV;
use roundhouse_server::control_config::TURN_KEY_HEADER;
use roundhouse_server::dialect::mcp_server_name;
use roundhouse_server::mcp_api::MCP_MOUNT_PATH;
use roundhouse_server::messages_api::wire;
use roundhouse_server::relay_handoff::{RELAY_STATE_VARS, RelayHandoff};

mod common;
// The harness itself is `common::e2e` (M11.2b review F1): recorder, bootstrap,
// fork probe and version probe are the same rig the codex sibling stands its
// client inside, and were a line-for-line copy of it until the two drifted.
use common::e2e::{
    Exchange, PROJECT, TOPHAM_PROFILE, USER, base_session, clean, fork_probe, topham_binary,
    topham_version,
};
// And the *claude* half of it is `common::claude_rig` (M12 review F11): the
// deployment, the topologies, and the child command — everything that stands a
// real client up, as opposed to what this file claims about one.
use common::claude_rig::{
    CANARY, CANARY_FILE, CHAINED_DEADLINE, CHILD_DEADLINE, ControlRace, RELAY_BIN_VAR, Rig,
    Topology, Wiring, build_child_command, claim_root, claude_argv, claude_version,
    interrupt_then_kill, relay_dry_run, relay_version,
};
use common::{Scripted, ScriptedTurns, ToolCallingFrontierClient};

// ---------------------------------------------------------------------------
// What this deployment is
// ---------------------------------------------------------------------------

/// What the scripted frontier answers an ordinary turn with.
///
/// Distinctive on purpose: an assertion that *this* text came out of `claude -p`
/// is an assertion that the turn was served by roundhouse's frontier path and
/// not by anything the client invented or cached.
const ANSWER: &str = "roundhouse answered this turn";

/// The prose the scripted upstream speaks before it calls a tool.
///
/// Separate from [`ANSWER`] so the tool test can tell the calling turn's text
/// from the turn that closes the loop — two identical strings would make "the
/// client came back" indistinguishable from "the client repeated itself".
const BEFORE_THE_CALL: &str = "Let me look at that file.";

/// The `tool_use.id` the scripted upstream emits for its control call.
///
/// The whole of R-M2's correlation runs through this one string: roundhouse
/// emits it, the log stores it as the call's `call_id`, the client quotes it
/// back on `_meta["claudecode/toolUseId"]`, and the control surface resolves it
/// to the conversation it was emitted into.
const CONTROL_CALL_ID: &str = "toolu_e2e_mcp_01";

/// One of roundhouse's own control tools, in both spellings a run of it needs.
struct ControlTool {
    /// What `roundhouse-mcp` serves it under, and what the client posts as
    /// `params.name` once it has split the flat name apart (§5.8, request 6).
    bare: &'static str,
    /// What a Claude Code model spells — `mcp__roundhouse__<tool>` — and
    /// therefore what `tools[]`, the `tool_use` block, `--allowedTools` and
    /// this deployment's own log all carry (R-M1).
    flat: String,
}

/// The control tool the closure run asks for.
///
/// **Neither spelling is written here.** The bare name is looked up in
/// `CONTROL_TOOL_NAMES` — the single definition of the eight, which
/// `roundhouse_mcp::tools::TOOL_NAMES` re-exports — so a rename in
/// `roundhouse-mcp` turns this red rather than shipping a run that asks a real
/// client for a tool this deployment does not serve. The flat one is
/// [`ClientDialect::claude_messages`]'s own rendering, which is also what the
/// generated `--mcp-config` registers and what the validate fold recognises: a
/// fourth hand-joined copy is how a registration stops matching the tool it
/// registers.
///
/// A `LazyLock` and not a `const` because the flat spelling is a `String`, and
/// a `&'static str` borrowed from a `static` is what [`Scripted::Call`] needs —
/// which is also why this file still contains no leak (see
/// [`the_tool_loop_double_is_shared_and_needs_no_leaks`]).
static CONTROL_TOOL: LazyLock<ControlTool> = LazyLock::new(|| {
    let bare = CONTROL_TOOL_NAMES
        .into_iter()
        .find(|name| *name == "status")
        .expect("roundhouse-mcp serves a `status` tool, and the signage points at it");
    ControlTool {
        bare,
        flat: ClientDialect::claude_messages().stored_call_name(bare),
    }
});

/// How long a single `claude -p` may take before the test kills it.

/// The header Relay's transparent-run credential rides on
/// (`provider_auth.rs`'s `TRANSPARENT_PROXY_CREDENTIAL_HEADER`).
///
/// Named here so the chained tests can assert its **absence** at roundhouse's
/// edge. Relay merges it into the client's `ANTHROPIC_CUSTOM_HEADERS` so the
/// client presents it to the gateway, and strips it again before dispatch
/// (Relay evidence §2.3) — so this is the one header whose arrival here
/// would mean Relay's own credential had been handed to an upstream that is not
/// Relay.
const RELAY_PROXY_TOKEN_HEADER: &str = "x-nemo-relay-proxy-token";

// ---------------------------------------------------------------------------
// The scripted upstream
// ---------------------------------------------------------------------------

/// A frontier that answers prose on every dispatch.
fn prose_upstream() -> Arc<ScriptedTurns> {
    Arc::new(ScriptedTurns::answering(ANSWER))
}

/// A frontier whose first turn speaks and then calls `Read` on `path`, then
/// prose.
///
/// `path` is owned, and that is the whole of M11.2b review F13's second half:
/// the file this rig asks the client to read only has a path at run time, and
/// while [`Scripted::Call`] held its arguments as `&'static str` the only way
/// to script it was `Box::leak`, at two sites, each with a paragraph arguing
/// that a bounded per-rig leak was acceptable. Widening the one field that
/// actually varies deleted both the leaks and both the paragraphs.
fn reading_upstream(path: &Path) -> Arc<ScriptedTurns> {
    Arc::new(ScriptedTurns::then_answering(
        vec![ToolCallingFrontierClient::new(
            vec![
                Scripted::Text(BEFORE_THE_CALL),
                Scripted::Call {
                    id: "toolu_e2e_01",
                    name: "Read",
                    arguments: serde_json::json!({ "file_path": path }).to_string(),
                },
            ],
            Some("tool_use"),
        )],
        ANSWER,
    ))
}

/// A frontier whose first turn speaks and then calls roundhouse's own `status`,
/// then prose.
///
/// The call names the **flat** spelling, because that is what a Claude Code
/// model has in front of it: the client flattens its MCP registration into
/// every tool it declares, and a `tool_use` naming anything else is one the
/// client answers with "no such tool" rather than dispatching. Its arguments
/// are the empty object — no `conversation` — which is the case R-M2 is about:
/// with nothing named, the surface has to work out which conversation the call
/// came from.
fn control_calling_upstream() -> Arc<ScriptedTurns> {
    Arc::new(ScriptedTurns::then_answering(
        vec![ToolCallingFrontierClient::new(
            vec![
                Scripted::Text(BEFORE_THE_CALL),
                Scripted::Call {
                    id: CONTROL_CALL_ID,
                    name: CONTROL_TOOL.flat.as_str(),
                    arguments: "{}".to_string(),
                },
            ],
            Some("tool_use"),
        )],
        ANSWER,
    ))
}

/// F13 (M11.2b review), now the guard on its own fix: the queue-then-prose
/// double lives in `tests/common` where every real-client tool-loop suite can
/// reach it, the identity wrapper that used to sit between it and
/// [`ToolCallingFrontierClient`] is gone, and no call site in this file leaks a
/// runtime-derived string to satisfy a `&'static str` field.
///
/// Structural, and read from this file's own source, because the defect was
/// structural: nothing about the rig it produced behaved differently, which is
/// exactly why it survived a milestone.
#[test]
fn the_tool_loop_double_is_shared_and_needs_no_leaks() {
    let this_file = include_str!("claude_e2e.rs");
    let common = include_str!("common/mod.rs");

    assert!(
        common.contains("pub struct ScriptedTurns"),
        "F13: the queue-then-prose double belongs in tests/common, where the next real-client \
         tool-loop suite can reach it rather than writing a second copy"
    );
    // Assembled at run time for the same reason the leak needle below is: a
    // literal spelling of the type this file must not contain would be found
    // in this assertion itself.
    let wrapper = format!("struct {}{}", "ScriptedTurn", " {");
    assert!(
        !this_file.contains(wrapper.as_str()),
        "F13: `ScriptedTurn` was an identity wrapper over ToolCallingFrontierClient — every use \
         of it was an immediate `.client` field access — and reintroducing it puts a type with \
         no behaviour of its own back between the suite and the double"
    );

    // The needle is assembled at run time rather than spelled as one literal:
    // `this_file` is this very file, so a literal here would match itself and
    // report a leak site that is only this assertion.
    let needle = format!("{}{}", "Box::leak", "(");
    let leaks = this_file.matches(needle.as_str()).count();
    assert_eq!(
        leaks, 0,
        "F13: {leaks} Box::leak site(s) remain in claude_e2e.rs; `Scripted::Call.arguments` is \
         owned now, so a script naming a path this rig only learns at run time needs no leak"
    );
}

/// **F23 (M11.3 review), now the guard on its own fix: `topham --version` must
/// name the commit it was built from.**
///
/// Every other real-binary probe in this file is checked against a
/// `VERIFIED_*` constant pinned to the version the suite's assertions were
/// written against (`VERIFIED_VERSION` for `claude`, `VERIFIED_RELAY_VERSION`
/// for `nemo-relay`) — a mismatch prints a loud warning naming the drift.
/// `topham` had nothing to compare, because clap's bare `version` renders
/// `CARGO_PKG_VERSION` alone: the workspace version, identical in every build
/// of every commit that does not touch `Cargo.toml`. The banner this suite
/// prints for every closure run could not distinguish today's `topham` from
/// one built last week, and the stale one is exactly the binary that reports
/// green for code nobody compiled.
///
/// A commit hash rather than a `VERIFIED_TOPHAM_VERSION` constant, because
/// `topham` is not a dependency this suite pins to a release — it is *this
/// tree*, and the only drift worth naming is "built somewhere else". The
/// identifier comes from `crates/topham/build.rs`; `topham_version`'s own
/// HEAD comparison is what turns it into the warning the other two probes
/// print, and this is the assertion that the identifier is there to compare at
/// all.
///
/// Compared against HEAD and not against the working tree: a build from
/// uncommitted changes still names the commit it was cut from, which is the
/// most a hash can say and enough to catch the checkout that never moved.
#[test]
#[ignore = "F23: needs the real topham binary: --features e2e-claude -- --include-ignored; \
            ROUNDHOUSE_TEST_TOPHAM_BIN names a built topham"]
fn topham_version_names_the_commit_it_was_built_from() {
    let topham = topham_binary();
    let version = topham_version(&topham);

    let commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git is available in this environment");
    assert!(commit.status.success(), "git rev-parse HEAD failed");
    let commit = String::from_utf8_lossy(&commit.stdout).trim().to_string();
    let short = &commit[..7.min(commit.len())];

    assert!(
        version.contains(short),
        "F23: `topham --version` prints {version:?}, which names no build identifier at all — \
         not even a short commit hash ({short}) for the tree it was built from. A binary built \
         from a stale checkout prints the exact same banner as one built from HEAD, so nothing \
         in this suite's own preflight (the version line `through_topham` already prints) can \
         tell the two apart."
    );
}

/// The text blocks of a resent message, in order.
fn text_blocks(message: &Value) -> Vec<String> {
    match &message["content"] {
        Value::String(text) => vec![text.clone()],
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// The session log as a failure message should print it: one line per item.
///
/// Never `{items:?}`. This client's turn configuration is a ~20 KB system prompt
/// that the surface re-appends per turn, so a debug dump of a three-turn log is
/// sixty screens of somebody else's prose with the one interesting line
/// somewhere inside it. Position, role and kind are what every assertion below
/// is actually about; the text is truncated because the assertions that care
/// about text name the substring they want in their own message.
fn log_shape(items: &[Item]) -> String {
    items
        .iter()
        .enumerate()
        .map(|(at, item)| {
            let (kind, sample) = match &item.content {
                ItemContent::Text { text } => ("text", text.clone()),
                ItemContent::ToolCall {
                    call_id,
                    name,
                    arguments,
                } => ("tool_call", format!("{name} {call_id} {arguments}")),
                ItemContent::ToolResult { call_id, output } => {
                    ("tool_result", format!("{call_id} {output}"))
                }
                ItemContent::Thinking { thinking, .. } => ("thinking", thinking.clone()),
                ItemContent::RedactedThinking { data } => ("redacted_thinking", data.clone()),
                ItemContent::Opaque { block_type, .. } => ("opaque", block_type.clone()),
            };
            let sample: String = sample.split_whitespace().collect::<Vec<_>>().join(" ");
            format!(
                "  #{at} {:?} {kind}: {}",
                item.role,
                sample.chars().take(72).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether this message is the trailing per-request budget notice (R-A).
///
/// Matched by role and by [`wire::is_budget_notice`] rather than by position:
/// the claim is about *which* message the client appends, and "the last one"
/// would be satisfied by any trailing message at all.
///
/// **The anchors are the drop rule's, not this suite's** (M11.2b review F5).
/// This function used to spell them itself, with a close anchor of `"tokens
/// left</total_tokens>"` — today's client wording rather than the tag — while
/// `wire::canonicalize` anchored on the tags alone. So a client that reworded
/// the number's caption would still be correctly dropped by the surface and
/// simultaneously reported here as "not the notice": a guard disagreeing with
/// the rule it exists to guard, which is worse than no guard because it would
/// have diagnosed a working drop as a failure to drop. Calling the exported
/// predicate leaves one spelling in the tree.
fn is_the_budget_notice(message: &Value) -> bool {
    message["role"] == "system"
        && text_blocks(message)
            .iter()
            .any(|text| wire::is_budget_notice(text))
}

/// F5 (M11.2b review), now the guard on its own fix: whatever
/// `wire::is_budget_notice` drops, this suite recognises — including a wording
/// this client does not use today.
///
/// The first assertion is the control: today's exact spelling must still be
/// recognised, so the second is not passing because the predicate became
/// vacuous. The second is the finding — a reworded notice, same tags, different
/// caption for the number — which `wire::canonicalize` drops (pinned directly
/// against the canonicaliser in `wire::tests::a_reworded_budget_notice_is_still_dropped`)
/// and which this suite's own copy of the recogniser used to miss.
#[test]
fn is_the_budget_notice_recognizes_any_wire_dropped_reword() {
    let today = serde_json::json!({
        "role": "system",
        "content": "<total_tokens>15000000 tokens left</total_tokens>",
    });
    assert!(
        is_the_budget_notice(&today),
        "control: today's exact client spelling must still be recognised"
    );

    let reworded = serde_json::json!({
        "role": "system",
        "content": "<total_tokens>1 remaining</total_tokens>",
    });
    assert!(
        is_the_budget_notice(&reworded),
        "F5: `wire::is_budget_notice` drops this text as the ephemeral notice (open \
         '<total_tokens>', close '</total_tokens>', no inner '<'), so this suite must call it \
         the notice too — a second, narrower spelling here is a guard that disagrees with the \
         rule it guards"
    );
}

// ---------------------------------------------------------------------------
// Guards that need no binary
// ---------------------------------------------------------------------------

/// The child's environment is the launcher's map plus the isolation set, and
/// nothing ambient.
///
/// The guard the module doc's isolation argument rests on, and it runs on every
/// `--features e2e-claude` compile: no real binary, no `--include-ignored`, no
/// `ROUNDHOUSE_TEST_CLAUDE_BIN`. Nothing on the wire can stand in for it. Under
/// `RoundhouseKey` an ambient `ANTHROPIC_AUTH_TOKEN` is resolved *ahead* of the
/// sentinel, and an ambient `CLAUDE_CODE_REMOTE=true` defeats the sentinel
/// altogether — the first swaps which credential is presented and the second
/// swaps it for a real subscription seat, and a run under either one still looks
/// perfectly healthy from the served side.
///
/// `==` on the key set rather than `contains`, because a *missing* entry is as
/// dangerous a drift as an extra one: without `CLAUDE_CONFIG_DIR` the child
/// resolves the developer's own settings, and without `ANTHROPIC_BASE_URL` the
/// SDK falls back to `api.anthropic.com` and bills somebody's account while
/// touching no part of this deployment.
#[test]
fn the_childs_environment_is_the_generated_map_plus_the_isolation_vars() {
    let turn_key = "rh_turn_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let env = ClaudeLaunch::new("http://127.0.0.1:9", turn_key)
        .expect("the documented-correct shape constructs")
        .env()
        .expect("the documented-correct shape renders");
    let root = PathBuf::from("/does/not/need/to/exist");
    let command = build_child_command(
        "claude",
        &Topology::Direct,
        &env,
        &root,
        "prompt",
        &[],
        false,
    );

    let envs: BTreeMap<String, Option<String>> = command
        .as_std()
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect();

    let generated: BTreeSet<&str> = env.names().collect();
    assert_eq!(
        generated,
        BTreeSet::from([BASE_URL_ENV, CUSTOM_HEADERS_ENV, API_KEY_ENV]),
        "the launcher's map is what this harness spawns with; a fourth variable in it is a \
         change to what a launch means and this suite must be read again before it is trusted"
    );
    let allowed: BTreeSet<&str> = generated
        .into_iter()
        .chain([
            "PATH",
            "HOME",
            "CLAUDE_CONFIG_DIR",
            "DISABLE_AUTOUPDATER",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
        ])
        .collect();
    let actual: BTreeSet<&str> = envs.keys().map(String::as_str).collect();
    assert_eq!(
        actual,
        allowed,
        "the child's constructed environment must carry exactly the generated map plus the \
         isolation set, got: {:?}",
        envs.keys().collect::<Vec<_>>()
    );

    // Named explicitly on top of the `==` above, because these are the specific
    // suspects: the first defeats the sentinel entirely (§5.7) and the rest
    // resolve ahead of it (§1.3). The set check already catches each as an extra
    // key; naming them is what makes a future reader's intent legible without
    // re-deriving it from a diff.
    for suspect in [
        "CLAUDE_CODE_REMOTE",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        assert!(
            !envs.contains_key(suspect),
            "an ambient credential input leaked into the child's environment: {suspect}"
        );
    }

    // And the one variable that does carry a secret carries it in the syntax the
    // client parses, with the turn key intact — the seam `ClaudeEnv::vars` exists
    // to be, checked at the point it is actually used.
    assert_eq!(
        envs["ANTHROPIC_CUSTOM_HEADERS"].as_deref(),
        Some(format!("{TURN_KEY_HEADER}: {turn_key}").as_str())
    );
    assert_eq!(
        envs["ANTHROPIC_API_KEY"].as_deref(),
        Some(ROUNDHOUSE_API_KEY_SENTINEL)
    );
}

/// **Names the limit of the guard above, as a fact about the standard library
/// rather than a claim about this file.**
///
/// [`the_childs_environment_is_the_generated_map_plus_the_isolation_vars`]
/// reads `Command::get_envs()` and was — until this was written — documented as
/// seeing an ambient leak from a dropped `env_clear()`. It does not: reproduced
/// directly against `std::process::Command` here, with no dependency on
/// [`build_child_command`] or [`ClaudeLaunch`] at all, so this stays true
/// regardless of anything this crate does. `get_envs()` reports the explicit
/// `env()`/`env_remove()` diff only, and that diff is byte-identical whether or
/// not `env_clear()` ran first — an ambient variable that was never named in an
/// explicit `env()` call is invisible to it either way. Concretely: deleting
/// `build_child_command`'s `command.env_clear()` call leaves
/// `the_childs_environment_is_the_generated_map_plus_the_isolation_vars` green
/// (confirmed by mutation), because the ambient `CLAUDE_CODE_REMOTE` it would
/// then leak was never something `.env()` added, so it was never in the diff to
/// begin with. Only [`the_seat_chain_a_launched_client_presents`], reading the
/// real wire under `--include-ignored`, would catch that mutation — which is
/// why guard 3 exists and is not merely a stronger restatement of guard 2.
#[test]
fn the_get_envs_diff_cannot_see_a_dropped_env_clear() {
    let without_clear = {
        let mut command = std::process::Command::new("does-not-need-to-exist");
        command.env("EXPLICIT", "1");
        // No env_clear() here — the mutation this test exists to name.
        command
    };
    let with_clear = {
        let mut command = std::process::Command::new("does-not-need-to-exist");
        command.env_clear();
        command.env("EXPLICIT", "1");
        command
    };

    let diff = |command: &std::process::Command| -> BTreeMap<String, Option<String>> {
        command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    };

    assert_eq!(
        diff(&without_clear),
        diff(&with_clear),
        "Command::get_envs() must be identical with and without env_clear() for the guard's \
         documented limit to be accurate — if this ever fails, std's contract changed and guard \
         2's doc comment (build_child_command) should be revisited rather than this test deleted"
    );
}

/// F18 (M11.2b review), now the guard on its own fix: the version probes are
/// isolated like every other process this file spawns.
///
/// They were not. `claude_version`/`relay_version` each built their command
/// inline — `Command::new(binary).arg("--version")`, no `env_clear()` — so the
/// child saw *this test process's own* `HOME`, no `CLAUDE_CONFIG_DIR`, and no
/// `DISABLE_AUTOUPDATER=1`. Inside this repository's own container that means
/// the probe ran `claude` against the real config directory with
/// `CLAUDE_CODE_REMOTE=true` still set — the exact ambient state the module doc
/// says every process here is cleared of. The cost of leaving it was not a wrong
/// version string; it was that the rule had an exception nobody could see from
/// the rule.
///
/// Proven against a stub "binary" — a shell script that ignores `--version` and
/// reports the three variables back — so this needs no refactor of the probe's
/// signature, no real binary and no `--include-ignored`.
///
/// **The three variables above do not, on their own, pin `env_clear()`** (its
/// own mutation-verifier, on this guard): `version_probe` sets all three with
/// an explicit `.env()` call regardless of whether `env_clear()` ran, so
/// deleting only `env_clear()` — the `.env()` overrides left in place — kept
/// this test green while leaving every *other* ambient variable free to reach
/// the child. The fourth assertion below is what only `env_clear()` can
/// satisfy: it plants a canary in this test process's own environment,
/// something `version_probe` is never told to isolate, and requires the stub
/// not see it. That costs one `std::env::set_var`, so this test now needs
/// `--test-threads=1` — already this module's documented invocation — for the
/// one line it writes to the ambient environment, restored immediately after
/// the probe returns.
#[test]
fn the_version_probe_isolates_the_child_it_spawns() {
    // A variable `version_probe` is never told to isolate. If `env_clear()` is
    // ever dropped (keeping the three `.env()` overrides below, exactly the
    // mutation the M11.2b mutation-verifier applied), `Command` inherits this
    // process's ambient environment by default and the stub echoes this straight
    // back; if `env_clear()` runs, the child never sees it at all.
    const AMBIENT_CANARY_VAR: &str = "ROUNDHOUSE_E2E_AMBIENT_CANARY";
    const AMBIENT_CANARY_VALUE: &str = "leaked";

    let dir = std::env::temp_dir().join(format!(
        "f18-version-probe-stub-{}-{}",
        std::process::id(),
        now_ms()
    ));
    std::fs::create_dir_all(&dir).expect("a scratch dir for the stub binary");
    let stub = dir.join("stub-version-probe.sh");
    std::fs::write(
        &stub,
        "#!/bin/sh\n\
         echo \"HOME=$HOME\"\n\
         echo \"CLAUDE_CONFIG_DIR=${CLAUDE_CONFIG_DIR:-__unset__}\"\n\
         echo \"DISABLE_AUTOUPDATER=${DISABLE_AUTOUPDATER:-__unset__}\"\n\
         echo \"ROUNDHOUSE_E2E_AMBIENT_CANARY=${ROUNDHOUSE_E2E_AMBIENT_CANARY:-__unset__}\"\n",
    )
    .expect("the stub script writes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("the stub script is made executable");
    }

    let ambient_home = std::env::var("HOME").expect(
        "this test needs an ambient HOME to prove the stub inherits it — every environment this \
         suite runs in, including this container, sets one",
    );

    // SAFETY: this test runs alone under `--test-threads=1` (this module's
    // documented invocation, "How to run it" above, and in practice the only
    // test this file's `the_version_probe` name filter matches), so no other
    // thread observes this process's environment while the canary is set.
    // set_var/remove_var are the only way to plant an ambient variable a
    // spawned child could inherit if version_probe's env_clear() were ever
    // dropped — the exact mutation this assertion exists to catch.
    unsafe {
        std::env::set_var(AMBIENT_CANARY_VAR, AMBIENT_CANARY_VALUE);
    }
    let output = claude_version(stub.to_str().expect("a scratch path that is valid UTF-8"));
    // SAFETY: see above.
    unsafe {
        std::env::remove_var(AMBIENT_CANARY_VAR);
    }
    let _ = std::fs::remove_dir_all(&dir);

    // `build_child_command`'s own rule (claude_e2e.rs's module doc, "Cleared
    // and rebuilt from the generated map plus a named isolation set, not
    // inherited"): an isolated HOME under the rig's root, an isolated
    // CLAUDE_CONFIG_DIR, and DISABLE_AUTOUPDATER=1. None of the three should
    // survive unset or ambient if claude_version followed that rule.
    assert!(
        !output.contains(&format!("HOME={ambient_home}")),
        "F18: claude_version spawned the stub with this test process's own ambient HOME \
         ({ambient_home}) rather than an isolated one — got:\n{output}"
    );
    assert_ne!(
        output
            .lines()
            .find(|line| line.starts_with("CLAUDE_CONFIG_DIR=")),
        Some("CLAUDE_CONFIG_DIR=__unset__"),
        "F18: claude_version left CLAUDE_CONFIG_DIR unset rather than isolating it — got:\n\
         {output}"
    );
    assert!(
        output.contains("DISABLE_AUTOUPDATER=1"),
        "F18: claude_version did not set DISABLE_AUTOUPDATER=1 on the child — got:\n{output}"
    );
    // The mutation-verifier's finding, made detectable: the three assertions
    // above pass whether or not `env_clear()` ran, because `version_probe`'s
    // explicit `.env()` calls set HOME/CLAUDE_CONFIG_DIR/DISABLE_AUTOUPDATER
    // either way. A variable neither `version_probe` nor its isolation set ever
    // names is the one thing only `env_clear()` can keep from the child.
    assert!(
        !output.contains(&format!("{AMBIENT_CANARY_VAR}={AMBIENT_CANARY_VALUE}")),
        "F18: claude_version's child echoed back {AMBIENT_CANARY_VAR}, a variable this test set \
         only in its own process and never passed to version_probe as isolation — that is only \
         possible if version_probe's env_clear() was dropped, since Command inherits the ambient \
         environment by default when it is not cleared. Got:\n{output}"
    );
}

/// **R-D′ made structural: the chained launch is the Direct launch, wrapped.**
///
/// The ruling is that one generator serves both topologies — Relay overwrites
/// `ANTHROPIC_BASE_URL` and merges into `ANTHROPIC_CUSTOM_HEADERS`, so the same
/// [`ClaudeEnv`] that hooks a client straight up also carries the turn key
/// through the hop. A test that only ran the chain end to end would confirm that
/// *a* turn key arrived without ever proving the two launches were the same
/// launch: a harness that quietly spelled the chained environment differently
/// would still be green, and the whole claim would be untested.
///
/// So this asserts the pair against each other, with no binary and no socket:
/// the client's argv is byte-identical across topologies, and the chained
/// environment is the Direct one plus exactly [`RELAY_STATE_VARS`]. It runs on
/// every `--features e2e-claude` compile, which is the other half of the point
/// — the chained tests below need two binaries and this needs none.
#[test]
fn the_chained_child_is_relay_wrapping_the_very_same_launch() {
    let turn_key = "rh_turn_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let env = ClaudeLaunch::new("http://127.0.0.1:9", turn_key)
        .expect("the documented-correct shape constructs")
        .env()
        .expect("the documented-correct shape renders");
    let root = PathBuf::from("/does/not/need/to/exist");
    let chained = Topology::Chained {
        relay: "nemo-relay".into(),
        config: root.join("relay-config.toml"),
        handoff: RelayHandoff::for_claude("http://127.0.0.1:9", "claude").expect("a handoff"),
    };

    let direct = build_child_command("claude", &Topology::Direct, &env, &root, "ask", &[], true);
    let through_relay = build_child_command("claude", &chained, &env, &root, "ask", &[], true);

    let argv = |command: &tokio::process::Command| -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    };
    let direct_argv = argv(&direct);
    let chained_argv = argv(&through_relay);

    // Relay's own argv comes first, and `--` is what makes the rest the
    // client's: `RunCommand::command` is `#[arg(last = true)]`, so a launch
    // missing the separator has clap reading `-p` as an unknown Relay flag and
    // failing before the gateway ever binds.
    assert_eq!(
        &chained_argv[..5],
        &[
            "run".to_string(),
            "--agent".into(),
            "claude".into(),
            "--config".into(),
            root.join("relay-config.toml")
                .to_string_lossy()
                .into_owned(),
        ],
        "the wizard-free entry point is `run --agent claude --config <toml>`; the bare `claude` \
         shortcut runs an interactive setup when no config layer exists, which in a test rig is a \
         hang"
    );
    assert_eq!(chained_argv[5], "--");
    assert_eq!(
        &chained_argv[6..],
        direct_argv.as_slice(),
        "the client's own argv must be identical on both topologies — a chained run that asked \
         the client to do something else would prove nothing about the hop"
    );

    let names = |command: &tokio::process::Command| -> BTreeSet<String> {
        command
            .as_std()
            .get_envs()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect()
    };
    let direct_names = names(&direct);
    let chained_names = names(&through_relay);
    assert_eq!(
        &chained_names - &direct_names,
        BTreeSet::from(RELAY_STATE_VARS.map(String::from)),
        "the chained environment is the Direct one plus Relay's own isolated state, and nothing \
         else: an extra variable here is a second way the two launches could differ, which is the \
         thing R-D′ rules out"
    );
    assert!(
        (&direct_names - &chained_names).is_empty(),
        "nothing may be dropped on the chained path either — Relay overwrites \
         `{BASE_URL_ENV}` and merges into `{CUSTOM_HEADERS_ENV}` itself, so the launcher's map is \
         handed over whole and Relay decides what survives"
    );
}

/// **R-T5's guard: the configuration this rig writes is the one `topham relay`
/// writes.**
///
/// The chained wiring used to be a `format!` inside [`Rig::wire_relay`], and the
/// launcher was going to need the same four lines. Two copies of a template
/// Relay parses with `deny_unknown_fields` is a drift whose failure lands on
/// whichever of the two nobody ran this week — and the one nobody runs is
/// exactly `topham relay`, which needs two binaries and a real deployment. So
/// the template moved into
/// [`relay_handoff`](roundhouse_server::relay_handoff) and both consume it.
///
/// What this test adds on top of that move is the thing the move alone cannot
/// give: **the literal bytes the rig used to write, pinned here.** With the rig
/// consuming the library, "the rig's config equals the library's rendering" is a
/// tautology; what is not a tautology is that the library still renders what a
/// real Relay 0.8.2 was observed accepting and echoing back. Change the
/// rendering — reorder the tables, add a header comment, rename a key — and this
/// goes red with the old bytes in the diff, which is the moment to re-run the
/// chained suite rather than to update the constant.
///
/// The codex half is pinned in `relay_handoff`'s own tests, where the
/// deployment-root-to-`openai_base_url` derivation lives; this file's business
/// is the agent it can actually spawn.
#[test]
fn the_rigs_relay_config_is_byte_identical_to_the_shared_rendering() {
    // Verbatim from `Rig::wire_relay`'s `format!` before R-T5 moved it, with
    // the two holes filled in by hand.
    let before_the_move = "[upstream]\nanthropic_base_url = \"http://127.0.0.1:4321\"\n\n\
                           [agents.claude]\ncommand = \"/opt/bin/claude\"\n";
    let handoff = RelayHandoff::for_claude("http://127.0.0.1:4321", "/opt/bin/claude")
        .expect("the shape the rig itself passes");
    assert_eq!(handoff.config_toml(), before_the_move);
}

/// The fork probe names a session a fork would actually create.
///
/// [`Rig::assert_never_forked`] is the only thing standing between this suite
/// and a silent fork, and it asserts the *absence* of a session id — an
/// assertion that passes trivially if the id it probes is one nothing ever
/// creates. Evaluated here without a rig, on both shapes it must handle: a
/// generation-zero id, where the probe is `#g1`; and an already-forked one,
/// where the probe must still be `#g1` rather than `#g1#g1`.
#[test]
fn the_fork_probe_names_a_session_a_fork_would_actually_create() {
    let base = SessionId::new("acme/ada/anthropic_messages/c0cb70b6-938b-4cbb-a8e8-1b8a60b7c4d8");
    assert_eq!(
        fork_probe(&base).as_str(),
        format!("{base}#g1"),
        "generation one of a fresh session is what a first fork writes"
    );
    let forked = SessionId::new(format!("{base}#g1"));
    assert_eq!(
        fork_probe(&forked),
        fork_probe(&base),
        "a probe derived from an already-forked id must name the same row, not `#g1#g1` — an id \
         nothing creates, whose absence would prove nothing"
    );
    assert_eq!(base_session(&forked), base);
}

/// A second rig can never silently share the first rig's root.
///
/// [`claim_root`]'s doc comment names the failure this closes: two `Rig`s
/// pointed at one root race `claude --continue`'s session resolution onto each
/// other. [`Rig::start`] can never trigger it — [`Rig::a_root_for`] mints a
/// fresh UUID every call — so this drives [`claim_root`] directly, the same
/// function [`Rig::start_at`] asserts on, rather than standing up two rigs to
/// exercise one directory-creation call.
#[test]
fn a_rigs_root_cannot_be_claimed_twice() {
    let root = std::env::temp_dir().join(format!(
        "roundhouse-claude-e2e-claim-probe-{}",
        uuid::Uuid::new_v4()
    ));
    assert!(
        claim_root(&root),
        "the first claim on a fresh root must succeed"
    );
    assert!(
        !claim_root(&root),
        "a second claim on the same root must fail — silently letting it succeed is exactly the \
         gap that let one rig's `--continue` extend another rig's conversation"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// A real `claude` binary, launched with nothing but
/// [`ClaudeLaunch`]'s environment, completes a turn through roundhouse.
///
/// The first thing this rung has to establish and deliberately ahead of the
/// rest: that the generated map is one a real client hooks up with. Every unit
/// test in `claude_launch` proves the map says the right thing — that the base
/// URL is a deployment root, that the header block parses the way §1.6 says the
/// client parses it — and none of them proves the client reads any of it. A
/// wrong answer here would otherwise surface two tests later as "the tool loop
/// did not close", and be diagnosed as a tool bug.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real claude binary: --features e2e-claude -- --include-ignored; ROUNDHOUSE_TEST_CLAUDE_BIN overrides PATH"]
async fn a_real_claude_binary_completes_a_prose_turn_through_roundhouse() {
    let rig = Rig::start("prose", prose_upstream()).await;

    let run = rig.print("Say the word alpha and stop.", &[]).await;
    run.assert_completed("the prose turn");

    // The client printed *our* answer. Nothing in the client can produce this
    // string, so this is the assertion that the turn was served by roundhouse's
    // frontier path rather than by anything cached or invented.
    assert_eq!(
        run.text(),
        ANSWER,
        "the client must have printed the answer this deployment served\n--- stdout\n{}",
        run.stdout
    );
    assert_eq!(run.turns(), 1, "a prose turn is one turn");

    // One request, admitted. A second would mean the client retried, which under
    // a scripted upstream that never fails means it did not like the first
    // answer — worth failing on rather than averaging over.
    let turns = rig.turns();
    assert_eq!(
        turns.len(),
        1,
        "one prose turn is one request; recorder saw:\n{}",
        rig.recorder.transcript()
    );
    assert_eq!(turns[0].status, 200, "the turn was refused: {:?}", turns[0]);
    assert_eq!(rig.upstream.dispatches(), 1, "one turn, one dispatch");

    // The session id the client named is inside the id roundhouse bound, and
    // qualified by the principal the minted key resolved to. Asserted as a
    // containment rather than as an equality because the qualification is the
    // deployment's and the UUID is the client's: spelling the whole id here
    // would be this test asserting `plane.qualify`'s format string.
    let session = rig.session();
    assert!(
        session
            .as_str()
            .starts_with(&format!("{PROJECT}/{USER}/{}", "anthropic_messages/")),
        "a Configured deployment namespaces its sessions by principal and by dialect, got \
         `{session}`"
    );
    assert!(
        session.as_str().ends_with(&run.session_id()),
        "the bound session must carry the client's own session id `{}`, got `{session}`",
        run.session_id()
    );

    // And roundhouse's own view: the prompt and the answer, in one log.
    let items = rig.items().await;
    let user_text = items
        .iter()
        .filter(|item| item.role == Role::User)
        .filter_map(|item| match &item.content {
            ItemContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        user_text.contains("alpha"),
        "the client's prompt must be in the session log, which holds:\n{user_text}"
    );
    assert!(
        items.iter().any(|item| item.role == Role::Assistant
            && item.content
                == ItemContent::Text {
                    text: ANSWER.into()
                }),
        "the answer must be committed as an assistant item, log holds:\n{}",
        log_shape(&items)
    );

    rig.clean();
}

/// **The M11.2a tool loop, closed by a real agent.**
///
/// M11.2a proved two of the three things this loop needs: that the surface emits
/// interleaved `tool_use` blocks a strict Messages reader accepts, and that a
/// `tool_result` resend *of the shape a client would send* is prefix-admitted
/// onto the same session. The third could not be reached from there — that a
/// real agent, handed our stream, runs the call and resends what our log
/// actually holds. A hand-built resend is authored by the same person as the
/// assertion; this one is authored by the client.
///
/// The evidence that it really ran is [`CANARY`]. The scripted upstream asks for
/// a `Read` of a file this rig wrote seconds earlier, inside the run's own
/// working directory, and the canary line comes back on the next request. No
/// prompt, no fixture and nothing in the client contains that string, so its
/// arrival is not something the test could have supplied.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real claude binary: --features e2e-claude -- --include-ignored; ROUNDHOUSE_TEST_CLAUDE_BIN overrides PATH"]
async fn a_tool_using_turn_is_run_by_the_real_client_and_its_result_rejoins_the_session() {
    // The script names the file before the rig exists: the scripted upstream has
    // to know what it will ask the client to read, and the rig writes that file
    // itself. The assertion below is what keeps the two from drifting apart.
    let root = Rig::a_root_for("tools");
    let canary = root.join("wd").join(CANARY_FILE);
    let rig = Rig::start_at(
        root,
        reading_upstream(&canary),
        Wiring::Direct,
        ControlRace::None,
    )
    .await;
    assert_eq!(
        rig.canary_path(),
        canary,
        "the script must name the file the rig actually wrote"
    );

    // `--allowedTools Read` and nothing else: the narrowest grant that lets a
    // print-mode run dispatch the one tool the script asks for. A run that
    // needed more would show up as a `permission_denials` entry, which
    // `assert_completed` prints.
    let run = rig
        .print(
            "Read the canary file and tell me what it says.",
            &["--allowedTools", "Read"],
        )
        .await;
    run.assert_completed("the tool-using turn");
    assert_eq!(
        run.turns(),
        2,
        "the client must have run the tool and come back for a second turn\n--- stdout\n{}",
        run.stdout
    );
    assert_eq!(
        run.text(),
        ANSWER,
        "the turn that closed the loop is the one this deployment answered with prose"
    );

    // Two requests, two dispatches: the call, and the turn that followed the
    // tool result. The counts are asserted together because they fail
    // differently — two requests and one dispatch would mean roundhouse refused
    // or short-circuited the resend.
    let turns = rig.turns();
    assert_eq!(
        turns.len(),
        2,
        "a tool call and its result are two requests; recorder saw:\n{}",
        rig.recorder.transcript()
    );
    assert_eq!(rig.upstream.dispatches(), 2);
    assert!(turns.iter().all(|turn| turn.status == 200));

    // The first response really did carry a `tool_use` block and said so in
    // `stop_reason` — the two halves the client reads to decide it is being asked
    // to act rather than being told the turn is over.
    let frames = turns[0].frames();
    assert!(
        frames.iter().any(|frame| {
            frame["type"] == "content_block_start" && frame["content_block"]["type"] == "tool_use"
        }),
        "the first turn's stream must have opened a tool_use block: {frames:?}"
    );
    assert!(
        frames
            .iter()
            .any(|frame| frame["type"] == "message_delta"
                && frame["delta"]["stop_reason"] == "tool_use"),
        "the first turn must have stopped for a tool call: {frames:?}"
    );

    // The client's resend: the assistant message it rebuilt from our stream, and
    // the `tool_result` it produced by actually running `Read`.
    let resent = turns[1].messages();
    let blocks: Vec<&Value> = resent
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .collect();
    let call = blocks
        .iter()
        .find(|block| block["type"] == "tool_use")
        .unwrap_or_else(|| panic!("the resend must carry the call it was answering: {resent:?}"));
    assert_eq!(call["id"], "toolu_e2e_01");
    assert_eq!(call["name"], "Read");
    assert_eq!(
        call["input"]["file_path"],
        Value::from(canary.to_string_lossy().as_ref()),
        "the client must have been asked to read the file this rig wrote"
    );
    let result = blocks
        .iter()
        .find(|block| block["type"] == "tool_result")
        .unwrap_or_else(|| panic!("the resend must carry the tool's output: {resent:?}"));
    assert_eq!(result["tool_use_id"], "toolu_e2e_01");
    assert!(
        serde_json::to_string(result)
            .expect("a recorded block re-serializes")
            .contains(CANARY),
        "the tool result must carry what the file holds — this is the evidence the real client \
         ran the real tool: {result:?}"
    );

    // And the resend was *admitted*, not forked: one session holding the call,
    // its result and the prose that followed, in order.
    rig.assert_never_forked().await;
    let items = rig.items().await;
    assert!(
        items.iter().any(|item| matches!(
            &item.content,
            ItemContent::ToolCall { call_id, name, .. }
                if call_id == "toolu_e2e_01" && name == "Read"
        )),
        "the log must hold the call this deployment emitted:\n{}",
        log_shape(&items)
    );
    assert!(
        items.iter().any(|item| matches!(
            &item.content,
            ItemContent::ToolResult { call_id, output }
                if call_id == "toolu_e2e_01" && output.contains(CANARY)
        )),
        "the log must hold the result the client ran, joined to the call by id:\n{}",
        log_shape(&items)
    );
    // The order the next turn's prefix check depends on: the call is committed
    // before its result, and the closing prose after both.
    let positions = |wanted: fn(&ItemContent) -> bool| {
        items
            .iter()
            .position(|item| wanted(&item.content))
            .unwrap_or_else(|| {
                panic!(
                    "the log is missing an item this test needs:\n{}",
                    log_shape(&items)
                )
            })
    };
    let call_at = positions(|content| matches!(content, ItemContent::ToolCall { .. }));
    let result_at = positions(|content| matches!(content, ItemContent::ToolResult { .. }));
    let answer_at = positions(
        |content| matches!(content, ItemContent::Text { text } if text.as_str() == ANSWER),
    );
    assert!(
        call_at < result_at && result_at < answer_at,
        "call, result, answer — in that order; got {call_at}, {result_at}, {answer_at} in:\n{}",
        log_shape(&items)
    );

    rig.clean();
}

/// **R-A, against the real client rather than against a fixture — and exactly
/// as far as a real client can carry it.**
///
/// Three runs, because two do not reach the shape the rule has to handle. At
/// 2.1.257 a `--continue` appends a trailing `role: "system"` message holding
/// nothing but `<total_tokens>N tokens left</total_tokens>` after the new user
/// turn, with the cache breakpoint on it; one turn later the *same* notice comes
/// back flattened to a bare string and a fresh one is appended behind the new
/// question. Those are two different arms of `is_ephemeral_client_notice` — the
/// list container and the bare string — and a two-run test exercises only the
/// first.
///
/// **What this pins, and it is the load-bearing half:** the notice really is on
/// the wire, in both containers, appended rather than accumulated — so the drop
/// rule guards a shape this client actually sends — and it never becomes a log
/// item. Reverting the drop turns the last assertion below red.
///
/// **What this deliberately does not pin, stated rather than implied:** the
/// *fork* R-A prevents. That failure needs `N` to move between the turn that
/// stored the notice and the turn that resends it, and this client's `N` does
/// not move over a run of this rig — all three requests of the committed
/// `claude-2.1.257-turn-{1,2,3}` captures carry the same `15000000`, and so do
/// this suite's, because the counter is coarse and the scripted answers are
/// tiny. So a session forked by a moved counter is not producible here at any
/// length worth spawning, and the claim is pinned where it can be: over a
/// synthesized moved-`N` resend in `wire`'s own suite
/// (`the_clients_remaining_budget_notice_never_becomes_an_item`). Asserting the
/// fork here anyway would be an assertion that passes for a reason unrelated to
/// the rule — the exact tautology the negative below replaces.
///
/// The no-fork assertion is kept, and it is not vacuous: it is the general
/// prefix-admission claim over a real client's resend, which the client's own
/// system reminders, its `metadata`, and every byte of a 64 KB body it rebuilds
/// each launch all have to survive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real claude binary: --features e2e-claude -- --include-ignored; ROUNDHOUSE_TEST_CLAUDE_BIN overrides PATH"]
async fn a_continued_run_extends_the_session_rather_than_forking_it() {
    let rig = Rig::start("continue", prose_upstream()).await;

    let first = rig.print("Say the word alpha and stop.", &[]).await;
    first.assert_completed("the first run");
    let second = rig.continued("Now say the word beta and stop.").await;
    second.assert_completed("the second run");
    let third = rig.continued("Now say the word gamma and stop.").await;
    third.assert_completed("the third run");

    // The client's own view: one conversation across three processes.
    assert_eq!(
        [first.session_id(), second.session_id(), third.session_id()]
            .into_iter()
            .collect::<BTreeSet<_>>()
            .len(),
        1,
        "`--continue` must continue the conversation the first run started"
    );

    let turns = rig.turns();
    assert_eq!(
        turns.len(),
        3,
        "three runs are three turns; recorder saw:\n{}",
        rig.recorder.transcript()
    );

    // The notice is real, and it is *trailing* — after the new user turn, which
    // is what makes it unrepresentable as turn configuration (a configuration
    // run is the leading run by construction) and therefore what makes dropping
    // it the only rule that leaves one shape per position.
    for (index, turn) in turns.iter().enumerate().skip(1) {
        let resent = turn.messages();
        let last = resent.last().expect("a continued turn resends its history");
        assert!(
            is_the_budget_notice(last),
            "2.1.257 appends the remaining-budget notice after the new user turn; if this client \
             no longer does, R-A's drop rule is guarding a shape nobody sends and the ruling must \
             be read again (turn {index}): {last:?}"
        );
    }
    // Turn three carries two: the one it is appending now, and turn two's, back
    // in the flattened container. Both must be recognised, and the second is the
    // arm a two-run test never reaches.
    let third_body = turns[2].messages();
    assert_eq!(
        third_body
            .iter()
            .filter(|message| is_the_budget_notice(message))
            .count(),
        2,
        "one notice per turn, appended rather than accumulated: {third_body:?}"
    );
    assert!(
        third_body
            .iter()
            .any(|message| is_the_budget_notice(message) && message["content"].is_string()),
        "the superseded notice comes back as a bare string, which is the container arm this run \
         exists to reach: {third_body:?}"
    );

    // And it changed nothing. One session, every prompt in it, no generation one
    // anywhere.
    assert_eq!(
        rig.conversations
            .latest(&rig.principal())
            .expect("all three runs bound a session"),
        rig.session()
    );
    rig.assert_never_forked().await;
    let items = rig.items().await;
    let user_text = items
        .iter()
        .filter(|item| item.role == Role::User)
        .filter_map(|item| match &item.content {
            ItemContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    for word in ["alpha", "beta", "gamma"] {
        assert!(
            user_text.contains(word),
            "every run's prompt must be in one session log, which holds:\n{user_text}"
        );
    }
    // And the notice itself is not in it. The negative that carries R-A here:
    // reverting the drop in `wire::canonicalize` turns exactly this red, while
    // every other assertion in this test stays green.
    //
    // The failure prints the offending items' *positions and openings*, never
    // the log: this session's turn configuration is a 20 KB system prompt
    // re-appended per turn, and a message that dumps every item buries the one
    // fact a reader needs under sixty screens of somebody else's prose.
    let admitted: Vec<String> = items
        .iter()
        .enumerate()
        .filter_map(|(at, item)| match &item.content {
            ItemContent::Text { text } if wire::is_budget_notice(text) => {
                Some(format!("#{at} {:?} {:?}", item.role, text.trim()))
            }
            _ => None,
        })
        .collect();
    assert!(
        admitted.is_empty(),
        "the ephemeral notice must never become a log item, but {} of {} items are one:\n{}",
        admitted.len(),
        items.len(),
        admitted.join("\n")
    );

    rig.clean();
}

/// `M11-SEAT-EVIDENCE`: which header a launched client presented what in.
///
/// Printed, and asserted only on its shape — what a reader should conclude about
/// a credential chain from one capture is not a thing a fixture should decide.
/// The shape is nonetheless four real claims, and the last two are the ones no
/// other test in this crate can make:
///
/// - the turn key arrived in [`TURN_KEY_HEADER`], which is what makes this a
///   pass-through-shaped request whose `Authorization` would belong to the
///   client's own upstream;
/// - the sentinel arrived on `x-api-key`, exactly where §1.3's suppression puts
///   it — so the serve side's rule that it is never captured as a seat
///   (`control_config`'s `the_launchers_api_key_sentinel_is_never_forwarded_as_a_seat`)
///   is a rule about a value that really does arrive;
/// - there is **no** `Authorization` header at all. That is the negative §1.3
///   predicts for a non-interactive launch with a resolved API key, and it is
///   the only way this suite can say anything about the forwarded-login arm it
///   cannot drive: the prediction is that the arm which suppresses nothing is
///   the arm where a bearer appears here;
/// - and no remote-container header. Their presence would mean the environment
///   clear leaked `CLAUDE_CODE_REMOTE`, the sentinel suppressed nothing, and the
///   `Authorization` above is a real managed OAuth token that has just been
///   recorded by a test rig (§5.7). That is the single most likely way a real
///   credential reaches this file, so it is checked on the wire and not only at
///   construction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real claude binary: --features e2e-claude -- --include-ignored; ROUNDHOUSE_TEST_CLAUDE_BIN overrides PATH"]
async fn the_seat_chain_a_launched_client_presents() {
    let rig = Rig::start("seat", prose_upstream()).await;
    let run = rig.print("Say the word alpha and stop.", &[]).await;
    run.assert_completed("the seat-evidence turn");

    let turns = rig.turns();
    let turn = turns.first().expect("the client sent a turn");

    println!("--- M11-SEAT-EVIDENCE");
    println!("    auth kind     : RoundhouseKey");
    println!("    request       : POST {}", turn.path);
    for (name, value) in turn.redacted_headers() {
        println!("    {name}: {value}");
    }
    println!(
        "    forwarded-login arm: not driven here — no `claude` login exists on this box and \
         none may be created for a test rig. §1.3 predicts a subscription bearer on \
         `authorization` beside the turn key on `{TURN_KEY_HEADER}`; the negative below is \
         what makes that prediction checkable."
    );

    assert_eq!(
        turn.header(TURN_KEY_HEADER),
        Some(rig.secret.as_str()),
        "the turn key must arrive in the dedicated header the generated `{CUSTOM_HEADERS_ENV}` \
         names: {:?}",
        turn.redacted_headers()
    );
    assert_eq!(
        turn.header("x-api-key"),
        Some(ROUNDHOUSE_API_KEY_SENTINEL),
        "the sentinel must arrive where §1.3 puts a resolved API key: {:?}",
        turn.redacted_headers()
    );
    assert_eq!(
        turn.header("authorization"),
        None,
        "a launch that resolved the sentinel must present no bearer; one here is either an \
         ambient login the environment clear failed to exclude or a managed token \
         `CLAUDE_CODE_REMOTE` re-enabled — in both cases a real credential reached a test rig: \
         {:?}",
        turn.redacted_headers()
    );
    for (name, _) in turn.headers.iter() {
        assert!(
            !name.contains("remote"),
            "a remote-container header (`{name}`) means the environment clear leaked \
             CLAUDE_CODE_REMOTE and this run is contaminated (§5.7): {:?}",
            turn.redacted_headers()
        );
    }

    rig.clean();
}

// ---------------------------------------------------------------------------
// The chained topology
// ---------------------------------------------------------------------------

/// **R-D / R-D′: the same launch, through a real NeMo Relay.**
///
/// Everything above this line is the Direct topology, which R-D makes the
/// reference. This is the other one an operator actually deploys — `nemo-relay
/// run --agent claude`, Relay's own loopback gateway between the client and
/// roundhouse — and it is supported only with the guards below instantiated,
/// because the hop is not transparent: Relay rewrites the request body through
/// an alphabetizing `serde_json::Map`, re-encodes the SSE stream, concatenates
/// the path onto a configured base URL, and injects a credential of its own.
///
/// What this closes that no unit test can: the four Relay-side hazards R7 named
/// are each pinned by a unit guard over a *synthesized* Relay artefact, and a
/// synthesized artefact is a claim about Relay written by us. Here Relay writes
/// it.
///
/// **Credential attribution is the first assertion and the reason for the
/// order.** Three keys exist on this path — roundhouse's turn key, the launch
/// sentinel, and Relay's own `x-nemo-relay-proxy-token` — and the question a
/// chained deployment has to answer is which of them went upstream. R-D′ rules
/// that the carrier is the client's own environment: Relay overwrites
/// `ANTHROPIC_BASE_URL` and *merges* into `ANTHROPIC_CUSTOM_HEADERS`
/// (Relay evidence §A.7), forwards unknown request headers
/// untouched (Relay evidence §2.3), and strips only its own credential
/// — so the turn key arrives on [`TURN_KEY_HEADER`] exactly as it does Direct,
/// and a chained turn is dedicated-header authed with the same semantics.
///
/// The unit guards this deliberately does not duplicate, cited instead:
/// `wire`'s Relay-alphabetized-resend test (hazard 1) and `emit`'s
/// no-`data:`-frame rule (hazard 2) both assert properties of *our* code that a
/// single end-to-end run could only sample.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real claude and nemo-relay binaries: --features e2e-claude -- --include-ignored; ROUNDHOUSE_TEST_CLAUDE_BIN and ROUNDHOUSE_TEST_RELAY_BIN override PATH"]
async fn a_chained_turn_reaches_roundhouse_with_the_turn_key_and_not_relays_own() {
    let rig = Rig::start_chained("chained", prose_upstream()).await;

    let run = rig.print("Say the word alpha and stop.", &[]).await;
    run.assert_completed("the chained prose turn");

    // The client printed our answer, which means the SSE stream survived Relay's
    // re-encoder end to end (R7 hazard 2's other half: the unit rule is that our
    // emitter never depends on a frame's `id:` line, and this is the run that
    // shows a re-encoded stream is still one this client can read).
    assert_eq!(
        run.text(),
        ANSWER,
        "the answer this deployment served must have reached the client through Relay's SSE \
         re-encoder\n--- stdout\n{}\n--- stderr\n{}",
        run.stdout,
        run.stderr
    );

    let turns = rig.turns();
    assert_eq!(
        turns.len(),
        1,
        "one chained prose turn is one request at roundhouse's edge; recorder saw:\n{}",
        rig.recorder.transcript()
    );
    let turn = &turns[0];
    assert_eq!(turn.status, 200, "the chained turn was refused: {turn:?}");

    println!("--- M11-SEAT-EVIDENCE (chained)");
    println!("    auth kind     : RoundhouseKey, via nemo-relay run --agent claude");
    println!(
        "    request       : POST {}{}",
        turn.path,
        turn.query
            .as_deref()
            .map(|query| format!("?{query}"))
            .unwrap_or_default()
    );
    for (name, value) in turn.redacted_headers() {
        println!("    {name}: {value}");
    }

    // 1. The turn key survived the hop, on the dedicated header. This is R-D′'s
    //    whole ruling: if it were missing, the chained carrier would have to be
    //    Relay's `[upstream] anthropic_auth_header` instead — the fallback the
    //    chained runbook documents — and this deployment would be answering 401.
    assert_eq!(
        turn.header(TURN_KEY_HEADER),
        Some(rig.secret.as_str()),
        "Relay merges rather than replaces `{CUSTOM_HEADERS_ENV}`, so the turn key must arrive on \
         `{TURN_KEY_HEADER}` exactly as it does Direct: {:?}",
        turn.redacted_headers()
    );

    // 2. It arrived *through Relay*, and this is what stops every negative
    //    below from being vacuous: a run in which the client had somehow
    //    reached roundhouse directly would show no proxy token either, and
    //    would be green for the wrong reason. 0.8.2 stamps its own attribution
    //    on every dispatched request — `x-nemo-relay-source: gateway` plus the
    //    scope/turn ids beside it in the block above — and none of those names
    //    appears on a Direct turn.
    assert_eq!(
        turn.header("x-nemo-relay-source"),
        Some("gateway"),
        "this request must have come through Relay's gateway; without that the assertions below \
         are about a Direct turn wearing a chained test's name: {:?}",
        turn.redacted_headers()
    );

    // 3. Relay's own credential did **not** arrive. It reaches Relay's gateway (Relay
    //    puts it in the client's custom headers to authenticate the transparent
    //    run) and is stripped before dispatch by `should_forward_request_header`;
    //    finding it here would mean a proxy credential had been handed to an
    //    upstream that is not the proxy.
    assert_eq!(
        turn.header(RELAY_PROXY_TOKEN_HEADER),
        None,
        "Relay's transparent-run credential must never leave its own gateway: {:?}",
        turn.redacted_headers()
    );

    // 4. `?beta=true` survived. R7 hazard 3, and until this run it was an
    //    argument from reading Relay's base-plus-path-and-query concatenation
    //    rather than an observation. It matters because the query is what
    //    selects the beta route this surface serves.
    assert_eq!(
        turn.query.as_deref(),
        Some("beta=true"),
        "the client's query string must survive Relay's base-URL concatenation \
         (Relay evidence §A.4); path was `{}`",
        turn.path
    );

    // 5. The sentinel arrived where §1.3 puts a resolved API key, and was not
    //    captured as a seat. Both halves: a chained deployment forwarding
    //    `rh_sentinel_not_a_credential` upstream as a tenant's own key is a 401
    //    three processes from its cause.
    assert_eq!(
        turn.header("x-api-key"),
        Some(ROUNDHOUSE_API_KEY_SENTINEL),
        "the launch sentinel rides through Relay untouched — it is an unknown header to Relay's \
         forwarding filter, not a credential it owns: {:?}",
        turn.redacted_headers()
    );
    assert!(
        !rig.upstream.any_credential_forwarded(),
        "the sentinel must never be captured as a forwarded seat; a chained turn that forwarded \
         one would be presenting a value that authenticates nothing to a real frontier"
    );

    // And one accounting log for the whole chain: one dispatch, one session,
    // holding the client's prompt and this deployment's answer.
    assert_eq!(
        rig.upstream.dispatches(),
        1,
        "one chained turn, one dispatch"
    );
    let items = rig.items().await;
    let user_text = items
        .iter()
        .filter(|item| item.role == Role::User)
        .filter_map(|item| match &item.content {
            ItemContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        user_text.contains("alpha"),
        "the chained prompt must be in the session log, which holds:\n{user_text}"
    );
    assert!(
        items.iter().any(|item| item.role == Role::Assistant
            && item.content
                == ItemContent::Text {
                    text: ANSWER.into()
                }),
        "the answer must be committed once, to one session:\n{}",
        log_shape(&items)
    );

    rig.clean();
}

/// **R7 hazard 1, against the real re-encoder: a `--continue` through Relay does
/// not fork the session.**
///
/// The hazard is that Relay deserializes a rewritten request body into a
/// `serde_json::Map` and re-serializes it — and both Relay crates declare
/// `serde_json` with no `preserve_order`, so that `Map` is `BTreeMap`-backed and
/// alphabetizes every object it round-trips (§A.2). A prefix check that hashed
/// rendered JSON would see turn two's resend of turn one as a different history
/// and rebind the conversation, silently, on the second turn of every chained
/// session.
///
/// The unit guard for that is `wire`'s Relay-alphabetized-resend test, which
/// feeds `canonicalize` a hand-alphabetized body: it is precise about the
/// mechanism and is nonetheless a claim about Relay written by us. This is the
/// run where Relay writes it — and it is the only place the two hazards compose,
/// since a resend also has to survive the client's own re-serialization on top
/// of Relay's.
///
/// What is asserted is what M11.1's F2/F3 prefix work promises: one session, its
/// generation-zero id, both prompts inside it. The `--continue` notice R-A drops
/// is covered by the Direct test; repeating it here would be asserting `wire`
/// twice rather than asserting the hop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real claude and nemo-relay binaries: --features e2e-claude -- --include-ignored; ROUNDHOUSE_TEST_CLAUDE_BIN and ROUNDHOUSE_TEST_RELAY_BIN override PATH"]
async fn a_chained_continue_survives_relays_re_encode_without_forking() {
    let rig = Rig::start_chained("chained-continue", prose_upstream()).await;

    let first = rig.print("Say the word alpha and stop.", &[]).await;
    first.assert_completed("the first chained run");
    let second = rig.continued("Now say the word beta and stop.").await;
    second.assert_completed("the chained --continue");

    // The client's own view first: two processes, one conversation. Without
    // this the assertion below would pass for a run in which `--continue`
    // silently started over, which is a client-side failure wearing a
    // roundhouse-side disguise.
    assert_eq!(
        first.session_id(),
        second.session_id(),
        "`--continue` must continue the conversation the first chained run started"
    );

    let turns = rig.turns();
    assert_eq!(
        turns.len(),
        2,
        "two chained runs are two requests; recorder saw:\n{}",
        rig.recorder.transcript()
    );
    assert!(turns.iter().all(|turn| turn.status == 200));
    assert!(
        turns
            .iter()
            .all(|turn| turn.header(TURN_KEY_HEADER) == Some(rig.secret.as_str())),
        "every chained turn carries the turn key, not only the first"
    );

    // The resend really did come back through the re-encoder with a full
    // history: two requests where the second is longer than the first is what
    // makes the prefix check below a check on something.
    assert!(
        turns[1].messages().len() > turns[0].messages().len(),
        "the continued turn must resend the first turn's history; got {} then {} messages",
        turns[0].messages().len(),
        turns[1].messages().len()
    );

    // And roundhouse admitted it as a prefix rather than rebinding: one
    // generation-zero session holding both prompts.
    rig.assert_never_forked().await;
    assert_eq!(rig.upstream.dispatches(), 2);
    let items = rig.items().await;
    let user_text = items
        .iter()
        .filter(|item| item.role == Role::User)
        .filter_map(|item| match &item.content {
            ItemContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    for word in ["alpha", "beta"] {
        assert!(
            user_text.contains(word),
            "both chained prompts must be in one session log, which holds:\n{user_text}"
        );
    }

    rig.clean();
}

// ---------------------------------------------------------------------------
// The closure: an operator's launcher, driving the real client
// ---------------------------------------------------------------------------

/// **A real `claude`, launched by a real `topham`, from a profile a person
/// wrote.**
///
/// Every other test in this file hands the client [`ClaudeEnv`] directly, which
/// proves the generated map is one a client hooks up with and leaves one link
/// unproven: that anything an operator can actually *run* produces that map.
/// Until M11.3 nothing did — both README deferrals said so in the same words,
/// "no CLI subcommand or admin route produces these files" — so the launcher's
/// own suite could only prove `topham` builds the map the generator builds,
/// which is a claim about two functions in one process.
///
/// This is the run that closes it. The child is `topham launch e2e`, handed a
/// turn key, two homes and a `PATH` and **no `ANTHROPIC_*` variable at all**;
/// what arrives at roundhouse's edge is asserted to be exactly what the Direct
/// test asserts. A launcher that resolved the profile wrongly cannot pass this
/// by inheriting anything, because there is nothing to inherit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real claude and topham binaries: --features e2e-claude -- --include-ignored; ROUNDHOUSE_TEST_CLAUDE_BIN overrides PATH and ROUNDHOUSE_TEST_TOPHAM_BIN names a built topham"]
async fn a_real_client_launched_through_topham_hooks_up_on_direct() {
    let rig = Rig::start("topham-direct", prose_upstream()).await;
    let profile = rig.write_profile("direct");
    println!("    profile       : {}", profile.display());

    let run = rig
        .through_topham(
            &["launch", TOPHAM_PROFILE],
            "Say the word alpha and stop.",
            &[],
            CHILD_DEADLINE,
        )
        .await;
    run.assert_completed("the prose turn launched through topham");

    // The client printed *our* answer, so the launcher resolved a base URL that
    // reached this deployment and a credential this deployment admitted.
    // Nothing in `claude` or in `topham` can produce this string.
    assert_eq!(
        run.text(),
        ANSWER,
        "a client launched through `topham launch` must have printed the answer this deployment \
         served\n--- stdout\n{}\n--- stderr\n{}",
        run.stdout,
        run.stderr
    );

    let turns = rig.turns();
    assert_eq!(
        turns.len(),
        1,
        "one launched prose turn is one request; recorder saw:\n{}",
        rig.recorder.transcript()
    );
    let turn = &turns[0];
    assert_eq!(turn.status, 200, "the launched turn was refused: {turn:?}");

    println!("--- M11-SEAT-EVIDENCE (topham launch, Direct)");
    for (name, value) in turn.redacted_headers() {
        println!("    {name}: {value}");
    }

    // 1. The turn key arrived on the dedicated header, which is the whole of
    //    what a `RoundhouseKey` profile promises: `topham` read it from the
    //    variable the *profile named* and the generator put it in the header
    //    block, without either of them ever writing it to a file.
    assert_eq!(
        turn.header(TURN_KEY_HEADER),
        Some(rig.secret.as_str()),
        "the launcher must carry the profile's `key-env` value onto `{TURN_KEY_HEADER}`: {:?}",
        turn.redacted_headers()
    );

    // 2. The sentinel is inert: it arrived where §1.3 puts a resolved API key,
    //    suppressing any subscription login, and this deployment did not treat
    //    it as a seat. A launcher that forwarded it as a tenant's own key would
    //    be a 401 two processes from its cause.
    assert_eq!(
        turn.header("x-api-key"),
        Some(ROUNDHOUSE_API_KEY_SENTINEL),
        "the launch sentinel must arrive verbatim: {:?}",
        turn.redacted_headers()
    );
    assert!(
        !rig.upstream.any_credential_forwarded(),
        "the sentinel must never be captured as a forwarded seat"
    );

    // 3. And no `Authorization` at all, which is the negative the whole
    //    isolation story rests on: the child's environment was cleared, so a
    //    bearer here would mean this box's ambient login had reached a
    //    deployment through a launcher that is supposed to have replaced it.
    assert_eq!(
        turn.header("authorization"),
        None,
        "a `RoundhouseKey` launch presents no bearer: {:?}",
        turn.redacted_headers()
    );

    // And roundhouse's own view, so the run is one accounted turn and not a
    // client that answered from somewhere else.
    assert_eq!(rig.upstream.dispatches(), 1, "one turn, one dispatch");
    let items = rig.items().await;
    assert!(
        items.iter().any(|item| item.role == Role::Assistant
            && item.content
                == ItemContent::Text {
                    text: ANSWER.into()
                }),
        "the answer must be committed as an assistant item, log holds:\n{}",
        log_shape(&items)
    );

    rig.clean();
}

/// **R-M6, the closure: a real client, launched through a real `topham`,
/// reaches roundhouse's own control surface from inside a turn — and the
/// answer it gets back is about the conversation it asked from.**
///
/// Every rung under this one proved a piece of it against something that was
/// not the whole: `mcp_surface` drives the tools with no client, `messages_api`
/// drives the flat-name round trip with no socket, `claude_launch` proves the
/// registration with nothing to register against, and the topham closure tests
/// above prove a launch that has no control surface in it at all. This is the
/// one run where a real `claude`, a real `topham` and both of this
/// deployment's routers are in the same process tree, and it asserts at both
/// edges:
///
/// 1. **The client hooked up to `/mcp` at all** — one `tools/call`, carrying
///    the turn key on [`TURN_KEY_HEADER`], from a child whose environment holds
///    no `ANTHROPIC_*` variable and whose argv the launcher wrote. Nothing here
///    registers the server: `topham` does, from the profile, as inline argv.
/// 2. **The name was split back apart on the MCP wire** — the model saw
///    `mcp__roundhouse__status` and the surface was asked for `status`, which
///    is §5.8's request 6 observed live rather than replayed from a fixture.
/// 3. **The call was correlated by tool-use id and not guessed** (R-M2). This
///    is the assertion the whole [`ControlRace`] apparatus exists for: a rival
///    conversation of the same principal's holds the `latest` slot when the
///    call is served, so an answer naming the client's own conversation can
///    only have come from `_meta["claudecode/toolUseId"]`. The rival is a real
///    session with a real log, so the failure this rules out is not a refusal —
///    it is a green answer about the wrong conversation.
/// 4. **The resend rejoined the session** — prefix-admitted, no `#g1`, with
///    the client's own 64 KB of rebuilt history and a `tool_result` in it.
/// 5. **The log holds one flat-named call and its result** (R-M1), and
/// 6. **the validate fold counts none of it as the agent's work** (R-M0/G04).
///
/// **What it deliberately does not re-prove.** The counterfactual — the same
/// call *without* the id resolving to the guess — is pinned at the seam by
/// `mcp_api::tests::a_tool_use_id_resolves_the_conversation_that_emitted_it`,
/// where it costs no process. Spawning a Node runtime to re-derive a branch a
/// unit test already owns would make this run slower without making it say
/// anything new.
///
/// Nor does it prove [`ClaudeLaunch::leading_argv`]'s signage reached the
/// child at all (M12 fix-stage F2): the scripted upstream answers a `tool_use`
/// it is told to emit regardless of what the model read, on purpose — a
/// closure test whose pass/fail depended on a live model choosing to call a
/// tool because it read a system prompt would not be deterministic. That claim
/// is pinned where it can be, at the argv itself:
/// `claude_launch::tests::the_generated_argv_is_two_flags_and_a_third_only_when_the_profile_asks`
/// and `topham::plan::tests::a_claude_bring_your_own_key_launch_renders` /
/// `a_claude_forwarded_login_launch_renders`. Do not read this test's green as
/// evidence signage is present in what topham execs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real claude and topham binaries: --features e2e-claude -- --include-ignored; ROUNDHOUSE_TEST_CLAUDE_BIN overrides PATH and ROUNDHOUSE_TEST_TOPHAM_BIN names a built topham"]
async fn a_real_client_reaches_the_control_surface_through_the_turn() {
    let rig = Rig::start_at(
        Rig::a_root_for("topham-control"),
        control_calling_upstream(),
        Wiring::Direct,
        ControlRace::RivalIsLatest,
    )
    .await;
    let profile = rig.write_profile("direct");
    println!("    profile       : {}", profile.display());
    println!("    control tool  : {}", CONTROL_TOOL.flat);
    println!("    rival session : {}", rig.rival().session);

    let run = rig
        .through_topham(
            &["launch", TOPHAM_PROFILE],
            "Ask roundhouse for its status, then tell me what it said.",
            // The one flag the operator owes, and the reason it is here rather
            // than in the launcher: headless, this client synthesises a
            // permission refusal for an `mcp__*` tool its own argv does not
            // name — no `tools/call` reaches the surface at all — and
            // `--dangerously-skip-permissions` is refused outright on a box
            // running as root (§5.8). `topham plan` says as much in its notes;
            // what it cannot do is decide a permission grant on an operator's
            // behalf.
            &["--allowedTools", &CONTROL_TOOL.flat],
            CHILD_DEADLINE,
        )
        .await;
    run.assert_completed("the control-tool turn launched through topham");
    assert_eq!(
        run.turns(),
        2,
        "the client must have dispatched the control call and come back for a second turn\n\
         --- stdout\n{}",
        run.stdout
    );
    assert_eq!(
        run.text(),
        ANSWER,
        "the turn that closed the loop is the one this deployment answered with prose"
    );

    let turns = rig.turns();
    assert_eq!(
        turns.len(),
        2,
        "a control call and its result are two turns; the deployment saw:\n{}",
        rig.recorder.transcript()
    );
    assert!(turns.iter().all(|turn| turn.status == 200));
    assert_eq!(rig.upstream.dispatches(), 2, "two turns, two dispatches");

    // ---- edge one: the MCP request, as it arrived -------------------------
    let calls = rig.control_calls();
    assert_eq!(
        calls.len(),
        1,
        "the client must have dispatched exactly one control call; the deployment saw:\n{}",
        rig.recorder.transcript()
    );
    let call = &calls[0];
    assert_eq!(
        call.status, 200,
        "the control call was refused: {:?}",
        call.response
    );
    println!("--- M12-CONTROL-EVIDENCE (topham launch, Direct)");
    for (name, value) in call.redacted_headers() {
        println!("    {name}: {value}");
    }
    // The turn key, on the header the *generated registration* named — read by
    // the client out of the environment `topham` laid, expanded from the
    // `${…}` the registration carries. No file, and no key in any argv.
    assert_eq!(
        call.header(TURN_KEY_HEADER),
        Some(rig.secret.as_str()),
        "the control call must carry the profile's `key-env` value on \
         `{TURN_KEY_HEADER}`: {:?}",
        call.redacted_headers()
    );

    let body = call
        .body
        .as_ref()
        .unwrap_or_else(|| panic!("a `tools/call` has a JSON body: {call:?}"));
    assert_eq!(
        body["params"]["name"].as_str(),
        Some(CONTROL_TOOL.bare),
        "the model spelled `{}` and the client posts the bare tool, the server prefix stripped \
         (§5.8, request 6): {body}",
        CONTROL_TOOL.flat
    );
    assert_eq!(
        body["params"]["_meta"]["claudecode/toolUseId"].as_str(),
        Some(CONTROL_CALL_ID),
        "the client must quote back the `tool_use.id` this deployment emitted — the whole of \
         R-M2's correlation rides on this one key, and rmcp hands it to a tool through the \
         request *context* rather than the typed params: {body}"
    );

    // ---- edge two: which conversation answered ----------------------------
    //
    // Named from `latest` rather than from the call table: the resend rebound
    // the conversation after the control call was served, so by now `latest` is
    // the client's own again — which is exactly why the rival had to record
    // what it displaced at the moment the call arrived.
    let session = rig.session();
    let rival = rig.rival();
    assert_ne!(
        session, rival.session,
        "the rival must be a different conversation, or there is no race to win"
    );
    assert_eq!(
        rival.displaced_before("tools/call"),
        Some(session.clone()),
        "the rival must have taken the `latest` slot *from the client's own conversation* \
         immediately before the control call was served; without that, the assertion below \
         holds whether or not the tool-use id was read at all"
    );

    let served = call
        .response
        .as_ref()
        .unwrap_or_else(|| panic!("the surface answered one JSON-RPC document: {call:?}"));
    assert_eq!(
        served["result"]["isError"],
        Value::from(false),
        "the control tool refused: {served}"
    );
    let answer: Value = serde_json::from_str(
        served["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("`status` answers in one text block: {served}")),
    )
    .expect("every control tool renders its answer as JSON inside that block");
    assert_eq!(
        answer["conversation"].as_str(),
        Some(session.as_str()),
        "the call was correlated by tool-use id: `{}` held this principal's `latest` slot when \
         this was served, so a surface that guessed would have answered — green — about the \
         wrong conversation: {answer}",
        rival.session
    );

    // ---- edge three: the log the resend rejoined --------------------------
    rig.assert_never_forked().await;
    let items = rig.items().await;
    assert!(
        items.iter().any(|item| matches!(
            &item.content,
            ItemContent::ToolCall { call_id, name, .. }
                if call_id == CONTROL_CALL_ID && name == &CONTROL_TOOL.flat
        )),
        "R-M1: the log holds the call under the flat name the client spells, whole:\n{}",
        log_shape(&items)
    );
    let output = items
        .iter()
        .find_map(|item| match &item.content {
            ItemContent::ToolResult { call_id, output } if call_id == CONTROL_CALL_ID => {
                Some(output.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "the resend must have carried what the control surface answered:\n{}",
                log_shape(&items)
            )
        });
    assert!(
        output.contains(session.as_str()),
        "the stored result is the answer this conversation was given: {output}"
    );
    assert!(
        !output.contains(rival.session.as_str()),
        "and it is not the rival's: {output}"
    );

    // ---- edge four: what the validate loop makes of it --------------------
    //
    // R-M0/G04. Every signal the trigger computes runs over the task view, so
    // a control call inside it is roundhouse's own chatter counted as the
    // agent's work — which is how a session that did nothing wrong buys a judge
    // side-call and a steer.
    let folded = exchanges(&items);
    assert_eq!(
        folded.len(),
        1,
        "one call, one result, one exchange: {folded:?}"
    );
    assert!(
        is_control_call_on(&folded[0].name, ControlCallDialect::ClaudeMessages),
        "the fold must recognise `{}` as roundhouse's own control traffic",
        folded[0].name
    );
    assert!(
        task_exchanges_on(&folded, ControlCallDialect::ClaudeMessages).is_empty(),
        "the agent asked roundhouse for its own status; the task view must hold nothing: {:?}",
        task_exchanges_on(&folded, ControlCallDialect::ClaudeMessages)
    );
    // And the dialect is what did the excluding, not the name. The *Responses*
    // recogniser reads a bare `status` and this call arrived flat, so the same
    // fold under that surface keeps it — which is why a session's surface has
    // to reach the fold rather than being guessed at it (M12 review, F8).
    assert_eq!(
        task_exchanges_on(&folded, ControlCallDialect::CodexResponses).len(),
        1,
        "the flat spelling is the Messages surface's; the Responses recogniser          must not claim it"
    );

    rig.clean();
}

/// The client's own MCP configuration form, read strictly.
///
/// **A model of what `claude` accepts, not of what roundhouse emits**, which is
/// the whole point of it: `deny_unknown_fields` and a `type`-tagged enum mean
/// this parse fails on exactly the documents the client would misread — an
/// extra key it ignores, or a shape it resolves to a different transport. The
/// three variants are the ones the config form offers; only one of them is a
/// server this deployment can be.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpConfigDocument {
    #[serde(rename = "mcpServers")]
    servers: BTreeMap<String, McpServer>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
// The two variants below are never constructed and their fields are never read,
// and that is what they are for: a `type` this deployment does not serve has to
// be *parseable* for the match on `Http` to be a discrimination rather than the
// only thing the reader can express. Deleting them would leave a reader that
// accepts one shape and calls every other one malformed, which is the opposite
// of the client's behaviour.
#[allow(dead_code)]
enum McpServer {
    /// Streamable HTTP — JSON-RPC over `POST <url>`, which is what the 2.1.257
    /// capture recorded against a server registered exactly this way (§5.8).
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    /// The older server-sent-events transport, which this deployment does not
    /// serve. Modelled so that a registration drifting onto it fails here
    /// rather than as a client that cannot open a stream.
    Sse {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    /// A child process. Modelled for the same reason: a document with a
    /// `command` and no `url` parses perfectly and registers a server that is
    /// not this deployment at all.
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
}

/// **The closure test's premise, proved without a binary: what the launcher
/// hands the client is a document the client's own config reader resolves to
/// this deployment's HTTP control surface, and to nothing else.**
///
/// `claude_launch`'s own suite already pins the registration as a string and
/// checks its fields against the capture. What that cannot say — because it
/// reads the document the way *we* wrote it, field by field off a
/// `serde_json::Value` — is the thing a client does: resolve it to a
/// *transport*. A registration that grew a `command` key, or lost its `type`,
/// or spelled `url` under a name only our own reader knew, would pass a
/// field-by-field check and produce a client that starts, runs every turn
/// perfectly, and has no control tools at all.
///
/// So this parses it strictly, and the assertions are about what the parse
/// resolved to rather than about which characters are in it.
#[test]
fn the_generated_mcp_registration_resolves_to_this_deployments_http_surface() {
    const ROOT: &str = "http://127.0.0.1:8080";
    let launch = ClaudeLaunch::new(ROOT, "rh_turn_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .expect("a deployment root with no API prefix and a key-shaped key");
    let registration = launch.mcp_registration();

    let document: McpConfigDocument = serde_json::from_str(&registration).unwrap_or_else(|error| {
        panic!("the client reads this document strictly and would not: {registration}\n{error}")
    });
    assert_eq!(
        document.servers.keys().collect::<Vec<_>>(),
        vec![mcp_server_name()],
        "one server, under the name that makes its tools come back as \
         `{}__<tool>` — the namespace the validate fold recognises and the \
         signage spells: {registration}",
        launch.mcp_url()
    );

    let Some(McpServer::Http { url, headers }) = document.servers.get(mcp_server_name()) else {
        panic!(
            "the registration must resolve to the Streamable-HTTP transport this deployment \
             serves; it resolved to {:?}",
            document.servers.get(mcp_server_name())
        );
    };
    assert_eq!(
        url,
        &launch.mcp_url(),
        "the url the client will post to and `mcp_url` are one derivation"
    );
    assert_eq!(
        url,
        &format!("{ROOT}{MCP_MOUNT_PATH}"),
        "and that derivation is this deployment's own mount, at the root beside the turn route"
    );
    assert_eq!(
        headers,
        &BTreeMap::from([(
            TURN_KEY_HEADER.to_string(),
            format!("${{{DEFAULT_KEY_ENV}}}")
        )]),
        "one header, and its value is the variable rather than the key: the client expands it \
         out of the environment the same launch laid, so no rendering of this document — a \
         process listing, a `topham plan`, a shell history — can hold the secret"
    );

    // And it survives being written down. The flag takes the JSON inline, but
    // the same document is what a project `.mcp.json` holds (§5.8), so a
    // rendering that only round-tripped in memory would be one an operator
    // could not save.
    let rewritten = serde_json::to_string(&serde_json::from_str::<Value>(&registration).unwrap())
        .expect("the document re-serializes");
    let reread: McpConfigDocument =
        serde_json::from_str(&rewritten).expect("and re-reads as the same form");
    assert_eq!(
        reread.servers.keys().collect::<Vec<_>>(),
        document.servers.keys().collect::<Vec<_>>()
    );
}

/// **F3 (M11.3 review), now the guard on its own fix: a settings-file `env`
/// block that would re-route the launch is refused, by name, before anything
/// spawns.**
///
/// Claude Code applies a settings file's `env` block by *replacing* the value
/// it inherited, so a `CLAUDE_CONFIG_DIR/settings.json` left behind by
/// something else — nemo-relay's persistent install writes exactly this,
/// `env.ANTHROPIC_BASE_URL` pointed at its gateway
/// (`agent-docs/research/nemo-relay-0.8.0-published-read.md`'s Finding 2.2) —
/// outranks the environment `topham` generated. When this was first run live
/// the launch went through and the client hung: zero turns at roundhouse's
/// edge, no refusal anywhere, and no way for an operator to tell a re-routed
/// session from a slow one.
///
/// What is asserted now is the refusal `plan.rs` makes, read the way an
/// operator meets it: a non-zero exit, and a message naming *the file to edit*
/// and *the key in it*. A refusal that said only "this launch is unsafe" would
/// leave them hunting three search paths, which is why the path is asserted
/// and not merely the failure.
///
/// The recorder is checked too, and for something the exit status cannot say:
/// that the refusal happened **instead of** a launch and not after one. A
/// `topham` that spawned the client and then failed would exit non-zero with a
/// turn already served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real claude and topham binaries: --features e2e-claude -- --include-ignored; ROUNDHOUSE_TEST_CLAUDE_BIN overrides PATH and ROUNDHOUSE_TEST_TOPHAM_BIN names a built topham"]
async fn f3_settings_file_env_block_overriding_topham_generated_base_url() {
    let rig = Rig::start("topham-settings-env", prose_upstream()).await;
    let profile = rig.write_profile("direct");
    println!("    profile       : {}", profile.display());

    // Planted exactly the way nemo-relay's persistent install leaves one
    // behind: `env.ANTHROPIC_BASE_URL` pointed at an address nothing serves.
    // The address is deliberately dead — if the refusal ever regressed, this
    // test fails on the assertions below rather than on a turn that quietly
    // went somewhere real.
    let settings_path = rig.root.join("home/.claude").join("settings.json");
    std::fs::write(
        &settings_path,
        serde_json::json!({ "env": { "ANTHROPIC_BASE_URL": "http://127.0.0.1:1" } }).to_string(),
    )
    .expect("the settings file this test plants");

    let run = rig
        .through_topham(
            &["launch", TOPHAM_PROFILE],
            "Say the word alpha and stop.",
            &[],
            CHILD_DEADLINE,
        )
        .await;

    let refusal = format!("{}\n{}", run.stdout, run.stderr);
    assert!(
        !run.success
            && refusal.contains(&settings_path.display().to_string())
            && refusal.contains("env.ANTHROPIC_BASE_URL"),
        "F3: a settings-file `env.ANTHROPIC_BASE_URL` must be refused before the launch, naming \
         the file ({}) and the key; `topham launch` exited ok: {}\n--- stdout\n{}\n--- stderr\n{}",
        settings_path.display(),
        run.success,
        run.stdout,
        run.stderr
    );

    let turns = rig.turns();
    assert!(
        turns.is_empty(),
        "F3: the refusal must stand *instead of* the launch — roundhouse's edge recorded {} \
         turn(s), so a client was started before or despite it. Recorder:\n{}",
        turns.len(),
        rig.recorder.transcript()
    );

    rig.clean();
}

/// **F3's negative control: an empty settings file, same isolation, same
/// profile.**
///
/// Rules out the reading of the sibling that would make the fix worthless: that
/// `topham` refuses whenever a `CLAUDE_CONFIG_DIR/settings.json` exists at all,
/// rather than when its `env` block names something that defeats the launch. A
/// launcher that refused on presence would pass the sibling and be unusable —
/// most operators have a settings file. This one turn reaching roundhouse the
/// way the plain Direct closure test's does is what makes the sibling's refusal
/// specifically about the `env` block's content.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "F3 control: needs the real claude and topham binaries: --features e2e-claude -- --include-ignored; ROUNDHOUSE_TEST_CLAUDE_BIN overrides PATH and ROUNDHOUSE_TEST_TOPHAM_BIN names a built topham"]
async fn f3_control_empty_settings_file_does_not_break_the_direct_launch() {
    let rig = Rig::start("topham-settings-env-control", prose_upstream()).await;
    let profile = rig.write_profile("direct");
    println!("    profile       : {}", profile.display());

    let settings_path = rig.root.join("home/.claude").join("settings.json");
    std::fs::write(&settings_path, serde_json::json!({}).to_string())
        .expect("the empty settings file this control plants");

    let run = rig
        .through_topham(
            &["launch", TOPHAM_PROFILE],
            "Say the word alpha and stop.",
            &[],
            CHILD_DEADLINE,
        )
        .await;
    run.assert_completed("the prose turn launched through topham with an empty settings file");

    let turns = rig.turns();
    assert_eq!(
        turns.len(),
        1,
        "F3 control: an empty settings file must not itself change what reaches roundhouse; \
         recorder:\n{}",
        rig.recorder.transcript()
    );

    rig.clean();
}

/// **The chained half: `topham relay` writes the wiring, runs the preflight,
/// and hands the same launch to a real Relay.**
///
/// The rig's own chained tests write the Relay config themselves (through
/// [`RelayHandoff`], the shared rendering) and spawn Relay directly. That proves
/// the *rendering* is one Relay accepts; it does not prove an operator can
/// produce it. Here the config is written by `topham relay` in the profile's
/// own scratch, the F8 preflight is `topham`'s, and the exec is `topham`'s — and
/// the assertions are the chained test's own, so what is being compared is two
/// ways of reaching the identical wire.
///
/// The rig is deliberately built **Direct** ([`Rig::start`]) even though the
/// profile is chained: [`Topology`] decides what *the rig* spawns, and the rig
/// spawns nothing here. Constructing a chained rig would write a second Relay
/// config that this run never uses, and would leave two answers in the tree to
/// "who wired this chain".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real claude, nemo-relay and topham binaries: --features e2e-claude -- --include-ignored; ROUNDHOUSE_TEST_CLAUDE_BIN and ROUNDHOUSE_TEST_RELAY_BIN override PATH, ROUNDHOUSE_TEST_TOPHAM_BIN names a built topham"]
async fn a_real_client_handed_to_relay_through_topham_hooks_up_chained() {
    let rig = Rig::start("topham-chained", prose_upstream()).await;
    let profile = rig.write_profile("chained");
    println!("    profile       : {}", profile.display());

    // The Relay binary is named here rather than resolved through `PATH` by the
    // launcher, for the reason `Rig::wire_relay` names it: a chained test that
    // ran whatever `nemo-relay` the box has is a test whose version banner is a
    // guess. `--relay` is the flag `topham` offers for exactly this.
    let relay = std::env::var(RELAY_BIN_VAR).unwrap_or_else(|_| {
        panic!(
            "the chained closure test needs a real Relay binary: set {RELAY_BIN_VAR}, or run \
             without --include-ignored"
        )
    });
    println!("    relay binary  : {relay}");
    println!("    relay version : {}", relay_version(&relay));

    let run = rig
        .through_topham(
            &["relay", TOPHAM_PROFILE, "--relay", &relay],
            "Say the word alpha and stop.",
            &[],
            CHAINED_DEADLINE,
        )
        .await;
    run.assert_completed("the chained prose turn handed over by topham");

    assert_eq!(
        run.text(),
        ANSWER,
        "the answer must reach the client through the Relay `topham` wired\n--- stdout\n{}\n\
         --- stderr\n{}",
        run.stdout,
        run.stderr
    );

    let turns = rig.turns();
    assert_eq!(
        turns.len(),
        1,
        "one chained prose turn is one request at roundhouse's edge; recorder saw:\n{}",
        rig.recorder.transcript()
    );
    let turn = &turns[0];
    assert_eq!(turn.status, 200, "the chained turn was refused: {turn:?}");

    println!("--- M11-SEAT-EVIDENCE (topham relay, Chained)");
    for (name, value) in turn.redacted_headers() {
        println!("    {name}: {value}");
    }

    // The proof of hop, first, because every negative below is vacuous without
    // it: a run in which the client had somehow reached roundhouse directly
    // would show no proxy token either, and would be green for the wrong
    // reason. Relay stamps this on every request it dispatches, and it never
    // appears on a Direct turn.
    assert_eq!(
        turn.header("x-nemo-relay-source"),
        Some("gateway"),
        "this request must have come through the Relay `topham relay` started: {:?}",
        turn.redacted_headers()
    );

    // R-D′ through the launcher: one generated environment serves both
    // topologies, so the turn key arrives on the dedicated header exactly as it
    // does Direct.
    assert_eq!(
        turn.header(TURN_KEY_HEADER),
        Some(rig.secret.as_str()),
        "the launcher's map survives the hop: {:?}",
        turn.redacted_headers()
    );
    assert_eq!(
        turn.header(RELAY_PROXY_TOKEN_HEADER),
        None,
        "Relay's transparent-run credential must never leave its own gateway: {:?}",
        turn.redacted_headers()
    );
    assert_eq!(
        turn.header("x-api-key"),
        Some(ROUNDHOUSE_API_KEY_SENTINEL),
        "the launch sentinel rides through Relay untouched: {:?}",
        turn.redacted_headers()
    );
    assert_eq!(
        turn.query.as_deref(),
        Some("beta=true"),
        "the client's query string must survive Relay's base-URL concatenation; path was `{}`",
        turn.path
    );
    assert!(
        !rig.upstream.any_credential_forwarded(),
        "the sentinel must never be captured as a forwarded seat"
    );

    // The config the launcher wrote, where R-T2 says it goes: under the
    // profile's own scratch in `XDG_DATA_HOME`, not in the operator's
    // configuration directory and not beside the rig's.
    let scratch = rig
        .xdg("data")
        .join("topham")
        .join(TOPHAM_PROFILE)
        .join("relay");
    let written = std::fs::read_to_string(scratch.join("relay-config.toml"))
        .expect("`topham relay` writes its config into the profile's scratch");
    assert_eq!(
        written,
        RelayHandoff::for_claude(&rig.base_url, "claude")
            .expect("the rig's own root is the correct shape")
            .config_toml(),
        "the launcher must write the shared rendering and not a copy of it"
    );

    assert_eq!(
        rig.upstream.dispatches(),
        1,
        "one chained turn, one dispatch"
    );
    rig.clean();
}

// ---------------------------------------------------------------------------
// Hazard 4, made detectable
// ---------------------------------------------------------------------------

/// **Hazard 4 is a documented refusal *and* now a guard.**
///
/// `claude_launch`'s module doc calls hazard 4 "a documented refusal, not a
/// guard we can enforce — the layering happens inside Relay's process". That
/// is true of driving it end to end through a real launch, which is why
/// nothing above spawns `claude` to reach it. But `replace_upstream_base_url`'s
/// clearing is observable directly: `nemo-relay run --dry-run` prints what it
/// resolved without ever spawning the agent, so this needs Relay and nothing
/// else — no `claude` binary, no roundhouse process, no
/// `ROUNDHOUSE_TEST_CLAUDE_BIN`.
///
/// Two controls bracket the one case that matters, so a failure here can only
/// mean one thing: the auth header alone in the config file resolves as
/// configured, and a *second* layer supplying the *same* base URL must not
/// clear it either — isolating that it is specifically a base URL arriving
/// from a **different** layer that trips `replace_upstream_base_url`, exactly
/// as the module doc's citation of the Relay evidence's §A.5 predicts.
///
/// The probe spawns Relay with the ambient environment **cleared** (M11.2b
/// review F7): Relay applies its `NEMO_RELAY_*` layer above the explicit
/// `--config`, so an operator with `NEMO_RELAY_ANTHROPIC_BASE_URL` set would
/// have watched the first control assertion here fail and read it as Relay
/// drift. [`f7_an_ambient_base_url_env_var_cannot_trip_the_hazard_4_control`]
/// is the guard on that.
#[test]
#[ignore = "needs the real nemo-relay binary: --features e2e-claude -- --include-ignored; ROUNDHOUSE_TEST_RELAY_BIN overrides PATH"]
fn hazard_4_a_different_base_url_layer_clears_the_configured_auth_header() {
    let relay = std::env::var(RELAY_BIN_VAR).unwrap_or_else(|_| {
        panic!(
            "this guard needs a real Relay binary: set {RELAY_BIN_VAR}, or run without \
             --include-ignored"
        )
    });
    println!("    relay version : {}", relay_version(&relay));

    let root = std::env::temp_dir().join(format!("roundhouse-hazard4-{}", uuid::Uuid::new_v4()));
    assert!(claim_root(&root), "the probe's scratch root must be fresh");
    let config = root.join("config.toml");
    std::fs::write(
        &config,
        "[upstream]\n\
         anthropic_base_url = \"http://127.0.0.1:9999\"\n\
         anthropic_auth_header = \"Bearer probe-turn-key-value\"\n",
    )
    .expect("the probe's config.toml");

    // `--dry-run` prints `anthropic_auth = configured|unset` and exits without
    // spawning the agent — the report [`ClaudeAuthKind`]'s own doc points at.
    // Through [`relay_dry_run`], which clears the ambient environment: hazard 4
    // *is* a layering claim, and a probe that let the operator's own
    // `NEMO_RELAY_*` variables layer in would be one more layer than the
    // experiment has (M11.2b review F7).
    let dry_run = |second_layer_base_url: Option<&str>| -> String {
        let extra: Vec<&str> = match second_layer_base_url {
            Some(url) => vec!["--anthropic-base-url", url],
            None => Vec::new(),
        };
        relay_dry_run(&relay, &root, &config, &extra)
    };

    assert!(
        dry_run(None).contains("anthropic_auth = configured"),
        "control: the header alone in config.toml must resolve as configured"
    );
    assert!(
        dry_run(Some("http://127.0.0.1:9999")).contains("anthropic_auth = configured"),
        "control: a second layer supplying the *same* base URL must not clear the header — \
         otherwise this guard would prove nothing about layering specifically"
    );
    assert!(
        dry_run(Some("http://127.0.0.1:8888")).contains("anthropic_auth = unset"),
        "hazard 4: a base URL supplied by a *different* layer silently cleared the configured \
         auth header. If this ever fails because the printed line changed shape, re-read the \
         diff before touching the assertion — a Relay upgrade that changed this behaviour is \
         exactly the drift CLAUDE.md's synergy-vigilance rule exists to catch, and \
         `claude_launch`'s hazard-4 runbook would need a matching addendum."
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// **F7 (M11.2b review), now the guard on its own fix: an ambient
/// `NEMO_RELAY_ANTHROPIC_BASE_URL` cannot reach the hazard-4 probe.**
///
/// F7 was that
/// [`hazard_4_a_different_base_url_layer_clears_the_configured_auth_header`]
/// built its `--dry-run` child with `std::process::Command::new` and no
/// `env_clear()`, so Relay's environment layer applied on top of the `--config`
/// file even for the `dry_run(None)` *control* — and an operator with
/// `NEMO_RELAY_ANTHROPIC_BASE_URL` set to anything other than the config's
/// `127.0.0.1:9999` would watch that control fail and diagnose it as Relay
/// drift rather than as the probe leaking its own environment. Reproduced
/// exactly that way before the fix: the control panicked with "the header alone
/// in config.toml must resolve as configured".
///
/// The guard is the same reproduction, read the other way round. Run the *real*
/// hazard-4 function — a `#[test] fn` is still an ordinary callable fn — with
/// the variable set in this process's environment, and require that it now
/// completes. `relay_dry_run`'s `env_clear()` is what makes it complete, so
/// removing that call turns this red on the same assertion the finding named.
#[test]
#[ignore = "needs the real nemo-relay binary: --features e2e-claude -- --include-ignored; ROUNDHOUSE_TEST_RELAY_BIN overrides PATH"]
fn f7_an_ambient_base_url_env_var_cannot_trip_the_hazard_4_control() {
    // Confirm the prerequisite up front so a missing binary fails with the
    // same clear message hazard-4 itself would give, rather than surfacing
    // only inside the caught panic below.
    let relay = std::env::var(RELAY_BIN_VAR).unwrap_or_else(|_| {
        panic!(
            "this guard needs a real Relay binary: set {RELAY_BIN_VAR}, or run without \
             --include-ignored"
        )
    });
    println!("    relay version : {}", relay_version(&relay));

    // Capture the panic message by formatted text, not by downcasting the
    // payload: assert!'s panic payload type is a std internal that does not
    // reliably downcast to `&str`/`String` across panic-machinery versions,
    // but every panic hook is handed the fully formatted message regardless.
    let captured: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let captured_in_hook = captured.clone();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        *captured_in_hook.lock().unwrap() = Some(info.to_string());
    }));

    // SAFETY: this test runs with --test-threads=1 (required for the whole
    // hazard-4/F7 pair, which share ambient process environment and, for
    // this scope, the global panic hook), so no other thread observes
    // either. set_var/remove_var are the only way to simulate an operator's
    // ambient environment reaching a child spawned via `Command::new`.
    unsafe {
        std::env::set_var("NEMO_RELAY_ANTHROPIC_BASE_URL", "http://127.0.0.1:7777");
    }
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hazard_4_a_different_base_url_layer_clears_the_configured_auth_header()
    }));
    // SAFETY: see above.
    unsafe {
        std::env::remove_var("NEMO_RELAY_ANTHROPIC_BASE_URL");
    }
    std::panic::set_hook(previous_hook);

    let message = captured.lock().unwrap().take().unwrap_or_default();
    assert!(
        outcome.is_ok(),
        "F7: hazard-4 must be immune to an ambient NEMO_RELAY_ANTHROPIC_BASE_URL — its probe \
         clears the environment before spawning Relay, so no operator variable can layer above \
         the --config file it is experimenting on. It panicked instead:\n{message}"
    );
}

/// **F8 (M11.2b review): Relay's system config layer at
/// `/etc/nemo-relay/config.toml` outranks the rig's explicit `--config` and
/// re-aims the resolved `anthropic_base_url` — pinned here, mitigated in
/// [`Rig::wire_relay`].**
///
/// This is the one finding in the round with no in-tree fix. The switch that
/// would exclude the system layer, `skip_implicit_config`, is behind a test-only
/// cargo feature and is absent from the published binary, so a chained rig
/// cannot make its explicit file authoritative — it can only *check* what Relay
/// resolved before it hands a real client this deployment's turn key, which is
/// what `Rig::wire_relay`'s preflight does. This test is the evidence that
/// check is not paranoia, so it asserts the upstream behaviour as it is rather
/// than the behaviour the rig wishes for: it goes red the day Relay changes its
/// precedence, which is exactly when the preflight and the chained runbook want
/// re-reading.
///
/// F8 claims the Relay evidence's §2.4 `config_paths` appends the
/// system file *after* the explicit one and `merge_toml`'s later-wins
/// semantics let a system file clobber an explicit config's leaf values, so
/// an operator box with a system Relay install could re-aim a chained turn
/// at a foreign upstream with nothing in the rig noticing. `--dry-run`
/// prints the resolved config without spawning the agent (the same
/// mechanism hazard-4 above already exploits), so this needs only Relay and
/// root's ability to write the real system config path — no `claude`
/// binary, no roundhouse process.
///
/// Two controls bracket the one case that matters: absent a system file, the
/// explicit config's `anthropic_base_url` must resolve unmolested (control
/// A); with a system file present but naming the *same* base URL, it must
/// still resolve unmolested (control B, isolating that any failure below is
/// about a *differing* system value winning, not an artifact of the system
/// file's mere presence). Only then does the case assertion — a system file
/// naming a *different* base URL wins over the explicit one — mean what it
/// means. A `Drop` guard removes `/etc/nemo-relay` again regardless
/// of outcome, and the test refuses to run at all if that path already holds
/// a real operator config, so it can never clobber an actual system install.
#[test]
#[ignore = "F8: confirmed and unfixable in-tree — the system config layer wins over the rig's \
            explicit --config, and the switch that would disable it is behind a test-only cargo \
            feature absent from the published binary; the rig verifies instead (Rig::wire_relay's \
            preflight). Kept as the evidence for that ruling; \
            needs the real nemo-relay binary and root: --features e2e-claude -- --include-ignored; \
            writes and then removes the real /etc/nemo-relay/config.toml — refuses to run if that \
            path already exists"]
fn f8_system_config_layer_outranks_the_chained_rigs_explicit_config() {
    let relay = std::env::var(RELAY_BIN_VAR).unwrap_or_else(|_| {
        panic!(
            "this guard needs a real Relay binary: set {RELAY_BIN_VAR}, or run without \
             --include-ignored"
        )
    });
    println!("    relay version : {}", relay_version(&relay));

    let root = std::env::temp_dir().join(format!("roundhouse-f8-{}", uuid::Uuid::new_v4()));
    assert!(claim_root(&root), "the probe's scratch root must be fresh");
    let config = root.join("config.toml");
    std::fs::write(
        &config,
        "[upstream]\nanthropic_base_url = \"http://127.0.0.1:9999\"\n",
    )
    .expect("the probe's explicit config.toml");

    let dry_run = || -> String { relay_dry_run(&relay, &root, &config, &[]) };

    // `system_config_dir()` is hardcoded to `/etc/nemo-relay` on non-Windows
    // (Relay evidence §2.4) — not overridable by any
    // env var — so proving this claim means writing the real path. Guard it:
    // refuse to touch a box that already has one, and remove it again on
    // scope exit whether the test panics or not.
    struct SystemConfigGuard {
        dir: PathBuf,
    }
    impl Drop for SystemConfigGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    let system_dir = PathBuf::from("/etc/nemo-relay");
    assert!(
        !system_dir.exists(),
        "this box already has a real /etc/nemo-relay — refusing to overwrite an operator's \
         actual system config; run this guard on a clean box only"
    );
    let _guard = SystemConfigGuard {
        dir: system_dir.clone(),
    };

    assert!(
        dry_run().contains("anthropic_base_url = http://127.0.0.1:9999"),
        "control A: with no system file at all, the explicit config's base URL must resolve \
         unmolested"
    );

    std::fs::create_dir_all(&system_dir).expect("the probe's system config directory");
    std::fs::write(
        system_dir.join("config.toml"),
        "[upstream]\nanthropic_base_url = \"http://127.0.0.1:9999\"\n",
    )
    .expect("the probe's same-value system config.toml");
    assert!(
        dry_run().contains("anthropic_base_url = http://127.0.0.1:9999"),
        "control B: a system file naming the *same* base URL must not read as a foreign \
         upstream — otherwise this guard would prove nothing about which value won"
    );

    std::fs::write(
        system_dir.join("config.toml"),
        "[upstream]\nanthropic_base_url = \"http://127.0.0.1:8888\"\n",
    )
    .expect("the probe's differing-value system config.toml");
    let resolved = dry_run();
    assert!(
        resolved.contains("anthropic_base_url = http://127.0.0.1:8888"),
        "F8: a system config.toml naming a different anthropic_base_url is supposed to win over \
         the rig's explicit --config — that is the upstream fact `Rig::wire_relay`'s preflight \
         exists for, and if it has stopped being true, the preflight can be reconsidered and \
         `claude_launch`'s chained runbook needs a matching addendum. Resolved:\n{resolved}"
    );
    assert!(
        !resolved.contains(&format!("anthropic_base_url = {}", "http://127.0.0.1:9999")),
        "F8: and the explicit value must be the one that lost — otherwise the two layers agree \
         and this probe proves nothing about precedence. Resolved:\n{resolved}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// **F1 (M11.2b review), now the guard on its own fix: the shared redactor
/// treats `x-api-key` as credential-bearing.**
///
/// The finding was structural and it was real: this suite's recorder was a
/// line-for-line copy of the codex sibling's, and the copies had already
/// diverged — codex's redactor had learned about `chatgpt-account-id`, this
/// one's had never learned about `x-api-key`. One redactor in
/// [`common::e2e`](../common/e2e.rs) is the fix; this asserts it from this side
/// of the copy that used to exist.
///
/// **What the finding got wrong, kept here because the correction matters more
/// than the fix.** It read the gap as a live credential leak — "under
/// `ForwardedClaudeLogin` chained through Relay, a caller's real Anthropic key"
/// — and no path in this file can produce that.
/// [`forwarded_claude_login_never_writes_the_api_key_env`] below shows why: the
/// only arm that writes `x-api-key` at all writes the public sentinel, and the
/// arm that could carry a real credential writes no API key whatsoever and
/// would present its bearer on `authorization`, which was redacted all along.
/// So this arm is defence in depth for a future auth kind, not a leak that was
/// open. Recording that distinction is what stops the next reader from
/// concluding a key was exposed.
#[test]
fn redacted_headers_redacts_an_api_key() {
    let real_looking_key = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let exchange = Exchange {
        method: "POST".to_string(),
        path: "/v1/messages".to_string(),
        query: None,
        headers: BTreeMap::from([("x-api-key".to_string(), real_looking_key.to_string())]),
        body: None,
        status: 200,
        response: None,
        response_text: None,
    };

    let redacted = exchange.redacted_headers();

    assert_ne!(
        redacted.get("x-api-key").map(String::as_str),
        Some(real_looking_key),
        "F1: `x-api-key` must be redacted the way `authorization` and the turn-key header are — \
         a printed evidence block is the shape a fixture holding something real would copy"
    );
    assert_eq!(
        redacted.get("x-api-key").map(String::as_str),
        Some(format!("<{} bytes redacted>", real_looking_key.len()).as_str()),
        "and redacted to its length, so the diagnostic a printed header set exists for — the \
         header arrived, and was this big — survives"
    );
}

/// **F1 refute, live control: `ForwardedClaudeLogin` never generates
/// `API_KEY_ENV`, so it never puts anything on `x-api-key` in the first
/// place.**
///
/// Where [`f1_citation_marker`] cites `claude_launch.rs`'s doc comment, this
/// exercises the actual generator both ways with no binary and no socket —
/// the same seam [`the_childs_environment_is_the_generated_map_plus_the_isolation_vars`]
/// reads. `ClaudeAuthKind::RoundhouseKey` (the default, and the only arm this
/// file's `Rig` drives) is the one that writes [`API_KEY_ENV`], and only ever
/// with [`ROUNDHOUSE_API_KEY_SENTINEL`]; `.forwarding_claude_login()` omits
/// the variable entirely. A client launched that way sends no `x-api-key` at
/// all — whatever real credential it does present rides its OAuth login
/// instead, straight to `authorization`, which `redacted_headers` already
/// redacts. This is the fact that makes F1's "a caller's real Anthropic key"
/// on `x-api-key`" reading of the mechanism unreachable, independent of the
/// real structural gap above.
#[test]
fn forwarded_claude_login_never_writes_the_api_key_env() {
    let turn_key = "rh_turn_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    let roundhouse_key_env = ClaudeLaunch::new("http://127.0.0.1:9", turn_key)
        .expect("the documented-correct shape constructs")
        .env()
        .expect("RoundhouseKey renders");
    assert_eq!(
        roundhouse_key_env.get(API_KEY_ENV).as_deref(),
        Some(ROUNDHOUSE_API_KEY_SENTINEL),
        "control: the default (RoundhouseKey) arm is the one this file's Rig actually drives, \
         and it must write only the sentinel — never a real key — onto API_KEY_ENV"
    );

    let forwarded_login_env = ClaudeLaunch::new("http://127.0.0.1:9", turn_key)
        .expect("the documented-correct shape constructs")
        .forwarding_claude_login()
        .env()
        .expect("ForwardedClaudeLogin renders");
    assert_eq!(
        forwarded_login_env.get(API_KEY_ENV),
        None,
        "F1's mechanism claims a real Anthropic key reaches `x-api-key` under \
         ForwardedClaudeLogin; it cannot, because that arm never writes API_KEY_ENV \
         (claude_launch.rs's `if self.auth == ClaudeAuthKind::RoundhouseKey` gate) — a client \
         launched this way sends no x-api-key at all, and its real credential (if any) rides \
         `authorization` instead, which this file's redacted_headers already redacts"
    );
}

/// **F2 (M11.2b review), now the guard on its own fix: the two topologies
/// differ at one site.**
///
/// The finding counted four — a `deadline` method and a `label` method,
/// each its own `match self`, plus `build_child_command`'s `match topology` for
/// program and argv and a `matches!(topology, ..)` two dozen lines later for
/// Relay's XDG state. Purely structural: nothing behaved wrongly, which is why
/// it survived a milestone. What it cost is that a fifth site could be added, or
/// one of four updated alone, and no assertion anywhere would notice.
///
/// A structural claim gets a structural test, counted the way the finding
/// counted it. One site is [`Topology::plan`], which answers program, argv,
/// environment, deadline and label together.
///
/// **Read from `common/claude_rig.rs` since M12 review F11**, which is where
/// the dispatch now lives. The guard stayed in the suite rather than moving
/// with the code it guards: a file that scans itself for the shape of its own
/// contents is a check with no independent reader, and this one exists
/// precisely to be the reader.
#[test]
fn topology_is_dispatched_on_at_one_site() {
    let source = include_str!("common/claude_rig.rs");

    let impl_topology_body = {
        let start = source
            .find("impl Topology {")
            .expect("impl Topology exists");
        let after = &source[start..];
        let end = after
            .find("\n\n/// A live roundhouse")
            .expect("Rig's doc comment follows impl Topology");
        &after[..end]
    };
    let impl_sites = impl_topology_body.matches("match self").count();

    let build_child_command_body = {
        let start = source
            .find("fn build_child_command(")
            .expect("build_child_command exists");
        let after = &source[start..];
        let end = after
            // The anchor moved with R-T5: the `RELAY_STATE_VARS` const that
            // used to follow this function is now in the library, so the next
            // item is `claude_argv`. A structural test reads source, and source
            // is what R-T5 rearranged.
            .find("\n/// The client's own argv")
            .expect("claude_argv's doc comment follows build_child_command");
        &after[..end]
    };
    let bcc_sites = build_child_command_body.matches("match topology").count()
        + build_child_command_body
            .matches("matches!(topology")
            .count();

    let total_sites = impl_sites + bcc_sites;
    assert_eq!(
        total_sites, 1,
        "F2: Topology must be branched at exactly one site — `Topology::plan`, which answers \
         program, argv, environment, deadline and label together. Found {total_sites} \
         (`impl Topology`: {impl_sites} `match self`; build_child_command: {bcc_sites})"
    );
}

/// **F2, second half: a chained [`Rig`] is constructed chained.**
///
/// `start_chained` used to build a Direct rig via `start_at` — which stamped
/// `topology: Topology::Direct` — and overwrite `rig.topology` afterwards.
/// Nothing read the field in between, so there was no behavioural difference a
/// black-box test could observe; the defect is that the type stopped being the
/// answer to "what topology is this", which is the property the enum exists to
/// carry. Read from source for that reason, rather than by standing up a rig to
/// fail to observe it.
///
/// The construction-order problem that forced the mutation is real and is
/// handled rather than removed: Relay's config names the deployment's own base
/// URL, so the caller states a [`Wiring`] and `start_at` resolves it once both
/// halves exist.
///
/// Read from `common/claude_rig.rs` for the same reason as the guard above.
#[test]
fn a_chained_rigs_topology_is_not_a_post_construction_mutation() {
    let source = include_str!("common/claude_rig.rs");

    let start_chained_body = {
        let start = source
            .find("async fn start_chained(")
            .expect("start_chained exists");
        let after = &source[start..];
        let end = after
            .find("\n    /// Where a run of `label` puts its home")
            .expect("a_root_for's doc comment follows start_chained");
        &after[..end]
    };

    assert!(
        !start_chained_body.contains(".topology ="),
        "F2: a chained rig must be constructed chained. Assigning `rig.topology` after \
         `start_at` returns leaves a window in which the value is a Direct rig wearing a chained \
         label, and puts the answer to \"what topology is this\" outside the type"
    );
}

/// **F4 (M11.2b review), now the guard on its own fix: the command the harness
/// spawns kills its child on drop.**
///
/// tokio's default is `false`, and nothing in `build_child_command` used to
/// change it — so the one scenario `CHILD_DEADLINE`/`CHAINED_DEADLINE` exist
/// for, a hung client, was exactly the one that leaked `claude`, and under
/// Chained `nemo-relay` and the temporary plugin directory it removes only on
/// its way out.
///
/// This is the no-binary half: construct the command the real harness spawns and
/// read `get_kill_on_drop()` back off it. The graceful half — interrupt first so
/// Relay tears down the client it owns, kill only what ignores that — is
/// [`Rig::spawn`]'s watchdog, which needs two real binaries and a hung upstream
/// to exercise and is documented there rather than sampled here.
#[test]
fn build_child_command_kills_on_drop() {
    let turn_key = "rh_turn_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let env = ClaudeLaunch::new("http://127.0.0.1:9", turn_key)
        .expect("the documented-correct shape constructs")
        .env()
        .expect("RoundhouseKey renders");
    for topology in [
        Topology::Direct,
        Topology::Chained {
            relay: "nemo-relay".into(),
            config: PathBuf::from("/does/not/need/to/exist/relay-config.toml"),
            handoff: RelayHandoff::for_claude("http://127.0.0.1:9", "claude").expect("a handoff"),
        },
    ] {
        let command = build_child_command(
            "claude",
            &topology,
            &env,
            Path::new("/tmp/f4-guard-root"),
            "irrelevant prompt",
            &[],
            false,
        );

        assert!(
            command.get_kill_on_drop(),
            "F4: `{}` has kill_on_drop == false (tokio's default), so any path out of \
             Rig::spawn that drops the Child — the deadline above all — leaves the child \
             running: claude on Direct, and on Chained nemo-relay plus its temp plugin dir",
            topology
                .plan("claude", Path::new("/tmp/f4-guard-root"))
                .label
        );
    }
}

/// **F11 (M12 thermo-nuclear review), now the guard on its own fix: the rig
/// left this file, and the guards that read it followed it.**
///
/// F11 claimed the extraction was "a line count plus a mechanical extraction",
/// and the refuter proved that half wrong: this file carried two structural
/// guards ([`topology_is_dispatched_on_at_one_site`],
/// [`a_chained_rigs_topology_is_not_a_post_construction_mutation`]) that read
/// their own source and located `impl Topology`, `build_child_command` and
/// `start_chained` by scanning *this file's* text for exact anchors. Moving
/// those items to `common/claude_rig.rs` without porting the guards would have
/// left two `.expect("... exists")` calls panicking — an extraction whose
/// "identical pass set" was two failures.
///
/// So the property worth guarding is not "this file never scans itself" — it
/// legitimately still does, three times, about facts that really are about this
/// file (its module doc, its own leak-free scripts, its half of the
/// copy-paste check). It is the narrower one the port established: **the rig's
/// definitions live in `claude_rig.rs`, and this file's structural guards read
/// them there.** Both halves are asserted, because either alone is satisfiable
/// by an extraction that silently stopped guarding anything: a definition that
/// came back here would go unnoticed by a guard still pointed at the module,
/// and a guard pointed back here would `.expect()`-panic rather than pass, but
/// only once someone ran it.
///
/// The needles are assembled at run time for the reason the other textual
/// guards in this file assemble theirs: this test's own source *is*
/// `this_file`, so a needle spelled whole below would find itself and report a
/// definition that is only this assertion.
#[test]
fn the_rig_lives_in_its_own_module_and_the_guards_read_it_there() {
    let this_file = include_str!("claude_e2e.rs");
    let rig = include_str!("common/claude_rig.rs");

    let definitions = [
        ["impl Topo", "logy {"],
        ["fn build_child_", "command("],
        ["fn claude_", "argv("],
        ["async fn start_", "chained("],
    ];
    // A *definition* opens a line; the same spelling inside a `.find("…")`
    // anchor does not, and the two guards below legitimately carry three of
    // those. Matching on `contains` would have found the anchors and reported
    // this file as still holding the rig.
    let defines = |source: &str, needle: &str| {
        source.lines().map(str::trim_start).any(|line| {
            line.starts_with(needle)
                || line
                    .strip_prefix("pub ")
                    .is_some_and(|rest| rest.starts_with(needle))
        })
    };
    for halves in definitions {
        let needle = halves.concat();
        assert!(
            defines(rig, &needle),
            "F11: `{needle}` is not defined in common/claude_rig.rs — the two structural guards \
             below locate it by scanning that file, and a `.expect(\"... exists\")` there panics \
             rather than reporting what moved"
        );
        assert!(
            !defines(this_file, &needle),
            "F11: `{needle}` is defined in claude_e2e.rs again. The deployment, the topologies \
             and the child command are common/claude_rig.rs's; a second definition here is the \
             accretion that put this file past 4 700 lines, and the guards that scan the module \
             would not see it"
        );
    }

    // And the two guards really do read the module rather than this file:
    // repointing one `include_str!` back is the single edit that satisfies
    // every assertion above while quietly guarding nothing at all. Checked per
    // guard rather than by counting scans of the module across the file, so
    // that a *new* guard reading it neither satisfies this nor breaks it.
    let scan = format!("include_str!(\"common/{}\")", "claude_rig.rs");
    for guard in [
        "fn topology_is_dispatched_on_at_one_site()",
        "fn a_chained_rigs_topology_is_not_a_post_construction_mutation()",
    ] {
        let body = {
            let start = this_file.find(guard).unwrap_or_else(|| {
                panic!(
                    "F11: `{guard}` is this suite's guard on the rig's \
                     structure and must still exist to be pointed anywhere"
                )
            });
            let after = &this_file[start..];
            &after[..after
                .find("\n}\n")
                .expect("a guard is a function with an end")]
        };
        assert!(
            body.contains(scan.as_str()),
            "F11: `{guard}` no longer reads common/claude_rig.rs. It locates `impl Topology` / \
             `build_child_command` / `start_chained` by scanning source for exact anchors, so \
             pointed at this file it does not report the rig moved back — it panics in \
             `.expect(\"... exists\")`, and only for whoever next runs it."
        );
    }
}

/// **F4, second half: the deadline's shutdown really does end the child, even
/// one that ignores the interrupt.**
///
/// [`build_child_command_kills_on_drop`] pins the backstop; this pins the path
/// that runs first. It drives [`interrupt_then_kill`] — the function
/// [`Rig::spawn`]'s watchdog calls — against a child that *traps* `SIGINT` and
/// then sleeps, so a harness that only interrupted would hang here and one that
/// only killed would never have given Relay the chance to remove its plugin
/// directory. The child is a stub script rather than a real client for the same
/// reason the version-probe guard uses one: what is under test is this file's
/// shutdown, not either binary's response to a signal.
///
/// Reaped before the assertion, deliberately: an unreaped child is a zombie, and
/// a zombie answers `kill -0` exactly as a live process does — so "is it gone"
/// asked that way would pass whether or not anything worked. The exit status is
/// the unambiguous form of the question.
///
/// **Two timing hazards, closed here rather than tolerated as a flake.** Both
/// were seen: the test failed once in a `--test-threads=1` run and passed on
/// every other.
///
/// The first is the interrupt racing the trap. `Command::spawn` returns as soon
/// as the fork has a pid — before `/bin/sh` has been exec'd, let alone before it
/// has run `trap '' INT` — so a `SIGINT` sent on the next line can land on a
/// process whose disposition for it is still the default. The child then dies of
/// signal 2 and the assertion below reads it as a shutdown that never reached
/// `SIGKILL`, which is the opposite of what happened. So the stub announces the
/// trap by creating a marker file *after* installing it, and nothing is signalled
/// until that file exists: the deadline is armed against a child that is
/// provably already ignoring interrupts.
///
/// The second is asserting on the exit at a fixed instant. Reaping is the
/// kernel's to schedule, not ours, and a loaded box can leave a killed child
/// unreaped for longer than any single `wait` we would care to hard-code. The
/// exit is polled to a generous deadline instead — which weakens nothing, since
/// the assertions are still `SIGKILL` exactly and a deadline that expires is
/// still a failure naming what was still running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_deadline_ends_a_child_that_ignores_the_interrupt() {
    let root = std::env::temp_dir().join(format!("f4-deadline-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("wd")).expect("the stub run's working directory");
    let stub = root.join("ignores-sigint.sh");
    // The marker is written *after* the trap is installed and before the sleep,
    // so its existence is the stub's own statement that a `SIGINT` from here on
    // will be ignored. Ordering inside a shell script is what makes that a fact
    // rather than an estimate of how long an exec takes.
    let trapped = root.join("sigint-trapped");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\ntrap '' INT\n: > \"{}\"\nsleep 300\n",
            trapped.display()
        ),
    )
    .expect("the stub script writes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("the stub script is made executable");
    }

    let env = ClaudeLaunch::new(
        "http://127.0.0.1:9",
        "rh_turn_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    )
    .expect("the documented-correct shape constructs")
    .env()
    .expect("RoundhouseKey renders");
    let mut command = build_child_command(
        stub.to_str().expect("a scratch path that is valid UTF-8"),
        &Topology::Direct,
        &env,
        &root,
        "irrelevant prompt",
        &[],
        false,
    );
    let mut child = command.spawn().expect("the stub script spawns");
    let pid = child.id().expect("a freshly spawned child has a pid");

    // Arm the deadline only once the child says it is ignoring interrupts.
    let armed = tokio::time::Instant::now() + Duration::from_secs(30);
    while !trapped.exists() {
        assert!(
            tokio::time::Instant::now() < armed,
            "F4: the stub never installed its `SIGINT` trap, so this run would have proved \
             nothing about a child that ignores the interrupt"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    interrupt_then_kill(pid).await;
    // Polled rather than waited on at one instant: reaping is the kernel's to
    // schedule, and a deadline that expires still fails, naming the child that
    // outlived it.
    let reaped = tokio::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child
            .try_wait()
            .expect("waiting on a spawned child succeeds")
        {
            Some(status) => break status,
            None => assert!(
                tokio::time::Instant::now() < reaped,
                "F4: pid {pid} was interrupted and then killed and is still running; the \
                 deadline's shutdown does not end a child that ignores the interrupt"
            ),
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    assert!(
        !status.success(),
        "F4: a child that ignores SIGINT must not be left to exit on its own terms; got {status}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(9),
            "F4: the grace period must end in SIGKILL for a child that ignored the interrupt — \
             otherwise a hung client outlives the deadline that exists to stop it"
        );
    }

    clean(&root);
}

/// **F10 (M11.2b review), now the guard on its own fix: the module doc names
/// kinds of test, and cites Relay by section rather than by line.**
///
/// Two halves of one habit. The doc opened "Seven real-binary tests" and listed
/// them — a count that was already eight by the time the finding was written,
/// because a later review round added one and nothing made the prose follow.
/// And Relay source citations were copied into this file by file and line, in
/// two Rust files, where nobody re-derives them when the pin moves — which is
/// precisely what CLAUDE.md's synergy-vigilance rule exists to stop.
///
/// Both are cheap to reintroduce and invisible when wrong, so both are guarded
/// textually here rather than left to review.
#[test]
fn the_module_doc_does_not_enumerate_a_count_of_real_binary_tests() {
    let source = include_str!("claude_e2e.rs");
    let doc: Vec<&str> = source
        .lines()
        .filter(|line| line.starts_with("//!"))
        .collect();

    // Assembled at run time: a literal "N real-binary tests" spelled here would
    // be found by the scan below in this very assertion.
    let subject = format!("{}-binary tests", "real");
    for count in [
        "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten",
    ] {
        let claim = format!("{count} {subject}");
        assert!(
            !doc.iter().any(|line| line.contains(claim.as_str())),
            "F10: the module doc counts the real-binary tests again (\"{claim}\"). \
             `#[ignore]` already says which tests need a binary; a second spelling in prose is \
             one that goes stale the next time a review round adds one — and did"
        );
    }

    // Relay's source belongs in the evidence document, cited by section. A
    // `file.rs:1070-1078` here is a claim about a pinned tree with no path back
    // to the tree it was read from.
    //
    // Scanned across `common/claude_rig.rs` too since M12 review F11: the
    // chained wiring, and with it most of this suite's Relay citations, moved
    // there. A scan left pointing at this file alone would have reported green
    // about a habit that had simply relocated.
    let relay_files = [
        "gateway/mod.rs",
        "gateway/routes.rs",
        "gateway/response.rs",
        "configuration/mod.rs",
        "codec/streaming.rs",
        "agents/claude/launch.rs",
        "agents/claude/mod.rs",
        "server/mod.rs",
        "plugin.rs",
    ];
    let offenders: Vec<&str> = source
        .lines()
        .chain(include_str!("common/claude_rig.rs").lines())
        .filter(|line| line.trim_start().starts_with("//"))
        .filter(|line| {
            relay_files
                .iter()
                .any(|file| line.contains(&format!("{file}:")))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "F10: Relay source is cited by file and line in this suite or its rig module; cite the \
         evidence document's section instead (see the module doc's \"Where Relay evidence §x \
         points\"):\n{}",
        offenders.join("\n")
    );
}

/// **F18 (M11.3 review), now the guard on its own fix: the topham closure-test
/// helpers live once, in `tests/common/e2e.rs`, and not as near-verbatim twins
/// of [`codex_e2e`](../codex_e2e.rs)'s.**
///
/// [`version_probe`] already lived in the common module and was what
/// `topham_version` wrapped in both suites — proof the seam existed and was
/// simply not used for the rest, while the two copies had already drifted in
/// how each built its isolated probe home.
///
/// Textual, not behavioral, and deliberately so: duplication is a fact about
/// the source, not something a passing binary can disprove, so this re-derives
/// F18's own shell-diff proof as an assertion rather than trusting it stays
/// read. It is the only test here that needs no binary at all, which is why it
/// carries no `#[ignore]`: a copy-paste reappearing should fail an ordinary run
/// of this suite, not wait for the gated one.
///
/// **Each needle is assembled from two halves rather than written whole**, and
/// that is not decoration. This test's own source *is* `claude_src`, so a
/// needle spelled verbatim in the array below would make `claude_src.contains`
/// true forever — leaving a guard that only ever asks about the other file, and
/// that a copy re-landing in this one would sail straight past. Split, the
/// literal the scan looks for exists nowhere in either file except where a
/// definition actually puts it.
#[test]
fn topham_closure_helpers_are_not_copy_pasted_across_the_two_e2e_suites() {
    // Both halves of the claude side, because the `topham` closure runs the
    // helpers serve are driven from `Rig::through_topham` — which is in the rig
    // module since M12 review F11, and is therefore where a fresh copy would
    // now land.
    let claude_src = format!(
        "{}{}",
        include_str!("claude_e2e.rs"),
        include_str!("common/claude_rig.rs")
    );
    let codex_src = include_str!("codex_e2e.rs");

    let shared_verbatim = [
        [
            "const TOPHAM_BIN_VAR",
            ": &str = \"ROUNDHOUSE_TEST_TOPHAM_BIN\";",
        ],
        ["const TOPHAM_PROFILE", ": &str = \"e2e\";"],
        ["fn topham_binary()", " -> String {"],
        ["fn path_with(binary: &str)", " -> String {"],
    ];
    let duplicated: Vec<String> = shared_verbatim
        .iter()
        .map(|halves| halves.concat())
        .filter(|needle| claude_src.contains(needle.as_str()) && codex_src.contains(needle))
        .collect();
    assert!(
        duplicated.is_empty(),
        "F18: these topham closure-test helpers are defined independently in both the claude \
         suite (or its rig module) and codex_e2e.rs instead of once in tests/common/e2e.rs, \
         even though \
         tests/common/e2e.rs already holds version_probe (the function topham_version wraps in \
         both suites):\n{}",
        duplicated.join("\n")
    );
}

/// **F14 (M11.2b review): the constructed-command guard cannot see a leaked
/// `CLAUDE_CODE_REMOTE`, and this is where that limit is pinned against the real
/// function.**
///
/// [`the_get_envs_diff_cannot_see_a_dropped_env_clear`] proves the same thing
/// about `std::process::Command` in the abstract, with no dependency on this
/// crate at all. This one re-derives it against
/// [`build_child_command`]'s own construction steps, because that is the
/// function two documents were describing when they credited the no-binary
/// guard with going red on one leaked `CLAUDE_CODE_REMOTE`. It does not:
/// `Command::get_envs()` reports only the explicit `env()`/`env_remove()` diff,
/// and an ambient variable that was never named in an `env()` call is not in
/// that diff whether or not `env_clear()` ran.
///
/// The one line deliberately absent below is `command.env_clear()` — the
/// mutation the documents claimed their guard would catch. Its absence changes
/// nothing about what `get_envs()` reports, which is the whole point: only
/// [`the_seat_chain_a_launched_client_presents`], reading the real wire, can
/// catch that mutation. Kept live so a future reader who reaches for the
/// no-binary guard as the leak check finds this instead of rediscovering it.
///
/// The two documents' sentences are corrected in the docs half of this review
/// round; what lives here is the fact they were wrong about.
#[test]
fn the_constructed_command_guard_cannot_see_a_leaked_claude_code_remote() {
    let turn_key = "rh_turn_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let env = ClaudeLaunch::new("http://127.0.0.1:9", turn_key)
        .expect("the documented-correct shape constructs")
        .env()
        .expect("the documented-correct shape renders");
    let root = PathBuf::from("/does/not/need/to/exist");

    // The exact steps build_child_command's Direct arm performs, minus
    // `command.env_clear()` — the mutation both documents claim their guard
    // would catch.
    let mut command = std::process::Command::new("claude");
    command.args(claude_argv("prompt", &[], false));
    command.current_dir(root.join("wd"));
    // No env_clear() here — the mutation this test exists to name.
    for (name, value) in env.vars() {
        command.env(name, value);
    }
    command.env("PATH", std::env::var("PATH").unwrap_or_default());
    command.env("HOME", root.join("home"));
    command.env("CLAUDE_CONFIG_DIR", root.join("home/.claude"));
    command.env("DISABLE_AUTOUPDATER", "1");
    command.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");

    let envs: BTreeMap<String, Option<String>> = command
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect();

    assert!(
        !envs.contains_key("CLAUDE_CODE_REMOTE"),
        "F14: if this ever passes, `Command::get_envs()` has started reporting more than the \
         explicit env()/env_remove() diff — at which point the no-binary guard really would \
         catch a dropped env_clear(), and both this test and build_child_command's doc comment \
         should be rewritten rather than deleted"
    );
}
