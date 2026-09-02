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
//! control directory and its minted turn key, the session log, the prefix check,
//! the tool the client chose to run, and [`ClaudeEnv`] — the launcher's output is
//! consumed verbatim rather than re-spelled.
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
//! `ROUNDHOUSE_TEST_RELAY_BIN`. `--test-threads=1` is not politeness: `claude
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
//! **Where "Relay evidence §x" points.** Every claim below about what Relay
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
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use serde_json::Value;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::Principal;
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::now_ms;
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::FrontierClient;
use roundhouse_server::claude_launch::{
    API_KEY_ENV, BASE_URL_ENV, CUSTOM_HEADERS_ENV, ClaudeEnv, ClaudeLaunch,
    ROUNDHOUSE_API_KEY_SENTINEL,
};
use roundhouse_server::control_config::TURN_KEY_HEADER;
use roundhouse_server::messages_api::{MESSAGES_PATH, wire};
use roundhouse_server::{
    API_PREFIX, Conversations, EchoLocalExecutor, Engine, EngineConfig, messages_router,
};

mod common;
// The harness itself is `common::e2e` (M11.2b review F1): recorder, bootstrap,
// fork probe and version probe are the same rig the codex sibling stands its
// client inside, and were a line-for-line copy of it until the two drifted.
use common::e2e::{
    Exchange, PROJECT, Recorder, USER, base_session, bootstrap, clean, fork_probe, principal,
    record, version_probe,
};
use common::{Scripted, ScriptedTurns, ToolCallingFrontierClient, frontier_catalog};

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

/// What the file the client is asked to read contains.
///
/// Nothing else in this rig, in the client, or in either prompt can produce this
/// string, so finding it in the request body that arrives *after* the tool call
/// is evidence that the real client opened the real file — not that it guessed,
/// and not that it echoed the prompt.
const CANARY: &str = "the-canary-line-roundhouse-wrote";

/// The file the scripted upstream asks the client to read.
const CANARY_FILE: &str = "canary.txt";

/// How long a single `claude -p` may take before the test kills it.
///
/// Generous against a measured baseline of about a second: the child starts a
/// Node runtime, resolves its settings, and runs one or two turns against a
/// loopback socket. A deadline of its own rather than only the suite's outer
/// `timeout` because the outer one reports "the suite hung" and this one reports
/// which run hung, with that run's stderr and the `HOME` to inspect.
const CHILD_DEADLINE: Duration = Duration::from_secs(90);

/// The environment variable that overrides which binary is driven.
const CLAUDE_BIN_VAR: &str = "ROUNDHOUSE_TEST_CLAUDE_BIN";

/// The version this suite's assertions were written against.
///
/// Compared against the first whitespace-delimited token of `claude --version`,
/// which at this line prints `2.1.257 (Claude Code)`.
const VERIFIED_VERSION: &str = "2.1.257";

/// How long a chained `nemo-relay run --agent claude` may take.
///
/// Longer than [`CHILD_DEADLINE`] because the chain is three processes rather
/// than two: Relay resolves its configuration, binds an ephemeral loopback
/// gateway, runs `claude --version` for its own minimum-version gate
/// (Relay evidence §A.7), writes a temporary plugin
/// directory and a synthesized `--settings` document, and only then spawns the
/// client this suite is actually about.
const CHAINED_DEADLINE: Duration = Duration::from_secs(180);

/// The environment variable that names the Relay binary the chained tests drive.
const RELAY_BIN_VAR: &str = "ROUNDHOUSE_TEST_RELAY_BIN";

/// The Relay release the chained assertions were written against.
///
/// Compared against the last whitespace-delimited token of
/// `nemo-relay --version`, which at this line prints `nemo-relay 0.8.2`. The
/// evidence the chained tests rest on is
/// `agent-docs/research/nemo-relay-0.8.0-published-read.md`'s 2026-09-01
/// addendum, which re-derived every hazard below against exactly this tarball.
const VERIFIED_RELAY_VERSION: &str = "0.8.2";

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

// ---------------------------------------------------------------------------
// The deployment
// ---------------------------------------------------------------------------

/// Which of the two topologies a run drives.
///
/// R-D: **Direct is the reference and Chained is the same launch through one
/// more process.** The value of modelling them as one enum rather than two
/// harnesses is exactly that claim: [`build_child_command`] takes this, and
/// every other input it takes — the generated [`ClaudeEnv`], the isolation set,
/// the client's own argv — is shared by construction. A second harness would
/// let the two drift, and the interesting failure ("Relay changed what the
/// client presents") would be indistinguishable from "the chained harness spells
/// the launch differently".
/// Which topology a caller is *asking* for, before the deployment exists.
///
/// Distinct from [`Topology`] for one reason, and only for that reason: a
/// chained rig's Relay configuration names this deployment's own base URL, which
/// nobody knows until the listener is bound. The request can be stated before
/// `start_at` runs; the resolved topology can only be built during it. Collapsing
/// the two would put the config write back *after* construction, which is the
/// post-construction mutation M11.2b review F2 ruled out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wiring {
    Direct,
    Chained,
}

enum Topology {
    /// The client is spawned directly, pointed at roundhouse.
    Direct,
    /// The client is spawned by `nemo-relay run --agent claude`, which points it
    /// at Relay's own loopback gateway and forwards to roundhouse.
    Chained {
        /// The Relay binary, from [`RELAY_BIN_VAR`].
        relay: String,
        /// The `config.toml` aiming Relay's Anthropic upstream at roundhouse.
        config: PathBuf,
    },
}

/// Everything that differs between the two topologies, answered together.
///
/// **One dispatch site, and that is the point** (M11.2b review F2). The
/// difference used to be branched at four places across two types — a `deadline`
/// method, a `label` method, `build_child_command`'s `match` for program and
/// argv, and a `matches!` twenty-four lines later for Relay's XDG state — so
/// adding a third topology, or changing what a chained run needs, meant finding
/// four sites that nothing held together. What it cost to leave that alone was
/// not a wrong answer today; it was that any of the four could have been updated
/// without the others and nothing would have said so.
struct Launch {
    /// The program actually spawned.
    program: String,
    /// Whatever must precede the client's own argv, `--` included.
    leading: Vec<String>,
    /// Environment this topology's own process needs, beyond the launch map and
    /// the client's isolation set.
    extra_env: Vec<(&'static str, PathBuf)>,
    /// How long one run of this topology may take.
    deadline: Duration,
    label: &'static str,
}

impl Topology {
    fn plan(&self, binary: &str, root: &Path) -> Launch {
        match self {
            Self::Direct => Launch {
                program: binary.to_string(),
                leading: Vec::new(),
                extra_env: Vec::new(),
                deadline: CHILD_DEADLINE,
                label: "Direct",
            },
            // `run` rather than the bare `claude` shortcut: the shortcut runs an
            // interactive setup wizard when no config layer exists
            // (`commands/run.rs`'s `easy_path`, `needs_setup`), and a wizard in a
            // test rig is a hang. `--` is not optional — `RunCommand::command` is
            // `#[arg(last = true)]`, so without it the client's own flags are
            // parsed as Relay's.
            Self::Chained { relay, config } => Launch {
                program: relay.clone(),
                leading: vec![
                    "run".into(),
                    "--agent".into(),
                    "claude".into(),
                    "--config".into(),
                    config.to_string_lossy().into_owned(),
                    "--".into(),
                ],
                // Relay's own state, isolated the same way and for the same
                // reason the client's is. `--config` replaces only the *user*
                // config layer (`nemo-relay --help`: "system config still
                // applies"), and Relay reads an XDG user config layer and writes
                // its bootstrap/marketplace state under XDG data and state — all
                // of which would otherwise land in whatever `HOME` this box's
                // developer happens to have, and be read back by the next run as
                // configuration this test never wrote.
                //
                // The earlier spelling of this comment justified the four
                // variables with "Relay writes a session store and a
                // resolved-config cache", which M11.2b review F14 found is not
                // what 0.8.2's `run` mode persists: neither exists on that path,
                // and what the isolation actually covers is the bootstrap state
                // file and the marketplace state beside it. The variables stay —
                // the isolation is right — but the reason had to stop naming
                // artefacts nobody writes.
                extra_env: RELAY_STATE_VARS
                    .iter()
                    .map(|name| (*name, root.join("relay")))
                    .collect(),
                deadline: CHAINED_DEADLINE,
                label: "Chained",
            },
        }
    }
}

/// A live roundhouse, its filesystem, and everything needed to read it back.
struct Rig {
    /// Where this run's `HOME`, `CLAUDE_CONFIG_DIR` and working directory live.
    root: PathBuf,
    /// The minted turn key, in plaintext — the value the generated environment
    /// carries into [`TURN_KEY_HEADER`], and the only place it exists outside
    /// the directory's hash.
    secret: String,
    /// The environment a launched client is given, generated rather than
    /// re-spelled. Consuming [`ClaudeLaunch`]'s own output is what makes this
    /// suite evidence about the launcher and not only about the surface.
    env: ClaudeEnv,
    store: Arc<MemoryStore>,
    conversations: Arc<Conversations>,
    recorder: Recorder,
    upstream: Arc<ScriptedTurns>,
    binary: String,
    topology: Topology,
}

impl Rig {
    /// A run rooted wherever this deployment's own convention puts it.
    ///
    /// Under the system temp directory, never under `target/`. See the module
    /// doc: this client walks up from its working directory for a CLAUDE.md, a
    /// project `.claude/` and a git repository, so a run rooted in this checkout
    /// would launch carrying this repository as context.
    async fn start(label: &str, upstream: Arc<ScriptedTurns>) -> Self {
        Self::start_at(Self::a_root_for(label), upstream, Wiring::Direct).await
    }

    /// The same deployment, driven through a real `nemo-relay run --agent
    /// claude` (R-D, R-D′).
    ///
    /// **The client's environment is the same [`ClaudeEnv`] the Direct tests
    /// use, and that is the ruling this constructor exists to instantiate.**
    /// Relay overwrites `ANTHROPIC_BASE_URL` with its own gateway and *merges*
    /// its proxy token into `ANTHROPIC_CUSTOM_HEADERS` rather than replacing the
    /// block (Relay evidence §A.7 — `replace_custom_header`
    /// drops only a line whose name matches), so the turn key survives the hop
    /// on [`TURN_KEY_HEADER`] and a chained turn keeps exactly Direct's
    /// semantics. One generator, two topologies.
    ///
    /// The config written here is the whole chained contract, and each line is a
    /// ruling:
    ///
    /// - `[upstream] anthropic_base_url` is this deployment's **root**. Relay
    ///   concatenates the inbound `path_and_query` onto it whole
    ///   (Relay evidence §A.4), so a value carrying `/v1` would send
    ///   `/v1/v1/messages`.
    /// - **No `anthropic_auth_header`.** Relay injects one only when the inbound
    ///   request carries none of `authorization` / `x-api-key` / `api-key` /
    ///   `anthropic-api-key` (Relay evidence §2.3, the `already_authed`
    ///   short circuit), and a client launched with [`ClaudeAuthKind::RoundhouseKey`]
    ///   always carries the sentinel on `x-api-key`. Configuring one here would
    ///   therefore be dead configuration that silently becomes live the day the
    ///   sentinel is dropped — see the chained runbook for the fallback where it
    ///   is the *only* carrier.
    /// - `[agents.claude] command` names the binary under test, so Relay drives
    ///   the same client [`CLAUDE_BIN_VAR`] names rather than whatever `claude`
    ///   resolves to on `PATH`.
    ///
    /// The bind address is deliberately **not** configured: `run` picks an
    /// ephemeral loopback port, and 0.8.2 refuses a non-loopback bind outright
    /// (Relay evidence §A.10.1, new at this release), so naming one here could
    /// only make the run fail in a way the evidence already predicts.
    ///
    /// **Constructed chained, never mutated into it** (M11.2b review F2). This
    /// used to build a Direct rig and overwrite `rig.topology` once `start_at`
    /// had returned, which left a window — short, and never read from — in which
    /// the value was a `Direct` rig wearing a chained label. Nothing behaved
    /// wrongly because of it; what it cost is that "what topology is this rig"
    /// had two answers depending on when you asked, and the second one was not
    /// in the type.
    ///
    /// The reason it was written that way is real and is handled rather than
    /// ignored: Relay's configuration names *this deployment's own base URL*,
    /// which does not exist until the listener is bound inside `start_at`. So
    /// the caller states a [`Wiring`] — a request, decidable before anything
    /// binds — and `start_at` resolves it into a [`Topology`] at the one moment
    /// both halves are known.
    async fn start_chained(label: &str, upstream: Arc<ScriptedTurns>) -> Self {
        Self::start_at(Self::a_root_for(label), upstream, Wiring::Chained).await
    }

    /// Where a run of `label` puts its home and working directory.
    ///
    /// Public to the file rather than inlined into [`Self::start`] because one
    /// test needs the path *before* the rig exists: the scripted upstream names
    /// the file the client will read, and it has to be scripted before the rig
    /// that writes it is built.
    fn a_root_for(label: &str) -> PathBuf {
        std::env::temp_dir()
            .join("roundhouse-claude-e2e")
            .join(format!("{label}-{}", uuid::Uuid::new_v4()))
    }

    /// Bind, serve, mint, and generate the client's environment.
    ///
    /// A *minted* key against the production `PlaneSource` — an
    /// `Arc<ControlDirectory>`, the one a shipped binary can name — rather than
    /// a file-declared one, for the reason the codex sibling gives: the point of
    /// the rung is that a real client authenticates the way a real tenant does.
    async fn start_at(root: PathBuf, upstream: Arc<ScriptedTurns>, wiring: Wiring) -> Self {
        assert!(
            claim_root(&root),
            "a Rig's root must be exclusive to it: {} already exists. Two rigs sharing one root \
             race `claude --continue`'s \"most recent conversation\" resolution onto each other's \
             sessions — see claim_root's doc comment for the reproduction.",
            root.display()
        );
        let label = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        std::fs::create_dir_all(root.join("home/.claude")).expect("the run's CLAUDE_CONFIG_DIR");
        std::fs::create_dir_all(root.join("wd")).expect("the run's working directory");
        std::fs::write(root.join("wd").join(CANARY_FILE), format!("{CANARY}\n"))
            .expect("the file the client is asked to read");

        // No judge in the cross-checks and no `validate` block on the project:
        // nothing here enrols its sessions in validation, and promising a judge
        // the cross-check would then have to find would be fixture state this
        // suite makes no claim about.
        let deployment = bootstrap(
            "claude-e2e bootstrap",
            "m11-claude-e2e",
            serde_json::json!({ "id": PROJECT }),
            None,
        );
        let directory = deployment.directory;
        let minted = deployment.minted;

        let store = Arc::new(MemoryStore::new());
        let conversations = Arc::new(Conversations::new());
        let arm_salt = directory.plane(now_ms()).arm_salt().to_string();
        let engine = Arc::new(Engine::new(
            Arc::clone(&store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local answer")),
            frontier_catalog(),
            Arc::clone(&upstream) as Arc<dyn FrontierClient>,
            Arc::new(AffinityPolicy::new()),
            EngineConfig {
                arm_salt,
                ..EngineConfig::default()
            },
        ));

        let recorder = Recorder::default();
        let app: Router = messages_router(
            Arc::clone(&directory),
            engine,
            Arc::clone(&store),
            Arc::clone(&conversations),
        )
        .layer(axum::middleware::from_fn_with_state(
            recorder.clone(),
            record,
        ));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let addr = listener.local_addr().expect("the bound address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // The **deployment root**, with no `{API_PREFIX}`: the client's vendored
        // SDK appends the version segment itself, and `ClaudeLaunch::new` refuses
        // a base URL that already carries one. Handing it the same string the
        // codex sibling hands `CodexLaunch` would be refused by name — which is
        // the refusal existing so that this line is right.
        let base_url = format!("http://{addr}");
        let env = ClaudeLaunch::new(&base_url, &minted.secret)
            .expect("the rig's own base URL and minted key are the correct shape")
            .env()
            .expect("a bring-your-own-key launch with no other variables renders");

        let binary = std::env::var(CLAUDE_BIN_VAR).unwrap_or_else(|_| "claude".to_string());
        let version = claude_version(&binary);
        println!("--- {label}");
        println!("    claude binary : {binary}");
        println!("    version       : {version}");
        if version.split_whitespace().next() != Some(VERIFIED_VERSION) {
            println!(
                "    WARNING: this suite's assertions were written against {VERIFIED_VERSION}. \
                 CLAUDE.md's synergy-vigilance rule applies: re-read what changed upstream \
                 before trusting a green run against a different binary."
            );
        }
        println!("    roundhouse    : {base_url}");
        println!("    HOME          : {}", root.join("home").display());
        println!(
            "    launch env    : {}",
            env.names().collect::<Vec<_>>().join(", ")
        );

        // The one place the requested wiring becomes a resolved topology, and
        // the only place it is decided at all (M11.2b review F2).
        let topology = match wiring {
            Wiring::Direct => Topology::Direct,
            Wiring::Chained => Self::wire_relay(&root, &base_url, &binary),
        };

        Self {
            root,
            secret: minted.secret,
            env,
            store,
            conversations,
            recorder,
            upstream,
            binary,
            topology,
        }
    }

    /// Write this run's Relay configuration, and refuse to spawn anything until
    /// Relay agrees it resolved to *this* deployment.
    ///
    /// **The preflight is M11.2b review F8, and it exists because the layering
    /// cannot be excluded.** `--config` replaces the *user* layer only; Relay
    /// then folds `/etc/nemo-relay/config.toml` in after it, and a leaf that
    /// appears in both wins from the system file (Relay evidence §2.4). The
    /// switch that would turn that off, `skip_implicit_config`, is behind a
    /// test-only cargo feature and is not in the published binary. So an
    /// operator box with a system Relay install could re-aim a chained turn —
    /// this rig's minted turn key and its launch sentinel — at whatever that
    /// file names, and every assertion downstream would be about a foreign
    /// upstream while reading perfectly green.
    ///
    /// Verifying instead of assuming costs one extra `--dry-run`, which resolves
    /// configuration and exits without spawning the agent. What it buys is that
    /// the failure names the file: a chained rig that silently pointed somewhere
    /// else is exactly the kind of drift a green suite cannot report.
    fn wire_relay(root: &Path, base_url: &str, binary: &str) -> Topology {
        let relay = std::env::var(RELAY_BIN_VAR).unwrap_or_else(|_| {
            panic!(
                "the chained topology needs a real Relay binary: set {RELAY_BIN_VAR}, or run \
                 without --include-ignored"
            )
        });
        let version = relay_version(&relay);
        let config = root.join("relay-config.toml");
        std::fs::write(
            &config,
            format!(
                "[upstream]\nanthropic_base_url = \"{base_url}\"\n\n[agents.claude]\ncommand = \
                 \"{binary}\"\n"
            ),
        )
        .expect("the run's Relay configuration");
        std::fs::create_dir_all(root.join("relay")).expect("the run's Relay state directory");

        println!("    relay binary  : {relay}");
        println!("    relay version : {version}");
        if version.split_whitespace().last() != Some(VERIFIED_RELAY_VERSION) {
            println!(
                "    WARNING: the chained assertions were written against nemo-relay \
                 {VERIFIED_RELAY_VERSION}. CLAUDE.md's synergy-vigilance rule applies: Relay is \
                 the other half of this product, so re-read what changed upstream before trusting \
                 a green run against a different build."
            );
        }
        println!("    relay config  : {}", config.display());

        let resolved = relay_dry_run(&relay, root, &config, &[]);
        let wanted = format!("anthropic_base_url = {base_url}");
        assert!(
            resolved.contains(&wanted),
            "F8: this chained rig's explicit --config names `{wanted}`, but Relay resolved \
             something else. `--config` replaces only the user layer, and \
             /etc/nemo-relay/config.toml is folded in *after* it, so a system Relay install on \
             this box is re-aiming the run — refusing to launch a real client with this rig's \
             turn key at an upstream this test did not choose. Relay resolved:\n{resolved}"
        );

        Topology::Chained { relay, config }
    }

    /// The principal every request below resolves to.
    fn principal(&self) -> Principal {
        principal()
    }

    /// Every turn request, in arrival order.
    ///
    /// Matched on the path alone: the client appends `?beta=true`, which axum
    /// routes past and this filter must too — a comparison against the whole URI
    /// would find nothing and every assertion below would fail as "the client
    /// never sent a turn".
    fn turns(&self) -> Vec<Exchange> {
        self.recorder.to(&format!("{API_PREFIX}/{MESSAGES_PATH}"))
    }

    /// The absolute path of the file the client is asked to read.
    fn canary_path(&self) -> PathBuf {
        self.root.join("wd").join(CANARY_FILE)
    }

    /// The session the client drove, discovered rather than predicted.
    ///
    /// The test cannot know the client's session UUID in advance — it is minted
    /// inside the child — and a Configured deployment qualifies the name by
    /// principal on top of that. This is the production accessor for "the last
    /// session this principal drove a turn on, on this node", reading the same
    /// `Arc<Conversations>` the router was handed.
    fn session(&self) -> SessionId {
        common::e2e::session(&self.conversations)
    }

    /// The session's committed items, in log order.
    async fn items(&self) -> Vec<Item> {
        common::e2e::items(&self.store, &self.session()).await
    }

    /// A fork is silent from the client's side, so the only way to catch one is
    /// to ask the store whether generation one exists at all.
    async fn assert_never_forked(&self) {
        common::e2e::assert_never_forked(&self.store, &self.session()).await;
    }

    /// A first `claude -p` in this run's isolated home.
    async fn print(&self, prompt: &str, extra: &[&str]) -> ClaudeRun {
        self.spawn(prompt, extra, false).await
    }

    /// A `claude --continue -p`, extending the conversation the previous run
    /// left in this home for this working directory.
    async fn continued(&self, prompt: &str) -> ClaudeRun {
        self.spawn(prompt, &[], true).await
    }

    /// Run one child to completion, or kill its whole tree at the deadline.
    ///
    /// **The deadline used to leak the very processes it exists to stop**
    /// (M11.2b review F4). It wrapped `Command::output()` in
    /// `tokio::time::timeout` and panicked on expiry, and tokio's `Child`
    /// defaults to *not* killing on drop — so the one scenario the deadline
    /// exists for, a hung client, was exactly the scenario that orphaned
    /// `claude`, and under Chained also `nemo-relay` and the temporary plugin
    /// directory it only removes on its way out.
    ///
    /// Three things close it, in the order they fire:
    ///
    /// 1. `SIGINT` to the direct child. Under Chained that child is Relay, which
    ///    puts the client in a process group of its own and tears it down — plus
    ///    its plugin temp dir — on interrupt or normal exit. A `SIGKILL` first
    ///    would skip both.
    /// 2. A short grace period, then `SIGKILL` for whatever ignored the
    ///    interrupt — pinned by
    ///    [`an_expired_deadline_ends_a_child_that_ignores_the_interrupt`],
    ///    which traps the interrupt on purpose so a harness that only
    ///    interrupted would hang there rather than here.
    /// 3. `kill_on_drop(true)` on the command itself, as the backstop for every
    ///    path out of this function that is not this one — a panic between spawn
    ///    and wait included.
    async fn spawn(&self, prompt: &str, extra: &[&str], resume: bool) -> ClaudeRun {
        let mut command = build_child_command(
            &self.binary,
            &self.topology,
            &self.env,
            &self.root,
            prompt,
            extra,
            resume,
        );
        let plan = self.topology.plan(&self.binary, &self.root);

        let child = command.spawn().unwrap_or_else(|error| {
            panic!(
                "could not run `{}`: {error}. Set {CLAUDE_BIN_VAR} to a real claude binary, \
                 or drop --include-ignored.",
                plan.program
            )
        });
        // A watchdog beside the child rather than a `timeout` around it: a
        // `timeout` that expires drops the future holding the `Child`, and a
        // dropped `Child` is killed outright by `kill_on_drop` — which is the
        // backstop, not the shutdown. Signalling from beside it lets the
        // interrupt land while the child is still ours to wait on, so Relay
        // takes its own client and its plugin temp dir down with it.
        let pid = child.id();
        let deadline = plan.deadline;
        let watchdog = tokio::spawn(async move {
            tokio::time::sleep(deadline).await;
            if let Some(pid) = pid {
                interrupt_then_kill(pid).await;
            }
        });
        let started = std::time::Instant::now();
        let output = child
            .wait_with_output()
            .await
            .expect("a spawned child's pipes are readable");
        watchdog.abort();
        assert!(
            started.elapsed() < deadline,
            "a {} `{} -p` did not finish within {deadline:?} and was interrupted, then killed. \
             HOME: {}",
            plan.label,
            self.binary,
            self.root.join("home").display()
        );

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        ClaudeRun {
            // `--output-format json` prints exactly one document. Parsed
            // leniently so a client that printed something else fails on the
            // assertion that names what was missing rather than on a panic here.
            result: serde_json::from_str::<Value>(stdout.trim()).ok(),
            stdout,
            stderr,
            success: output.status.success(),
        }
    }

    /// Remove this run's directory.
    fn clean(&self) {
        clean(&self.root);
    }
}

/// Take a hung child's tree down the way its own shutdown path expects, then
/// make sure it is down.
///
/// `SIGINT` first because under Chained the child is Relay: it restores the
/// temporary plugin directory it wrote and terminates the client it spawned into
/// a separate process group only on a normal exit or an interrupt, so killing it
/// outright leaves both behind. The grace period is short because nothing here
/// is meant to survive it — this path runs only after a deadline has already
/// expired, and the panic that follows is the diagnosis.
///
/// `kill(1)` rather than a signalling crate: sending a signal from Rust needs a
/// libc binding this test tree does not otherwise carry, and adding a dependency
/// to the workspace manifest to interrupt a test child is a larger change than
/// the defect. A missing `kill` degrades to the `SIGKILL` below, which is what
/// the previous behaviour would have been anyway.
async fn interrupt_then_kill(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status();
    tokio::time::sleep(KILL_GRACE).await;
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status();
}

/// How long an interrupted child gets to unwind before it is killed outright.
const KILL_GRACE: Duration = Duration::from_secs(3);

/// Claims `root` for exclusive use by one [`Rig`], or reports that something
/// else already holds it.
///
/// [`Rig::a_root_for`] mints a fresh UUID per call, so no two of *this file's*
/// own tests ever collide — but nothing previously stopped a caller from
/// pointing [`Rig::start_at`] at a hand-picked or reused root, and two rigs
/// sharing one root silently share one `claude --continue` history: `claude
/// --continue` resolves "the most recent conversation" from the session store
/// under the shared `HOME`, keyed by working directory, so rig A's continued
/// turn can extend rig B's conversation instead of its own (reproduced
/// directly: two `Rig::start_at` calls pointed at one root, driven
/// concurrently, and rig A's `--continue` came back with rig B's session id).
///
/// A separate, synchronous, no-process function — rather than inlined into
/// `start_at` — so the property this closes is checkable on its own: see
/// [`a_rigs_root_cannot_be_claimed_twice`] below. `std::fs::create_dir` is the
/// enforcement, not the assertion in `start_at` that reads it: `mkdir` is one
/// atomic syscall, so of two concurrent claims on the same path exactly one
/// succeeds, which is what makes this safe under real concurrency and not only
/// under the sequential case the guard test exercises.
fn claim_root(root: &Path) -> bool {
    if let Some(parent) = root.parent() {
        std::fs::create_dir_all(parent).expect("the root's parent directory");
    }
    std::fs::create_dir(root).is_ok()
}

/// Build the exact `claude` child command [`Rig::spawn`] runs, without running
/// it.
///
/// Pulled out as its own function — rather than left inline — so the module
/// doc's isolation claim has a guard that needs no process. A check on what
/// *arrived* structurally cannot see a credential that was merely *available*:
/// under `RoundhouseKey` an ambient `ANTHROPIC_AUTH_TOKEN` would be resolved
/// ahead of the sentinel and change which credential the client presents, while
/// an ambient `CLAUDE_CODE_REMOTE` would change it without adding any header a
/// naive assertion looks for. A check on construction sees the second case —
/// *if* `env_clear()` ran — via `Command::get_envs()` on the object this
/// returns.
///
/// **What that check cannot see: whether `env_clear()` itself ran.**
/// `Command::get_envs()` reports only the explicit `env()`/`env_remove()` diff,
/// identically whether or not the environment was cleared first — an ambient
/// variable that was never named in an `env()` call is invisible to it either
/// way, so a dropped `env_clear()` produces the exact same `get_envs()` output
/// as a correct one (verified directly: `the_get_envs_diff_cannot_see_a_dropped_env_clear`
/// below). The only thing that actually observes a leaked ambient credential is
/// [`the_seat_chain_a_launched_client_presents`], reading the real wire under
/// `--include-ignored` — see the module doc's ordering of guards 2 and 3.
///
/// One function used by both the real harness and its own test, rather than a
/// second copy that mirrors it: a copy is a fixture that can drift from what
/// actually spawns, which is exactly the gap this closes.
fn build_child_command(
    binary: &str,
    topology: &Topology,
    env: &ClaudeEnv,
    root: &Path,
    prompt: &str,
    extra: &[&str],
    resume: bool,
) -> tokio::process::Command {
    let plan = topology.plan(binary, root);
    let mut command = tokio::process::Command::new(&plan.program);
    command.args(&plan.leading);
    command.args(claude_argv(prompt, extra, resume));
    command.current_dir(root.join("wd"));

    // The backstop under [`Rig::spawn`]'s watchdog, and the reason F4 was a
    // defect rather than a style note: tokio's default is `false`, so before
    // this line every path out of `spawn` that dropped the `Child` — the
    // deadline above all — left the child running. Set here rather than in
    // `spawn` so the guard that reads it needs no process
    // ([`build_child_command_kills_on_drop`]).
    command.kill_on_drop(true);

    // Cleared and rebuilt from the generated map plus a named isolation set, not
    // inherited. `CLAUDE_CODE_REMOTE=true` — the ambient value inside the
    // container this repository's own sessions run in — makes the client present
    // that container's managed OAuth token to whatever `ANTHROPIC_BASE_URL`
    // names (§5.7), which here is a socket that records every header. The
    // allowlist below is the one
    // `the_childs_environment_is_the_generated_map_plus_the_isolation_vars`
    // checks against; change one without the other and that test goes red.
    command.env_clear();
    for (name, value) in env.vars() {
        command.env(name, value);
    }
    // Deliberately *not* in the generated map — see `claude_launch`'s "What this
    // deliberately does not set": they are deployment policy and process
    // isolation, not part of what makes a client reach roundhouse. Whoever
    // spawns the process owns them, and here that is this function.
    command.env("PATH", std::env::var("PATH").unwrap_or_default());
    command.env("HOME", root.join("home"));
    command.env("CLAUDE_CONFIG_DIR", root.join("home/.claude"));
    command.env("DISABLE_AUTOUPDATER", "1");
    command.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
    // Whatever else this topology's own process needs — for Chained, Relay's
    // isolated XDG state. Decided in [`Topology::plan`] with the program and the
    // argv, not branched on a second time here.
    for (name, value) in &plan.extra_env {
        command.env(name, value);
    }

    // Without this the child waits three seconds for piped input and says so on
    // stderr before proceeding — harmless, and exactly the kind of noise that
    // gets read as "the newest test hangs" under a bounded suite.
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    command
}

/// The XDG variables a chained run points at its own scratch.
///
/// Named as a constant rather than inlined because
/// [`the_chained_child_is_relay_wrapping_the_very_same_launch`] asserts the
/// child's key set with `==`, and a second copy of this list is exactly the
/// drift that assertion exists to catch.
const RELAY_STATE_VARS: [&str; 4] = [
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
];

/// The client's own argv, identical on both topologies.
///
/// The point of the split: under Chained this vector is handed to Relay after a
/// `--` and Relay splices its own `--plugin-dir` and `--settings` into it
/// (Relay evidence §A.7), so what the client is *asked* to
/// do is by construction the same thing on both paths and any difference in
/// outcome is Relay's doing rather than the harness's.
fn claude_argv(prompt: &str, extra: &[&str], resume: bool) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    if resume {
        // "The most recent conversation in the current directory", which is why
        // `current_dir` is set on every run and not only on this one.
        argv.push("--continue".into());
    }
    // `-p` is what makes this non-interactive, and non-interactive is what makes
    // the sentinel deterministic: §1.3's documented behaviour is that a resolved
    // API key is used without asking in print mode, and merely *offered* to
    // override a subscription in interactive mode.
    argv.extend(["-p", "--output-format", "json"].map(String::from));
    // **The prompt goes before `extra`, not after it.** `--allowedTools` is
    // variadic (`<tools...>`), so a prompt following it is parsed as one more
    // tool name and the run dies with "Input must be provided either through
    // stdin or as a prompt argument" — a message that reads like a harness that
    // forgot to pass a prompt, when the prompt was passed and eaten.
    argv.push(prompt.into());
    argv.extend(extra.iter().copied().map(String::from));
    argv
}

/// A scratch home for a version probe, thrown away as soon as it answers.
///
/// A probe cannot borrow the rig's root: the Relay probes run from tests that
/// have no rig at all, and the client probe runs inside `start_at` before the
/// root is fully furnished. Its own directory per call is what lets one
/// isolation rule cover every process this file spawns.
fn probe_home() -> PathBuf {
    std::env::temp_dir().join(format!("roundhouse-version-probe-{}", uuid::Uuid::new_v4()))
}

/// What `claude --version` prints, or a loud panic naming the override.
///
/// Isolated exactly as [`build_child_command`] isolates a real run (M11.2b
/// review F18): cleared, then `PATH`, a scratch `HOME`, an isolated
/// `CLAUDE_CONFIG_DIR`, and the two variables that stop a probe reaching the
/// network on its way to printing a number.
fn claude_version(binary: &str) -> String {
    let home = probe_home();
    std::fs::create_dir_all(home.join(".claude")).expect("the probe's isolated config directory");
    let version = version_probe(
        binary,
        &[
            ("HOME", home.clone().into_os_string()),
            ("CLAUDE_CONFIG_DIR", home.join(".claude").into_os_string()),
            ("DISABLE_AUTOUPDATER", OsString::from("1")),
            (
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
                OsString::from("1"),
            ),
        ],
        CLAUDE_BIN_VAR,
    );
    let _ = std::fs::remove_dir_all(&home);
    version
}

/// What `nemo-relay --version` prints, or a loud panic naming the override.
///
/// A missing Relay under `--include-ignored` is a hard failure and never a
/// silent skip, for the reason the whole suite is gated this way: a chained test
/// that quietly does not run reports "green" for the topology nobody checked.
///
/// Isolated for F18's reason and for one of its own: a probe that read the
/// developer's XDG configuration could print a version resolved under
/// configuration this test never wrote.
fn relay_version(binary: &str) -> String {
    let home = probe_home();
    std::fs::create_dir_all(&home).expect("the probe's isolated home");
    let mut isolation = vec![("HOME", home.clone().into_os_string())];
    isolation.extend(
        RELAY_STATE_VARS
            .iter()
            .map(|name| (*name, home.clone().into_os_string())),
    );
    let version = version_probe(binary, &isolation, RELAY_BIN_VAR);
    let _ = std::fs::remove_dir_all(&home);
    version
}

/// `nemo-relay run --agent claude --config <toml> [extra] --dry-run`, and what
/// it printed.
///
/// **Spawned with the ambient environment cleared** (M11.2b review F7). Relay
/// applies its `NEMO_RELAY_*` environment layer *above* the explicit `--config`
/// file, and a differing `NEMO_RELAY_ANTHROPIC_BASE_URL` also clears any
/// configured auth header — so a probe that inherited the operator's environment
/// could fail its own control assertion and read as Relay drift. The one thing
/// preserved is `PATH`, because the binary has to be able to find its loader.
///
/// `--dry-run` resolves configuration and exits without spawning the agent,
/// which is what makes every caller here need Relay and nothing else.
fn relay_dry_run(relay: &str, home: &Path, config: &Path, extra: &[&str]) -> String {
    let mut command = std::process::Command::new(relay);
    command.args(["run", "--agent", "claude", "--config"]);
    command.arg(config);
    command.args(extra);
    command.args(["--dry-run", "--", "-p", "probe"]);
    command.env_clear();
    command.env("PATH", std::env::var("PATH").unwrap_or_default());
    command.env("HOME", home);
    for name in RELAY_STATE_VARS {
        command.env(name, home);
    }
    let output = command.output().unwrap_or_else(|error| {
        panic!(
            "could not run `{relay}`: {error}. Set {RELAY_BIN_VAR} to a real Relay binary, or \
             drop --include-ignored."
        )
    });
    String::from_utf8_lossy(&output.stdout).into_owned()
}

// ---------------------------------------------------------------------------
// What one `claude -p` produced
// ---------------------------------------------------------------------------

struct ClaudeRun {
    /// The one JSON document `--output-format json` prints.
    result: Option<Value>,
    stdout: String,
    stderr: String,
    success: bool,
}

impl ClaudeRun {
    /// Fail unless the client completed the turn without an error.
    ///
    /// Three signals rather than the exit status alone: a non-zero exit, an
    /// `is_error` document, and a `permission_denials` entry all mean the run
    /// proved nothing, and each is diagnosed differently — the third in
    /// particular is what a client that started asking about `Read` would show.
    fn assert_completed(&self, what: &str) {
        let denials = self
            .result
            .as_ref()
            .and_then(|result| result["permission_denials"].as_array())
            .cloned()
            .unwrap_or_default();
        let errored = self
            .result
            .as_ref()
            .is_some_and(|result| result["is_error"] == Value::Bool(true));
        assert!(
            self.success && self.result.is_some() && !errored && denials.is_empty(),
            "{what}: the client did not complete the turn (exit ok: {}, is_error: {errored}, \
             permission denials: {denials:?})\n--- stdout\n{}\n--- stderr\n{}",
            self.success,
            self.stdout,
            self.stderr
        );
    }

    fn field(&self, name: &str) -> Value {
        self.result
            .as_ref()
            .map(|result| result[name].clone())
            .unwrap_or(Value::Null)
    }

    /// The final assistant text, which is what `-p` exists to print.
    fn text(&self) -> String {
        self.field("result")
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// The client's own session UUID, minted inside the child.
    fn session_id(&self) -> String {
        self.field("session_id")
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// How many assistant turns the client ran inside one invocation — two when
    /// it dispatched a tool and came back.
    fn turns(&self) -> u64 {
        self.field("num_turns").as_u64().unwrap_or_default()
    }
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
    let rig = Rig::start_at(root, reading_upstream(&canary), Wiring::Direct).await;
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
/// A structural claim gets a structural test, read from this file's own source,
/// counted the way the finding counted it. One site is [`Topology::plan`],
/// which answers program, argv, environment, deadline and label together.
#[test]
fn topology_is_dispatched_on_at_one_site() {
    let source = include_str!("claude_e2e.rs");

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
            .find("\n/// The XDG variables")
            .expect("the XDG-variables doc comment follows build_child_command");
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
#[test]
fn a_chained_rigs_topology_is_not_a_post_construction_mutation() {
    let source = include_str!("claude_e2e.rs");

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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_deadline_ends_a_child_that_ignores_the_interrupt() {
    let root = std::env::temp_dir().join(format!("f4-deadline-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("wd")).expect("the stub run's working directory");
    let stub = root.join("ignores-sigint.sh");
    std::fs::write(&stub, "#!/bin/sh\ntrap '' INT\nsleep 300\n").expect("the stub script writes");
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

    interrupt_then_kill(pid).await;
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("the killed child must be reapable rather than still running")
        .expect("waiting on a spawned child succeeds");

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
        .filter(|line| line.trim_start().starts_with("//"))
        .filter(|line| {
            relay_files
                .iter()
                .any(|file| line.contains(&format!("{file}:")))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "F10: Relay source is cited by file and line in this file; cite the evidence document's \
         section instead (see the module doc's \"Where Relay evidence §x points\"):\n{}",
        offenders.join("\n")
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
