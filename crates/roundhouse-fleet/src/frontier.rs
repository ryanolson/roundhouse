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

use roundhouse_core::control::{CredentialError, TurnCredential};
use roundhouse_core::metrics::{ReferenceModel, ShadowPricing};
use roundhouse_core::routing::{
    AttemptClass, CacheLedger, CacheModel, Candidate, ProviderPricing, Target,
};

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

    /// The entry a chosen target came from.
    ///
    /// Sound because the configuration boundary refuses a catalog listing one
    /// `(provider, model)` twice — see `CatalogConfig::validate` in
    /// `roundhouse-server`. That check is what makes this a lookup rather than
    /// a guess: `Target` alone cannot distinguish two entries for one model
    /// served over different dialects, which is exactly the shape the boundary
    /// rejects. A catalog built by hand rather than parsed carries that
    /// obligation itself.
    pub fn spec_for(&self, target: &Target) -> Option<&FrontierModelSpec> {
        let Target::Frontier { provider, model } = target else {
            return None;
        };
        self.models
            .iter()
            .find(|spec| &spec.provider == provider && &spec.model == model)
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
        /// What the provider says this call cost, when it says so at all.
        ///
        /// **A price, never a token count, and the separation is the point.**
        /// OpenRouter attaches `cost` to the usage object of every response
        /// (`agent-docs/research/openrouter-api-surface.md` Q3, live
        /// 2026-08-24); OpenAI's own endpoint does not. Folding it in beside
        /// the token counts would put a number we did not derive from our own
        /// rate card into the column the savings figure is computed from, and
        /// the whole reconciliation idea is that those two numbers stay apart
        /// and are *compared* — `committed_usd` against a provider's own
        /// ledger — rather than summed.
        ///
        /// `None` means the provider reported no price, which is the ordinary
        /// answer and is emphatically not the same as free.
        ///
        /// **Not yet folded into [`Usage`], and that is a deferral with a
        /// named unlock.** Nothing in this build reads a provider-reported
        /// dollar figure — `admin_api::reconciliation` compares `committed_usd`
        /// against `measured_usd`, both ours — and the consumer arrives with
        /// M10.3's reconciliation rung, which is the first time the savings
        /// claim meets an external bill. Widening `Usage` before that consumer
        /// exists would put a field in the durable log's serde shape that no
        /// reader could check, which is the wrong order: the log's shape should
        /// change when something is going to read it. Until then this is
        /// decoded and logged, so the claim that OpenRouter reports it is
        /// checkable against a real stream rather than against a doc page.
        ///
        /// [`Usage`]: roundhouse_core::event::Usage
        provider_reported_cost: Option<f64>,
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
                // A non-streaming backend adapted into a stream reports the
                // counts it was given and no price: this adapter is handed
                // token numbers by its caller, and inventing a dollar figure
                // from them would be exactly the rate-card-in-source mistake
                // the catalog exists to prevent.
                provider_reported_cost: None,
            }),
        ])
        .boxed()
    }
}

/// What a provider was asked to do.
#[derive(Debug, Clone)]
pub struct FrontierQuote {
    pub target: Target,
    /// The dialect this request must be serialized in.
    ///
    /// Carried here rather than looked up by the client, because this is the
    /// only argument [`FrontierClient::execute`] receives and
    /// [`crate::usage`] makes enforcing usage reporting the client's
    /// obligation. Without it the obligation cannot be discharged from inside
    /// the trait at all, and `Target` is not a substitute: it keys on provider
    /// and model, and one model served over two dialects is an ordinary
    /// deployment.
    pub wire_protocol: WireProtocol,
    pub prompt: String,
    /// Stable per-session key. Providers use it to steer requests to the same
    /// cache node, so it must not vary turn to turn.
    pub prompt_cache_key: String,
    pub expected_output_tokens: Option<u32>,
    /// What this request authenticates with.
    ///
    /// **Carried here for the same reason [`Self::wire_protocol`] is, and the
    /// argument is the stronger one of the two.** This is the only argument
    /// [`FrontierClient::execute`] receives, and the engine deliberately holds
    /// exactly one `Arc<dyn FrontierClient>` for a catalog of providers whose
    /// transports have nothing in common — a client per user would be
    /// connection-pool machinery asked to hold a secret, which is a worse place
    /// for one than a value that lives as long as the turn. So the credential
    /// arrives in the quote or the client cannot authenticate at all, exactly
    /// as the dialect arrives here or the usage obligation cannot be
    /// discharged.
    ///
    /// It is a [`TurnCredential`] and not a string, so `Debug` on this struct —
    /// which is what a `tracing` field on a dispatch renders — yields a
    /// fingerprint. The plaintext is reachable only through
    /// [`TurnCredential::require_api_key`], inside `execute`.
    ///
    /// There is deliberately no default. Every construction site has to say
    /// which credential it resolved, because a site that forgot would otherwise
    /// get [`TurnCredential::Absent`] silently — and an unauthenticated request
    /// nobody meant to send is precisely the failure mode M7's auth ruling
    /// found on the client side.
    pub credential: TurnCredential,
}

#[derive(Debug, thiserror::Error)]
pub enum FrontierError {
    #[error("provider `{0}` is not configured")]
    UnknownProvider(String),
    #[error("provider call failed: {0}")]
    Upstream(String),
    /// No credential, or the wrong shape of one.
    ///
    /// Its own arm rather than an [`Self::Upstream`] string, because nothing
    /// reached an upstream: a client that cannot authenticate must refuse
    /// locally instead of sending the request and letting the provider decide,
    /// which is a fail-open on a request path and — per the M7 auth ruling —
    /// the exact silent failure a misconfigured pass-through route produces.
    /// Transparent so the credential layer's code and message survive to the
    /// client unchanged.
    #[error(transparent)]
    Credential(#[from] CredentialError),
    /// A client was handed a quote in a dialect it does not speak.
    ///
    /// Its own arm for the same reason [`Self::Credential`] is: nothing reached
    /// an upstream. A client holds one transport and one serialization, and the
    /// engine holds one `Arc<dyn FrontierClient>` for the whole catalog — so a
    /// catalog entry whose `wire_protocol` does not match the client a
    /// deployment composed is a configuration mistake, and sending the request
    /// anyway in the hope that the upstream is forgiving is a fail-open with a
    /// mis-serialized body attached. Names both halves because the remedy is to
    /// change one of them.
    #[error(
        "this client speaks `{expected}` and the catalog asked it for `{got}` on `{target}`; \
         refusing to send a request in a dialect it cannot serialize"
    )]
    UnsupportedDialect {
        expected: &'static str,
        got: &'static str,
        target: String,
    },
    /// The request never reached a model: DNS, connect, TLS, a reset — or the
    /// client gave up waiting.
    ///
    /// **Split out of [`Self::Upstream`] so that failover has something to
    /// match on.** A per-dispatch fallback has to distinguish "this origin is
    /// not answering" from "this origin answered and said no", and the only
    /// honest way to do that is for the transport to state which one happened
    /// at the point it knows. Recovering it downstream by grepping an error
    /// string would be a routing decision resting on a `format!`.
    #[error("the request to the upstream failed: {message}")]
    Transport {
        message: String,
        /// The client's own patience ran out, rather than the connection
        /// failing. A separate field and not a separate variant, because every
        /// caller that treats one as a reason to try elsewhere treats the other
        /// the same way, and the distinction is worth exactly one row in an
        /// audit trail.
        timed_out: bool,
    },
    /// The origin answered, with a status that was not a success.
    ///
    /// Structured rather than formatted into [`Self::Upstream`] for the reason
    /// above, and carrying the body because that is what it carried before this
    /// variant existed — a 400 whose message says which field was rejected is
    /// most of the diagnosis. The body is redacted on construction like every
    /// other message leaving a client, and it is deliberately *not* what travels
    /// into the decision record: an attempt row carries the status and the
    /// class, never the prose.
    #[error("the upstream answered {status}: {message}")]
    Status { status: u16, message: String },
}

impl FrontierError {
    /// Why this failure is worth trying a different target for, or `None` when
    /// it is not.
    ///
    /// **The whole failover trigger, in one function.** Upstream's retryable
    /// set is the same three shapes (`client.rs:587-596` @ `053a61e`) and its
    /// status predicate is [`AttemptClass::is_retryable_http_status`], which
    /// this delegates to rather than restating. Everything else answers `None`
    /// — a missing credential, a dialect nobody can serialize, an unknown
    /// provider, and any 4xx that is not 408 or 429 are all the same kind of
    /// fact: a second target would fail the same way, and pretending otherwise
    /// turns one misconfiguration into a tour of the whole tier.
    ///
    /// A model *refusal* is not in this enum at all, which is the point: a
    /// refusal arrives as a completed stream, so it reaches the caller as an
    /// answer and there is nothing here for it to match.
    pub fn failover_class(&self) -> Option<AttemptClass> {
        match self {
            FrontierError::Transport { timed_out, .. } => Some(match timed_out {
                true => AttemptClass::Timeout,
                false => AttemptClass::Transport,
            }),
            FrontierError::Status { status, .. } => AttemptClass::is_retryable_http_status(*status)
                .then_some(AttemptClass::Status { status: *status }),
            FrontierError::UnknownProvider(_)
            | FrontierError::Upstream(_)
            | FrontierError::Credential(_)
            | FrontierError::UnsupportedDialect { .. } => None,
        }
    }
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

/// Every transport this process can dispatch through, keyed by provider name.
///
/// **The M10.1 replacement for the engine's single `Arc<dyn FrontierClient>`.**
/// Until this milestone one client was chosen at boot and every
/// `Target::Frontier` in the process went to it, whatever its `provider` said —
/// which is fine while a deployment addresses one origin and wrong the moment
/// the point of the phase is a turn whose capable tier is a model on OpenRouter
/// and whose fallback is OpenAI's own endpoint. Two origins, two keys, two
/// connection pools, one candidate list.
///
/// **Two shapes, and the difference is not an optimization.**
/// [`Self::uniform`] is one client answering every name: the offline echo stub,
/// and the pre-M10.1 wiring where one `openai_responses` transport served the
/// whole catalog. [`Self::keyed`] is the registry a `providers` section builds.
/// Keeping `uniform` rather than requiring every deployment and every test to
/// enumerate its providers is what makes this change invisible to a
/// configuration that had nothing to enumerate.
///
/// **[`Self::for_provider`] is total on a booted process**, and that is a
/// property of the boundary rather than of this type:
/// `CatalogConfig::validate` refuses an entry naming a provider nothing
/// defines, and the registry constructor refuses a definition this build has no
/// transport for. So the error arm below is unreachable through a routing
/// decision — it exists for the catalog assembled by hand in a test, which
/// carries the obligation itself exactly as `StaticFrontierCatalog::spec_for`
/// says.
///
/// `Debug` renders the provider names and never the clients: a transport holds
/// no secret (the credential travels on the quote), but it does hold two
/// `reqwest::Client`s whose own `Debug` is pages of pool state nobody reading a
/// boot line wants.
pub struct FrontierClients {
    by_provider: std::collections::HashMap<String, std::sync::Arc<dyn FrontierClient>>,
    /// Answers every provider name. `Some` for the one-transport deployments
    /// this milestone did not break.
    uniform: Option<std::sync::Arc<dyn FrontierClient>>,
}

impl std::fmt::Debug for FrontierClients {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.provider_names().collect();
        names.sort_unstable();
        f.debug_struct("FrontierClients")
            .field("providers", &names)
            .field("uniform", &self.uniform.is_some())
            .finish()
    }
}

impl FrontierClients {
    /// One transport for every provider in the catalog.
    pub fn uniform(client: std::sync::Arc<dyn FrontierClient>) -> Self {
        Self {
            by_provider: std::collections::HashMap::new(),
            uniform: Some(client),
        }
    }

    /// One transport per provider, and nothing for a name that is not here.
    pub fn keyed(
        by_provider: std::collections::HashMap<String, std::sync::Arc<dyn FrontierClient>>,
    ) -> Self {
        Self {
            by_provider,
            uniform: None,
        }
    }

    /// The transport `provider`'s traffic goes through.
    ///
    /// Named in the error rather than reported as a generic upstream failure:
    /// the remedy is a `providers` entry or a `ROUNDHOUSE_FRONTIER_UPSTREAM`
    /// this build has, and both are files an operator edits.
    pub fn for_provider(
        &self,
        provider: &str,
    ) -> Result<&std::sync::Arc<dyn FrontierClient>, FrontierError> {
        self.by_provider
            .get(provider)
            .or(self.uniform.as_ref())
            .ok_or_else(|| FrontierError::UnknownProvider(provider.to_string()))
    }

    /// The provider names this registry answers for, for a boot log line.
    ///
    /// Empty on a uniform registry, which answers for every name and so has no
    /// list to print — the log line at the composition root says which shape it
    /// built rather than inferring it from a count.
    pub fn provider_names(&self) -> impl Iterator<Item = &str> {
        self.by_provider.keys().map(String::as_str)
    }
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
    use roundhouse_core::control::Secret;

    const MINUTE: u64 = 60_000;

    /// Every arm, classified, because a failover trigger that is right about
    /// five shapes and wrong about the sixth fails over on a bad API key —
    /// which is a way of trying the same wrong credential against every
    /// provider in the tier.
    #[test]
    fn only_the_shapes_that_never_reached_a_model_are_worth_another_target() {
        let transport = FrontierError::Transport {
            message: "connection refused".into(),
            timed_out: false,
        };
        assert_eq!(transport.failover_class(), Some(AttemptClass::Transport));

        let timed_out = FrontierError::Transport {
            message: "operation timed out".into(),
            timed_out: true,
        };
        assert_eq!(timed_out.failover_class(), Some(AttemptClass::Timeout));

        for status in [408u16, 429, 500, 503] {
            assert_eq!(
                FrontierError::Status {
                    status,
                    message: "busy".into(),
                }
                .failover_class(),
                Some(AttemptClass::Status { status }),
                "{status} says `not now`"
            );
        }

        // The discriminating half. Each of these is somebody's mistake, and a
        // second target repeats it.
        for terminal in [
            FrontierError::Status {
                status: 401,
                message: "invalid api key".into(),
            },
            FrontierError::Status {
                status: 404,
                message: "no such model".into(),
            },
            FrontierError::Status {
                status: 422,
                message: "unknown field".into(),
            },
            FrontierError::UnknownProvider("moonshot".into()),
            FrontierError::Upstream("the upstream sent an unparseable event".into()),
            FrontierError::UnsupportedDialect {
                expected: "openai_responses",
                got: "anthropic_messages",
                target: "anthropic/claude".into(),
            },
        ] {
            assert_eq!(
                terminal.failover_class(),
                None,
                "`{terminal}` is an answer, not an outage"
            );
        }
    }

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

    /// A plaintext with no substring in common with any fingerprint, marker or
    /// field name in the quote, so a scan that finds it found the real thing.
    const LIVE_KEY: &str = "sk-live-ZZZQQQ0000-do-not-log-me";

    fn quote_with(credential: TurnCredential) -> FrontierQuote {
        FrontierQuote {
            target: Target::Frontier {
                provider: "anthropic".into(),
                model: "claude".into(),
            },
            wire_protocol: WireProtocol::AnthropicMessages,
            prompt: "some prompt".into(),
            prompt_cache_key: "sess_x".into(),
            expected_output_tokens: None,
            credential,
        }
    }

    #[test]
    fn a_quote_never_carries_a_secret() {
        let secret = Secret::api_key(LIVE_KEY).expect("an ordinary API key");
        let fingerprint = secret.fingerprint().to_string();
        let quote = quote_with(TurnCredential::Stored(secret));

        // PROBE. The two ways a quote reaches a log: a `tracing` field on the
        // dispatch, which is `Debug`, and any serialization of the credential
        // it carries. Neither may contain the key.
        for (surface, rendered) in [
            ("Debug of the whole quote", format!("{quote:?}")),
            ("Debug of the credential", format!("{:?}", quote.credential)),
            (
                "serde_json of the credential",
                serde_json::to_string(&quote.credential).unwrap(),
            ),
        ] {
            assert!(
                !rendered.contains(LIVE_KEY),
                "{surface} disclosed the key: {rendered}"
            );
            assert!(
                rendered.contains(&fingerprint),
                "{surface} must still identify the key by fingerprint: {rendered}"
            );
        }

        // CONTROL, and it is what makes the assertions above about *rendering*
        // rather than about the quote having lost the key: the one named seam
        // a client's `execute` calls does return it.
        assert_eq!(
            quote.credential.require_api_key("anthropic").unwrap(),
            LIVE_KEY
        );

        // The two arms that carry nothing to disclose still say which they are,
        // because a client has to tell "nobody resolved a key" from "the key
        // travels in the client's own headers" and must refuse on both.
        assert_eq!(
            format!("{:?}", quote_with(TurnCredential::Absent).credential),
            "Absent"
        );
        let forwarded =
            roundhouse_core::control::PresentedCredential::captured(|name| match name {
                "authorization" => Some("Bearer eyJhbGciOiJub25lIn0.e30.seat".to_string()),
                _ => None,
            })
            .expect("a bearer was presented")
            .for_provider("openai")
            .expect("openai has an allowlist row");
        let quote = quote_with(TurnCredential::Forwarded(forwarded));
        assert!(quote.credential.is_forwarded());
        // A forwarded credential carries the caller's own bearer, so the
        // no-secret promise has to hold for it too -- and it holds through a
        // different mechanism from the stored arm's, which is why both are
        // asserted rather than one standing in for the other.
        for rendered in [
            format!("{quote:?}"),
            serde_json::to_string(&quote.credential).unwrap(),
        ] {
            assert!(!rendered.contains("seat"), "{rendered}");
        }
    }

    #[tokio::test]
    async fn the_echo_client_reports_usage() {
        let client = EchoFrontierClient::new("hello");
        let stream = client
            .execute(&quote_with(TurnCredential::Absent))
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
