// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which state a deployment shares between its nodes, and the one rule that
//! decides.
//!
//! **In the library rather than in `main.rs`, and that placement is the
//! finding.** M13 shipped this rule as a private function of the binary, so
//! the only thing that could check it was a unit test over booleans; the
//! end-to-end question — does a ceiling an operator adds through the admin
//! plane at 3pm actually reach the Redis this deployment names? — had nowhere
//! to be asked from, and the answer was "no" for a whole class of deployment
//! without a single test going red. `tests/fair_use_backend_boot.rs` asks it
//! now, against a real Redis, by calling *this* function rather than
//! re-deriving its rule — which is the same lesson M13's own refute pass
//! learned about the boot warning one layer down.
//!
//! **It was `fair_use_backend` until M14.1, and the rename is the point**
//! (R-C4). The correlation maps became the fourth family chosen by
//! `ROUNDHOUSE_REDIS_URL`, beside the session log, the spend ledger and the
//! fair-use buckets, and a second function spelling the same predicate is how
//! a deployment ends up with three of the four shared. One name, one rule, and
//! every caller reads it.
//!
//! **And since M14.1's thermo-nuclear review, F1, one *caller*.** Naming the
//! rule once was not enough: the composition root evaluated it twice and
//! matched the raw `Option` a third time, and the wiring each of those three
//! matches chose — which ledger, which maps, which store — stayed inside a
//! `[[bin]]` with no `[lib]` counterpart, where nothing outside it could call
//! it. The two boot suites could only re-type the match by hand and rely on a
//! reviewer noticing when the copies drifted; a mutation that wired the
//! per-process correlation table into the *shared* arm left the whole
//! workspace green. [`open`] is where the families are built now, in one
//! match, so a mutation of the wiring is a mutation of the thing the boot
//! suites actually run.
//!
//! **And since M16.1 (R-D8) there are five.** The admin directory joined the
//! four as an opaque document store, through the same one switch, because it
//! is the family whose absence the other four made worst: a deployment with a
//! durable ledger and a per-process directory re-grants an archived project's
//! id on the next restart and silently joins the new tenant to the old one's
//! spend. `tests/directory_backend_boot.rs` is its end-to-end assertion, in
//! the shape the fair-use and correlation families already had.

use std::sync::Arc;

use anyhow::Context;
use roundhouse_core::control::{
    DocumentStore, FairUseLedger, MemoryDocumentStore, MemoryFairUseLedger, MemorySpendLedger,
    SpendLedger,
};
use roundhouse_core::store::MemoryStore;
use roundhouse_store_redis::{
    EmptyNamespace, KeyNamespace, RedisCorrelationMaps, RedisDocumentStore, RedisFairUseLedger,
    RedisSessionStore, RedisSpendLedger,
};

use crate::Conversations;

/// Where sessions, committed spend, fair-use windows, conversation correlation
/// and the admin directory live, as a `redis://` URL. Absent means this
/// process's memory.
///
/// Named in this module rather than in the binary because this is the module
/// that reads it and quotes it: every boot line and every connect failure
/// below names the variable an operator would act on, and a second spelling
/// somewhere else is a message pointing at a variable the process never
/// consulted.
pub const REDIS_VAR: &str = "ROUNDHOUSE_REDIS_URL";

/// The deployment namespace every shared key is built under (M14.2, R-S3).
/// Absent means the default `KeyNamespace` (`rh`); set-but-empty is refused
/// at boot rather than silently read as absent — an operator who typed the
/// variable meant *something* by it, and treating a blank value as "no
/// namespace" would answer both cases with the same key an unnamespaced
/// deployment already uses, silently reuniting two deployments an operator
/// meant to keep apart.
///
/// Read here whether or not [`REDIS_VAR`] is set: a deployment with no Redis
/// has no keys for a namespace to separate, but a typo an operator will not
/// discover until the day they add a Redis is worth refusing at the boot
/// that introduced it.
pub const REDIS_NAMESPACE_VAR: &str = "ROUNDHOUSE_REDIS_NAMESPACE";

/// Resolve [`REDIS_NAMESPACE_VAR`]'s value into a [`KeyNamespace`].
///
/// A pure function of the one string the environment might hold, for
/// [`shared_backend`]'s own testability reason: a boot-time refusal is the
/// load-bearing half of "an empty namespace stops the process", and a test
/// that had to set process-wide environment to reach it would race every
/// other test in the binary.
pub fn resolve_namespace(raw: Option<&str>) -> Result<KeyNamespace, EmptyNamespace> {
    match raw {
        Some(raw) => KeyNamespace::new(raw),
        None => Ok(KeyNamespace::default()),
    }
}

/// Where a deployment's shared state lives.
///
/// Two variants rather than a `bool` because the shared arm carries the URL it
/// was chosen by: a bare `bool` would leave the composition root holding
/// "shared" and an `Option<String>` it had to unwrap again, and the two
/// disagreeing is exactly the boot-time state this rung exists to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedBackend<'a> {
    /// The state every node of the deployment shares, in the Redis named by
    /// this URL.
    Shared { url: &'a str },
    /// This process's own memory.
    PerProcess,
}

/// The same rule that chooses a session store and a spend ledger, and *only*
/// that rule.
///
/// `ROUNDHOUSE_REDIS_URL` is the variable an operator sets when they mean
/// "this is more than one process", and it is exactly then that a per-process
/// rolling counter stops being the ceiling its configuration says it is, and
/// that a per-process correlation table stops being able to say which
/// conversation a control call is about. So Redis selects the shared buckets
/// and the shared maps, the same way it selects the durable log and the
/// durable ledger, and for the same reason.
///
/// **It used to take a second argument — whether any membership had a
/// `fair_use` block — and that was the M13 thermo-nuclear review's F1.** The
/// argument could only ever be a *boot-time snapshot* of an axis the admin
/// plane patches at runtime (`patch_project` writes a `fair_use` block onto a
/// live project, and the engine reads the recompiled plane on the very next
/// request), so a deployment that booted with a Redis and no ceiling anywhere
/// enforced every later-added ceiling in one node's memory for the rest of the
/// process's life, with nothing in Redis and no warning. The stated
/// justification for the argument — that connecting a third handle could make
/// an unreachable Redis fail the boot of a deployment with no ceiling to fail
/// about — was false besides: `RedisSessionStore::connect` on the same URL
/// already fails that boot, several lines earlier.
///
/// What is left is one connection per family per deployment that named a
/// Redis, which is what naming one already means for the backends beside them.
///
/// [`open`] is the one production caller; a second one is how the four
/// families start disagreeing again (M14.1 review, F1).
pub fn shared_backend(redis_url: Option<&str>) -> SharedBackend<'_> {
    match redis_url {
        Some(url) => SharedBackend::Shared { url },
        None => SharedBackend::PerProcess,
    }
}

/// Every backend a deployment names at once, opened together and logged once.
///
/// **A two-arm enum rather than five separately-resolved handles**, because
/// separate handles is what the composition root had when M14.1's review found
/// F1: nothing in the type system said the fair-use ledger, the correlation
/// maps, the session store and the spend ledger had all answered the same
/// question, and in fact three separate evaluations of that question were what
/// produced them. Here the question is asked once and the answer is the value.
///
/// The store is concrete in each arm rather than an `Arc<dyn SessionStore>`
/// because the binary's `serve` is generic over it — erasing it here would buy
/// one fewer `match` arm at the callsite by adding a vtable hop to the hottest
/// path in the process.
pub enum Backends {
    /// Every family in the Redis this deployment named. A second node opening
    /// the same URL sees everything this one writes.
    Shared {
        /// The URL all five were opened against.
        ///
        /// Carried rather than re-read, so a caller that needs to name the
        /// Redis cannot name a *different* one than the handles beside it were
        /// opened on — the two-facts-that-can-disagree shape this module
        /// exists to remove, one level up.
        url: String,
        store: Arc<RedisSessionStore>,
        spend: Arc<dyn SpendLedger>,
        fair_use: Arc<dyn FairUseLedger>,
        conversations: Arc<Conversations>,
        /// The opaque document the admin directory is stored as (M16.1,
        /// R-D8). An `Arc<dyn DocumentStore>` and not the typed directory,
        /// because this crate's config vocabulary has no business in the
        /// choice of backend — the composition root wraps whichever of these
        /// two it is handed in the one adapter that knows how to serialize
        /// records (R-D5, R-D7).
        directory: Arc<dyn DocumentStore>,
    },
    /// Every family in this process's own memory, dying with it.
    PerProcess {
        store: Arc<MemoryStore>,
        spend: Arc<dyn SpendLedger>,
        fair_use: Arc<dyn FairUseLedger>,
        conversations: Arc<Conversations>,
        directory: Arc<dyn DocumentStore>,
    },
}

impl Backends {
    /// The fair-use ledger, whichever arm this is.
    ///
    /// An accessor for the families whose *type* is the same in both arms, so
    /// a caller that wants only one of them does not re-type the match to get
    /// at it. Re-typed matches are what F1 was about: two of them, in the two
    /// boot suites, kept passing while the wiring they mirrored was mutated.
    pub fn fair_use(&self) -> &Arc<dyn FairUseLedger> {
        match self {
            Backends::Shared { fair_use, .. } | Backends::PerProcess { fair_use, .. } => fair_use,
        }
    }

    /// The conversation correlator, whichever arm this is. See
    /// [`fair_use`](Self::fair_use) for why this is an accessor.
    pub fn conversations(&self) -> &Arc<Conversations> {
        match self {
            Backends::Shared { conversations, .. } | Backends::PerProcess { conversations, .. } => {
                conversations
            }
        }
    }

    /// The document the admin directory lives in, whichever arm this is (M16.1,
    /// R-D8). See [`fair_use`](Self::fair_use) for why this is an accessor.
    ///
    /// The composition root reads it *here*, before the match that wires the
    /// store and the ledger, because the directory is built before either:
    /// its first load is the boot check, and a Redis that serves the other
    /// four families and cannot answer for this one has to stop the process
    /// rather than be discovered on the first admin call.
    pub fn directory(&self) -> &Arc<dyn DocumentStore> {
        match self {
            Backends::Shared { directory, .. } | Backends::PerProcess { directory, .. } => {
                directory
            }
        }
    }
}

/// Open every backend [`shared_backend`] chose, in one match.
///
/// **This is the composition root's whole decision**, moved out of the binary
/// by M14.1's review (F1) so that something can call it. What lives in
/// `main.rs` now is the wiring of what this returns, and nothing that could
/// disagree with it.
///
/// The fair-use ledger is connected first on purpose: it is the first thing
/// that touches the named Redis, so an unreachable one fails the boot with the
/// message that names [`REDIS_VAR`] rather than four lines further down. Every
/// connect's failure names that variable, because it is the part an operator
/// acts on.
///
/// The correlation maps are wrapped in [`Conversations`] here rather than
/// handed back raw and wrapped by the caller: *which maps a `Conversations` is
/// over* is precisely the decision F1 found untested, and returning the maps
/// would leave that last step back in the binary where it started.
///
/// One boot line per arm, naming every family, rather than one line per
/// family: a deployment reading four separate "shared in Redis" lines cannot
/// tell whether the fifth is missing because it is per-process or because it
/// was never logged.
///
/// `namespace` is taken already resolved rather than as a raw `Option<&str>`
/// this function validates itself: [`resolve_namespace`]'s refusal is a boot
/// error the caller reports before ever reaching here, the same shape
/// [`REDIS_VAR`] already has by being read once, in `main.rs`, and handed in.
pub async fn open(redis_url: Option<&str>, namespace: &KeyNamespace) -> anyhow::Result<Backends> {
    match shared_backend(redis_url) {
        SharedBackend::Shared { url } => {
            let fair_use = RedisFairUseLedger::connect_namespaced(url, namespace.clone())
                .await
                .with_context(|| {
                    format!("opening the fair-use ledger in the Redis named by {REDIS_VAR}")
                })?;
            let maps = RedisCorrelationMaps::connect_namespaced(url, namespace.clone())
                .await
                .with_context(|| {
                    format!("opening the correlation maps in the Redis named by {REDIS_VAR}")
                })?;
            let store = RedisSessionStore::connect_namespaced(url, namespace.clone())
                .await
                .with_context(|| format!("connecting to the Redis named by {REDIS_VAR}"))?;
            let spend = RedisSpendLedger::connect_namespaced(url, namespace.clone())
                .await
                .with_context(|| {
                    format!("opening the spend ledger in the Redis named by {REDIS_VAR}")
                })?;
            // The fifth family (M16.1, R-D8), in the same match as the other
            // four and for the reason the module doc gives about the first
            // four: a deployment whose sessions, spend, ceilings and threads
            // are shared and whose *tenancy* is not is the shape M8's own
            // deferral note described, where an archived project's tombstone
            // dies with a process while the ledger row that gives its id
            // meaning does not.
            //
            // `map_err` rather than `with_context`: `DocumentStoreError` is
            // this seam's own error and not an `anyhow` chain, and the
            // sentence an operator acts on is the one naming the variable.
            let directory = RedisDocumentStore::connect_namespaced(url, namespace.clone())
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "opening the admin directory in the Redis named by {REDIS_VAR}: {error}"
                    )
                })?;
            // The URL itself is never logged -- a `redis://` URL may carry
            // credentials.
            tracing::info!(
                var = REDIS_VAR,
                "sessions, committed spend, fair-use windows, conversation correlation and \
                 admin-created tenancy are all shared in the Redis this deployment names: \
                 every node serving a project shares one rolling ceiling, a cache key, a \
                 tool call or a client thread bound by one node resolves on every node, and \
                 a project, member or key created through the admin plane outlives this \
                 process and reaches every other node"
            );
            Ok(Backends::Shared {
                url: url.to_string(),
                store: Arc::new(store),
                spend: Arc::new(spend),
                fair_use: Arc::new(fair_use),
                conversations: Arc::new(Conversations::over(Arc::new(maps))),
                directory: Arc::new(directory),
            })
        }
        SharedBackend::PerProcess => {
            tracing::warn!(
                var = REDIS_VAR,
                "no Redis configured; sessions and committed spend are in-memory and die \
                 with this process, admin-created projects, members and keys die with it \
                 too -- including the tombstone that keeps an archived project's id \
                 retired -- a fair-use ceiling configured here or added later through the \
                 admin plane is enforced per node and says so when it first enforces one, \
                 and a control call landing on a node that served none of its \
                 conversation's turns falls back to a guess or refuses"
            );
            Ok(Backends::PerProcess {
                store: Arc::new(MemoryStore::new()),
                spend: Arc::new(MemorySpendLedger::new()),
                fair_use: Arc::new(MemoryFairUseLedger::new()),
                conversations: Arc::new(Conversations::new()),
                directory: Arc::new(MemoryDocumentStore::new()),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Naming a Redis is the whole rule, and nothing else is.**
    ///
    /// The cell that changed in the M13 review is the first one: a deployment
    /// with a Redis and no configured window used to get `PerProcess`, which
    /// is what let a window PATCHed in an hour later be counted in one node's
    /// memory. There is no "configured" axis left to tabulate against — that
    /// is the fix — so the interesting assertion is that the URL survives the
    /// choice, since a `Shared` that had to be paired with the URL again at
    /// the call site is the two-facts-that-can-disagree shape over again.
    #[test]
    fn naming_a_redis_selects_the_shared_state_and_carries_its_url() {
        assert_eq!(
            shared_backend(Some("redis://127.0.0.1:6379/")),
            SharedBackend::Shared {
                url: "redis://127.0.0.1:6379/"
            }
        );
        assert_eq!(shared_backend(None), SharedBackend::PerProcess);
    }

    /// **M14.2, R-S3: absent means the default, set-but-empty is refused.**
    ///
    /// Not "absent or blank both mean default": an operator who set the
    /// variable to an empty or whitespace-only string meant *something* by
    /// it, and reading that as "no namespace" would silently reunite two
    /// deployments an empty-string typo was never supposed to be able to
    /// join — see [`resolve_namespace`]'s own doc.
    #[test]
    fn an_unset_namespace_is_the_default_and_an_empty_one_is_refused() {
        assert_eq!(resolve_namespace(None).unwrap(), KeyNamespace::default());
        assert_eq!(
            resolve_namespace(Some("acme-prod")).unwrap(),
            KeyNamespace::new("acme-prod").unwrap()
        );
        assert!(resolve_namespace(Some("")).is_err());
        assert!(resolve_namespace(Some("   ")).is_err());
    }
}
