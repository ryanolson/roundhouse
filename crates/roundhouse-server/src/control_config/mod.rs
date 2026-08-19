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
//! # Division of labour
//!
//! Three files, split along the line between *a file on disk* and *a request
//! on the wire* — the two ends of this module, which look alike (both check
//! the shape of a string that names a key) and are read at opposite ends of
//! the system:
//!
//! - [`config`] holds the format and the boundary that judges it:
//!   [`ProjectEntry`], [`UserEntry`], [`KeyEntry`], [`PolicyConfig`],
//!   [`ControlPlaneConfig`], [`ControlPlaneError`], and
//!   `ControlPlaneConfig::validate`. Nothing there sees a header.
//! - [`auth`] holds the refusal table: [`AuthError`], its codes, its statuses
//!   and its wire body. Nothing there decides *which* row applies.
//! - **This file** holds the runtime: [`ControlPlane`], [`KeyScope`],
//!   [`Admission`], and the resolution that turns a presented secret into one
//!   of them. It is the only one of the three that reads a [`HeaderMap`].
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
//! `Arc<TurnPolicy>` in one lookup; [`ControlPlane::turn_principal`] is a thin
//! projection of it, for the two surfaces — `create_session` and the event
//! stream — that open or read a session rather than serving a turn, and so
//! have no policy question to ask.
//!
//! [`HeaderMap`]: axum::http::HeaderMap

pub mod auth;
pub mod budget;
pub mod config;
pub mod validate;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use sha2::{Digest, Sha256};

use roundhouse_core::control::{BudgetTerms, Principal, TurnPolicy};
use roundhouse_core::ids::SessionId;
use roundhouse_core::validate::ValidationTerms;

use crate::dialect::ClientDialect;

pub use auth::AuthError;
pub use budget::{AllocationConfig, BudgetConfig, OnExhaustionConfig};
pub use config::{
    ControlPlaneConfig, ControlPlaneError, KeyEntry, PolicyConfig, ProjectEntry, UserEntry,
};
pub use validate::{ArmSharesConfig, ValidateConfig};

/// Path to a control-plane JSON file. Absent means [`ControlPlane::Open`].
pub const CONTROL_PLANE_VAR: &str = "ROUNDHOUSE_CONTROL_PLANE";

/// The dialect [`ControlPlane::Open`] answers with.
///
/// A shared value rather than one built per call, because
/// [`ControlPlane::client_dialect`] is asked once per request and a
/// [`ClientDialect`] owns a `String`: an unconfigured deployment should not
/// allocate a namespace to say it is using the default one.
static OPEN_DIALECT: LazyLock<ClientDialect> = LazyLock::new(ClientDialect::default);

/// `true` for `rh_(turn|admin)_` followed by 43 base62 characters — the shape
/// a *presented* secret must have before its hash is even looked up, so an
/// obviously-wrong header never reaches the hash table.
///
/// In this file with the resolver rather than in [`config`] with the config
/// validators: this one is about what a *client sends*, and the ones there are
/// about what an operator *wrote in a file*. They look alike and are checked at
/// opposite ends of the system, which is the whole reason the two are separate
/// files.
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
/// Beside [`ControlPlane::scope`], its only producer, rather than in
/// `roundhouse-core`'s control vocabulary: a scope is a fact about a
/// *credential*, and core deliberately knows nothing about credentials — see
/// [`roundhouse_core::control`] on why a [`Principal`] carries no key. Nothing
/// below this crate has any use for the distinction.
///
/// The turn arm carries the whole [`Admission`] rather than a bare
/// [`Principal`]. There used to be a second, private enum with exactly this
/// shape sitting behind it — `scope` projected the policy away and
/// `turn_admission` kept it — which meant one resolution answer expressed as
/// two types, and a `resolve` whose only job was to convert between them.
/// Carrying the pair here costs a caller that wants only identity one field
/// access, and it buys back the guarantee that both halves of an admission
/// come from the same lookup of the same key, at the *one* type the resolver
/// produces.
#[derive(Debug, Clone)]
pub enum KeyScope {
    /// Pays for turns as one membership, under one policy.
    Turn(Admission),
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
    /// A key is required; `authenticate` looks up its hash.
    Configured {
        /// `sha256(secret)` hex, to the complete [`Admission`] resolved for
        /// it at load time: its membership, its effective [`TurnPolicy`]
        /// (its project's policy narrowed by its own overrides), and its
        /// effective budget terms (its project's budget paired with its own
        /// allocation, or `None`) — see `ControlPlaneConfig::validate`, which
        /// builds this table.
        turn_keys: HashMap<String, Admission>,
        /// `sha256(secret)` hex, for keys with no membership to spend as.
        admin_keys: HashSet<String>,
        /// How this deployment's synthetic tool calls are spelled on the
        /// wire, resolved once at load time from the file's optional
        /// `"mcp_namespace"`.
        ///
        /// Beside the key tables rather than in an `EngineConfig` because it
        /// is a *client-facing* name: the engine never renders a wire frame,
        /// and this is the same question `qualify` answers for session ids —
        /// what does this deployment call the things its clients say back to
        /// it. See [`Self::client_dialect`].
        dialect: ClientDialect,
        /// What this deployment hashes arm assignment against, resolved once
        /// at load time. Read by the composition root on its way into
        /// [`EngineConfig`](crate::EngineConfig) and by nothing else.
        arm_salt: String,
    },
}

impl ControlPlane {
    /// Build the runtime resolver from a validated config.
    ///
    /// Takes `self` by value rather than `&ControlPlaneConfig`: the config's
    /// `Vec`s exist only to be turned into these two lookup tables, and there
    /// is no second reader of the parsed form once resolution is wired.
    ///
    /// A move and nothing else. `validate` already built `turn_keys` as it
    /// judged each key, so there is no re-join here, no lookup that could
    /// miss, and therefore no default for a missed lookup to fall back to —
    /// see [`ControlPlaneConfig::turn_keys`](config::ControlPlaneConfig).
    pub fn configured(config: ControlPlaneConfig) -> Self {
        let ControlPlaneConfig {
            projects: _,
            users: _,
            keys: _,
            admin_keys,
            mcp_namespace,
            arm_salt,
            turn_keys,
        } = config;
        ControlPlane::Configured {
            turn_keys,
            admin_keys: admin_keys.into_iter().collect(),
            arm_salt: arm_salt.unwrap_or_default(),
            // An absent name is the default one rather than an absence
            // carried forward: see [`ClientDialect::default`] on why there is
            // no honest `None` for a surface every client of which is a
            // Responses client.
            dialect: match mcp_namespace {
                Some(namespace) => ClientDialect::CodexResponses { namespace },
                None => ClientDialect::default(),
            },
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

    /// Every configured membership and everything its key resolves to, in no
    /// particular order.
    ///
    /// The one way to read the whole table, and it exists so that the sentence
    /// on [`Self::turn_admission`] — one lookup, one place the table's shape is
    /// known — stays literally true of a binary that also wants to *audit* the
    /// table at startup. Without it the composition root reaches in with a
    /// `let ControlPlane::Configured { turn_keys, .. } = plane else { ... }`,
    /// which is a second reader of the same layout, and the kind of second
    /// reader that keeps compiling after the first one changes shape.
    ///
    /// [`Self::Open`] yields nothing, and that is the accurate answer rather
    /// than a special case to handle: an open deployment has no configured
    /// memberships. Every caller so far wants to check something about *each
    /// configured key*, and "there are none" is exactly the right number of
    /// things to check.
    ///
    /// It yields the whole [`Admission`] rather than the
    /// `(&Principal, &TurnPolicy)` pair it used to: the startup cross-check
    /// now asks about a key's *budget* as well as its policy, and projecting
    /// one field away here would have sent the composition root into
    /// `Configured { turn_keys, .. }` for the other — reinstating the second
    /// reader of the table's layout that this accessor exists to prevent.
    pub fn configured_admissions(&self) -> impl Iterator<Item = &Admission> {
        let table = match self {
            ControlPlane::Open => None,
            ControlPlane::Configured { turn_keys, .. } => Some(turn_keys),
        };
        table.into_iter().flat_map(|turn_keys| turn_keys.values())
    }

    /// The one admission this principal's turns are made under.
    ///
    /// **The one place identity is resolved backwards**, and the only caller
    /// that needs it is the MCP control surface: every other surface resolves a
    /// *secret* to an [`Admission`] through [`Self::turn_admission`], but a tool
    /// handler is handed a [`Principal`] the transport already resolved, and it
    /// asks entitlement questions — what may this key be routed to, what is left
    /// to spend — about *that*.
    ///
    /// The config keys entitlements by secret and permits two keys to name one
    /// membership with different `overrides`, so the backwards question has no
    /// answer where they disagree. It is refused rather than resolved: picking
    /// either would answer an agent about a policy its own key does not have,
    /// which is a wrong answer that looks exactly like a right one.
    /// [`ambiguous_memberships`](Self::ambiguous_memberships) asks the same
    /// question at boot, so an operator learns it from a startup refusal rather
    /// than from a tenant.
    ///
    /// [`Self::Open`] resolves to [`Admission::open`] for every principal, which
    /// is the same value it admits every *request* as — one definition of what
    /// an unconfigured deployment allows, not two.
    pub fn membership(&self, principal: &Principal) -> Result<Admission, MembershipError> {
        match self {
            ControlPlane::Open => Ok(Admission::open()),
            ControlPlane::Configured { .. } => {
                let mut found = self
                    .configured_admissions()
                    .filter(|admission| &admission.principal == principal);
                let first = found
                    .next()
                    .ok_or_else(|| MembershipError::Unknown(principal.clone()))?;
                // Two keys naming one membership are fine as long as they mean
                // the same thing — an operator rotating a secret has two rows
                // for a while, and refusing that would make rotation an outage.
                // What cannot be resolved is two keys that mean different
                // things.
                //
                // **Meaning, through [`TurnPolicy::admits_the_same_as`], and
                // not [`TurnPolicy::digest`].** A digest fingerprints how a
                // policy was *written* — which is what makes it the right
                // thing to stamp on a `DecisionRecord` and the wrong thing to
                // compare two keys by. A key that inherits its project's
                // `allow` and a key that restates the same filter as its own
                // override admit exactly the same targets and fingerprint
                // differently, because the restatement intersects into a
                // second identical layer; comparing digests turned that
                // rotation into a boot failure whose message ("different
                // policies or budgets") was not merely unhelpful but untrue.
                // The budget stays a structural comparison: it is a pair of
                // numbers with one spelling.
                for other in found {
                    if !other.policy.admits_the_same_as(&first.policy)
                        || other.budget != first.budget
                    {
                        return Err(MembershipError::Ambiguous(principal.clone()));
                    }
                }
                Ok(first.clone())
            }
        }
    }

    /// Every membership whose keys disagree about what it may do.
    ///
    /// The boot-time half of [`Self::membership`], collected rather than
    /// reported on the first hit for the reason the other startup cross-checks
    /// are: the table is a hash map, so a deployment with two bad memberships
    /// would otherwise be told about a different one on each restart.
    pub fn ambiguous_memberships(&self) -> Vec<Principal> {
        let mut ambiguous: Vec<Principal> = self
            .configured_admissions()
            .map(|admission| admission.principal.clone())
            .filter(|principal| {
                matches!(
                    self.membership(principal),
                    Err(MembershipError::Ambiguous(_))
                )
            })
            .collect();
        ambiguous
            .sort_by_key(|principal| (principal.project.to_string(), principal.user.to_string()));
        ambiguous.dedup();
        ambiguous
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

    /// How this deployment's synthetic tool calls are spelled on the wire.
    ///
    /// The one reader of the deployment's `"mcp_namespace"`, and the one place
    /// [`ControlPlane::Open`]'s answer is decided — a default rather than an
    /// absence, exactly as [`Admission::open`] is the *value* an unconfigured
    /// deployment admits every request as rather than a flag meaning "no
    /// admission". An open deployment still serves Codex clients, so it still
    /// has to name the namespace they would resolve a call against.
    ///
    /// Deliberately not `Option`-returning: a `None` here would put a case on
    /// the wire projection whose only possible behavior is to emit a call that
    /// resolves against nothing.
    /// What arm assignment is hashed against.
    ///
    /// Empty in [`Self::Open`], which is the accurate answer rather than a
    /// placeholder: an open deployment enrols nothing, so nothing is ever
    /// hashed against it.
    pub fn arm_salt(&self) -> &str {
        match self {
            ControlPlane::Open => "",
            ControlPlane::Configured { arm_salt, .. } => arm_salt,
        }
    }

    pub fn client_dialect(&self) -> &ClientDialect {
        match self {
            // Borrowed from a shared value rather than built per call: the
            // projection asks once per request, and an unconfigured deployment
            // should not allocate a namespace string to answer.
            ControlPlane::Open => &OPEN_DIALECT,
            ControlPlane::Configured { dialect, .. } => dialect,
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
        self.authenticate(header)
    }

    /// The membership a turn-serving surface's caller spends as.
    ///
    /// A thin projection of [`Self::turn_admission`] for the surfaces that
    /// need identity and have no policy question to ask — see that method for
    /// the shared logic, including why an admin key is refused here rather
    /// than quietly given a principal of its own.
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
    /// The one other reader of the whole table goes through
    /// [`Self::configured_admissions`] for exactly that reason.
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
        match self.scope(headers)? {
            KeyScope::Turn(admission) => Ok(admission),
            KeyScope::Admin => Err(AuthError::WrongKeyKind),
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
    /// Private, and takes the header value rather than the map: [`Self::scope`]
    /// and [`Self::turn_admission`] are the two public ways in, both read the
    /// header through [`Self::header_str`] first, and keeping the pure core
    /// separate is what lets the tests below exercise resolution as a function
    /// of a string without a [`HeaderMap`] to build.
    fn authenticate(&self, authorization_header: Option<&str>) -> Result<KeyScope, AuthError> {
        match self {
            ControlPlane::Open => Ok(KeyScope::Turn(Admission::open())),
            ControlPlane::Configured {
                turn_keys,
                admin_keys,
                // Named and ignored rather than swept under a `..`: this arm
                // decides who a key is; the dialect decides how a call is
                // spelled and the salt decides which arm a session lands in,
                // and neither bears on identity. A field added here that
                // authentication does have to read should make this line stop
                // compiling.
                dialect: _,
                arm_salt: _,
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
                    return Ok(KeyScope::Admin);
                }
                if let Some(admission) = turn_keys.get(&hash) {
                    return Ok(KeyScope::Turn(admission.clone()));
                }
                Err(AuthError::UnknownKey)
            }
        }
    }
}

/// Why a [`Principal`] does not resolve to one set of entitlements.
///
/// Both arms are errors and neither has a default, which is the whole point:
/// the caller is a tool handler about to tell an agent what its key may do, and
/// there is no honest thing to say when the deployment does not know.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MembershipError {
    #[error("no configured key names the membership `{0}`")]
    Unknown(Principal),
    #[error(
        "the membership `{0}` is named by two keys with different entitlements, so there is no \
         single policy or budget to report for it"
    )]
    Ambiguous(Principal),
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
    /// This membership's fully resolved budget ceilings — its project's
    /// [`Budget`](roundhouse_core::control::Budget) paired with its own
    /// [`Allocation`](roundhouse_core::control::Allocation) — or `None` when
    /// the project has no `"budget"` configured.
    ///
    /// `None` is what lets the engine skip the ledger entirely on the
    /// open-mode and no-budget paths (decision 4): a `Some` here always
    /// means a real ceiling somebody wrote down, never an infinite one
    /// standing in for "no ceiling". Resolved once, here, alongside
    /// `policy` — the same one-seam reasoning `Self::open` states for the
    /// pair it already carries: two facts resolved from the same key must
    /// travel together or a caller can read one without the other.
    pub budget: Option<BudgetTerms>,
    /// Whether this membership's sessions are enrolled in the validate/steer
    /// loop, and under what arms — or `None` for the shipped posture, which is
    /// off.
    ///
    /// `None` is not "validate with default settings" and not "validate but do
    /// nothing": it is *not enrolled*, and it costs a turn exactly nothing.
    /// The engine reads it once, at session creation, to decide whether to
    /// stamp an arm into `SessionCreated`; an unstamped session is one the
    /// validator declines to be asked about, so a deployment that has not
    /// turned the loop on pays for no trigger, no brief and no judge. Resolved
    /// here beside the policy and the budget for the reason they are: three
    /// facts read off one key must travel together.
    pub validation: Option<ValidationTerms>,
}

impl Admission {
    /// What an unconfigured deployment admits every request as: the one
    /// built-in membership, under the policy that changes no routing decision
    /// and with no budget to spend against.
    ///
    /// Named once rather than spelled at each site, and it is the value
    /// [`ControlPlane::Open`] itself resolves to — so this is the definition
    /// of open mode's admission and not a convenience beside it. The three
    /// fields have to travel together: an open deployment that paired the
    /// default principal with anything narrower than
    /// [`TurnPolicy::unrestricted`] or with a real budget would re-route or
    /// meter workloads that predate the control plane, which is the one
    /// thing turning it on must not do.
    pub fn open() -> Self {
        Self {
            principal: Principal::default_open(),
            policy: Arc::new(TurnPolicy::unrestricted()),
            budget: None,
            // An open deployment has no file to enable the experiment in, and
            // enrolling its traffic anyway would meter and interrupt workloads
            // that predate the control plane — the one thing turning it on
            // must not do.
            validation: None,
        }
    }
}

/// Secret/hash pairs and the fixture config that declares them.
///
/// In the parent module rather than in either child's test module because the
/// coupling *is* the fixture: [`config`]'s tests need the hashes an operator
/// would write in a file, and this file's tests need the secrets a client
/// would present, and the two are only a fixture at all because one is the
/// SHA-256 of the other.
#[cfg(test)]
pub(crate) mod fixtures {
    use axum::http::HeaderMap;
    use axum::http::header::AUTHORIZATION;

    // Regenerate with:
    //   python3 -c "import hashlib; print(hashlib.sha256(b'...').hexdigest())"
    pub const TURN_SECRET: &str = "rh_turn_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    pub const TURN_HASH: &str = "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677ddfd";
    pub const ADMIN_SECRET: &str = "rh_admin_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    pub const ADMIN_HASH: &str = "d2166d25b0938bced2c878c396356867ee6f05abaa02f4ad4b80a3cdbe5c1ff3";
    /// Well-shaped and well-hashed, but declared nowhere in any fixture config.
    pub const UNKNOWN_SECRET: &str = "rh_turn_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";

    /// One project, one user, one turn key and one admin key, no policies.
    pub fn sample_config() -> &'static str {
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

    pub fn bearer_headers(secret: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            format!("Bearer {secret}").parse().expect("a valid value"),
        );
        headers
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use roundhouse_core::control::FrontierCadence;

    /// A [`KeyScope`] flattened to what the resolution tests assert on.
    ///
    /// `KeyScope::Turn` carries an `Arc<TurnPolicy>` and the tests below are
    /// about *which membership a secret resolves to*; the policy half has its
    /// own tests further down. Comparing this instead of deriving `PartialEq`
    /// on `KeyScope` keeps an equality on the real type from quietly becoming
    /// a policy comparison somebody did not mean to write.
    #[derive(Debug, PartialEq, Eq)]
    enum Resolved {
        Turn(Principal),
        Admin,
    }

    fn resolved(plane: &ControlPlane, header: Option<&str>) -> Result<Resolved, AuthError> {
        plane.authenticate(header).map(|scope| match scope {
            KeyScope::Turn(admission) => Resolved::Turn(admission.principal),
            KeyScope::Admin => Resolved::Admin,
        })
    }

    #[test]
    fn open_mode_resolves_every_request_to_the_default_principal() {
        let plane = ControlPlane::Open;
        for header in [None, Some("Bearer rh_turn_garbage"), Some("not even close")] {
            assert_eq!(
                resolved(&plane, header).expect("Open mode never refuses"),
                Resolved::Turn(Principal::default_open())
            );
        }
    }

    #[test]
    fn a_missing_header_is_missing_key_and_a_wrong_shape_is_malformed_key() {
        let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
        let plane = ControlPlane::configured(config);

        assert_eq!(resolved(&plane, None), Err(AuthError::MissingKey));
        for header in [
            "not a bearer header",
            "Bearer rh_turn_tooshort",
            "Bearer rh_something_else",
        ] {
            assert_eq!(
                resolved(&plane, Some(header)),
                Err(AuthError::MalformedKey),
                "`{header}`"
            );
        }
    }

    #[test]
    fn an_unknown_hash_is_unknown_key_and_a_known_turn_key_resolves_to_its_principal() {
        let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
        let plane = ControlPlane::configured(config);

        let unknown = format!("Bearer {UNKNOWN_SECRET}");
        assert_eq!(resolved(&plane, Some(&unknown)), Err(AuthError::UnknownKey));

        let known = format!("Bearer {TURN_SECRET}");
        assert_eq!(
            resolved(&plane, Some(&known)),
            Ok(Resolved::Turn(Principal::new("acme", "ada")))
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
        assert_eq!(resolved(&plane, Some(&header)), Ok(Resolved::Admin));
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
        assert_eq!(plane.scope(&headers).err(), Some(AuthError::MalformedKey));

        assert_eq!(
            plane.scope(&HeaderMap::new()).err(),
            Some(AuthError::MissingKey),
            "no header at all is still the missing-key row"
        );
    }

    #[test]
    fn an_admin_key_may_not_spend_as_a_membership() {
        let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
        let plane = ControlPlane::configured(config);

        let headers = bearer_headers(ADMIN_SECRET);
        assert!(
            matches!(plane.scope(&headers), Ok(KeyScope::Admin)),
            "the admin scope is what the key resolves to"
        );
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

    #[test]
    fn configured_admissions_walks_the_table_and_open_mode_walks_nothing() {
        // The accessor exists so the composition root does not destructure
        // `Configured` for itself; what it has to be is complete and
        // policy-accurate, or the startup cross-check built on it would pass a
        // key it never looked at.
        let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
        let plane = ControlPlane::configured(config);
        let seen: Vec<_> = plane.configured_admissions().collect();
        assert_eq!(seen.len(), 1, "one turn key in the fixture");
        assert_eq!(seen[0].principal, Principal::new("acme", "ada"));
        assert_eq!(*seen[0].policy, TurnPolicy::unrestricted());
        assert!(
            seen[0].budget.is_none(),
            "the fixture declares no `budget`, and absent means unlimited \
             rather than a ceiling nobody wrote"
        );

        assert_eq!(
            ControlPlane::Open.configured_admissions().count(),
            0,
            "an open deployment has no configured memberships, which is an \
             answer rather than a special case"
        );
    }

    /// Every deployment answers the dialect question, and the two answers come
    /// from one place.
    ///
    /// The Open arm is the half worth a test. An unconfigured deployment still
    /// serves Codex clients, so "no control plane" must not mean "no
    /// namespace": a projection handed an empty one would emit calls that
    /// resolve against nothing, and the turn would look perfectly healthy from
    /// both ends while the steer did nothing.
    #[test]
    fn every_deployment_names_a_namespace_and_a_configured_one_may_choose_it() {
        assert_eq!(
            *ControlPlane::Open.client_dialect(),
            ClientDialect::CodexResponses {
                namespace: crate::dialect::DEFAULT_MCP_NAMESPACE.to_string(),
            },
            "an open deployment renders the default rather than nothing"
        );

        let named = ControlPlane::configured(
            ControlPlaneConfig::from_json(
                r#"{
                  "projects": [{ "id": "acme" }],
                  "users": [{ "id": "ada" }],
                  "mcp_namespace": "mcp__acme"
                }"#,
                "test",
            )
            .expect("the fixture validates"),
        );
        assert_eq!(
            *named.client_dialect(),
            ClientDialect::CodexResponses {
                namespace: "mcp__acme".to_string(),
            },
            "and a configured one renders the name its operator wrote"
        );

        // The control: a configured deployment that named none falls back to
        // the same default the open one uses, rather than to an empty string.
        let unnamed = ControlPlane::configured(
            ControlPlaneConfig::from_json(sample_config(), "test").expect("the fixture validates"),
        );
        assert_eq!(
            unnamed.client_dialect(),
            ControlPlane::Open.client_dialect()
        );
    }
}
