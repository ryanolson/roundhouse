// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Deployment configuration for the control plane, and key resolution.
//!
//! `ROUNDHOUSE_CONTROL_PLANE` names a JSON file, exactly the
//! `ROUNDHOUSE_CATALOG` idiom (`catalog_config.rs`): the config format *is*
//! the deserialized types, a named-but-unreadable file stops the process, and
//! a validate boundary rejects the shapes that would otherwise resolve
//! ambiguously at request time rather than at load time. Unset means
//! [`ControlPlane::Open`]: every request resolves to the single
//! `default/default` [`Principal`], with no key required anywhere — see
//! [`roundhouse_core::control`] for why that principal is a named value
//! rather than a `Default` impl.
//!
//! **No secrets in the file.** A key is `rh_turn_<43 base62 chars>` or
//! `rh_admin_<43 base62 chars>` — 32 CSPRNG bytes, base62-encoded, behind a
//! role prefix a [`KeyScope`] match can trust structurally. The config holds
//! only `sha256(secret)`, hex-encoded; the hash *is* the lookup key, so
//! resolving a presented key is a hash-and-look-up, not a comparison against
//! anything secret held in memory. SHA-256 rather than a slow KDF (Argon2,
//! bcrypt, ...) because 256 bits of CSPRNG entropy are not password-shaped: a
//! work factor defends against an attacker who can *guess* candidates from a
//! dictionary, and there is no dictionary of 32-byte random strings. Paying
//! 50-100ms of KDF work on every turn admission to defend against a threat
//! that does not exist would be the fix a future reader reaches for on
//! sight — which is why this paragraph exists.
//!
//! **The auth seam is [`ControlPlane::scope`], and it is the only one.** It
//! reads the header, applies the plan's error table — missing `Authorization`
//! is `401 missing_key`, a header that is not
//! `Bearer rh_(turn|admin)_<43 base62 chars>` is `401 malformed_key`, a hash
//! with no record is `401 unknown_key` — and hands back the [`KeyScope`] the
//! hash actually maps to. Two rows are not resolution's to decide and are
//! decided one step out, both still in this file:
//! [`ControlPlane::turn_principal`] refuses an admin key on a turn route with
//! `403 wrong_key_kind`, because only the route knows what it wanted, and
//! [`AuthError::OutOfNamespace`] refuses a session id from another tenant with
//! `403 session_out_of_namespace`. A transport that extracted the header for
//! itself would be a second place for the table to be almost right.
//!
//! **The namespace convention is one function pair.**
//! [`ControlPlane::qualify`] mints a name inside a caller's namespace and
//! [`ControlPlane::contains`] asks whether an id is one it could have minted.
//! Both surfaces mint through the first and check through the second, so an id
//! this deployment hands out is always an id the same key can then use.
//!
//! **Policy resolution rides the same seam as identity, because they are
//! resolved together.** A project entry's optional `"policy"` object becomes
//! its [`TurnPolicy`]; a key entry's optional `"overrides"` narrows that
//! policy via [`TurnPolicy::narrow`] — the *only* composition, so an override
//! can shrink what a key may do and never grow it. Both readings share one
//! [`PolicyConfig`] shape (the three axes `TurnPolicy` and `PolicyOverrides`
//! already agree on); they differ only in what an absent axis means, decided
//! by which `to_*` conversion is used. Two rules are enforced at the
//! boundary, not clamped at runtime, because an operator-authored file that
//! silently means less than it says is worse than one that fails to load: a
//! malformed glob is rejected naming the entry it came from, and an override
//! wider than its project's policy on any numeric axis is rejected naming
//! both the project and the key — clamping that quietly instead is exactly
//! [`TurnPolicy::narrow`]'s job at *runtime*, for the axes a config boundary
//! cannot see coming (an MCP overlay, later). [`ControlPlane::turn_admission`]
//! is where a request's key becomes both its [`Principal`] and its resolved
//! `Arc<TurnPolicy>` in one lookup; [`ControlPlane::turn_principal`] is now a
//! thin projection of it, kept for the surfaces that only need identity.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use roundhouse_core::control::{
    FilterError, FrontierCadence, PolicyOverrides, Principal, TargetFilter, TurnPolicy,
};
use roundhouse_core::ids::SessionId;

/// Path to a control-plane JSON file. Absent means [`ControlPlane::Open`].
pub const CONTROL_PLANE_VAR: &str = "ROUNDHOUSE_CONTROL_PLANE";

// ---------------------------------------------------------------------------
// Config file format
// ---------------------------------------------------------------------------

/// One entry of the config's `"projects"` array.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectEntry {
    pub id: String,
    /// A human label. Accepted and validated, read by nothing yet — consumed by
    /// the admin plane (M8), which lists projects for an operator. Kept in the
    /// format now so a deployment's config file does not have to be rewritten
    /// when that milestone lands.
    #[serde(default)]
    pub name: Option<String>,
    /// What this project's turns may do. Absent means
    /// [`TurnPolicy::unrestricted`] — the same routing a deployment with no
    /// control plane at all produces.
    #[serde(default)]
    pub policy: Option<PolicyConfig>,
}

/// One entry of the config's `"users"` array.
#[derive(Debug, Clone, Deserialize)]
pub struct UserEntry {
    pub id: String,
}

/// One entry of the config's `"keys"` array: a turn key's membership and hash.
#[derive(Debug, Clone, Deserialize)]
pub struct KeyEntry {
    pub project: String,
    pub user: String,
    pub key_sha256: String,
    /// A narrowing overlay on the owning project's policy, applied via
    /// [`TurnPolicy::narrow`]. Absent touches nothing — the key resolves to
    /// its project's policy exactly. An axis that would *widen* the project's
    /// policy is rejected at validation rather than clamped, naming both
    /// entries — see the module doc.
    #[serde(default)]
    pub overrides: Option<PolicyConfig>,
}

/// The shape of a `"policy"` (project) or `"overrides"` (key) object: the
/// three axes [`TurnPolicy`] and [`PolicyOverrides`] already agree on, read
/// as raw config data so validation can name the entry it belongs to before
/// any of it becomes a real [`TargetFilter`] or [`TurnPolicy`].
///
/// One shape serves both readings on purpose. They differ only in what an
/// absent axis means — "unrestricted" for a project's policy,
/// "leave alone" for a key's overrides — which is why that distinction is
/// decided by which `to_*` conversion below is called, not by two parallel
/// structs that would drift apart the first time a field is added to one and
/// not the other.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    pub min_quality: Option<f64>,
    /// Raw glob patterns, parsed (and thereby validated) by
    /// [`Self::allow_filter`] rather than at `serde` time — deserializing
    /// straight into a [`TargetFilter`] would fail with a bare parse error
    /// naming no entry, which is exactly what the module doc says this
    /// boundary must not do.
    pub allow: Option<Vec<String>>,
    pub frontier_cadence: Option<FrontierCadence>,
}

impl PolicyConfig {
    /// Field-level sanity shared by both readings: a quality floor inside
    /// `0.0..=1.0`, and a cadence that is a real window (`per_turns >= 1`)
    /// promising no more frontier traffic than it has turns to grant
    /// (`max_frontier <= per_turns`).
    fn validate_shape(&self, path: &str, entry: &str) -> Result<(), ControlPlaneError> {
        if let Some(min_quality) = self.min_quality
            && !(0.0..=1.0).contains(&min_quality)
        {
            return Err(ControlPlaneError::MinQualityOutOfRange {
                path: path.to_string(),
                entry: entry.to_string(),
                min_quality,
            });
        }
        if let Some(cadence) = self.frontier_cadence {
            if cadence.per_turns == 0 {
                return Err(ControlPlaneError::CadencePerTurnsZero {
                    path: path.to_string(),
                    entry: entry.to_string(),
                });
            }
            if cadence.max_frontier > cadence.per_turns {
                return Err(ControlPlaneError::CadenceExceedsWindow {
                    path: path.to_string(),
                    entry: entry.to_string(),
                    max_frontier: cadence.max_frontier,
                    per_turns: cadence.per_turns,
                });
            }
        }
        Ok(())
    }

    /// Parse `allow`, if present, naming `entry` on a malformed pattern.
    fn allow_filter(
        &self,
        path: &str,
        entry: &str,
    ) -> Result<Option<TargetFilter>, ControlPlaneError> {
        self.allow
            .as_ref()
            .map(|patterns| {
                TargetFilter::parse(patterns.iter().cloned()).map_err(|source| {
                    ControlPlaneError::MalformedGlob {
                        path: path.to_string(),
                        entry: entry.to_string(),
                        source,
                    }
                })
            })
            .transpose()
    }

    /// Read as a project's `"policy"`: an absent axis is unrestricted.
    fn to_project_policy(&self, path: &str, entry: &str) -> Result<TurnPolicy, ControlPlaneError> {
        self.validate_shape(path, entry)?;
        Ok(TurnPolicy {
            min_quality: self.min_quality.unwrap_or(0.0),
            allow: self
                .allow_filter(path, entry)?
                .unwrap_or_else(TargetFilter::allow_all),
            frontier_cadence: self.frontier_cadence,
        })
    }

    /// Read as a key's `"overrides"`: an absent axis touches nothing.
    fn to_overrides(&self, path: &str, entry: &str) -> Result<PolicyOverrides, ControlPlaneError> {
        self.validate_shape(path, entry)?;
        Ok(PolicyOverrides {
            min_quality: self.min_quality,
            allow: self.allow_filter(path, entry)?,
            frontier_cadence: self.frontier_cadence,
        })
    }
}

/// `"project `{id}`"`, the label every project-policy error names.
fn project_entry_label(id: &str) -> String {
    format!("project `{id}`")
}

/// `"key for project `{project}`, user `{user}`"`, the label every
/// key-overrides error names.
fn key_entry_label(project: &str, user: &str) -> String {
    format!("key for project `{project}`, user `{user}`")
}

/// What a deployment supplies at `ROUNDHOUSE_CONTROL_PLANE`.
///
/// The format is the deserialized shape, on purpose: a hand-maintained schema
/// document would drift from what `serde` actually accepts, and this way it
/// cannot.
#[derive(Debug, Clone, Deserialize)]
pub struct ControlPlaneConfig {
    pub projects: Vec<ProjectEntry>,
    pub users: Vec<UserEntry>,
    #[serde(default)]
    pub keys: Vec<KeyEntry>,
    /// Hashes of admin secrets. Unlike `keys`, these name no membership —
    /// `KeyScope::Admin` acts on the deployment, not from inside a project.
    #[serde(default)]
    pub admin_keys: Vec<String>,
    /// Each key's fully-resolved [`TurnPolicy`] — its project's policy
    /// narrowed by its own overrides — keyed by `key_sha256`.
    ///
    /// Computed once, by [`Self::validate`], and not by `serde`: resolving a
    /// key's policy requires its project's policy to already exist and the
    /// widening check to have already run, both of which only `validate` has
    /// done by the time this is populated. `#[serde(skip)]` rather than a
    /// second pass over `keys` in [`ControlPlane::configured`] — the two
    /// would otherwise be two places a malformed glob could be judged
    /// differently, one of them silent (no path, no entry name) because it
    /// would run after the boundary that names them.
    #[serde(skip)]
    key_effective_policies: HashMap<String, TurnPolicy>,
}

#[derive(Debug, thiserror::Error)]
pub enum ControlPlaneError {
    #[error("could not read control-plane config `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse control-plane config `{path}`: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "control-plane config `{path}`: project id `{id}` is not a valid slug -- ids must \
         match `^[a-z0-9][a-z0-9_-]{{0,63}}$` so a `/` can never appear inside one and the \
         session namespace `{{project}}/{{user}}/...` stays unambiguous"
    )]
    BadProjectSlug { path: String, id: String },
    #[error(
        "control-plane config `{path}`: user id `{id}` is not a valid slug -- ids must match \
         `^[a-z0-9][a-z0-9_-]{{0,63}}$`, for the same reason project ids do"
    )]
    BadUserSlug { path: String, id: String },
    #[error("control-plane config `{path}`: project id `{id}` is declared more than once")]
    DuplicateProject { path: String, id: String },
    #[error("control-plane config `{path}`: user id `{id}` is declared more than once")]
    DuplicateUser { path: String, id: String },
    #[error(
        "control-plane config `{path}`: a key names project `{project}`, which no `projects` \
         entry declares"
    )]
    UnknownProject { path: String, project: String },
    #[error(
        "control-plane config `{path}`: a key names user `{user}`, which no `users` entry \
         declares"
    )]
    UnknownUser { path: String, user: String },
    #[error(
        "control-plane config `{path}`: `{key_sha256}` is not a well-formed sha256 hex digest \
         -- expected 64 lowercase hex characters"
    )]
    MalformedHash { path: String, key_sha256: String },
    #[error(
        "control-plane config `{path}`: hash `{key_sha256}` is declared more than once, across \
         `keys` and/or `admin_keys` -- one secret must resolve to exactly one scope"
    )]
    DuplicateHash { path: String, key_sha256: String },
    #[error(
        "control-plane config `{path}`: {entry}'s min_quality {min_quality} is outside \
         0.0..=1.0"
    )]
    MinQualityOutOfRange {
        path: String,
        entry: String,
        min_quality: f64,
    },
    #[error(
        "control-plane config `{path}`: {entry}'s frontier_cadence.per_turns is 0 -- a window \
         must be at least one turn wide"
    )]
    CadencePerTurnsZero { path: String, entry: String },
    #[error(
        "control-plane config `{path}`: {entry}'s frontier_cadence allows {max_frontier} \
         frontier dispatch(es) in a window of {per_turns} turn(s) -- max_frontier must not \
         exceed per_turns"
    )]
    CadenceExceedsWindow {
        path: String,
        entry: String,
        max_frontier: u32,
        per_turns: u32,
    },
    #[error("control-plane config `{path}`: {entry}'s allow pattern is malformed: {source}")]
    MalformedGlob {
        path: String,
        entry: String,
        #[source]
        source: FilterError,
    },
    #[error(
        "control-plane config `{path}`: {key_entry}'s overrides are wider than \
         {project_entry}'s policy on {axes:?} -- an override may only narrow the policy of the \
         project it belongs to"
    )]
    OverrideWiderThanProject {
        path: String,
        project_entry: String,
        key_entry: String,
        axes: Vec<&'static str>,
    },
}

/// `true` for `^[a-z0-9][a-z0-9_-]{0,63}$`.
///
/// Hand-rolled rather than the `regex` crate: the alphabet is three ASCII
/// classes and a length bound, which a byte scan checks as reliably as a
/// compiled pattern, for one dependency fewer than "nothing heavier"
/// (decision 11) allows.
fn is_valid_slug(id: &str) -> bool {
    if id.is_empty() || id.len() > 64 {
        return false;
    }
    let mut chars = id.chars();
    let first = chars.next().expect("checked non-empty above");
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// `true` for 64 lowercase hex characters — what `hex::encode(Sha256::digest(..))`
/// produces, and the only shape a stored hash can be looked up by.
fn is_valid_sha256_hex(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

impl ControlPlaneConfig {
    pub fn from_json(json: &str, path: &str) -> Result<Self, ControlPlaneError> {
        let mut config: Self =
            serde_json::from_str(json).map_err(|source| ControlPlaneError::Parse {
                path: path.to_string(),
                source,
            })?;
        config.validate(path)?;
        Ok(config)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ControlPlaneError> {
        let path = path.as_ref();
        let display = path.display().to_string();
        let json = std::fs::read_to_string(path).map_err(|source| ControlPlaneError::Read {
            path: display.clone(),
            source,
        })?;
        Self::from_json(&json, &display)
    }

    /// Refuse a config that cannot resolve a presented key to exactly one
    /// scope, or that names a membership half of which does not exist.
    ///
    /// Order matters for the error a caller sees, not for correctness: slugs
    /// are checked before the ids are trusted as lookup keys, and a key's
    /// project/user references are checked before its hash, so a config with
    /// several problems reports the one closest to the top of the file.
    fn validate(&mut self, path: &str) -> Result<(), ControlPlaneError> {
        let mut project_ids: HashSet<&str> = HashSet::new();
        // Every project's effective policy, resolved once here so the keys
        // loop below can narrow against a real `TurnPolicy` rather than
        // re-parsing raw config data (and risking a second, differently-worded
        // judgment of the same glob).
        let mut project_policies: HashMap<&str, TurnPolicy> = HashMap::new();
        for project in &self.projects {
            if !is_valid_slug(&project.id) {
                return Err(ControlPlaneError::BadProjectSlug {
                    path: path.to_string(),
                    id: project.id.clone(),
                });
            }
            if !project_ids.insert(project.id.as_str()) {
                return Err(ControlPlaneError::DuplicateProject {
                    path: path.to_string(),
                    id: project.id.clone(),
                });
            }
            let entry = project_entry_label(&project.id);
            let policy = match &project.policy {
                Some(policy_config) => policy_config.to_project_policy(path, &entry)?,
                None => TurnPolicy::unrestricted(),
            };
            project_policies.insert(project.id.as_str(), policy);
        }

        let mut user_ids: HashSet<&str> = HashSet::new();
        for user in &self.users {
            if !is_valid_slug(&user.id) {
                return Err(ControlPlaneError::BadUserSlug {
                    path: path.to_string(),
                    id: user.id.clone(),
                });
            }
            if !user_ids.insert(user.id.as_str()) {
                return Err(ControlPlaneError::DuplicateUser {
                    path: path.to_string(),
                    id: user.id.clone(),
                });
            }
        }

        // One secret must resolve one way: a hash cannot be a member of both
        // `keys` and `admin_keys`, nor appear twice within either.
        let mut hashes: HashSet<&str> = HashSet::new();
        // Every key's fully-resolved policy: its project's, narrowed by its
        // own overrides. Built here rather than in `ControlPlane::configured`
        // — see the field's doc comment.
        let mut key_effective_policies: HashMap<String, TurnPolicy> = HashMap::new();
        for key in &self.keys {
            if !project_ids.contains(key.project.as_str()) {
                return Err(ControlPlaneError::UnknownProject {
                    path: path.to_string(),
                    project: key.project.clone(),
                });
            }
            if !user_ids.contains(key.user.as_str()) {
                return Err(ControlPlaneError::UnknownUser {
                    path: path.to_string(),
                    user: key.user.clone(),
                });
            }
            if !is_valid_sha256_hex(&key.key_sha256) {
                return Err(ControlPlaneError::MalformedHash {
                    path: path.to_string(),
                    key_sha256: key.key_sha256.clone(),
                });
            }
            if !hashes.insert(key.key_sha256.as_str()) {
                return Err(ControlPlaneError::DuplicateHash {
                    path: path.to_string(),
                    key_sha256: key.key_sha256.clone(),
                });
            }

            // `project_ids.contains` above already proved this project
            // exists, so it is in `project_policies` too — both loops walk
            // the same `self.projects`.
            let project_policy = project_policies
                .get(key.project.as_str())
                .expect("a project checked present above was resolved to a policy above");
            let key_entry = key_entry_label(&key.project, &key.user);
            let overrides = match &key.overrides {
                Some(overrides_config) => {
                    let overrides = overrides_config.to_overrides(path, &key_entry)?;
                    let widened = project_policy.widenings_of(&overrides);
                    if !widened.is_empty() {
                        return Err(ControlPlaneError::OverrideWiderThanProject {
                            path: path.to_string(),
                            project_entry: project_entry_label(&key.project),
                            key_entry,
                            axes: widened,
                        });
                    }
                    overrides
                }
                None => PolicyOverrides::default(),
            };
            key_effective_policies
                .insert(key.key_sha256.clone(), project_policy.narrow(&overrides));
        }

        for hash in &self.admin_keys {
            if !is_valid_sha256_hex(hash) {
                return Err(ControlPlaneError::MalformedHash {
                    path: path.to_string(),
                    key_sha256: hash.clone(),
                });
            }
            if !hashes.insert(hash.as_str()) {
                return Err(ControlPlaneError::DuplicateHash {
                    path: path.to_string(),
                    key_sha256: hash.clone(),
                });
            }
        }

        self.key_effective_policies = key_effective_policies;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// `true` for `rh_(turn|admin)_` followed by 43 base62 characters — the shape
/// a *presented* secret must have before its hash is even looked up, so an
/// obviously-wrong header never reaches the hash table.
///
/// Below the divider with the resolver rather than above it with the config
/// validators: this one is about what a *client sends*, and the ones above are
/// about what an operator *wrote in a file*. They look alike and are checked at
/// opposite ends of the system.
fn has_valid_key_shape(secret: &str) -> bool {
    let tail = secret
        .strip_prefix("rh_turn_")
        .or_else(|| secret.strip_prefix("rh_admin_"));
    match tail {
        Some(tail) => tail.len() == 43 && tail.chars().all(|c| c.is_ascii_alphanumeric()),
        None => false,
    }
}

/// What a presented key is allowed to do.
///
/// An enum rather than a `role` field beside an optional principal, because the
/// two arms carry genuinely different data: an admin key has no membership to
/// spend against, and a turn key has no business mutating tenancy. Matching on
/// this at a surface is what makes "an admin key served a turn" a shape the
/// code cannot express, rather than a check somebody has to remember.
///
/// Beside [`ControlPlane::resolve`], its only producer, rather than in
/// `roundhouse-core`'s control vocabulary: a scope is a fact about a
/// *credential*, and core deliberately knows nothing about credentials — see
/// [`roundhouse_core::control`] on why a [`Principal`] carries no key. Nothing
/// below this crate has any use for the distinction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyScope {
    /// Pays for turns as one membership.
    Turn(Principal),
    /// Reads and writes the control plane itself. Deliberately carries no
    /// principal: an admin acts on the deployment, not from inside a project.
    Admin,
}

/// The two modes a deployment runs in, and the one seam every surface's
/// gating goes through.
///
/// `Open` and `Configured` are not "auth on" and "auth off" as a boolean plus
/// a lookup table — they are different enough (no key anywhere vs. a key
/// everywhere, unnamespaced session ids vs. namespaced ones) that a single
/// struct with an `enabled: bool` would leave every call site re-deriving
/// which behavior follows from which flag. Matching on the enum instead makes
/// "did I handle both modes" a question the compiler asks.
#[derive(Debug, Clone)]
pub enum ControlPlane {
    /// No key required anywhere; every request resolves to
    /// [`Principal::default_open`].
    Open,
    /// A key is required; `resolve` looks up its hash.
    Configured {
        /// `sha256(secret)` hex, to the membership it authenticates and the
        /// effective [`TurnPolicy`] resolved for it at load time (its
        /// project's policy narrowed by its own overrides — see
        /// [`ControlPlaneConfig::validate`]).
        turn_keys: HashMap<String, (Principal, Arc<TurnPolicy>)>,
        /// `sha256(secret)` hex, for keys with no membership to spend as.
        admin_keys: HashSet<String>,
    },
}

impl ControlPlane {
    /// Build the runtime resolver from a validated config.
    ///
    /// Takes `self` by value rather than `&ControlPlaneConfig`: the config's
    /// `Vec`s exist only to be turned into these two lookup tables, and there
    /// is no second reader of the parsed form once resolution is wired.
    pub fn configured(config: ControlPlaneConfig) -> Self {
        let ControlPlaneConfig {
            projects: _,
            users: _,
            keys,
            admin_keys,
            key_effective_policies,
        } = config;
        let turn_keys = keys
            .into_iter()
            .map(|key| {
                // Always present: `validate` inserts an entry (the
                // project's own policy, narrowed by nothing) for every key
                // that reaches here, overridden or not.
                let policy = key_effective_policies
                    .get(&key.key_sha256)
                    .cloned()
                    .unwrap_or_else(TurnPolicy::unrestricted);
                let principal = Principal::new(key.project, key.user);
                (key.key_sha256, (principal, Arc::new(policy)))
            })
            .collect();
        let admin_keys = admin_keys.into_iter().collect();
        ControlPlane::Configured {
            turn_keys,
            admin_keys,
        }
    }

    /// [`Self::Open`], shared.
    ///
    /// Every router takes its plane as `Arc<ControlPlane>`, and an unconfigured
    /// one is a value rather than a decision — this is the sentence that says
    /// so, once, instead of `Arc::new(ControlPlane::Open)` at each of the
    /// fixtures and call sites that want it.
    pub fn open() -> Arc<Self> {
        Arc::new(ControlPlane::Open)
    }

    /// The control plane named by [`CONTROL_PLANE_VAR`], or [`Self::Open`] if
    /// the variable is unset.
    ///
    /// A variable that *is* set but names an unreadable or malformed file
    /// stops the process, mirroring `catalog_config::from_env`: starting
    /// anyway would serve every request as if no key were required, which is
    /// the exact failure a deployment sets this variable to prevent.
    pub fn from_env() -> Result<Self, ControlPlaneError> {
        match std::env::var(CONTROL_PLANE_VAR) {
            Ok(path) if !path.trim().is_empty() => {
                ControlPlaneConfig::load(path.trim()).map(Self::configured)
            }
            _ => Ok(Self::Open),
        }
    }

    // -----------------------------------------------------------------------
    // The namespace convention
    // -----------------------------------------------------------------------

    /// The prefix every session id belonging to `principal` carries in this
    /// deployment, or `None` where ids are exactly what the client named.
    ///
    /// The one place the *mode* decides whether there is a namespace at all;
    /// the one place the namespace's *shape* is decided is
    /// [`Principal::namespace_prefix`], which this defers to. Private, because
    /// no surface should be answering that question for itself: the two things
    /// a surface actually wants are [`Self::qualify`] and [`Self::contains`],
    /// and going through them is what keeps minting and checking two views of
    /// one convention rather than two conventions.
    ///
    /// `None` in [`Self::Open`] is not "the empty prefix": an unconfigured
    /// deployment's sessions keep the ids they already have, and prepending
    /// `default/default/` to them would strand every session in the store the
    /// day the process restarted with this code.
    fn session_prefix(&self, principal: &Principal) -> Option<String> {
        match self {
            ControlPlane::Open => None,
            ControlPlane::Configured { .. } => Some(principal.namespace_prefix()),
        }
    }

    /// `name` as it is spelled inside `principal`'s namespace.
    ///
    /// Both surfaces mint session ids through here — the native one from a
    /// generated id, the responses one from a client-chosen cache key — because
    /// a name the client chooses is a name two clients can choose. In
    /// [`Self::Open`] the answer is the name verbatim.
    pub fn qualify(&self, principal: &Principal, name: &str) -> String {
        match self.session_prefix(principal) {
            Some(prefix) => format!("{prefix}{name}"),
            None => name.to_string(),
        }
    }

    /// Whether `session_id` is one [`Self::qualify`] could have produced for
    /// `principal`.
    ///
    /// The exact inverse of `qualify`, and stated as such on purpose: the pair
    /// is what makes an id this deployment handed out an id the same key can
    /// then use. A mint and a check that disagreed by one character would
    /// create sessions that are immediately unreachable, and the client would
    /// see a 403 on the id we just gave it.
    ///
    /// Always `true` in [`Self::Open`], which is what keeps an unconfigured
    /// deployment's client-supplied ids working unchanged.
    pub fn contains(&self, principal: &Principal, session_id: &SessionId) -> bool {
        match self.session_prefix(principal) {
            None => true,
            Some(prefix) => session_id.as_str().starts_with(&prefix),
        }
    }

    // -----------------------------------------------------------------------
    // Authenticating a request
    // -----------------------------------------------------------------------

    /// The scope the request's `Authorization` header authenticates.
    ///
    /// The auth seam, and the only one: every surface that gates on a key comes
    /// through here or through [`Self::turn_principal`] / [`Self::turn_admission`]
    /// below, so the header name, the ASCII rule and the error table are read
    /// out of one function rather than re-derived per transport.
    ///
    /// A header that is present but not ASCII is malformed rather than missing.
    /// Reporting it as missing would tell a client to add a key it already
    /// sent, which is the least actionable of the answers in the table.
    pub fn scope(&self, headers: &HeaderMap) -> Result<KeyScope, AuthError> {
        let header = self.header_str(headers)?;
        self.resolve(header)
    }

    /// The membership a turn-serving surface's caller spends as.
    ///
    /// A thin projection of [`Self::turn_admission`] for the surfaces that
    /// only need identity and have not yet been threaded through to consult a
    /// policy — see that method for the shared logic, including why an admin
    /// key is refused here rather than quietly given a principal of its own.
    pub fn turn_principal(&self, headers: &HeaderMap) -> Result<Principal, AuthError> {
        self.turn_admission(headers)
            .map(|admission| admission.principal)
    }

    /// The membership a turn-serving surface's caller spends as, together with
    /// its resolved [`TurnPolicy`] — the admission seam decision 10 names.
    ///
    /// One lookup produces both, because both are resolved from the same key:
    /// a second function reaching into `Configured { turn_keys, .. }` on its
    /// own would be a second place that table's shape could be gotten wrong.
    /// `WrongKeyKind` is decided here, not in [`Self::authenticate`], because
    /// whether an admin key is the wrong key depends on what the route
    /// wanted — an admin acts on the deployment and has no membership to bill,
    /// and the alternative — minting one — would put spend on a row no
    /// project owns.
    ///
    /// In [`Self::Open`] this always resolves to
    /// [`Principal::default_open`] paired with [`TurnPolicy::unrestricted`],
    /// regardless of the header: an unconfigured deployment authenticates
    /// nothing, so there is no wrong answer a header could produce, and no
    /// policy narrower than the one every pre-control-plane deployment already
    /// routes under.
    pub fn turn_admission(&self, headers: &HeaderMap) -> Result<Admission, AuthError> {
        let header = self.header_str(headers)?;
        match self.authenticate(header)? {
            Authenticated::Turn(principal, policy) => Ok(Admission { principal, policy }),
            Authenticated::Admin => Err(AuthError::WrongKeyKind),
        }
    }

    /// Read `headers`' `Authorization` value as UTF-8, or refuse it as
    /// malformed. Shared by [`Self::scope`] and [`Self::turn_admission`] so
    /// the ASCII rule is read out of one function rather than two.
    fn header_str<'a>(&self, headers: &'a HeaderMap) -> Result<Option<&'a str>, AuthError> {
        match headers.get(AUTHORIZATION) {
            None => Ok(None),
            Some(value) => value
                .to_str()
                .map(Some)
                .map_err(|_| AuthError::MalformedKey),
        }
    }

    /// [`Self::authenticate`], with the policy dropped — [`Self::scope`]'s
    /// pure-function core, kept separate so the tests below can exercise
    /// resolution as a function of a string without a [`HeaderMap`] to build.
    fn resolve(&self, authorization_header: Option<&str>) -> Result<KeyScope, AuthError> {
        match self.authenticate(authorization_header)? {
            Authenticated::Turn(principal, _policy) => Ok(KeyScope::Turn(principal)),
            Authenticated::Admin => Ok(KeyScope::Admin),
        }
    }

    /// Resolve a presented `Authorization` header value to what it
    /// authenticates: a membership and its resolved policy, or the admin
    /// scope.
    ///
    /// The error table (decision 3) is: missing header -> `MissingKey`;
    /// present but not `Bearer rh_(turn|admin)_<43 chars>` -> `MalformedKey`;
    /// well-shaped but no record of its hash -> `UnknownKey`. `WrongKeyKind`
    /// is not decided here — see [`Self::turn_admission`] — because this
    /// function has no notion of which surface is asking.
    ///
    /// Private, and takes the header value rather than the map, for the same
    /// reason [`Self::resolve`] does: [`Self::scope`] and
    /// [`Self::turn_admission`] are the two public ways in, and both read the
    /// header through [`Self::header_str`] first.
    fn authenticate(&self, authorization_header: Option<&str>) -> Result<Authenticated, AuthError> {
        match self {
            ControlPlane::Open => {
                let open = Admission::open();
                Ok(Authenticated::Turn(open.principal, open.policy))
            }
            ControlPlane::Configured {
                turn_keys,
                admin_keys,
            } => {
                let header = authorization_header.ok_or(AuthError::MissingKey)?;
                let secret = header
                    .strip_prefix("Bearer ")
                    .ok_or(AuthError::MalformedKey)?;
                if !has_valid_key_shape(secret) {
                    return Err(AuthError::MalformedKey);
                }
                let hash = hex::encode(Sha256::digest(secret.as_bytes()));
                if admin_keys.contains(&hash) {
                    return Ok(Authenticated::Admin);
                }
                if let Some((principal, policy)) = turn_keys.get(&hash) {
                    return Ok(Authenticated::Turn(principal.clone(), Arc::clone(policy)));
                }
                Err(AuthError::UnknownKey)
            }
        }
    }
}

/// What a presented `Authorization` header authenticates, with the data each
/// arm actually carries — the internal counterpart to [`KeyScope`], which
/// [`ControlPlane::scope`] projects this down to for callers that only need
/// the tenancy-shape guarantee "an admin key served a turn" is unrepresentable.
/// [`ControlPlane::turn_admission`] keeps the policy this drops; [`KeyScope`]
/// exists for the surfaces that predate that seam.
enum Authenticated {
    Turn(Principal, Arc<TurnPolicy>),
    Admin,
}

/// A turn-serving caller, resolved once at admission: which membership, and
/// what that membership's turns may do.
///
/// The pair decision 10 asks for, rather than two calls a caller could make
/// out of order or against two different headers: [`ControlPlane::turn_admission`]
/// is the one place that produces it, so "the principal from one key and the
/// policy from another" is not a mistake this crate's callers can make.
#[derive(Debug, Clone)]
pub struct Admission {
    pub principal: Principal,
    pub policy: Arc<TurnPolicy>,
}

impl Admission {
    /// What an unconfigured deployment admits every request as: the one
    /// built-in membership, under the policy that changes no routing decision.
    ///
    /// Named once rather than spelled at each site, and it is the value
    /// [`ControlPlane::Open`] itself resolves to — so this is the definition
    /// of open mode's admission and not a convenience beside it. The two
    /// halves have to travel together: an open deployment that paired the
    /// default principal with anything narrower than
    /// [`TurnPolicy::unrestricted`] would re-route workloads that predate the
    /// control plane, which is the one thing turning it on must not do.
    pub fn open() -> Self {
        Self {
            principal: Principal::default_open(),
            policy: Arc::new(TurnPolicy::unrestricted()),
        }
    }
}

/// Why a request never reached a session: decision 3's error table, whole.
///
/// All five rows, including the two no single function decides on its own —
/// `WrongKeyKind` comes from [`ControlPlane::turn_admission`] (and its
/// projection [`ControlPlane::turn_principal`]), which know what the surface
/// wanted, and `OutOfNamespace` from the surface that was handed a
/// session id. They live here anyway, because the table is the contract: a
/// refusal spelled somewhere else is a refusal a client's error handling has
/// never heard of.
///
/// `Clone` but not `Copy`: `OutOfNamespace` carries the prefix that would have
/// worked, which is the only actionable part of that answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("missing Authorization header")]
    MissingKey,
    #[error("Authorization header is not `Bearer rh_(turn|admin)_<43 base62 chars>`")]
    MalformedKey,
    #[error("key not recognized")]
    UnknownKey,
    #[error("this key's scope may not be used on this surface")]
    WrongKeyKind,
    /// A session id the caller's key does not reach.
    ///
    /// 403 rather than 404: the caller is authenticated and the id is
    /// well-formed, it simply belongs to somebody else. The message names the
    /// prefix that *would* have worked and never says whether the session
    /// exists — namespaced ids are guessable in a way cache keys were not, and
    /// "not found" versus "forbidden" would turn every session route into an
    /// existence oracle over other tenants' sessions.
    #[error("a session id must begin with `{prefix}` for this key")]
    OutOfNamespace { prefix: String },
}

impl AuthError {
    /// The stable machine-readable code — same field name `http.rs`'s
    /// `ApiError` uses, so a client parsing either body sees one convention.
    pub fn code(&self) -> &'static str {
        match self {
            AuthError::MissingKey => "missing_key",
            AuthError::MalformedKey => "malformed_key",
            AuthError::UnknownKey => "unknown_key",
            AuthError::WrongKeyKind => "wrong_key_kind",
            AuthError::OutOfNamespace { .. } => "session_out_of_namespace",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            AuthError::MissingKey | AuthError::MalformedKey | AuthError::UnknownKey => {
                StatusCode::UNAUTHORIZED
            }
            AuthError::WrongKeyKind | AuthError::OutOfNamespace { .. } => StatusCode::FORBIDDEN,
        }
    }
}

/// Mirrors `http.rs`'s `ApiError` body shape (`{"error": {"code", "message"}}`)
/// without depending on that type, whose fields are private to its own
/// module. Written beside [`AuthError`] rather than as a `From<AuthError> for
/// ApiError` in `http.rs`, because this stage does not touch `http.rs` — the
/// surface that wires an extractor in front of a route is the one that
/// decides whether it wants this response directly or converted once more.
impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "code": self.code(), "message": self.to_string() } });
        (self.status(), axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed secret/hash pairs so tests read as data, not as a call to a hash
    // function the reader has to trust. Regenerate with:
    //   python3 -c "import hashlib; print(hashlib.sha256(b'...').hexdigest())"
    const TURN_SECRET: &str = "rh_turn_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const TURN_HASH: &str = "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677ddfd";
    const ADMIN_SECRET: &str = "rh_admin_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    const ADMIN_HASH: &str = "d2166d25b0938bced2c878c396356867ee6f05abaa02f4ad4b80a3cdbe5c1ff3";
    // Well-shaped and well-hashed, but declared nowhere in any fixture config.
    const UNKNOWN_SECRET: &str = "rh_turn_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";

    fn sample_config() -> &'static str {
        r#"{
          "projects": [{ "id": "acme", "name": "Acme Corp" }],
          "users": [{ "id": "ada" }],
          "keys": [
            { "project": "acme", "user": "ada", "key_sha256": "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677ddfd" }
          ],
          "admin_keys": [
            "d2166d25b0938bced2c878c396356867ee6f05abaa02f4ad4b80a3cdbe5c1ff3"
          ]
        }"#
    }

    #[test]
    fn a_named_but_unreadable_control_plane_file_stops_the_process() {
        let error = ControlPlaneConfig::load("/nonexistent/roundhouse-control-plane-test.json")
            .unwrap_err();
        assert!(
            matches!(error, ControlPlaneError::Read { .. }),
            "an unreadable file must be a Read error, not silently treated as Open: {error}"
        );
        assert!(
            error
                .to_string()
                .contains("roundhouse-control-plane-test.json")
        );
    }

    #[test]
    fn a_duplicate_key_hash_is_rejected_naming_the_key() {
        // The same hash appears once in `keys` and once more in `admin_keys`:
        // one secret must resolve to exactly one scope.
        let json = r#"{
          "projects": [{ "id": "acme" }],
          "users": [{ "id": "ada" }],
          "keys": [
            { "project": "acme", "user": "ada", "key_sha256": "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677ddfd" }
          ],
          "admin_keys": [
            "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677ddfd"
          ]
        }"#;
        let error = ControlPlaneConfig::from_json(json, "test").unwrap_err();
        match error {
            ControlPlaneError::DuplicateHash { key_sha256, .. } => {
                assert_eq!(key_sha256, TURN_HASH);
            }
            other => panic!("expected DuplicateHash, got {other:?}"),
        }
    }

    #[test]
    fn a_key_referencing_an_unknown_project_or_user_is_rejected() {
        let unknown_project = r#"{
          "projects": [{ "id": "acme" }],
          "users": [{ "id": "ada" }],
          "keys": [
            { "project": "ghost", "user": "ada", "key_sha256": "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677ddfd" }
          ]
        }"#;
        let error = ControlPlaneConfig::from_json(unknown_project, "test").unwrap_err();
        match error {
            ControlPlaneError::UnknownProject { project, .. } => assert_eq!(project, "ghost"),
            other => panic!("expected UnknownProject, got {other:?}"),
        }

        let unknown_user = r#"{
          "projects": [{ "id": "acme" }],
          "users": [{ "id": "ada" }],
          "keys": [
            { "project": "acme", "user": "ghost", "key_sha256": "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677ddfd" }
          ]
        }"#;
        let error = ControlPlaneConfig::from_json(unknown_user, "test").unwrap_err();
        match error {
            ControlPlaneError::UnknownUser { user, .. } => assert_eq!(user, "ghost"),
            other => panic!("expected UnknownUser, got {other:?}"),
        }
    }

    #[test]
    fn a_bad_slug_is_rejected() {
        let cases = ["Acme", "ac/me", "", &"a".repeat(65)];
        for id in cases {
            let json = format!(
                r#"{{ "projects": [{{ "id": {id:?} }}], "users": [] }}"#,
                id = id
            );
            let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
            assert!(
                matches!(error, ControlPlaneError::BadProjectSlug { .. }),
                "slug `{id}` should have been rejected, got {error:?}"
            );
        }

        // A slug that is valid at exactly the length bound is accepted: the
        // bound is `<= 64`, not `< 64`.
        let boundary = "a".repeat(64);
        let json = format!(r#"{{ "projects": [{{ "id": {boundary:?} }}], "users": [] }}"#);
        ControlPlaneConfig::from_json(&json, "test")
            .expect("a 64-character slug is exactly at the bound and must validate");
    }

    #[test]
    fn a_malformed_sha256_is_rejected() {
        let cases = [
            "not-hex-at-all",
            "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677dd", // 63 chars
            "0BD5182863262C911D4479F1B25FEC5F3E6846653B9028E65F61B2B33677DDF", // uppercase
        ];
        for hash in cases {
            let json = format!(
                r#"{{
                  "projects": [{{ "id": "acme" }}],
                  "users": [{{ "id": "ada" }}],
                  "keys": [{{ "project": "acme", "user": "ada", "key_sha256": {hash:?} }}]
                }}"#
            );
            let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
            assert!(
                matches!(error, ControlPlaneError::MalformedHash { .. }),
                "hash `{hash}` should have been rejected, got {error:?}"
            );
        }
    }

    #[test]
    fn open_mode_resolves_every_request_to_the_default_principal() {
        let plane = ControlPlane::Open;
        for header in [None, Some("Bearer rh_turn_garbage"), Some("not even close")] {
            let scope = plane.resolve(header).expect("Open mode never refuses");
            assert_eq!(scope, KeyScope::Turn(Principal::default_open()));
        }
    }

    #[test]
    fn a_missing_header_is_missing_key_and_a_wrong_shape_is_malformed_key() {
        let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
        let plane = ControlPlane::configured(config);

        assert_eq!(plane.resolve(None), Err(AuthError::MissingKey));
        assert_eq!(
            plane.resolve(Some("not a bearer header")),
            Err(AuthError::MalformedKey)
        );
        assert_eq!(
            plane.resolve(Some("Bearer rh_turn_tooshort")),
            Err(AuthError::MalformedKey)
        );
        assert_eq!(
            plane.resolve(Some("Bearer rh_something_else")),
            Err(AuthError::MalformedKey)
        );
    }

    #[test]
    fn an_unknown_hash_is_unknown_key_and_a_known_turn_key_resolves_to_its_principal() {
        let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
        let plane = ControlPlane::configured(config);

        let unknown = format!("Bearer {UNKNOWN_SECRET}");
        assert_eq!(plane.resolve(Some(&unknown)), Err(AuthError::UnknownKey));

        let known = format!("Bearer {TURN_SECRET}");
        assert_eq!(
            plane.resolve(Some(&known)),
            Ok(KeyScope::Turn(Principal::new("acme", "ada")))
        );
    }

    #[test]
    fn an_admin_hash_resolves_to_key_scope_admin() {
        // The fixture's `admin_keys` entry must actually be `sha256(ADMIN_SECRET)`,
        // or this test would pass for the wrong reason (an unrelated hash
        // that happens to also be an admin key).
        assert!(
            sample_config().contains(ADMIN_HASH),
            "ADMIN_HASH and ADMIN_SECRET have drifted apart"
        );

        let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
        let plane = ControlPlane::configured(config);

        let header = format!("Bearer {ADMIN_SECRET}");
        assert_eq!(plane.resolve(Some(&header)), Ok(KeyScope::Admin));
    }

    #[test]
    fn a_header_that_is_not_ascii_is_malformed_rather_than_missing() {
        // The one thing `scope` does that `resolve` cannot: it reads the header
        // map. Reporting an unreadable header as *missing* would tell a client
        // to add a key it already sent, which is the least actionable answer in
        // the table — and this is the only test that can say so, since `scope`
        // is now the only place the header is read.
        let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
        let plane = ControlPlane::configured(config);

        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            axum::http::HeaderValue::from_bytes(b"Bearer \xff\xfe").expect("a non-ASCII value"),
        );
        assert_eq!(plane.scope(&headers), Err(AuthError::MalformedKey));

        assert_eq!(
            plane.scope(&HeaderMap::new()),
            Err(AuthError::MissingKey),
            "no header at all is still the missing-key row"
        );
    }

    #[test]
    fn an_admin_key_may_not_spend_as_a_membership() {
        let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
        let plane = ControlPlane::configured(config);

        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            format!("Bearer {ADMIN_SECRET}")
                .parse()
                .expect("a valid value"),
        );
        assert_eq!(plane.scope(&headers), Ok(KeyScope::Admin));
        assert_eq!(
            plane.turn_principal(&headers),
            Err(AuthError::WrongKeyKind),
            "an admin has no membership to bill, and minting one would put spend \
             on a row no project owns"
        );
    }

    #[test]
    fn qualify_and_contains_are_inverses_in_both_modes() {
        // The property the namespace rests on: an id this deployment mints for
        // a caller is an id that caller's own key then reaches. A mint and a
        // check that disagreed by one character would create sessions that are
        // immediately unreachable.
        let ada = Principal::new("acme", "ada");
        let bob = Principal::new("globex", "bob");

        let open = ControlPlane::Open;
        assert_eq!(
            open.qualify(&ada, "main"),
            "main",
            "no namespace, no prefix"
        );
        assert!(open.contains(&ada, &SessionId::new(open.qualify(&ada, "main"))));
        assert!(
            open.contains(&ada, &SessionId::new("anything at all")),
            "an unconfigured deployment's client-supplied ids keep working"
        );

        let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
        let configured = ControlPlane::configured(config);
        assert_eq!(configured.qualify(&ada, "main"), "acme/ada/main");
        assert!(configured.contains(&ada, &SessionId::new(configured.qualify(&ada, "main"))));
        assert!(
            !configured.contains(&ada, &SessionId::new(configured.qualify(&bob, "main"))),
            "the same name in another namespace is another tenant's session"
        );
        assert!(
            !configured.contains(&ada, &SessionId::new("main")),
            "and the bare name belongs to nobody"
        );
    }

    #[test]
    fn the_example_file_validates() {
        // From the crate root up to the workspace root, mirroring
        // `tests/example_catalog.rs::example_path`.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/control-plane.example.json");
        let config = ControlPlaneConfig::load(&path)
            .unwrap_or_else(|error| panic!("the shipped example must validate: {error}"));
        assert!(!config.projects.is_empty());
        assert!(!config.users.is_empty());

        // Decision 9's two config additions, both present in the shipped
        // example: a project policy, and a key override that narrows it. A
        // widening override would have failed `load` above, so reaching this
        // line already proves the override narrows -- these assertions pin
        // down *that it is exercised at all*, not just that nothing widened.
        let acme = config
            .projects
            .iter()
            .find(|project| project.id == "acme")
            .expect("the example's acme project");
        assert!(
            acme.policy.is_some(),
            "the example must demonstrate a project policy"
        );
        let key = config
            .keys
            .first()
            .expect("the example must ship at least one key");
        assert!(
            key.overrides.is_some(),
            "the example must demonstrate a key override"
        );
    }

    // -----------------------------------------------------------------------
    // Decision 9: policy on project entries, overrides on key entries
    // -----------------------------------------------------------------------

    fn bearer_headers(secret: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            format!("Bearer {secret}").parse().expect("a valid value"),
        );
        headers
    }

    #[test]
    fn a_min_quality_outside_the_unit_interval_is_rejected_naming_the_project() {
        for min_quality in [-0.1, 1.1] {
            let json = format!(
                r#"{{
                  "projects": [
                    {{ "id": "acme", "policy": {{ "min_quality": {min_quality} }} }}
                  ],
                  "users": []
                }}"#
            );
            let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
            match error {
                ControlPlaneError::MinQualityOutOfRange {
                    entry,
                    min_quality: got,
                    ..
                } => {
                    assert_eq!(entry, "project `acme`");
                    assert_eq!(got, min_quality);
                }
                other => panic!("expected MinQualityOutOfRange, got {other:?}"),
            }
        }

        // Control: the bounds themselves are inside the interval and validate.
        let json = r#"{
          "projects": [{ "id": "acme", "policy": { "min_quality": 0.0 } }],
          "users": []
        }"#;
        ControlPlaneConfig::from_json(json, "test").expect("0.0 is inside 0.0..=1.0");
        let json = r#"{
          "projects": [{ "id": "acme", "policy": { "min_quality": 1.0 } }],
          "users": []
        }"#;
        ControlPlaneConfig::from_json(json, "test").expect("1.0 is inside 0.0..=1.0");
    }

    #[test]
    fn a_cadence_with_zero_per_turns_or_excess_max_frontier_is_rejected() {
        let zero_window = r#"{
          "projects": [
            {
              "id": "acme",
              "policy": { "frontier_cadence": { "max_frontier": 0, "per_turns": 0 } }
            }
          ],
          "users": []
        }"#;
        let error = ControlPlaneConfig::from_json(zero_window, "test").unwrap_err();
        match error {
            ControlPlaneError::CadencePerTurnsZero { entry, .. } => {
                assert_eq!(entry, "project `acme`");
            }
            other => panic!("expected CadencePerTurnsZero, got {other:?}"),
        }

        let excess = r#"{
          "projects": [
            {
              "id": "acme",
              "policy": { "frontier_cadence": { "max_frontier": 5, "per_turns": 2 } }
            }
          ],
          "users": []
        }"#;
        let error = ControlPlaneConfig::from_json(excess, "test").unwrap_err();
        match error {
            ControlPlaneError::CadenceExceedsWindow {
                entry,
                max_frontier,
                per_turns,
                ..
            } => {
                assert_eq!(entry, "project `acme`");
                assert_eq!(max_frontier, 5);
                assert_eq!(per_turns, 2);
            }
            other => panic!("expected CadenceExceedsWindow, got {other:?}"),
        }

        // Control: `max_frontier == per_turns` is the bound, not past it.
        let boundary = r#"{
          "projects": [
            {
              "id": "acme",
              "policy": { "frontier_cadence": { "max_frontier": 3, "per_turns": 3 } }
            }
          ],
          "users": []
        }"#;
        ControlPlaneConfig::from_json(boundary, "test")
            .expect("max_frontier == per_turns is exactly at the bound");
    }

    #[test]
    fn a_malformed_glob_is_rejected_naming_the_entry() {
        let project_glob = r#"{
          "projects": [{ "id": "acme", "policy": { "allow": ["anthropic/**"] } }],
          "users": []
        }"#;
        let error = ControlPlaneConfig::from_json(project_glob, "test").unwrap_err();
        match error {
            ControlPlaneError::MalformedGlob { entry, .. } => {
                assert_eq!(entry, "project `acme`");
            }
            other => panic!("expected MalformedGlob, got {other:?}"),
        }

        let key_glob = format!(
            r#"{{
              "projects": [{{ "id": "acme" }}],
              "users": [{{ "id": "ada" }}],
              "keys": [{{
                "project": "acme",
                "user": "ada",
                "key_sha256": "{TURN_HASH}",
                "overrides": {{ "allow": ["anthropic/{{a,b}}"] }}
              }}]
            }}"#
        );
        let error = ControlPlaneConfig::from_json(&key_glob, "test").unwrap_err();
        match error {
            ControlPlaneError::MalformedGlob { entry, .. } => {
                assert_eq!(entry, "key for project `acme`, user `ada`");
            }
            other => panic!("expected MalformedGlob, got {other:?}"),
        }
    }

    #[test]
    fn an_override_wider_than_the_project_policy_is_rejected_naming_both() {
        let json = format!(
            r#"{{
              "projects": [
                {{ "id": "acme", "policy": {{ "min_quality": 0.7 }} }}
              ],
              "users": [{{ "id": "ada" }}],
              "keys": [{{
                "project": "acme",
                "user": "ada",
                "key_sha256": "{TURN_HASH}",
                "overrides": {{ "min_quality": 0.3 }}
              }}]
            }}"#
        );
        let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
        match error {
            ControlPlaneError::OverrideWiderThanProject {
                project_entry,
                key_entry,
                axes,
                ..
            } => {
                assert_eq!(project_entry, "project `acme`");
                assert_eq!(key_entry, "key for project `acme`, user `ada`");
                assert_eq!(axes, vec!["min_quality"]);
            }
            other => panic!("expected OverrideWiderThanProject, got {other:?}"),
        }

        // Control: an override that only tightens validates.
        let json = format!(
            r#"{{
              "projects": [
                {{ "id": "acme", "policy": {{ "min_quality": 0.5 }} }}
              ],
              "users": [{{ "id": "ada" }}],
              "keys": [{{
                "project": "acme",
                "user": "ada",
                "key_sha256": "{TURN_HASH}",
                "overrides": {{ "min_quality": 0.8 }}
              }}]
            }}"#
        );
        ControlPlaneConfig::from_json(&json, "test")
            .expect("an override that only raises the floor narrows and must validate");
    }

    #[test]
    fn an_absent_policy_resolves_to_unrestricted_and_open_mode_always_does() {
        // Configured mode, no `"policy"` and no `"overrides"` anywhere in the
        // fixture: the key resolves to exactly `TurnPolicy::unrestricted()`.
        let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
        let plane = ControlPlane::configured(config);
        let admission = plane
            .turn_admission(&bearer_headers(TURN_SECRET))
            .expect("a known turn key");
        assert_eq!(*admission.policy, TurnPolicy::unrestricted());

        // Open mode: unrestricted regardless of what the header says, since an
        // unconfigured deployment authenticates nothing.
        let open = ControlPlane::Open;
        for headers in [HeaderMap::new(), bearer_headers("rh_turn_garbage")] {
            let admission = open
                .turn_admission(&headers)
                .expect("Open mode never refuses");
            assert_eq!(*admission.policy, TurnPolicy::unrestricted());
        }
    }

    #[test]
    fn a_key_with_overrides_resolves_to_the_narrowed_policy() {
        let json = format!(
            r#"{{
              "projects": [
                {{
                  "id": "acme",
                  "policy": {{
                    "min_quality": 0.5,
                    "frontier_cadence": {{ "max_frontier": 2, "per_turns": 10 }}
                  }}
                }}
              ],
              "users": [{{ "id": "ada" }}],
              "keys": [{{
                "project": "acme",
                "user": "ada",
                "key_sha256": "{TURN_HASH}",
                "overrides": {{
                  "min_quality": 0.8,
                  "frontier_cadence": {{ "max_frontier": 1, "per_turns": 10 }}
                }}
              }}]
            }}"#
        );
        let config = ControlPlaneConfig::from_json(&json, "test").unwrap();
        let plane = ControlPlane::configured(config);
        let admission = plane
            .turn_admission(&bearer_headers(TURN_SECRET))
            .expect("a known turn key");

        assert_eq!(admission.principal, Principal::new("acme", "ada"));
        assert_eq!(admission.policy.min_quality, 0.8);
        assert_eq!(
            admission.policy.frontier_cadence,
            Some(FrontierCadence {
                max_frontier: 1,
                per_turns: 10
            })
        );
    }
}
