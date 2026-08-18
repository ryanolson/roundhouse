// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The file an operator writes, and the boundary that refuses a bad one.
//!
//! Everything here is about *what is on disk*: the deserialized shapes, the
//! labels an error names an entry by, and [`ControlPlaneConfig::validate`],
//! which is the single place a config is judged. Nothing here knows what a
//! *request* looks like — no headers, no presented secrets, no
//! [`Principal`](roundhouse_core::control::Principal) resolution. That half is
//! [`mod.rs`](super), and the two are one `use` line apart on purpose: the
//! validators below and the resolver above look alike (both check the shape of
//! a string that names a key) and are checked at opposite ends of the system,
//! which is exactly the confusion a shared file invites.
//!
//! The one thing that crosses the seam is [`ControlPlaneConfig::turn_keys`]:
//! `validate` builds the finished lookup table, and
//! [`ControlPlane::configured`](super::ControlPlane::configured) takes it
//! whole. See that field for why it is built here and not there.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

use roundhouse_core::control::{
    Allocation, Budget, BudgetTerms, FilterError, FrontierCadence, PolicyOverrides, Principal,
    TargetFilter, TurnPolicy,
};

use super::Admission;
use super::budget::{AllocationConfig, BudgetConfig};

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
    /// This project's spending ceiling. Absent means unlimited: every key's
    /// resolved [`Admission::budget`] is `None`, the engine skips the ledger
    /// entirely, and routing is exactly what a pre-M3 deployment already
    /// does — see [`roundhouse_core::control::budget`] on why unlimited is a
    /// distinct value and not a budget with a very large limit.
    #[serde(default)]
    pub budget: Option<BudgetConfig>,
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
    /// This key's ceiling on top of its project's budget. Absent means
    /// [`Allocation::Pooled`] — no *second* ceiling, not no budget: the
    /// project's own limit still binds. An `allocation` on a key whose
    /// project has no `"budget"` is accepted and resolves to nothing —
    /// decision 8 does not ask for it to be refused, unlike a widening
    /// `overrides`.
    #[serde(default)]
    pub allocation: Option<AllocationConfig>,
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
    /// (`max_frontier <= per_turns`) and at least some (`max_frontier >= 1`).
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
            // A ration of zero is a filter wearing a cadence's clothes: it
            // forbids every hosted target on every turn, forever, which is
            // `allow: ["local/*"]` spelled in the one vocabulary that promises
            // the opposite — that a spent window still *serves* the turn
            // rather than failing it. Refused rather than honored, because the
            // two axes are read in different places: the cadence only inside
            // `TurnPolicy::admits`, while `TurnPolicy::permits` — the question
            // the startup cross-check and the engine's `considered` filter ask
            // — deliberately ignores it. A knob that means "never" has to be
            // written where all three can see it.
            if cadence.max_frontier == 0 {
                return Err(ControlPlaneError::CadenceRationsNothing {
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

/// What a deployment supplies at
/// [`CONTROL_PLANE_VAR`](super::CONTROL_PLANE_VAR).
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
    /// The finished turn-key lookup table: `key_sha256` to the complete
    /// [`Admission`] the key resolves to — its membership, its fully-resolved
    /// [`TurnPolicy`] (its project's policy narrowed by its own overrides),
    /// and its fully-resolved budget terms (its project's [`Budget`] paired
    /// with its own [`Allocation`], or `None` when the project has none).
    ///
    /// Built once, by [`Self::validate`], and not by `serde`: resolving a
    /// key's policy and budget requires its project's policy and budget to
    /// already exist and the widening check to have already run, both of
    /// which only `validate` has done by the time this is populated.
    ///
    /// The *finished* table rather than a side map keyed by hash that
    /// [`ControlPlane::configured`](super::ControlPlane::configured) then
    /// re-joins against `keys`: a re-join is a second chance to look up a key
    /// that is not there, and the only available answer at that point — with
    /// no path and no entry name to name — would be a silent
    /// [`TurnPolicy::unrestricted`], the most permissive value in the system,
    /// substituted for the narrowest. Building the table here means the
    /// question cannot be asked twice, so it cannot be answered two ways —
    /// the same reasoning that makes `Admission` the table's value type
    /// instead of a tuple `config` and `mod.rs` would each destructure their
    /// own way.
    #[serde(skip)]
    pub(super) turn_keys: HashMap<String, Admission>,
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
        "control-plane config `{path}`: {entry}'s frontier_cadence.max_frontier is 0, which \
         forbids every hosted target on every turn -- that is an allow-list, not a cadence, \
         and it must be written as one: `\"allow\": [\"local/*\"]`. A cadence promises that a \
         spent window still serves the turn locally; a ration of zero promises nothing to \
         spend"
    )]
    CadenceRationsNothing { path: String, entry: String },
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
         {project_entry}'s policy on {axes} -- an override may only narrow the policy of the \
         project it belongs to"
    )]
    OverrideWiderThanProject {
        path: String,
        project_entry: String,
        key_entry: String,
        /// The axis names, already joined with `", "`.
        ///
        /// Joined at construction rather than rendered with `{:?}` in the
        /// format string: an operator reading `["min_quality"]` off a startup
        /// failure has to work out that the brackets and quotes are Rust's and
        /// not part of the field name they are being told to go and fix.
        axes: String,
    },
    #[error(
        "control-plane config `{path}`: {entry}'s budget.limit_usd {limit_usd} is not positive \
         -- a zero or negative limit would refuse every turn from boot, which nobody writes on \
         purpose; write \"on_exhaustion\": \"refuse\" with a real limit for that"
    )]
    BudgetLimitNotPositive {
        path: String,
        entry: String,
        limit_usd: f64,
    },
    #[error(
        "control-plane config `{path}`: {entry}'s budget.warn_at {warn_at} is outside \
         0.0..=1.0, exclusive of 0.0 -- a warning at 0.0 would fire before anything was spent"
    )]
    WarnAtOutOfRange {
        path: String,
        entry: String,
        warn_at: f64,
    },
    #[error(
        "control-plane config `{path}`: {entry}'s allocation.share.fraction {fraction} is \
         outside 0.0..=1.0, exclusive of 0.0 -- a share of nothing is not a ceiling"
    )]
    ShareFractionOutOfRange {
        path: String,
        entry: String,
        fraction: f64,
    },
    #[error(
        "control-plane config `{path}`: {entry}'s budget sets \
         \"overflow_when_local_saturated\" alongside \"on_exhaustion\": \"refuse\" -- overflow \
         is a degrade-mode valve, meaningless once the project has decided to refuse a turn \
         rather than degrade it"
    )]
    OverflowWithRefuse { path: String, entry: String },
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
    /// scope, or that names a membership half of which does not exist — and,
    /// on the way through, build the [`Self::turn_keys`] table the resolver
    /// runs on.
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
        // Every project's resolved budget, `None` meaning unlimited. Resolved
        // here for the same reason the policy is: the keys loop below reads
        // it once per key rather than re-judging the same `BudgetConfig`.
        let mut project_budgets: HashMap<&str, Option<Budget>> = HashMap::new();
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

            let budget = match &project.budget {
                Some(budget_config) => Some(budget_config.to_budget(path, &entry)?),
                None => None,
            };
            project_budgets.insert(project.id.as_str(), budget);
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
        // The finished table, assembled as each key is judged. See the field.
        let mut turn_keys: HashMap<String, Admission> = HashMap::new();
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
                            axes: widened.join(", "),
                        });
                    }
                    overrides
                }
                None => PolicyOverrides::default(),
            };

            // `project_ids.contains` above already proved this project
            // exists, so it is in `project_budgets` too — both loops walk
            // the same `self.projects`. `None` here is unlimited, not
            // "unresolved" — see the field.
            let project_budget = project_budgets
                .get(key.project.as_str())
                .expect("a project checked present above was resolved to a budget above");
            let allocation = match &key.allocation {
                Some(allocation_config) => Some(allocation_config.to_allocation(path, &key_entry)?),
                None => None,
            };
            // An allocation only means something once a project has a budget
            // to allocate — see `KeyEntry::allocation`'s doc for why an
            // allocation with no project budget is accepted rather than
            // refused.
            let budget = project_budget.clone().map(|budget| BudgetTerms {
                budget,
                allocation: allocation.unwrap_or(Allocation::Pooled),
            });

            turn_keys.insert(
                key.key_sha256.clone(),
                Admission {
                    principal: Principal::new(key.project.clone(), key.user.clone()),
                    policy: Arc::new(project_policy.narrow(&overrides)),
                    budget,
                },
            );
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

        self.turn_keys = turn_keys;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_config::fixtures::{TURN_HASH, sample_config};

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

        // Decision 8's config additions: a budgeted project with overflow
        // spelled out explicitly, a capped key, and a share key. Reading
        // these off the *resolved* `turn_keys` table (not the raw
        // `AllocationConfig`) proves the whole seam -- parse, validate,
        // resolve -- rather than just that the JSON shape parses.
        assert!(
            acme.budget.is_some(),
            "the example must demonstrate a project budget"
        );
        assert_eq!(
            acme.budget.as_ref().unwrap().overflow_when_local_saturated,
            Some(true),
            "the example spells the overflow valve out explicitly rather than \
             relying on the default"
        );

        let ceilings: HashSet<_> = config
            .turn_keys
            .values()
            .filter_map(|admission| admission.budget.as_ref())
            .map(|terms| terms.member_ceiling_usd().map(|usd| usd.to_bits()))
            .collect();
        assert!(
            ceilings.contains(&Some(100.0f64.to_bits())),
            "the example must demonstrate a capped key resolving to its ceiling: {ceilings:?}"
        );
        assert!(
            ceilings.contains(&Some(125.0f64.to_bits())),
            "the example must demonstrate a share key resolving to its ceiling \
             (25% of the $500 project limit): {ceilings:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Decision 8: budget on projects, allocation on keys
    // -----------------------------------------------------------------------

    /// The smallest JSON that carries a valid budget, with `limit_usd`
    /// substitutable so the boundary tests below can push it out of range.
    fn budget_json(limit_usd: impl std::fmt::Display) -> String {
        format!(
            r#"{{
              "projects": [
                {{
                  "id": "acme",
                  "budget": {{
                    "limit_usd": {limit_usd},
                    "window": "total",
                    "on_exhaustion": "degrade_to_local"
                  }}
                }}
              ],
              "users": []
            }}"#
        )
    }

    #[test]
    fn a_budget_with_nonpositive_limit_is_rejected() {
        for limit_usd in ["0.0", "-5.0"] {
            let error = ControlPlaneConfig::from_json(&budget_json(limit_usd), "test").unwrap_err();
            match error {
                ControlPlaneError::BudgetLimitNotPositive {
                    entry,
                    limit_usd: got,
                    ..
                } => {
                    assert_eq!(entry, "project `acme`");
                    assert_eq!(got, limit_usd.parse::<f64>().unwrap());
                }
                other => panic!("expected BudgetLimitNotPositive, got {other:?}"),
            }
        }

        // Control: a real limit validates.
        ControlPlaneConfig::from_json(&budget_json("1.0"), "test")
            .expect("a positive limit is not refused");
    }

    #[test]
    fn warn_at_outside_the_half_open_interval_is_rejected() {
        let json = format!(
            r#"{{
              "projects": [
                {{
                  "id": "acme",
                  "budget": {{
                    "limit_usd": 10.0,
                    "window": "total",
                    "on_exhaustion": "degrade_to_local",
                    "warn_at": {warn_at}
                  }}
                }}
              ],
              "users": []
            }}"#,
            warn_at = 0.0
        );
        let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
        match error {
            ControlPlaneError::WarnAtOutOfRange { entry, warn_at, .. } => {
                assert_eq!(entry, "project `acme`");
                assert_eq!(warn_at, 0.0);
            }
            other => panic!("expected WarnAtOutOfRange, got {other:?}"),
        }

        let json = format!(
            r#"{{
              "projects": [
                {{
                  "id": "acme",
                  "budget": {{
                    "limit_usd": 10.0,
                    "window": "total",
                    "on_exhaustion": "degrade_to_local",
                    "warn_at": {warn_at}
                  }}
                }}
              ],
              "users": []
            }}"#,
            warn_at = 1.1
        );
        let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
        assert!(matches!(error, ControlPlaneError::WarnAtOutOfRange { .. }));

        // Control: 1.0 is the closed end of the interval and validates; an
        // absent warn_at falls back to the shared default.
        let json = r#"{
          "projects": [
            {
              "id": "acme",
              "budget": {
                "limit_usd": 10.0,
                "window": "total",
                "on_exhaustion": "degrade_to_local",
                "warn_at": 1.0
              }
            }
          ],
          "users": []
        }"#;
        ControlPlaneConfig::from_json(json, "test").expect("1.0 is exactly at the bound");
        ControlPlaneConfig::from_json(&budget_json(10.0), "test")
            .expect("an absent warn_at is not refused");
    }

    #[test]
    fn a_share_fraction_outside_the_interval_is_rejected() {
        let json = format!(
            r#"{{
              "projects": [
                {{
                  "id": "acme",
                  "budget": {{
                    "limit_usd": 100.0,
                    "window": "total",
                    "on_exhaustion": "degrade_to_local"
                  }}
                }}
              ],
              "users": [{{ "id": "ada" }}],
              "keys": [{{
                "project": "acme",
                "user": "ada",
                "key_sha256": "{TURN_HASH}",
                "allocation": {{ "share": {{ "fraction": 0.0 }} }}
              }}]
            }}"#
        );
        let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
        match error {
            ControlPlaneError::ShareFractionOutOfRange {
                entry, fraction, ..
            } => {
                assert_eq!(entry, "key for project `acme`, user `ada`");
                assert_eq!(fraction, 0.0);
            }
            other => panic!("expected ShareFractionOutOfRange, got {other:?}"),
        }

        // Control 1: 1.0 is the closed end of a single share's interval (a
        // member may be allowed the whole project) and validates.
        let json = format!(
            r#"{{
              "projects": [
                {{
                  "id": "acme",
                  "budget": {{
                    "limit_usd": 100.0,
                    "window": "total",
                    "on_exhaustion": "degrade_to_local"
                  }}
                }}
              ],
              "users": [{{ "id": "ada" }}],
              "keys": [{{
                "project": "acme",
                "user": "ada",
                "key_sha256": "{TURN_HASH}",
                "allocation": {{ "share": {{ "fraction": 1.0 }} }}
              }}]
            }}"#
        );
        ControlPlaneConfig::from_json(&json, "test").expect("1.0 is exactly at the bound");

        // Control 2: this rule is about one member's own fraction, not the
        // sum across members -- two keys each within (0.0, 1.0] whose shares
        // sum past 1.0 is the legitimate over-subscription decision 1 and
        // decision 8 both describe, and is accepted (the project limit still
        // binds both).
        let json = format!(
            r#"{{
              "projects": [
                {{
                  "id": "acme",
                  "budget": {{
                    "limit_usd": 100.0,
                    "window": "total",
                    "on_exhaustion": "degrade_to_local"
                  }}
                }}
              ],
              "users": [{{ "id": "ada" }}, {{ "id": "bob" }}],
              "keys": [
                {{
                  "project": "acme",
                  "user": "ada",
                  "key_sha256": "{TURN_HASH}",
                  "allocation": {{ "share": {{ "fraction": 0.6 }} }}
                }},
                {{
                  "project": "acme",
                  "user": "bob",
                  "key_sha256": "{SHARE_HASH}",
                  "allocation": {{ "share": {{ "fraction": 0.6 }} }}
                }}
              ]
            }}"#
        );
        ControlPlaneConfig::from_json(&json, "test")
            .expect("0.6 + 0.6 over-subscribes the project, which is accepted");
    }

    #[test]
    fn overflow_with_refuse_is_rejected_naming_the_project() {
        let json = r#"{
          "projects": [
            {
              "id": "acme",
              "budget": {
                "limit_usd": 10.0,
                "window": "total",
                "on_exhaustion": "refuse",
                "overflow_when_local_saturated": true
              }
            }
          ],
          "users": []
        }"#;
        let error = ControlPlaneConfig::from_json(json, "test").unwrap_err();
        let message = error.to_string();
        match error {
            ControlPlaneError::OverflowWithRefuse { entry, .. } => {
                assert_eq!(entry, "project `acme`");
            }
            other => panic!("expected OverflowWithRefuse, got {other:?}"),
        }
        assert!(
            message.contains("degrade-mode valve"),
            "the refusal has to say why refuse and overflow do not mix: {message}"
        );

        // The same rule with the flag set to false: still meaningless under
        // refuse, still rejected.
        let json = r#"{
          "projects": [
            {
              "id": "acme",
              "budget": {
                "limit_usd": 10.0,
                "window": "total",
                "on_exhaustion": "refuse",
                "overflow_when_local_saturated": false
              }
            }
          ],
          "users": []
        }"#;
        assert!(matches!(
            ControlPlaneConfig::from_json(json, "test").unwrap_err(),
            ControlPlaneError::OverflowWithRefuse { .. }
        ));

        // Control: refuse with no overflow field at all validates.
        let json = r#"{
          "projects": [
            {
              "id": "acme",
              "budget": {
                "limit_usd": 10.0,
                "window": "total",
                "on_exhaustion": "refuse"
              }
            }
          ],
          "users": []
        }"#;
        ControlPlaneConfig::from_json(json, "test")
            .expect("refuse with no overflow field is the ordinary spelling");
    }

    #[test]
    fn an_absent_budget_resolves_to_unconstrained() {
        // No `"budget"` anywhere in the fixture: every resolved admission
        // carries `budget: None`.
        let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
        let admission = config
            .turn_keys
            .get(TURN_HASH)
            .expect("the fixture's one turn key");
        assert!(
            admission.budget.is_none(),
            "an absent project budget must resolve to no budget terms, not a \
             very large one"
        );

        // M2-compat: adding `budget`/`allocation` to the config shapes must
        // not move a single byte of what a config with neither already
        // resolved to. Principal and policy are exactly what M2 produced.
        assert_eq!(admission.principal, Principal::new("acme", "ada"));
        assert_eq!(*admission.policy, TurnPolicy::unrestricted());
    }

    /// A second, unrelated well-formed hash, distinct from every fixture
    /// hash: `a_capped_and_a_share_key_resolve_to_their_ceilings` needs two
    /// keys under one project.
    const SHARE_HASH: &str = "075a9ac6b0608cfb207cb2f2e21e41b8e9771815b4403e0de138d18a240e0624";

    #[test]
    fn a_capped_and_a_share_key_resolve_to_their_ceilings() {
        let json = format!(
            r#"{{
              "projects": [
                {{
                  "id": "acme",
                  "budget": {{
                    "limit_usd": 100.0,
                    "window": "total",
                    "on_exhaustion": "degrade_to_local"
                  }}
                }}
              ],
              "users": [{{ "id": "cappy" }}, {{ "id": "sharey" }}],
              "keys": [
                {{
                  "project": "acme",
                  "user": "cappy",
                  "key_sha256": "{TURN_HASH}",
                  "allocation": {{ "capped": {{ "limit_usd": 30.0 }} }}
                }},
                {{
                  "project": "acme",
                  "user": "sharey",
                  "key_sha256": "{SHARE_HASH}",
                  "allocation": {{ "share": {{ "fraction": 0.25 }} }}
                }}
              ]
            }}"#
        );
        let config = ControlPlaneConfig::from_json(&json, "test").unwrap();

        let capped = config
            .turn_keys
            .get(TURN_HASH)
            .expect("cappy's key")
            .budget
            .as_ref()
            .expect("acme has a budget");
        assert_eq!(capped.allocation, Allocation::Capped { limit_usd: 30.0 });
        assert_eq!(capped.member_ceiling_usd(), Some(30.0));

        let shared = config
            .turn_keys
            .get(SHARE_HASH)
            .expect("sharey's key")
            .budget
            .as_ref()
            .expect("acme has a budget");
        assert_eq!(shared.allocation, Allocation::Share { fraction: 0.25 });
        assert_eq!(
            shared.member_ceiling_usd(),
            Some(25.0),
            "a quarter of the $100 project limit"
        );

        // Control: a key with no `allocation` on the same budgeted project
        // resolves to `Pooled` -- a budget, but no second ceiling.
        let pooled_json = format!(
            r#"{{
              "projects": [
                {{
                  "id": "acme",
                  "budget": {{
                    "limit_usd": 100.0,
                    "window": "total",
                    "on_exhaustion": "degrade_to_local"
                  }}
                }}
              ],
              "users": [{{ "id": "ada" }}],
              "keys": [{{ "project": "acme", "user": "ada", "key_sha256": "{TURN_HASH}" }}]
            }}"#
        );
        let config = ControlPlaneConfig::from_json(&pooled_json, "test").unwrap();
        let pooled = config
            .turn_keys
            .get(TURN_HASH)
            .unwrap()
            .budget
            .as_ref()
            .unwrap();
        assert_eq!(pooled.allocation, Allocation::Pooled);
        assert_eq!(pooled.member_ceiling_usd(), None);
    }

    #[test]
    fn every_declared_key_is_in_the_table_the_resolver_runs_on() {
        // The invariant that used to be a re-join in `ControlPlane::configured`
        // guarded by `unwrap_or_else(TurnPolicy::unrestricted)`. There is no
        // second lookup now, so the claim is stated where it is established:
        // one table entry per declared key, built as the key was judged.
        let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
        assert_eq!(config.turn_keys.len(), config.keys.len());
        for key in &config.keys {
            let admission = config
                .turn_keys
                .get(&key.key_sha256)
                .unwrap_or_else(|| panic!("key `{}` reached no table entry", key.key_sha256));
            assert_eq!(
                admission.principal,
                Principal::new(key.project.as_str(), key.user.as_str())
            );
        }
    }

    // -----------------------------------------------------------------------
    // Decision 9: policy on project entries, overrides on key entries
    // -----------------------------------------------------------------------

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
    fn a_cadence_that_rations_nothing_is_refused_and_pointed_at_the_allow_list() {
        // `max_frontier: 0` is a filter spelled as a cadence: it forbids every
        // hosted target on every turn. Refusing it is what lets the two
        // history-independent axes (`permits`) be the whole of what a startup
        // check and a candidate-set filter need to ask -- with a zero ration
        // accepted, "is this candidate reachable at all" and "is it reachable
        // this turn" would have different answers for a policy that never
        // reaches it.
        let json = r#"{
          "projects": [
            {
              "id": "acme",
              "policy": { "frontier_cadence": { "max_frontier": 0, "per_turns": 10 } }
            }
          ],
          "users": []
        }"#;
        let error = ControlPlaneConfig::from_json(json, "test").unwrap_err();
        let message = error.to_string();
        match error {
            ControlPlaneError::CadenceRationsNothing { entry, .. } => {
                assert_eq!(entry, "project `acme`");
            }
            other => panic!("expected CadenceRationsNothing, got {other:?}"),
        }
        assert!(
            message.contains(r#""allow": ["local/*"]"#),
            "the refusal has to say what to write instead: {message}"
        );

        // The same rule on the overrides half of the format.
        let key_side = format!(
            r#"{{
              "projects": [{{ "id": "acme" }}],
              "users": [{{ "id": "ada" }}],
              "keys": [{{
                "project": "acme",
                "user": "ada",
                "key_sha256": "{TURN_HASH}",
                "overrides": {{ "frontier_cadence": {{ "max_frontier": 0, "per_turns": 4 }} }}
              }}]
            }}"#
        );
        match ControlPlaneConfig::from_json(&key_side, "test").unwrap_err() {
            ControlPlaneError::CadenceRationsNothing { entry, .. } => {
                assert_eq!(entry, "key for project `acme`, user `ada`");
            }
            other => panic!("expected CadenceRationsNothing, got {other:?}"),
        }

        // Control: one dispatch per window is the smallest real ration and
        // validates.
        let smallest = r#"{
          "projects": [
            {
              "id": "acme",
              "policy": { "frontier_cadence": { "max_frontier": 1, "per_turns": 10 } }
            }
          ],
          "users": []
        }"#;
        ControlPlaneConfig::from_json(smallest, "test")
            .expect("one per ten is a cadence, not an allow-list");
    }

    #[test]
    fn a_misspelled_field_inside_frontier_cadence_is_refused_rather_than_ignored() {
        // `PolicyConfig` carries `deny_unknown_fields`, and serde does not
        // recurse: the attribute guards the three axes and nothing inside
        // them. So a stale or misspelled key left inside `frontier_cadence`
        // was accepted and dropped, and the operator got the cadence they did
        // not mean with no indication that a line of their file had been
        // ignored -- the exact failure `deny_unknown_fields` is on
        // `PolicyConfig` to prevent, one level down.
        let json = r#"{
          "projects": [
            {
              "id": "acme",
              "policy": {
                "frontier_cadence": { "max_frontier": 1, "per_turns": 10, "per_turn": 3 }
              }
            }
          ],
          "users": []
        }"#;
        let error = ControlPlaneConfig::from_json(json, "test").unwrap_err();
        assert!(
            matches!(error, ControlPlaneError::Parse { .. }),
            "a field nobody reads must stop the load: {error:?}"
        );
        assert!(
            error.to_string().contains("per_turn"),
            "and name the line to delete: {error}"
        );

        // Control: the same object without the stray key loads.
        let clean = r#"{
          "projects": [
            {
              "id": "acme",
              "policy": { "frontier_cadence": { "max_frontier": 1, "per_turns": 10 } }
            }
          ],
          "users": []
        }"#;
        ControlPlaneConfig::from_json(clean, "test").expect("the two real fields are enough");
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
                assert_eq!(axes, "min_quality");
            }
            other => panic!("expected OverrideWiderThanProject, got {other:?}"),
        }

        // Two axes at once read as prose rather than as a debug-printed Vec:
        // the operator is being told which field names to go and edit.
        let both = format!(
            r#"{{
              "projects": [
                {{
                  "id": "acme",
                  "policy": {{
                    "min_quality": 0.7,
                    "frontier_cadence": {{ "max_frontier": 1, "per_turns": 4 }}
                  }}
                }}
              ],
              "users": [{{ "id": "ada" }}],
              "keys": [{{
                "project": "acme",
                "user": "ada",
                "key_sha256": "{TURN_HASH}",
                "overrides": {{
                  "min_quality": 0.3,
                  "frontier_cadence": {{ "max_frontier": 3, "per_turns": 4 }}
                }}
              }}]
            }}"#
        );
        let message = ControlPlaneConfig::from_json(&both, "test")
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("on min_quality, frontier_cadence --"),
            "{message}"
        );

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
}
