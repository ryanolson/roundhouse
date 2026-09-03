// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Roundhouse binary.
//!
//! Deliberately thin: everything interesting is a seam, and this only chooses
//! which implementation of each seam to instantiate. Configuration is one
//! environment variable, because a flag parser here would be the first place a
//! deployment concern leaked into the composition root.
//!
//! Durability is the one seam a deployment selects, and it selects every family
//! of it at once — but the selecting does not happen here. `shared_backend::open`
//! makes it, in the library, and this file wires whichever four backends it
//! hands back. That split is M14.1's review, F1: the choice used to be spelled
//! out three times in this function, inside a `[[bin]]` nothing else can call,
//! so the boot suites could only re-type it by hand and a mutation of the real
//! wiring went unnoticed.
//!
//! A URL
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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{FairUseLedger, SpendLedger};
use roundhouse_core::metrics::MetricsConfig;
use roundhouse_core::routing::{
    AffinityPolicy, CacheLedger, CacheModel, Candidate, ProviderPricing, RoutingPolicy,
    StagePolicy, Target,
};
use roundhouse_core::store::SessionStore;
use roundhouse_core::validate::{Validator, ValidatorConfig};
use roundhouse_fleet::{
    AnthropicMessagesClient, DEFAULT_API_BASE, DEFAULT_PASS_THROUGH_BASE, EchoFrontierClient,
    FrontierClient, FrontierClients, FrontierModelSpec, OpenAiResponsesClient,
    StaticFrontierCatalog, WireProtocol,
};
use roundhouse_mcp::ControlStore;
use roundhouse_server::catalog_config::{BUILT_IN_OPENAI, ProviderConfig};
use roundhouse_server::control_config::crosscheck::CrossChecks;
use roundhouse_server::{
    Backends, ControlDirectory, ControlPlane, ControlPlaneReads, Conversations, DirectoryError,
    EchoLocalExecutor, Engine, EngineConfig, FleetJudge, JudgeConfig, MemoryDirectoryStore,
    REDIS_NAMESPACE_VAR, REDIS_VAR, admin_api, catalog_config, control_config, http, mcp_api,
    messages_api, metrics_api, relay_api, resolve_namespace, responses_api, shared_backend,
};
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

/// Which catalog model the validate loop's judge runs on, as `provider/model`.
///
/// An environment variable rather than a control-plane field, and the split is
/// the same one the two files already draw: *whether* a project's traffic is
/// validated is a tenancy decision and lives with the tenants, while *which
/// model does the judging* is a fact about what this deployment can reach and
/// lives with the catalog. A deployment that moves its judge to a different
/// provider edits the same variable it edits to change providers at all.
///
/// Absent means no judge, which is a deployment that cannot validate — and a
/// config that enrolled a project anyway stops the boot. See
/// [`crosscheck`](roundhouse_server::control_config::crosscheck), which asks
/// that question at boot and again after every admin write.
const JUDGE_MODEL_VAR: &str = "ROUNDHOUSE_JUDGE_MODEL";

/// Whether this deployment dispatches to real providers at all.
///
/// Absent means the offline echo stub, which is what every test and every
/// pre-M7 deployment gets — a real client is opted into, never defaulted to,
/// because composing one changes where a turn's tokens actually go.
///
/// **The name is historical and the value is a switch, not a dialect.** When
/// M9 wrote it there was one client, so naming the transport and enabling it
/// were the same act, and this doc said a second transport would add a value
/// here. M10.1's provider registry superseded that: the dialect became a
/// per-catalog-entry fact (`FrontierModelSpec::wire_protocol`) and each
/// provider's client is built from it, so a per-dialect value here would be a
/// second, coarser answer to a question the catalog already answers per entry —
/// and the two could disagree. `openai_responses` therefore stays the one
/// accepted value: it means "dispatch for real", and which wire each provider
/// is dispatched over is read from the catalog. Renaming it would break every
/// deployment's environment for a spelling.
const FRONTIER_UPSTREAM_VAR: &str = "ROUNDHOUSE_FRONTIER_UPSTREAM";

/// Where a stored key authenticates, overriding the published endpoint.
///
/// Separate from the pass-through base below because the two auth modes address
/// genuinely different origins — stage 0's ruling, and the reason
/// `OpenAiResponsesClient` takes two. A deployment behind one egress proxy
/// points both at it.
const OPENAI_API_BASE_VAR: &str = "ROUNDHOUSE_OPENAI_API_BASE";

/// Where a forwarded ChatGPT device login authenticates, overriding the
/// endpoint the pass-through stanza implies.
const OPENAI_PASS_THROUGH_BASE_VAR: &str = "ROUNDHOUSE_OPENAI_PASS_THROUGH_BASE";

/// What a variable of this process's environment holds.
///
/// A closure rather than `std::env::var` called inside the constructor below,
/// and the reason is testability rather than taste: the registry's refusals are
/// the load-bearing half of "an unknown provider is refused at boot, not at
/// first dispatch", and a test that had to mutate the process environment to
/// reach them would race every other test in the binary. Passing the
/// environment in makes each refusal a pure function of two files and a map.
type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The environment as this process actually has it.
fn process_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string())
}

/// The transport each provider's turns are dispatched through, as this
/// deployment configured it.
///
/// Load-or-die on a *named* transport, the same posture the catalog and the
/// control plane take: a deployment that asked for a real upstream and got the
/// echo stub would report a full dashboard of turns that never left the
/// process. An unrecognised name is refused rather than falling back, for the
/// same reason.
///
/// **The M10.1 change is that this returns a registry rather than a client**,
/// and with it the third of the three boot cross-checks the phase adds. The
/// first two are the catalog boundary's, because they are pure functions of the
/// file: every entry's provider is defined or is the built-in `openai`, and a
/// defined provider declares a route for the dialect its entries speak. The
/// third can only be asked here, because it is a fact about the *binary* — this
/// build compiles two transports, `OpenAiResponsesClient` and
/// `AnthropicMessagesClient`, so a provider whose entries speak anything else
/// has nowhere to go, and a boundary that asked it would refuse a good catalog
/// on a build with fewer clients compiled in.
///
/// **M11.0 turned the dialect gate into an exhaustive `match`.** It used to be
/// `!=` against one constant, which meant the second client could have landed
/// with the gate still shut and nothing to say so. The `match` below has no
/// catch-all, so a third dialect is a compile error on the line that decides
/// which transport serves it.
///
/// Together the three make [`FrontierClients::for_provider`] total on a booted
/// process: the router cannot produce a provider name this registry does not
/// hold. That is the property the whole change is for — the alternative is a
/// tenant discovering the misconfiguration inside their own turn, after the
/// decision has already been written to their log.
fn frontier_clients(
    catalog: &StaticFrontierCatalog,
    providers: &HashMap<String, ProviderConfig>,
    env: Env<'_>,
) -> anyhow::Result<FrontierClients> {
    let Some(named) = env(FRONTIER_UPSTREAM_VAR) else {
        tracing::warn!(
            var = FRONTIER_UPSTREAM_VAR,
            "no frontier upstream configured; serving the offline echo stub, which \
             reaches no provider and bills nothing"
        );
        // Uniform rather than keyed, and it is the one place uniform is
        // *right*: the stub answers every provider identically because it
        // reaches none of them, so enumerating the catalog here would build a
        // map whose keys mean nothing.
        return Ok(FrontierClients::uniform(Arc::new(EchoFrontierClient::new(
            "frontier answer",
        ))));
    };
    if named != "openai_responses" {
        anyhow::bail!(
            "{FRONTIER_UPSTREAM_VAR} names `{named}`, which is not a value this build accepts; \
             the one accepted value is `openai_responses`, which means \"dispatch to real \
             providers\" rather than naming a wire -- each provider's dialect is read from its \
             catalog entries' `wire_protocol` -- and leaving the variable unset serves the \
             offline echo stub"
        );
    }

    // Which dialects each provider is actually asked to speak, so the refusal
    // below can name the entry an operator would go and move rather than the
    // provider in the abstract.
    let mut wanted: HashMap<&str, Vec<&FrontierModelSpec>> = HashMap::new();
    for spec in catalog.models() {
        wanted.entry(spec.provider.as_str()).or_default().push(spec);
    }

    let mut clients: HashMap<String, Arc<dyn FrontierClient>> = HashMap::new();
    for (provider, specs) in wanted {
        // **Definition first, dialect second, and the order is the message.**
        // An entry that is both undefined and in an unspeakable dialect has one
        // remedy — write the provider down — and being told about the dialect
        // instead sends an operator to change a `wire_protocol` that was never
        // the problem. Reachable from a catalog this process did not parse: the
        // built-in echo catalog is exactly one, and it prices every turn at
        // zero, so a deployment that named a real upstream and got that would
        // dispatch real traffic under fabricated free prices and then report
        // the savings.
        let definition = providers.get(provider);
        if definition.is_none() && provider != BUILT_IN_OPENAI {
            anyhow::bail!(
                "catalog entry `{provider}/{}` names a provider nothing defines, and \
                 {FRONTIER_UPSTREAM_VAR} names a real upstream. Add a `\"providers\"` entry \
                 for `{provider}` to the file {} names, or unset {FRONTIER_UPSTREAM_VAR} to \
                 serve the offline stub",
                specs[0].model,
                catalog_config::CATALOG_VAR,
            );
        }

        // **One transport per provider, so a provider's entries must agree on
        // one dialect.** The registry is keyed on the provider name alone,
        // because that is all a routing decision carries — so a provider whose
        // entries speak two wires has no single client it could be, and picking
        // either would leave the other half of its traffic refused by
        // `FrontierError::UnsupportedDialect` one turn at a time. The remedy is
        // two definitions pointing at the same origin, which the registry
        // already supports and which also keeps the two rate cards apart in the
        // metrics rollup — so this is a boot refusal naming both entries rather
        // than a coin flip. Unreachable before M11 because there was one client
        // and the check below refused everything else.
        let dialect = specs[0].wire_protocol;
        if let Some(other) = specs.iter().find(|spec| spec.wire_protocol != dialect) {
            anyhow::bail!(
                "provider `{provider}` has catalog entries in two dialects -- `{}/{}` speaks \
                 `{}` and `{provider}/{}` speaks `{}` -- and this registry holds one transport \
                 per provider, so one of them would have nowhere to be dispatched. Define the \
                 provider twice under two names pointing at the same base URL, one per dialect",
                provider,
                specs[0].model,
                dialect.wire_name(),
                other.model,
                other.wire_protocol.wire_name(),
            );
        }

        // Before the dialect is decided, because it is not a fact about one:
        // `auth.env` names a variable this process cannot see, and that is as
        // true of a Messages provider as of a Responses one. Spelled once here
        // rather than inside each arm so a third arm cannot be written without
        // it — which is exactly how the Anthropic arm nearly shipped silent.
        if let Some(definition) = definition {
            warn_if_no_key(provider, definition, env);
        }

        // **Exhaustive, and that is the whole point of the shape.** What this
        // replaced was `spec.wire_protocol != WireProtocol::OpenAiResponses` —
        // an `!=` against one constant, which M11's seam map listed first among
        // the places that assume a single wire precisely because the compiler
        // cannot point at it. A `match` with no catch-all makes the *next*
        // dialect a compile error here, on the line that would otherwise have
        // silently mis-dispatched it.
        let client: Arc<dyn FrontierClient> = match dialect {
            WireProtocol::OpenAiResponses => match definition {
                Some(definition) => Arc::new(responses_client(provider, definition, &specs, env)?),
                // The implicit `openai` provider: the endpoints
                // `ROUNDHOUSE_OPENAI_API_BASE` has always named. Every catalog
                // written before M10.1 lands here, which is the whole of the
                // backward-compatibility promise.
                None => Arc::new(implicit_openai_client(env)?),
            },
            WireProtocol::AnthropicMessages => match definition {
                Some(definition) => Arc::new(messages_client(provider, definition, &specs)?),
                // **There is no implicit `anthropic` provider, deliberately.**
                // The implicit one exists only for `openai`, and only because
                // catalogs predating the registry named it with no definition;
                // no catalog can predate a dialect that had no client. Inventing
                // one here would mean a typo'd `wire_protocol` on an `openai`
                // entry silently opened a connection to `api.anthropic.com`.
                None => anyhow::bail!(
                    "catalog entry `{provider}/{}` speaks `anthropic_messages`, and \
                     `{provider}` has no `\"providers\"` definition. Unlike `openai` there is \
                     no implicit Anthropic provider -- add a definition naming its `base_url` \
                     and a `\"messages\"` route to the file {} names",
                    specs[0].model,
                    catalog_config::CATALOG_VAR,
                ),
            },
            // No client speaks this wire. An explicit arm rather than a
            // catch-all so that whoever writes one is sent here by the compiler
            // instead of discovering that a `_ =>` had been quietly refusing it.
            WireProtocol::OpenAiChatCompletions => anyhow::bail!(
                "catalog entry `{provider}/{}` speaks `{}`, and this build has no client for \
                 that dialect -- it speaks `openai_responses` and `anthropic_messages`. \
                 Refused at boot rather than at the turn that would dispatch it: a routing \
                 decision naming this entry would fail one tenant's turn for a line in a file",
                specs[0].model,
                dialect.wire_name(),
            ),
        };
        clients.insert(provider.to_string(), client);
    }
    Ok(FrontierClients::keyed(clients))
}

/// A provider with no key anywhere, said out loud at boot.
///
/// A warning and not a refusal, because this file is not where keys live: a
/// member or a project may attach one through the control plane's tiers, and
/// this process cannot see that from here. Saying so at boot is what stops an
/// operator finding out one turn at a time.
///
/// Called from the loop above rather than from inside either dialect arm,
/// because the fact it reports — `auth.env` names a variable this process cannot
/// see — is a property of the definition and has nothing to do with which wire
/// the provider speaks.
fn warn_if_no_key(provider: &str, definition: &ProviderConfig, env: Env<'_>) {
    if env(&definition.auth.env).is_none() {
        tracing::warn!(
            %provider,
            var = %definition.auth.env,
            "this provider's catalog entry names an environment variable that is not set; \
             turns routed here will need a credential from the control plane's project or \
             member tier, or they will be refused before a socket is opened"
        );
    }
}

/// The OpenAI Responses transport for one defined provider.
fn responses_client(
    provider: &str,
    definition: &ProviderConfig,
    specs: &[&FrontierModelSpec],
    env: Env<'_>,
) -> anyhow::Result<OpenAiResponsesClient> {
    // Both bases are this provider's own origin. A pass-through credential is a
    // ChatGPT device login and the header allowlist is per provider, so a
    // forwarded seat cannot resolve for anything but `openai` anyway -- but
    // pointing the forwarding client at the same origin rather than at
    // chatgpt.com is what makes that a redundancy instead of a way for a seat to
    // reach an origin nobody configured it for.
    let route = definition
        .routes
        .for_dialect(WireProtocol::OpenAiResponses)
        .expect("the catalog boundary refuses an entry whose dialect has no route");
    tracing::info!(
        %provider,
        base_url = %definition.base_url,
        %route,
        entries = specs.len(),
        "dispatching this provider's turns over the OpenAI Responses wire"
    );
    // **The built-in `openai` provider, explicitly redefined.** This arm reads
    // `definition.base_url` for both bases and never reads the two variables,
    // and that precedence is the intended design — an operator who writes the
    // provider down has said where it is, and the comment above says why both
    // bases come from the one origin. What was missing is that the line above
    // reads identically whether or not those variables are set, so a deployment
    // behind an egress proxy that added an `openai` definition to attach
    // `extra_headers` had the proxy silently leave the path (M10 review G15). A
    // warning rather than a refusal: the configuration is legitimate, and
    // refusing it would make attaching a header impossible for anyone who had
    // ever set the variable. Named per variable rather than in one line, because
    // the two address different origins — stored key and forwarded seat — and a
    // deployment may have set only one.
    if provider == BUILT_IN_OPENAI {
        for var in [OPENAI_API_BASE_VAR, OPENAI_PASS_THROUGH_BASE_VAR] {
            if let Some(shadowed) = env(var) {
                tracing::warn!(
                    %provider,
                    shadowed_var = var,
                    shadowed_value = %shadowed,
                    definition_base_url = %definition.base_url,
                    "an explicit `openai` provider definition takes precedence over this \
                     variable, which is not read while the definition stands; every turn of \
                     this provider's is dispatched at the definition's base URL, not at the \
                     one the variable names"
                );
            }
        }
    }
    Ok(
        OpenAiResponsesClient::with_bases(&definition.base_url, &definition.base_url)?
            .with_responses_path(route)
            .with_extra_headers(
                definition
                    .extra_headers
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone())),
            )?,
    )
}

/// The Anthropic Messages transport for one defined provider.
///
/// Built exactly the way a registry `openai_responses` provider is — one origin
/// for both bases, the definition's own route, its static headers — and the
/// sameness is deliberate. The one structural difference between the two
/// clients — Anthropic's pass-through base is the same origin as its stored-key
/// base, where a ChatGPT device login addresses a separate host — is a fact
/// about the provider and belongs in the client's own constants, not in a second
/// shape here.
///
/// **One thing this arm reads that the Responses one does not: `auth.style`.**
/// This dialect has two GA providers that authenticate a stored key in
/// different headers, so the spelling is per-provider configuration resolved
/// here, at boot, once — never sniffed from a hostname inside `route()`, where
/// it would be a routing decision resting on a substring and would
/// mis-authenticate every gateway fronting either provider under a third name.
fn messages_client(
    provider: &str,
    definition: &ProviderConfig,
    specs: &[&FrontierModelSpec],
) -> anyhow::Result<AnthropicMessagesClient> {
    let route = definition
        .routes
        .for_dialect(WireProtocol::AnthropicMessages)
        .expect("the catalog boundary refuses an entry whose dialect has no route");
    let auth_style = definition
        .auth
        .stored_auth_style()
        .expect("the catalog boundary refuses a definition naming a style nothing sends");
    tracing::info!(
        %provider,
        base_url = %definition.base_url,
        %route,
        // Logged because it is the difference between a provider that
        // authenticates and one that answers 401 forever, and an operator
        // debugging the second needs to see which spelling this process chose.
        // The header *name* is configuration; the key is not, and is not here.
        auth_style = auth_style.wire_name(),
        entries = specs.len(),
        "dispatching this provider's turns over the Anthropic Messages wire"
    );
    Ok(
        AnthropicMessagesClient::with_bases(&definition.base_url, &definition.base_url)?
            .with_messages_path(route)
            .with_stored_auth_style(auth_style)
            .with_extra_headers(
                definition
                    .extra_headers
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone())),
            )?,
    )
}

/// The implicit `openai` provider, at the endpoints the two variables name.
fn implicit_openai_client(env: Env<'_>) -> anyhow::Result<OpenAiResponsesClient> {
    let api_base = env(OPENAI_API_BASE_VAR).unwrap_or_else(|| DEFAULT_API_BASE.to_string());
    let pass_through_base =
        env(OPENAI_PASS_THROUGH_BASE_VAR).unwrap_or_else(|| DEFAULT_PASS_THROUGH_BASE.to_string());
    // The bases are logged and the credentials are not, which is the whole of
    // what an operator needs to see here: which origin this process will talk
    // to. A URL is configuration; a key is not.
    tracing::info!(
        %api_base,
        %pass_through_base,
        "dispatching the built-in `openai` provider's turns over the OpenAI Responses wire"
    );
    Ok(OpenAiResponsesClient::with_bases(
        api_base,
        pass_through_base,
    )?)
}

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

/// The catalog entry the judge runs on, if this deployment named a reachable
/// one.
///
/// Resolved against the catalog rather than trusted as written, for the reason
/// the two cross-checks below exist: a variable naming a model this process
/// cannot reach is a mistake nothing before this point could catch, and its
/// symptom — every validation abandoned — is invisible from a client's side.
fn judge_spec(catalog: &StaticFrontierCatalog) -> Option<FrontierModelSpec> {
    let named = std::env::var(JUDGE_MODEL_VAR).ok()?;
    let (provider, model) = named.split_once('/')?;
    catalog
        .spec_for(&Target::Frontier {
            provider: provider.to_string(),
            model: model.to_string(),
        })
        .cloned()
}

/// A directory that will not compile, as the sentence a boot has always
/// printed.
///
/// [`DirectoryError`] wraps a cross-check refusal in "this change would not
/// start this deployment (...)", which is the right sentence for a refused
/// `PATCH` and the wrong one here: the process is not applying a change, it is
/// declining to start, and an operator greps the log for the check's own words.
/// Unwrapped so the boot door and the API door print the same refusal they each
/// always have, rather than one of them printing the other's frame around it.
fn boot_refusal(error: DirectoryError) -> anyhow::Error {
    match error {
        DirectoryError::CrossCheckRefused { detail, .. } => anyhow::anyhow!("{detail}"),
        DirectoryError::Invalid(source) | DirectoryError::EnvironmentIncomplete(source) => {
            anyhow::anyhow!(source)
        }
        other => anyhow::anyhow!(other),
    }
}

/// Does any project on this plane route between tiers? (M10.2, S3)
///
/// The whole of the composition root's tier decision, named so it can be
/// asserted: `serve` reads it once and wraps
/// [`StagePolicy`](roundhouse_core::routing::StagePolicy) around the ordinary
/// policy when it is true. Read through `configured_admissions` — the same
/// accessor the fair-use boot flag uses, and for the same reason: the key
/// table's layout has exactly one reader outside its own module and this is not
/// going to be the second.
///
/// **Conditional composition, and the condition is not a micro-optimization.**
/// `StagePolicy` delegates to its inner policy for every project with no recipe,
/// and the target and rationale it produces there are pinned byte-identical to
/// the inner policy's — but [`DecisionRecord::policy`] reports `stage`, because
/// that is the object in force, and reporting `affinity` would make the audit
/// trail name a router that did not serve the turn. Composing it unconditionally
/// would therefore relabel every existing deployment's decisions on an upgrade
/// that changed no routing at all. Composing it only where a recipe exists moves
/// the field exactly when the router moved.
///
/// **The hole this leaves is a recipe added through the admin plane after boot**,
/// which nothing here can see, and which would otherwise be the worst shape a
/// config mistake can take: an operator's recipe re-routing nothing, with every
/// surface reporting the configuration as fine. The engine warns once when a
/// turn arrives carrying a recipe its policy cannot read — see
/// `Engine::unread_recipe` — which states the same fact at the one moment it is
/// knowable. `ControlPlane::Open` has no file to write a recipe in and answers
/// `false` by construction.
///
/// [`DecisionRecord::policy`]: roundhouse_core::routing::DecisionRecord::policy
fn composes_the_stage_router(plane: &ControlPlane) -> bool {
    plane
        .configured_admissions()
        .any(|admission| admission.tiers.is_some())
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

    // The registry, built here rather than inside `serve` because it is the
    // third boot cross-check and boot checks belong together: an operator
    // reading this log sees the catalog load, the providers resolve, and the
    // control plane compile in the order a mistake in each would be found.
    // See `frontier_clients` for what the three checks divide between them.
    let providers = config
        .as_ref()
        .map(|config| config.providers.clone())
        .unwrap_or_default();
    let frontier = Arc::new(frontier_clients(&catalog, &providers, &process_env)?);

    // Same posture as the catalog, and for a sharper reason: a control plane
    // that was named but cannot be read stops the process, because starting
    // anyway would serve every request as if no key were required — the exact
    // failure the variable is set to prevent — and would do it silently, with
    // every tenant's turns landing in one unnamespaced session space.
    //
    // The file's half is read here and compiled below, once the catalog is
    // known: the admin plane's directory is what every surface resolves
    // against, and it cannot be built until the cross-checks it re-runs after
    // every write have something to check against.
    let file = control_config::config_from_env()?;

    // Both files are loaded now, and only now can they be compared. See the
    // functions: neither loader can see the other, so a policy naming no model
    // this deployment has — or promising a local fallback it does not have —
    // is a mistake nothing before this point could catch.
    let reachable = reachable_candidates(&catalog);
    // Resolved before the cross-checks because one of them asks about it: a
    // project enrolled in the validate loop on a deployment with no judge is a
    // configuration that would load, serve, and quietly validate nothing.
    let judge = judge_spec(&catalog);
    match &judge {
        Some(spec) => tracing::info!(
            provider = %spec.provider,
            model = %spec.model,
            "validate/steer loop: judge resolved; enrolled projects will be checked"
        ),
        None => tracing::info!(
            var = JUDGE_MODEL_VAR,
            "validate/steer loop: no judge configured, so nothing is validated"
        ),
    }
    // Every cross-check, through the one list that also runs after every admin
    // write — including the third, which this deployment's *control surface*
    // needs: that surface answers entitlement questions by principal, and the
    // config lets two keys name one membership with different overrides. A
    // deployment where they disagree would tell an agent about a policy its own
    // key does not have. See `ControlPlane::membership`.
    //
    // The refusal is printed as the check wrote it. Prefixing it with the
    // check's name here would give an operator two spellings of one failure --
    // one from the boot log and one from a 422 -- to recognise as the same
    // thing.
    let checks = CrossChecks::new(reachable.clone(), judge.clone());

    // The one thing every surface authenticates against, and the one thing the
    // admin plane writes to. Built here rather than beside the catalog because
    // constructing it *is* the boot check: it compiles the file, runs
    // `checks.refuse` on the result, and refuses to exist if either says no —
    // the same two judgements every later admin write goes through.
    //
    // `MemoryDirectoryStore` is this milestone's only backing store, so
    // admin-created tenancy dies with the process; the unlock condition for a
    // durable one is written at `ControlDirectory`.
    //
    // Captured before `file` moves into the match below: it is what decides,
    // once the Redis branch is chosen further down, whether this deployment's
    // durability is actually one thing or secretly two — see the warning
    // there. A `None` file means [`ControlDirectory::open`] below, which has
    // no admin plane at all, so nothing about it can be mismatched with
    // anything.
    //
    // Named for what it reads (a file was configured), not for the store
    // that follows from it, because those are two facts today only because
    // `MemoryDirectoryStore` is this branch's *only* store. The day a
    // durable `DirectoryStore` lands and the `Some` arm below picks between
    // stores, this flag has to move with it — to whichever branch is still
    // memory-backed — or the warning below keeps firing after the gap it
    // describes is closed.
    let control_plane_file_configured = file.is_some();
    let directory = match file {
        Some((file, path)) => Arc::new(
            ControlDirectory::new(
                file,
                path,
                Arc::new(MemoryDirectoryStore::new()),
                checks,
                roundhouse_core::now_ms(),
            )
            .map_err(boot_refusal)?,
        ),
        None => ControlDirectory::open(),
    };
    match &*directory.plane(roundhouse_core::now_ms()) {
        // Counted through the accessor rather than by reaching into
        // `Configured { turn_keys, .. }`: the table's layout has exactly one
        // reader outside its own module, and this is not going to be the
        // second one for the sake of a log line.
        plane @ ControlPlane::Configured { .. } => tracing::info!(
            memberships = plane.configured_admissions().count(),
            var = control_config::CONTROL_PLANE_VAR,
            "control plane loaded; a key is required on every surface"
        ),
        ControlPlane::Open => tracing::warn!(
            var = control_config::CONTROL_PLANE_VAR,
            "no control plane configured; every request is served as the built-in \
             default/default membership, with no key and no session namespace, and the \
             admin plane is refused for want of a root of trust"
        ),
    }

    // Whether anything in the plane this deployment *booted* with is standing
    // in front of a fair-use ceiling. An operator-facing fact about the file,
    // and nothing branches on it: a ceiling the admin plane adds an hour from
    // now is equally real and equally enforced, and announces itself at the
    // seam that enforces it rather than here, where this read has long since
    // gone stale (M13 thermo-nuclear review, F1).
    let fair_use_configured = directory
        .plane(roundhouse_core::now_ms())
        .configured_admissions()
        .any(|admission| !admission.fair_use.is_empty());
    if fair_use_configured {
        tracing::info!("fair-use windows are configured; rolling ceilings are enforced");
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

    // **One call, one match** (M14.1 review, F1). Which four backends this
    // deployment gets is `shared_backend::open`'s answer, taken in the library
    // where the boot suites can call it and where a mutation of the wiring is
    // therefore a mutation of something a test runs. This site does nothing
    // but wire what it hands back: re-deriving any part of the choice here is
    // exactly what put it beyond every test's reach, three spellings deep.
    //
    // One variable selects every family, and they are chosen together on
    // purpose. The session log and the spend ledger answer two questions about
    // the same turns, and a deployment that made one durable and left the other
    // in memory would re-grant its whole budget on every restart while the log
    // that proves it was already spent survives.
    //
    // The two arms monomorphize `serve` twice; that is the entire cost of
    // keeping the engine generic over its store.
    // Read and validated before anything connects: an empty
    // ROUNDHOUSE_REDIS_NAMESPACE is a boot error, not a per-process quirk
    // that surfaces as two deployments silently sharing a keyspace (R-S3).
    let namespace = resolve_namespace(std::env::var(REDIS_NAMESPACE_VAR).ok().as_deref())
        .with_context(|| format!("{REDIS_NAMESPACE_VAR} must not be empty"))?;
    let backends =
        shared_backend::open(std::env::var(REDIS_VAR).ok().as_deref(), &namespace).await?;

    match backends {
        Backends::Shared {
            store,
            spend,
            fair_use,
            conversations,
            ..
        } => {
            if control_plane_file_configured {
                tracing::warn!(
                    var = control_config::CONTROL_PLANE_VAR,
                    "sessions and committed spend just became durable in Redis, but \
                     admin-created tenancy -- every project, user and turn key an \
                     operator creates or archives through the admin plane -- still \
                     lives only in memory and does not survive this process's \
                     restart. Concretely: an archived project's tombstone is what \
                     keeps its id retired (see ProjectRecord::archived_at_ms); lose \
                     it on restart and the ordinary admin API will let that id be \
                     recreated as if it were new, silently joining the new tenant \
                     to the old one's spend history in the ledger that DID survive. \
                     The fix is a durable DirectoryStore, not yet built -- see \
                     ControlDirectory's own deferral note for the unlock condition"
                );
            }
            serve(
                store,
                spend,
                fair_use,
                conversations,
                Arc::clone(&directory),
                catalog,
                frontier,
                judge,
                reachable,
                metrics_config,
                listener,
            )
            .await
        }
        Backends::PerProcess {
            store,
            spend,
            fair_use,
            conversations,
        } => {
            serve(
                store,
                spend,
                fair_use,
                conversations,
                Arc::clone(&directory),
                catalog,
                frontier,
                judge,
                reachable,
                metrics_config,
                listener,
            )
            .await
        }
    }
}

/// Compose the engine and the five surfaces over whichever backends were
/// chosen.
///
/// **The one composition site**, and two of its values are shared rather than
/// minted per router on purpose. [`Conversations`] is the deployment's answer
/// to "which session is the conversation the client calls `main`?", and the
/// Responses surface and the control surface both ask it — two of them would
/// agree only until a client edited its own history. It arrives as an argument
/// rather than being built here because *which maps are behind it* is
/// `shared_backend::open`'s one decision (M14.1, R-C4), taken by the same
/// predicate — in the same match — that chose the store, the spend ledger and
/// the fair-use buckets. [`ControlStore`] is
/// the node's control-plane state, and the engine and the control surface hold
/// opposite ends of it: the surface writes an agent's overlay and the engine
/// spends it at the start of the next turn.
///
/// **The steer used to be the second half of that sentence and is not any
/// more.** Until M10.0 the engine deposited a correction's payload here and the
/// surface served it to `fetch_steer`; the correction is a conversation item now
/// (`PLAN-frontier-selection.md` R1), so it lives in the session log with
/// everything else and this store holds only overlays, intents, bindings and the
/// advisory outcome an agent reports.
#[allow(clippy::too_many_arguments)]
async fn serve<S: SessionStore>(
    store: Arc<S>,
    spend: Arc<dyn SpendLedger>,
    fair_use: Arc<dyn FairUseLedger>,
    conversations: Arc<Conversations>,
    directory: Arc<ControlDirectory>,
    catalog: StaticFrontierCatalog,
    frontier: Arc<FrontierClients>,
    judge: Option<FrontierModelSpec>,
    reachable: Vec<Candidate>,
    metrics_config: Arc<MetricsConfig>,
    listener: tokio::net::TcpListener,
) -> anyhow::Result<()> {
    let control = Arc::new(ControlStore::new());
    // The judge's own transport, resolved from its own catalog entry's
    // provider rather than from whatever client the engine happens to hold.
    //
    // **This is the seam the registry could break silently.** Until M10.1 there
    // was one client and the judge shared it, which was correct because there
    // was nothing else it could be. With a registry, a judge handed "the
    // frontier client" would dispatch through whichever provider was first in
    // a map — wrong base URL, wrong auth, wrong extra headers — and the
    // symptom would be a validate loop that fails or, worse, one that quietly
    // bills a different provider. Resolved by name, and a name the registry
    // does not hold stops the process here beside the other boot checks.
    //
    // Unreachable through configuration on a booted deployment: `judge_spec`
    // resolves the variable *against the catalog*, and every catalog provider
    // has a client by the three cross-checks. It is spelled as a refusal
    // anyway, because "unreachable by an argument someone has to re-derive" is
    // exactly the shape that stops being true when a fourth way to build a
    // registry arrives.
    let judge_client = match &judge {
        Some(spec) => Some(
            frontier
                .for_provider(&spec.provider)
                .map(Arc::clone)
                .with_context(|| {
                    format!(
                        "{JUDGE_MODEL_VAR} names `{}/{}`, and this process has no transport for \
                         provider `{}`; the validate loop would dispatch its judge through some \
                         other provider's client",
                        spec.provider, spec.model, spec.provider
                    )
                })?,
        ),
        None => None,
    };
    let engine_config = EngineConfig {
        // The salt reaches the engine and nowhere else: it is an input to the
        // stamp written at session creation, and every later reader — the
        // occupant, the fold, a replay — reads the *arm*, not the salt.
        //
        // Read once here rather than per turn, unlike everything else resolved
        // through the directory: the salt comes from the file and no admin
        // write can move it, and re-reading it per turn would suggest a
        // deployment could re-randomize a study already in flight.
        arm_salt: directory
            .plane(roundhouse_core::now_ms())
            .arm_salt()
            .to_string(),
        ..EngineConfig::default()
    };

    let tiers_configured = composes_the_stage_router(&directory.plane(roundhouse_core::now_ms()));
    if tiers_configured {
        tracing::info!(
            "a project configures a tier recipe; the stage router is composed over the \
             ordinary policy, and projects with no recipe route through it unchanged"
        );
    }

    let mut engine = Engine::with_provider_clients(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        catalog,
        Arc::clone(&frontier),
        // The recipe reader wrapped around the ordinary policy, or the ordinary
        // policy alone. See `tiers_configured` for why the wrapper is not
        // composed unconditionally.
        match tiers_configured {
            true => Arc::new(StagePolicy::new(Box::new(AffinityPolicy::new())))
                as Arc<dyn RoutingPolicy>,
            false => Arc::new(AffinityPolicy::new()) as Arc<dyn RoutingPolicy>,
        },
        engine_config.clone(),
    )
    .with_spend_ledger(Arc::clone(&spend))
    // Chosen by `fair_use_backend`, not here: this site takes whichever ledger
    // the composition root resolved, so the boot log and the enforcement are
    // the same decision.
    .with_fair_use_ledger(fair_use)
    .with_control_store(Arc::clone(&control));

    // The validator is installed only where there is a judge to install it
    // around, and the boot check above has already refused the configuration
    // where that absence would be a broken promise. A deployment with a judge
    // and no enrolled project installs it too and it decides nothing: no
    // session is stamped, so the occupant's first question answers "not
    // enrolled" and no turn pays for a trigger.
    if let Some((spec, client)) = judge.zip(judge_client) {
        let fleet_judge = FleetJudge::new(
            client,
            spec,
            ByteTokenizer,
            engine_config.turn_deadline_ms,
            JudgeConfig::default(),
        )
        .with_spend_ledger(Arc::clone(&spend));
        engine = engine.with_interjector(Arc::new(Validator::new(
            Arc::new(fleet_judge),
            // The trigger, the brief and the action defaults; the per-project
            // half — channel, arms, placebo rate — travels on the admission
            // instead, because it is a tenancy decision and this is not the
            // file tenancy is written in.
            ValidatorConfig {
                arm_salt: engine_config.arm_salt.clone(),
                ..ValidatorConfig::default()
            },
        )));
    }
    let engine = Arc::new(engine);

    // Seven surfaces, one process and one log: the native transport, which
    // exposes sessions and the log itself; the Responses API, which lets an
    // agent written against OpenAI drive the same sessions unmodified; the
    // Messages API, which does the same for Claude Code, whose native dialect
    // that is; the Relay-format reads, which project the log into somebody
    // else's document shapes; the metrics surface, which reports on all of them
    // by folding the same log; the MCP control surface, which is the only one an
    // agent rather than a client drives — it reads what the others did and lets
    // the model ask to be routed to less than its key allows; and the admin
    // plane, which is the only one that *writes* tenancy.
    //
    // One control directory behind all seven, not one each: a key that pays for
    // a turn on one surface and is unknown to another would be a deployment with
    // two answers to the same question. The six read-only surfaces take it as a
    // `PlaneSource` and re-resolve per request — which is what makes a key
    // revoked on the admin plane stop working on all of them, rather than only
    // on whichever ones happened to be built after the revocation.
    let app = http::router(
        Arc::clone(&directory),
        Arc::clone(&engine),
        Arc::clone(&store),
    )
    .merge(metrics_api::metrics_router(
        Arc::clone(&directory),
        engine.metrics(),
        Arc::clone(&metrics_config),
    ))
    .merge(admin_api::admin_router(
        Arc::clone(&directory),
        Arc::clone(&spend),
        engine.metrics(),
        Arc::clone(&metrics_config),
    ))
    // The Relay-format reads. They take the rate card for the reason the
    // metrics surface does -- pricing is a reporting concern -- and the store
    // rather than the engine, because every document is a projection of the
    // log and nothing else.
    .merge(relay_api::relay_router(
        Arc::clone(&directory),
        Arc::clone(&store),
        metrics_config,
    ))
    .merge(mcp_api::mcp_router(
        Arc::clone(&directory),
        Arc::new(ControlPlaneReads::new(
            Arc::clone(&directory),
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
        Arc::clone(&directory),
        Arc::clone(&engine),
        Arc::clone(&store),
        Arc::clone(&conversations),
    ))
    // The Messages API, which lets Claude Code drive the same sessions
    // unmodified. Four arguments identical to the Responses surface's, and
    // that sameness is the point: it is the same log under a second
    // vocabulary, so the same directory revokes a key on it, the same engine
    // runs its turns, and the same `Conversations` table answers "which
    // session is this conversation" — an agent that narrowed its routing over
    // MCP and then sent a turn on this surface has to reach the session the
    // narrowing was installed against.
    .merge(messages_api::messages_router(
        directory,
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
    // The two checks by name, because these tests are about *what each one
    // refuses*: `CrossChecks::refuse` answers only "some check said no", and a
    // suite written against it could not tell a cadence refusal from a budget
    // one.
    use roundhouse_server::ControlPlaneConfig;
    use roundhouse_server::control_config::crosscheck::{
        refuse_policies_that_admit_nothing, refuse_promises_of_a_local_fallback,
    };

    fn plane_with_policy(policy: serde_json::Value) -> ControlPlane {
        plane_with(policy, serde_json::Value::Null)
    }

    // -----------------------------------------------------------------------
    // The client registry (M10.1, P2)
    // -----------------------------------------------------------------------

    /// One catalog entry, parameterized on the two fields the registry reads.
    fn entry(provider: &str, model: &str, wire_protocol: WireProtocol) -> FrontierModelSpec {
        FrontierModelSpec {
            provider: provider.into(),
            model: model.into(),
            wire_protocol,
            cache_model: CacheModel::Deterministic { ttl_ms: 300_000 },
            pricing: ProviderPricing::free(),
            quality_prior: 0.5,
            base_ttft_ms: 1.0,
            ttft_ms_per_uncached_token: 0.0,
        }
    }

    fn responses_provider(base_url: &str) -> ProviderConfig {
        serde_json::from_value(serde_json::json!({
            "base_url": base_url,
            "routes": { "responses": "/responses" },
            "auth": { "env": "A_PROVIDER_KEY" },
        }))
        .expect("the fixture definition must parse")
    }

    /// An environment naming a real upstream and nothing else — the shape
    /// every assertion below is about, since an unset upstream short-circuits
    /// to the echo stub before any check runs.
    fn real_upstream(name: &str) -> Option<String> {
        match name {
            FRONTIER_UPSTREAM_VAR => Some("openai_responses".to_string()),
            _ => None,
        }
    }

    /// **P2's boot check, the registry half.**
    ///
    /// The catalog boundary refuses an entry naming an undefined provider when
    /// the catalog came from a *file*. This is the other door: the built-in echo
    /// catalog is not parsed, so nothing has cross-checked it, and a deployment
    /// that named a real upstream while configuring no catalog would dispatch
    /// real traffic under the stub's fabricated free prices — then report the
    /// savings.
    #[test]
    fn an_unknown_provider_is_refused_at_boot_not_at_first_dispatch() {
        let error = frontier_clients(&echo_catalog(), &HashMap::new(), &real_upstream)
            .expect_err("a provider with no definition has no transport");
        let message = error.to_string();
        assert!(
            message.contains("echo/echo") && message.contains(catalog_config::CATALOG_VAR),
            "the refusal must name the entry and the file that would define it: {message}"
        );

        // CONTROL 1: the identical catalog with the identical environment,
        // once the provider is defined. One map entry different, and it boots
        // — which is what makes the refusal about the missing definition
        // rather than about the echo catalog or about `openai_responses`.
        let defined = HashMap::from([(
            "echo".to_string(),
            responses_provider("https://echo.test/v1"),
        )]);
        let mut echo_over_responses = echo_catalog().models().to_vec();
        echo_over_responses[0].wire_protocol = WireProtocol::OpenAiResponses;
        frontier_clients(
            &StaticFrontierCatalog::new(echo_over_responses),
            &defined,
            &real_upstream,
        )
        .expect("a defined provider has a transport");

        // CONTROL 2: the same undefined provider with *no* upstream named.
        // That is the offline stub, which reaches nothing and bills nothing, so
        // there is no wrong origin for a turn to land on and nothing to refuse.
        frontier_clients(&echo_catalog(), &HashMap::new(), &|_| None)
            .expect("the offline stub answers for every provider");
    }

    fn messages_provider(base_url: &str) -> ProviderConfig {
        serde_json::from_value(serde_json::json!({
            "base_url": base_url,
            "routes": { "messages": "/messages" },
            "auth": { "env": "A_PROVIDER_KEY" },
        }))
        .expect("the fixture definition must parse")
    }

    /// The third cross-check, and the one only this file can make: whether
    /// *this build* has a transport that speaks the entry's dialect.
    ///
    /// Deliberately not asked at the config boundary. A catalog naming a dialect
    /// this binary has no client for is a perfectly good catalog — it is this
    /// binary that is short a transport — and a boundary that refused it would
    /// have to be edited every time a client was added, on the wrong side of the
    /// crate graph.
    ///
    /// **The fixture moved with M11.0.** It used to be `anthropic_messages`,
    /// which this build now speaks; the unspeakable dialect is
    /// `openai_chat_completions`, which is the last `WireProtocol` arm with no
    /// client. When one is written this test has nothing left to assert with and
    /// should be deleted rather than kept alive on a fabricated variant — the
    /// exhaustive `match` it guards is by then guarded by the compiler.
    #[test]
    fn a_dialect_this_build_cannot_speak_stops_the_boot_and_names_the_entry() {
        let catalog = StaticFrontierCatalog::new(vec![entry(
            "dynamo-fleet",
            "llama",
            WireProtocol::OpenAiChatCompletions,
        )]);
        let providers = HashMap::from([(
            "dynamo-fleet".to_string(),
            serde_json::from_value::<ProviderConfig>(serde_json::json!({
                "base_url": "https://dynamo.test",
                "routes": { "chat_completions": "/v1/chat/completions" },
                "auth": { "env": "A_PROVIDER_KEY" },
            }))
            .unwrap(),
        )]);

        let error = frontier_clients(&catalog, &providers, &real_upstream)
            .expect_err("this build has no Chat Completions client");
        let message = error.to_string();
        assert!(
            message.contains("dynamo-fleet/llama") && message.contains("openai_chat_completions"),
            "the refusal must name the entry and the dialect, because the remedy is to move \
             one of them: {message}"
        );

        // CONTROL: the identical provider, identical environment, one entry
        // whose dialect this build does speak.
        let providers = HashMap::from([(
            "dynamo-fleet".to_string(),
            responses_provider("https://dynamo.test/v1"),
        )]);
        frontier_clients(
            &StaticFrontierCatalog::new(vec![entry(
                "dynamo-fleet",
                "llama",
                WireProtocol::OpenAiResponses,
            )]),
            &providers,
            &real_upstream,
        )
        .expect("a dialect this build speaks is routable");
    }

    /// **M11.0's gate, from the other side: an `anthropic_messages` entry now
    /// boots, and it boots into its own transport.**
    ///
    /// The twin of the test above, and neither is enough alone. That one says a
    /// dialect with no client is refused; this one says a dialect *with* one is
    /// not merely tolerated but resolved to a different client than the
    /// Responses provider beside it. A registry that had opened the gate and
    /// then built an `OpenAiResponsesClient` for both would pass every
    /// pre-M11 test in this file and POST a Responses body to `/messages`.
    #[tokio::test]
    async fn an_anthropic_entry_boots_into_its_own_transport_beside_a_responses_one() {
        let catalog = StaticFrontierCatalog::new(vec![
            entry("anthropic", "claude-x", WireProtocol::AnthropicMessages),
            entry("openrouter", "kimi", WireProtocol::OpenAiResponses),
        ]);
        let providers = HashMap::from([
            (
                "anthropic".to_string(),
                messages_provider("https://api.anthropic.test/v1"),
            ),
            (
                "openrouter".to_string(),
                responses_provider("https://openrouter.test/api/v1"),
            ),
        ]);

        let registry = frontier_clients(&catalog, &providers, &real_upstream)
            .expect("both dialects have a client in this build");
        let anthropic = registry.for_provider("anthropic").unwrap();
        let openrouter = registry.for_provider("openrouter").unwrap();
        assert!(!Arc::ptr_eq(anthropic, openrouter));

        // And each really speaks its own wire. `Arc::ptr_eq` alone would pass on
        // a registry that built two `OpenAiResponsesClient`s, which is exactly
        // the mistake an opened gate makes — so the claim is asserted through
        // the one seam that reveals a client's dialect: it refuses a quote in
        // any other, before a socket is opened.
        for (provider, spoken, refused) in [
            (
                "anthropic",
                WireProtocol::AnthropicMessages,
                WireProtocol::OpenAiResponses,
            ),
            (
                "openrouter",
                WireProtocol::OpenAiResponses,
                WireProtocol::AnthropicMessages,
            ),
        ] {
            let client = registry.for_provider(provider).unwrap();
            let quote = |wire_protocol| roundhouse_fleet::FrontierQuote {
                target: Target::Frontier {
                    provider: provider.into(),
                    model: "m".into(),
                },
                wire_protocol,
                prompt: "hi".into(),
                segment_boundaries: Vec::new(),
                prompt_cache_key: "sess".into(),
                expected_output_tokens: Some(16),
                // No client in this test, so nothing declared a ceiling and
                // nothing declared tools.
                output_token_cap: None,
                tools: None,
                tool_choice: None,
                tools_dialect: None,
                credential: roundhouse_core::control::TurnCredential::Absent,
            };
            let Err(error) = client.execute(&quote(refused)).await else {
                panic!("a client must refuse a dialect it cannot serialize")
            };
            assert!(
                matches!(
                    &error,
                    roundhouse_fleet::FrontierError::UnsupportedDialect { expected, .. }
                        if *expected == spoken.wire_name()
                ),
                "provider `{provider}` was built with a transport speaking the wrong wire: \
                 {error}"
            );
            // CONTROL: the same client on the dialect it does speak gets past
            // the dialect check -- it fails on the absent credential instead,
            // which is a later refusal and proves the assertion above is about
            // the wire rather than about every quote being rejected.
            let Err(error) = client.execute(&quote(spoken)).await else {
                panic!("`TurnCredential::Absent` is refused before a socket")
            };
            assert!(
                matches!(error, roundhouse_fleet::FrontierError::Credential(_)),
                "{error}"
            );
        }
    }

    /// **One transport per provider, so its entries may not disagree about the
    /// wire.**
    ///
    /// Unreachable before M11.0 — with one client every other dialect was
    /// refused outright, so no catalog could reach the ambiguity — and it is
    /// reachable now: OpenRouter genuinely serves both `/responses` and
    /// `/messages`, so a catalog naming both under one provider is a mistake an
    /// operator will actually make. The registry keys on the provider name, so
    /// one of the two entries would be dispatched through a serializer that
    /// refuses it, one tenant's turn at a time.
    #[test]
    fn a_provider_whose_entries_disagree_about_the_dialect_stops_the_boot() {
        let both = StaticFrontierCatalog::new(vec![
            entry("openrouter", "kimi", WireProtocol::OpenAiResponses),
            entry("openrouter", "claude-x", WireProtocol::AnthropicMessages),
        ]);
        let providers = HashMap::from([(
            "openrouter".to_string(),
            serde_json::from_value::<ProviderConfig>(serde_json::json!({
                "base_url": "https://openrouter.test/api/v1",
                "routes": { "responses": "/responses", "messages": "/messages" },
                "auth": { "env": "A_PROVIDER_KEY" },
            }))
            .unwrap(),
        )]);

        let error = frontier_clients(&both, &providers, &real_upstream)
            .expect_err("one provider cannot hold two transports in this registry");
        let message = error.to_string();
        assert!(
            message.contains("kimi") && message.contains("claude-x"),
            "the refusal must name both entries, because the remedy is to split one of them \
             out under its own provider name: {message}"
        );

        // CONTROL: the same two models, the same two dialects, split across two
        // provider definitions pointing at the same origin. One map entry
        // different and it boots -- which is what makes the refusal about the
        // *provider* holding two dialects rather than about the catalog holding
        // them.
        let split = StaticFrontierCatalog::new(vec![
            entry("openrouter", "kimi", WireProtocol::OpenAiResponses),
            entry(
                "openrouter-messages",
                "claude-x",
                WireProtocol::AnthropicMessages,
            ),
        ]);
        let providers = HashMap::from([
            (
                "openrouter".to_string(),
                responses_provider("https://openrouter.test/api/v1"),
            ),
            (
                "openrouter-messages".to_string(),
                messages_provider("https://openrouter.test/api/v1"),
            ),
        ]);
        frontier_clients(&split, &providers, &real_upstream)
            .expect("two definitions at one origin is the supported shape");
    }

    /// **There is no implicit `anthropic` provider.**
    ///
    /// `openai` is implicit because catalogs predating the registry named it
    /// with no definition; nothing can predate a dialect that had no client, so
    /// an `anthropic_messages` entry with no definition is a mistake rather than
    /// a legacy shape. The refusal matters because the only provider that
    /// *reaches* the undefined arm at all is `openai` itself: without this arm,
    /// a `wire_protocol` typo'd onto an `openai` entry would build a client
    /// pointed at `api.anthropic.com` from an environment variable named for
    /// OpenAI.
    #[test]
    fn an_anthropic_entry_with_no_definition_is_refused_rather_than_defaulted() {
        let catalog = StaticFrontierCatalog::new(vec![entry(
            BUILT_IN_OPENAI,
            "gpt-5.6-sol",
            WireProtocol::AnthropicMessages,
        )]);
        let error = frontier_clients(&catalog, &HashMap::new(), &real_upstream)
            .expect_err("there is no implicit Anthropic provider to fall back to");
        let message = error.to_string();
        assert!(
            message.contains("gpt-5.6-sol") && message.contains("anthropic_messages"),
            "the refusal must name the entry and its dialect: {message}"
        );

        // CONTROL: the identical entry on the dialect the implicit provider is
        // for. One field different and it boots, so the refusal is about the
        // missing definition rather than about `openai` entries in general.
        frontier_clients(
            &StaticFrontierCatalog::new(vec![entry(
                BUILT_IN_OPENAI,
                "gpt-5.6-sol",
                WireProtocol::OpenAiResponses,
            )]),
            &HashMap::new(),
            &real_upstream,
        )
        .expect("the implicit `openai` provider speaks the Responses wire");
    }

    /// G08 (review finding): the file `examples/catalog.example.json` tells an
    /// operator to copy it, and its own README calls it the starting point.
    /// `tests/example_catalog.rs` only proves it survives `CatalogConfig::load`
    /// — the config boundary's checks — never this file's third cross-check,
    /// which only runs here because it is a fact about the binary, not the
    /// file. The shipped `anthropic`/`anthropic_messages` entry is exactly the
    /// shape `a_dialect_this_build_cannot_speak_stops_the_boot_and_names_the_entry`
    /// exercises by hand above, so the desired property is that the shipped
    /// example, loaded for real and pointed at a real upstream the way the
    /// README instructs, boots the shipped binary rather than being refused
    /// by a dialect this build has no client for.
    ///
    /// **M10 closed it by moving the entry; M11.0 closed it by adding the
    /// client.** The example kept `providers.anthropic` as a definition nothing
    /// named — the same treatment `dynamo-fleet` still has — because an
    /// Anthropic Messages client was a transport that milestone had not set out
    /// to add, and adding one to satisfy a comment is the wrong order. This
    /// milestone did set out to add it, so the example now carries an
    /// `anthropic_messages` **models** entry naming that definition, and this
    /// test is what says the shipped file still boots the shipped binary with it
    /// — which is a stronger claim than it was, because the entry now has to
    /// resolve a route, a dialect *and* a transport rather than being skipped.
    /// `dynamo-fleet` stays unnamed: `openai_chat_completions` still has no
    /// client, and that is the case the twin test above covers.
    #[test]
    fn the_shipped_example_catalog_boots_the_shipped_binary() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/catalog.example.json");
        let config = roundhouse_server::CatalogConfig::load(&path)
            .expect("tests/example_catalog.rs already pins that this file parses and validates");

        frontier_clients(&config.catalog(), &config.providers, &real_upstream).expect(
            "an operator who copies the README's own example and names a real upstream must \
             get a booted process, not a boot-time refusal naming a dialect they never chose",
        );
    }

    /// **P2: one client per provider, not one client for the catalog.**
    ///
    /// The property the whole registry exists for. Two entries whose providers
    /// are two origins must resolve to two *different* transports — if they
    /// resolved to one, a turn routed to the second would authenticate against
    /// the first's base URL with the first's headers, and the only symptom
    /// would be a 401 from an origin nobody meant to call.
    #[test]
    fn each_provider_resolves_to_its_own_client() {
        let catalog = StaticFrontierCatalog::new(vec![
            entry(
                "openrouter",
                "moonshotai/kimi-k3",
                WireProtocol::OpenAiResponses,
            ),
            entry("openai", "gpt-5.6-sol", WireProtocol::OpenAiResponses),
        ]);
        let providers = HashMap::from([(
            "openrouter".to_string(),
            responses_provider("https://openrouter.ai/api/v1"),
        )]);

        let registry = frontier_clients(&catalog, &providers, &real_upstream).unwrap();
        let openrouter = registry.for_provider("openrouter").unwrap();
        let openai = registry.for_provider("openai").unwrap();
        assert!(
            !Arc::ptr_eq(openrouter, openai),
            "two providers sharing one transport would send one's traffic to the other's \
             origin under the other's headers"
        );

        // And a name nothing defined is an error rather than whichever client
        // happened to be first. A keyed registry has no fallback on purpose:
        // the alternative is an undefined provider quietly reaching a real
        // origin.
        assert!(
            matches!(
                registry.for_provider("anthropic"),
                Err(roundhouse_fleet::FrontierError::UnknownProvider(name)) if name == "anthropic"
            ),
            "a keyed registry must not answer for a provider nobody defined"
        );

        // CONTROL: the uniform registry, which is what every pre-M10.1
        // deployment and every echo-stub test is. It answers for every name,
        // with one client — so the assertion above is about *keyed* registries
        // and not about `for_provider` being strict everywhere.
        let uniform = FrontierClients::uniform(Arc::new(EchoFrontierClient::new("x")));
        assert!(Arc::ptr_eq(
            uniform.for_provider("openrouter").unwrap(),
            uniform.for_provider("anything-at-all").unwrap()
        ));
    }

    /// Everything `tracing::warn!` wrote during one closure, as text.
    ///
    /// `frontier_clients` cannot refuse a provider with no key anywhere — it is
    /// not where keys live, per its own doc comment — so a missing credential
    /// has nowhere to go but a boot-time warning. Nothing else in this suite
    /// reads what `tracing` emits, which is exactly why M10.1 refute's item 15
    /// found this warning silenceable without turning a single test red: no
    /// capture point existed. This is that point.
    fn captured_warnings(f: impl FnOnce()) -> String {
        use std::io;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl io::Write for Buf {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for Buf {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        // **One capture at a time, and not for tidiness.** `with_default`
        // installs a *thread-local* subscriber, and installing one makes
        // `tracing` reconsider its callsite interest cache against the global
        // dispatcher — which in a test binary is nobody. A reconsideration that
        // lands while another test is mid-capture can cache "nothing is
        // interested" for the very callsite that test is asserting on, and its
        // warning silently never arrives: the guard goes red for a reason that
        // has nothing to do with the code under test, on one run in some
        // hundreds. Seen for real once G15 gave this helper a second caller.
        // The cost is microseconds of serialized test time; the alternative is
        // an intermittently green guard, which enforces nothing and gets
        // re-diagnosed from scratch by whoever meets it next.
        static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
        let _serialized = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        // Rebuilding the interest cache *inside* the thread-local default is
        // the second half of the serialization above. The merge that brought
        // more uncaptured `frontier_clients` callers into this binary made the
        // poisoned-cache case go from one-in-hundreds to two-in-three: a
        // concurrent test evaluating the warn callsite under the no-op global
        // dispatcher caches "never interested", and this capture then records
        // the info line but not the warning it exists to assert on. Rebuilding
        // while our subscriber is the active default re-evaluates every
        // callsite against a dispatcher that wants them.
        tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            f()
        });
        String::from_utf8(buf.0.lock().unwrap().clone()).expect("tracing output is UTF-8")
    }

    /// **A provider with a definition and no key anywhere warns at boot.**
    ///
    /// The refute suite's item 15: silencing this line left every test in this
    /// file green, because none of them looked at `tracing` output. The
    /// production behavior it guards is real — an operator finds out here or
    /// finds out from a tenant's failed turn later — so the gap was in the
    /// test, not the code; this closes it the same way the fair-use ordering
    /// gap was closed, by making the previously-unobserved effect observable
    /// rather than by touching `frontier_clients` itself.
    ///
    /// **Both dialects, since M11.0, and that is not symmetry for its own
    /// sake.** The warning began life inside the one arm that existed; adding a
    /// second arm is exactly the edit that leaves it behind, and a Messages
    /// provider whose key is missing is *more* likely than a Responses one,
    /// because `ANTHROPIC_API_KEY` is a variable a deployment adds later. The
    /// loop-level call site is what makes this hold for a third arm too, and
    /// this is the test that says so.
    #[test]
    fn a_defined_provider_with_no_key_anywhere_warns_at_boot() {
        for (dialect, definition) in [
            (
                WireProtocol::OpenAiResponses,
                responses_provider("https://openrouter.ai/api/v1"),
            ),
            (
                WireProtocol::AnthropicMessages,
                messages_provider("https://api.anthropic.test/v1"),
            ),
        ] {
            let catalog = StaticFrontierCatalog::new(vec![entry(
                "openrouter",
                "moonshotai/kimi-k3",
                dialect,
            )]);
            let providers = HashMap::from([("openrouter".to_string(), definition)]);

            let output = captured_warnings(|| {
                frontier_clients(&catalog, &providers, &real_upstream)
                    .expect("a missing key is a warning, not a boot refusal");
            });
            assert!(
                output.contains("A_PROVIDER_KEY") && output.contains("not set"),
                "the warning must name the unset variable on the `{}` arm too: {output}",
                dialect.wire_name()
            );
        }

        let catalog = StaticFrontierCatalog::new(vec![entry(
            "openrouter",
            "moonshotai/kimi-k3",
            WireProtocol::OpenAiResponses,
        )]);
        let providers = HashMap::from([(
            "openrouter".to_string(),
            responses_provider("https://openrouter.ai/api/v1"),
        )]);

        // CONTROL: the identical catalog and provider, with the env carrying
        // the key `responses_provider` names. No warning is the only correct
        // silence -- distinguishing it from the one above is what stops this
        // test passing on a build that warns unconditionally.
        let output = captured_warnings(|| {
            frontier_clients(&catalog, &providers, &|name| {
                (name == "A_PROVIDER_KEY" || name == FRONTIER_UPSTREAM_VAR)
                    .then(|| real_upstream(name).unwrap_or_else(|| "present".to_string()))
            })
            .expect("a present key boots clean");
        });
        assert!(
            !output.contains("A_PROVIDER_KEY"),
            "a provider whose key is set must not warn about it: {output}"
        );
    }

    /// **Thermo-nuclear review G15.** An explicit `providers.openai` entry
    /// takes the `Some(definition)` arm and dispatches on `definition.base_url`
    /// alone — the `None` arm is the only place `ROUNDHOUSE_OPENAI_API_BASE`
    /// and `ROUNDHOUSE_OPENAI_PASS_THROUGH_BASE` are read at all. A deployment
    /// behind a corporate egress proxy that sets the former, then adds an
    /// `openai` provider entry to attach `extra_headers`, has the proxy
    /// silently stop being used: the only boot output is an `info!` line
    /// naming the definition's own base URL, worded exactly like the case
    /// where no variable was ever set. This asserts the boot output says so by
    /// naming each shadowed variable that is actually set.
    ///
    /// **Both variables, not just the one the finding exercised.** They address
    /// different origins — a stored key's API base and a forwarded ChatGPT
    /// seat's — and a deployment may well have set only the second, so a
    /// warning wired for one of them would leave the other exactly as silent as
    /// before.
    #[test]
    fn an_explicit_openai_definition_says_it_is_taking_over_from_the_variables() {
        let catalog = StaticFrontierCatalog::new(vec![entry(
            BUILT_IN_OPENAI,
            "gpt-5.6-sol",
            WireProtocol::OpenAiResponses,
        )]);
        let providers = HashMap::from([(
            BUILT_IN_OPENAI.to_string(),
            responses_provider("https://openai-relay.internal/v1"),
        )]);
        let env = |name: &str| match name {
            FRONTIER_UPSTREAM_VAR => Some("openai_responses".to_string()),
            OPENAI_API_BASE_VAR => Some("https://egress-proxy.internal/v1".to_string()),
            OPENAI_PASS_THROUGH_BASE_VAR => Some("https://egress-proxy.internal/v1".to_string()),
            "A_PROVIDER_KEY" => Some("present".to_string()),
            _ => None,
        };

        let output = captured_warnings(|| {
            frontier_clients(&catalog, &providers, &env)
                .expect("an explicit openai definition with its key present boots clean");
        });
        for var in [OPENAI_API_BASE_VAR, OPENAI_PASS_THROUGH_BASE_VAR] {
            assert!(
                output.contains(var),
                "an explicit `openai` provider definition silently overrides \
                 {var}; the boot log must name the shadowed variable so an \
                 operator who set it does not conclude their proxy is still in \
                 the path from an info! line that reads identically either way: \
                 {output}"
            );
        }
        assert!(
            output.contains("https://openai-relay.internal/v1"),
            "and the origin that won, since the remedy is to choose between the \
             two: {output}"
        );

        // CONTROL 1: the identical definition with neither variable set. This is
        // the ordinary case — an operator who never had a proxy — and naming a
        // variable they did not set would be the same noise in the other
        // direction, which is how a real warning gets ignored.
        let output = captured_warnings(|| {
            frontier_clients(&catalog, &providers, &|name| match name {
                FRONTIER_UPSTREAM_VAR => Some("openai_responses".to_string()),
                "A_PROVIDER_KEY" => Some("present".to_string()),
                _ => None,
            })
            .expect("a definition with no variables to shadow boots clean");
        });
        assert!(
            !output.contains(OPENAI_API_BASE_VAR) && !output.contains(OPENAI_PASS_THROUGH_BASE_VAR),
            "nothing was shadowed here, so nothing may be reported as shadowed: {output}"
        );

        // CONTROL 2: the same variables set with *no* explicit definition —
        // the `None` arm, where they are read and honored. The warning is about
        // a definition taking precedence, so the arm that obeys them must stay
        // silent about it.
        let output = captured_warnings(|| {
            frontier_clients(&catalog, &HashMap::new(), &env)
                .expect("the implicit provider reads the variables");
        });
        assert!(
            !output.contains("takes precedence"),
            "the implicit arm honors both variables; a shadowing warning there \
             would send an operator to remove a definition they never wrote: {output}"
        );
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

    /// The same one-key plane, plus a tier recipe on the project.
    ///
    /// Built by adding one field to `plane_with`'s output rather than by a
    /// second constructor, so the probe below and its control differ in exactly
    /// that field.
    fn plane_with_tiers(tiers: serde_json::Value) -> ControlPlane {
        let json = serde_json::json!({
            "projects": [{ "id": "acme", "tiers": tiers }],
            "users": [{ "id": "ada" }],
            "keys": [{ "project": "acme", "user": "ada", "key_sha256": "a".repeat(64) }],
        })
        .to_string();
        ControlPlane::configured(
            ControlPlaneConfig::from_json(&json, "startup cross-check fixture")
                .expect("the fixture config must validate"),
        )
    }

    /// **M10.2, S3.** The stage router is composed for the deployments that
    /// configured one and for no others.
    ///
    /// Both halves fail differently and both are silent. Composing it for a
    /// deployment with no recipe relabels every decision in its log `stage` on
    /// an upgrade that changed no routing; *not* composing it for one that has a
    /// recipe leaves the recipe resolving to an `Admission` field nothing reads
    /// — an operator's routing configuration doing nothing, with every surface
    /// reporting the file as valid. See `composes_the_stage_router`.
    #[test]
    fn the_stage_router_is_composed_exactly_when_a_project_configures_tiers() {
        assert!(
            composes_the_stage_router(&plane_with_tiers(serde_json::json!({
                "capable": ["anthropic/big"],
                "efficient": ["local/small"],
            }))),
            "a project with a recipe must get the router that reads one"
        );

        // CONTROL: the same plane shape, the same key, no `tiers`. A recipe is
        // the only thing that may move this answer -- not a policy, not a
        // budget, both of which every one of these fixtures also carries.
        for plane in [
            plane_with_policy(serde_json::json!({ "min_quality": 0.5 })),
            plane_with(
                serde_json::json!({}),
                serde_json::json!({
                    "limit_usd": 10.0,
                    "window": "total",
                    "on_exhaustion": "degrade_to_local"
                }),
            ),
        ] {
            assert!(
                !composes_the_stage_router(&plane),
                "a project that configured no recipe must route through the \
                 policy it always did, under the name it always had"
            );
        }

        // And an unconfigured deployment has no file to write a recipe in.
        assert!(!composes_the_stage_router(&ControlPlane::Open));
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
            None,
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
        refuse_promises_of_a_local_fallback(&plane_with_policy(cadence), &with_fleet, None)
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
            None,
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
            None,
        )
        .expect("an exhausted budget has somewhere to go when a local worker is quoted");
    }

    /// A one-key plane whose project declares a credential arrangement.
    ///
    /// Through the config file for the reason [`plane_with`] is: the mode, the
    /// tiers and the mutual-exclusion check are all inside `validate`, and a
    /// fixture that assembled an `Admission` by hand would be checking a
    /// promise no operator could write.
    fn plane_with_credentials(credentials: serde_json::Value) -> ControlPlane {
        let json = serde_json::json!({
            "projects": [{ "id": "acme", "credentials": credentials }],
            "users": [{ "id": "ada" }],
            "keys": [{ "project": "acme", "user": "ada", "key_sha256": "a".repeat(64) }],
        })
        .to_string();
        ControlPlane::configured(
            ControlPlaneConfig::from_json(&json, "credential cross-check fixture")
                .expect("the fixture config must validate"),
        )
    }

    #[test]
    fn a_credential_mode_that_reaches_no_provider_refuses_to_serve_without_a_fleet() {
        // The credential half of the same promise, and the one only this file
        // can ask: `config.rs` refuses an environment variable this process
        // does not *have*, which needs no catalog. Whether the providers a key
        // can authenticate to are providers this deployment can route to is the
        // catalog's half, and the two files cannot see each other.
        //
        // PROBE: `user_only` with no member key anywhere. Every hosted
        // candidate goes, and a fleetless deployment has nothing to degrade to,
        // so every turn of that key's would fail.
        let error = refuse_promises_of_a_local_fallback(
            &plane_with_credentials(serde_json::json!({ "mode": "user_only" })),
            &reachable_candidates(&priced_catalog()),
            None,
        )
        .expect_err("a mode that reaches nothing has nowhere to degrade to");
        let message = error.to_string();
        assert!(
            message.contains("project `acme`, user `ada`"),
            "the refusal has to name the key an operator would go and fix: {message}"
        );
        assert!(
            message.contains("credential mode reaches no hosted provider"),
            "and it has to name the promise: {message}"
        );

        // Acceptance: the identical arrangement on a deployment that quotes a
        // local worker. Degrading to local is what the mode promises, and here
        // there is a local worker to degrade to -- which is what makes the
        // refusal above about the fleet rather than about `user_only`.
        let mut with_fleet = reachable_candidates(&priced_catalog());
        with_fleet.push(local_candidate());
        refuse_promises_of_a_local_fallback(
            &plane_with_credentials(serde_json::json!({ "mode": "user_only" })),
            &with_fleet,
            None,
        )
        .expect("a member with no key degrades to local when there is a local worker");

        // CONTROL, and the one that keeps this check from refusing every
        // deployment: a project that declares no credentials at all is not
        // gating on them, so nothing is withheld and nothing is promised.
        refuse_promises_of_a_local_fallback(
            &plane_with(serde_json::json!({}), serde_json::Value::Null),
            &reachable_candidates(&priced_catalog()),
            None,
        )
        .expect("a file that says nothing about credentials promises nothing about them");
    }

    #[test]
    fn a_pass_through_project_boots_because_its_credential_arrives_with_the_turn() {
        // The other half of the credential check, and the half that decides
        // whether the marquee BYOK arm is deployable at all.
        //
        // PROBE: `pass_through`, no local worker, a catalog with hosted
        // providers in it — the ordinary shape of a pass-through deployment.
        // Reachability under this mode is a fact about *one request*: the
        // credential is the caller's, and at boot no caller exists, so the
        // configured resolution says "nothing presented" and every provider
        // reads as unreachable. Asked the same question the other modes are
        // asked, that answer refuses every pass-through project on every
        // deployment and the process never reaches `axum::serve`.
        refuse_promises_of_a_local_fallback(
            &plane_with_credentials(serde_json::json!({ "mode": "pass_through" })),
            &reachable_candidates(&priced_catalog()),
            None,
        )
        .expect("a forwarded credential arrives with the turn, not with the boot");

        // And the same project on a deployment that quotes a local worker too,
        // which must not start refusing for some *other* reason.
        let mut with_fleet = reachable_candidates(&priced_catalog());
        with_fleet.push(local_candidate());
        refuse_promises_of_a_local_fallback(
            &plane_with_credentials(serde_json::json!({ "mode": "pass_through" })),
            &with_fleet,
            None,
        )
        .expect("and a fleet beside it changes nothing about the answer");

        // CONTROL, and the reason this is an exemption rather than a weakening:
        // `user_only` with no member key anywhere is a *structural* fact a boot
        // can see — no request will ever supply the missing key, because the
        // mode reads a tier the file leaves empty — and it is still refused.
        // An exemption that covered both would trade one undeployable arm for
        // a silent one.
        refuse_promises_of_a_local_fallback(
            &plane_with_credentials(serde_json::json!({ "mode": "user_only" })),
            &reachable_candidates(&priced_catalog()),
            None,
        )
        .expect_err("a mode whose key is missing from the file is still refused");
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
                None,
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
                None,
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
            None,
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

    /// A one-key plane whose project enrols in the validate loop.
    fn plane_that_validates() -> ControlPlane {
        let json = serde_json::json!({
            "projects": [{ "id": "acme", "validate": { "enabled": true } }],
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
    fn a_project_enrolled_in_the_validate_loop_with_no_judge_refuses_to_serve() {
        let reachable = reachable_candidates(&echo_catalog());

        // Probe: the config says these sessions will be checked, and this
        // deployment has nothing to check them with. Serving anyway would stamp
        // arms, fire triggers, and record `NotRun { JudgeUnavailable }` on every
        // one of them — an experiment that loads, runs, and produces an empty
        // comparison, discovered whenever somebody finally reads the dashboard.
        let error = refuse_promises_of_a_local_fallback(&plane_that_validates(), &reachable, None)
            .expect_err("an enrolled project with no judge must stop the process");
        let message = error.to_string();
        assert!(
            message.contains("project `acme`, user `ada`"),
            "the refusal has to name the key an operator would go and fix: {message}"
        );
        assert!(
            message.contains("ROUNDHOUSE_JUDGE_MODEL"),
            "and the variable that fixes it, since the remedy is not in the \
             control-plane file the rest of this key lives in: {message}"
        );

        // Control 1: the identical config on a deployment that *has* a judge
        // serves. The refusal is about the missing judge and not about the
        // `validate` block existing.
        let judge = echo_catalog().models()[0].clone();
        refuse_promises_of_a_local_fallback(&plane_that_validates(), &reachable, Some(&judge))
            .expect("an enrolled project with a judge behind it is servable");

        // Control 2: a project that never enrolled is asked nothing, so a
        // deployment with no judge is unaffected by this check.
        refuse_promises_of_a_local_fallback(
            &plane_with_policy(serde_json::json!({})),
            &reachable,
            None,
        )
        .expect("a key that makes no validation promise has none to break");
    }

    #[test]
    fn an_open_deployment_has_no_policies_to_cross_check() {
        let reachable = reachable_candidates(&echo_catalog());
        refuse_policies_that_admit_nothing(&ControlPlane::Open, &reachable)
            .expect("open mode resolves to the unrestricted policy");
        refuse_promises_of_a_local_fallback(&ControlPlane::Open, &reachable, None)
            .expect("and the unrestricted policy carries no cadence and no budget");
        // And the empty catalog is the deployment's problem, not this check's:
        // with nothing quoted there is nothing to disagree about, and the
        // routing layer's own `NoCandidates` is the accurate answer.
        refuse_policies_that_admit_nothing(&ControlPlane::Open, &[])
            .expect("no catalog is not a policy mistake");
    }
}
