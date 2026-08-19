// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Roundhouse binary.
//!
//! Deliberately thin: everything interesting is a seam, and this only chooses
//! which implementation of each seam to instantiate. Configuration is one
//! environment variable, because a flag parser here would be the first place a
//! deployment concern leaked into the composition root.
//!
//! Durability is the one seam a deployment selects here, and it selects both
//! halves of it at once: `ROUNDHOUSE_REDIS_URL` set means sessions *and*
//! committed spend live in that Redis and survive this process; absent means
//! [`MemoryStore`] and [`MemorySpendLedger`], both of which die with it. A URL
//! that is set but unreachable stops the process at startup — falling back to
//! memory would silently demote "durable" to "until the next restart", which
//! is the one property the variable exists to promise, and it would demote it
//! for the ledger too: a process that forgets a month's spend on restart hands
//! the budget back while the log that proves it was spent survives.
//!
//! The rest of the wiring is the offline demo set — [`ByteTokenizer`],
//! [`EchoLocalExecutor`], [`EchoFrontierClient`] — so the process serves with
//! no GPU, no provider account and no model assets.
//!
//! The catalog carries one entry, for the echo provider wired below — not a
//! rate card. The no-baked-rate-cards rule (`roundhouse-fleet/src/frontier.rs`)
//! is about real providers, whose prices and TTLs are deployment facts that go
//! stale in source; the echo provider is this binary's own stub, its "pricing"
//! is free, and without any entry there would be nothing to route to and every
//! turn would terminate `response_incomplete` — a demo that cannot demo.
//! `ROUNDHOUSE_CATALOG` is how a real deployment replaces it: see
//! [`catalog_config`](roundhouse_server::catalog_config).
//!
//! Under the stub every dollar figure on the dashboard is zero, which is
//! correct — nothing was billed — and is why the offline demo is a demo of the
//! token breakdown rather than of the savings.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{MemorySpendLedger, SpendLedger, TurnBudget};
use roundhouse_core::metrics::MetricsConfig;
use roundhouse_core::routing::{
    AffinityPolicy, CacheLedger, CacheModel, Candidate, ProviderPricing,
};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierModelSpec, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_mcp::ControlStore;
use roundhouse_server::{
    Admission, ControlPlane, ControlPlaneReads, Conversations, EchoLocalExecutor, Engine,
    EngineConfig, catalog_config, control_config, http, mcp_api, metrics_api, responses_api,
};
use roundhouse_store_redis::{RedisSessionStore, RedisSpendLedger};
use tracing_subscriber::EnvFilter;

/// The echo provider's catalog entry.
///
/// Free and deterministically cached: the numbers describe the stub they price,
/// not any real provider.
fn echo_catalog() -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![FrontierModelSpec {
        provider: "echo".into(),
        model: "echo".into(),
        wire_protocol: WireProtocol::OpenAiChatCompletions,
        cache_model: CacheModel::Deterministic { ttl_ms: 5 * 60_000 },
        pricing: ProviderPricing::free(),
        quality_prior: 0.5,
        base_ttft_ms: 1.0,
        ttft_ms_per_uncached_token: 0.0,
    }])
}

/// Where to bind, as `host:port`.
const ADDR_VAR: &str = "ROUNDHOUSE_ADDR";
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

/// Where sessions live, as a `redis://` URL. Absent means in-memory.
const REDIS_VAR: &str = "ROUNDHOUSE_REDIS_URL";

/// Prompt shape the startup cross-check quotes the catalog under.
///
/// Nominal, and it does not affect the answer: [`TurnPolicy::permits`] reads a
/// candidate's target identity and its quality prior, and a quote's length
/// moves neither. It is here so the check goes through the same quoting path
/// the router does rather than assembling candidates by hand.
///
/// [`TurnPolicy::permits`]: roundhouse_core::control::TurnPolicy::permits
const PROBE_ISL_TOKENS: u64 = 1_024;
const PROBE_OSL_TOKENS: u64 = 256;

/// Every target a turn of this process's could actually be routed to, priced
/// the way the router prices them.
///
/// The catalog and nothing else, because [`serve`] wires no [`LocalFleet`]:
/// this binary quotes no local candidate, so `local/<model>` names nothing it
/// could send a turn to and a policy that named only local really would refuse
/// every turn. A deployment that attaches a fleet adds its local model to this
/// list at the same site it attaches the fleet — the two facts have to move
/// together, or the check starts refusing configurations that would in fact
/// serve.
///
/// [`LocalFleet`]: roundhouse_fleet::LocalFleet
fn reachable_candidates(catalog: &StaticFrontierCatalog) -> Vec<Candidate> {
    let mut ledger = CacheLedger::new();
    catalog.apply_to_ledger(&mut ledger);
    catalog.quote(
        &ledger,
        roundhouse_core::now_ms(),
        PROBE_ISL_TOKENS,
        PROBE_OSL_TOKENS,
    )
}

/// Refuse to serve a key whose policy admits nothing this process can route to.
///
/// The catalog and the control plane are separate files, so neither loader can
/// see the other: a `TargetFilter` cannot tell at parse time that its patterns
/// name no model, and a quality floor cannot tell that it sits above every
/// model in the catalog. Here both are loaded, which makes this the one place
/// the question can be asked.
///
/// Asking it is the same load-or-die posture both loaders already take. A
/// policy that admits nothing does not degrade — every turn it serves ends in
/// `policy_refused` — so starting anyway would turn one mistyped pattern into
/// a tenant whose every request fails, discovered by the tenant.
///
/// Per key rather than per project, and that is not a shortcut: a key's
/// effective policy is its project's narrowed by its own overrides, so a
/// project whose filter is fine can still hold a key whose override intersects
/// it down to nothing — and a turn arrives on a key.
///
/// The question is [`TurnPolicy::permits`] and deliberately not
/// [`TurnPolicy::admits`]: this asks whether a target is reachable *at all*
/// under the policy's history-independent axes, and a cadence-rationed model
/// is reachable on some turns. Feeding `admits` a synthetic unspent window to
/// get the same answer is how this used to be written, and it left the reader
/// to work out from a fabricated [`FrontierHistory`] which question was being
/// asked. What a *spent* window leaves is the separate question
/// [`refuse_promises_of_a_local_fallback`] asks, one call below.
///
/// [`TurnPolicy::permits`]: roundhouse_core::control::TurnPolicy::permits
/// [`TurnPolicy::admits`]: roundhouse_core::control::TurnPolicy::admits
/// [`FrontierHistory`]: roundhouse_core::control::FrontierHistory
fn refuse_policies_that_admit_nothing(
    plane: &ControlPlane,
    reachable: &[Candidate],
) -> anyhow::Result<()> {
    // Collected and sorted rather than reported on the first hit: the table is
    // a hash map, so a deployment with two bad entries would otherwise be told
    // about a different one on each restart. `configured_admissions` yields
    // nothing in open mode, which is the accurate answer — every request there
    // resolves to the unrestricted policy, and there is nothing to disagree
    // with.
    let mut refused: Vec<String> = plane
        .configured_admissions()
        .filter(|admission| {
            !reachable
                .iter()
                .any(|candidate| admission.policy.permits(candidate))
        })
        .map(describe)
        .collect();
    refused.sort();
    if !refused.is_empty() {
        anyhow::bail!(
            "these control-plane keys admit none of the {} model(s) this deployment can route to, \
             so every one of their turns would fail: {}",
            reachable.len(),
            refused.join("; ")
        );
    }
    Ok(())
}

/// How a refusal names the key an operator has to go and edit.
///
/// One spelling for both checks below. A digest tells an operator that two
/// keys differ and never which one they mistyped, so the patterns go in beside
/// it.
fn describe(admission: &Admission) -> String {
    format!(
        "project `{}`, user `{}` (policy {}, allow {})",
        admission.principal.project,
        admission.principal.user,
        admission.policy.digest(),
        admission.policy.allow,
    )
}

/// What a [`FrontierCadence`] promises about a window it has spent.
///
/// [`FrontierCadence`]: roundhouse_core::control::FrontierCadence
const CADENCE_PROMISE: &str =
    "its frontier_cadence promises that a spent window serves locally instead of failing";

/// What a degrade-mode [`Budget`] with the overflow valve off promises about a
/// limit it has spent.
///
/// [`Budget`]: roundhouse_core::control::Budget
const BUDGET_PROMISE: &str = "its budget degrades to local with overflow_when_local_saturated off, \
     which promises that an exhausted budget serves locally instead of failing";

/// Every promise this key's configuration makes about a *spent* allowance that
/// this deployment cannot keep.
///
/// **Two configurations, one promise, one check.** A cadence spends a
/// per-session ration and a degrade-mode budget spends money, but both say the
/// same sentence when their allowance runs out — *the hosted options go
/// inadmissible and the turn serves locally instead of failing* — and both say
/// it in a file that cannot see a fleet. Whether the sentence is true depends
/// on one fact either way: is anything this key may reach still admissible
/// once the allowance is gone? Asking it twice, in two functions with two
/// lookalike error messages, would be two spellings of one question, and the
/// second one is where the answers start to differ.
///
/// Each promise is asked in the vocabulary of the thing that made it, through
/// the same predicate the router will apply at runtime rather than a
/// restatement of it — [`TurnPolicy::admits_when_spent`] for the cadence, and
/// [`TurnBudget::exhausted`] plus [`TurnPolicy::permits`] for the budget.
/// A key that makes neither promise is asked nothing, which is why a
/// deployment with no cadences and no budgets is unaffected by this check.
///
/// The budget half asks `permits` and not `admits_when_spent`: an exhausted
/// budget and a spent cadence are separate allowances, and a key that has run
/// out of one has not necessarily run out of the other. Where a key really does
/// exhaust both, the cadence half of this same list has already refused it.
///
/// [`TurnPolicy::admits_when_spent`]: roundhouse_core::control::TurnPolicy::admits_when_spent
/// [`TurnPolicy::permits`]: roundhouse_core::control::TurnPolicy::permits
fn unkeepable_promises(admission: &Admission, reachable: &[Candidate]) -> Vec<&'static str> {
    let mut broken = Vec::new();
    if admission.policy.frontier_cadence.is_some()
        && !reachable
            .iter()
            .any(|candidate| admission.policy.admits_when_spent(candidate))
    {
        broken.push(CADENCE_PROMISE);
    }
    if let Some(terms) = &admission.budget {
        // Only one exhaustion setting promises local service at all: `Refuse`
        // never made the promise, and the valve keeps it on frontier. See
        // `Exhaustion::promises_local_service`.
        if terms.budget.on_exhaustion.promises_local_service() {
            let spent = TurnBudget::exhausted(terms.budget.on_exhaustion);
            if !reachable
                .iter()
                .any(|candidate| admission.policy.permits(candidate) && spent.admits(candidate))
            {
                broken.push(BUDGET_PROMISE);
            }
        }
    }
    broken
}

/// Refuse to serve a key that promises a local fallback this deployment cannot
/// provide.
///
/// The promise is checked where the fleet is finally visible, which is the
/// same place [`refuse_policies_that_admit_nothing`] checks the other half.
/// Those two stay separate functions because they are separate questions with
/// separate remedies: that one says "this policy names nothing at all", this
/// one says "this configuration names something for as long as an allowance
/// lasts". Reported together would leave an operator unsure which sentence to
/// go and edit. What is *not* separate is the pair of promises inside this
/// one — see [`unkeepable_promises`].
fn refuse_promises_of_a_local_fallback(
    plane: &ControlPlane,
    reachable: &[Candidate],
) -> anyhow::Result<()> {
    let mut refused: Vec<String> = plane
        .configured_admissions()
        .filter_map(|admission| {
            let broken = unkeepable_promises(admission, reachable);
            (!broken.is_empty())
                .then(|| format!("{} — {}", describe(admission), broken.join("; and ")))
        })
        .collect();
    refused.sort();
    if !refused.is_empty() {
        anyhow::bail!(
            "these control-plane keys promise that a spent allowance serves locally; this \
             deployment has no local capacity to serve it, so their turns would fail instead \
             of degrading: {}",
            refused.join(" | ")
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // A catalog that was named but cannot be read stops the process. Falling
    // back to the free stub would serve every turn under prices nobody chose
    // and then report savings against them.
    let config = catalog_config::from_env()?;
    let (catalog, metrics_config) = match &config {
        Some(config) => {
            tracing::info!(models = config.models.len(), "catalog loaded");
            (config.catalog(), config.metrics_config())
        }
        None => {
            tracing::warn!(
                var = catalog_config::CATALOG_VAR,
                "no catalog configured; serving the offline echo stub, \
                 for which every price is zero"
            );
            let catalog = echo_catalog();
            let pricing = catalog.shadow_pricing();
            (catalog, MetricsConfig::new(pricing))
        }
    };
    let metrics_config = Arc::new(metrics_config);

    // Same posture as the catalog, and for a sharper reason: a control plane
    // that was named but cannot be read stops the process, because starting
    // anyway would serve every request as if no key were required — the exact
    // failure the variable is set to prevent — and would do it silently, with
    // every tenant's turns landing in one unnamespaced session space.
    let plane = Arc::new(ControlPlane::from_env()?);
    match &*plane {
        // Counted through the accessor rather than by reaching into
        // `Configured { turn_keys, .. }`: the table's layout has exactly one
        // reader outside its own module, and this is not going to be the
        // second one for the sake of a log line.
        ControlPlane::Configured { .. } => tracing::info!(
            memberships = plane.configured_admissions().count(),
            var = control_config::CONTROL_PLANE_VAR,
            "control plane loaded; a key is required on every surface"
        ),
        ControlPlane::Open => tracing::warn!(
            var = control_config::CONTROL_PLANE_VAR,
            "no control plane configured; every request is served as the built-in \
             default/default membership, with no key and no session namespace"
        ),
    }

    // Both files are loaded now, and only now can they be compared. See the
    // functions: neither loader can see the other, so a policy naming no model
    // this deployment has — or promising a local fallback it does not have —
    // is a mistake nothing before this point could catch.
    let reachable = reachable_candidates(&catalog);
    refuse_policies_that_admit_nothing(&plane, &reachable)?;
    refuse_promises_of_a_local_fallback(&plane, &reachable)?;
    // The third cross-check, and the one this deployment's *control surface*
    // needs: that surface answers entitlement questions by principal, and the
    // config lets two keys name one membership with different overrides. A
    // deployment where they disagree would tell an agent about a policy its own
    // key does not have — so it is refused here, where an operator reads it,
    // rather than discovered by a tenant. See `ControlPlane::membership`.
    if let Some(refusal) = mcp_api::describe_ambiguous_memberships(&plane) {
        anyhow::bail!(refusal);
    }

    let addr: SocketAddr = std::env::var(ADDR_VAR)
        .unwrap_or_else(|_| DEFAULT_ADDR.to_string())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    // The bound address, not the requested one: binding to port 0 is how a test
    // harness or a container asks the OS to pick, and only this side knows what
    // it picked.
    tracing::info!(addr = %listener.local_addr()?, "roundhouse listening");
    tracing::info!(
        "dashboard at http://{}/v1/metrics/dashboard",
        listener.local_addr()?
    );

    // The two arms monomorphize `serve` twice; that is the entire cost of
    // keeping the engine generic over its store. The URL itself is never
    // logged — a `redis://` URL may carry credentials.
    //
    // One variable selects *both* durable backends, and they are chosen
    // together on purpose. The session log and the spend ledger answer two
    // questions about the same turns, and a deployment that made one durable
    // and left the other in memory would re-grant its whole budget on every
    // restart while the log that proves it was already spent survives.
    match std::env::var(REDIS_VAR) {
        Ok(url) => {
            let store = RedisSessionStore::connect(&url)
                .await
                .with_context(|| format!("connecting to the Redis named by {REDIS_VAR}"))?;
            let spend = RedisSpendLedger::connect(&url).await.with_context(|| {
                format!("opening the spend ledger in the Redis named by {REDIS_VAR}")
            })?;
            tracing::info!(
                var = REDIS_VAR,
                "sessions and committed spend are durable in Redis"
            );
            serve(
                Arc::new(store),
                Arc::new(spend),
                plane,
                catalog,
                reachable,
                metrics_config,
                listener,
            )
            .await
        }
        Err(_) => {
            tracing::warn!(
                var = REDIS_VAR,
                "no Redis configured; sessions and committed spend are in-memory and die \
                 with this process"
            );
            serve(
                Arc::new(MemoryStore::new()),
                Arc::new(MemorySpendLedger::new()),
                plane,
                catalog,
                reachable,
                metrics_config,
                listener,
            )
            .await
        }
    }
}

/// Compose the engine and the four surfaces over whichever backends were
/// chosen.
///
/// **The one composition site**, and two of its values are shared rather than
/// minted per router on purpose. [`Conversations`] is the node's answer to
/// "which session is the conversation the client calls `main`?", and the
/// Responses surface and the control surface both ask it — two tables would
/// agree only until a client edited its own history. [`ControlStore`] is the
/// node's control-plane state, and the engine and the control surface hold
/// opposite ends of it: the surface writes an agent's overlay and the engine
/// spends it at the start of the next turn, the engine deposits a steer's
/// payload and the surface serves it to `fetch_steer`.
#[allow(clippy::too_many_arguments)]
async fn serve<S: SessionStore>(
    store: Arc<S>,
    spend: Arc<dyn SpendLedger>,
    plane: Arc<ControlPlane>,
    catalog: StaticFrontierCatalog,
    reachable: Vec<Candidate>,
    metrics_config: Arc<MetricsConfig>,
    listener: tokio::net::TcpListener,
) -> anyhow::Result<()> {
    let conversations = Arc::new(Conversations::new());
    let control = Arc::new(ControlStore::new());

    let engine = Arc::new(
        Engine::new(
            Arc::clone(&store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local answer")),
            catalog,
            Arc::new(EchoFrontierClient::new("frontier answer")),
            Arc::new(AffinityPolicy::new()),
            EngineConfig::default(),
        )
        .with_spend_ledger(Arc::clone(&spend))
        .with_control_store(Arc::clone(&control)),
    );

    // Four surfaces, one process and one log: the native transport, which
    // exposes sessions and the log itself; the Responses API, which lets an
    // agent written against OpenAI drive the same sessions unmodified; the
    // metrics surface, which reports on both by folding the same log; and the
    // MCP control surface, which is the only one an agent rather than a client
    // drives — it reads what the others did and lets the model ask to be routed
    // to less than its key allows.
    // One control plane behind all four, not one each: a key that pays for a
    // turn on one surface and is unknown to another would be a deployment with
    // two answers to the same question.
    let app = http::router(Arc::clone(&plane), Arc::clone(&engine), Arc::clone(&store))
        .merge(metrics_api::metrics_router(
            Arc::clone(&plane),
            engine.metrics(),
            metrics_config,
        ))
        .merge(mcp_api::mcp_router(
            Arc::clone(&plane),
            Arc::new(ControlPlaneReads::new(
                Arc::clone(&plane),
                Arc::clone(&store),
                spend,
                Arc::clone(&conversations),
                // The same list the startup cross-checks above are built on, and
                // it is right for the same reason: this binary attaches no
                // fleet, so the catalog is everything a turn of its could be
                // routed to. A deployment that attaches one adds its local model
                // here at the same site — see `reachable_candidates`.
                reachable,
            )),
            control,
        ))
        .merge(responses_api::responses_router(
            plane,
            engine,
            store,
            conversations,
        ));
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use roundhouse_server::ControlPlaneConfig;

    fn plane_with_policy(policy: serde_json::Value) -> ControlPlane {
        plane_with(policy, serde_json::Value::Null)
    }

    /// A one-key plane carrying a policy, a budget, or both.
    ///
    /// Through the config file rather than by building the lookup table, so
    /// the fixture exercises the same narrowing and validation a deployment
    /// does. The hash is a plausible-looking constant: this check never
    /// authenticates anything, it only reads the policies and budgets.
    fn plane_with(policy: serde_json::Value, budget: serde_json::Value) -> ControlPlane {
        let mut project = serde_json::json!({ "id": "acme", "policy": policy });
        if !budget.is_null() {
            project["budget"] = budget;
        }
        let json = serde_json::json!({
            "projects": [project],
            "users": [{ "id": "ada" }],
            "keys": [{ "project": "acme", "user": "ada", "key_sha256": "a".repeat(64) }],
        })
        .to_string();
        ControlPlane::configured(
            ControlPlaneConfig::from_json(&json, "startup cross-check fixture")
                .expect("the fixture config must validate"),
        )
    }

    #[test]
    fn a_policy_that_names_no_model_in_the_catalog_refuses_to_serve() {
        let reachable = reachable_candidates(&echo_catalog());
        assert_eq!(
            reachable.len(),
            1,
            "the echo stub is the one thing this binary can route to"
        );

        // Probe: a filter that is well-formed, parses, and names nothing.
        let error = refuse_policies_that_admit_nothing(
            &plane_with_policy(serde_json::json!({ "allow": ["anthropic/*"] })),
            &reachable,
        )
        .expect_err("a filter matching no catalog entry must stop the process");
        let message = error.to_string();
        assert!(
            message.contains("project `acme`, user `ada`"),
            "the refusal has to name the entry an operator would go and fix: {message}"
        );
        assert!(
            message.contains("allow anthropic/*"),
            "and the patterns themselves, not only their hash: a digest tells an \
             operator that two keys differ, never which one they mistyped: {message}"
        );

        // Probe: the same failure through the other axis. A floor above every
        // quality prior in the catalog leaves nothing admissible just as surely.
        assert!(
            refuse_policies_that_admit_nothing(
                &plane_with_policy(serde_json::json!({ "min_quality": 0.9 })),
                &reachable,
            )
            .is_err(),
            "the echo stub's prior is 0.5, so a 0.9 floor admits nothing"
        );

        // Control: a filter that does name it, and one that names everything.
        for policy in [
            serde_json::json!({ "allow": ["echo/echo"] }),
            serde_json::json!({ "allow": ["*"] }),
            serde_json::json!({ "min_quality": 0.5 }),
            serde_json::json!({}),
        ] {
            refuse_policies_that_admit_nothing(&plane_with_policy(policy.clone()), &reachable)
                .unwrap_or_else(|error| panic!("{policy} admits the catalog: {error}"));
        }
    }

    #[test]
    fn a_cadence_is_never_what_makes_a_policy_look_empty_at_startup() {
        // A rationed frontier model is one this key reaches on some turns, so
        // this check must not refuse over it — it asks `permits`, the
        // history-independent axes, for exactly this reason. Whether the
        // *spent* window has anything left is the next test's subject.
        refuse_policies_that_admit_nothing(
            &plane_with_policy(
                serde_json::json!({ "frontier_cadence": { "max_frontier": 1, "per_turns": 10 } }),
            ),
            &reachable_candidates(&echo_catalog()),
        )
        .expect("a cadence rations a target, it does not remove it");
    }

    /// One local worker, priced the way a quote would price it.
    ///
    /// Hand-built rather than quoted, because [`reachable_candidates`] quotes
    /// the catalog and this binary attaches no fleet — the whole subject of
    /// the two tests below is what changes when a deployment *does*.
    fn local_candidate() -> Candidate {
        Candidate {
            target: roundhouse_core::routing::Target::Local {
                worker_id: 1,
                dp_rank: 0,
                model: "llama".into(),
            },
            expected_prefill_tokens: PROBE_ISL_TOKENS as f64,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 60.0,
            expected_cost_usd: 0.0,
            quality_prior: 0.6,
            load: Some(0.0),
        }
    }

    /// The echo stub at a real price.
    ///
    /// [`echo_catalog`] is free, and a free hosted model is admissible at a
    /// ceiling of zero — so an exhausted budget would still have somewhere to
    /// go and the budget half of the promise check would never fire. That is
    /// the honest answer for the free stub and the wrong fixture for testing
    /// the check, which is why the budget tests below quote this instead.
    fn priced_catalog() -> StaticFrontierCatalog {
        StaticFrontierCatalog::new(vec![FrontierModelSpec {
            pricing: ProviderPricing {
                input_per_mtok_usd: 3.0,
                cached_input_per_mtok_usd: 0.3,
                cache_write_per_mtok_usd: 3.75,
                output_per_mtok_usd: 15.0,
            },
            ..echo_catalog().models()[0].clone()
        }])
    }

    /// A degrade-mode budget with the valve off, spelled for `plane_with`.
    fn strict_budget() -> serde_json::Value {
        serde_json::json!({
            "limit_usd": 10.0,
            "window": "total",
            "on_exhaustion": "degrade_to_local",
            "overflow_when_local_saturated": false,
        })
    }

    #[test]
    fn a_cadence_with_no_local_fleet_to_fall_back_on_refuses_to_serve() {
        // The promise a cadence makes, checked against the fleet for the first
        // time: "when the window is spent, hosted targets go inadmissible and
        // the turn serves locally". This binary wires no fleet, so for a
        // rationed key that sentence is false from the second turn onwards —
        // and it was false silently, since the other cross-check asks only
        // about the unspent window.
        let cadence = serde_json::json!({
            "frontier_cadence": { "max_frontier": 1, "per_turns": 10 }
        });
        let error = refuse_promises_of_a_local_fallback(
            &plane_with_policy(cadence.clone()),
            &reachable_candidates(&echo_catalog()),
        )
        .expect_err("a cadence with no local capacity behind it must stop the process");
        let message = error.to_string();
        assert!(
            message.contains("project `acme`, user `ada`"),
            "the refusal has to name the key an operator would go and fix: {message}"
        );
        assert!(
            message.contains("spent window serves locally")
                && message.contains("no local capacity to serve it"),
            "and it has to name the promise it is enforcing: {message}"
        );
        assert!(
            message.contains("frontier_cadence"),
            "and which of the two promises broke, since only one of them is in \
             this key's file: {message}"
        );
        assert!(
            !message.contains("overflow_when_local_saturated"),
            "this key has no budget, so the budget promise must not be reported \
             against it: {message}"
        );

        // Acceptance: the identical policy on a deployment that does quote a
        // local candidate. Same key, same cadence, and the check is satisfied
        // — which is what makes the refusal above about the fleet rather than
        // about the cadence.
        let mut with_fleet = reachable_candidates(&echo_catalog());
        with_fleet.push(local_candidate());
        refuse_promises_of_a_local_fallback(&plane_with_policy(cadence), &with_fleet)
            .expect("a spent window has somewhere to go when a local worker is quoted");
    }

    #[test]
    fn a_degrade_mode_budget_with_the_valve_off_and_no_local_fleet_refuses_to_serve() {
        // Decision 9, and the same sentence the cadence makes one test up: an
        // exhausted budget takes every priced target away, and with the valve
        // off there is nothing but local left to serve the turn. In a
        // deployment with no local capacity that is a promise nothing can
        // keep, so it is refused at boot rather than discovered by a tenant
        // whose turns start failing the day their budget runs out.
        let error = refuse_promises_of_a_local_fallback(
            &plane_with(serde_json::json!({}), strict_budget()),
            &reachable_candidates(&priced_catalog()),
        )
        .expect_err("a valve-off budget with no local capacity behind it must stop the process");
        let message = error.to_string();
        assert!(
            message.contains("project `acme`, user `ada`"),
            "the refusal has to name the key an operator would go and fix: {message}"
        );
        assert!(
            message.contains("overflow_when_local_saturated off")
                && message.contains("exhausted budget serves locally"),
            "and it has to name the field and the promise, not just the project: {message}"
        );

        // Acceptance: the identical budget on a deployment that quotes a local
        // worker. Same key, same budget, and the check is satisfied — which is
        // what makes the refusal above about the fleet rather than about
        // budgets.
        let mut with_fleet = reachable_candidates(&priced_catalog());
        with_fleet.push(local_candidate());
        refuse_promises_of_a_local_fallback(
            &plane_with(serde_json::json!({}), strict_budget()),
            &with_fleet,
        )
        .expect("an exhausted budget has somewhere to go when a local worker is quoted");
    }

    #[test]
    fn the_two_exhaustion_settings_that_promise_nothing_local_boot_without_a_fleet() {
        // The control that keeps the check above from being "refuse every
        // budgeted fleetless deployment". Only one of the three exhaustion
        // settings promises local service: `refuse` never made the promise,
        // and the valve keeps it on frontier. Both must boot here, or turning
        // a budget on would require a fleet nobody said they needed.
        for budget in [
            serde_json::json!({
                "limit_usd": 10.0, "window": "total", "on_exhaustion": "refuse",
            }),
            serde_json::json!({
                "limit_usd": 10.0,
                "window": "monthly",
                "on_exhaustion": "degrade_to_local",
                "overflow_when_local_saturated": true,
            }),
        ] {
            refuse_promises_of_a_local_fallback(
                &plane_with(serde_json::json!({}), budget.clone()),
                &reachable_candidates(&priced_catalog()),
            )
            .unwrap_or_else(|error| panic!("{budget} promises no local service: {error}"));
        }
    }

    #[test]
    fn a_configuration_that_spends_no_allowance_promises_nothing_about_one() {
        // The other half of the same control: a policy that never rations and
        // a project with no budget have no spent-allowance behavior to make
        // good on, so a fleetless deployment serves them happily.
        for policy in [
            serde_json::json!({}),
            serde_json::json!({ "allow": ["echo/echo"] }),
            serde_json::json!({ "min_quality": 0.5 }),
        ] {
            refuse_promises_of_a_local_fallback(
                &plane_with_policy(policy.clone()),
                &reachable_candidates(&echo_catalog()),
            )
            .unwrap_or_else(|error| panic!("{policy} spends no allowance: {error}"));
        }
    }

    #[test]
    fn a_key_that_breaks_both_promises_is_told_about_both_in_one_refusal() {
        // The reason the two questions are one function rather than two
        // lookalikes: a key that rations frontier traffic *and* budgets it has
        // made the same promise twice, and being sent to fix one of them and
        // then restarting into the other is two outages where there should be
        // one message.
        let error = refuse_promises_of_a_local_fallback(
            &plane_with(
                serde_json::json!({ "frontier_cadence": { "max_frontier": 1, "per_turns": 10 } }),
                strict_budget(),
            ),
            &reachable_candidates(&priced_catalog()),
        )
        .expect_err("both promises are unkeepable here");
        let message = error.to_string();
        assert!(
            message.contains("frontier_cadence")
                && message.contains("overflow_when_local_saturated"),
            "one refusal, both promises named: {message}"
        );
        assert_eq!(
            message.matches("project `acme`, user `ada`").count(),
            1,
            "and the key named once rather than once per promise: {message}"
        );
    }

    #[test]
    fn an_open_deployment_has_no_policies_to_cross_check() {
        let reachable = reachable_candidates(&echo_catalog());
        refuse_policies_that_admit_nothing(&ControlPlane::Open, &reachable)
            .expect("open mode resolves to the unrestricted policy");
        refuse_promises_of_a_local_fallback(&ControlPlane::Open, &reachable)
            .expect("and the unrestricted policy carries no cadence and no budget");
        // And the empty catalog is the deployment's problem, not this check's:
        // with nothing quoted there is nothing to disagree about, and the
        // routing layer's own `NoCandidates` is the accurate answer.
        refuse_policies_that_admit_nothing(&ControlPlane::Open, &[])
            .expect("no catalog is not a policy mistake");
    }
}
