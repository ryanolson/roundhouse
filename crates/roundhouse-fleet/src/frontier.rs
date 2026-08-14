// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Frontier providers.
//!
//! A frontier model cannot be asked what it has cached, so its candidate is
//! priced entirely from the routing ledger: what we last sent it, when, and how
//! that provider's cache expires. The executor's job is then to make the model
//! self-fulfilling — sending a stable `prompt_cache_key` and, where the
//! provider supports explicit breakpoints, `cache_control` markers at the same
//! prefix boundary each turn. Routing on a predicted cache hit and then
//! prompting in a way that defeats it is the obvious failure mode.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use roundhouse_core::metrics::{ReferenceModel, ShadowPricing};
use roundhouse_core::routing::{CacheLedger, CacheModel, Candidate, ProviderPricing, Target};

use crate::usage::WireProtocol;

/// A frontier model we may route to.
///
/// Deserializable so a deployment's catalog file is this struct rather than a
/// parallel schema that has to be kept in agreement with it. Adding a field
/// here is then a change to the config format by construction, which is the
/// only way the two stay honest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontierModelSpec {
    pub provider: String,
    pub model: String,
    /// The dialect this target speaks, which decides what a client has to add
    /// to an outbound request before the provider will account for the call at
    /// all. See [`crate::usage`] — on the most common dialect, forgetting it
    /// costs every token and every dollar on this model's row of the
    /// dashboard.
    pub wire_protocol: WireProtocol,
    pub cache_model: CacheModel,
    pub pricing: ProviderPricing,
    /// Relative capability, 0.0..=1.0. Configuration, not measurement.
    pub quality_prior: f64,
    /// Latency floor before any prefill, i.e. network plus queueing.
    pub base_ttft_ms: f64,
    /// Additional TTFT per uncached prompt token.
    pub ttft_ms_per_uncached_token: f64,
}

impl FrontierModelSpec {
    pub fn target(&self) -> Target {
        Target::Frontier {
            provider: self.provider.clone(),
            model: self.model.clone(),
        }
    }
}

/// The set of frontier models available to a session.
///
/// Static because provider capability and pricing are deployment
/// configuration. Prices are supplied by the caller rather than hardcoded here:
/// baking a rate card into source guarantees it goes stale.
#[derive(Debug, Clone, Default)]
pub struct StaticFrontierCatalog {
    models: Vec<FrontierModelSpec>,
}

impl StaticFrontierCatalog {
    pub fn new(models: Vec<FrontierModelSpec>) -> Self {
        Self { models }
    }

    pub fn models(&self) -> &[FrontierModelSpec] {
        &self.models
    }

    /// Seed a ledger with this catalog's cache models and pricing.
    ///
    /// Must run before the session replays its log, so replayed dispatches are
    /// interpreted under the right TTL.
    pub fn apply_to_ledger(&self, ledger: &mut CacheLedger) {
        for spec in &self.models {
            ledger.register(&spec.target(), spec.cache_model, spec.pricing);
        }
    }

    /// The rate card and capability priors this catalog implies.
    ///
    /// Derived rather than configured separately, because a deployment that
    /// stated its prices twice would have them disagree the first time one
    /// copy was updated — and the two copies are the number the router chooses
    /// on and the number the dashboard reports saving. They must be the same
    /// number or neither means anything.
    ///
    /// Correlaries are not derived here: which hosted model our own Llama
    /// stands in for is a claim about capability that this catalog does not
    /// contain. The caller declares those on the result.
    pub fn shadow_pricing(&self) -> ShadowPricing {
        ShadowPricing::new(
            self.models
                .iter()
                .map(|spec| ReferenceModel {
                    provider: spec.provider.clone(),
                    model: spec.model.clone(),
                    pricing: spec.pricing,
                    quality_prior: spec.quality_prior,
                })
                .collect(),
        )
    }

    /// Price every frontier model against the current prompt.
    pub fn quote(
        &self,
        ledger: &CacheLedger,
        now_ms: u64,
        isl_tokens: u64,
        expected_output_tokens: u64,
    ) -> Vec<Candidate> {
        self.models
            .iter()
            .map(|spec| {
                let target = spec.target();
                let cached = ledger.expected_cached_tokens(&target, now_ms, isl_tokens);
                let uncached = (isl_tokens as f64 - cached).max(0.0);
                Candidate {
                    target: target.clone(),
                    // The same axis the router reports for local workers:
                    // prompt tokens that actually have to be processed.
                    expected_prefill_tokens: uncached,
                    matched_prefix_tokens: cached as u64,
                    expected_ttft_ms: spec.base_ttft_ms
                        + uncached * spec.ttft_ms_per_uncached_token,
                    expected_cost_usd: ledger.estimate_cost_usd(
                        &target,
                        now_ms,
                        isl_tokens,
                        expected_output_tokens,
                    ),
                    quality_prior: spec.quality_prior,
                    // Provider-side load is not observable to us. Reporting a
                    // guess here would let a fabricated number gate routing.
                    load: None,
                }
            })
            .collect()
    }
}

/// A provider's response as it is produced.
///
/// Boxed rather than an associated type so the trait stays object-safe: the
/// engine holds one `Arc<dyn FrontierClient>` for a catalog of providers whose
/// transports have nothing in common.
pub type FrontierStream = BoxStream<'static, Result<FrontierChunk, FrontierError>>;

/// One streamed chunk from a provider.
#[derive(Debug, Clone, PartialEq)]
pub enum FrontierChunk {
    OutputText(String),
    Done {
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        /// Thinking tokens, already counted inside `output_tokens`.
        ///
        /// Reported separately because a reasoning model can spend most of a
        /// turn's output budget here without the client seeing a byte of it,
        /// and a dashboard that folded it into ordinary output would show a
        /// verbose answer where the truth is an expensive silence. Zero for
        /// providers and models that do not reason.
        reasoning_tokens: u64,
    },
}

impl FrontierChunk {
    /// A completed response presented as a stream.
    ///
    /// The adapter any non-streaming backend reaches for: one text chunk, one
    /// accounting chunk. Keeping it here, next to the chunk type, means a
    /// backend that cannot stream still feeds the same durable-delta fold as
    /// one that can, instead of growing a second path for output to reach the
    /// log — with the honest cost that no delta lands any earlier than the
    /// last token does.
    pub fn whole_response(
        text: String,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
    ) -> FrontierStream {
        futures::stream::iter([
            Ok(FrontierChunk::OutputText(text)),
            Ok(FrontierChunk::Done {
                input_tokens,
                cached_input_tokens,
                output_tokens,
                reasoning_tokens,
            }),
        ])
        .boxed()
    }
}

/// What a provider was asked to do.
#[derive(Debug, Clone)]
pub struct FrontierQuote {
    pub target: Target,
    pub prompt: String,
    /// Stable per-session key. Providers use it to steer requests to the same
    /// cache node, so it must not vary turn to turn.
    pub prompt_cache_key: String,
    pub expected_output_tokens: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum FrontierError {
    #[error("provider `{0}` is not configured")]
    UnknownProvider(String),
    #[error("provider call failed: {0}")]
    Upstream(String),
}

/// Executes a turn against a hosted provider.
///
/// The stream is the contract, not a convenience: the session layer appends
/// each delta durably as it arrives, so a process that dies mid-generation
/// leaves the partial answer in the log for its successor to resume from.
/// Handing back a whole response instead would make that impossible, and would
/// also erase time-to-first-token — the quantity the routing is optimizing for
/// — from the record, since the log would only ever show the moment the last
/// byte landed.
#[async_trait]
pub trait FrontierClient: Send + Sync + 'static {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError>;
}

/// Deterministic [`FrontierClient`] for tests and offline runs.
pub struct EchoFrontierClient {
    reply: String,
}

impl EchoFrontierClient {
    pub fn new(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
        }
    }
}

#[async_trait]
impl FrontierClient for EchoFrontierClient {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        Ok(FrontierChunk::whole_response(
            self.reply.clone(),
            quote.prompt.len() as u64,
            0,
            self.reply.len() as u64,
            0,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = 60_000;

    fn catalog() -> StaticFrontierCatalog {
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

    #[test]
    fn a_cold_frontier_prices_the_whole_prompt_as_prefill() {
        let catalog = catalog();
        let mut ledger = CacheLedger::new();
        catalog.apply_to_ledger(&mut ledger);

        let quotes = catalog.quote(&ledger, 0, 50_000, 500);
        assert_eq!(quotes.len(), 1);
        assert_eq!(quotes[0].expected_prefill_tokens, 50_000.0);
        assert_eq!(quotes[0].matched_prefix_tokens, 0);
        assert_eq!(quotes[0].load, None, "provider load is not observable");
    }

    #[test]
    fn a_warm_frontier_prices_far_less_prefill_and_far_less_money() {
        let catalog = catalog();
        let mut ledger = CacheLedger::new();
        catalog.apply_to_ledger(&mut ledger);

        let cold = catalog.quote(&ledger, 0, 50_000, 500).remove(0);

        ledger.record(&catalog.models()[0].target(), 0, 50_000);
        let warm = catalog.quote(&ledger, MINUTE, 50_000, 500).remove(0);

        assert_eq!(warm.expected_prefill_tokens, 0.0);
        assert_eq!(warm.matched_prefix_tokens, 50_000);
        assert!(warm.expected_cost_usd < cold.expected_cost_usd);
        assert!(warm.expected_ttft_ms < cold.expected_ttft_ms);
    }

    #[test]
    fn cache_expiry_returns_the_frontier_to_cold_pricing() {
        let catalog = catalog();
        let mut ledger = CacheLedger::new();
        catalog.apply_to_ledger(&mut ledger);
        ledger.record(&catalog.models()[0].target(), 0, 50_000);

        let inside = catalog.quote(&ledger, 4 * MINUTE, 50_000, 500).remove(0);
        let outside = catalog.quote(&ledger, 6 * MINUTE, 50_000, 500).remove(0);

        assert_eq!(inside.expected_prefill_tokens, 0.0);
        assert_eq!(outside.expected_prefill_tokens, 50_000.0);
    }

    #[tokio::test]
    async fn the_echo_client_reports_usage() {
        let client = EchoFrontierClient::new("hello");
        let stream = client
            .execute(&FrontierQuote {
                target: Target::Frontier {
                    provider: "anthropic".into(),
                    model: "claude".into(),
                },
                prompt: "some prompt".into(),
                prompt_cache_key: "sess_x".into(),
                expected_output_tokens: None,
            })
            .await
            .unwrap();
        let chunks: Vec<_> = stream.map(|chunk| chunk.unwrap()).collect().await;

        assert_eq!(chunks[0], FrontierChunk::OutputText("hello".into()));
        assert!(matches!(
            chunks[1],
            FrontierChunk::Done {
                output_tokens: 5,
                ..
            }
        ));
    }
}
