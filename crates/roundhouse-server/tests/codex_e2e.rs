// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
#![cfg(feature = "e2e-codex")]

//! M9 of `PLAN-agentic-control-plane.md`: a **real `codex` binary** driving a
//! real roundhouse over a real socket.
//!
//! Every other suite in this crate proves a claim about one seam with the rest
//! of the world doubled. This one doubles nothing on the client side at all: it
//! spawns `codex exec`, points it at a loopback roundhouse with the config
//! [`roundhouse_server::codex_launch`] generates, and lets the real client
//! decide what to send, what to dispatch, and what to send back. What it proves
//! is the one class of claim a double cannot: that the projection an agent is
//! *told* it will get is the one it actually acts on.
//!
//! # What the suite is the closure of
//!
//! Three of the five tests below are named in the plan's §9 M9 rung, and they
//! are named there rather than invented here because each closes one thing
//! M0–M6 could only document:
//!
//! - `a_real_codex_binary_executes_our_synthetic_tool_call_and_returns_its_output`
//!   — the dispatch assumption;
//! - `a_real_codex_binary_resends_the_call_and_output_and_the_session_does_not_fork`
//!   — the history-buffer resend, which §10 open item 1 records as
//!   unverifiable without `codex-core` and names M9 as the only closure;
//! - `the_next_turn_reflects_the_correction` — that the correction the loop
//!   built is what the agent actually read.
//!
//! Green here retires the documented-assumption block that
//! `crates/roundhouse-mcp/src/lib.rs` carried until now, which §9 makes the
//! explicit condition. The other two tests are preconditions this suite could
//! not assume: that `exec resume --last` continues one roundhouse session, and
//! that our rmcp 3.1.3 service answers codex's rmcp 1.8.0 client at all.
//!
//! §10 open item **2** — whether reporting a judge's usage on a steered turn
//! disturbs the client's own bookkeeping — is *evidence* here and not a test.
//! The `M9-USAGE-EVIDENCE` block is printed, never asserted; deciding what
//! relationship should hold is the plan addendum's job, and an assertion in
//! this file would be the fixture quietly making that call.
//!
//! # What is real here, and what is scripted
//!
//! Real: the codex binary, the HTTP transport, the MCP handshake and dispatch,
//! the control directory, the minted turn key, the `Validator`, the trigger,
//! the action map, the steer deposit, and the `/mcp` service that serves it
//! back. Scripted: the judge's verdict (what a hosted reviewer would *say* is
//! not the subject, and a real one would make every assertion below partly
//! about a model's opinion), the signal (`trigger.rs` owns when a signal should
//! fire), and the frontier answer.
//!
//! # How to run it
//!
//! ```text
//! timeout 300 cargo test -p roundhouse-server --features e2e-codex \
//!     --test codex_e2e -- --include-ignored --test-threads=1 --nocapture
//! ```
//!
//! `--features e2e-codex` compiles the file at all; `--include-ignored` opts
//! into spawning processes. `--test-threads=1` is not politeness: each test owns
//! a `CODEX_HOME` and `codex exec resume --last` resolves "last" inside it, so
//! two tests interleaving their spawns would be two clients racing for one
//! rollout. Once opted in, a missing binary is a loud panic naming
//! `ROUNDHOUSE_TEST_CODEX_BIN` rather than a silent skip.
//!
//! **No network is needed.** The server binds `127.0.0.1:0`, the model catalog
//! is pinned on disk (so the client's models manager has no network path at
//! all), and the child's environment is cleared before it is built — so a
//! login the developer happens to hold cannot leak into a request. Under
//! `RoundhouseKey`, no `auth.json` is ever written; the one exception is
//! [`Rig::start_forwarding`], whose crafted, unsigned `auth.json` is still
//! hermetic (see `forwarded_login_auth_json`'s doc for why 0.146.0 accepts it
//! without a network round trip). Nothing here reaches beyond loopback. The
//! "cannot leak"
//! half of that claim is enforced by
//! `the_childs_environment_carries_only_the_allowlisted_keys_and_no_ambient_credential`
//! below, on the constructed [`std::process::Command`] itself — added in
//! stage 5 after stage 4's refute (Finding B) found that no assertion on the
//! wire could see the negative: a leaked `OPENAI_API_KEY` left every
//! steering-test assertion green, because `RoundhouseKey`'s `env_key`
//! resolves ahead of any ambient login and the leaked variable was simply
//! never consulted. A guard that checks only what arrived can never catch an
//! extra credential that was available but happened not to be picked; this
//! one checks what was available at all.
//!
//! # Version vigilance
//!
//! Written against, and verified against, `codex-cli 0.146.0`. The version is
//! printed on every run and a mismatch prints a WARNING rather than failing:
//! the suite is evidence about a *binary*, and a green run against an unread
//! version is exactly the silent change of meaning CLAUDE.md's vigilance rule
//! exists to catch. One 0.146.0-specific fact is load-bearing and would move:
//! `request_max_retries` / `stream_max_retries` are **provider-scoped** keys —
//! the top-level spelling is rejected by `--strict-config`, which this harness
//! passes on purpose so that drift is loud.
//!
//! `CODEX_HOME` still lives under `target/` rather than the system temp dir,
//! but — corrected here after stage 4's refute found the original framing
//! overstated (Finding A) — that is precaution, not a measured fact. The
//! arg0-symlink refusal it guards against, if it exists at 0.146.0, sits on
//! the sandboxed-shell-exec path (`codex-linux-sandbox`); this harness's
//! `sandbox_mode = "read-only"` posture, with no `exec_command` ever
//! dispatched, does not reach it. Direct retest confirms this on this box:
//! pointing `CODEX_HOME` and the workdir at `/tmp` produced two full green
//! runs, including the whole three-run steering suite with its usage-evidence
//! block. Kept under `target/` regardless, because "the refusal path is never
//! reached here" is a narrower and cheaper-to-attribute claim than "release
//! builds refuse it" — the latter is not something this suite exercises or
//! proves, in either direction, and no test here should be read as asserting
//! it.
//!
//! One environmental note so nobody debugs it from scratch: codex's first user
//! message embeds `<current_date>`, which is stable within a run but would
//! change across local midnight — a suite straddling it would see the item
//! change and the session fork.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
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
use roundhouse_core::control::{MemorySpendLedger, Principal};
use roundhouse_core::event::{SessionEventKind, Usage};
use roundhouse_core::ids::SessionId;
use roundhouse_core::interject::Interjector;
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::now_ms;
use roundhouse_core::routing::{AffinityPolicy, Candidate, Target};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_core::validate::{Validator, ValidatorConfig};
use roundhouse_fleet::EchoFrontierClient;
use roundhouse_mcp::ControlStore;
use roundhouse_server::codex_launch::{
    CONTEXT_WINDOW_TOKENS, CodexAuthKind, CodexLaunch, DEFAULT_KEY_ENV,
};
use roundhouse_server::control_config::directory::key_id;
use roundhouse_server::control_config::{MembershipRole, TURN_KEY_HEADER};
use roundhouse_server::mcp_api::MCP_MOUNT_PATH;
use roundhouse_server::{
    ControlDirectory, ControlPlaneReads, Conversations, CrossChecks, DirectoryMutation,
    EchoLocalExecutor, Engine, EngineConfig, MemoryDirectoryStore, mcp_api::mcp_router,
    responses_api::responses_router,
};

use codex_utils_rustls_provider::ensure_rustls_crypto_provider;

mod common;
use common::validate::{AlwaysFires, OFF_TRACK, ScriptedJudge, judge_spec, open_trigger};
use common::{frontier_catalog, sha256_hex};

// ---------------------------------------------------------------------------
// What this deployment is
// ---------------------------------------------------------------------------

/// What the echo provider answers an ordinary turn with.
///
/// Distinctive on purpose: an assertion that *this* text came out of `codex
/// exec` is an assertion that the turn was served by roundhouse's frontier
/// path and not by anything the client invented.
const ANSWER: &str = "roundhouse answered this turn";

/// The tenant every request below authenticates as.
const PROJECT: &str = "acme";
const USER: &str = "ada";

/// A fragment of the correction `render_directive` builds.
///
/// The *first sentence only*, and both halves of that matter. Roundhouse's own
/// words, so asserting on them is asserting the agent read what roundhouse
/// wrote; and one line, because codex wraps an MCP result as
/// `"Wall time: …\nOutput:\n[…]"` and JSON-escapes the newlines inside it, so a
/// multi-line fragment would never match the stored string.
const GUIDANCE_FRAGMENT: &str =
    "A review of this session's recent steps found it is not making progress";

/// The directive's closing instruction, which is what makes it a correction.
///
/// The other end of the same string [`GUIDANCE_FRAGMENT`] samples, and on
/// purpose: the opening sentence says a review found a problem, and this one
/// says what the agent should now do about it. `render_directive` appends it
/// unconditionally (`verdict.rs:441-467`), so a correction missing it is a
/// correction that was truncated somewhere between us and the agent.
const DIRECTIVE_INSTRUCTION: &str = "Re-read the task";

/// The judge's own prose, which must **never** reach the agent.
///
/// From [`OFF_TRACK`]'s `divergence.description`. `render_directive` excludes it
/// deliberately — quoting a model that just read attacker-influenceable
/// transcript into a payload the agent dispatches is the injection path the
/// design refuses — so this is a negative control, not a spare fragment.
const JUDGE_PROSE: &str = "editing a file the task did not name";

/// How long a single `codex exec` may take before the test kills it.
///
/// Generous, because the child compiles nothing but does spawn an MCP client,
/// negotiate a handshake and run a turn. A deadline rather than the suite's
/// outer `timeout` because the outer one reports "the suite hung" and this one
/// reports which run hung, with that run's stderr.
const CHILD_DEADLINE: Duration = Duration::from_secs(60);

/// The environment variable that overrides which binary is driven.
const CODEX_BIN_VAR: &str = "ROUNDHOUSE_TEST_CODEX_BIN";

/// The version this suite's assertions were written against.
const VERIFIED_VERSION: &str = "codex-cli 0.146.0";

/// A hermetic seat token: an unsigned, three-part JWT whose payload is
/// `{"exp":2053740800}` (2035-01-01), base64url-encoded with no padding.
///
/// F12 (`ForwardedOpenAiLogin` had never been driven by a real binary): the
/// header and signature segments are never decoded by 0.146.0
/// (`decode_jwt_payload`, `login/src/token_data.rs:117-127`, binds them to
/// `_header_b64` / `_sig_b64` and reads neither), so they are placeholders
/// rather than anything cryptographic; only the payload has to survive
/// base64url decode into JSON. The far `exp` is what keeps
/// `should_refresh_proactively` (`login/src/auth/manager.rs:2510-2532`) from
/// ever reaching for the network: it returns as soon as a parsed `exp` is
/// more than five minutes out, before `last_refresh` is even looked at.
const SEAT_ACCESS_TOKEN: &str = "header.eyJleHAiOjIwNTM3NDA4MDB9.sig";

/// The account id this run's hermetic login carries.
///
/// Lives on `TokenData.account_id` in the crafted `auth.json`, not inside the
/// JWT payload above — `CodexAuth::get_account_id` (`login/src/auth/manager.rs:567-573`)
/// reads the token-data field directly for every non-headers/agent-identity
/// auth kind; the id-token JWT's own claims are never consulted for it.
const SEAT_ACCOUNT_ID: &str = "acct-e2e-seat";

/// The hermetic `$CODEX_HOME/auth.json` a real `codex login` would have
/// written, built instead of run: `auth_mode` omitted (defaults to
/// `AuthMode::Chatgpt`, `login/src/auth/manager.rs:1708-1720`, once no other
/// mode's fields are set) is what a real login also leaves unset, so this
/// matches rather than special-cases the default path.
fn forwarded_login_auth_json(access_token: &str, account_id: &str) -> String {
    serde_json::json!({
        "OPENAI_API_KEY": null,
        "tokens": {
            // The id-token JWT is never inspected for `account_id` (see
            // `SEAT_ACCOUNT_ID`'s doc) and its claims are all optional
            // (`IdClaims`, codex `login/src/token_data.rs:71-79`), so an
            // empty payload is enough to satisfy the three-part-JWT parse.
            "id_token": "header.e30.sig",
            "access_token": access_token,
            "refresh_token": "unused-hermetic-refresh-token",
            "account_id": account_id,
        },
        "last_refresh": "2020-01-01T00:00:00Z",
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// The recorder
// ---------------------------------------------------------------------------

/// One request the deployment served, as it arrived.
#[derive(Clone, Debug)]
struct Exchange {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    /// The request body, parsed if it was JSON.
    ///
    /// Parsed rather than kept as bytes because every assertion downstream is
    /// on a *value*: codex re-serializes items in its own struct order, so a
    /// byte comparison of a resent item fails on field ordering even when
    /// nothing changed. The one field that is byte-exact — `arguments` — is a
    /// JSON string, and comparing two `String`s pulled out of two parsed
    /// documents is still a byte comparison of that field.
    body: Option<Value>,
    status: u16,
    /// The response body, captured for `/mcp` only.
    ///
    /// `/v1/responses` streams: buffering it here would hold the whole SSE body
    /// until the turn ended, which is the one property that surface exists to
    /// not have. `/mcp` answers a single JSON document per POST, so capturing it
    /// costs nothing and is what makes the handshake assertions readable.
    response: Option<Value>,
}

impl Exchange {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
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

    /// Every request to `path`, in arrival order.
    fn to(&self, path: &str) -> Vec<Exchange> {
        self.all()
            .into_iter()
            .filter(|exchange| exchange.path == path)
            .collect()
    }

    /// Every `/mcp` request whose JSON-RPC method is `method`.
    fn rpc(&self, method: &str) -> Vec<Exchange> {
        self.to(MCP_MOUNT_PATH)
            .into_iter()
            .filter(|exchange| {
                exchange
                    .body
                    .as_ref()
                    .and_then(|body| body["method"].as_str())
                    == Some(method)
            })
            .collect()
    }

    /// A one-line rendering of every exchange, for a failure message.
    fn transcript(&self) -> String {
        self.all()
            .iter()
            .map(|exchange| {
                let rpc = exchange
                    .body
                    .as_ref()
                    .and_then(|body| body["method"].as_str())
                    .unwrap_or("-");
                format!(
                    "{} {} -> {} (jsonrpc method: {rpc})",
                    exchange.method, exchange.path, exchange.status
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Capture what arrived, without changing what is served.
///
/// A tower layer over the *merged* app rather than a wrapper per router,
/// because the interleaving is the subject: a steer is a `/v1/responses`
/// response followed by an `/mcp` dispatch followed by another
/// `/v1/responses` request, and three separate recorders could not say that.
async fn record(State(recorder): State<Recorder>, request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
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
    // Generously bounded: turn 1 is already ~43 KB of instructions and the
    // steered turn resends the whole history. A silent truncation here would
    // surface as a 422 from our own canonicalizer, which reads exactly like a
    // roundhouse bug and is not one.
    let bytes = axum::body::to_bytes(body, 32 * 1024 * 1024)
        .await
        .expect("a loopback client's request body is readable");
    let parsed = serde_json::from_slice::<Value>(&bytes).ok();
    let response = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;

    let status = response.status().as_u16();
    let (response_parts, response_body) = response.into_parts();
    let (captured, response_body) = if path == MCP_MOUNT_PATH {
        let bytes = axum::body::to_bytes(response_body, 4 * 1024 * 1024)
            .await
            .expect("the MCP service answers one bounded document per POST");
        (
            serde_json::from_slice::<Value>(&bytes).ok(),
            Body::from(bytes),
        )
    } else {
        (None, response_body)
    };

    recorder
        .exchanges
        .lock()
        .expect("recording")
        .push(Exchange {
            method,
            path,
            headers,
            body: parsed,
            status,
            response: captured,
        });
    Response::from_parts(response_parts, response_body)
}

// ---------------------------------------------------------------------------
// The deployment
// ---------------------------------------------------------------------------

/// A live roundhouse, its filesystem, and everything needed to read it back.
struct Rig {
    /// Where this run's `CODEX_HOME`, work directory and generated files live.
    root: PathBuf,
    /// The minted turn key, in plaintext — the value of the env var the client
    /// is launched with, and the only place it exists outside the directory's
    /// hash.
    secret: String,
    /// `sha256(secret)`, kept alongside it so F15's revocation test can name
    /// the row `RevokeKey` wants without re-deriving it via a crate this file
    /// does not otherwise depend on.
    key_sha256: String,
    /// The production admin plane this run minted its key from — kept live so
    /// a test can revoke mid-run the way an operator's `DELETE` would, rather
    /// than tearing down and rebuilding a directory the client is already
    /// pointed at.
    directory: Arc<ControlDirectory>,
    store: Arc<MemoryStore>,
    conversations: Arc<Conversations>,
    recorder: Recorder,
    judge: Arc<ScriptedJudge>,
    binary: String,
    version: String,
}

impl Rig {
    /// Bind, serve, mint, and write the client's two files.
    ///
    /// Configured mode and a *minted* key, not a file-declared one: the point
    /// of the milestone is that a real client authenticates the way a real
    /// tenant does, which means the production `PlaneSource` — an
    /// `Arc<ControlDirectory>`, the one a shipped binary can name — and a
    /// secret that only ever existed inside `MintedKey`.
    ///
    /// Parameterized on [`CodexAuthKind`] rather than duplicated per kind: the
    /// ~200 lines above the config write (bootstrap, directory, engine,
    /// listener) are one fact about the deployment, not two, and a second copy
    /// of them is exactly the fixture drift the module doc's "one function
    /// used by both" reasoning (see `build_child_command`) warns against.
    /// [`Self::start`] and [`Self::start_forwarding`] are the two call shapes;
    /// this is where they meet.
    async fn start_as(label: &str, auth: CodexAuthKind) -> Self {
        ensure_rustls_crypto_provider();

        // Under `target/`, not the system temp dir — precaution, not a
        // measured fact (module doc, "Version vigilance": stage 4's refute
        // mutation 13 pointed this at `/tmp` and got two full green runs).
        // The arg0-symlink refusal this guards against, if it exists at
        // 0.146.0, sits on the sandboxed-shell-exec path; this harness's
        // read-only posture with no `exec_command` ever dispatched does not
        // reach it. Kept here anyway because "never reached on this box, this
        // run" is a narrower claim than "release builds refuse it", and the
        // narrower one is the one this suite can actually stand behind.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/codex-e2e")
            .join(format!("{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("home")).expect("the run's CODEX_HOME");
        std::fs::create_dir_all(root.join("wd")).expect("the run's work directory");

        // Bootstrap is file-only, by design: `admin_keys` in the file is the
        // sole root of trust, and a directory with no admin plane refuses to
        // mint. The arm salt is here for the same reason — it is deployment-wide
        // file state no admin write can move.
        let admin = common::admin_key("root");
        let file = common::control_plane(
            serde_json::json!({
                "projects": [],
                "users": [],
                "admin_keys": [sha256_hex(&admin)],
                "arm_salt": "m9-e2e",
            }),
            "codex-e2e bootstrap",
        );
        let directory = Arc::new(
            ControlDirectory::new(
                file,
                "ROUNDHOUSE_CONTROL_PLANE",
                Arc::new(MemoryDirectoryStore::new()),
                // `Some(judge_spec())` and not `None`: a project whose
                // `validate` block enrols its sessions promises a judge, and the
                // startup cross-check refuses a plane that promises one with
                // none reachable. The spec is the same one `ScriptedJudge`
                // reports its side calls under, so the promise the directory
                // checks and the target the fold books are one model.
                CrossChecks::new(reachable(), Some(judge_spec())),
                now_ms(),
            )
            .expect("the bootstrap file alone compiles"),
        );

        // The project carries `validate` because that is the *file* vocabulary
        // for enrolment: `ValidationTerms` are per-project config state, and
        // there is no other way for a socket-driven turn to arrive enrolled.
        directory
            .apply(
                DirectoryMutation::CreateProject {
                    entry: serde_json::from_value(serde_json::json!({
                        "id": PROJECT,
                        "validate": {
                            "enabled": true,
                            // Explicit, never `auto`: under `auto` the engine's
                            // capability detection decides, and a fixture whose
                            // steer depended on detection would be testing §7.
                            "channel": "tool_call",
                            "arms": { "live": 1, "shadow": 0, "placebo": 0 },
                            // Zero would leave outcome B off: escalation claims
                            // the first intervention, so a cap of zero admits
                            // nothing after it.
                            "steer_after_interventions": 1,
                        },
                    }))
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
        let control = Arc::new(ControlStore::new());
        let conversations = Arc::new(Conversations::new());
        let judge = ScriptedJudge::always(OFF_TRACK);
        let arm_salt = directory.plane(now_ms()).arm_salt().to_string();
        let validator = Validator::new(
            Arc::clone(&judge) as Arc<dyn roundhouse_core::validate::JudgeClient>,
            ValidatorConfig {
                trigger: open_trigger(),
                arm_salt: arm_salt.clone(),
                ..ValidatorConfig::default()
            },
        )
        .with_signals(vec![Box::new(AlwaysFires)]);
        let engine = Arc::new(
            Engine::new(
                Arc::clone(&store),
                ByteTokenizer,
                Arc::new(EchoLocalExecutor::new("local answer")),
                frontier_catalog(),
                Arc::new(EchoFrontierClient::new(ANSWER)),
                Arc::new(AffinityPolicy::new()),
                EngineConfig {
                    arm_salt,
                    ..EngineConfig::default()
                },
            )
            .with_spend_ledger(Arc::new(MemorySpendLedger::new()))
            .with_control_store(Arc::clone(&control))
            .with_interjector(Arc::new(validator) as Arc<dyn Interjector>),
        );

        let recorder = Recorder::default();
        // One listener for both surfaces, exactly as `main::serve` mounts them:
        // the generated config points `base_url` and the MCP `url` at the same
        // address, and a fixture that served them on two ports would prove
        // nothing about the deployment an operator runs.
        let app: Router = responses_router(
            Arc::clone(&directory),
            Arc::clone(&engine),
            Arc::clone(&store),
            Arc::clone(&conversations),
        )
        .merge(mcp_router(
            Arc::clone(&directory),
            Arc::new(ControlPlaneReads::new(
                Arc::clone(&directory),
                Arc::clone(&store),
                Arc::new(MemorySpendLedger::new()),
                Arc::clone(&conversations),
                reachable(),
            )),
            control,
        ))
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

        let base_url = format!("http://{addr}/v1");
        let catalog_path = root.join("home/models.json");
        let mut launch = CodexLaunch::new(base_url.clone(), &catalog_path);
        if auth == CodexAuthKind::ForwardedOpenAiLogin {
            launch = launch.forwarding_openai_login();
            // F12: the one file this suite's module doc ("no `auth.json` is
            // ever written") did not anticipate, and deliberately still
            // hermetic. 0.146.0 does no JWT signature check
            // (codex `login/src/token_data.rs:117-127` decodes only the
            // payload segment; `_sig_b64` is bound and never read) and
            // schedules no network refresh for a token whose `exp` claim is
            // more than five minutes out
            // (`login/src/auth/manager.rs:2510-2532`,
            // `CHATGPT_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES = 5`), so a locally
            // crafted, unsigned three-part JWT reproduces what a real
            // `codex login` leaves in `CODEX_HOME` closely enough for this
            // suite's purposes: nothing here reaches beyond loopback.
            std::fs::write(
                root.join("home/auth.json"),
                forwarded_login_auth_json(SEAT_ACCESS_TOKEN, SEAT_ACCOUNT_ID),
            )
            .expect("writing the hermetic ChatGPT login");
        }
        std::fs::write(root.join("home/config.toml"), launch.config_toml())
            .expect("writing the client's config");
        std::fs::write(&catalog_path, launch.model_catalog_json())
            .expect("writing the client's model catalog");

        let binary = std::env::var(CODEX_BIN_VAR).unwrap_or_else(|_| "codex".to_string());
        let version = codex_version(&binary);
        println!("--- {label}");
        println!("    codex binary : {binary}");
        println!("    {version}");
        if version.trim() != VERIFIED_VERSION {
            println!(
                "    WARNING: this suite's assertions were written against {VERIFIED_VERSION}. \
                 CLAUDE.md's synergy-vigilance rule applies: re-read what changed upstream \
                 before trusting a green run against a different binary."
            );
        }
        println!("    roundhouse   : {base_url}");
        println!("    CODEX_HOME   : {}", root.join("home").display());

        Self {
            root,
            key_sha256: minted.key_sha256.clone(),
            secret: minted.secret,
            directory,
            store,
            conversations,
            recorder,
            judge,
            binary,
            version,
        }
    }

    /// A bring-your-own-key deployment: the client holds a minted roundhouse
    /// turn key and nothing else.
    async fn start(label: &str) -> Self {
        Self::start_as(label, CodexAuthKind::RoundhouseKey).await
    }

    /// A pass-through deployment: the client's own hermetic ChatGPT login
    /// rides `Authorization`, and roundhouse's turn key still has to arrive
    /// somewhere, which is [`TURN_KEY_HEADER`] via `env_http_headers`. See
    /// [`Self::start_as`] for why this file never held a real login before.
    async fn start_forwarding(label: &str) -> Self {
        Self::start_as(label, CodexAuthKind::ForwardedOpenAiLogin).await
    }

    /// The principal every request below resolves to.
    fn principal(&self) -> Principal {
        Principal::new(PROJECT, USER)
    }

    /// Revoke this run's turn key the way `DELETE /v1/admin/...` does: an
    /// `apply` on the live directory, not a fixture shortcut. The compiled
    /// plane swaps immediately (module doc, "Revocation, staleness, and the
    /// two clocks") — no TTL wait is needed on this single-node rig, which is
    /// the property F15 exists to exercise.
    fn revoke_turn_key(&self) {
        self.directory
            .apply(
                DirectoryMutation::RevokeKey {
                    id: key_id(&self.key_sha256),
                },
                now_ms(),
            )
            .expect("the API-minted turn key is this API's to revoke");
    }

    /// The session codex drove, discovered rather than predicted.
    ///
    /// The test cannot know codex's conversation UUID in advance, and in
    /// Configured mode the id is `{project}/{user}/{uuid}` anyway. This is the
    /// production accessor for "the last session this principal drove a turn
    /// on, on this node", reading the same `Arc<Conversations>` the router was
    /// handed.
    fn session(&self) -> SessionId {
        self.conversations
            .latest(&self.principal())
            .expect("codex drove at least one turn")
    }

    /// The session's committed items, in log order.
    async fn items(&self) -> Vec<Item> {
        self.store
            .read_events(&self.session(), 0, 1024)
            .await
            .expect("the session exists")
            .into_iter()
            .filter_map(|event| match event.kind {
                SessionEventKind::ItemAppended { item } => Some(item),
                _ => None,
            })
            .collect()
    }

    /// How many validations this session decided.
    async fn validations(&self) -> usize {
        self.store
            .read_events(&self.session(), 0, 1024)
            .await
            .expect("the session exists")
            .into_iter()
            .filter(|event| matches!(event.kind, SessionEventKind::ValidationDecided { .. }))
            .count()
    }

    /// The turn index each validation was decided on, in log order.
    ///
    /// The *indices*, not the count, because the count is satisfied by the
    /// wrong two turns: two validations on turns 3 and 4 pass a `== 2` check
    /// while breaking the one claim the fulfilling turn is here to make. The
    /// trigger keeps the observation it acted on in the event, so the set is
    /// readable without inferring it from arithmetic.
    async fn validation_turns(&self) -> Vec<u64> {
        self.store
            .read_events(&self.session(), 0, 1024)
            .await
            .expect("the session exists")
            .into_iter()
            .filter_map(|event| match event.kind {
                SessionEventKind::ValidationDecided { trigger, .. } => Some(trigger.turn_index),
                _ => None,
            })
            .collect()
    }

    /// The usage booked for each side call, in log order.
    ///
    /// The judge's own cost as the *log* recorded it, rather than as the
    /// fixture would have reported it. The two should agree, and printing the
    /// booked one is what makes that checkable by a reader of the evidence
    /// block instead of assumed by its author.
    async fn side_call_usage(&self) -> Vec<Usage> {
        self.store
            .read_events(&self.session(), 0, 1024)
            .await
            .expect("the session exists")
            .into_iter()
            .filter_map(|event| match event.kind {
                SessionEventKind::SideCallCompleted { usage, .. } => Some(usage),
                _ => None,
            })
            .collect()
    }

    /// The usage roundhouse booked for each completed response, in turn order.
    async fn response_usage(&self) -> Vec<Usage> {
        self.store
            .read_events(&self.session(), 0, 1024)
            .await
            .expect("the session exists")
            .into_iter()
            .filter_map(|event| match event.kind {
                SessionEventKind::ResponseCompleted { usage, .. } => Some(usage),
                _ => None,
            })
            .collect()
    }

    /// A fork is silent from the client's side, so the only way to catch one is
    /// to ask the store whether generation one exists at all.
    async fn assert_never_forked(&self) {
        let forked = SessionId::new(format!("{}#g1", self.session()));
        assert!(
            self.store.last_seq(&forked).await.is_err(),
            "the client's resend must have matched its prefix: `{forked}` exists, which means \
             the prefix check refused the claim and rebound the conversation"
        );
    }

    /// The three `codex exec` invocations one steer costs, driven in order.
    ///
    /// Shared by every steering test because the arithmetic is the fixture and
    /// not the subject: turn 1 is below the trigger's turn-index gate, turn 2
    /// validates and escalates (the action map escalates unconditionally on the
    /// first intervention), turn 3 validates and steers. Three tests each
    /// spelling that out would be three places to update when the trigger's
    /// gate moves, and two of them would keep passing while asserting nothing.
    ///
    /// Exactly three, never four: a fourth run is roundhouse turn 5, the
    /// trigger fires again, and the validations the tests below count stop
    /// being the ones they name.
    async fn drive_to_a_steer(&self) -> [CodexRun; 3] {
        let first = self.exec("Say the word alpha and stop.").await;
        first.assert_completed("turn 1");
        first.assert_catalog_was_used();
        let second = self.resume("Now say the word beta and stop.").await;
        second.assert_completed("turn 2");
        let third = self.resume("Now say the word gamma and stop.").await;
        third.assert_completed("turn 3, and the turn that fulfils the steer inside it");
        [first, second, third]
    }

    /// A first `codex exec` in this run's `CODEX_HOME`.
    async fn exec(&self, prompt: &str) -> CodexRun {
        self.spawn(&["exec"], prompt).await
    }

    /// A `codex exec resume --last`, continuing the rollout the previous run
    /// left in this `CODEX_HOME`.
    async fn resume(&self, prompt: &str) -> CodexRun {
        self.spawn(&["exec", "resume", "--last"], prompt).await
    }

    async fn spawn(&self, subcommand: &[&str], prompt: &str) -> CodexRun {
        let last_message = self.root.join(format!("last-{}.txt", uuid::Uuid::new_v4()));
        let mut command = build_child_command(
            &self.binary,
            subcommand,
            prompt,
            &self.root,
            &self.secret,
            &last_message,
        );

        let output = tokio::time::timeout(CHILD_DEADLINE, command.output())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "`{} {}` did not finish within {:?}. CODEX_HOME: {}",
                    self.binary,
                    subcommand.join(" "),
                    CHILD_DEADLINE,
                    self.root.join("home").display()
                )
            })
            .unwrap_or_else(|error| {
                panic!(
                    "could not run `{}`: {error}. Set {CODEX_BIN_VAR} to a real codex binary, or \
                     drop --include-ignored.",
                    self.binary
                )
            });

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let events = stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect::<Vec<_>>();
        CodexRun {
            events,
            stdout,
            stderr,
            success: output.status.success(),
            last_message: std::fs::read_to_string(&last_message).unwrap_or_default(),
        }
    }

    /// Remove this run's directory.
    ///
    /// Called explicitly at the end of a passing test rather than from a
    /// `Drop`: a guard fires on unwind too, which would delete the `CODEX_HOME`
    /// and the rollout of the run that just failed — the only two artefacts
    /// worth having at that moment.
    fn clean(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Build the exact `codex` child command `Rig::spawn` runs, without running it.
///
/// Pulled out of `spawn` as its own function — rather than left inline — so
/// the "nothing leaks" half of the module doc's "No network is needed"
/// paragraph has a guard that does not require spawning a process. Stage 4's
/// refute (mutation 14) showed why the wire-level assertions cannot be that
/// guard: leaking `OPENAI_API_KEY` into the child left every steering-test
/// assertion green, because `RoundhouseKey`'s `env_key` resolves ahead of any
/// ambient login and the leaked variable was simply never consulted for this
/// auth kind. A check on what *arrived* structurally cannot see a credential
/// that was merely *available*; a check on construction can, via
/// `Command::get_envs()` on the object this function returns before anyone
/// calls `.output()` on it.
///
/// One function used by both the real harness and its own test, rather than a
/// second copy that mirrors it: a copy is a fixture that can drift from what
/// actually spawns, which is exactly the gap this function closes.
fn build_child_command(
    binary: &str,
    subcommand: &[&str],
    prompt: &str,
    root: &std::path::Path,
    secret: &str,
    last_message: &std::path::Path,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(binary);
    command.args(subcommand);
    command.args([
        "--json",
        // Unknown config keys become hard errors rather than silent no-ops.
        // Verified to pass against the generated config, and kept on purpose:
        // this suite exists to notice client drift, and a knob that quietly
        // stopped applying is exactly the drift it would otherwise miss.
        "--strict-config",
        "--skip-git-repo-check",
        "-o",
    ]);
    command.arg(last_message);
    command.args([
        "-c",
        "sandbox_mode=\"read-only\"",
        // Provider-scoped, not top level: at 0.146.0 the bare
        // `request_max_retries` is not a config field and `--strict-config`
        // rejects it. Zero so a server bug fails once, loudly, instead of
        // three times with the first failure scrolled away.
        "-c",
        "model_providers.roundhouse.request_max_retries=0",
        "-c",
        "model_providers.roundhouse.stream_max_retries=0",
    ]);
    command.arg(prompt);

    // The working directory rather than `-C`: `exec resume` has no `--cd`
    // flag at all, and `--last` filters recorded sessions *by cwd*, so the
    // resumed process has to stand where the first one stood. Setting it on
    // both keeps one spelling instead of a flag on one path and a chdir on
    // the other.
    command.current_dir(root.join("wd"));

    // Cleared and rebuilt from an explicit allowlist, not inherited: an
    // `OPENAI_API_KEY` or a ChatGPT login in the developer's environment would
    // make the client authenticate as itself against our base_url, and the
    // test would pass while exercising a path it never meant to. The
    // allowlist below is the one
    // `the_childs_environment_carries_only_the_allowlisted_keys_and_no_ambient_credential`
    // checks against — change one without the other and that test is the one
    // that goes red.
    command.env_clear();
    command.env("PATH", std::env::var("PATH").unwrap_or_default());
    command.env("HOME", root);
    command.env("CODEX_HOME", root.join("home"));
    command.env(DEFAULT_KEY_ENV, secret);
    command.env("RUST_LOG", "info");

    // Without this the child blocks reading stdin — the first probe of this
    // idiom hung for a full minute printing "Reading additional input from
    // stdin...", which under a bounded suite reads as "the newest test hangs"
    // and points at the wrong thing entirely.
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    command
}

/// Every target this deployment can route to, priced the way the router prices
/// them.
///
/// The catalog's one model and nothing else: no fleet is attached, so a turn
/// has exactly one place to go and "which target answered" is never a race.
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

fn codex_version(binary: &str) -> String {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "--include-ignored asks for the real binary; `{binary} --version` failed: \
                 {error}. Set {CODEX_BIN_VAR} to one, or run without --include-ignored."
            )
        });
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Finding B, stage 4's refute (mutation 14): nothing on the wire can prove
/// the child's environment carries only what this harness allows, because a
/// leaked credential the auth kind never consults looks, from the wire,
/// identical to one that was never there — every steering-test header
/// assertion stayed green with `OPENAI_API_KEY` leaked in, since
/// `RoundhouseKey`'s `env_key` resolves ahead of any ambient login.
///
/// This is the guard that can actually see it: on the constructed
/// [`tokio::process::Command`] itself, via `Command::get_envs()`, before
/// anything is spawned. No real `codex` binary, no `--include-ignored`, no
/// `ROUNDHOUSE_TEST_CODEX_BIN` — this runs on every `--features e2e-codex`
/// compile.
///
/// Re-applying stage 4's mutation 14 against [`build_child_command`] (adding
/// an extra `command.env("OPENAI_API_KEY", "sk-test")` beside the allowlist)
/// turns this red on the key-set assertion below; reverting turns it green
/// again. That red/green pair is the evidence stage 5's report cites for this
/// finding.
#[test]
fn the_childs_environment_carries_only_the_allowlisted_keys_and_no_ambient_credential() {
    let root = std::path::PathBuf::from("/does/not/need/to/exist");
    let last_message = root.join("last.txt");
    let command = build_child_command(
        "codex",
        &["exec"],
        "prompt",
        &root,
        "rh_turn_test-secret",
        &last_message,
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

    // Exactly these five, named exactly this way — not "at least", because a
    // missing entry (say, a dropped `CODEX_HOME`) is exactly as dangerous a
    // drift as an extra one, and `==` on the key set catches both directions.
    let allowed: BTreeSet<&str> = ["PATH", "HOME", "CODEX_HOME", DEFAULT_KEY_ENV, "RUST_LOG"]
        .into_iter()
        .collect();
    let actual: BTreeSet<&str> = envs.keys().map(String::as_str).collect();
    assert_eq!(
        actual, allowed,
        "the child's constructed environment must carry exactly the allowlist, got: {envs:?}"
    );

    // Named explicitly, on top of the `==` above, because this is the exact
    // property stage 4's mutation broke: `env_clear()` followed by an
    // ambient-credential leak is a *sixth* key, which the set check above
    // already catches — naming the specific suspects here is what makes a
    // future reviewer's intent legible without re-deriving it from a diff.
    for suspect in [
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "CHATGPT_ACCOUNT_ID",
        "OPENAI_BASE_URL",
    ] {
        assert!(
            !envs.contains_key(suspect),
            "an ambient credential leaked into the child's environment: {suspect}"
        );
    }
}

// ---------------------------------------------------------------------------
// What one `codex exec` produced
// ---------------------------------------------------------------------------

struct CodexRun {
    /// The `--json` thread events, one per stdout line.
    events: Vec<Value>,
    stdout: String,
    stderr: String,
    success: bool,
    /// What `-o` wrote: the final agent message, which is a sturdier assertion
    /// target than scraping JSONL for the last `agent_message`.
    last_message: String,
}

impl CodexRun {
    /// Fail unless the client completed the turn.
    ///
    /// The signal is `turn.completed`, not the absence of an `error`: an
    /// unknown model slug emits an `item.completed` of type `error` about
    /// fallback metadata on every run, so a test grepping stdout for `"error"`
    /// false-positives. With the catalog pinned that item must not appear at
    /// all, which is asserted separately below.
    fn assert_completed(&self, what: &str) {
        let completed = self.kinds().iter().any(|kind| kind == "turn.completed");
        assert!(
            self.success && completed,
            "{what}: codex did not complete the turn (exit ok: {}, saw: {:?})\n--- stdout\n{}\n--- stderr\n{}",
            self.success,
            self.kinds(),
            self.stdout,
            self.stderr
        );
    }

    /// The top-level `type` of each thread event, in order.
    fn kinds(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| event["type"].as_str().map(str::to_string))
            .collect()
    }

    /// Every `item.completed` item of the given type.
    fn items_of(&self, kind: &str) -> Vec<&Value> {
        self.events
            .iter()
            .filter(|event| event["type"] == "item.completed")
            .map(|event| &event["item"])
            .filter(|item| item["type"] == kind)
            .collect()
    }

    /// `turn.completed.usage`, which is the client's own bookkeeping.
    fn usage(&self) -> Option<&Value> {
        self.events
            .iter()
            .find(|event| event["type"] == "turn.completed")
            .map(|event| &event["usage"])
    }

    /// The conversation id the client reported, which is also the
    /// `prompt_cache_key` it sends.
    fn thread_id(&self) -> Option<String> {
        self.events
            .iter()
            .find(|event| event["type"] == "thread.started")
            .and_then(|event| event["thread_id"].as_str())
            .map(str::to_string)
    }

    /// The client must not have fallen back to invented model metadata.
    ///
    /// With `model_catalog_json` pinned this item cannot appear; if it does,
    /// the catalog did not load and every later assertion is about a client
    /// running on different metadata than the one this suite describes.
    fn assert_catalog_was_used(&self) {
        let errors = self.items_of("error");
        assert!(
            errors.is_empty(),
            "the pinned model catalog must have loaded, but the client reported: {errors:?}\n{}",
            self.stderr
        );
    }
}

/// A recorded turn request's whole `input` array.
fn input_of(exchange: &Exchange) -> Vec<Value> {
    exchange
        .body
        .as_ref()
        .and_then(|body| body["input"].as_array())
        .cloned()
        .unwrap_or_default()
}

/// The `function_call` items in a recorded request's `input`.
fn calls_in(exchange: &Exchange) -> Vec<&Value> {
    exchange
        .body
        .as_ref()
        .and_then(|body| body["input"].as_array())
        .map(|input| {
            input
                .iter()
                .filter(|item| item["type"] == "function_call")
                .collect()
        })
        .unwrap_or_default()
}

/// The `function_call_output` items in a recorded request's `input`.
fn outputs_in(exchange: &Exchange) -> Vec<&Value> {
    exchange
        .body
        .as_ref()
        .and_then(|body| body["input"].as_array())
        .map(|input| {
            input
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// The precondition every steering test below is built on: a resumed
/// `codex exec` continues the *same* roundhouse session.
///
/// Three roundhouse turns are needed to reach a steer — the trigger excludes
/// the first turn and the first intervention is always an escalation — and one
/// `codex exec` is one roundhouse turn unless a call is emitted. So the whole
/// fixture rests on `resume --last` keeping the conversation id: the client
/// sends it as `prompt_cache_key`, and roundhouse binds a session per
/// `(principal, cache key)`. If a resumed process started a fresh id, every run
/// would be turn one and nothing would ever validate.
///
/// Proved here, first and on its own, because the failure is silent in the
/// worst way: three green runs that each answered perfectly and never steered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real codex binary: --features e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
async fn a_resumed_exec_continues_the_same_roundhouse_session() {
    let rig = Rig::start("resume").await;

    let first = rig.exec("Say the word alpha and stop.").await;
    first.assert_completed("the first run");
    first.assert_catalog_was_used();
    let second = rig.resume("Now say the word beta and stop.").await;
    second.assert_completed("the resumed run");

    // The client's own view: one thread across two processes.
    assert_eq!(
        first.thread_id(),
        second.thread_id(),
        "`exec resume --last` must continue the thread the first run started"
    );

    // The wire's view: one `prompt_cache_key` on every turn request.
    let turns = rig.recorder.to("/v1/responses");
    assert!(
        turns.len() >= 2,
        "two runs are two turns; recorder saw:\n{}",
        rig.recorder.transcript()
    );
    let keys = turns
        .iter()
        .map(|exchange| {
            exchange.body.as_ref().expect("a JSON turn body")["prompt_cache_key"]
                .as_str()
                .expect("every request names its session")
                .to_string()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        keys.len(),
        1,
        "a resumed run must reuse the first run's cache key, but saw {keys:?}"
    );

    // Roundhouse's view: one session, both user messages in it, no fork.
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
        user_text.contains("alpha") && user_text.contains("beta"),
        "both runs' prompts must be in one session log, but it holds:\n{user_text}"
    );
    rig.assert_never_forked().await;

    rig.clean();
}

/// F15: a key revoked between two `codex exec` runs must fail the next turn,
/// and must not leave the session with a half-written turn behind it.
///
/// This is the one credential-lifecycle event the M8 admin plane exists to
/// perform, and the M9 rig is the only place able to watch it end to end: the
/// unit suites in `control_config/directory/tests.rs` (e.g.
/// `a_revoked_key_compiles_to_a_named_refusal`) prove the plane refuses a
/// revoked hash, but nothing there drives a real client across the boundary —
/// a second process, holding the same secret in its environment, that has to
/// discover the refusal itself.
///
/// **Open design question this test settles empirically, for codex-cli
/// 0.146.0, rather than assuming an answer:** does a *resumed* process's MCP
/// reconnect refuse first, or does `/v1/responses` refuse first? The answer
/// is not what `a_real_codex_binary_completes_the_mcp_handshake_against_our_server`
/// would suggest by analogy. That test shows a **fresh** `exec` sends
/// `initialize`/`tools/list` synchronously before its first turn request,
/// because it has no cached tool list yet. A **resumed** `exec` already has
/// one — from the rollout `resume --last` reopens — so it sends
/// `/v1/responses` immediately and reconnects to `/mcp` concurrently rather
/// than gating the turn on it. Stderr timestamps from a captured run show the
/// `/v1/responses` 401 (`codex_core::session::turn: Turn error: unexpected
/// status 401`) logged strictly before the MCP client's own 401
/// (`failed to initialize MCP client during shutdown`) — the MCP reconnect
/// fails too, but only as a consequence of the turn already having ended, not
/// as its cause. So for the credential-lifecycle path this test is actually
/// about — a revoked key discovered on a *resumed* run — `/v1/responses` is
/// the surface the client's turn depends on, and it is the one asserted below.
/// The MCP request is still checked when present (both must agree it is
/// `revoked_key`), but is not required to exist, since a client version that
/// deferred its MCP reconnect further could plausibly drop it from the same
/// process lifetime entirely.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "F15: needs the real codex binary: --features e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
async fn a_key_revoked_between_runs_fails_the_next_turn_and_leaves_no_half_written_one() {
    let rig = Rig::start("revoke").await;

    let first = rig.exec("Say the word alpha and stop.").await;
    first.assert_completed("the first run, before revocation");
    first.assert_catalog_was_used();

    let session = rig.session();
    let seq_before_revocation = rig
        .store
        .last_seq(&session)
        .await
        .expect("the first run's session exists");

    rig.revoke_turn_key();

    // Same `CODEX_HOME`, same secret in the child's environment (`spawn`
    // resolves it once, in `Rig::start`, and every run reuses it) — the only
    // thing that changed between the two runs is the directory's row.
    let second = rig.resume("Now say the word beta and stop.").await;

    assert!(
        !second.kinds().contains(&"turn.completed".to_string()),
        "a revoked key must not let the resumed run complete a turn, but it saw: {:?}\n\
         --- stdout\n{}\n--- stderr\n{}",
        second.kinds(),
        second.stdout,
        second.stderr
    );

    // The surface the resumed turn actually depends on: `/v1/responses` must
    // have been sent (a resumed run does not re-fetch tools before its first
    // turn, per the doc comment above) and refused by name, not merely by
    // status — `unknown_key` would mean the revocation never took effect on
    // this node, and a 200 would mean the key still worked.
    let turns_after = rig.recorder.to("/v1/responses");
    let last_turn = turns_after
        .last()
        .expect("the resumed run must have attempted its turn against /v1/responses");
    assert_eq!(
        last_turn.status,
        401,
        "a revoked key must refuse the resumed turn with 401, not accept it or answer some \
         other status; recorder:\n{}",
        rig.recorder.transcript()
    );

    // The MCP surface, if the client also reached it in this process: same
    // key, same directory, so it must agree. Not required to exist — see the
    // doc comment on why a resumed run's MCP reconnect is not load-bearing for
    // the turn and a future client could omit it from this process lifetime
    // entirely.
    if let Some(mcp_after) = rig.recorder.rpc("initialize").last() {
        assert_eq!(
            mcp_after.status,
            401,
            "the same revoked key must be refused on `/mcp` too, if the client reached it; \
             recorder:\n{}",
            rig.recorder.transcript()
        );
        assert_eq!(
            mcp_after
                .response
                .as_ref()
                .and_then(|body| body["error"]["code"].as_str()),
            Some("revoked_key"),
            "the refusal must be the tombstone's own code, not `unknown_key`: {:?}",
            mcp_after.response
        );
    }

    // The claim proving fix: no half-written turn. `create_response` resolves
    // admission before it ever reads the body or touches the store (see
    // `responses_api.rs`'s "Before the body is even read"), so a refused
    // second run must leave the session's log at exactly the sequence number
    // the first run left it at — not one `TurnStarted` further, and not one
    // partial `ItemAppended` further.
    let seq_after_revocation = rig
        .store
        .last_seq(&session)
        .await
        .expect("the session still exists");
    assert_eq!(
        seq_after_revocation, seq_before_revocation,
        "a refused turn must append nothing to the session log: expected the log to stay at \
         seq {seq_before_revocation}, found it at {seq_after_revocation}"
    );

    rig.clean();
}

/// A real codex binary completes the MCP handshake against our own service.
///
/// The first thing this milestone has to establish, and deliberately ahead of
/// any steering test: roundhouse serves `/mcp` through rmcp 3.1.3 and codex
/// dispatches through rmcp 1.8.0, a pairing nothing had ever exercised. A
/// protocol mismatch here would otherwise surface three runs later as "the
/// steer was never dispatched", and be diagnosed as a steering bug.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real codex binary: --features e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
async fn a_real_codex_binary_completes_the_mcp_handshake_against_our_server() {
    let rig = Rig::start("handshake").await;
    let run = rig.exec("Say the word alpha and stop.").await;
    run.assert_completed("the handshake run");

    let initialize = rig.recorder.rpc("initialize");
    assert_eq!(
        initialize.len(),
        1,
        "codex must have initialized the MCP server exactly once:\n{}",
        rig.recorder.transcript()
    );
    let initialize = &initialize[0];
    assert_eq!(initialize.status, 200, "the handshake was refused");
    assert_eq!(
        initialize
            .header("authorization")
            .map(|value| value.strip_prefix("Bearer ").unwrap_or(value).to_string()),
        Some(rig.secret.clone()),
        "the MCP surface must be reached with the same minted turn key"
    );

    let listed = rig.recorder.rpc("tools/list");
    assert!(
        !listed.is_empty(),
        "codex must have listed our tools:\n{}",
        rig.recorder.transcript()
    );
    let listed = &listed[0];
    assert_eq!(listed.status, 200, "tools/list was refused");
    let names = listed
        .response
        .as_ref()
        .and_then(|body| body["result"]["tools"].as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert!(
        names.contains(&"fetch_steer"),
        "the tool a steer is dispatched through must be listed, but tools/list answered: {:?}",
        listed.response
    );

    // Both credential doors, on the turn surface this time. `env_key` puts the
    // key in `Authorization` and `[model_providers.*.env_http_headers]` puts it
    // in ours — and the second is the half that carries the whole pass-through
    // stanza, where `Authorization` will belong to the client's own upstream.
    // The unit tests can only prove the generated file *says* so; this is the
    // only place that proves codex sends it.
    let turn = &rig.recorder.to("/v1/responses")[0];
    assert_eq!(
        turn.header(TURN_KEY_HEADER),
        Some(rig.secret.as_str()),
        "the dedicated turn-key header must have arrived: {:?}",
        turn.headers
    );
    assert_eq!(
        turn.header("authorization"),
        Some(format!("Bearer {}", rig.secret).as_str()),
        "and so must the bearer, since a BYOK stanza names one env var in both"
    );

    // The principal the key resolved to, read off the id the surface minted.
    // Configured mode qualifies every session as `{project}/{user}/{uuid}`, so
    // this is the assertion that the minted key authenticated as *its own*
    // membership rather than as some other principal that drove a turn.
    let session = rig.session();
    assert!(
        session.as_str().starts_with(&format!("{PROJECT}/{USER}/")),
        "a Configured deployment namespaces its sessions by principal, got `{session}`"
    );

    // The client's side of the same fact: codex advertises our server to the
    // model as one namespace rather than as eight tools, because MCP tools are
    // deferred under `ToolSearchAlwaysDeferMcpTools`. Direct dispatch still
    // resolves — which is what the steering test below proves — so this is the
    // shape to assert, not a per-tool entry.
    let tools = turn.body.as_ref().expect("a JSON turn body")["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        tools.iter().any(|tool| {
            tool["type"] == "namespace" && tool["name"] == roundhouse_server::DEFAULT_MCP_NAMESPACE
        }),
        "codex must offer the model our namespace: {tools:?}"
    );

    rig.clean();
}

/// F12: the other credential doors, on a real request.
///
/// Every assertion above this one runs under `RoundhouseKey`, where a BYOK
/// stanza names one env var for both the bearer and the dedicated header —
/// so `authorization` and `TURN_KEY_HEADER` carry the *same* value on every
/// prior green run, and "the dedicated header wins" is unobservable there:
/// nothing distinguishes "preferred" from "only option considered". This is
/// the first fixture where the two doors carry *different* values, which is
/// the only shape in which a preference is a testable property at all, and
/// the only real-binary evidence that `ForwardedOpenAiLogin` — whose only
/// caller anywhere in the workspace was its own two unit tests in
/// `codex_launch.rs` before this test existed — sends what its generated
/// config says it will.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "F12: needs the real codex binary: --features e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
async fn a_forwarded_login_sends_the_seat_and_our_key_on_the_same_request() {
    let rig = Rig::start_forwarding("forwarded").await;
    let run = rig.exec("Say the word alpha and stop.").await;
    run.assert_completed("the forwarded-login run");

    let turn = &rig.recorder.to("/v1/responses")[0];

    // The turn completed at all, which `presented_key`
    // (`control_config/mod.rs:896-940`) would refuse with `MalformedKey` had
    // it read `Authorization` first: `SEAT_ACCESS_TOKEN` is not `rh_*`-shaped.
    // That refusal is the mechanism this assertion pair is the wire evidence
    // for — the completion is not incidental to it.
    assert_eq!(
        turn.header("authorization"),
        Some(format!("Bearer {SEAT_ACCESS_TOKEN}").as_str()),
        "the forwarded stanza's Authorization must carry the client's own seat: {:?}",
        turn.headers
    );
    assert_eq!(
        turn.header(TURN_KEY_HEADER),
        Some(rig.secret.as_str()),
        "and roundhouse's own key must still arrive, in its own header: {:?}",
        turn.headers
    );
    assert_ne!(
        turn.header("authorization"),
        Some(format!("Bearer {}", rig.secret).as_str()),
        "the two doors must disagree here -- RoundhouseKey's BYOK stanza cannot tell \
         'preferred' from 'only option present' because both headers there carry the same \
         value (see this test's own module doc)"
    );

    rig.clean();
}

/// The whole milestone in one run: roundhouse decides to steer, emits a
/// synthetic MCP tool call, the real client dispatches it against the real
/// control surface, and the correction comes back into the conversation.
///
/// Three `codex exec` invocations because that is what three roundhouse turns
/// costs: turn 1 is unvalidated (the trigger's turn-index gate is not
/// configurable), turn 2 validates and escalates (the action map escalates
/// unconditionally on the first intervention), and turn 3 validates and steers.
/// The client then dispatches, appends, and resends — which is roundhouse turn
/// 4, inside the third process, and the one turn that must *not* be validated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real codex binary: --features e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
async fn a_real_codex_binary_executes_our_synthetic_tool_call_and_returns_its_output() {
    let rig = Rig::start("steer").await;
    let [first, second, third] = rig.drive_to_a_steer().await;

    // ---- what roundhouse emitted -------------------------------------------
    let items = rig.items().await;
    let calls = items
        .iter()
        .filter(|item| matches!(item.content, ItemContent::ToolCall { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        calls.len(),
        1,
        "exactly one synthetic call, or the client's resend forked rather than \
         extended the session:\n{items:#?}"
    );
    let call = calls[0];
    let ItemContent::ToolCall {
        call_id,
        name,
        arguments,
    } = &call.content
    else {
        unreachable!("filtered above")
    };
    assert_eq!(name, "fetch_steer");
    assert_eq!(call.role, Role::Assistant);
    // Emitted by this deployment, so the log carries the response it was
    // committed under. The client-sent output below carries none, and that
    // asymmetry is the only thing distinguishing the two on a replay.
    assert!(call.response_id.is_some(), "an emitted call is stamped");

    // ---- what the client dispatched ----------------------------------------
    let dispatched = rig.recorder.rpc("tools/call");
    let steer_call = dispatched
        .iter()
        .find(|exchange| {
            exchange.body.as_ref().map(|body| &body["params"]["name"])
                == Some(&Value::from("fetch_steer"))
        })
        .unwrap_or_else(|| {
            panic!(
                "codex must have dispatched our steer over /mcp:\n{}",
                rig.recorder.transcript()
            )
        });
    assert_eq!(steer_call.status, 200, "the dispatch was refused");
    assert_eq!(
        steer_call.body.as_ref().expect("a JSON-RPC body")["params"]["arguments"]["steer_id"]
            .as_str(),
        Some(call_id.as_str()),
        "the client must have dispatched the id the emitted call named"
    );

    // ---- what came back into the conversation ------------------------------
    let result = items
        .iter()
        .find(|item| matches!(item.content, ItemContent::ToolResult { .. }))
        .expect("the client resent the tool's output");
    let ItemContent::ToolResult {
        call_id: back,
        output,
    } = &result.content
    else {
        unreachable!("filtered above")
    };
    assert_eq!(back, call_id, "the output names the call it answers");
    assert_eq!(result.role, Role::Tool);
    assert!(
        result.response_id.is_none(),
        "an item the client sent is not stamped with a response of ours"
    );
    // `contains`, not equality: codex wraps an MCP result as
    // "Wall time: 0.004 seconds\nOutput:\n[…]", and the wall time is measured.
    // The M4 suite's byte-exact `ToolResult` assertion cannot be ported here,
    // and tightening it would be pinning the client's rendering rather than our
    // contract.
    assert!(
        output.contains(GUIDANCE_FRAGMENT),
        "the correction must have reached the agent through fetch_steer, but the output was:\n{output}"
    );
    assert!(
        !output.contains(JUDGE_PROSE),
        "the judge's own prose must never reach the agent — `render_directive` excludes it \
         precisely so a model that read attacker-influenceable transcript cannot write into a \
         payload the agent dispatches. Output was:\n{output}"
    );

    // ---- what the client resent --------------------------------------------
    let turns = rig.recorder.to("/v1/responses");
    let fulfilling = turns
        .iter()
        .find(|exchange| !calls_in(exchange).is_empty())
        .expect("one request carried the call back");
    let resent = calls_in(fulfilling);
    assert_eq!(resent.len(), 1, "one call went out, so one comes back");
    let resent = resent[0];
    assert_eq!(resent["name"].as_str(), Some("fetch_steer"));
    assert_eq!(
        resent["namespace"].as_str(),
        Some(roundhouse_server::DEFAULT_MCP_NAMESPACE),
        "the namespace is what makes the client's exact (namespace, name) lookup resolve"
    );
    assert_eq!(resent["call_id"].as_str(), Some(call_id.as_str()));
    // The one byte-exact field in the whole exchange. `arguments` is minted
    // once and echoed as an opaque string; a re-serialization — even a
    // semantically identical one — would canonicalize to a different item and
    // fork the session on the next turn.
    assert_eq!(
        resent["arguments"].as_str(),
        Some(arguments.as_str()),
        "`arguments` must come back byte-for-byte"
    );
    let outputs = outputs_in(fulfilling);
    assert_eq!(outputs.len(), 1, "the call is immediately answered");
    assert_eq!(outputs[0]["call_id"].as_str(), Some(call_id.as_str()));

    // ---- what the loop decided ---------------------------------------------
    assert_eq!(
        rig.judge.asked(),
        2,
        "turns 2 and 3 validate; turn 1 is below the trigger's gate and turn 4 fulfils a steer"
    );
    assert_eq!(
        rig.validations().await,
        2,
        "the fulfilling turn must never fire a validation of its own"
    );
    rig.assert_never_forked().await;

    // ---- §10.2 evidence: recorded, not asserted ----------------------------
    //
    // Deliberately printed rather than compared. What relationship *should*
    // hold between a judge's usage, the usage roundhouse reports for a steered
    // turn, and the client's own accumulation is the open question the plan's
    // §10.2 leaves to this milestone's evidence; an assertion here would be
    // this file deciding it.
    //
    // Four numbers per run and not three: the client's accumulation is only
    // interpretable against the window it is accumulating toward, and that
    // window is the catalog's, which this deployment wrote. A block that
    // printed the usages without it would leave the one arithmetic a reader
    // wants to do — how close did the client think it was to compacting —
    // impossible without opening another file.
    println!("--- M9-USAGE-EVIDENCE ({})", rig.version);
    println!("    catalog context_window: {CONTEXT_WINDOW_TOKENS}");
    for (label, run) in [("run 1", &first), ("run 2", &second), ("run 3", &third)] {
        println!("    client {label} turn.completed.usage: {:?}", run.usage());
    }
    for (index, usage) in rig.response_usage().await.iter().enumerate() {
        println!("    roundhouse response {} usage: {usage:?}", index + 1);
    }
    // The booked side calls, not `judge_usage()`: the constant is what the
    // fixture would report and the log is what the deployment recorded, and
    // §10.2 is a question about the second.
    for (index, usage) in rig.side_call_usage().await.iter().enumerate() {
        println!("    judge side call {} usage: {usage:?}", index + 1);
    }
    println!("    validated turns: {:?}", rig.validation_turns().await);
    println!("    final agent message: {:?}", third.last_message.trim());
    println!("--- end M9-USAGE-EVIDENCE");

    rig.clean();
}

/// §10.2 evidence, made concrete: F03 finds that the M9-USAGE-EVIDENCE block
/// above prints codex's *cumulative* `total_token_usage` (the client's own
/// `turn.completed.usage`) as if it reassured a reader about the compaction
/// gate, when the gate and `get_context_remaining` are actually driven by
/// `last_token_usage` — a value the pinned client *replaces*, never sums, on
/// every response (`codex-rs/protocol/src/protocol.rs:2122-2124`
/// `append_last_usage`; `codex-rs/core/src/context_manager/history.rs:415-419`
/// `get_total_token_usage`, which despite its name reads
/// `last_token_usage.total_tokens`).
///
/// A steered turn's completion carries the judge's usage, not a measure of
/// the history it stood in for (`Session::complete_with_item`'s "the usage is
/// the interjection's"), so on the real client that usage becomes the new
/// `last_token_usage` — collapsing it — for a turn about to resend the whole
/// growing conversation to fulfil the steer.
///
/// This asserts the relationship the "reassuring" reading implies: that the
/// steered turn's reported usage is at least in the neighbourhood of what the
/// very next real turn actually costs. It is not — measured on this box, the
/// steered turn (judge side call 1's usage, exactly) reports total 1147
/// tokens while the fulfilling turn's real input is 5666, a ~4.9x gap — which
/// is why the assertion is written as a red line rather than a green one.
#[ignore = "F03: the steered turn's booked usage (the judge's, exactly — asserted \
            below) understates the fulfilling turn's real input by ~5x on this \
            box; codex's compaction gate reads last_token_usage \
            (core/context_manager/history.rs:415-419), which this usage replaces \
            wholesale, not the cumulative total_token_usage the removed evidence \
            block printed — needs the real codex binary: --features e2e-codex -- \
            --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
#[tokio::test]
async fn a_steered_turns_reported_usage_understates_the_next_turns_real_input() {
    let rig = Rig::start("usage-evidence").await;
    let [_first, _second, _third] = rig.drive_to_a_steer().await;

    let responses = rig.response_usage().await;
    assert_eq!(
        responses.len(),
        4,
        "turn 1, turn 2, the steer deposit (turn 3), and the fulfilling turn \
         (turn 4): {responses:#?}"
    );
    let steered = &responses[2];
    let fulfilling = &responses[3];

    let side_calls = rig.side_call_usage().await;
    assert_eq!(
        *steered, side_calls[0],
        "the steered turn's booked usage must be exactly the judge's — it is \
         not a measure of the conversation the steer stood in for, which is \
         the premise the mismatch below rests on"
    );

    // The relationship a reader who takes the display accumulator as
    // reassuring would expect: that a steered turn's reported cost is roughly
    // in line with what the conversation actually costs one turn later. This
    // is the line that must go red for F03 to be more than a reading of
    // codex's source — it is the same gap `last_token_usage` opens on the
    // real client, made visible without needing codex-core as a dependency.
    assert!(
        fulfilling.input_tokens <= steered.total().saturating_mul(2),
        "F03: the fulfilling turn's real input ({} tokens) is {:.1}x the \
         steered turn's reported usage ({} tokens) -- on the real client this \
         is exactly the quantity that replaces last_token_usage between the \
         two turns, so a compaction gate reading last_token_usage sees the \
         small number, not the real one, for the turn most likely to need it",
        fulfilling.input_tokens,
        fulfilling.input_tokens as f64 / steered.total().max(1) as f64,
        steered.total(),
    );

    rig.clean();
}

/// The M4 resend contract, over a real client instead of a hand-written
/// history: the client sends the call *and* its output back, and roundhouse
/// recognizes them as the prefix they are.
///
/// The real-binary mirror of `steering_emission.rs`'s
/// `the_resent_call_and_its_output_extend_rather_than_fork`, which plays the
/// same round trip with Codex's own types but with the test writing the resend.
/// Everything that test asserts about *our* handling still holds here; what
/// only this one can say is that the history codex actually built is the one
/// that suite assumed it would.
///
/// Three claims live here and nowhere else in the suite:
///
/// 1. the wire `id` survives the round trip. 0.146.0 strips any item id without
///    an interior underscore (`client.rs:927-933` at `e363b08`, via
///    `ResponseItemId::is_prefixed`), and `fc_<response_id>` has one — so the
///    prediction is that it comes back, and either answer is evidence;
/// 2. the resent request is the previous one *extended*, not rebuilt: same
///    items in the same order, two appended. A client that re-serialized its
///    history differently would fork the session on the next turn, silently and
///    one turn late;
/// 3. nothing was inserted between the call and its output.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real codex binary: --features e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
async fn a_real_codex_binary_resends_the_call_and_output_and_the_session_does_not_fork() {
    let rig = Rig::start("resend").await;
    let _runs = rig.drive_to_a_steer().await;

    // ---- what roundhouse emitted, and under which response -----------------
    let items = rig.items().await;
    let calls = items
        .iter()
        .filter(|item| matches!(item.content, ItemContent::ToolCall { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        calls.len(),
        1,
        "the resent call must be recognized as the prefix it is, not appended a \
         second time:\n{items:#?}"
    );
    let call = calls[0];
    let ItemContent::ToolCall {
        call_id,
        name,
        arguments,
    } = &call.content
    else {
        unreachable!("filtered above")
    };
    let response_id = call
        .response_id
        .as_ref()
        .expect("an emitted call is stamped with the response that emitted it");

    // ---- the two requests the round trip spans -----------------------------
    let turns = rig.recorder.to("/v1/responses");
    let fulfilling = turns
        .iter()
        .position(|exchange| !calls_in(exchange).is_empty())
        .expect("one request carried the call back");
    assert!(
        fulfilling > 0,
        "the fulfilling turn is a resend, so it has a predecessor to be a resend *of*:\n{}",
        rig.recorder.transcript()
    );
    let before = input_of(&turns[fulfilling - 1]);
    let after = input_of(&turns[fulfilling]);

    // Printed unconditionally: the brief asks what the client did with the
    // fields it is free to rewrite, and an assertion answers that only when it
    // fails. This is the one place the resent item is visible as received.
    let resent = calls_in(&turns[fulfilling]);
    assert_eq!(resent.len(), 1, "one call went out, so one comes back");
    let resent = resent[0];
    println!("--- M9-RESENT-CALL as received");
    println!("{}", serde_json::to_string_pretty(resent).expect("JSON"));

    // ---- the resent call, field by field -----------------------------------
    assert_eq!(resent["name"].as_str(), Some(name.as_str()));
    assert_eq!(
        resent["namespace"].as_str(),
        Some(roundhouse_server::DEFAULT_MCP_NAMESPACE),
        "the namespace is what makes the client's exact (namespace, name) lookup resolve"
    );
    assert_eq!(resent["call_id"].as_str(), Some(call_id.as_str()));
    assert_eq!(
        resent["id"].as_str(),
        Some(format!("fc_{response_id}").as_str()),
        "0.146.0 keeps an item id only if it has an interior underscore, and `fc_…` \
         does — so this is the assertion that our own item-id spelling survives a \
         real client. If it is gone, the projection must stop claiming an id the \
         client will not carry."
    );
    // The one byte-exact field in the whole exchange. `arguments` is minted
    // once and echoed as an opaque string; a re-serialization — even a
    // semantically identical one — canonicalizes to a different item and forks
    // the session on the next turn.
    assert_eq!(
        resent["arguments"].as_str(),
        Some(arguments.as_str()),
        "`arguments` must come back byte-for-byte"
    );

    // ---- the call and its output are adjacent ------------------------------
    let at = after
        .iter()
        .position(|item| item["type"] == "function_call")
        .expect("the call is in the resent input");
    assert_eq!(
        after[at + 1]["type"].as_str(),
        Some("function_call_output"),
        "the output must sit immediately after the call it answers, or a turn \
         that read the history in order would see an unanswered call:\n{:#?}",
        &after[at..]
    );
    assert_eq!(after[at + 1]["call_id"].as_str(), Some(call_id.as_str()));

    // ---- the resend extends rather than rebuilds ---------------------------
    //
    // Pairwise on the overlap rather than a whole-array compare, so a failure
    // names the item that moved instead of printing two 40 KB documents. Whole
    // `Value` equality per element is deliberate here and not the "assert on
    // parsed values" rule being broken: the rule is about not byte-comparing
    // *serializations*, and two items pulled from two parsed documents compare
    // as values whatever order their fields arrived in.
    assert_eq!(
        after.len(),
        before.len() + 2,
        "the fulfilling turn resends what it had plus the call and its output, and \
         nothing else — it is not a fresh history and there is no new user message \
         in it (no human spoke between the two requests)"
    );
    for (index, (was, now)) in before.iter().zip(after.iter()).enumerate() {
        assert_eq!(
            was, now,
            "item {index} was rewritten between the two requests of one turn; a \
             client that rebuilds its history rather than extending it forks this \
             session on the turn after the one anybody is watching"
        );
    }
    assert_eq!(after[before.len()]["type"].as_str(), Some("function_call"));
    assert_eq!(
        after[before.len() + 1]["type"].as_str(),
        Some("function_call_output")
    );

    // ---- and the store agrees ----------------------------------------------
    let result = items
        .iter()
        .find(|item| matches!(item.content, ItemContent::ToolResult { .. }))
        .expect("the client resent the tool's output");
    let ItemContent::ToolResult { call_id: back, .. } = &result.content else {
        unreachable!("filtered above")
    };
    assert_eq!(back, call_id, "the output names the call it answers");
    assert_eq!(result.role, Role::Tool, "a tool result is the tool's turn");
    // The call kept its stamp and the result never had one: that asymmetry is
    // the only thing distinguishing an emitted call from a client-sent item on
    // a replay, and it has to survive the client's round trip to be usable.
    assert!(result.response_id.is_none());
    rig.assert_never_forked().await;

    rig.clean();
}

/// The correction reaches the agent, the turn that acts on it is not judged
/// again, and the run ends with an answer.
///
/// "The correction" is a term of art: it is `render_directive`'s output
/// (`verdict.rs:441-467`), built from roundhouse's own structured facts — the
/// step the judge located, the signals that fired — and **never** the judge's
/// prose. So this test asserts a fragment of the directive arrived and that the
/// judge's own sentence did not, in the same breath. The second half is the
/// injection boundary, and it is the half a passing suite would otherwise never
/// mention.
///
/// The other claim is about restraint rather than delivery: the turn that
/// fulfils a steer must not itself be validated. Validating it would judge the
/// agent on a turn whose whole content is the correction being obeyed, and
/// would do it while the previous verdict is still the freshest evidence — a
/// loop that interrupts its own repair. `validate_loop.rs`'s
/// `a_turn_fulfilling_an_open_steer_never_fires` proves it at the engine level;
/// here it is proved across a real client's dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real codex binary: --features e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
async fn the_next_turn_reflects_the_correction() {
    let rig = Rig::start("correction").await;
    let [_first, _second, third] = rig.drive_to_a_steer().await;

    // ---- the directive, in the agent's context -----------------------------
    let items = rig.items().await;
    let result = items
        .iter()
        .find(|item| matches!(item.content, ItemContent::ToolResult { .. }))
        .expect("the client resent the tool's output");
    let ItemContent::ToolResult { output, .. } = &result.content else {
        unreachable!("filtered above")
    };
    // The directive's *closing* sentence, where the steering test above asserts
    // its opening one. Deliberately the other end of the string: an assertion
    // on the first line would pass against a correction that had been truncated
    // to its diagnosis, and the instruction — what the agent is being asked to
    // do differently — is the half that makes it a correction at all.
    assert!(
        output.contains(DIRECTIVE_INSTRUCTION),
        "the agent must have received the instruction half of the correction, but \
         `fetch_steer` returned:\n{output}"
    );

    // ---- and the judge's prose nowhere at all ------------------------------
    //
    // Every captured document, request and response alike. The request bodies
    // are the weaker half of this check: the one place the description could
    // physically appear is the `/mcp` response that carries the steer, since
    // that payload is the thing `render_directive` built. A sweep that read
    // only requests would be checking the client's honesty rather than ours.
    for exchange in rig.recorder.all() {
        for (half, document) in [
            ("request", &exchange.body),
            ("response", &exchange.response),
        ] {
            let Some(document) = document else { continue };
            let rendered = document.to_string();
            assert!(
                !rendered.contains(JUDGE_PROSE),
                "the judge's own prose must never leave the log: found it in the \
                 {half} of {} {}. `render_directive` excludes it precisely so a \
                 model that read attacker-influenceable transcript cannot write \
                 into a payload the agent dispatches.",
                exchange.method,
                exchange.path
            );
        }
    }

    // ---- what the loop decided, and on which turns --------------------------
    //
    // The indices and not the count: two validations decided on turns 3 and 4
    // would satisfy a `== 2` while breaking the whole claim, and the trigger
    // keeps the observation it acted on in the event, so there is no reason to
    // infer the turns from arithmetic.
    assert_eq!(
        rig.validation_turns().await,
        vec![2, 3],
        "turns 2 and 3 validate; turn 1 is below the trigger's turn-index gate and \
         turn 4 fulfils an open steer, which the trigger refuses to judge"
    );
    assert_eq!(
        rig.judge.asked(),
        2,
        "one judge consultation per validation, and no more"
    );

    // ---- the run finished as a run, not as a steer --------------------------
    //
    // `-o` is read with `unwrap_or_default`, so an explicit emptiness check is
    // what keeps a missing file from reading as a passing empty answer. The
    // exit status is already asserted by `drive_to_a_steer`; what this adds is
    // that the fulfilling turn produced a *message*, which is the only evidence
    // that the client came back from the tool call and finished its turn rather
    // than ending on the dispatch.
    assert!(
        !third.last_message.trim().is_empty(),
        "the steered run must end with an agent message, but `-o` was empty. stdout:\n{}",
        third.stdout
    );
    assert!(
        third.last_message.contains(ANSWER),
        "and that message is the one roundhouse served: {:?}",
        third.last_message
    );
    rig.assert_never_forked().await;

    rig.clean();
}

/// F11: the injection-boundary sweep in `the_next_turn_reflects_the_correction`
/// reads "every captured document, request and response alike" and asserts
/// `JUDGE_PROSE` is absent from both halves of every exchange. That is true of
/// what is captured — but `record` only ever parses a response body for
/// [`MCP_MOUNT_PATH`] (see the doc comment on `Exchange::response`), so a
/// `SteerAction::Halt`'s reason — committed as the assistant text of the very
/// `/v1/responses` response that ends the run — is never captured at all, and
/// unlike a `Steer`, there is no next turn to resend it one turn late.
///
/// This does not need the real codex binary: it drives the actual `record`
/// middleware, defined in this file, against a synthetic `/v1/responses`
/// route that answers with `JUDGE_PROSE` in its body — standing in for a
/// Halt's rendered directive — and shows the recorder never sees it.
#[tokio::test]
#[ignore = "F11: record() only parses a response body for MCP_MOUNT_PATH (codex_e2e.rs:325-338), \
            so a Halt's reason committed as the /v1/responses body is never captured and the \
            injection-boundary sweep in `the_next_turn_reflects_the_correction` cannot see it; \
            the mitigation documented on that test (a later turn resends the text) does not apply \
            to Halt, which ends the run. Fix: narrow the sweep's doc claim to name Halt as \
            structurally uncovered, or extend `record` to capture bounded non-streaming \
            /v1/responses bodies (non-stream requests only) so a real e2e Halt test can assert on it."]
async fn the_injection_sweep_cannot_see_a_halts_reason_in_the_v1_responses_body() {
    use axum::routing::post;
    use tower::ServiceExt;

    let recorder = Recorder::default();

    // Stands in for the Halt path (`engine.rs`'s `Interjection::Complete` arm):
    // a `/v1/responses` response whose body is the judge's rendered directive.
    // A real Halt never reaches this test's harness — it needs a live codex
    // binary — so this exercises the one thing that does not: the capture
    // boundary `record` draws before any sweep ever runs.
    async fn halt_like_response() -> String {
        format!("{{\"halt_reason\": \"{JUDGE_PROSE}\"}}")
    }

    let app: Router = Router::new()
        .route("/v1/responses", post(halt_like_response))
        .layer(axum::middleware::from_fn_with_state(
            recorder.clone(),
            record,
        ));

    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .expect("a well-formed request");

    let response = app.oneshot(request).await.expect("the app answers");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let exchange = recorder
        .to("/v1/responses")
        .into_iter()
        .next()
        .expect("record() must have captured the request that just went through");

    // The claim under test: a response body containing JUDGE_PROSE on
    // `/v1/responses` is captured (so the sweep could catch a Halt's leaked
    // reason). Today it is not — `record` never parses this path's response
    // — so this assertion fails, which is F11's mechanism made concrete.
    assert!(
        exchange
            .response
            .as_ref()
            .is_some_and(|body| body.to_string().contains(JUDGE_PROSE)),
        "record() must capture a /v1/responses response body containing the \
         judge's prose so `the_next_turn_reflects_the_correction`'s sweep can \
         see it, but the captured exchange's `response` field was {:?}. This is \
         exactly the gap F11 names: a Halt's reason lands only in this body, on \
         the one path `record` never parses.",
        exchange.response
    );
}

/// F09: codex stamps `params._meta.threadId` on **every** `tools/call` it
/// dispatches — unconditionally, via `with_mcp_tool_call_thread_id_meta`
/// (`codex-rs/core/src/mcp_tool_call.rs` at the pin, `6344a65`) — and that
/// value is byte-identical to the `prompt_cache_key` the same process's
/// `/v1/responses` traffic carries, both of them the client's own
/// `sess.thread_id`. Neither `ControlPlaneReads::resolve_session`
/// (`crates/roundhouse-server/src/mcp_api.rs`, resolves from the tool's own
/// `conversation` argument or `Conversations::latest`) nor `fetch_steer`
/// (`crates/roundhouse-mcp/src/plane.rs`, resolves from `request.steer_id`
/// alone) ever reads it.
///
/// This is not a behavioral defect: tenant-scoped `Principal` plus
/// `Conversations::latest`/the qualified `conversation` argument already
/// isolate sessions correctly with no help from `_meta`, so the assertion
/// below is expected to **pass today**. The point it proves is narrower and
/// still real — that a free, client-supplied session correlator rides every
/// single tool call and is discarded — which the documented-assumption block
/// this suite retires (`crates/roundhouse-mcp/src/lib.rs`'s module doc, "Note
/// the tense") does not mention. That block describes dispatch and resend as
/// proven and says nothing about a correlator M9's own captures show arriving
/// on every call, so its account of what M9 closed is incomplete rather than
/// wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "F09: doc-accuracy only, not a behavioral defect — needs the real codex binary: --features e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
async fn codexs_meta_thread_id_rides_every_tools_call_and_is_never_read() {
    let rig = Rig::start("meta-thread-id").await;
    let [_first, _second, third] = rig.drive_to_a_steer().await;

    let dispatched = rig.recorder.rpc("tools/call");
    let steer_call = dispatched
        .iter()
        .find(|exchange| {
            exchange.body.as_ref().map(|body| &body["params"]["name"])
                == Some(&Value::from("fetch_steer"))
        })
        .unwrap_or_else(|| {
            panic!(
                "codex must have dispatched our steer over /mcp:\n{}",
                rig.recorder.transcript()
            )
        });

    let meta_thread_id =
        steer_call.body.as_ref().expect("a JSON-RPC body")["params"]["_meta"]["threadId"]
            .as_str()
            .map(str::to_string);
    let reported_thread_id = third.thread_id();

    assert_eq!(
        meta_thread_id, reported_thread_id,
        "codex must stamp params._meta.threadId on every tools/call with the \
         same id it reports as thread_id on `thread.started`, but the dispatched \
         fetch_steer call carried {meta_thread_id:?} while the client reported \
         {reported_thread_id:?}. Roundhouse never reads this field either way \
         (`ControlPlaneReads::resolve_session` resolves from the tool's own \
         `conversation` argument or `Conversations::latest`; `fetch_steer` \
         resolves from `request.steer_id` alone) — this assertion passing is \
         exactly F09's point: the correlator exists on the wire and is unused."
    );

    rig.clean();
}
