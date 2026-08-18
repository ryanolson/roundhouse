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

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use roundhouse_core::control::Principal;
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
        let config: Self =
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
    fn validate(&self, path: &str) -> Result<(), ControlPlaneError> {
        let mut project_ids: HashSet<&str> = HashSet::new();
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
        /// `sha256(secret)` hex, to the membership it authenticates.
        turn_keys: HashMap<String, Principal>,
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
        let turn_keys = config
            .keys
            .into_iter()
            .map(|key| (key.key_sha256, Principal::new(key.project, key.user)))
            .collect();
        let admin_keys = config.admin_keys.into_iter().collect();
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
    /// through here or through [`Self::turn_principal`] below, so the header
    /// name, the ASCII rule and the error table are read out of one function
    /// rather than re-derived per transport.
    ///
    /// A header that is present but not ASCII is malformed rather than missing.
    /// Reporting it as missing would tell a client to add a key it already
    /// sent, which is the least actionable of the answers in the table.
    pub fn scope(&self, headers: &HeaderMap) -> Result<KeyScope, AuthError> {
        let header = match headers.get(AUTHORIZATION) {
            None => None,
            Some(value) => Some(value.to_str().map_err(|_| AuthError::MalformedKey)?),
        };
        self.resolve(header)
    }

    /// The membership a turn-serving surface's caller spends as.
    ///
    /// `wrong_key_kind` is decided here rather than in [`Self::resolve`],
    /// because whether an admin key is the wrong key depends on what the route
    /// wanted. Every turn surface wants exactly this answer, so there is one
    /// place for them to disagree with instead of three.
    ///
    /// An admin key is refused rather than quietly given a principal of its
    /// own: an admin acts on the deployment and has no membership to bill, and
    /// the alternative — minting one — would put spend on a row no project
    /// owns.
    pub fn turn_principal(&self, headers: &HeaderMap) -> Result<Principal, AuthError> {
        match self.scope(headers)? {
            KeyScope::Turn(principal) => Ok(principal),
            KeyScope::Admin => Err(AuthError::WrongKeyKind),
        }
    }

    /// Resolve a presented `Authorization` header value to the scope it
    /// authenticates.
    ///
    /// The error table (decision 3) is: missing header -> `MissingKey`;
    /// present but not `Bearer rh_(turn|admin)_<43 chars>` -> `MalformedKey`;
    /// well-shaped but no record of its hash -> `UnknownKey`. `WrongKeyKind`
    /// is not decided here — see [`Self::turn_principal`] — because this
    /// function has no notion of which surface is asking.
    ///
    /// Private, and takes the header value rather than the map, so that it can
    /// be exercised as a pure function of a string by the tests below while
    /// [`Self::scope`] remains the one way in.
    ///
    /// In [`Self::Open`] every request resolves to
    /// [`Principal::default_open`] regardless of the header: an unconfigured
    /// deployment authenticates nothing, so there is no wrong answer a header
    /// could produce.
    fn resolve(&self, authorization_header: Option<&str>) -> Result<KeyScope, AuthError> {
        match self {
            ControlPlane::Open => Ok(KeyScope::Turn(Principal::default_open())),
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
                    return Ok(KeyScope::Admin);
                }
                if let Some(principal) = turn_keys.get(&hash) {
                    return Ok(KeyScope::Turn(principal.clone()));
                }
                Err(AuthError::UnknownKey)
            }
        }
    }
}

/// Why a request never reached a session: decision 3's error table, whole.
///
/// All five rows, including the two no single function decides on its own —
/// `WrongKeyKind` comes from [`ControlPlane::turn_principal`], which knows what
/// the surface wanted, and `OutOfNamespace` from the surface that was handed a
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
    }
}
