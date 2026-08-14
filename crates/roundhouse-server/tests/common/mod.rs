// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Fixtures shared by the integration-test binaries.
//!
//! One canonical copy: a change to the catalog shape or the worker registration
//! touches this file, not one file per test binary. Each binary compiles its
//! own copy via `mod common;`, and none uses every item, so the module opts out
//! of dead-code analysis rather than sprinkling `allow`s per item.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use roundhouse_core::routing::{CacheModel, ProviderPricing};
use roundhouse_fleet::{
    EmbeddedFleet, FrontierModelSpec, KvRouterConfig, SelectionServiceBuilder,
    StaticFrontierCatalog, WireProtocol, WorkerRegistration,
};
use roundhouse_server::EngineConfig;

pub const BLOCK_SIZE: u32 = 16;
pub const LOCAL_MODEL: &str = "local";
pub const MINUTE: u64 = 60_000;

/// One priced frontier model, so a turn always has somewhere to go.
pub fn frontier_catalog() -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![FrontierModelSpec {
        provider: "anthropic".into(),
        model: "claude".into(),
        wire_protocol: WireProtocol::AnthropicMessages,
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

pub fn config() -> EngineConfig {
    EngineConfig {
        block_size: BLOCK_SIZE,
        local_model: LOCAL_MODEL.to_string(),
        ..Default::default()
    }
}

/// A selection service with one registered worker, so local is a real option.
///
/// KV events stay off: these binaries test the session, routing, and transport
/// layers, and a cold indexer keeps their pricing deterministic. The one test
/// that needs a warm worker builds its own fleet (`mocker_cache_hits.rs`).
pub async fn embedded_fleet() -> Arc<EmbeddedFleet> {
    let service = SelectionServiceBuilder::new(KvRouterConfig {
        use_kv_events: false,
        router_queue_threshold: None,
        ..Default::default()
    })
    .indexer_threads(1)
    .build()
    .await
    .expect("selection service should start");
    let fleet = Arc::new(EmbeddedFleet::new(Arc::new(service)));
    fleet
        .register_worker(WorkerRegistration {
            worker_id: 1,
            model_name: LOCAL_MODEL.to_string(),
            routing_group: "default".to_string(),
            endpoint: "http://worker-1:8000".to_string(),
            block_size: BLOCK_SIZE,
            kv_events_endpoints: HashMap::new(),
        })
        .await
        .expect("the worker must register");
    fleet
}
