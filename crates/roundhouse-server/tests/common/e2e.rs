// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The harness the **real-binary** suites share.
//!
//! [`codex_e2e`](../codex_e2e.rs) and [`claude_e2e`](../claude_e2e.rs) drive two
//! different clients over two different dialects, and above that difference they
//! are the same rig: bind a loopback roundhouse, bootstrap a Configured control
//! directory and mint one turn key, record every exchange as it arrives, spawn a
//! real binary with a cleared environment, and read the session log back.
//!
//! **Written because the second suite was a copy of the first** (M11.2b review
//! F1). `claude_e2e.rs` was landed by copying `codex_e2e.rs`'s recorder,
//! bootstrap, fork probe and version probe line for line, and within one
//! milestone the copies had already disagreed: codex's redactor learned about
//! `chatgpt-account-id` and claude's never learned about `x-api-key`. Nothing
//! reported that, because a copy has no seam to report from. What that drift
//! cost was small *this* time — the only value either suite ever puts on
//! `x-api-key` is the launcher's public sentinel, so the gap was hygiene rather
//! than a leak — and the next divergence is the one nobody gets to audit in
//! advance. One redactor, named here, is what makes "which headers does this
//! harness consider credential-bearing" a question with one answer.
//!
//! What deliberately stays per-suite: the client's own argv and launch
//! generator, the topology, and every assertion. This file holds what stands
//! *around* a real client, never what a suite claims about one.
//!
//! The `topham` closure helpers at the bottom arrived the same way and for the
//! same reason (M11.3 review F18): both suites grew their own copy of the
//! launcher's binary override, profile name, version probe and `PATH` splice in
//! one milestone, and the copies had already drifted in how they built an
//! isolated home before anyone read them side by side.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value;

use roundhouse_core::control::Principal;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::Item;
use roundhouse_core::now_ms;
use roundhouse_core::routing::{Candidate, Target};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::FrontierModelSpec;
// The name a profile's `key-env` defaults to, read from the generator that
// defines it rather than spelled here: a fixture that named its own variable
// would keep passing after the default moved and prove nothing about the file
// an operator types.
use roundhouse_server::codex_launch::DEFAULT_KEY_ENV;
use roundhouse_server::control_config::{MembershipRole, MintedKey, TURN_KEY_HEADER};
use roundhouse_server::{
    ControlDirectory, Conversations, CrossChecks, DirectoryMutation, MemoryDirectoryStore,
};

/// The tenant every real-binary run authenticates as.
pub const PROJECT: &str = "acme";
pub const USER: &str = "ada";

/// The principal a minted turn key resolves to in either suite.
pub fn principal() -> Principal {
    Principal::new(PROJECT, USER)
}

// ---------------------------------------------------------------------------
// The recorder
// ---------------------------------------------------------------------------

/// One request the deployment served, as it arrived.
#[derive(Clone, Debug)]
pub struct Exchange {
    pub method: String,
    pub path: String,
    /// The request's query string, if any.
    ///
    /// Kept apart from `path` rather than folded into it because the two are
    /// asserted for opposite reasons: every filter below matches on the path
    /// alone (axum routes past the query, and so must the filter), while a
    /// chained Messages turn asserts on the query alone — `?beta=true` surviving
    /// Relay's base-plus-path-and-query concatenation is R7 hazard 3, and a
    /// combined string would make "the route matched" and "the query survived"
    /// one assertion that cannot fail separately.
    pub query: Option<String>,
    pub headers: BTreeMap<String, String>,
    /// The request body, parsed if it was JSON.
    ///
    /// Parsed rather than kept as bytes because every assertion downstream is on
    /// a *value*, and a client re-serializes what it resends in its own field
    /// order. The one field that is byte-exact — a tool call's `arguments` — is
    /// a JSON string, and comparing two `String`s pulled out of two parsed
    /// documents is still a byte comparison of that field.
    pub body: Option<Value>,
    pub status: u16,
    /// The response body as bytes-turned-text, captured on **every** path.
    ///
    /// What buffering a streaming route costs, stated rather than assumed: the
    /// child sees one turn's frames arrive at once instead of as they are
    /// produced. No assertion in either suite is about frame *timing*, every
    /// turn is served by an in-process frontier and finishes in milliseconds,
    /// and both clients parse a complete SSE body identically to an incremental
    /// one. What is traded away is this harness's fidelity to backpressure,
    /// which nothing here measures; what is bought is that the stream the client
    /// actually read is inspectable at all.
    pub response_text: Option<String>,
    /// The response body parsed as one JSON document, when it is one.
    ///
    /// A JSON-RPC surface answers exactly one document per POST, which is what
    /// makes a handshake assertion readable. A streaming surface answers SSE, so
    /// this stays `None` there and [`Exchange::frames`] is the accessor.
    pub response: Option<Value>,
}

impl Exchange {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// The `messages` array of this request's body, in arrival order.
    pub fn messages(&self) -> Vec<Value> {
        self.body
            .as_ref()
            .and_then(|body| body["messages"].as_array())
            .cloned()
            .unwrap_or_default()
    }

    /// The SSE `data:` payloads of this response, parsed, in arrival order.
    ///
    /// Parsed on demand rather than at capture time because the recorder is a
    /// transport-level thing and SSE framing is a property of one route: a
    /// recorder that pre-parsed frames would have to know which paths stream.
    pub fn frames(&self) -> Vec<Value> {
        self.response_text
            .as_deref()
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
            .collect()
    }

    /// The first SSE frame whose `type` is `kind`.
    pub fn frame(&self, kind: &str) -> Option<Value> {
        self.frames()
            .into_iter()
            .find(|frame| frame["type"].as_str() == Some(kind))
    }

    /// The headers as an evidence block or a failure message should print them:
    /// credential-bearing values replaced by their length.
    ///
    /// **One list for both suites, which is the point.** Every value either
    /// suite has ever put on one of these headers is either a key the test
    /// minted seconds earlier or a hermetic constant compiled into the file, so
    /// redaction costs nothing today. It is here because the shape of this block
    /// is what a fixture holding something real would copy, and because the two
    /// copies this replaced had already drifted to two different lists — the
    /// failure mode where a header is credential-bearing in one suite's opinion
    /// and printable in the other's. The diagnostic that survives redaction —
    /// "the header arrived, and was this big" — is the whole reason a failure
    /// message prints headers at all.
    pub fn redacted_headers(&self) -> BTreeMap<String, String> {
        self.headers
            .iter()
            .map(|(name, value)| {
                let value = match name.as_str() {
                    "authorization" | TURN_KEY_HEADER | "chatgpt-account-id" | "x-api-key" => {
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
pub struct Recorder {
    pub exchanges: Arc<Mutex<Vec<Exchange>>>,
}

impl Recorder {
    pub fn all(&self) -> Vec<Exchange> {
        self.exchanges.lock().expect("recording").clone()
    }

    /// Every request to `path`, in arrival order.
    ///
    /// Matched on the path alone: a client may append a query string, which axum
    /// routes past and this filter must too — a comparison against the whole URI
    /// would find nothing and every assertion downstream would fail as "the
    /// client never sent a turn".
    pub fn to(&self, path: &str) -> Vec<Exchange> {
        self.all()
            .into_iter()
            .filter(|exchange| exchange.path == path)
            .collect()
    }

    /// A one-line rendering of every exchange, for a failure message.
    ///
    /// The JSON-RPC method is appended only when the body carries one, so the
    /// Messages suite's transcript stays two columns wide and the MCP suite's
    /// keeps the field that tells its requests apart.
    pub fn transcript(&self) -> String {
        self.all()
            .iter()
            .map(|exchange| {
                let line = format!(
                    "{} {} -> {}",
                    exchange.method, exchange.path, exchange.status
                );
                match exchange
                    .body
                    .as_ref()
                    .and_then(|body| body["method"].as_str())
                {
                    Some(rpc) => format!("{line} (jsonrpc method: {rpc})"),
                    None => line,
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Capture what arrived, without changing what is served.
///
/// A tower layer over the *merged* app rather than a wrapper per router, because
/// the interleaving is part of the subject: a steered codex turn is a
/// `/v1/responses` response followed by an `/mcp` dispatch followed by another
/// `/v1/responses` request, and three separate recorders could not say that.
pub async fn record(State(recorder): State<Recorder>, request: Request, next: Next) -> Response {
    let method = request.method().to_string();
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
    // Generously bounded: one turn of a real coding agent is already tens of KB
    // of instructions and tool schemas, and a resend carries the whole history.
    // A silent truncation here would surface as a 422 from our own canonicalizer,
    // which reads exactly like a roundhouse bug and is not one.
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
            method,
            path,
            query,
            headers,
            body: parsed,
            status,
            response: serde_json::from_slice::<Value>(&bytes).ok(),
            response_text: text,
        });
    Response::from_parts(response_parts, Body::from(bytes))
}

// ---------------------------------------------------------------------------
// The deployment behind the socket
// ---------------------------------------------------------------------------

/// A Configured control directory and the one turn key a run authenticates with.
pub struct Bootstrapped {
    pub directory: Arc<ControlDirectory>,
    pub minted: MintedKey,
}

/// Bootstrap a Configured deployment and mint one turn key for [`principal`].
///
/// Bootstrap is file-only, by design: `admin_keys` in the file is the sole root
/// of trust, and a directory with no admin plane refuses to mint. The arm salt
/// is here for the same reason — it is deployment-wide file state no admin write
/// can move.
///
/// A *minted* key against the production `PlaneSource` — an
/// `Arc<ControlDirectory>`, the one a shipped binary can name — rather than a
/// file-declared one, because the point of a real-binary rung is that a real
/// client authenticates the way a real tenant does.
///
/// `project` is the caller's, not this function's: a project's `validate` block
/// is the *file* vocabulary for enrolment, so a suite that wants a steered turn
/// and a suite that wants none differ in exactly that document and in nothing
/// else here.
pub fn bootstrap(
    label: &str,
    arm_salt: &str,
    project: Value,
    judge: Option<FrontierModelSpec>,
) -> Bootstrapped {
    let admin = super::admin_key("root");
    let file = super::control_plane(
        serde_json::json!({
            "projects": [],
            "users": [],
            "admin_keys": [super::sha256_hex(&admin)],
            "arm_salt": arm_salt,
        }),
        label,
    );
    let directory = Arc::new(
        ControlDirectory::new(
            file,
            "ROUNDHOUSE_CONTROL_PLANE",
            Arc::new(MemoryDirectoryStore::new()),
            // A project whose `validate` block enrols its sessions promises a
            // judge, and the startup cross-check refuses a plane that promises
            // one with none reachable — so the judge spec travels with the
            // project entry rather than being assumed either way.
            CrossChecks::new(reachable(), judge),
            now_ms(),
        )
        .expect("the bootstrap file alone compiles"),
    );
    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: serde_json::from_value(project)
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

    Bootstrapped { directory, minted }
}

/// Every target a real-binary deployment can route to, priced the way the router
/// prices them.
///
/// The one model [`frontier_catalog`](super::frontier_catalog) declares and
/// nothing else: no fleet is attached, so a turn has exactly one place to go and
/// "which target answered" is never a race.
pub fn reachable() -> Vec<Candidate> {
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

/// The session this principal last drove a turn on, discovered rather than
/// predicted.
///
/// A test cannot know the client's own conversation UUID in advance — it is
/// minted inside the child — and a Configured deployment qualifies the name by
/// principal on top of that. This is the production accessor, reading the same
/// `Arc<Conversations>` the router was handed.
pub fn session(conversations: &Conversations) -> SessionId {
    conversations
        .latest(&principal())
        .expect("the client drove at least one turn")
}

/// The session's committed items, in log order.
pub async fn items(store: &MemoryStore, session: &SessionId) -> Vec<Item> {
    store
        .read_events(session, 0, 4096)
        .await
        .expect("the session exists")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ItemAppended { item } => Some(item),
            _ => None,
        })
        .collect()
}

/// The generation-zero id behind `session`, whatever generation it is at.
///
/// `conversations::bound_session` spells generation zero as the namespaced key
/// verbatim and every later generation as `{key}#g{n}` — pinned by
/// `conversations::tests::a_reader_and_a_turn_resolve_one_cache_key_to_one_session`
/// — so the suffix *is* the fork, and stripping it recovers the stem. Sound here
/// because the stem ends in a UUID, which carries no `#`: there is no key this
/// can truncate by accident.
pub fn base_session(session: &SessionId) -> SessionId {
    match session.as_str().split_once("#g") {
        Some((base, _)) => SessionId::new(base),
        None => session.clone(),
    }
}

/// The session id a first fork of `session`'s conversation would have created.
///
/// A free function rather than a method on either rig so the guard it powers can
/// be evaluated without a rig, a binary or a socket — an arithmetic no test can
/// evaluate is how a vacuous fork probe survives. Derived from [`base_session`]
/// and never from `Conversations::latest`: a fork moves `latest` to the forked
/// id *before* any assertion runs, so appending `#g1` to it asks about
/// `key#g1#g1`, which nothing ever creates and whose absence therefore says
/// nothing.
pub fn fork_probe(session: &SessionId) -> SessionId {
    SessionId::new(format!("{}#g1", base_session(session)))
}

/// A fork is silent from the client's side, so the only way to catch one is to
/// ask the store whether generation one exists at all.
///
/// Two assertions rather than one, because they fail on different evidence. The
/// first reads the binding: `Conversations::commit` moves `latest` to the forked
/// id, so a session id that still carries no generation suffix is this node's own
/// statement that nothing rebound. The second reads the store, which does not
/// depend on the binding table being right about itself.
pub async fn assert_never_forked(store: &MemoryStore, session: &SessionId) {
    let probe = fork_probe(session);
    assert_eq!(
        session,
        &base_session(session),
        "the client's resend must have matched its prefix: this principal's latest session is \
         `{session}`, and a generation suffix means the prefix check refused the claim and \
         rebound the conversation"
    );
    assert!(
        store.last_seq(&probe).await.is_err(),
        "the client's resend must have matched its prefix: `{probe}` exists, which means the \
         prefix check refused the claim and rebound the conversation"
    );
}

/// Remove a run's directory.
///
/// Called explicitly at the end of a passing test rather than from a `Drop`: a
/// guard fires on unwind too, which would delete the isolated home and the
/// session store of the run that just failed — the only two artefacts worth
/// having at that moment.
pub fn clean(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

/// A scratch home for a version probe, thrown away as soon as it answers.
///
/// A probe cannot borrow the rig's root: the Relay probes run from tests that
/// have no rig at all, and the client probe runs before a root is fully
/// furnished. Its own directory per call is what lets one isolation rule cover
/// every process these suites spawn.
pub fn probe_home() -> PathBuf {
    std::env::temp_dir().join(format!("roundhouse-version-probe-{}", uuid::Uuid::new_v4()))
}

/// What `<binary> --version` prints, or a loud panic naming `override_var`.
///
/// **Spawned under isolation like every other process these suites start**
/// (M11.2b review F18). The probes used to build their command inline —
/// `Command::new(binary).arg("--version")`, no `env_clear()` — which made them
/// the one exception to the module rule each suite states about itself, and
/// inside this repository's own container that exception runs `claude` against
/// the developer's real `HOME` with `CLAUDE_CODE_REMOTE=true` still set. The
/// alternative to fixing it was to keep arguing the exception ("it only prints a
/// version") every time somebody read the rule; the exception is cheaper to
/// delete than to defend.
///
/// The isolation set is the caller's because it is client-specific: the variable
/// that points a client at an isolated config directory is not the same variable
/// for both, and a shared list would have to name both clients' vocabulary here.
/// `PATH` is supplied for every caller, since a probe that cannot find its own
/// dynamic loader fails in a way that reads as a missing binary.
pub fn version_probe(binary: &str, isolation: &[(&str, OsString)], override_var: &str) -> String {
    let mut command = std::process::Command::new(binary);
    command.arg("--version");
    command.env_clear();
    command.env("PATH", std::env::var("PATH").unwrap_or_default());
    for (name, value) in isolation {
        command.env(name, value);
    }
    let output = command.output().unwrap_or_else(|error| {
        panic!(
            "--include-ignored asks for the real binary; `{binary} --version` failed: {error}. \
             Set {override_var} to one, or run without --include-ignored."
        )
    });
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

// ---------------------------------------------------------------------------
// The `topham` closure tests
// ---------------------------------------------------------------------------

/// The environment variable naming the built `topham` a closure test drives.
///
/// No `PATH` fallback, unlike the two client binaries' overrides: `topham` is
/// this workspace's own binary and is installed nowhere, so a bare name would
/// resolve to whatever a developer happened to have — an older build, or
/// something else entirely. The variable is the only honest way to say "this
/// tree's `target/debug/topham`", and a missing one under `--include-ignored`
/// is the same loud panic a missing client is.
pub const TOPHAM_BIN_VAR: &str = "ROUNDHOUSE_TEST_TOPHAM_BIN";

/// The profile name every `topham` closure test writes and resolves.
///
/// One name for both agents and both topologies because each test owns its own
/// rig, and therefore its own isolated configuration directory: two tests can
/// hold this name and mean two different files, which is what a per-rig
/// `XDG_CONFIG_HOME` is for.
pub const TOPHAM_PROFILE: &str = "e2e";

/// The built `topham` a closure test drives, or a loud panic naming the
/// variable that would have said where it is.
///
/// A panic and never a fallback: see [`TOPHAM_BIN_VAR`]. The failure an
/// operator of these suites is most likely to cause is forgetting to *rebuild*
/// the launcher after changing it, and a test that silently ran the last build
/// would report green for code nobody compiled.
pub fn topham_binary() -> String {
    std::env::var(TOPHAM_BIN_VAR).unwrap_or_else(|_| {
        panic!(
            "the closure tests drive this workspace's own launcher: set {TOPHAM_BIN_VAR} to a \
             freshly built `target/debug/topham` (`cargo build -p topham`), or run without \
             --include-ignored"
        )
    })
}

/// What `topham --version` prints, or a loud panic naming the override.
///
/// Isolated the way every other probe in these suites is (M11.2b review F18),
/// and with one extra reason of its own: `topham` reads profiles out of
/// `XDG_CONFIG_HOME`, so a probe that inherited the developer's would resolve —
/// and, if a future `--version` grew a diagnostic, print — profiles no suite
/// here ever wrote.
///
/// **The banner is checked against this checkout's HEAD** (M11.3 review F23),
/// which is the `VERIFIED_*` discipline the `claude` and `nemo-relay` probes
/// already follow, aimed at the drift that a binary built here rather than
/// downloaded actually has: `topham` is not a version anybody pinned, it is
/// *this tree*, so the only way a green closure run lies about it is by having
/// driven a `target/debug/topham` nobody rebuilt. A printed warning rather than
/// a panic, because a working tree with uncommitted changes legitimately
/// differs from HEAD and a launcher rebuilt from it is the normal state of a
/// fix stage — the thing worth catching is the build from *another* commit.
pub fn topham_version(binary: &str) -> String {
    let home = probe_home();
    std::fs::create_dir_all(&home).expect("the probe's isolated home");
    let version = version_probe(
        binary,
        &[
            ("HOME", home.clone().into_os_string()),
            ("XDG_CONFIG_HOME", home.join("config").into_os_string()),
            ("XDG_DATA_HOME", home.join("data").into_os_string()),
        ],
        TOPHAM_BIN_VAR,
    );
    let _ = std::fs::remove_dir_all(&home);
    if let Some(head) = head_commit()
        && !version.contains(&head)
    {
        println!(
            "    WARNING: `topham --version` says {version:?}, which does not name this \
             checkout's HEAD ({head}). The binary {TOPHAM_BIN_VAR} points at was built from a \
             different tree: rebuild it (`cargo build -p topham`) before trusting a green run."
        );
    }
    version
}

/// This checkout's HEAD, abbreviated exactly the way `topham`'s build script
/// abbreviates it, or `None` where there is no checkout to read.
///
/// Truncated from the full object name rather than asked of `git rev-parse
/// --short`, because that flag answers with the shortest *unambiguous* prefix
/// and would disagree with the build script's fixed width the first time a
/// seven-character prefix collided — a mismatch warning about nothing.
fn head_commit() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (commit.len() >= 7).then(|| commit[..7].to_string())
}

/// The ambient `PATH` with `binary`'s own directory in front of it.
///
/// What a `topham` child needs and what no other child in these suites does:
/// the launcher resolves the client from the **bare name** its profile's agent
/// implies, deliberately — an operator with two `claude` binaries has already
/// answered which one they mean, in their `PATH`. So the only way to point a
/// launched client at the binary under test is to make that answer be the
/// rig's, which is what this does. A bare name that resolved to some other
/// build would leave the version banner naming a binary the run never used.
pub fn path_with(binary: &str) -> String {
    let ambient = std::env::var("PATH").unwrap_or_default();
    match Path::new(binary)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => format!("{}:{ambient}", parent.display()),
        None => ambient,
    }
}

/// One of a run's isolated XDG directories, under its own root, created.
///
/// `topham` resolves its profiles from `XDG_CONFIG_HOME` and its per-profile
/// `CODEX_HOME` or scratch from `XDG_DATA_HOME`, so a run that inherited either
/// would read a developer's profiles and write generated files into their real
/// data directory.
pub fn xdg(root: &Path, what: &str) -> PathBuf {
    let path = root.join("xdg").join(what);
    std::fs::create_dir_all(&path).expect("the run's XDG directory");
    path
}

/// Write the launch profile a `topham` child resolves, into `config_home`, and
/// answer with its path.
///
/// **Hand-written TOML rather than `Profile::to_toml`**, and that is the point
/// of the closure tests. `topham`'s own suite proves the round trip — that what
/// `save` writes is what `load` reads — which is a claim about two functions
/// agreeing with each other. What no test in that crate can make is the claim
/// these files need: that the file *an operator types*, from the vocabulary the
/// README documents, resolves into a launch a real client hooks up with. A
/// fixture built by the serializer would agree with a renamed field on both
/// sides and say nothing.
///
/// The absence here is as load-bearing as the presence: **no key**. The turn
/// key reaches the child on its environment, under [`DEFAULT_KEY_ENV`], which
/// is R-T2's whole rule. `deployment_root` is likewise the **root** the profile
/// vocabulary names, with no version or Responses prefix: each generator
/// derives its own, and the absence of a prefix here is what proves it.
pub fn write_profile(
    config_home: &Path,
    agent: &str,
    deployment_root: &str,
    topology: &str,
) -> PathBuf {
    let directory = config_home.join("topham").join("profiles");
    std::fs::create_dir_all(&directory).expect("the run's profiles directory");
    let path = directory.join(format!("{TOPHAM_PROFILE}.toml"));
    std::fs::write(
        &path,
        format!(
            "agent = \"{agent}\"\n\
             deployment-root = \"{deployment_root}\"\n\
             auth = \"roundhouse-key\"\n\
             key-env = \"{DEFAULT_KEY_ENV}\"\n\
             topology = \"{topology}\"\n"
        ),
    )
    .expect("the run's launch profile");
    path
}
