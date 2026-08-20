// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What reaches the transport: a handle that renders as a fingerprint.
//!
//! [`super`] is what an operator writes down; this is what a turn carries. The
//! two are separate files because they answer to different readers — a
//! configuration boundary reads the first, and every `Debug` line, every
//! serialized event and every provider client reads this one.
//!
//! **The invariant: there is exactly one way to get plaintext out, and it is
//! spelled `reveal`.** `Debug`, `Display` and `Serialize` all render a
//! fingerprint, so a secret that lands in a `tracing` field, a serialized event
//! or a `{:?}` of some enclosing struct lands as eight hex characters. That is
//! not defence in depth for its own sake: the engine holds one
//! `Arc<dyn FrontierClient>` and the credential rides the quote, so the quote
//! is `Debug`-formatted by anything that logs a dispatch, and a derived `Debug`
//! there would put a live provider key in the log of every turn.
//!
//! Worth stating because it is what makes the event-log half of that guarantee
//! cheap: **no event type carries a [`Secret`] at all.** A credential travels on
//! the quote, which is an argument to `execute` and never a log record — the
//! decision the log *does* hold records a [`Payer`](crate::control::Payer),
//! which is an enum. The redacting impls below are therefore not the only thing
//! standing between a key and the log; they are what keeps the promise if that
//! ever stops being true, which is the shape a guarantee has to have to survive
//! a refactor nobody re-reads this file for.

use std::fmt;

use serde::{Serialize, Serializer};
use sha2::{Digest, Sha256};

use super::CredentialError;
use super::forwarded::{ForwardedCredential, REDACTED};

/// Domain separator for [`Secret::fingerprint`].
///
/// A bare `sha256(plaintext)` would be comparable against a table anyone else
/// can build — including one built from a leaked key list — so the fingerprint
/// would answer "is this *that* key?" for any candidate an attacker already
/// holds. Prefixing a constant nobody else uses makes the digest specific to
/// this deployment's log format rather than to the secret alone. The `v1` is
/// the usual rule: if this string changes, every fingerprint in every existing
/// log changes with it, so it changes only with a reason written down.
const FINGERPRINT_DOMAIN: &str = "roundhouse-credential-fingerprint-v1\n";

/// A provider API key, which renders as a fingerprint everywhere but
/// [`Secret::reveal`].
///
/// # Not `Deserialize`, on purpose
///
/// There is no `Deserialize` impl and there must not be one. A `Secret` that
/// could be deserialized is a `Secret` that can appear inline in a
/// control-plane file, which is the one thing §3's "no secrets in the file"
/// rule forbids. Secrets arrive by resolving a [`CredentialRef`](super::CredentialRef)
/// against the process environment, and the only constructor is
/// [`Secret::api_key`], which refuses OAuth-shaped material — so an env var
/// holding a device-login token is refused at boot rather than stored.
///
/// # Not `PartialEq`, also on purpose
///
/// Comparing two secrets is not a question this system asks, and the two ways
/// to answer it are both bad: comparing plaintext with `==` is a timing oracle,
/// and comparing fingerprints makes a 32-bit collision read as equality on a
/// type whose whole job is to be exact. Tests compare [`Secret::reveal`], which
/// is a sentence a reviewer can see.
///
/// # What this does not buy
///
/// The plaintext is an ordinary `String` and is **not** zeroed on drop. Doing
/// that honestly needs `zeroize`: a hand-rolled `Drop` cannot reach the copies
/// `String` may have left behind when its buffer was reallocated, so it would
/// look like a guarantee while providing none. Stated rather than approximated.
#[derive(Clone)]
pub struct Secret {
    plaintext: String,
    /// Precomputed so [`fmt::Debug`] cannot allocate-and-hash on a path that
    /// may be inside a panic handler, and so the same secret fingerprints
    /// identically on every line it appears on.
    fingerprint: String,
}

impl Secret {
    /// Accept a provider API key, refusing OAuth-shaped material by name.
    ///
    /// **The only constructor**, and deliberately fallible. Every route a
    /// secret can enter by — an admin request body, an env var read at boot,
    /// a test — goes through this one check, so "roundhouse never stores an
    /// OAuth token" is a property of the type rather than a rule each caller
    /// has to remember. See [`CredentialError::check_api_key_shape`].
    ///
    /// Surrounding whitespace is stripped — see [`Self::held`] for why that is
    /// a correctness requirement rather than a convenience.
    pub fn api_key(value: impl Into<String>) -> Result<Self, CredentialError> {
        let value: String = value.into();
        CredentialError::check_api_key_shape(&value)?;
        Ok(Self::held(value.trim()))
    }

    /// Hold a credential the client presented, for the length of one turn.
    ///
    /// **Deliberately skips [`CredentialError::check_api_key_shape`], and that
    /// is the whole difference between the two constructors.** The shape check
    /// exists to stop an OAuth token being *stored*; what pass-through forwards
    /// is an OAuth token, by construction — codex's device login produces a JWT
    /// access token, which is precisely the shape `api_key` refuses. Running
    /// the check here would refuse the one mode that exists to handle it, and
    /// running it and ignoring the answer would be worse.
    ///
    /// The rule the check enforces is kept a different way: nothing built here
    /// is ever written down. This constructor's callers are
    /// [`PresentedCredential::captured`](super::forwarded::PresentedCredential::captured)
    /// and nothing else, its output reaches the transport and the turn's own
    /// quote, and neither [`super::forwarded::ForwardedCredential`] nor this
    /// type deserializes — so there is no path from a store or a file to a
    /// value of this shape.
    ///
    /// Infallible, unlike `api_key`: a caller has already decided this value is
    /// forwardable (see `is_forwardable` in [`super::forwarded`]), and a second
    /// judgement here would be a second place the answer could differ.
    pub(super) fn forwarded(value: &str) -> Self {
        Self::held(value.trim())
    }

    /// Both constructors' shared tail: trim, fingerprint, keep.
    ///
    /// Trimming is a correctness requirement rather than a convenience. The
    /// shape check judges the trimmed form, so storing the untrimmed one would
    /// mean the string that was inspected and the string that goes on the wire
    /// are different strings — a gap a shape check must not have. It is also
    /// the ordinary case: an env var sourced from a file carries the file's
    /// trailing newline, and a newline inside an `Authorization` value is a
    /// malformed header rather than a credential a provider rejects cleanly.
    fn held(plaintext: &str) -> Self {
        Self {
            plaintext: plaintext.to_string(),
            fingerprint: fingerprint_of(plaintext),
        }
    }

    /// The plaintext.
    ///
    /// **The one seam that yields it.** Its callers are the provider clients'
    /// `execute`, which is the only code that has an upstream to present a key
    /// to. A call anywhere else — a log line, an event, a metrics label — is
    /// the defect this whole module exists to make visible in review, because
    /// there is no other way to spell it.
    pub fn reveal(&self) -> &str {
        &self.plaintext
    }

    /// Eight hex characters identifying this secret without disclosing it.
    ///
    /// Enough to answer "did the key change?" and "are these two turns using
    /// the same key?" off a log, which are the questions an operator actually
    /// has. Deliberately not the length of the plaintext: a length distinguishes
    /// key formats, and a format is the first thing an attacker needs.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

fn fingerprint_of(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(FINGERPRINT_DOMAIN.as_bytes());
    hasher.update(plaintext.as_bytes());
    hex::encode(&hasher.finalize()[..4])
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret({self})")
    }
}

impl fmt::Display for Secret {
    /// `redacted:1a2b3c4d`.
    ///
    /// The word first so a reader scanning a log knows what they are looking at
    /// before they wonder what the hex is, and the colon so the whole token is
    /// one grep.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "redacted:{}", self.fingerprint)
    }
}

impl Serialize for Secret {
    /// The same fingerprint [`fmt::Display`] renders.
    ///
    /// A `Serialize` impl on a secret looks like a footgun and is the opposite:
    /// without one, a struct carrying a `Secret` cannot derive `Serialize` at
    /// all, so the next person to need one writes `#[serde(skip)]` — or, worse,
    /// stores the key as a `String` beside the handle. With one, the derive is
    /// safe by construction and the fingerprint is what lands in the event.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// What one turn authenticates with.
///
/// Three arms because there are three genuinely different situations, and
/// collapsing any two of them costs a real guarantee:
///
/// - [`Absent`](Self::Absent) — no credential resolved. A provider client must
///   **refuse** rather than send the request: an unauthenticated call to a
///   frontier endpoint is exactly the silent failure the M7 auth ruling found
///   in codex, where `requires_openai_auth = false` attaches no `Authorization`
///   at all and the client reports nothing. [`Self::require_api_key`] is the
///   refusal, spelled once.
/// - [`Stored`](Self::Stored) — a key roundhouse resolved, from whichever tier
///   [`CredentialMode`](super::CredentialMode) selected.
/// - [`Forwarded`](Self::Forwarded) — pass-through: the caller's own credential,
///   already narrowed to what this provider's allowlist admits.
///
/// # Why the forwarded arm carries material
///
/// Stage 1 shipped this arm as a bare marker, on the argument that copying the
/// client's header into the quote would be persisting-by-another-name. That
/// argument does not survive contact with the transport, and the correction is
/// worth writing down rather than quietly making: **the quote is the only
/// argument [`FrontierClient::execute`] receives**
/// (`roundhouse-fleet/src/frontier.rs`, the `wire_protocol` rationale), and the
/// engine holds exactly one client for every provider — so a marker with no
/// material leaves the client with nothing to forward and pass-through cannot
/// work at all.
///
/// What the original argument was actually about is *persistence*, and that is
/// kept by different means: [`super::forwarded::ForwardedCredential`] has no
/// `Serialize` and no `Deserialize`, so it cannot enter a store or an event;
/// this module's own doc records that no event type carries a credential; and
/// the quote is an argument that lives as long as the turn, never a log record.
/// The allowlist that bounds what is copied, the redirect-disabled client that
/// stops it following a redirect to another origin, and the redaction of
/// echoed credentials out of upstream errors are Switchyard's design
/// (`crates/libsy-llm-client/src/backend.rs:179-241`,
/// `client.rs:115-118, 299-303` @ `5341f71`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnCredential {
    Absent,
    Stored(Secret),
    /// Serialized as the bare name, with no payload: the fingerprints inside
    /// would be a stable identifier for a user's session token across every
    /// event it appeared in, and a fingerprint an attacker can correlate is a
    /// smaller disclosure than the token but not no disclosure.
    #[serde(serialize_with = "serialize_forwarded")]
    Forwarded(ForwardedCredential),
}

/// `"forwarded"`, whatever the arm holds. See the variant.
fn serialize_forwarded<S: Serializer>(
    _credential: &ForwardedCredential,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str("forwarded")
}

impl TurnCredential {
    /// The key to authenticate with, or a refusal naming the provider.
    ///
    /// **The shape that makes the right thing the shortest thing.** A client
    /// that reached for `stored()` and got `None` has to invent a behavior, and
    /// the tempting invention — send it anyway and let the upstream decide — is
    /// a fail-open on a request path. This returns a `Result`, so the `?` a
    /// client would write anyway is the loud failure.
    ///
    /// [`Forwarded`](Self::Forwarded) is an error here rather than a special
    /// case, because a client asking for a *stored* key under pass-through has
    /// misread its own mode: forwarding happens at the header layer, not by
    /// revealing something into a request body.
    pub fn require_api_key(&self, provider: &str) -> Result<&str, CredentialError> {
        match self {
            TurnCredential::Stored(secret) => Ok(secret.reveal()),
            TurnCredential::Absent => Err(CredentialError::NoCredential {
                provider: provider.to_string(),
            }),
            TurnCredential::Forwarded(_) => Err(CredentialError::ForwardedNotStored {
                provider: provider.to_string(),
            }),
        }
    }

    /// The caller's own credential, for a client that forwards rather than
    /// authenticates.
    ///
    /// An `Option` and not a `Result`, unlike [`Self::require_api_key`], and
    /// the asymmetry is deliberate: a client asking this question is asking
    /// *which* of two modes it is in and has a stored-key branch to fall to,
    /// while a client asking for a stored key has already decided and must be
    /// refused loudly if it decided wrong.
    pub fn forwarded(&self) -> Option<&ForwardedCredential> {
        match self {
            TurnCredential::Forwarded(credential) => Some(credential),
            _ => None,
        }
    }

    pub fn is_forwarded(&self) -> bool {
        matches!(self, TurnCredential::Forwarded(_))
    }

    /// Remove this turn's own credential from an upstream's own words.
    ///
    /// **On the credential rather than on the client, because the client is
    /// where the arm gets lost.** The first version of this lived in the OpenAI
    /// client and took an `Option<&ForwardedCredential>` — which is `None` on
    /// the stored-key route, so a provider that quoted a *deployment's* key back
    /// in a 401 body ("invalid token: sk-proj-…") had it survive verbatim into
    /// [`Self::Stored`]'s error, out through the engine, and into the frame a
    /// client reads. The forwarded arm was scrubbed and the stored arm was not,
    /// and nothing in the shape of the code said so.
    ///
    /// Asking the credential closes that by construction: there is no
    /// credential a caller can hold that this does not cover, so the three arms
    /// are a `match` the compiler checks rather than an `Option` a caller can
    /// be handed the empty half of.
    ///
    /// - [`Self::Stored`] — the resolved key, through the one seam that yields
    ///   plaintext. Substring replacement for the reason
    ///   [`ForwardedCredential::redact`] is: the body is the upstream's and may
    ///   be JSON, HTML, or a proxy's plain text, and a body that happens to
    ///   contain the key for some other reason is redacted too, which is the
    ///   harmless direction.
    /// - [`Self::Forwarded`] — the caller's own headers, already narrowed to
    ///   this provider's allowlist.
    /// - [`Self::Absent`] — nothing was sent, so nothing can have been echoed.
    ///   Identity, and not a hole: an absent credential is refused before a
    ///   socket is opened.
    pub fn redact(&self, body: String) -> String {
        match self {
            TurnCredential::Absent => body,
            TurnCredential::Stored(secret) => body.replace(secret.reveal(), REDACTED),
            TurnCredential::Forwarded(forwarded) => forwarded.redact(body),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plaintext with no substring in common with any fingerprint or marker,
    /// so a scan that finds it found the real thing.
    pub(crate) const PLAINTEXT: &str = "sk-live-ZZZQQQ0000-do-not-log-me";

    #[test]
    fn a_secret_never_renders_its_plaintext() {
        let secret = Secret::api_key(PLAINTEXT).unwrap();

        for (surface, rendered) in [
            ("Debug", format!("{secret:?}")),
            ("Display", format!("{secret}")),
            ("Serialize", serde_json::to_string(&secret).unwrap()),
            (
                "Serialize inside a struct",
                serde_json::to_string(&TurnCredential::Stored(secret.clone())).unwrap(),
            ),
            (
                "Debug inside an enum",
                format!("{:?}", TurnCredential::Stored(secret.clone())),
            ),
        ] {
            assert!(
                !rendered.contains(PLAINTEXT),
                "{surface} disclosed the plaintext: {rendered}"
            );
            assert!(
                rendered.contains(secret.fingerprint()),
                "{surface} must still identify the key by fingerprint: {rendered}"
            );
        }

        // The control, and it is what makes the assertions above about
        // *rendering* rather than about the plaintext having been lost: the one
        // named seam does return it.
        assert_eq!(secret.reveal(), PLAINTEXT);
        assert_eq!(
            TurnCredential::Stored(secret.clone())
                .require_api_key("anthropic")
                .unwrap(),
            PLAINTEXT
        );
    }

    #[test]
    fn a_fingerprint_identifies_a_key_without_being_the_key() {
        let one = Secret::api_key(PLAINTEXT).unwrap();
        let same = Secret::api_key(PLAINTEXT).unwrap();
        let other = Secret::api_key("sk-live-a-different-key").unwrap();

        assert_eq!(
            one.fingerprint(),
            same.fingerprint(),
            "the same key must fingerprint the same on every line it appears on"
        );
        assert_ne!(one.fingerprint(), other.fingerprint());
        assert_eq!(one.fingerprint().len(), 8);
        assert!(!PLAINTEXT.contains(one.fingerprint()));

        // The string that was checked and the string that goes on the wire are
        // the same string. An env var sourced from a file is the ordinary way
        // this arises, and a trailing newline in an `Authorization` value is a
        // malformed header rather than a credential a provider refuses cleanly.
        let from_a_file = Secret::api_key(format!("{PLAINTEXT}\n")).unwrap();
        assert_eq!(from_a_file.reveal(), PLAINTEXT);
        assert_eq!(
            from_a_file.fingerprint(),
            one.fingerprint(),
            "and the same key does not fingerprint two ways because of how it \
             was sourced"
        );
    }

    #[test]
    fn an_unauthenticated_dispatch_is_a_refusal_and_not_a_request() {
        // The failure codex makes silently — `requires_openai_auth` unset means
        // no `Authorization` at all and no client-side error — is loud here.
        let absent = TurnCredential::Absent.require_api_key("anthropic");
        assert!(matches!(
            absent,
            Err(CredentialError::NoCredential { ref provider }) if provider == "anthropic"
        ));
        assert_eq!(
            absent.unwrap_err().code(),
            "no_credential_for_provider",
            "a client's refusal needs a code a deployment can alert on"
        );

        // A client reaching for a stored key under pass-through has misread its
        // own mode; forwarding is a different seam, not a different key.
        let forwarded = TurnCredential::Forwarded(forwarded_credential());
        assert!(matches!(
            forwarded.require_api_key("openai"),
            Err(CredentialError::ForwardedNotStored { .. })
        ));
        assert!(forwarded.is_forwarded());
        assert!(forwarded.forwarded().is_some());
        assert!(!TurnCredential::Absent.is_forwarded());
        assert!(TurnCredential::Absent.forwarded().is_none());
    }

    /// A pass-through credential shaped like what codex actually forwards.
    fn forwarded_credential() -> super::super::forwarded::ForwardedCredential {
        super::super::forwarded::PresentedCredential::captured(|name| match name {
            "authorization" => Some(FORWARDED_BEARER.to_string()),
            _ => None,
        })
        .expect("a bearer was presented")
        .for_provider("openai")
        .expect("openai has an allowlist row")
    }

    /// A JWT, because that is what a device login produces — and therefore
    /// exactly the shape [`Secret::api_key`] refuses.
    const FORWARDED_BEARER: &str = "Bearer eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJZWVkifQ.forwarded-only";

    #[test]
    fn a_forwarded_credential_is_held_but_never_serialized() {
        let credential = TurnCredential::Forwarded(forwarded_credential());

        // PROBE: the two renderings a quote produces. `Debug` is what a
        // `tracing` field on a dispatch emits; the serialization is what an
        // event would carry.
        let debug = format!("{credential:?}");
        let json = serde_json::to_string(&credential).unwrap();
        assert!(!debug.contains(FORWARDED_BEARER), "{debug}");
        assert!(!json.contains(FORWARDED_BEARER), "{json}");
        assert_eq!(
            json, r#"{"forwarded":"forwarded"}"#,
            "the serialized form carries the arm's name and no payload -- not \
             even a fingerprint, which would be a stable identifier for one \
             user's session token across every event it appeared in"
        );

        // CONTROL: the material really is there, reachable at the one seam a
        // provider client uses.
        assert!(
            credential
                .forwarded()
                .unwrap()
                .headers()
                .any(|(name, value)| name == "authorization" && value == FORWARDED_BEARER)
        );

        // And the constructor that lets it in is the one that skips the API-key
        // shape check, which is the whole reason it exists: a device login's
        // bearer is a JWT, and `api_key` refuses those on purpose.
        assert_eq!(
            Secret::api_key(FORWARDED_BEARER).unwrap_err().code(),
            "oauth_credentials_unsupported"
        );
    }

    #[test]
    fn every_arm_scrubs_its_own_credential_out_of_an_upstreams_words() {
        // PROBE: the shape a real 401 takes — the upstream quotes the
        // credential back. Both arms that *have* one must take it out, and the
        // stored arm is the one an earlier `Option<&ForwardedCredential>`
        // spelling left uncovered: it was `None` on that route, so a
        // deployment's own key travelled out of the client verbatim.
        let stored = TurnCredential::Stored(Secret::api_key(PLAINTEXT).unwrap());
        let forwarded = TurnCredential::Forwarded(forwarded_credential());
        for (arm, credential, secret) in [
            ("stored", &stored, PLAINTEXT),
            ("forwarded", &forwarded, FORWARDED_BEARER),
        ] {
            let echoed = format!(r#"{{"error":{{"message":"invalid token: {secret}"}}}}"#);
            let redacted = credential.redact(echoed);
            assert!(!redacted.contains(secret), "{arm}: {redacted}");
            assert!(redacted.contains("[REDACTED]"), "{arm}: {redacted}");
            // What the upstream actually said survives, or the redaction has
            // eaten the diagnosis along with the credential.
            assert!(redacted.contains("invalid token"), "{arm}: {redacted}");
        }

        // CONTROL: a body that echoed nothing comes back unchanged, on every
        // arm — including `Absent`, where nothing was ever sent and there is
        // therefore nothing an upstream could have quoted.
        let clean = r#"{"error":{"message":"model overloaded"}}"#.to_string();
        for (arm, credential) in [
            ("stored", &stored),
            ("forwarded", &forwarded),
            ("absent", &TurnCredential::Absent),
        ] {
            assert_eq!(credential.redact(clean.clone()), clean, "{arm}");
        }
    }
}
