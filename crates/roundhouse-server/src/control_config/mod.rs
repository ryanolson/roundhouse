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
pub mod credentials;
pub mod crosscheck;
pub mod directory;
pub mod validate;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use sha2::{Digest, Sha256};

use roundhouse_core::control::{
    BudgetCounts, BudgetTerms, PresentedCredential, Principal, TurnCredentials, TurnPolicy,
};
use roundhouse_core::ids::SessionId;
use roundhouse_core::validate::ValidationTerms;

use crate::dialect::ClientDialect;

pub use auth::AuthError;
pub use budget::{AllocationConfig, BudgetConfig, OnExhaustionConfig};
pub use config::{
    ControlPlaneConfig, ControlPlaneError, DEFAULT_ADMISSION_CACHE_TTL_MS, KeyEntry, PolicyConfig,
    ProjectEntry, UserEntry,
};
pub use credentials::{CredentialsConfig, ProviderCredentialConfig};
pub use crosscheck::{CrossCheckRefusal, CrossChecks};
pub use directory::{
    ApiKeyRecord, ControlDirectory, DirectoryError, DirectoryMutation, DirectoryRecords,
    DirectoryStore, DirectoryView, EntityKind, KeyFingerprint, KeyRecordScope, MembershipRecord,
    MembershipRole, MemoryDirectoryStore, PlaneSource, ProjectPatch, ProjectRecord, Provenance,
    StoreFailure, UserRecord,
};
pub use validate::{ArmSharesConfig, ValidateConfig};

/// Path to a control-plane JSON file. Absent means [`ControlPlane::Open`].
pub const CONTROL_PLANE_VAR: &str = "ROUNDHOUSE_CONTROL_PLANE";

/// The validated config named by [`CONTROL_PLANE_VAR`] and the path it came
/// from, or `None` when the variable is unset.
///
/// A variable that *is* set but names an unreadable or malformed file stops the
/// process, mirroring `catalog_config::from_env`: starting anyway would serve
/// every request as if no key were required, which is the exact failure a
/// deployment sets this variable to prevent.
///
/// The *config* rather than a finished [`ControlPlane`], which is what this
/// used to hand back. Since the admin plane the file is only half of what a
/// deployment authenticates against — see
/// [`ControlDirectory`](directory::ControlDirectory), which merges it with what
/// an operator created over the API and compiles the two together. Returning a
/// compiled plane here would have made the file's half look like the whole, and
/// the composition root would have had to take it apart again.
///
/// The path travels with the config because every refusal names it: a `PATCH`
/// refused because it collides with a file-declared project has to say which
/// file, and by then the variable has long been read.
pub fn config_from_env() -> Result<Option<(ControlPlaneConfig, String)>, ControlPlaneError> {
    match std::env::var(CONTROL_PLANE_VAR) {
        Ok(path) if !path.trim().is_empty() => {
            let path = path.trim().to_string();
            let config = ControlPlaneConfig::load(&path)?;
            Ok(Some((config, path)))
        }
        _ => Ok(None),
    }
}

/// The header a client may present its roundhouse turn key in, beside
/// `Authorization`.
///
/// **Load-bearing for pass-through, and for nothing else.** Under the
/// device-login stanza the client's `Authorization` belongs to *its* upstream —
/// it is the ChatGPT bearer codex forwards — so roundhouse's own key has to
/// arrive somewhere else, and `[model_providers.*.env_http_headers]` is the
/// mechanism codex offers (stage 0's ruling; PLAN §3). This is the header name
/// that stanza writes.
///
/// The name is lowercase because `HeaderMap` lookups are case-insensitive and a
/// constant that matched only one capitalization would be a rule about how a
/// client shouted rather than about what it sent.
pub const TURN_KEY_HEADER: &str = "x-roundhouse-key";

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
///
/// `pub` since the admin plane, for one reason: the milestone test that mints a
/// key asserts the secret against *this* predicate. A test that re-spelled the
/// prefix and the length would pass while the deployment refused its own freshly
/// issued key — which is precisely the drift that keeping [`mint_key`] beside
/// this function exists to prevent, reintroduced in the one place that was
/// supposed to catch it.
pub fn has_valid_key_shape(secret: &str) -> bool {
    let tail = secret
        .strip_prefix("rh_turn_")
        .or_else(|| secret.strip_prefix("rh_admin_"));
    match tail {
        Some(tail) => tail.len() == KEY_TAIL_LEN && tail.chars().all(|c| c.is_ascii_alphanumeric()),
        None => false,
    }
}

/// How many base62 digits a minted secret's tail carries.
///
/// Not a taste: 32 bytes is 256 bits, and 62^43 is 2^256.03 — the smallest
/// digit count that can render every 256-bit value. 42 digits would truncate
/// the entropy this file's opening paragraph claims, and 44 would pad every
/// secret with a digit that is always the same.
const KEY_TAIL_LEN: usize = 43;

/// The digits a minted secret's tail is spelled in.
///
/// Base62 rather than base64 because [`has_valid_key_shape`] is the gate every
/// presented secret passes, and it asks for `is_ascii_alphanumeric` — a `+`, a
/// `/` or a `=` would be refused by the deployment that issued it. Base62 also
/// survives a round trip through a URL, a shell, and an environment variable
/// without an encoding step nobody would remember to reverse.
const BASE62_DIGITS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Which prefix a minted secret wears, and therefore what a [`KeyScope`] match
/// may trust structurally about it.
///
/// An enum rather than a `&str` prefix the caller supplies: the prefix is half
/// of what the resolver reads, and a mint site free to invent one could issue a
/// secret this deployment refuses on sight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// `rh_turn_` — pays for turns as one membership.
    Turn,
    /// `rh_admin_` — reads and writes the control plane itself.
    Admin,
}

impl KeyKind {
    pub fn prefix(self) -> &'static str {
        match self {
            KeyKind::Turn => "rh_turn_",
            KeyKind::Admin => "rh_admin_",
        }
    }
}

/// The system CSPRNG was unavailable, so nothing was minted.
///
/// Surfaced rather than `expect`ed. On Linux this means the kernel could not
/// give the process randomness, which is not a state a control plane should
/// paper over by panicking inside a request handler — and the honest answer to
/// the caller is a 500 naming the cause, not a key drawn from something else.
#[derive(Debug, thiserror::Error)]
#[error("the system CSPRNG is unavailable, so no key could be minted: {source}")]
pub struct MintError {
    #[source]
    source: getrandom::Error,
}

/// A secret, and the two facts about it that outlive it.
///
/// **The plaintext leaves this struct exactly once.** Nothing that gets stored
/// carries it — see
/// [`ApiKeyRecord`](directory::ApiKeyRecord), which has no field it could go
/// in — so "returned once and never again" is a property of the types rather
/// than of a handler remembering not to log it.
#[derive(Debug)]
pub struct MintedKey {
    /// The secret as the operator will paste it. Held by value on the way to
    /// one response body and dropped with it.
    pub secret: String,
    /// `sha256(secret)`, hex — the lookup key, and the only form of the secret
    /// this deployment keeps.
    pub key_sha256: String,
    /// The last four characters of the secret.
    ///
    /// Enough for an operator to match a row in a list against the value in
    /// their secret manager, and far too little to reconstruct: four base62
    /// characters is ~24 bits of a 256-bit secret, so a table of every possible
    /// tail is 62^4 rows and identifies nothing.
    pub display_tail: String,
}

/// Mint a secret of `kind`: 32 CSPRNG bytes, base62, behind its role prefix.
///
/// Beside [`has_valid_key_shape`] on purpose — the function that *makes* a
/// secret and the function that *judges* one must agree on the alphabet and the
/// length, and the way they stay agreed is by being read together. A mint that
/// drifted would issue keys its own resolver refuses with `malformed_key`,
/// which reads to an operator like a paste error in the one place there was
/// none.
pub fn mint_key(kind: KeyKind) -> Result<MintedKey, MintError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|source| MintError { source })?;
    let secret = format!("{}{}", kind.prefix(), base62(bytes));
    let key_sha256 = hex::encode(Sha256::digest(secret.as_bytes()));
    // Safe to slice: the tail is base62, so every one of its bytes is one
    // ASCII character and the boundary is a character boundary.
    let display_tail = secret[secret.len() - 4..].to_string();
    Ok(MintedKey {
        secret,
        key_sha256,
        display_tail,
    })
}

/// A 256-bit big-endian value as exactly [`KEY_TAIL_LEN`] base62 digits.
///
/// Long division by 62 over the byte array rather than a big-integer
/// dependency, which would be a crate in the graph for forty lines of
/// schoolbook arithmetic. Digits are filled from the least significant end, so
/// a value below `62^42` is left-padded with the zero digit rather than coming
/// out one character short — which would be a secret this deployment's own
/// shape check refuses, on roughly one mint in sixty-two.
fn base62(bytes: [u8; 32]) -> String {
    let mut value = bytes;
    let mut digits = [0u8; KEY_TAIL_LEN];
    for digit in digits.iter_mut().rev() {
        let mut remainder = 0u32;
        for byte in value.iter_mut() {
            // `remainder` is below 62, so this stays well inside a `u32`.
            let accumulated = (remainder << 8) | u32::from(*byte);
            *byte = (accumulated / 62) as u8;
            remainder = accumulated % 62;
        }
        *digit = BASE62_DIGITS[remainder as usize];
    }
    debug_assert!(
        value.iter().all(|byte| *byte == 0),
        "62^43 exceeds 2^256, so 43 digits must consume the whole value"
    );
    String::from_utf8(digits.to_vec()).expect("every base62 digit is ASCII")
}

/// Why a hash that this deployment does recognize nevertheless resolves to
/// nothing.
///
/// The tombstone vocabulary, and the reason revocation deletes no row: a hash
/// with a reason attached is what lets [`AuthError::RevokedKey`] and
/// [`AuthError::ProjectArchived`] be told apart from `unknown_key` — and from
/// each other, which matters because their remedies are opposite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRefusal {
    /// The key itself was revoked.
    Revoked,
    /// The key is intact; its project is archived.
    ProjectArchived,
}

impl From<KeyRefusal> for AuthError {
    fn from(refusal: KeyRefusal) -> Self {
        match refusal {
            KeyRefusal::Revoked => AuthError::RevokedKey,
            KeyRefusal::ProjectArchived => AuthError::ProjectArchived,
        }
    }
}

/// Whether a header value carries one of *roundhouse's own* secrets rather
/// than somebody else's credential.
///
/// **The second half of the forwarding gate, and the half that cannot be
/// inferred from a header name.** Where a turn key arrived says which stanza a
/// client is speaking; it does not say what the client put in `Authorization`,
/// and the documented BYOK stanza puts the same `rh_turn_…` value in both
/// places. A capture gated on the header alone therefore forwards roundhouse's
/// own turn key to a frontier provider on the happy path — and an
/// `rh_admin_…` beside a turn key would go the same way. Neither is a
/// credential any upstream has any business seeing.
///
/// Every whitespace-separated token is checked rather than just the value after
/// a `Bearer ` strip, so the scheme a client chose — `Bearer`, `bearer`, none
/// at all — cannot decide whether the key leaves the process. What that costs
/// is a third-party credential containing a token shaped exactly like one of
/// ours, which is refused rather than forwarded; that direction degrades the
/// turn to local with a marker, which is the direction this module already errs
/// in (see [`header_value`]).
fn carries_a_roundhouse_secret(value: &str) -> bool {
    value.split_whitespace().any(has_valid_key_shape)
}

/// A turn key as the request presented it.
///
/// The pair travels together because the second half is only meaningful beside
/// the first: "the key came in its own header" is what licenses treating
/// `Authorization` as somebody else's credential, and a caller that had one
/// without the other could forward roundhouse's own key upstream. See
/// [`ControlPlane::turn_admission`].
struct PresentedKey<'a> {
    /// The bare secret, with any scheme prefix already removed.
    secret: &'a str,
    /// Whether it arrived in [`TURN_KEY_HEADER`] rather than `Authorization`.
    dedicated_header: bool,
}

/// One header's value as UTF-8, or nothing.
///
/// Lossy in exactly one direction and deliberately so: a header this cannot
/// read is treated as absent, and for the forwarded-credential capture that is
/// the fail-closed answer — a credential roundhouse cannot render is one it
/// cannot forward, so the provider goes unreachable and the turn degrades with
/// a marker rather than reaching an upstream half-authenticated.
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
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
        /// `sha256(secret)` hex, for keys this deployment recognizes and
        /// refuses: the admin plane's tombstones, compiled in beside the live
        /// tables.
        ///
        /// **Here rather than in a check the admin surface runs**, because a
        /// revoked key is presented to the *turn* surfaces, which is the whole
        /// point of revoking it. A tombstone held anywhere but inside the one
        /// auth seam would leave the key working everywhere the admin plane
        /// does not look — which is everywhere that matters.
        ///
        /// Empty on a deployment that has never revoked anything, and empty on
        /// every deployment that predates the admin plane, so the lookup below
        /// is a miss on an empty map and the resolution path is what it was.
        ///
        /// A hash can never be in both this and one of the tables above: the
        /// compiler refuses a duplicate hash across `keys` and `admin_keys`,
        /// and a tombstoned key is left out of the config it compiles from.
        refusals: HashMap<String, KeyRefusal>,
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
        Self::configured_with_refusals(config, HashMap::new())
    }

    /// The same runtime resolver, plus the hashes it must refuse *by name*.
    ///
    /// The entry point the admin plane compiles through — see
    /// [`ControlDirectory`](directory::ControlDirectory). Separate from
    /// [`Self::configured`] rather than an extra argument on it, because a
    /// deployment with no admin plane has no tombstones and should not have to
    /// pass an empty map to say so; and separate rather than a
    /// `with_refusals(self)` builder, because a builder would compile in
    /// [`Self::Open`] too, where it could only be a no-op that reads like a
    /// guarantee.
    pub fn configured_with_refusals(
        config: ControlPlaneConfig,
        refusals: HashMap<String, KeyRefusal>,
    ) -> Self {
        let ControlPlaneConfig {
            projects: _,
            users: _,
            keys: _,
            admin_keys,
            mcp_namespace,
            arm_salt,
            // Named and ignored: `validate` has already resolved these into
            // every `Admission` in `turn_keys`, secrets and all. Carrying the
            // raw block forward would be a second copy of the same keys, live
            // for the life of the process and reachable by anything that can
            // see the plane.
            credentials: _,
            // Named and ignored for a different reason: this is a *clock*
            // setting, and a compiled plane has no clock. How long one of these
            // may be served before the directory recompiles is the directory's
            // question, and it reads the field itself.
            admission_cache_ttl_ms: _,
            turn_keys,
        } = config;
        ControlPlane::Configured {
            turn_keys,
            admin_keys: admin_keys.into_iter().collect(),
            refusals,
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
        self.authenticate(self.presented_key(headers)?.map(|key| key.secret))
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
    ///
    /// **Where a forwarded credential enters the system**, and the only place.
    ///
    /// The capture is conditional on where the turn key came from, which is the
    /// rule that keeps pass-through from leaking roundhouse's own key upstream.
    /// Under the BYOK stanza a client puts `rh_turn_…` in `Authorization`; if
    /// this captured that header regardless, a pass-through-configured project
    /// would forward roundhouse's own turn key to a frontier provider. So the
    /// caller's credential is taken **only** when the key arrived in
    /// [`TURN_KEY_HEADER`] — that is exactly the configuration in which
    /// `Authorization` is somebody else's, and the two stanzas are
    /// distinguishable by nothing else.
    ///
    /// **The header is a necessary condition and not a sufficient one**, and
    /// the difference is a real client's doing rather than a hypothetical's:
    /// PLAN §3's BYOK stanza sends the same `rh_turn_…` value in `env_key` *and*
    /// in `env_http_headers`, so "the key arrived in the dedicated header" is
    /// true of a request whose `Authorization` is also roundhouse's own key.
    /// The value is therefore checked as well as the header —
    /// [`carries_a_roundhouse_secret`] — and a capture that would forward one
    /// of this deployment's own secrets is refused. The turn is still admitted;
    /// what it loses is the hosted half of its pool, which degrades to local
    /// with a marker like any other unreachable provider.
    pub fn turn_admission(&self, headers: &HeaderMap) -> Result<Admission, AuthError> {
        let presented = self.presented_key(headers)?;
        let forwardable = presented.as_ref().is_some_and(|key| key.dedicated_header);
        match self.authenticate(presented.map(|key| key.secret))? {
            KeyScope::Turn(admission) => Ok(admission.with_forwarded(
                forwardable
                    .then(|| {
                        PresentedCredential::captured(|name| {
                            header_value(headers, name)
                                .filter(|value| !carries_a_roundhouse_secret(value))
                        })
                    })
                    .flatten(),
            )),
            KeyScope::Admin => Err(AuthError::WrongKeyKind),
        }
    }

    /// The turn key this request presented, and which header it arrived in.
    ///
    /// Two headers, one rule, one function — so the ASCII check, the precedence
    /// and the `Bearer ` handling are read out of one place rather than
    /// re-derived per surface.
    ///
    /// [`TURN_KEY_HEADER`] wins when both are present, and that precedence is
    /// the load-bearing half: it is the *specific* header, so a request
    /// carrying both is a pass-through request whose `Authorization` belongs to
    /// its own upstream. Reading `Authorization` first would authenticate
    /// against a ChatGPT bearer, fail the shape check, and refuse every
    /// pass-through turn with `MalformedKey`.
    ///
    /// The `Bearer ` prefix is required on `Authorization` and optional on
    /// [`TURN_KEY_HEADER`]: codex copies an environment variable's value into
    /// `env_http_headers` verbatim, so what arrives there is a bare
    /// `rh_turn_…`, while `Authorization` has a scheme by definition.
    fn presented_key<'a>(
        &self,
        headers: &'a HeaderMap,
    ) -> Result<Option<PresentedKey<'a>>, AuthError> {
        // An unconfigured deployment authenticates nothing, so there is no
        // header it can be *wrong* about — every request resolves to the one
        // built-in membership whatever it carried. Short-circuited here rather
        // than in the caller so the refusal table below is what
        // `ControlPlane::Configured` means and nothing else, and so open mode
        // also captures no forwarded credential: it has no project that could
        // ask for one.
        if matches!(self, ControlPlane::Open) {
            return Ok(None);
        }
        let as_str = |value: &'a axum::http::HeaderValue| {
            value.to_str().map_err(|_| AuthError::MalformedKey)
        };
        if let Some(value) = headers.get(TURN_KEY_HEADER) {
            let value = as_str(value)?.trim();
            return Ok(Some(PresentedKey {
                secret: value.strip_prefix("Bearer ").unwrap_or(value),
                dedicated_header: true,
            }));
        }
        // A header that is present but not ASCII is malformed rather than
        // missing. Reporting it as missing would tell a client to add a key it
        // already sent, which is the least actionable answer in the table.
        match headers.get(AUTHORIZATION) {
            None => Ok(None),
            Some(value) => Ok(Some(PresentedKey {
                secret: as_str(value)?
                    .strip_prefix("Bearer ")
                    .ok_or(AuthError::MalformedKey)?,
                dedicated_header: false,
            })),
        }
    }

    /// Resolve a presented secret to what it authenticates: a membership and
    /// its resolved policy, or the admin scope.
    ///
    /// The error table (decision 3) is: no key in either header -> `MissingKey`;
    /// present but not `rh_(turn|admin)_<43 chars>` -> `MalformedKey`;
    /// well-shaped but no record of its hash -> `UnknownKey`. `WrongKeyKind`
    /// is not decided here — see [`Self::turn_admission`] — because this
    /// function has no notion of which surface is asking.
    ///
    /// Private, and takes the *bare* secret rather than a header value or the
    /// map: which header a key arrived in, and what scheme prefix it wore, is
    /// [`Self::presented_key`]'s question and is answered once there. Keeping
    /// the pure core separate is what lets identity resolution be exercised as
    /// a function of a string.
    fn authenticate(&self, presented: Option<&str>) -> Result<KeyScope, AuthError> {
        match self {
            ControlPlane::Open => Ok(KeyScope::Turn(Admission::open())),
            ControlPlane::Configured {
                turn_keys,
                admin_keys,
                // `refusals` is the field that stopped this line compiling, and
                // it is read below rather than ignored here: a tombstone *is*
                // an identity answer — "this deployment knows this secret and
                // will not honour it" — which is precisely the kind of field
                // the comment this replaces was written to catch.
                refusals,
                // Named and ignored rather than swept under a `..`: this arm
                // decides who a key is; the dialect decides how a call is
                // spelled and the salt decides which arm a session lands in,
                // and neither bears on identity. A field added here that
                // authentication does have to read should make this line stop
                // compiling.
                dialect: _,
                arm_salt: _,
            } => {
                let secret = presented.ok_or(AuthError::MissingKey)?;
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
                // Before `UnknownKey`, and that ordering is the row's whole
                // value: a revoked key that fell through to `unknown_key` would
                // be indistinguishable in a log from a typo, at exactly the
                // moment somebody is trying to work out whether a leaked secret
                // is still being tried. The two tables cannot both hold one
                // hash — see the field.
                if let Some(refusal) = refusals.get(&hash) {
                    return Err((*refusal).into());
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
    /// Which providers this membership can authenticate to, and with whose key.
    ///
    /// Resolved here, at admission, for the reason the three above are — and
    /// for one more that is specific to it: the candidate filter it drives has
    /// to run **before** `choose()`, so the answer must already exist when the
    /// engine starts pricing. A credential resolved in the connect branch is
    /// resolved after the [`DecisionRecord`] it belongs on has been written and
    /// after `considered` has priced a saving against a model this principal
    /// could not have reached.
    ///
    /// [`TurnCredentials::unrestricted`] on an open deployment and on any
    /// project that declares no credentials, which is what keeps a pre-M7
    /// workload routing exactly as it did: every quoted provider stays in the
    /// candidate set, and the transport authenticates itself.
    ///
    /// [`DecisionRecord`]: roundhouse_core::routing::DecisionRecord
    pub credentials: TurnCredentials,
    /// Whether a member's own credential draws this project's budget.
    ///
    /// Beside `credentials` rather than inside `budget`, because it is read off
    /// the project's `"credentials"` block and answers a credential question:
    /// *does BYOK spend the ceiling*. It is meaningful only where there is a
    /// budget, and harmless where there is not — a membership with no budget
    /// never reaches the ledger at all.
    pub budget_counts: BudgetCounts,
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
            // **Unrestricted, and not a default.** Any other value withholds
            // every frontier candidate from a deployment that has configured no
            // credentials at all, which would silently re-route every pre-M7
            // workload to local capacity it may not even have. The permissive
            // value in a security-shaped field is a sentence a reader can find,
            // which is what `TurnCredentials::unrestricted` is written out for.
            credentials: TurnCredentials::unrestricted(),
            budget_counts: BudgetCounts::default(),
        }
    }

    /// The same admission, told what *this request* carried.
    ///
    /// A no-op except under pass-through — see
    /// [`TurnCredentials::with_forwarded`] — and a method rather than a struct
    /// literal for the reason [`Self::with_policy`] is one: the other five
    /// fields are copied through, and a caller assembling this by hand is a
    /// caller who could quietly move one of them.
    pub fn with_forwarded(self, presented: Option<PresentedCredential>) -> Self {
        Self {
            credentials: self.credentials.with_forwarded(presented),
            ..self
        }
    }

    /// The same admission under a narrowed policy.
    ///
    /// **The one shape a per-turn narrowing may take**, and the reason it is a
    /// method rather than a struct literal at each site: the other three fields
    /// are copied through, and a caller assembling this by hand is a caller who
    /// could quietly move one of them. A budget is not an axis a narrowing may
    /// touch — an agent editing its own project's ceiling is the failure, and
    /// widening it needs no argument — and which experiment arm a session is in
    /// was decided when the session was created, so no per-turn narrowing may
    /// move it either.
    ///
    /// Takes an owned [`TurnPolicy`] because every caller has just computed
    /// one; the `Arc` is minted here so the narrowed policy is shared by the
    /// turn rather than cloned again at each read.
    pub fn with_policy(&self, policy: TurnPolicy) -> Self {
        Self {
            principal: self.principal.clone(),
            policy: Arc::new(policy),
            budget: self.budget.clone(),
            validation: self.validation.clone(),
            // Not an axis a narrowing may touch either: which key a turn
            // authenticates with is not something an agent's own overlay — or
            // the judge's escalation — may move, and widening it would let a
            // turn reach a provider its project cannot pay for.
            credentials: self.credentials.clone(),
            budget_counts: self.budget_counts,
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

    /// Resolve an `Authorization` value through the whole header seam.
    ///
    /// Through `scope` rather than `authenticate` since M7 split the two: what
    /// a client sends is now a question with two possible headers and an
    /// optional scheme prefix, and a helper that skipped that half would assert
    /// resolution against a string no request produces.
    fn resolved(plane: &ControlPlane, header: Option<&str>) -> Result<Resolved, AuthError> {
        let mut headers = HeaderMap::new();
        if let Some(header) = header {
            headers.insert(AUTHORIZATION, header.parse().expect("a valid header value"));
        }
        plane.scope(&headers).map(|scope| match scope {
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
            // The admin prefix shares `has_valid_key_shape`'s length/charset
            // check with the turn prefix above rather than a branch of its
            // own, so this looked exercised by proxy -- until the M8 refute
            // found a mutation that special-cased `rh_admin_` ahead of the
            // shared check and nothing here caught it. Malformed admin secrets
            // are vanishingly unlikely to matter in practice (the SHA256 of a
            // guessed string still has to collide with a minted key's hash),
            // but the two prefixes sharing one function is exactly the reason
            // a divergence between them is worth asserting on directly.
            "Bearer rh_admin_tooshort",
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
    fn has_valid_key_shape_treats_the_turn_and_admin_prefixes_alike() {
        // Direct on the function itself, rather than through `resolved`'s
        // header parsing: the two prefixes fall through to one shared
        // length/charset check (see the function), and this is the test that
        // would catch either arm growing a special case the other lacks.
        for tag in ["rh_turn_", "rh_admin_"] {
            assert!(
                !has_valid_key_shape(&format!("{tag}tooshort")),
                "{tag}: shorter than a real tail must be refused"
            );
            assert!(
                !has_valid_key_shape(&format!("{tag}{}", "!".repeat(KEY_TAIL_LEN))),
                "{tag}: the right length in a charset no minted key ever uses"
            );
            assert!(
                !has_valid_key_shape(&format!("{tag}{}", "a".repeat(KEY_TAIL_LEN + 1))),
                "{tag}: one digit too many"
            );
        }
        assert!(
            !has_valid_key_shape("rh_something_else_entirely"),
            "neither prefix"
        );
        assert!(!has_valid_key_shape(""), "empty");
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

    /// A one-project plane whose turns forward the caller's own credential.
    ///
    /// Pass-through is the only mode in which a capture is read at all — a
    /// stored resolution drops it on the floor — so it is the only fixture that
    /// can observe what the edge decided to forward.
    fn pass_through_plane() -> ControlPlane {
        let json = format!(
            r#"{{
              "projects": [{{ "id": "acme", "credentials": {{ "mode": "pass_through" }} }}],
              "users": [{{ "id": "ada" }}],
              "keys": [{{ "project": "acme", "user": "ada", "key_sha256": "{TURN_HASH}" }}],
              "admin_keys": ["{ADMIN_HASH}"]
            }}"#
        );
        ControlPlane::configured(
            ControlPlaneConfig::from_json(&json, "pass-through capture fixture")
                .expect("the fixture config must validate"),
        )
    }

    /// The turn key in its own header, and whatever the client put in
    /// `Authorization` beside it — the shape both documented stanzas produce.
    fn both_headers(dedicated: &str, authorization: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            TURN_KEY_HEADER,
            dedicated.parse().expect("a valid header value"),
        );
        headers.insert(
            AUTHORIZATION,
            authorization.parse().expect("a valid header value"),
        );
        headers
    }

    /// What this request would forward to `openai` as its `Authorization`, if
    /// anything.
    ///
    /// Read through `access` rather than off a field, because that is the seam
    /// a provider client reads: a value this returns is a value that would go
    /// on an upstream request.
    fn forwarded_authorization(plane: &ControlPlane, headers: &HeaderMap) -> Option<String> {
        let admission = plane.turn_admission(headers).expect("a known turn key");
        let access = admission.credentials.access("openai")?;
        let forwarded = access.credential.forwarded()?;
        forwarded
            .headers()
            .find(|(name, _)| *name == "authorization")
            .map(|(_, value)| value.to_string())
    }

    #[test]
    fn roundhouses_own_secret_is_never_the_credential_that_gets_forwarded() {
        let plane = pass_through_plane();

        // PROBE: the pair of headers the *documented* BYOK stanza sends. PLAN
        // §3 puts the same `rh_turn_…` value in `env_key` and in
        // `env_http_headers`, so a capture gated only on "the turn key arrived
        // in the dedicated header" takes roundhouse's own turn key off
        // `Authorization` and forwards it to a frontier provider — on the happy
        // path, on every turn.
        for authorization in [
            format!("Bearer {TURN_SECRET}"),
            // Bare, because codex copies an environment variable's value into a
            // header verbatim and a client may do the same into `Authorization`.
            TURN_SECRET.to_string(),
            // Lowercase scheme: a client's spelling of `Bearer` must not decide
            // whether roundhouse's key leaves the process.
            format!("bearer {TURN_SECRET}"),
            // And the sharpest one: the deployment's *admin* key, which spends
            // nothing and administers everything.
            format!("Bearer {ADMIN_SECRET}"),
        ] {
            assert_eq!(
                forwarded_authorization(&plane, &both_headers(TURN_SECRET, &authorization)),
                None,
                "`{authorization}` is roundhouse's own secret and must never reach an upstream"
            );
        }

        // And the turn is still served as the membership the key names: the
        // refusal is about what is forwarded, not about who is admitted. What
        // the caller loses is the hosted half of its pool, which degrades to
        // local with a marker — the same shape as a member who attached no key.
        let admission = plane
            .turn_admission(&both_headers(
                TURN_SECRET,
                &format!("Bearer {ADMIN_SECRET}"),
            ))
            .expect("the dedicated header still authenticates the turn key");
        assert_eq!(admission.principal, Principal::new("acme", "ada"));
        assert!(!admission.credentials.reaches("openai"));

        // CONTROL, and it is what keeps the rule above from being "forward
        // nothing": a genuine third-party bearer beside the dedicated header is
        // exactly the pass-through stanza, and it still forwards.
        let seat = "Bearer eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJhZGEifQ.a-real-seat-token";
        assert_eq!(
            forwarded_authorization(&plane, &both_headers(TURN_SECRET, seat)),
            Some(seat.to_string()),
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
