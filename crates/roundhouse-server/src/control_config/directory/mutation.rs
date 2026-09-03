// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What the admin plane can change, and every way a change is refused.
//!
//! The vocabulary of a *write* and of a *refused write*, kept apart from the
//! records they act on for the reason [`records`](super::records) is kept apart
//! from the directory: these two enums are read as tables — one arm per route,
//! one variant per HTTP outcome — and a table is only readable while it is the
//! whole of what a reader is looking at.

use serde::{Deserialize, Deserializer};

use roundhouse_core::control::BudgetWindow;

use super::super::budget::{AllocationConfig, BudgetConfig};
use super::super::config::{ControlPlaneError, PolicyConfig, ProjectEntry, UserEntry};
use super::super::credentials::CredentialsConfig;
use super::super::crosscheck::CrossCheckRefusal;
use super::super::fair_use::FairUseConfig;
use super::super::validate::ValidateConfig;
use super::super::{MintError, MintedKey};
use super::records::{EntityKind, MembershipRole};
use super::store::StoreFailure;

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

/// The axes a project `PATCH` may move.
///
/// **Absent means "leave alone", and there is no spelling for "remove this
/// block".** That is a deliberate gap rather than an unfinished one: removing a
/// budget widens a ceiling to unlimited and removing a credentials block
/// un-gates a project, and both of those read like a field an operator forgot
/// to include. Widening by omission is the shape of mistake a partial-update
/// API is worst at making visible, so in M8 it is only expressible in the file,
/// where the whole document is in front of whoever edits it.
///
/// **Which is why every axis is `Option<Option<T>>`.** A plain `Option<T>`
/// collapses an absent field and an explicit JSON `null` into one `None` at
/// deserialization time, so the gap above became a silent one: `{"budget":
/// null}` — the only JSON spelling that reads like an attempt at removal — took
/// the "leave alone" branch and answered 200, having thrown away the fact that
/// the caller wrote anything at all. The outer `Option` is "was this field on
/// the wire", the inner one is "was it `null`", and
/// [`Self::explicit_null_axis`] is what turns the second into a refusal that
/// names the axis. Refusing loudly is the choice here because both alternatives
/// are silent: a no-op 200 tells an operator their clear worked, and an actual
/// clear performs the widening this type exists to keep out of the API.
///
/// `deny_unknown_fields`, and the project's own `id` is therefore *not*
/// accepted: it is in the path, and a body that carried a second one would have
/// to decide what a disagreement between them means. Strict rather than lenient
/// because the failure this shape is worst at showing is a misspelled field
/// name — which reads, silently, as "leave that alone".
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectPatch {
    #[serde(default, deserialize_with = "keep_explicit_null")]
    pub name: Option<Option<String>>,
    #[serde(default, deserialize_with = "keep_explicit_null")]
    pub policy: Option<Option<PolicyConfig>>,
    #[serde(default, deserialize_with = "keep_explicit_null")]
    pub budget: Option<Option<BudgetConfig>>,
    /// This project's rolling fair-use windows.
    ///
    /// **No window-mutation hazard here, which is why this axis is patchable at
    /// all while `budget.window` is refused.** A `BudgetWindow` change
    /// reinterprets committed spend — a total read as a month — so the admin
    /// plane declines it. Fair use has nothing committed to reinterpret: both
    /// backing ledgers — `MemoryFairUseLedger`'s `BTreeMap` and the Redis
    /// ledger's hash per scope (M13, relaid by M13.1) — bucket draws by
    /// wall-clock index under `(project, member)` and nothing else, and
    /// `would_exceed` reads the configured span at admission time. Narrowing a
    /// window therefore sums fewer of the same buckets and widening one sums
    /// more, both over draws that really happened; the pruning horizon is the
    /// widest window the module offers, so widening 5h to 7d finds its history
    /// intact rather than zeroed. A change takes effect on the next admitted
    /// turn and no counter moves.
    ///
    /// **That is a property of the storage layout, and both Redis layouts were
    /// built to keep it.** The bucket a draw lands in is a function of `at_ms`
    /// alone, with no window in it anywhere, so a window change cannot
    /// reinterpret an existing bucket the way a layout keyed *by window* would
    /// have. M13.1 added a *derived* per-window counter — a running sum, so a
    /// ceiling check need not re-scan the buckets — and the derivation is what
    /// keeps this axis patchable: a draw maintains every window's sum whether
    /// or not that window is capped today, and a read ages each sum against
    /// the span it is configured with right now. So a `PATCH` that starts
    /// capping a window nobody had capped finds that window's history already
    /// counted, and one that stops capping it leaves a sum that keeps ageing
    /// correctly for whenever it comes back.
    #[serde(default, deserialize_with = "keep_explicit_null")]
    pub fair_use: Option<Option<FairUseConfig>>,
    #[serde(default, deserialize_with = "keep_explicit_null")]
    pub validate: Option<Option<ValidateConfig>>,
    #[serde(default, deserialize_with = "keep_explicit_null")]
    pub credentials: Option<Option<CredentialsConfig>>,
}

/// The seam that keeps "the caller wrote `null`" alive past deserialization.
///
/// `serde` only calls a field's `deserialize_with` when the field is *present*,
/// so wrapping whatever it read in `Some` is the whole trick: a missing field
/// never reaches here and stays at the `Default` `None`.
fn keep_explicit_null<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

impl ProjectPatch {
    /// The name of the first axis the caller sent as an explicit JSON `null`.
    ///
    /// Field order, which is what makes it deterministic: a body that nulls two
    /// axes is refused naming the same one every time, on every deployment, and
    /// an operator who corrects it hits the other. Naming all of them at once
    /// would be kinder and is not a shape this error carries; naming whichever
    /// the JSON object happened to list first would make one request refuse two
    /// different ways.
    pub fn explicit_null_axis(&self) -> Option<&'static str> {
        [
            ("name", nulled(&self.name)),
            ("policy", nulled(&self.policy)),
            ("budget", nulled(&self.budget)),
            ("fair_use", nulled(&self.fair_use)),
            ("validate", nulled(&self.validate)),
            ("credentials", nulled(&self.credentials)),
        ]
        .into_iter()
        .find_map(|(axis, is_null)| is_null.then_some(axis))
    }
}

fn nulled<T>(axis: &Option<Option<T>>) -> bool {
    matches!(axis, Some(None))
}

/// The identity half of a minted key: everything about it that outlives the
/// secret.
///
/// Carried by [`DirectoryMutation::MintTurnKey`] and
/// [`DirectoryMutation::MintAdminKey`] rather than the [`MintedKey`] itself, so
/// the mutation vocabulary — the thing a durable store would one day
/// serialize — has no field a plaintext could reach.
#[derive(Debug, Clone)]
pub struct KeyFingerprint {
    pub key_sha256: String,
    pub display_tail: String,
}

impl From<&MintedKey> for KeyFingerprint {
    fn from(minted: &MintedKey) -> Self {
        Self {
            key_sha256: minted.key_sha256.clone(),
            display_tail: minted.display_tail.clone(),
        }
    }
}

/// Everything the admin plane can change.
///
/// One enum rather than a method per route, because provenance and identity are
/// checked the same way for all of them and the check belongs in one `match`
/// where the compiler can see a new arm was added to it.
#[derive(Debug, Clone)]
pub enum DirectoryMutation {
    CreateProject {
        entry: ProjectEntry,
    },
    PatchProject {
        id: String,
        patch: ProjectPatch,
    },
    /// `DELETE /v1/admin/projects/{id}` — archive, never delete. See
    /// [`ProjectRecord::archived_at_ms`].
    ArchiveProject {
        id: String,
    },
    CreateUser {
        entry: UserEntry,
    },
    UpsertMembership {
        project: String,
        user: String,
        role: MembershipRole,
        allocation: Option<AllocationConfig>,
        overrides: Option<PolicyConfig>,
    },
    /// Removes the edge and revokes every key minted under it. See
    /// [`ControlDirectory::apply`] on why the cascade is not optional.
    DeleteMembership {
        project: String,
        user: String,
    },
    MintTurnKey {
        project: String,
        user: String,
        key: KeyFingerprint,
    },
    MintAdminKey {
        key: KeyFingerprint,
    },
    RevokeKey {
        id: String,
    },
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// Why a directory read or write did not happen.
///
/// **One variant per outcome, named for the outcome's cause**, rather than a
/// single `Refused { code, message }`. The surface that turns these into HTTP
/// writes a `match`, and a `match` is checked: a variant added here without a
/// status is a compile error rather than a route that answers 500 for a
/// perfectly ordinary 409.
#[derive(Debug, thiserror::Error)]
pub enum DirectoryError {
    #[error(
        "{kind} `{id}` is declared in ROUNDHOUSE_CONTROL_PLANE, which owns it -- edit the file \
         and restart. An API change that shadowed the file would be undone by the next restart, \
         silently"
    )]
    ConfigOwned { kind: EntityKind, id: String },
    #[error("{kind} `{id}` already exists, so creating it here would give one identity two owners")]
    IdentityCollision { kind: EntityKind, id: String },
    #[error(
        "project `{id}` is archived, and archiving is final in this milestone -- its spend \
         history keeps the id, so nothing else may be created under it"
    )]
    ProjectIsArchived { id: String },
    /// A write against a directory with no writable half.
    ///
    /// The only *reachable* state it describes is a deployment that configured
    /// no control plane, which is exactly what
    /// [`AuthError::AdminRequiresControlPlane`] says — and the admin router says
    /// it first, on the mode, before any handler runs. This is what stops a
    /// future caller that forgets that gate from writing into a deployment that
    /// has no root of trust to have authorized the write.
    ///
    /// A fixed directory over a *configured* plane would also land here, and the
    /// message would then name the wrong remedy. Nothing constructs that
    /// arrangement outside a test fixture, and no fixture mounts the admin
    /// router over one; a route that made it reachable would need a message of
    /// its own rather than this one.
    ///
    /// [`AuthError::AdminRequiresControlPlane`]: super::super::AuthError::AdminRequiresControlPlane
    #[error(
        "this deployment configured no control plane, so there is no root of trust an admin \
         key could have been issued from and nothing here to administer -- set \
         ROUNDHOUSE_CONTROL_PLANE"
    )]
    NoAdminPlane,
    #[error("no project `{id}`")]
    UnknownProject { id: String },
    #[error("no user `{id}`")]
    UnknownUser { id: String },
    #[error("no membership for user `{user}` in project `{project}`")]
    UnknownMembership { project: String, user: String },
    #[error("no key `{id}`")]
    UnknownKey { id: String },
    #[error(
        "project `{project}`'s budget window is `{from:?}` and this change would make it \
         `{to:?}`, which is refused: committed spend is counted *within* a window, so moving one \
         either zeroes what the project has already spent this period or reinterprets a total as \
         a month. Neither is a change an API can make honestly. Create a project on the window \
         you want, or edit ROUNDHOUSE_CONTROL_PLANE and restart, where the reset is at least \
         visible"
    )]
    WindowChangeUnsupported {
        project: String,
        from: BudgetWindow,
        to: BudgetWindow,
    },
    /// A `PATCH` axis sent as an explicit JSON `null`.
    ///
    /// Refused rather than read as "leave alone" — which is what an *absent*
    /// field means — or as "remove this block", which M8 has no spelling for at
    /// all. See [`ProjectPatch`]: a caller who wrote `null` has said something
    /// about the axis, and both available readings of it are silent ones.
    #[error(
        "project `{project}`'s `{axis}` was sent as an explicit `null`, which is not a change \
         this API can make. An absent field means leave it alone, and there is no spelling for \
         removing a block: removing a budget widens a ceiling to unlimited and removing a \
         credentials block un-gates a project, so both are edits only ROUNDHOUSE_CONTROL_PLANE \
         can make, where the whole document is in front of whoever makes them. Omit `{axis}` to \
         leave it unchanged"
    )]
    NullPatchUnsupported { project: String, axis: &'static str },
    /// The merged config did not compile.
    ///
    /// The same boundary, and the same words, a boot failure would have used —
    /// which is the point of compiling admin state through it.
    #[error("this change would not load as a control plane: {0}")]
    Invalid(#[source] ControlPlaneError),
    /// The merged config compiled but the deployment could not serve it.
    #[error("this change would not start this deployment ({check}): {detail}")]
    CrossCheckRefused { check: &'static str, detail: String },
    /// The *process* is missing something the config names.
    ///
    /// Split out of [`Self::Invalid`] because the remedy is somewhere else
    /// entirely: an unset environment variable is not a bad request, and
    /// answering 422 would send an operator to re-read a `PATCH` body that was
    /// correct. It is reported as a server fault naming the variable, because
    /// that is what it is.
    #[error("this deployment is missing something its control plane names: {0}")]
    EnvironmentIncomplete(#[source] ControlPlaneError),
    #[error(transparent)]
    Store(#[from] StoreFailure),
    #[error(transparent)]
    Mint(#[from] MintError),
    /// A record referenced something that should have been impossible to
    /// remove.
    ///
    /// Never a caller's fault: every route that could orphan a row cascades
    /// instead. Reported rather than repaired, because the repair — dropping the
    /// key — would turn a revocation this deployment cannot explain into an
    /// `unknown_key` nobody investigates.
    #[error("internal inconsistency in the control directory: {detail}")]
    Inconsistent { detail: String },
}

impl From<CrossCheckRefusal> for DirectoryError {
    fn from(refusal: CrossCheckRefusal) -> Self {
        DirectoryError::CrossCheckRefused {
            check: refusal.check,
            detail: refusal.detail,
        }
    }
}

impl From<ControlPlaneError> for DirectoryError {
    /// Sorted into the two that read completely differently to whoever is
    /// holding the failed request. See [`DirectoryError::EnvironmentIncomplete`].
    fn from(error: ControlPlaneError) -> Self {
        match error {
            ControlPlaneError::CredentialEnvVarUnset { .. } => {
                DirectoryError::EnvironmentIncomplete(error)
            }
            other => DirectoryError::Invalid(other),
        }
    }
}
