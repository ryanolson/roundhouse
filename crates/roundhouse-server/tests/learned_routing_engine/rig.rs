// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The rig every learned-routing engine test runs on: two priced hosted
//! targets on one tier recipe, the stage router as the engine's policy, a
//! learner store that counts and can be told to fail, and a session store that
//! counts mark clears and acknowledgements and can refuse the latter.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{ProjectId, SpendLedger};
use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::interject::Interjector;
use roundhouse_core::item::Item;
use roundhouse_core::learn_store::{
    Applied, LearnerError, LearnerStore, LearningBatch, MemoryLearnerStore, ReadRequest,
};
use roundhouse_core::routing::learn::{
    ActiveMode, EpochId, KeyLevel, LearnedEvidence, LearnedInput, LearnerMode, LearnerTerms,
    LevelKey, OnInfeasible, PriorUnits, QualityTerms, ReadView, Strategy, StrategySet, Units,
};
use roundhouse_core::routing::stage::DEFAULT_CONFIDENCE_THRESHOLD;
use roundhouse_core::routing::{
    AffinityPolicy, DecisionRecord, PickerMode, ProviderPricing, SelectorBranch, StagePolicy,
    Target, Tier, TierRecipe,
};
use roundhouse_core::session::{Deltas, LearningEntry, QualityDelta, TargetDelta};
use roundhouse_core::store::doubles::Delegating;
use roundhouse_core::store::{
    ClearOutcome, LearningMark, Lease, MemoryStore, SessionStore, StoreError,
};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierClient, FrontierClients, FrontierModelSpec, StaticFrontierCatalog,
    WireProtocol,
};
use roundhouse_server::test_support::frontier_spec;
use roundhouse_server::{
    Admission, EchoLocalExecutor, Engine, EngineConfig, EngineError, LocalExecutor, TurnResult,
};

pub const SALT: &str = "learned-routing-engine";
pub const LARGE: &str = "large";
pub const SMALL: &str = "small";
pub const ANSWER: &str = "answered";

pub fn large() -> Target {
    Target::Frontier {
        provider: LARGE.into(),
        model: "m".into(),
    }
}

pub fn small() -> Target {
    Target::Frontier {
        provider: SMALL.into(),
        model: "m".into(),
    }
}

/// A priced hosted model. `large` costs ten times `small`, so `efficient` is
/// always the cheaper plan and exploration has somewhere to go.
pub fn spec(
    provider: &str,
    per_mtok: f64,
    base_ttft_ms: f64,
    wire: WireProtocol,
) -> FrontierModelSpec {
    FrontierModelSpec {
        quality_prior: 0.9,
        pricing: ProviderPricing {
            input_per_mtok_usd: per_mtok,
            cached_input_per_mtok_usd: per_mtok / 10.0,
            cache_write_per_mtok_usd: per_mtok * 1.25,
            output_per_mtok_usd: per_mtok * 5.0,
        },
        base_ttft_ms,
        ttft_ms_per_uncached_token: 0.0,
        ..frontier_spec(provider, "m", wire)
    }
}

pub fn catalog() -> StaticFrontierCatalog {
    catalog_with(10.0, WireProtocol::OpenAiResponses)
}

/// The catalog with `small`'s quoted TTFT and `large`'s wire set.
pub fn catalog_with(small_ttft_ms: f64, large_wire: WireProtocol) -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![
        spec(LARGE, 10.0, 10.0, large_wire),
        spec(SMALL, 1.0, small_ttft_ms, WireProtocol::OpenAiResponses),
    ])
}

/// capable = [large], efficient = [small], capable first: an unremarkable
/// turn is picked capable and served `large`, so `rules` and `capable` agree
/// and `efficient` is the cheaper alternative.
pub fn recipe() -> TierRecipe {
    TierRecipe::new(
        vec![format!("{LARGE}/m")],
        vec![format!("{SMALL}/m")],
        PickerMode::CapableFirst,
        DEFAULT_CONFIDENCE_THRESHOLD,
    )
    .expect("a two-tier recipe")
}

pub fn epoch() -> EpochId {
    EpochId::new([0x5e; 16])
}

/// A learner at fixture thresholds: one interval of live evidence opens a
/// level, one session can pass it, and the Wilson bound at 20 positive
/// intervals clears the 0.8 floor.
pub fn terms(mode: LearnerMode) -> LearnerTerms {
    LearnerTerms {
        mode,
        strategies: StrategySet::new(vec![
            Strategy::Rules,
            Strategy::Efficient,
            Strategy::Capable,
        ])
        .unwrap(),
        epoch: epoch(),
        prior: PriorUnits::default(),
        quality: QualityTerms {
            floor: 0.8,
            z: 1.96,
            min_evidence: 1_000,
            min_sessions: 1,
        },
        latency_limit_ms: 10_000,
        latency_min_samples: 20,
        cache_min_samples: 20,
        on_infeasible: OnInfeasible::ServeRules,
        exploration: None,
        read_timeout_ms: 2_000,
        apply_timeout_ms: 2_000,
    }
}

pub fn admission(project: &str, terms: Option<LearnerTerms>) -> Admission {
    Admission {
        principal: roundhouse_core::control::Principal::new(project, "ada"),
        tiers: Some(Arc::new(recipe())),
        learner: terms.map(Arc::new),
        ..Admission::open()
    }
}

/// The input of a fresh session's turn: picked capable by the picker, no
/// classification, no tools.
pub fn fresh_input() -> LearnedInput {
    LearnedInput::encode(Tier::Capable, false, None, &[])
}

pub fn l2(input: &LearnedInput) -> LevelKey {
    input.key(KeyLevel::L2)
}

/// Positive evidence that passes the fixture gate, and negative evidence that
/// sits below its floor.
pub const PASS: (u64, u64) = (20_000, 20_000);
pub const BELOW: (u64, u64) = (0, 20_000);

// ------------------------------------------------------------ learner store

/// What the next apply does instead of the plain store call.
pub enum ApplyScript {
    /// Refuse with this error and write nothing.
    Refuse(LearnerError),
    /// Apply to the store, then stall past any timeout: the result the engine
    /// sees is unknown, and the store has the entries.
    LandThenStall(Duration),
}

/// The memory learner store, counted and sabotageable.
#[derive(Default)]
pub struct ProbeStore {
    pub inner: MemoryLearnerStore,
    reads: AtomicUsize,
    applies: AtomicUsize,
    read_delay: Mutex<Option<Duration>>,
    fail_reads: AtomicBool,
    script: Mutex<VecDeque<ApplyScript>>,
}

impl ProbeStore {
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    pub fn applies(&self) -> usize {
        self.applies.load(Ordering::SeqCst)
    }

    pub fn delay_reads(&self, delay: Duration) {
        *self.read_delay.lock().unwrap() = Some(delay);
    }

    pub fn fail_reads(&self, fail: bool) {
        self.fail_reads.store(fail, Ordering::SeqCst);
    }

    pub fn script(&self, step: ApplyScript) {
        self.script.lock().unwrap().push_back(step);
    }
}

#[async_trait]
impl LearnerStore for ProbeStore {
    async fn read(&self, request: &ReadRequest) -> Result<ReadView, LearnerError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let delay = *self.read_delay.lock().unwrap();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(LearnerError::Unavailable("the probe is down".into()));
        }
        self.inner.read(request).await
    }

    async fn apply(&self, batch: &LearningBatch<'_>) -> Result<Applied, LearnerError> {
        self.applies.fetch_add(1, Ordering::SeqCst);
        let step = self.script.lock().unwrap().pop_front();
        match step {
            None => self.inner.apply(batch).await,
            Some(ApplyScript::Refuse(error)) => Err(error),
            Some(ApplyScript::LandThenStall(stall)) => {
                let landed = self.inner.apply(batch).await;
                tokio::time::sleep(stall).await;
                landed
            }
        }
    }

    async fn watermark(
        &self,
        project: &ProjectId,
        session: &SessionId,
    ) -> Result<u64, LearnerError> {
        self.inner.watermark(project, session).await
    }
}

// ------------------------------------------------------------ session store

/// The memory session store, counting mark clears and `LearningApplied`
/// appends, and able to refuse the latter.
#[derive(Default)]
pub struct CountingStore {
    pub inner: MemoryStore,
    clears: AtomicUsize,
    acks: AtomicUsize,
    refuse_acks: AtomicBool,
}

impl CountingStore {
    pub fn clears(&self) -> usize {
        self.clears.load(Ordering::SeqCst)
    }

    pub fn acks(&self) -> usize {
        self.acks.load(Ordering::SeqCst)
    }

    pub fn refuse_acks(&self, refuse: bool) {
        self.refuse_acks.store(refuse, Ordering::SeqCst);
    }
}

#[async_trait]
impl Delegating for CountingStore {
    type Backend = MemoryStore;

    fn backend(&self) -> &MemoryStore {
        &self.inner
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
        mark: Option<LearningMark>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        if kinds
            .iter()
            .any(|kind| matches!(kind, SessionEventKind::LearningApplied { .. }))
        {
            self.acks.fetch_add(1, Ordering::SeqCst);
            if self.refuse_acks.load(Ordering::SeqCst) {
                return Err(StoreError::LeaseLost {
                    session_id: lease.session_id.clone(),
                    node_id: lease.node_id.clone(),
                });
            }
        }
        SessionStore::append_events(&self.inner, lease, kinds, mark).await
    }

    async fn clear_learning_mark(
        &self,
        session_id: &SessionId,
        confirmed_through: u64,
    ) -> Result<ClearOutcome, StoreError> {
        self.clears.fetch_add(1, Ordering::SeqCst);
        SessionStore::clear_learning_mark(&self.inner, session_id, confirmed_through).await
    }
}

// --------------------------------------------------------------------- rig

/// A provider that is not there: every connection fails at the transport,
/// which is a failover class.
pub struct Refusing;

#[async_trait]
impl FrontierClient for Refusing {
    async fn execute(
        &self,
        _quote: &roundhouse_fleet::FrontierQuote,
    ) -> Result<roundhouse_fleet::FrontierStream, roundhouse_fleet::FrontierError> {
        Err(roundhouse_fleet::FrontierError::Transport {
            message: "connection refused".into(),
            timed_out: false,
        })
    }
}

pub struct RigConfig {
    pub catalog: StaticFrontierCatalog,
    pub interjector: Option<Arc<dyn Interjector>>,
    pub spend: Option<Arc<dyn SpendLedger>>,
    pub classifier:
        Option<Arc<roundhouse_server::classify_runtime::ClassificationRuntime<ByteTokenizer>>>,
    /// Providers whose transport refuses every connection.
    pub down: Vec<&'static str>,
    pub learner: bool,
}

impl Default for RigConfig {
    fn default() -> Self {
        Self {
            catalog: catalog(),
            interjector: None,
            spend: None,
            classifier: None,
            down: Vec::new(),
            learner: true,
        }
    }
}

pub struct Rig {
    pub engine: Arc<Engine<CountingStore, ByteTokenizer>>,
    pub sessions: Arc<CountingStore>,
    pub learner: Arc<ProbeStore>,
}

impl Rig {
    pub fn new(config: RigConfig) -> Self {
        Self::over(
            config,
            Arc::new(CountingStore::default()),
            Arc::new(ProbeStore::default()),
        )
    }

    /// An engine over stores another rig already used: a successor node.
    pub fn over(config: RigConfig, sessions: Arc<CountingStore>, learner: Arc<ProbeStore>) -> Self {
        let clients = FrontierClients::keyed(
            [LARGE, SMALL]
                .into_iter()
                .map(|provider| {
                    let client: Arc<dyn FrontierClient> = match config.down.contains(&provider) {
                        true => Arc::new(Refusing),
                        false => Arc::new(EchoFrontierClient::new(ANSWER)),
                    };
                    (provider.to_string(), client)
                })
                .collect(),
        );
        let mut engine = Engine::with_provider_clients(
            Arc::clone(&sessions),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local")) as Arc<dyn LocalExecutor>,
            config.catalog,
            Arc::new(clients),
            Arc::new(StagePolicy::new(Box::new(AffinityPolicy::new()))),
            EngineConfig {
                arm_salt: SALT.into(),
                turn_deadline_ms: 10_000,
                ..EngineConfig::default()
            },
        );
        if config.learner {
            engine = engine.with_learner(Arc::clone(&learner) as Arc<dyn LearnerStore>);
        }
        if let Some(interjector) = config.interjector {
            engine = engine.with_interjector(interjector);
        }
        if let Some(spend) = config.spend {
            engine = engine.with_spend_ledger(spend);
        }
        if let Some(classifier) = config.classifier {
            engine = engine.with_classifier(classifier);
        }
        Self {
            engine: Arc::new(engine),
            sessions,
            learner,
        }
    }

    pub async fn turn(
        &self,
        session: &SessionId,
        turn: &str,
        admission: &Admission,
    ) -> Result<TurnResult, EngineError> {
        self.turn_with(
            session,
            turn,
            vec![Item::user_text(format!("question {turn}"))],
            admission,
        )
        .await
    }

    pub async fn turn_with(
        &self,
        session: &SessionId,
        turn: &str,
        input: Vec<Item>,
        admission: &Admission,
    ) -> Result<TurnResult, EngineError> {
        self.engine
            .create_session(session)
            .await
            .expect("a session");
        self.engine
            .run_turn(session, TurnId::new(turn), input, admission)
            .await
    }

    pub async fn events(&self, session: &SessionId) -> Vec<SessionEvent> {
        SessionStore::read_events(self.sessions.as_ref(), session, 0, 100_000)
            .await
            .expect("the log reads")
    }

    pub async fn decisions(&self, session: &SessionId) -> Vec<DecisionRecord> {
        self.events(session)
            .await
            .into_iter()
            .filter_map(|event| match event.kind {
                SessionEventKind::Routed { decision, .. } => Some(decision),
                _ => None,
            })
            .collect()
    }

    /// The newest decision's learned record, which every learned turn has.
    pub async fn last_learned(&self, session: &SessionId) -> LearnedEvidence {
        let decisions = self.decisions(session).await;
        learned(decisions.last().expect("a routed turn")).expect("a learned record")
    }

    pub async fn acknowledged(&self, session: &SessionId) -> Vec<u64> {
        self.events(session)
            .await
            .into_iter()
            .filter_map(|event| match event.kind {
                SessionEventKind::LearningApplied { through_seq } => Some(through_seq),
                _ => None,
            })
            .collect()
    }
}

/// The learned record a decision carries, if any.
pub fn learned(decision: &DecisionRecord) -> Option<LearnedEvidence> {
    match &decision.selection.as_ref()?.selector.as_ref()?.branch {
        SelectorBranch::Learned(evidence) => Some((**evidence).clone()),
        _ => None,
    }
}

pub fn mode_of(evidence: &LearnedEvidence) -> ActiveMode {
    evidence.mode
}

// ----------------------------------------------------------------- seeding

/// Write quality evidence into the store directly, as one session of its own
/// (draft 7.8: serving without exploration cannot produce these states).
pub async fn seed(
    store: &ProbeStore,
    project: &str,
    key: LevelKey,
    evidence: &[(Strategy, (u64, u64))],
) {
    let deltas = Deltas {
        epoch: epoch(),
        quality: evidence
            .iter()
            .map(|&(strategy, (pos, n))| QualityDelta {
                key,
                strategy,
                units: Units { pos, n },
            })
            .collect(),
        targets: Vec::new(),
        overhead: Default::default(),
        jev: Vec::new(),
    };
    seed_deltas(store, project, deltas).await;
}

/// Write operational rows for `target` directly.
pub async fn seed_ops(store: &ProbeStore, project: &str, row: TargetDelta) {
    let deltas = Deltas {
        epoch: epoch(),
        quality: Vec::new(),
        targets: vec![row],
        overhead: Default::default(),
        jev: Vec::new(),
    };
    seed_deltas(store, project, deltas).await;
}

async fn seed_deltas(store: &ProbeStore, project: &str, deltas: Deltas) {
    let project = ProjectId::from(project);
    let session = SessionId::generate();
    let entries = [LearningEntry {
        seq: 1,
        prev_seq: 0,
        credit_revision: roundhouse_core::routing::learn::LEARNING_CREDIT_REVISION,
        review_rule_revision: roundhouse_core::validate::REVIEW_RULE_REVISION,
        deltas: Some(deltas),
    }];
    store
        .inner
        .apply(&LearningBatch {
            project: &project,
            session: &session,
            entries: &entries,
        })
        .await
        .expect("a seed applies");
}

/// The store's quality counts for `strategy` at `key`: `(pos, n, sessions)`.
pub async fn counts(
    store: &ProbeStore,
    project: &str,
    key: LevelKey,
    strategy: Strategy,
) -> (u64, u64, u64) {
    let input_keys = key;
    let view = view_at(store, project, input_keys).await;
    view.levels
        .iter()
        .find(|level| level.key == key)
        .and_then(|level| {
            level
                .strategies
                .iter()
                .find(|counts| counts.strategy == strategy)
        })
        .map(|counts| (counts.pos_units, counts.n_units, counts.sessions))
        .unwrap_or_default()
}

/// A read of the three keys of the input whose key at its level is `key`.
async fn view_at(store: &ProbeStore, project: &str, key: LevelKey) -> ReadView {
    let input = match key {
        LevelKey::L2 {
            rules_pick,
            newest,
            prior,
            tool_turn,
        } => LearnedInput {
            rules_pick,
            newest,
            prior,
            tool_turn,
        },
        LevelKey::L1 { rules_pick, newest } => LearnedInput {
            rules_pick,
            newest,
            prior: roundhouse_core::routing::learn::PriorBand::Absent,
            tool_turn: false,
        },
        LevelKey::L0 { rules_pick } => LearnedInput {
            rules_pick,
            newest: roundhouse_core::routing::learn::Band::None,
            prior: roundhouse_core::routing::learn::PriorBand::Absent,
            tool_turn: false,
        },
    };
    let request = ReadRequest::new(
        ProjectId::from(project),
        epoch(),
        &input,
        &terms(LearnerMode::Shadow).strategies,
        [&large(), &small()],
    );
    store.inner.read(&request).await.expect("a memory read")
}

/// The sum of `n` units for `strategy` over the three keys of `input`: one
/// interval's credit is split across them.
pub async fn units_over_keys(
    store: &ProbeStore,
    project: &str,
    input: &LearnedInput,
    strategy: Strategy,
) -> (u64, u64) {
    let mut total = (0, 0);
    for key in input.keys() {
        let (pos, n, _) = counts(store, project, key, strategy).await;
        total.0 += pos;
        total.1 += n;
    }
    total
}

/// The entries a successor would fold from the log, from the first.
pub async fn replayed_chain(rig: &Rig, session: &SessionId) -> Vec<LearningEntry> {
    roundhouse_core::session::SessionState::project_learning(rig.sessions.as_ref(), session, 0)
        .await
        .expect("the log replays")
        .learning_page()
        .to_vec()
}

pub fn project(name: &str) -> ProjectId {
    ProjectId::from(name)
}
