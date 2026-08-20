// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The caller's own credential, carried for the length of one turn.
//!
//! Pass-through is the one mode where roundhouse holds material it did not
//! issue and must not keep. Everything in this file exists to make "held
//! in-flight only" a property of the types rather than a rule each caller
//! remembers, and it is modelled directly on Switchyard's forwarding backend
//! (`crates/libsy-llm-client/src/backend.rs:179-241` @ `5341f71`) — cited as a
//! design reference, not depended on.
//!
//! **Two types, because there are two moments and they must not be confused.**
//! [`PresentedCredential`] is what the request edge captured: whatever the
//! client sent that could be a credential, before anybody knows which upstream
//! this turn will be routed to. [`ForwardedCredential`] is what one provider's
//! allowlist admits, built only once a target has been chosen. Collapsing them
//! into one type would make "forward everything the client sent to whichever
//! provider won" a one-line mistake — an Anthropic header offered to OpenAI, or
//! the reverse — and that mistake is silent, because the upstream simply
//! ignores what it does not recognize.
//!
//! **The allowlist is per provider and closed.** A provider with no row
//! forwards nothing, which makes it unreachable under pass-through and degrades
//! the turn to local with a marker — see
//! [`Reachable::withheld_providers`](super::access::Reachable). That is the
//! fail-closed direction and it is deliberate: the alternative, forwarding
//! `Authorization` to any upstream a catalog happens to name, would send a
//! user's ChatGPT bearer wherever an operator's typo pointed.
//!
//! **Nothing here is `Serialize` or `Deserialize`, and that is the point.** A
//! forwarded credential that could be serialized is one that can land in an
//! event, and a deserializable one is a credential that can arrive from a
//! store — the two shapes §3 forbids outright. [`Secret`]'s redacting `Debug`
//! is what keeps the remaining path (a `{:?}` of the enclosing quote) honest.

use std::collections::BTreeMap;

use super::secret::Secret;

/// Header names one provider family will accept a forwarded credential in.
///
/// Lowercase because HTTP header names are case-insensitive and a lookup that
/// respected case would depend on which client capitalized what.
///
/// The OpenAI row is exactly the triple codex's `BearerAuthProvider` emits
/// (`model-provider/src/bearer_auth_provider.rs:32-46` @ `3b45c29`) and exactly
/// the triple Switchyard forwards for its OpenAI backends
/// (`backend.rs:190-200` @ `5341f71`) — two independent implementations
/// agreeing on the set, which is the strongest evidence available that it is
/// the whole set.
///
/// There is deliberately **no** Anthropic row yet. One would be easy to write
/// from Switchyard's (`authorization`, `x-api-key`), and it would be a claim
/// this repository has not tested against a real client: roundhouse's only
/// pass-through client speaks the Responses wire. A row nothing exercises is a
/// promise made to whoever reads the table, so the table says what is true.
const ALLOWLIST: &[(&str, &[&str])] = &[(
    "openai",
    &["authorization", "chatgpt-account-id", "x-openai-fedramp"],
)];

/// Every header name any provider's row admits.
///
/// What the request edge captures, because the edge runs before routing and
/// cannot know which row will apply. Narrowing happens in
/// [`PresentedCredential::for_provider`].
fn union_allowlist() -> impl Iterator<Item = &'static str> {
    ALLOWLIST
        .iter()
        .flat_map(|(_, names)| names.iter().copied())
}

/// The header names `provider` admits, empty for a provider with no row.
fn allowlist_for(provider: &str) -> &'static [&'static str] {
    let provider = provider.to_ascii_lowercase();
    ALLOWLIST
        .iter()
        .find(|(name, _)| *name == provider)
        .map(|(_, names)| *names)
        .unwrap_or(&[])
}

/// The header name that *is* the credential.
///
/// The others identify an account or select a deployment tier; without this one
/// there is nothing to authenticate with, so a capture that lacks it is not a
/// credential at all. Named once because two places ask the question.
const CREDENTIAL_HEADER: &str = "authorization";

/// Whether a header value can be put on the wire unchanged.
///
/// Visible ASCII and horizontal tab, which is what a header value may contain.
/// Checked here rather than left to the HTTP client because the failure it
/// prevents is not a transport error: a value carrying a newline is a request
/// smuggling primitive, and one carrying a NUL is a value some proxies truncate
/// and others do not. A value that fails this is dropped rather than repaired —
/// repairing a credential produces a different credential.
fn is_forwardable(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
}

/// What the request edge captured that could be a forwarded credential.
///
/// Built once per request, before routing. Holds no plaintext a reader can
/// reach: every value is a [`Secret`], so a `{:?}` of an enclosing admission
/// renders fingerprints.
#[derive(Debug, Clone)]
pub struct PresentedCredential {
    /// Lowercase header name to value. Keys are `&'static str` borrowed from
    /// [`ALLOWLIST`], not `String`, so a name that is not on the allowlist has
    /// nowhere to be stored — the filter is the type rather than a check a
    /// later edit could move.
    headers: BTreeMap<&'static str, Secret>,
}

impl PresentedCredential {
    /// Capture the allowlisted headers of one request.
    ///
    /// `lookup` is the request's own header map, passed as a closure so this
    /// module needs no HTTP types: `roundhouse-core` sits below every transport
    /// and stays there.
    ///
    /// `None` when the request carried no `Authorization` — the single fact
    /// that decides whether there is a credential to forward at all. An account
    /// id with no bearer beside it is an identity claim nobody signed, and
    /// forwarding it alone would produce an anonymous upstream request wearing
    /// somebody's account number.
    pub fn captured(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let mut headers: BTreeMap<&'static str, Secret> = BTreeMap::new();
        for name in union_allowlist() {
            let Some(value) = lookup(name) else { continue };
            let value = value.trim();
            if is_forwardable(value) {
                headers.insert(name, Secret::forwarded(value));
            }
        }
        headers
            .contains_key(CREDENTIAL_HEADER)
            .then_some(Self { headers })
    }

    /// Whether `provider` has an allowlist row this capture can satisfy.
    ///
    /// The cheap question, for the candidate filter. It exists for the reason
    /// [`TurnCredentials::reaches`](super::access::TurnCredentials::reaches)
    /// does: answering it through [`Self::for_provider`] would clone the
    /// caller's credential once per quoted candidate per turn, to learn a
    /// `bool`. Fewer live copies of a credential is the cheap half of not
    /// leaking one.
    pub fn covers(&self, provider: &str) -> bool {
        allowlist_for(provider).contains(&CREDENTIAL_HEADER)
    }

    /// What this provider's row admits, or `None` when it has no row.
    ///
    /// The narrowing, and the only way to build a [`ForwardedCredential`].
    pub fn for_provider(&self, provider: &str) -> Option<ForwardedCredential> {
        let admitted = allowlist_for(provider);
        if !admitted.contains(&CREDENTIAL_HEADER) {
            return None;
        }
        let headers: BTreeMap<&'static str, Secret> = self
            .headers
            .iter()
            .filter(|(name, _)| admitted.contains(*name))
            .map(|(name, secret)| (*name, secret.clone()))
            .collect();
        headers
            .contains_key(CREDENTIAL_HEADER)
            .then_some(ForwardedCredential { headers })
    }
}

/// One provider's share of the caller's credential, ready to go on the wire.
///
/// Reachable only through [`PresentedCredential::for_provider`], so a value of
/// this type has already been through an allowlist — there is no constructor
/// that skips one.
#[derive(Debug, Clone)]
pub struct ForwardedCredential {
    headers: BTreeMap<&'static str, Secret>,
}

impl ForwardedCredential {
    /// The headers to set, plaintext included.
    ///
    /// **The one seam that yields the caller's credential**, and the analogue of
    /// [`Secret::reveal`]. Its only caller is a provider client's `execute`,
    /// which is the only code with an upstream to present it to; a call
    /// anywhere else is the defect this module exists to make visible in
    /// review, because there is no other way to spell it.
    pub fn headers(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.headers
            .iter()
            .map(|(name, secret)| (*name, secret.reveal()))
    }

    /// Remove any echoed credential from an upstream's own words.
    ///
    /// Switchyard's `redact_forwarded_auth` (`backend.rs:214-240` @ `5341f71`),
    /// and it closes a real hole rather than a theoretical one: an upstream
    /// that rejects a bearer commonly quotes it back in the error body ("invalid
    /// token: eyJ…"), and that body is what a client sees, what a log line
    /// carries, and what an event payload would hold.
    ///
    /// Reached through [`TurnCredential::redact`](super::secret::TurnCredential::redact)
    /// rather than called directly, which is what makes "every path an upstream
    /// error takes out of a provider client goes through a redaction" true of
    /// *all three* arms rather than of this one. A client holding an
    /// `Option<&ForwardedCredential>` had a `None` on the stored-key route and
    /// scrubbed nothing there.
    ///
    /// Substring replacement rather than a parse, because the body is the
    /// upstream's and may be anything — JSON, HTML, a proxy's plain text. What
    /// it costs is that a body which happens to contain the token as a
    /// substring for some other reason is also redacted, which is the harmless
    /// direction.
    pub fn redact(&self, body: String) -> String {
        self.headers
            .values()
            .fold(body, |body, secret| body.replace(secret.reveal(), REDACTED))
    }
}

/// What an echoed credential is replaced with. Spelled loudly so a reader of a
/// redacted error knows something was taken out rather than wondering what an
/// empty string meant.
///
/// Shared with [`super::secret`], which scrubs the stored arm: one marker, so
/// an operator greps for one string whichever credential a turn used.
pub(super) const REDACTED: &str = "[REDACTED]";

#[cfg(test)]
mod tests {
    use super::*;

    /// A bearer with no substring in common with any fingerprint or marker, so
    /// a scan that finds it found the real thing. Shaped like what codex
    /// actually forwards: a JWT, which is exactly what
    /// [`Secret::api_key`] refuses.
    const BEARER: &str = "Bearer eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJaWlpRUVEifQ.do-not-log-me";
    const ACCOUNT: &str = "acct-ZZZQQQ-0000";

    fn presented(pairs: &[(&str, &str)]) -> Option<PresentedCredential> {
        PresentedCredential::captured(|name| {
            pairs
                .iter()
                .find(|(header, _)| *header == name)
                .map(|(_, value)| (*value).to_string())
        })
    }

    fn codex_headers() -> Option<PresentedCredential> {
        presented(&[
            ("authorization", BEARER),
            ("chatgpt-account-id", ACCOUNT),
            ("x-openai-fedramp", "true"),
        ])
    }

    #[test]
    fn the_allowlist_is_what_reaches_the_wire_and_nothing_else() {
        // PROBE: the triple codex emits arrives whole, in the order a provider
        // client will set it.
        let forwarded = codex_headers()
            .expect("a bearer was presented")
            .for_provider("openai")
            .expect("openai has a row");
        let sent: Vec<_> = forwarded.headers().collect();
        assert_eq!(
            sent,
            vec![
                ("authorization", BEARER),
                ("chatgpt-account-id", ACCOUNT),
                ("x-openai-fedramp", "true"),
            ]
        );

        // CONTROL: a header nobody's row names is not captured at all, so it
        // cannot reach an upstream even by a later edit that forgot to filter.
        let with_extras = presented(&[
            ("authorization", BEARER),
            ("cookie", "session=zzzz"),
            ("x-api-key", "sk-ant-not-ours"),
        ])
        .expect("a bearer was presented")
        .for_provider("openai")
        .expect("openai has a row");
        assert_eq!(
            with_extras.headers().collect::<Vec<_>>(),
            vec![("authorization", BEARER)]
        );

        // A provider with no row forwards nothing, which makes it unreachable
        // rather than reachable anonymously.
        assert!(
            codex_headers()
                .unwrap()
                .for_provider("some-new-vendor")
                .is_none()
        );
        assert!(!codex_headers().unwrap().covers("some-new-vendor"));
        assert!(
            codex_headers().unwrap().covers("OpenAI"),
            "case-insensitive"
        );
    }

    #[test]
    fn a_capture_without_a_bearer_is_not_a_credential() {
        // An account id alone identifies somebody without proving anything, and
        // forwarding it would produce an anonymous upstream request wearing a
        // real account number.
        assert!(presented(&[("chatgpt-account-id", ACCOUNT)]).is_none());
        assert!(presented(&[]).is_none());
        // Empty and whitespace are absences spelled differently.
        assert!(presented(&[("authorization", "   ")]).is_none());
        // A value that cannot go on the wire is dropped rather than repaired: a
        // newline in a header value is a smuggling primitive, not a credential.
        assert!(presented(&[("authorization", "Bearer a\r\nX-Evil: 1")]).is_none());

        // CONTROL: the ordinary case still lands, so the assertions above are
        // about the missing bearer and not about the capture refusing
        // everything.
        assert!(presented(&[("authorization", BEARER)]).is_some());
    }

    #[test]
    fn a_forwarded_credential_never_renders_its_plaintext() {
        let presented = codex_headers().expect("a bearer was presented");
        let forwarded = presented.for_provider("openai").unwrap();

        for (surface, rendered) in [
            ("Debug of the capture", format!("{presented:?}")),
            ("Debug of the narrowed credential", format!("{forwarded:?}")),
        ] {
            assert!(
                !rendered.contains(BEARER),
                "{surface} disclosed the bearer: {rendered}"
            );
            assert!(
                !rendered.contains(ACCOUNT),
                "{surface} disclosed the account id: {rendered}"
            );
            assert!(rendered.contains("redacted:"), "{surface}: {rendered}");
        }

        // CONTROL, and it is what makes the above about *rendering* rather than
        // about the credential having been lost: the one named seam yields it.
        assert!(forwarded.headers().any(|(_, value)| value == BEARER));
    }

    #[test]
    fn an_upstream_that_echoes_the_credential_is_redacted_before_anyone_reads_it() {
        let forwarded = codex_headers().unwrap().for_provider("openai").unwrap();

        // The shape a real 401 takes: the upstream quotes the token back.
        let echoed =
            format!(r#"{{"error":{{"message":"invalid token: {BEARER}","account":"{ACCOUNT}"}}}}"#);
        let redacted = forwarded.redact(echoed);
        assert!(!redacted.contains(BEARER), "{redacted}");
        assert!(!redacted.contains(ACCOUNT), "{redacted}");
        assert!(redacted.contains(REDACTED), "{redacted}");
        // What the upstream actually said still survives, or the redaction has
        // eaten the diagnosis along with the credential.
        assert!(redacted.contains("invalid token"), "{redacted}");

        // CONTROL: a body that never echoed anything is returned unchanged, so
        // the redaction is about the credential and not about rewriting errors.
        let clean = r#"{"error":{"message":"model overloaded"}}"#.to_string();
        assert_eq!(forwarded.redact(clean.clone()), clean);
    }
}
