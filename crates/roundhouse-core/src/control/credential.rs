// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What an operator writes down when they attach a provider credential.
//!
//! Four files, four questions. This one is the configuration vocabulary: what
//! kinds of credential exist, how a file names one without carrying it, and
//! which of a project's and a member's keys wins. [`secret`] is what reaches
//! the transport. [`access`] is which candidates a principal can therefore
//! reach. [`forwarded`] is the one credential roundhouse holds and does not
//! own — the caller's own, under pass-through, allowlisted per provider and
//! kept for the length of one turn.
//!
//! Two refusals are the whole content of this module, and both are loud.
//!
//! **OAuth-shaped material is refused, by name.** The vision's "attach your
//! Codex or Claude account" half has no vendor approval, no gateway precedent,
//! and no client-side mechanism that could make a subscription spendable
//! server-side. So [`CredentialKind`] has exactly one variant and the
//! unsupported state is unrepresentable, and — because a one-variant enum
//! cannot refuse a *value* — [`CredentialError::check_api_key_shape`] refuses
//! the shapes an OAuth token actually arrives in, at the one constructor every
//! secret passes through. Refusing loudly is the point: a deferral nobody
//! announces becomes a token in a database.
//!
//! What is *not* refused is forwarding a credential inside the request the
//! client itself made. That is [`CredentialMode::PassThrough`], and it stores
//! nothing.
//!
//! **A secret never appears in a configuration file.** [`CredentialRef`] names
//! an environment variable, and its validation is what enforces the rule
//! structurally rather than by inspection: an environment variable name is
//! `[A-Za-z_][A-Za-z0-9_]*`, and every credential format in circulation carries
//! a character that alphabet does not have — `sk-…` and `at-…` a hyphen, a JWT
//! two dots, a sealed blob base64's `+/=`. A pasted key is therefore not merely
//! discouraged here, it does not parse.

pub mod access;
pub mod forwarded;
pub mod secret;

use std::fmt;

use serde::{Deserialize, Serialize};

pub use access::{ProviderAccess, Reachable, TurnCredentials};
pub use forwarded::{ForwardedCredential, PresentedCredential};
pub use secret::{Secret, TurnCredential};

/// The kinds of credential roundhouse will hold.
///
/// One variant, so "we do not support OAuth" is a fact about the type rather
/// than a branch somebody can forget to write. Adding a second is a decision
/// with a vendor conversation behind it, not a refactor.
///
/// It carries no runtime information — every [`Secret`] is an API key — and
/// that is the point: its job is at the parse boundary, where a request or a
/// file naming some other kind has nowhere to deserialize into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    #[default]
    ApiKey,
}

impl CredentialKind {
    /// Parse a kind named by an admin request or a configuration file.
    ///
    /// Spelled out rather than left to serde because the two produce different
    /// answers to the same mistake: serde says `unknown variant "oauth"`, which
    /// tells an operator their JSON is wrong, and this says *why* roundhouse
    /// will not take it. The second is the one a person can act on, and it is
    /// the one §3 promises (`400 oauth_credentials_unsupported`, with a message
    /// naming the reason).
    pub fn parse(kind: &str) -> Result<Self, CredentialError> {
        match kind.trim().to_ascii_lowercase().as_str() {
            "api_key" | "apikey" | "api-key" => Ok(CredentialKind::ApiKey),
            // The spellings a client would reach for when they mean "let me
            // attach my subscription". Named individually so the refusal is
            // about what they asked for, not about a typo.
            other @ ("oauth" | "oauth2" | "chatgpt" | "claude" | "device_login"
            | "device-login" | "subscription" | "refresh_token" | "access_token") => {
                Err(CredentialError::OauthUnsupported {
                    evidence: OauthEvidence::KindNamed(other.to_string()),
                })
            }
            other => Err(CredentialError::UnknownKind {
                kind: other.to_string(),
            }),
        }
    }
}

/// Where a secret lives, as a configuration file may say it.
///
/// One variant today, and the second one is named rather than stubbed. §3's
/// sealed store — XChaCha20-Poly1305 under a key from `ROUNDHOUSE_CONTROL_KEY`
/// — needs three things that do not exist yet: a control *store* to hold
/// ciphertext (M8's admin plane writes it), the key material, and a decrypt
/// seam. An arm nothing can construct and nothing can open is not a smaller
/// version of that; it is a claim the type makes and the code does not keep, so
/// the config-file phase ships alone and the sealed arm arrives with the store
/// that produces it. Adding it is additive — every match on this enum is inside
/// this crate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialRef {
    EnvVar { name: String },
}

/// The longest environment variable name this will accept.
///
/// Not a platform limit — it is a shape check. Real names are short; a long
/// one is a pasted secret that happened to survive the alphabet, and the two
/// checks together are what make "no secrets in the file" structural.
const MAX_ENV_VAR_NAME: usize = 128;

impl CredentialRef {
    /// Name the environment variable a secret lives in.
    ///
    /// Validated here rather than at first use, so a file that inlines a key —
    /// or names a variable that cannot exist — stops the process at boot
    /// instead of failing one turn at a time on a deployment that looked
    /// healthy.
    pub fn env_var(name: impl Into<String>) -> Result<Self, CredentialError> {
        let name: String = name.into();
        let refuse = |reason| {
            Err(CredentialError::NotAnEnvVarName {
                name: name.clone(),
                reason,
            })
        };
        if name.is_empty() {
            return refuse("it is empty");
        }
        if name.len() > MAX_ENV_VAR_NAME {
            return refuse(
                "it is longer than any real variable name, which is what a pasted secret looks like",
            );
        }
        let mut chars = name.chars();
        let first = chars.next().unwrap_or('0');
        if !(first.is_ascii_alphabetic() || first == '_') {
            return refuse("a variable name starts with a letter or an underscore");
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return refuse(
                "a variable name is letters, digits and underscores only -- every credential \
                 format in circulation carries a character this alphabet does not have, which \
                 is what stops a key being pasted where its variable's name belongs",
            );
        }
        Ok(CredentialRef::EnvVar { name })
    }

    pub fn env_var_name(&self) -> &str {
        match self {
            CredentialRef::EnvVar { name } => name,
        }
    }
}

/// Whose credential a project's turns are paid with.
///
/// Resolution order is configured rather than implicit, because the implicit
/// answer differs by deployment: a company handing out one corporate key and a
/// company where each engineer brings their own want opposite defaults, and
/// guessing costs somebody a bill they did not expect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialMode {
    /// The project's own credential, never a member's.
    ProjectOnly,
    /// A member's own credential where they have one, the project's otherwise.
    ///
    /// The default because it is the only order that is right for both kinds of
    /// member: somebody who attached a key meant to use it, and somebody who
    /// did not still gets served.
    #[default]
    PreferUser,
    /// A member's own credential or nothing.
    ///
    /// A member with none loses that provider's models from their candidate set
    /// and the turn degrades to local — the same mechanism as budget
    /// exhaustion, a served turn plus a marker rather than a 500. Deliberately
    /// no fall back to the deployment's own key: the whole content of this mode
    /// is "this project's frontier spend is paid by its members", and a
    /// deployment-tier fallback would quietly make the deployment pay for
    /// exactly the members the mode exists to charge.
    UserOnly,
    /// Forward the credential the client's own request carried.
    ///
    /// No stored credential resolves under this mode and none may be
    /// configured alongside it — see
    /// [`TurnCredentials::stored`](access::TurnCredentials::stored). Switchyard
    /// states the same rule in configuration (`forward_auth` is rejected
    /// together with `api_key_env`,
    /// `crates/switchyard-server/src/config.rs:873-877` @ `5341f71`), and codex
    /// enforces it natively on the other side of the same wire: `env_key`
    /// resolves before any first-party auth, so a route with both silently
    /// forwards nothing.
    PassThrough,
}

/// Why a credential was refused.
///
/// The evidence, not a guess about intent. An operator told "oauth credentials
/// are unsupported" and nothing else goes looking for a setting; one told
/// "the value begins `eyJ`, which is how every JWT begins" knows which field
/// they pasted into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OauthEvidence {
    /// The request or file named an OAuth-shaped kind outright.
    KindNamed(String),
    /// `eyJ` is `{"` base64url-encoded, so every JWT — every id token, and
    /// every access token that is one — begins with it.
    JwtPrefix,
    /// The prefix a first-party OAuth access token carries.
    AccessTokenPrefix,
    /// A token *document*: the JSON an OAuth exchange returns, pasted whole.
    TokenDocument,
}

impl fmt::Display for OauthEvidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OauthEvidence::KindNamed(kind) => {
                write!(f, "the credential kind was given as `{kind}`")
            }
            OauthEvidence::JwtPrefix => {
                f.write_str("the value begins `eyJ`, which is how every JSON Web Token begins")
            }
            OauthEvidence::AccessTokenPrefix => {
                f.write_str("the value begins `at-`, an OAuth access-token prefix")
            }
            OauthEvidence::TokenDocument => f.write_str(
                "the value is a token document carrying `refresh_token`, `access_token` or \
                 `id_token`",
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    #[error("a credential may not be empty")]
    Empty,
    /// §3's refusal, whole.
    ///
    /// The message carries the alternative because a refusal with no way
    /// forward is how a deployment ends up with a workaround nobody reviewed.
    #[error(
        "oauth credentials are unsupported ({evidence}). Roundhouse never stores or re-presents \
         an OAuth token: attach a provider API key, or set the project's credential mode to \
         pass-through, which forwards the client's own credential in-flight and stores nothing"
    )]
    OauthUnsupported { evidence: OauthEvidence },
    #[error("credential kind `{kind}` is not one roundhouse holds; the only kind is `api_key`")]
    UnknownKind { kind: String },
    #[error(
        "`{name}` is not an environment variable name ({reason}); the control-plane file names \
         the variable a secret lives in and never carries the secret itself"
    )]
    NotAnEnvVarName { name: String, reason: &'static str },
    /// Forwarding and a stored key are mutually exclusive.
    #[error(
        "credential mode `pass_through` forwards the client's own credential and may not be \
         configured with a stored one (`{provider}`): whichever resolves first wins silently, so \
         the pair is refused rather than ordered"
    )]
    PassThroughWithStoredCredential { provider: String },
    #[error(
        "no credential resolved for provider `{provider}`; refusing to send an unauthenticated \
         request"
    )]
    NoCredential { provider: String },
    #[error(
        "provider `{provider}` is on the pass-through route, where the client's credential is \
         forwarded at the header layer; there is no stored key to reveal"
    )]
    ForwardedNotStored { provider: String },
}

impl CredentialError {
    /// The stable machine-readable code, in the convention
    /// [`AuthError::code`](../../../roundhouse_server/control_config/enum.AuthError.html)
    /// established: one code per row, and no row sharing another's.
    pub fn code(&self) -> &'static str {
        match self {
            CredentialError::Empty => "credential_empty",
            CredentialError::OauthUnsupported { .. } => "oauth_credentials_unsupported",
            CredentialError::UnknownKind { .. } => "unknown_credential_kind",
            CredentialError::NotAnEnvVarName { .. } => "credential_must_name_an_env_var",
            CredentialError::PassThroughWithStoredCredential { .. } => {
                "pass_through_with_stored_credential"
            }
            CredentialError::NoCredential { .. } => "no_credential_for_provider",
            CredentialError::ForwardedNotStored { .. } => "forwarded_credential_not_stored",
        }
    }

    /// Refuse a value that is shaped like an OAuth token rather than an API key.
    ///
    /// **Public because every boundary needs it and a second copy is how one
    /// rule stops being one rule** — the same argument
    /// [`SpendError::check_amount`](crate::control::SpendError::check_amount)
    /// is written out for. The boundaries are the admin request body, the env
    /// var a [`CredentialRef`] names, and any future import; all three reach
    /// [`Secret::api_key`], which is this.
    ///
    /// **Shape, not intent, and it errs toward refusing.** `at-` could in
    /// principle prefix somebody's legitimate API key. That direction is the
    /// safe one: a false refusal is a loud message an operator reports, and a
    /// false accept is a stored OAuth token — the one outcome §3 forbids
    /// outright. The three shapes are what the ecosystem actually emits: codex
    /// writes `access_token` / `refresh_token` / `id_token` into its auth file
    /// on device login, and the access token it then forwards is a JWT.
    pub fn check_api_key_shape(value: &str) -> Result<(), CredentialError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(CredentialError::Empty);
        }
        // Every prefix test below compares *bytes*, never `&str[..n]`. This
        // function's whole input is hostile — an admin request body — and
        // slicing a `&str` at a fixed index panics when a multi-byte character
        // straddles it, which would turn a malformed credential into a downed
        // admin plane. Byte comparison has no such index, and a match on an
        // ASCII prefix guarantees the offset that follows *is* a boundary.
        let prefixed = |value: &str, prefix: &[u8]| {
            value
                .as_bytes()
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        };
        // A pasted `Authorization` header value: judge what follows the scheme,
        // so `Bearer eyJ…` is refused for being a JWT rather than for having a
        // prefix, which is the reason an operator can act on.
        let bare = match prefixed(trimmed, b"bearer ") {
            true => trimmed[7..].trim_start(),
            false => trimmed,
        };
        let evidence = if bare.starts_with("eyJ") {
            Some(OauthEvidence::JwtPrefix)
        } else if bare.len() > 3 && prefixed(bare, b"at-") {
            Some(OauthEvidence::AccessTokenPrefix)
        } else if is_token_document(bare) {
            Some(OauthEvidence::TokenDocument)
        } else {
            None
        };
        match evidence {
            Some(evidence) => Err(CredentialError::OauthUnsupported { evidence }),
            None => Ok(()),
        }
    }
}

/// Whether `value` is the JSON an OAuth exchange returns, pasted whole.
///
/// Structural rather than a substring search: an API key is one opaque token,
/// so the only way it contains `"refresh_token"` is if it is not one. Requiring
/// the leading `{` as well keeps a key that merely happens to embed the letters
/// from being refused.
fn is_token_document(value: &str) -> bool {
    value.starts_with('{')
        && ["refresh_token", "access_token", "id_token"]
            .iter()
            .any(|field| value.contains(field))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oauth_shaped_credential_is_refused_with_a_reason() {
        // PROBE. Every shape an OAuth credential actually arrives in, each
        // refused with the evidence that identifies it -- and all under one
        // code, because a client's error handling branches on the code and a
        // refusal spelled two ways is a refusal it has never heard of.
        let refused: [(&str, OauthEvidence); 6] = [
            (
                "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0In0.sig",
                OauthEvidence::JwtPrefix,
            ),
            (
                "Bearer eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiIxIn0.sig",
                OauthEvidence::JwtPrefix,
            ),
            (
                "at-01JQZ9K2M3N4P5Q6R7S8T9V0W1",
                OauthEvidence::AccessTokenPrefix,
            ),
            (
                "AT-01JQZ9K2M3N4P5Q6R7S8T9V0W1",
                OauthEvidence::AccessTokenPrefix,
            ),
            (
                r#"{"access_token":"abc","refresh_token":"def","expires_in":3600}"#,
                OauthEvidence::TokenDocument,
            ),
            (
                r#"{"id_token":"abc","token_type":"Bearer"}"#,
                OauthEvidence::TokenDocument,
            ),
        ];
        for (value, expected) in refused {
            let error = Secret::api_key(value).expect_err("must be refused: {value}");
            assert_eq!(
                error,
                CredentialError::OauthUnsupported {
                    evidence: expected.clone()
                },
                "`{value}` must be refused as {expected}"
            );
            assert_eq!(error.code(), "oauth_credentials_unsupported");
            // The message has to name the reason and the way forward, or an
            // operator's next move is a workaround nobody reviewed.
            let message = error.to_string();
            assert!(message.contains("pass-through"), "{message}");
            assert!(message.contains(&expected.to_string()), "{message}");
        }

        // The kind axis, refused under the same code so a request that names
        // `"kind": "oauth"` and one that pastes a token read alike to a client.
        for kind in ["oauth", "OAuth2", "chatgpt", "device_login", "subscription"] {
            let error = CredentialKind::parse(kind).expect_err("must be refused: {kind}");
            assert_eq!(error.code(), "oauth_credentials_unsupported");
        }
        assert_eq!(
            CredentialKind::parse("totp").unwrap_err().code(),
            "unknown_credential_kind",
            "a kind nobody has heard of is a different answer from one we refuse on purpose"
        );

        // CONTROL. Ordinary provider keys, which must all parse -- otherwise
        // the assertions above are about strictness rather than about OAuth.
        for value in [
            "sk-ant-api03-AAAABBBBCCCCDDDDEEEEFFFF",
            "sk-proj-0123456789abcdefghijklmnop",
            "rh_turn_0123456789abcdefghijklmnopqrstuvwxyzABCDE",
            "gsk_0123456789abcdefghij",
            "0123456789abcdef0123456789abcdef",
            // Contains the letters, is not a document: one opaque token.
            "sk-live-refresh_token-lookalike",
        ] {
            assert!(
                Secret::api_key(value).is_ok(),
                "`{value}` is an ordinary API key and must parse"
            );
        }
        assert_eq!(
            CredentialKind::parse("api_key").unwrap(),
            CredentialKind::ApiKey
        );

        // Empty is its own answer: not an OAuth refusal, because telling an
        // operator who set a variable to "" that they pasted a token sends them
        // to the wrong file.
        assert_eq!(Secret::api_key("   ").unwrap_err(), CredentialError::Empty);
    }

    #[test]
    fn a_multibyte_value_is_judged_rather_than_panicking_on_a_char_boundary() {
        // The input to this check is an admin request body, so a value that
        // crashes the process is a denial of service on the admin plane rather
        // than a malformed credential. Every one of these puts a multi-byte
        // character across an index a `&str[..n]` prefix test would have sliced
        // at -- 7 for `bearer `, 3 for `at-`.
        for hostile in ["béarer x", "béa", "aé-", "ét", "日本語のキー", "aé"] {
            // The assertion is that this returns at all. What it returns is the
            // second question and either answer is defensible.
            let _ = CredentialError::check_api_key_shape(hostile);
            let _ = Secret::api_key(hostile);
        }

        // The control: the ASCII spellings the guard exists for still land on
        // the right evidence, so the fix is about the index and not about
        // having stopped looking.
        assert_eq!(
            Secret::api_key("Bearer eyJhbGciOiJIUzI1NiJ9.e30.s").unwrap_err(),
            CredentialError::OauthUnsupported {
                evidence: OauthEvidence::JwtPrefix
            }
        );
        assert_eq!(
            Secret::api_key("at-0123456789").unwrap_err(),
            CredentialError::OauthUnsupported {
                evidence: OauthEvidence::AccessTokenPrefix
            }
        );
    }

    #[test]
    fn a_credential_ref_names_a_variable_and_cannot_carry_a_key() {
        // PROBE: every credential format in circulation fails the alphabet, so
        // "no secrets in the config file" is structural rather than a review
        // convention.
        for pasted in [
            "sk-ant-api03-AAAABBBBCCCC",
            "at-01JQZ9K2M3N4",
            "eyJhbGciOiJIUzI1NiJ9.eyJhIjoxfQ.sig",
            "c2VhbGVkLWJsb2I=",
            "ROUNDHOUSE KEY",
            "",
            "9NOT_A_NAME",
        ] {
            let error = CredentialRef::env_var(pasted).expect_err("must be refused: {pasted}");
            assert_eq!(error.code(), "credential_must_name_an_env_var");
        }
        assert!(CredentialRef::env_var("A".repeat(MAX_ENV_VAR_NAME + 1)).is_err());

        // CONTROL: the names an operator actually writes.
        for name in [
            "ANTHROPIC_API_KEY",
            "_PRIVATE",
            "roundhouse_key_2",
            &"A".repeat(MAX_ENV_VAR_NAME),
        ] {
            let named = CredentialRef::env_var(name)
                .unwrap_or_else(|error| panic!("`{name}` is a variable name: {error}"));
            assert_eq!(named.env_var_name(), name);
        }
    }

    #[test]
    fn prefer_user_is_the_default_mode() {
        // Named rather than assumed: the default decides who is billed, and a
        // silent change of it moves money.
        assert_eq!(CredentialMode::default(), CredentialMode::PreferUser);
        assert_eq!(
            serde_json::to_string(&CredentialMode::PassThrough).unwrap(),
            r#""pass_through""#
        );
    }
}
