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
//! Seven real-binary tests, each closing one thing the milestones below it
//! could only document. Four are the Direct topology:
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
//! …two more drive the Chained one, through a real `nemo-relay run --agent
//! claude`: [`a_chained_turn_reaches_roundhouse_with_the_turn_key_and_not_relays_own`]
//! (credential attribution and `?beta=true` survival across the hop) and
//! [`a_chained_continue_survives_relays_re_encode_without_forking`] (R7 hazard 1
//! against the real re-encoder). See "The chained topology" below.
//!
//! …and one more needs *only* Relay, not `claude` at all:
//! [`hazard_4_a_different_base_url_layer_clears_the_configured_auth_header`]
//! drives `nemo-relay run --dry-run` directly. See "Hazard 4, made detectable"
//! at the bottom of this file.
//!
//! Five more need no binary, because what they catch is this harness lying to
//! itself: [`the_childs_environment_is_the_generated_map_plus_the_isolation_vars`],
//! [`the_get_envs_diff_cannot_see_a_dropped_env_clear`],
//! [`the_chained_child_is_relay_wrapping_the_very_same_launch`],
//! [`the_fork_probe_names_a_session_a_fork_would_actually_create`] and
//! [`a_rigs_root_cannot_be_claimed_twice`].
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
//!   (`configuration/mod.rs:1672-1681`), so a deployment that names the base URL
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
//!   a turn to an explicit target (`gateway/mod.rs:874-908`), so a turn
//!   redirected by a plugin arrives carrying no forwarded seat whatever the
//!   client presented. No pass-through deployment may assume otherwise.
//!
//! **Resumption is not offered in band on this surface**, and R-D closes plan
//! open question 4 that way for this rung: Relay's SSE decoder ignores `id:`
//! lines outright (`codec/streaming.rs:182-198`), so a cursor carried as an SSE
//! id does not survive the hop — and the Messages emitter carries none, so there
//! is nothing to lose today and a documented reason not to add one tomorrow.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::Principal;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::now_ms;
use roundhouse_core::routing::{AffinityPolicy, Candidate, Target};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{FrontierClient, FrontierError, FrontierQuote, FrontierStream};
use roundhouse_server::claude_launch::{
    API_KEY_ENV, BASE_URL_ENV, CUSTOM_HEADERS_ENV, ClaudeEnv, ClaudeLaunch,
    ROUNDHOUSE_API_KEY_SENTINEL,
};
use roundhouse_server::control_config::{MembershipRole, TURN_KEY_HEADER};
use roundhouse_server::messages_api::MESSAGES_PATH;
use roundhouse_server::{
    API_PREFIX, ControlDirectory, Conversations, CrossChecks, DirectoryMutation, EchoLocalExecutor,
    Engine, EngineConfig, MemoryDirectoryStore, messages_router,
};

mod common;
use common::{Scripted, ToolCallingFrontierClient, frontier_catalog, sha256_hex};

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

/// The tenant every request below authenticates as.
const PROJECT: &str = "acme";
const USER: &str = "ada";

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
/// (`agents/claude/mod.rs:21`, `(2, 1, 121)`), writes a temporary plugin
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
/// (`gateway/response.rs:59-72`) — so this is the one header whose arrival here
/// would mean Relay's own credential had been handed to an upstream that is not
/// Relay.
const RELAY_PROXY_TOKEN_HEADER: &str = "x-nemo-relay-proxy-token";

/// The trailing per-request notice R-A rules is ephemeral (§5.7).
///
/// Spelled as its two anchors rather than as a whole string because the number
/// between them is what makes the notice ephemeral: it is a client-side counter
/// regenerated per request, so a test that matched the whole text would be
/// asserting today's remaining budget.
const NOTICE_OPEN: &str = "<total_tokens>";
const NOTICE_CLOSE: &str = "tokens left</total_tokens>";

// ---------------------------------------------------------------------------
// The scripted upstream
// ---------------------------------------------------------------------------

/// One dispatch's worth of script.
struct ScriptedTurn {
    /// Reused rather than re-implemented: [`ToolCallingFrontierClient`] already
    /// owns the chunk construction — interleaved text and calls, then a `Done`
    /// carrying a caller-chosen `stop_reason` — and a second copy of it here
    /// would be a double that drifts from the one every other Messages suite
    /// asserts against.
    client: ToolCallingFrontierClient,
}

impl ScriptedTurn {
    fn new(script: Vec<Scripted>, stop_reason: Option<&str>) -> Self {
        Self {
            client: ToolCallingFrontierClient::new(script, stop_reason),
        }
    }

    fn prose(text: &'static str) -> Self {
        Self::new(vec![Scripted::Text(text)], Some("end_turn"))
    }
}

/// A frontier that answers a queue of scripted turns and then prose forever.
///
/// **The tail is prose rather than a repeat of the last turn, and that is the
/// whole reason this type exists** rather than [`ToolCallingFrontierClient`]
/// being used directly. A real client answers a `tool_use` by running the tool
/// and dispatching again, so an upstream whose fixed script is a call answers
/// the resend with the same call and the loop never closes: the run ends at
/// [`CHILD_DEADLINE`] and reads as a hung client. Queue-then-prose makes
/// termination a property of the double instead of a hope about the client.
struct ScriptedTurns {
    queued: Mutex<VecDeque<ScriptedTurn>>,
    then: ScriptedTurn,
    /// Every quote this deployment dispatched, in call order — what roundhouse
    /// actually sent upstream, as opposed to what the client sent us.
    quotes: Mutex<Vec<FrontierQuote>>,
}

impl ScriptedTurns {
    /// Prose on every dispatch: the shape three of the four tests want.
    fn prose() -> Self {
        Self {
            queued: Mutex::new(VecDeque::new()),
            then: ScriptedTurn::prose(ANSWER),
            quotes: Mutex::new(Vec::new()),
        }
    }

    /// One turn that speaks and then calls `Read` on `path`, then prose.
    ///
    /// `path` is `&'static str` because [`Scripted`] holds its arguments that
    /// way — every other user of it writes a literal — and the file this rig
    /// writes only has a path at run time. The leak is one boxed string per rig
    /// in a test binary, which is bounded and deliberate; the alternative was to
    /// widen a shared fixture type for one caller.
    fn reading(path: &'static str) -> Self {
        let arguments: &'static str = Box::leak(
            serde_json::json!({ "file_path": path })
                .to_string()
                .into_boxed_str(),
        );
        Self {
            queued: Mutex::new(VecDeque::from([ScriptedTurn::new(
                vec![
                    Scripted::Text(BEFORE_THE_CALL),
                    Scripted::Call {
                        id: "toolu_e2e_01",
                        name: "Read",
                        arguments,
                    },
                ],
                Some("tool_use"),
            )])),
            then: ScriptedTurn::prose(ANSWER),
            quotes: Mutex::new(Vec::new()),
        }
    }

    fn dispatches(&self) -> usize {
        self.quotes.lock().expect("recording").len()
    }

    /// Whether any dispatch carried a *forwarded* caller credential upstream.
    ///
    /// Read from the quote rather than from the wire because that is where the
    /// decision is made: `turn_admission` captures a presented credential into
    /// `TurnCredential::Forwarded` and the dispatch client then puts it on the
    /// upstream request. The chained tests assert this is `false` — the launch
    /// sentinel arrives on `x-api-key` on every turn, and the serve side's rule
    /// that it is never mistaken for a seat is what stops a chained deployment
    /// forwarding `rh_sentinel_not_a_credential` to a frontier as if a tenant
    /// had brought their own.
    fn any_credential_forwarded(&self) -> bool {
        self.quotes
            .lock()
            .expect("recording")
            .iter()
            .any(|quote| quote.credential.is_forwarded())
    }
}

#[async_trait]
impl FrontierClient for ScriptedTurns {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.quotes.lock().expect("recording").push(quote.clone());
        // Popped and the guard dropped before the await: holding a `std::sync`
        // lock across an await point in a multi-threaded runtime is how a rig
        // deadlocks under `--test-threads=1` and gets diagnosed as a client hang.
        let queued = self.queued.lock().expect("recording").pop_front();
        match queued {
            Some(turn) => turn.client.execute(quote).await,
            None => self.then.client.execute(quote).await,
        }
    }
}

// ---------------------------------------------------------------------------
// The recorder
// ---------------------------------------------------------------------------

/// One request the deployment served, as it arrived.
#[derive(Clone, Debug)]
struct Exchange {
    path: String,
    /// The request's query string, if any.
    ///
    /// Kept apart from `path` rather than folded into it because the two are
    /// asserted for opposite reasons: every filter below matches on the path
    /// alone (axum routes past the query, and so must the filter), while the
    /// chained tests assert on the query alone — `?beta=true` surviving Relay's
    /// `format!("{base}{path_and_query}")` concatenation is R7 hazard 3, and a
    /// combined string would make "the route matched" and "the query survived"
    /// one assertion that cannot fail separately.
    query: Option<String>,
    headers: BTreeMap<String, String>,
    /// The request body, parsed if it was JSON.
    ///
    /// Parsed rather than kept as bytes for the reason the codex sibling gives:
    /// every assertion downstream is on a *value*, and a client re-serializes
    /// what it resends in its own field order.
    body: Option<Value>,
    status: u16,
    /// The response body as bytes-turned-text.
    ///
    /// Buffered on every path, which for `/v1/messages` means the child sees one
    /// turn's frames arrive at once instead of as they are produced. Stated
    /// rather than assumed: no assertion in this file is about frame *timing*,
    /// every turn is served by an in-process frontier and finishes in
    /// milliseconds, and the client parses a complete SSE body identically to an
    /// incremental one. What is traded away is this harness's fidelity to
    /// backpressure, which nothing here measures; what is bought is that the
    /// stream the client actually read is inspectable at all.
    response_text: Option<String>,
}

impl Exchange {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// The `messages` array of this request's body, in arrival order.
    fn messages(&self) -> Vec<Value> {
        self.body
            .as_ref()
            .and_then(|body| body["messages"].as_array())
            .cloned()
            .unwrap_or_default()
    }

    /// The SSE `data:` payloads of this response, parsed, in arrival order.
    fn frames(&self) -> Vec<Value> {
        self.response_text
            .as_deref()
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
            .collect()
    }

    /// The headers as an evidence block or a failure message should print them:
    /// credential-bearing values replaced by their length.
    ///
    /// The turn key here is one this test minted seconds earlier, so printing it
    /// whole would cost nothing *today*. Redacted anyway, because the shape of
    /// this block is what a fixture holding something real would copy, and the
    /// diagnostic — "the header arrived, and was this big" — is what a printed
    /// header set is for.
    fn redacted_headers(&self) -> BTreeMap<String, String> {
        self.headers
            .iter()
            .map(|(name, value)| {
                let value = match name.as_str() {
                    "authorization" | TURN_KEY_HEADER => {
                        format!("<{} bytes redacted>", value.len())
                    }
                    _ => value.clone(),
                };
                (name.clone(), value)
            })
            .collect()
    }
}

/// Every request the deployment served, in arrival order.
#[derive(Clone, Default)]
struct Recorder {
    exchanges: Arc<Mutex<Vec<Exchange>>>,
}

impl Recorder {
    fn all(&self) -> Vec<Exchange> {
        self.exchanges.lock().expect("recording").clone()
    }

    /// Every turn request, in arrival order.
    ///
    /// Matched on the path alone: the client appends `?beta=true`, which axum
    /// routes past and this filter must too — a comparison against the whole URI
    /// would find nothing and every assertion below would fail as "the client
    /// never sent a turn".
    fn turns(&self) -> Vec<Exchange> {
        self.all()
            .into_iter()
            .filter(|exchange| exchange.path == format!("{API_PREFIX}/{MESSAGES_PATH}"))
            .collect()
    }

    /// A one-line rendering of every exchange, for a failure message.
    fn transcript(&self) -> String {
        self.all()
            .iter()
            .map(|exchange| format!("POST {} -> {}", exchange.path, exchange.status))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Capture what arrived, without changing what is served.
async fn record(State(recorder): State<Recorder>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let query = request.uri().query().map(str::to_string);
    let headers = request
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or("<non-utf8>").to_string(),
            )
        })
        .collect();

    let (parts, body) = request.into_parts();
    // Generously bounded: one turn of this client is already ~70 KB of system
    // prompt and tool schemas, and a resend carries the whole history. A silent
    // truncation here would surface as a 422 from our own canonicalizer, which
    // reads exactly like a roundhouse bug and is not one.
    let bytes = axum::body::to_bytes(body, 32 * 1024 * 1024)
        .await
        .expect("a loopback client's request body is readable");
    let parsed = serde_json::from_slice::<Value>(&bytes).ok();
    let response = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;

    let status = response.status().as_u16();
    let (mut response_parts, response_body) = response.into_parts();
    let bytes = axum::body::to_bytes(response_body, 32 * 1024 * 1024)
        .await
        .expect("a loopback response body is readable");
    // The body just went from streamed to definite-length. Any framing header
    // the streaming response carried would now describe a body that no longer
    // exists, and hyper would serialize the mismatch rather than reconcile it —
    // a corrupt response the child reports as a protocol error, which reads like
    // a roundhouse bug and is not one.
    response_parts.headers.remove("transfer-encoding");
    response_parts.headers.remove("content-length");
    let text = String::from_utf8(bytes.to_vec()).ok();

    recorder
        .exchanges
        .lock()
        .expect("recording")
        .push(Exchange {
            path,
            query,
            headers,
            body: parsed,
            status,
            response_text: text,
        });
    Response::from_parts(response_parts, Body::from(bytes))
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

impl Topology {
    /// How long one run of this topology may take.
    fn deadline(&self) -> Duration {
        match self {
            Self::Direct => CHILD_DEADLINE,
            Self::Chained { .. } => CHAINED_DEADLINE,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Direct => "Direct",
            Self::Chained { .. } => "Chained",
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
    /// This deployment's root, as an operator would hand it to a client or to
    /// Relay's `[upstream] anthropic_base_url`. No `{API_PREFIX}`: both readers
    /// append the version segment themselves, one by SDK default and one by
    /// concatenating the inbound `path_and_query` whole.
    base_url: String,
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
        Self::start_at(Self::a_root_for(label), upstream).await
    }

    /// The same deployment, driven through a real `nemo-relay run --agent
    /// claude` (R-D, R-D′).
    ///
    /// **The client's environment is the same [`ClaudeEnv`] the Direct tests
    /// use, and that is the ruling this constructor exists to instantiate.**
    /// Relay overwrites `ANTHROPIC_BASE_URL` with its own gateway and *merges*
    /// its proxy token into `ANTHROPIC_CUSTOM_HEADERS` rather than replacing the
    /// block (`agents/claude/launch.rs:19-31,113-127` — `replace_custom_header`
    /// drops only a line whose name matches), so the turn key survives the hop
    /// on [`TURN_KEY_HEADER`] and a chained turn keeps exactly Direct's
    /// semantics. One generator, two topologies.
    ///
    /// The config written here is the whole chained contract, and each line is a
    /// ruling:
    ///
    /// - `[upstream] anthropic_base_url` is this deployment's **root**. Relay
    ///   concatenates the inbound `path_and_query` onto it whole
    ///   (`gateway/routes.rs:141-151`), so a value carrying `/v1` would send
    ///   `/v1/v1/messages`.
    /// - **No `anthropic_auth_header`.** Relay injects one only when the inbound
    ///   request carries none of `authorization` / `x-api-key` / `api-key` /
    ///   `anthropic-api-key` (`gateway/mod.rs:1070-1078`, the `already_authed`
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
    /// (`server/mod.rs:92-97`, new at this release), so naming one here could
    /// only make the run fail in a way the evidence already predicts.
    async fn start_chained(label: &str, upstream: Arc<ScriptedTurns>) -> Self {
        let mut rig = Self::start_at(Self::a_root_for(label), upstream).await;
        let relay = std::env::var(RELAY_BIN_VAR).unwrap_or_else(|_| {
            panic!(
                "the chained topology needs a real Relay binary: set {RELAY_BIN_VAR}, or run \
                 without --include-ignored"
            )
        });
        let version = relay_version(&relay);
        let config = rig.root.join("relay-config.toml");
        std::fs::write(
            &config,
            format!(
                "[upstream]\nanthropic_base_url = \"{}\"\n\n[agents.claude]\ncommand = \"{}\"\n",
                rig.base_url, rig.binary
            ),
        )
        .expect("the run's Relay configuration");
        // Relay's own XDG state, beside the client's `HOME` rather than inside
        // it: Relay writes a session store and a resolved-config cache, and a
        // stray file under `CLAUDE_CONFIG_DIR` is exactly the kind of ambient
        // input this suite's isolation exists to exclude.
        std::fs::create_dir_all(rig.root.join("relay")).expect("the run's Relay state directory");

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

        rig.topology = Topology::Chained { relay, config };
        rig
    }

    /// Where a run of `label` puts its home and working directory.
    ///
    /// Public to the file rather than inlined into [`Self::start`] because one
    /// test needs the path *before* the rig exists: the scripted upstream names
    /// the file the client will read, and [`Scripted`] holds that name as a
    /// `&'static str` decided at script-construction time.
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
    async fn start_at(root: PathBuf, upstream: Arc<ScriptedTurns>) -> Self {
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

        // Bootstrap is file-only: `admin_keys` in the file is the sole root of
        // trust, and a directory with no admin plane refuses to mint.
        let admin = common::admin_key("root");
        let file = common::control_plane(
            serde_json::json!({
                "projects": [],
                "users": [],
                "admin_keys": [sha256_hex(&admin)],
                "arm_salt": "m11-claude-e2e",
            }),
            "claude-e2e bootstrap",
        );
        let directory = Arc::new(
            ControlDirectory::new(
                file,
                "ROUNDHOUSE_CONTROL_PLANE",
                Arc::new(MemoryDirectoryStore::new()),
                // No judge: no project here enrols its sessions in validation,
                // and promising one the cross-check would then have to find
                // would be fixture state this suite makes no claim about.
                CrossChecks::new(reachable(), None),
                now_ms(),
            )
            .expect("the bootstrap file alone compiles"),
        );
        directory
            .apply(
                DirectoryMutation::CreateProject {
                    entry: serde_json::from_value(serde_json::json!({ "id": PROJECT }))
                        .expect("the project entry is the file vocabulary"),
                },
                now_ms(),
            )
            .expect("creating a project");
        directory
            .apply(
                DirectoryMutation::CreateUser {
                    entry: serde_json::from_value(serde_json::json!({ "id": USER }))
                        .expect("the user entry is the file vocabulary"),
                },
                now_ms(),
            )
            .expect("creating a user");
        directory
            .apply(
                DirectoryMutation::UpsertMembership {
                    project: PROJECT.to_string(),
                    user: USER.to_string(),
                    role: MembershipRole::Member,
                    allocation: None,
                    overrides: None,
                },
                now_ms(),
            )
            .expect("enrolling the member");
        let minted = directory
            .mint_turn_key(PROJECT, USER, now_ms())
            .expect("the admin plane mints");

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

        Self {
            root,
            secret: minted.secret,
            env,
            store,
            conversations,
            recorder,
            upstream,
            binary,
            base_url,
            topology: Topology::Direct,
        }
    }

    /// The principal every request below resolves to.
    fn principal(&self) -> Principal {
        Principal::new(PROJECT, USER)
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
        self.conversations
            .latest(&self.principal())
            .expect("the client drove at least one turn")
    }

    /// The session's committed items, in log order.
    async fn items(&self) -> Vec<Item> {
        self.store
            .read_events(&self.session(), 0, 4096)
            .await
            .expect("the session exists")
            .into_iter()
            .filter_map(|event| match event.kind {
                SessionEventKind::ItemAppended { item } => Some(item),
                _ => None,
            })
            .collect()
    }

    /// A fork is silent from the client's side, so the only way to catch one is
    /// to ask the store whether generation one exists at all.
    ///
    /// Two assertions rather than one, because they fail on different evidence.
    /// The first reads the binding: `Conversations::fork` moves `latest` to the
    /// forked id, so a session id that still carries no generation suffix is this
    /// node's own statement that nothing rebound. The second reads the store,
    /// which does not depend on the binding table being right about itself.
    async fn assert_never_forked(&self) {
        let session = self.session();
        let probe = fork_probe(&session);
        assert_eq!(
            session,
            base_session(&session),
            "the client's resend must have matched its prefix: this principal's latest session \
             is `{session}`, and a generation suffix means the prefix check refused the claim \
             and rebound the conversation"
        );
        assert!(
            self.store.last_seq(&probe).await.is_err(),
            "the client's resend must have matched its prefix: `{probe}` exists, which means \
             the prefix check refused the claim and rebound the conversation"
        );
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
        let deadline = self.topology.deadline();

        let output = tokio::time::timeout(deadline, command.output())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "a {} `{} -p` did not finish within {deadline:?}. HOME: {}",
                    self.topology.label(),
                    self.binary,
                    self.root.join("home").display()
                )
            })
            .unwrap_or_else(|error| {
                panic!(
                    "could not run `{}`: {error}. Set {CLAUDE_BIN_VAR} to a real claude binary, \
                     or drop --include-ignored.",
                    self.binary
                )
            });

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
    ///
    /// Called explicitly at the end of a passing test rather than from a `Drop`:
    /// a guard fires on unwind too, which would delete the isolated home and the
    /// session store of the run that just failed — the only two artefacts worth
    /// having at that moment.
    fn clean(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

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

/// The generation-zero id behind `session`, whatever generation it is at.
///
/// `conversations::bound_session` spells generation zero as the namespaced key
/// verbatim and every later generation as `{key}#g{n}`, so the suffix *is* the
/// fork and stripping it recovers the stem. Sound here because the stem is
/// `{project}/{user}/anthropic_messages/{uuid}` and a UUID carries no `#`.
fn base_session(session: &SessionId) -> SessionId {
    match session.as_str().split_once("#g") {
        Some((base, _)) => SessionId::new(base),
        None => session.clone(),
    }
}

/// The session id a first fork of `session`'s conversation would have created.
///
/// A free function rather than a method on [`Rig`] so the guard it powers can be
/// evaluated without a rig, a binary or a socket — an arithmetic no test can
/// evaluate is how a vacuous fork probe survives. Derived from [`base_session`]
/// and never from `Conversations::latest`: a fork moves `latest` to the forked id
/// *before* any assertion runs, so appending `#g1` to it asks about `key#g1#g1`,
/// which nothing ever creates and whose absence therefore says nothing.
fn fork_probe(session: &SessionId) -> SessionId {
    SessionId::new(format!("{}#g1", base_session(session)))
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
    let client_argv = claude_argv(prompt, extra, resume);
    let mut command = match topology {
        Topology::Direct => {
            let mut command = tokio::process::Command::new(binary);
            command.args(&client_argv);
            command
        }
        // `run` rather than the bare `claude` shortcut: the shortcut runs an
        // interactive setup wizard when no config layer exists
        // (`commands/run.rs`'s `easy_path`, `needs_setup`), and a wizard in a
        // test rig is a hang. `--` is not optional — `RunCommand::command` is
        // `#[arg(last = true)]`, so without it the client's own flags are parsed
        // as Relay's.
        Topology::Chained { relay, config } => {
            let mut command = tokio::process::Command::new(relay);
            command.args(["run", "--agent", "claude", "--config"]);
            command.arg(config);
            command.arg("--");
            command.args(&client_argv);
            command
        }
    };
    command.current_dir(root.join("wd"));

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
    if matches!(topology, Topology::Chained { .. }) {
        // Relay's own state, isolated the same way and for the same reason the
        // client's is. `--config` replaces only the *user* config layer
        // (`nemo-relay --help`: "system config still applies"), and Relay reads
        // an XDG user layer, writes a session store, and caches resolved
        // configuration — all of which would otherwise land in whatever `HOME`
        // this box's developer happens to have, and be read back by the next
        // run as configuration this test never wrote.
        for name in RELAY_STATE_VARS {
            command.env(name, root.join("relay"));
        }
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
/// (`agents/claude/launch.rs:83-92,100-111`), so what the client is *asked* to
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

/// Every target this deployment can route to, priced the way the router prices
/// them.
///
/// The one model [`frontier_catalog`] declares and nothing else: no fleet is
/// attached, so a turn has exactly one place to go and "which target answered"
/// is never a race.
fn reachable() -> Vec<Candidate> {
    vec![Candidate {
        target: Target::Frontier {
            provider: "anthropic".into(),
            model: "claude".into(),
        },
        expected_prefill_tokens: 1_024.0,
        matched_prefix_tokens: 0,
        expected_ttft_ms: 1.0,
        expected_cost_usd: 0.0,
        quality_prior: 0.95,
        load: None,
    }]
}

/// What `claude --version` prints, or a loud panic naming the override.
fn claude_version(binary: &str) -> String {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "--include-ignored asks for the real binary; `{binary} --version` failed: \
                 {error}. Set {CLAUDE_BIN_VAR} to one, or run without --include-ignored."
            )
        });
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// What `nemo-relay --version` prints, or a loud panic naming the override.
///
/// A missing Relay under `--include-ignored` is a hard failure and never a
/// silent skip, for the reason the whole suite is gated this way: a chained test
/// that quietly does not run reports "green" for the topology nobody checked.
fn relay_version(binary: &str) -> String {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "--include-ignored asks for the real Relay; `{binary} --version` failed: \
                 {error}. Set {RELAY_BIN_VAR} to one, or run without --include-ignored."
            )
        });
    String::from_utf8_lossy(&output.stdout).trim().to_string()
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
/// Matched by role and by the notice's two anchors rather than by position: the
/// claim is about *which* message the client appends, and "the last one" would
/// be satisfied by any trailing message at all.
fn is_the_budget_notice(message: &Value) -> bool {
    message["role"] == "system"
        && text_blocks(message)
            .iter()
            .any(|text| text.trim().starts_with(NOTICE_OPEN) && text.trim().ends_with(NOTICE_CLOSE))
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
    let rig = Rig::start("prose", Arc::new(ScriptedTurns::prose())).await;

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
    let turns = rig.recorder.turns();
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
    // The script names the file before the rig exists, because `Scripted` holds
    // its arguments as `&'static str`. The rig writes the file itself, and the
    // assertion below is what keeps the two from drifting apart.
    let root = Rig::a_root_for("tools");
    let canary: &'static str = Box::leak(
        root.join("wd")
            .join(CANARY_FILE)
            .to_string_lossy()
            .into_owned()
            .into_boxed_str(),
    );
    let upstream = Arc::new(ScriptedTurns::reading(canary));
    let rig = Rig::start_at(root, upstream).await;
    assert_eq!(
        rig.canary_path().to_string_lossy(),
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
    let turns = rig.recorder.turns();
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
        call["input"]["file_path"], canary,
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
    let rig = Rig::start("continue", Arc::new(ScriptedTurns::prose())).await;

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

    let turns = rig.recorder.turns();
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
            ItemContent::Text { text } if text.trim().starts_with(NOTICE_OPEN) => {
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
    let rig = Rig::start("seat", Arc::new(ScriptedTurns::prose())).await;
    let run = rig.print("Say the word alpha and stop.", &[]).await;
    run.assert_completed("the seat-evidence turn");

    let turns = rig.recorder.turns();
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
/// (`agents/claude/launch.rs:19-31`), forwards unknown request headers
/// untouched (`gateway/response.rs:59-72`), and strips only its own credential
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
    let rig = Rig::start_chained("chained", Arc::new(ScriptedTurns::prose())).await;

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

    let turns = rig.recorder.turns();
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
         (gateway/routes.rs:141-151); path was `{}`",
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
    let rig = Rig::start_chained("chained-continue", Arc::new(ScriptedTurns::prose())).await;

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

    let turns = rig.recorder.turns();
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
/// as the module doc's citation of `configuration/mod.rs:1672-1681` predicts.
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
    let dry_run = |second_layer_base_url: Option<&str>| -> String {
        let mut command = std::process::Command::new(&relay);
        command.args(["run", "--agent", "claude", "--config"]);
        command.arg(&config);
        if let Some(url) = second_layer_base_url {
            command.args(["--anthropic-base-url", url]);
        }
        command.args(["--dry-run", "--", "-p", "probe"]);
        let output = command.output().unwrap_or_else(|error| {
            panic!(
                "could not run `{relay}`: {error}. Set {RELAY_BIN_VAR} to a real Relay binary, \
                 or drop --include-ignored."
            )
        });
        String::from_utf8_lossy(&output.stdout).into_owned()
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
