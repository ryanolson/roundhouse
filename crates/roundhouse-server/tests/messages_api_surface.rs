// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Anthropic Messages surface, end to end and against the real client's
//! bodies.
//!
//! Three things are being proved here and they need different instruments.
//!
//! **That the stream is conformant** — asserted by the tier-1 oracle in
//! [`common::anthropic`], a strict reader written from the pinned spec whose
//! polarity is the opposite of the shipped types. The Responses surface can
//! borrow Codex's own parser for this; there is no equivalent for this dialect
//! (both official SDKs are deliberately non-validating, and the strict community
//! crates reject correct 2026 traffic), so the oracle is roundhouse-built and
//! every stream this suite produces goes through it. What it catches that an
//! eyeball cannot: a frame whose `event:` line and payload `type` disagree, a
//! `stop_reason` outside the spec's seven, an invented `usage` property, and the
//! ordering mistakes Claude Code's accumulator *throws* on rather than skips.
//!
//! **That a session survives a resend** — asserted against the store, because
//! the failure is silent: a prefix check that forks on every second turn still
//! answers every turn, and only the log shows the conversation being appended
//! twice.
//!
//! **That it is the real client's shape** — asserted against
//! `tests/fixtures/claude-2.1.2{51,57}-*.json`, request bodies captured from the
//! native binaries through a loopback mock (isolated `CLAUDE_CONFIG_DIR`,
//! cleared environment, fake API key, `ANTHROPIC_BASE_URL` pointed at the mock).
//! Only `metadata.user_id`'s `device_id` is edited, to a placeholder of the same
//! shape; everything else is verbatim, tools and 9 KB system prompt included.
//! That sentence is a claim about bytes and is checked as one, in
//! `review_m11_2b_f12.rs` — the redaction that first stamped the placeholder in
//! parsed and re-dumped the whole `user_id` string, silently rewriting the
//! client's separators, and nothing here could see it because every reader of
//! that field parses it too.
//! Two of those bytes falsified a ruling made from reading alone — see
//! `the_shipping_clients_two_turns_are_one_conversation_but_for_the_prompt_it_changed`.
//!
//! **Two lines are pinned, not one, and every fixture-driven test runs against
//! both** — see [`CapturedLine`]. 2.1.251 is the prior line and 2.1.257 the
//! current one; the current line appends a trailing remaining-budget notice a
//! `--continue` rewrites per request, which is the one shape difference between
//! them and the reason R-A exists.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, StatusCode};
use futures::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::routing::{AffinityPolicy, CacheModel, ProviderPricing};
use roundhouse_core::store::{Lease, MemoryStore, SessionStore, StoreError};
use roundhouse_core::validate::{BriefConfig, Objective, ValidationBrief};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierChunk, FrontierClient, FrontierError, FrontierModelSpec,
    FrontierQuote, FrontierStream, LocalFleet, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::claude_launch::ROUNDHOUSE_API_KEY_SENTINEL;
use roundhouse_server::messages_api::wire::{
    CreateMessageParams, canonicalize, is_budget_notice, session_key,
};
use roundhouse_server::{
    ControlPlane, ControlPlaneConfig, Conversations, EchoLocalExecutor, Engine, messages_router,
};

mod common;
use common::anthropic::{
    Accumulated, AccumulatedCall, StrictBlock, StrictErrorKind, StrictEvent, StrictStopReason,
    audit, split_frames,
};
use common::{
    MINUTE, Scripted, ScriptedFrontierClient, ToolCallingFrontierClient, config, embedded_fleet,
    frontier_catalog, key, sha256_hex,
};

/// What the echo provider answers with, and therefore what a turn says.
const ANSWER: &str = "frontier answer";

/// F2: what [`PartialThenFailClient`]'s first call streams before it dies.
const PARTIAL: &str = "the first half of the answer, ";
/// F2: what every later call streams to completion — deliberately unrelated
/// text, so a byte match with [`PARTIAL`] cannot happen by coincidence.
const CONTINUATION: &str = "and only the second half.";

/// One captured client line, and the facts that are properties *of the capture*
/// rather than of the surface.
///
/// **Both lines stay pinned, and every fixture-driven test runs against both.**
/// The prior line is what a deployment's older seats are still on; the current
/// line is what a fresh install gets. A suite that pinned only one of them would
/// answer "does the surface still serve the client it was written against" or
/// "does it serve the client shipping today" — never both, which is the only
/// question a mixed fleet asks.
///
/// The version and the session id are fields rather than literals inside the
/// tests because they come *from* the capture: a test that spelled one would
/// pass for one line and fail for the other with a `SessionNotFound` that looks
/// like a serve-surface refusal and is not (§5.7's second confound). The tool
/// count is not a field at all — it is read off each fixture's own toolbox,
/// because the two rigs declared different toolboxes (24 vs. 21, §5.7's first
/// confound) and an expected literal there would read client drift into a
/// difference between two invocations.
struct CapturedLine {
    /// The client version the attribution pseudo-header names.
    version: &'static str,
    /// The session this capture's own `metadata.user_id` names.
    session: &'static str,
    turn_one: &'static str,
    turn_two: &'static str,
    /// The recorded header set of this line's two requests, as a JSON list of
    /// `{path, headers}` — `x-api-key` redacted at capture time.
    headers: &'static str,
}

impl CapturedLine {
    /// The session id this line's turns actually resolve to on this surface.
    fn named(&self) -> String {
        named(self.session)
    }
}

/// The prior line: 2.1.251, captured 2026-08-29 (§5.6).
static LINE_PRIOR: CapturedLine = CapturedLine {
    version: "2.1.251",
    session: "e13acbde-ab70-46ff-b094-fd8ce95d286d",
    turn_one: include_str!("fixtures/claude-2.1.251-turn-1.json"),
    turn_two: include_str!("fixtures/claude-2.1.251-turn-2-continue.json"),
    headers: include_str!("fixtures/claude-2.1.251-headers.json"),
};

/// The current line: 2.1.257, captured 2026-09-01 (§5.7).
static LINE_CURRENT: CapturedLine = CapturedLine {
    version: "2.1.257",
    session: "c0cb70b6-938b-4cbb-a8e8-1b8a60b7c4d8",
    turn_one: include_str!("fixtures/claude-2.1.257-turn-1.json"),
    turn_two: include_str!("fixtures/claude-2.1.257-turn-2-continue.json"),
    headers: include_str!("fixtures/claude-2.1.257-headers.json"),
};

/// Turn one `fn body(&CapturedLine)` into one `#[test]` per pinned line.
///
/// `per_line_tests!(fn foo)` emits `foo::line_2_1_251` and `foo::line_2_1_257`,
/// each calling `foo` with its own line; `per_line_tests!(async foo)` does the
/// same for a `#[tokio::test]`. (The module shares the body's name — modules
/// and functions live in different namespaces — so the reported path reads as
/// the test and the line it ran on, and no name is spelled twice.)
///
/// **Why generation and not a `for line in LINES` loop, which is what this
/// replaced (M11.2b review, F6).** A loop makes the two lines one test, and
/// that costs three things the fix buys back. A panic on the prior line unwinds
/// the whole test, so the current line's own checks never run in the run that
/// needed them — the shipping client's result is hidden behind the older one's
/// failure, which is exactly backwards. No filter can select one line, so
/// bisecting a client-drift failure means editing the suite. And because a
/// failure message otherwise cannot say which line produced it, every assertion
/// in the loop had to carry a hand-threaded `line.version` prefix — twenty-eight
/// of them, each one a chance to forget. The test name carries it now.
macro_rules! per_line_tests {
    (async $body:ident) => {
        mod $body {
            #[tokio::test]
            async fn line_2_1_251() {
                super::$body(&super::LINE_PRIOR).await;
            }
            #[tokio::test]
            async fn line_2_1_257() {
                super::$body(&super::LINE_CURRENT).await;
            }
        }
    };
    (fn $body:ident) => {
        mod $body {
            #[test]
            fn line_2_1_251() {
                super::$body(&super::LINE_PRIOR);
            }
            #[test]
            fn line_2_1_257() {
                super::$body(&super::LINE_CURRENT);
            }
        }
    };
}

/// The current line's *third* turn, captured by resuming the very session
/// [`LINE_CURRENT`]'s two turns built (same isolated `HOME`, same
/// `CLAUDE_CONFIG_DIR`, same cwd, same day), so its resent history is turn two's
/// bytes and not a reconstruction of them.
///
/// It exists because turn three is the first turn on which the client's
/// remaining-budget notice appears *twice* — once flattened in the history and
/// once fresh at the end — and that is the shape R-A rules on (§5.7.1). There is
/// no 2.1.251 counterpart because the prior line sends no notice at all.
const TURN_THREE_CURRENT: &str = include_str!("fixtures/claude-2.1.257-turn-3-continue.json");

// ---------------------------------------------------------------------------
// The service under test
// ---------------------------------------------------------------------------

fn engine(store: Arc<MemoryStore>) -> Arc<Engine<MemoryStore, ByteTokenizer>> {
    Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(EchoFrontierClient::new(ANSWER)),
        Arc::new(AffinityPolicy::new()),
        config(),
    ))
}

/// A router over a fresh in-memory store, plus that store for direct probing.
///
/// One store and not two: the surface reads a session's stored items to compute
/// the resent-prefix delta, so a router holding its own store would recompute
/// every conversation from empty and every test here would pass while the
/// property under test was gone.
fn surface() -> (Router, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    (
        messages_router(
            ControlPlane::open(),
            engine(Arc::clone(&store)),
            Arc::clone(&store),
            Arc::new(Conversations::new()),
        ),
        store,
    )
}

/// As [`engine`], but dispatching through [`ScriptedFrontierClient`] rather
/// than the plain echo, so a test can recover what the engine actually asked
/// the frontier for — not just what the turn answered with. Same catalog, same
/// config, same policy: only the client's type changes, so a test built on
/// this must route exactly as the ordinary `surface()` tests do.
fn engine_scripted(
    store: Arc<MemoryStore>,
    client: Arc<ScriptedFrontierClient>,
) -> Arc<Engine<MemoryStore, ByteTokenizer>> {
    Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        client,
        Arc::new(AffinityPolicy::new()),
        config(),
    ))
}

/// As [`surface`], plus the [`ScriptedFrontierClient`] handle for reading back
/// `quotes_seen()` after a request.
fn surface_scripted() -> (Router, Arc<MemoryStore>, Arc<ScriptedFrontierClient>) {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(ScriptedFrontierClient::new(ANSWER));
    (
        messages_router(
            ControlPlane::open(),
            engine_scripted(Arc::clone(&store), Arc::clone(&client)),
            Arc::clone(&store),
            Arc::new(Conversations::new()),
        ),
        store,
        client,
    )
}

/// F1: a catalog whose one entry speaks the Responses dialect — everything
/// else copied from [`frontier_catalog`], so the only variable a test built on
/// this changes is `wire_protocol`.
///
/// The point is what this is dispatched *against*: this suite's surface is
/// `/v1/messages`, which parses Anthropic-shaped `tools`
/// (`{name, description, input_schema}`) and threads them verbatim onto
/// whatever the router picks (`ClientDeclarations` in `engine.rs`, no dialect
/// read anywhere on that path). A deployment whose catalog names an
/// `openai_responses` provider — `examples/catalog.example.json` ships four —
/// makes this exact pairing reachable from a real client, not a hand-built one.
fn cross_dialect_catalog() -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![FrontierModelSpec {
        provider: "openrouter".into(),
        model: "cross-dialect-flagship".into(),
        wire_protocol: WireProtocol::OpenAiResponses,
        cache_model: CacheModel::Deterministic { ttl_ms: 5 * MINUTE },
        pricing: ProviderPricing {
            input_per_mtok_usd: 3.0,
            cached_input_per_mtok_usd: 0.3,
            cache_write_per_mtok_usd: 3.75,
            output_per_mtok_usd: 15.0,
        },
        quality_prior: 0.95,
        base_ttft_ms: 350.0,
        ttft_ms_per_uncached_token: 0.002,
    }])
}

/// As [`engine_scripted`], but over [`cross_dialect_catalog`] rather than
/// [`frontier_catalog`] — the only difference from an ordinary scripted engine
/// is which dialect the sole candidate is quoted as speaking.
fn engine_scripted_cross_dialect(
    store: Arc<MemoryStore>,
    client: Arc<ScriptedFrontierClient>,
) -> Arc<Engine<MemoryStore, ByteTokenizer>> {
    Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        cross_dialect_catalog(),
        client,
        Arc::new(AffinityPolicy::new()),
        config(),
    ))
}

/// As [`surface_scripted`], but wired to [`engine_scripted_cross_dialect`] —
/// the Anthropic Messages surface in front of a catalog that resolves to a
/// Responses-dialect target.
fn surface_scripted_cross_dialect() -> (Router, Arc<MemoryStore>, Arc<ScriptedFrontierClient>) {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(ScriptedFrontierClient::new(ANSWER));
    (
        messages_router(
            ControlPlane::open(),
            engine_scripted_cross_dialect(Arc::clone(&store), Arc::clone(&client)),
            Arc::clone(&store),
            Arc::new(Conversations::new()),
        ),
        store,
        client,
    )
}

/// F2: an engine with a live local option, so a turn can actually land there.
///
/// Every other rig in this file wires `fleet: None` (`Engine::new`'s default),
/// which makes `plan`'s `local_quote` unconditionally `None` and every request
/// in this suite a frontier dispatch regardless of what it asks for — fine for
/// the dialect and tool-shape questions this file otherwise answers, wrong for
/// a claim about what a client loses *on the local path specifically*.
/// [`common::embedded_fleet`] plus [`frontier_catalog`] is the same pairing
/// `turn_lifecycle.rs` (`a_turn_longer_than_the_lease_ttl_still_commits`) and
/// `tier_selection.rs` (`rig_with_fleet`) already prove routes local by
/// default: one free registered worker beside one $3/$15-per-Mtok frontier
/// candidate, so local wins on price with nothing else in play.
async fn engine_scripted_with_fleet(
    store: Arc<MemoryStore>,
    client: Arc<ScriptedFrontierClient>,
) -> Arc<Engine<MemoryStore, ByteTokenizer>> {
    Arc::new(
        Engine::new(
            Arc::clone(&store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local answer")),
            frontier_catalog(),
            client,
            Arc::new(AffinityPolicy::new()),
            config(),
        )
        .with_fleet(embedded_fleet().await as Arc<dyn LocalFleet>),
    )
}

/// As [`surface_scripted`], but wired to [`engine_scripted_with_fleet`] — same
/// catalog, same config, same policy, plus the one difference the F2 finding is
/// about: a local candidate that is actually reachable and, on price alone, the
/// one `plan` prefers.
async fn surface_scripted_with_fleet() -> (Router, Arc<MemoryStore>, Arc<ScriptedFrontierClient>) {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(ScriptedFrontierClient::new(ANSWER));
    (
        messages_router(
            ControlPlane::open(),
            engine_scripted_with_fleet(Arc::clone(&store), Arc::clone(&client)).await,
            Arc::clone(&store),
            Arc::new(Conversations::new()),
        ),
        store,
        client,
    )
}

/// As [`surface`], but dispatching through a frontier that calls tools.
///
/// The engine is otherwise identical — same catalog, same config, same policy —
/// so a turn served here routes exactly as every other test's does and the only
/// difference is what comes back off the stream.
fn surface_calling(
    script: Vec<Scripted>,
    stop_reason: Option<&str>,
) -> (Router, Arc<MemoryStore>, Arc<ToolCallingFrontierClient>) {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(ToolCallingFrontierClient::new(script, stop_reason));
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::clone(&client) as Arc<dyn roundhouse_fleet::FrontierClient>,
        Arc::new(AffinityPolicy::new()),
        config(),
    ));
    (
        messages_router(
            ControlPlane::open(),
            engine,
            Arc::clone(&store),
            Arc::new(Conversations::new()),
        ),
        store,
        client,
    )
}

/// The turn a model that speaks and then calls two tools produces.
///
/// Interleaved rather than "text then calls", because the interleaving is the
/// part a projection can get wrong while still producing the right text and the
/// right number of blocks.
fn speaking_and_calling() -> Vec<Scripted> {
    vec![
        Scripted::Text("Let me look."),
        Scripted::Call {
            id: "toolu_01",
            name: "Grep",
            // Keys deliberately not in sorted order and spaced the way a model
            // emits them: if anything on the path parsed and re-serialized this,
            // the bytes would change and the client's resend would stop matching
            // what the log holds.
            arguments: r#"{"pattern": "fn main", "path": "/src"}"#.into(),
        },
        Scripted::Text(" And also:"),
        Scripted::Call {
            id: "toolu_02",
            name: "Read",
            arguments: r#"{"path": "/src/main.rs"}"#.into(),
        },
    ]
}

/// The assistant message a client rebuilds from a stream, as it resends it.
///
/// Built from the oracle's own accumulated blocks rather than written out by
/// hand, so the second turn's request really is the first turn's answer — a
/// hand-written resend would assert prefix admission against a history no client
/// would ever send.
fn resent_assistant(blocks: &[StrictBlock]) -> Value {
    json!({ "role": "assistant", "content": blocks })
}

/// F2's provider double: the first call streams some text and then dies
/// mid-answer (the shape that commits a partial and reports
/// `overloaded_error`); every later call streams a distinct, independent reply
/// to completion. Every `quote.prompt` handed to the client is recorded, so a
/// test can see exactly what context a retried generation was built from —
/// not just what it answered with.
struct PartialThenFailClient {
    calls: AtomicUsize,
    prompts_seen: Mutex<Vec<String>>,
}

impl PartialThenFailClient {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            prompts_seen: Mutex::new(Vec::new()),
        }
    }

    fn prompts_seen(&self) -> Vec<String> {
        self.prompts_seen
            .lock()
            .expect("the recording mutex is never held across a panic in this harness")
            .clone()
    }
}

#[async_trait]
impl FrontierClient for PartialThenFailClient {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.prompts_seen
            .lock()
            .expect("the recording mutex is never held across a panic in this harness")
            .push(quote.prompt.clone());
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(futures::stream::iter([
                Ok(FrontierChunk::OutputText(PARTIAL.to_string())),
                Err(FrontierError::Upstream(
                    "provider exploded mid-answer".into(),
                )),
            ])
            .boxed());
        }
        Ok(futures::stream::iter([
            Ok(FrontierChunk::OutputText(CONTINUATION.to_string())),
            Ok(FrontierChunk::Done {
                input_tokens: quote.prompt.len() as u64,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: CONTINUATION.len() as u64,
                reasoning_tokens: 0,
                provider_reported_cost: None,
                // A scripted stream, so no provider named a reason.
                stop_reason: None,
            }),
        ])
        .boxed())
    }
}

/// As [`surface`], but over [`PartialThenFailClient`].
fn surface_partial_then_fail() -> (Router, Arc<MemoryStore>, Arc<PartialThenFailClient>) {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(PartialThenFailClient::new());
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::clone(&client) as Arc<dyn FrontierClient>,
        Arc::new(AffinityPolicy::new()),
        config(),
    ));
    (
        messages_router(
            ControlPlane::open(),
            engine,
            Arc::clone(&store),
            Arc::new(Conversations::new()),
        ),
        store,
        client,
    )
}

// ---------------------------------------------------------------------------
// F3: a durable tool call whose response never terminates (review of
// b8e8ddd)
// ---------------------------------------------------------------------------

/// F3's provider double: the first call streams one tool call and nothing
/// else -- so `dispatch`'s `trailing` is `None` and `Session::complete`'s
/// batch holds only the terminal event, never an item alongside it. Every
/// later call answers in plain text, so a retry that lands on a fresh
/// dispatch (see [`DropsFirstTerminalWrite`]: nothing ever marks the first
/// response complete, so `begin_turn`'s dedup lookup misses and a fresh
/// dispatch is exactly what happens) produces an ordinary, comparable reply
/// rather than the same tool call a second time.
const F3_RETRY_REPLY: &str = "a clean answer once the retry landed";

struct ToolCallThenTextClient {
    calls: AtomicUsize,
}

impl ToolCallThenTextClient {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl FrontierClient for ToolCallThenTextClient {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(futures::stream::iter([
                Ok(FrontierChunk::ToolCall {
                    id: "toolu_01".to_string(),
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"/tmp/x"}"#.to_string(),
                }),
                Ok(FrontierChunk::Done {
                    input_tokens: quote.prompt.len() as u64,
                    cached_input_tokens: 0,
                    cache_write_tokens: 0,
                    output_tokens: 8,
                    reasoning_tokens: 0,
                    provider_reported_cost: None,
                    stop_reason: Some("tool_use".to_string()),
                }),
            ])
            .boxed());
        }
        Ok(futures::stream::iter([
            Ok(FrontierChunk::OutputText(F3_RETRY_REPLY.to_string())),
            Ok(FrontierChunk::Done {
                input_tokens: quote.prompt.len() as u64,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: F3_RETRY_REPLY.len() as u64,
                reasoning_tokens: 0,
                provider_reported_cost: None,
                stop_reason: Some("end_turn".to_string()),
            }),
        ])
        .boxed())
    }
}

/// F3's store double: drops exactly one terminal-event append -- the shape
/// the finding names, "a lease renewal misses its window, or redis blips for
/// the seconds the turn is mid-flight." Every other append succeeds
/// normally, including the one `append_emitted` makes for the tool call
/// immediately *before* the dropped one on the same lease -- so the durable
/// orphan item and the missing terminal are the only two facts this fixture
/// manufactures, nothing else about the turn is disturbed, and every later
/// turn's own terminal write succeeds.
struct DropsFirstTerminalWrite {
    inner: MemoryStore,
    terminal_writes_seen: AtomicUsize,
}

impl DropsFirstTerminalWrite {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
            terminal_writes_seen: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl SessionStore for DropsFirstTerminalWrite {
    async fn create_session(
        &self,
        session_id: &SessionId,
        model_policy: &str,
    ) -> Result<bool, StoreError> {
        self.inner.create_session(session_id, model_policy).await
    }

    async fn acquire_lease(
        &self,
        session_id: &SessionId,
        node_id: &str,
        ttl_ms: u64,
    ) -> Result<Option<Lease>, StoreError> {
        self.inner.acquire_lease(session_id, node_id, ttl_ms).await
    }

    async fn renew_lease(&self, lease: &Lease, ttl_ms: u64) -> Result<Option<Lease>, StoreError> {
        self.inner.renew_lease(lease, ttl_ms).await
    }

    async fn release_lease(&self, lease: &Lease) -> Result<(), StoreError> {
        self.inner.release_lease(lease).await
    }

    // Delegated like every other read, and it has to be: the trait's default
    // answers "cannot prove this session is idle", which would leave the
    // orphaned tool call unsupersedable for a reason that has nothing to do
    // with what this double models. What it *does* model is one dropped
    // terminal write; the lease is the real `MemoryStore`'s throughout, and by
    // the time the retry arrives the failed turn has released it.
    async fn is_leased(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        self.inner.is_leased(session_id).await
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let has_terminal = kinds.iter().any(|kind| {
            matches!(
                kind,
                SessionEventKind::ResponseCompleted { .. }
                    | SessionEventKind::ResponseIncomplete { .. }
            )
        });
        if has_terminal && self.terminal_writes_seen.fetch_add(1, Ordering::SeqCst) == 0 {
            // The one commit this fixture ever drops: `Session::complete`'s
            // batch for the response that already durably committed its tool
            // call via `append_emitted` -- a different, already-succeeded
            // call on this same lease.
            return Err(StoreError::LeaseLost {
                session_id: lease.session_id.clone(),
                node_id: lease.node_id.clone(),
            });
        }
        self.inner.append_events(lease, kinds).await
    }

    async fn read_events(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        self.inner.read_events(session_id, after_seq, limit).await
    }

    async fn last_seq(&self, session_id: &SessionId) -> Result<u64, StoreError> {
        self.inner.last_seq(session_id).await
    }
}

/// As [`surface`], but over [`ToolCallThenTextClient`] and
/// [`DropsFirstTerminalWrite`] -- F3's rig.
fn surface_orphaned_tool_call() -> (
    Router,
    Arc<DropsFirstTerminalWrite>,
    Arc<ToolCallThenTextClient>,
) {
    let store = Arc::new(DropsFirstTerminalWrite::new());
    let client = Arc::new(ToolCallThenTextClient::new());
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::clone(&client) as Arc<dyn FrontierClient>,
        Arc::new(AffinityPolicy::new()),
        config(),
    ));
    (
        messages_router(
            ControlPlane::open(),
            engine,
            Arc::clone(&store),
            Arc::new(Conversations::new()),
        ),
        store,
        client,
    )
}

// ---------------------------------------------------------------------------
// Driving one request
// ---------------------------------------------------------------------------

/// A minimal streaming create, the way the client shapes one.
fn body(text: &str) -> Value {
    json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [{ "role": "user", "content": text }],
    })
}

/// `POST` a body with a caller-chosen header set, over the router as a service.
async fn post(
    app: &Router,
    uri: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(body).expect("a JSON body")))
                .expect("a well-formed request"),
        )
        .await
        .expect("the router answers");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a readable body")
        .to_bytes();
    (
        status,
        headers,
        String::from_utf8(bytes.to_vec()).expect("bodies are UTF-8"),
    )
}

/// One streaming turn, put through the strict oracle.
///
/// The oracle rather than a frame-name list, because the frame names are the
/// part a wrong implementation gets right: what it gets wrong is a payload the
/// client cannot parse, and a name-only assertion is green for both.
async fn stream(app: &Router, headers: &[(&str, &str)], body: &Value) -> Accumulated {
    let (status, _, text) = post(app, "/v1/messages", headers, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    audit(&text).unwrap_or_else(|error| panic!("the stream is not conformant: {error}\n\n{text}"))
}

/// The session's committed items, read straight out of the store.
async fn stored_items(store: &MemoryStore, session_id: &str) -> Vec<Item> {
    store
        .read_events(&SessionId::new(session_id), 0, 4096)
        .await
        .expect("the session exists")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ItemAppended { item } => Some(item),
            _ => None,
        })
        .collect()
}

/// The session id a client-chosen name actually resolves to on this surface.
///
/// Every name this dialect derives lives in its own namespace (M11.1 review,
/// F6): a Messages session id and a Responses `prompt_cache_key` that read the
/// same string are not the same conversation, and putting them in one is a
/// contested log that forks on every alternating turn. A test that spelled the
/// bare header value would be asserting about a session nothing ever writes to
/// — which passes for the wrong reason.
fn named(session: &str) -> String {
    format!("anthropic_messages/{session}")
}

/// Whether the store has never heard of this session.
///
/// The honest spelling of "did not fork": `stored_items` on an absent session
/// panics rather than answering empty, and an empty answer would in any case be
/// indistinguishable from a session that exists and holds nothing.
async fn no_such_session(store: &MemoryStore, session_id: &str) -> bool {
    store.last_seq(&SessionId::new(session_id)).await.is_err()
}

fn parse(fixture: &str) -> CreateMessageParams {
    serde_json::from_str(fixture).expect("a captured body is a well-formed request")
}

/// A captured body as JSON, for the tests that edit one before serving it.
fn fixture(text: &str) -> Value {
    serde_json::from_str(text).expect("the fixture is JSON")
}

/// The toolbox a capture declares, with the one check that keeps a fixture
/// refresh from quietly turning these tests into assertions about a smaller
/// request.
///
/// **The count is read, never expected.** The two rigs declared 24 and 21 tools
/// — a difference in what a plain `-p` invocation offers, not client drift
/// (§5.7's first confound) — so a literal here would have failed the current
/// line for a reason that has nothing to do with the surface. What is asserted
/// instead is the property the tests below actually rest on: that this really is
/// a live capture's toolbox and not a hand-written stub.
fn declared_tools(captured: &Value) -> Value {
    let tools = captured["tools"].clone();
    let declared = tools
        .as_array()
        .expect("every capture declares a toolbox")
        .len();
    assert!(
        declared >= 20,
        "the fixture is the live capture; a toolbox of {declared} is \
         describing a different request"
    );
    tools
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// **`?beta=true` is ignored, and it is proved rather than assumed.**
///
/// Claude Code posts to `/v1/messages?beta=true` on every inference request
/// (confirmed again at 2.1.251). Axum routes on the path, so the query should be
/// invisible — but "should be" is exactly the assumption that, if wrong, 404s
/// every request the shipping client makes and does so only in production.
/// Asserted on both routes and with a second, invented query parameter, because
/// a router matching on the full path-and-query would pass a one-parameter test
/// written against the parameter it was built for.
#[tokio::test]
async fn the_beta_query_the_client_appends_reaches_the_same_route() {
    let (app, _store) = surface();

    let plain = stream(&app, &[], &body("hello")).await;
    let (status, _, with_beta) = post(&app, "/v1/messages?beta=true", &[], &body("hello")).await;
    assert_eq!(status, StatusCode::OK);
    let with_beta = audit(&with_beta).expect("the query must not change the stream");
    assert_eq!(plain.text, with_beta.text);

    let (status, _, _) = post(
        &app,
        "/v1/messages?beta=true&something_from_the_next_release=1",
        &[],
        &body("hello"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a query parameter this build has never seen must not change the route"
    );

    let (status, _, _) = post(
        &app,
        "/v1/messages/count_tokens?beta=true",
        &[],
        &body("hello"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// `/v1/models` is not served, and the refusal is the router's own.
///
/// Plan R5 defers discovery: exposing the catalog would put roundhouse's routing
/// choices into the user's `/model` picker. Asserted so that "deferred" is a
/// fact about the build rather than a note in a plan — and asserted as a 4xx of
/// any shape, because which one axum picks for an unrouted path is its business
/// and not this surface's contract.
#[tokio::test]
async fn model_discovery_is_not_served() {
    let (app, _store) = surface();
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/models")
                .body(Body::empty())
                .expect("a well-formed request"),
        )
        .await
        .expect("the router answers");
    assert!(
        response.status().is_client_error(),
        "the catalog must not be discoverable: {}",
        response.status()
    );
}

// ---------------------------------------------------------------------------
// One turn
// ---------------------------------------------------------------------------

/// A whole turn, judged by the strict oracle rather than by our own reading.
#[tokio::test]
async fn a_dispatched_turn_is_a_conformant_stream() {
    let (app, _store) = surface();

    let accumulated = stream(
        &app,
        &[("x-claude-code-session-id", "sess-one-turn")],
        &body("hello"),
    )
    .await;

    assert_eq!(accumulated.text, ANSWER);
    assert_eq!(accumulated.model, "claude-opus-5", "the client's own name");
    assert!(!accumulated.message_id.is_empty());
    assert_eq!(accumulated.completed_blocks, 1);
    assert_eq!(accumulated.error, None);

    // The usage the *client* computes after its own merge, not the numbers we
    // put in the frames. The three input axes are disjoint on this wire, so a
    // prompt counted once is the property; counting it under both
    // `input_tokens` and `cache_read_input_tokens` would double the total here
    // and nowhere else.
    assert!(
        accumulated.usage.total_input() > 0,
        "a turn that carried a prompt must not report a free one: {:?}",
        accumulated.usage
    );
    assert!(
        accumulated.usage.output_tokens > 0,
        "the terminal frame must carry the real output count: {:?}",
        accumulated.usage
    );
}

/// The one-token probes Claude Code opens a session with are served genuinely.
///
/// Its auth probe and its quota probe are both `stream`-less creates with
/// `max_tokens: 1` (§3.6). A surface that 4xx'd or 500'd them would fail before
/// the first turn — and the failure would look like a broken deployment rather
/// than an unimplemented mode.
#[tokio::test]
async fn the_clients_non_streaming_probe_gets_a_whole_message() {
    let (app, _store) = surface();
    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[],
        &json!({
            "model": "claude-opus-5",
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "test" }],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{text}");
    let message: Value = serde_json::from_str(&text).expect("a JSON message");
    assert_eq!(message["type"], "message");
    assert_eq!(message["role"], "assistant");
    assert_eq!(message["model"], "claude-opus-5");
    assert_eq!(message["stop_reason"], "end_turn");
    assert_eq!(
        message["content"],
        json!([{ "type": "text", "text": ANSWER }]),
        "the non-streaming body must carry the same answer the stream does"
    );
    assert!(
        message["usage"]["output_tokens"].as_u64().unwrap_or(0) > 0,
        "a turn answered without streaming still costs output: {message}"
    );
}

/// A provider double whose terminal `Done` reports realistic, non-zero cache
/// figures — every count distinct from the others, and distinct in
/// particular from the prelude's own estimate, whose cache fields are always
/// zero at quote time (`messages_api.rs`'s `admitted` is built as
/// `Usage { input_tokens: admitted_input_tokens, ..Default::default() }`,
/// before any provider has answered). That distinctness is what the test
/// below needs: a bug that left the prelude's zero cache counts standing
/// would still show *some* non-zero usage, and only a comparison against
/// numbers the prelude could not have guessed proves the terminal figure
/// actually reached the client.
struct CacheReportingFrontierClient;

#[async_trait]
impl FrontierClient for CacheReportingFrontierClient {
    async fn execute(&self, _quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        Ok(futures::stream::iter([
            Ok(FrontierChunk::OutputText("noted.".to_string())),
            Ok(FrontierChunk::Done {
                input_tokens: 12_345,
                cached_input_tokens: 9_000,
                cache_write_tokens: 500,
                output_tokens: 7,
                reasoning_tokens: 0,
                provider_reported_cost: None,
                stop_reason: Some("end_turn".to_string()),
            }),
        ])
        .boxed())
    }
}

/// **Residue (M11.2a fix round).** `emit::message_body` was dead production
/// code — nothing outside its own tests ever called it — whose doc claimed to
/// be "the complete `Message` for a finished turn, for a `stream: false`
/// request", but the actual non-streaming assembly is `messages_api.rs`'s
/// `complete_message`/`BlockAccumulator`, which never called it either.
/// Deleted, and its "two projections of one turn cannot disagree" assertion
/// moved here, onto the path that is actually live.
///
/// **What the dead test was masking: the usage line.** `message_body` took a
/// `roundhouse_core::event::Usage` and ran it through `wire_usage` itself, so
/// its old test only ever proved that one conversion function converts
/// correctly — already covered by `wire_usage`'s own unit tests in
/// `messages_api/emit.rs`. `complete_message` does something narrower and
/// easier to get wrong: it takes the terminal `message_delta`'s usage
/// *wholesale* when one arrives, replacing the prelude's estimate rather than
/// merging with it field by field (see the `MessageDelta` arm's own comment
/// in `complete_message`). A bug that merged instead of replacing — or that
/// left the prelude's estimate standing whenever the terminal restated a
/// smaller or differently-shaped cache figure — would still satisfy "usage
/// came back non-zero"; it would not satisfy "the non-streaming body's usage
/// is the identical object the streaming terminal reported", which is the
/// live path's actual rule and what this test checks, against
/// [`CacheReportingFrontierClient`]'s deliberately prelude-distinguishing
/// figures.
#[tokio::test]
async fn the_non_streaming_bodys_usage_is_the_same_object_the_streaming_terminal_reported() {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(CacheReportingFrontierClient) as Arc<dyn FrontierClient>,
        Arc::new(AffinityPolicy::new()),
        config(),
    ));
    let app = messages_router(
        ControlPlane::open(),
        engine,
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );

    // --- Streaming half: read the terminal message_delta's own usage object,
    // straight off the wire, the way the client itself would. ---------------
    let mut stream_request = body("hi");
    stream_request["stream"] = json!(true);
    let (status, _, stream_text) = post(
        &app,
        "/v1/messages",
        &[("x-claude-code-session-id", "sess-usage-stream")],
        &stream_request,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{stream_text}");
    let terminal_usage = split_frames(&stream_text)
        .into_iter()
        .filter_map(|frame| {
            let (name, data) = (frame.name?, frame.data?);
            let event: StrictEvent = serde_json::from_str(&data).ok()?;
            (event.wire_name() == name).then_some(event)
        })
        .find_map(|event| match event {
            StrictEvent::MessageDelta {
                usage: Some(usage), ..
            } => Some(usage),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!("the terminal message_delta must carry a usage object: {stream_text}")
        });

    // Premise: the double's cache figures actually reached the wire, and are
    // not the (always-zero) prelude estimate -- otherwise the comparison
    // below would hold even for a `complete_message` that never looked past
    // the prelude at all.
    assert_eq!(
        terminal_usage.cache_read_input_tokens,
        Some(9_000),
        "premise: the terminal frame must carry the double's real cache-read \
         figure, not the prelude's zero: {stream_text}"
    );
    assert_eq!(
        terminal_usage.cache_creation_input_tokens,
        Some(500),
        "premise: same, for the cache-write figure: {stream_text}"
    );

    // --- Non-streaming half: the live `complete_message` path ---------------
    let mut complete_request = body("hi");
    complete_request["stream"] = json!(false);
    let (status, _, complete_text) = post(
        &app,
        "/v1/messages",
        &[("x-claude-code-session-id", "sess-usage-complete")],
        &complete_request,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{complete_text}");
    let message: Value = serde_json::from_str(&complete_text).expect("a JSON message");

    // THE CLAIM: the non-streaming body's usage is the *same object* the
    // streaming terminal reported for the identically-scripted turn -- not a
    // re-derivation, not the prelude's estimate, not a field-by-field merge
    // of the two.
    assert_eq!(
        message["usage"],
        json!({
            "input_tokens": terminal_usage.input_tokens,
            "cache_creation_input_tokens": terminal_usage.cache_creation_input_tokens,
            "cache_read_input_tokens": terminal_usage.cache_read_input_tokens,
            "output_tokens": terminal_usage.output_tokens,
        }),
        "the live complete_message path's usage must be the streaming \
         terminal's own usage object, wholesale: streaming reported \
         {terminal_usage:?}, non-streaming answered {}",
        message["usage"]
    );
}

/// **F1 (M11.1 thermo-nuclear review), fixed and pinned.** The claim was that
/// `CreateMessageParams::max_tokens` (`wire.rs`) is read and never used anywhere
/// in `messages_api` — so the ceiling sent upstream was always
/// `EngineConfig::expected_output_tokens` (256 by default, and `main.rs` never
/// overrides it), and every real answer truncated at roughly a paragraph.
/// Ruled **valid**, and fixed by splitting the two meanings apart:
/// `FrontierQuote::output_token_cap` is the client's declared ceiling and
/// `expected_output_tokens` stays the router's pricing estimate.
///
/// PROBE: two otherwise-identical requests whose only difference is the
/// client's declared `max_tokens` — one asking for a single token, the other
/// for a million. Both halves of the split are asserted, and the second is not
/// decoration: a "fix" that wrote the client's ceiling into
/// `expected_output_tokens` would satisfy the first assertion while inflating
/// every quote, every spend reservation and every projected saving by three
/// orders of magnitude on a turn that answers in forty tokens.
///
/// The pipeline is asserted end to end rather than at the wire, because the
/// finding was about the *seam*: `AnthropicMessagesClient::body`'s own unit
/// tests already pin what a cap becomes on the wire, and what nothing pinned
/// was that a client's number reaches the quote at all.
#[tokio::test]
async fn f1_the_clients_max_tokens_is_the_dispatch_ceiling_and_not_the_estimate() {
    let (app, _store, client) = surface_scripted();

    let mut low = body("hello");
    low["max_tokens"] = json!(1);
    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[("x-claude-code-session-id", "sess-f1-low")],
        &low,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let mut high = body("hello");
    high["max_tokens"] = json!(1_000_000);
    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[("x-claude-code-session-id", "sess-f1-high")],
        &high,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let quotes = client.quotes_seen();
    assert_eq!(
        quotes.len(),
        2,
        "one frontier dispatch per session: {quotes:?}"
    );
    assert_eq!(
        (quotes[0].output_token_cap, quotes[1].output_token_cap),
        (Some(1), Some(1_000_000)),
        "F1: the ceiling each dispatch carried is the one its client declared, \
         verbatim — a `max_tokens: 1` auth probe (client surface §3.6) and a \
         64 000-token coding turn are not the same request: {quotes:?}"
    );
    assert_eq!(
        quotes[0].expected_output_tokens, quotes[1].expected_output_tokens,
        "F1, the other half: the *estimate* is the router's and must not move \
         with what a client declared — a quote priced at a million tokens \
         reserves a million tokens of budget for a turn that answers in forty"
    );
}

/// **The client's tools reach the dispatch, byte-for-byte** — the seam that
/// makes an agentic turn possible at all.
///
/// The re-scoping finding for M11.2: `CreateMessageParams` did not name `tools`,
/// so the twenty-four definitions in every Claude Code request were accepted,
/// ignored, and never sent upstream. The model was then asked a coding question
/// with no toolbox, could only answer in prose, and the client's loop stalled on
/// the first turn that needed a tool. Nothing was red; the deployment simply
/// could not do the thing it exists to do.
///
/// PROBE: the **captured 2.1.251 body**, not a hand-written one. Twenty-four
/// real definitions with `$schema` keys, nested `anyOf`s, `additionalProperties`
/// flags and description prose — the exact material a typed re-encoding would
/// quietly thin out. The assertion is equality with the fixture's own value, so
/// any normalisation at all fails it.
///
/// Asserted at the quote rather than at the wire because the finding was about
/// the *seam*: `AnthropicMessagesClient::body`'s unit tests already pin what a
/// quote's tools become on the wire, and what nothing pinned was that a client's
/// tools reach the quote.
async fn the_clients_tool_definitions_reach_the_dispatch_verbatim(line: &CapturedLine) {
    let (app, _store, client) = surface_scripted();
    let captured = fixture(line.turn_one);
    let tools = declared_tools(&captured);

    let mut request = captured.clone();
    // The capture streams; the scripted client answers either way, and a
    // non-streaming turn keeps this test to one assertion about one thing.
    request["stream"] = json!(false);
    // The captured body carries no `tool_choice` (the client relies on the
    // default), so one is added here: the field is independently optional
    // and a surface that threaded only `tools` would pass every assertion
    // above.
    request["tool_choice"] = json!({ "type": "auto", "disable_parallel_tool_use": false });

    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[("x-claude-code-session-id", "sess-tools")],
        &request,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let quotes = client.quotes_seen();
    assert_eq!(quotes.len(), 1, "one frontier dispatch: {quotes:?}");
    assert_eq!(
        quotes[0].tools.as_ref(),
        Some(&tools),
        "the dispatch must carry every one of the client's own \
         definitions, unmodified — a model told about a smaller toolbox \
         than the client has fails in the one way nobody debugs"
    );
    assert_eq!(
        quotes[0].tool_choice,
        Some(json!({ "type": "auto", "disable_parallel_tool_use": false }))
    );

    // CONTROL: a request that declares neither carries neither, so the
    // assertions above are about threading rather than about a default that
    // would have matched anything. `body()` is the minimal request this
    // suite uses everywhere else, which is also what makes every other test
    // here a control for the same thing.
    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[("x-claude-code-session-id", "sess-no-tools")],
        &body("hello"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let quotes = client.quotes_seen();
    assert_eq!(quotes.len(), 2, "{quotes:?}");
    assert_eq!(quotes[1].tools, None);
    assert_eq!(quotes[1].tool_choice, None);
}
per_line_tests!(async the_clients_tool_definitions_reach_the_dispatch_verbatim);

/// The control for the ignored F1 test below: [`surface_scripted_cross_dialect`]
/// really does resolve an ordinary tool-declaring turn against a target that
/// speaks the Responses dialect, and the fixture this suite reuses really is
/// Anthropic-shaped. Neither fact is the contested behavior -- this is what
/// proves the ignored test's failure is about a missing dialect-shape gate,
/// and not about this harness failing to reach a cross-dialect route at all.
///
/// [`surface_scripted_cross_dialect`] is wired exactly like [`surface_scripted`]
/// above — same engine, same policy, same config — except its one catalog
/// entry is declared `WireProtocol::OpenAiResponses` rather than
/// `AnthropicMessages`. That is a legitimate deployment shape and not a rigged
/// one: `examples/catalog.example.json` ships four `openai_responses` entries
/// beside one `anthropic_messages` entry, `main.rs` builds one
/// `StaticFrontierCatalog` from every entry in a manifest like it (~config.rs
/// `catalog()`), and its boot gate sanctions exactly this shape, in its own
/// refusal text, as the fix for a provider that would otherwise speak two
/// dialects itself ("define the provider twice under two names"). `plan` has
/// nowhere else to send this turn, so with one candidate in the catalog it
/// resolves there regardless of what the client declared -- there is no
/// candidate-side dialect field for a policy to filter on either (`Candidate`
/// carries only `target`, timing and price).
async fn cross_dialect_routing_reaches_a_responses_target_with_anthropic_shaped_tools(
    line: &CapturedLine,
) {
    let captured = fixture(line.turn_one);
    let tools = declared_tools(&captured);
    let first_tool = &tools[0];
    assert!(
        first_tool.get("type").is_none(),
        "the fixture must actually be Anthropic-shaped for the ignored \
         test below to mean anything -- a `type` key on a tool here would \
         make it already Responses-shaped, which is not the scenario"
    );
    assert!(
        first_tool.get("input_schema").is_some() && first_tool.get("parameters").is_none(),
        "same as above, for the schema key the two dialects spell \
         differently -- Anthropic's `input_schema` vs. the Responses \
         upstream's required `parameters`"
    );

    let (app, _store, client) = surface_scripted_cross_dialect();
    let mut request = captured.clone();
    request["stream"] = json!(false);
    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[("x-claude-code-session-id", "sess-cross-dialect-control")],
        &request,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "today (before F1 is fixed) this turn is accepted and dispatched \
         rather than refused -- see the ignored test below: {text}"
    );
    let quotes = client.quotes_seen();
    assert_eq!(quotes.len(), 1, "one frontier dispatch: {quotes:?}");
    assert_eq!(
        quotes[0].wire_protocol,
        WireProtocol::OpenAiResponses,
        "the only entry in this catalog speaks Responses -- if the quote \
         says otherwise this harness is not exercising the cross-dialect \
         path it claims to"
    );
}
per_line_tests!(async cross_dialect_routing_reaches_a_responses_target_with_anthropic_shaped_tools);

/// **F1 (thermo-nuclear review of b8e8ddd), fixed: a toolbox and the target it
/// is dispatched to may speak different dialects, and now something says so.**
///
/// Same request, same cross-dialect harness as the control above -- see its doc
/// comment for why the scenario is real and reachable, not hand-rigged. What the
/// control does not check, and what this test asserts must hold, is that the
/// mismatch is *represented* and *reconciled* rather than passed through: before
/// the fix, `plan` picked a candidate by price with no dialect read anywhere on
/// that path, `connect` spliced the client's raw JSON onto the quote regardless
/// of `spec.wire_protocol`, and `OpenAiResponsesClient::body` put whatever
/// `tools` held under the wire body's `"tools"` key unexamined -- so a real
/// dispatch POSTed an Anthropic-shaped array (no `type`, `input_schema` not
/// `parameters`) to a Responses upstream, which 400s every tool-using turn, and
/// `plan`'s failover landed on the next candidate in the same dialect and 400'd
/// identically.
///
/// **Two assertions, because the fix has two halves and either alone is still
/// the defect.** The quote now carries `tools_dialect` — the dialect of the
/// *surface that accepted the toolbox*, which is a different fact from
/// `wire_protocol`, the dialect of the *target the turn resolved to* — and
/// `FrontierQuote::tools_for` is the seam every dispatch client reads a toolbox
/// through, restating the plain function-tool core when the two differ and
/// refusing before a socket what it cannot restate.
///
/// The scripted double this harness dispatches through does not serialize, so
/// this test asserts the *seam*; the two clients' own suites assert that their
/// bodies go through it
/// (`openai_responses.rs`'s
/// `an_anthropic_shaped_toolbox_is_restated_in_this_dialect_before_it_is_sent`
/// and the Messages client's mirror of it), and `frontier.rs` pins the
/// translation table and every refusal.
async fn f1_cross_dialect_tools_must_not_reach_dispatch_unexamined(line: &CapturedLine) {
    let captured = fixture(line.turn_one);
    let tools = declared_tools(&captured);
    let declared = tools.as_array().map(Vec::len).unwrap_or(0);

    let (app, _store, client) = surface_scripted_cross_dialect();
    let mut request = captured.clone();
    request["stream"] = json!(false);
    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[("x-claude-code-session-id", "sess-cross-dialect-guard")],
        &request,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let quotes = client.quotes_seen();
    assert_eq!(quotes.len(), 1, "one frontier dispatch: {quotes:?}");
    let quote = &quotes[0];

    // The first half: the two dialects are now two fields, and they disagree --
    // which is the fact nothing in this system could represent before.
    assert_eq!(
        quote.wire_protocol,
        WireProtocol::OpenAiResponses,
        "the only entry in this catalog speaks Responses"
    );
    assert_eq!(
        quote.tools_dialect,
        Some(WireProtocol::AnthropicMessages),
        "and the surface that accepted these {declared} declarations speaks \
         Messages -- a quote that could not say so is a quote whose tools no \
         client can shape correctly"
    );

    // The second half: the seam every dispatch client reads a toolbox through
    // restates them, rather than handing back the client's raw bytes.
    let (shaped, _) = quote
        .tools_for(quote.wire_protocol)
        .expect("plain function tools restate faithfully");
    let shaped = shaped.expect("the client declared tools");
    assert_ne!(
        shaped, tools,
        "F1: a Responses-dialect target received Anthropic-shaped tools verbatim"
    );
    let entries = shaped.as_array().expect("an array of tools");
    assert_eq!(
        entries.len(),
        declared,
        "and not one of the client's tools was dropped on the way -- a thinned          toolbox is the failure mode a translation must never take"
    );
    for entry in entries {
        assert_eq!(
            entry["type"],
            json!("function"),
            "the Responses wire tags every tool: {entry}"
        );
        assert!(
            entry.get("parameters").is_some() && entry.get("input_schema").is_none(),
            "and spells the schema `parameters`: {entry}"
        );
    }
}
per_line_tests!(async f1_cross_dialect_tools_must_not_reach_dispatch_unexamined);

/// The control for the ignored F2 test below: [`surface_scripted_with_fleet`]
/// really does route an ordinary, tool-free turn to the local worker by
/// default -- the same "local is the cheap default" pairing
/// `turn_lifecycle.rs` (`a_turn_longer_than_the_lease_ttl_still_commits`) and
/// `tier_selection.rs` (`rig_with_fleet`) already prove, with one paid
/// frontier candidate ($3/$15 per Mtok) beside one free registered worker.
/// Neither fact is the contested behavior -- this is what proves the ignored
/// test's failure is about tools specifically, and not about this harness
/// failing to reach the local path at all.
#[tokio::test]
async fn f2_control_the_rig_routes_local_by_default_without_tools() {
    let (app, _store, client) = surface_scripted_with_fleet().await;
    let mut request = body("hello");
    request["stream"] = json!(false);
    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[("x-claude-code-session-id", "sess-no-tools-local")],
        &request,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let message: Value = serde_json::from_str(&text).expect("a JSON message");
    assert_eq!(
        message["content"],
        json!([{ "type": "text", "text": "local answer" }]),
        "the local executor's canned reply -- this rig answers from the \
         local path with nothing else in play: {text}"
    );
    assert!(
        client.quotes_seen().is_empty(),
        "the frontier candidate must not have been contacted -- if it was, \
         this harness is not exercising the local-by-default path the F2 \
         finding below is about"
    );
}

/// **F2 (thermo-nuclear review of b8e8ddd), fixed: a tool-declaring turn is
/// never routed to a worker that cannot be told about a toolbox.**
///
/// `connect`'s own doc comment named the gap on its `declarations` parameter:
/// [`LocalExecutor::execute`] takes prompt token ids and an output cap, nothing
/// else — "this build has no way to tell a locally served model about a toolbox
/// at all" — and `LocalExecution::text` is a plain `String`, structurally
/// incapable of carrying a call back. What that comment did not say, and what
/// this test asserts must not hold, is that routing was blind to the gap it
/// named: `plan` priced every candidate, local included, with no read of
/// `declarations.tools` anywhere on that path, so a turn that declared tools was
/// exactly as eligible for the cheap local candidate as a prose one — and the
/// client got prose, `stop_reason: end_turn`, and no signal at all.
///
/// Same rig as the control above — see its doc comment for why the "local wins
/// on price" setup is real and not contrived. Three assertions:
///
/// - the turn is served, and served by the *frontier* candidate, which is the
///   one that can carry the client's twenty-four real tool definitions (the
///   fixture is the live 2.1.251 capture, not a hand-written one);
/// - the local worker's canned reply is nowhere in the answer, because "a
///   frontier quote was also seen" would still pass if the turn had somehow
///   answered locally too;
/// - and the decision record *says why* local was skipped. Without that, the
///   audit trail shows a frontier dispatch chosen over a cheaper local
///   candidate that is simply absent, and the counterfactual reads as a router
///   preference rather than a capability limit.
async fn f2_a_tool_declaring_turn_routed_local_loses_its_toolbox_silently(line: &CapturedLine) {
    let captured = fixture(line.turn_one);
    let tools = declared_tools(&captured);

    let (app, store, client) = surface_scripted_with_fleet().await;
    let mut request = captured.clone();
    request["stream"] = json!(false);

    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[("x-claude-code-session-id", "sess-tools-local")],
        &request,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let quotes = client.quotes_seen();
    assert_eq!(
        quotes.len(),
        1,
        "F2: a tool-declaring turn must reach the one candidate that \
         can carry a toolbox, not the one structurally unable to carry any \
         of them: {text}"
    );
    assert_eq!(
        quotes[0].tools.as_ref(),
        Some(&tools),
        "and it must arrive with the client's own declarations"
    );

    let message: Value = serde_json::from_str(&text).expect("a JSON message");
    assert_eq!(
        message["content"],
        json!([{ "type": "text", "text": ANSWER }]),
        "the frontier answered; `local answer` here would mean the turn was \
         served by the worker after all: {text}"
    );

    // And the audit trail says why the cheap candidate is missing.
    let rationale = routing_rationale(&store, &named("sess-tools-local")).await;
    assert!(
        rationale.contains("the client declared tools, so local candidates were excluded"),
        "F2: the decision record has to say why local was skipped, or the \
         counterfactual reads as a router preference: {rationale}"
    );
}
per_line_tests!(async f2_a_tool_declaring_turn_routed_local_loses_its_toolbox_silently);

/// The rationale on this session's one routing decision.
async fn routing_rationale(store: &MemoryStore, session_id: &str) -> String {
    let decisions: Vec<String> = store
        .read_events(&SessionId::new(session_id), 0, 1_000)
        .await
        .expect("an in-memory log reads")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision.rationale),
            _ => None,
        })
        .collect();
    assert_eq!(decisions.len(), 1, "one dispatch: {decisions:?}");
    decisions.into_iter().next().expect("checked above")
}

// ---------------------------------------------------------------------------
// The tool loop (M11.2)
// ---------------------------------------------------------------------------

/// **The whole of M11.2's serve half in one turn: a model that speaks, calls a
/// tool, speaks again and calls another arrives as four indexed content blocks a
/// client can act on.**
///
/// This is the turn Claude Code spends its entire life inside, and before this
/// milestone the surface could not express it: `FrontierChunk` had no tool-call
/// variant, so the same upstream stream reached the client as prose alone and
/// the agent's loop stalled on its first `Read`.
///
/// Four claims, and each fails differently:
///
/// - The stream is conformant *to the strict oracle*, which is the tier-1 judge
///   written from the pinned spec with its enums closed. A `tool_use` block it
///   cannot read is a block the real client cannot read either.
/// - The blocks come out interleaved, in the order the model produced them. A
///   projection that emitted all the text first would produce the same `text`
///   and the same block count while handing back an answer whose shape the
///   client then resends — and the resend would no longer match the log.
/// - Each call's arguments survive as the *bytes the model emitted*, reassembled
///   by the oracle exactly as the client's accumulator reassembles them: the
///   fragments, not the start frame's `input`.
/// - `stop_reason` is `tool_use`, which is how the client knows the turn is
///   waiting on it rather than finished.
#[tokio::test]
async fn a_tool_using_turn_streams_interleaved_blocks_the_client_can_run() {
    let (app, store, client) = surface_calling(speaking_and_calling(), Some("tool_use"));

    let accumulated = stream(
        &app,
        &[("x-claude-code-session-id", "sess-tools")],
        &body("find main"),
    )
    .await;

    assert_eq!(
        accumulated.blocks,
        vec![
            StrictBlock::Text {
                text: "Let me look.".into()
            },
            StrictBlock::ToolUse {
                id: "toolu_01".into(),
                name: "Grep".into(),
                input: json!({ "pattern": "fn main", "path": "/src" }),
            },
            StrictBlock::Text {
                text: " And also:".into()
            },
            StrictBlock::ToolUse {
                id: "toolu_02".into(),
                name: "Read".into(),
                input: json!({ "path": "/src/main.rs" }),
            },
        ],
        "the client's content array must be the model's own sequence"
    );
    assert_eq!(
        accumulated.tool_calls,
        vec![
            AccumulatedCall {
                id: "toolu_01".into(),
                name: "Grep".into(),
                input: json!({ "pattern": "fn main", "path": "/src" }),
            },
            AccumulatedCall {
                id: "toolu_02".into(),
                name: "Read".into(),
                input: json!({ "path": "/src/main.rs" }),
            },
        ]
    );
    assert_eq!(
        accumulated.stop_reason,
        Some(StrictStopReason::ToolUse),
        "a turn holding tool_use blocks and reporting anything else is a turn \
         the agent does not act on"
    );
    assert_eq!(accumulated.text, "Let me look. And also:");

    // And the log holds exactly what went out, in the same order — the property
    // the next turn's prefix check depends on. The arguments are the *canonical*
    // spelling rather than the model's own bytes, which is the correction M11.2
    // made to its first reading: the client resends this call with its arguments
    // as a JSON object, and canonicalizing that resend sorts the keys, so a log
    // holding `{"pattern": …, "path": …}` would disagree with the resend and
    // fork the session. See `canonical_arguments`; the round trip is closed by
    // `the_clients_tool_results_come_back_onto_the_same_session`.
    let items = stored_items(&store, &named("sess-tools")).await;
    let emitted: Vec<&Item> = items
        .iter()
        .filter(|item| item.response_id.is_some())
        .collect();
    assert_eq!(
        emitted
            .iter()
            .map(|item| item.content.clone())
            .collect::<Vec<_>>(),
        vec![
            ItemContent::Text {
                text: "Let me look.".into()
            },
            ItemContent::ToolCall {
                call_id: "toolu_01".into(),
                name: "Grep".into(),
                arguments: r#"{"path":"/src","pattern":"fn main"}"#.into(),
            },
            ItemContent::Text {
                text: " And also:".into()
            },
            ItemContent::ToolCall {
                call_id: "toolu_02".into(),
                name: "Read".into(),
                arguments: r#"{"path":"/src/main.rs"}"#.into(),
            },
        ],
        "the log's items and the wire's blocks are one sequence: {items:#?}"
    );
    assert!(
        emitted.iter().all(|item| item.role == Role::Assistant),
        "everything a response emits is the assistant's: {emitted:#?}"
    );

    // The client's toolbox reached the dispatch on the way in; without it the
    // upstream would have had nothing to call.
    assert_eq!(client.quotes_seen().len(), 1);
}

/// **The loop closes: the client runs the tools, sends the results back, and the
/// conversation stays on one session.**
///
/// The two-turn shape is the milestone. Turn one's history is turn one's
/// *answer* — the blocks the oracle accumulated, serialized back exactly as the
/// client would resend them — so this asserts prefix admission against a real
/// resend rather than against a hand-written one. A single byte of drift
/// anywhere on that round trip (a re-encoded argument object, a text run split
/// differently, an empty block the emitter invented) forks the conversation into
/// a second session, silently, while every turn still answers.
#[tokio::test]
async fn the_clients_tool_results_come_back_onto_the_same_session() {
    let (app, store, _client) = surface_calling(
        vec![
            Scripted::Text("Checking."),
            Scripted::Call {
                id: "toolu_01",
                name: "Grep",
                arguments: r#"{"pattern": "fn main", "path": "/src"}"#.into(),
            },
        ],
        Some("tool_use"),
    );
    let headers = [("x-claude-code-session-id", "sess-loop")];

    let first = stream(&app, &headers, &body("find main")).await;
    assert_eq!(first.stop_reason, Some(StrictStopReason::ToolUse));
    let after_first = stored_items(&store, &named("sess-loop")).await;

    // Turn two, as the client composes it: the original request, the assistant
    // message it just accumulated, and the result of the call it ran.
    let second_request = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [
            { "role": "user", "content": "find main" },
            resent_assistant(&first.blocks),
            { "role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_01",
                "content": "src/main.rs:1: fn main() {",
            }] },
        ],
    });
    let second = stream(&app, &headers, &second_request).await;
    assert_eq!(second.stop_reason, Some(StrictStopReason::ToolUse));

    // One session, and it is the one the first turn used. `Conversations::fork`
    // names the second session `…#g1`, so its absence is the honest spelling of
    // "did not fork" — an assertion on item counts alone would pass while a
    // fork quietly served the second turn from an empty history.
    assert!(
        no_such_session(&store, &format!("{}#g1", named("sess-loop"))).await,
        "the resent history forked instead of being admitted as a prefix"
    );

    let after_second = stored_items(&store, &named("sess-loop")).await;
    assert_eq!(
        after_second[..after_first.len()],
        after_first[..],
        "the first turn's items must survive the second turn unchanged"
    );
    let appended: Vec<ItemContent> = after_second[after_first.len()..]
        .iter()
        .map(|item| item.content.clone())
        .collect();
    assert_eq!(
        appended,
        vec![
            // Exactly one new input item: the tool result. The whole assistant
            // message was recognised as history we already hold.
            ItemContent::ToolResult {
                call_id: "toolu_01".into(),
                output: "src/main.rs:1: fn main() {".into(),
            },
            // And the second turn's own answer, in the same shape as the first.
            ItemContent::Text {
                text: "Checking.".into()
            },
            ItemContent::ToolCall {
                call_id: "toolu_01".into(),
                name: "Grep".into(),
                arguments: r#"{"path":"/src","pattern":"fn main"}"#.into(),
            },
        ],
        "the second turn appended more than the result it was carrying: \
         {after_second:#?}"
    );

    // The validate loop's extractor reads the whole log, and what it sees is a
    // *paired* exchange: a call this deployment emitted, answered by the result
    // the client brought back. That pairing is what the repeat and no-progress
    // signals are computed over, and before M11.2 it could only ever come from a
    // client that had made the call itself.
    let exchanges = roundhouse_core::validate::exchanges(&after_second);
    assert_eq!(exchanges.len(), 2, "{exchanges:#?}");
    assert_eq!(exchanges[0].call_id, "toolu_01");
    assert_eq!(
        exchanges[0].output.as_deref(),
        Some("src/main.rs:1: fn main() {"),
        "roundhouse's own emitted call must pair with the client's result"
    );
    assert_eq!(
        exchanges[1].output, None,
        "the turn's newest call has not been answered yet"
    );
}

/// A turn whose whole answer is a tool call carries it in a non-streaming body
/// too.
///
/// Claude Code re-issues a turn non-streaming when it cannot parse the stream
/// (§3.6), so the two projections answer the same question and must not
/// disagree: a non-streaming path that flattened `content` to one text block
/// would return, for the *same turn*, an answer that calls nothing.
#[tokio::test]
async fn the_non_streaming_body_carries_the_tool_use_blocks() {
    let (app, _store, _client) = surface_calling(
        vec![Scripted::Call {
            id: "toolu_01",
            name: "Bash",
            arguments: r#"{"command": "ls -la"}"#.into(),
        }],
        Some("tool_use"),
    );

    let mut request = body("list the files");
    request["stream"] = json!(false);
    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[("x-claude-code-session-id", "sess-nonstream")],
        &request,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let message: Value = serde_json::from_str(&text).expect("a JSON message");
    assert_eq!(
        message["content"],
        json!([{
            "type": "tool_use",
            "id": "toolu_01",
            "name": "Bash",
            "input": { "command": "ls -la" },
        }]),
        "the whole answer is the call, and no empty text block precedes it: {text}"
    );
    assert_eq!(message["stop_reason"], json!("tool_use"));
}

/// **A turn answered by a wire that has no word for `tool_use` still tells the
/// client the turn is waiting on it.**
///
/// Routing across dialects is the point of this product: a Claude Code turn can
/// be answered by an OpenAI-shaped upstream, whose stream names no stop reason
/// at all for an ordinary completion. Forwarding that silence would hand the
/// client `end_turn` beside a `tool_use` block — a message the real API never
/// sends and an agent never acts on, so the loop ends on the turn that was
/// supposed to start it.
///
/// The CONTROL is the same surface with no call in the script: *there* the
/// absent reason really does mean the turn is over, and inventing `tool_use`
/// would be the opposite bug.
#[tokio::test]
async fn a_call_from_a_wire_with_no_stop_reason_still_reports_tool_use() {
    let (app, _store, _client) = surface_calling(
        vec![Scripted::Call {
            id: "call_1",
            name: "shell",
            arguments: r#"{"command":["ls"]}"#.into(),
        }],
        None,
    );
    let accumulated = stream(
        &app,
        &[("x-claude-code-session-id", "sess-crossdialect")],
        &body("list"),
    )
    .await;
    assert_eq!(accumulated.stop_reason, Some(StrictStopReason::ToolUse));
    assert_eq!(accumulated.tool_calls.len(), 1);

    let (app, _store, _client) = surface_calling(vec![Scripted::Text("just an answer")], None);
    let control = stream(
        &app,
        &[("x-claude-code-session-id", "sess-crossdialect-control")],
        &body("say something"),
    )
    .await;
    assert_eq!(
        control.stop_reason,
        Some(StrictStopReason::EndTurn),
        "a turn with no calls and no reason named is a turn that finished"
    );
}

/// **F1's reporting half, end to end: a turn cut off at the ceiling says so.**
///
/// M11.1 fixed the *dispatch* half — the client's `max_tokens` became the
/// upstream ceiling — and left the reporting half with an `#[ignore]`d evidence
/// test, because the decoder discarded the upstream's `stop_reason` and nothing
/// downstream could tell a truncated answer from a finished one. Stage 1 carried
/// it onto `Done`; this is the other end of that wire, where a client finally
/// reads it.
#[tokio::test]
async fn f1_a_truncated_turn_reports_max_tokens_rather_than_end_turn() {
    let (app, _store, _client) =
        surface_calling(vec![Scripted::Text("as far as I got")], Some("max_tokens"));
    let accumulated = stream(
        &app,
        &[("x-claude-code-session-id", "sess-truncated")],
        &body("write an essay"),
    )
    .await;
    assert_eq!(accumulated.stop_reason, Some(StrictStopReason::MaxTokens));
    assert_ne!(
        accumulated.stop_reason,
        Some(StrictStopReason::EndTurn),
        "a truncated answer that reads as a complete one is the defect F1 named"
    );
}

/// **F5 (post-M11.1 thermo-nuclear review, 724dba8), fixed and pinned.** The
/// claim: `ItemContent::render` is simultaneously the identity encoding, the
/// token-count encoding, and the literal upstream prompt — so an opaque
/// block (an `image`, a `document`, anything this build does not name) is
/// billed and dispatched as its raw rendered JSON, base64 payload included,
/// rather than as the media type it actually is. Ruled **valid**, and fixed at
/// the one seam all three readings share: `ItemContent::Opaque::render` is a
/// `sha256` digest placeholder, so the payload reaches none of them. The block
/// is still stored verbatim, and a model that could *see* the image needs a
/// typed content-block path, which is the future work R5 names — this is the
/// bound on what the milestone bills and ships, not an image feature.
///
/// PROBE: an `image` block (Claude Code's own shape for a pasted screenshot)
/// carrying an easily-recognized base64 payload. The two assertions state the
/// *healthy* contract — bounded billing, no raw image bytes loose in the
/// text prompt — so a red run here is the defect, not a passing one: (1) the
/// turn's client-visible, ledger-drawn input count
/// (`message_start.usage.input_tokens`, sourced from
/// `Engine::admitted_input_tokens`) must not scale with the base64 payload's
/// *character* length the way byte-for-byte tokenizing would; (2) the string
/// the frontier client actually receives (`FrontierQuote::prompt`, what
/// `anthropic_messages::body` slices into `ContentBlock::Text` —
/// `wire::ContentBlock` has no image/document variant to slice it into
/// instead) must not carry that base64 payload verbatim, i.e. as prose.
#[tokio::test]
async fn f5_an_opaque_image_block_is_neither_billed_nor_dispatched_as_raw_base64() {
    let (app, _store, client) = surface_scripted();

    // 4096 repeated 'A's: long enough to dominate the turn's token count, and
    // distinctive enough that its appearance downstream cannot be anything
    // but this block's own payload.
    let payload = "A".repeat(4096);
    let request = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": "describe this image" },
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": payload,
                    },
                },
            ],
        }],
    });

    let accumulated = stream(
        &app,
        &[("x-claude-code-session-id", "sess-f5-image")],
        &request,
    )
    .await;
    assert_eq!(accumulated.error, None, "{:?}", accumulated.error);

    // An image-aware estimate reports a small constant regardless of
    // resolution; billing at or above one token per base64 character is the
    // signature of tokenizing the raw rendered JSON instead.
    assert!(
        (accumulated.usage.total_input() as usize) < payload.len(),
        "F5: a {}-byte base64 image was billed as {} total input tokens for \
         the whole turn — that is >= one token per base64 character, which is \
         what tokenizing ItemContent::Opaque::render()'s literal \
         `<block type=\"image\">{{...\"data\":\"AAAA...\"}}</block>` JSON \
         produces, not any image-aware estimate: {:?}",
        payload.len(),
        accumulated.usage.total_input(),
        accumulated.usage
    );

    let quotes = client.quotes_seen();
    assert_eq!(quotes.len(), 1, "one frontier dispatch: {quotes:?}");
    assert!(
        !quotes[0].prompt.contains(&payload),
        "F5: the dispatched prompt carries the image's raw base64 data \
         verbatim as prose text (not as an image content block), because \
         wire::ContentBlock has no image/document variant for \
         anthropic_messages::body to slice a segment into instead: {}",
        quotes[0].prompt
    );
}

/// `count_tokens` answers from this deployment's tokenizer.
///
/// The number is an estimate and the handler's doc says so; what this asserts is
/// that it is *served* and that it moves with the input. A refusal here does not
/// save the estimate's cost — the client falls back to a real one-token create
/// against the routed model — so the endpoint existing is a spend decision, not
/// a completeness one.
#[tokio::test]
async fn count_tokens_answers_and_grows_with_the_conversation() {
    let (app, _store) = surface();

    let (status, _, small) = post(&app, "/v1/messages/count_tokens", &[], &body("hi")).await;
    assert_eq!(status, StatusCode::OK, "{small}");
    let small: Value = serde_json::from_str(&small).expect("JSON");
    let small = small["input_tokens"].as_u64().expect("a count");

    let (_, _, large) = post(
        &app,
        "/v1/messages/count_tokens",
        &[],
        &body("hi, and then a great deal more text than that first one carried"),
    )
    .await;
    let large: Value = serde_json::from_str(&large).expect("JSON");
    let large = large["input_tokens"].as_u64().expect("a count");

    assert!(
        small > 0,
        "an estimate of zero for a real prompt is not one"
    );
    assert!(large > small, "{large} is not more than {small}");
}

/// **`count_tokens` counts the toolbox, because the toolbox is most of the
/// request** (M11.2a's F4).
///
/// The client asks this question *before* sending a body whose tool
/// declarations are most of its bytes — 65,835 of them on the prior line, 47,278
/// on the current one. An answer that counted only the messages told an agent
/// its context was a fifth as full as it really was, on the one endpoint that
/// exists to keep it from having to guess; and since the same
/// [`Engine::admitted_input_tokens`] is what `message_start` reports as
/// `input_tokens`, the two answers for one body would have had to differ.
///
/// The probe is the live capture rather than a hand-written toolbox, and the
/// two requests differ in exactly one key, so the delta measured is the tool
/// preamble and nothing else. **The floor is derived from each fixture's own
/// toolbox** rather than written out: the two rigs declared different toolboxes,
/// and a literal here would fail the smaller one for a reason that is about the
/// invocation and not about the count.
async fn count_tokens_counts_the_declared_toolbox(line: &CapturedLine) {
    let (app, _store) = surface();
    let captured = fixture(line.turn_one);
    declared_tools(&captured);
    let tools_bytes = serde_json::to_string(&captured["tools"])
        .expect("the fixture's tools serialize")
        .len() as u64;

    let mut untooled = captured.clone();
    untooled
        .as_object_mut()
        .expect("a JSON object")
        .remove("tools")
        .expect("the capture declares tools");

    async fn count(app: &Router, request: &Value) -> u64 {
        let (status, _, text) = post(app, "/v1/messages/count_tokens", &[], request).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        serde_json::from_str::<Value>(&text).expect("JSON")["input_tokens"]
            .as_u64()
            .expect("a count")
    }

    let bare = count(&app, &untooled).await;
    let tooled = count(&app, &captured).await;
    // Nine tenths rather than all of it: the estimator is a tokenizer, not a
    // byte counter, and pinning it to the exact serialization would make
    // this test fail the day the tokenizer improves — which is not the
    // finding it exists to hold.
    assert!(
        tooled >= bare + tools_bytes * 9 / 10,
        "F4: the estimate must include the {tools_bytes}-byte toolbox \
         the same request is about to send — bare={bare}, tooled={tooled}"
    );
}
per_line_tests!(async count_tokens_counts_the_declared_toolbox);

// ---------------------------------------------------------------------------
// The session across turns
// ---------------------------------------------------------------------------

/// **Full-history resend admits the prefix and appends only the suffix.**
///
/// This is what the whole surface exists for. Claude Code re-sends the entire
/// conversation on every turn — verified again at 2.1.251, where the
/// `--continue` body replayed the mock's own reply verbatim — so a server that
/// treated the resend as input would append the conversation again on turn two,
/// bill the doubled prompt, and never match a warm prefix. The failure is
/// invisible from the client's side because every turn still answers, which is
/// why the assertion is on the log rather than on the reply.
#[tokio::test]
async fn a_resent_history_is_admitted_as_a_prefix_and_not_appended_twice() {
    let (app, store) = surface();
    let headers = [("x-claude-code-session-id", "sess-two-turns")];

    let first = stream(&app, &headers, &body("hello")).await;
    assert_eq!(first.text, ANSWER);

    // Exactly what the client sends next: everything it had, plus the answer it
    // was just given, plus the new question.
    let grown = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": [{ "type": "text", "text": ANSWER }] },
            { "role": "user", "content": "and again" },
        ],
    });
    let second = stream(&app, &headers, &grown).await;
    assert_ne!(
        second.message_id, first.message_id,
        "a new question is a new response"
    );

    let items = stored_items(&store, &named("sess-two-turns")).await;
    assert_eq!(
        items
            .iter()
            .filter(|item| **item == Item::user_text("hello"))
            .count(),
        1,
        "the resent history must not be re-appended: {items:#?}"
    );
    assert_eq!(
        items
            .iter()
            .filter(|item| item.role == Role::Assistant)
            .count(),
        2,
        "one assistant item per answered turn: {items:#?}"
    );
}

/// A history the client rewrote forks to a fresh session rather than merging.
///
/// The other half of the same rule. A compaction or an edited message is not a
/// continuation, and appending the difference would produce a conversation
/// neither side believes in — so the fork is the conservative answer, at the
/// price of a cold prefix.
#[tokio::test]
async fn a_divergent_resend_forks_rather_than_merging() {
    let (app, store) = surface();
    let headers = [("x-claude-code-session-id", "sess-fork")];

    stream(&app, &headers, &body("hello")).await;
    // The same session name, a different first message: the client edited its
    // own history out from under us.
    let forked = stream(&app, &headers, &body("actually, goodbye")).await;
    assert_eq!(forked.text, ANSWER);

    let original = stored_items(&store, &named("sess-fork")).await;
    let forked = stored_items(&store, &named("sess-fork#g1")).await;
    assert!(
        original
            .iter()
            .any(|item| *item == Item::user_text("hello")),
        "the original session keeps the history it was told: {original:#?}"
    );
    assert!(
        forked
            .iter()
            .any(|item| *item == Item::user_text("actually, goodbye")),
        "the rewritten history opens a fresh generation: {forked:#?}"
    );
    assert!(
        !forked.iter().any(|item| *item == Item::user_text("hello")),
        "and the fork starts empty rather than inheriting: {forked:#?}"
    );
}

/// **A retried turn replays its answer instead of generating a second one.**
///
/// The idempotency this dialect needs most. Claude Code re-POSTs after a 5xx and
/// after a stream that died mid-answer, and it re-sends the *same conversation*
/// when it does — so the turn id, a content hash of the whole canonicalized
/// conversation, is the same and the engine replays. A surface that generated
/// again would answer correctly, cost twice, and show nothing wrong anywhere the
/// client can see.
///
/// This is also the only test that drives the follower's replay phase, which is
/// a different code path from tailing: it re-reads the log from zero, bounded by
/// the `turn_deduplicated` marker, and projects the *earlier* response's entries
/// through the same emission. A stream assembled wrongly there is one the client
/// throws on rather than one it ignores.
#[tokio::test]
async fn a_retried_turn_replays_rather_than_answering_twice() {
    let (app, store) = surface();
    let headers = [("x-claude-code-session-id", "sess-retry")];

    let first = stream(&app, &headers, &body("hello")).await;
    // Byte for byte the request the client sends again when its connection
    // dropped before the terminal frame.
    let replayed = stream(&app, &headers, &body("hello")).await;

    assert_eq!(
        replayed.message_id, first.message_id,
        "a retry must land on the response it already paid for"
    );
    assert_eq!(
        replayed.text, first.text,
        "and carry that response's answer, assembled from the replayed deltas"
    );
    assert_eq!(replayed.completed_blocks, 1);

    let items = stored_items(&store, &named("sess-retry")).await;
    assert_eq!(
        items
            .iter()
            .filter(|item| item.role == Role::Assistant)
            .count(),
        1,
        "one answer, not two: {items:#?}"
    );
}

/// **F2: a retry after a mid-answer `overloaded_error` keeps the conversation
/// on the session it has been using all along.**
///
/// The scenario `mark_incomplete`'s own doc names as the reason it commits a
/// partial at all: "the successor can resume from it." Here the successor is
/// the *same* turn id retried after a transient failure, exactly as `Z59`
/// (`research/claude-code-client-surface.md` §3.2/§2.5) retries a mid-stream
/// `overloaded_error` by re-issuing the identical request — the client never
/// saw the partial (its parser throws on `event: error` before any
/// `message_stop`), so the retry's body cannot and does not carry it.
///
/// **What used to happen.** The log held the partial and the continuation as
/// two assistant items; the client's next resend carried only the continuation
/// it had actually received; the two disagreed at that item under `same_item`
/// and the session forked — punishing a client that did exactly what the retry
/// contract asked of it, on a turn a transient upstream failure had already
/// cost it once, and taking the routing history and warm prefix with it.
///
/// **What happens now.** An item stamped by a response the log records as
/// *incomplete* is provisional: prefix admission leaves it out of what a claim
/// is checked against, so a client that discarded it continues on the same
/// session and a client that resends it has it re-admitted as ordinary history.
/// Supersession is a reading of what the log already records — the
/// `ResponseIncomplete` event — rather than a rewrite of it; nothing committed
/// is edited or removed.
///
/// Everything else about the mechanism is asserted unchanged, because the fix
/// is deliberately the admission half only:
///
/// 1. The retry does not deduplicate (the first attempt never completed) and
///    its own prompt — captured off the double, never off the wire — still
///    contains the partial, because `Engine::plan` rehydrates from
///    `session.state().items` and the partial is a genuine cache hit on the
///    target that produced it. The wire gives no sign of this: the retry's
///    stream is an ordinary `message_start`…`message_stop`.
/// 2. The log still holds the partial and the continuation as two separate
///    assistant items. Append-only means append-only.
/// 3. The client's *next* turn, carrying only what it actually received, lands
///    on the same session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_after_a_mid_stream_failure_keeps_the_conversation_on_one_session() {
    let (app, store, client) = surface_partial_then_fail();
    let headers = [("x-claude-code-session-id", "sess-partial-retry")];

    // Attempt 1: the model gets partway through, then the provider dies.
    let first = stream(&app, &headers, &body("hello")).await;
    let failure = first
        .error
        .clone()
        .expect("a mid-stream death must end the turn in an error event");
    assert_eq!(
        failure.kind,
        StrictErrorKind::OverloadedError,
        "only this spelling is retried under subscription OAuth (§2.5's `Z59`): {failure:?}"
    );
    assert_eq!(
        first.text, PARTIAL,
        "what a live client would have rendered before the throw"
    );
    assert_eq!(
        first.completed_blocks, 1,
        "the block the prelude opened must still be closed before the error"
    );

    // Attempt 2: byte-for-byte the same request — the client's own retry,
    // unaware the partial exists.
    let retry = stream(&app, &headers, &body("hello")).await;
    assert_eq!(
        retry.error, None,
        "the retry must not itself end in an error: {:?}",
        retry.error
    );
    assert_ne!(
        retry.message_id, first.message_id,
        "the failed attempt never completed, so this is a fresh response, not a replay"
    );
    assert_eq!(
        retry.text, CONTINUATION,
        "the wire carries only the continuation — nothing marks it as one, and nothing \
         restates the partial the client already lost: {:?}",
        retry.text
    );
    assert!(
        retry.stop_reason.is_some() && retry.completed_blocks == 1,
        "an ordinary, unremarkable-looking completed turn: {retry:?}"
    );

    // The mechanism: the retried generation's own prompt silently carried the
    // partial as context, which is *why* the model produced a bare
    // continuation instead of a fresh, self-contained answer.
    let prompts = client.prompts_seen();
    assert_eq!(prompts.len(), 2, "exactly one prompt per dispatch attempt");
    assert!(
        prompts[1].contains(PARTIAL.trim_end()),
        "the retry's own prompt must contain the partial the client discarded, or the \
         continuation could not follow it as prose: {:?}",
        prompts[1]
    );

    // The log never merges the two halves into one answer.
    let after_retry = stored_items(&store, &named("sess-partial-retry")).await;
    let assistant_texts: Vec<String> = after_retry
        .iter()
        .filter(|item| item.role == Role::Assistant)
        .map(|item| item.content.render())
        .collect();
    assert_eq!(
        assistant_texts,
        vec![PARTIAL.to_string(), CONTINUATION.to_string()],
        "two separate assistant items, not one spliced answer: {assistant_texts:?}"
    );
    assert_eq!(
        after_retry
            .iter()
            .filter(|item| **item == Item::user_text("hello"))
            .count(),
        1,
        "the retry's empty suffix must not re-append the user turn: {after_retry:#?}"
    );

    // The next turn: the client's own history now, carrying only what it ever
    // actually received as "the assistant's reply."
    let next_turn = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": [{ "type": "text", "text": CONTINUATION }] },
            { "role": "user", "content": "and then?" },
        ],
    });
    let third = stream(&app, &headers, &next_turn).await;
    assert_eq!(third.error, None, "{:?}", third.error);

    // The invariant a client's honest retry-then-continue is owed: resending
    // exactly what it received must carry the conversation forward on the
    // *same* session, not fork away from it. `bind_prefix`'s own doc calls a
    // fork "the conservative answer, at the price of a cold prefix" for a
    // client that edited or compacted its history — but this client did
    // neither; it resent the unedited answer it was actually given.
    let original_after_next_turn = stored_items(&store, &named("sess-partial-retry")).await;
    assert!(
        original_after_next_turn
            .iter()
            .any(|item| *item == Item::user_text("and then?")),
        "the next turn must land on the session the client has been using all along, not fork \
         away from it over a split its own retry could not have avoided: {original_after_next_turn:#?}"
    );
    assert!(
        no_such_session(&store, &named("sess-partial-retry#g1")).await,
        "and it must not have landed in a fresh generation, which is where the routing \
         history and the warm prefix would have been left behind"
    );
    // The partial is still on the log — superseded, not deleted. Append-only
    // means the supersession is a reading of what was recorded (the
    // `ResponseIncomplete` beside it), never an edit to it.
    assert!(
        original_after_next_turn
            .iter()
            .any(|item| item.content.render() == PARTIAL),
        "the partial stays committed: {original_after_next_turn:#?}"
    );
}

/// CONTROL for F2: a client that *keeps* the partial is not punished for it
/// either — it is re-admitted as ordinary history and the conversation
/// continues on the same session.
///
/// The other half of the same rule, and the reason the fix is not "drop
/// partials on the floor": `mark_incomplete`'s own doc commits the partial so a
/// successor can resume from it, and a client whose SSE layer *did* surface the
/// bytes before the error will resend them. Both readings of the same failure
/// have to land on one session, or the surface has merely moved which honest
/// client it punishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f2_control_a_client_that_resends_the_partial_also_stays_on_one_session() {
    let (app, store, _client) = surface_partial_then_fail();
    let headers = [("x-claude-code-session-id", "sess-partial-kept")];

    let first = stream(&app, &headers, &body("hello")).await;
    assert!(first.error.is_some(), "the fixture must fail mid-answer");

    // What a client that rendered the partial before the throw resends: the
    // question, the half-answer it saw, and the next question.
    let next_turn = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": [{ "type": "text", "text": PARTIAL }] },
            { "role": "user", "content": "and then?" },
        ],
    });
    let second = stream(&app, &headers, &next_turn).await;
    assert_eq!(second.error, None, "{:?}", second.error);

    let items = stored_items(&store, &named("sess-partial-kept")).await;
    assert!(
        items
            .iter()
            .any(|item| *item == Item::user_text("and then?")),
        "the resent partial must be admitted as history rather than forked over: {items:#?}"
    );
    assert!(
        no_such_session(&store, &named("sess-partial-kept#g1")).await,
        "a client that kept the partial must not fork either"
    );
}

/// F3 (thermo-nuclear review of b8e8ddd): a durably-committed tool call can
/// outlive its response's terminal event, and the client's honest
/// retry-then-continue then forks the session -- the failure class M11.1's
/// F2 fix exists to prevent, reopened by a new item class that fix does not
/// cover.
///
/// `Session::append_emitted`'s own doc names the invariant directly: "What
/// it must never leave behind is an emitted item and *no* terminal event."
/// Nothing enforces it. `engine.rs`'s `Ok(Completed{..})` arm commits the
/// terminal with `session.complete(..)` and maps a failure straight out with
/// no fallback at all -- unlike the adjacent `Err(failed)` arm, which at
/// least attempts `mark_incomplete` (best-effort, `let _ = ..`, because "the
/// usual reason this append fails is a lost lease" -- the same class of
/// failure that can also be why the surrounding dispatch failed).
/// [`DropsFirstTerminalWrite`] models exactly that: the `append_emitted`
/// commit for the tool call succeeds, and the very next append --
/// `Session::complete`'s batch, holding only `ResponseCompleted` since a
/// whole-answer-is-a-call turn commits no trailing item -- fails.
///
/// `stored_conversation`'s provisional-item tracking (`responses_api.rs`,
/// the M11.1 F2 fix that `messages_api.rs` reuses via `bind_prefix`) keys
/// exclusively on `SessionEventKind::ResponseIncomplete`. An item stamped by
/// a response that never wrote *any* terminal event -- neither
/// `ResponseCompleted` nor `ResponseIncomplete` -- is not provisional by
/// that reading: it is ordinary, permanently confirmed history, compared
/// strictly against a client that never saw it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_orphaned_tool_call_does_not_fork_the_session_on_the_next_turn() {
    let (app, store, _client) = surface_orphaned_tool_call();
    let headers = [("x-claude-code-session-id", "sess-f3-orphan")];

    // Attempt 1: the tool call streams and is durably committed via
    // `append_emitted`, then the store drops the terminal write. The client
    // sees the block close and then an `error` frame with no `message_stop`
    // -- a permanent fault (`api_error`, not `overloaded_error`), not one the
    // SDK retries on its own (§3.2).
    let first = stream(&app, &headers, &body("do it")).await;
    let failure = first
        .error
        .clone()
        .expect("dropping the terminal write must end the turn in an error event");
    assert_eq!(
        failure.kind,
        StrictErrorKind::ApiError,
        "a permanent fault, not a retry-me signal an agent could spend its \
         whole retry budget on: {failure:?}"
    );

    // The premise, checked directly against the log rather than assumed: the
    // tool call is durable, and genuinely nothing terminates its response --
    // neither `ResponseCompleted` nor `ResponseIncomplete`.
    let raw = store
        .inner
        .read_events(&SessionId::new(named("sess-f3-orphan")), 0, 4096)
        .await
        .expect("the session exists");
    let orphan_response = raw
        .iter()
        .find_map(|event| {
            let SessionEventKind::ItemAppended { item } = &event.kind else {
                return None;
            };
            let ItemContent::ToolCall { call_id, .. } = &item.content else {
                return None;
            };
            (call_id == "toolu_01")
                .then(|| item.response_id.clone())
                .flatten()
        })
        .unwrap_or_else(|| panic!("the tool call must have been durably committed: {raw:#?}"));
    assert!(
        raw.iter()
            .filter(|event| event.is_terminal())
            .all(|event| event.response_id() != Some(&orphan_response)),
        "F3's premise: no terminal event may exist for the response that \
         emitted the orphaned tool call, or this fixture is not modelling \
         the finding: {raw:#?}"
    );

    // Attempt 2: the client's honest retry -- byte-for-byte the same
    // request, unaware the tool call exists (its own stream threw before
    // `message_stop`, exactly the discard M11.1's F2 fix already establishes
    // for a partial *text* run -- see
    // `a_retry_after_a_mid_stream_failure_keeps_the_conversation_on_one_session`
    // above). The retry's turn id is unchanged (identical body);
    // `begin_turn`'s dedup lookup finds no *completed* response for it --
    // the orphan never reached one -- so it dispatches fresh rather than
    // replaying or hanging.
    let retry = stream(&app, &headers, &body("do it")).await;
    assert_eq!(retry.error, None, "{:?}", retry.error);
    assert_eq!(
        retry.text, F3_RETRY_REPLY,
        "a fresh dispatch answering in prose, exactly like F2's retry: {:?}",
        retry.text
    );

    // Attempt 3: the client's own history now, carrying only what it
    // actually received across the two attempts above -- no tool call,
    // because it never saw one close before an `error` ended the turn.
    let next_turn = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [
            { "role": "user", "content": "do it" },
            { "role": "assistant", "content": [{ "type": "text", "text": F3_RETRY_REPLY }] },
            { "role": "user", "content": "and then?" },
        ],
    });
    let third = stream(&app, &headers, &next_turn).await;
    assert_eq!(third.error, None, "{:?}", third.error);

    // THE FINDING: the honest retry-then-continue must land on the session
    // the client has been using all along -- exactly the invariant F2's own
    // fix proves for a partial text run -- not fork away from it over an
    // item the client could never have resent. `bind_prefix`'s own doc calls
    // a fork "the conservative answer, at the price of a cold prefix" for a
    // client that edited or compacted its history; this client did neither.
    assert!(
        no_such_session(&store.inner, &named("sess-f3-orphan#g1")).await,
        "F3: the orphaned tool call forked the session on the next turn, \
         losing the routing history and warm prefix on a turn a transient \
         store failure already cost the client once"
    );
}

/// **F3's liveness guard: an item with no terminal event is not an orphan
/// while somebody is still writing the session.**
///
/// The A/B against the test above, and the reason the widened provisional set
/// is safe at all. "No terminal event" is not only what a dead turn leaves
/// behind — it is also exactly what a turn *mid-stream* looks like, one that
/// has committed a tool call through `append_emitted` and has not reached
/// `Session::complete` yet. A reading that did not consult the lease would let
/// a second request arriving in that window treat a live turn's committed
/// items as supersedable, admit a claim that contradicts them, and append the
/// difference into a log another writer is still extending. That is not a
/// fork; it is one session's log carrying two interleaved conversations,
/// neither of which either client believes in — and unlike a fork, nothing
/// downstream can tell afterwards that it happened.
///
/// So the same rig, the same orphaned tool call, the same honest retry — and
/// one difference: another node holds the lease. The log is byte-identical to
/// the one the test above supersedes over; only the answer to "is anybody
/// writing this" has changed, which is what makes this an isolation of the
/// guard rather than of anything else. The turn takes the conservative arm and
/// forks, at the price of a cold prefix, which is precisely what
/// `bind_prefix`'s own doc calls that outcome.
///
/// The lease is taken by a *different node id* on purpose: that is the real
/// shape of the hazard. Within one process the engine's per-session turn gate
/// already serializes turns, but it is taken inside `run_turn`, long after
/// `bind_prefix` has read and admitted — and a second roundhouse serving the
/// same store has no gate at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_orphan_is_left_standing_while_another_node_holds_the_lease() {
    let (app, store, _client) = surface_orphaned_tool_call();
    let headers = [("x-claude-code-session-id", "sess-f3-live")];

    // Attempt 1, exactly as above: the tool call is durably committed through
    // `append_emitted` and the terminal write for its response is dropped.
    let first = stream(&app, &headers, &body("do it")).await;
    assert!(
        first.error.is_some(),
        "the fixture must drop the terminal write"
    );

    // The one difference from the test above. A successor node picks this
    // session up — mid-recovery, or simply because the fleet put the client's
    // next turn on a different roundhouse — and holds the lease while our
    // request is admitted.
    let session = SessionId::new(named("sess-f3-live"));
    store
        .inner
        .acquire_lease(&session, "another-node", 60_000)
        .await
        .expect("the session exists")
        .expect("nothing else holds this lease");

    // The client's next turn, carrying only what it actually received — the
    // same shape the test above continues with, and the only shape that can
    // tell the two readings apart. A *byte-identical retry* cannot: a claim
    // shorter than the stored history is the ordinary retry and is admitted
    // with an empty suffix whether or not the orphan was superseded. It takes
    // a claim that reaches *past* the orphan's position to ask whether the
    // orphan is still being compared against.
    let next_turn = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [
            { "role": "user", "content": "do it" },
            { "role": "user", "content": "and then?" },
        ],
    });
    let (status, _, text) = post(&app, "/v1/messages", &headers, &next_turn).await;
    assert_eq!(status, StatusCode::OK, "{text}");

    assert!(
        !no_such_session(&store.inner, &named("sess-f3-live#g1")).await,
        "F3's guard: while another node holds the lease, an item with no \
         terminal event may be a turn still in flight, so it must be compared \
         strictly and the disagreeing claim must fork rather than supersede \
         items a live writer is still producing"
    );
    let items = stored_items(&store.inner, &named("sess-f3-live")).await;
    assert_eq!(
        items
            .iter()
            .filter(|item| **item == Item::user_text("do it"))
            .count(),
        1,
        "and nothing was appended into the leased session behind its writer — \
         a superseded orphan would have re-admitted this question onto a log \
         another node is holding: {items:#?}"
    );
}

/// **The header and both `user_id` spellings name one session.**
///
/// R5's resolution order, asserted where it matters: a deployment serving a
/// mixed fleet — one user on 2.1.42, one on 2.1.251, one behind a Relay that
/// strips headers — must put each client's own session on one log. A reader that
/// preferred `user_id` over the header would bind a subagent's turns to its
/// parent's session; a reader that parsed only one `user_id` shape would re-key
/// every session the day a user upgraded.
#[tokio::test]
async fn the_header_and_both_user_id_shapes_reach_one_session() {
    let (app, store) = surface();
    let session = "e13acbde-ab70-46ff-b094-fd8ce95d286d";
    let modern = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [{ "role": "user", "content": "hello" }],
        "metadata": { "user_id": format!(
            "{{\"device_id\":\"{}\",\"account_uuid\":\"\",\"session_id\":\"{session}\"}}",
            "0".repeat(64),
        )},
    });
    let legacy = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": ANSWER },
            { "role": "user", "content": "again" },
        ],
        "metadata": { "user_id": format!("user_abc_account_def_session_{session}") },
    });

    // Turn one names the session in the 2.1.251 body shape and no header.
    stream(&app, &[], &modern).await;
    // Turn two names it in the pre-2.1.247 shape *and* in the header, which is
    // the mixed-fleet case: they agree, so they must resolve together.
    stream(&app, &[("x-claude-code-session-id", session)], &legacy).await;

    let items = stored_items(&store, &named(session)).await;
    assert_eq!(
        items
            .iter()
            .filter(|item| **item == Item::user_text("hello"))
            .count(),
        1,
        "the two spellings named one session, so the second turn saw a prefix: {items:#?}"
    );
    assert!(
        items.iter().any(|item| *item == Item::user_text("again")),
        "and the new question was appended to it: {items:#?}"
    );
}

/// An unnamed conversation gets a session of its own rather than a refusal.
///
/// The anonymous arm is for a bare `curl`, not for the product path: every
/// version of the client read sends `metadata.user_id` on every request. What it
/// must not do is put two unrelated callers on one log, which a content-derived
/// name would have done for two identical bodies.
#[tokio::test]
async fn two_unnamed_turns_do_not_share_a_conversation() {
    let store = Arc::new(MemoryStore::new());
    // Held rather than minted inside the router, because the session an
    // anonymous turn lands in has no name the test can predict — the binding
    // table is the only thing that knows it.
    let conversations = Arc::new(Conversations::new());
    let app = messages_router(
        ControlPlane::open(),
        engine(Arc::clone(&store)),
        Arc::clone(&store),
        Arc::clone(&conversations),
    );
    let anonymous = roundhouse_core::control::Principal::default_open();

    // Two byte-identical bodies, which is what makes this a test rather than a
    // tautology: a name derived from the request's content would put them both
    // in one session and every assertion about *answers* would still pass.
    stream(&app, &[], &body("hello")).await;
    let first = conversations
        .latest(&anonymous)
        .expect("the turn bound a session");
    stream(&app, &[], &body("hello")).await;
    let second = conversations
        .latest(&anonymous)
        .expect("the second turn bound one too");

    assert_ne!(
        first, second,
        "two identical anonymous bodies must not land in one conversation"
    );
    assert!(first.to_string().starts_with("anonymous-"), "{first}");
    assert_eq!(
        stored_items(&store, &second.to_string()).await.len(),
        2,
        "the second session holds its own question and answer and nothing else"
    );
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// A control plane whose one project caps tokens over five hours.
fn capped_plane(max_tokens: u64) -> Arc<ControlPlane> {
    let json = json!({
        "projects": [{
            "id": "bench",
            "fair_use": { "windows": [{ "window": "5h", "max_tokens": max_tokens }] },
        }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "bench", "user": "ada", "key_sha256": sha256_hex(&key("ada")) }],
    })
    .to_string();
    Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "messages fair-use fixture")
            .expect("the fixture config must validate"),
    ))
}

/// **The fair-use `429` in the envelope and the header this client reads.**
///
/// Two halves, and the second is the one a reading would miss. The body must say
/// `rate_limit_error`, because that is what routes a subscription-OAuth client to
/// its rate-limit UI rather than to its retry loop. And `retry-after` must be
/// *present*: that path sleeps on the header and, absent it, defaults to thirty
/// minutes floored at ten — so a two-minute window reported only in the body is a
/// two-minute ceiling the agent waits half an hour on.
#[tokio::test]
async fn a_fair_use_refusal_is_a_rate_limit_error_with_a_retry_time() {
    let plane = capped_plane(1);
    let store = Arc::new(MemoryStore::new());
    let app = messages_router(
        plane,
        engine(Arc::clone(&store)),
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );
    let authorized = [(AUTHORIZATION.as_str(), &*format!("Bearer {}", key("ada")))];

    // The first turn fills the one-token window.
    let (status, _, _) = post(&app, "/v1/messages", &authorized, &body("hello")).await;
    assert_eq!(status, StatusCode::OK);

    let (status, headers, text) = post(&app, "/v1/messages", &authorized, &body("again")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{text}");
    let refusal: Value = serde_json::from_str(&text).expect("an error envelope");
    assert_eq!(refusal["type"], "error", "{refusal}");
    assert_eq!(refusal["error"]["type"], "rate_limit_error", "{refusal}");
    assert!(
        refusal["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "{refusal}"
    );
    // The machine-readable half of the same refusal, carried through unchanged.
    // An agent acts on this; a person acts on the sentence above.
    assert!(
        refusal["error"]["resets_at"].as_u64().is_some(),
        "{refusal}"
    );
    assert_eq!(refusal["error"]["roundhouse_code"], "fair_use_exceeded");
    assert!(
        headers.contains_key("retry-after"),
        "the client's 429 path sleeps on `retry-after` and defaults to half an \
         hour without it: {headers:?}"
    );
}

/// Authentication is decided before the body is read.
///
/// Ordering, asserted the only way it can be: an unauthenticated request whose
/// body is *also* unreadable must answer `401`, not `422`. A handler that parsed
/// first would let a stranger's malformed body choose this process's error path
/// — and, worse, would let a stranger name a session.
#[tokio::test]
async fn an_unauthenticated_request_is_refused_before_its_body_is_parsed() {
    let plane = capped_plane(1_000_000);
    let store = Arc::new(MemoryStore::new());
    let app = messages_router(
        plane,
        engine(Arc::clone(&store)),
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from("{this is not JSON"))
                .expect("a well-formed request"),
        )
        .await
        .expect("the router answers");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let refusal: Value = serde_json::from_slice(&bytes).expect("an error envelope");

    assert_eq!(status, StatusCode::UNAUTHORIZED, "{refusal}");
    assert_eq!(refusal["type"], "error");
    assert_eq!(
        refusal["error"]["type"], "authentication_error",
        "{refusal}"
    );

    // CONTROL: with a key, the same unreadable body is a `422` naming the body.
    // Without this the assertion above would pass for a handler that answered
    // `401` to everything.
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header(CONTENT_TYPE, "application/json")
                .header(AUTHORIZATION, format!("Bearer {}", key("ada")))
                .body(Body::from("{this is not JSON"))
                .expect("a well-formed request"),
        )
        .await
        .expect("the router answers");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let refusal: Value = serde_json::from_slice(&bytes).expect("an error envelope");
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refusal}");
    assert_eq!(
        refusal["error"]["type"], "invalid_request_error",
        "{refusal}"
    );
}

/// A router with nowhere to route, so a turn is admitted and then fails.
fn nowhere() -> Router {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        roundhouse_fleet::StaticFrontierCatalog::new(vec![]),
        Arc::new(EchoFrontierClient::new(ANSWER)),
        Arc::new(AffinityPolicy::new()),
        config(),
    ));
    messages_router(
        ControlPlane::open(),
        engine,
        store,
        Arc::new(Conversations::new()),
    )
}

/// **A turn that fails after admission ends the stream, and says how.**
///
/// The failure arrives once the headers are out, so a status code is no longer
/// expressible and an `error` event is the only answer left. Two things have to
/// be true of it and neither is obvious.
///
/// The block opened by the prelude must be *closed* before the error — a stream
/// that ends with a block still open is one the client's accumulator never
/// finishes, and the oracle refuses it.
///
/// And the error type has to be the one the client's recovery reads. Here it is
/// `overloaded_error`, which is correct for *this* reason and only this one: a
/// catalog with nowhere to route is `IncompleteReason::UpstreamError`, a
/// transient fault that a retry can clear, and `overloaded_error` is the only
/// spelling Claude Code retries a mid-stream failure on. The complementary claim
/// — that a policy refusal or a spent budget is *not* spelled that way, because
/// an agent would then burn its whole retry budget on a turn that can never
/// succeed — is the partition asserted over every
/// [`IncompleteReason`](roundhouse_core::event::IncompleteReason) in
/// `messages_api::emit`'s own suite, where all six can be reached without
/// building six engines.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_with_nowhere_to_go_ends_the_stream_with_an_error_event() {
    let (status, _, text) = post(&nowhere(), "/v1/messages", &[], &body("hello")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the headers were already out: {text}"
    );

    let accumulated = audit(&text)
        .unwrap_or_else(|error| panic!("even a failure must be conformant: {error}\n{text}"));
    let failure = accumulated
        .error
        .expect("a turn that produced no answer must say so");
    assert_eq!(
        failure.kind,
        StrictErrorKind::OverloadedError,
        "a transient upstream failure is the one case a retry clears: {failure:?}"
    );
    assert!(!failure.message.is_empty(), "{failure:?}");
    // **Zero, and it changed from one in M11.2.** While the prelude opened a
    // content block eagerly, a failed turn closed that block before its error
    // and the count here was one. Blocks are opened by their content now — the
    // change that stops an agent turn carrying an empty text block ahead of its
    // tool call — so a turn that failed before saying anything opens none, which
    // is also what the upstream API does: an `overloaded_error` mid-stream is
    // `message_start` then `error`, with no content between them. The §3.6
    // re-issue condition this does *not* trip is about a stream that *completes*
    // with no block; an `error` event throws in the client's SSE layer before
    // any of that is reached (§3.2).
    assert_eq!(
        accumulated.completed_blocks, 0,
        "a turn that produced nothing opens no block to close: {text}"
    );
}

/// The same failure without streaming is a status code, not a `200`.
///
/// The non-streaming path still has the status line available, and a client that
/// had to parse a success body to discover a failure would not — the SDK's error
/// handling runs off the status off the streaming path. This is also the only
/// test that reaches the inverse mapping from the emission's wire vocabulary
/// back to a status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_failure_without_streaming_is_a_status_code() {
    let (status, _, text) = post(
        &nowhere(),
        "/v1/messages",
        &[],
        &json!({
            "model": "claude-opus-5",
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "test" }],
        }),
    )
    .await;
    assert!(
        status.is_client_error() || status.is_server_error(),
        "a failed turn must not answer 200 with an error body: {status} {text}"
    );
    let refusal: Value = serde_json::from_str(&text).expect("an error envelope");
    assert_eq!(refusal["type"], "error", "{refusal}");
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "the mid-stream `overloaded_error` inverts to the one status whose own \
         mapping spells it back the same way: {refusal}"
    );
    assert_eq!(refusal["error"]["type"], "overloaded_error", "{refusal}");
}

/// A content shape that cannot be stored is a `422` naming what was wrong.
#[tokio::test]
async fn an_unstorable_block_is_refused_in_the_clients_envelope() {
    let (app, _store) = surface();
    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[],
        &json!({
            "model": "claude-opus-5",
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": [{ "text": "no type here" }] }],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
    let refusal: Value = serde_json::from_str(&text).expect("an error envelope");
    assert_eq!(refusal["error"]["type"], "invalid_request_error");
    assert!(
        refusal["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("type")),
        "the refusal must name the rule it broke: {refusal}"
    );
}

/// F3 CONTROL: an ordinary large body is admitted and answered normally.
///
/// Two sizes, and the second is the finding itself. 1.5 MB was always served;
/// 4 MB is *over* axum's undisclosed 2,097,152-byte default and was refused
/// before the fix — a legitimate resent history, well inside the 32 MB the
/// platform documents, turned away for a limit nobody chose. Kept live beside
/// the probe below so that what the probe proves is specific: not "this router
/// refuses any large body" but "this router refuses exactly the bodies the
/// upstream would".
#[tokio::test]
async fn f3_control_an_ordinary_large_body_is_served_normally() {
    let (app, _store) = surface();
    for size in [1_500_000, 4_000_000] {
        let under = body(&"a".repeat(size));
        let (status, _, text) = post(&app, "/v1/messages", &[], &under).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a well-formed {size}-byte body is inside the documented 32 MB \
             ceiling and must be served: {text}"
        );
    }
}

/// **F3 (M11.1 thermo-nuclear review), fixed and pinned.** The claim:
/// `create_message` and `count_tokens` extracted `body: Bytes` with no
/// `DefaultBodyLimit` override anywhere in the workspace, so axum's implicit
/// 2 MiB cap applied — and a request over it never reached the handler at all.
/// The client got axum's own plain-text 413 ("Failed to buffer the request
/// body: length limit exceeded"), not this dialect's JSON envelope: no
/// `"type":"error"`, no `roundhouse_code`. `error_kind`'s own
/// `PAYLOAD_TOO_LARGE => "request_too_large"` row was unreachable in
/// production, exercised only by the unit test that calls `error_kind` as a
/// pure function. Ruled **valid** on both halves — the wrong limit *and* the
/// wrong envelope — and both are fixed: the routes carry
/// `DefaultBodyLimit::max(MAX_REQUEST_BYTES)` at the platform's documented
/// 32 MB (`research/claude-code-client-surface.md` §3.6), and the `Bytes`
/// rejection is translated by the `RequestBody` extractor into a refusal
/// `MessagesError` renders.
///
/// PROBE: a well-formed, otherwise-legitimate request that crosses the *new*
/// ceiling — the control above is the same shape under it. What is asserted is
/// the envelope as much as the status: a client that cannot parse a refusal
/// treats it as an unparseable stream, and this dialect's client answers that
/// by re-issuing the whole turn (§3.6).
#[tokio::test]
async fn f3_an_oversized_body_is_refused_in_the_clients_envelope() {
    let (app, _store) = surface();

    // Over the 32 MB ceiling by a comfortable margin, and legitimate in every
    // other way: the finding is about *where* the limit is and what a client
    // is told when it crosses it, not about malformed input.
    let over = body(&"a".repeat(34_000_000));
    let (status, headers, text) = post(&app, "/v1/messages", &[], &over).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{text}");
    assert!(
        headers
            .get(CONTENT_TYPE)
            .is_some_and(|value| value.as_bytes().starts_with(b"application/json")),
        "the refusal must be served as this dialect's JSON envelope, not axum's \
         raw rejection: content-type was {:?}, body was {text:?}",
        headers.get(CONTENT_TYPE)
    );
    let refusal: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("an error envelope: {error}\n\nraw body: {text}"));
    assert_eq!(refusal["type"], "error", "{refusal}");
    assert_eq!(refusal["error"]["type"], "request_too_large", "{refusal}");
    assert!(
        refusal["error"]["roundhouse_code"].is_string(),
        "every refusal on this path carries roundhouse's own code: {refusal}"
    );

    // Both routes, because the limit is a property of the router rather than of
    // a handler — and because `count_tokens` is the endpoint a client calls to
    // find out whether a body is too big. One that answered a plain-text 413
    // there would send the client to the path that spends money instead.
    let (status, _, text) = post(&app, "/v1/messages/count_tokens", &[], &over).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{text}");
    let refusal: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("an error envelope: {error}\n\nraw body: {text}"));
    assert_eq!(refusal["error"]["type"], "request_too_large", "{refusal}");
}

// ---------------------------------------------------------------------------
// The forwarded seat
// ---------------------------------------------------------------------------

/// **A seat is captured only when the turn key rode the dedicated header.**
///
/// The rule belongs to `ControlPlane::turn_admission` and is shared by every
/// surface; what this asserts is that *this* surface's header set does not
/// disturb it. That is a real question rather than a formality: a Messages
/// request carries `x-claude-code-session-id`, `anthropic-version` and ten
/// `x-stainless-*` headers that no other surface sees, and the capture walks the
/// header map. The property is one-directional and both directions are asserted,
/// because "no seat was captured" is trivially true of an implementation that
/// captures nothing.
fn a_seat_rides_only_beside_a_dedicated_turn_key(line: &CapturedLine) {
    let plane = ControlPlane::configured(
        ControlPlaneConfig::from_json(
            &json!({
                "projects": [{ "id": "seat", "credentials": { "mode": "pass_through" } }],
                "users": [{ "id": "ada" }],
                "keys": [{
                    "project": "seat", "user": "ada",
                    "key_sha256": sha256_hex(&key("seat")),
                }],
            })
            .to_string(),
            "messages pass-through fixture",
        )
        .expect("the fixture config must validate"),
    );

    let mut with_seat = client_headers(line);
    with_seat.insert(
        HeaderName::from_static("x-roundhouse-key"),
        HeaderValue::from_str(&key("seat")).expect("a header value"),
    );
    with_seat.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer sk-ant-oat01-not-a-real-seat"),
    );
    let admitted = plane
        .turn_admission(&with_seat)
        .expect("a well-formed turn key is admitted");
    // `reaches` and not `is_forwarding`: the latter is a property of the
    // *project's mode* and is true of every turn under it, captured seat or
    // none. What is being asserted is that a credential was actually taken off
    // this request, which under a forwarding resolution is exactly what makes a
    // hosted provider reachable at all.
    assert!(
        admitted.credentials.reaches("anthropic"),
        "the seat beside a dedicated turn key must be captured"
    );

    // The other direction: the same key in `Authorization` is the roundhouse
    // secret itself, and forwarding it upstream would send our own credential to
    // a provider.
    let mut key_only = client_headers(line);
    key_only.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", key("seat"))).expect("a header value"),
    );
    let admitted = plane
        .turn_admission(&key_only)
        .expect("a well-formed turn key is admitted");
    assert!(
        !admitted.credentials.reaches("anthropic"),
        "roundhouse's own turn key must never be forwarded as a seat"
    );
}
per_line_tests!(fn a_seat_rides_only_beside_a_dedicated_turn_key);

/// **The launcher's `ANTHROPIC_API_KEY` sentinel is served, and is inert.**
///
/// R-B's serve-side half. `claude_launch` puts
/// [`ROUNDHOUSE_API_KEY_SENTINEL`] in a launched client's `ANTHROPIC_API_KEY`
/// so the client's auth resolution is a property of the launch rather than of
/// whoever last ran `claude` on that machine (§1.3) — and a client that resolves
/// that variable sends the value on `x-api-key`, which is a *credential* header
/// on this dialect's allowlist row. So the sentinel is only safe to set if this
/// surface both serves the turn and refuses to pass the value on.
///
/// Driven over the real captured header set rather than a hand-built map, for
/// the reason [`client_headers`] gives one test up: the sentinel arrives among
/// twenty other headers no other surface sees, and the capture is the only
/// statement of what those are that cannot drift from the client.
async fn the_launchers_api_key_sentinel_is_served_and_never_becomes_a_seat(line: &CapturedLine) {
    let plane = Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(
            &json!({
                "projects": [{ "id": "seat", "credentials": { "mode": "pass_through" } }],
                "users": [{ "id": "ada" }],
                "keys": [{
                    "project": "seat", "user": "ada",
                    "key_sha256": sha256_hex(&key("seat")),
                }],
            })
            .to_string(),
            "messages sentinel fixture",
        )
        .expect("the fixture config must validate"),
    ));

    // Served: the whole surface, not just the admission call. A rule that only
    // held at `turn_admission` would still leave a launched client refused by
    // the router for a header it was told to send.
    let store = Arc::new(MemoryStore::new());
    let app = messages_router(
        Arc::clone(&plane),
        engine(Arc::clone(&store)),
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );
    let launched = [
        ("x-roundhouse-key", &*key("seat")),
        ("x-api-key", ROUNDHOUSE_API_KEY_SENTINEL),
    ];
    let (status, _, text) = post(&app, "/v1/messages", &launched, &body("hello")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a launched client's own header set must be served: {text}"
    );
    audit(&text).unwrap_or_else(|error| panic!("the stream is not conformant: {error}\n\n{text}"));

    // Inert, in both directions, against the real header set.
    let mut with_sentinel = client_headers(line);
    with_sentinel.insert(
        HeaderName::from_static("x-roundhouse-key"),
        HeaderValue::from_str(&key("seat")).expect("a header value"),
    );
    with_sentinel.insert(
        HeaderName::from_static("x-api-key"),
        HeaderValue::from_static(ROUNDHOUSE_API_KEY_SENTINEL),
    );
    let admitted = plane
        .turn_admission(&with_sentinel)
        .expect("the dedicated header authenticates the turn key");
    assert!(
        !admitted.credentials.reaches("anthropic"),
        "the sentinel authenticates nothing and must make no provider reachable"
    );

    // And the sharp case, which is what a chained Relay makes reachable
    // without the client changing at all: Relay forwards an inbound
    // `x-api-key` untouched while injecting its own `Authorization`, so the
    // sentinel can arrive beside a real bearer. The bearer is the caller's
    // credential and still forwards; the sentinel must not ride with it,
    // because Anthropic answers a bad `x-api-key` next to a valid bearer
    // with a `401` an operator reads as a revoked login.
    let mut beside_a_seat = with_sentinel.clone();
    beside_a_seat.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer sk-ant-oat01-not-a-real-seat"),
    );
    let admitted = plane
        .turn_admission(&beside_a_seat)
        .expect("the dedicated header authenticates the turn key");
    let forwarded = admitted
        .credentials
        .access("anthropic")
        .and_then(|access| access.credential.forwarded().cloned())
        .expect("a real seat beside the sentinel is still a captured credential");
    let names: Vec<&str> = forwarded.headers().map(|(name, _)| name).collect();
    assert!(
        names.contains(&"authorization"),
        "the caller's own bearer is still forwarded: {names:?}"
    );
    assert!(
        !forwarded
            .headers()
            .any(|(_, value)| value == ROUNDHOUSE_API_KEY_SENTINEL),
        "a value roundhouse generated must never reach an upstream: {names:?}"
    );
}
per_line_tests!(async the_launchers_api_key_sentinel_is_served_and_never_becomes_a_seat);

/// The header set an inference request of this line actually carries.
///
/// **Read out of the capture rather than transcribed from it.** The earlier
/// spelling of this helper listed seven headers by hand "in spirit", which is
/// the shape of assertion that keeps passing after the thing it describes has
/// moved: the two captures' `anthropic-beta` lists differ by two values, and a
/// hand-written map would have asserted the seat rule against a header set no
/// client sends.
fn client_headers(line: &CapturedLine) -> HeaderMap {
    let recorded: Value = serde_json::from_str(line.headers).expect("the headers fixture is JSON");
    let recorded = recorded[0]["headers"]
        .as_object()
        .expect("each record carries a header map");
    let mut headers = HeaderMap::new();
    for (name, value) in recorded {
        // `content-length` and `host` belong to the hop that was captured, not
        // to the client's own header set, and re-asserting them here would be
        // asserting about the mock's socket.
        //
        // `x-api-key` is dropped for a different and sharper reason: it is a
        // credential header Anthropic's row admits, and the capture holds a
        // redaction placeholder in it. Leaving it in would hand every direction
        // of the test below a third credential nobody put there, so both arms
        // would pass on a seat the test never chose. What the client's own
        // `x-api-key` should be under a roundhouse launch is R-B's question and
        // is asserted where the launcher's own tests are.
        if name == "content-length" || name == "host" || name == "x-api-key" {
            continue;
        }
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).expect("a captured header name"),
            HeaderValue::from_str(value.as_str().expect("a captured header value"))
                .expect("a captured header value"),
        );
    }
    assert!(
        headers.contains_key("anthropic-beta") && headers.contains_key("x-claude-code-session-id"),
        "the capture must actually carry this dialect's header set"
    );
    headers
}

// ---------------------------------------------------------------------------
// The captured client bodies
// ---------------------------------------------------------------------------

/// **The shipping client's body canonicalizes, block by block.**
///
/// The whole request as each line sends it: three system blocks, a two-block
/// user message, a mid-conversation `system` message, the client's whole
/// toolbox, `context_management`, `thinking`, `output_config`. Everything but
/// `system` and `messages` is accepted and ignored, and the assertion that
/// matters is the one about what a *stored prefix* looks like — because that is
/// what every later turn is checked against.
///
/// Both pinned lines, and the version literal comes from the line rather than
/// from the test: the attribution block's *shape* (block 0, uncached, first) is
/// what this pins, and the only thing that moved between the two captures is the
/// number inside it (§5.7).
fn the_shipping_clients_body_becomes_the_prefix_it_will_be_checked_against(line: &CapturedLine) {
    let params = parse(line.turn_one);
    let items = canonicalize(&params).expect("the live client's body must be servable");

    assert_eq!(
        items.len(),
        6,
        "three system blocks, two user blocks and the mid-conversation \
         system message: {:#?}",
        items.iter().map(|item| item.role).collect::<Vec<_>>()
    );
    // Block 0 of `system` is the attribution pseudo-header, stored as ordinary
    // prefix with no special case (§5.5 ¶5). It is stable per conversation, so
    // its stability is the client's to keep and a server stripping it would be
    // guessing at which parts of a system prompt matter.
    assert_eq!(items[0].role, Role::Developer);
    let attribution = format!("x-anthropic-billing-header: cc_version={}", line.version);
    assert!(
        matches!(&items[0].content, ItemContent::Text { text }
            if text.starts_with(&attribution)),
        "{:?}",
        items[0].content
    );
    // **The leading run of `system` blocks is turn configuration, and carries
    // `Role::Developer` to say so** (M11.1 review, F7). An interior system
    // message is not: it happened at a position both sides agree on, so it is
    // history and keeps `Role::System`. The split is decided once, here, by
    // position — everything downstream reads the role rather than re-deriving
    // the boundary, because a run of identical-looking system items is not
    // splittable by any later reader.
    assert_eq!(
        items.iter().map(|item| item.role).collect::<Vec<_>>(),
        vec![
            Role::Developer,
            Role::Developer,
            Role::Developer,
            Role::User,
            Role::User,
            // The `mid-conversation-system-2026-04-07` beta's message. Refusing
            // this — which the first reading of this surface did — 422s every
            // request the current client line makes; and treating it as
            // configuration would take a message out of the history both sides
            // are checked against.
            Role::System,
        ]
    );
    // The `cache_control` breakpoints on system blocks 1 and 2 leave no trace:
    // roundhouse places its own from the segment boundaries it knows, and
    // keeping the client's would let it name a prefix boundary in a prompt it
    // does not assemble.
    assert!(
        items
            .iter()
            .all(|item| matches!(item.content, ItemContent::Text { .. })),
        "{items:#?}"
    );
    // The session name the client gave, in the shape it gives it in.
    assert_eq!(
        session_key(&HeaderMap::new(), &params),
        Some(line.named()),
        "the capture's `metadata.user_id` is a JSON object string, and the \
         name it yields lives in this dialect's own namespace (F6)"
    );
}
per_line_tests!(fn the_shipping_clients_body_becomes_the_prefix_it_will_be_checked_against);

/// **Two real turns of one conversation, and the one item that moved.**
///
/// The `--continue` body resends the whole history, so the first six items ought
/// to be the prefix the session already holds. Five of them are — including the
/// mid-conversation system message, which the client sends as a **one-block list
/// on turn one and as a bare string on the resend**, and which must therefore
/// canonicalize identically or the session forks at item 5 on every second turn.
/// That is the property this fixture pair was captured to prove.
///
/// The sixth is a genuine divergence and it is recorded rather than papered
/// over: the client rebuilt its own system prompt between the two turns (the
/// model-identity line changed as the 1M-context variant dropped out of the beta
/// header), so this pair really does fork. The fork is correct behaviour on a
/// rewritten prompt; what would be wrong is forking for the *other* five items,
/// and that is what the equality below rules out.
///
/// **Both lines, and the count means what it says** (R-A). The number two here
/// is not a shape the client happens to have: it is *the answer and the new
/// question*, the only two things a `--continue` adds to a conversation. The
/// current line posts a third new `messages` item on every `--continue` — the
/// remaining-budget notice, which it rewrites per request — and if that were
/// admitted as history this assertion would read three for one line and two for
/// the other, which is the shape §5.7 found and mistook for arithmetic. It is
/// not arithmetic: an ephemeral notice is not a thing anyone said, so it never
/// becomes an item, and the count is two on every line that ever ships.
fn the_shipping_clients_two_turns_are_one_conversation_but_for_the_prompt_it_changed(
    line: &CapturedLine,
) {
    let first = canonicalize(&parse(line.turn_one)).expect("turn one is servable");
    let second = canonicalize(&parse(line.turn_two)).expect("turn two is servable");

    assert_eq!(
        second.len(),
        first.len() + 2,
        "the answer and the new question, and nothing else a \
         `--continue` happens to carry: {second:#?}"
    );
    let diverged: Vec<usize> = first
        .iter()
        .zip(&second)
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        diverged,
        vec![2],
        "only the system prompt the client itself rewrote may differ: \
         {diverged:?}"
    );
    assert_eq!(
        first[5], second[5],
        "the mid-conversation system message is a block list on turn \
         one and a string on the resend; if those canonicalize differently, \
         every second turn of every session forks and every turn still \
         answers"
    );
    assert_eq!(
        second[6],
        Item {
            role: Role::Assistant,
            content: ItemContent::Text {
                text: "MOCKED".to_string()
            },
            response_id: None,
        },
        "the client replays the assistant's reply verbatim, unstamped"
    );

    // And both turns name the same session, which is what makes the prefix
    // check reach the same log at all.
    assert_eq!(
        session_key(&HeaderMap::new(), &parse(line.turn_one)),
        session_key(&HeaderMap::new(), &parse(line.turn_two)),
    );
}
per_line_tests!(fn the_shipping_clients_two_turns_are_one_conversation_but_for_the_prompt_it_changed);

/// **F6 (M11.2b review), closed: each pinned line is its own reported test.**
///
/// The finding was that `for line in LINES` made the two lines one test, and
/// so a panic on 2.1.251 unwound before 2.1.257's own checks ever ran — the
/// shipping client's result hidden behind the older line's failure — while no
/// filter could select one line to look at. [`per_line_tests`] replaced the
/// loop with generation.
///
/// What makes that a fix rather than a preference is that libtest now *has*
/// two entries where it had one, so it schedules, runs and reports each
/// independently. That is observed here rather than argued: the running test
/// binary is asked for its own list (`--list` enumerates without running
/// anything), and every parameterized body must appear once per line. A test
/// that instead re-modelled independent execution in-process — two
/// `catch_unwind`s in a row — would pass identically against the loop this
/// replaced, and would be pinning the model rather than the suite.
#[test]
fn f6_every_pinned_line_is_its_own_reported_test() {
    let listing = std::process::Command::new(
        std::env::current_exe().expect("a running test knows its own binary"),
    )
    .args(["--list", "--format=terse"])
    .output()
    .expect("the test binary enumerates its own tests");
    let listing = String::from_utf8(listing.stdout).expect("libtest's listing is UTF-8");

    let named_for = |suffix: &str| -> Vec<String> {
        let suffix = format!("::{suffix}: test");
        let mut names: Vec<String> = listing
            .lines()
            .filter_map(|entry| entry.strip_suffix(&suffix))
            .map(str::to_string)
            .collect();
        names.sort();
        names
    };
    let prior = named_for("line_2_1_251");
    let current = named_for("line_2_1_257");

    assert!(
        prior.len() >= 14,
        "every fixture-driven test is parameterized over both lines, and there \
         are at least fourteen of them; the listing found {}: {prior:#?}",
        prior.len()
    );
    assert_eq!(
        prior, current,
        "a body reported for one line and not the other is a line that stopped \
         being checked — the failure mode the loop had, arriving a different way"
    );
    assert!(
        prior.contains(
            &"f7_the_live_continue_pair_continues_across_ordinary_system_volatility".to_string()
        ),
        "and the names are the bodies' own, so `cargo test <body>` runs both \
         lines and `cargo test line_2_1_257` runs the shipping client's: \
         {prior:#?}"
    );
}

/// **F7: replaying the two live turns through the running server continues one
/// conversation across ordinary system-prompt volatility.**
///
/// [`the_shipping_clients_two_turns_are_one_conversation_but_for_the_prompt_it_changed`]
/// proves `canonicalize()` disagrees at item 2 only, between two consecutive
/// real turns of what is, from the user's point of view, one `--continue`d
/// conversation. Nothing before this test drove both fixtures through the
/// *running* router to see what `bind_prefix` does with that one-item
/// disagreement — this does. `ScriptedFrontierClient` is primed to answer
/// `"MOCKED"`, exactly the text turn two's own fixture replays verbatim as
/// history, so the *only* disagreement between the two turns really is the one
/// line this test is about (the model-identity line the CLI itself rewrote as
/// `context-1m-2025-08-07` dropped out of the beta header, §5.6 addendum 2) —
/// nothing here rests on the test double answering differently than the live
/// capture rig did.
///
/// **What used to happen, and why it was the finding.** Every item was admitted
/// under one strict rule, so item 2's rewritten line forked the session to a
/// fresh generation: cold routing history and, per `conversations.rs`'s own
/// `fork()` doc, a silently orphaned MCP `scope=session` narrowing — on
/// precisely the turn a warm prefix would first have paid off. The trigger
/// recurs for the life of every session on an unpredictable cadence (the date,
/// cwd, git branch, any beta flag, an overnight client self-update, §5.6), so
/// the warm-prefix thesis this surface exists to serve did not survive contact
/// with the shipping client.
///
/// What happens now: the leading system run is turn configuration, so a resend
/// that rewrote it *replaces* it and continues — while the conversation itself
/// (the two user blocks, the mid-conversation system message, the assistant
/// reply) is still admitted strictly and still forks on a real edit, which is
/// what [`a_divergent_resend_forks_rather_than_merging`] pins.
///
/// The third turn is not decoration. It is what proves the replacement is a
/// *stable* projection rather than a one-off tolerance: turn three is checked
/// against a session whose configuration run was rewritten in place, and it has
/// to agree with it.
async fn f7_the_live_continue_pair_continues_across_ordinary_system_volatility(
    line: &CapturedLine,
) {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(ScriptedFrontierClient::new("MOCKED"));
    let app = messages_router(
        ControlPlane::open(),
        engine_scripted(Arc::clone(&store), Arc::clone(&client)),
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );

    let mut turn_one = fixture(line.turn_one);
    turn_one["stream"] = json!(true);
    stream(&app, &[], &turn_one).await;

    let session = line.named();
    let session = session.as_str();
    let after_turn_one = stored_items(&store, session).await;
    assert_eq!(
        after_turn_one.len(),
        7,
        "turn one's own six canonicalized items plus its answer: \
         {after_turn_one:#?}"
    );

    let mut turn_two = fixture(line.turn_two);
    turn_two["stream"] = json!(true);
    stream(&app, &[], &turn_two).await;

    // What the product's own value proposition requires: a `--continue` naming
    // the same session id appends its new question and answer to the
    // conversation the user believes is one conversation.
    //
    // Twelve raw appends, not nine: the three-block configuration run is
    // *re-recorded* because it changed, and the log is append-only — the
    // replacement happens in the projection, not by rewriting what was
    // committed. The control below, where nothing about the configuration
    // moved, records nothing extra and lands on nine.
    let continued = stored_items(&store, session).await;
    assert_eq!(
        continued.len(),
        12,
        "turn two must extend the session it named, not silently orphan it: \
         {continued:#?}"
    );
    assert!(
        no_such_session(&store, &format!("{session}#g1")).await,
        "and it must not have landed in a freshly forked generation instead"
    );

    // Turn three: the client's own next `--continue`, carrying turn two's
    // configuration unchanged, the answer it was just given, and a new
    // question. It is checked against a session whose configuration was
    // replaced in place, which is the property a one-turn test cannot see.
    let mut turn_three = turn_two.clone();
    let messages = turn_three["messages"]
        .as_array_mut()
        .expect("the fixture's `messages` is a list");
    messages
        .push(json!({ "role": "assistant", "content": [{ "type": "text", "text": "MOCKED" }] }));
    messages.push(json!({ "role": "user", "content": "and what about the other one?" }));
    stream(&app, &[], &turn_three).await;

    let after_turn_three = stored_items(&store, session).await;
    assert_eq!(
        after_turn_three.len(),
        14,
        "the new question and its answer, and nothing re-recorded: turn \
         three's configuration is the one already stored: {after_turn_three:#?}"
    );
    assert!(
        no_such_session(&store, &format!("{session}#g1")).await,
        "a replaced configuration run must be a stable projection, not a \
         tolerance that expires on the next turn"
    );

    // And the replacement is a replacement: what the session holds at the head
    // is turn two's rewritten block, exactly once.
    let rewritten = turn_two["system"][2]["text"]
        .as_str()
        .expect("the fixture's system block 2 is text");
    let superseded = turn_one["system"][2]["text"]
        .as_str()
        .expect("the fixture's system block 2 is text");
    assert_ne!(rewritten, superseded, "control: the fixtures still differ");
}
per_line_tests!(async f7_the_live_continue_pair_continues_across_ordinary_system_volatility);

/// CONTROL for the F7 probe above: neutralize the one line that differs
/// between the two live captures (patch turn two's `system[2]` back to turn
/// one's own text, undoing exactly the `context-1m-2025-08-07` beta drift)
/// and the same replay must NOT fork.
///
/// What this rules out: that the probe above fails because of something else
/// in the harness — a stray whitespace byte in how `serde_json` round-trips
/// the fixture, the `ScriptedFrontierClient` double, the router wiring — and
/// not because of the one line the probe's doc names. If this control ever
/// starts failing too, the probe's failure has stopped being about system-
/// prompt volatility specifically and the F7 finding needs re-reading before
/// anyone trusts it.
async fn f7_control_the_same_pair_does_not_fork_once_the_one_line_is_neutralized(
    line: &CapturedLine,
) {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(ScriptedFrontierClient::new("MOCKED"));
    let app = messages_router(
        ControlPlane::open(),
        engine_scripted(Arc::clone(&store), Arc::clone(&client)),
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );

    let mut turn_one = fixture(line.turn_one);
    turn_one["stream"] = json!(true);
    stream(&app, &[], &turn_one).await;

    let mut turn_two = fixture(line.turn_two);
    turn_two["stream"] = json!(true);
    // The only edit: item 2 of `system` reset to turn one's own words, so
    // every byte `suffix_after` compares now agrees.
    turn_two["system"][2]["text"] = turn_one["system"][2]["text"].clone();
    stream(&app, &[], &turn_two).await;

    let continued = stored_items(&store, &line.named()).await;
    assert_eq!(
        continued.len(),
        9,
        "with the one volatile line neutralized the configuration run \
         is the one already stored, so nothing is re-recorded and the \
         session grows by exactly the new question and its answer: \
         {continued:#?}"
    );
}
per_line_tests!(async f7_control_the_same_pair_does_not_fork_once_the_one_line_is_neutralized);

// ---------------------------------------------------------------------------
// R-A: the current line's remaining-budget notice
// ---------------------------------------------------------------------------

/// CONTROL for R-A: the third captured turn really does carry the notice twice,
/// in the two different shapes the ruling is about.
///
/// Without this, the two probes below could pass against a fixture that had
/// quietly lost the notice — which is the one way "the session did not fork" is
/// true for the wrong reason. Asserted on the raw fixture rather than on
/// canonicalized items, because what it is checking is what the *client* sent.
#[test]
fn control_the_third_captured_turn_carries_the_budget_notice_twice() {
    let turn_three = fixture(TURN_THREE_CURRENT);
    let messages = turn_three["messages"]
        .as_array()
        .expect("the fixture's `messages` is a list");
    assert_eq!(
        messages.len(),
        8,
        "the capture is turn three of one conversation: {messages:#?}"
    );

    // Resent in history, flattened to a bare string — a string container cannot
    // carry the `cache_control` breakpoint it had when it was the newest item.
    assert_eq!(messages[4]["role"], json!("system"));
    assert!(
        messages[4]["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("<total_tokens>")),
        "turn two's notice, resent flattened: {}",
        messages[4]
    );
    // Appended fresh at the end, a one-block list carrying the breakpoint
    // forward.
    assert_eq!(messages[7]["role"], json!("system"));
    assert_eq!(
        messages[7]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(
        messages[4]["content"], messages[7]["content"][0]["text"],
        "and the two spell the same budget, which is what makes the probe below \
         about the *shape* rather than about a number that moved"
    );

    // And the prior line sends none at all, which is why R-A is a property of
    // the current line and not of the dialect.
    assert!(
        !LINE_PRIOR.turn_two.contains("<total_tokens>"),
        "the prior line's `--continue` carries no budget notice"
    );
}

/// **R-A: three real turns of the current client line are one conversation, and
/// the budget notice is not one of its items.**
///
/// The fixture is a genuine third turn — captured by resuming the very session
/// [`LINE_CURRENT`]'s two turns built, so its resent history is turn two's bytes
/// and not a reconstruction (§5.7.1). That matters because turn three is where
/// the notice first appears twice: flattened in the history and fresh at the
/// end.
///
/// **What would go wrong if it were history.** The notice is a counter the
/// client recomputes per request. Admitted as an ordinary item it would be
/// stored on turn two, resent on turn three, and agree — until `N` moved, which
/// is the only thing a counter does; then the resend would disagree with the
/// stored copy at a position no client can edit, the session would fork to a
/// cold generation, and every turn would still answer. That is the same silent
/// failure F7 was about, arriving through a message rather than through a system
/// prompt — and it could not be fixed the same way, because turn configuration
/// is the *leading* run by construction and this item is trailing.
///
/// Both the fork and the count are asserted, because either alone is passable by
/// an implementation that has the other wrong.
#[tokio::test]
async fn r_a_three_real_turns_are_one_conversation_and_the_notice_is_not_an_item() {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(ScriptedFrontierClient::new("MOCKED"));
    let app = messages_router(
        ControlPlane::open(),
        engine_scripted(Arc::clone(&store), Arc::clone(&client)),
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );
    let session = LINE_CURRENT.named();

    for (turn, body) in [
        LINE_CURRENT.turn_one,
        LINE_CURRENT.turn_two,
        TURN_THREE_CURRENT,
    ]
    .into_iter()
    .enumerate()
    {
        let mut body = fixture(body);
        body["stream"] = json!(true);
        stream(&app, &[], &body).await;
        assert!(
            no_such_session(&store, &format!("{session}#g1")).await,
            "turn {} forked the session: the notice is the only thing that \
             moved between these three real turns",
            turn + 1
        );
    }

    // Seven for turn one (six items and its answer), five more for turn two (the
    // configuration run the client itself rewrote, re-recorded, plus the new
    // question and its answer), and exactly two for turn three, whose
    // configuration is the one already stored. The notice contributes nothing at
    // any point, which is what "not a log item" means in the only place it can
    // be observed.
    let items = stored_items(&store, &session).await;
    assert_eq!(items.len(), 14, "{items:#?}");
    assert!(
        !items.iter().any(|item| matches!(
            &item.content,
            ItemContent::Text { text } if is_budget_notice(text)
        )),
        "no turn's budget notice may be in the log: {items:#?}"
    );

    // CONTROL for the assertion above: the *system prompt* ends with the same
    // tag and is still in the log, because it is configuration and configuration
    // is replaced rather than dropped. A rule that matched on the tag anywhere
    // would have deleted a multi-KB system prompt and passed the line above.
    assert!(
        items.iter().any(|item| matches!(
            &item.content,
            ItemContent::Text { text }
                if text.contains("<total_tokens>")
                    && text.contains("interactive agent that helps users")
        )),
        "the client's own environment block quotes the same tag and is real \
         configuration: {items:#?}"
    );
}

/// F5 (M11.2b review), closed: the assertion above asks the *one* recognizer,
/// [`is_budget_notice`], rather than re-spelling it as
/// `text.contains("<total_tokens>") && text.len() < 200`. The length half was
/// the drift: a real notice at or above 200 bytes — a plausible `N`, or a
/// client that pads the wrapper — failed it, so the negative assertion would
/// have gone on passing while no longer looking for anything.
///
/// Kept live as the pin on that: the recognizer must accept a notice the old
/// heuristic rejected, and must still refuse the environment block that merely
/// ends with the same tag (the reason the heuristic existed at all).
#[test]
fn the_one_recognizer_accepts_a_notice_the_length_heuristic_missed() {
    let long_real_notice = format!("<total_tokens>{}</total_tokens>", "9".repeat(200));
    assert!(
        long_real_notice.len() >= 200,
        "sanity: the fixture must actually exceed the old heuristic's threshold"
    );
    assert!(
        is_budget_notice(&long_real_notice),
        "F5: wire's tag-anchor rule does not care about length, and this is the          notice: {long_real_notice}"
    );
    assert!(
        !is_budget_notice(&format!(
            "<env>You are an interactive agent</env>\n<total_tokens>1 left</total_tokens>"
        )),
        "and it still refuses the environment block that merely ends with the tag"
    );
}

/// **R-A's failure mode, made to happen: the budget moves and the session does
/// not fork.**
///
/// The captures cannot show this on their own — the mock rig answers a constant
/// `usage`, so `N` read 15000000 on every turn of every run, including the one
/// that deliberately varied the reported output tokens (§5.7.1). A test that
/// only replayed them would therefore be green against an implementation that
/// stored the notice as history, and would go red in production on the first
/// session long enough for the client's counter to move.
///
/// So the number is moved here, in both places turn three carries it — the
/// flattened copy in the history and the fresh one at the end — leaving every
/// other byte of the real capture alone. The session must still be one session,
/// and must still grow by exactly the new question and its answer.
#[tokio::test]
async fn r_a_a_budget_that_counted_down_between_turns_does_not_fork_the_session() {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(ScriptedFrontierClient::new("MOCKED"));
    let app = messages_router(
        ControlPlane::open(),
        engine_scripted(Arc::clone(&store), Arc::clone(&client)),
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );
    let session = LINE_CURRENT.named();

    for body in [LINE_CURRENT.turn_one, LINE_CURRENT.turn_two] {
        let mut body = fixture(body);
        body["stream"] = json!(true);
        stream(&app, &[], &body).await;
    }
    let after_turn_two = stored_items(&store, &session).await.len();

    let mut turn_three = fixture(TURN_THREE_CURRENT);
    turn_three["stream"] = json!(true);
    const SPENT: &str = "<total_tokens>14812345 tokens left</total_tokens>";
    turn_three["messages"][4]["content"] = json!(SPENT);
    turn_three["messages"][7]["content"][0]["text"] = json!(SPENT);
    stream(&app, &[], &turn_three).await;

    assert!(
        no_such_session(&store, &format!("{session}#g1")).await,
        "a client counting its own budget down must not fork the conversation \
         it is counting"
    );
    let items = stored_items(&store, &session).await;
    assert_eq!(
        items.len(),
        after_turn_two + 2,
        "the new question and its answer, and no re-recorded prefix: {items:#?}"
    );
}

/// The captured body, served — not just parsed.
///
/// The unit above proves canonicalization; this proves the whole path handles
/// a whole real request, including every tool definition this surface ignores
/// and a `thinking` object whose shape changed between 2.1.247 and 2.1.251
/// (`budget_tokens` became `{"type":"adaptive"}`). An accepted-and-ignored
/// field is only accepted if a request carrying it is answered.
async fn the_captured_client_body_is_served_as_a_conformant_stream(line: &CapturedLine) {
    let (app, store) = surface();
    let mut body = fixture(line.turn_one);
    body["stream"] = json!(true);

    let accumulated = stream(&app, &[], &body).await;
    assert_eq!(accumulated.text, ANSWER);
    assert_eq!(accumulated.model, "claude-opus-5");

    let items = stored_items(&store, &line.named()).await;
    assert_eq!(
        items.len(),
        7,
        "the six canonicalized items plus the answer: {:#?}",
        items.iter().map(|item| item.role).collect::<Vec<_>>()
    );
}
per_line_tests!(async the_captured_client_body_is_served_as_a_conformant_stream);

/// CONTROL for F4, live: the captured body really does carry a real system
/// prompt past the attribution header, so the probe below is not vacuously
/// checking a session with no instructions to lose. Item 1 is the agent-SDK
/// identity line and item 2 is the actual multi-KB system prompt — both
/// Developer-role Text (the leading system run is turn configuration, F7), i.e.
/// both shapes `instructions_of` (`validate/brief.rs`) accepts, and both
/// textually distinct from the attribution header at item 0. If this test ever
/// fails, the fixture changed shape and F4's probe needs re-deriving, not just
/// re-running.
fn f4_control_the_captured_body_carries_a_real_system_prompt_past_the_header(line: &CapturedLine) {
    let items =
        canonicalize(&parse(line.turn_one)).expect("the live client's body must be servable");

    assert_eq!(items[1].role, Role::Developer);
    assert!(
        matches!(&items[1].content, ItemContent::Text { text }
            if text.contains("Claude Agent SDK")),
        "control: item 1 should be the agent-SDK identity line: {:?}",
        items[1].content
    );
    assert_eq!(items[2].role, Role::Developer);
    assert!(
        matches!(&items[2].content, ItemContent::Text { text }
            if text.contains("interactive agent that helps users with software engineering")),
        "control: item 2 should be the real system prompt: {:?}",
        items[2].content
    );
}
per_line_tests!(fn f4_control_the_captured_body_carries_a_real_system_prompt_past_the_header);

/// **F4: the judge is briefed on the whole instruction block, not on its first
/// block.**
///
/// The finding: `instructions_of` (`validate/brief.rs`) took the *first*
/// system/developer text item as "the session's instructions", which for every
/// Claude Code Messages session is the ~70-byte billing attribution
/// pseudo-header — so every drift, no-progress and steer verdict was decided
/// against billing metadata rather than against the task.
///
/// The fix is client-agnostic on purpose: the leading run is concatenated
/// oldest first and the pre-existing instruction budget does the bounding.
/// Nothing here knows what an attribution header looks like, because a rule
/// that recognised one would break the next time the client re-orders its
/// blocks or another client ships a different preamble. So this test asserts
/// *what the judge can now see* — the real prompt — rather than the absence of
/// the header, which the honest fix does not remove: the header is genuinely
/// part of what the client sent, it is small, and it costs the budget almost
/// nothing.
///
/// Driven through the real `wire::canonicalize` on each line's real captured
/// body and the real `ValidationBrief::build`, matching
/// `validate::mod::consult`'s call shape exactly (same items, same
/// `Objective::from_items`, same `BriefConfig::default()`), because the
/// finding was about those two functions meeting.
fn f4_the_judge_is_briefed_on_the_whole_leading_instruction_run(line: &CapturedLine) {
    let items =
        canonicalize(&parse(line.turn_one)).expect("the live client's body must be servable");

    let brief = ValidationBrief::build(
        &items,
        Objective::from_items(&items),
        Vec::new(),
        BriefConfig::default(),
    );

    let instructions = brief
        .instructions
        .as_deref()
        .expect("a session with three leading instruction items must produce some instructions");

    assert!(
        instructions.contains("interactive agent that helps users with software engineering"),
        "F4: the judge's `instructions` must reach the real system prompt at item 2 and not \
         stop at the billing attribution header — every drift/steer verdict for this session \
         is otherwise judged against billing metadata, not the task: {instructions:?}"
    );
    // And the run really is a run: the identity line between the header and the
    // prompt is carried too, in the order the client sent it. A fix that
    // skipped to the "real" block would pass the assertion above and still be
    // guessing at which parts of a system prompt matter.
    assert!(
        instructions.contains("Claude Agent SDK"),
        "the whole leading run, oldest first: {instructions:?}"
    );
    // Bounded, as it always was: the budget truncates rather than the reader
    // choosing one block.
    assert_eq!(
        instructions.chars().count(),
        BriefConfig::default().instruction_chars,
        "a multi-KB system prompt still leaves the brief on its existing budget"
    );

    // The mid-conversation system message is *history*, not instructions, and
    // it must not be dragged into the block the judge reads as the task. This
    // is the same boundary prefix admission draws (F7), asserted here so the
    // two cannot drift apart silently.
    let mid_conversation = items
        .last()
        .expect("the fixture ends with the mid-conversation system message");
    assert_eq!(mid_conversation.role, Role::System);
    let ItemContent::Text { text } = &mid_conversation.content else {
        panic!("control: the mid-conversation message is text: {mid_conversation:?}");
    };
    assert!(
        !instructions.contains(text.trim()),
        "an interior system message is history and must stay out of the instructions"
    );
}
per_line_tests!(fn f4_the_judge_is_briefed_on_the_whole_leading_instruction_run);

// ---------------------------------------------------------------------------
// The oracle's own proofs
// ---------------------------------------------------------------------------

/// A well-formed stream, as SSE text.
fn conformant() -> Vec<(&'static str, Value)> {
    vec![
        (
            "message_start",
            json!({ "type": "message_start", "message": {
                "type": "message", "id": "resp_1", "role": "assistant",
                "model": "claude-opus-5", "content": [],
                "usage": { "input_tokens": 900, "output_tokens": 1 },
            }}),
        ),
        (
            "content_block_start",
            json!({ "type": "content_block_start", "index": 0,
                    "content_block": { "type": "text", "text": "" } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "text_delta", "text": "hi" } }),
        ),
        (
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "message_delta",
            json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" },
                    "usage": { "output_tokens": 7 } }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ]
}

fn sse(frames: &[(&str, Value)]) -> String {
    frames
        .iter()
        .map(|(name, payload)| format!("event: {name}\ndata: {payload}\n\n"))
        .collect()
}

/// **The oracle rejects each defect it exists to catch.**
///
/// Without this, every green assertion above is compatible with an oracle that
/// accepts anything — which is the failure mode of a hand-written validator and
/// the reason the Responses surface borrows Codex's parser instead. Each case
/// below is one line changed from [`conformant`], and each is a defect that
/// costs something specific: a re-issued turn at full price, a thrown
/// accumulator, or a turn billed as free.
#[test]
fn the_oracle_is_not_a_rubber_stamp() {
    // CONTROL first: the unmodified stream passes, so every rejection below is
    // about the one thing that changed.
    let good = audit(&sse(&conformant())).expect("the conformant stream must pass");
    assert_eq!(good.text, "hi");
    assert_eq!(good.usage.input_tokens, 900);
    assert_eq!(
        good.usage.output_tokens, 7,
        "the terminal frame's count replaces the prelude's `1`"
    );

    // A frame with no `event:` line. Claude Code dispatches on the name, so this
    // frame is dropped in silence and the whole turn is re-issued non-streaming.
    let nameless = sse(&conformant()).replace("event: content_block_delta\n", "");
    assert!(
        audit(&nameless).is_err(),
        "a frame with no `event:` line must be refused"
    );

    // A `data:`-less frame — alive on a direct connection, discarded by a
    // chained Relay's re-encoder.
    let dataless = format!("{}event: ping\n\n", sse(&conformant()));
    assert!(audit(&dataless).is_err(), "a frame with no `data:` line");

    // The name and the payload disagreeing. Two readers then understand one
    // frame differently: the client believes the line, our own dispatch decoder
    // believes the payload.
    let mismatched = sse(&conformant()).replace("event: message_stop", "event: message_delta");
    assert!(audit(&mismatched).is_err(), "a lying `event:` line");

    // A delta at an index nothing opened: `RangeError("Content block not
    // found")`, a thrown accumulator rather than a dropped frame.
    let mut orphaned = conformant();
    orphaned.remove(1);
    assert!(audit(&sse(&orphaned)).is_err(), "a delta with no start");

    // The most expensive frame this surface could emit. The client merges
    // `output_tokens` with `??`, so an explicit zero overwrites a real count and
    // the turn bills as free.
    let free = sse(&conformant()).replace("\"output_tokens\":7", "\"output_tokens\":0");
    assert!(
        audit(&free).is_err(),
        "an explicit `output_tokens: 0` in a `message_delta`"
    );

    // A `stop_reason` outside the pinned seven.
    let invented = sse(&conformant()).replace("end_turn", "finished_normally");
    assert!(audit(&invented).is_err(), "a stop reason the spec lacks");

    // A usage property nobody publishes — the `adk-anthropic` defect, which
    // reported a cache counter to nobody for a year.
    let extra = sse(&conformant()).replace(
        "\"output_tokens\":7",
        "\"output_tokens\":7,\"cache_creation_input_tokens_1h\":5",
    );
    assert!(audit(&extra).is_err(), "an invented usage property");

    // A stream that completes no content block: the second of the two
    // non-streaming-fallback triggers, and a full extra turn's cost.
    let blockless: Vec<(&str, Value)> = conformant()
        .into_iter()
        .filter(|(name, _)| !name.starts_with("content_block"))
        .collect();
    assert!(
        audit(&sse(&blockless)).is_err(),
        "a stream with no completed content block"
    );

    // Anything after the terminal frame.
    let trailing = format!(
        "{}event: message_delta\ndata: {}\n\n",
        sse(&conformant()),
        json!({ "type": "message_delta", "delta": {} })
    );
    assert!(audit(&trailing).is_err(), "a frame after `message_stop`");

    // A delta whose type disagrees with its block's: `Error("Content block is
    // not a text block")`.
    // (`serde_json` renders object keys in sorted order, so `text` precedes
    // `type` — the substring is written the way the fixture actually serializes
    // rather than the way it is spelled above.)
    let crossed = sse(&conformant()).replace(
        "\"text\":\"hi\",\"type\":\"text_delta\"",
        "\"thinking\":\"hm\",\"type\":\"thinking_delta\"",
    );
    assert_ne!(
        crossed,
        sse(&conformant()),
        "the mutation must have applied"
    );
    assert!(audit(&crossed).is_err(), "a thinking delta on a text block");

    // And two things that must *not* be refused, or the oracle is simply strict
    // rather than correct: a `ping` before the prelude, and an `error` event as
    // a stream's only terminal.
    let mut with_ping = vec![("ping", json!({ "type": "ping" }))];
    with_ping.extend(conformant());
    audit(&sse(&with_ping)).expect("a ping is legal anywhere, including first");
    let failed = vec![
        conformant()[0].clone(),
        (
            "error",
            json!({ "type": "error", "error": {
                "type": "overloaded_error", "message": "try again" } }),
        ),
    ];
    let failed = audit(&sse(&failed)).expect("an error event is a legal terminal on its own");
    assert!(failed.error.is_some());
}

// ---------------------------------------------------------------------------
// A real socket
// ---------------------------------------------------------------------------

/// The same turn over a real connection, with a real chunked body.
///
/// Everything above drives the router as a `tower::Service`, which is enough for
/// the protocol and keeps the tests hermetic — but it skips the parts a socket
/// does not: the chunked transfer encoding, the response headers axum's `Sse`
/// sets, and the fact that the frames arrive as bytes rather than as a
/// pre-assembled body. The client is written by hand here rather than pulled in,
/// because the one thing a borrowed HTTP client would hide is exactly what this
/// test is for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_socket_round_trip() {
    let (app, _store) = surface();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let payload = serde_json::to_vec(&body("hello")).expect("a JSON body");
    let mut socket = tokio::net::TcpStream::connect(addr)
        .await
        .expect("the server is listening");
    // The path the client actually posts to, query and all.
    let request = format!(
        "POST /v1/messages?beta=true HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    socket
        .write_all(request.as_bytes())
        .await
        .expect("write headers");
    socket.write_all(&payload).await.expect("write body");

    let mut raw = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        socket.read_to_end(&mut raw),
    )
    .await
    .expect("the stream must end rather than hang")
    .expect("read");
    let raw = String::from_utf8(raw).expect("HTTP/1.1 responses here are UTF-8");

    let (head, body) = raw.split_once("\r\n\r\n").expect("a complete response");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        head.to_ascii_lowercase().contains("text/event-stream"),
        "the content type is what makes a client stream rather than buffer: {head}"
    );
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(body)
    } else {
        body.to_string()
    };

    let accumulated = audit(&body)
        .unwrap_or_else(|error| panic!("the socket stream is not conformant: {error}\n\n{body}"));
    assert_eq!(accumulated.text, ANSWER);
    // Every frame carried both lines. Asserted here as well as inside the oracle
    // because this is the only path where the framing is produced by the real
    // encoder rather than by a collected body.
    assert!(
        split_frames(&body)
            .iter()
            .all(|frame| frame.name.is_some() && frame.data.is_some()),
        "{body}"
    );
}

/// Undo HTTP/1.1 chunked transfer encoding.
///
/// Written out rather than pulled in for the reason the test above gives: the
/// chunk framing is part of what is under test, so decoding it with the same
/// library that produced it would be a tautology.
fn dechunk(body: &str) -> String {
    let mut rest = body;
    let mut out = String::new();
    while let Some((header, tail)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(header.trim(), 16).expect("a chunk size in hex");
        if size == 0 {
            break;
        }
        out.push_str(&tail[..size]);
        rest = &tail[size + 2..];
    }
    out
}
