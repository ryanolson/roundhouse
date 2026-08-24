// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M10.2: the scorer picks a tier, the recipe picks a target, and a dead
//! provider does not take the turn with it.
//!
//! The unit tests one layer down prove the arithmetic (`routing::stage`) and
//! the classification (`FrontierError::failover_class`). What only an engine
//! can prove is the three joins between them:
//!
//! - the session's **real exchanges** reach the scorer, through the extractor
//!   rather than through a hand-built `ToolSignals` that would pass whatever the
//!   extractor happened to compute;
//! - a failed dispatch **advances** to the next candidate of the same tier,
//!   inside one turn, one grant and one deadline;
//! - and a provider that *answered* — a refusal, a 404, a bad key — does not,
//!   because a second model would answer it the same way and the difference
//!   between those two cases is the whole of R5.
//!
//! Every claim here has a control that varies exactly one thing, because "the
//! second provider served the turn" would also pass on an engine that always
//! dispatched to the second provider.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::StreamExt;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{
    Allocation, Balance, BalanceQuery, Budget, BudgetTerms, BudgetWindow, DEFAULT_WARN_AT,
    Exhaustion, Grant, GrantRequest, MemorySpendLedger, Settled, Settlement, SpendError,
    SpendLedger,
};
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::metrics::{MetricsFold, ModelKey, Scope};
use roundhouse_core::routing::{
    AffinityPolicy, AttemptClass, CacheModel, DecisionSource, PickerMode, ProviderPricing,
    StagePolicy, Target, TierRecipe,
};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierClients, FrontierError, FrontierModelSpec,
    FrontierQuote, FrontierStream, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::{Admission, EchoLocalExecutor, Engine, EngineConfig, LocalExecutor};

mod common;
use common::MINUTE;

// ---------------------------------------------------------------------------
// The fleet: three hosted models on three providers, and scripted transports
// ---------------------------------------------------------------------------

const PRIMARY: &str = "alpha";
const SECONDARY: &str = "beta";
const THRIFTY: &str = "gamma";

fn target(provider: &str) -> Target {
    Target::Frontier {
        provider: provider.into(),
        model: "m".into(),
    }
}

fn spec(provider: &str, quality_prior: f64) -> FrontierModelSpec {
    FrontierModelSpec {
        provider: provider.into(),
        model: "m".into(),
        wire_protocol: WireProtocol::OpenAiResponses,
        cache_model: CacheModel::Deterministic { ttl_ms: 5 * MINUTE },
        pricing: ProviderPricing::free(),
        quality_prior,
        base_ttft_ms: 1.0,
        ttft_ms_per_uncached_token: 0.0,
    }
}

/// The whole hosted catalog these tests route over.
fn catalog() -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![
        spec(PRIMARY, 0.95),
        spec(SECONDARY, 0.90),
        spec(THRIFTY, 0.60),
    ])
}

/// What a scripted transport does when it is asked to serve.
#[derive(Clone)]
enum Script {
    /// Answer, with this text.
    Answer(String),
    /// Fail the way a provider that is not there fails.
    Transport,
    /// Fail with an HTTP status.
    Status(u16),
}

/// A transport that follows a script and counts what it was asked.
///
/// The count is the load-bearing half: "the turn was answered by beta" is also
/// true of an engine that never tried alpha at all, and only alpha's own call
/// count tells the two apart.
struct Scripted {
    script: Script,
    calls: AtomicUsize,
}

impl Scripted {
    fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script,
            calls: AtomicUsize::new(0),
        })
    }

    fn answering(text: &str) -> Arc<Self> {
        Self::new(Script::Answer(text.to_string()))
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl FrontierClient for Scripted {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.script {
            Script::Answer(text) => Ok(FrontierChunk::whole_response(
                text.clone(),
                quote.prompt.len() as u64,
                0,
                text.len() as u64,
                0,
            )),
            Script::Transport => Err(FrontierError::Transport {
                message: "connection refused".into(),
                timed_out: false,
            }),
            Script::Status(status) => Err(FrontierError::Status {
                status: *status,
                message: "the provider said so".into(),
            }),
        }
    }
}

/// A transport whose *stream* fails after the request was accepted.
///
/// The other side of the failover boundary: `execute` succeeded, so nothing
/// here is a candidate for a second target however the body ends.
struct StreamsThenDies;

#[async_trait]
impl FrontierClient for StreamsThenDies {
    async fn execute(&self, _quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        Ok(futures::stream::iter([
            Ok(FrontierChunk::OutputText("half an ans".into())),
            Err(FrontierError::Transport {
                message: "the upstream stream ended early".into(),
                timed_out: false,
            }),
        ])
        .boxed())
    }
}

// ---------------------------------------------------------------------------
// The rig
// ---------------------------------------------------------------------------

/// capable = [alpha, beta], efficient = [gamma].
///
/// Two entries in the capable tier is what makes a *within-tier* failover
/// possible at all; `capable_first` is what puts an unremarkable session there,
/// so a test about failover is not also a test about the scorer.
fn recipe(picker: PickerMode) -> TierRecipe {
    TierRecipe::new(
        vec![format!("{PRIMARY}/m"), format!("{SECONDARY}/m")],
        vec![format!("{THRIFTY}/m")],
        picker,
        roundhouse_core::routing::stage::DEFAULT_CONFIDENCE_THRESHOLD,
    )
    .expect("a two-tier recipe at the shipped threshold")
}

fn admission_with(picker: PickerMode) -> Admission {
    Admission {
        tiers: Some(Arc::new(recipe(picker))),
        ..Admission::open()
    }
}

/// The same admission with a real ceiling, for the one claim that is about the
/// ledger.
///
/// **`Admission::open()` carries no budget, and the engine skips the ledger
/// entirely on that path** — so a grant/settle count taken under it confirms the
/// skip and says nothing about the hold arithmetic, which is the whole of risk
/// 3. The limit is far above anything this catalog can quote (every spec is
/// `ProviderPricing::free()`), so the budget axis cannot reach the routing
/// assertions; what it does is put a real `open_grant`/`settle_grant` pair on
/// the path so their *count* is a claim.
fn budgeted_admission_with(picker: PickerMode) -> Admission {
    Admission {
        budget: Some(BudgetTerms {
            budget: Budget {
                limit_usd: 1_000.0,
                window: BudgetWindow::Total,
                on_exhaustion: Exhaustion::degrade_with_overflow(),
                warn_at: DEFAULT_WARN_AT,
            },
            allocation: Allocation::Pooled,
        }),
        ..admission_with(picker)
    }
}

struct Rig {
    engine: Arc<Engine<MemoryStore, ByteTokenizer>>,
    store: Arc<MemoryStore>,
}

/// An engine over the three-provider catalog, with a scripted transport each.
///
/// No local fleet: every candidate is hosted, so no assertion below can be
/// satisfied by a local worker quietly taking the turn.
fn rig_of(clients: Vec<(&str, Arc<dyn FrontierClient>)>) -> Rig {
    rig_with(clients, None, 5_000)
}

fn rig_with_ledger(
    clients: Vec<(&str, Arc<dyn FrontierClient>)>,
    spend: Option<Arc<dyn SpendLedger>>,
) -> Rig {
    rig_with(clients, spend, 5_000)
}

/// The same fleet under a router of the caller's choosing.
///
/// One test below needs an engine composed the way a process that booted
/// *before* any project had a recipe is composed — the plain policy, no stage
/// wrapper — because the claim is about what happens when a recipe reaches a
/// router that cannot read it. Every other rig here is the stage router, which
/// is what `rig_with` still hands out.
fn rig_over_policy(
    clients: Vec<(&str, Arc<dyn FrontierClient>)>,
    policy: Arc<dyn roundhouse_core::routing::RoutingPolicy>,
) -> Rig {
    rig_inner(clients, None, 5_000, policy)
}

fn rig_with(
    clients: Vec<(&str, Arc<dyn FrontierClient>)>,
    spend: Option<Arc<dyn SpendLedger>>,
    turn_deadline_ms: u64,
) -> Rig {
    rig_inner(
        clients,
        spend,
        turn_deadline_ms,
        // The stage router over the ordinary policy, which is how a deployment
        // that has any project with a recipe composes it. Every assertion about
        // a project *without* one is an assertion about this same object.
        Arc::new(StagePolicy::new(Box::new(AffinityPolicy::new()))),
    )
}

fn rig_inner(
    clients: Vec<(&str, Arc<dyn FrontierClient>)>,
    spend: Option<Arc<dyn SpendLedger>>,
    turn_deadline_ms: u64,
    policy: Arc<dyn roundhouse_core::routing::RoutingPolicy>,
) -> Rig {
    let store = Arc::new(MemoryStore::new());
    let registry = FrontierClients::keyed(
        clients
            .into_iter()
            .map(|(provider, client)| (provider.to_string(), client))
            .collect(),
    );
    let mut engine = Engine::with_provider_clients(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local")) as Arc<dyn LocalExecutor>,
        catalog(),
        Arc::new(registry),
        policy,
        EngineConfig {
            turn_deadline_ms,
            ..EngineConfig::default()
        },
    );
    if let Some(spend) = spend {
        engine = engine.with_spend_ledger(spend);
    }
    Rig {
        engine: Arc::new(engine),
        store,
    }
}

impl Rig {
    async fn turn(
        &self,
        session_id: &SessionId,
        turn: &str,
        input: Vec<Item>,
        admission: &Admission,
    ) -> Result<roundhouse_server::TurnResult, roundhouse_server::EngineError> {
        self.engine.create_session(session_id).await.unwrap();
        self.engine
            .run_turn(session_id, TurnId::new(turn), input, admission)
            .await
    }

    /// Every routing decision in the log, oldest first.
    async fn decisions(
        &self,
        session_id: &SessionId,
    ) -> Vec<roundhouse_core::routing::DecisionRecord> {
        self.store
            .read_events(session_id, 0, 1_000)
            .await
            .expect("an in-memory log reads")
            .into_iter()
            .filter_map(|event| match event.kind {
                SessionEventKind::Routed { decision, .. } => Some(decision),
                _ => None,
            })
            .collect()
    }

    async fn fold(&self, session_id: &SessionId) -> MetricsFold {
        let events = self
            .store
            .read_events(session_id, 0, 1_000)
            .await
            .expect("an in-memory log reads");
        let mut fold = MetricsFold::new();
        fold.extend(events.iter());
        fold
    }
}

fn ask() -> Vec<Item> {
    vec![Item::user_text("please answer this")]
}

// ---------------------------------------------------------------------------
// S5: per-dispatch fallback
// ---------------------------------------------------------------------------

/// **The claim.** A provider that is not there advances the turn to the next
/// candidate of the same tier, inside one turn.
#[tokio::test]
async fn a_transport_failure_falls_forward_within_the_deadline() {
    let dead = Scripted::new(Script::Transport);
    let alive = Scripted::answering("beta answered");
    let unused = Scripted::answering("gamma answered");
    let rig = rig_of(vec![
        (PRIMARY, Arc::clone(&dead) as Arc<dyn FrontierClient>),
        (SECONDARY, Arc::clone(&alive) as Arc<dyn FrontierClient>),
        (THRIFTY, Arc::clone(&unused) as Arc<dyn FrontierClient>),
    ]);

    let session_id = SessionId::generate();
    let result = rig
        .turn(
            &session_id,
            "t1",
            ask(),
            &admission_with(PickerMode::CapableFirst),
        )
        .await
        .expect("the tier's second entry is there to take the turn");

    assert_eq!(result.text, "beta answered");
    assert_eq!(
        (dead.calls(), alive.calls(), unused.calls()),
        (1, 1, 0),
        "the dead provider was tried once, the fallback served, and the *other \
         tier* was never reached -- a fallback is a second attempt at the same \
         question, not a tier change nobody scored"
    );

    // Two decisions, each written before its own request went out.
    let decisions = rig.decisions(&session_id).await;
    assert_eq!(decisions.len(), 2, "one `Routed` per dispatch");
    assert_eq!(decisions[0].chosen, target(PRIMARY));
    assert!(
        decisions[0].attempts.is_empty(),
        "the first dispatch is nobody's consequence"
    );
    assert_eq!(decisions[1].chosen, target(SECONDARY));
    assert_eq!(
        decisions[1].attempts.len(),
        1,
        "and the second carries exactly the failure it is a consequence of"
    );
    assert_eq!(decisions[1].attempts[0].target, target(PRIMARY));
    assert_eq!(decisions[1].attempts[0].class, AttemptClass::Transport);

    // CONTROL: the identical rig with the first entry alive never reaches the
    // second, so the assertions above are about the *failure* and not about an
    // engine that dispatches twice on principle.
    let first = Scripted::answering("alpha answered");
    let second = Scripted::answering("beta answered");
    let rig = rig_of(vec![
        (PRIMARY, Arc::clone(&first) as Arc<dyn FrontierClient>),
        (SECONDARY, Arc::clone(&second) as Arc<dyn FrontierClient>),
        (
            THRIFTY,
            Scripted::answering("gamma") as Arc<dyn FrontierClient>,
        ),
    ]);
    let session_id = SessionId::generate();
    let result = rig
        .turn(
            &session_id,
            "t1",
            ask(),
            &admission_with(PickerMode::CapableFirst),
        )
        .await
        .unwrap();
    assert_eq!(result.text, "alpha answered");
    assert_eq!((first.calls(), second.calls()), (1, 0));
    assert_eq!(rig.decisions(&session_id).await.len(), 1);
}

/// The failover runs under the turn's *own* deadline, not a fresh one per
/// candidate.
///
/// Without this the mechanism is a way to spend N times the turn's allowance: a
/// tier of four dead providers, each given the full deadline, is four times the
/// wait a client agreed to. The first provider here burns the whole allowance
/// and then fails in a class that *would* have fallen forward — so the second
/// entry being untouched is a statement about the clock rather than about the
/// error.
#[tokio::test]
async fn a_failover_never_outlives_the_turns_deadline() {
    /// Sleeps past any deadline it is given, then fails in a retryable class.
    struct Slow;

    #[async_trait]
    impl FrontierClient for Slow {
        async fn execute(&self, _quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
            tokio::time::sleep(std::time::Duration::from_millis(2_000)).await;
            Err(FrontierError::Transport {
                message: "too late to matter".into(),
                timed_out: true,
            })
        }
    }

    let fallback = Scripted::answering("beta answered");
    let rig = rig_with(
        vec![
            (PRIMARY, Arc::new(Slow) as Arc<dyn FrontierClient>),
            (SECONDARY, Arc::clone(&fallback) as Arc<dyn FrontierClient>),
            (
                THRIFTY,
                Scripted::answering("gamma") as Arc<dyn FrontierClient>,
            ),
        ],
        None,
        150,
    );

    let session_id = SessionId::generate();
    let error = rig
        .turn(
            &session_id,
            "t1",
            ask(),
            &admission_with(PickerMode::CapableFirst),
        )
        .await
        .expect_err("a turn out of time has nowhere left to fall forward to");
    assert!(
        error.to_string().contains("deadline"),
        "the turn fails on the clock, not on the provider: {error}"
    );
    assert_eq!(
        fallback.calls(),
        0,
        "a second dispatch would have run past the deadline the client agreed to"
    );
    let decisions = rig.decisions(&session_id).await;
    assert_eq!(
        decisions.len(),
        1,
        "and no record was written for a dispatch that never happened"
    );
    assert!(decisions[0].attempts.is_empty());

    // CONTROL: the identical fixture with room on the clock does fall forward,
    // which is what makes the assertions above about the deadline rather than
    // about `Slow` being unretryable.
    let fallback = Scripted::answering("beta answered");
    let rig = rig_with(
        vec![
            (PRIMARY, Arc::new(Slow) as Arc<dyn FrontierClient>),
            (SECONDARY, Arc::clone(&fallback) as Arc<dyn FrontierClient>),
            (
                THRIFTY,
                Scripted::answering("gamma") as Arc<dyn FrontierClient>,
            ),
        ],
        None,
        30_000,
    );
    let session_id = SessionId::generate();
    let result = rig
        .turn(
            &session_id,
            "t1",
            ask(),
            &admission_with(PickerMode::CapableFirst),
        )
        .await
        .expect("with time on the clock the same failure falls forward");
    assert_eq!(result.text, "beta answered");
    assert_eq!(fallback.calls(), 1);
    assert_eq!(
        rig.decisions(&session_id).await[1].attempts[0].class,
        AttemptClass::Timeout,
        "and the row says the provider was slow rather than absent"
    );
}

/// A provider that *answered* is an answer, whatever it said.
///
/// Three shapes, one claim. A refusal arrives as a completed stream and is
/// served; a 404 and a 401 arrive as errors the provider chose to send, and a
/// second model would send the same one. Falling forward from any of them is
/// shopping for a verdict, or trying a bad key against every provider in the
/// tier.
#[tokio::test]
async fn a_refusal_is_an_answer_not_a_failover() {
    // A refusal: the model spoke, and what it said was no.
    let refuser = Scripted::answering("I can't help with that.");
    let fallback = Scripted::answering("beta answered");
    let rig = rig_of(vec![
        (PRIMARY, Arc::clone(&refuser) as Arc<dyn FrontierClient>),
        (SECONDARY, Arc::clone(&fallback) as Arc<dyn FrontierClient>),
        (
            THRIFTY,
            Scripted::answering("gamma") as Arc<dyn FrontierClient>,
        ),
    ]);
    let session_id = SessionId::generate();
    let result = rig
        .turn(
            &session_id,
            "t1",
            ask(),
            &admission_with(PickerMode::CapableFirst),
        )
        .await
        .expect("a refusal completes the turn");
    assert_eq!(result.text, "I can't help with that.");
    assert_eq!(
        (refuser.calls(), fallback.calls()),
        (1, 0),
        "a second model asked the same question is a way of shopping for a \
         verdict, not of surviving an outage"
    );
    assert_eq!(rig.decisions(&session_id).await.len(), 1);

    // The statuses a provider chose to send. Each fails the turn where it
    // stands, and the fallback is never called.
    for status in [400u16, 401, 404, 422] {
        let refusing = Scripted::new(Script::Status(status));
        let fallback = Scripted::answering("beta answered");
        let rig = rig_of(vec![
            (PRIMARY, Arc::clone(&refusing) as Arc<dyn FrontierClient>),
            (SECONDARY, Arc::clone(&fallback) as Arc<dyn FrontierClient>),
            (
                THRIFTY,
                Scripted::answering("gamma") as Arc<dyn FrontierClient>,
            ),
        ]);
        let session_id = SessionId::generate();
        rig.turn(
            &session_id,
            "t1",
            ask(),
            &admission_with(PickerMode::CapableFirst),
        )
        .await
        .expect_err("a status the provider chose is the turn's answer");
        assert_eq!(
            (refusing.calls(), fallback.calls()),
            (1, 0),
            "{status} must not send the turn shopping"
        );
        let decisions = rig.decisions(&session_id).await;
        assert_eq!(decisions.len(), 1, "{status} recorded one dispatch");
        assert!(decisions[0].attempts.is_empty());
    }

    // CONTROL: the same fixture with a *retryable* status does fall forward,
    // which is what makes the loop above about the status code rather than
    // about failover never firing.
    for status in [429u16, 500, 503] {
        let busy = Scripted::new(Script::Status(status));
        let fallback = Scripted::answering("beta answered");
        let rig = rig_of(vec![
            (PRIMARY, Arc::clone(&busy) as Arc<dyn FrontierClient>),
            (SECONDARY, Arc::clone(&fallback) as Arc<dyn FrontierClient>),
            (
                THRIFTY,
                Scripted::answering("gamma") as Arc<dyn FrontierClient>,
            ),
        ]);
        let session_id = SessionId::generate();
        let result = rig
            .turn(
                &session_id,
                "t1",
                ask(),
                &admission_with(PickerMode::CapableFirst),
            )
            .await
            .unwrap_or_else(|error| panic!("{status} should have fallen forward: {error}"));
        assert_eq!(result.text, "beta answered");
        assert_eq!((busy.calls(), fallback.calls()), (1, 1));
    }
}

/// The other side of the failover boundary: a body that dies mid-stream is not
/// retried anywhere.
///
/// Deltas are durable as they arrive, so a second attempt would append a second
/// answer to one response. Upstream draws the line in the same place, and this
/// is the test that stops somebody "improving" the loop into the stream.
#[tokio::test]
async fn a_stream_that_dies_after_the_first_delta_is_not_fallen_forward_from() {
    let flaky = Arc::new(StreamsThenDies) as Arc<dyn FrontierClient>;
    let fallback = Scripted::answering("beta answered");
    let rig = rig_of(vec![
        (PRIMARY, flaky),
        (SECONDARY, Arc::clone(&fallback) as Arc<dyn FrontierClient>),
        (
            THRIFTY,
            Scripted::answering("gamma") as Arc<dyn FrontierClient>,
        ),
    ]);
    let session_id = SessionId::generate();
    rig.turn(
        &session_id,
        "t1",
        ask(),
        &admission_with(PickerMode::CapableFirst),
    )
    .await
    .expect_err("a half-written answer fails the turn");
    assert_eq!(
        fallback.calls(),
        0,
        "the partial answer is already durable; a second dispatch would append \
         a second one to the same response"
    );
    assert_eq!(rig.decisions(&session_id).await.len(), 1);
}

/// Every fallback failed. The turn fails with the last error, and the log
/// carries the history.
#[tokio::test]
async fn exhausted_fallbacks_fail_with_the_history_on_the_record() {
    let first = Scripted::new(Script::Transport);
    let second = Scripted::new(Script::Status(503));
    let untouched = Scripted::answering("gamma answered");
    let rig = rig_of(vec![
        (PRIMARY, Arc::clone(&first) as Arc<dyn FrontierClient>),
        (SECONDARY, Arc::clone(&second) as Arc<dyn FrontierClient>),
        (THRIFTY, Arc::clone(&untouched) as Arc<dyn FrontierClient>),
    ]);

    let session_id = SessionId::generate();
    let error = rig
        .turn(
            &session_id,
            "t1",
            ask(),
            &admission_with(PickerMode::CapableFirst),
        )
        .await
        .expect_err("both entries of the tier are gone");
    assert!(
        error.to_string().contains("503"),
        "the turn fails with the *last* error, which is the one a caller can \
         act on: {error}"
    );
    assert_eq!(
        untouched.calls(),
        0,
        "an exhausted tier does not spill into the other one"
    );

    let decisions = rig.decisions(&session_id).await;
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[1].chosen, target(SECONDARY));
    assert_eq!(
        decisions[1].attempts[0].class,
        AttemptClass::Transport,
        "the first failure rides the record of the dispatch it caused"
    );
    // The *last* failure has no successor record to ride, and does not get a
    // duplicate `Routed` invented for it: it is the turn's failure, and it
    // arrives as the error above and as the terminal event the settle seam
    // writes from it.
    let terminal = rig
        .store
        .read_events(&session_id, 0, 1_000)
        .await
        .unwrap()
        .into_iter()
        .any(|event| matches!(event.kind, SessionEventKind::ResponseIncomplete { .. }));
    assert!(terminal, "the turn terminated rather than being left open");
}

/// A grant per *turn*, not per attempt — and a failed attempt is marked rather
/// than free.
///
/// Risk 3 in the plan, and the sharp edge of the whole mechanism: a hold opened
/// per attempt would let a flaky provider pyramid reservations until the ledger
/// refused a turn nobody had spent anything on.
#[tokio::test]
async fn a_failed_attempt_is_booked_and_the_hold_never_pyramids() {
    #[derive(Default)]
    struct CountingLedger {
        inner: MemorySpendLedger,
        grants: AtomicUsize,
        settles: AtomicUsize,
    }

    #[async_trait]
    impl SpendLedger for CountingLedger {
        async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError> {
            self.grants.fetch_add(1, Ordering::SeqCst);
            self.inner.open_grant(request).await
        }
        async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError> {
            self.settles.fetch_add(1, Ordering::SeqCst);
            self.inner.settle_grant(settlement).await
        }
        async fn balance(&self, query: BalanceQuery) -> Result<Balance, SpendError> {
            self.inner.balance(query).await
        }
    }

    let ledger = Arc::new(CountingLedger::default());
    let dead = Scripted::new(Script::Transport);
    let alive = Scripted::answering("beta answered");
    let rig = rig_with_ledger(
        vec![
            (PRIMARY, Arc::clone(&dead) as Arc<dyn FrontierClient>),
            (SECONDARY, Arc::clone(&alive) as Arc<dyn FrontierClient>),
            (
                THRIFTY,
                Scripted::answering("gamma") as Arc<dyn FrontierClient>,
            ),
        ],
        Some(Arc::clone(&ledger) as Arc<dyn SpendLedger>),
    );

    // A *budgeted* admission, deliberately: an unbudgeted one skips the ledger
    // entirely, so counts taken under it would be zero however many holds the
    // loop opened -- a guard for risk 3 that enforced nothing.
    let session_id = SessionId::generate();
    rig.turn(
        &session_id,
        "t1",
        ask(),
        &budgeted_admission_with(PickerMode::CapableFirst),
    )
    .await
    .unwrap();

    assert_eq!(
        (dead.calls(), alive.calls()),
        (1, 1),
        "the turn really did dispatch twice, which is what makes the counts \
         below a statement about the hold rather than about a turn that only \
         ever tried once"
    );
    assert_eq!(
        (
            ledger.grants.load(Ordering::SeqCst),
            ledger.settles.load(Ordering::SeqCst)
        ),
        (1, 1),
        "two dispatches, one hold, one settlement: a grant opened per attempt \
         would let a flaky provider pyramid reservations until the ledger \
         refused a turn nobody had spent anything on"
    );

    // Marked, never free: the dead provider owns a failed attempt on the
    // dashboard and no calls, which is the diagnosis. Booking it as a call
    // would put a zero-token row on the board and make the outage look like the
    // cheapest model in the fleet.
    let fold = rig.fold(&session_id).await;
    assert_eq!(
        fold.failed_attempts(Scope::Deployment),
        vec![(ModelKey::from_target(&target(PRIMARY)), 1)],
        "one row, on the provider that failed rather than on the one that served"
    );

    // CONTROL: the same rig with the first entry alive marks nothing.
    let rig = rig_of(vec![
        (
            PRIMARY,
            Scripted::answering("alpha answered") as Arc<dyn FrontierClient>,
        ),
        (
            SECONDARY,
            Scripted::answering("beta") as Arc<dyn FrontierClient>,
        ),
        (
            THRIFTY,
            Scripted::answering("gamma") as Arc<dyn FrontierClient>,
        ),
    ]);
    let session_id = SessionId::generate();
    rig.turn(
        &session_id,
        "t1",
        ask(),
        &admission_with(PickerMode::CapableFirst),
    )
    .await
    .unwrap();
    assert!(
        rig.fold(&session_id)
            .await
            .failed_attempts(Scope::Deployment)
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// S1 + S3: the signals reach the scorer, through the extractor
// ---------------------------------------------------------------------------

fn call(id: &str, name: &str, arguments: &str) -> Item {
    Item::tool_call(id, name, arguments)
}

fn result(id: &str, output: &str) -> Item {
    Item {
        role: Role::Tool,
        content: ItemContent::ToolResult {
            call_id: id.into(),
            output: output.into(),
        },
        response_id: None,
    }
}

fn shell(command: &str) -> String {
    serde_json::json!({ "command": command }).to_string()
}

/// A session deep enough to judge, producing nothing, investigating nothing,
/// and finishing on a traceback.
///
/// Built as *items* and driven through the engine, not as a hand-built
/// `ToolSignals`: the join under test is the extractor, and a fabricated signal
/// struct would pass on a build where nothing ever computed one.
fn a_stalling_session() -> Vec<Item> {
    let mut items = vec![Item::user_text("make the failing test pass")];
    for index in 0..9 {
        let id = format!("c{index}");
        items.push(call(&id, "shell_command", &shell("make build")));
        items.push(result(
            &id,
            match index {
                8 => "Traceback (most recent call last):\n  File \"x.py\"\n",
                _ => "linking...\n",
            },
        ));
    }
    items
}

/// The same depth, but the agent is producing work and the tests just passed.
fn a_settling_session() -> Vec<Item> {
    let mut items = vec![Item::user_text("make the failing test pass")];
    for index in 0..9 {
        let id = format!("c{index}");
        let (name, arguments, output) = match index {
            7 => ("write", r#"{"path":"src/lib.rs"}"#, "wrote 12 lines"),
            8 => (
                "shell_command",
                r#"{"command":"cargo test"}"#,
                "running 4 tests\ntest result: ok. 4 passed; 0 failed",
            ),
            _ => (
                "shell_command",
                r#"{"command":"make build"}"#,
                "linking...\n",
            ),
        };
        items.push(call(&id, name, arguments));
        items.push(result(&id, output));
    }
    items
}

/// **The claim.** Trouble in the session's own tool results moves the turn to
/// the capable tier.
#[tokio::test]
async fn a_stalling_session_escalates_to_the_capable_tier() {
    let capable = Scripted::answering("alpha answered");
    let thrifty = Scripted::answering("gamma answered");
    let rig = rig_of(vec![
        (PRIMARY, Arc::clone(&capable) as Arc<dyn FrontierClient>),
        (
            SECONDARY,
            Scripted::answering("beta") as Arc<dyn FrontierClient>,
        ),
        (THRIFTY, Arc::clone(&thrifty) as Arc<dyn FrontierClient>),
    ]);

    // `efficient_first`, so a turn that lands capable did so because the
    // *signals* put it there and not because the picker did.
    let session_id = SessionId::generate();
    let result = rig
        .turn(
            &session_id,
            "t1",
            a_stalling_session(),
            &admission_with(PickerMode::EfficientFirst),
        )
        .await
        .unwrap();
    assert_eq!(result.text, "alpha answered");
    let decision = result.decision.expect("a dispatched turn records one");
    assert_eq!(decision.target, target(PRIMARY));
    assert_eq!(
        decision.source,
        Some(DecisionSource::Dimensions),
        "a stall is scored, not overridden: two corroborating signals over the \
         threshold, which is what makes it narratable to the model inheriting \
         the turn"
    );
    assert!(
        decision.rationale.contains("strong tier"),
        "{}",
        decision.rationale
    );

    // CONTROL: the identical rig and picker over a session with nothing wrong
    // lands on the efficient tier. Same recipe, same fleet, different
    // exchanges — so the assertion above is about the signals.
    let session_id = SessionId::generate();
    let quiet = rig
        .turn(
            &session_id,
            "t2",
            ask(),
            &admission_with(PickerMode::EfficientFirst),
        )
        .await
        .unwrap();
    assert_eq!(quiet.text, "gamma answered");
    assert_eq!(
        quiet.decision.unwrap().source,
        Some(DecisionSource::Ambiguous),
        "and it got there by falling open, which a handoff note must never \
         narrate as an intervention"
    );
}

/// The other direction: a turn that produced work and passed its tests goes
/// down a tier even under a capable-first picker.
#[tokio::test]
async fn a_tests_passed_production_turn_deescalates() {
    let capable = Scripted::answering("alpha answered");
    let thrifty = Scripted::answering("gamma answered");
    let rig = rig_of(vec![
        (PRIMARY, Arc::clone(&capable) as Arc<dyn FrontierClient>),
        (
            SECONDARY,
            Scripted::answering("beta") as Arc<dyn FrontierClient>,
        ),
        (THRIFTY, Arc::clone(&thrifty) as Arc<dyn FrontierClient>),
    ]);

    let session_id = SessionId::generate();
    let result = rig
        .turn(
            &session_id,
            "t1",
            a_settling_session(),
            &admission_with(PickerMode::CapableFirst),
        )
        .await
        .unwrap();
    assert_eq!(result.text, "gamma answered");
    let decision = result.decision.unwrap();
    assert_eq!(decision.target, target(THRIFTY));
    assert_eq!(decision.source, Some(DecisionSource::TestsPassed));

    // CONTROL: the same picker over an ordinary session stays capable, so the
    // move above is the de-escalation and not the picker.
    let session_id = SessionId::generate();
    let plain = rig
        .turn(
            &session_id,
            "t2",
            ask(),
            &admission_with(PickerMode::CapableFirst),
        )
        .await
        .unwrap();
    assert_eq!(plain.text, "alpha answered");
    assert_eq!(
        plain.decision.unwrap().source,
        Some(DecisionSource::Ambiguous)
    );
}

/// A project with no recipe routes exactly as it did before M10, through the
/// same composed policy object.
#[tokio::test]
async fn a_project_without_a_recipe_routes_as_it_always_did() {
    let rig = rig_of(vec![
        (
            PRIMARY,
            Scripted::answering("alpha answered") as Arc<dyn FrontierClient>,
        ),
        (
            SECONDARY,
            Scripted::answering("beta answered") as Arc<dyn FrontierClient>,
        ),
        (
            THRIFTY,
            Scripted::answering("gamma answered") as Arc<dyn FrontierClient>,
        ),
    ]);

    // Same fleet, same stalling session, no `tiers`: the affinity scorer picks
    // on cost and warmth, records no tier source, and offers no fallbacks.
    let session_id = SessionId::generate();
    let result = rig
        .turn(&session_id, "t1", a_stalling_session(), &Admission::open())
        .await
        .unwrap();
    let decision = result.decision.unwrap();
    assert_eq!(
        decision.source, None,
        "an unstaged decision states no tier source, which is what the handoff \
         gate reads"
    );
    assert!(decision.fallbacks.is_empty());
    assert!(
        decision.rationale.starts_with("score "),
        "the inner policy's own rationale, verbatim: {}",
        decision.rationale
    );
    assert_eq!(rig.decisions(&session_id).await.len(), 1);
}

// ---------------------------------------------------------------------------
// S4: the declared baseline
// ---------------------------------------------------------------------------

/// The `model` field is read, recorded, and never routed on.
#[tokio::test]
async fn the_declared_baseline_is_recorded_and_changes_no_route() {
    use roundhouse_server::TurnInput;

    let rig = rig_of(vec![
        (
            PRIMARY,
            Scripted::answering("alpha answered") as Arc<dyn FrontierClient>,
        ),
        (
            SECONDARY,
            Scripted::answering("beta answered") as Arc<dyn FrontierClient>,
        ),
        (
            THRIFTY,
            Scripted::answering("gamma answered") as Arc<dyn FrontierClient>,
        ),
    ]);

    // A baseline naming the *thrifty* model on a turn the picker sends capable.
    // If routing could see it, this is the turn that would move.
    let session_id = SessionId::generate();
    rig.engine.create_session(&session_id).await.unwrap();
    let result = rig
        .engine
        .run_turn(
            &session_id,
            TurnId::new("t1"),
            TurnInput {
                items: ask(),
                declared_baseline: Some(format!("{THRIFTY}/m")),
            },
            &admission_with(PickerMode::CapableFirst),
        )
        .await
        .unwrap();
    assert_eq!(
        result.text, "alpha answered",
        "the declared baseline is the counterfactual's name, never a target"
    );

    let decisions = rig.decisions(&session_id).await;
    assert_eq!(
        decisions[0].declared_baseline.as_deref(),
        Some(format!("{THRIFTY}/m").as_str()),
        "and it is on the record verbatim, because that is the only place the \
         pricing seam can read it from"
    );

    // A baseline nothing resolves is recorded exactly as written rather than
    // dropped: a log that discarded it would leave nothing to explain why the
    // counterfactual was inferred.
    let session_id = SessionId::generate();
    rig.engine.create_session(&session_id).await.unwrap();
    rig.engine
        .run_turn(
            &session_id,
            TurnId::new("t1"),
            TurnInput {
                items: ask(),
                declared_baseline: Some("a-model-nobody-serves".into()),
            },
            &admission_with(PickerMode::CapableFirst),
        )
        .await
        .unwrap();
    assert_eq!(
        rig.decisions(&session_id).await[0]
            .declared_baseline
            .as_deref(),
        Some("a-model-nobody-serves")
    );

    // CONTROL: a client that named nothing records nothing, which is what
    // separates "inferred because nobody declared" from "inferred because the
    // declaration did not resolve".
    let session_id = SessionId::generate();
    rig.turn(
        &session_id,
        "t1",
        ask(),
        &admission_with(PickerMode::CapableFirst),
    )
    .await
    .unwrap();
    assert_eq!(rig.decisions(&session_id).await[0].declared_baseline, None);
}

// ---------------------------------------------------------------------------
// S3: the composition root's half — a recipe nothing reads is not silent
// ---------------------------------------------------------------------------

/// Everything `tracing` wrote while `f` ran, as text.
///
/// The runtime is built *inside* the subscriber guard rather than by
/// `#[tokio::test]` outside it. `set_default` installs the collector on the
/// calling thread, and a current-thread runtime polls the future on that same
/// thread — so every `tracing` call the turn makes lands in this buffer. Driven
/// by `#[tokio::test]` instead, the guard and the executor thread are not
/// guaranteed to be the same one and the assertion becomes a coin toss.
fn captured(f: impl std::future::Future<Output = ()>) -> String {
    use std::io;
    use std::sync::Mutex as StdMutex;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct Buf(Arc<StdMutex<Vec<u8>>>);
    impl io::Write for Buf {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for Buf {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let buf = Buf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .finish();
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime")
            .block_on(f);
    }
    String::from_utf8(buf.0.lock().unwrap().clone()).expect("tracing output is UTF-8")
}

/// **The composition hole, made observable.** A `tiers` block that reaches a
/// router which cannot read one selects nothing, and says so.
///
/// `main.rs` composes `StagePolicy` only when some project already had a recipe
/// at boot — deliberately, because composing it unconditionally would relabel
/// `DecisionRecord::policy` for every deployment that changed no routing. The
/// residue is a recipe added through the admin plane afterwards: the field
/// parses, validates, resolves onto the `Admission`, and then re-routes nothing
/// on a process whose router picks candidates rather than tiers. That is the
/// worst shape a config mistake takes — no error, no behavior change, and
/// nothing to grep for.
///
/// The control is what makes this a claim about the *router* rather than about
/// the recipe being present: the identical admission on the stage router emits
/// nothing at all.
#[test]
fn a_recipe_a_router_cannot_read_warns_once_and_only_there() {
    let unread = captured(async {
        let rig = rig_over_policy(
            vec![
                (
                    PRIMARY,
                    Scripted::answering("alpha answered") as Arc<dyn FrontierClient>,
                ),
                (
                    SECONDARY,
                    Scripted::answering("beta answered") as Arc<dyn FrontierClient>,
                ),
                (
                    THRIFTY,
                    Scripted::answering("gamma answered") as Arc<dyn FrontierClient>,
                ),
            ],
            // The router a process that booted before any recipe existed holds.
            Arc::new(AffinityPolicy::new()),
        );
        // Two turns, because the claim is "once per process" and one turn cannot
        // tell a `Once` from an unconditional `warn!`.
        for turn in ["t1", "t2"] {
            let session_id = SessionId::generate();
            rig.turn(
                &session_id,
                turn,
                ask(),
                &admission_with(PickerMode::EfficientFirst),
            )
            .await
            .unwrap();
        }
    });

    assert!(
        unread.contains("selecting nothing"),
        "an unread recipe must reach an operator's log: {unread}"
    );
    assert!(
        unread.contains("affinity"),
        "and it must name the router that could not read it, which is the half \
         that tells an operator what to change: {unread}"
    );
    assert_eq!(
        unread.matches("selecting nothing").count(),
        1,
        "once per process, not once per turn: the condition is a property of the \
         composition and is true for every turn this process will serve, so a \
         per-turn line trains an operator to filter exactly the line they need — \
         {unread}"
    );

    // CONTROL: the same recipe, the same admission, the stage router. Silence.
    let read = captured(async {
        let rig = rig_of(vec![
            (
                PRIMARY,
                Scripted::answering("alpha answered") as Arc<dyn FrontierClient>,
            ),
            (
                SECONDARY,
                Scripted::answering("beta answered") as Arc<dyn FrontierClient>,
            ),
            (
                THRIFTY,
                Scripted::answering("gamma answered") as Arc<dyn FrontierClient>,
            ),
        ]);
        let session_id = SessionId::generate();
        rig.turn(
            &session_id,
            "t1",
            ask(),
            &admission_with(PickerMode::EfficientFirst),
        )
        .await
        .unwrap();
    });
    assert!(
        !read.contains("selecting nothing"),
        "a recipe the router *does* read is the ordinary case and must be \
         silent: {read}"
    );
}
