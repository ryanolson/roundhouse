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
//!
//! # The file shares the API's strictness
//!
//! Every shape below is `deny_unknown_fields`, top-level document included, so
//! a stray or misspelled key in `ROUNDHOUSE_CONTROL_PLANE` is a **boot
//! refusal** rather than a line quietly dropped. That is deliberately the same
//! answer the admin plane gives a misspelled `POST` body, and for the same
//! reason: a file that silently means less than it says is worse than one that
//! fails to load, because the difference between `credentials` and `credential`
//! is an ungated project nobody can see from the outside. The cost is real and
//! accepted — a config written for a newer roundhouse will not boot an older
//! one — and it is the cheaper of the two failures, because it happens at
//! start-up in front of whoever made the change.
//!
//! JSON has no comments, so prose has exactly one home:
//! [`ControlPlaneConfig::comment`] at the document root. A `$comment` written
//! beside a project, a user or a key is refused like any other stray field —
//! a carve-out narrow enough to be worth naming here, because that refusal is
//! otherwise indistinguishable from a typo.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

use roundhouse_core::control::credential::access::ProviderKeys;
use roundhouse_core::control::{
    Budget, BudgetCounts, CredentialError, CredentialMode, FilterError, FrontierCadence,
    PolicyOverrides, Principal, TargetFilter, TurnCredentials, TurnPolicy,
};

use super::Admission;
use super::budget::{AllocationConfig, BudgetConfig, budget_terms};
use super::credentials::CredentialsConfig;
use roundhouse_core::validate::ValidationTerms;

use super::validate::ValidateConfig;

/// One entry of the config's `"projects"` array.
///
/// `deny_unknown_fields` because every optional axis below *widens* when it is
/// absent: `credential` for `credentials` is an ungated project, `policy` lost
/// to a typo is unrestricted routing, and a dropped `budget` is unlimited
/// spend. None of those is visible from any read surface afterwards, so the
/// only place the mistake can be caught is the load that made it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Whether this project's sessions are enrolled in the validate/steer
    /// loop, and how. Absent means off — see [`ValidateConfig`], and note that
    /// off is the *shipped* posture rather than a fallback: a deployment that
    /// upgrades validates nothing until a project says otherwise.
    ///
    /// On the project and not on a key, unlike `overrides` and `allocation`:
    /// an arm is the unit of a comparison, and two keys of one project running
    /// different arm splits would put two experiments inside one project's
    /// numbers.
    #[serde(default)]
    pub validate: Option<ValidateConfig>,
    /// Whose provider keys this project's turns authenticate with, and whether
    /// a member's own key draws the project's budget. Absent means the
    /// deployment's own keys under the default mode — which, on a deployment
    /// that declares none either, is
    /// [`TurnCredentials::unrestricted`](roundhouse_core::control::TurnCredentials::unrestricted):
    /// every quoted provider stays in the candidate set and the transport
    /// authenticates itself, exactly as a pre-M7 deployment routes.
    #[serde(default)]
    pub credentials: Option<CredentialsConfig>,
}

/// One entry of the config's `"users"` array.
///
/// One field today, and `deny_unknown_fields` is worth more here than on a
/// wide struct rather than less: everything an operator might reasonably write
/// beside `id` — a display name, a team, an email — is a field this shape does
/// not have, and accepting them silently would let a whole vocabulary
/// accumulate in a file that reads none of it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserEntry {
    pub id: String,
}

/// One entry of the config's `"keys"` array: a turn key's membership and hash.
///
/// `deny_unknown_fields` for [`ProjectEntry`]'s reason, sharpened: `override`
/// for `overrides` drops a *narrowing* overlay, so the key silently resolves to
/// its project's whole policy — the widest reading of the entry, produced by
/// the one kind of mistake nothing downstream can distinguish from an operator
/// who meant it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// This member's own provider keys — the `user` tier
    /// [`CredentialMode`] resolves against. `"mode"` and `"budget_counts"` are
    /// refused here rather than ignored: both decide who pays, and a member who
    /// could set either could spend somebody else's key or exempt their own
    /// turns from the ceiling they are meant to draw.
    #[serde(default)]
    pub credentials: Option<CredentialsConfig>,
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
///
/// `pub(super)` since the admin plane: the directory derives a membership's
/// budget terms from the same project block this compiler reads, through the
/// same resolvers, and a refusal from either has to name the entry the same way
/// or the two halves of one control plane describe one project two ways.
pub(super) fn project_entry_label(id: &str) -> String {
    format!("project `{id}`")
}

/// `"key for project `{project}`, user `{user}`"`, the label every
/// key-overrides error names. `pub(super)` for the reason
/// [`project_entry_label`] is.
pub(super) fn key_entry_label(project: &str, user: &str) -> String {
    format!("key for project `{project}`, user `{user}`")
}

/// What a deployment supplies at
/// [`CONTROL_PLANE_VAR`](super::CONTROL_PLANE_VAR).
///
/// The format is the deserialized shape, on purpose: a hand-maintained schema
/// document would drift from what `serde` actually accepts, and this way it
/// cannot.
///
/// `deny_unknown_fields` on the document as well as on every entry inside it,
/// because the top level is where the widening typos are worst: `admin_key` for
/// `admin_keys` is a deployment with no root of trust for its admin plane, and
/// `credential` for `credentials` is a process holding no provider keys at all.
/// Both would start, and both would look like a configuration problem
/// somewhere else entirely.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlPlaneConfig {
    /// Prose, read by nothing.
    ///
    /// JSON has no comments, and the shipped example leads with a `$comment`
    /// array that explains the whole format — which parsed only because unknown
    /// fields used to be dropped in silence. Declared now that they are
    /// refused, so the one escape hatch this document has is a field the format
    /// admits rather than a hole it happens to leave open. Any *other* stray
    /// key is still a boot refusal, which is the point.
    #[serde(default, rename = "$comment")]
    pub comment: Option<serde_json::Value>,
    pub projects: Vec<ProjectEntry>,
    pub users: Vec<UserEntry>,
    #[serde(default)]
    pub keys: Vec<KeyEntry>,
    /// Hashes of admin secrets. Unlike `keys`, these name no membership —
    /// `KeyScope::Admin` acts on the deployment, not from inside a project.
    #[serde(default)]
    pub admin_keys: Vec<String>,
    /// The namespace this deployment's synthetic tool calls are rendered
    /// under, or `None` for
    /// [`DEFAULT_MCP_NAMESPACE`](crate::dialect::DEFAULT_MCP_NAMESPACE).
    ///
    /// Deployment-wide rather than per-project, and that is a claim rather
    /// than a simplification: the namespace has to match what the *client's*
    /// MCP registration calls this server, and one deployment serves one
    /// endpoint, so a per-project namespace would be a name no project could
    /// make its own agent use. It sits in this file because this is where a
    /// deployment already names the things its clients say back to it — the
    /// keys they present and the session namespace their conversations are
    /// qualified into.
    ///
    /// Read through [`ControlPlane::client_dialect`](super::ControlPlane::client_dialect)
    /// and nowhere else, so an open deployment and a configured one answer the
    /// same question in one place.
    #[serde(default)]
    pub mcp_namespace: Option<String>,
    /// What arm assignment is hashed against, deployment-wide.
    ///
    /// Here rather than in an environment variable for the same reason
    /// `mcp_namespace` is: it is a deployment-wide name whose value decides how
    /// something an operator wrote down is interpreted, and it belongs in the
    /// file the rest of that is written in. Absent is the empty salt, which is
    /// a salt like any other rather than "no experiment" — a deployment with no
    /// project enrolled stamps no arm whatever this says.
    ///
    /// **Moving it is a study boundary.** The arm it produces is stamped into
    /// `SessionCreated` and never recomputed, so an edit re-buckets sessions
    /// created afterwards and none created before. That is the intended
    /// behavior and the reason the stamp exists; what it is not is a way to
    /// re-randomize a study already in flight.
    #[serde(default)]
    pub arm_salt: Option<String>,
    /// The deployment's own provider keys: the tier every mode but `UserOnly`
    /// falls back to, and the one a project that declares nothing of its own
    /// runs on.
    ///
    /// At the top level rather than repeated per project because that is what
    /// it is — one set of keys this process holds. A project selects *whether*
    /// it may reach them through its `mode`; it does not restate them.
    #[serde(default)]
    pub credentials: Option<CredentialsConfig>,
    /// How long a node may keep serving a compiled plane before it re-reads the
    /// admin directory, in milliseconds. Absent means
    /// [`DEFAULT_ADMISSION_CACHE_TTL_MS`].
    ///
    /// **This is the staleness bound on a revocation, and it is the only one.**
    /// The node that performs a write swaps its own snapshot immediately, so a
    /// key revoked here stops working here on the next request. Every *other*
    /// node is serving a snapshot compiled before the write, and this is how
    /// long it may go on doing so — see
    /// [`ControlDirectory::plane`](super::directory::ControlDirectory::plane).
    /// An operator revoking a leaked key is choosing this number, so it is
    /// written in the same file the keys are.
    ///
    /// `0` is legal and means "re-read on every request", which is the honest
    /// spelling of a deployment that would rather pay a store read per
    /// admission than leave a revoked key working for a second. It is not a
    /// disabled-cache sentinel: the refresh still only recompiles when the
    /// store's version has actually moved.
    #[serde(default)]
    pub admission_cache_ttl_ms: Option<u64>,
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
        "control-plane config `{path}`: `mcp_namespace` is `{namespace}` -- it must be \
         non-empty and free of whitespace, because it is matched by an agent's exact \
         tool-name lookup and a namespace nothing can name emits calls nothing can dispatch"
    )]
    BadMcpNamespace { path: String, namespace: String },
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
        "control-plane config `{path}`: {entry}'s allocation.capped.limit_usd {limit_usd} is not \
         positive -- a member cap of zero or less refuses every turn this key sends, which is a \
         revoked key spelled as a budget; delete the key, or give it a real ceiling"
    )]
    MemberCapNotPositive {
        path: String,
        entry: String,
        limit_usd: f64,
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
        "control-plane config `{path}`: {entry}'s validate.arms weights are all zero -- a share \
         table that shares nothing has no honest reading, and the tempting fallback (everything \
         in one arm) is exactly the silent mis-assignment the arms exist to prevent; give at \
         least one arm a weight, or leave `validate` out"
    )]
    ArmSharesEmpty { path: String, entry: String },
    #[error(
        "control-plane config `{path}`: {entry}'s validate.placebo_rate {placebo_rate} is \
         outside 0.0..=1.0 -- it is the fraction of fired triggers the sham arm interrupts on, \
         and a fraction outside that range is a control that is not one"
    )]
    PlaceboRateOutOfRange {
        path: String,
        entry: String,
        placebo_rate: f64,
    },
    #[error(
        "control-plane config `{path}`: {entry}'s validate.escalation_floor {escalation_floor} \
         is outside 0.0..=1.0 -- it is a quality prior, on the same scale every model in the \
         catalog is scored on"
    )]
    EscalationFloorOutOfRange {
        path: String,
        entry: String,
        escalation_floor: f64,
    },
    #[error(
        "control-plane config `{path}`: {entry}'s budget sets \
         \"overflow_when_local_saturated\" alongside \"on_exhaustion\": \"refuse\" -- overflow \
         is a degrade-mode valve, meaningless once the project has decided to refuse a turn \
         rather than degrade it"
    )]
    OverflowWithRefuse { path: String, entry: String },
    #[error(
        "control-plane config `{path}`: {entry}'s credentials for provider `{provider}` are \
         refused: {source}"
    )]
    Credential {
        path: String,
        entry: String,
        provider: String,
        #[source]
        source: CredentialError,
    },
    #[error(
        "control-plane config `{path}`: {entry}'s credentials for provider `{provider}` name the \
         environment variable `{var}`, which is not set in this process -- the file names the \
         variable a secret lives in and never carries the secret itself, so an unset variable is \
         a credential this deployment does not have. Every turn of that key's would lose \
         `{provider}` from its candidate set and quietly route elsewhere; set the variable, or \
         take the entry out"
    )]
    CredentialEnvVarUnset {
        path: String,
        entry: String,
        provider: String,
        var: String,
    },
    #[error(
        "control-plane config `{path}`: {entry} sets `credentials.{field}`, which only a project \
         may set -- it decides who pays, and a tier that could set it for itself could spend \
         somebody else's key or exempt its own turns from the ceiling they draw"
    )]
    CredentialFieldNotAllowedHere {
        path: String,
        entry: String,
        field: &'static str,
    },
    #[error("control-plane config `{path}`: {entry} is refused: {source}")]
    CredentialResolution {
        path: String,
        entry: String,
        #[source]
        source: CredentialError,
    },
}

/// How long a compiled plane is served before the directory is re-read, where
/// the file names no other number.
///
/// Thirty seconds because it is the bound on how long a *revoked* key keeps
/// working on a node that did not perform the revocation, and that is the
/// number an operator is really choosing. Long enough that an admin surface
/// under load is not a read amplifier on the store; short enough that "revoke
/// the leaked key" is an action with a visible end. A deployment that wants
/// either extreme writes `admission_cache_ttl_ms` rather than arguing with this
/// constant.
pub const DEFAULT_ADMISSION_CACHE_TTL_MS: u64 = 30_000;

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
    ///
    /// `pub(super)` rather than private since the admin plane: the directory
    /// merges the file's entries with the ones an operator created over the API
    /// into one config of this same shape and compiles it *here*, so a
    /// runtime-minted key is judged by the same boundary as a boot-loaded one.
    /// A second compiler for admin-created entities is the one thing that would
    /// let the two halves of the control plane disagree — see
    /// [`ControlDirectory`](super::directory::ControlDirectory).
    pub(super) fn validate(&mut self, path: &str) -> Result<(), ControlPlaneError> {
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
        // And every project's resolved validate terms, `None` meaning the loop
        // is off for it — resolved here for the same reason the two above are,
        // and refused here for a sharper one: a broken share table has to stop
        // the boot on the day it is written, not on the day somebody flips
        // `enabled`.
        let mut project_validation: HashMap<&str, Option<ValidationTerms>> = HashMap::new();
        // The deployment's own keys, resolved once: every project's resolution
        // reads them, and reading the environment per project would let one
        // variable be judged twice and -- if it changed underneath us -- judged
        // two ways.
        let deployment_keys = match &self.credentials {
            Some(credentials) => credentials.to_tier(path, "the deployment")?,
            None => ProviderKeys::new(),
        };
        // Every project's credential resolution: its mode, its budget axis, and
        // its own keys. Resolved here for the reason the three above are --
        // once per project rather than once per key -- and refused here for the
        // sharper one: an environment variable this process does not have has
        // to stop the boot on the day it is written down, not on the day a
        // tenant's turn quietly loses a provider.
        let mut project_credentials: HashMap<&str, (CredentialMode, BudgetCounts, ProviderKeys)> =
            HashMap::new();
        // Which projects wrote a `"credentials"` block at all, which is a
        // different question from what it resolved to: a project may declare
        // the block, name no provider, and mean it -- see the `declared` branch
        // in the keys loop below.
        let mut project_declares_credentials: HashSet<&str> = HashSet::new();
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

            let validation = match &project.validate {
                Some(validate_config) => validate_config.to_terms(path, &entry)?,
                None => None,
            };
            project_validation.insert(project.id.as_str(), validation);

            if project.credentials.is_some() {
                project_declares_credentials.insert(project.id.as_str());
            }
            let credentials = match &project.credentials {
                Some(credentials) => credentials.to_project(path, &entry)?,
                None => (
                    CredentialMode::default(),
                    BudgetCounts::default(),
                    ProviderKeys::new(),
                ),
            };
            project_credentials.insert(project.id.as_str(), credentials);
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
            //
            // Through `budget_terms` rather than built here, so that this is the
            // only *pairing* of a project budget with a member allocation in the
            // process — see that function on why a second one would eventually
            // roll somebody's budget window.
            let budget = budget_terms(project_budget.clone(), allocation);

            // Copied down from the project unchanged: an arm is the unit of a
            // comparison, so every key of one project is in one experiment.
            let validation = project_validation
                .get(key.project.as_str())
                .expect("a project checked present above was resolved to validate terms above")
                .clone();

            // `project_ids.contains` above already proved this project exists,
            // so it is in `project_credentials` too -- both loops walk the same
            // `self.projects`.
            let (mode, budget_counts, project_keys) = project_credentials
                .get(key.project.as_str())
                .expect("a project checked present above was resolved to credentials above");
            let user_keys = match &key.credentials {
                Some(credentials) => credentials.to_tier(path, &key_entry)?,
                None => ProviderKeys::new(),
            };
            // **A file that says nothing about credentials is not gating on
            // them.** Without this line, every deployment that upgrades into M7
            // resolves to a `Stored` resolution with three empty tiers, which
            // reaches no provider, which withholds every hosted candidate — so
            // turning the milestone on would silently re-route every existing
            // M1–M6 workload to local capacity it may not even have. That is
            // the one thing a new milestone must not do, and it is the same
            // rule `Admission::open` states for the unconfigured deployment,
            // extended to the configured one that simply has not written this
            // block. Declaring the block *anywhere* — deployment, project or
            // key — turns the gate on for that key.
            let declared = self.credentials.is_some()
                || project_declares_credentials.contains(key.project.as_str())
                || key.credentials.is_some();
            // Through `configured` and never through a caller-side branch on
            // the mode: it is the one entry point that runs the
            // pass-through/stored mutual-exclusion check, and a branch here
            // would skip it for exactly the configuration it exists to refuse.
            // The `declared` branch above is not that branch — it selects
            // between gating and not gating, and the un-gated arm has no tiers
            // for the check to be about.
            let credentials = match declared {
                false => TurnCredentials::unrestricted(),
                true => TurnCredentials::configured(
                    *mode,
                    deployment_keys.clone(),
                    project_keys.clone(),
                    user_keys,
                )
                .map_err(|source| ControlPlaneError::CredentialResolution {
                    path: path.to_string(),
                    entry: key_entry.clone(),
                    source,
                })?,
            };

            turn_keys.insert(
                key.key_sha256.clone(),
                Admission {
                    principal: Principal::new(key.project.clone(), key.user.clone()),
                    policy: Arc::new(project_policy.narrow(&overrides)),
                    budget,
                    validation,
                    credentials,
                    budget_counts: *budget_counts,
                },
            );
        }

        // Rejected at the boundary rather than trimmed or defaulted at the
        // projection: a namespace that is empty or carries whitespace renders
        // a call an agent's exact lookup can never match, so the turn would
        // complete and the steer would silently do nothing. An operator-authored
        // name that means nothing must fail to load, not fail to work.
        if let Some(namespace) = &self.mcp_namespace
            && (namespace.is_empty() || namespace.chars().any(char::is_whitespace))
        {
            return Err(ControlPlaneError::BadMcpNamespace {
                path: path.to_string(),
                namespace: namespace.clone(),
            });
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

/// The whole-file tests: parsing, every rejection an operator can trigger, and
/// the resolved `Admission` a key turns into.
///
/// Moved out of this file rather than living at the bottom of it. They had
/// grown to more than nine hundred lines — well past `config.rs`'s own
/// production half — so a reader scrolling for a validator was scrolling
/// through fixtures, and the module doc's claim that this file is "one config
/// object, judged by one boundary" was only true of its first third.
#[cfg(test)]
mod tests;
