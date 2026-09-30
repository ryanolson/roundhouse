// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The catalog, the credentials and the engine, built the way a deployment is.

use std::collections::BTreeMap;
use std::sync::Arc;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{
    CredentialMode, MemorySpendLedger, Principal, Secret, TurnCredentials,
};
use roundhouse_core::routing::{AffinityPolicy, CacheModel, ProviderPricing};
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::anthropic_messages::AnthropicMessagesClient;
use roundhouse_fleet::{
    FrontierClient, FrontierClients, FrontierModelSpec, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::catalog_config::ProviderConfig;
use roundhouse_server::{
    Admission, ControlPlane, ControlPlaneConfig, EchoLocalExecutor, Engine, EngineConfig,
    LocalExecutor,
};

use crate::common::{key, sha256_hex};

/// The provider name the fixture catalog and the fixture credentials agree on.
pub(super) const PROVIDER: &str = "anthropic";
/// The fixture's model. The live run pins its own — see `live`.
const MODEL: &str = "claude-probe";

/// A catalog of one Anthropic-dialect entry at `pricing`.
///
/// Deterministic five-minute cache: the probe is about whether the provider
/// reads back what it wrote, not about the one-hour lifetime C4 configures.
pub(super) fn catalog(pricing: ProviderPricing) -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![FrontierModelSpec {
        provider: PROVIDER.to_string(),
        model: MODEL.to_string(),
        wire_protocol: WireProtocol::AnthropicMessages,
        cache_model: CacheModel::Deterministic { ttl_ms: 300_000 },
        pricing,
        quality_prior: 0.9,
        base_ttft_ms: 350.0,
        ttft_ms_per_uncached_token: 0.002,
    }])
}

/// The fixture spec, with the two fields the preflight tests vary.
pub(super) fn spec_with(
    pricing: ProviderPricing,
    wire_protocol: WireProtocol,
) -> FrontierModelSpec {
    FrontierModelSpec {
        provider: PROVIDER.to_string(),
        model: MODEL.to_string(),
        wire_protocol,
        cache_model: CacheModel::Deterministic { ttl_ms: 300_000 },
        pricing,
        quality_prior: 0.9,
        base_ttft_ms: 350.0,
        ttft_ms_per_uncached_token: 0.002,
    }
}

/// A rate card with a real output price, so a budget can bite.
///
/// Not zeros: a spend-bounded probe whose catalog prices everything at nothing
/// takes a zero hold, and a cap over a zero hold is not a cap.
pub(super) fn priced() -> ProviderPricing {
    ProviderPricing {
        input_per_mtok_usd: 3.0,
        cached_input_per_mtok_usd: 0.3,
        cache_write_per_mtok_usd: 3.75,
        output_per_mtok_usd: 15.0,
    }
}

/// The deployment's own stored key for this provider, typed rather than set in
/// the environment.
///
/// `TurnCredentials::configured` is the same constructor the control-plane
/// loader calls once it has read the variable an operator named; taking it
/// directly is what lets this suite exercise the stored-credential path with no
/// process-wide mutation and no `unsafe`.
fn stored_credentials(provider: &str, secret: &str) -> TurnCredentials {
    let deployment = BTreeMap::from([(
        provider.to_string(),
        Secret::api_key(secret).expect("the key is shaped like an api key"),
    )]);
    TurnCredentials::configured(
        CredentialMode::ProjectOnly,
        deployment,
        BTreeMap::new(),
        BTreeMap::new(),
    )
    .expect("stored keys with no pass-through mode resolve")
}

/// An admission from a configured plane, with a stored credential over it.
///
/// The budget and the policy come from a control-plane file the way an
/// operator writes one — `Admission::open()` would carry no ceiling and
/// `TurnCredentials::unrestricted` resolves to `TurnCredential::Absent`, which
/// this dialect refuses. Only the credential is substituted, and only because
/// the file's own path reads an environment variable.
pub(super) fn admission(limit_usd: f64, provider: &str, secret: &str) -> Admission {
    let json = serde_json::json!({
        "projects": [{
            "id": "probe",
            "budget": { "limit_usd": limit_usd, "window": "total", "on_exhaustion": "refuse" },
        }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "probe", "user": "ada", "key_sha256": sha256_hex(&key("probe")) }],
    })
    .to_string();
    let plane = ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "cache probe fixture")
            .expect("the fixture config must validate"),
    );
    let admission = plane
        .membership(&Principal::new("probe", "ada"))
        .expect("the fixture key names a membership");
    Admission {
        credentials: stored_credentials(provider, secret),
        ..admission
    }
}

/// The engine a deployment composes, pointed at `base`.
pub(super) fn engine(
    definition: &ProviderConfig,
    provider: &str,
    catalog: StaticFrontierCatalog,
    store: Arc<MemoryStore>,
) -> Engine<MemoryStore, ByteTokenizer> {
    // The same four fields `main`'s `messages_client` reads, in the same order.
    // A probe that took only the base URL would post to the client's default
    // path under a gateway's versioned base, send the key in the header that
    // gateway ignores, and drop the static headers it requires — three ways to
    // fail the one run that costs money.
    let client = AnthropicMessagesClient::with_bases(&definition.base_url, &definition.base_url)
        .expect("the client builds over a base")
        .with_messages_path(
            definition
                .routes
                .for_dialect(WireProtocol::AnthropicMessages)
                .expect("the definition declares a messages route"),
        )
        .with_stored_auth_style(
            definition
                .auth
                .stored_auth_style()
                .expect("the definition names a spelling this build sends"),
        )
        .with_extra_headers(
            definition
                .extra_headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        )
        .expect("the definition's static headers are sendable");
    let clients = FrontierClients::keyed(
        [(
            provider.to_string(),
            Arc::new(client) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    Engine::with_provider_clients(
        store,
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")) as Arc<dyn LocalExecutor>,
        catalog,
        Arc::new(clients),
        Arc::new(AffinityPolicy::new()),
        EngineConfig {
            turn_deadline_ms: 30_000,
            expected_output_tokens: 16,
            ..EngineConfig::default()
        },
    )
    .with_spend_ledger(Arc::new(MemorySpendLedger::new()))
}
