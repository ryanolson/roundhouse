// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The second real provider client: Anthropic Messages, over two auth modes.
//!
//! Deliberately the same shape as [`crate::openai_responses`], line for line
//! where it can be: one `const SPOKEN` the client refuses to deviate from, a
//! static [`AnthropicMessagesClient::body`] separated from `execute` so the
//! request is assertable without a socket, one `route()` bundling the HTTP
//! client, the base URL, the headers and the whole credential, redaction
//! exhaustive by error variant, and
//! [`TurnCredential::Absent`] refused before a socket is opened. A second client
//! that invented its own arrangement of those parts would be a second place for
//! each of them to be got wrong, and the auth ruling's finding — that setting
//! the mode wrongly fails *silently* in both directions — applies here
//! unchanged.
//!
//! Five things genuinely differ from the Responses client, and each is a fact
//! about this API rather than a preference:
//!
//! 1. **A stored key's spelling is per-provider configuration, defaulting to
//!    `x-api-key`.** Anthropic's own convention is the bare `x-api-key` header,
//!    and sending `Authorization: Bearer sk-ant-…` to `api.anthropic.com` is a
//!    401 with a message that does not say why — but this dialect has a second
//!    GA provider, OpenRouter's `/messages` route, which authenticates the other
//!    way round. So the header is named by the provider definition and resolved
//!    at boot: [`StoredAuthStyle`].
//! 2. **`anthropic-version` is mandatory on every request** — see
//!    [`ANTHROPIC_VERSION`].
//! 3. **`max_tokens` is required by the schema**, so this client always sends
//!    one — see [`DEFAULT_MAX_TOKENS`].
//! 4. **The prompt goes out as several content blocks, not one string**, so a
//!    `cache_control` breakpoint has something to attach to. Anthropic caches
//!    *nothing* without an explicit breakpoint, so flat-string parity with the
//!    Responses client would zero the provider cache discount on every Anthropic
//!    turn — against the sentence this product is built to satisfy. Ruling R3.
//! 5. **No route here follows a redirect, the stored one included.** The
//!    Responses client keeps an ordinary redirect-following transport for its
//!    own key; that is safe only because a stored OpenAI key rides
//!    `Authorization`, which `reqwest` strips when a 3xx crosses origins.
//!    Nothing strips `x-api-key`. See [`AnthropicMessagesClient::with_bases`].
//!
//! **What is never logged.** As in the Responses client: no line here renders a
//! credential, every upstream error body goes through [`TurnCredential::redact`]
//! first, and outbound credential headers are marked sensitive so `hyper` will
//! not print them in its own diagnostics.

mod stream;
pub mod wire;

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
use wire::{CacheControl, ContentBlock, Extra};

/// Where a stored Anthropic key authenticates.
///
/// No `/v1` suffix, because [`DEFAULT_MESSAGES_PATH`] carries it — the example
/// catalog's `anthropic` stanza spells the same endpoint the other way round
/// (`base_url` with `/v1`, `routes.messages = "/messages"`) and both compose to
/// the same URL, which is the point of routes being data.
pub const DEFAULT_API_BASE: &str = "https://api.anthropic.com";

/// Where a forwarded Claude subscription seat authenticates.
///
/// The *same origin* as the stored-key base, which is the one structural
/// difference from the Responses client: a ChatGPT device login addresses
/// `chatgpt.com/backend-api/codex` rather than the platform API, whereas Claude
/// Code's OAuth bearer goes to `api.anthropic.com` exactly as an API key does
/// (`research/claude-code-client-surface.md` §1.3). The two bases stay separate
/// *fields* anyway, because a deployment that fronts a seat through a gateway
/// and its own key through another still needs to say so — and because a
/// forwarded credential must not be able to inherit a base URL that was
/// configured for our own.
pub const DEFAULT_PASS_THROUGH_BASE: &str = DEFAULT_API_BASE;

/// Where the Messages route lives under a base URL, absent a definition saying
/// otherwise.
///
/// Anthropic serves it here and so does OpenRouter's GA Messages route. A
/// deployment addressing something else states its own path in the catalog's
/// `providers` section.
pub const DEFAULT_MESSAGES_PATH: &str = "/v1/messages";

/// The API version every request must declare.
///
/// **Not a formality, and not a date to keep fresh.** Anthropic requires the
/// header on every request and rejects one without it; `2023-06-01` is the only
/// value the Messages API has ever had, and the vocabulary that has grown since
/// is gated by `anthropic-beta` instead. Pinned as a constant beside this
/// sentence so the next reader does not "update" it to a newer-looking date and
/// discover that every request 400s.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The header [`ANTHROPIC_VERSION`] travels in.
const ANTHROPIC_VERSION_HEADER: HeaderName = HeaderName::from_static("anthropic-version");

/// The header a stored Anthropic key travels in.
///
/// Anthropic's convention, and the reason this client cannot simply reuse the
/// Responses client's bearer path. Nothing else in roundhouse reads or writes
/// this name today — the pass-through allowlist's Anthropic row is what makes it
/// readable at the edge, and it lands with this client.
const API_KEY_HEADER: HeaderName = HeaderName::from_static("x-api-key");

/// How a *stored* key is spelled on the way out.
///
/// **The dialect does not decide this; the provider does.** Anthropic's own
/// endpoint authenticates a key on [`API_KEY_HEADER`] and answers a bearer with
/// a 401 whose message does not say why. OpenRouter's GA `/messages` route
/// speaks the same dialect and authenticates only on `Authorization: Bearer` —
/// probed live, an `x-api-key` there answers `"Missing Authentication header"`
/// (`research/openrouter-api-surface.md`). So a client that hardcoded either
/// spelling makes the other provider unreachable, which is what R3 asked for
/// when it named OpenRouter's route the second `anthropic_messages`-speaking
/// provider.
///
/// **Configuration resolved at boot, never a host sniff at dispatch.** The
/// alternative — matching on the base URL, or on the provider name, inside
/// `route()` — would put a routing decision in a `contains("anthropic.com")`,
/// silently mis-authenticate every gateway and proxy that fronts either
/// provider under a third hostname, and give an operator no line to edit when
/// it got it wrong. A provider definition already says where its traffic goes
/// and where its key lives; which header the key rides in is the same kind of
/// fact and belongs beside them, so the boundary can refuse a spelling nobody
/// implements instead of a turn failing 401 one tenant at a time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum StoredAuthStyle {
    /// `x-api-key: <key>`, bare. Anthropic's first-party convention and the
    /// default a definition gets by saying nothing.
    #[default]
    XApiKey,
    /// `Authorization: Bearer <key>`. OpenRouter's `/messages` route.
    Bearer,
}

impl StoredAuthStyle {
    /// Every spelling a definition may name, in the order a refusal lists them.
    pub const ALL: [StoredAuthStyle; 2] = [StoredAuthStyle::XApiKey, StoredAuthStyle::Bearer];

    /// How a configuration file spells this style.
    ///
    /// Same argument [`WireProtocol::wire_name`] makes for itself: a refusal
    /// should name the value the way the operator would write it, and `{:?}`
    /// would say `XApiKey`, which appears in no file anyone can go and edit.
    pub fn wire_name(&self) -> &'static str {
        match self {
            StoredAuthStyle::XApiKey => "x_api_key",
            StoredAuthStyle::Bearer => "bearer",
        }
    }

    /// The style a file named, or `None` for a spelling nothing implements.
    ///
    /// `None` rather than a default, so the config boundary refuses a typo at
    /// load. Silently falling back to `x_api_key` would send a deployment's
    /// OpenRouter key out in a header that provider ignores, and the symptom —
    /// a 401 on every turn — names neither the file nor the field.
    pub fn from_wire_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|style| style.wire_name() == name)
    }
}

/// What this client asks for when the quote does not say.
///
/// **Anthropic requires `max_tokens` on every request** — it is one of the three
/// required properties of `CreateMessageParams` — so unlike the Responses
/// client's optional `max_output_tokens`, there is no "let the model decide"
/// here. The trade a number has to make: too low truncates an agentic turn
/// mid-answer, which arrives at the client as `stop_reason: max_tokens` and
/// costs a whole turn's input to retry; too high asks for a ceiling some models
/// reject outright as above their maximum. 8192 is the largest value every
/// current Claude model accepts without an output-extension beta, which makes it
/// the largest safe answer rather than the most generous one.
///
/// **Reached only when the caller declared no ceiling** — see
/// [`FrontierQuote::output_token_cap`]. Until M11.1's F1 this fell back to
/// `expected_output_tokens` instead, which made the router's *pricing estimate*
/// (256 by default) the upstream ceiling and truncated every real answer at
/// roughly a paragraph.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

/// The dialect this client serializes. Anything else is refused rather than
/// mis-serialized — see [`FrontierError::UnsupportedDialect`].
const SPOKEN: WireProtocol = WireProtocol::AnthropicMessages;

/// Executes turns against an Anthropic-Messages upstream.
///
/// Two `reqwest::Client`s so that a forwarded seat token never shares a
/// connection pool with the deployment's own key — pools are per client. What
/// is *not* a difference between them is the redirect policy: **both refuse
/// redirects**, which is the one place this client departs from the Responses
/// client's arrangement rather than mirroring it.
pub struct AnthropicMessagesClient {
    /// For stored keys. Redirects disabled — see [`Self::with_bases`].
    direct: reqwest::Client,
    /// For forwarded credentials. Redirects disabled, so a credential cannot
    /// follow one to another origin.
    forwarding: reqwest::Client,
    api_base: String,
    pass_through_base: String,
    /// The path under the base URL that a Messages request is POSTed to.
    messages_path: String,
    /// Static headers this provider asked for, sent on every request.
    ///
    /// Applied *before* the credential headers and never after — see
    /// [`Self::route`]. A definition that could overwrite `x-api-key` would be a
    /// file that is not the credential file deciding whose money a turn spends.
    extra_headers: HeaderMap,
    /// Which header a stored key goes out in for this provider, decided at
    /// boot. See [`StoredAuthStyle`].
    stored_auth_style: StoredAuthStyle,
}

impl AnthropicMessagesClient {
    /// A client against the published endpoints.
    pub fn new() -> Result<Self, FrontierError> {
        Self::with_bases(DEFAULT_API_BASE, DEFAULT_PASS_THROUGH_BASE)
    }

    /// The same client against named base URLs.
    ///
    /// **Every credential-bearing route is built redirect-disabled here, and the
    /// Responses client's split does not transfer.** That client keeps an
    /// ordinary redirect-following transport for its stored key, on the argument
    /// that roundhouse's own key following a 3xx is a smaller problem than a
    /// user's session token doing so. The argument depends on a fact about the
    /// *header*: a stored OpenAI key rides `Authorization`, which is one of the
    /// five names `reqwest`'s cross-host redirect sanitizer strips — so on that
    /// wire "follows a redirect" and "presents the key at the new origin" are
    /// different things. On this wire they are the same thing. A stored
    /// Anthropic key rides `x-api-key` and the allowlist row lets a forwarded
    /// one ride it too; `reqwest` strips `Authorization`, `Cookie`, `Cookie2`,
    /// `Proxy-Authorization` and `WWW-Authenticate`, and nothing else — so a
    /// followed redirect hands the deployment's long-lived key to whatever
    /// origin the `Location` names, bare, with nothing said. `anthropic-version`
    /// would ride along too; it is not a secret, which is why the rule is stated
    /// over credential-bearing *routes* rather than over one header name.
    pub fn with_bases(
        api_base: impl Into<String>,
        pass_through_base: impl Into<String>,
    ) -> Result<Self, FrontierError> {
        let build = || {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|source| {
                    FrontierError::Upstream(format!("could not build an HTTP client: {source}"))
                })
        };
        Ok(Self {
            direct: build()?,
            forwarding: build()?,
            api_base: trim_base(api_base.into()),
            pass_through_base: trim_base(pass_through_base.into()),
            messages_path: DEFAULT_MESSAGES_PATH.to_string(),
            extra_headers: HeaderMap::new(),
            stored_auth_style: StoredAuthStyle::default(),
        })
    }

    /// Serve the Messages route at a path other than [`DEFAULT_MESSAGES_PATH`].
    pub fn with_messages_path(mut self, path: impl Into<String>) -> Self {
        self.messages_path = path.into();
        self
    }

    /// Spell a stored key for this provider the way it authenticates.
    ///
    /// A builder rather than an argument to `route()`, because the answer is a
    /// property of the provider this client was composed for and not of the turn
    /// being dispatched — see [`StoredAuthStyle`] for why that distinction is
    /// the whole fix. Absent a call, the first-party convention stands.
    pub fn with_stored_auth_style(mut self, style: StoredAuthStyle) -> Self {
        self.stored_auth_style = style;
        self
    }

    /// Send `headers` on every request this client makes.
    ///
    /// Fallible for the reason [`Self::new`] is: a header name or value the HTTP
    /// stack will not accept is a configuration mistake, and discovering it at
    /// the first dispatch would fail one tenant's turn for a line in a file. The
    /// composition root turns this into a boot refusal.
    ///
    /// Values are **not** marked sensitive: these are identification headers a
    /// gateway asks for, written in a file that is not the credential file. A key
    /// does not belong here — it travels on the quote.
    pub fn with_extra_headers<I>(mut self, headers: I) -> Result<Self, FrontierError>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        for (name, value) in headers {
            let parsed = HeaderName::from_bytes(name.as_bytes())
                .ok()
                .zip(HeaderValue::from_str(&value).ok())
                .ok_or_else(|| {
                    FrontierError::Upstream(format!(
                        "`{name}` is not a header this client can send; refusing to build a \
                         transport that would drop a header a provider asked for"
                    ))
                })?;
            self.extra_headers.insert(parsed.0, parsed.1);
        }
        Ok(self)
    }

    /// The request body this quote becomes.
    ///
    /// Separated from [`Self::execute`] so it can be asserted without a socket,
    /// and fallible where the Responses client's is not: this one *slices* the
    /// prompt, and a segment structure that does not describe the string it
    /// belongs to would silently change what the model is asked.
    ///
    /// [`WireProtocol::enforce_usage_reporting`] runs on every body this client
    /// builds. It adds nothing on this dialect — Messages reports usage
    /// unconditionally, if in two halves — and it is wired anyway, because the
    /// obligation belongs to the client rather than to the dialect that
    /// currently happens to need no help.
    fn body(quote: &FrontierQuote, model: &str) -> Result<Value, FrontierError> {
        let segments = quote.segments()?;
        // **The breakpoint goes on the penultimate block, and nowhere else.**
        //
        // Anthropic caches nothing without an explicit `cache_control` marker —
        // unlike the Responses API, where `prompt_cache_key` steers a request to
        // a node that caches on its own. So a client that sent no breakpoint
        // would get a 0% hit rate on every turn, and the router would keep
        // pricing this target on a `CacheModel::Deterministic` prediction that
        // nothing could fulfil. Routing on a predicted cache hit and then
        // prompting in a way that defeats it is the failure this module's
        // sibling doc names first.
        //
        // Penultimate rather than last, because a breakpoint caches everything
        // *up to and including* the block it sits on. The final segment is this
        // turn's new input — the part that by construction was not in the prefix
        // last turn — so marking it would write a cache entry that the next turn
        // cannot read, paying the write premium for nothing. Marking the one
        // before it caches exactly the stable prefix.
        //
        // Fewer than two segments means there is no stable prefix to name yet:
        // one block is the whole prompt, which is entirely this turn's input.
        let breakpoint = segments.len().checked_sub(2);
        // Built out of [`wire::ContentBlock`] rather than hand-written JSON,
        // because this is the one place roundhouse *originates* this wire's
        // vocabulary and R1's rule is "typed where roundhouse reads or
        // originates". A `json!` literal here would be a second spelling of
        // `"type": "text"` that the module's pinning tests do not cover, and it
        // would keep spelling it after an upstream rename that turned those
        // tests red.
        let content: Vec<ContentBlock> = segments
            .iter()
            .enumerate()
            .map(|(index, text)| ContentBlock::Text {
                text: (*text).to_string(),
                cache_control: (Some(index) == breakpoint).then(CacheControl::ephemeral),
                extra: Extra::new(),
            })
            .collect();
        // `expect` and not a `?`: these are owned `String`s and a fixed struct,
        // so the only way this fails is a serde bug. Routing it through
        // `FrontierError::Upstream` would be worse than a panic, because that
        // arm is the one the redaction path scrubs an *upstream's* words out of
        // — putting our own sentences in it makes the arm mean two things.
        let content = serde_json::to_value(content).expect("text blocks serialize");

        let mut body = json!({
            "model": model,
            // Required by the schema; see `DEFAULT_MAX_TOKENS`. The client's
            // declared ceiling and never the router's estimate — the two were
            // one field until M11.1's F1, and reading the estimate here capped
            // every answer at the 256 tokens the *pricing* default happened to
            // be.
            "max_tokens": quote.output_token_cap.unwrap_or(DEFAULT_MAX_TOKENS),
            "stream": true,
            // **One user message, several blocks.** Structural parity with the
            // Responses client, which also wraps the whole render in one user
            // message: the prompt is a single `<|role|>`-prefixed projection of
            // the log, and re-deriving roles from it here would be a second
            // projection able to disagree with `rendered()` — and therefore with
            // `turn_id_for` and every block hash. The blocks are a *slicing* of
            // that one string, which is why the segments are asserted to rejoin
            // to it byte-exactly.
            "messages": [{ "role": "user", "content": content }],
        });
        // **The client's tool definitions, verbatim and untouched.**
        //
        // This is the one part of a Messages request roundhouse forwards rather
        // than originates, and the asymmetry with the blocks above is the whole
        // point of [`FrontierQuote::tools`] being JSON. The blocks are a slicing
        // of roundhouse's own render, so they are typed and built here. The
        // tools are the *client's* declaration of what its own process can run —
        // twenty-four of them on a real Claude Code turn, several carrying
        // input schemas this build has never modelled and one carrying the
        // client's own `cache_control` breakpoint — and there is nothing for
        // roundhouse to be right about in them. Re-encoding through a type would
        // drop what it did not know, and the model would then be told about a
        // smaller toolbox than the client has: a client whose tools silently
        // stop working, never an error.
        //
        // Absent means absent: no key at all rather than `null`, because
        // `"tools": null` and `"tool_choice": null` are properties the schema
        // does not accept, and sending them would 400 every turn from an
        // internal caller that has no tools — the judge, the validate loop, an
        // MCP turn.
        if let Some(tools) = &quote.tools {
            body["tools"] = tools.clone();
        }
        if let Some(tool_choice) = &quote.tool_choice {
            body["tool_choice"] = tool_choice.clone();
        }
        quote.wire_protocol.enforce_usage_reporting(&mut body);
        Ok(body)
    }

    /// The headers, the base URL and the HTTP client this credential implies.
    ///
    /// One function, because the three answers must not be reachable
    /// separately: a caller that picked the pass-through base and the
    /// stored-key transport would send a user's seat token down a connection
    /// pool the deployment's own key shares, and nothing would say so. Since F1
    /// the redirect policy is no longer one of the differences — both
    /// transports refuse redirects — which makes the pool the whole of what
    /// picking the wrong one costs, and it is still not a choice a caller
    /// should be able to make one half of.
    fn route<'a>(
        &'a self,
        credential: &'a TurnCredential,
        provider: &str,
    ) -> Result<Route<'a>, FrontierError> {
        match credential {
            TurnCredential::Stored(_) => {
                // Through `require_api_key` rather than by matching the secret
                // out: that is the one seam that yields plaintext.
                let key = credential.require_api_key(provider)?;
                let mut headers = self.extra_headers.clone();
                // **The spelling was decided at boot, not here**, and the
                // `match` is what keeps that true: there is no `provider`
                // string, no base URL and no hostname in this arm, so a third
                // provider gets a line in a catalog file rather than a branch
                // in a dispatch path. `x-api-key` carries the key bare and the
                // bearer carries it prefixed — the one detail a reader porting
                // either client would get wrong in the other direction.
                let (header, value) = match self.stored_auth_style {
                    StoredAuthStyle::XApiKey => (API_KEY_HEADER, key.to_string()),
                    StoredAuthStyle::Bearer => {
                        (reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
                    }
                };
                // Seeded with the provider's static headers, then the credential
                // on top: a definition cannot displace the one header that
                // decides whose money this turn spends.
                headers.insert(
                    header,
                    sensitive(&value).ok_or_else(|| {
                        FrontierError::Upstream(
                            "the resolved key cannot be put in a header value".to_string(),
                        )
                    })?,
                );
                Ok(Route {
                    client: &self.direct,
                    base: &self.api_base,
                    headers: self.envelope(headers),
                    credential,
                })
            }
            TurnCredential::Forwarded(forwarded) => {
                let mut headers = self.extra_headers.clone();
                for (name, value) in forwarded.headers() {
                    // Both halves are already bounded: the name comes from the
                    // allowlist and the value passed the edge's forwardable
                    // check. A failure here is therefore a bug rather than
                    // hostile input, and dropping the header silently would turn
                    // it into an anonymous upstream request -- so it is refused
                    // instead. On this provider that matters more than on the
                    // other one: stripping `anthropic-beta` from a subscription
                    // seat is a documented 401.
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
                    headers: self.envelope(headers),
                    credential,
                })
            }
            // The loud refusal, spelled once in `require_api_key` so this client
            // and every other one say the same thing.
            TurnCredential::Absent => Err(credential
                .require_api_key(provider)
                .expect_err("Absent never yields a key")
                .into()),
        }
    }

    /// Stamp the API version onto a route's headers.
    ///
    /// **Last, on both routes, and it wins over everything.** The version header
    /// describes the body *this client* serialized, not the body some other
    /// client sent: a request built here is a fresh Messages body from
    /// roundhouse's own log, so a value copied from a catalog file or forwarded
    /// from a caller would be describing a request that no longer exists. The
    /// caller's own `anthropic-beta` still rides verbatim — that one names
    /// features, and stripping it from a seat is a 401.
    fn envelope(&self, mut headers: HeaderMap) -> HeaderMap {
        headers.insert(
            ANTHROPIC_VERSION_HEADER,
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        headers
    }
}

/// Everything one auth mode decides, resolved together.
struct Route<'a> {
    client: &'a reqwest::Client,
    base: &'a str,
    headers: HeaderMap,
    /// The whole credential, held past the request for one job: redacting it
    /// back out of whatever the upstream says. The whole credential and not the
    /// forwarded half, so the redaction path cannot be reached with a credential
    /// it does not scrub — see the Responses client's field of the same name.
    credential: &'a TurnCredential,
}

#[async_trait]
impl FrontierClient for AnthropicMessagesClient {
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

        let body = Self::body(quote, model)?;
        let route = self.route(&quote.credential, provider)?;
        let response = route
            .client
            .post(format!("{}{}", route.base, self.messages_path))
            .headers(route.headers.clone())
            .json(&body)
            .send()
            .await
            .map_err(|source| {
                // A transport error's `Display` includes the URL but never a
                // header, so there is nothing to redact here -- stated because
                // the next person to add context to this message needs to know
                // it is not exempt.
                //
                // `is_timeout` is read here and nowhere else: it is a fact the
                // transport knows and nothing downstream can recover, and it is
                // the difference between an attempt row that says the provider
                // was unreachable and one that says it was slow.
                FrontierError::Transport {
                    timed_out: source.is_timeout(),
                    message: source.to_string(),
                }
            })?;

        let status = response.status();
        // **A redirect is refused by name rather than reported as a bare 3xx.**
        // Both transports are redirect-disabled, so this arm is what the policy
        // actually *does* — and a 3xx body is empty, which would otherwise leave
        // an operator with "the upstream answered 307:" and nothing to act on.
        // The `Location` is the diagnosis (a gateway pointing somewhere nobody
        // configured), so it is named, through the same redaction every other
        // upstream-supplied string in this module goes through.
        if status.is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(|value| route.credential.redact(value.to_string()))
                .unwrap_or_else(|| "an unnamed location".to_string());
            return Err(FrontierError::Status {
                status: status.as_u16(),
                message: format!(
                    "the upstream redirected to `{location}`, which this client refuses to \
                     follow: every route here carries a credential in a header `reqwest` \
                     does not strip on a cross-host redirect, so following one would \
                     present it at an origin nobody configured"
                ),
            });
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(FrontierError::Status {
                status: status.as_u16(),
                message: route.credential.redact(body),
            });
        }

        Ok(decode(
            Box::pin(response.bytes_stream()),
            route.credential.clone(),
        ))
    }
}

/// A byte stream from the upstream, as [`FrontierChunk`](crate::FrontierChunk)s.
///
/// Takes an owned [`TurnCredential`] rather than a borrow because the stream
/// outlives this call: it is handed to the engine, which reads it against a
/// deadline. That is the same clone the redaction needs anyway.
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
                    // Stop after the error: a decoder that has refused its input
                    // has no business being fed more of it.
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
fn bytes_state(
    bytes: &mut BoxStream<'static, reqwest::Result<bytes::Bytes>>,
) -> BoxStream<'static, reqwest::Result<bytes::Bytes>> {
    std::mem::replace(bytes, futures::stream::empty().boxed())
}

/// The same error with any echoed credential removed.
///
/// Every error leaving this module goes through here or through
/// [`TurnCredential::redact`] directly. Two arms can carry an upstream's words —
/// [`FrontierError::Upstream`] and [`FrontierError::Status`] — and both are
/// scrubbed; the others are this client's own sentences. The match is exhaustive
/// rather than a wildcard so that a variant added later cannot join the list of
/// things that carry a body without somebody deciding it should.
fn redact_error(credential: &TurnCredential, error: FrontierError) -> FrontierError {
    match error {
        FrontierError::Upstream(message) => FrontierError::Upstream(credential.redact(message)),
        FrontierError::Status { status, message } => FrontierError::Status {
            status,
            message: credential.redact(message),
        },
        other @ (FrontierError::UnknownProvider(_)
        | FrontierError::Credential(_)
        | FrontierError::MalformedQuote(_)
        | FrontierError::UnsupportedDialect { .. }
        | FrontierError::Transport { .. }) => other,
    }
}

/// A header value the HTTP stack will not print in its own diagnostics.
fn sensitive(value: &str) -> Option<HeaderValue> {
    let mut value = HeaderValue::from_str(value).ok()?;
    value.set_sensitive(true);
    Some(value)
}

/// A base URL with any trailing slash removed, so `{base}/v1/messages` is one
/// slash rather than two — some gateways route on the exact path.
fn trim_base(base: String) -> String {
    base.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use roundhouse_core::control::{PresentedCredential, Secret};

    /// Three items, so there is a stable prefix *and* a new turn: the shape
    /// every real dispatch has and the only one where a breakpoint has a place
    /// to go.
    const SYSTEM: &str = "<|system|>you are a coding agent";
    const HISTORY: &str = "<|assistant|>I read the file.";
    const TURN: &str = "<|user|>now fix it";

    fn prompt() -> String {
        format!("{SYSTEM}{HISTORY}{TURN}")
    }

    fn boundaries() -> Vec<usize> {
        vec![SYSTEM.len(), SYSTEM.len() + HISTORY.len()]
    }

    fn quote(credential: TurnCredential, wire_protocol: WireProtocol) -> FrontierQuote {
        FrontierQuote {
            target: Target::Frontier {
                provider: "anthropic".into(),
                model: "claude-sonnet".into(),
            },
            wire_protocol,
            prompt: prompt(),
            segment_boundaries: boundaries(),
            prompt_cache_key: "sess_anthropic".into(),
            expected_output_tokens: Some(512),
            // No client declared a ceiling on these fixtures, which is what
            // every internal caller looks like; see `output_token_cap`.
            output_token_cap: None,
            // Nor any tools, for the same reason. The tests that need them set
            // them on a clone, so every other assertion here is also a control
            // for "a quote with no tools sends no `tools` key".
            tools: None,
            tool_choice: None,
            credential,
        }
    }

    #[tokio::test]
    async fn a_dialect_this_client_cannot_serialize_is_refused_before_a_socket_is_opened() {
        // PROBE: the catalog and the composed client disagree. Sending anyway
        // would put a Messages body on a Responses route and let the upstream
        // decide, which is a fail-open with a mis-serialized request attached.
        let client = AnthropicMessagesClient::new().unwrap();
        let Err(error) = client
            .execute(&quote(
                TurnCredential::Stored(Secret::api_key("sk-ant-AAAA").unwrap()),
                WireProtocol::OpenAiResponses,
            ))
            .await
        else {
            panic!("a dialect this client cannot serialize must be refused")
        };
        assert!(
            matches!(
                &error,
                FrontierError::UnsupportedDialect { expected, got, target }
                    if *expected == "anthropic_messages"
                        && *got == "openai_responses"
                        && target == "anthropic/claude-sonnet"
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn no_credential_is_a_refusal_and_never_an_anonymous_request() {
        let client = AnthropicMessagesClient::new().unwrap();
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
    fn the_body_blocks_the_prompt_at_its_item_boundaries_and_rejoins_byte_exactly() {
        let body =
            AnthropicMessagesClient::body(&quote(TurnCredential::Absent, SPOKEN), "claude-sonnet")
                .unwrap();

        assert_eq!(body["model"], json!("claude-sonnet"));
        assert_eq!(body["stream"], json!(true));
        // The fixture declares no client ceiling, so this is the client's own
        // default rather than the quote's 512-token pricing estimate — which it
        // was until M11.1's F1 split the two. Which number wins is pinned by
        // `the_wire_ceiling_is_the_declared_cap_and_never_the_pricing_estimate`;
        // here it is only asserted so this body stays fully described.
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));

        let content = body["messages"][0]["content"]
            .as_array()
            .expect("one user message carrying blocks")
            .clone();
        assert_eq!(body["messages"][0]["role"], json!("user"));
        assert_eq!(content.len(), 3);

        // **The invariant the whole segment structure rests on.** The blocks are
        // a slicing of the one canonical render, so `turn_id_for`, the block
        // hashes and `rendered()` stay one projection. Anything else is a second
        // rendering able to disagree with the log.
        let rejoined: String = content
            .iter()
            .map(|block| block["text"].as_str().expect("a text block").to_string())
            .collect();
        assert_eq!(rejoined, prompt());

        // The breakpoint sits on the penultimate block: the stable prefix's last
        // block. On the final one it would cache this turn's own new input,
        // paying the write premium for an entry the next turn cannot read.
        assert!(content[0].get("cache_control").is_none());
        assert_eq!(
            content[1]["cache_control"],
            json!({ "type": "ephemeral" }),
            "the stable prefix must carry the only breakpoint"
        );
        assert!(content[2].get("cache_control").is_none());

        // Nothing added on this dialect, which is the point of asserting it: the
        // call is wired, and the day this catalog entry speaks Chat Completions
        // it starts adding `stream_options.include_usage` rather than silently
        // reporting nothing.
        assert!(body.get("stream_options").is_none());
    }

    /// **The outbound body names only properties the schema has**, because
    /// `CreateMessageParams` carries `additionalProperties: false` — the one
    /// place this API is strict (evidence doc §3). An extra top-level field is
    /// not ignored here the way it is on the Responses wire; it is a 400 on
    /// every turn.
    ///
    /// A whitelist read out of the pinned spec rather than a hand-typed list,
    /// because the failure this guards is somebody *adding* a field, and a
    /// deny-list of the fields we thought of would stay green while a third
    /// arrived.
    #[test]
    fn the_outbound_body_names_only_properties_the_request_schema_allows() {
        let pin = wire::pin::spec_pin();
        let schema = &pin["vocabulary"]["create_message_params"];
        let allowed = wire::pin::strings(&schema["properties"]);
        assert!(
            schema["additional_properties_false"]
                .as_bool()
                .expect("the pin records the request schema's strictness"),
            "if the request schema stops being closed this test is still correct \
             but no longer load-bearing; the pin is what says which it is"
        );

        let body =
            AnthropicMessagesClient::body(&quote(TurnCredential::Absent, SPOKEN), "claude-opus")
                .unwrap();
        for field in body.as_object().expect("the body is an object").keys() {
            assert!(
                allowed.iter().any(|property| property == field),
                "`{field}` is not a property of CreateMessageParams; the schema \
                 is closed, so a request carrying it is a 400 on every turn"
            );
        }

        // The three the schema requires, named rather than merely present in the
        // list above — this is the assertion a future edit has to argue with.
        for required in wire::pin::strings(&schema["required"]) {
            assert!(
                body.get(&required).is_some(),
                "`{required}` is required by CreateMessageParams and this body \
                 omits it"
            );
        }

        // And the field the Responses client sends that this one must not: there
        // is no session or cache-key property on this wire at all, so sending
        // one would be an unknown field on a closed schema.
        assert!(body.get("prompt_cache_key").is_none());
    }

    /// **The client's tools ride out byte-for-byte, and only when it sent
    /// some.**
    ///
    /// The payload is deliberately hostile to a re-encoding: a nested
    /// `input_schema` this build models nowhere, a `cache_control` breakpoint of
    /// the client's own on the last tool (which is how Claude Code caches its
    /// twenty-four-tool preamble — dropping it costs the discount on the largest
    /// stable block in the request), a server-tool `type` this build has never
    /// named, and a `tool_choice` that is an object rather than a string. A
    /// typed projection would have quietly dropped at least the last two, and a
    /// model told about a smaller toolbox than the client has fails in the one
    /// way nobody debugs: the client's tools simply stop being offered.
    #[test]
    fn the_clients_tools_and_tool_choice_travel_verbatim_or_not_at_all() {
        let tools = json!([
            {
                "name": "Grep",
                "description": "search",
                "input_schema": {
                    "type": "object",
                    "properties": { "pattern": { "type": "string" } },
                    "required": ["pattern"],
                },
            },
            // A server tool: named by `type`, carrying no schema, and modelled
            // by nothing in this crate.
            { "type": "web_search_20250305", "name": "web_search", "max_uses": 5 },
            {
                "name": "Read",
                "input_schema": { "type": "object" },
                // The client's own breakpoint, on the last tool: the boundary
                // that caches the whole tool preamble.
                "cache_control": { "type": "ephemeral" },
            },
        ]);
        let tool_choice = json!({ "type": "auto", "disable_parallel_tool_use": false });

        let mut with_tools = quote(TurnCredential::Absent, SPOKEN);
        with_tools.tools = Some(tools.clone());
        with_tools.tool_choice = Some(tool_choice.clone());
        let body = AnthropicMessagesClient::body(&with_tools, "claude-sonnet").unwrap();

        assert_eq!(body["tools"], tools, "the client's bytes, unmodified");
        assert_eq!(body["tool_choice"], tool_choice);

        // And they are properties the *pinned* schema allows, which is what says
        // forwarding them cannot 400 on a closed request schema. Read from the
        // pin rather than asserted from memory, so the day upstream renames one
        // this goes red instead of every turn doing so.
        let allowed = wire::pin::strings(
            &wire::pin::spec_pin()["vocabulary"]["create_message_params"]["properties"],
        );
        for field in ["tools", "tool_choice"] {
            assert!(
                allowed.iter().any(|property| property == field),
                "`{field}` is not a property of CreateMessageParams in the pinned \
                 spec, so forwarding it is a 400 on every tool-using turn"
            );
        }

        // CONTROL: a quote with nothing declared sends no key at all, rather
        // than a `null`. `"tools": null` is not a value the closed schema
        // accepts, so a defaulted `null` would 400 every internal turn — the
        // judge, the validate loop, an MCP call — none of which has tools.
        let bare =
            AnthropicMessagesClient::body(&quote(TurnCredential::Absent, SPOKEN), "claude-sonnet")
                .unwrap();
        assert!(bare.get("tools").is_none());
        assert!(bare.get("tool_choice").is_none());

        // A choice without tools is forwarded as sent, not suppressed: the
        // upstream refuses it with a message naming the field, which is a better
        // answer than this client silently deciding the request was malformed.
        let mut choice_only = quote(TurnCredential::Absent, SPOKEN);
        choice_only.tool_choice = Some(tool_choice.clone());
        let body = AnthropicMessagesClient::body(&choice_only, "claude-sonnet").unwrap();
        assert_eq!(body["tool_choice"], tool_choice);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn a_prompt_with_no_known_structure_is_one_block_and_carries_no_breakpoint() {
        // The degenerate case: a quote built by a caller that knows nothing
        // about item boundaries. One block, the whole prompt, and *no*
        // breakpoint — with one block there is no stable prefix to name, and
        // marking the only block would write a cache entry covering this turn's
        // own input.
        let mut quote = quote(TurnCredential::Absent, SPOKEN);
        quote.segment_boundaries.clear();
        let body = AnthropicMessagesClient::body(&quote, "claude-sonnet").unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], json!(prompt()));
        assert!(content[0].get("cache_control").is_none());

        // Two segments is the smallest shape that has a prefix, and it gets one.
        let mut two = quote.clone();
        two.segment_boundaries = vec![SYSTEM.len()];
        let body = AnthropicMessagesClient::body(&two, "claude-sonnet").unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["cache_control"], json!({ "type": "ephemeral" }));
        assert!(content[1].get("cache_control").is_none());
    }

    #[test]
    fn a_segment_structure_that_does_not_describe_the_prompt_is_refused_not_guessed() {
        // PROBE: each way a boundary list can be wrong. Slicing on any of them
        // would either panic or send the model a differently-cut prompt than the
        // one the turn was priced and hashed on — and the second is the silent
        // one.
        for bad in [
            vec![9_999],                                      // past the end
            vec![SYSTEM.len() + HISTORY.len(), SYSTEM.len()], // out of order
            vec![SYSTEM.len(), SYSTEM.len()],                 // an empty segment
            vec![0],                                          // an empty first block
            vec![prompt().len()],                             // an empty last block
        ] {
            let mut quote = quote(TurnCredential::Absent, SPOKEN);
            quote.segment_boundaries = bad.clone();
            let error = AnthropicMessagesClient::body(&quote, "claude-sonnet")
                .expect_err(&format!("{bad:?} must be refused"));
            assert!(
                matches!(error, FrontierError::MalformedQuote(_)),
                "{error} for {bad:?}"
            );
        }

        // CONTROL: the well-formed list from the same helper is accepted, so the
        // assertions above are about the boundaries and not about the check
        // refusing everything.
        assert!(
            AnthropicMessagesClient::body(&quote(TurnCredential::Absent, SPOKEN), "claude-sonnet")
                .is_ok()
        );
    }

    #[test]
    fn max_tokens_is_always_sent_because_the_schema_requires_it() {
        let mut quote = quote(TurnCredential::Absent, SPOKEN);
        quote.expected_output_tokens = None;
        quote.output_token_cap = None;
        let body = AnthropicMessagesClient::body(&quote, "claude-sonnet").unwrap();
        assert_eq!(
            body["max_tokens"],
            json!(DEFAULT_MAX_TOKENS),
            "a Messages request without max_tokens is a 400, so there is no \
             `let the model decide` to fall back to"
        );
    }

    /// **The ceiling on the wire is the client's, never the router's estimate.**
    ///
    /// M11.1's F1, on this side of the seam: `expected_output_tokens` is what
    /// the turn was *priced* at (256 by default, and nothing overrides it), and
    /// while it was also what this body sent, every answer this client
    /// dispatched was cut off at roughly a paragraph — reported to the client
    /// as an ordinary `stop_reason` and to nobody as a defect.
    ///
    /// The estimate is left set in both arms deliberately: an implementation
    /// that reads it here passes an assertion that only sets the cap, so the
    /// second arm — a large estimate that must *not* reach the wire — is the
    /// one carrying the finding.
    #[test]
    fn the_wire_ceiling_is_the_declared_cap_and_never_the_pricing_estimate() {
        let mut quote = quote(TurnCredential::Absent, SPOKEN);
        quote.expected_output_tokens = Some(256);
        quote.output_token_cap = Some(64_000);
        let body = AnthropicMessagesClient::body(&quote, "claude-sonnet").unwrap();
        assert_eq!(
            body["max_tokens"],
            json!(64_000),
            "the client asked for 64 000 tokens and the router expected 256 of \
             them; the ceiling sent upstream is the client's"
        );

        quote.output_token_cap = None;
        quote.expected_output_tokens = Some(100_000);
        let body = AnthropicMessagesClient::body(&quote, "claude-sonnet").unwrap();
        assert_eq!(
            body["max_tokens"],
            json!(DEFAULT_MAX_TOKENS),
            "with no declared cap the fallback is this client's own default — a \
             pricing estimate is not a ceiling in either direction"
        );
    }

    #[test]
    fn a_stored_key_goes_out_bare_in_x_api_key_and_never_as_a_bearer() {
        let client = AnthropicMessagesClient::new().unwrap();
        let stored = TurnCredential::Stored(Secret::api_key("sk-ant-api03-ZZZZ").unwrap());
        let route = client.route(&stored, "anthropic").unwrap();

        assert_eq!(route.headers["x-api-key"], "sk-ant-api03-ZZZZ");
        assert!(
            route.headers.get(reqwest::header::AUTHORIZATION).is_none(),
            "Anthropic authenticates on x-api-key; a bearer beside it is a 401 \
             whose message does not say why"
        );
        assert!(
            route.headers["x-api-key"].is_sensitive(),
            "the HTTP stack must not render this in its own diagnostics"
        );
        assert_eq!(route.headers["anthropic-version"], ANTHROPIC_VERSION);
        assert!(
            matches!(route.credential, TurnCredential::Stored(_)),
            "the route carries the whole credential, so the redaction path \
             cannot be reached with one it does not scrub"
        );
    }

    /// F4 (thermo-nuclear review of d0821f9, **valid**): R3 names OpenRouter's
    /// GA `/messages` route as the second `anthropic_messages`-speaking
    /// provider, "stored-key only" (`PLAN-anthropic-messages.md:166-169`), and
    /// `route()`'s `Stored` arm used to insert `API_KEY_HEADER` unconditionally
    /// for every provider name — while OpenRouter's `/messages` route
    /// authenticates only on `Authorization: Bearer`, never `x-api-key`
    /// (`research/openrouter-api-surface.md:528`: probed live, `x-api-key:` +
    /// `anthropic-version:` -> `"Missing Authentication header"`). So that
    /// provider could not authenticate at all.
    ///
    /// **The remedy is configuration, not a host sniff**, which is why this
    /// test builds the client the way the composition root builds an OpenRouter
    /// definition rather than by pointing it at OpenRouter's hostname: a
    /// `route()` that branched on the base URL would mis-authenticate every
    /// gateway fronting either provider under a third name. The control below
    /// is what proves the style is doing the work.
    #[test]
    fn a_stored_key_against_openrouters_messages_route_authenticates_with_a_bearer() {
        let openrouter = AnthropicMessagesClient::with_bases(
            "https://openrouter.ai/api/v1",
            "https://openrouter.ai/api/v1",
        )
        .unwrap()
        .with_messages_path("/messages")
        .with_stored_auth_style(StoredAuthStyle::Bearer);
        let stored = TurnCredential::Stored(Secret::api_key("sk-or-v1-ZZZZ").unwrap());
        let route = openrouter.route(&stored, "openrouter").unwrap();

        assert_eq!(
            route.headers[reqwest::header::AUTHORIZATION],
            "Bearer sk-or-v1-ZZZZ",
            "OpenRouter's /messages route authenticates only on `Authorization: \
             Bearer`, and it wants the scheme prefix; a bare key there answers \
             \"Missing Authentication header\" on every attempt -- F4"
        );
        assert!(route.headers[reqwest::header::AUTHORIZATION].is_sensitive());
        // The mirror negative, for the same reason the Anthropic test asserts no
        // bearer: a client that sent the key both ways passes a `contains` and
        // still leaks the key to a provider that had no business seeing it.
        assert!(
            route.headers.get("x-api-key").is_none(),
            "{:?}",
            route.headers
        );
        // The envelope is stamped on this provider too -- OpenRouter's Messages
        // route requires the version header exactly as the first-party one does.
        assert_eq!(route.headers["anthropic-version"], ANTHROPIC_VERSION);

        // CONTROL: the same origin, the same key, no style configured. Still
        // `x-api-key`, because the *definition* decides and the default is the
        // first-party convention -- so the assertions above are about the
        // configured style and not about this client having become a bearer
        // client for everyone.
        let default_style = AnthropicMessagesClient::with_bases(
            "https://openrouter.ai/api/v1",
            "https://openrouter.ai/api/v1",
        )
        .unwrap();
        let route = default_style.route(&stored, "openrouter").unwrap();
        assert_eq!(route.headers["x-api-key"], "sk-or-v1-ZZZZ");
        assert!(route.headers.get(reqwest::header::AUTHORIZATION).is_none());
    }

    /// The spellings a catalog file may name, pinned the way `WireProtocol`'s
    /// are.
    ///
    /// The config boundary refuses anything else, and it builds its refusal by
    /// listing [`StoredAuthStyle::ALL`] — so a style added here without a
    /// spelling an operator can write would produce a refusal naming a value
    /// no file could contain.
    #[test]
    fn the_auth_styles_are_spelled_the_way_a_config_file_would_write_them() {
        assert_eq!(StoredAuthStyle::default(), StoredAuthStyle::XApiKey);
        for (style, spelling) in [
            (StoredAuthStyle::XApiKey, "x_api_key"),
            (StoredAuthStyle::Bearer, "bearer"),
        ] {
            assert_eq!(style.wire_name(), spelling);
            assert_eq!(StoredAuthStyle::from_wire_name(spelling), Some(style));
        }
        // A spelling nothing implements is `None` rather than the default: the
        // boundary turns that into a refusal naming the field, where a silent
        // fallback would send an OpenRouter key out in a header that provider
        // ignores and report it as a 401 forever.
        assert_eq!(StoredAuthStyle::from_wire_name("x-api-key"), None);
        assert_eq!(StoredAuthStyle::from_wire_name("Bearer"), None);
        assert_eq!(StoredAuthStyle::ALL.len(), 2);
    }

    #[test]
    fn a_provider_definition_moves_the_route_and_cannot_displace_the_credential() {
        let client = AnthropicMessagesClient::with_bases(
            "https://gateway.test/anthropic/",
            "https://gateway.test/anthropic/",
        )
        .unwrap()
        .with_messages_path("/messages")
        .with_extra_headers([
            ("X-Gateway-Tenant".to_string(), "roundhouse".to_string()),
            // PROBE: a definition that tries to supply its own credential and
            // its own envelope. Neither may win — the first would let a file
            // that is not the credential file decide whose money a turn spends,
            // and the second would describe a body this client did not build.
            ("x-api-key".to_string(), "sk-ant-not-ours".to_string()),
            ("anthropic-version".to_string(), "1999-01-01".to_string()),
        ])
        .unwrap();
        assert_eq!(client.messages_path, "/messages");

        let stored = TurnCredential::Stored(Secret::api_key("sk-ant-api03-REAL").unwrap());
        let route = client.route(&stored, "anthropic").unwrap();
        assert_eq!(
            route.base, "https://gateway.test/anthropic",
            "slash trimmed"
        );
        assert_eq!(route.headers["x-gateway-tenant"], "roundhouse");
        assert_eq!(route.headers["x-api-key"], "sk-ant-api03-REAL");
        assert_eq!(route.headers["anthropic-version"], ANTHROPIC_VERSION);

        // A header the HTTP stack cannot carry is a boot-time refusal rather
        // than a request that silently goes out without it.
        assert!(
            AnthropicMessagesClient::new()
                .unwrap()
                .with_extra_headers([("not a header".to_string(), "x".to_string())])
                .is_err()
        );
    }

    #[test]
    fn a_forwarded_seat_rides_the_redirect_disabled_client_with_its_headers_verbatim() {
        // The allowlist row for `anthropic` lands with the edge work in this
        // same milestone, so this exercises the client's forwarding *mechanism*
        // through the one provider that has a row today. What is being asserted
        // is this file's half: which transport, which base, and that the
        // caller's headers survive the hop unchanged.
        let client = AnthropicMessagesClient::with_bases(
            "https://api.example.test",
            "https://seat.example.test",
        )
        .unwrap();

        let bearer = "Bearer eyJhbGciOiJub25lIn0.e30.seat-token";
        let forwarded = TurnCredential::Forwarded(
            PresentedCredential::captured(|name| match name {
                "authorization" => Some(bearer.to_string()),
                _ => None,
            })
            .unwrap()
            .for_provider("openai")
            .unwrap(),
        );
        let seat = client.route(&forwarded, "openai").unwrap();

        assert_eq!(seat.base, "https://seat.example.test");
        assert_eq!(seat.headers[reqwest::header::AUTHORIZATION], bearer);
        assert!(seat.headers[reqwest::header::AUTHORIZATION].is_sensitive());
        assert_eq!(seat.headers["anthropic-version"], ANTHROPIC_VERSION);
        assert!(matches!(seat.credential, TurnCredential::Forwarded(_)));

        // The two routes are different `reqwest::Client`s, so a forwarded
        // credential never shares a connection pool with the deployment's own
        // key. Redirects are refused on *both* since F1 — a stored `x-api-key`
        // is no more strippable by `reqwest` than a seat's bearer is — so pool
        // separation is now the whole of what two clients buy, and this is the
        // assertion that says so.
        let stored = TurnCredential::Stored(Secret::api_key("sk-ant-api03-ZZZZ").unwrap());
        let byok = client.route(&stored, "anthropic").unwrap();
        assert_eq!(byok.base, "https://api.example.test");
        assert!(
            !std::ptr::eq(byok.client, seat.client),
            "a forwarded seat must not share a connection pool with a stored key"
        );
    }
}
