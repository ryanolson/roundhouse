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
//! **Three limits of that suppression, recorded rather than reconciled**,
//! because a launcher cannot lie about the environment it runs in:
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
//!    `oauth-2025-04-20` beta). Nothing this module writes changes what the
//!    client does with that variable; what it does is **refuse the launch**
//!    ([`ClaudeLaunchError::SentinelDefeated`]), because a `RoundhouseKey` launch
//!    under CCR is a `ForwardedClaudeLogin` in fact whatever it says on the tin,
//!    and the whole point of the sentinel is that the operator's choice, not the
//!    ambient environment, decides which one is running. Under
//!    [`ClaudeAuthKind::ForwardedClaudeLogin`] the same variable is harmless and
//!    is not refused — it is [`OAUTH_SUPPRESSORS`]' one entry whose refusal
//!    belongs to the *other* kind.
//! 3. **It covers one arm of `VV()`, not the wire.** The sentinel decides what
//!    the *API-key* arm resolves to and nothing else, so an ambient
//!    `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR` or
//!    `apiKeyHelper` still puts a credential of the operator's own on
//!    `Authorization`, which roundhouse's edge records as the forwarded seat —
//!    a bring-your-own-key launch spending a subscription it never mentioned
//!    (F2). Refused under **both** kinds, for reasons that differ by kind; see
//!    [`OauthSuppressor::refused_beside_the_sentinel`].
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
//! `x-nemo-relay-proxy-token` into `ANTHROPIC_CUSTOM_HEADERS` (Relay evidence
//! §A.7), which reads at first like a surface that cannot carry a key of ours.
//! It carries every one of them. ("Relay evidence §x" throughout this runbook
//! points at the 2026-09-01 addendum of
//! `agent-docs/research/nemo-relay-0.8.0-published-read.md`, where each claim
//! carries its `file:line` against the 0.8.2 tarballs — citations live there,
//! not here, so a pin bump re-verifies them in one place.)
//!
//! - the merge is a **line-wise replacement of the matching name only**
//!   (`replace_custom_header`, Relay evidence §A.7), so a [`TURN_KEY_HEADER`]
//!   line in the operator's `ANTHROPIC_CUSTOM_HEADERS` survives beside Relay's
//!   token;
//! - Relay's dispatch forwards headers it does not own untouched
//!   (`should_forward_request_header`, Relay evidence §A.13, which
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
//! (Relay evidence §A.4), so a value carrying [`API_PREFIX`] produces
//! `/v1/v1/messages`. `?beta=true` survives that concatenation (R7 hazard 3),
//! and the gateway must bind loopback — 0.8.2 refuses anything else outright
//! (Relay evidence §A.10, new at that release). `run` is the wizard-free entry
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
//!   (Relay evidence §A.13), and [`ROUNDHOUSE_API_KEY_SENTINEL`] on
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
//!   whenever the base URL is changed by a *different* layer (Relay evidence
//!   §A.5 — the function body is unchanged from 0.8.0), so
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
//!   a turn to an explicit target (Relay evidence §A.6), so such a turn
//!   arrives carrying no forwarded seat whatever the client presented. Also a
//!   refusal rather than a guard, and for the same reason.
//! - **Resumption is not offered in band on this surface.** Relay's SSE decoder
//!   ignores `id:` lines outright (Relay evidence §A.3), so a cursor
//!   carried as an SSE id would not survive the hop; the Messages emitter
//!   carries none, and plan open question 4 is closed that way for this rung.
//!
//! Verified against `claude-cli 2.1.257`. The captures the reasoning above rests
//! on are committed at
//! `crates/roundhouse-server/tests/fixtures/claude-2.1.257-*.json`, and the
//! gated real-binary suite that drives the actual client with this map is
//! `crates/roundhouse-server/tests/claude_e2e.rs`.

/// The evidence half: the §1.3 table, and which launch each row is fatal to.
/// Split out because a client-surface re-capture edits the table and nothing
/// else, and because it is the one part of this module that is claims about
/// somebody else's binary rather than mechanism of ours.
pub mod suppressors;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;

use roundhouse_core::control::{CredentialError, Secret};

use crate::control_config::{KeyKind, TURN_KEY_HEADER, has_valid_key_shape};
use crate::messages_api::MESSAGES_PATH;
use crate::responses_api::API_PREFIX;
use suppressors::quoted_names;

pub use crate::control_config::ROUNDHOUSE_API_KEY_SENTINEL;
/// Re-exported at the module root so `claude_launch::OAUTH_SUPPRESSORS` keeps
/// naming the table after the split — the launcher crate and the e2e suite reach
/// for it by that path, and a move that renames a public path is a change to the
/// hook-up surface rather than to this file's shape.
pub use suppressors::{Defeats, OAUTH_SUPPRESSORS, OauthSuppressor, SuppressorSite};

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
/// A refusal per failure rather than one bucket, because the failures they
/// prevent look nothing alike from the operator's chair — and the operator's
/// next move differs with them. Each names what the *client* does with the bad
/// input, not what this module wanted instead.
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
    /// **The suppressors themselves, not a rendered list of their names**, and
    /// the difference is one a caller cannot recover afterwards. A joined
    /// `String` answers "does some suppressor whose name contains this text
    /// appear" where the question is "did *this* suppressor fire", and one real
    /// entry's name is a prefix of nothing less than another plausible spelling
    /// (`CLAUDE_CODE_API_KEY` is not a suppressor at all;
    /// `CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR` is), so the two answers differ on
    /// live data (F9). Rendering is [`Display`](std::fmt::Display)'s job, from
    /// the same list [`ClaudeLaunch::must_be_unset`] returns.
    #[error(
        "this launch also sets {}, which suppress{} the subscription login \
         `ForwardedClaudeLogin` exists to forward. Each one leaves every request valid, so the \
         run looks healthy and the seat simply never arrives; unset them at launch \
         (`ClaudeLaunch::must_be_unset`) rather than launching and reading the routing decisions",
        quoted_names(.suppressors),
        if .suppressors.len() == 1 { "es" } else { "" },
    )]
    OauthSuppressorsPresent {
        suppressors: Vec<&'static OauthSuppressor>,
    },
    /// The same inputs as [`Self::OauthSuppressorsPresent`], under the other
    /// kind and costing something else — which is why it is a second variant
    /// and not a second reading of the first. There the operator has two
    /// credentials and must choose; here they have one they did not know was in
    /// play, and the remedy is to unset it rather than to decide between them.
    #[error(
        "this launch also sets {}, which put{} a credential of the operator's own on the wire \
         beside the `{API_KEY_ENV}` sentinel. The sentinel decides what the client's API-key arm \
         resolves to and nothing else, so that credential is presented on `Authorization` and \
         roundhouse records it as the forwarded seat -- a `RoundhouseKey` launch promises a turn \
         key and nothing else, and this one spends a subscription while every turn still answers",
        quoted_names(.suppressors),
        if .suppressors.len() == 1 { "s" } else { "" },
    )]
    CredentialBesideTheSentinel {
        suppressors: Vec<&'static OauthSuppressor>,
    },
    #[error(
        "this launch sets `{name}`, which is the one input that turns off the `{API_KEY_ENV}` \
         sentinel a `RoundhouseKey` launch depends on -- the client's API-key arm is gated on it \
         being unset. The sentinel then suppresses nothing, an ambient subscription login is \
         presented to this deployment instead, and roundhouse receives a real seat from an \
         operator who chose bring-your-own-key and never said so"
    )]
    SentinelDefeated { name: &'static str },
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
#[derive(Clone)]
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

/// Hand-written for one field, and it is [`Self::also`].
///
/// `Secret`'s own `Debug` covers the turn key, so a derive would look correct —
/// and would print an operator's `ANTHROPIC_AUTH_TOKEN` verbatim, because
/// `also` holds `String`s this module cannot classify (F16, and see
/// [`LaunchValue::Declared`] for the same argument on the rendered map). A
/// launcher prints this type while debugging; the promise the module doc makes
/// about `Debug` has to hold for the whole struct or it is not a promise.
impl fmt::Debug for ClaudeLaunch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeLaunch")
            .field("base_url", &self.base_url)
            .field("turn_key", &self.turn_key)
            .field("auth", &self.auth)
            .field(
                "also",
                &self
                    .also
                    .keys()
                    .map(|name| (name, REDACTED_VALUE))
                    .collect::<BTreeMap<_, _>>(),
            )
            .field("api_key_helper", &self.api_key_helper)
            .finish()
    }
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
    /// Kind-dependent, and the asymmetry is the point — but it is an asymmetry
    /// and not a subset. [`ClaudeAuthKind::ForwardedClaudeLogin`] owes every
    /// input that turns the forwarding off while leaving the run looking
    /// healthy; [`ClaudeAuthKind::RoundhouseKey`] owes everything except
    /// [`API_KEY_ENV`] — the three that defeat the *redirect*, the one that
    /// defeats its own sentinel, and the three that put a second credential on
    /// the wire beside it ([`OauthSuppressor::refused_beside_the_sentinel`]) —
    /// and owes that one exception because it deliberately sets the variable
    /// itself: listing it here would be asking a launcher to unset what the
    /// generator is about to write. Which side each input falls on is
    /// [`OauthSuppressor::refused_under`], read from the table.
    pub fn must_be_unset(&self) -> Vec<&'static OauthSuppressor> {
        OAUTH_SUPPRESSORS
            .iter()
            .filter(|suppressor| suppressor.refused_under(self.auth))
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

        let mut offending: Vec<&'static OauthSuppressor> = Vec::new();
        let mut beside_the_sentinel: Vec<&'static OauthSuppressor> = Vec::new();
        for suppressor in self.must_be_unset() {
            let present = match suppressor.site {
                SuppressorSite::EnvVar => self.also.contains_key(suppressor.name),
                SuppressorSite::SettingsKey => self.api_key_helper,
            };
            if !present {
                continue;
            }
            // Three refusals rather than one bucket, because the operator's next
            // move differs: an `ANTHROPIC_AUTH_TOKEN` beside a forwarded login is
            // a choice between two credentials, a `CLAUDE_CODE_USE_VERTEX` is a
            // client that never arrives, and a `CLAUDE_CODE_REMOTE` is a launch
            // that reaches roundhouse on a credential the operator did not
            // choose to hand over. Folding the last two into the first would
            // print a sentence about a subscription login that the failing
            // launch has none of.
            match suppressor.defeats {
                Defeats::TheRedirect => {
                    return Err(ClaudeLaunchError::RedirectDefeated {
                        name: suppressor.name,
                    });
                }
                Defeats::TheApiKeySentinel => {
                    return Err(ClaudeLaunchError::SentinelDefeated {
                        name: suppressor.name,
                    });
                }
                // The one input class whose cost depends on the kind rather
                // than on the input: under a forwarded login it replaces the
                // login being forwarded, and under a roundhouse key it adds a
                // credential beside the sentinel that the edge then reads as
                // the seat. Same rows, two sentences, because the operator's
                // next move is "choose one" in the first case and "unset the
                // one you did not know was set" in the second.
                Defeats::TheSubscriptionLogin if self.auth == ClaudeAuthKind::RoundhouseKey => {
                    beside_the_sentinel.push(suppressor)
                }
                Defeats::TheSubscriptionLogin => offending.push(suppressor),
            }
        }
        if !beside_the_sentinel.is_empty() {
            return Err(ClaudeLaunchError::CredentialBesideTheSentinel {
                suppressors: beside_the_sentinel,
            });
        }
        if !offending.is_empty() {
            return Err(ClaudeLaunchError::OauthSuppressorsPresent {
                suppressors: offending,
            });
        }

        for (name, value) in &self.also {
            if vars.contains_key(name) {
                return Err(ClaudeLaunchError::CollidesWithGeneratedVar { name: name.clone() });
            }
            vars.insert(name.clone(), LaunchValue::Declared(value.clone()));
        }
        Ok(ClaudeEnv { vars })
    }
}

/// One entry of a generated environment, and whether it carries a secret.
///
/// An enum rather than a `String`, so a `Debug` of the map cannot print the turn
/// key. The secret arm keeps its non-secret prefix separate — the header *name*
/// is exactly what a reader of a redacted map needs to see, and concatenating it
/// into the `Secret` would hide it along with the key.
///
/// Three arms rather than two, and the third is the one this module knows least
/// about: [`Self::Declared`] holds a value the *launcher* supplied through
/// [`ClaudeLaunch::also_launching_with`], which this module cannot classify —
/// `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` and an operator's own
/// `ANTHROPIC_AUTH_TOKEN` arrive through the same call. Printing the ones we
/// happen to recognise would be a redaction that fails on precisely the value
/// nobody anticipated (F16), so the arm redacts all of them and shows the name,
/// which is the half a launcher's `Debug` is read for.
#[derive(Clone)]
enum LaunchValue {
    Public(String),
    Declared(String),
    Secret { prefix: String, secret: Secret },
}

impl LaunchValue {
    fn reveal(&self) -> String {
        match self {
            LaunchValue::Public(value) | LaunchValue::Declared(value) => value.clone(),
            LaunchValue::Secret { prefix, secret } => format!("{prefix}{}", secret.reveal()),
        }
    }
}

/// What a redacted value renders as: enough to see the variable is set, and
/// nothing about what it is set to.
///
/// `pub` because `topham` renders the same redaction in its own plan output and
/// had restated the literal to do it (F14). One spelling, so the two cannot
/// come to disagree about what "redacted" looks like on a screen an operator
/// reads as one page.
pub const REDACTED_VALUE: &str = "<set>";

impl fmt::Debug for LaunchValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LaunchValue::Public(value) => write!(f, "{value:?}"),
            LaunchValue::Declared(_) => write!(f, "\"{REDACTED_VALUE}\""),
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
