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
use serde_json::json;

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
    /// The model asked for a tool to be run.
    ///
    /// **The variant that makes an agentic client usable through roundhouse at
    /// all.** Until M11.2 this enum could carry only prose, so a Claude Code or
    /// codex turn whose whole purpose is `Read`, `Bash` or `Grep` decoded to an
    /// empty answer and the client's loop stalled on its first tool turn — the
    /// finding that re-scoped this milestone. Roundhouse still runs no tool
    /// itself: the client does, exactly as on the Responses surface, and this is
    /// the channel that carries the *request* back out to it.
    ///
    /// Emitted once per completed tool block, never per fragment. Both wires
    /// stream the arguments in pieces — Anthropic as `input_json_delta`
    /// fragments between a `content_block_start` and its `content_block_stop`,
    /// the Responses wire as `response.function_call_arguments.delta` — and a
    /// fragment is not JSON on its own, so a per-fragment chunk would hand every
    /// consumer the same reassembly problem and let two of them disagree about
    /// it. A block that never completes emits nothing, which is the same rule as
    /// [`Self::Done`] and for the same reason: half a tool call is not a smaller
    /// tool call, it is a call to a tool with arguments nobody can parse.
    ToolCall {
        /// The provider's own id for this call, echoed back on the result.
        ///
        /// Anthropic's `tool_use.id`, the Responses wire's `call_id`. Verbatim
        /// in both cases: it is an opaque token whose only job is to pair a
        /// result with the call, and rewriting it breaks the pairing.
        id: String,
        name: String,
        /// The accumulated argument JSON, as a *string*.
        ///
        /// Field-shaped to match
        /// [`ItemContent::ToolCall::arguments`](roundhouse_core::item::ItemContent),
        /// so the join to a durable item is a field-for-field one. A parsed
        /// `Value` would have been the tempting choice and is the wrong one: the
        /// Responses wire hands this over already-a-string (the codex oracle's
        /// own `ResponseItem::FunctionCall::arguments` is a `String`, with a
        /// comment saying why), and a fragment run that does not parse is
        /// something a decoder must be able to carry rather than reject — a
        /// `Value` here would make an unparseable call unrepresentable and turn
        /// it into a lost turn instead of a call the client can refuse.
        ///
        /// **What this is *not* is the log's spelling.** M11.2's wiring stage
        /// found that storing the model's own bytes forks every tool-using
        /// session on its second turn, because the client resends the call as a
        /// JSON object and canonicalizing that resend sorts the keys. The
        /// durable form is
        /// [`canonical_arguments`](roundhouse_core::item::canonical_arguments)
        /// of this string, applied once where the chunk becomes an item; this
        /// field stays what the upstream said, which is the only thing a
        /// dispatch decoder can honestly report.
        arguments: String,
    },
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
        /// Why the provider stopped, in the provider's own word.
        ///
        /// **An open string and deliberately not an enum.** This is what the
        /// wire said, and the log's job is to record that rather than to
        /// classify it: Anthropic ships seven values and added two of them after
        /// the crates that closed the enum shipped, the Responses wire spells
        /// the same facts differently again, and a dialect-neutral enum here
        /// would have to invent a shared vocabulary that neither provider uses.
        /// The mapping into whatever a *client* is owed belongs to the emit
        /// layer that knows which dialect it is answering in — which is also the
        /// only layer that could be right about it.
        ///
        /// `None` means the provider named no reason, which is the ordinary
        /// answer on a wire that has no such field for an ordinary completion,
        /// and is emphatically not the same as "it finished normally".
        ///
        /// **Why this is on `Done` rather than inferred downstream** (M11.1's
        /// F1, reporting half): a turn cut off at the dispatch ceiling arrives
        /// as `stop_reason: max_tokens` and is otherwise byte-identical to one
        /// that finished on its own, so a decoder that discards this makes the
        /// two indistinguishable *everywhere after it* — no client can be told,
        /// and no operator reading the log can find out. `tool_use` is the same
        /// fact for the agentic loop: it is how a client knows the turn is
        /// waiting on it rather than over.
        stop_reason: Option<String>,
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
                // And no stop reason, for the same reason and not as an
                // oversight: a caller that handed this adapter a finished string
                // told it nothing about *why* the model stopped, and
                // synthesizing `end_turn` here would put a claim in the log that
                // nobody made. `None` reads as "the provider named no reason",
                // which is exactly what happened.
                stop_reason: None,
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
    /// What the *router* expects this turn to produce, for pricing.
    ///
    /// An estimate, and never a ceiling. It is the number every candidate was
    /// quoted against and the number the spend grant was opened for, so the
    /// client's declared ceiling must not be written here: a client that
    /// routinely declares `max_tokens: 64000` and answers in forty would
    /// inflate every quote, every reservation and every projected saving by
    /// three orders of magnitude, on every turn, while changing nothing about
    /// what the model actually did.
    ///
    /// **Split from [`Self::output_token_cap`] by M11.1's F1**, where the two
    /// meanings shared this one field and the estimate was therefore acting as
    /// the dispatch ceiling — a 256-token default truncating every real answer.
    /// Two fields because the two numbers are answers to different questions,
    /// and because collapsing them makes one of the two wrong whichever value
    /// wins.
    pub expected_output_tokens: Option<u32>,
    /// The ceiling the *client* asked for, when the serve surface has one.
    ///
    /// This is what goes on the wire as `max_tokens` / `max_output_tokens`: a
    /// hard limit on the answer, declared by the caller, that costs nothing
    /// until the model reaches it. `None` means the client declared none —
    /// every internal caller (the judge, the validate loop, an MCP turn) and
    /// every dialect whose surface has no such field — and each client then
    /// falls back to what its own dialect requires: the Anthropic client to its
    /// `DEFAULT_MAX_TOKENS`, since the Messages schema requires the property,
    /// and the Responses client to [`Self::expected_output_tokens`], which is
    /// the semantics it shipped with.
    ///
    /// Additive, deliberately: a construction site that does not set it gets
    /// `None` and the behaviour it had before the split.
    pub output_token_cap: Option<u32>,
    /// The tool definitions the client declared, as the client's own JSON.
    ///
    /// **Untyped on purpose, and it is the one field here where that is a
    /// ruling rather than a shortcut.** The quote is *transport*: it carries a
    /// turn from the engine to whichever client speaks the target's dialect, and
    /// the wire modules own shape. A typed re-encoding here would be a third
    /// projection of the same tool definitions — the client's bytes, this
    /// struct's types, the dialect module's types — and three projections of one
    /// thing is two chances to disagree. They disagree about exactly what
    /// matters: a tool schema this build does not model (`cache_control` on the
    /// last tool, a server-tool type, whatever the next beta adds) would be
    /// silently dropped on the way through, and the model would then be told
    /// about a smaller toolbox than the client has — which surfaces as a client
    /// whose tools mysteriously stop working, never as an error.
    ///
    /// So the dialects' shapes are *different* JSON and this field is whichever
    /// one the surface that built the quote received — which is the honest
    /// description of what this value is: the caller's bytes, on their way to an
    /// upstream that may or may not have defined them.
    ///
    /// **The sentence that used to stand here — "the serve surface and the
    /// dispatch client always speak the same dialect for a pass-through turn" —
    /// was false, and it is M11.2a's F1.** Routing picks a target by price,
    /// quality and TTFT with no read of the declaring surface, so an Anthropic
    /// client's toolbox reaches a Responses target on any deployment whose
    /// catalog mixes dialects — which the shipped example catalog does. The
    /// premise is now a *field*, [`Self::tools_dialect`], and the seam that
    /// reconciles the two is [`Self::tools_for`].
    ///
    /// `None` means the client declared no tools — every internal caller (the
    /// judge, the validate loop, an MCP turn) and any client that simply did not
    /// send any. Additive: a construction site that does not set it sends no
    /// `tools` key at all, which is what every dispatch did before M11.2.
    ///
    /// **Read through [`Self::tools_for`] and never directly**, because these
    /// bytes are only meaningful beside [`Self::tools_dialect`] — see F1 below.
    pub tools: Option<serde_json::Value>,
    /// How the client wants the model to choose among [`Self::tools`], verbatim.
    ///
    /// Separate from `tools` rather than folded in beside it because the wire
    /// keeps them separate on both dialects, and because they are independently
    /// optional: a client may declare tools and say nothing about choosing, and
    /// `tool_choice` without `tools` is a request the *upstream* should refuse
    /// with a message naming the field — not one this struct should make
    /// unrepresentable and thereby hide.
    ///
    /// Shaped in [`Self::tools_dialect`] like [`Self::tools`], and read through
    /// [`Self::tools_for`] for the same reason.
    pub tool_choice: Option<serde_json::Value>,
    /// The dialect [`Self::tools`] and [`Self::tool_choice`] are written in.
    ///
    /// **Stamped by the serve surface that accepted them, and it is not
    /// [`Self::wire_protocol`].** That field is the dialect of the *target this
    /// turn resolved to*; this one is the dialect of the *client that declared
    /// the toolbox*, and M11.2a's F1 is the whole argument for why one field
    /// cannot be both. `frontier.rs` used to assert the premise directly — "the
    /// serve surface and the dispatch client always speak the same dialect for a
    /// pass-through turn" — and the premise is false by construction: routing
    /// picks a target by price, quality and TTFT with no read of the declaring
    /// surface anywhere on that path, and the shipped example catalog pairs four
    /// `openai_responses` entries with one `anthropic_messages` entry, so a
    /// Claude Code turn crossing to a Responses target was posting
    /// `{name, description, input_schema}` where the upstream requires
    /// `{type: "function", …, parameters}` — a 400 on every tool-using turn,
    /// which `plan`'s failover then repeated on the next same-dialect candidate.
    ///
    /// `None` means nothing declared a toolbox, which is every internal caller
    /// (the judge, the validate loop, an MCP turn) and every client that sent no
    /// tools. `None` *beside* a `Some` in either field above is a construction
    /// bug and [`Self::tools_for`] refuses it as a [`FrontierError::MalformedQuote`]
    /// rather than guessing a dialect: guessing wrong is exactly the silent
    /// mis-serialization this field exists to end.
    pub tools_dialect: Option<WireProtocol>,
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

    /// [`Self::tools`] and [`Self::tool_choice`], shaped for the dialect
    /// `spoken` by the client about to serialize them.
    ///
    /// **The one seam a dispatch client reads a toolbox through, and the answer
    /// to M11.2a's F1.** Three outcomes, and the third is the point:
    ///
    /// - **Verbatim** when the declaring surface and the dispatch client speak
    ///   the same dialect, which is every same-dialect deployment and therefore
    ///   the overwhelmingly common path. The client's bytes cross untouched —
    ///   input schemas this build has never modelled, `cache_control`
    ///   breakpoints, server tools and all — which is the property the existing
    ///   thinning guards in both clients assert and which this function must not
    ///   quietly weaken.
    /// - **Translated** when they differ and the entry is a plain function tool.
    ///   Anthropic's `{name, description, input_schema}` and the Responses
    ///   wire's `{type: "function", name, description, parameters, strict?}`
    ///   are the same declaration spelled twice — the Responses shape read from
    ///   the pinned codex crates (`codex-rs/tools/src/responses_api.rs`
    ///   @ `6344a65`), which are this tree's wire oracle — so restating one as
    ///   the other loses nothing the model can act on.
    /// - **Refused, before any socket**, for an entry that is *not* that core: a
    ///   server tool (`web_search_20250305`, the Responses wire's `web_search`,
    ///   `custom`, `namespace`), a shape carrying a key this translation does not
    ///   understand, or a `tool_choice` spelling outside the four both dialects
    ///   have. The error names the offending tool and both dialects, because the
    ///   two alternatives are worse in the two ways this codebase cares about: a
    ///   verbatim forward is a 400 the client cannot read, and a *thinned*
    ///   toolbox — dropping what we cannot restate — tells the model about
    ///   fewer tools than the client has and surfaces as tools that
    ///   mysteriously stop working, never as an error.
    ///
    /// `cache_control` is the one key translated *away* rather than refused, and
    /// the loss is priced rather than hidden: it is a caching directive with no
    /// counterpart on the Responses wire, so dropping it costs the provider
    /// discount on the tool preamble and costs the model nothing. Refusing the
    /// turn over it would make a real Claude Code toolbox — which caches its
    /// own preamble exactly this way — uncrossable for a reason that is about
    /// money rather than capability.
    pub fn tools_for(
        &self,
        spoken: WireProtocol,
    ) -> Result<(Option<serde_json::Value>, Option<serde_json::Value>), FrontierError> {
        if self.tools.is_none() && self.tool_choice.is_none() {
            return Ok((None, None));
        }
        // A toolbox with no stamp is a quote this process built wrong, not
        // hostile input — the same class as a segment structure that does not
        // describe its prompt, and refused the same way. Guessing "probably the
        // target's dialect" here would restore precisely the false premise F1
        // found, and it would restore it invisibly.
        let Some(declared) = self.tools_dialect else {
            return Err(FrontierError::MalformedQuote(
                "the quote carries tool declarations but no `tools_dialect`, so nothing knows \
                 which dialect they are written in"
                    .to_string(),
            ));
        };
        if declared == spoken {
            return Ok((self.tools.clone(), self.tool_choice.clone()));
        }
        let tools = self
            .tools
            .as_ref()
            .map(|tools| translate_tools(tools, declared, spoken))
            .transpose()?;
        let tool_choice = self
            .tool_choice
            .as_ref()
            .map(|choice| translate_tool_choice(choice, declared, spoken))
            .transpose()?;
        Ok((tools, tool_choice))
    }
}

/// The refusal every untranslatable shape becomes, named the same way.
fn untranslatable(tool: impl Into<String>, from: WireProtocol, to: WireProtocol) -> FrontierError {
    FrontierError::UntranslatableTools {
        tool: tool.into(),
        from: from.wire_name(),
        to: to.wire_name(),
    }
}

/// Every entry of a `tools` array, restated in `to`.
fn translate_tools(
    tools: &serde_json::Value,
    from: WireProtocol,
    to: WireProtocol,
) -> Result<serde_json::Value, FrontierError> {
    let entries = tools
        .as_array()
        .ok_or_else(|| untranslatable("the `tools` payload, which is not an array", from, to))?;
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| translate_tool(entry, index, from, to))
        .collect::<Result<Vec<_>, _>>()
        .map(serde_json::Value::Array)
}

/// One tool declaration, restated in `to`.
fn translate_tool(
    entry: &serde_json::Value,
    index: usize,
    from: WireProtocol,
    to: WireProtocol,
) -> Result<serde_json::Value, FrontierError> {
    // Named by the tool's own name where it has one, because that is the string
    // an operator greps for in a client's config; positionally otherwise, since
    // "some tool" would leave nobody anywhere to look.
    let named = |object: Option<&serde_json::Map<String, serde_json::Value>>| {
        object
            .and_then(|object| object.get("name"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("tools[{index}]"))
    };
    let object = entry.as_object();
    let name = named(object);
    let object = object.ok_or_else(|| untranslatable(name.clone(), from, to))?;
    let allowed = |keys: &[&str]| -> Result<(), FrontierError> {
        match object.keys().all(|key| keys.contains(&key.as_str())) {
            true => Ok(()),
            false => Err(untranslatable(name.clone(), from, to)),
        }
    };

    match (from, to) {
        (WireProtocol::AnthropicMessages, WireProtocol::OpenAiResponses) => {
            // `type` is absent on an Anthropic *function* tool and present on
            // every server tool, so it is not on this list and a server tool
            // refuses here — which is the whole intent: roundhouse cannot run
            // Anthropic's server-side tools on OpenAI's behalf, and pretending
            // otherwise would drop a capability the client declared.
            allowed(&["name", "description", "input_schema", "cache_control"])?;
            let schema = object
                .get("input_schema")
                .ok_or_else(|| untranslatable(name.clone(), from, to))?;
            let mut out = serde_json::Map::new();
            out.insert(
                "type".to_string(),
                serde_json::Value::String("function".into()),
            );
            out.insert("name".to_string(), serde_json::Value::String(name));
            if let Some(description) = object.get("description") {
                out.insert("description".to_string(), description.clone());
            }
            out.insert("parameters".to_string(), schema.clone());
            Ok(serde_json::Value::Object(out))
        }
        (WireProtocol::OpenAiResponses, WireProtocol::AnthropicMessages) => {
            // `strict` is accepted and dropped: Anthropic has no property for
            // it, and it constrains how the *model* is decoded rather than what
            // the tool is, so restating the tool without it still declares the
            // same tool. `defer_loading` and `output_schema` are deliberately
            // not on this list — the first changes when the model is told about
            // the tool at all, and the second is a contract on the result.
            allowed(&["type", "name", "description", "parameters", "strict"])?;
            if object.get("type").and_then(serde_json::Value::as_str) != Some("function") {
                return Err(untranslatable(name, from, to));
            }
            let parameters = object
                .get("parameters")
                .ok_or_else(|| untranslatable(name.clone(), from, to))?;
            let mut out = serde_json::Map::new();
            out.insert("name".to_string(), serde_json::Value::String(name));
            if let Some(description) = object.get("description") {
                out.insert("description".to_string(), description.clone());
            }
            out.insert("input_schema".to_string(), parameters.clone());
            Ok(serde_json::Value::Object(out))
        }
        // Chat Completions spells tools a third way and no serve surface speaks
        // it, so a quote claiming it is a configuration nobody has exercised.
        // An arm rather than a wildcard: a fourth dialect is a compile error
        // here, which is where the decision belongs.
        (WireProtocol::OpenAiChatCompletions, _)
        | (_, WireProtocol::OpenAiChatCompletions)
        | (WireProtocol::AnthropicMessages, WireProtocol::AnthropicMessages)
        | (WireProtocol::OpenAiResponses, WireProtocol::OpenAiResponses) => {
            Err(untranslatable(name, from, to))
        }
    }
}

/// `tool_choice`, restated in `to`.
///
/// Four spellings both dialects have — auto, any/required, none, and one named
/// tool — and nothing else. Anthropic's `disable_parallel_tool_use` is
/// deliberately not among them: the Responses wire carries that fact in a
/// *separate top-level* `parallel_tool_calls` property, so honouring it would
/// mean this function editing a field it was not given, and dropping it would
/// silently let a model fan out where the client asked it not to.
fn translate_tool_choice(
    choice: &serde_json::Value,
    from: WireProtocol,
    to: WireProtocol,
) -> Result<serde_json::Value, FrontierError> {
    let refuse = || untranslatable("tool_choice", from, to);
    match (from, to) {
        (WireProtocol::AnthropicMessages, WireProtocol::OpenAiResponses) => {
            let object = choice.as_object().ok_or_else(refuse)?;
            let kind = object
                .get("type")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(refuse)?;
            match kind {
                "auto" | "any" | "none" if object.len() == 1 => Ok(serde_json::Value::String(
                    match kind {
                        "auto" => "auto",
                        "any" => "required",
                        _ => "none",
                    }
                    .to_string(),
                )),
                "tool" if object.len() == 2 => {
                    let name = object
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(refuse)?;
                    Ok(json!({ "type": "function", "name": name }))
                }
                _ => Err(refuse()),
            }
        }
        (WireProtocol::OpenAiResponses, WireProtocol::AnthropicMessages) => {
            if let Some(mode) = choice.as_str() {
                return match mode {
                    "auto" => Ok(json!({ "type": "auto" })),
                    "required" => Ok(json!({ "type": "any" })),
                    "none" => Ok(json!({ "type": "none" })),
                    _ => Err(refuse()),
                };
            }
            let object = choice.as_object().ok_or_else(refuse)?;
            if object.get("type").and_then(serde_json::Value::as_str) != Some("function")
                || object.len() != 2
            {
                return Err(refuse());
            }
            let name = object
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(refuse)?;
            Ok(json!({ "type": "tool", "name": name }))
        }
        (WireProtocol::OpenAiChatCompletions, _)
        | (_, WireProtocol::OpenAiChatCompletions)
        | (WireProtocol::AnthropicMessages, WireProtocol::AnthropicMessages)
        | (WireProtocol::OpenAiResponses, WireProtocol::OpenAiResponses) => Err(refuse()),
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
    /// The client's toolbox is shaped for one dialect and this turn resolved to
    /// a target speaking another, and one entry cannot be honestly restated.
    ///
    /// **Its own arm, and it refuses before a socket, for the reason
    /// [`Self::UnsupportedDialect`] does — with a sharper edge.** Sending anyway
    /// is a 400 the client cannot read, on every tool-using turn, which
    /// `plan`'s failover then repeats against the next candidate in the same
    /// dialect. Sending a *thinned* toolbox — dropping the entries that do not
    /// translate — is worse: the model is told about fewer tools than the
    /// client has, answers without them, and nothing anywhere reports a
    /// failure. Naming the tool and both dialects is what makes the remedy
    /// findable: either the deployment separates the two dialects' catalogs, or
    /// the client stops declaring a tool no other dialect has.
    #[error(
        "the client declared tool `{tool}` in `{from}` and this turn resolved to a `{to}` \
         target, which has no faithful spelling for it; refusing rather than sending a \
         mis-shaped or thinned toolbox"
    )]
    UntranslatableTools {
        tool: String,
        from: &'static str,
        to: &'static str,
    },
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
            // A second target does not fix a toolbox nobody can restate: the
            // failover candidates that share the chosen target's dialect fail
            // identically, and the ones that do not are the reason this refusal
            // exists. The remedy is a catalog or a client change, not a retry.
            | FrontierError::UntranslatableTools { .. }
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
    use roundhouse_core::item::{Item, ItemContent, canonical_arguments};
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
            FrontierError::UntranslatableTools {
                tool: "web_search".into(),
                from: "anthropic_messages",
                to: "openai_responses",
            },
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
            output_token_cap: None,
            tools: None,
            tool_choice: None,
            tools_dialect: None,
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

    /// **The join to a durable item, and the correction M11.2's wiring stage
    /// made to it.**
    ///
    /// The variant exists so a decoder can hand a completed tool call to the
    /// engine, and the engine's only durable home for one is
    /// [`ItemContent::ToolCall`]: same three fields, same types, so the join is
    /// a field-for-field mapping with nothing to invent.
    ///
    /// What the first reading of this test asserted — that the arguments cross
    /// that join *byte-exactly* — was wrong, and wrong in the expensive
    /// direction. The client resends the call as history on its next turn, the
    /// Messages wire carries the arguments as a JSON **object**, and
    /// canonicalizing that object serializes a `serde_json` value: compact,
    /// key-sorted. So a log holding the model's own
    /// `{"pattern": "fn main", "path": "/a"}` is compared against
    /// `{"path":"/a","pattern":"fn main"}` and disagrees, and prefix admission
    /// forks the conversation — silently, while every turn still answers. The
    /// durable spelling is
    /// [`canonical_arguments`](roundhouse_core::item::canonical_arguments), and
    /// the serve projections put that same string back on the wire, which is
    /// what closes the round trip.
    ///
    /// The payload's keys are deliberately *not* in sorted order and its spacing
    /// is not `serde_json`'s, so a canonicalization that did nothing would fail
    /// the first assertion rather than pass it by coincidence.
    #[test]
    fn a_tool_call_chunk_becomes_a_durable_item_in_its_canonical_spelling() {
        const ARGUMENTS: &str = r#"{"pattern": "fn main", "path": "/a"}"#;
        let chunk = FrontierChunk::ToolCall {
            id: "toolu_01A".into(),
            name: "Grep".into(),
            arguments: ARGUMENTS.to_string(),
        };

        let FrontierChunk::ToolCall {
            id,
            name,
            arguments,
        } = chunk
        else {
            unreachable!("constructed as a tool call")
        };
        let item = Item::tool_call(id, name, canonical_arguments(&arguments));

        assert_eq!(
            item.content,
            ItemContent::ToolCall {
                call_id: "toolu_01A".into(),
                name: "Grep".into(),
                arguments: r#"{"path":"/a","pattern":"fn main"}"#.to_string(),
            }
        );

        // CONTROL, and it is what makes the assertion above about *this*
        // payload rather than about any string surviving: the model's spelling
        // and the canonical one really do differ here. If that ever stops being
        // true the assertion above stops proving anything, and this line goes
        // red to say so.
        assert_ne!(
            canonical_arguments(ARGUMENTS),
            ARGUMENTS,
            "the fixture must be a payload the canonicalization actually moves"
        );
        // And it is a fixed point: what the log holds canonicalizes to itself,
        // which is what makes the next turn's comparison stable rather than
        // merely correct once.
        assert_eq!(
            canonical_arguments(&canonical_arguments(ARGUMENTS)),
            canonical_arguments(ARGUMENTS)
        );
    }

    /// The two dialects' spellings of one plain function tool, and the payload
    /// every cross-dialect assertion below is built from.
    ///
    /// The Anthropic side is the shape a real Claude Code turn sends (its live
    /// capture's twenty-four entries are all exactly this: `name`,
    /// `description`, `input_schema` and nothing else); the Responses side is
    /// the shape the pinned codex crates serialize —
    /// `codex-rs/tools/src/responses_api.rs` @ `6344a65`, `ResponsesApiTool`
    /// under `#[serde(tag = "type")] ToolSpec::Function` — which is this tree's
    /// oracle for that wire.
    fn anthropic_tool() -> serde_json::Value {
        json!({
            "name": "Grep",
            "description": "search the tree",
            "input_schema": {
                "type": "object",
                "properties": { "pattern": { "type": "string" } },
                "required": ["pattern"],
            },
        })
    }

    fn responses_tool() -> serde_json::Value {
        json!({
            "type": "function",
            "name": "Grep",
            "description": "search the tree",
            "parameters": {
                "type": "object",
                "properties": { "pattern": { "type": "string" } },
                "required": ["pattern"],
            },
        })
    }

    fn declaring(
        tools: Option<serde_json::Value>,
        tool_choice: Option<serde_json::Value>,
        dialect: WireProtocol,
    ) -> FrontierQuote {
        FrontierQuote {
            tools,
            tool_choice,
            tools_dialect: Some(dialect),
            ..quote_with(TurnCredential::Absent)
        }
    }

    /// **F1: a toolbox crossing dialects is restated, and a same-dialect one is
    /// not touched at all.**
    ///
    /// The verbatim half is asserted first and is the load-bearing half of the
    /// two: it is what says this translation cannot quietly become a
    /// re-encoding on the ordinary path, where the client's bytes carry input
    /// schemas, breakpoints and server tools this build models nowhere.
    #[test]
    fn a_toolbox_crosses_dialects_by_translation_and_stays_byte_identical_within_one() {
        let anthropic = json!([anthropic_tool()]);
        let responses = json!([responses_tool()]);

        // CONTROL, and the property both clients' thinning guards rest on: same
        // dialect in and out, the caller's exact bytes -- including the two
        // shapes translation refuses, which is what proves "verbatim" is not
        // "translated and happened to match".
        let hostile = json!([
            anthropic_tool(),
            { "type": "web_search_20250305", "name": "web_search", "max_uses": 5 },
            { "name": "Read", "input_schema": { "type": "object" },
              "cache_control": { "type": "ephemeral" } },
        ]);
        let choice = json!({ "type": "auto", "disable_parallel_tool_use": false });
        let same = declaring(
            Some(hostile.clone()),
            Some(choice.clone()),
            WireProtocol::AnthropicMessages,
        );
        assert_eq!(
            same.tools_for(WireProtocol::AnthropicMessages).unwrap(),
            (Some(hostile), Some(choice))
        );

        // PROBE: Anthropic-declared, dispatched to a Responses target.
        let crossing = declaring(
            Some(anthropic.clone()),
            Some(json!({ "type": "any" })),
            WireProtocol::AnthropicMessages,
        );
        assert_eq!(
            crossing.tools_for(WireProtocol::OpenAiResponses).unwrap(),
            (Some(responses.clone()), Some(json!("required"))),
            "the Responses wire requires `type: function` and spells the schema \
             `parameters`; forwarding Anthropic's spelling is a 400 on every turn"
        );

        // And back the other way, because a Responses-speaking client (codex)
        // routed to an Anthropic target is the same defect mirrored.
        let returning = declaring(
            Some(responses),
            Some(json!({ "type": "function", "name": "Grep" })),
            WireProtocol::OpenAiResponses,
        );
        assert_eq!(
            returning
                .tools_for(WireProtocol::AnthropicMessages)
                .unwrap(),
            (
                Some(anthropic),
                Some(json!({ "type": "tool", "name": "Grep" }))
            )
        );
    }

    /// Every `tool_choice` spelling both dialects have, in both directions.
    ///
    /// A table rather than four tests because the mapping is the claim: `any`
    /// and `required` are the same instruction under two names, and a
    /// translation that got one pair backwards would let a client that demanded
    /// a tool call get a model free to answer in prose — which reads as a model
    /// being unhelpful, never as a bug here.
    #[test]
    fn tool_choice_maps_across_both_spellings_in_both_directions() {
        for (anthropic, responses) in [
            (json!({ "type": "auto" }), json!("auto")),
            (json!({ "type": "any" }), json!("required")),
            (json!({ "type": "none" }), json!("none")),
            (
                json!({ "type": "tool", "name": "Bash" }),
                json!({ "type": "function", "name": "Bash" }),
            ),
        ] {
            let out = declaring(
                None,
                Some(anthropic.clone()),
                WireProtocol::AnthropicMessages,
            )
            .tools_for(WireProtocol::OpenAiResponses)
            .unwrap_or_else(|error| panic!("{anthropic} must translate: {error}"));
            assert_eq!(out, (None, Some(responses.clone())));

            let back = declaring(None, Some(responses.clone()), WireProtocol::OpenAiResponses)
                .tools_for(WireProtocol::AnthropicMessages)
                .unwrap_or_else(|error| panic!("{responses} must translate: {error}"));
            assert_eq!(back, (None, Some(anthropic)));
        }
    }

    /// **What cannot be restated is refused by name, never thinned and never
    /// forwarded.**
    ///
    /// Each probe is a shape a real client sends: Anthropic's server tools, the
    /// Responses wire's own `web_search`/`custom` variants (both in the pinned
    /// `ToolSpec` enum), and the parallel-tool-use flag that lives in a
    /// different property on the other wire. The assertion is on the *message*
    /// as much as on the variant: an operator holding this has to be able to
    /// find the tool in a client config and the pairing in a catalog file.
    #[test]
    fn a_tool_neither_dialect_can_restate_refuses_by_name_before_any_socket() {
        let probes: Vec<(WireProtocol, WireProtocol, serde_json::Value, &str)> = vec![
            (
                WireProtocol::AnthropicMessages,
                WireProtocol::OpenAiResponses,
                json!([{ "type": "web_search_20250305", "name": "web_search", "max_uses": 5 }]),
                "web_search",
            ),
            (
                WireProtocol::AnthropicMessages,
                WireProtocol::OpenAiResponses,
                json!([{ "name": "Bash", "description": "run", "defer_loading": true }]),
                "Bash",
            ),
            (
                WireProtocol::AnthropicMessages,
                WireProtocol::OpenAiResponses,
                // No schema at all: Responses would then be told a tool takes
                // no arguments, which is a claim the client never made.
                json!([{ "name": "Read", "description": "read" }]),
                "Read",
            ),
            (
                WireProtocol::OpenAiResponses,
                WireProtocol::AnthropicMessages,
                json!([{ "type": "web_search" }]),
                "tools[0]",
            ),
            (
                WireProtocol::OpenAiResponses,
                WireProtocol::AnthropicMessages,
                json!([{ "type": "custom", "name": "patch", "format": { "type": "grammar" } }]),
                "patch",
            ),
        ];
        for (from, to, tools, named) in probes {
            let error = declaring(Some(tools.clone()), None, from)
                .tools_for(to)
                .expect_err(&format!("{tools} must be refused"));
            let FrontierError::UntranslatableTools {
                tool,
                from: f,
                to: t,
            } = &error
            else {
                panic!("{error} for {tools}")
            };
            assert_eq!(tool, named, "{error}");
            assert_eq!((*f, *t), (from.wire_name(), to.wire_name()), "{error}");
            let rendered = error.to_string();
            assert!(
                rendered.contains(named)
                    && rendered.contains(from.wire_name())
                    && rendered.contains(to.wire_name()),
                "the refusal has to name the tool and both dialects: {rendered}"
            );
        }

        // `tool_choice` the same way, and `disable_parallel_tool_use` is the
        // case worth spelling out: the Responses wire carries that fact in a
        // separate top-level property, so there is no honest place to put it.
        for (from, to, choice) in [
            (
                WireProtocol::AnthropicMessages,
                WireProtocol::OpenAiResponses,
                json!({ "type": "auto", "disable_parallel_tool_use": true }),
            ),
            (
                WireProtocol::OpenAiResponses,
                WireProtocol::AnthropicMessages,
                json!("anything_else"),
            ),
        ] {
            let error = declaring(None, Some(choice.clone()), from)
                .tools_for(to)
                .expect_err(&format!("{choice} must be refused"));
            assert!(
                matches!(&error, FrontierError::UntranslatableTools { tool, .. } if tool == "tool_choice"),
                "{error}"
            );
        }

        // CONTROL: the same call with a translatable payload succeeds, so the
        // assertions above are about these shapes and not about the seam
        // refusing every cross-dialect turn.
        assert!(
            declaring(
                Some(json!([anthropic_tool()])),
                Some(json!({ "type": "auto" })),
                WireProtocol::AnthropicMessages
            )
            .tools_for(WireProtocol::OpenAiResponses)
            .is_ok()
        );
    }

    /// A toolbox with no dialect stamped is a quote this process built wrong,
    /// and it is refused rather than guessed at.
    ///
    /// The guess that would be tempting — "assume the target's dialect" —
    /// is exactly the false premise F1 found, and taking it would restore the
    /// silent 400 while looking like a safe default. The no-tools control below
    /// is what keeps the stamp from becoming mandatory for the judge, the
    /// validate loop and every other internal caller that declares nothing.
    #[test]
    fn a_toolbox_with_no_dialect_stamped_is_refused_and_an_empty_one_is_not() {
        let unstamped = FrontierQuote {
            tools: Some(json!([anthropic_tool()])),
            ..quote_with(TurnCredential::Absent)
        };
        assert!(matches!(
            unstamped.tools_for(WireProtocol::AnthropicMessages),
            Err(FrontierError::MalformedQuote(_))
        ));

        // CONTROL: nothing declared, no stamp needed, and both dialects answer
        // the same nothing.
        let bare = quote_with(TurnCredential::Absent);
        for spoken in [
            WireProtocol::AnthropicMessages,
            WireProtocol::OpenAiResponses,
        ] {
            assert_eq!(bare.tools_for(spoken).unwrap(), (None, None));
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
