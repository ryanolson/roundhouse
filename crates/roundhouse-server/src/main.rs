// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Roundhouse binary.
//!
//! Deliberately thin: everything interesting is a seam, and this only chooses
//! which implementation of each seam to instantiate. Configuration is one
//! environment variable, because a flag parser here would be the first place a
//! deployment concern leaked into the composition root.
//!
//! The store is the one seam a deployment selects here: `ROUNDHOUSE_REDIS_URL`
//! set means sessions live in that Redis and survive this process;
//! absent means [`MemoryStore`] and sessions die with it. A URL that is set
//! but unreachable stops the process at startup — falling back to memory
//! would silently demote "durable" to "until the next restart", which is the
//! one property the variable exists to promise. The rest of the wiring is the
//! offline demo set — [`ByteTokenizer`], [`EchoLocalExecutor`],
//! [`EchoFrontierClient`] — so the process serves with no GPU, no provider
//! account and no model assets.
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
use roundhouse_core::metrics::MetricsConfig;
use roundhouse_core::routing::{AffinityPolicy, CacheModel, ProviderPricing};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierModelSpec, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::{
    ControlPlane, EchoLocalExecutor, Engine, EngineConfig, catalog_config, control_config, http,
    metrics_api, responses_api,
};
use roundhouse_store_redis::RedisSessionStore;
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
        ControlPlane::Configured { turn_keys, .. } => tracing::info!(
            memberships = turn_keys.len(),
            var = control_config::CONTROL_PLANE_VAR,
            "control plane loaded; a key is required on every surface"
        ),
        ControlPlane::Open => tracing::warn!(
            var = control_config::CONTROL_PLANE_VAR,
            "no control plane configured; every request is served as the built-in \
             default/default membership, with no key and no session namespace"
        ),
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
    match std::env::var(REDIS_VAR) {
        Ok(url) => {
            let store = RedisSessionStore::connect(url)
                .await
                .with_context(|| format!("connecting to the Redis named by {REDIS_VAR}"))?;
            tracing::info!(var = REDIS_VAR, "sessions are durable in Redis");
            serve(Arc::new(store), plane, catalog, metrics_config, listener).await
        }
        Err(_) => {
            tracing::warn!(
                var = REDIS_VAR,
                "no Redis configured; sessions are in-memory and die with this process"
            );
            serve(
                Arc::new(MemoryStore::new()),
                plane,
                catalog,
                metrics_config,
                listener,
            )
            .await
        }
    }
}

/// Compose the engine and the three surfaces over whichever store was chosen.
async fn serve<S: SessionStore>(
    store: Arc<S>,
    plane: Arc<ControlPlane>,
    catalog: StaticFrontierCatalog,
    metrics_config: Arc<MetricsConfig>,
    listener: tokio::net::TcpListener,
) -> anyhow::Result<()> {
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        catalog,
        Arc::new(EchoFrontierClient::new("frontier answer")),
        Arc::new(AffinityPolicy::new()),
        EngineConfig::default(),
    ));

    // Three surfaces, one process and one log: the native transport, which
    // exposes sessions and the log itself; the Responses API, which lets an
    // agent written against OpenAI drive the same sessions unmodified; and the
    // metrics surface, which reports on both by folding the same log.
    // One control plane behind all three, not one each: a key that pays for a
    // turn on one surface and is unknown to another would be a deployment with
    // two answers to the same question.
    let app = http::router_under(Arc::clone(&plane), Arc::clone(&engine), Arc::clone(&store))
        .merge(metrics_api::metrics_router_under(
            Arc::clone(&plane),
            engine.metrics(),
            metrics_config,
        ))
        .merge(responses_api::responses_router_under(plane, engine, store));
    axum::serve(listener, app).await?;
    Ok(())
}
