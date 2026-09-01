// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What an operator hands a Claude Code client so that client hooks up to this
//! deployment without being modified.
//!
//! [`codex_launch`](crate::codex_launch)'s sibling, and the differences between
//! them are all facts about the two clients rather than taste.
//!
//! **Nothing here writes a file, and that is the whole shape of this module.**
//! Codex is configured by a `config.toml` it reads out of its `CODEX_HOME`;
//! Claude Code's entire redirect surface is environment
//! (`agent-docs/research/claude-code-client-surface.md` §1.1–§1.6) — a base URL
//! read by the vendored SDK, a newline-separated header block, and the API-key
//! variable. So the output is an [`ClaudeEnv`] and there is no settings overlay
//! beside it: a `settings.json` would be a second place the same three answers
//! could be given, and the client resolves the two in an order this module would
//! then have to be right about. If something non-secret ever genuinely needs one,
//! it is a decision to make then rather than a door to leave open now.
//!
//! **The turn key is *in* that map, and the `codex_launch` rule still holds.**
//! That rule is "a secret rides the environment and never a file", and
//! `codex_launch` could keep the secret out of its own hands entirely because
//! codex's `env_key` / `env_http_headers` name a *variable*. Claude Code offers
//! no such indirection: `ANTHROPIC_CUSTOM_HEADERS` is parsed as literal
//! `Name: Value` lines (§1.6), so the only way to put roundhouse's turn key in
//! [`TURN_KEY_HEADER`] is to put the key itself in a variable. This module
//! therefore holds one, as a [`Secret`], and the type is what keeps the rule
//! honest: [`ClaudeEnv`]'s `Debug` renders a fingerprint, and
//! [`ClaudeEnv::vars`] is the single seam that yields plaintext — the same shape
//! `ForwardedCredential::headers` uses for the other credential this system
//! holds in flight.
//!
//! **Which topology this is.** The *Direct* one — an agent pointed straight at
//! roundhouse — and it is the reference by the same ruling that made it so for
//! Codex (`agent-docs/synergies/ecosystem-round-2.md`'s launch-surface dedup).
//! Chained through a NeMo Relay is supported **by this same map**: Relay
//! overwrites the base URL and merges into the header block rather than
//! replacing it, so the turn key survives the hop and one generator serves both
//! topologies. See [the chained runbook](#the-chained-runbook) below for what
//! has to be right on Relay's side for that to hold.
//!
//! **What this deliberately does not set.**
//!
//! - **No model.** Codex needs a slug because its catalog is pinned to one;
//!   here the model rides in the request body, roundhouse ignores it (routing
//!   policy chooses the target), and naming one would not be free on the
//!   client's side: the `anthropic-beta` envelope is built *from the model
//!   string* (§1.5 — `[1m]`, `haiku` and the thinking-mode gates are all
//!   substring tests on it), so a slug chosen here changes which betas arrive
//!   without changing anything roundhouse does with them.
//! - **No autoupdater or telemetry variables.** `DISABLE_AUTOUPDATER` and
//!   `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` are deployment policy, not part
//!   of what makes a client reach roundhouse, and a generator that mixed the two
//!   would make its own output impossible to read as "the hook-up, and nothing
//!   else". They belong to whoever spawns the process — M11.3's launcher, and
//!   the gated e2e suite, both of which set them explicitly.
//! - **No MCP wiring.** Claude Code takes `--mcp-config` / `.mcp.json` rather
//!   than a TOML table, and it spells an MCP tool `mcp__server__tool` **flat**
//!   where codex nests the pair. That flat spelling is open question 3 of the
//!   plan — [`ClientDialect`](crate::dialect::ClientDialect) owes an arm for it
//!   and `canonical_item` owes the reverse split — and wiring an MCP server this
//!   surface cannot yet resolve a steer through would produce a client whose
//!   every correction comes back unresolvable. Deferred with the MCP control
//!   surface, by name.
//!
//! # The two auth kinds
//!
//! [`ClaudeAuthKind`] mirrors
//! [`CodexAuthKind`](crate::codex_launch::CodexAuthKind) exactly, and the pair of
//! failures it stands between are the same two: an ambient login silently
//! aimed at roundhouse, and a launch that quietly forwards nothing.
//!
//! [`ClaudeAuthKind::RoundhouseKey`] writes
//! [`ROUNDHOUSE_API_KEY_SENTINEL`] into `ANTHROPIC_API_KEY`, and that line is the
//! analogue of `codex_launch` writing `env_key` beside
//! `requires_openai_auth = false`. §1.3's `VV()` is the whole reason: a
//! subscription login is suppressed when — and only when — one of five inputs
//! resolves, and `ANTHROPIC_BASE_URL` is not one of them. Leave the variable
//! empty and a client whose user happens to be logged in presents that login's
//! OAuth bearer to *our* base URL (§1.4: no host check gates the inference
//! path), so roundhouse receives a real subscription seat from an operator who
//! chose the bring-your-own-roundhouse-key stanza and never said so.
//!
//! **Two limits of that suppression, recorded rather than reconciled**, because
//! a launcher cannot lie about the environment it runs in:
//!
//! 1. **Interactive mode prompts once.** The documented behaviour
//!    (`code.claude.com/docs/en/env-vars`, quoted at §1.3) is that in
//!    non-interactive mode (`-p`) a present key is always used, while in
//!    interactive mode the user is asked to approve it overriding their
//!    subscription — once. Until they do, an interactive session is on the
//!    subscription.
//! 2. **`CLAUDE_CODE_REMOTE=true` defeats it entirely.** The API-key arm of
//!    `VV()` is guarded by `!CLAUDE_CODE_REMOTE`, so inside a Claude Code Remote
//!    container the sentinel suppresses nothing and the container's managed
//!    OAuth token is presented instead — observed, not inferred (§5.7's isolation
//!    note captured exactly that, including two remote-only headers and the
//!    `oauth-2025-04-20` beta). Nothing this module writes changes that; what it
//!    means is that a launch under CCR is a `ForwardedClaudeLogin` in fact
//!    whatever it says on the tin.
//!
//! The sentinel is safe to set because the serve side treats it as inert:
//! [`ROUNDHOUSE_API_KEY_SENTINEL`] lives with the admission boundary that
//! refuses to forward it, and `control_config`'s
//! `the_launchers_api_key_sentinel_is_never_forwarded_as_a_seat` is where that
//! is pinned. Without that rule the value would ride upstream on `x-api-key` —
//! a header the Anthropic allowlist row admits — beside a real seat, and
//! Anthropic answers a bad key next to a valid bearer with a `401` an operator
//! reads as a revoked login.
//!
//! [`ClaudeAuthKind::ForwardedClaudeLogin`] writes no API key at all, and
//! **refuses** any launch that sets one of the five suppressing inputs. Refusal
//! rather than a warning, and rather than silently unsetting them, for the
//! posture `codex_launch` set: each of the five turns forwarding off while
//! leaving every request valid, so the run looks healthy and the seat simply
//! never happens. [`ClaudeLaunch::must_be_unset`] is the same list in the form a
//! launcher can enforce.
//!
//! # The chained runbook
//!
//! **The chained topology is the Direct one with a NeMo Relay in the middle,
//! and it takes this same map.** That is not the shape it looked like from the
//! evidence alone, so it is worth stating what changed: `nemo-relay claude`
//! *overwrites* `ANTHROPIC_BASE_URL` with its own gateway and merges its
//! `x-nemo-relay-proxy-token` into `ANTHROPIC_CUSTOM_HEADERS`
//! (`agents/claude/launch.rs:19-31,46-48,113-127`), which reads at first like a
//! surface that cannot carry a key of ours. It carries every one of them:
//!
//! - the merge is a **line-wise replacement of the matching name only**
//!   (`replace_custom_header`, `launch.rs:113-127`), so a [`TURN_KEY_HEADER`]
//!   line in the operator's `ANTHROPIC_CUSTOM_HEADERS` survives beside Relay's
//!   token;
//! - Relay's dispatch forwards headers it does not own untouched
//!   (`gateway/response.rs:59-72`, `should_forward_request_header`, which
//!   subtracts hop-by-hop names, `Host`, `Content-Length`, `Accept-Encoding`
//!   and its own two credential headers — and nothing else);
//! - and it therefore also strips `x-nemo-relay-proxy-token` before the hop, so
//!   Relay's credential never leaves Relay.
//!
//! So the reference chained wiring is: **hand the client this [`ClaudeEnv`]
//! (either auth kind), launch it through `nemo-relay run --agent claude
//! --config <toml>`, and aim `[upstream] anthropic_base_url` at this
//! deployment's root with no `anthropic_auth_header` at all.** The turn key
//! reaches roundhouse on its dedicated header, a chained turn keeps Direct's
//! semantics exactly (seat-capable under
//! [`ClaudeAuthKind::ForwardedClaudeLogin`], sentinel inert under
//! [`ClaudeAuthKind::RoundhouseKey`]), and one generator serves both topologies.
//! Observed rather than inferred: the chained tests in
//! `crates/roundhouse-server/tests/claude_e2e.rs` drive a real Relay 0.8.2 and
//! assert exactly that header set arriving.
//!
//! The base URL is the **deployment root** for the same reason it is one here:
//! Relay concatenates the inbound `path_and_query` onto it whole
//! (`gateway/routes.rs:141-151`), so a value carrying [`API_PREFIX`] produces
//! `/v1/v1/messages`. `?beta=true` survives that concatenation (R7 hazard 3),
//! and the gateway must bind loopback — 0.8.2 refuses anything else outright
//! (`server/mod.rs:92-97`, new at that release). `run` is the wizard-free entry
//! point: the bare `nemo-relay claude` shortcut runs interactive setup when no
//! config layer exists.
//!
//! ## The fallback: a credential-less client
//!
//! When the client presents *no* credential of its own — no login, no
//! `ANTHROPIC_API_KEY`, so neither of this module's two arms as written — Relay
//! can carry the turn key itself, in `[upstream] anthropic_auth_header`.
//! Deliberately untested: the reference wiring works, and a guard on a path
//! nobody deploys is a guard that rots. What an operator reaching for it must
//! know:
//!
//! - **Relay injects it only into `Authorization`, and only when the inbound
//!   request is unauthenticated.** `already_authed` short-circuits on any of
//!   `authorization` / `x-api-key` / `api-key` / `anthropic-api-key`
//!   (`gateway/mod.rs:1070-1078`), and [`ROUNDHOUSE_API_KEY_SENTINEL`] on
//!   `x-api-key` is exactly such a header. So this arm requires
//!   `ANTHROPIC_API_KEY` unset, which under §1.3 means an ambient login is *not*
//!   suppressed — hence "credential-less client" is a precondition rather than a
//!   preference.
//! - **The value is `Bearer <turn key>`**, because `ControlPlane::presented_key`
//!   requires that scheme on `Authorization`.
//! - **Therefore those turns are key-authed only**, arriving with
//!   `dedicated_header == false`: a turn key in `Authorization` is not a
//!   pass-through-shaped request, and no seat is ever captured on that path.
//!
//! ## Two refusals, and one thing this surface does not offer
//!
//! - **Hazard 4 — set the base URL and the auth header in one config layer.**
//!   `replace_upstream_base_url` clears a configured `anthropic_auth_header`
//!   whenever the base URL is changed by a *different* layer
//!   (`configuration/mod.rs:1672-1681` at 0.8.2, body unchanged from 0.8.0), so
//!   naming the base URL on the command line and the header in `config.toml`
//!   runs unauthenticated. **A documented refusal, not something roundhouse can
//!   enforce** — the layering happens inside Relay's process, and this
//!   deployment cannot stop an operator from misconfiguring it that way. The
//!   reference wiring above is immune only because it configures no auth
//!   header at all. The clearing itself is not merely asserted here, though:
//!   `nemo-relay run --dry-run` reports what it resolved without spawning the
//!   agent, so `claude_e2e.rs`'s
//!   `hazard_4_a_different_base_url_layer_clears_the_configured_auth_header`
//!   drives that directly and fails loudly if a Relay release ever changes the
//!   behaviour this bullet describes.
//! - **Hazard 5 — a plugin's dispatch-override turn is key-authed only.**
//!   `effective_dispatch_request` strips provider credentials before redirecting
//!   a turn to an explicit target (`gateway/mod.rs:874-908`), so such a turn
//!   arrives carrying no forwarded seat whatever the client presented. Also a
//!   refusal rather than a guard, and for the same reason.
//! - **Resumption is not offered in band on this surface.** Relay's SSE decoder
//!   ignores `id:` lines outright (`codec/streaming.rs:182-198`), so a cursor
//!   carried as an SSE id would not survive the hop; the Messages emitter
//!   carries none, and plan open question 4 is closed that way for this rung.
//!
//! Verified against `claude-cli 2.1.257`. The captures the reasoning above rests
//! on are committed at
//! `crates/roundhouse-server/tests/fixtures/claude-2.1.257-*.json`, and the
//! gated real-binary suite that drives the actual client with this map is
//! `crates/roundhouse-server/tests/claude_e2e.rs`.

use std::collections::BTreeMap;
use std::fmt;

use roundhouse_core::control::{CredentialError, Secret};

use crate::control_config::{KeyKind, TURN_KEY_HEADER, has_valid_key_shape};
use crate::messages_api::MESSAGES_PATH;
use crate::responses_api::API_PREFIX;

pub use crate::control_config::ROUNDHOUSE_API_KEY_SENTINEL;

/// Where the client's vendored SDK reads the deployment address.
///
/// **Read by the SDK, not by Claude Code** (§1.2): the client's own factory
/// never passes `baseURL`, so the SDK's environment default applies, with no
/// validation and no scheme check on that path. The SDK then appends the
/// version segment itself — which is why [`ClaudeLaunch::new`] refuses a value
/// that already carries one.
pub const BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";

/// The newline-separated `Name: Value` block the client merges into its own
/// default headers (§1.6).
///
/// The merge order puts these *after* the SDK's auth headers, so a name spelled
/// here overrides the SDK's own — which is what makes this the carrier for
/// [`TURN_KEY_HEADER`] and would make it a foot-gun for anything else.
pub const CUSTOM_HEADERS_ENV: &str = "ANTHROPIC_CUSTOM_HEADERS";

/// The variable whose resolution suppresses a subscription login (§1.3).
///
/// Written under [`ClaudeAuthKind::RoundhouseKey`] and refused under
/// [`ClaudeAuthKind::ForwardedClaudeLogin`], which is the one line that
/// separates the two kinds.
pub const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// Where an OAuth-suppressing input lives, because they are not all environment
/// variables.
///
/// The distinction is load-bearing for a launcher: four of the five §1.3 names
/// can be cleared by not passing them to the child process, and `apiKeyHelper`
/// cannot — it is a settings key, read out of a file the client finds on its
/// own. A list that flattened the two would promise an enforcement that half of
/// it cannot deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressorSite {
    /// An environment variable the launcher controls.
    EnvVar,
    /// A key in the settings file the client resolves for itself. A launcher can
    /// refuse to run, or point an isolated `CLAUDE_CONFIG_DIR` at a file without
    /// it; it cannot unset this in the child's environment.
    SettingsKey,
}

/// One input that changes how a launched client authenticates, and what it
/// costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OauthSuppressor {
    /// The variable or settings key, spelled exactly as the client reads it.
    pub name: &'static str,
    /// Where it lives. See [`SuppressorSite`].
    pub site: SuppressorSite,
    /// `true` when the input defeats the *redirect* and not only the login — the
    /// three cloud-provider selectors, which make `I7()` return something other
    /// than `firstParty` and therefore make [`BASE_URL_ENV`] unread.
    ///
    /// The field exists because that failure is worse and applies to both auth
    /// kinds: a client under `CLAUDE_CODE_USE_VERTEX` never reaches roundhouse
    /// at all, and nothing on either side reports it.
    pub defeats_the_redirect: bool,
}

/// The inputs §1.3's `VV()` names, in the order it reads them.
///
/// Refused **by presence**, not by truthiness, and the honesty is deliberate:
/// the three cloud selectors are read through a truth function (`$6`) whose body
/// the evidence does not reproduce, so a rule that admitted
/// `CLAUDE_CODE_USE_BEDROCK=0` would be guessing at somebody else's parser. The
/// fail-closed reading costs an operator one deletion and buys never being wrong
/// about it.
///
/// `pub` so a launcher can enumerate the whole table rather than only the subset
/// [`ClaudeLaunch::must_be_unset`] returns for one kind — an operator diagnosing
/// "why is my seat not forwarding" needs the list itself, and a second copy of
/// it in the launcher crate is the drift this constant exists to prevent.
pub const OAUTH_SUPPRESSORS: &[OauthSuppressor] = &[
    OauthSuppressor {
        name: "CLAUDE_CODE_USE_BEDROCK",
        site: SuppressorSite::EnvVar,
        defeats_the_redirect: true,
    },
    OauthSuppressor {
        name: "CLAUDE_CODE_USE_VERTEX",
        site: SuppressorSite::EnvVar,
        defeats_the_redirect: true,
    },
    OauthSuppressor {
        name: "CLAUDE_CODE_USE_FOUNDRY",
        site: SuppressorSite::EnvVar,
        defeats_the_redirect: true,
    },
    OauthSuppressor {
        name: "ANTHROPIC_AUTH_TOKEN",
        site: SuppressorSite::EnvVar,
        defeats_the_redirect: false,
    },
    OauthSuppressor {
        name: "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
        site: SuppressorSite::EnvVar,
        defeats_the_redirect: false,
    },
    OauthSuppressor {
        name: API_KEY_ENV,
        site: SuppressorSite::EnvVar,
        defeats_the_redirect: false,
    },
    OauthSuppressor {
        name: "apiKeyHelper",
        site: SuppressorSite::SettingsKey,
        defeats_the_redirect: false,
    },
];

/// How the client authenticates to roundhouse.
///
/// Two kinds because there are two deployments, mirroring
/// [`CodexAuthKind`](crate::codex_launch::CodexAuthKind) variant for variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeAuthKind {
    /// The client holds a roundhouse turn key and nothing else.
    ///
    /// [`ROUNDHOUSE_API_KEY_SENTINEL`] goes into [`API_KEY_ENV`], never nothing:
    /// an empty variable leaves §1.3's `VV()` free to resolve a subscription
    /// login, and §1.4 establishes that the resulting OAuth bearer follows
    /// [`BASE_URL_ENV`] anywhere — so the ambient login is presented to
    /// roundhouse as if the operator had chosen the other variant. The sentinel
    /// is what makes the client's auth resolution a property of the launch
    /// rather than of whoever last ran `claude` on that machine.
    RoundhouseKey,
    /// The client's own Claude subscription login is forwarded upstream by
    /// roundhouse.
    ///
    /// No [`API_KEY_ENV`], deliberately, and no other suppressor either — see
    /// [`OAUTH_SUPPRESSORS`] and [`ClaudeLaunch::must_be_unset`].
    ///
    /// **The precondition is a completed `claude` login, not this variant.**
    /// Nothing here creates a credential; what the variant does is decline to
    /// suppress one. A client that never logged in sends no `Authorization` at
    /// all, which roundhouse *admits* — `turn_admission` treats "the caller
    /// presented nothing" as a first-class case and degrades the turn to
    /// local-only rather than refusing it — so turns keep answering and frontier
    /// routes simply never happen.
    ForwardedClaudeLogin,
}

/// Why a launch could not be built from the inputs it was given.
///
/// Four refusals rather than one, because the four failures they prevent look
/// nothing alike from the operator's chair. Each names what the *client* does
/// with the bad input, not what this module wanted instead.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClaudeLaunchError {
    #[error(
        "the base URL `{base_url}` already ends in `{API_PREFIX}`. Claude Code's vendored SDK \
         appends the version segment itself, so this launch posts turns to \
         `{base_url}{API_PREFIX}/messages` -- a path nothing serves, on a client that reports it \
         as an upstream connection error rather than as a configuration mistake"
    )]
    BaseUrlCarriesApiPrefix { base_url: String },
    #[error(
        "the base URL is empty. An empty `{BASE_URL_ENV}` is not an error in the client: the SDK \
         falls back to `https://api.anthropic.com`, so the launch runs, answers, and bills \
         somebody's Anthropic account while touching no part of this deployment"
    )]
    BaseUrlIsEmpty,
    #[error(
        "the launch was given an admin key. An `rh_admin_…` secret administers the control plane \
         and is refused on every turn-serving surface as the wrong *kind* of key, so this builds a \
         client that starts, connects, and fails on its first turn and on every turn after"
    )]
    AdminKeyIsNotATurnKey,
    #[error(
        "the launch was not given a roundhouse turn key ({KEY_SHAPE}). The value is deliberately \
         not echoed here: the likeliest wrong value is a credential of somebody else's, and an \
         error message is the last place one should be copied to"
    )]
    NotATurnKey,
    /// **Unreachable today, and kept rather than `expect`ed.**
    ///
    /// [`Secret::api_key`] is the only constructor for the type this module
    /// holds the key in, and it refuses OAuth-shaped material — which nothing
    /// passing the turn-key shape check above can be. The alternative is an
    /// `expect` that claims another crate's predicate will never fire, which is
    /// precisely the claim that stops being true when either predicate moves.
    #[error("the turn key is not a credential roundhouse can hold: {source}")]
    TurnKeyRefused {
        #[from]
        source: CredentialError,
    },
    #[error(
        "this launch also sets {names}, which suppress{plural} the subscription login \
         `ForwardedClaudeLogin` exists to forward. Each one leaves every request valid, so the \
         run looks healthy and the seat simply never arrives; unset them at launch \
         (`ClaudeLaunch::must_be_unset`) rather than launching and reading the routing decisions"
    )]
    OauthSuppressorsPresent { names: String, plural: &'static str },
    #[error(
        "this launch also sets `{name}`, which the generated environment already names. Whichever \
         of the two the launcher applies second wins silently, and the two disagree about where \
         this client posts turns or which key it presents"
    )]
    CollidesWithGeneratedVar { name: String },
    #[error(
        "this launch sets `{name}`, which makes the client select a cloud provider instead of the \
         first-party path -- so `{BASE_URL_ENV}` is never read and the client never reaches this \
         deployment at all. Nothing on either side reports that: the agent answers, from somebody \
         else's serving plane"
    )]
    RedirectDefeated { name: &'static str },
}

/// Spelled once, because two error messages and one check would otherwise drift.
const KEY_SHAPE: &str = "`rh_turn_` followed by 43 base62 characters";

/// Everything a generated launch depends on.
///
/// **Private fields with builders, where
/// [`CodexLaunch`](crate::codex_launch::CodexLaunch) keeps its fields `pub`**,
/// and the difference is the secret. That struct's own doc argues its `pub`
/// fields are a check-at-the-door rather than an invariant, which is a fair
/// trade for a type holding only paths and slugs. Here one field is a live turn
/// key: `pub` would let a caller swap it after [`Self::new`]'s shape check ran,
/// and — worse — would put a `Secret` on a struct whose `Debug` a launcher will
/// print, where the field a future "make it easier to launch" change adds is
/// exactly the one that breaks the redaction.
#[derive(Debug, Clone)]
pub struct ClaudeLaunch {
    base_url: String,
    turn_key: Secret,
    auth: ClaudeAuthKind,
    /// Environment the launcher will apply *beside* the generated map.
    ///
    /// Held rather than checked at the call site because the refusals this
    /// module owes are about the whole launch, not about its own three
    /// variables: a `ForwardedClaudeLogin` that is correct in isolation and
    /// launched next to an `ANTHROPIC_AUTH_TOKEN` forwards nothing.
    also: BTreeMap<String, String>,
    /// Whether the settings file this client will read defines `apiKeyHelper`.
    ///
    /// A `bool` rather than an entry in [`Self::also`] because a settings key is
    /// not an environment variable, and the launcher's remedy for it is
    /// different — see [`SuppressorSite::SettingsKey`].
    api_key_helper: bool,
}

impl ClaudeLaunch {
    /// A bring-your-own-roundhouse-key launch against `base_url`.
    ///
    /// `base_url` is the **deployment root**, not the Messages prefix, and that
    /// is the mirror image of [`CodexLaunch::new`](crate::codex_launch::CodexLaunch::new)'s
    /// rule rather than an inconsistency with it: codex is configured with the
    /// URL it posts to, and Claude Code's SDK builds `{base}/v1/messages` from a
    /// root. Each constructor refuses the shape its own client cannot use.
    ///
    /// A trailing slash is normalised rather than refused, for the reason the
    /// codex sibling gives: it is what a copy-pasted address carries and it has
    /// one unambiguous meaning.
    pub fn new(
        base_url: impl Into<String>,
        turn_key: impl Into<String>,
    ) -> Result<Self, ClaudeLaunchError> {
        let base_url = base_url.into().trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(ClaudeLaunchError::BaseUrlIsEmpty);
        }
        if base_url.ends_with(API_PREFIX) {
            return Err(ClaudeLaunchError::BaseUrlCarriesApiPrefix { base_url });
        }
        let presented = turn_key.into().trim().to_string();
        if !presented.starts_with(KeyKind::Turn.prefix()) || !has_valid_key_shape(&presented) {
            // The admin key gets a refusal of its own, and it is the one worth
            // separating: it is a real secret of this deployment's that an
            // operator plausibly has to hand, it authenticates everywhere
            // *except* here, and "not a turn key" would send them to check
            // whether they had pasted it correctly. Neither arm echoes the
            // value — see the variants.
            return Err(match presented.starts_with(KeyKind::Admin.prefix()) {
                true => ClaudeLaunchError::AdminKeyIsNotATurnKey,
                false => ClaudeLaunchError::NotATurnKey,
            });
        }
        Ok(Self {
            base_url,
            turn_key: Secret::api_key(presented)?,
            auth: ClaudeAuthKind::RoundhouseKey,
            also: BTreeMap::new(),
            api_key_helper: false,
        })
    }

    /// The same, forwarding the client's own subscription login upstream.
    pub fn forwarding_claude_login(mut self) -> Self {
        self.auth = ClaudeAuthKind::ForwardedClaudeLogin;
        self
    }

    /// Declare a variable the launcher will also set on the child process.
    ///
    /// Not a way to add to the generated map — [`Self::env`] refuses a name it
    /// already writes — but the way the generator is *told* what else the launch
    /// carries, so it can refuse the combinations that are silently wrong.
    pub fn also_launching_with(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.also.insert(name.into(), value.into());
        self
    }

    /// Declare that the settings file this client resolves defines
    /// `apiKeyHelper`.
    pub fn with_settings_api_key_helper(mut self) -> Self {
        self.api_key_helper = true;
        self
    }

    /// How the client authenticates. See [`ClaudeAuthKind`].
    pub fn auth(&self) -> ClaudeAuthKind {
        self.auth
    }

    /// The deployment root this launch aims the client at.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Where this deployment serves the Messages API, as the client will
    /// assemble it.
    ///
    /// Derived rather than written so a reader can check the refusal in
    /// [`Self::new`] against the URL it protects. [`API_PREFIX`] is read from the
    /// module that serves the route, for the reason
    /// [`codex_launch`](crate::codex_launch)'s `deployment_root` takes it as a
    /// parameter: two literals agree today and part company silently.
    pub fn messages_url(&self) -> String {
        format!("{}{API_PREFIX}/{MESSAGES_PATH}", self.base_url)
    }

    /// The inputs that must not be set when this launch runs, in the form a
    /// launcher can enforce.
    ///
    /// Kind-dependent, and the asymmetry is the point.
    /// [`ClaudeAuthKind::ForwardedClaudeLogin`] owes all of
    /// [`OAUTH_SUPPRESSORS`] — every one of them turns the forwarding off while
    /// leaving the run looking healthy. [`ClaudeAuthKind::RoundhouseKey`] owes
    /// only the three that defeat the *redirect*, because it deliberately sets
    /// one of the others itself: suppressing the login is that variant's whole
    /// purpose, so listing [`API_KEY_ENV`] here would be asking a launcher to
    /// unset the variable the generator is about to write.
    pub fn must_be_unset(&self) -> Vec<&'static OauthSuppressor> {
        OAUTH_SUPPRESSORS
            .iter()
            .filter(|suppressor| match self.auth {
                ClaudeAuthKind::ForwardedClaudeLogin => true,
                ClaudeAuthKind::RoundhouseKey => suppressor.defeats_the_redirect,
            })
            .collect()
    }

    /// The environment a launched client is given.
    ///
    /// Fallible for the same reason the codex sibling's constructor is: each
    /// refusal below produces a launch that *runs*. The client starts, answers,
    /// and does so against something other than this deployment or on somebody
    /// else's credential.
    pub fn env(&self) -> Result<ClaudeEnv, ClaudeLaunchError> {
        let mut vars: BTreeMap<String, LaunchValue> = BTreeMap::new();
        vars.insert(
            BASE_URL_ENV.to_string(),
            LaunchValue::Public(self.base_url.clone()),
        );
        // Newline-separated `Name: Value`, first colon wins, whitespace trimmed
        // (§1.6). One line, because this is the only header roundhouse needs and
        // every additional one would override an SDK default of the same name.
        vars.insert(
            CUSTOM_HEADERS_ENV.to_string(),
            LaunchValue::Secret {
                prefix: format!("{TURN_KEY_HEADER}: "),
                secret: self.turn_key.clone(),
            },
        );
        if self.auth == ClaudeAuthKind::RoundhouseKey {
            vars.insert(
                API_KEY_ENV.to_string(),
                LaunchValue::Public(ROUNDHOUSE_API_KEY_SENTINEL.to_string()),
            );
        }

        let mut offending: Vec<&'static str> = Vec::new();
        for suppressor in self.must_be_unset() {
            let present = match suppressor.site {
                SuppressorSite::EnvVar => self.also.contains_key(suppressor.name),
                SuppressorSite::SettingsKey => self.api_key_helper,
            };
            if !present {
                continue;
            }
            // The redirect-defeating three get their own refusal even under the
            // kind that tolerates the rest, because the operator's next move is
            // different: an `ANTHROPIC_AUTH_TOKEN` beside a forwarded login is a
            // choice between two credentials, and a `CLAUDE_CODE_USE_VERTEX` is
            // a client that never arrives.
            if suppressor.defeats_the_redirect {
                return Err(ClaudeLaunchError::RedirectDefeated {
                    name: suppressor.name,
                });
            }
            offending.push(suppressor.name);
        }
        if !offending.is_empty() {
            return Err(ClaudeLaunchError::OauthSuppressorsPresent {
                names: offending
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                plural: if offending.len() == 1 { "es" } else { "" },
            });
        }

        for (name, value) in &self.also {
            if vars.contains_key(name) {
                return Err(ClaudeLaunchError::CollidesWithGeneratedVar { name: name.clone() });
            }
            vars.insert(name.clone(), LaunchValue::Public(value.clone()));
        }
        Ok(ClaudeEnv { vars })
    }
}

/// One entry of a generated environment, and whether it carries a secret.
///
/// A two-armed enum rather than a `String`, so a `Debug` of the map cannot print
/// the turn key. The secret arm keeps its non-secret prefix separate — the
/// header *name* is exactly what a reader of a redacted map needs to see, and
/// concatenating it into the `Secret` would hide it along with the key.
#[derive(Clone)]
enum LaunchValue {
    Public(String),
    Secret { prefix: String, secret: Secret },
}

impl LaunchValue {
    fn reveal(&self) -> String {
        match self {
            LaunchValue::Public(value) => value.clone(),
            LaunchValue::Secret { prefix, secret } => format!("{prefix}{}", secret.reveal()),
        }
    }
}

impl fmt::Debug for LaunchValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LaunchValue::Public(value) => write!(f, "{value:?}"),
            LaunchValue::Secret { prefix, secret } => write!(f, "\"{prefix}{secret}\""),
        }
    }
}

/// The environment a launched Claude Code client is given, and the only output
/// of this module.
///
/// **No `Serialize`, no `Deserialize`, no `Display`, and that is the point** —
/// the same rule `roundhouse_core`'s forwarded-credential module states for the
/// other secret this system holds in flight. A map that could be serialized is
/// one that can land in a log line or a generated file, which is the failure the
/// whole "secrets ride env only" rule exists to prevent. `Debug` renders the
/// turn key as `redacted:<fingerprint>`.
#[derive(Debug, Clone)]
pub struct ClaudeEnv {
    vars: BTreeMap<String, LaunchValue>,
}

impl ClaudeEnv {
    /// Every variable and its plaintext value, in name order.
    ///
    /// **The one seam that yields the turn key**, and the analogue of
    /// `ForwardedCredential::headers`. Its caller is whatever spawns the client
    /// process; a call anywhere else is the defect this type exists to make
    /// visible in review, because there is no other way to spell it.
    pub fn vars(&self) -> impl Iterator<Item = (&str, String)> {
        self.vars
            .iter()
            .map(|(name, value)| (name.as_str(), value.reveal()))
    }

    /// The variable names this map sets, in order.
    ///
    /// Free of the reveal above, which is what lets a launcher log *what* it set
    /// without logging what it set them to.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.vars.keys().map(String::as_str)
    }

    /// One variable's plaintext value. Same seam, same rule, as [`Self::vars`].
    pub fn get(&self, name: &str) -> Option<String> {
        self.vars.get(name).map(LaunchValue::reveal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TURN_KEY: &str = "rh_turn_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const ADMIN_KEY: &str = "rh_admin_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const ROOT: &str = "http://127.0.0.1:8080";

    fn launch() -> ClaudeLaunch {
        ClaudeLaunch::new(ROOT, TURN_KEY).expect("the documented-correct shape constructs")
    }

    fn env_of(launch: &ClaudeLaunch) -> BTreeMap<String, String> {
        launch
            .env()
            .expect("the documented-correct shape renders")
            .vars()
            .map(|(name, value)| (name.to_string(), value))
            .collect()
    }

    /// **The snapshot M11.3's launcher will consume**, for both kinds.
    ///
    /// Written as one exhaustive map per kind rather than as a handful of
    /// `contains` assertions, because the property that matters to the launcher
    /// is that the map is *complete and closed*: a fourth variable appearing
    /// here is a change to what a launch means, and one disappearing is a client
    /// that silently falls back to `api.anthropic.com` or to an ambient login.
    #[test]
    fn each_auth_kind_renders_one_exact_environment() {
        assert_eq!(
            env_of(&launch()),
            BTreeMap::from([
                (BASE_URL_ENV.to_string(), ROOT.to_string()),
                (
                    CUSTOM_HEADERS_ENV.to_string(),
                    format!("{TURN_KEY_HEADER}: {TURN_KEY}")
                ),
                (
                    API_KEY_ENV.to_string(),
                    ROUNDHOUSE_API_KEY_SENTINEL.to_string()
                ),
            ]),
        );
        assert_eq!(
            env_of(&launch().forwarding_claude_login()),
            BTreeMap::from([
                (BASE_URL_ENV.to_string(), ROOT.to_string()),
                (
                    CUSTOM_HEADERS_ENV.to_string(),
                    format!("{TURN_KEY_HEADER}: {TURN_KEY}")
                ),
            ]),
            "a forwarded login must carry no API key: any resolved value suppresses \
             the very login it exists to forward"
        );
    }

    /// The header block is in the syntax §1.6 says the client parses.
    ///
    /// Re-derived here by running the client's own regex rather than by
    /// asserting the string this module just built, which would be a tautology.
    /// The parse is `^\s*(.*?)\s*:\s*(.*?)\s*$` per line, non-greedy, so the
    /// first colon wins and whitespace is trimmed on both halves.
    #[test]
    fn the_custom_header_block_parses_the_way_the_client_parses_it() {
        let rendered = env_of(&launch())[CUSTOM_HEADERS_ENV].clone();
        let lines: Vec<&str> = rendered
            .split(['\n', '\r'])
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 1, "one header, one line: {rendered:?}");
        let (name, value) = lines[0]
            .split_once(':')
            .expect("the client splits on the first colon");
        assert_eq!(name.trim(), TURN_KEY_HEADER);
        assert_eq!(value.trim(), TURN_KEY);
        // And the value survives the client's own header-safety check
        // (v2.1.227+ rejects non-HTTP-safe characters): every byte is visible
        // ASCII, so nothing here can be split into a second header.
        assert!(
            rendered.bytes().all(|b| (0x20..=0x7e).contains(&b)),
            "an HTTP-unsafe byte in {rendered:?} is a header the client refuses to send"
        );
    }

    /// The turn key is in the environment and nowhere a reader can reach.
    ///
    /// The structural half is that this module renders no file at all; this is
    /// the behavioural half, and it is aimed at the two things a launcher will
    /// actually do with these types — print them while debugging, and log which
    /// variables it set.
    #[test]
    fn the_turn_key_rides_the_environment_and_no_rendering_of_it() {
        let launch = launch();
        let env = launch.env().expect("renders");
        assert!(
            format!("{env:?}").contains(TURN_KEY_HEADER),
            "a redacted map must still say which header carries the key"
        );
        for rendered in [format!("{env:?}"), format!("{launch:?}")] {
            assert!(
                !rendered.contains(TURN_KEY) && !rendered.contains("rh_turn_"),
                "a Debug of the launch or its environment must redact the key:\n{rendered}"
            );
        }
        assert!(
            !env.names().any(|name| name.contains(TURN_KEY)),
            "the names half of the seam must be free of the value half"
        );
        // The one seam that does yield it, so the redaction above is a
        // redaction rather than a key that was never there.
        assert_eq!(
            env.get(CUSTOM_HEADERS_ENV),
            Some(format!("{TURN_KEY_HEADER}: {TURN_KEY}"))
        );
    }

    /// Every one of §1.3's five suppressing inputs is refused by name under a
    /// forwarded login.
    ///
    /// One assertion per input rather than one over the list, because what the
    /// test is for is that no arm was skipped — a loop over
    /// [`OAUTH_SUPPRESSORS`] would pass against an implementation that also
    /// looped over it and got the list wrong.
    #[test]
    fn a_forwarded_login_refuses_each_input_that_would_suppress_it() {
        let refuse = |launch: ClaudeLaunch| {
            launch
                .forwarding_claude_login()
                .env()
                .expect_err("a suppressor beside a forwarded login is refused")
        };
        for name in [
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
            API_KEY_ENV,
        ] {
            let error = refuse(launch().also_launching_with(name, "anything"));
            assert!(
                matches!(&error, ClaudeLaunchError::OauthSuppressorsPresent { names, .. }
                    if names.contains(name)),
                "`{name}` must be refused by name, got: {error}"
            );
        }
        for name in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            // These three are refused one level harder: they change which
            // provider the client selects at all, so the base URL is never read
            // and the turn never reaches this deployment.
            let error = refuse(launch().also_launching_with(name, "1"));
            assert_eq!(
                error,
                ClaudeLaunchError::RedirectDefeated { name },
                "`{name}` defeats the redirect, not only the login"
            );
        }
        // The settings key, which is the one a launcher cannot fix by clearing
        // the child's environment — hence its own site and its own input.
        let error = refuse(launch().with_settings_api_key_helper());
        assert!(
            matches!(&error, ClaudeLaunchError::OauthSuppressorsPresent { names, .. }
                if names.contains("apiKeyHelper")),
            "the settings key must be refused too, got: {error}"
        );

        // CONTROL: an unrelated variable beside a forwarded login is not a
        // refusal. Without this the rule above is indistinguishable from
        // "refuse every extra variable", which would make the type useless to a
        // launcher that has to set `PATH`.
        let fine = launch()
            .forwarding_claude_login()
            .also_launching_with("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env()
            .expect("an unrelated variable is not a suppressor");
        assert_eq!(
            fine.get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"),
            Some("1".to_string())
        );
    }

    /// The redirect-defeating three are refused under **both** kinds.
    ///
    /// The ruling names them among the five a forwarded login refuses; they are
    /// refused for the bring-your-own-key launch as well, and the reason is a
    /// different one. §1.3's `I7()` picks the provider before any credential is
    /// resolved, and a non-`firstParty` answer means [`BASE_URL_ENV`] is not read
    /// at all — so the sentinel does its job perfectly and the client still
    /// never reaches roundhouse. That failure is silent on both sides: the agent
    /// answers, from somebody else's serving plane, and no roundhouse log has a
    /// row for the turn that did not arrive.
    #[test]
    fn a_cloud_provider_selector_is_refused_even_under_a_roundhouse_key() {
        for name in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            assert_eq!(
                launch()
                    .also_launching_with(name, "1")
                    .env()
                    .expect_err("a cloud selector makes the base URL unread"),
                ClaudeLaunchError::RedirectDefeated { name }
            );
        }
        // CONTROL: the *login* suppressors are not refused here, because this
        // kind sets one of them on purpose. A rule that refused them under both
        // kinds would refuse the generator's own output.
        assert!(
            launch()
                .also_launching_with("ANTHROPIC_AUTH_TOKEN", "sk-ant-something")
                .env()
                .is_ok(),
            "a bring-your-own-key launch has no subscription login to protect"
        );
    }

    /// The list a launcher enforces matches the refusals the generator makes.
    ///
    /// Two lists that agree today and would part company on the edit that adds a
    /// sixth input, which is exactly the drift that makes an enforcement
    /// promise false without making any test red.
    #[test]
    fn must_be_unset_names_what_the_generator_would_refuse() {
        let forwarded: Vec<&str> = launch()
            .forwarding_claude_login()
            .must_be_unset()
            .iter()
            .map(|suppressor| suppressor.name)
            .collect();
        assert_eq!(forwarded.len(), OAUTH_SUPPRESSORS.len());
        assert!(forwarded.contains(&API_KEY_ENV));
        assert!(forwarded.contains(&"apiKeyHelper"));

        let byok: Vec<&str> = launch()
            .must_be_unset()
            .iter()
            .map(|suppressor| suppressor.name)
            .collect();
        assert_eq!(
            byok,
            vec![
                "CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CODE_USE_VERTEX",
                "CLAUDE_CODE_USE_FOUNDRY"
            ],
            "a bring-your-own-key launch must not ask a launcher to unset the \
             variable the generator is about to write"
        );
        // And every name a launcher is asked to unset is one it *can*: the
        // settings key is the exception, and it is marked rather than mixed in.
        for suppressor in launch().forwarding_claude_login().must_be_unset() {
            assert_eq!(
                suppressor.site == SuppressorSite::SettingsKey,
                suppressor.name == "apiKeyHelper"
            );
        }
    }

    /// A base URL that already carries the served API prefix is refused.
    ///
    /// The exact inverse of the codex sibling's `BaseUrlMissingApiPrefix`, and
    /// asserted against [`API_PREFIX`] rather than against a second `"/v1"`
    /// literal so that moving the served prefix moves this refusal with it.
    #[test]
    fn a_base_url_that_already_carries_the_api_prefix_is_refused() {
        let with_prefix = format!("https://rh.example.com{API_PREFIX}");
        assert_eq!(
            ClaudeLaunch::new(&with_prefix, TURN_KEY).expect_err("the SDK appends it itself"),
            ClaudeLaunchError::BaseUrlCarriesApiPrefix {
                base_url: with_prefix,
            }
        );
        // A trailing slash is normalised, on both the accepted and the refused
        // shape — it is what a copy-pasted address carries.
        assert_eq!(
            ClaudeLaunch::new("https://rh.example.com/", TURN_KEY)
                .expect("a trailing slash is not a mistake")
                .base_url(),
            "https://rh.example.com"
        );
        assert!(
            ClaudeLaunch::new(format!("https://rh.example.com{API_PREFIX}/"), TURN_KEY).is_err(),
            "normalising the slash must not smuggle the prefix past the refusal"
        );
        assert!(matches!(
            ClaudeLaunch::new("   ", TURN_KEY),
            Err(ClaudeLaunchError::BaseUrlIsEmpty)
        ));
        // And the URL the client will actually assemble is the one this refusal
        // is protecting, spelled once so a reader can check the two against
        // each other.
        assert_eq!(
            launch().messages_url(),
            format!("{ROOT}{API_PREFIX}/{MESSAGES_PATH}")
        );
    }

    /// Only a turn key builds a launch, and no refusal quotes the value.
    ///
    /// The admin key gets a row of its own rather than being one more wrong
    /// string: it is a real secret of this deployment's, an operator plausibly
    /// has one to hand, and it authenticates on every surface except the one
    /// this launch is for — where `turn_admission` refuses it as the wrong
    /// *kind* of key on every turn, after the client has already started. Told
    /// only "not a turn key", that operator checks their paste.
    #[test]
    fn only_a_turn_key_builds_a_launch_and_no_refusal_quotes_it() {
        assert_eq!(
            ClaudeLaunch::new(ROOT, ADMIN_KEY).expect_err("an admin key is not a turn key"),
            ClaudeLaunchError::AdminKeyIsNotATurnKey,
        );
        // A JWT and a provider key are refused by the *shape* check rather than
        // by `Secret::api_key`'s OAuth check — asserted because the two read
        // alike from outside and only one of them names the fix.
        for wrong in [
            "rh_turn_tooshort",
            "sk-ant-api03-somebody-elses",
            "eyJhbGciOiJub25lIn0.e30.x",
            "",
        ] {
            let error = ClaudeLaunch::new(ROOT, wrong).expect_err("not a turn key");
            assert_eq!(error, ClaudeLaunchError::NotATurnKey);
            // The refusal must not carry the value into whatever logs it. The
            // likeliest wrong value here is a live credential of somebody
            // else's, which is exactly the shape of the third case.
            assert!(
                wrong.is_empty() || !error.to_string().contains(wrong),
                "a refusal quoted the rejected credential: {error}"
            );
        }
    }

    /// A variable the generated map already names is a refusal, not an
    /// overwrite.
    ///
    /// Both directions of the collision are silent: an operator's own
    /// `ANTHROPIC_BASE_URL` aims the client somewhere else, and their own
    /// `ANTHROPIC_CUSTOM_HEADERS` drops the turn key — after which every turn is
    /// a `401` from a deployment the operator can see is running.
    #[test]
    fn a_variable_the_generated_map_already_names_is_refused() {
        for name in [BASE_URL_ENV, CUSTOM_HEADERS_ENV] {
            assert_eq!(
                launch()
                    .also_launching_with(name, "https://elsewhere.example.com")
                    .env()
                    .expect_err("the generated map already names it"),
                ClaudeLaunchError::CollidesWithGeneratedVar {
                    name: name.to_string(),
                }
            );
        }
        // `ANTHROPIC_API_KEY` collides under the kind that writes it and is a
        // *suppressor* refusal under the kind that does not — two different
        // sentences for two different mistakes, which is the whole reason the
        // error type has more than one variant.
        assert!(matches!(
            launch()
                .also_launching_with(API_KEY_ENV, "sk-ant-api03-mine")
                .env(),
            Err(ClaudeLaunchError::CollidesWithGeneratedVar { .. })
        ));
        assert!(matches!(
            launch()
                .forwarding_claude_login()
                .also_launching_with(API_KEY_ENV, "sk-ant-api03-mine")
                .env(),
            Err(ClaudeLaunchError::OauthSuppressorsPresent { .. })
        ));
    }

    /// The sentinel is a roundhouse value that can never be a roundhouse key.
    ///
    /// The tripwire for the one line this module and `control_config` share. If
    /// the constant ever became key-shaped, `presented_key` would take it out of
    /// an `Authorization` header and hand it to the resolver, where a hash
    /// collision is the only thing between a published literal and a membership.
    /// If it left the `rh_` namespace, the same header would be answered as "no
    /// key presented" and an operator chasing a `401` would be told to add a
    /// header they had already set.
    #[test]
    fn the_api_key_sentinel_is_namespaced_and_is_not_key_shaped() {
        assert!(ROUNDHOUSE_API_KEY_SENTINEL.starts_with("rh_"));
        assert!(
            !has_valid_key_shape(ROUNDHOUSE_API_KEY_SENTINEL),
            "the sentinel must never resolve as a key"
        );
        assert!(
            !ROUNDHOUSE_API_KEY_SENTINEL.is_empty(),
            "an empty value resolves no source"
        );
    }
}
