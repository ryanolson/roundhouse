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
//!
//! The breakpoint half of that promise is discharged by
//! [`FrontierQuote::segment_boundaries`] and read by the Anthropic client, which
//! is the only dialect roundhouse speaks where a cache exists but does nothing
//! without one.

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
        /// Prompt tokens the provider wrote into its cache on this call.
        ///
        /// A *component* of `input_tokens`, exactly as `cached_input_tokens` is:
        /// on the Anthropic wire the three counters are disjoint and the client
        /// folds them into roundhouse's total, so adding this to `input_tokens`
        /// downstream would double-count every cached prompt.
        ///
        /// Zero on a dialect that does not report one, which reads as "nothing
        /// was written" — the honest answer on the Responses wire, where a cache
        /// write is not a separately billed event at all. The dialect where the
        /// distinction would matter is the one that reports the number.
        cache_write_tokens: u64,
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
                // Not a parameter, so that the dozen call sites that adapt a
                // non-streaming backend do not each have to answer a question
                // none of them can: a backend handed token counts by its caller
                // was told nothing about a remote cache write, and zero is what
                // "nothing was written" reads as.
                cache_write_tokens: 0,
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
    /// Byte offsets into [`Self::prompt`] where one conversation item's render
    /// ends and the next begins.
    ///
    /// **Additive, and it exists because Anthropic caches nothing without an
    /// explicit breakpoint.** Every other dialect roundhouse speaks caches on a
    /// steering key and needs no structure in the prompt at all, which is why
    /// this stayed unwritten through M10 while `frontier.rs`'s own module doc
    /// promised it. A client that sends the flat string to a Messages upstream
    /// gets a 0% hit rate forever while the router keeps pricing that target on
    /// a `CacheModel::Deterministic` prediction nothing can fulfil.
    ///
    /// **A slicing, never a second rendering.** These are offsets *into the
    /// canonical render*, so the blocks a client cuts from them rejoin to
    /// `prompt` byte-exactly and `turn_id_for`, the block hashes and
    /// `ContextAssembler::rendered` stay one projection. Re-deriving roles or
    /// items here would be a second projection able to disagree with the log —
    /// the trade the seam map states both ways, resolved this way by R3.
    ///
    /// Empty means "no structure known", which every existing construction site
    /// means and which [`Self::segments`] answers with the whole prompt as one
    /// segment. Strictly increasing, each strictly inside `0..prompt.len()`, and
    /// each on a UTF-8 character boundary; anything else is refused by
    /// [`Self::segments`] rather than sliced.
    pub segment_boundaries: Vec<usize>,
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

impl FrontierQuote {
    /// [`Self::prompt`], cut at [`Self::segment_boundaries`].
    ///
    /// **Fallible, and the refusal is the point.** Slicing a `String` at an
    /// offset that is not a character boundary panics, and slicing at a
    /// *plausible but wrong* offset does something worse: it sends the model a
    /// differently-cut prompt than the one the turn was priced, hashed and
    /// routed on, silently. So every way a boundary list can fail to describe
    /// this prompt is refused here — past the end, out of order, repeated, at
    /// either edge (which would produce an empty block that the Messages schema
    /// rejects outright), or mid-codepoint.
    ///
    /// An empty boundary list is not a failure: it is a caller that knows
    /// nothing about item structure, and the whole prompt as one segment is the
    /// correct answer for it.
    pub fn segments(&self) -> Result<Vec<&str>, FrontierError> {
        if self.segment_boundaries.is_empty() {
            return Ok(vec![&self.prompt]);
        }
        let mut previous = 0usize;
        for &boundary in &self.segment_boundaries {
            if boundary <= previous || boundary >= self.prompt.len() {
                return Err(FrontierError::MalformedQuote(format!(
                    "boundary {boundary} is not strictly inside \
                     ({previous}, {}) for a {}-byte prompt",
                    self.prompt.len(),
                    self.prompt.len()
                )));
            }
            if !self.prompt.is_char_boundary(boundary) {
                return Err(FrontierError::MalformedQuote(format!(
                    "boundary {boundary} falls inside a UTF-8 character"
                )));
            }
            previous = boundary;
        }

        let mut segments = Vec::with_capacity(self.segment_boundaries.len() + 1);
        let mut start = 0usize;
        for &boundary in &self.segment_boundaries {
            segments.push(&self.prompt[start..boundary]);
            start = boundary;
        }
        segments.push(&self.prompt[start..]);
        Ok(segments)
    }
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
    /// The quote's own structure does not describe the prompt it carries.
    ///
    /// Its own arm for the same reason [`Self::Credential`] and
    /// [`Self::UnsupportedDialect`] have theirs: nothing reached an upstream,
    /// and nothing should. This is not hostile input — a quote is built inside
    /// this process — so it is a bug, and the two alternatives are both worse
    /// than a loud failure. Slicing anyway panics on a mid-codepoint offset;
    /// falling back to the flat prompt silently drops the cache breakpoints and
    /// takes the provider discount with them, on every turn, with nothing said.
    #[error(
        "the quote's segment structure does not describe its prompt: {0}; refusing to \
         slice a prompt at offsets that would change what the model is asked"
    )]
    MalformedQuote(String),
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
            | FrontierError::MalformedQuote(_)
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
    use roundhouse_core::context::{ByteTokenizer, ContextAssembler};
    use roundhouse_core::control::Secret;
    use roundhouse_core::ids::ResponseId;
    use roundhouse_core::item::Item;
    use roundhouse_core::validate::append_handoff_note;

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
            FrontierError::MalformedQuote("boundary 9999 is past the end".into()),
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
            segment_boundaries: Vec::new(),
            prompt_cache_key: "sess_x".into(),
            expected_output_tokens: None,
            credential,
        }
    }

    fn segmented(prompt: &str, boundaries: Vec<usize>) -> FrontierQuote {
        FrontierQuote {
            prompt: prompt.to_string(),
            segment_boundaries: boundaries,
            ..quote_with(TurnCredential::Absent)
        }
    }

    /// **The invariant every cache breakpoint rests on: the segments rejoin to
    /// the prompt byte-exactly.**
    ///
    /// If they did not, a client would send the model a differently-cut prompt
    /// than the one `turn_id_for` hashed and the router priced — and it would do
    /// it silently, because nothing downstream compares the two.
    #[test]
    fn segments_are_a_slicing_of_the_prompt_and_rejoin_to_it() {
        // Offsets derived from the renders rather than typed: a hand-counted
        // one that is off by a byte still slices, still rejoins, and quietly
        // moves the cache breakpoint into the middle of an item -- which is
        // exactly the class of mistake this seam exists to make impossible, so
        // the test must not be able to make it either.
        let renders = ["<|system|>be brief", "<|user|>hello", "<|assistant|>hi"];
        let prompt = renders.concat();
        let boundaries = vec![renders[0].len(), renders[0].len() + renders[1].len()];
        let quote = segmented(&prompt, boundaries);
        let segments = quote.segments().unwrap();

        assert_eq!(segments, renders.to_vec());
        assert_eq!(segments.concat(), prompt);

        // No structure known is one segment, not zero and not an error: a
        // caller that never learned the item boundaries still has a prompt.
        let flat = segmented(&prompt, Vec::new());
        assert_eq!(flat.segments().unwrap(), vec![prompt.as_str()]);
        assert_eq!(flat.segments().unwrap().concat(), prompt);
    }

    /// **The producer and the consumer, joined: what `ContextAssembler` names
    /// is what `segments()` accepts.**
    ///
    /// The two halves live in different crates and neither test above sees the
    /// other, so a boundary convention that drifted — interior versus leading,
    /// byte versus char, cumulative versus per-item — would leave both suites
    /// green and every Anthropic dispatch refused at the seam. This is the test
    /// that goes red for that.
    #[test]
    fn boundaries_the_assembler_produces_are_boundaries_the_quote_accepts() {
        let items = [
            Item::system_text("you are a careful assistant"),
            Item::user_text("first question"),
            Item::assistant_text("first answer", ResponseId::new("r1")),
            // Multi-byte, because a convention that counted characters rather
            // than bytes would agree with itself on ASCII forever.
            Item::user_text("¿segunda pregunta?"),
        ];
        let mut assembler = ContextAssembler::new(ByteTokenizer, 16);
        for item in items.clone() {
            assembler.push(item);
        }
        let (rendered, segment_boundaries) = assembler.rendered_with_boundaries();

        let quote = FrontierQuote {
            prompt: rendered.clone(),
            segment_boundaries,
            ..quote_with(TurnCredential::Absent)
        };
        let segments = quote.segments().expect("the assembler's own boundaries");
        assert_eq!(segments, items.iter().map(Item::render).collect::<Vec<_>>());
        assert_eq!(segments.concat(), rendered);

        // And the decoration the engine may append to this render does not
        // invalidate them: the note goes on the *end*, so every interior offset
        // still names the same item edge and the note lands inside the final
        // segment — which is where it belongs, being the one part of the prompt
        // that is new this turn and must not sit inside a cached block.
        let decorated = FrontierQuote {
            prompt: append_handoff_note(rendered.clone(), "narrow the search"),
            ..quote.clone()
        };
        let decorated_segments = decorated
            .segments()
            .expect("a note is appended, not spliced");
        assert_eq!(decorated_segments.concat(), decorated.prompt);
        assert_eq!(
            decorated_segments[..segments.len() - 1],
            segments[..segments.len() - 1],
            "only the final segment may differ, and it differs by the note"
        );
        assert!(
            decorated_segments
                .last()
                .unwrap()
                .contains("narrow the search")
        );
    }

    #[test]
    fn a_boundary_list_that_does_not_describe_the_prompt_is_refused_at_the_seam() {
        let prompt = "<|user|>héllo<|assistant|>hi";
        // PROBE: every shape of wrong. Each would otherwise either panic in the
        // slice or produce a block the Messages schema rejects, and the fifth
        // would do neither -- it would quietly send a different prompt.
        for (bad, why) in [
            (vec![prompt.len()], "at the end: an empty final block"),
            (vec![0], "at the start: an empty first block"),
            (vec![prompt.len() + 1], "past the end"),
            (vec![8, 8], "repeated: an empty block between them"),
            (vec![20, 8], "out of order"),
            (vec![10], "mid-codepoint, which would panic the slice"),
        ] {
            let error = segmented(prompt, bad.clone())
                .segments()
                .expect_err(&format!("{bad:?} ({why}) must be refused"));
            assert!(
                matches!(error, FrontierError::MalformedQuote(_)),
                "{error} for {bad:?} ({why})"
            );
        }

        // CONTROL: the same prompt with boundaries that *do* describe it slices,
        // so the assertions above are about the boundaries and not about the
        // check refusing everything with a multi-byte character in it.
        let good = segmented(prompt, vec![14]);
        assert_eq!(good.segments().unwrap().concat(), prompt);
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
