// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What an operator hands a Codex client so that client hooks up to this
//! deployment without being modified.
//!
//! Two files come out of here: the `config.toml` a client reads out of its
//! `CODEX_HOME`, and the model catalog that `config.toml` points at. Nothing
//! else about the client changes — no wrapper, no patched binary, no forked
//! provider — which is the whole of "transparently" in the product sentence.
//!
//! Since M10.1 there is an **optional** third output, in [`skills`]: files that
//! tell the *model* what roundhouse's MCP tools are for. Optional because they
//! add no capability — a client without them still reaches every tool — and
//! separate because they are the only generated files whose audience is the
//! model rather than the client. See that module for why they are skills and
//! not the `prompts/` directory the plan named.
//!
//! **Which topology this is.** The *Direct* one — an agent pointed straight at
//! roundhouse — and it is the reference by ruling rather than by default.
//! `agent-docs/synergies/ecosystem-round-2.md`'s launch-surface dedup found
//! three implementations of one surface (Relay's Rust launchers, Switchyard's
//! Python launcher, this) and kept Direct as the reference with roundhouse
//! generating its own minimal config, because the one test M9 exists for — a
//! real codex binary executing our synthetic tool call — cannot be delegated
//! to a launcher we do not own. Relay's CLI stays the supported instrumented
//! front end for the *chained* topology; Switchyard's launcher is reference
//! and evidence — its `caller_auth_kind` conditional is exactly what
//! [`CodexAuthKind`] mirrors — and not a blessed front end.
//!
//! **Two lines here look like belt-and-braces and are not.** Both are facts
//! about `codex-cli 0.146.0` that a later client could change, and deleting
//! either re-opens a failure that is silent in the dangerous direction:
//!
//! - `env_key` is written *beside* `requires_openai_auth = false`, never
//!   instead of it. At 0.146.0 the flag being `false` suppresses nothing —
//!   `model-provider/src/auth.rs::resolve_provider_auth` (auth.rs:179-197
//!   @ `e363b08`), which every live request reaches, never reads the flag at
//!   all. It asks two questions in order: is there an `env_key` (or an
//!   `experimental_bearer_token`), and failing that, does the auth manager hold
//!   a `CodexAuth` — the thing a completed `codex login` persists to
//!   `auth.json` in `CODEX_HOME`. So a provider with no `env_key` resolves to
//!   *whatever that `CODEX_HOME` was last logged into*, aimed at **our**
//!   `base_url`; and if it was never logged into, to
//!   `unauthenticated_auth_provider()` — no `Authorization` header at all. Both
//!   halves are silent, in opposite directions, which is why the deterministic
//!   answer is written rather than left to the ambient state. The flag says
//!   what this deployment is; `env_key` is what makes the resolution
//!   deterministic. (F08 sharpened this: the earlier text named only the
//!   ambient-credential half and read as if a logged-out client were safe.)
//! - `model_catalog_json` is written under **both** auth kinds, not only the
//!   forwarding one. The `GET {base_url}/models` fetch is gated on the ambient
//!   auth mode in `CODEX_HOME` rather than on `requires_openai_auth`
//!   (`models-manager/src/manager.rs:413-417`,
//!   `model-provider/src/models_endpoint.rs:67-72` @ `e363b08`), so a
//!   bring-your-own-key client can fetch too. A pinned catalog does not
//!   short-circuit that request — it swaps in a static manager, so there is no
//!   network path at all — which is why it is the answer for both rather than
//!   a `/v1/models` route roundhouse would have to serve.
//!
//! **Why this lives in the server crate.** The stanza needs four things at
//! once: the address this deployment is bound to, the turn-key header name
//! ([`TURN_KEY_HEADER`]), the MCP mount path ([`MCP_MOUNT_PATH`]) and the
//! prefix the Responses API is served at ([`API_PREFIX`] — the fourth since
//! F14, which is what `base_url` has to end in and what the MCP url is
//! recovered by stripping). This crate is the only place that already knows
//! all four. `control_config` is the wrong half of
//! the same crate — it *reads* the file an operator wrote, and this *writes*
//! the file a client will read; putting a TOML emitter beside
//! `ControlPlaneConfig::validate` would invite reading the two as a round trip,
//! which they are not.
//!
//! **Why the TOML is hand-templated rather than serialized from a struct.**
//! Every stanza below carries a comment saying what it costs to get wrong, and
//! those comments are the reason a generated config is worth reading at all.
//! `toml::Serializer` would drop all of them and reorder the tables into
//! whatever the struct declaration happened to be. `toml` is still a
//! dependency, used for the two jobs a template cannot do safely: quoting a
//! free string, and parsing the result back in the tests below so a
//! hand-written template cannot ship syntactically broken.
//!
//! **The secret is never in the file.** Both auth kinds name an *environment
//! variable*; the turn key travels in the client's environment at launch. A
//! generator that took the secret would put a `rh_turn_…` into a file that
//! ends up in a dotfile repo, and nothing downstream could tell that copy from
//! a live one.
//!
//! Verified against `codex-cli 0.146.0` by
//! `crates/roundhouse-server/tests/codex_e2e.rs`, which writes these two files
//! into a hermetic `CODEX_HOME` and drives the real binary against a real
//! roundhouse.

pub mod skills;

use std::path::Path;

pub use skills::{GeneratedFile, SKILLS_DIR, namespaced_tool_name, skill_files};

use crate::control_config::TURN_KEY_HEADER;
use crate::dialect::DEFAULT_MCP_NAMESPACE;
use crate::mcp_api::MCP_MOUNT_PATH;
use crate::responses_api::API_PREFIX;

/// The environment variable a generated config names by default.
pub const DEFAULT_KEY_ENV: &str = "ROUNDHOUSE_API_KEY";

/// The model slug a generated config names by default.
///
/// **Deliberately not a real OpenAI slug, and that is a safety property rather
/// than a naming preference.** `/v1/responses` accepts `model` and ignores it —
/// v1 chooses its target by routing policy — so the slug is free on our side.
/// On the client's side it is not: naming a real slug can resolve metadata with
/// `use_responses_lite: true`, which puts `ResponseItem::AdditionalTools` into
/// `input`, and that is an item type this surface refuses with a 422.
pub const DEFAULT_MODEL_SLUG: &str = "roundhouse-local";

/// The provider table key, which is also the value of `model_provider`.
///
/// One constant for both because they are the same identifier in two places:
/// codex resolves `model_provider` as a key into `[model_providers.*]`, and a
/// pair that disagreed would fail at startup with a message about an unknown
/// provider rather than about a typo.
const PROVIDER_KEY: &str = "roundhouse";

/// The prefix codex puts in front of an MCP server's table key to build the
/// tool namespace it dispatches on (`mcp__{key}`).
///
/// Here rather than in [`crate::dialect`] because it is a fact about the
/// *client*: this crate's [`DEFAULT_MCP_NAMESPACE`] is the whole namespace, and
/// the server table key is what has to be written so codex reconstructs it. The
/// unit test below pins the two together, which is the only place the
/// reconstruction is checkable without a running agent.
const MCP_NAMESPACE_PREFIX: &str = "mcp__";

/// What `[mcp_servers.<key>].default_tools_approval_mode` is set to, and the
/// home of the ruling on why it is server-wide rather than per tool.
///
/// A constant rather than a literal inside the template because the review
/// (F01) proposed replacing it with `[mcp_servers.<key>.tools.fetch_steer]
/// approval_mode = "approve"` — grant the one read, leave the seven writers
/// "to the client" — and that remedy was **refused**. The per-tool table is
/// real (`McpServerToolConfig::approval_mode`, `config/src/mcp_types.rs:54-61`
/// @ pin `6344a65`) and does take precedence
/// (`McpServerMetadata::tool_approval_mode`, `codex-mcp/src/server.rs:249-255`
/// @ `e363b08`: the per-tool entry, then the server default, then
/// `AppToolApproval::default()` = `Auto`, `config/src/mcp_types.rs:19-26`), so
/// the mechanism the finding described exists. Two reasons not to use it, and
/// the second is the one that survives:
///
/// 1. As the finding was written, it broke the writers. Dropping the server
///    default puts the other seven tools on `Auto`, which decides from
///    annotations alone (`requires_mcp_tool_approval`,
///    `core/src/mcp_tool_call.rs:2155-2173` @ `e363b08`), and at the time the
///    tools shipped `annotations: None` — so `destructive_hint.unwrap_or(true)
///    || open_world_hint.unwrap_or(true)` said "needs approval". `codex exec`
///    forces `approval_policy = never` (`exec/src/lib.rs:427`), under which an
///    approval nobody can be asked for resolves to *cancelled*, not deferred:
///    `prefer` and `set_quality_floor` would have failed permanently rather
///    than asked permanently.
/// 2. F06 then made the tools annotate themselves truthfully
///    (`roundhouse-mcp/src/tools.rs`: `destructive_hint: false`,
///    `open_world_hint: false` on all eight), which flips that same `Auto`
///    branch to "no approval needed". So the per-tool scoping would no longer
///    break anything — and no longer buy anything either. The narrowing it was
///    reaching for now lives one layer down, in the annotations, where it holds
///    for a client roundhouse never generated a file for. What is left for this
///    line is the deployment-side belt: it is what still admits a tool whose
///    annotations are ever wrong, missing, or dropped by an rmcp upgrade —
///    which is precisely what the eight-tools tripwire below now demands
///    somebody re-derive before the count moves.
const TOOLS_APPROVAL_MODE: &str = "approve";

/// How many tokens the generated catalog claims the model's context window is.
///
/// A stated number rather than `null`, because the client accumulates the usage
/// roundhouse reports into a session total and compares it against this — so a
/// deployment reading the §10.2 usage evidence needs to know what the client
/// was measuring against. Matches the figure 0.146.0's own fallback metadata
/// uses, so pinning the catalog does not silently change the client's
/// compaction arithmetic relative to an unpinned run.
pub const CONTEXT_WINDOW_TOKENS: u64 = 272_000;

/// How the client authenticates to roundhouse.
///
/// Two kinds because there are two deployments, not because there is a
/// preference: a client that has its own roundhouse key, and a client whose
/// user is logged into ChatGPT and whose login roundhouse forwards upstream.
/// The difference is one flag and one line, and getting it wrong is silent in
/// the dangerous direction — see the variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexAuthKind {
    /// The client holds a roundhouse turn key and nothing else.
    ///
    /// `requires_openai_auth = false` **with** `env_key`. Never without: at
    /// 0.146.0 the flag does not gate what the auth manager resolves, so a
    /// provider with no `env_key` sends whatever login the client's
    /// `CODEX_HOME` happens to hold to *our* `base_url` — or, if it holds
    /// none, no `Authorization` at all. `env_key` is what makes the resolution
    /// deterministic in both cases.
    RoundhouseKey,
    /// The client's ChatGPT login is forwarded upstream by roundhouse.
    ///
    /// `requires_openai_auth = true` and **no** `env_key`, so `Authorization`
    /// carries the client's own bearer. Roundhouse's key then has to arrive
    /// somewhere else, which is what `env_http_headers` is for — see
    /// [`TURN_KEY_HEADER`].
    ///
    /// **The precondition is a completed `codex login`, not this flag.** The
    /// flag chooses a code path — `provider_uses_first_party_auth_path`
    /// (`model-provider/src/provider.rs:223-229` @ `e363b08`) is the only
    /// production reader — and both of the paths it chooses between end in
    /// `resolve_provider_auth`, which builds the header from the auth
    /// manager's cached `CodexAuth` and nothing else. That cache is populated
    /// only by `codex login` writing `auth.json` into this `CODEX_HOME`
    /// (`login/src/auth/manager.rs`). Skip the login and the request arrives
    /// with no `Authorization` at all — which roundhouse *admits*, because
    /// `control_config::turn_admission` treats "the caller presented nothing"
    /// as a first-class case and degrades the turn to local-only rather than
    /// rejecting it (`control_config/crosscheck.rs`'s `withheld_providers`).
    /// Nothing in the run then looks broken: turns answer, frontier routes
    /// simply never happen. Naming the login in the generated file is the only
    /// place an operator can learn this before spending a day on it.
    ForwardedOpenAiLogin,
}

/// Why a launch config could not be built from the inputs it was given.
///
/// Three refusals rather than one, because the three failures they prevent look
/// nothing alike from the operator's chair and a single "bad launch config"
/// would send whoever reads it to the wrong file. Each names what codex does
/// with the value, not what this crate wanted instead.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodexLaunchError {
    #[error(
        "the model catalog path `{path}` is relative. Codex resolves \
         `model_catalog_json` against the directory it loaded config.toml from -- not the \
         directory roundhouse was run in -- so a relative path names a file that is not \
         there, and the client falls back to invented model metadata instead of erroring"
    )]
    RelativeCatalogPath { path: String },
    #[error(
        "the model catalog path is not valid UTF-8 (lossily: `{lossy}`). TOML holds UTF-8 \
         strings only, so writing it means writing `Path::display()`'s substituted \
         replacement characters -- a different path from the one on disk, with nothing \
         anywhere saying a substitution happened"
    )]
    NonUtf8CatalogPath { lossy: String },
    #[error(
        "the base URL `{base_url}` does not end in `{API_PREFIX}`, which is where this \
         deployment serves the Responses API. Codex posts turns to `{{base_url}}/responses`, \
         so this stanza 404s on every turn -- while the MCP handshake, derived from the same \
         string, still succeeds and makes the client look healthy"
    )]
    BaseUrlMissingApiPrefix { base_url: String },
}

/// Everything a generated launch config depends on.
///
/// Plain fields with a constructor that fills the two defaults, rather than a
/// builder: there are five inputs, two of them have one sensible value, and a
/// builder would make the two that must never be defaulted — the address and
/// the catalog path — look optional.
///
/// **What [`CodexLaunch::new`] checking its inputs does and does not buy.** The
/// fields stay `pub` and are still writable after construction, so this is a
/// check at the door rather than an invariant the type carries; `non_exhaustive`
/// only closes the *other* door, the struct literal an outside crate would
/// otherwise use to skip `new` entirely. That is enough because the three
/// checked values have no builder that touches them —
/// [`Self::with_key_env`], [`Self::with_model`] and
/// [`Self::forwarding_openai_login`] each move a different field — so the only
/// way past the check is to assign `base_url` or `model_catalog_path` by hand,
/// which reads as deliberate. Private fields with five accessors would make the
/// guarantee total and would cost every caller a method call for a field they
/// currently read; the honest description of what is here is "the constructor
/// refuses the three shapes that fail silently", and it is written down rather
/// than implied.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CodexLaunch {
    /// Where this deployment serves the Responses API, including the
    /// [`API_PREFIX`] suffix — exactly what goes into `base_url`. Any trailing
    /// slash is normalised away by [`CodexLaunch::new`].
    pub base_url: String,
    /// The environment variable the client's turn key arrives in.
    pub key_env: String,
    /// How the client authenticates. See [`CodexAuthKind`].
    pub auth: CodexAuthKind,
    /// The slug written into `model`. See [`DEFAULT_MODEL_SLUG`].
    pub model: String,
    /// Where [`Self::model_catalog_json`]'s output will be written, as the
    /// client will see it.
    ///
    /// Absolute and UTF-8, because codex resolves a relative
    /// `model_catalog_json` against the directory the config was loaded from —
    /// which is correct, and not the directory this process ran in.
    /// [`CodexLaunch::new`] refuses both other shapes.
    pub model_catalog_path: String,
}

impl CodexLaunch {
    /// A bring-your-own-key launch against `base_url`, with the defaults.
    ///
    /// Fallible (F13) because each of the three refusals below produced a
    /// config that *loads*: the client starts, half of it works, and the half
    /// that does not fails somewhere the operator would look at roundhouse for.
    /// A trailing slash on `base_url` is normalised rather than refused —
    /// it is what a copy-pasted address carries, it has one unambiguous
    /// meaning, and refusing it would teach nobody anything. The other three
    /// have no unambiguous reading, so they are refused by name.
    pub fn new(
        base_url: impl Into<String>,
        model_catalog_path: &Path,
    ) -> Result<Self, CodexLaunchError> {
        let base_url = base_url.into();
        let base_url = base_url.trim_end_matches('/').to_string();
        if !base_url.ends_with(API_PREFIX) {
            return Err(CodexLaunchError::BaseUrlMissingApiPrefix { base_url });
        }
        // `to_str` before `is_absolute`: a non-UTF-8 path can be absolute, and
        // reporting "relative" for it would name the wrong problem.
        let catalog =
            model_catalog_path
                .to_str()
                .ok_or_else(|| CodexLaunchError::NonUtf8CatalogPath {
                    lossy: model_catalog_path.display().to_string(),
                })?;
        if !model_catalog_path.is_absolute() {
            return Err(CodexLaunchError::RelativeCatalogPath {
                path: catalog.to_string(),
            });
        }
        Ok(Self {
            base_url,
            key_env: DEFAULT_KEY_ENV.to_string(),
            auth: CodexAuthKind::RoundhouseKey,
            model: DEFAULT_MODEL_SLUG.to_string(),
            model_catalog_path: catalog.to_string(),
        })
    }

    /// The same, forwarding the client's own ChatGPT login upstream.
    pub fn forwarding_openai_login(mut self) -> Self {
        self.auth = CodexAuthKind::ForwardedOpenAiLogin;
        self
    }

    /// Name a different environment variable for the turn key.
    pub fn with_key_env(mut self, key_env: impl Into<String>) -> Self {
        self.key_env = key_env.into();
        self
    }

    /// Name a different model slug. See [`DEFAULT_MODEL_SLUG`] first.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Where the client's MCP registration should point.
    pub fn mcp_url(&self) -> String {
        mcp_endpoint(&self.base_url)
    }

    /// The `config.toml` a client reads out of its `CODEX_HOME`.
    pub fn config_toml(&self) -> String {
        let model = quote(&self.model);
        let provider = quote(PROVIDER_KEY);
        let catalog = quote(&self.model_catalog_path);
        let base_url = quote(&self.base_url);
        let key_env = quote(&self.key_env);
        let header = quote(TURN_KEY_HEADER);
        let mcp_url = quote(&self.mcp_url());
        let server_key = mcp_server_key();

        // `requires_openai_auth` and `env_key` move together, which is why they
        // are formatted as one block rather than two lines with an `if` around
        // the second: the invalid combination is `true` beside an `env_key`,
        // where codex resolves the env key first and the forwarded login is
        // silently never used.
        let auth = match self.auth {
            CodexAuthKind::RoundhouseKey => format!(
                "# This client holds its own roundhouse key. `env_key` is what makes the\n\
                 # credential resolution deterministic: without it, codex 0.146.0 attaches\n\
                 # whatever ambient login sits in CODEX_HOME to this base_url.\n\
                 requires_openai_auth = false\n\
                 env_key = {key_env}\n"
            ),
            CodexAuthKind::ForwardedOpenAiLogin => concat!(
                "# This client's own ChatGPT login rides `Authorization` and roundhouse\n",
                "# forwards it upstream. No `env_key` here, deliberately: codex resolves an\n",
                "# env key ahead of the login, so one would disable the forwarding this\n",
                "# stanza exists for -- silently, since both produce a valid request.\n",
                "#\n",
                "# PRECONDITION: run `codex login` against this CODEX_HOME before launching.\n",
                "# The flag below only selects a code path; the Authorization header itself\n",
                "# comes from the login codex persisted to auth.json in this CODEX_HOME, and\n",
                "# from nothing else. Skip the login and every request arrives with no\n",
                "# Authorization at all -- which roundhouse admits and quietly degrades to\n",
                "# local-only routing rather than refusing, so turns keep answering and no\n",
                "# frontier route ever happens. Nothing in the run reports this.\n",
                "requires_openai_auth = true\n",
            )
            .to_string(),
        };

        format!(
            "# Generated by roundhouse (`roundhouse_server::codex_launch`). Everything here\n\
             # is what makes an unmodified Codex drive roundhouse: the provider it posts to,\n\
             # the header its turn key rides in, and the MCP surface it dispatches roundhouse's\n\
             # own tool calls back to. No secret is in this file -- the key travels in the\n\
             # environment variable named below.\n\
             \n\
             # Accepted and ignored by roundhouse: /v1/responses chooses its target by routing\n\
             # policy, not by requested model. It still matters on this side -- a real OpenAI\n\
             # slug resolves metadata that puts item types roundhouse refuses into the request.\n\
             model = {model}\n\
             model_provider = {provider}\n\
             # Pinned rather than fetched. With a catalog on disk the client uses a static\n\
             # models manager with no network path at all; without one it may issue\n\
             # `GET {{base_url}}/models` -- gated at 0.146.0 on the ambient auth mode in\n\
             # CODEX_HOME rather than on `requires_openai_auth`, so it applies to both kinds\n\
             # of stanza and not only to the forwarded one.\n\
             model_catalog_json = {catalog}\n\
             \n\
             [model_providers.{PROVIDER_KEY}]\n\
             # Never \"OpenAI\": codex matches this *name* (not the table key) to decide\n\
             # whether to attach its routing-hint header, use remote compaction, and zstd-\n\
             # compress the request body. Roundhouse serves none of the three.\n\
             name = \"Roundhouse\"\n\
             base_url = {base_url}\n\
             wire_api = \"responses\"\n\
             # Roundhouse serves SSE over POST and no websocket upgrade.\n\
             supports_websockets = false\n\
             {auth}\
             \n\
             # Roundhouse's own turn key, in a header of its own. Required for the forwarded\n\
             # login (where `Authorization` belongs to the upstream) and harmless beside a\n\
             # roundhouse key, where both headers carry the same secret and the surface\n\
             # captures neither.\n\
             [model_providers.{PROVIDER_KEY}.env_http_headers]\n\
             {header} = {key_env}\n\
             \n\
             # The control surface. The table key is load-bearing: codex builds the tool\n\
             # namespace as `{MCP_NAMESPACE_PREFIX}<key>`, and roundhouse emits its synthetic calls under\n\
             # `{DEFAULT_MCP_NAMESPACE}` -- a different key here makes every steer resolve\n\
             # against nothing and come back to the model as an unsupported call.\n\
             [mcp_servers.{server_key}]\n\
             url = {mcp_url}\n\
             bearer_token_env_var = {key_env}\n\
             # Roundhouse's own tools run without asking the operator first, and that is a\n\
             # property of what they do rather than a convenience. `fetch_steer` reads back the\n\
             # correction this same deployment just emitted; the writing tools only ever narrow\n\
             # what the caller's own key already allows, never widen it. The tools say so\n\
             # themselves, in their MCP annotations, so a client that was handed no config at\n\
             # all already runs them. This line is the belt for the one that was: codex 0.146.0\n\
             # treats a tool it sees *no* annotations on as needing approval, and under\n\
             # `approval_policy = never` -- which `codex exec` forces -- an approval nobody\n\
             # can be asked for resolves to *cancelled*: the agent is handed a cancellation\n\
             # notice in place of the steer, and roundhouse's correction never arrives.\n\
             default_tools_approval_mode = \"{TOOLS_APPROVAL_MODE}\"\n\
             \n\
             [features]\n\
             # Agent identity is an OpenAI-backend credential mode; roundhouse authenticates\n\
             # by turn key and would see a credential it has no use for.\n\
             use_agent_identity = false\n"
        )
    }

    /// The model catalog `config.toml` points at.
    ///
    /// Written against the schema of the binary that reads it, not of the
    /// codex crates this workspace pins for wire conformance: the two are not
    /// ancestors of each other and `ModelInfo` differs between them. Unknown
    /// keys are ignored by the reader and missing required ones are a hard
    /// config-load error, so over-specifying is the safe direction.
    pub fn model_catalog_json(&self) -> String {
        let entry = serde_json::json!({
            "slug": self.model,
            "display_name": "Roundhouse",
            "description": "Routed by roundhouse: the target is chosen per turn, not by this slug.",
            "supported_reasoning_levels": [],
            // Decides which shell tool the client advertises. `shell_command` is
            // the plain-string form; the alternative (`unified_exec`) advertises
            // a different pair of tools. Stated rather than defaulted because a
            // catalog that omits it does not load at all.
            "shell_type": "shell_command",
            "visibility": "list",
            "supported_in_api": true,
            "priority": 1,
            "upgrade": null,
            "base_instructions": "You are running against roundhouse, which routes each turn to \
                                  the model it judges best for the work. Answer the task.",
            "model_messages": null,
            "default_reasoning_summary": "auto",
            "support_verbosity": false,
            "default_verbosity": null,
            "apply_patch_tool_type": null,
            "truncation_policy": { "mode": "bytes", "limit": 10_000 },
            "supports_parallel_tool_calls": false,
            "supports_image_detail_original": false,
            "context_window": CONTEXT_WINDOW_TOKENS,
            // Left off on purpose: roundhouse reports the *judge's* usage on a
            // steered turn, and a compaction limit would make the client rewrite
            // its own history off the back of a number that describes a side
            // call. See the plan's §10.2.
            "auto_compact_token_limit": null,
            "effective_context_window_percent": 95,
            "experimental_supported_tools": [],
            // Stated `false` although `false` is also what omitting it yields
            // (`supports_search_tool` is `#[serde(default)]`,
            // `protocol/src/openai_models.rs:434-435` @ `e363b08`). The point
            // is not the value, it is that the value has to be overwritten
            // deliberately: this single field is the only gate on whether the
            // client offers the model a `tool_search` tool at all
            // (`search_tool_enabled` = `supports_search_tool &&
            // namespace_tools_enabled`, `core/src/tools/spec_plan.rs:333-335`
            // @ `e363b08`, and `namespace_tools` defaults *true* for every
            // provider including this one). Turn it on and the very next turn
            // can resend a `tool_search_call` / `tool_search_output` pair,
            // which `responses_api::wire::canonical_item` refuses with a 422 —
            // taking the whole turn down. F18 named the way that happens by
            // accident: authoring this catalog by copying upstream per-model
            // metadata, where a flagship's entry carries `true`. An absent key
            // is silent about that; a written `false` is a line the copy has to
            // argue with.
            "supports_search_tool": false,
            // Turned on for the surface [`skills`] emits, and stated even on a
            // deployment that emits none — the field costs nothing when the
            // `skills` directory is empty, because the whole block it gates is
            // skipped when there are no skills to list
            // (`core/src/session/mod.rs:3350-3380` @ `e363b08`).
            //
            // What it buys when there are: the listing the model always gets is
            // `- {name}: {description} (file: {path})` and nothing more
            // (`core-skills/src/render.rs:520-532`). With this `false` — which
            // is the reader's `#[serde(default)]`,
            // `protocol/src/openai_models.rs:392-393` — the model is handed
            // three file paths and no instruction that reading one is how a
            // skill is used, so the most likely outcome is that it treats the
            // list as trivia. `true` appends codex's own "How to use skills"
            // protocol, whose first rule is to read the `SKILL.md` completely
            // before acting on it. Written here rather than left to the default
            // for `supports_search_tool`'s reason one field up: an absent key
            // is silent about a decision that was made.
            "include_skills_usage_instructions": true,
        });
        serde_json::to_string_pretty(&serde_json::json!({ "models": [entry] }))
            .expect("a catalog built from literals encodes")
    }
}

/// The MCP endpoint that belongs to a Responses `base_url`.
///
/// A named function with its own tests because the two URLs are *not* one
/// string with a suffix: `base_url` ends in the API version and the MCP route
/// is mounted beside it, at the deployment root. Getting it wrong produces a
/// client that starts, times out reaching its MCP server, and then runs turns
/// perfectly — with every steer silently unresolvable.
///
/// The version it strips is [`API_PREFIX`], read from the module that serves
/// the route, not a second `"/v1"` spelled here (F14). Two literals agreed
/// today and would part company on the edit that moved the turn surface, and
/// the parting is silent: [`deployment_root`] keeps the un-stripped root, so
/// the generated MCP url grows a version segment the router does not serve.
fn mcp_endpoint(base_url: &str) -> String {
    format!("{}{MCP_MOUNT_PATH}", deployment_root(base_url, API_PREFIX))
}

/// The deployment root a Responses `base_url` is served under, given the
/// prefix it is served at.
///
/// Takes the prefix as an argument rather than closing over [`API_PREFIX`],
/// and that parameter is the whole point: it is what lets a test drive this
/// with a version the deployment does *not* serve and watch the derivation
/// follow. A function that spelled the constant inline would be checkable only
/// by reading it, which is exactly the state F14 found it in.
///
/// The prefix is stripped rather than the last path segment. Stripping a
/// segment blindly would be shorter and would break a deployment served under
/// a path of its own (`https://host/roundhouse/v1`), which is the ordinary
/// shape behind a reverse proxy.
fn deployment_root<'a>(base_url: &'a str, api_prefix: &str) -> &'a str {
    let root = base_url.trim_end_matches('/');
    root.strip_suffix(api_prefix)
        .unwrap_or(root)
        .trim_end_matches('/')
}

/// The `[mcp_servers.*]` table key codex must see to rebuild
/// [`DEFAULT_MCP_NAMESPACE`].
///
/// Derived rather than written, so renaming the namespace renames the table key
/// in the same edit. The `expect` is unreachable for any namespace the
/// constant can hold and is a louder failure than emitting a config whose steer
/// calls resolve against nothing.
fn mcp_server_key() -> &'static str {
    DEFAULT_MCP_NAMESPACE
        .strip_prefix(MCP_NAMESPACE_PREFIX)
        .expect("the MCP namespace is `mcp__` plus the server's config table key")
}

/// One free string, quoted the way TOML wants it.
///
/// Through `toml` rather than `format!("\"{s}\"")` because a path or a base URL
/// can contain a quote or a backslash, and a template that produced a broken
/// file would fail inside the *client*, where the error names a line number in
/// a file nobody wrote by hand.
fn quote(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn launch() -> CodexLaunch {
        CodexLaunch::new(
            format!("http://127.0.0.1:8080{API_PREFIX}"),
            &PathBuf::from("/srv/roundhouse/models.json"),
        )
        .expect("the documented-correct shape constructs")
    }

    fn parsed(launch: &CodexLaunch) -> toml::Value {
        launch
            .config_toml()
            .parse::<toml::Value>()
            .unwrap_or_else(|error| {
                panic!(
                    "the generated config must be TOML: {error}\n{}",
                    launch.config_toml()
                )
            })
    }

    /// The generated file is TOML, and every value survives the round trip.
    ///
    /// The parse is the half a hand-written template can break; the value
    /// checks are the half a template can get syntactically right and
    /// semantically wrong — a stanza under the wrong table parses perfectly.
    #[test]
    fn the_generated_config_parses_back_to_the_values_it_was_built_from() {
        let launch = launch();
        let config = parsed(&launch);
        assert_eq!(config["model"].as_str(), Some(DEFAULT_MODEL_SLUG));
        assert_eq!(config["model_provider"].as_str(), Some(PROVIDER_KEY));
        assert_eq!(
            config["model_catalog_json"].as_str(),
            Some("/srv/roundhouse/models.json")
        );
        let provider = &config["model_providers"][PROVIDER_KEY];
        assert_eq!(
            provider["base_url"].as_str(),
            Some("http://127.0.0.1:8080/v1")
        );
        assert_eq!(provider["wire_api"].as_str(), Some("responses"));
        assert_eq!(provider["supports_websockets"].as_bool(), Some(false));
        assert_eq!(
            config["features"]["use_agent_identity"].as_bool(),
            Some(false)
        );
    }

    /// One environment variable, named by all three places that need it.
    ///
    /// The failure this catches is a config where the provider authenticates
    /// and the MCP surface does not: the client starts, runs turns, and only
    /// the steer path is dead.
    #[test]
    fn every_credential_reference_names_the_same_one_env_var() {
        let launch = launch().with_key_env("MY_KEY");
        let config = parsed(&launch);
        let provider = &config["model_providers"][PROVIDER_KEY];
        assert_eq!(provider["env_key"].as_str(), Some("MY_KEY"));
        assert_eq!(
            provider["env_http_headers"][TURN_KEY_HEADER].as_str(),
            Some("MY_KEY")
        );
        assert_eq!(
            config["mcp_servers"][mcp_server_key()]["bearer_token_env_var"].as_str(),
            Some("MY_KEY")
        );
    }

    /// The forwarded-login stanza never carries an `env_key`.
    ///
    /// Codex resolves an env key ahead of the login, so the pair is not a
    /// belt-and-braces config: it is forwarding switched off, with every
    /// request still valid.
    #[test]
    fn a_forwarded_login_stanza_carries_no_env_key_beside_the_flag() {
        let forwarding = launch().forwarding_openai_login();
        let config = parsed(&forwarding);
        let provider = &config["model_providers"][PROVIDER_KEY];
        assert_eq!(provider["requires_openai_auth"].as_bool(), Some(true));
        assert!(
            provider.get("env_key").is_none(),
            "an env_key beside requires_openai_auth = true silently disables forwarding: {provider}"
        );
        // The roundhouse key still has to arrive, and this is the only door
        // left once `Authorization` belongs to the upstream.
        assert_eq!(
            provider["env_http_headers"][TURN_KEY_HEADER].as_str(),
            Some(DEFAULT_KEY_ENV)
        );
        // The bring-your-own-key stanza is the control: same file, opposite pair.
        let byok = parsed(&launch());
        let byok = &byok["model_providers"][PROVIDER_KEY];
        assert_eq!(byok["requires_openai_auth"].as_bool(), Some(false));
        assert_eq!(byok["env_key"].as_str(), Some(DEFAULT_KEY_ENV));
    }

    /// F08: the forwarded stanza must name the real precondition for
    /// forwarding, which is a completed `codex login` -- not the flag.
    ///
    /// At `codex-cli` 0.146.0, `model-provider/src/auth.rs::resolve_provider_auth`
    /// (reached from both `api_auth` and `api_auth_for_scope`, the latter being
    /// what `core/src/client.rs` calls for every live request) decides the
    /// `Authorization` header from exactly two facts: whether `env_key` is set,
    /// and whether the auth manager has a cached `CodexAuth` -- populated only by
    /// a prior `codex login`, which persists it to `auth.json` in `CODEX_HOME`.
    /// `requires_openai_auth` is never read in that function. A client that has
    /// never logged in resolves to `unauthenticated_auth_provider()`: no
    /// `Authorization` header at all, so `turn_admission` in `control_config`
    /// captures nothing and the turn is admitted anyway, degraded to local.
    /// So the generated file has to say the words: an operator handed only the
    /// flag's comment has no way to learn that a skipped `codex login` costs
    /// them every frontier route and reports nothing. Asserting on the emitted
    /// text rather than on a doc comment is deliberate — the doc comment is
    /// read by whoever edits this file, and the person who needs this sentence
    /// is the one reading the config it produced.
    #[test]
    fn the_forwarded_stanza_tells_the_operator_the_login_is_the_precondition() {
        let forwarding = launch().forwarding_openai_login();
        let toml_text = forwarding.config_toml();
        assert!(
            toml_text.to_ascii_lowercase().contains("codex login"),
            "the forwarded-login stanza must name `codex login` as the actual \
             precondition for forwarding -- `requires_openai_auth` alone gates \
             nothing in the auth-resolution chain, so a client that never ran \
             `codex login` sends no Authorization header at all and every turn \
             silently degrades to local:\n{toml_text}"
        );
        // The control: the bring-your-own-key stanza must *not* pick the
        // sentence up. It has no login precondition -- `env_key` is resolved
        // ahead of any cached auth -- and telling that operator to log in
        // would send them to configure the one thing that would break their
        // stanza if it took effect.
        let byok = launch().config_toml();
        assert!(
            !byok.to_ascii_lowercase().contains("codex login"),
            "the bring-your-own-key stanza has no login precondition:\n{byok}"
        );
    }

    /// The provider name is not `OpenAI`, and the check the client makes is on
    /// this field rather than on the table key.
    #[test]
    fn the_provider_name_is_not_openai() {
        let config = parsed(&launch());
        let name = config["model_providers"][PROVIDER_KEY]["name"]
            .as_str()
            .expect("the provider is named");
        assert_ne!(
            name.to_ascii_lowercase(),
            "openai",
            "an `OpenAI` provider name turns on the routing hint header, remote compaction \
             and zstd request compression, none of which roundhouse serves"
        );
    }

    /// The MCP table key is exactly what codex needs to rebuild the namespace
    /// roundhouse emits its steers under.
    #[test]
    fn the_mcp_table_key_rebuilds_the_namespace_roundhouse_emits() {
        let config = parsed(&launch());
        let key = config["mcp_servers"]
            .as_table()
            .expect("the MCP servers table exists")
            .keys()
            .next()
            .expect("exactly one MCP server is registered")
            .clone();
        assert_eq!(
            format!("{MCP_NAMESPACE_PREFIX}{key}"),
            DEFAULT_MCP_NAMESPACE
        );
    }

    /// The client is told to run roundhouse's own tools without asking.
    ///
    /// Not a convenience knob: `codex exec` forces `approval_policy = never`,
    /// and at 0.146.0 an unannotated MCP tool under that policy is *cancelled*
    /// rather than run. Without this line every steer comes back to the agent
    /// as a cancellation notice -- the correction is never read, and nothing in
    /// the turn says so.
    #[test]
    fn the_client_is_told_to_trust_the_deployments_own_control_tools() {
        let config = parsed(&launch());
        assert_eq!(
            config["mcp_servers"][mcp_server_key()]["default_tools_approval_mode"].as_str(),
            Some("approve")
        );
    }

    /// The blanket `"approve"` above is justified by a claim about
    /// `roundhouse-mcp`'s tool list (every writing tool only narrows what the
    /// caller's key already allows, never widens it) that this crate cannot
    /// itself check. This is the tripwire for that claim: it does not
    /// re-derive the narrowing property (nothing here can), it only pins the
    /// surface's *size* at the count the justification above was written
    /// against. `roundhouse-server/tests/mcp_surface.rs` already pins the
    /// same list against the live wire response, but that assertion carries
    /// no message -- a ninth tool fails it, but the failure points at a
    /// mismatched const, not at this comment.
    ///
    /// F16 sharpened what the message has to ask for. A re-read of the
    /// paragraph above is no longer enough on its own: since F06 the tools
    /// carry MCP annotations, and those annotations -- not this config line --
    /// are what an unconfigured client reads. A ninth tool therefore needs two
    /// answers, and its annotations are the one that holds everywhere.
    #[test]
    fn the_surface_is_still_the_eight_tools_the_launch_config_grants_blanket_approval_to() {
        assert_eq!(
            roundhouse_mcp::tools::TOOL_NAMES.len(),
            8,
            "roundhouse-mcp's tool surface changed size. Two things have to be \
             re-derived before this number moves, not one: (1) does the new or \
             changed tool still only NARROW what the caller's own key already \
             allows -- a tool that spends a budget or widens a policy is exactly \
             what codex_launch.rs's default_tools_approval_mode = \"approve\" \
             does not cover; and (2) are its ANNOTATIONS in \
             roundhouse-mcp/src/tools.rs truthful -- read_only_hint matching \
             whether it mutates, destructive_hint and open_world_hint false only \
             if it really cannot destroy and really cannot reach past \
             roundhouse's own plane. Check (2) is the load-bearing one: the \
             annotations are what a client roundhouse generated no config for \
             reads, and codex decides approval from them alone under the default \
             Auto mode"
        );
    }

    /// The MCP url is the deployment root plus the mount path, not the API
    /// base plus a suffix.
    #[test]
    fn the_mcp_url_is_the_deployment_root_and_the_mount_path() {
        assert_eq!(
            mcp_endpoint("http://127.0.0.1:8080/v1"),
            "http://127.0.0.1:8080/mcp"
        );
        // A trailing slash is what a copy-pasted address usually carries.
        assert_eq!(
            mcp_endpoint("http://127.0.0.1:8080/v1/"),
            "http://127.0.0.1:8080/mcp"
        );
        // A base that names no version still gets one well-formed answer
        // rather than a silently doubled path. Not a supported shape: since
        // F13 the constructor refuses it by name
        // (`CodexLaunchError::BaseUrlMissingApiPrefix`), and
        // `a_base_url_without_the_served_api_prefix_is_refused` is where the
        // door is pinned. This pins the *function's* totality instead, which
        // still matters because the fields stay writable after construction —
        // `mcp_endpoint` must have one answer for every string, not a panic
        // and not a doubled path, since it is reached from `mcp_url()` on
        // whatever `base_url` currently holds.
        assert_eq!(
            mcp_endpoint("https://rh.example.com"),
            "https://rh.example.com/mcp"
        );
        assert_eq!(
            mcp_endpoint("https://rh.example.com/"),
            "https://rh.example.com/mcp"
        );
        // And the generated file agrees with the function.
        let config = parsed(&launch());
        assert_eq!(
            config["mcp_servers"][mcp_server_key()]["url"].as_str(),
            Some("http://127.0.0.1:8080/mcp")
        );
    }

    /// F14: `mcp_endpoint` strips the literal `"/v1"`, not "whatever version
    /// `responses_api::responses_router` actually serves" -- and nothing ties
    /// the two together. The route is a bare literal at
    /// `responses_api.rs:139` (`.route("/v1/responses", ...)`); this function's
    /// `strip_suffix("/v1")` is a second, independent literal that happens to
    /// agree with it today. A future rung that moves the turn surface (this
    /// function's own doc: "base_url ends in the API version") only updates
    /// one of the two if the edit is made where the route lives, and the
    /// failure is silent: `unwrap_or(root)` keeps the un-stripped root rather
    /// than erroring, so the generated MCP url gains a stray version segment
    /// the real router -- mounted at the deployment root by a flat `.merge` in
    /// `main.rs`, not nested under any version -- does not serve. That is
    /// exactly the drift [`MCP_MOUNT_PATH`]'s own doc comment was written to
    /// prevent for the mount path; this is the same shape one literal to the
    /// left, unfixed.
    ///
    /// Fixed by [`deployment_root`], which takes the prefix as an argument so
    /// that the "a future /v2 rung" case the original assertion described can
    /// actually be executed rather than only argued about. The three
    /// assertions below are three different mutations going red: hardcoding a
    /// version back into `deployment_root`, moving [`API_PREFIX`] without
    /// moving [`mcp_endpoint`], and "just strip the last segment".
    #[test]
    fn mcp_endpoint_tracks_whatever_version_the_responses_route_actually_serves() {
        // "A future /v2 rung moves the turn surface" (the claim's own words):
        // if `responses_api`'s route ever serves `/v2/responses`, `base_url`
        // becomes `.../v2` and the MCP mount -- unchanged, always at the
        // deployment root -- must still resolve to `.../mcp`. Executable only
        // because the prefix is a parameter; red again the moment a version is
        // spelled inside the function.
        assert_eq!(
            deployment_root("http://127.0.0.1:8080/v2", "/v2"),
            "http://127.0.0.1:8080",
            "the derivation must follow whatever prefix it is given, not one it names itself"
        );
        // And the shipped endpoint reads the prefix the route actually serves,
        // rather than a second literal that agrees with it today. Written as
        // "the answer does not still contain the prefix" so that moving
        // API_PREFIX alone -- the exact F14 edit -- fails here instead of
        // passing by coincidence.
        let mcp_url = mcp_endpoint(&format!("https://rh.example.com{API_PREFIX}"));
        assert_eq!(mcp_url, format!("https://rh.example.com{MCP_MOUNT_PATH}"));
        assert!(
            !mcp_url.contains(API_PREFIX),
            "the served API prefix `{API_PREFIX}` is still in the MCP url `{mcp_url}`: \
             mcp_endpoint is stripping something else"
        );
        // The prefix is stripped, not the last segment. A deployment behind a
        // reverse proxy is served under a path of its own, and the shorter
        // implementation would eat it.
        assert_eq!(
            mcp_endpoint(&format!("https://host.example.com/roundhouse{API_PREFIX}")),
            format!("https://host.example.com/roundhouse{MCP_MOUNT_PATH}")
        );
    }

    /// The catalog is a non-empty `{"models":[…]}` carrying every key 0.146.0
    /// requires.
    ///
    /// Listed rather than spot-checked because the reader's failure mode is
    /// all-or-nothing: a missing required key is an `InvalidData` config-load
    /// error naming the file, and every test downstream then fails for a
    /// reason that reads like a harness bug.
    #[test]
    fn the_catalog_carries_every_key_the_client_requires() {
        let catalog: serde_json::Value =
            serde_json::from_str(&launch().model_catalog_json()).expect("the catalog is JSON");
        let models = catalog["models"].as_array().expect("a models array");
        assert_eq!(models.len(), 1, "an empty catalog is a hard load error");
        let entry = &models[0];
        for key in [
            "slug",
            "display_name",
            "supported_reasoning_levels",
            "shell_type",
            "visibility",
            "supported_in_api",
            "priority",
            "base_instructions",
            "support_verbosity",
            "truncation_policy",
            "supports_parallel_tool_calls",
            "experimental_supported_tools",
        ] {
            assert!(
                entry.get(key).is_some(),
                "the catalog entry must carry `{key}`"
            );
            assert!(
                !entry[key].is_null(),
                "`{key}` has no serde default in the reader, so null is not the same as present"
            );
        }
        assert_eq!(entry["slug"].as_str(), Some(DEFAULT_MODEL_SLUG));
        assert_eq!(entry["shell_type"].as_str(), Some("shell_command"));
        assert_eq!(
            entry["context_window"].as_u64(),
            Some(CONTEXT_WINDOW_TOKENS)
        );
        assert!(
            entry["auto_compact_token_limit"].is_null(),
            "a compaction limit would let a judge's reported usage rewrite the client's history"
        );
    }

    /// F18: `supports_search_tool` is written, and written `false`.
    ///
    /// Asserting on a field whose absence means the same thing looks
    /// redundant, and is the point: the reader defaults it to `false`
    /// (`#[serde(default)]`, `protocol/src/openai_models.rs:434-435`
    /// @ `e363b08`), so the risk was never that today's catalog turns it on --
    /// it is that tomorrow's is authored by copying an upstream per-model
    /// entry, where a flagship's `true` rides along unnoticed. This is the only
    /// gate: `search_tool_enabled` is `supports_search_tool &&
    /// namespace_tools_enabled` (`core/src/tools/spec_plan.rs:333-335`
    /// @ `e363b08`), and `namespace_tools` defaults *true* for every provider,
    /// this one included. Flip it and the client may resend a
    /// `tool_search_call` / `tool_search_output` pair, which
    /// `responses_api::wire::canonical_item` refuses with a 422 that takes the
    /// whole turn with it -- the boundary
    /// `the_item_types_a_real_client_can_resend_are_named` pins from the other
    /// side.
    #[test]
    fn the_catalog_states_that_this_model_has_no_search_tool() {
        let catalog: serde_json::Value =
            serde_json::from_str(&launch().model_catalog_json()).expect("the catalog is JSON");
        let entry = &catalog["models"][0];
        assert_eq!(
            entry.get("supports_search_tool"),
            Some(&serde_json::Value::Bool(false)),
            "the catalog must state `supports_search_tool: false` rather than leave it to \
             the reader's default: it is the only thing standing between a copied upstream \
             entry and a tool_search_call that 422s every turn it appears in:\n{entry:#}"
        );
    }

    /// The catalog turns on the half of skills support that lives in model
    /// metadata.
    ///
    /// Paired with [`skills::skill_files`]: without this field the generated
    /// skills are still listed to the model, as three names with three file
    /// paths and no statement that opening one is what a skill is for. The
    /// reader defaults it `false`
    /// (`protocol/src/openai_models.rs:392-393` @ `e363b08`), so this is a
    /// deliberate overwrite in the same shape as `supports_search_tool` — and
    /// in the opposite direction, which is why both are asserted rather than
    /// one being taken as evidence for the other.
    #[test]
    fn the_catalog_tells_the_client_to_explain_skills_to_the_model() {
        let catalog: serde_json::Value =
            serde_json::from_str(&launch().model_catalog_json()).expect("the catalog is JSON");
        assert_eq!(
            catalog["models"][0].get("include_skills_usage_instructions"),
            Some(&serde_json::Value::Bool(true)),
            "the skills roundhouse generates are listed to the model either way; this is what \
             tells it that reading one is how a skill is used"
        );
    }

    /// The catalog names the slug the config names.
    ///
    /// Two files, one identifier: a client whose catalog does not carry the
    /// configured slug falls back to invented metadata and reports it as an
    /// `error` item on stdout, which is the one shape a harness assertion
    /// cannot tell from a real failure.
    #[test]
    fn the_catalog_and_the_config_name_one_slug() {
        let launch = launch().with_model("roundhouse-e2e");
        let config = parsed(&launch);
        let catalog: serde_json::Value =
            serde_json::from_str(&launch.model_catalog_json()).expect("the catalog is JSON");
        assert_eq!(config["model"].as_str(), Some("roundhouse-e2e"));
        assert_eq!(
            catalog["models"][0]["slug"].as_str(),
            Some("roundhouse-e2e")
        );
    }

    /// No secret is in either generated file.
    ///
    /// Structural, not incidental: [`CodexLaunch`] has no field a secret could
    /// be put in. This asserts the property anyway, because the field that
    /// would break it is exactly the one a future "make it easier to launch"
    /// change adds.
    #[test]
    fn neither_generated_file_can_carry_a_key() {
        let launch = launch();
        for text in [launch.config_toml(), launch.model_catalog_json()] {
            assert!(
                !text.contains("rh_turn_") && !text.contains("rh_admin_"),
                "a generated file must name the env var, never the secret:\n{text}"
            );
        }
        // The env var *name* is what is there instead.
        assert!(launch.config_toml().contains(DEFAULT_KEY_ENV));
    }
}
