// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The third launch artifact: a NeMo Fabric `FabricConfig` that points a
//! Fabric-driven Codex at this deployment.
//!
//! [`config_toml`](super::CodexLaunch::config_toml) is what an operator hands
//! a `codex` binary. This is what they hand a *Fabric consumer* — Harbor, an
//! evaluation platform, a rollout runner — which never reads a `config.toml`
//! at all: Fabric's Codex adapter builds a request-scoped config dict and
//! hands it to the Codex SDK's `thread_start`, which spawns its own pinned
//! app-server. Same `model_providers` / `mcp_servers` vocabulary, different
//! injection point, and a third topology beside Direct and Chained, ruled in
//! `agent-docs/synergies/nemo-fabric.md`: Fabric-driven, Codex-only today.
//!
//! **Why this is typed by `nemo_fabric_core` rather than hand-written JSON.**
//! The document is Fabric's, not ours. Writing it as `serde_json::json!` would
//! make every field name a string this crate spells and Fabric checks, so a
//! rename upstream surfaces as a planning error on the consumer's machine. With
//! the consumer's own types the same rename is a compile error here, at the pin
//! bump, where the synergy-vigilance rule already says to look. The pin is a
//! git rev and its unlock condition is beside it in the workspace manifest.
//!
//! **What travels in `config_overrides`, and why.** Fabric has normalized
//! fields for the endpoint, the key's environment variable, the MCP server and
//! the skill directories, and none for four lines whose absence fails
//! silently in the dangerous direction — the reasons are on each line of the
//! TOML template. Fabric's Codex adapter accepts dotted keys under
//! `harness.settings.config_overrides`, nests them, and applies them *after*
//! its own provider and MCP layers, so an override wins. The four go there.
//! One of them, `bearer_token_env_var`, is what keeps the turn key out of the
//! payload: Fabric's normalized `custom_headers` expands the variable's value
//! into the config it sends over JSON-RPC, which inverts "the secret is never
//! in the file".
//!
//! **What this artifact refuses.** The forwarded-login stanza
//! ([`CodexAuthKind::ForwardedOpenAiLogin`](super::CodexAuthKind)) needs
//! `requires_openai_auth = true` with *no* `env_key`; Fabric writes `env_key`
//! for every custom provider and its overrides can set a key but never delete
//! one, so the only expressible result is the pair the TOML's own comment calls
//! "forwarding switched off, with every request still valid". Refused by name
//! rather than emitted wrong. `runtime.max_turns` and `tools.enabled` are not
//! written either: Fabric's compatibility matrix says Codex does not implement
//! them and an explicitly configured value fails planning rather than being
//! ignored — the test below proves that against Fabric's own descriptor.
//!
//! **The slug is load-bearing here more than in the TOML.** Fabric passes
//! `models.default.model` to the app-server verbatim with no default; measured
//! at `openai-codex==0.144.4`, a real OpenAI slug puts a `tool_search` tool
//! definition on every request, and the `tool_search_call` item a later turn
//! resends is one this surface refuses with 422. The launch's model
//! ([`DEFAULT_MODEL_SLUG`](super::DEFAULT_MODEL_SLUG) unless overridden) is
//! written as-is; the safety is in the default, not in this module.
//!
//! **Model catalog: deliberately absent.** Fabric has no `model_catalog_json`
//! field. Under env-key auth with the fresh `CODEX_HOME` Fabric's adapter
//! arranges for a custom provider, the `GET {base_url}/models` fetch the pin
//! exists to remove was not observed; the auth mode that does gate it is the
//! forwarded login, which this artifact refuses. What the missing catalog does
//! cost is `include_skills_usage_instructions`, so the skills half of this
//! document is "composes, degraded" and the ruling says so.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// `McpServerConfig` is `pub` in Fabric's `config` module but not re-exported
// from the crate root at this rev; every other type here is.
use nemo_fabric_core::config::McpServerConfig;
use nemo_fabric_core::{
    FabricConfig, HarnessConfig, McpConfig, McpExposure, McpTransport, MetadataConfig, ModelConfig,
    RuntimeConfig, SkillConfig,
};
use serde_json::{Map, Value, json};

use super::skills::{SKILLS_DIR, skill_files};
use super::{CodexAuthKind, CodexLaunch, PROVIDER_KEY, TOOLS_APPROVAL_MODE, mcp_server_key};
use crate::control_config::TURN_KEY_HEADER;

/// The adapter this artifact selects. Fabric resolves it to the descriptor
/// whose `settings_schema` and `model_schema` the test below plans against.
pub const FABRIC_ADAPTER_ID: &str = "nvidia.fabric.codex";

/// The config schema version Fabric's Python SDK writes by default. The Rust
/// core carries the field as an unvalidated string, so this crate supplies the
/// canonical value itself; a consumer reading a document with the wrong one
/// gets no error, which is why it is a named constant and not an inline
/// literal.
pub const FABRIC_SCHEMA_VERSION: &str = "fabric.agent/v1alpha1";

/// Fabric's own default for `runtime.input_schema` / `output_schema`, restated
/// because the Rust type has no `Default` and its serde defaults are private.
const FABRIC_TEXT_SCHEMA: &str = "text";

/// Why a launch cannot be expressed as a `FabricConfig`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FabricLaunchError {
    /// The forwarded ChatGPT login is not expressible: Fabric always writes
    /// `env_key` for a custom provider, and an `env_key` beside
    /// `requires_openai_auth = true` silently disables the forwarding.
    #[error(
        "the forwarded-login stanza cannot be expressed as a FabricConfig: Fabric's Codex \
         adapter writes `env_key` for every custom provider and its overrides can only set \
         keys, never delete one, so the result would carry `requires_openai_auth = true` \
         beside an `env_key` -- forwarding switched off, with every request still valid. \
         Hand this client the generated config.toml instead"
    )]
    ForwardedLoginNotExpressible,
    /// A relative skills root would resolve against Fabric's `base_dir`, not
    /// against the directory roundhouse wrote the skills into — the same
    /// failure class as a relative model catalog path.
    #[error(
        "the skills root `{path}` is relative. Fabric resolves `skills.paths` against its own \
         base directory, not the directory roundhouse wrote the skill files into, so a \
         relative root names directories that are not there and the adapter refuses to start"
    )]
    RelativeSkillsRoot { path: String },
}

/// Build the `FabricConfig` that points a Fabric-driven Codex at the
/// deployment `launch` describes.
///
/// `skills_root` is the directory the generated skill files were written under
/// (the client's `CODEX_HOME` in the Direct topology): each generated
/// `skills/<name>/SKILL.md` becomes one `skills.paths` entry naming its leaf
/// directory, which is the shape Fabric's Codex adapter registers. `None`
/// omits the skills block.
pub fn fabric_config(
    launch: &CodexLaunch,
    skills_root: Option<&Path>,
) -> Result<FabricConfig, FabricLaunchError> {
    if launch.auth == CodexAuthKind::ForwardedOpenAiLogin {
        return Err(FabricLaunchError::ForwardedLoginNotExpressible);
    }
    let skills = skills_root.map(skill_paths).transpose()?;

    let mut settings = Map::new();
    settings.insert(
        "config_overrides".to_string(),
        Value::Object(config_overrides(launch)),
    );

    let mut models = BTreeMap::new();
    models.insert(
        "default".to_string(),
        ModelConfig {
            provider: PROVIDER_KEY.to_string(),
            model: launch.model.clone(),
            temperature: None,
            api_key_env: Some(launch.key_env.clone()),
            base_url: Some(launch.base_url.clone()),
            settings: Map::new(),
            extensions: BTreeMap::new(),
        },
    );

    let mut servers = BTreeMap::new();
    servers.insert(
        mcp_server_key().to_string(),
        McpServerConfig {
            transport: McpTransport::StreamableHttp,
            url: launch.mcp_url(),
            args: Vec::new(),
            env: BTreeMap::new(),
            authentication: None,
            exposure: McpExposure::HarnessNative,
            allowed_tools: None,
            blocked_tools: Vec::new(),
            extensions: BTreeMap::new(),
            // Deliberately empty: the bearer rides `bearer_token_env_var` in
            // `config_overrides`, so the secret stays an environment-variable
            // *name* everywhere this document is stored or logged.
            custom_headers: BTreeMap::new(),
        },
    );

    Ok(FabricConfig {
        schema_version: FABRIC_SCHEMA_VERSION.to_string(),
        metadata: MetadataConfig {
            name: "roundhouse".to_string(),
            description: Some(format!(
                "Codex driven by NeMo Fabric against the roundhouse deployment at {}",
                launch.base_url
            )),
            extensions: BTreeMap::new(),
        },
        harness: Some(HarnessConfig {
            adapter_id: FABRIC_ADAPTER_ID.to_string(),
            resolution: None,
            settings,
            extensions: BTreeMap::new(),
        }),
        workflow: None,
        discovery: None,
        models,
        instructions: None,
        runtime: RuntimeConfig {
            input_schema: FABRIC_TEXT_SCHEMA.to_string(),
            output_schema: FABRIC_TEXT_SCHEMA.to_string(),
            artifacts: None,
            timeout_seconds: None,
            max_turns: None,
            extensions: BTreeMap::new(),
        },
        environment: None,
        tools: None,
        skills: skills.map(|paths| SkillConfig {
            paths,
            extensions: BTreeMap::new(),
        }),
        mcp: Some(McpConfig {
            servers,
            extensions: BTreeMap::new(),
        }),
        telemetry: None,
        relay: None,
        extensions: BTreeMap::new(),
    })
}

/// [`fabric_config`], serialized the way an operator writes it to disk.
pub fn fabric_config_json(
    launch: &CodexLaunch,
    skills_root: Option<&Path>,
) -> Result<String, FabricLaunchError> {
    let config = fabric_config(launch, skills_root)?;
    Ok(serde_json::to_string_pretty(&config).expect("FabricConfig serializes"))
}

/// The four lines Fabric has no normalized field for, as the dotted keys its
/// Codex adapter nests into the thread config — each with the same value the
/// TOML template writes, read from the same constant, so the two artifacts
/// cannot disagree.
fn config_overrides(launch: &CodexLaunch) -> Map<String, Value> {
    let server = mcp_server_key();
    let mut overrides = Map::new();
    overrides.insert(
        format!("mcp_servers.{server}.bearer_token_env_var"),
        json!(launch.key_env),
    );
    overrides.insert(
        format!("mcp_servers.{server}.default_tools_approval_mode"),
        json!(TOOLS_APPROVAL_MODE),
    );
    overrides.insert(
        format!("model_providers.{PROVIDER_KEY}.requires_openai_auth"),
        json!(false),
    );
    overrides.insert(
        format!("model_providers.{PROVIDER_KEY}.env_http_headers"),
        json!({ TURN_KEY_HEADER: launch.key_env }),
    );
    overrides
}

/// The leaf directory of every generated skill under `root`, in the order the
/// files are generated and without repeats.
fn skill_paths(root: &Path) -> Result<Vec<PathBuf>, FabricLaunchError> {
    if !root.is_absolute() {
        return Err(FabricLaunchError::RelativeSkillsRoot {
            path: root.display().to_string(),
        });
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    for file in skill_files() {
        let leaf = Path::new(&file.relative_path)
            .parent()
            .map(|dir| root.join(dir))
            .expect("a generated skill file lives in a directory");
        debug_assert!(
            leaf.starts_with(root.join(SKILLS_DIR)),
            "a generated skill lives under `{SKILLS_DIR}`"
        );
        if !paths.contains(&leaf) {
            paths.push(leaf);
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use nemo_fabric_core::{
        FabricError, ResolveContext, resolve_run_plan_from_config_with_adapter_directories,
    };

    use super::super::{API_PREFIX, DEFAULT_KEY_ENV, DEFAULT_MODEL_SLUG};
    use super::*;

    /// Fabric's own Codex adapter descriptor, vendored at the pinned rev so
    /// planning runs against the schema the real adapter is validated with
    /// rather than against this crate's idea of it.
    fn fabric_descriptors() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nemo-fabric")
    }

    fn launch() -> CodexLaunch {
        CodexLaunch::new(
            "http://127.0.0.1:8080/v1",
            Path::new("/srv/roundhouse/codex/models.json"),
        )
        .expect("a valid launch")
    }

    fn plan(config: FabricConfig) -> Result<nemo_fabric_core::RunPlan, FabricError> {
        resolve_run_plan_from_config_with_adapter_directories(
            config,
            ResolveContext::new(env!("CARGO_MANIFEST_DIR")),
            &[fabric_descriptors()],
        )
    }

    #[test]
    fn the_document_round_trips_through_fabrics_own_type() {
        let expected = fabric_config(&launch(), Some(Path::new("/home/agent/.codex"))).unwrap();
        let json = fabric_config_json(&launch(), Some(Path::new("/home/agent/.codex"))).unwrap();
        let read: FabricConfig = serde_json::from_str(&json).expect("Fabric's type reads it");
        assert_eq!(read, expected);
        assert_eq!(read.schema_version, FABRIC_SCHEMA_VERSION);
    }

    #[test]
    fn fabrics_codex_descriptor_plans_the_document() {
        let config = fabric_config(&launch(), Some(Path::new("/home/agent/.codex"))).unwrap();
        let plan =
            plan(config).expect("Fabric plans the emitted config against its Codex descriptor");
        let descriptor = plan
            .adapter_descriptor
            .expect("the Codex descriptor resolved");
        assert_eq!(descriptor.descriptor.adapter_id, FABRIC_ADAPTER_ID);
        // The skill directories and the MCP server survive projection into
        // the adapter-facing plan, which is what the adapter will register:
        // one leaf directory per generated skill, under the root, in order.
        let expected: Vec<PathBuf> = skill_files()
            .iter()
            .map(|file| {
                Path::new("/home/agent/.codex")
                    .join(Path::new(&file.relative_path).parent().unwrap())
            })
            .collect();
        assert_eq!(plan.capability_plan.skill_paths, expected);
        assert!(
            expected
                .iter()
                .all(|p| p.starts_with("/home/agent/.codex/skills") && p.file_name().is_some())
        );
        assert!(
            plan.capability_plan
                .mcp_servers
                .contains_key(mcp_server_key())
        );
    }

    /// The descriptor is load-bearing, not decorative: the field the owner's
    /// Hermes quickstart carries (`runtime.max_turns`) is one Fabric's Codex
    /// adapter does not implement, and Fabric refuses it at planning rather
    /// than ignoring it. If this test ever passes planning, the vendored
    /// descriptor stopped being the Codex one.
    #[test]
    fn the_descriptor_refuses_what_codex_does_not_implement() {
        let mut config = fabric_config(&launch(), None).unwrap();
        config.runtime.max_turns = Some(1);
        let error = plan(config).expect_err("max_turns fails planning for Codex");
        assert!(
            matches!(&error, FabricError::AdapterCompatibility { field, .. } if field.contains("max_turns")),
            "{error}"
        );
    }

    /// And `harness.settings` is checked against the adapter's closed schema:
    /// a key the Codex adapter does not declare is refused, which is the
    /// guarantee that lets `config_overrides` be the *only* untyped door.
    #[test]
    fn the_descriptor_refuses_an_undeclared_setting() {
        let mut config = fabric_config(&launch(), None).unwrap();
        config
            .harness
            .as_mut()
            .unwrap()
            .settings
            .insert("model_catalog_json".to_string(), json!("/x/models.json"));
        let error = plan(config).expect_err("an undeclared harness setting fails planning");
        assert!(
            format!("{error}").contains("model_catalog_json"),
            "the refusal names the key: {error}"
        );
    }

    #[test]
    fn the_four_silent_failure_lines_ride_config_overrides_with_the_tomls_values() {
        let config = fabric_config(&launch().with_key_env("RH_KEY"), None).unwrap();
        let settings = &config.harness.as_ref().unwrap().settings;
        let overrides = settings["config_overrides"].as_object().unwrap();
        let server = mcp_server_key();
        assert_eq!(
            overrides[&format!("mcp_servers.{server}.bearer_token_env_var")],
            json!("RH_KEY")
        );
        assert_eq!(
            overrides[&format!("mcp_servers.{server}.default_tools_approval_mode")],
            json!(TOOLS_APPROVAL_MODE)
        );
        assert_eq!(
            overrides[&format!("model_providers.{PROVIDER_KEY}.requires_openai_auth")],
            json!(false)
        );
        assert_eq!(
            overrides[&format!("model_providers.{PROVIDER_KEY}.env_http_headers")],
            json!({ TURN_KEY_HEADER: "RH_KEY" })
        );
        assert_eq!(
            overrides.len(),
            4,
            "exactly the four lines Fabric has no field for"
        );
        // Every dotted key satisfies the descriptor's `propertyNames` pattern.
        for key in overrides.keys() {
            assert!(
                key.split('.').all(|segment| !segment.is_empty()),
                "`{key}` has an empty dotted segment"
            );
        }
    }

    #[test]
    fn the_endpoint_the_slug_and_the_mount_are_the_launchs_own() {
        let config = fabric_config(&launch(), None).unwrap();
        let model = &config.models["default"];
        assert_eq!(model.provider, PROVIDER_KEY);
        assert_eq!(model.model, DEFAULT_MODEL_SLUG);
        assert_eq!(model.api_key_env.as_deref(), Some(DEFAULT_KEY_ENV));
        assert_eq!(model.base_url.as_deref(), Some("http://127.0.0.1:8080/v1"));
        assert!(model.base_url.as_deref().unwrap().ends_with(API_PREFIX));
        let server = &config.mcp.as_ref().unwrap().servers[mcp_server_key()];
        assert_eq!(server.url, launch().mcp_url());
        assert_eq!(server.transport, McpTransport::StreamableHttp);
        assert_eq!(server.exposure, McpExposure::HarnessNative);
        assert!(
            server.custom_headers.is_empty(),
            "no header carries a value"
        );
        assert!(config.runtime.max_turns.is_none());
        assert!(config.tools.is_none());
        assert!(config.skills.is_none(), "no root, no skills block");
    }

    #[test]
    fn the_forwarded_login_is_refused_by_name() {
        let error = fabric_config(&launch().forwarding_openai_login(), None).unwrap_err();
        assert_eq!(error, FabricLaunchError::ForwardedLoginNotExpressible);
    }

    #[test]
    fn a_relative_skills_root_is_refused() {
        let error = fabric_config(&launch(), Some(Path::new("codex-home"))).unwrap_err();
        assert!(matches!(
            error,
            FabricLaunchError::RelativeSkillsRoot { .. }
        ));
    }

    #[test]
    fn no_secret_can_be_in_the_document() {
        // Everything credential-shaped is an environment-variable *name*: the
        // key's variable appears, and nothing that looks like a minted key.
        let json = fabric_config_json(&launch(), Some(Path::new("/home/agent/.codex"))).unwrap();
        assert!(json.contains(DEFAULT_KEY_ENV));
        assert!(!json.contains("rh_turn_"));
        assert!(!json.contains("Bearer "));
    }
}
