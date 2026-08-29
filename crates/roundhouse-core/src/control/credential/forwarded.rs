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
/// The Anthropic row landed with the client that exercises it (M11.0), and the
/// rule that kept it out until then is the same rule that admits it now: a row
/// nothing exercises is a promise made to whoever reads the table, so the table
/// says what is true. `roundhouse-fleet`'s `AnthropicMessagesClient` forwards
/// exactly these four on its redirect-disabled client, so the table and the wire
/// now agree.
///
/// Four names rather than Switchyard's two, and the extra pair is not
/// decoration:
///
/// - `authorization` is a Claude Code subscription seat's OAuth bearer, and
///   `x-api-key` is a caller bringing its own Anthropic key — Anthropic accepts
///   either, on different header names, so a row carrying one of them makes half
///   of the real callers unreachable.
/// - `anthropic-beta` is required, not optional: stripping the
///   `oauth-2025-04-20` beta from a subscription seat is a documented 401
///   (`agent-docs/research/claude-code-client-surface.md` §1.2), so dropping it
///   would turn every seat turn into an authentication failure that looks like a
///   revoked login.
/// - `anthropic-version` is forwardable so a caller pinned to a version can say
///   so. The client stamps its own value *after* the forwarded headers anyway —
///   it serialized the body, so it is the one that knows which version describes
///   it — which makes this row entry inert on today's dispatch path and honest
///   about what the edge is willing to carry.
///
/// What is deliberately *not* here: `x-api-key` is admitted for `anthropic` and
/// for nobody else, so a caller's Anthropic key presented on an OpenAI-routed
/// turn is still dropped at `for_provider`.
const ALLOWLIST: &[(&str, &[&str])] = &[
    (
        "openai",
        &["authorization", "chatgpt-account-id", "x-openai-fedramp"],
    ),
    (
        "anthropic",
        &[
            "authorization",
            "x-api-key",
            "anthropic-beta",
            "anthropic-version",
        ],
    ),
];

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
    /// A caller bringing its own Anthropic key. Unique for the same reason
    /// [`BEARER`] is: a scan that finds this string found the real thing.
    const ANTHROPIC_KEY: &str = "sk-ant-ZZZQQQ2222-not-ours";

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

        // CONTROL, and M11.0 split it in two because the row it used to rest on
        // moved. `x-api-key` was the negative control here until Anthropic got a
        // row; it is now a *cross-provider* control, which is the stronger of
        // the two claims and the one the two-type split exists to make:
        //
        // - `cookie` is on nobody's row, so it is never captured at all and has
        //   nowhere to be stored — the filter is the type, not a later check.
        // - `x-api-key` *is* captured, because Anthropic's row names it, and
        //   `for_provider("openai")` drops it anyway. A capture that forwarded
        //   everything it held to whichever provider won would send a caller's
        //   Anthropic key to OpenAI, which is exactly the silent mistake
        //   `PresentedCredential`/`ForwardedCredential` are two types to prevent.
        let with_extras = presented(&[
            ("authorization", BEARER),
            ("cookie", "session=zzzz"),
            ("x-api-key", ANTHROPIC_KEY),
        ])
        .expect("a bearer was presented");
        assert_eq!(
            with_extras
                .clone()
                .for_provider("openai")
                .expect("openai has a row")
                .headers()
                .collect::<Vec<_>>(),
            vec![("authorization", BEARER)]
        );
        // And the same capture narrowed to the provider whose row *does* name
        // it. Without this half the assertion above would also pass on a build
        // that had stopped capturing `x-api-key` at the edge entirely — which
        // would make the Anthropic row unreachable rather than respected.
        assert_eq!(
            with_extras
                .for_provider("anthropic")
                .expect("anthropic has a row")
                .headers()
                .collect::<Vec<_>>(),
            vec![("authorization", BEARER), ("x-api-key", ANTHROPIC_KEY)]
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

    /// **The Anthropic row, and the reason each of its four names is on it.**
    ///
    /// A Claude Code subscription seat presents an OAuth bearer *and* the
    /// `oauth-2025-04-20` beta, and Anthropic answers 401 to the bearer without
    /// the beta — so a row that carried only `authorization` would look correct,
    /// forward a real credential, and fail every seat turn with an error that
    /// says nothing about a stripped header. That is what makes this a
    /// four-name row rather than Switchyard's two, and this test is where the
    /// claim is written down.
    #[test]
    fn an_anthropic_seat_forwards_the_bearer_and_the_beta_that_makes_it_work() {
        let seat = presented(&[
            ("authorization", BEARER),
            (
                "anthropic-beta",
                "oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14",
            ),
            ("anthropic-version", "2023-06-01"),
        ])
        .expect("a bearer was presented")
        .for_provider("anthropic")
        .expect("anthropic has a row");

        assert_eq!(
            seat.headers().collect::<Vec<_>>(),
            vec![
                (
                    "anthropic-beta",
                    "oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14"
                ),
                ("anthropic-version", "2023-06-01"),
                ("authorization", BEARER),
            ]
        );

        // A key alone is a credential on this provider's *other* auth mode, but
        // `CREDENTIAL_HEADER` is `authorization` for every row — so a caller
        // presenting only `x-api-key` is not a forwardable credential and the
        // turn resolves through the deployment's own stored key instead of
        // reaching an upstream as a half-authenticated request.
        //
        // Recorded rather than argued: it is a real limitation of the one-name
        // credential rule, and the remedy if a BYO-key Claude client ever needs
        // it is a second `CREDENTIAL_HEADER` per row, not a special case here.
        assert!(presented(&[("x-api-key", ANTHROPIC_KEY)]).is_none());

        // CONTROL: nothing on this row leaks to the other provider that also
        // names `authorization`. The two rows share exactly one header name and
        // the narrowing is what keeps them apart.
        let leaked = presented(&[
            ("authorization", BEARER),
            ("anthropic-beta", "oauth-2025-04-20"),
            ("x-api-key", ANTHROPIC_KEY),
        ])
        .unwrap()
        .for_provider("openai")
        .expect("openai has a row");
        assert_eq!(
            leaked.headers().collect::<Vec<_>>(),
            vec![("authorization", BEARER)]
        );
    }

    /// **Pins each row's exact contents, closing a gap every test above
    /// leaves open.**
    ///
    /// Every test above drives a row through `PresentedCredential::captured`
    /// / `for_provider`, and both only ever ask about the header names their
    /// own fixture already lists (`authorization`, `x-api-key`,
    /// `anthropic-beta`, `anthropic-version`, `chatgpt-account-id`,
    /// `x-openai-fedramp`, plus the negative controls `cookie` and
    /// `x-claude-code-session-id`). A name appended to a row that no fixture
    /// ever presents is never looked up, so `captured` never gets the
    /// opportunity to forward it — confirmed by appending a fifth,
    /// never-asked-for entry to the anthropic row and watching every test in
    /// this file pass unchanged. Reading `ALLOWLIST` through `allowlist_for`
    /// directly, instead of through a capture, is what turns a widened (or
    /// narrowed) row into a failing assertion here rather than a silent one
    /// — the same promise-the-table-must-not-make-unexercised rule the
    /// module doc (`:53-57`) already argues for a *new* row applies equally
    /// to widening one that already exists.
    #[test]
    fn the_allowlist_names_exactly_these_headers_and_no_more() {
        assert_eq!(
            allowlist_for("openai").to_vec(),
            vec!["authorization", "chatgpt-account-id", "x-openai-fedramp"]
        );
        assert_eq!(
            allowlist_for("anthropic").to_vec(),
            vec![
                "authorization",
                "x-api-key",
                "anthropic-beta",
                "anthropic-version",
            ]
        );

        // CONTROL: a provider with no row admits nothing, so the assertions
        // above are about these two rows' exact contents and not about
        // `allowlist_for` returning something non-empty for anything asked.
        assert!(allowlist_for("some-new-vendor").is_empty());
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
