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
//! [`ControlPlane::resolve`] implements the plan's error table in full:
//! missing `Authorization` is `401 missing_key`, a header that is not
//! `Bearer rh_(turn|admin)_<43 base62 chars>` is `401 malformed_key`, a hash
//! with no record is `401 unknown_key`. The fourth row — an admin key
//! presented on a turn route, or vice versa — is `403 wrong_key_kind` and is
//! a property of the *route*, not of resolution: `resolve` returns whichever
//! [`KeyScope`] the hash actually maps to, and the surface that knows what it
//! requires is the one that can refuse a mismatch. That wiring is a later
//! milestone; this module only guarantees the scope it hands back is honest.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use roundhouse_core::control::{KeyScope, Principal};

/// Path to a control-plane JSON file. Absent means [`ControlPlane::Open`].
pub const CONTROL_PLANE_VAR: &str = "ROUNDHOUSE_CONTROL_PLANE";

// ---------------------------------------------------------------------------
// Config file format
// ---------------------------------------------------------------------------

/// One entry of the config's `"projects"` array.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectEntry {
    pub id: String,
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
    hash.len() == 64 && hash.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// `true` for `rh_(turn|admin)_` followed by 43 base62 characters — the shape
/// a *presented* secret must have before its hash is even looked up, so a
/// obviously-wrong header never reaches the hash table.
fn has_valid_key_shape(secret: &str) -> bool {
    let tail = secret
        .strip_prefix("rh_turn_")
        .or_else(|| secret.strip_prefix("rh_admin_"));
    match tail {
        Some(tail) => tail.len() == 43 && tail.chars().all(|c| c.is_ascii_alphanumeric()),
        None => false,
    }
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

    /// The prefix every session id belonging to `principal` carries in this
    /// deployment, or `None` where ids are exactly what the client named.
    ///
    /// The one place the mode decides the namespace. Two surfaces have to
    /// agree on it exactly — the responses surface mints ids with it, the
    /// native surface refuses ids without it — and a second spelling of the
    /// same convention is how a namespace stops being one.
    ///
    /// `None` in [`Self::Open`] is not "the empty prefix": an unconfigured
    /// deployment's sessions keep the ids they already have, and prepending
    /// `default/default/` to them would strand every session in the store the
    /// day the process restarted with this code.
    pub fn session_prefix(&self, principal: &Principal) -> Option<String> {
        match self {
            ControlPlane::Open => None,
            ControlPlane::Configured { .. } => Some(principal.namespace_prefix()),
        }
    }

    /// Resolve a presented `Authorization` header value to the scope it
    /// authenticates.
    ///
    /// The four-row error table (decision 3) is: missing header ->
    /// `MissingKey`; present but not `Bearer rh_(turn|admin)_<43 chars>` ->
    /// `MalformedKey`; well-shaped but no record of its hash -> `UnknownKey`.
    /// The fourth row, `WrongKeyKind`, is not decided here — see the module
    /// doc — because this function has no notion of which surface is asking.
    ///
    /// In [`Self::Open`] every request resolves to
    /// [`Principal::default_open`] regardless of the header: an unconfigured
    /// deployment authenticates nothing, so there is no wrong answer a header
    /// could produce.
    pub fn resolve(&self, authorization_header: Option<&str>) -> Result<KeyScope, AuthError> {
        match self {
            ControlPlane::Open => Ok(KeyScope::Turn(Principal::default_open())),
            ControlPlane::Configured {
                turn_keys,
                admin_keys,
            } => {
                let header = authorization_header.ok_or(AuthError::MissingKey)?;
                let secret = header.strip_prefix("Bearer ").ok_or(AuthError::MalformedKey)?;
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

/// Why a request never reached a session: the first three rows of decision
/// 3's error table. (The fourth, `WrongKeyKind`, is produced by the surface
/// that knows what scope it requires — see the module doc.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("missing Authorization header")]
    MissingKey,
    #[error("Authorization header is not `Bearer rh_(turn|admin)_<43 base62 chars>`")]
    MalformedKey,
    #[error("key not recognized")]
    UnknownKey,
    #[error("this key's scope may not be used on this surface")]
    WrongKeyKind,
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
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            AuthError::MissingKey | AuthError::MalformedKey | AuthError::UnknownKey => {
                StatusCode::UNAUTHORIZED
            }
            AuthError::WrongKeyKind => StatusCode::FORBIDDEN,
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
        let error =
            ControlPlaneConfig::load("/nonexistent/roundhouse-control-plane-test.json")
                .unwrap_err();
        assert!(
            matches!(error, ControlPlaneError::Read { .. }),
            "an unreadable file must be a Read error, not silently treated as Open: {error}"
        );
        assert!(error.to_string().contains("roundhouse-control-plane-test.json"));
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
