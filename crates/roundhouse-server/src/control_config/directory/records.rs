// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The rows: what a project, a user, a membership and a key are here.
//!
//! The entity vocabulary and nothing that decides anything with it. Each record
//! *wraps* the config entry the file would carry rather than restating its
//! fields — which is what makes "admin-created entities are expressed in the
//! file's vocabulary" true of the storage and not only of the compile step.
//! Who may change them is [`super`]'s question; what they compile to is
//! [`ControlDirectory::plane`](super::ControlDirectory::plane)'s.
//!
//! # R-D7 — these rows are what gets written down
//!
//! Every type here is `Serialize` as well as `Deserialize` since M16.1, and
//! the asymmetry that preceded it is worth naming rather than quietly fixing:
//! the wrapped config entries were read-only because a *file* is read and
//! never written, and these rows had no serde at all because they lived in a
//! `Mutex` that outlived nothing. Both stop being true the moment the
//! directory is a durable document — the rows are the deployment's tenancy,
//! and the entries they wrap go into the document with them.
//!
//! **Every optional field carries `#[serde(default)]`, and that is the
//! forward-compatibility rule rather than tidiness.** A node runs whatever
//! build it was deployed with, and two builds share one document during any
//! rolling upgrade: a newer build reading an older document must not fail
//! because a field it learned about last week is absent, or the older half of
//! the fleet's writes become unreadable to the newer half. What the *reverse*
//! direction costs — an older build meeting a field it has never heard of — is
//! decided at the envelope, not here: see
//! [`document`](super::document) on the `schema` number and on why these rows
//! keep the file vocabulary's `deny_unknown_fields` instead of tolerating
//! their way past a vocabulary change nobody declared.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::super::budget::AllocationConfig;
use super::super::config::{PolicyConfig, ProjectEntry, UserEntry};
use super::super::fair_use::FairUseConfig;

/// Who owns a row: the file, or the API.
///
/// The one fact every mutation is checked against — see the module doc. A
/// boolean named `from_file` would carry the same information and none of the
/// meaning: what this decides is *who may edit this*, and that reads as a
/// question about ownership rather than about origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Declared in `ROUNDHOUSE_CONTROL_PLANE`. Read-only to this API.
    Config,
    /// Created over `/v1/admin`. Owned here.
    Admin,
}

impl fmt::Display for Provenance {
    /// The word a listing shows beside every row, so an operator can see which
    /// of their two sources of truth a project came from before they try to
    /// edit it and are refused.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Provenance::Config => "config",
            Provenance::Admin => "admin",
        })
    }
}

/// Which kind of thing an identity error is about.
///
/// Carried on the collision and ownership errors so the surface can say
/// "project `acme`" rather than "`acme`", which is the difference between an
/// operator knowing where to look and guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Project,
    User,
    Membership,
    Key,
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EntityKind::Project => "project",
            EntityKind::User => "user",
            EntityKind::Membership => "membership",
            EntityKind::Key => "key",
        })
    }
}

/// What a member is to their project.
///
/// **Stored and not yet load-bearing, and this sentence is the whole of its
/// contract in M8.** Nothing reads it: an `Owner` may do exactly what a
/// `Member` may, because every entitlement in this system is resolved from a
/// *key*, and the admin surface is gated on `KeyScope::Admin` rather than on
/// anything about a membership. It is recorded now so that the day a
/// project-scoped admin key exists there is a field to hang it on and a history
/// of who was what; a role invented at that point would have no past.
///
/// `Deserialize` because it is the one field of a membership `PUT` that is not
/// already config vocabulary: a project's policy and a member's allocation are
/// shapes the file declares and the body reuses, and this is the exception the
/// file has no spelling for at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipRole {
    Owner,
    Member,
}

impl fmt::Display for MembershipRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            MembershipRole::Owner => "owner",
            MembershipRole::Member => "member",
        })
    }
}

/// One project, as the directory knows it.
///
/// Wraps [`ProjectEntry`] rather than restating its fields, which is what makes
/// "admin-created entities are expressed in the file's vocabulary" true of the
/// storage and not only of the compile step: there is no second spelling of a
/// project's policy for the two halves to disagree in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRecord {
    /// Exactly what the file's `"projects"` array would carry for this project.
    pub entry: ProjectEntry,
    pub provenance: Provenance,
    /// When this row was created, or `None` for a file-owned one.
    ///
    /// `None` rather than the epoch or the boot time: the file does not date its
    /// entries, and a timestamp invented here would be indistinguishable from
    /// one an operator could rely on.
    #[serde(default)]
    pub created_at_ms: Option<u64>,
    /// When this project was archived, if it was.
    ///
    /// **Archived, never deleted.** A project's spend history outlives the
    /// project — the ledger's rows are keyed by principal and do not vanish — so
    /// a deployment that dropped the row would answer `unknown_key` for a
    /// membership its own ledger still has numbers for, and would free the id
    /// for a *different* project to be created under, silently joining two
    /// tenants' histories.
    ///
    /// There is no un-archive route in M8, so this is terminal and the id stays
    /// taken. That is deliberate rather than unfinished: un-archiving has to
    /// decide what happens to the keys that were refused while the project was
    /// closed, and that question has no obviously right answer to guess at here.
    #[serde(default)]
    pub archived_at_ms: Option<u64>,
}

impl ProjectRecord {
    pub fn id(&self) -> &str {
        &self.entry.id
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at_ms.is_some()
    }
}

/// One user, as the directory knows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    pub entry: UserEntry,
    pub provenance: Provenance,
    #[serde(default)]
    pub created_at_ms: Option<u64>,
}

impl UserRecord {
    pub fn id(&self) -> &str {
        &self.entry.id
    }
}

/// One person's place in one project: the entity a key is minted *under*.
///
/// **First-class, rather than implied by a key's `project`/`user` pair.** The
/// file implies its memberships that way and gets away with it because a file
/// is rewritten whole; an API cannot, because two keys of one membership then
/// have two independent copies of that membership's entitlements, and the day
/// they differ [`ControlPlane::membership`] refuses to describe the membership
/// at all. Here the entitlements live on the membership and every one of its
/// keys is compiled *from* it, so two keys of one membership cannot disagree —
/// not by convention, but because there is only one place the answer is stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MembershipRecord {
    pub project: String,
    pub user: String,
    /// `None` for a file-declared membership.
    ///
    /// The file has no role vocabulary — it declares memberships implicitly,
    /// through its `keys` entries — so a projected row's role is *absent*
    /// rather than defaulted to [`MembershipRole::Member`], which would be a
    /// fact this deployment invented and then displayed as if an operator had
    /// written it.
    #[serde(default)]
    pub role: Option<MembershipRole>,
    /// This membership's ceiling inside its project's budget, stamped onto
    /// every key compiled from it. `None` is [`Allocation::Pooled`] — no
    /// *second* ceiling, not no budget.
    ///
    /// Always `None` on a file-declared membership: there, an allocation is
    /// written per key, and restating one of them here would pick a winner
    /// among rows the file is entitled to disagree about.
    ///
    /// [`Allocation::Pooled`]: roundhouse_core::control::Allocation::Pooled
    #[serde(default)]
    pub allocation: Option<AllocationConfig>,
    /// A narrowing overlay on the project's policy, stamped onto every key
    /// compiled from this membership. `None` touches nothing.
    ///
    /// Always `None` on a file-declared membership, for the reason
    /// [`Self::allocation`] is.
    #[serde(default)]
    pub overrides: Option<PolicyConfig>,
    pub provenance: Provenance,
    #[serde(default)]
    pub created_at_ms: Option<u64>,
}

impl MembershipRecord {
    /// Whether this row is the membership `(project, user)` names.
    pub fn names(&self, project: &str, user: &str) -> bool {
        self.project == project && self.user == user
    }
}

/// What a key may do, as the directory records it.
///
/// The record's own vocabulary rather than [`KeyScope`](super::KeyScope): that
/// one carries a resolved [`Admission`](super::Admission), which is a fact
/// about a *compiled plane* and would put a policy snapshot inside a stored
/// row — where it would go stale the first time its membership was edited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyRecordScope {
    /// Pays for turns as one membership.
    Turn { project: String, user: String },
    /// Administers the deployment. Belongs to no project.
    Admin,
}

/// One key, as the directory records it — and never the secret itself.
///
/// **There is no field the plaintext could go in**, which is what makes
/// "returned once and never again" a property of this type rather than of a
/// handler that remembers. See [`MintedKey`], which is the only thing that ever
/// holds a secret and which is dropped with the response that carried it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    /// The public handle a revocation names: `key_` and the first sixteen
    /// characters of the hash. See [`key_id`].
    pub id: String,
    /// `sha256(secret)`, hex — the lookup key, and the only form of the secret
    /// this deployment keeps.
    pub key_sha256: String,
    /// The last four characters of the secret, or `None` for a file-declared
    /// key.
    ///
    /// `None` because the file carries only a hash: this deployment has never
    /// seen that key's plaintext and cannot show its tail. Displaying four
    /// characters of the *hash* instead would be a string that looks exactly
    /// like the one an operator is trying to match against their secret manager
    /// and never matches it.
    #[serde(default)]
    pub display_tail: Option<String>,
    pub scope: KeyRecordScope,
    pub provenance: Provenance,
    #[serde(default)]
    pub created_at_ms: Option<u64>,
    /// When this key was revoked, if it was. See the module doc on tombstones.
    #[serde(default)]
    pub revoked_at_ms: Option<u64>,
    /// This member's own fair-use windows, as the file declared them.
    ///
    /// Carried on the record rather than looked up from the file at render
    /// time, so the read surface answers from the same view every other field
    /// comes from. Always `None` for an API-minted key and for an admin key:
    /// there is no route that writes a member window, and a value here that the
    /// admin plane invented would be a ceiling nobody wrote (G14).
    #[serde(default)]
    pub fair_use: Option<FairUseConfig>,
}

impl ApiKeyRecord {
    pub fn is_revoked(&self) -> bool {
        self.revoked_at_ms.is_some()
    }
}

/// The public handle for a key: `key_` and the first sixteen hex characters of
/// its sha256.
///
/// Derived rather than drawn, and that is what lets a *file-declared* key have
/// an id at all — the file carries no id field, and adding one would make every
/// existing deployment's config incomplete. Sixteen hex characters is 64 bits,
/// so a collision inside one deployment's key set is not a case worth writing
/// code for; and disclosing a prefix of a SHA-256 discloses nothing, since the
/// hash is not a secret (it is written in the file in plain sight) and the
/// preimage is 32 CSPRNG bytes.
pub fn key_id(key_sha256: &str) -> String {
    let head: String = key_sha256.chars().take(16).collect();
    format!("key_{head}")
}

// ---------------------------------------------------------------------------
// The stored half
// ---------------------------------------------------------------------------

/// Everything the API has created, and nothing the file declares.
///
/// `Vec` rather than a map keyed by id: the collections are small (a
/// deployment's projects and members, not its sessions), creation order is what
/// a list route wants to show, and a map would need a second structure for the
/// order anyway. Lookups are linear and deliberately so.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DirectoryRecords {
    /// `#[serde(default)]` on all four, so a document written before a
    /// collection existed still loads as "none of those" rather than failing
    /// the whole directory. Four empty vectors is exactly the empty
    /// directory, which is the value a first boot compiles from anyway.
    #[serde(default)]
    pub projects: Vec<ProjectRecord>,
    #[serde(default)]
    pub users: Vec<UserRecord>,
    #[serde(default)]
    pub memberships: Vec<MembershipRecord>,
    #[serde(default)]
    pub keys: Vec<ApiKeyRecord>,
}

/// The lookups the directory's own rules are written in terms of.
///
/// `pub(super)` rather than `pub`: these are how [`super`]'s ownership checks
/// ask their questions, and a surface reaching past them would be a second
/// place "does this project exist" is decided. What a *caller* reads is
/// [`DirectoryView`], which is a snapshot rather than a live handle.
impl DirectoryRecords {
    pub(super) fn project(&self, id: &str) -> Option<&ProjectRecord> {
        self.projects.iter().find(|project| project.id() == id)
    }

    pub(super) fn project_mut(&mut self, id: &str) -> Option<&mut ProjectRecord> {
        self.projects.iter_mut().find(|project| project.id() == id)
    }

    pub(super) fn user(&self, id: &str) -> Option<&UserRecord> {
        self.users.iter().find(|user| user.id() == id)
    }

    pub(super) fn membership(&self, project: &str, user: &str) -> Option<&MembershipRecord> {
        self.memberships
            .iter()
            .find(|membership| membership.names(project, user))
    }

    pub(super) fn holds_hash(&self, key_sha256: &str) -> bool {
        self.keys.iter().any(|key| key.key_sha256 == key_sha256)
    }
}

/// Every entity a `GET` lists, file-owned and API-owned together.
///
/// One struct rather than four accessors, because the four are read together by
/// every caller that has one and because taking them separately would be four
/// chances to read them across a refresh.
///
/// A snapshot, and never a handle: it is the projection
/// [`ControlDirectory::view`](super::ControlDirectory::view) built at one
/// version, with the file's own entities in it. Holding a live borrow of the
/// records instead would mean a list route deciding for itself what to do when
/// a write landed halfway through rendering it.
#[derive(Debug, Clone)]
pub struct DirectoryView {
    pub projects: Vec<ProjectRecord>,
    pub users: Vec<UserRecord>,
    pub memberships: Vec<MembershipRecord>,
    pub keys: Vec<ApiKeyRecord>,
}
