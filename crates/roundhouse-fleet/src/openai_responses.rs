// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The first real provider client: OpenAI Responses, over two auth modes.
//!
//! One client, two routes, and the route is chosen by the credential the quote
//! carries rather than by configuration — because the credential is the only
//! thing that knows. Stage 0's auth ruling establishes that the same choice is
//! made on the client's side by `requires_openai_auth`, and that setting it
//! wrongly fails *silently* in both directions; this file is the server half of
//! that pair, and it is written so that every way of getting it wrong is loud.
//!
//! - **[`TurnCredential::Stored`]** — BYOK. `Authorization: Bearer <key>` from
//!   the resolved secret, against the platform API (or whatever base URL the
//!   deployment configured). Roundhouse authenticates as itself.
//! - **[`TurnCredential::Forwarded`]** — pass-through. The caller's own
//!   `Authorization` and `ChatGPT-Account-ID` go upstream verbatim, against the
//!   endpoint a device login implies. Roundhouse authenticates as nobody; it
//!   carries somebody else's proof and never holds one of its own.
//! - **[`TurnCredential::Absent`]** — refused, before a socket is opened. An
//!   unauthenticated request to a frontier endpoint is precisely the failure
//!   the auth ruling found on the client side, where codex sends one and
//!   reports nothing.
//!
//! Four properties are Switchyard's design, cited rather than depended on
//! (`/workspace/nvidia/switchyard` @ `5341f71`):
//!
//! 1. **A separate, redirect-disabled client carries forwarded credentials**
//!    (`crates/libsy-llm-client/src/client.rs:115-118, 299-303`). A 3xx from
//!    the configured host to any other origin would otherwise re-present the
//!    user's bearer there, and `reqwest`'s default policy follows up to ten of
//!    them. The stored-key route keeps the ordinary client: roundhouse's own
//!    key following a redirect is a smaller problem than a user's session
//!    token doing so, and refusing redirects outright on that path would break
//!    deployments behind an ordinary rewriting proxy.
//! 2. **Forwarded headers come from an explicit per-provider allowlist**
//!    (`backend.rs:179-211`), enforced here by construction: the client can
//!    only ask a
//!    [`ForwardedCredential`](roundhouse_core::control::ForwardedCredential) for
//!    its headers, and the only way to build one is through the allowlist — see
//!    [`roundhouse_core::control::PresentedCredential::for_provider`].
//! 3. **An upstream's error body is redacted of any echoed credential**
//!    (`backend.rs:214-240`) before it is returned, logged, or becomes an event
//!    payload. A provider that rejects a bearer commonly quotes it back — and
//!    it quotes a stored API key back just as readily, which is why the
//!    redaction is asked of [`TurnCredential`] rather than of the forwarded
//!    half: a `Route` carries the whole credential, so there is no arm the
//!    scrub can be reached without.
//! 4. **Forwarding is mutually exclusive with a stored key** (`config.rs:873-877`).
//!    Here that is a property of the type — [`TurnCredential`] has one arm, not
//!    a pair of optional fields — decided at the control plane, not here.
//!
//! **What is never logged.** No line in this file renders a credential.
//! `Secret` and the forwarded credential both redact in `Debug`, which covers
//! the accidental `?` on a quote; what the two seams named `reveal` and `headers`
//! return is put on a request and nowhere else. Every upstream error body goes
//! through [`TurnCredential::redact`] first — every arm of it, the stored key
//! as well as the forwarded seat.

mod stream;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use roundhouse_core::control::TurnCredential;
use roundhouse_core::routing::Target;

use crate::frontier::{FrontierClient, FrontierError, FrontierQuote, FrontierStream};
use crate::usage::WireProtocol;
use stream::SseDecoder;

/// Where a stored key authenticates: the platform API.
pub const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";

/// Where a forwarded ChatGPT device login authenticates.
///
/// Stage 0's ruling, against codex `3b45c29`: a `CodexAuth` in ChatGPT mode is
/// wrapped into a bearer and sent to the provider's configured `base_url`, and
/// `https://chatgpt.com/backend-api/codex` is what that defaults to
/// (`model-provider-info/src/lib.rs:39, 289-303`). It is a *different* origin
/// from the platform API and takes a different credential, which is why the two
/// bases are separate fields rather than one with a header switch.
pub const DEFAULT_PASS_THROUGH_BASE: &str = "https://chatgpt.com/backend-api/codex";

/// The dialect this client serializes. Anything else is refused rather than
/// mis-serialized — see [`FrontierError::UnsupportedDialect`].
const SPOKEN: WireProtocol = WireProtocol::OpenAiResponses;

/// Executes turns against an OpenAI-Responses upstream.
///
/// Holds two `reqwest::Client`s rather than one, and the reason is property (1)
/// above: connection pools are per client, so this is also what keeps forwarded
/// traffic and roundhouse's own traffic on separate pools — a user's bearer and
/// the deployment's key never share a connection.
pub struct OpenAiResponsesClient {
    /// For stored keys. Ordinary redirect policy.
    direct: reqwest::Client,
    /// For forwarded credentials. Redirects disabled, so a credential cannot
    /// follow one to another origin.
    forwarding: reqwest::Client,
    api_base: String,
    pass_through_base: String,
}

impl OpenAiResponsesClient {
    /// A client against the published endpoints.
    ///
    /// Fallible because building a `reqwest::Client` is: a process with no
    /// usable TLS backend cannot serve frontier turns, and discovering that at
    /// the first dispatch rather than at composition would fail one tenant's
    /// turn instead of failing to start.
    pub fn new() -> Result<Self, FrontierError> {
        Self::with_bases(DEFAULT_API_BASE, DEFAULT_PASS_THROUGH_BASE)
    }

    /// The same client against named base URLs.
    ///
    /// Two of them, because the two auth modes genuinely address different
    /// origins. A test points both at one mock upstream; a deployment behind a
    /// corporate egress proxy points both at it.
    pub fn with_bases(
        api_base: impl Into<String>,
        pass_through_base: impl Into<String>,
    ) -> Result<Self, FrontierError> {
        let build = |builder: reqwest::ClientBuilder| {
            builder.build().map_err(|source| {
                FrontierError::Upstream(format!("could not build an HTTP client: {source}"))
            })
        };
        Ok(Self {
            direct: build(reqwest::Client::builder())?,
            // Switchyard's `forward_auth_client`, and the comment there is the
            // whole argument: "A redirect could move provider-specific headers
            // to another origin. Forwarded credentials are sent only to the
            // configured URL." (`client.rs:115-118` @ `5341f71`.)
            forwarding: build(
                reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()),
            )?,
            api_base: trim_base(api_base.into()),
            pass_through_base: trim_base(pass_through_base.into()),
        })
    }

    /// The request body this quote becomes.
    ///
    /// Separated from [`Self::execute`] so it can be asserted without a socket,
    /// and so the mandatory step is visible in one place:
    /// [`WireProtocol::enforce_usage_reporting`] runs on every body this client
    /// builds. It adds nothing on this dialect — Responses reports usage
    /// unconditionally — and it is wired anyway, because the obligation belongs
    /// to the client rather than to the dialect that currently happens to need
    /// no help. A client that skipped it would be correct today and silently
    /// unaccounted the day its catalog entry moved to Chat Completions.
    fn body(quote: &FrontierQuote, model: &str) -> Value {
        let mut body = json!({
            "model": model,
            "stream": true,
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": quote.prompt }],
            }],
            // The whole point of the routing: providers use it to steer a
            // request to the node holding this session's prefix.
            "prompt_cache_key": quote.prompt_cache_key,
            "max_output_tokens": quote.expected_output_tokens,
            // Roundhouse rebuilds every prompt from its own log, so server-side
            // conversation state would be a second history able to disagree
            // with the one the fold replays. Off, explicitly.
            "store": false,
        });
        quote.wire_protocol.enforce_usage_reporting(&mut body);
        body
    }

    /// The headers, the base URL and the HTTP client this credential implies.
    ///
    /// One function, because the three answers must not be reachable
    /// separately: a caller that picked the pass-through base and the ordinary
    /// client would forward a user's bearer over a redirect-following
    /// connection, and nothing would say so.
    fn route<'a>(
        &'a self,
        credential: &'a TurnCredential,
        provider: &str,
    ) -> Result<Route<'a>, FrontierError> {
        match credential {
            TurnCredential::Stored(_) => {
                // Through `require_api_key` rather than by matching the secret
                // out: that is the one seam that yields plaintext, and routing
                // every read through it is what makes a grep for it complete.
                let key = credential.require_api_key(provider)?;
                let mut headers = HeaderMap::new();
                headers.insert(
                    reqwest::header::AUTHORIZATION,
                    sensitive(&format!("Bearer {key}")).ok_or_else(|| {
                        FrontierError::Upstream(
                            "the resolved key cannot be put in a header value".to_string(),
                        )
                    })?,
                );
                Ok(Route {
                    client: &self.direct,
                    base: &self.api_base,
                    headers,
                    credential,
                })
            }
            TurnCredential::Forwarded(forwarded) => {
                let mut headers = HeaderMap::new();
                for (name, value) in forwarded.headers() {
                    // Both halves are already bounded: the name comes from the
                    // allowlist and the value passed the edge's forwardable
                    // check. A failure here is therefore a bug rather than
                    // hostile input, and dropping the header silently would
                    // turn it into an anonymous upstream request -- so it is
                    // refused instead.
                    let (name, value) = HeaderName::from_bytes(name.as_bytes())
                        .ok()
                        .zip(sensitive(value))
                        .ok_or_else(|| {
                            FrontierError::Upstream(format!(
                                "the forwarded header `{name}` is not a header this client can \
                                 send; refusing to send the request without it"
                            ))
                        })?;
                    headers.insert(name, value);
                }
                Ok(Route {
                    client: &self.forwarding,
                    base: &self.pass_through_base,
                    headers,
                    credential,
                })
            }
            // The loud refusal, spelled once in `require_api_key` so this
            // client and every other one say the same thing.
            TurnCredential::Absent => Err(credential
                .require_api_key(provider)
                .expect_err("Absent never yields a key")
                .into()),
        }
    }
}

/// Everything one auth mode decides, resolved together.
struct Route<'a> {
    client: &'a reqwest::Client,
    base: &'a str,
    headers: HeaderMap,
    /// The whole credential, held past the request for one job: redacting it
    /// back out of whatever the upstream says.
    ///
    /// **The whole credential and not the forwarded half.** This field used to
    /// be an `Option<&ForwardedCredential>`, which is `None` on the stored-key
    /// route — so the redaction below ran against nothing there, and a provider
    /// that quoted a deployment's own key back in a 401 body handed it to the
    /// caller verbatim. Carrying the credential itself makes the redaction path
    /// unreachable with a credential it does not scrub, because there is no
    /// arm [`TurnCredential::redact`] does not cover.
    credential: &'a TurnCredential,
}

#[async_trait]
impl FrontierClient for OpenAiResponsesClient {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        let Target::Frontier { provider, model } = &quote.target else {
            return Err(FrontierError::UnknownProvider(format!(
                "{:?} is not a frontier target",
                quote.target
            )));
        };
        if quote.wire_protocol != SPOKEN {
            return Err(FrontierError::UnsupportedDialect {
                expected: SPOKEN.wire_name(),
                got: quote.wire_protocol.wire_name(),
                target: format!("{provider}/{model}"),
            });
        }

        let route = self.route(&quote.credential, provider)?;
        let response = route
            .client
            .post(format!("{}/responses", route.base))
            .headers(route.headers.clone())
            .json(&Self::body(quote, model))
            .send()
            .await
            .map_err(|source| {
                // A transport error's `Display` includes the URL but never a
                // header, so there is nothing to redact here -- stated because
                // the next person to add context to this message needs to know
                // it is not exempt.
                FrontierError::Upstream(format!("the request to the upstream failed: {source}"))
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(FrontierError::Upstream(format!(
                "the upstream answered {status}: {}",
                route.credential.redact(body)
            )));
        }

        Ok(decode(
            Box::pin(response.bytes_stream()),
            route.credential.clone(),
        ))
    }
}

/// A byte stream from the upstream, as [`FrontierChunk`]s.
///
/// Takes an owned [`TurnCredential`] rather than a borrow because the stream
/// outlives this call: it is handed to the engine, which reads it against a
/// deadline. That is the same clone the redaction needs anyway, and it is one
/// more live copy of the turn's credential for the length of one turn — the
/// cost of being able to redact an error that arrives mid-stream.
fn decode(
    mut bytes: BoxStream<'static, reqwest::Result<bytes::Bytes>>,
    credential: TurnCredential,
) -> FrontierStream {
    let state = (SseDecoder::default(), false);
    futures::stream::unfold(
        (bytes_state(&mut bytes), state, credential),
        |(mut bytes, (mut decoder, mut stopped), credential)| async move {
            loop {
                if let Some(chunk) = decoder.next_chunk() {
                    return Some((Ok(chunk), (bytes, (decoder, stopped), credential)));
                }
                if stopped || decoder.finished() {
                    return None;
                }
                let fed = match bytes.next().await {
                    Some(Ok(piece)) => decoder.feed(&piece),
                    Some(Err(source)) => Err(FrontierError::Upstream(format!(
                        "the upstream stream ended early: {source}"
                    ))),
                    None => decoder.eof().map(|()| stopped = true),
                };
                if let Err(error) = fed {
                    // Stop after the error: a decoder that has refused its
                    // input has no business being fed more of it.
                    return Some((
                        Err(redact_error(&credential, error)),
                        (bytes, (decoder, true), credential),
                    ));
                }
            }
        },
    )
    .boxed()
}

/// Move the boxed byte stream into the fold's state.
///
/// A named function rather than an inline move because `unfold`'s closure has
/// to own it and `BoxStream` is not `Clone`; this is the one line that says so.
fn bytes_state(
    bytes: &mut BoxStream<'static, reqwest::Result<bytes::Bytes>>,
) -> BoxStream<'static, reqwest::Result<bytes::Bytes>> {
    std::mem::replace(bytes, futures::stream::empty().boxed())
}

/// The same error with any echoed credential removed.
///
/// Every error leaving this module goes through here or through
/// [`TurnCredential::redact`] directly. Only the [`FrontierError::Upstream`]
/// arm can carry an upstream's words; the others are this client's own
/// sentences and have nothing to scrub.
fn redact_error(credential: &TurnCredential, error: FrontierError) -> FrontierError {
    match error {
        FrontierError::Upstream(message) => FrontierError::Upstream(credential.redact(message)),
        other => other,
    }
}

/// A header value the HTTP stack will not print in its own diagnostics.
///
/// `set_sensitive` is what Switchyard's `sensitive_header` does
/// (`backend.rs:292` @ `5341f71`): `hyper` consults it when deciding what to
/// put in HPACK's dynamic table and what to render in `Debug`. It is not a
/// substitute for the redaction above — it governs the *outbound* copy — and
/// both are needed because a credential can leak on the way out and on the way
/// back.
fn sensitive(value: &str) -> Option<HeaderValue> {
    let mut value = HeaderValue::from_str(value).ok()?;
    value.set_sensitive(true);
    Some(value)
}

/// A base URL with any trailing slash removed, so `{base}/responses` is one
/// slash rather than two — some gateways route on the exact path.
fn trim_base(base: String) -> String {
    base.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use roundhouse_core::control::{PresentedCredential, Secret};

    fn quote(credential: TurnCredential, wire_protocol: WireProtocol) -> FrontierQuote {
        FrontierQuote {
            target: Target::Frontier {
                provider: "openai".into(),
                model: "flagship".into(),
            },
            wire_protocol,
            prompt: "how many tokens did that turn bill?".into(),
            prompt_cache_key: "sess_openai".into(),
            expected_output_tokens: Some(512),
            credential,
        }
    }

    #[tokio::test]
    async fn a_dialect_this_client_cannot_serialize_is_refused_before_a_socket_is_opened() {
        // PROBE: the catalog and the composed client disagree. Sending anyway
        // would put a Responses body on an Anthropic route and let the upstream
        // decide, which is a fail-open with a mis-serialized request attached.
        let client = OpenAiResponsesClient::new().unwrap();
        let Err(error) = client
            .execute(&quote(
                TurnCredential::Stored(Secret::api_key("sk-proj-AAAA").unwrap()),
                WireProtocol::AnthropicMessages,
            ))
            .await
        else {
            panic!("a dialect this client cannot serialize must be refused")
        };
        assert!(
            matches!(
                &error,
                FrontierError::UnsupportedDialect { expected, got, target }
                    if *expected == "openai_responses"
                        && *got == "anthropic_messages"
                        && target == "openai/flagship"
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn no_credential_is_a_refusal_and_never_an_anonymous_request() {
        let client = OpenAiResponsesClient::new().unwrap();
        let Err(error) = client.execute(&quote(TurnCredential::Absent, SPOKEN)).await else {
            panic!("an unauthenticated dispatch must be refused, never sent")
        };
        assert!(
            matches!(&error, FrontierError::Credential(inner)
                if inner.code() == "no_credential_for_provider"),
            "{error}"
        );
    }

    #[test]
    fn the_body_carries_the_cache_key_and_the_usage_obligation_is_wired() {
        let body = OpenAiResponsesClient::body(&quote(TurnCredential::Absent, SPOKEN), "flagship");
        assert_eq!(body["model"], json!("flagship"));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["prompt_cache_key"], json!("sess_openai"));
        assert_eq!(body["max_output_tokens"], json!(512));
        assert_eq!(body["store"], json!(false));
        assert_eq!(
            body["input"][0]["content"][0]["text"],
            json!("how many tokens did that turn bill?")
        );
        // Nothing added on this dialect, which is the point of asserting it:
        // the call is wired, and the day this catalog entry speaks Chat
        // Completions it starts adding `stream_options.include_usage` rather
        // than silently reporting nothing.
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn the_two_auth_modes_choose_different_origins_and_different_clients() {
        let client = OpenAiResponsesClient::with_bases(
            "https://api.example.test/v1/",
            "https://seat.example.test",
        )
        .unwrap();

        let stored = TurnCredential::Stored(Secret::api_key("sk-proj-ZZZZ").unwrap());
        let byok = client.route(&stored, "openai").unwrap();
        assert_eq!(
            byok.base, "https://api.example.test/v1",
            "trailing slash trimmed"
        );
        assert!(
            matches!(byok.credential, TurnCredential::Stored(_)),
            "the route carries the whole credential, so the redaction path \
             cannot be reached with one it does not scrub"
        );
        assert_eq!(
            byok.headers[reqwest::header::AUTHORIZATION],
            "Bearer sk-proj-ZZZZ"
        );
        assert!(
            byok.headers[reqwest::header::AUTHORIZATION].is_sensitive(),
            "the HTTP stack must not render this in its own diagnostics"
        );

        let bearer = "Bearer eyJhbGciOiJub25lIn0.e30.seat-token";
        let forwarded = TurnCredential::Forwarded(
            PresentedCredential::captured(|name| match name {
                "authorization" => Some(bearer.to_string()),
                "chatgpt-account-id" => Some("acct-777".to_string()),
                _ => None,
            })
            .unwrap()
            .for_provider("openai")
            .unwrap(),
        );
        let seat = client.route(&forwarded, "openai").unwrap();
        assert_eq!(seat.base, "https://seat.example.test");
        assert!(matches!(seat.credential, TurnCredential::Forwarded(_)));
        assert_eq!(seat.headers[reqwest::header::AUTHORIZATION], bearer);
        assert_eq!(seat.headers["chatgpt-account-id"], "acct-777");
        // The two routes are different `reqwest::Client`s, so a forwarded
        // credential never rides a redirect-following connection and never
        // shares a pool with the deployment's own key.
        assert!(
            !std::ptr::eq(byok.client, seat.client),
            "the forwarded route must not use the redirect-following client"
        );
    }
}
