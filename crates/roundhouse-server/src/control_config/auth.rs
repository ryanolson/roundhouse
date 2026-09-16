// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Decision 3's refusal table, and how it reaches a client.
//!
//! The narrowest of the three files [`control_config`](super) splits into, and
//! deliberately so: this holds the *vocabulary* of a refusal — every row, its
//! machine-readable code, its status, and the body shape — and none
//! of the logic that decides which row applies. That decision is
//! [`ControlPlane::scope`](super::ControlPlane::scope) and its two callers in
//! [`mod.rs`](super), which is where a reader looking for "why was I refused"
//! goes; this is where a reader looking for "what does the refusal look like on
//! the wire" goes. Keeping them apart is what lets the table be read as a table.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Why a request never reached a session: decision 3's error table, whole.
///
/// Every row, including the ones no single function decides on its own —
/// `WrongKeyKind` comes from [`ControlPlane::turn_admission`] (and its
/// projection [`ControlPlane::turn_principal`]), which know what the surface
/// wanted, `OutOfNamespace` from the surface that was handed a session id, and
/// `AdminRequiresControlPlane` from the admin router, which refuses on the
/// deployment's *mode* before any header is read. They live here anyway,
/// because the table is the contract: a refusal spelled somewhere else is a
/// refusal a client's error handling has never heard of.
///
/// The admin plane added three rows rather than reusing `UnknownKey` for its
/// two tombstones, and that is the point of the tombstones — see
/// [`Self::RevokedKey`]. A parallel table beside this one would have been the
/// other way to spell them, and it is the way that leaves a client parsing two
/// vocabularies for the same question.
///
/// `Clone` but not `Copy`: `OutOfNamespace` carries the prefix that would have
/// worked, which is the only actionable part of that answer.
///
/// [`ControlPlane::turn_admission`]: super::ControlPlane::turn_admission
/// [`ControlPlane::turn_principal`]: super::ControlPlane::turn_principal
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// No roundhouse key was presented at all.
    ///
    /// The message names the *dedicated* header and the mechanism that fills
    /// it, rather than saying "missing Authorization header", because of F07:
    /// the commonest way to land on this row is not a client that forgot a key
    /// but codex silently dropping the `env_http_headers` entry that carries
    /// it — its `build_header_map` omits the header without an error when the
    /// named variable is unset, blank, or holds a value `HeaderValue` rejects
    /// (a trailing newline from `$(cat key)`), unlike the loud `EnvVar` error
    /// its `env_key` sibling raises for the identical case. An operator told
    /// "missing Authorization header" in pass-through mode goes and inspects
    /// an `Authorization` that is present and correct — it is their upstream
    /// seat token — and never looks at the variable that actually broke.
    #[error(
        "no roundhouse key: send it in the `x-roundhouse-key` header — codex fills that \
         header from `env_http_headers`, which it drops without an error when the named \
         variable is unset, blank, or holds a value a header cannot carry — or as \
         `Authorization: Bearer rh_turn_<43 base62 chars>`"
    )]
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
    /// A key the admin plane minted and then tombstoned.
    ///
    /// **Distinct from `UnknownKey`, and the distinction is the whole point of
    /// keeping a revoked key's hash instead of deleting the row.** An operator
    /// who revoked a leaked key wants to know that the thief is still trying it;
    /// an operator whose deploy script pasted the wrong secret wants to know
    /// that nothing here has ever heard of it. One code for both answers makes
    /// theft and typo indistinguishable in a log, which is exactly the moment
    /// somebody needs to tell them apart.
    ///
    /// Disclosing that the key *did* exist costs nothing: a secret is 32 CSPRNG
    /// bytes, so a caller holding one holds it because it was issued to them or
    /// leaked to them, never because they guessed. 401 rather than 403 because
    /// the credential is no longer a credential — re-presenting it will never
    /// work, and a client's retry logic should treat it as it treats an unknown
    /// key.
    #[error("this key has been revoked")]
    RevokedKey,
    /// A live key whose project is archived.
    ///
    /// 403 rather than 401: the key is intact and would authenticate, and it is
    /// the *project* that has been closed. Told apart from
    /// [`Self::RevokedKey`] because the remedies are opposite — un-archive the
    /// project, or move the member to a live one — and because a fleet of keys
    /// going dark at once reads very differently from one key going dark.
    ///
    /// Archived and not deleted, so this row exists at all: a project's spend
    /// history outlives the project, and a deployment that dropped the row would
    /// answer `unknown_key` for a membership its own ledger still has numbers
    /// for.
    #[error("this key's project has been archived")]
    ProjectArchived,
    /// An admin route on a deployment that configured no control plane.
    ///
    /// **The bootstrap root of trust is the file and there is no second one.**
    /// In [`ControlPlane::Open`](super::ControlPlane::Open) every request
    /// resolves to the built-in membership with no key at all, so an admin
    /// surface that served open mode would be an unauthenticated writer of the
    /// deployment's own tenancy — reachable by anything that can reach the port.
    /// Refused mode-first, before any header is read, which is why this row
    /// carries no key vocabulary: there is no key it could be about.
    ///
    /// 403 rather than 404: the route exists and the deployment is misconfigured
    /// for it, and an operator who gets "not found" goes looking for a typo in a
    /// path that was right.
    #[error(
        "the admin plane is only served on a deployment with a control plane; set \
         ROUNDHOUSE_CONTROL_PLANE, which is the only root of trust an admin key can be \
         issued from"
    )]
    AdminRequiresControlPlane,
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
            AuthError::RevokedKey => "revoked_key",
            AuthError::ProjectArchived => "project_archived",
            AuthError::AdminRequiresControlPlane => "admin_requires_control_plane",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            AuthError::MissingKey
            | AuthError::MalformedKey
            | AuthError::UnknownKey
            | AuthError::RevokedKey => StatusCode::UNAUTHORIZED,
            AuthError::WrongKeyKind
            | AuthError::OutOfNamespace { .. }
            | AuthError::ProjectArchived
            | AuthError::AdminRequiresControlPlane => StatusCode::FORBIDDEN,
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

    #[test]
    fn every_row_carries_its_own_code_and_the_statuses_split_401_from_403() {
        // The table read as a table: a code repeated across two rows would make
        // two different refusals indistinguishable to a client's error
        // handling, which is the one thing this type exists to prevent.
        let rows = [
            (
                AuthError::MissingKey,
                "missing_key",
                StatusCode::UNAUTHORIZED,
            ),
            (
                AuthError::MalformedKey,
                "malformed_key",
                StatusCode::UNAUTHORIZED,
            ),
            (
                AuthError::UnknownKey,
                "unknown_key",
                StatusCode::UNAUTHORIZED,
            ),
            (
                AuthError::WrongKeyKind,
                "wrong_key_kind",
                StatusCode::FORBIDDEN,
            ),
            (
                AuthError::OutOfNamespace {
                    prefix: "acme/ada/".into(),
                },
                "session_out_of_namespace",
                StatusCode::FORBIDDEN,
            ),
            (
                AuthError::RevokedKey,
                "revoked_key",
                StatusCode::UNAUTHORIZED,
            ),
            (
                AuthError::ProjectArchived,
                "project_archived",
                StatusCode::FORBIDDEN,
            ),
            (
                AuthError::AdminRequiresControlPlane,
                "admin_requires_control_plane",
                StatusCode::FORBIDDEN,
            ),
        ];
        let mut codes: Vec<&str> = Vec::new();
        for (error, code, status) in &rows {
            assert_eq!(&error.code(), code, "{error:?}");
            assert_eq!(&error.status(), status, "{error:?}");
            assert!(!codes.contains(code), "`{code}` is claimed by two rows");
            codes.push(code);
        }
    }

    #[test]
    fn the_out_of_namespace_message_names_the_prefix_that_would_have_worked() {
        // The only actionable part of that answer, and the reason the row
        // carries data at all.
        let error = AuthError::OutOfNamespace {
            prefix: "acme/ada/".into(),
        };
        assert!(error.to_string().contains("acme/ada/"), "{error}",);
    }
}
