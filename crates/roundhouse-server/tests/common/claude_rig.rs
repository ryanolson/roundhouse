// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The deployment a real `claude` is stood inside, and the child it becomes.
//!
//! Split out of [`claude_e2e`](../claude_e2e.rs) (M12 review F11) once that file
//! had passed 4 700 lines and three consecutive milestones had each added to it.
//! The seam was already there and named: everything below stands a deployment up
//! and drives a process, and everything left behind is a claim about what came
//! back. A file that holds both grows a rig by accretion, because the reader
//! arriving to add a test never has to see how much harness they are adding to.
//!
//! **Not [`e2e`](super::e2e), deliberately.** That module holds what both
//! real-binary suites share — recorder, bootstrap, fork probe, version probe —
//! and its own doc says what stays per-suite: "the client's own argv and launch
//! generator, the topology". This file is exactly that per-suite half for
//! `claude`, so putting it beside the codex suite's would recreate the drift
//! M11.2b review F1 removed rather than avoid it.
//!
//! **What did not come with it: every assertion.** The structural guards that
//! read source — [`Topology`] branched at one site, `start_chained` constructing
//! rather than mutating — stay in the suite and now scan *this* file, which is
//! the part of the extraction that was never mechanical (F11's own refutation).
//! A guard that moved here with the code it guards would be a file checking
//! itself.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{MemorySpendLedger, Principal, SpendLedger};
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::Item;
use roundhouse_core::now_ms;
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::FrontierClient;
use roundhouse_mcp::ControlStore;
use roundhouse_server::claude_launch::{ClaudeEnv, ClaudeLaunch};
// The variable a launch profile names as where the turn key is read from, taken
// from the generator that defines it rather than spelled here.
use roundhouse_server::codex_launch::DEFAULT_KEY_ENV;
use roundhouse_server::mcp_api::MCP_MOUNT_PATH;
use roundhouse_server::messages_api::MESSAGES_PATH;
// R-T5: the chained wiring this rig writes is a rendering in the library, not a
// template spelled here. `topham relay` consumes the same one, so a rig that
// went green against a config the launcher does not produce is not a state this
// file can reach.
use roundhouse_server::relay_handoff::{RELAY_STATE_VARS, RelayAgent, RelayHandoff};
use roundhouse_server::test_support::bind_conversation;
use roundhouse_server::{
    API_PREFIX, ControlPlaneReads, Conversations, EchoLocalExecutor, Engine, EngineConfig,
    mcp_router, messages_router,
};

use super::e2e::{
    Exchange, PROJECT, Recorder, TOPHAM_BIN_VAR, USER, bootstrap, clean, path_with, principal,
    probe_home, reachable, record, topham_binary, topham_version, version_probe, write_profile,
    xdg,
};
use super::{ScriptedTurns, frontier_catalog};

/// What the file the client is asked to read contains.
///
/// Nothing else in this rig, in the client, or in either prompt can produce this
/// string, so finding it in the request body that arrives *after* the tool call
/// is evidence that the real client opened the real file — not that it guessed,
/// and not that it echoed the prompt.
pub const CANARY: &str = "the-canary-line-roundhouse-wrote";

/// The file the scripted upstream asks the client to read.
pub const CANARY_FILE: &str = "canary.txt";

///
/// Generous against a measured baseline of about a second: the child starts a
/// Node runtime, resolves its settings, and runs one or two turns against a
/// loopback socket. A deadline of its own rather than only the suite's outer
/// `timeout` because the outer one reports "the suite hung" and this one reports
/// which run hung, with that run's stderr and the `HOME` to inspect.
pub const CHILD_DEADLINE: Duration = Duration::from_secs(90);

/// The environment variable that overrides which binary is driven.
pub const CLAUDE_BIN_VAR: &str = "ROUNDHOUSE_TEST_CLAUDE_BIN";

/// The version this suite's assertions were written against.
///
/// Compared against the first whitespace-delimited token of `claude --version`,
/// which at this line prints `2.1.257 (Claude Code)`.
pub const VERIFIED_VERSION: &str = "2.1.257";

/// How long a chained `nemo-relay run --agent claude` may take.
///
/// Longer than [`CHILD_DEADLINE`] because the chain is three processes rather
/// than two: Relay resolves its configuration, binds an ephemeral loopback
/// gateway, runs `claude --version` for its own minimum-version gate
/// (Relay evidence §A.7), writes a temporary plugin
/// directory and a synthesized `--settings` document, and only then spawns the
/// client this suite is actually about.
pub const CHAINED_DEADLINE: Duration = Duration::from_secs(180);

/// The environment variable that names the Relay binary the chained tests drive.
pub const RELAY_BIN_VAR: &str = "ROUNDHOUSE_TEST_RELAY_BIN";

/// The Relay release the chained assertions were written against.
///
/// Compared against the last whitespace-delimited token of
/// `nemo-relay --version`, which at this line prints `nemo-relay 0.8.2`. The
/// evidence the chained tests rest on is
/// `agent-docs/research/nemo-relay-0.8.0-published-read.md`'s 2026-09-01
/// addendum, which re-derived every hazard below against exactly this tarball.
pub const VERIFIED_RELAY_VERSION: &str = "0.8.2";

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
pub enum Wiring {
    Direct,
    Chained,
}

pub enum Topology {
    /// The client is spawned directly, pointed at roundhouse.
    Direct,
    /// The client is spawned by `nemo-relay run --agent claude`, which points it
    /// at Relay's own loopback gateway and forwards to roundhouse.
    Chained {
        /// The Relay binary, from [`RELAY_BIN_VAR`].
        relay: String,
        /// The `config.toml` aiming Relay's Anthropic upstream at roundhouse.
        config: PathBuf,
        /// The rendering that wrote it, and the source of the argv below.
        ///
        /// Kept rather than discarded after the write so that this rig's
        /// `--agent`, its `--config` placement and its trailing `--` come from
        /// the same value `topham relay` execs with — the argv is as much part
        /// of the shared seam as the four lines of TOML are.
        handoff: RelayHandoff,
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
pub struct Launch {
    /// The program actually spawned.
    pub program: String,
    /// Whatever must precede the client's own argv, `--` included.
    pub leading: Vec<String>,
    /// Environment this topology's own process needs, beyond the launch map and
    /// the client's isolation set.
    pub extra_env: Vec<(&'static str, PathBuf)>,
    /// How long one run of this topology may take.
    pub deadline: Duration,
    pub label: &'static str,
}

impl Topology {
    pub fn plan(&self, binary: &str, root: &Path) -> Launch {
        match self {
            Self::Direct => Launch {
                program: binary.to_string(),
                leading: Vec::new(),
                extra_env: Vec::new(),
                deadline: CHILD_DEADLINE,
                label: "Direct",
            },
            // The `run … --` argv comes from the handoff rather than being
            // spelled here (R-T5): why it is `run` and not the bare `claude`
            // shortcut, and why the trailing `--` is not optional, are facts
            // about Relay that `topham relay` needs to be right about too, so
            // they live with the rendering in `relay_handoff` and not in a test.
            Self::Chained {
                relay,
                config,
                handoff,
            } => Launch {
                program: relay.clone(),
                leading: handoff.run_argv(config),
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

/// Whether a rig opens a **rival conversation** for the same principal in
/// front of every control call.
///
/// R-M6's negative is the whole reason this exists. An MCP call correlated by
/// tool-use id and one resolved by "the principal's most recent conversation"
/// give the *same* answer on an ordinary single-conversation run — the client
/// makes its `tools/call` between the turn that emitted the call and the turn
/// that answers it, and nothing else has moved `latest` in between. So an
/// assertion that the id is what answered would pass on a deployment where the
/// id is ignored entirely, which is a test that proves nothing.
///
/// [`ControlRace::RivalIsLatest`] removes that by *making* the guess wrong: a
/// second conversation of the same principal's takes the `latest` slot
/// immediately before each `/mcp` request is served. That is not a contrivance
/// — it is the parent-and-subagent race R-M2 exists to remove, run
/// deterministically rather than hoped for. The counterfactual half (the same
/// call answered *without* the id resolves to the guess) is pinned at the seam
/// by `mcp_api::tests::a_tool_use_id_resolves_the_conversation_that_emitted_it`;
/// what this rig adds is that a real client really does send the id.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ControlRace {
    /// None. `latest` and the tool-use id name one session.
    None,
    /// One, bound in front of every control call.
    RivalIsLatest,
}

/// The rival conversation, and what it displaced each time it took the slot.
pub struct Rival {
    pub conversations: Arc<Conversations>,
    pub principal: Principal,
    /// The cache key the rival is bound under, and the session it resolves to.
    pub key: String,
    pub session: SessionId,
    /// The JSON-RPC method of each control request, with `latest` as it stood
    /// immediately *before* that request was served.
    ///
    /// The method is carried because the sequence matters and the entries are
    /// not interchangeable: the handshake happens before the client's first
    /// turn exists, so the only entry whose displaced value can be the client's
    /// own conversation is the `tools/call` the tool loop makes. An assertion
    /// over the list as a whole would pass on a client that never made the call
    /// at all.
    pub seen: Mutex<Vec<(String, Option<SessionId>)>>,
}

/// Take the `latest` slot, and record what was in it.
///
/// **Before `next.run`, which is what makes the claim exact**: every request the
/// control surface serves under this layer is served with the rival as this
/// principal's most recent conversation, so an answer naming anything else can
/// only have come from the tool-use id.
///
/// The body is buffered and put back rather than peeked, for the reason
/// `common::e2e::record` buffers: a JSON-RPC method is in the body and axum
/// hands a layer the stream, not the bytes. The bound is small because a
/// `tools/call` is a few hundred bytes — the turn bodies, which are tens of
/// kilobytes, never reach this route.
async fn take_the_latest_slot(
    State(rival): State<Arc<Rival>>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .expect("a loopback control request body is readable");
    let method = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|body| body["method"].as_str().map(str::to_string))
        .unwrap_or_else(|| "<no jsonrpc method>".to_string());
    rival
        .seen
        .lock()
        .expect("recording")
        .push((method, rival.conversations.latest(&rival.principal)));
    bind_conversation(&rival.conversations, &rival.principal, &rival.key).await;
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

impl Rival {
    /// What this principal's `latest` held immediately before the one control
    /// request whose JSON-RPC method is `method`.
    ///
    /// Exactly one, and a panic printing the whole sequence otherwise. "The
    /// client never made that call" and "the client made two" are different
    /// defects, neither is legible from a missing `Option`, and the order of
    /// the handshake against the turns is precisely what a reader needs when
    /// the correlation assertion goes red.
    pub fn displaced_before(&self, method: &str) -> Option<SessionId> {
        let seen = self.seen.lock().expect("recording");
        let matching: Vec<&(String, Option<SessionId>)> =
            seen.iter().filter(|(seen, _)| seen == method).collect();
        assert_eq!(
            matching.len(),
            1,
            "exactly one `{method}` reaches the control surface in this run; it saw:\n{}",
            seen.iter()
                .map(|(method, displaced)| format!(
                    "    {method} (latest before it: {})",
                    displaced
                        .as_ref()
                        .map(SessionId::as_str)
                        .unwrap_or("<none>")
                ))
                .collect::<Vec<_>>()
                .join("\n")
        );
        matching[0].1.clone()
    }
}

/// A live roundhouse, its filesystem, and everything needed to read it back.
pub struct Rig {
    /// Where this run's `HOME`, `CLAUDE_CONFIG_DIR` and working directory live.
    pub root: PathBuf,
    /// The minted turn key, in plaintext — the value the generated environment
    /// carries into [`TURN_KEY_HEADER`], and the only place it exists outside
    /// the directory's hash.
    pub secret: String,
    /// This deployment's **root**, with no [`API_PREFIX`].
    ///
    /// Kept rather than recovered from [`Self::env`] because the `topham` tests
    /// need it as a *profile field*, which is a string an operator types — and
    /// reading it back out of the generated map would make the profile a
    /// restatement of the launch it is supposed to produce.
    pub base_url: String,
    /// The environment a launched client is given, generated rather than
    /// re-spelled. Consuming [`ClaudeLaunch`]'s own output is what makes this
    /// suite evidence about the launcher and not only about the surface.
    pub env: ClaudeEnv,
    pub store: Arc<MemoryStore>,
    pub conversations: Arc<Conversations>,
    /// The rival conversation, when this rig was asked for one. See
    /// [`ControlRace`].
    pub rival: Option<Arc<Rival>>,
    pub recorder: Recorder,
    pub upstream: Arc<ScriptedTurns>,
    pub binary: String,
    pub topology: Topology,
}

impl Rig {
    /// A run rooted wherever this deployment's own convention puts it.
    ///
    /// Under the system temp directory, never under `target/`. See the module
    /// doc: this client walks up from its working directory for a CLAUDE.md, a
    /// project `.claude/` and a git repository, so a run rooted in this checkout
    /// would launch carrying this repository as context.
    pub async fn start(label: &str, upstream: Arc<ScriptedTurns>) -> Self {
        Self::start_at(
            Self::a_root_for(label),
            upstream,
            Wiring::Direct,
            ControlRace::None,
        )
        .await
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
    pub async fn start_chained(label: &str, upstream: Arc<ScriptedTurns>) -> Self {
        Self::start_at(
            Self::a_root_for(label),
            upstream,
            Wiring::Chained,
            ControlRace::None,
        )
        .await
    }

    /// Where a run of `label` puts its home and working directory.
    ///
    /// Public to the file rather than inlined into [`Self::start`] because one
    /// test needs the path *before* the rig exists: the scripted upstream names
    /// the file the client will read, and it has to be scripted before the rig
    /// that writes it is built.
    pub fn a_root_for(label: &str) -> PathBuf {
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
    pub async fn start_at(
        root: PathBuf,
        upstream: Arc<ScriptedTurns>,
        wiring: Wiring,
        race: ControlRace,
    ) -> Self {
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
        )
        .await;
        let directory = deployment.directory;
        let minted = deployment.minted;

        let store = Arc::new(MemoryStore::new());
        let conversations = Arc::new(Conversations::new());
        // One control store behind both routers, exactly as `main::serve`
        // shares it: the surface writes an overlay and the engine reads it, and
        // a second copy of either half is a control plane reporting on a
        // deployment adjacent to the one serving turns.
        let control = Arc::new(ControlStore::new());
        let spend: Arc<dyn SpendLedger> = Arc::new(MemorySpendLedger::new());
        let arm_salt = directory.plane(now_ms()).await.arm_salt().to_string();
        let engine = Arc::new(
            Engine::new(
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
            )
            .with_control_store(Arc::clone(&control)),
        );

        // The rival, bound and *made real in the store* before anything is
        // spawned. Real because the failure R-M2 removes is not a refusal: a
        // surface that fell back to `latest` here would answer perfectly well,
        // about the wrong conversation, and only the id in the answer would
        // say so. A rival that existed only in the binding table would make
        // that fallback fail loudly instead, which is a weaker claim than the
        // one this rig is for.
        let rival = match race {
            ControlRace::None => None,
            ControlRace::RivalIsLatest => {
                let key = format!("{PROJECT}/{USER}/a-rival-conversation");
                let session = bind_conversation(&conversations, &principal(), &key).await;
                store
                    .create_session(&session, "rival")
                    .await
                    .expect("the rival conversation's own log");
                Some(Arc::new(Rival {
                    conversations: Arc::clone(&conversations),
                    principal: principal(),
                    key,
                    session,
                    seen: Mutex::new(Vec::new()),
                }))
            }
        };

        let mut mcp = mcp_router(
            Arc::clone(&directory),
            Arc::new(ControlPlaneReads::new(
                Arc::clone(&directory),
                Arc::clone(&store),
                spend,
                Arc::clone(&conversations),
                reachable(),
            )),
            Arc::clone(&control),
        )
        .await;
        if let Some(rival) = &rival {
            mcp = mcp.layer(axum::middleware::from_fn_with_state(
                Arc::clone(rival),
                take_the_latest_slot,
            ));
        }

        let recorder = Recorder::default();
        // Both routers behind one recorder, for the reason `mcp_surface`'s own
        // rig merges them: the interleaving *is* the subject here — a turn that
        // emits a control call, the client's `tools/call` against `/mcp`, and
        // the resend that follows — and three recorders could not say what
        // order those arrived in.
        let app: Router = messages_router(
            Arc::clone(&directory),
            engine,
            Arc::clone(&store),
            Arc::clone(&conversations),
        )
        .merge(mcp)
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
            base_url,
            env,
            store,
            conversations,
            rival,
            recorder,
            upstream,
            binary,
            topology,
        }
    }

    /// Write this run's Relay configuration, and refuse to spawn anything until
    /// Relay agrees it resolved to *this* deployment.
    ///
    /// **Neither the configuration nor the check is written here any more**
    /// (R-T5). [`RelayHandoff`] renders the four lines and rules on the
    /// `--dry-run` report; this function's job is the two things only a rig has
    /// — a scratch root and a version banner — plus the panic that turns a
    /// refusal into a test failure.
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
    pub fn wire_relay(root: &Path, base_url: &str, binary: &str) -> Topology {
        let relay = std::env::var(RELAY_BIN_VAR).unwrap_or_else(|_| {
            panic!(
                "the chained topology needs a real Relay binary: set {RELAY_BIN_VAR}, or run \
                 without --include-ignored"
            )
        });
        let version = relay_version(&relay);
        let handoff = RelayHandoff::for_claude(base_url, binary)
            .expect("the rig's own deployment root and binary are the correct shape");
        let config = root.join("relay-config.toml");
        std::fs::write(&config, handoff.config_toml()).expect("the run's Relay configuration");
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
        if let Err(re_aimed) = handoff.verify_resolved(&resolved) {
            panic!("F8: {re_aimed}");
        }

        Topology::Chained {
            relay,
            config,
            handoff,
        }
    }

    /// The principal every request below resolves to.
    pub fn principal(&self) -> Principal {
        principal()
    }

    /// Every turn request, in arrival order.
    ///
    /// Matched on the path alone: the client appends `?beta=true`, which axum
    /// routes past and this filter must too — a comparison against the whole URI
    /// would find nothing and every assertion below would fail as "the client
    /// never sent a turn".
    pub fn turns(&self) -> Vec<Exchange> {
        self.recorder.to(&format!("{API_PREFIX}/{MESSAGES_PATH}"))
    }

    /// Every `tools/call` this deployment's control surface served, in order.
    ///
    /// Filtered on the JSON-RPC method and not on the path: the client reaches
    /// `/mcp` four times before it dispatches anything — `initialize`,
    /// `notifications/initialized`, the optional `GET` stream this deployment
    /// answers `405`, and `tools/list` (§5.8) — so a count of requests to the
    /// path would be a claim about a client's startup sequence rather than
    /// about its tool loop.
    pub fn control_calls(&self) -> Vec<Exchange> {
        self.recorder
            .to(MCP_MOUNT_PATH)
            .into_iter()
            .filter(|exchange| {
                exchange
                    .body
                    .as_ref()
                    .and_then(|body| body["method"].as_str())
                    == Some("tools/call")
            })
            .collect()
    }

    /// The rival conversation this rig was built with.
    ///
    /// A panic and not an `Option` at the call site: a claim about *which* of
    /// two conversations answered a control call is vacuous on a rig that has
    /// only one, and a test reaching for this on a [`ControlRace::None`] rig
    /// has asked the wrong rig rather than found nothing.
    pub fn rival(&self) -> &Rival {
        self.rival.as_deref().expect(
            "this rig was started with `ControlRace::None`, so `latest` and the tool-use id name \
             one session and every claim about which of them answered would hold either way",
        )
    }

    /// The absolute path of the file the client is asked to read.
    pub fn canary_path(&self) -> PathBuf {
        self.root.join("wd").join(CANARY_FILE)
    }

    /// The session the client drove, discovered rather than predicted.
    ///
    /// The test cannot know the client's session UUID in advance — it is minted
    /// inside the child — and a Configured deployment qualifies the name by
    /// principal on top of that. This is the production accessor for "the last
    /// session this principal drove a turn on, on this node", reading the same
    /// `Arc<Conversations>` the router was handed.
    pub fn session(&self) -> SessionId {
        super::e2e::session(&self.conversations)
    }

    /// The session's committed items, in log order.
    pub async fn items(&self) -> Vec<Item> {
        super::e2e::items(&self.store, &self.session()).await
    }

    /// A fork is silent from the client's side, so the only way to catch one is
    /// to ask the store whether generation one exists at all.
    pub async fn assert_never_forked(&self) {
        super::e2e::assert_never_forked(&self.store, &self.session()).await;
    }

    /// A first `claude -p` in this run's isolated home.
    pub async fn print(&self, prompt: &str, extra: &[&str]) -> ClaudeRun {
        self.spawn(prompt, extra, false).await
    }

    /// A `claude --continue -p`, extending the conversation the previous run
    /// left in this home for this working directory.
    pub async fn continued(&self, prompt: &str) -> ClaudeRun {
        self.spawn(prompt, &[], true).await
    }

    /// This rig's deployment, as the profile vocabulary spells it — the file
    /// written and its path answered by [`write_profile`], the one copy both
    /// real-binary suites now share (M11.3 review F18). What is this rig's own
    /// is the two values below: the loopback root it is serving on, and which
    /// topology the profile should name.
    pub fn write_profile(&self, topology: &str) -> PathBuf {
        write_profile(&self.xdg("config"), "claude", &self.base_url, topology)
    }

    /// One of this run's isolated XDG directories, created — [`xdg`] under the
    /// rig's own root, which is the only part of it a rig supplies.
    pub fn xdg(&self, what: &str) -> PathBuf {
        xdg(&self.root, what)
    }

    /// Drive the real client through a real `topham`, and answer with what the
    /// client printed.
    ///
    /// `subcommand` is `topham`'s own argv up to the `--`; everything after it
    /// is [`claude_argv`], the same vector the Direct and Chained tests hand the
    /// client. That split is what makes these tests about the launcher: what
    /// the client is *asked* to do is identical, so any difference in what
    /// arrives at roundhouse's edge is `topham`'s doing.
    ///
    /// `extra` is the operator's own tail, and it is the *only* place a flag
    /// may be spelled: `topham` generates a leading argv of its own and refuses
    /// a tail that repeats one of its flags (`LaunchError::ArgvCollidesWithGenerated`),
    /// so a test that restated `--mcp-config` here would be refused before
    /// anything spawned. What legitimately belongs in it is the client's own
    /// permission grant — `--allowedTools`, which the launcher deliberately
    /// does not decide (M12, R-M3; `topham plan`'s notes say so).
    ///
    /// **The child's environment is cleared and rebuilt from a named set that
    /// contains no `ANTHROPIC_*` variable at all** — which is the difference
    /// between this and every other spawn in this module. The other tests hand
    /// the client [`ClaudeEnv`] directly; here the child is handed a turn key,
    /// two homes and a `PATH`, and `topham` is what has to produce the rest. A
    /// leaked `ANTHROPIC_BASE_URL` would make a broken launcher look like a
    /// working one.
    pub async fn through_topham(
        &self,
        subcommand: &[&str],
        prompt: &str,
        extra: &[&str],
        deadline: Duration,
    ) -> ClaudeRun {
        let topham = topham_binary();
        println!("    topham binary : {topham}");
        println!("    topham version: {}", topham_version(&topham));

        let mut command = tokio::process::Command::new(&topham);
        command.args(subcommand);
        command.arg("--");
        command.args(claude_argv(prompt, extra, false));
        command.current_dir(self.root.join("wd"));
        command.kill_on_drop(true);

        command.env_clear();
        // The client's own binary is resolved by `topham` through `PATH`, from
        // the bare name the profile's agent implies — so the directory holding
        // the binary under test goes first. Without this the launcher would
        // find whatever `claude` the box has, or none, and the test would be
        // about a different binary than the one the version banner printed.
        command.env("PATH", path_with(&self.binary));
        command.env("HOME", self.root.join("home"));
        command.env("CLAUDE_CONFIG_DIR", self.root.join("home/.claude"));
        command.env("XDG_CONFIG_HOME", self.xdg("config"));
        command.env("XDG_DATA_HOME", self.xdg("data"));
        command.env("XDG_STATE_HOME", self.xdg("state"));
        command.env("XDG_CACHE_HOME", self.xdg("cache"));
        // The one credential, under the name the profile names — never as a
        // generated `ANTHROPIC_*` variable, which is what `topham` is here to
        // produce.
        command.env(DEFAULT_KEY_ENV, &self.secret);

        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        drive_child(
            command,
            deadline,
            &topham,
            TOPHAM_BIN_VAR,
            &format!("a `topham {}`", subcommand.join(" ")),
            &self.root,
        )
        .await
    }

    /// One `claude -p` on this rig's topology, bounded by [`drive_child`] —
    /// which is where the deadline, the interrupt-then-kill and the reasoning
    /// behind both now live, because a `topham` child needs exactly the same
    /// treatment.
    pub async fn spawn(&self, prompt: &str, extra: &[&str], resume: bool) -> ClaudeRun {
        let command = build_child_command(
            &self.binary,
            &self.topology,
            &self.env,
            &self.root,
            prompt,
            extra,
            resume,
        );
        let plan = self.topology.plan(&self.binary, &self.root);
        drive_child(
            command,
            plan.deadline,
            &plan.program,
            CLAUDE_BIN_VAR,
            &format!("a {} `{} -p`", plan.label, self.binary),
            &self.root,
        )
        .await
    }

    /// Remove this run's directory.
    pub fn clean(&self) {
        clean(&self.root);
    }
}

/// Run one child to completion, or kill its whole tree at `deadline`.
///
/// A free function rather than a method on [`Rig`] because there are now two
/// kinds of child a run of this suite starts: the client (or Relay) the rig
/// builds itself, and a `topham` that resolves a profile and *becomes* one of
/// those. Both have to be bounded, both have to be killed the same way, and
/// both produce the same one JSON document — so a second copy of the watchdog
/// beside the second spawn would be the place the two quietly stopped agreeing
/// about what a hung run does.
///
/// **The deadline used to leak the very processes it exists to stop** (M11.2b
/// review F4). Three things close it, in the order they fire:
///
/// 1. `SIGINT` to the direct child. Under Chained that child is Relay, which
///    puts the client in a process group of its own and tears it down — plus
///    its plugin temp dir — on interrupt or normal exit. A `SIGKILL` first
///    would skip both.
/// 2. A short grace period, then `SIGKILL` for whatever ignored the interrupt —
///    pinned by [`an_expired_deadline_ends_a_child_that_ignores_the_interrupt`],
///    which traps the interrupt on purpose so a harness that only interrupted
///    would hang there rather than here.
/// 3. `kill_on_drop(true)` on the command itself, as the backstop for every
///    path out of this function that is not this one — a panic between spawn
///    and wait included.
async fn drive_child(
    mut command: tokio::process::Command,
    deadline: Duration,
    program: &str,
    override_var: &str,
    what: &str,
    root: &Path,
) -> ClaudeRun {
    let child = command.spawn().unwrap_or_else(|error| {
        panic!(
            "could not run `{program}`: {error}. Set {override_var} to a real binary, or drop \
             --include-ignored."
        )
    });
    // A watchdog beside the child rather than a `timeout` around it: a
    // `timeout` that expires drops the future holding the `Child`, and a
    // dropped `Child` is killed outright by `kill_on_drop` — which is the
    // backstop, not the shutdown. Signalling from beside it lets the interrupt
    // land while the child is still ours to wait on, so Relay takes its own
    // client and its plugin temp dir down with it.
    let pid = child.id();
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
        "{what} did not finish within {deadline:?} and was interrupted, then killed. HOME: {}",
        root.join("home").display()
    );

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    ClaudeRun {
        // `--output-format json` prints exactly one document. Parsed leniently
        // so a client that printed something else fails on the assertion that
        // names what was missing rather than on a panic here.
        result: serde_json::from_str::<Value>(stdout.trim()).ok(),
        stdout,
        stderr,
        success: output.status.success(),
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
pub async fn interrupt_then_kill(pid: u32) {
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
/// [`Rig::a_root_for`] mints a fresh UUID per call, so no two of the suite's
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
pub fn claim_root(root: &Path) -> bool {
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
/// `--include-ignored` — see the suite's module doc, ordering of guards 2 and 3.
///
/// One function used by both the real harness and its own test, rather than a
/// second copy that mirrors it: a copy is a fixture that can drift from what
/// actually spawns, which is exactly the gap this closes.
pub fn build_child_command(
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

/// The client's own argv, identical on both topologies.
///
/// The point of the split: under Chained this vector is handed to Relay after a
/// `--` and Relay splices its own `--plugin-dir` and `--settings` into it
/// (Relay evidence §A.7), so what the client is *asked* to
/// do is by construction the same thing on both paths and any difference in
/// outcome is Relay's doing rather than the harness's.
pub fn claude_argv(prompt: &str, extra: &[&str], resume: bool) -> Vec<String> {
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

/// What `claude --version` prints, or a loud panic naming the override.
///
/// Isolated exactly as [`build_child_command`] isolates a real run (M11.2b
/// review F18): cleared, then `PATH`, a scratch `HOME`, an isolated
/// `CLAUDE_CONFIG_DIR`, and the two variables that stop a probe reaching the
/// network on its way to printing a number.
pub fn claude_version(binary: &str) -> String {
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
pub fn relay_version(binary: &str) -> String {
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
///
/// Both the argv and the isolation come from
/// [`relay_handoff`](roundhouse_server::relay_handoff) (R-T5), so this rig's
/// preflight and `topham relay`'s are the same experiment rather than two that
/// agree today. What stays here is the `extra` slot — the hazard-4 guards below
/// need a *second* configuration layer, which is not a thing a launcher ever
/// wants.
pub fn relay_dry_run(relay: &str, home: &Path, config: &Path, extra: &[&str]) -> String {
    let mut command = std::process::Command::new(relay);
    command.args(RelayAgent::Claude.preflight_argv(config));
    command.args(extra);
    command.args(["--", "-p", "probe"]);
    command.env_clear();
    for (name, value) in
        RelayHandoff::preflight_env(home, &std::env::var("PATH").unwrap_or_default())
    {
        command.env(name, value);
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

pub struct ClaudeRun {
    /// The one JSON document `--output-format json` prints.
    pub result: Option<Value>,
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

impl ClaudeRun {
    /// Fail unless the client completed the turn without an error.
    ///
    /// Three signals rather than the exit status alone: a non-zero exit, an
    /// `is_error` document, and a `permission_denials` entry all mean the run
    /// proved nothing, and each is diagnosed differently — the third in
    /// particular is what a client that started asking about `Read` would show.
    pub fn assert_completed(&self, what: &str) {
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

    pub fn field(&self, name: &str) -> Value {
        self.result
            .as_ref()
            .map(|result| result[name].clone())
            .unwrap_or(Value::Null)
    }

    /// The final assistant text, which is what `-p` exists to print.
    pub fn text(&self) -> String {
        self.field("result")
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// The client's own session UUID, minted inside the child.
    pub fn session_id(&self) -> String {
        self.field("session_id")
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// How many assistant turns the client ran inside one invocation — two when
    /// it dispatched a tool and came back.
    pub fn turns(&self) -> u64 {
        self.field("num_turns").as_u64().unwrap_or_default()
    }
}
