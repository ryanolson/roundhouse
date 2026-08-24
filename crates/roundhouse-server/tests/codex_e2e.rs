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
//! Three tests below are named in the plan's §9 M9 rung, and they are named
//! there rather than invented here because each closes one thing M0–M6 could
//! only document:
//!
//! - `a_real_codex_binary_executes_our_synthetic_tool_call_and_returns_its_output`
//!   — the dispatch assumption. **Deleted by M10.0 T7**, with the ruling above
//!   `a_real_codex_binary_receives_the_correction_as_the_turns_answer`: R1
//!   retires the tool-call channel, so there is no dispatch left to assume;
//! - `a_real_codex_binary_resends_the_call_and_output_and_the_session_does_not_fork`
//!   — the history-buffer resend, which §10 open item 1 records as
//!   unverifiable without `codex-core` and names M9 as the only closure.
//!   **Deleted by T7 as well, and its surviving half re-landed**: the claim was
//!   never really about calls, it was that a real client *extends* its history
//!   rather than rebuilding it, and `the_next_turn_reflects_the_correction` now
//!   asserts that pairwise over the guidance item instead;
//! - `the_next_turn_reflects_the_correction` — that the correction the loop
//!   built is what the agent actually read.
//!
//! **What M10.0 changed about all three.** Outcome B is an assistant message
//! now, not a synthetic tool call, so a steered run *ends* on our guidance and
//! the turn that acts on it is a fourth `codex exec resume`. The suite is
//! therefore one roundhouse turn per run, where it used to fold two into the
//! third; what it proves is unchanged in kind and cheaper in mechanism — the
//! correction reaching a real agent no longer depends on that agent being
//! willing and able to call us back.
//!
//! Green here retires the documented-assumption block that
//! `crates/roundhouse-mcp/src/lib.rs` carried until now, which §9 makes the
//! explicit condition. Two more are preconditions this suite could not assume:
//! that `exec resume --last` continues one roundhouse session, and that our
//! rmcp 3.1.3 service answers codex's rmcp 1.8.0 client at all. The rest were
//! added by the M9 thermo-nuclear review — the forwarded-login stanza (F12) and
//! mid-session revocation (F15), each the first real-binary evidence for a
//! claim only prose had carried, plus two guards that need no binary at all
//! (F02, F11) because what they catch is this harness lying to itself.
//!
//! §10 open item **2** — whether reporting a judge's usage on a steered turn
//! disturbs the client's own bookkeeping — arrived here as *evidence* and left
//! as a ruling. The evidence block (now `M10-USAGE-RULED`, folded into the one
//! test that rules on it) is still printed and never asserted, because what a
//! reader should conclude from four numbers is not a thing a fixture should
//! decide. What review finding F03 settled is narrower
//! and is now asserted, in
//! `a_steered_turns_reported_usage_is_the_context_it_admitted`: the wire number
//! and the ledger number answer different questions, so the wire reports the
//! turn's context contribution while the log keeps booking the judge. The
//! answer was "it disturbs it by 5x", which is why the block gained the ratio
//! that makes the disturbance visible without cross-reading four blocks.
//!
//! # What is real here, and what is scripted
//!
//! Real: the codex binary, the HTTP transport, the MCP handshake and tool
//! listing, the control directory, the minted turn key, the `Validator`, the
//! trigger, the action map, and the composition of the answer a steered turn is
//! served. Scripted: the judge's verdict (what a hosted reviewer would *say* is
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
//! exists to catch. Three 0.146.0-specific facts are load-bearing and would
//! move:
//!
//! - `request_max_retries` / `stream_max_retries` are **provider-scoped** keys
//!   — the top-level spelling is rejected by `--strict-config`, which this
//!   harness passes on purpose so that drift is loud;
//! - a failed turn's stderr carries the *server's own message body*, not just
//!   the status (`Turn error: unexpected status 401 Unauthorized: this key has
//!   been revoked`). `a_key_revoked_between_runs_…` asserts on that text
//!   because absence-of-completion is satisfied by a child that died before
//!   sending anything; a client that logged a bare status would take it red
//!   and the assertion would need re-aiming at `turn.failed`, not deleting;
//!   the claim — the refusal reached the agent — is the same either way;
//! - the turn that fulfils a steer resends the steered turn's conversation
//!   *extended*, not rebuilt: every item that was there before comes back
//!   byte-identical, with the correction and the new user message appended.
//!   `the_next_turn_reflects_the_correction` asserts that pairwise over the
//!   overlap, and it is what keeps the session from forking one turn after
//!   anybody is looking. The M9 spelling of this bullet named the delta exactly
//!   — "the emitted call and its output, nothing interposed" — which a text
//!   steer makes false: the fulfilling turn is a fresh `resume` carrying a user
//!   message this suite does not author. `a_steered_turns_reported_usage_is_the_context_it_admitted`
//!   is still an equality rather than a band because each end is anchored to
//!   the log's own items instead of to a predicted delta.
//!
//! `CODEX_HOME` still lives under `target/` rather than the system temp dir,
//! but — corrected here after stage 4's refute found the original framing
//! overstated (Finding A) — that is precaution, not a measured fact. The
//! arg0-symlink refusal it guards against, if it exists at 0.146.0, sits on
//! the sandboxed-shell-exec path (`codex-linux-sandbox`); this harness's
//! `sandbox_mode = "read-only"` posture, with no `exec_command` ever
//! dispatched, does not reach it. Direct retest confirms this on this box:
//! pointing `CODEX_HOME` and the workdir at `/tmp` produced two full green
//! runs, including the whole steering suite with its usage-evidence
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
    CONTEXT_WINDOW_TOKENS, CodexAuthKind, CodexLaunch, DEFAULT_KEY_ENV, skill_files,
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
/// wrote; and one line, because the string is matched against three different
/// renderings of the same correction — codex's `-o` file, an SSE body where the
/// newlines are JSON-escaped, and the stored item — and only a single-line
/// fragment survives all three unchanged.
///
/// The one-line constraint used to be justified by codex wrapping an MCP result
/// as `"Wall time: …\nOutput:\n[…]"`. That rendering is gone with the tool-call
/// channel (M10.0 R1); the constraint outlived its original reason, which is why
/// it is restated here against the renderings that still exist.
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

/// The refusal a revoked turn key earns, in both of the shapes F15 asserts on.
///
/// Spelled here rather than inline because the two assertions read the same
/// refusal through two different windows — the code off the response document
/// roundhouse served, the message off the child's own stderr — and a literal
/// copied into each would let one drift while the other kept passing. Both come
/// from `control_config::auth`'s `AuthError::RevokedKey`, and the distinction
/// from `unknown_key` is the whole point: a revoked key is one the directory
/// *remembers* refusing.
const REVOKED_KEY_CODE: &str = "revoked_key";
const REVOKED_KEY_MESSAGE: &str = "this key has been revoked";

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
    /// The response body as bytes-turned-text, captured on **every** path.
    ///
    /// It used to be `/mcp` only, on the reasoning that buffering
    /// `/v1/responses` would hold the whole SSE body until the turn ended —
    /// "the one property that surface exists to not have". F11 showed what that
    /// bought and what it cost. Two claims this suite makes live *only* in that
    /// body and were therefore unobservable: a `SteerAction::Halt`'s reason is
    /// committed as the assistant text of the very response that ends the run
    /// (so the injection-boundary sweep in `the_next_turn_reflects_the_correction`
    /// swept a document it could never see, and unlike a `Steer` there is no
    /// next turn to resend it), and `response.completed.usage` — the number
    /// codex folds into `last_token_usage` — is what F03's ruling is about.
    ///
    /// What buffering costs *here*, stated rather than assumed: the child sees
    /// the frames of one turn arrive at once instead of as they are produced.
    /// No assertion in this file is about frame arrival *timing*, every turn is
    /// served by an in-process echo client and finishes in milliseconds, and
    /// `codex exec` parses a complete SSE body identically to an incremental
    /// one. The property genuinely traded away is the harness's fidelity to
    /// backpressure, which nothing here measures; the property bought is two
    /// findings' worth of evidence.
    response_text: Option<String>,
    /// The response body parsed as one JSON document, when it is one.
    ///
    /// `/mcp` answers exactly one document per POST, which is what makes the
    /// handshake assertions readable. `/v1/responses` answers an SSE stream, so
    /// this stays `None` there and [`Exchange::frames`] is the accessor.
    response: Option<Value>,
}

impl Exchange {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// The SSE `data:` payloads of this response, parsed, in arrival order.
    ///
    /// Parsed on demand rather than at capture time because the recorder is a
    /// transport-level thing and SSE framing is a property of one route: a
    /// recorder that pre-parsed frames would have to know which paths stream,
    /// which is exactly the coupling F11's fix was supposed to remove.
    fn frames(&self) -> Vec<Value> {
        self.response_text
            .as_deref()
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
            .collect()
    }

    /// The first SSE frame whose `type` is `kind`.
    fn frame(&self, kind: &str) -> Option<Value> {
        self.frames()
            .into_iter()
            .find(|frame| frame["type"].as_str() == Some(kind))
    }

    /// The `usage` object this response reported on the wire.
    ///
    /// The one the *client* reads: codex folds `response.completed.usage` into
    /// `last_token_usage`, replacing it, and that is what drives its compaction
    /// gate. Since F03 it is deliberately not the same number the log books for
    /// the same turn, so a test asking "what did the client learn" has to read
    /// the wire and a test asking "what did this cost" has to read the log.
    fn wire_usage(&self) -> Option<Value> {
        self.frame("response.completed")
            .map(|frame| frame["response"]["usage"].clone())
    }

    /// The headers as a failure message should print them: credential-bearing
    /// values replaced by their length.
    ///
    /// Under `RoundhouseKey` every captured bearer is a key this test minted
    /// seconds earlier, so printing it whole cost nothing. `ForwardedOpenAiLogin`
    /// (F12) is the first fixture where `Authorization` carries something that
    /// is not ours, and although *this* seat is a hermetic constant compiled
    /// into the file, the shape of the assertion is what a later fixture holding
    /// a real one would copy. Redacting to a length keeps the diagnostic — "the
    /// header arrived, and was this big" — which is the whole reason a failure
    /// message prints headers at all.
    fn redacted_headers(&self) -> BTreeMap<String, String> {
        self.headers
            .iter()
            .map(|(name, value)| {
                let value = match name.as_str() {
                    "authorization" | TURN_KEY_HEADER | "chatgpt-account-id" => {
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

    /// The `/v1/responses` exchange whose stream carried the correction.
    ///
    /// Found by frame content rather than by index into [`Self::to`]: "the
    /// third request" is an assumption about how many requests the client chose
    /// to make, and a client retry silently moves it. What makes a turn the
    /// steered one is what it answered with, so that is what this looks for.
    ///
    /// **It used to look for an emitted `function_call` item by name**
    /// (`emitting_a_call("fetch_steer")`), which is how a steer was
    /// recognizable while outcome B was a synthetic tool call. Since M10.0 R1
    /// the correction is the turn's assistant text, so the discriminator is the
    /// text: [`GUIDANCE_FRAGMENT`] is roundhouse's own opening sentence and no
    /// dispatched turn ever produces it — the echo provider answers [`ANSWER`].
    fn emitting_the_guidance(&self) -> Option<Exchange> {
        self.to("/v1/responses").into_iter().find(|exchange| {
            exchange
                .response_text
                .as_deref()
                .is_some_and(|body| body.contains(GUIDANCE_FRAGMENT))
        })
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
    let (mut response_parts, response_body) = response.into_parts();
    // Every path, since F11: see `Exchange::response_text` for what buffering
    // the streaming one costs and what it bought.
    let bytes = axum::body::to_bytes(response_body, 32 * 1024 * 1024)
        .await
        .expect("a loopback response body is readable");
    // The body just went from streamed to definite-length. Any framing header
    // the streaming response carried would now describe a body that no longer
    // exists, and hyper would serialize the mismatch rather than reconcile it —
    // a corrupt response the child would report as a protocol error, which
    // reads like a roundhouse bug and is not one.
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
            headers,
            body: parsed,
            status,
            response: serde_json::from_slice::<Value>(&bytes).ok(),
            response_text: text,
        });
    Response::from_parts(response_parts, Body::from(bytes))
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
                            // **`text`, and `tool_call` is now a load failure**
                            // (M10.0 T2). It used to say `tool_call` because
                            // `auto` ran a capability probe and a fixture whose
                            // steer depended on detection would have been
                            // testing §7. The probe is gone — every
                            // interjection is text — so `auto` and `text` are
                            // one thing, and the retired spelling would make
                            // `directory.apply` below refuse the project rather
                            // than steer differently. `text` over `auto`
                            // because a fixture should name what it wants.
                            "channel": "text",
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
        // Fallible since F13; the rig's inputs are the documented-correct
        // shape, so a refusal here means the rig built them wrong.
        let mut launch = CodexLaunch::new(base_url.clone(), &catalog_path)
            .expect("the rig's own base_url and catalog path are the correct shape");
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

    /// Write the optional third surface — the generated skills — into this
    /// run's `CODEX_HOME`, and answer with what was written.
    ///
    /// A separate call rather than part of [`Self::start_as`] on purpose: every
    /// other test in this file runs against a client that was handed *no*
    /// skills, and that is not an oversight but the control. Skills are listed
    /// in a developer message at thread start, so writing them for every rig
    /// would add tokens to the prompt of the usage test that measures exactly
    /// that, and would leave nothing in the suite proving the listing came from
    /// these files rather than from something codex ships.
    fn write_generated_skills(&self) -> Vec<roundhouse_server::codex_launch::GeneratedFile> {
        let files = skill_files();
        for file in &files {
            let path = self.root.join("home").join(&file.relative_path);
            std::fs::create_dir_all(path.parent().expect("a skill lives in a directory"))
                .expect("the skill's directory");
            std::fs::write(&path, &file.contents).expect("writing a generated skill");
        }
        files
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
    ///
    /// Two assertions rather than one, because they fail on different evidence.
    /// The first reads the binding: `Conversations::fork` moves `latest` to the
    /// forked id, so a session id that still carries no generation suffix is
    /// this node's own statement that nothing rebound. The second reads the
    /// store, which does not depend on the binding table being right about
    /// itself.
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

    /// The three `codex exec` invocations one steer costs, driven in order.
    ///
    /// Shared by every steering test because the arithmetic is the fixture and
    /// not the subject: turn 1 is below the trigger's turn-index gate, turn 2
    /// validates and escalates (the action map escalates unconditionally on the
    /// first intervention), turn 3 validates and steers. Three tests each
    /// spelling that out would be three places to update when the trigger's
    /// gate moves, and two of them would keep passing while asserting nothing.
    ///
    /// **One run per roundhouse turn since M10.0, which it was not before.** A
    /// tool-call steer cost the client a dispatch and a resend *inside* run 3,
    /// so run 3 was two roundhouse turns and a fourth run would have been turn
    /// 5. A text steer is the turn's answer: run 3 is turn 3 and ends there,
    /// with codex printing our guidance. So the fulfilling turn is a fourth
    /// `resume` — [`Self::resume_after_the_steer`] — and turn 5 is where the
    /// trigger fires again.
    async fn drive_to_a_steer(&self) -> [CodexRun; 3] {
        let first = self.exec("Say the word alpha and stop.").await;
        first.assert_completed("turn 1");
        first.assert_catalog_was_used();
        let second = self.resume("Now say the word beta and stop.").await;
        second.assert_completed("turn 2");
        let third = self.resume("Now say the word gamma and stop.").await;
        third.assert_completed("turn 3, the steered turn");
        [first, second, third]
    }

    /// The turn that reads the correction: roundhouse turn 4, and the one turn
    /// the trigger must refuse to judge.
    ///
    /// Its own function rather than an inline `resume` because two tests need
    /// the same fourth turn and the prompt is part of the fixture: it asks for
    /// something the agent could only produce by carrying on, which is what
    /// separates "the client resent the guidance and continued" from "the
    /// client stopped at the correction".
    async fn resume_after_the_steer(&self) -> CodexRun {
        let fourth = self.resume("Now say the word delta and stop.").await;
        fourth.assert_completed("turn 4, the turn that fulfils the steer");
        fourth
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

/// The generation-zero id behind `session`, whatever generation it is at.
///
/// `conversations::bound_session` spells generation zero as the namespaced key
/// verbatim and every later generation as `{key}#g{n}` — pinned by
/// `conversations::tests::a_reader_and_a_turn_resolve_one_cache_key_to_one_session`
/// — so the suffix *is* the fork, and stripping it recovers the stem. Sound
/// here because the stem is `{project}/{user}/{uuid}` and a UUID carries no
/// `#`: there is no key this can truncate by accident.
fn base_session(session: &SessionId) -> SessionId {
    match session.as_str().split_once("#g") {
        Some((base, _)) => SessionId::new(base),
        None => session.clone(),
    }
}

/// The session id a first fork of `session`'s conversation would have created.
///
/// A free function rather than a method on [`Rig`] so the guard it powers can
/// be tested without a rig, a binary, or a socket — F02 was that the guard's
/// arithmetic was vacuous, and an arithmetic no test can evaluate is exactly
/// how that survived. Derived from [`base_session`] and never from
/// `Conversations::latest`: a fork moves `latest` to the forked id *before*
/// any assertion runs, so appending `#g1` to it asks about `key#g1#g1`, which
/// nothing ever creates and whose absence therefore says nothing.
fn fork_probe(session: &SessionId) -> SessionId {
    SessionId::new(format!("{}#g1", base_session(session)))
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
#[ignore = "needs the real codex binary: --features e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
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

    // The positive half, and the reason the negative one above is not enough: a
    // child that died before sending anything — a bad binary, a bind failure, a
    // deadline kill — also completes no turn, and would satisfy an
    // absence-of-completion check while proving nothing about revocation. This
    // asserts the client *saw* the refusal and attributed it to the turn.
    // Matched on codex's own log line (`codex_core::session::turn: Turn error:
    // unexpected status 401 Unauthorized: …`) plus the refusal's message, which
    // is roundhouse's: a client that logged a bare 401 without our text would
    // mean the tombstone's own explanation never reached the operator watching
    // the agent.
    assert!(
        second.stderr.contains("401") && second.stderr.contains(REVOKED_KEY_MESSAGE),
        "the revoked key's refusal must have reached codex as the turn's own error, but its \
         stderr never named a 401 carrying `{REVOKED_KEY_MESSAGE}`:\n--- stderr\n{}",
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
    // By name, not merely by status, and on *this* surface — which needed F11's
    // capture fix to be assertable at all. Before it, the only body this test
    // could read was the `/mcp` reconnect's, which the refuter's own timestamps
    // show is a consequence of the turn having already ended rather than its
    // cause: the resumed run's outcome hangs on this response. `unknown_key`
    // here would mean the tombstone never took effect on this node and the key
    // was merely unrecognised, which is a different (and much weaker) claim.
    assert_eq!(
        last_turn
            .response
            .as_ref()
            .and_then(|body| body["error"]["code"].as_str()),
        Some(REVOKED_KEY_CODE),
        "the turn surface's refusal must be the tombstone's own code: {:?}",
        last_turn.response_text
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
            Some(REVOKED_KEY_CODE),
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

// **T7 ruling: `codexs_meta_thread_id_rides_every_tools_call_and_is_never_read`
// is retired here, and this comment is what it leaves behind.**
//
// F09 recorded that codex stamps `params._meta.threadId` on **every**
// `tools/call` it dispatches — unconditionally, via
// `with_mcp_tool_call_thread_id_meta` (`codex-rs/core/src/mcp_tool_call.rs` at
// the pin, `6344a65`) — byte-identical to the `prompt_cache_key` the same
// process's `/v1/responses` traffic carries, and that roundhouse reads it
// nowhere: neither `ControlPlaneReads::resolve_session` (resolves from the
// tool's own `conversation` argument or `Conversations::latest`) nor the tools
// in `roundhouse-mcp/src/plane.rs` ever look at it. That was never a defect —
// tenant-scoped `Principal` plus the qualified `conversation` argument isolate
// sessions with no help from `_meta` — and the test was expected to pass; what
// it bought was evidence that a free, client-supplied correlator arrives on
// every call and is discarded.
//
// **It cannot be re-observed hermetically after M10.0.** The only `tools/call`
// a run of this suite ever produced was the client dispatching roundhouse's own
// synthetic steer, and T4 deleted the wire projection that emitted it: this
// deployment's `/v1/responses` stream carries assistant text and nothing else,
// so no turn it serves can ask a model to call a tool, so no `tools/call` is
// dispatched. Re-aiming the assertion at `tools/list` would be a different
// claim about a different helper. The fact is therefore recorded here and in
// `roundhouse-mcp`'s module doc rather than guarded, and the day a
// provider-emitted tool call is relayed through this wire is the day it becomes
// testable again — which is worth noticing, because that day is also when the
// MCP plugin surface becomes reachable from a roundhouse-served turn.

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
    // deferred under `ToolSearchAlwaysDeferMcpTools`. So this is the shape to
    // assert, not a per-tool entry.
    //
    // **What no longer stands behind that sentence.** It used to end "direct
    // dispatch still resolves — which is what the steering test below proves",
    // and the steering test proved it by having roundhouse emit a synthetic
    // `fetch_steer` call the client then dispatched. T4 deleted that path, and
    // with it the only hermetic evidence that a *dispatch* against this
    // namespace resolves; what is proved here is the handshake and the listing.
    // See the T7 ruling above this test for why nothing in this suite can
    // produce a `tools/call` any more.
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

/// M10.1 P7: the generated plugin surface reaches the model, and it reaches it
/// as **skills** rather than as the `prompts/` files the plan named.
///
/// This is the falsifier for the pivot in
/// [`roundhouse_server::codex_launch::skills`]'s module doc. That doc argues
/// from a source read — no `prompts` loader exists at `e363b08` or at the Cargo
/// pin; `$CODEX_HOME/skills` is scanned by `core-skills`, and the listing is
/// rendered from `core` so `codex exec` gets it. A source read is an opinion
/// about a binary. This is the binary.
///
/// Two rigs, because the assertion that matters is a *difference*. The first
/// writes the three generated files and must find them in the turn codex sent
/// us; the second is identical in every other respect and writes none, and must
/// find nothing. Without the second, "the request body contains the string
/// `rh-prefer`" would also pass if codex shipped a skill by that name, or if
/// some other part of the prompt happened to carry it — and it would keep
/// passing after a regression that stopped reading the directory entirely,
/// because a *shipped* skill listing would still be there.
///
/// The catalog's `include_skills_usage_instructions` is asserted on the same
/// pair. It is the field that turns three listed file paths into an instruction
/// to open one, and `false` is what the reader defaults to — so it is exactly
/// the kind of "written, and the writing is the point" decision that is
/// invisible from the generated file alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real codex binary: --features e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
async fn a_real_codex_binary_is_told_about_the_generated_skills() {
    let rig = Rig::start("skills").await;
    let written = rig.write_generated_skills();
    assert_eq!(written.len(), 3, "the shipped surface is three skills");
    let run = rig.exec("Say the word alpha and stop.").await;
    run.assert_completed("the skills run");

    let turn = &rig.recorder.to("/v1/responses")[0];
    let sent =
        serde_json::to_string(turn.body.as_ref().expect("a JSON turn body")).expect("re-encodes");

    // Codex lists a skill as `- {name}: {description} (file: {path})`
    // (`core-skills/src/render.rs:520-532` @ `e363b08`), so both halves have to
    // be there: the name alone could be a path fragment the client echoed, and
    // the description is the line the model actually selects on.
    for file in &written {
        let name = file
            .relative_path
            .trim_start_matches("skills/")
            .trim_end_matches("/SKILL.md");
        assert!(
            sent.contains(name),
            "codex did not tell the model about `{name}`. Either the loader no longer reads \
             $CODEX_HOME/skills, or the file was written somewhere it does not scan:\n{}",
            &sent[..sent.len().min(4000)]
        );
    }
    let a_description = "Use when the user asks to keep this session on this deployment";
    assert!(
        sent.contains(a_description),
        "the skill *names* arrived without their descriptions, which is the half a model \
         chooses on -- the likeliest cause is frontmatter that did not parse as a scalar"
    );
    // The protocol the catalog field turns on. Without
    // `include_skills_usage_instructions` the model is handed the list and
    // never told that reading one is how a skill is used.
    assert!(
        sent.contains("How to use skills"),
        "the catalog sets include_skills_usage_instructions = true, so codex must have \
         appended its own usage protocol to the listing"
    );
    rig.clean();

    // The control: the same deployment, the same client, no skills written.
    let bare = Rig::start("skills-control").await;
    let bare_run = bare.exec("Say the word alpha and stop.").await;
    bare_run.assert_completed("the control run");
    let bare_sent = serde_json::to_string(
        bare.recorder.to("/v1/responses")[0]
            .body
            .as_ref()
            .expect("a JSON turn body"),
    )
    .expect("re-encodes");
    assert!(
        !bare_sent.contains("rh-prefer") && !bare_sent.contains(a_description),
        "a client handed no skills must carry none: the assertions above would otherwise be \
         about something codex ships rather than about what roundhouse generated"
    );
    bare.clean();
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
#[ignore = "needs the real codex binary: --features e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
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
        turn.redacted_headers()
    );
    assert_eq!(
        turn.header(TURN_KEY_HEADER),
        Some(rig.secret.as_str()),
        "and roundhouse's own key must still arrive, in its own header: {:?}",
        turn.redacted_headers()
    );
    assert_ne!(
        turn.header("authorization"),
        Some(format!("Bearer {}", rig.secret).as_str()),
        "the two doors must disagree here -- RoundhouseKey's BYOK stanza cannot tell \
         'preferred' from 'only option present' because both headers there carry the same \
         value (see this test's own module doc)"
    );

    // ---- and the key that arrived is the one that resolved the principal ----
    //
    // Configured mode namespaces every session as `{project}/{user}/{uuid}`, so
    // the id the surface minted *is* the statement of which membership
    // authenticated. The claim is specific to this fixture: the only `rh_*`
    // credential on the request rode `TURN_KEY_HEADER`, and `Authorization`
    // carried a foreign bearer that resolves to no membership at all — so a
    // session named for this tenant can only have come from the dedicated
    // header being read.
    let session = rig.session();
    assert!(
        session.as_str().starts_with(&format!("{PROJECT}/{USER}/")),
        "the dedicated header must have resolved the principal even though Authorization \
         belonged to the client's own upstream, but the session is `{session}`"
    );

    // ---- and the seat itself is in none of our records ---------------------
    //
    // The forwarded credential is the client's, not ours: it exists to be
    // passed upstream, and every place roundhouse *keeps* something is a place
    // it must not appear. Swept rather than reasoned about, because the
    // pass-through stanza is the first shape in this system where a credential
    // roundhouse never minted travels through it — and "we do not store it" is
    // the claim §3 rests on. The `config.toml` is swept too: the generator must
    // name the *mechanism* (a completed login in `CODEX_HOME`), never the seat,
    // and a generator that inlined one would put it on disk world-readable.
    let events = rig
        .store
        .read_events(&session, 0, 1024)
        .await
        .expect("the session exists");
    let log = format!("{events:?}");
    // The control, and the reason the sweep below is not tautological: a
    // rendering that reached no item text at all would report the seat absent
    // for the wrong reason, and pass forever. `ANSWER` is text this deployment
    // committed to the log on this run, so finding it proves the haystack is
    // the one the needle would be in.
    assert!(
        log.contains(ANSWER),
        "control failed: the swept rendering must reach committed item text, or its silence \
         about the seat says nothing"
    );
    assert!(
        !log.contains(SEAT_ACCESS_TOKEN),
        "the forwarded seat must never be captured into the session log: it is the client's \
         credential passing through, and the log is a durable record an operator reads"
    );
    let config = std::fs::read_to_string(rig.root.join("home/config.toml"))
        .expect("the generated config is on disk");
    assert!(
        config.contains("[mcp_servers."),
        "control failed: the file read must be the generated config, or its silence about the \
         seat says nothing"
    );
    assert!(
        !config.contains(SEAT_ACCESS_TOKEN),
        "the generated config must name the login *mechanism*, never a token: a stanza that \
         inlined the seat would write a live credential into a file on disk"
    );

    // ---- §3 evidence: the pass-through stanza, on a real request -----------
    //
    // Printed rather than only asserted, because the plan's §3 describes this
    // stanza in prose and until this test nothing had ever run it against a
    // binary. The bearer is redacted to its length: what a reader needs from
    // this block is that the two doors carried *different* values and which
    // header each took, not the values themselves.
    println!("--- M9-PASSTHROUGH-EVIDENCE ({})", rig.version);
    println!("    stanza          : ForwardedOpenAiLogin (requires_openai_auth, no env_key)");
    println!(
        "    Authorization   : Bearer <{} bytes: the client's own seat, from CODEX_HOME/auth.json>",
        SEAT_ACCESS_TOKEN.len()
    );
    println!(
        "    {TURN_KEY_HEADER}: <{} bytes: roundhouse's minted turn key, via env_http_headers>",
        rig.secret.len()
    );
    println!("    principal       : {session}");
    println!("    seat in session log / generated config: no / no");
    println!("--- end M9-PASSTHROUGH-EVIDENCE");

    rig.clean();
}

/// **T7's deletions, and the ruling that takes them.**
///
/// Two M9 tests used to stand here:
/// `a_real_codex_binary_executes_our_synthetic_tool_call_and_returns_its_output`
/// and `a_real_codex_binary_resends_the_call_and_output_and_the_session_does_not_fork`.
/// Both asserted a dispatch round trip — roundhouse emits a synthetic
/// `fetch_steer` call, a real client dispatches it against `/mcp`, and the
/// tool's output comes back as a `function_call_output` the next turn admits.
/// M10.0 R1 retires that channel: no verdict maps to a tool call, so there is no
/// dispatch to make and no call-and-output pair to resend.
///
/// Deleted rather than re-pointed, because what they proved was that the
/// *emission machinery* survived a real client, and the emission machinery is
/// gone (T4). What survives of their subject is split across the two tests
/// below: the correction still has to reach a real agent, and the client's
/// resend of it still has to admit as our prefix rather than fork the session —
/// which is the M4 claim the second of them was really about, now made of one
/// ordinary assistant item instead of two protocol items.
///
/// What is *not* deleted: `a_real_codex_binary_completes_the_mcp_handshake_against_our_server`
/// and `codexs_meta_thread_id_rides_every_tools_call_and_is_never_read`. R1
/// re-purposes the MCP surface rather than removing it — it is the plugin
/// surface an agent changes roundhouse's behavior through, and `fetch_steer`
/// still answers there as a re-read of the correction in the log.
///
/// ---
///
/// The correction is the turn's answer, and a real client prints it.
///
/// The one thing only a real binary can say about the M10.0 pivot: that codex
/// treats roundhouse's guidance as an ordinary assistant message — surfaces it,
/// writes it to `-o`, and ends the run — rather than as something it has to
/// dispatch, approve or unwrap. Everything in `steering_emission.rs` is played
/// against Codex's own types; this is played against the process.
///
/// Three claims, and the third is the one the pivot bought:
///
/// 1. the agent received roundhouse's own words — both ends of the directive,
///    and the restated request quoted beneath it;
/// 2. the judge's prose did not travel with them (the injection boundary, in
///    the one place a real client could have surfaced a leak);
/// 3. **nothing was dispatched to get any of it**: no tool call in the log, no
///    `tools/call` on the wire, one `codex exec` and no round trip. A tool-call
///    steer needed the client to be willing and able to call us back; a text
///    steer needs the client to be able to read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real codex binary: --features e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
async fn a_real_codex_binary_receives_the_correction_as_the_turns_answer() {
    let rig = Rig::start("steer").await;
    let [_first, _second, third] = rig.drive_to_a_steer().await;

    // ---- what the agent was handed -----------------------------------------
    //
    // `-o` is read with `unwrap_or_default`, so the emptiness check is what
    // keeps a missing file from reading as a passing empty answer.
    assert!(
        !third.last_message.trim().is_empty(),
        "the steered run must end with an agent message — our guidance — but \
         `-o` was empty. stdout:\n{}",
        third.stdout
    );
    // Both ends of the directive. The opening sentence says a review found a
    // problem and the closing one says what to do about it; an assertion on
    // either alone passes against a correction truncated to its other half.
    assert!(
        third.last_message.contains(GUIDANCE_FRAGMENT),
        "the agent must have been handed the diagnosis half of the correction: {:?}",
        third.last_message
    );
    assert!(
        third.last_message.contains(DIRECTIVE_INSTRUCTION),
        "and the instruction half, which is what makes it a correction: {:?}",
        third.last_message
    );
    // The restated request, quoted. This is the M10.0 composition
    // (`render_steer_answer`) surviving a real client's rendering: the agent has
    // to see the task beside the correction, or it has to reconstruct what it
    // was doing from scrollback — which is the thing the correction just told it
    // it is getting wrong.
    assert!(
        third
            .last_message
            .contains("> Now say the word gamma and stop."),
        "the pending request must be restated under the guidance, every line \
         quoted: {:?}",
        third.last_message
    );
    assert!(
        !third.last_message.contains(JUDGE_PROSE),
        "the judge's own prose must never reach the agent — `render_directive` \
         excludes it precisely so a model that read attacker-influenceable \
         transcript cannot write into the answer roundhouse serves: {:?}",
        third.last_message
    );
    // The negative control that makes the three above about *this* turn: the
    // echo provider's answer is what a dispatched turn produces, and a steered
    // turn is not dispatched.
    assert!(
        !third.last_message.contains(ANSWER),
        "a steered turn is answered by roundhouse, not by a provider: {:?}",
        third.last_message
    );

    // ---- and nothing was dispatched to get it -------------------------------
    let items = rig.items().await;
    assert!(
        items.iter().all(|item| !matches!(
            item.content,
            ItemContent::ToolCall { .. } | ItemContent::ToolResult { .. }
        )),
        "M10.0 R1 retires the tool-call channel: a steered session must carry no \
         protocol items at all:\n{items:#?}"
    );
    assert!(
        rig.recorder.rpc("tools/call").is_empty(),
        "the correction must have cost no round trip. Dispatched:\n{}",
        rig.recorder.transcript()
    );

    // ---- the guidance is an ordinary answer, and stamped like one -----------
    let guidance = items
        .iter()
        .find(|item| item.content.render().contains(GUIDANCE_FRAGMENT))
        .expect("the correction is in the conversation");
    assert_eq!(
        guidance.role,
        Role::Assistant,
        "a steer is the turn's answer, so it is the assistant's turn"
    );
    // Emitted by this deployment, so the log carries the response it was
    // committed under. Every item the *client* sent carries none, and that
    // asymmetry is the only thing distinguishing the two on a replay — it
    // survives the pivot because the stamp is on the item, not on its shape.
    assert!(
        guidance.response_id.is_some(),
        "the guidance is completed under a response like any other answer"
    );

    println!("--- M10-STEER-AS-TEXT ({})", rig.version);
    println!("    codex `-o` after the steered run:");
    for line in third.last_message.lines() {
        println!("      | {line}");
    }
    println!("--- end M10-STEER-AS-TEXT");

    rig.assert_never_forked().await;
    rig.clean();
}

/// §10.2, ruled: **the wire and the ledger answer different questions and no
/// longer share one number.**
///
/// F03 found what sharing it cost. Codex folds `response.completed.usage` into
/// `last_token_usage`, *replacing* it rather than summing
/// (`codex-rs/protocol/src/protocol.rs:2122-2125` at `e363b08`), and that value
/// — not the cumulative `total_token_usage` the evidence block prints — is what
/// drives auto-compaction and `get_context_remaining`
/// (`core/src/context_manager/history.rs:415-419`, which despite its name reads
/// `last_token_usage`). A steered turn used to report the judge's usage there,
/// measured at 1147 tokens against a fulfilling turn whose real input was 5729:
/// the client was told its context had collapsed on the very turn before it
/// resent the largest history the session had ever held.
///
/// The ruling moved the *wire* number and left the ledger alone, so this test
/// reads both and asserts they disagree in the intended direction:
///
/// - the **log** still books the judge's usage on the steered turn (the
///   dashboard's total stays equal to the sum of its rows — asserted first,
///   because everything below is only interesting if the ledger did not move);
/// - the **wire** reports the turn's context contribution, and it does so in
///   the same tokenizer over the same rendering that prices the next turn.
///
/// **The equality this pins changed shape with M10.0, and deliberately did not
/// become a tolerance.** It used to read
/// `wire_input(steered) + render(call) + render(result) == log_input(fulfilling)`,
/// which worked because the fulfilling turn happened *inside* the steered run:
/// no human spoke between the two requests, so the delta was exactly the two
/// protocol items. A text steer ends the run, so the fulfilling turn is a fresh
/// `codex exec resume` carrying a new user message whose bytes this test cannot
/// predict — and a two-item delta hardcoded here would now be wrong rather than
/// merely fragile. So each side is anchored to the log instead:
///
/// ```text
/// wire_input(steered)     == Σ render(items before the guidance)
/// log_input(fulfilling)   == Σ render(items before the fulfilling answer)
/// ```
///
/// which is the same claim — one tokenizer over one rendering, on both ends of
/// the gap — and is strictly stronger, because it names *where* a residual came
/// from instead of leaving "what else turn 4 carried" to a comment. A tolerance
/// would still hide the two ways this can be wrong: a tokenizer disagreement and
/// a missing item look identical inside a band.
///
/// The equality is exact because this deployment's tokenizer is
/// [`ByteTokenizer`] (one token per byte) and `ContextAssembler` concatenates
/// renders with no separator, so `render().len()` *is* the token count. On a BPE
/// that merges across an item boundary it would be an equality up to the merge,
/// which is why each sum is taken over the same per-item renders the assembler
/// buffers rather than over a joined string.
#[ignore = "needs the real codex binary: --features e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steered_turns_reported_usage_is_the_context_it_admitted() {
    let rig = Rig::start("usage-evidence").await;
    let [first, second, third] = rig.drive_to_a_steer().await;
    let fourth = rig.resume_after_the_steer().await;

    // ---- the ledger, which the ruling deliberately did not move ------------
    let responses = rig.response_usage().await;
    assert_eq!(
        responses.len(),
        4,
        "turn 1, turn 2, the steered turn (3) and the fulfilling turn (4) — one \
         roundhouse turn per `codex exec` since the correction stopped costing a \
         round trip: {responses:#?}"
    );
    let booked = &responses[2];
    let fulfilling = &responses[3];
    let side_calls = rig.side_call_usage().await;
    assert_eq!(
        side_calls.len(),
        2,
        "turns 2 and 3 each consulted the judge once: {side_calls:#?}"
    );
    assert_eq!(
        *booked, side_calls[1],
        "the steered turn's *booked* usage must still be exactly the judge call \
         that decided it: the ruling changed what the client is told, not what \
         the deployment spent, and a dashboard whose turn row stopped equalling \
         its side-call row would be the worse bug"
    );

    // ---- the wire, which it did -------------------------------------------
    let steered = rig.recorder.emitting_the_guidance().unwrap_or_else(|| {
        panic!(
            "one /v1/responses stream must have carried the correction:\n{}",
            rig.recorder.transcript()
        )
    });
    let wire = steered.wire_usage().unwrap_or_else(|| {
        panic!(
            "the steered response must have completed: {:?}",
            steered.response_text
        )
    });
    let wire_input = wire["input_tokens"]
        .as_u64()
        .expect("`input_tokens` is what codex folds into last_token_usage");
    assert_ne!(
        wire["total_tokens"].as_u64(),
        Some(booked.total()),
        "the wire must no longer report the judge's number — that equality *is* F03: it is \
         what told a real client its context had collapsed. Wire usage: {wire}"
    );
    assert_eq!(
        wire["input_tokens_details"]["cached_tokens"].as_u64(),
        Some(0),
        "nothing was dispatched, so nothing was served from a prefix cache; a cached count \
         here would understate what the next turn has to prefill"
    );

    // ---- and both ends are one tokenizer over one rendering ----------------
    //
    // `render().len()`, not a second tokenizer: `ByteTokenizer::encode` is
    // `text.as_bytes()`, so this is `Engine::admitted_input_tokens`'s own
    // arithmetic spelled without reaching into the engine for it.
    let items = rig.items().await;
    let rendered_through = |end: usize| -> u64 {
        items[..end]
            .iter()
            .map(|item| item.render().len() as u64)
            .sum()
    };
    let guidance_at = items
        .iter()
        .position(|item| item.content.render().contains(GUIDANCE_FRAGMENT))
        .expect("the correction is in the conversation");
    let answer_at = items
        .iter()
        .rposition(|item| item.content.render().contains(ANSWER))
        .expect("the fulfilling turn was served by the provider");
    assert!(
        answer_at > guidance_at,
        "the fulfilling turn's answer comes after the correction it answers:\n{items:#?}"
    );

    // Recorded, not asserted — the §10.2 evidence block the deleted tool-call
    // test carried, kept because the one arithmetic a reader wants to do (how
    // close did the client think it was to compacting?) needs the window the
    // client was accumulating toward, and that window is this catalog's. What
    // relationship *should* hold between the judge's usage, the usage roundhouse
    // reports for a steered turn, and the client's own accumulation is the
    // question the assertions below answer for two of the three; an assertion on
    // the client's accumulation would be this file deciding the third.
    println!("--- M10-USAGE-RULED ({})", rig.version);
    println!("    catalog context_window    : {CONTEXT_WINDOW_TOKENS}");
    for (label, run) in [
        ("run 1", &first),
        ("run 2", &second),
        ("run 3 (steered)", &third),
        ("run 4 (fulfilling)", &fourth),
    ] {
        println!("    client {label} turn.completed.usage: {:?}", run.usage());
    }
    println!("    wire input (steered turn) : {wire_input}");
    println!(
        "    Σ renders before guidance : {}",
        rendered_through(guidance_at)
    );
    println!(
        "    log input (fulfilling)    : {}",
        fulfilling.input_tokens
    );
    println!(
        "    Σ renders before answer   : {}",
        rendered_through(answer_at)
    );
    println!(
        "    the gap (guidance + turn 4's input): {}",
        rendered_through(answer_at) - rendered_through(guidance_at)
    );
    println!(
        "    ratio wire:next           : {:.2}x (was 5.0x when the wire carried the judge's)",
        fulfilling.input_tokens as f64 / wire_input.max(1) as f64
    );
    println!("    booked (judge) on turn 3  : {booked:?}");
    println!(
        "    fourth run's answer       : {:?}",
        fourth.last_message.trim()
    );
    println!("--- end M10-USAGE-RULED");

    assert_eq!(
        wire_input,
        rendered_through(guidance_at),
        "the number the client was told on the steered turn must be the context \
         this deployment admitted for it — every item committed before the \
         correction, and nothing else. Residual {} tokens names what the wire \
         counted that the log does not hold",
        wire_input as i64 - rendered_through(guidance_at) as i64
    );
    assert_eq!(
        fulfilling.input_tokens,
        rendered_through(answer_at),
        "and the number the fulfilling turn was priced on must be every item \
         committed before its answer — the correction included, which is the \
         whole of prefix admission. Residual {} tokens names an item neither \
         `rig.items()` nor this arithmetic accounted for, and is worth reading \
         rather than absorbing into a tolerance",
        fulfilling.input_tokens as i64 - rendered_through(answer_at) as i64
    );

    rig.clean();
}

/// The correction reaches the agent, the turn that acts on it is not judged
/// again, and the session does not fork on the way.
///
/// **The other half of the pivot, and the half only a fourth run can show.** A
/// text steer ends the run it interrupts, so "the agent acted on the correction"
/// is not observable inside it: the client resends its history on the *next*
/// `codex exec resume`, and the two things that must then be true are the two
/// things a tool-call steer used to prove with a dispatch —
///
/// 1. **the guidance admits as prefix.** It is an ordinary assistant item, so
///    the client resends it with the rest of the conversation and roundhouse has
///    to recognize it as the prefix it is. A fork is silent from the client's
///    side, which is why [`Rig::assert_never_forked`] asks the store rather than
///    the client;
/// 2. **the fulfilling turn is not validated.** Judging it would judge the agent
///    on a turn whose whole content is the correction being obeyed, while the
///    previous verdict is still the freshest evidence — a loop that interrupts
///    its own repair. `validate_loop.rs`'s
///    `the_turn_after_a_steer_is_not_validated_and_the_one_after_that_is` proves
///    the rule at the engine level; here it is proved across a real client's
///    resend, which is the only place the *shape* of what came back could have
///    broken it.
///
/// The injection sweep stays and is the reason this test reads every captured
/// document rather than just the answer: after the pivot the correction rides
/// the `/v1/responses` body itself, which is exactly the capture F11 had to fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real codex binary: --features e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH"]
async fn the_next_turn_reflects_the_correction() {
    let rig = Rig::start("correction").await;
    let [_first, _second, third] = rig.drive_to_a_steer().await;
    let fourth = rig.resume_after_the_steer().await;

    // ---- the correction, in the agent's context, one turn later ------------
    //
    // Read off the *log* and not off run 3's `-o`: what matters here is that the
    // client brought it back and roundhouse admitted it as its own, which is a
    // claim about the stored conversation. `a_real_codex_binary_receives_the_
    // correction_as_the_turns_answer` is where the `-o` half is asserted.
    let items = rig.items().await;
    let guidance = items
        .iter()
        .find(|item| item.content.render().contains(DIRECTIVE_INSTRUCTION))
        .unwrap_or_else(|| panic!("the correction must be in the conversation:\n{items:#?}"));
    assert_eq!(guidance.role, Role::Assistant);
    assert_eq!(
        items
            .iter()
            .filter(|item| item.content.render().contains(DIRECTIVE_INSTRUCTION))
            .count(),
        1,
        "the resent correction must be recognized as the prefix it is, not \
         appended a second time:\n{items:#?}"
    );

    // ---- the resend extends rather than rebuilds ---------------------------
    //
    // The surviving half of the deleted `a_real_codex_binary_resends_the_call_
    // and_output_and_the_session_does_not_fork`, which is the half that was
    // never about tool calls: a client that *rebuilds* its history rather than
    // extending it forks this session on the turn after the one anybody is
    // watching, and the fork is silent from the client's side. What changed is
    // only what the delta contains — one assistant item and a new user message
    // instead of a call and its output — so the assertion is on the overlap and
    // not on the delta's length, which a fresh `resume` cannot predict.
    //
    // Pairwise on the overlap rather than a whole-array compare, so a failure
    // names the item that moved instead of printing two 40 KB documents. Whole
    // `Value` equality per element is deliberate and not the "assert on parsed
    // values" rule being broken: the rule is about not byte-comparing
    // *serializations*, and two items pulled from two parsed documents compare
    // as values whatever order their fields arrived in.
    let turns = rig.recorder.to("/v1/responses");
    let steered_at = turns
        .iter()
        .position(|exchange| {
            exchange
                .response_text
                .as_deref()
                .is_some_and(|body| body.contains(GUIDANCE_FRAGMENT))
        })
        .expect("one request was answered with the correction");
    let before = input_of(&turns[steered_at]);
    let after = input_of(&turns[steered_at + 1]);
    assert!(
        after.len() > before.len(),
        "the fulfilling turn resends what it had and adds to it; an input that \
         shrank or stood still is a client that rebuilt its history"
    );
    for (index, (was, now)) in before.iter().zip(after.iter()).enumerate() {
        assert_eq!(
            was, now,
            "item {index} was rewritten between the steered turn and the one \
             that fulfils it; a client that rebuilds its history rather than \
             extending it forks this session one turn after anybody is looking"
        );
    }
    assert!(
        after[before.len()..]
            .iter()
            .any(|item| item.to_string().contains(DIRECTIVE_INSTRUCTION)),
        "and the correction is *in* what it appended — the guidance is an \
         ordinary assistant item, so the client carries it back like any other:\n{:#?}",
        &after[before.len()..]
    );

    // ---- and the judge's prose nowhere at all ------------------------------
    //
    // Every captured document, request and response alike. Both halves matter
    // and they matter differently now: the correction rides the `/v1/responses`
    // response body (it *is* the steered turn's answer), and it comes back in
    // the next request's input — so a leak would be visible twice, and a sweep
    // that read only one half would be checking the client's honesty rather than
    // ours.
    //
    // The response half sweeps the raw captured text and not the parsed
    // `Exchange::response`, since F11: `/v1/responses` answers SSE, which does
    // not parse as one document, and a `SteerAction::Halt`'s reason lands *only*
    // there. `the_injection_sweep_can_see_a_halts_reason_in_the_v1_responses_body`
    // is what keeps that capture honest.
    for exchange in rig.recorder.all() {
        for (half, document) in [
            ("request", exchange.body.as_ref().map(Value::to_string)),
            ("response", exchange.response_text.clone()),
        ] {
            let Some(rendered) = document else { continue };
            assert!(
                !rendered.contains(JUDGE_PROSE),
                "the judge's own prose must never leave the log: found it in the \
                 {half} of {} {}. `render_directive` excludes it precisely so a \
                 model that read attacker-influenceable transcript cannot write \
                 into the answer roundhouse serves.",
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
        "turns 2 and 3 validate; turn 1 is below the trigger's turn-index gate \
         and turn 4 fulfils the steer, which the trigger refuses to judge"
    );
    assert_eq!(
        rig.judge.asked(),
        2,
        "one judge consultation per validation, and no more"
    );

    // ---- the fulfilling turn ran, and ran as an ordinary turn ---------------
    //
    // The steered run ends on our guidance; this one ends on the provider's
    // answer, and the difference between the two `-o` files is the whole of
    // "the agent carried on".
    assert!(
        third.last_message.contains(GUIDANCE_FRAGMENT),
        "run 3 ends on the correction: {:?}",
        third.last_message
    );
    assert!(
        fourth.last_message.contains(ANSWER),
        "run 4 ends on the answer roundhouse served, which is the evidence the \
         client resumed rather than stopping at the correction: {:?}",
        fourth.last_message
    );
    rig.assert_never_forked().await;

    rig.clean();
}

/// F11's guard: the injection-boundary sweep in
/// `the_next_turn_reflects_the_correction` reads "every captured document,
/// request and response alike" and asserts `JUDGE_PROSE` is absent from both
/// halves of every exchange. That was true of what was captured — but `record`
/// used to parse a response body for [`MCP_MOUNT_PATH`] only, so a
/// `SteerAction::Halt`'s reason — committed as the assistant text of the very
/// `/v1/responses` response that ends the run — was never captured at all, and
/// unlike a `Steer`, there is no next turn to resend it one turn late. The
/// sweep was therefore accurate about its inputs and blind to the one path a
/// Halt's prose can take.
///
/// **Live, not gated**, and that is the point: it needs no codex binary,
/// because the defect was in `record`'s capture boundary rather than in
/// anything a real client does. It drives the actual middleware — the same
/// function `Rig::start_as` layers over the merged app — against a synthetic
/// `/v1/responses` route answering with `JUDGE_PROSE`, standing in for a
/// Halt's rendered directive.
///
/// The assertion reads the raw captured text rather than the parsed
/// [`Exchange::response`]: the claim is "the body was captured at all", and a
/// route that answered a bare string would otherwise satisfy the guard by
/// failing to parse rather than by being seen.
#[tokio::test]
async fn the_injection_sweep_can_see_a_halts_reason_in_the_v1_responses_body() {
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
    // `/v1/responses` is captured, so the sweep *can* catch a Halt's leaked
    // reason. Mutating `record` back to `if path == MCP_MOUNT_PATH` takes this
    // line red, which is what makes the sweep's claim load-bearing again.
    assert!(
        exchange
            .response_text
            .as_deref()
            .is_some_and(|body| body.contains(JUDGE_PROSE)),
        "record() must capture a /v1/responses response body containing the \
         judge's prose so `the_next_turn_reflects_the_correction`'s sweep can \
         see it, but the captured exchange's body was {:?}. This is exactly the \
         gap F11 names: a Halt's reason lands only in this body, on the one path \
         `record` used not to parse.",
        exchange.response_text
    );
}

/// F02's guard: the fork probe `Rig::assert_never_forked` builds must be
/// derived from something a fork does **not** move.
///
/// Live and binary-free for the same reason the F11 guard above is: the defect
/// was fixture arithmetic, not client behaviour. It drives the rig's own
/// [`fork_probe`] — the exact function the rig calls, not a copy of it, which
/// is what stops this guard from drifting away from the assertion it guards —
/// against a real `Conversations` fork.
///
/// The original guard read `SessionId::new(format!("{}#g1", self.session()))`.
/// `Rig::session()` is `Conversations::latest(principal)`, and
/// `Conversations::fork` writes the *new* id into `latest` before returning it,
/// so after a real fork the guard probed `key#g1#g1` — an id nothing in the
/// system ever constructs — found it absent, and called that clean. It could
/// not fail for the reason it named.
#[tokio::test]
async fn the_fork_probe_names_the_session_a_fork_would_have_created() {
    let principal = Principal::new(PROJECT, USER);
    let key = format!("{PROJECT}/{USER}/main");
    let conversations = Conversations::new();
    let store = MemoryStore::new();

    let zero = conversations.bind(&principal, &key);
    store
        .create_session(&zero, "policy")
        .await
        .expect("generation zero is fresh");
    assert_eq!(
        fork_probe(&zero).as_str(),
        format!("{key}#g1"),
        "before any fork the probe must already name the id a fork would create"
    );

    // The fork `responses_api` performs when a client's resend disagrees with
    // the log. `latest` now answers `key#g1`, which is what made the old
    // arithmetic vacuous.
    let forked = conversations.fork(&principal, &key);
    store
        .create_session(&forked, "policy")
        .await
        .expect("the fork's session is newly created");
    let after = conversations
        .latest(&principal)
        .expect("a bound principal has a latest session");
    assert_eq!(after, forked, "control: the fork moved `latest`");
    assert_eq!(
        fork_probe(&after),
        forked,
        "the probe must name the fork that happened — deriving it by appending \
         `#g1` to an already-forked id asks about `{key}#g1#g1`, which nothing \
         ever creates, so the store's `Err` says nothing about forking"
    );
    assert!(
        store.last_seq(&fork_probe(&after)).await.is_ok(),
        "and the store must then hold it, which is the ground truth \
         `assert_never_forked` turns into a failure"
    );
}
