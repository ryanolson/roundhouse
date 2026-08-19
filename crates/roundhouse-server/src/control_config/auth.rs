// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Decision 3's refusal table, and how it reaches a client.
//!
//! The narrowest of the three files [`control_config`](super) splits into, and
//! deliberately so: this holds the *vocabulary* of a refusal — the five rows,
//! their machine-readable codes, their statuses, and the body shape — and none
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
///
/// [`ControlPlane::turn_admission`]: super::ControlPlane::turn_admission
/// [`ControlPlane::turn_principal`]: super::ControlPlane::turn_principal
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
