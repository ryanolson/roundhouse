// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Test-only fixtures shared across this crate's own unit tests and the
//! integration-test binaries under `tests/`.
//!
//! **One copy, not two.** Before M13.1's review (F5), `SeveredStore` was a
//! 270-line addition duplicated verbatim between `engine/fair_use.rs`'s
//! `#[cfg(test)]` module and `tests/fair_use.rs` — the two could not share a
//! copy because a `#[cfg(test)]` unit test module compiles as part of this
//! crate itself, with no path to `tests/common`, which is private to each
//! integration-test binary. Gated behind `test-support` — the same feature
//! [`control_config::directory`](crate::control_config::directory)'s
//! `PlaneSource for ControlPlane` uses for the identical reason — this module
//! is visible from both: this crate's own unit tests build under
//! `#[cfg(test)]`, which the self-referential dev-dependency in `Cargo.toml`
//! turns `test-support` on for, and every downstream integration-test binary
//! already depends on this crate with the same feature to reach
//! `roundhouse_store_redis::test_support` for its own Redis fixtures.
//!
//! [`bind_conversation`] and [`fork_conversation`] are the same story told
//! about [`crate::conversations::Conversations`] rather than a store (M15,
//! H1): `Conversations::bind` and `::fork` had no caller left on the serving
//! path once M14.0 moved every real turn's write onto `Conversations::commit`,
//! but the fixtures that stand a conversation up without driving a turn
//! through admission still need the shape those two spelled — so it is named
//! once, here, rather than reappearing as `conversations.generation(key).await`
//! followed by a hand-rolled `commit` at each of the call sites that used to
//! read `conversations.bind(...)`.
//!
//! [`frontier_spec`], [`single_model_catalog`] and [`engine_over_echo`] are
//! the same discipline applied to a shape M15's H2 named eleven copies of:
//! `fn catalog() -> StaticFrontierCatalog` and `fn engine(...)` /
//! `fn engine_over(...)` under `tests/` and (once, for the unit suite that
//! cannot reach `tests/common`) `src/prefix_admission/tests.rs`. Each copy
//! hand-rolled the same [`FrontierModelSpec`] literal — provider, model and
//! quality genuinely varying; `wire_protocol`, `cache_model` and
//! `ttft_ms_per_uncached_token` never doing anything but repeating — and the
//! same seven-argument `Engine::new`, varying only in the store, the catalog,
//! the frontier client and the config. `tests/common/mod.rs::frontier_catalog`
//! already named the single-entry, zero-variation case for the binaries that
//! could reach it (M14.0 review, F5's shared-fixture lesson applied to a
//! different pair of duplicates); these three go one level down, to the
//! *pieces* a catalog and an engine are built from, so a fixture that needs
//! two or three priced models, or a store `tests/common` cannot name, still
//! shares the boilerplate rather than retyping it.
//!
//! [`ScriptedDirectoryStore`] is the same discipline applied to a
//! [`DirectoryStore`] double (M16.0 review, F1): before this, the directory
//! suite (`control_config/directory/tests.rs`) and the admin HTTP suite
//! (`tests/admin_api.rs`) each hand-rolled their own `(records, version)`
//! store with its own copy of the compare-and-set `commit` performs — three
//! copies, two of which never had their `commit` driven by a test at all,
//! because every fixture that needed a store double reached for `load` and
//! `version` alone. This wraps the real production store instead of
//! re-implementing it, so a fixture that scripts a stall, a count, a failure
//! or a scripted write is still exercising the one compare-and-set this
//! deployment actually ships — and, since M16.1 (R-D7), the real *codec* too:
//! the store it wraps is [`DocumentDirectoryStore`] over a
//! [`MemoryDocumentStore`], so every scripted fixture in the workspace round
//! trips its records through the same JSON envelope a deployment writes to
//! Redis.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{RwLock, Semaphore};
use tokio::task::JoinHandle;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::Principal;
use roundhouse_core::routing::{AffinityPolicy, CacheModel, ProviderPricing};
use roundhouse_core::store::SessionStore;
use roundhouse_fleet::{FrontierClient, FrontierModelSpec, StaticFrontierCatalog, WireProtocol};

use roundhouse_core::control::MemoryDocumentStore;

use crate::control_config::directory::{
    DirectoryRecords, DirectoryStore, DocumentDirectoryStore, StoreFailure, VersionedRecords,
};
use crate::engine::{EchoLocalExecutor, Engine, EngineConfig};
use roundhouse_core::ids::SessionId;

use crate::conversations::Conversations;

/// Bind `key` to the generation [`Conversations::commit`] would land a fresh,
/// never-forked turn on — the shape `Conversations::bind` used to spell before
/// M14.0 moved every serving-path write onto `commit` once prefix admission's
/// search had an answer (M15, H1). A fixture that stands a conversation up
/// without driving a real turn through admission still needs exactly this:
/// "generation zero (or whatever this key is already at), now" — and one
/// helper here, rather than `self.generation(key).await` repeated at each of
/// the dozens of call sites `bind` used to serve, is what stops the shape from
/// silently drifting the day `commit`'s signature next changes.
pub async fn bind_conversation(
    conversations: &Conversations,
    principal: &Principal,
    key: &str,
) -> SessionId {
    let generation = conversations.generation(key).await;
    conversations.commit(principal, key, generation).await
}

/// Rebind `key` to a fresh session one generation past whatever it is at now
/// — the shape `Conversations::fork` used to spell, for the fixtures that
/// need to play a client whose resent history disagreed with the log without
/// running prefix admission's search to get there. See
/// [`bind_conversation`] for why this lives here rather than at each call
/// site.
pub async fn fork_conversation(
    conversations: &Conversations,
    principal: &Principal,
    key: &str,
) -> SessionId {
    let generation = conversations.generation(key).await.saturating_add(1);
    conversations.commit(principal, key, generation).await
}

/// One [`FrontierModelSpec`], named and dialected the way a caller asks and
/// priced the way every one of H2's eleven fixtures that never varied price
/// agreed on: `$3`/`$0.3`/`$3.75`/`$15` per Mtok, `quality_prior` 0.95,
/// `base_ttft_ms` 350.0, `ttft_ms_per_uncached_token` 0.002, and a
/// five-minute deterministic cache. A call site whose price, quality or TTFT
/// genuinely differs from that names the field it overrides with struct-update
/// syntax over this — `FrontierModelSpec { quality_prior: 0.6, ..frontier_spec(p,
/// m, w) }` — rather than a positional argument.
///
/// **Why not parameters, after M15's F1.** The four values that varied were
/// three bare `f64`s in a row (`quality_prior`, `base_ttft_ms`,
/// `ttft_ms_per_uncached_token`) with a `ProviderPricing` between the first
/// two — nothing in that signature stopped a call site from transposing
/// `quality_prior` and `base_ttft_ms`; the swap still type-checked and
/// silently moved both the capability gate (`quality_prior`) and the
/// router's TTFT term (`base_ttft_ms`) at once, with no diagnostic between
/// the call site and whatever assertion later noticed. A named
/// struct-update field is a compile error to misspell and a diff to
/// transpose — it cannot silently swap with its neighbor the way a bare
/// positional `f64` can.
pub fn frontier_spec(
    provider: &str,
    model: &str,
    wire_protocol: WireProtocol,
) -> FrontierModelSpec {
    FrontierModelSpec {
        provider: provider.to_string(),
        model: model.to_string(),
        wire_protocol,
        cache_model: CacheModel::Deterministic { ttl_ms: 5 * 60_000 },
        pricing: ProviderPricing {
            input_per_mtok_usd: 3.0,
            cached_input_per_mtok_usd: 0.3,
            cache_write_per_mtok_usd: 3.75,
            output_per_mtok_usd: 15.0,
        },
        quality_prior: 0.95,
        base_ttft_ms: 350.0,
        ttft_ms_per_uncached_token: 0.002,
    }
}

/// A catalog of one priced frontier model, so a turn always has somewhere to
/// go — `spec` wrapped in the one-entry [`StaticFrontierCatalog`] every
/// single-model fixture built by hand.
///
/// `tests/common/mod.rs::frontier_catalog` already names this exact shape
/// for the integration binaries that can reach it; this is the same
/// function for the two audiences that cannot — this crate's own unit
/// tests (`src/prefix_admission/tests.rs`), and a fixture whose price,
/// wire protocol, quality or TTFT genuinely needs to differ from
/// `frontier_catalog`'s own. Takes the built [`FrontierModelSpec`] rather
/// than [`frontier_spec`]'s own parameters, so a caller who needs to
/// override a field does it once, by name, at the call site — see
/// [`frontier_spec`]'s doc for why that replaced a positional tail.
pub fn single_model_catalog(spec: FrontierModelSpec) -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![spec])
}

/// `Engine::new` over this crate's local double, [`EchoLocalExecutor`]
/// answering `"local answer"`, and a plain [`AffinityPolicy`] — the two
/// things every `fn engine(...)` / `fn engine_over(...)` fixture H2
/// replaces built identically, whatever else it varied.
///
/// The frontier client stays a parameter rather than fixed to
/// [`roundhouse_fleet::EchoFrontierClient`], because one of the five
/// fixtures this replaces (`metrics_end_to_end.rs`) is parameterized over it
/// for exactly the reason a caller would still want to be: the same engine
/// shape driven by a client that answers differently per test. A fixture
/// that dispatches through more than one provider's own client
/// (`tests/provider_registry.rs`) needs `Engine::with_provider_clients`
/// instead and is not this function's shape at all — a different
/// constructor is a real variation, not boilerplate this could absorb.
pub fn engine_over_echo<S: SessionStore>(
    store: Arc<S>,
    catalog: StaticFrontierCatalog,
    frontier: Arc<dyn FrontierClient>,
    config: EngineConfig,
) -> Engine<S, ByteTokenizer> {
    Engine::new(
        store,
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        catalog,
        frontier,
        Arc::new(AffinityPolicy::new()),
        config,
    )
}

/// A TCP relay in front of a real Redis, so a store can be taken away
/// mid-test without touching a Redis the test does not own.
///
/// **Why a relay rather than a `SHUTDOWN`.** The Redis
/// `ROUNDHOUSE_TEST_REDIS_URL` names is shared with every other gated suite
/// in this workspace, and stopping it would fail them rather than the one
/// test exercising an outage; starting a second server would put a binary on
/// every such test's requirements. What an outage *is*, from this process's
/// side, is a connection that stops answering and a reconnect that is
/// refused — which is exactly what dropping the relay produces, with the
/// store itself untouched.
pub struct SeveredStore {
    /// The `redis://…` URL a ledger should connect through — the relay's own
    /// loopback address, not the upstream's.
    pub url: String,
    accept: JoinHandle<()>,
    pipes: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl SeveredStore {
    /// Start relaying to `upstream` and return the loopback address to
    /// connect through instead of it.
    pub async fn in_front_of(upstream: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback listener binds");
        let port = listener.local_addr().unwrap().port();
        // `redis://host:port/db`, which is the only shape
        // `ROUNDHOUSE_TEST_REDIS_URL` takes in this workspace. Parsed by hand
        // rather than through the `redis` crate's own URL type: that crate is
        // a dependency of `roundhouse-store-redis` only, and pulling it into
        // this crate just to parse a test fixture's own env var would be a
        // production dependency added for a test-only reason.
        let target = upstream
            .trim_start_matches("redis://")
            .split('/')
            .next()
            .expect("a redis URL names a host and a port")
            .to_string();
        let pipes: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let accept = tokio::spawn({
            let pipes = Arc::clone(&pipes);
            async move {
                loop {
                    let Ok((mut inbound, _)) = listener.accept().await else {
                        return;
                    };
                    let target = target.clone();
                    let pipe = tokio::spawn(async move {
                        let Ok(mut outbound) = TcpStream::connect(target).await else {
                            return;
                        };
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    });
                    pipes.lock().unwrap().push(pipe);
                }
            }
        });
        Self {
            url: format!("redis://127.0.0.1:{port}/"),
            accept,
            pipes,
        }
    }

    /// The outage: stop listening — so a reconnect is refused — and drop
    /// every connection already open.
    ///
    /// Dropping only the accept loop leaves any connection already piped
    /// alive and answering, which is a relay that never actually severs
    /// anything a `ConnectionManager` had already established — so both are
    /// torn down here.
    pub fn cut(&self) {
        self.accept.abort();
        for pipe in self.pipes.lock().unwrap().drain(..) {
            pipe.abort();
        }
    }
}

/// A [`DirectoryStore`] double that stalls, counts, fails and scripts writes
/// -- over the real production store, never a copy of its own.
///
/// # Why this wraps rather than re-implements (M16.0 review, F1)
///
/// `WriteBetweenReads`, `ScriptedStore` and `ArmedStore` -- one in the
/// directory suite, two of them in the same file, one in the admin HTTP
/// suite -- each hand-rolled the same `(records, version)` state and the
/// same `if state.1 != expected_version { Concurrent }` guard `commit`
/// performs. Three independent copies is already the duplication the
/// module doc these crates share warns against; the sharper defect was
/// that two of the three never had that guard driven by a test at all --
/// every fixture that reached for a double wanted `load` or `version`
/// scripted, never `commit`, so the copies existed only to be `load`ed
/// from. A commit guard nothing calls is a guard nothing protects.
///
/// So this delegates `load`, `commit` and `version` to a real
/// [`DocumentDirectoryStore`] over a [`MemoryDocumentStore`] that it owns,
/// behind a [`RwLock`] only so [`Self::set`] can swap in a fresh one -- the
/// one operation a real store's own compare-and-set cannot express, because
/// a document store's version is monotone by contract and a scripted
/// regression or an arbitrary starting version is neither. Everything else -- a stalled
/// `load`, a stalled `commit`, a failed `version`, a write that lands on a
/// scripted later read -- is scripted *around* that real store, so the
/// compare-and-set every one of them eventually reaches is the one
/// deployment ships, not a copy of it.
pub struct ScriptedDirectoryStore {
    inner: RwLock<DocumentDirectoryStore>,
    /// Every [`DirectoryStore::version`] and [`DirectoryStore::load`] call.
    /// Counted separately because the two answer different questions: one is
    /// the cheap half of a refresh and the other is the expensive half, and
    /// "how many callers paid anything at all" is the single-flight
    /// property M16.0's guards pin.
    versions: AtomicUsize,
    loads: AtomicUsize,
    /// When set, every [`DirectoryStore::version`] read moves the store on
    /// one -- a stand-in for a neighbour node committing continuously, which
    /// is what makes "did this caller refresh" observable without any
    /// blocking at all.
    moving: AtomicBool,
    /// When set, `version` answers [`StoreFailure::Unavailable`].
    version_fails: AtomicBool,
    /// When set, `load` answers [`StoreFailure::Unavailable`].
    load_fails: AtomicBool,
    /// What the next scripted write installs, and how many
    /// [`DirectoryStore::version`] calls since [`Self::arm`] it lands on --
    /// generalises `WriteBetweenReads` (which always landed on the second
    /// call ever) and `ArmedStore` (which landed on a call counted from
    /// whenever the test armed it) into the one primitive: a write that
    /// lands on the Nth `version` read since arming, however many reads that
    /// takes.
    pending: Mutex<Option<DirectoryRecords>>,
    countdown: Mutex<Option<(u64, u64)>>,
    /// Taken by the *next* `load` to arrive, which then waits on it. One
    /// load at a time, so a test can hold one refresh open and let a later
    /// one run to completion past it.
    gate: Mutex<Option<Arc<Semaphore>>>,
    /// A permit per `load` that has begun, so a test can wait for one to be
    /// in flight instead of guessing.
    entered: Semaphore,
    /// Taken by the *next* `commit` to arrive, held **after** the store's
    /// state has already been mutated and **before** the call returns to its
    /// caller -- the only place a commit-to-publish race in the directory's
    /// own `apply` can be driven under test control, since nothing else in
    /// `apply` awaits between `commit` returning and its own publish.
    commit_gate: Mutex<Option<Arc<Semaphore>>>,
    /// A permit per `commit` that has already mutated the store and is
    /// waiting on `commit_gate`.
    commit_entered: Semaphore,
}

impl ScriptedDirectoryStore {
    /// A store at `version`, holding `records` -- reached the same way a real
    /// deployment would reach it, one real [`DocumentDirectoryStore::commit`]
    /// at a time, so `records` is what the *last* of those commits carried
    /// rather than a value this double invented some other way.
    pub async fn new(records: DirectoryRecords, version: u64) -> Self {
        Self {
            inner: RwLock::new(Self::store_at(records, version).await),
            versions: AtomicUsize::new(0),
            loads: AtomicUsize::new(0),
            moving: AtomicBool::new(false),
            version_fails: AtomicBool::new(false),
            load_fails: AtomicBool::new(false),
            pending: Mutex::new(None),
            countdown: Mutex::new(None),
            gate: Mutex::new(None),
            entered: Semaphore::new(0),
            commit_gate: Mutex::new(None),
            commit_entered: Semaphore::new(0),
        }
    }

    /// A fresh [`DocumentDirectoryStore`] over an empty [`MemoryDocumentStore`],
    /// committed up to `version` -- through
    /// its own real compare-and-set, `version` times -- with `records` landing
    /// on the last of those commits. What a store `set`/`new` scripts to a
    /// version below zero commits (an empty, version-0 store) or above it
    /// looks like from a store that has genuinely been written to that many
    /// times, including a "version" lower than whatever this double answered
    /// before -- the regression topology M16.0's F3 guards need, and something
    /// no sequence of calls against a real store's own monotone `commit` could
    /// produce, which is exactly why this reaches for a fresh one rather than
    /// asking the current one to go backwards.
    async fn store_at(records: DirectoryRecords, version: u64) -> DocumentDirectoryStore {
        let store = DocumentDirectoryStore::over(Arc::new(MemoryDocumentStore::new()));
        for at in 0..version {
            let payload = if at + 1 == version {
                records.clone()
            } else {
                DirectoryRecords::default()
            };
            store
                .commit(at, payload)
                .await
                .expect("a fresh store's own commits against its own versions never conflict");
        }
        store
    }

    /// Replace the records and version outright -- what a neighbour node's
    /// write, or a restored backup, looks like from this store's side. See
    /// [`Self::store_at`] for why this is a fresh store rather than a further
    /// commit against the one already there.
    pub async fn set(&self, records: DirectoryRecords, version: u64) {
        *self.inner.write().await = Self::store_at(records, version).await;
    }

    /// Bump the wrapped store by exactly one real commit, landing `records` --
    /// the mechanics `keep_moving` and a scripted [`Self::arm`] landing both
    /// reduce to: a write that advances the version this store already has by
    /// one, through its own compare-and-set, never around it.
    async fn bump_with(&self, records: DirectoryRecords) {
        let inner = self.inner.read().await;
        if let Ok(current) = inner.load().await {
            let _ = inner.commit(current.version, records).await;
        }
    }

    /// Forget the store traffic the boot itself made.
    ///
    /// [`crate::control_config::ControlDirectory::new`] loads once to compile
    /// what it starts serving, and every count a guard asserts on is about
    /// what a *refresh* costs -- so the boot's read is subtracted here rather
    /// than added to each guard's expected number, where it would read as an
    /// unexplained off-by-one.
    pub fn forget_boot(&self) {
        self.versions.store(0, Ordering::SeqCst);
        self.loads.store(0, Ordering::SeqCst);
        while let Ok(stale) = self.entered.try_acquire() {
            stale.forget();
        }
    }

    pub fn versions(&self) -> usize {
        self.versions.load(Ordering::SeqCst)
    }

    pub fn loads(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }

    /// A neighbour that never stops writing.
    pub fn keep_moving(&self) {
        self.moving.store(true, Ordering::SeqCst);
    }

    pub fn fail_versions(&self) {
        self.version_fails.store(true, Ordering::SeqCst);
    }

    pub fn fail_loads(&self) {
        self.load_fails.store(true, Ordering::SeqCst);
    }

    /// Stage `records` to land on the `land_at`-th [`DirectoryStore::version`]
    /// call from this point on, replacing whatever this store already holds
    /// the moment it lands. `WriteBetweenReads`'s shape is `arm` called once,
    /// immediately after construction, with `land_at` fixed at `2`; `ArmedStore`'s
    /// is `arm` called whenever a test is ready, with `land_at` chosen against
    /// [`Self::reads_since_armed`] rather than a count from construction.
    pub fn arm(&self, records: DirectoryRecords, land_at: u64) {
        *self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(records);
        *self
            .countdown
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some((0, land_at));
    }

    /// How many [`DirectoryStore::version`] calls have landed since the last
    /// [`Self::arm`], `0` if never armed.
    pub fn reads_since_armed(&self) -> u64 {
        self.countdown
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .map(|(count, _)| count)
            .unwrap_or(0)
    }

    /// Hold the next `load` open. The returned handle releases it.
    ///
    /// Drains the in-flight signal first, because construction already read
    /// the store once: a test that waited on a permit left over from
    /// construction would be told a refresh was in flight before one had
    /// started, and would then race the very caller it means to observe.
    pub fn block_next_load(&self) -> Arc<Semaphore> {
        while let Ok(stale) = self.entered.try_acquire() {
            stale.forget();
        }
        let gate = Arc::new(Semaphore::new(0));
        *self.gate.lock().unwrap_or_else(|error| error.into_inner()) = Some(Arc::clone(&gate));
        gate
    }

    /// Returns once a `load` has begun. The signal, never a sleep.
    pub async fn load_in_flight(&self) {
        self.entered
            .acquire()
            .await
            .expect("the store outlives the loads it counts")
            .forget();
    }

    /// Hold the next `commit` open, *after* it has mutated the store's
    /// state. The returned handle releases it.
    pub fn block_next_commit(&self) -> Arc<Semaphore> {
        while let Ok(stale) = self.commit_entered.try_acquire() {
            stale.forget();
        }
        let gate = Arc::new(Semaphore::new(0));
        *self
            .commit_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::clone(&gate));
        gate
    }

    /// Returns once a `commit` has already mutated the store and is waiting
    /// on the gate [`Self::block_next_commit`] armed. The signal, never a
    /// sleep.
    pub async fn commit_in_flight(&self) {
        self.commit_entered
            .acquire()
            .await
            .expect("the store outlives the commits it gates")
            .forget();
    }
}

/// `compiled_under` is deliberately **not** overridden: every store this
/// double wraps is built by [`ScriptedDirectoryStore::store_at`] through
/// [`DocumentDirectoryStore::over`], which stamps the empty fingerprint, so
/// the trait's default is already the wrapped store's own answer. Overriding
/// it would need a blocking read of a `tokio` lock from a synchronous method,
/// which is a deadlock waiting for the first fixture that calls it from inside
/// the runtime.
#[async_trait::async_trait]
impl DirectoryStore for ScriptedDirectoryStore {
    async fn load(&self) -> Result<VersionedRecords, StoreFailure> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        if self.load_fails.load(Ordering::SeqCst) {
            return Err(StoreFailure::Unavailable(
                "the scripted store is refusing load reads".into(),
            ));
        }
        let gate = self
            .gate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        // Read before the gate, never after. A load held open is a request
        // already in flight, and what comes back is what the wrapped store
        // held when it was made -- not what a commit that landed while it was
        // open changed it to. Reading after the gate would make every blocked
        // load answer with the newest records, which is the one thing the
        // out-of-order publish guard needs it not to do.
        let answer = {
            let inner = self.inner.read().await;
            inner.load().await?
        };
        self.entered.add_permits(1);
        if let Some(gate) = gate {
            gate.acquire()
                .await
                .expect("the gate outlives the load waiting on it")
                .forget();
        }
        Ok(answer)
    }

    async fn commit(
        &self,
        expected_version: u64,
        records: DirectoryRecords,
    ) -> Result<u64, StoreFailure> {
        // The wrapped store's own compare-and-set decides Concurrent-or-not
        // and, on success, is already mutated by the time this returns -- so
        // a caller that reads the store from here on (a concurrent refresh's
        // `version`/`load`) sees this commit's own result, not a stale one.
        // That is what makes the gate below able to hold a *successful*
        // commit open long enough for another writer to publish past it
        // first, the seam the directory's own commit-to-publish race needs.
        let new_version = {
            let inner = self.inner.read().await;
            inner.commit(expected_version, records).await?
        };
        let gate = self
            .commit_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        self.commit_entered.add_permits(1);
        if let Some(gate) = gate {
            gate.acquire()
                .await
                .expect("the gate outlives the commit waiting on it")
                .forget();
        }
        Ok(new_version)
    }

    async fn version(&self) -> Result<u64, StoreFailure> {
        self.versions.fetch_add(1, Ordering::SeqCst);
        if self.version_fails.load(Ordering::SeqCst) {
            return Err(StoreFailure::Unavailable(
                "the scripted store is refusing version reads".into(),
            ));
        }
        let landing = {
            let mut countdown = self
                .countdown
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            match countdown.as_mut() {
                Some((count, land_at)) => {
                    *count += 1;
                    if *count == *land_at {
                        self.pending
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .take()
                    } else {
                        None
                    }
                }
                None => None,
            }
        };
        match landing {
            Some(records) => self.bump_with(records).await,
            None if self.moving.load(Ordering::SeqCst) => {
                let current = {
                    let inner = self.inner.read().await;
                    inner.load().await?
                };
                self.bump_with(current.records).await;
            }
            None => {}
        }
        let inner = self.inner.read().await;
        inner.version().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Control for the F1 guard below: a named struct-update override round
    /// trips into the built spec exactly, and the fields that were not
    /// named keep `frontier_spec`'s defaults. Keeping this live is what
    /// proves the guard is checking a real shape property, not failing for
    /// an unrelated reason.
    #[test]
    fn correctly_ordered_quality_prior_and_base_ttft_ms_round_trip() {
        let correct = single_model_catalog(FrontierModelSpec {
            quality_prior: 0.6,
            base_ttft_ms: 900.0,
            ..frontier_spec("anthropic", "claude", WireProtocol::AnthropicMessages)
        });
        let spec = &correct.models()[0];
        assert_eq!(spec.quality_prior, 0.6);
        assert_eq!(spec.base_ttft_ms, 900.0);
        // Untouched fields keep frontier_spec's own defaults — the whole
        // point of struct-update over a positional tail: naming the two
        // fields that vary does not require restating the rest.
        assert_eq!(spec.ttft_ms_per_uncached_token, 0.002);
    }

    /// M15 review, F1: `frontier_spec`/`single_model_catalog` used to take
    /// `quality_prior`, `base_ttft_ms` and `ttft_ms_per_uncached_token` as
    /// three positional, unlabeled `f64` parameters, so a call site that
    /// transposed `quality_prior` and `base_ttft_ms` type-checked and
    /// silently moved both the capability gate and the router's TTFT term.
    ///
    /// The fix removed the positional tail rather than validating it: a
    /// transposed *value* can still be wrong (0.6 really was meant for one
    /// field or the other), but it can no longer be wrong from the shape of
    /// the call alone. That is a static property of the signature, not a
    /// runtime one — the old ignored test's demonstration (an out-of-range
    /// `quality_prior` slipping through unvalidated) has no equivalent to
    /// swap in any more, because there is no longer a positional pair to
    /// transpose. What replaces it is this: read the source and assert
    /// neither helper declares an `f64` parameter, so a positional tail
    /// cannot come back unnoticed in a later edit.
    #[test]
    fn frontier_spec_and_single_model_catalog_take_no_positional_f64_parameters() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let source =
            std::fs::read_to_string(std::path::Path::new(manifest_dir).join("src/test_support.rs"))
                .expect("this crate's own test_support.rs must be readable from its own test");

        for signature in ["fn frontier_spec(", "fn single_model_catalog("] {
            let start = source
                .find(signature)
                .unwrap_or_else(|| panic!("test_support.rs should declare {signature}"));
            let params_end = source[start..]
                .find(") ->")
                .unwrap_or_else(|| panic!("{signature} should have a return type"));
            let params = &source[start..start + params_end];
            assert!(
                !params.contains("f64"),
                "{signature} declares a bare f64 parameter again: {params:?} — \
                 F1 replaced the positional quality_prior/base_ttft_ms/\
                 ttft_ms_per_uncached_token tail with named struct-update \
                 overrides on FrontierModelSpec precisely because a \
                 positional f64 pair type-checks when transposed and moves \
                 the capability gate and the router's TTFT term silently"
            );
        }
    }
}
