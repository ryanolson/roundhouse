// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What an operator writes down once so they never type it again — and the one
//! thing it may never contain.
//!
//! A profile is a TOML file under `<config>/topham/profiles/<name>.toml`
//! carrying names: which agent, which deployment, which of the two auth kinds,
//! **the name of the variable** the turn key arrives in, which topology, and
//! for codex a model slug and a catalog path. That is the whole vocabulary, and
//! everything in this crate is a function of it plus the environment.
//!
//! # The refusal this module exists for
//!
//! [`Profile::from_toml`] refuses a file carrying anything that looks like a
//! roundhouse key — in any field, including one whose name suggests it should
//! hold one — and names the field it found it in.
//!
//! The rule is not this module's invention: `codex_launch` states it ("the
//! secret is never in the file") and `claude_launch` enforces it by making
//! `ClaudeEnv` un-serializable. What a profile adds is a *new* file in a place
//! secrets are especially likely to end up. A configuration directory is
//! exactly what people commit to a dotfile repository, and a `rh_turn_…` in one
//! is a live credential that every tool downstream — grep, a backup, a
//! screen-share — will treat as a string.
//!
//! **The check is coarser than `has_valid_key_shape`, deliberately.** Anything
//! wearing a minted prefix is refused, well-formed or not, and so is the launch
//! sentinel. A truncated paste is not a usable key and *is* still a secret in a
//! file; refusing only the well-formed ones would admit the copy that was cut
//! short by a terminal width, which is the likeliest way one gets in here at
//! all.
//!
//! # What a profile does not carry, and why
//!
//! - **A project or a member.** Those are arguments to [`mint`](crate::mint),
//!   not fields here: a profile names a deployment and an agent, and the
//!   tenancy a key is minted under is a question about the control plane. A
//!   profile that named one would be a second place a membership is written
//!   down, disagreeing with the directory the moment either moves.
//! - **A separate base URL per agent.** One `deployment-root` is stored and the
//!   two generators derive what they need from it — codex's `base_url` carries
//!   `API_PREFIX`, Claude Code's must not, because that client's SDK appends
//!   the version segment itself. Storing both would let one profile name two
//!   deployments, and the generator that refused would be whichever one the
//!   operator was not looking at.

use std::path::PathBuf;

use roundhouse_server::claude_launch::ROUNDHOUSE_API_KEY_SENTINEL;
use roundhouse_server::codex_launch::{DEFAULT_KEY_ENV, DEFAULT_MODEL_SLUG};
use roundhouse_server::control_config::KeyKind;
use serde::{Deserialize, Serialize};

use crate::env::{self, EnvMap, NoHome};

/// The directory under `<config>`/`<data>` that belongs to this launcher.
const TOPHAM_DIR: &str = "topham";

/// Which client this profile launches.
///
/// Not a detail of the launch: it decides which generator resolves the profile,
/// and the two generators produce different *kinds* of thing — codex is
/// configured by files in a `CODEX_HOME`, Claude Code entirely by environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    /// The program a launch execs when the profile names no argv of its own.
    ///
    /// The bare name, resolved through `PATH` by the exec — not an absolute
    /// path discovered here. An operator who has two `claude` binaries has
    /// already answered which one they mean, in their `PATH`, and a launcher
    /// that searched independently would answer it differently.
    pub fn program(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
}

/// How the launched client authenticates to roundhouse.
///
/// One enum for both agents, because it is one decision:
/// [`ClaudeAuthKind`](roundhouse_server::ClaudeAuthKind) and
/// [`CodexAuthKind`](roundhouse_server::CodexAuthKind) mirror each other
/// variant for variant, and a profile that could name a Claude kind for a codex
/// agent would be a state whose resolution has no meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthKind {
    /// The client holds a roundhouse turn key and nothing else. The default,
    /// because it is the kind that works without a prior login to anywhere.
    #[default]
    RoundhouseKey,
    /// The client's own subscription login — Claude for `claude`, ChatGPT for
    /// `codex` — is forwarded upstream by roundhouse.
    ///
    /// **The precondition is that login, not this field.** Both generators say
    /// so at length: the flag selects a code path, and the credential comes
    /// from a login the operator already completed. Without it every turn
    /// arrives with no credential, which roundhouse admits and degrades to
    /// local-only routing rather than refusing — so nothing looks broken and
    /// no frontier route ever happens.
    ForwardedLogin,
}

impl AuthKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthKind::RoundhouseKey => "roundhouse-key",
            AuthKind::ForwardedLogin => "forwarded-login",
        }
    }
}

/// Whether the client reaches roundhouse directly or through a NeMo Relay.
///
/// Recorded in the profile rather than chosen per launch because it decides
/// *which subcommand* is the right one to run: `topham launch` on
/// [`Topology::Direct`], `topham relay` on [`Topology::Chained`]. Both hand the
/// client the same generated environment — that is the chained runbook's whole
/// finding — so the topology changes what wraps the client, never what the
/// client is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Topology {
    #[default]
    Direct,
    Chained,
}

impl Topology {
    pub fn as_str(self) -> &'static str {
        match self {
            Topology::Direct => "direct",
            Topology::Chained => "chained",
        }
    }
}

/// One saved answer to "how does this agent hook up to that deployment".
///
/// `deny_unknown_fields` for the reason the admin API's project entry has it: a
/// misspelled key here is silent otherwise, and every field in this struct
/// changes where a client posts turns or which credential it presents. A
/// dropped `auth-kind` would launch the default kind and look exactly like a
/// launch that was asked for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Profile {
    /// Which client. See [`Agent`].
    pub agent: Agent,
    /// **The deployment root**, with no `/v1`: the address roundhouse is served
    /// on and nothing more. Each generator is handed the shape it needs — see
    /// the module doc.
    pub deployment_root: String,
    #[serde(default)]
    pub auth: AuthKind,
    /// The environment variable the turn key is read from.
    ///
    /// A name, never a value. Defaults to
    /// [`DEFAULT_KEY_ENV`] — codex's own default, so a profile that says
    /// nothing agrees with the config a generator would have written on its
    /// own.
    #[serde(default = "default_key_env")]
    pub key_env: String,
    #[serde(default)]
    pub topology: Topology,
    /// The model slug codex is configured with. `None` means
    /// [`DEFAULT_MODEL_SLUG`], and reading that default rather than writing a
    /// slug here is the point: the generator's doc explains why the slug is
    /// deliberately not a real OpenAI one, and a profile that copied a real
    /// slug in would resolve client-side metadata this surface refuses with a
    /// 422.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Where codex's generated model catalog is written. `None` means "beside
    /// the generated config, in this profile's `CODEX_HOME`" — see
    /// [`Profile::codex_home`].
    ///
    /// Absolute when given, because codex resolves a relative
    /// `model_catalog_json` against the directory it loaded the config from and
    /// then falls back to *invented* metadata rather than erroring.
    /// [`CodexLaunch::new`](roundhouse_server::CodexLaunch::new) refuses the
    /// relative shape; this field exists so an operator can point at a catalog
    /// they maintain themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_catalog_path: Option<PathBuf>,
}

fn default_key_env() -> String {
    DEFAULT_KEY_ENV.to_string()
}

/// Why a profile could not be read, written, or trusted.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error(
        "the profile `{name}` carries what looks like a roundhouse secret in `{field}`. A profile \
         names the *variable* a turn key arrives in and never the key itself: this file lives in a \
         configuration directory, which is where dotfile repositories, backups and screen-shares \
         find it, and nothing downstream can tell that copy from a live credential. Remove the \
         value, export it instead (`topham mint` prints the export line), and leave `key-env` \
         naming the variable"
    )]
    CarriesSecret { name: String, field: String },
    #[error(
        "the profile name `{name}` is not a single filename. A profile name becomes one path \
         segment under the profiles directory, so a name carrying a separator or a `..` would \
         read a file nobody wrote as though it were a profile somebody did"
    )]
    UnusableName { name: String },
    #[error("no profile `{name}` at {path}")]
    NotFound { name: String, path: PathBuf },
    #[error("the profile `{name}` at {path} is not valid TOML: {source}")]
    Malformed {
        name: String,
        path: PathBuf,
        /// Boxed, and the reason is every *other* variant. A `toml::de::Error`
        /// carries its own span table and is over a hundred bytes on its own,
        /// which would make every `Result<_, ProfileError>` in this crate — the
        /// path resolvers, the name check, the listing — that wide on the
        /// success path too.
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error(
        "the profile `{name}` sets `{field}`, which only a codex profile has. Read on a `claude` \
         profile it would be dropped in silence -- and a slug or a catalog path is exactly the \
         kind of field an operator sets and then believes"
    )]
    NotACodexField { name: String, field: &'static str },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    NoHome(#[from] NoHome),
}

impl Profile {
    /// A bring-your-own-key Direct profile for `agent` against `deployment_root`.
    pub fn new(agent: Agent, deployment_root: impl Into<String>) -> Self {
        Self {
            agent,
            deployment_root: deployment_root.into(),
            auth: AuthKind::default(),
            key_env: default_key_env(),
            topology: Topology::default(),
            model: None,
            model_catalog_path: None,
        }
    }

    /// The slug codex is configured with, resolved against the default.
    pub fn model_slug(&self) -> &str {
        self.model.as_deref().unwrap_or(DEFAULT_MODEL_SLUG)
    }

    /// Parse a profile, refusing a file that carries a secret.
    ///
    /// Two passes over one document, and the order matters: the scan runs
    /// against the raw TOML **before** deserialization, so a secret in a field
    /// this struct does not have is still found. `deny_unknown_fields` would
    /// otherwise reject that file with a message about a spelling mistake,
    /// leaving the operator to fix the name of the field their key is sitting
    /// in.
    pub fn from_toml(text: &str, name: &str) -> Result<Self, ProfileError> {
        let document: toml::Value =
            toml::from_str(text).map_err(|source| ProfileError::Malformed {
                name: name.to_string(),
                path: PathBuf::from(format!("{name}.toml")),
                source: Box::new(source),
            })?;
        if let Some(field) = find_secret(&document, "") {
            return Err(ProfileError::CarriesSecret {
                name: name.to_string(),
                field,
            });
        }
        let profile: Profile = toml::from_str(text).map_err(|source| ProfileError::Malformed {
            name: name.to_string(),
            path: PathBuf::from(format!("{name}.toml")),
            source: Box::new(source),
        })?;
        profile.validate(name)?;
        Ok(profile)
    }

    /// The cross-field rules, which are about fields belonging to the wrong
    /// agent.
    fn validate(&self, name: &str) -> Result<(), ProfileError> {
        if self.agent == Agent::Claude {
            if self.model.is_some() {
                return Err(ProfileError::NotACodexField {
                    name: name.to_string(),
                    field: "model",
                });
            }
            if self.model_catalog_path.is_some() {
                return Err(ProfileError::NotACodexField {
                    name: name.to_string(),
                    field: "model-catalog-path",
                });
            }
        }
        Ok(())
    }

    /// The file this profile is saved as, header comment and all.
    ///
    /// The comment is the reason this is not a bare `to_string`: the one thing
    /// an operator must not do to this file is add a key to it, and the file
    /// itself is the only place that sentence is read at the moment it would be
    /// ignored.
    pub fn to_toml(&self) -> String {
        let body = toml::to_string_pretty(self).expect("a profile is a flat table of strings");
        format!(
            "# A roundhouse launch profile, read by `topham`.\n\
             #\n\
             # NO SECRET BELONGS IN THIS FILE. `key-env` names the environment variable the\n\
             # turn key is read from; the key itself rides the operator's environment, which\n\
             # is what both launch generators require. `topham` refuses to load a profile\n\
             # carrying a `rh_`-shaped value, naming the field.\n\
             \n\
             {body}"
        )
    }

    /// Read the profile called `name`.
    pub fn load(env: &EnvMap, name: &str) -> Result<Self, ProfileError> {
        let path = Self::path(env, name)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(ProfileError::NotFound {
                    name: name.to_string(),
                    path,
                });
            }
            Err(source) => return Err(ProfileError::Io { path, source }),
        };
        Self::from_toml(&text, name).map_err(|error| match error {
            // The parse errors above are built without the resolved path,
            // because `from_toml` is also the seam a test and the TUI editor
            // use on text that never was a file. Restated here, where there is
            // one.
            ProfileError::Malformed { name, source, .. } => {
                ProfileError::Malformed { name, path, source }
            }
            other => other,
        })
    }

    /// Write the profile called `name`, creating the profiles directory.
    pub fn save(&self, env: &EnvMap, name: &str) -> Result<PathBuf, ProfileError> {
        self.validate(name)?;
        let path = Self::path(env, name)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ProfileError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(&path, self.to_toml()).map_err(|source| ProfileError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// Every profile name on this machine, in name order.
    ///
    /// A missing profiles directory is an empty list rather than an error: a
    /// deployment that has never been launched from is the ordinary first run,
    /// and the TUI's profile list has to render it.
    pub fn names(env: &EnvMap) -> Result<Vec<String>, ProfileError> {
        let dir = Self::directory(env)?;
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(ProfileError::Io { path: dir, source }),
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension()? != "toml" {
                    return None;
                }
                Some(path.file_stem()?.to_str()?.to_string())
            })
            .collect();
        names.sort();
        Ok(names)
    }

    /// `<config>/topham/profiles`.
    pub fn directory(env: &EnvMap) -> Result<PathBuf, ProfileError> {
        Ok(env::config_home(env)?.join(TOPHAM_DIR).join("profiles"))
    }

    /// `<config>/topham/profiles/<name>.toml`.
    pub fn path(env: &EnvMap, name: &str) -> Result<PathBuf, ProfileError> {
        check_name(name)?;
        Ok(Self::directory(env)?.join(format!("{name}.toml")))
    }

    /// `<data>/topham/<name>/codex-home` — the `CODEX_HOME` a codex launch
    /// writes into and points the client at.
    ///
    /// Per profile rather than per machine, and that is the isolation the whole
    /// codex stanza depends on: `auth.json` lives in a `CODEX_HOME`, so two
    /// profiles sharing one directory would share a login, and a
    /// `ForwardedLogin` profile would silently pick up whichever login the
    /// other profile's `codex login` last wrote.
    pub fn codex_home(env: &EnvMap, name: &str) -> Result<PathBuf, ProfileError> {
        check_name(name)?;
        Ok(env::data_home(env)?
            .join(TOPHAM_DIR)
            .join(name)
            .join("codex-home"))
    }
}

/// A profile name is one filename, and nothing else.
///
/// An allowlist rather than a rejection of `/` and `..`: the name is joined
/// onto a directory this crate chose, and the set of strings that do something
/// surprising when joined is longer than the set anybody would think to reject
/// (a leading `-` that the shell reads as a flag, a trailing dot, an absolute
/// path that replaces the join outright).
fn check_name(name: &str) -> Result<(), ProfileError> {
    let usable = !name.is_empty()
        && !name.starts_with('.')
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    match usable {
        true => Ok(()),
        false => Err(ProfileError::UnusableName {
            name: name.to_string(),
        }),
    }
}

/// The first field holding something that looks like a roundhouse secret, as a
/// dotted path.
///
/// Recursive over the whole document rather than a check on the struct's
/// fields, so a key parked in a table or an array this crate does not model is
/// still found. The *path* is returned and the value never is: an error message
/// is the last place a credential should be copied to, which is the rule
/// `ClaudeLaunchError::NotATurnKey` states for the same reason.
fn find_secret(value: &toml::Value, path: &str) -> Option<String> {
    let join = |key: &str| match path.is_empty() {
        true => key.to_string(),
        false => format!("{path}.{key}"),
    };
    match value {
        toml::Value::String(text) => looks_like_a_secret(text).then(|| path.to_string()),
        toml::Value::Table(table) => table
            .iter()
            .find_map(|(key, value)| find_secret(value, &join(key))),
        toml::Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(index, value)| find_secret(value, &join(&index.to_string()))),
        _ => None,
    }
}

/// Whether a string is a roundhouse secret, or close enough that it is one.
///
/// Prefix rather than [`has_valid_key_shape`], plus the launch sentinel — see
/// the module doc for why the coarse question is the right one here.
///
/// [`has_valid_key_shape`]: roundhouse_server::has_valid_key_shape
fn looks_like_a_secret(value: &str) -> bool {
    let value = value.trim();
    value == ROUNDHOUSE_API_KEY_SENTINEL
        || value.starts_with(KeyKind::Turn.prefix())
        || value.starts_with(KeyKind::Admin.prefix())
}

/// Every profile on this machine, in name order, each with the profile or the
/// reason it could not be read.
///
/// The inner `Result` is the point: one unreadable profile must not hide the
/// other nine, because the list is the surface an operator uses to *find* the
/// broken one. The outer one is the directory itself being unreachable, which
/// is a different question with a different answer.
pub type Listing = Vec<(String, Result<Profile, ProfileError>)>;

/// Every profile, as the TUI's list reads them. See [`Listing`].
pub fn load_all(env: &EnvMap) -> Result<Listing, ProfileError> {
    Ok(Profile::names(env)?
        .into_iter()
        .map(|name| {
            let profile = Profile::load(env, &name);
            (name, profile)
        })
        .collect())
}

#[cfg(test)]
mod tests;
