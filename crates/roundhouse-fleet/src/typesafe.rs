// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The TypeSafe System One transport: a map of typed `choice` questions, one
//! answer each, in one request.
//!
//! Schema re-read from `docs.typesafe.ai/api`
//! on 2026-09-21: `POST {base}/systemone`, bearer auth, a body of `state`,
//! `model` and a `questions` map keyed by names the caller chooses, answering
//! under the same keys with `choice`, `probabilities` and `confidence`, plus a
//! top-level `usage` of `input_tokens` and `output_tokens`. The evidence is
//! `agent-docs/research/typesafe-jev-primary-read.md`; the design this serves is
//! `agent-docs/PLAN-routing-strategy-bandit.md` B4.
//!
//! A question map lets several classifications share one transmitted state and
//! one HTTP call. This does not guarantee identical answers, cost, or latency
//! compared with separate calls. Only `choice` has a caller here.
//!
//! Its one consumer is `roundhouse_server::typesafe_shadow`, which is what
//! decides whether a call may happen at all, and whose background runtime is
//! what makes one. **No scheduling, no strategy selection and no routing
//! decision lives here** — this half serializes a checked request, sends it
//! once, and validates what comes back.
//!
//! ## This is the low-level half, deliberately
//!
//! It takes a `state` string and sends it. It does **not** decide whether a
//! session may egress content to a third party, does not build the bounded
//! projection, and does not touch a budget. Those are the server module's, and
//! a transport that also decided admission would be a second place the egress
//! ruling has to be remembered.
//!
//! ## Three outcomes, not two
//!
//! A service call can end in a way the ordinary `Result` shape cannot say:
//! **the answers arrived, the accounting arrived, and the signal is unusable.**
//! Collapsing that into an error would book a real spend at zero, so the usage
//! sits *outside* the answers' `Result` — see [`SystemOneReply`]. Only a call
//! that produced no envelope at all yields a [`SystemOneError`], and that one
//! genuinely has unknown accounting.
//!
//! Batching does not change that and does not subdivide it. The service prices
//! and answers a batch as one call, so one bad answer among five leaves a
//! reported spend and no usable signal — the same middle outcome, reached with
//! more questions in it.
//!
//! ## What an error may carry
//!
//! Three things, none of which any arm carries: the credential, the prompt,
//! and the configured URL.
//!
//! [`SystemOneError::Status`] holds a status code and no body, because of the
//! prompt: a `422` from a validating service is the response most likely to
//! quote the request back, and no redaction distinguishes the service's words
//! from the transcript's inside one. The four documented codes — 401, 422, 429,
//! 529 — are diagnosable without it.
//!
//! [`SystemOneError::Transport`] strips the URL a `reqwest` error would
//! otherwise print. A base URL is deployment configuration and can carry a
//! tenant id or a gateway token in a query string, so "the URL is not a
//! credential" is an assumption about somebody else's configuration rather than
//! something this module can check.
//!
//! ## One shot
//!
//! The docs advise retrying 429 and 529 with backoff. This client does not.
//! A background classification has no routing effect, so a retry buys a later
//! answer to a question nobody is waiting on, at a second charge. Exactly-once
//! is the caller's, not a property a deterministic body could give this module:
//! a fresh attempt needs a fresh call identity and a fresh durable intent, both
//! of which live one layer up.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};

use roundhouse_core::control::{CredentialError, TurnCredential};

/// The published API root.
pub const DEFAULT_SYSTEM_ONE_BASE: &str = "https://api.typesafe.ai/v1";

/// Where the one endpoint lives under a base URL.
///
/// Not a default with alternatives — the service publishes exactly one
/// endpoint, so there is nothing to override and no setter for it.
pub const SYSTEM_ONE_PATH: &str = "/systemone";

/// The name this client asks the credential layer for a key under.
const PROVIDER: &str = "typesafe";

/// How far the probabilities may miss 1.0 and still be accepted.
///
/// `primitives/choice` states the distribution sums to 1.0 over every option,
/// so this is not slack for a documented rounding convention — it is slack for
/// float representation and for a service that rounds what it prints. What the
/// check is for is a *wrong-shaped* distribution, which misses by far more.
pub const PROBABILITY_SUM_TOLERANCE: f64 = 1e-3;

/// One `choice` question: what is asked, and the options it may be answered
/// with.
///
/// The key in [`SystemOneRequest::questions`] supplies its identity, so a second
/// key inside the question cannot disagree with it.
///
/// `criteria` is a [`BTreeMap`] so the serialized body is byte-identical for
/// identical inputs. Not a cache argument — this service publishes no prefix
/// cache — but so a recorded request can be compared against a replayed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceQuestion {
    pub instructions: String,
    /// Option name to its rubric description.
    pub criteria: BTreeMap<String, String>,
}

/// One call's body, before it is serialized.
#[derive(Clone, PartialEq, Eq)]
pub struct SystemOneRequest {
    /// The pinned model id. No default anywhere in this crate: `jev-latest`
    /// re-ranks itself underneath a deployment that never changed a line.
    pub model: String,
    /// What every question is asked about. One state, scored once per question.
    pub state: String,
    /// Question id to the question asked under it. Answers come back under the
    /// same ids.
    ///
    /// A [`BTreeMap`] for the ordering, but not for the *bytes*: this
    /// workspace pins `serde_json` with `preserve_order` off (see the note on
    /// the dependency in the root `Cargo.toml`), so a `Value` renders in sorted
    /// key order whatever map built it. What the ordering buys is the
    /// validation pass — questions are checked in one fixed order, so the fault
    /// reported for a batch with two bad answers in it is the same fault every
    /// time, and a replayed request diagnoses the way the recorded one did.
    ///
    /// An empty map is refused by [`SystemOneClient::prepare`] rather than
    /// sent.
    pub questions: BTreeMap<String, ChoiceQuestion>,
}

/// Elides `state`: a `Debug` that printed it would put a transcript into
/// whatever log caught a dropped request. The length survives, because "the
/// payload was 11 kB" is the fact a size bound is debugged with.
impl fmt::Debug for SystemOneRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemOneRequest")
            .field("model", &self.model)
            .field(
                "state",
                &format_args!("<{} bytes elided>", self.state.len()),
            )
            .field("questions", &self.questions)
            .finish()
    }
}

/// What the service says one call cost it.
///
/// Two axes and no cache term, because that is what this service reports. A
/// third field filled with a zero would be a pricing convention wearing the
/// name of a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemOneUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A validated `choice` answer.
#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceAnswer {
    /// The service's argmax, and always one of the offered options.
    pub choice: String,
    /// Every offered option mapped to its probability.
    pub probabilities: BTreeMap<String, f64>,
    /// The service's own statistic over the distribution, 0.0..=1.0.
    ///
    /// **An observation about the answer, never a measure of answer quality.**
    /// `confidence.md` calls it a statistic computed from the distribution and
    /// publishes neither the formula nor any calibration evidence
    /// (`typesafe-jev-primary-read.md` §2), so a caller may gate on it and may
    /// not report it as confidence in an outcome.
    pub confidence: f64,
}

/// What one call produced.
///
/// The usage sits beside the answers rather than inside them so that a reported
/// spend survives an unusable signal. `None` is unknown accounting — a service
/// that answered and said nothing priceable about cost — and never a free call.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemOneReply {
    pub usage: Option<SystemOneUsage>,
    /// The model reported by the service, which can differ from the requested
    /// identity. Missing or malformed metadata stays unknown; it does not
    /// discard valid answers or usage.
    pub reported_model: Option<String>,
    /// Every question's answer, or the first reason the batch is unusable.
    ///
    /// A partial set supplies no classification. Usage remains independent, so
    /// an unusable answer set does not discard reported spend.
    pub answers: Result<BTreeMap<String, ChoiceAnswer>, SignalError>,
}

/// A batch of answers that arrived and cannot be used.
///
/// Unit variants keep untrusted response keys and values out of diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignalError {
    #[error("no answer came back under a key some question was asked under")]
    MissingAnswer,
    /// Answer ids must match the request. Unrelated envelope fields remain allowed.
    #[error("an answer came back under a key no question was asked under")]
    UnexpectedAnswer,
    #[error("the answer is not a `choice`")]
    NotAChoice,
    #[error("the probability map's options are not the options the question offered")]
    OptionsDisagree,
    #[error("a probability is not a finite number in 0..=1")]
    ProbabilityOutOfRange,
    #[error("the probabilities do not sum to 1 within tolerance")]
    SumIsNotOne,
    #[error("`choice` names no offered option")]
    ChoiceNotOffered,
    #[error("`confidence` is not a finite number in 0..=1")]
    ConfidenceOutOfRange,
}

/// A call that produced no envelope, and therefore no accounting.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SystemOneError {
    /// No key, or the wrong shape of one. Refused before a socket.
    #[error(transparent)]
    Credential(#[from] CredentialError),
    /// A caller's own forwarded seat, offered to a service it does not
    /// authenticate against.
    ///
    /// Refused rather than sent: a shadow evaluation is deployment work and
    /// spends the deployment's key, and forwarding a tenant's bearer to a third
    /// party is the egress failure this whole path is gated to prevent.
    #[error("a forwarded credential is not this service's to spend; refusing to send it")]
    ForwardedCredentialRefused,
    /// Refused before credentials or serialization because it requests no signal.
    #[error("a request must carry at least one question")]
    NoQuestions,
    /// Refused before a socket. `actual_bytes` is a length, never content.
    #[error(
        "the request body is {actual_bytes} bytes, over the configured {limit_bytes}-byte bound"
    )]
    RequestTooLarge {
        limit_bytes: usize,
        actual_bytes: usize,
    },
    #[error("the service could not be reached (timed out: {timed_out})")]
    Transport { message: String, timed_out: bool },
    /// The status, and deliberately not the body. See the module note.
    #[error("the service answered {status}")]
    Status { status: u16 },
    #[error("the response exceeded the configured {limit_bytes}-byte bound")]
    ResponseTooLarge { limit_bytes: usize },
    #[error("the response envelope did not parse")]
    Malformed,
    #[error("the call exceeded its configured deadline")]
    DeadlineExceeded,
}

/// The bounds every call is made under.
///
/// No [`Default`]: a deployment that did not choose them has not decided how
/// much of a third party's answer it is willing to buffer or wait for, and a
/// constant here would make that decision silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemOneLimits {
    /// The most serialized request body this client will send.
    pub max_request_bytes: usize,
    /// The most response body this client will buffer before refusing.
    pub max_response_bytes: usize,
    /// The whole call, connect to last byte.
    pub deadline_ms: u64,
}

/// One call, serialized and checked, holding the exact bytes that will be sent.
///
/// The body is readable ([`Self::body`]) because the one caller that builds one
/// has to *quote* it to a budget before spending against it, and a quote taken
/// from a re-serialization would be a number about a different string.
pub struct PreparedRequest {
    body: String,
    headers: HeaderMap,
    /// Kept so every answer is validated against the question it was asked
    /// under rather than against a set the caller supplies later.
    questions: BTreeMap<String, ChoiceQuestion>,
}

impl PreparedRequest {
    /// The exact JSON body that will be sent.
    pub fn body(&self) -> &str {
        &self.body
    }
}

/// Elides the body and keeps its length, for the same reason
/// [`SystemOneRequest`]'s `Debug` elides the state: the body *contains* the
/// state, and a dropped prepared call must not put a transcript in a log. The
/// headers are elided wholesale because one of them is the deployment's key.
impl fmt::Debug for PreparedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedRequest")
            .field("body", &format_args!("<{} bytes elided>", self.body.len()))
            .field("headers", &format_args!("<{} elided>", self.headers.len()))
            .field("questions", &self.questions)
            .finish()
    }
}

/// Executes one batch of `choice` questions against a System One upstream.
///
/// Concrete, and there is no trait beside it: the thing worth testing is what
/// arrives at a socket, and a trait introduced so a test could avoid one would
/// be a seam that exists for the test rather than for the design.
pub struct SystemOneClient {
    http: reqwest::Client,
    base: String,
    limits: SystemOneLimits,
}

impl SystemOneClient {
    /// A client against `base`, under `limits`.
    ///
    /// Fallible for the reason every client in this crate is: a process with no
    /// usable TLS backend should fail to start rather than fail one call.
    pub fn new(base: impl Into<String>, limits: SystemOneLimits) -> Result<Self, SystemOneError> {
        let http = reqwest::Client::builder()
            // Redirects disabled for the same reason the forwarding transports
            // disable them: a redirect could carry the deployment's key to
            // another origin.
            .redirect(reqwest::redirect::Policy::none())
            // Connect only. A client-level *total* timeout set to the same
            // deadline raced `ask`'s own and won, so the documented
            // `DeadlineExceeded` arrived as a transport error instead and a
            // caller reading the failure reason got a different answer
            // depending on which phase was slow.
            .connect_timeout(Duration::from_millis(limits.deadline_ms))
            .build()
            .map_err(|source| SystemOneError::Transport {
                message: format!("could not build an HTTP client: {source}"),
                timed_out: false,
            })?;
        let base = base.into();
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            http,
            limits,
        })
    }

    /// The request body, as JSON.
    ///
    /// One `state` for however many questions, which is the saving: the state
    /// is most of the body, and it is sent once.
    fn body(request: &SystemOneRequest) -> Value {
        let questions: serde_json::Map<String, Value> = request
            .questions
            .iter()
            .map(|(key, question)| {
                (
                    key.clone(),
                    json!({
                        "type": "choice",
                        "instructions": question.instructions,
                        "criteria": question.criteria,
                    }),
                )
            })
            .collect();
        json!({
            "model": request.model,
            "state": request.state,
            "questions": questions,
        })
    }

    /// Serialize one call and apply everything this client can refuse without
    /// a socket: the credential it authenticates with, and the size of the body
    /// it would send.
    ///
    /// **Serialized once, and the caller keeps the bytes.** The boundary above
    /// needs the same body three times — to bound it, to quote it to a budget,
    /// and to send it — and three serializations are three chances for the
    /// thing measured to differ from the thing sent. A hold taken against one
    /// body while another goes on the wire is a budget that authorized spend it
    /// never approved.
    ///
    /// Split from [`Self::send`] for a second reason: that boundary reserves
    /// budget between the two, and a refusal taken afterwards opens and closes
    /// a hold for a call that was never eligible.
    pub fn prepare(
        &self,
        request: &SystemOneRequest,
        credential: &TurnCredential,
    ) -> Result<PreparedRequest, SystemOneError> {
        // First, ahead of both the credential and the size bound: a request
        // with nothing to ask is not one this deployment could pay for or
        // shrink into range, and all three refusals reach the policy boundary
        // as one arm where the variant is the whole diagnosis.
        if request.questions.is_empty() {
            return Err(SystemOneError::NoQuestions);
        }
        let headers = Self::headers(credential)?;
        // `expect` and not a `?`: `Self::body` builds its `Value` from owned
        // `String`s and a fixed shape, so the only way `to_string` fails is a
        // serde bug. `SystemOneError::Malformed` already means "the response
        // envelope did not parse" -- routing a request-side failure through it
        // would put two meanings on one arm.
        let body =
            serde_json::to_string(&Self::body(request)).expect("a serde_json::Value serializes");
        if body.len() > self.limits.max_request_bytes {
            return Err(SystemOneError::RequestTooLarge {
                limit_bytes: self.limits.max_request_bytes,
                actual_bytes: body.len(),
            });
        }
        Ok(PreparedRequest {
            body,
            headers,
            questions: request.questions.clone(),
        })
    }

    /// Send one prepared call, once.
    pub async fn send(&self, prepared: PreparedRequest) -> Result<SystemOneReply, SystemOneError> {
        let PreparedRequest {
            body,
            headers,
            questions,
        } = prepared;
        // Read before the headers move onto the request: the reply's reported
        // identity is checked against the key this call was made with, and
        // this is the only scope that holds it.
        let sent_key = bearer_key(&headers);
        let url = format!("{}{}", self.base, SYSTEM_ONE_PATH);

        // One instant for the whole call rather than a fresh budget per phase,
        // so headers and body cannot each spend the deadline in turn.
        let until = tokio::time::Instant::now() + Duration::from_millis(self.limits.deadline_ms);
        let sent = tokio::time::timeout_at(
            until,
            self.http.post(&url).headers(headers).body(body).send(),
        )
        .await
        .map_err(|_| SystemOneError::DeadlineExceeded)?
        .map_err(|source| SystemOneError::Transport {
            // `timed_out` first: `without_url` consumes the error, and fields
            // evaluate in the order they are written.
            timed_out: source.is_timeout(),
            // A `reqwest` error's `Display` carries the URL, and a base URL is
            // deployment configuration that can hold a tenant id or a gateway
            // token in a query string. Stripped rather than trusted: the
            // module's rule is that an error carries no credential, and "the
            // URL is not a credential" is an assumption about somebody else's
            // configuration.
            message: source.without_url().to_string(),
        })?;

        let status = sent.status();
        if !status.is_success() {
            return Err(SystemOneError::Status {
                status: status.as_u16(),
            });
        }

        let raw = tokio::time::timeout_at(until, self.drain(sent))
            .await
            .map_err(|_| SystemOneError::DeadlineExceeded)??;
        let mut reply = Self::reply(&raw, &questions)?;
        // A service that reflects request metadata -- a bug, or a hostile
        // answer -- can name this deployment's own key as the model that
        // served the call, and the caller writes that field into a durable
        // log. `Debug` elision and `without_url` close the two ways a key
        // reaches a log from the request side; this closes the reply side.
        // Substring rather than equality, so a `Bearer `-prefixed echo is
        // caught by the same check as a bare one.
        if let (Some(key), Some(model)) = (&sent_key, &reply.reported_model)
            && model.contains(key.as_str())
        {
            reply.reported_model = None;
        }
        Ok(reply)
    }

    /// Buffer the body, refusing past the configured bound.
    async fn drain(&self, response: reqwest::Response) -> Result<Vec<u8>, SystemOneError> {
        let mut buffered = Vec::new();
        let mut pieces = response.bytes_stream();
        while let Some(piece) = pieces.next().await {
            let piece = piece.map_err(|source| SystemOneError::Transport {
                timed_out: source.is_timeout(),
                // Stripped for the same reason as the send path above.
                message: source.without_url().to_string(),
            })?;
            if buffered.len() + piece.len() > self.limits.max_response_bytes {
                return Err(SystemOneError::ResponseTooLarge {
                    limit_bytes: self.limits.max_response_bytes,
                });
            }
            buffered.extend_from_slice(&piece);
        }
        Ok(buffered)
    }

    /// The headers this credential implies.
    fn headers(credential: &TurnCredential) -> Result<HeaderMap, SystemOneError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        match credential {
            // Through `require_api_key` rather than by matching the secret out:
            // that is the one seam that yields plaintext, and routing every read
            // through it is what makes a grep for it complete.
            TurnCredential::Stored(_) => {
                let key = credential.require_api_key(PROVIDER)?;
                // Marked sensitive so a `HeaderMap` that reaches a log or a
                // span prints `Sensitive` instead of the deployment's key --
                // the same treatment the two sibling transports give theirs.
                let mut value = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| {
                    SystemOneError::Transport {
                        message: "the resolved key cannot be put in a header value".to_string(),
                        timed_out: false,
                    }
                })?;
                value.set_sensitive(true);
                headers.insert(AUTHORIZATION, value);
                Ok(headers)
            }
            TurnCredential::Forwarded(_) => Err(SystemOneError::ForwardedCredentialRefused),
            TurnCredential::Absent => Err(credential
                .require_api_key(PROVIDER)
                .expect_err("Absent never yields a key")
                .into()),
        }
    }

    /// The envelope, split into its accounting, its identity and its signal.
    ///
    /// The usage is read *first* and independently of the answers, which is the
    /// whole of "a reported spend survives an unusable signal".
    fn reply(
        raw: &[u8],
        questions: &BTreeMap<String, ChoiceQuestion>,
    ) -> Result<SystemOneReply, SystemOneError> {
        let envelope: Envelope =
            serde_json::from_slice(raw).map_err(|_| SystemOneError::Malformed)?;
        // Read through a `Value` and discarded on any mismatch, so a usage
        // object missing an axis becomes *unknown* rather than a zero on that
        // axis -- a zero would book a billed call as free. Held as `Option<Value>`
        // on the envelope rather than a strict `Option<WireUsage>` for the other
        // half: a malformed accounting block must not fail the whole envelope
        // and throw away an answer that did arrive.
        let usage = envelope
            .usage
            .and_then(|usage| serde_json::from_value::<WireUsage>(usage).ok())
            .map(|usage| SystemOneUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
            });
        // Read the same lenient way, and for the same reason: the identity is
        // metadata about the call, so a `model` of the wrong JSON shape must
        // not fail the envelope and take the answers and the accounting with
        // it. A non-string is no identity, not a malformed reply.
        let reported_model = match envelope.model {
            Some(Value::String(model)) => Some(model),
            _ => None,
        };
        Ok(SystemOneReply {
            usage,
            reported_model,
            answers: Self::signal(&envelope.answers, questions),
        })
    }

    /// Validate every answer against the question it was asked under.
    ///
    /// **All or nothing.** The first fault ends the batch, so a caller never
    /// sees a partial answer set it would have to decide the sufficiency of.
    fn signal(
        answers: &BTreeMap<String, Value>,
        questions: &BTreeMap<String, ChoiceQuestion>,
    ) -> Result<BTreeMap<String, ChoiceAnswer>, SignalError> {
        // The id set first, for the same reason [`Self::answer`] checks a
        // question's options before its distribution: a reply whose ids are not
        // this request's ids is not this request's reply, and reporting a bad
        // distribution inside one would name the wrong fault.
        //
        // Missing leads deliberately, so a reply that is wrong in both
        // directions reports the direction whose id roundhouse chose itself.
        if questions.keys().any(|key| !answers.contains_key(key)) {
            return Err(SignalError::MissingAnswer);
        }
        if answers.keys().any(|key| !questions.contains_key(key)) {
            return Err(SignalError::UnexpectedAnswer);
        }
        questions
            .iter()
            .map(|(key, question)| {
                // Present: the id check above established it for every key
                // here. Spelled as a lookup rather than an index because no
                // third party's reply should be able to panic a caller.
                let raw = answers.get(key).ok_or(SignalError::MissingAnswer)?;
                Self::answer(raw, question).map(|answer| (key.clone(), answer))
            })
            .collect()
    }

    /// Validate one answer against the question that was asked.
    fn answer(raw: &Value, question: &ChoiceQuestion) -> Result<ChoiceAnswer, SignalError> {
        let wire: WireChoice =
            serde_json::from_value(raw.clone()).map_err(|_| SignalError::NotAChoice)?;
        if wire.kind != "choice" {
            return Err(SignalError::NotAChoice);
        }
        // The options first: an answer over options we never offered is a
        // different question's answer, and reporting it as a bad `choice` would
        // name the wrong thing. Everything after it is about *this* question's
        // distribution.
        if wire.probabilities.len() != question.criteria.len()
            || !wire
                .probabilities
                .keys()
                .all(|option| question.criteria.contains_key(option))
        {
            return Err(SignalError::OptionsDisagree);
        }
        // `is_finite` is unreachable from JSON, which carries no NaN or
        // infinity literal, and kept because this predicate is what the range
        // means rather than what one encoding can express.
        if wire
            .probabilities
            .values()
            .any(|p| !p.is_finite() || !(0.0..=1.0).contains(p))
        {
            return Err(SignalError::ProbabilityOutOfRange);
        }
        let sum: f64 = wire.probabilities.values().sum();
        if (sum - 1.0).abs() > PROBABILITY_SUM_TOLERANCE {
            return Err(SignalError::SumIsNotOne);
        }
        if !question.criteria.contains_key(&wire.choice) {
            return Err(SignalError::ChoiceNotOffered);
        }
        if !wire.confidence.is_finite() || !(0.0..=1.0).contains(&wire.confidence) {
            return Err(SignalError::ConfidenceOutOfRange);
        }
        Ok(ChoiceAnswer {
            choice: wire.choice,
            probabilities: wire.probabilities,
            confidence: wire.confidence,
        })
    }
}

/// The bare key out of an `Authorization: Bearer …` header.
///
/// The send path needs this temporary copy to suppress credential echoes in
/// reported metadata. Empty keys are excluded because every string contains
/// the empty string.
fn bearer_key(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let key = value.trim_start_matches("Bearer ").trim();
    match key.is_empty() {
        true => None,
        false => Some(key.to_string()),
    }
}

/// The response envelope. Unknown fields are ignored, which is `serde`'s
/// default and is load-bearing: the service is free to add a field and a client
/// that refused one would break on a deployment nobody touched.
#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    answers: BTreeMap<String, Value>,
    #[serde(default)]
    usage: Option<Value>,
    /// Held as a `Value` for the reason `usage` is: a wrong-shaped one is an
    /// absent identity, and never a reason to discard the reply it came on.
    #[serde(default)]
    model: Option<Value>,
}

/// Both axes required: a default on either turns silence about one into a
/// reported zero, and this service's whole accounting is these two numbers.
#[derive(Debug, Deserialize)]
struct WireUsage {
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    #[serde(rename = "type")]
    kind: String,
    choice: String,
    probabilities: BTreeMap<String, f64>,
    confidence: f64,
}

#[cfg(test)]
mod tests;
