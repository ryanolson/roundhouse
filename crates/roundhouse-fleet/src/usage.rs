// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Making a provider account for itself.
//!
//! Every number on the metrics dashboard descends from what a provider reports
//! about a call, and the default behaviour of the most common wire protocol is
//! to report nothing. An OpenAI-compatible **streaming** Chat Completions
//! request omits the `usage` object entirely unless the request opted in with
//! `stream_options.include_usage`; that is true of the OpenAI API itself and of
//! every server that implements the spec faithfully, Dynamo's own frontend
//! included. So a turn forwarded verbatim from a client that never asked for
//! usage comes back with no accounting at all.
//!
//! The failure that causes is worse than an error, which is why this module
//! exists rather than a comment somewhere. Missing usage folds into a rollup as
//! *zero tokens and zero dollars*, and zero dollars on a frontier target is
//! indistinguishable from a saving. The dashboard would report its best numbers
//! exactly when its instrumentation was broken.
//!
//! Two defences, and both are needed:
//!
//! 1. **Rewrite the request on the way out** so the provider reports at all.
//!    That is [`WireProtocol::enforce_usage_reporting`], and it is mandatory
//!    for any [`FrontierClient`](crate::FrontierClient) speaking these
//!    protocols.
//! 2. **Record when it did not work anyway**, because a proxy, a gateway, or an
//!    older self-hosted server can still answer without usage. That is
//!    [`Accounting`](roundhouse_core::event::Accounting) on the usage record:
//!    an unreported call is marked, never silently zeroed.
//!
//! Rewriting a client's request is a real liberty to take with it, so the scope
//! is deliberately narrow: these edits only ever *add* accounting, and they
//! refuse to touch a field the caller already set. Nothing here changes what the
//! model is asked to do or what the client gets back.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// The request/response dialect a target speaks.
///
/// Not the same axis as provider identity: Anthropic models served through
/// Bedrock speak [`WireProtocol::AnthropicMessages`], and a locally served
/// Llama behind Dynamo's frontend speaks [`WireProtocol::OpenAiChatCompletions`]
/// exactly as OpenAI's own endpoint does. What has to be rewritten follows the
/// dialect, not the company.
/// Wire names are pinned explicitly rather than derived. `rename_all` would
/// split `OpenAi` into `open_ai`, and a config format that spells the vendor's
/// name wrong is one every operator gets wrong once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireProtocol {
    /// OpenAI Chat Completions, and everything that clones it — vLLM, SGLang,
    /// Dynamo's frontend, most gateways.
    ///
    /// The one that actually bites: under `stream: true` the server sends no
    /// `usage` object at all unless `stream_options.include_usage` is set, and
    /// then sends it on a final chunk whose `choices` array is empty. A reader
    /// that stops at the first empty-`choices` chunk therefore throws away the
    /// only accounting it asked for.
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,

    /// OpenAI Responses.
    ///
    /// Reports usage on `response.completed` without being asked, so there is
    /// nothing to opt into. `prompt_cache_key` still has to be sent — not for
    /// accounting but because it is what steers the request to a cache node,
    /// and `input_tokens_details.cached_tokens` can only report a hit that
    /// actually happened.
    #[serde(rename = "openai_responses")]
    OpenAiResponses,

    /// Anthropic Messages.
    ///
    /// Also unconditional, but *split*: input, cache-read and cache-write
    /// counts arrive on `message_start`, output tokens on the final
    /// `message_delta`. A client that reads only one of the two records half a
    /// call — and reading only `message_delta`, the natural choice, is the half
    /// that reports zero input tokens and no cache reads, which is precisely
    /// the quantity this system exists to maximize.
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
}

impl WireProtocol {
    /// Add whatever this dialect needs in order to report usage.
    ///
    /// Returns the fields that were added, for logging: a deployment that finds
    /// this list non-empty on every call is one whose clients never ask for
    /// accounting, and it is worth knowing that the numbers rest on a rewrite.
    ///
    /// A field the caller already set is left exactly as it is, including when
    /// it is set to something unhelpful. Overriding an explicit `false` would
    /// mean a client could not turn this off even deliberately, and silently
    /// disagreeing with a request is a worse failure than an unaccounted call —
    /// the unaccounted call is at least *marked* as one downstream.
    pub fn enforce_usage_reporting(&self, body: &mut Value) -> Vec<&'static str> {
        let Some(object) = body.as_object_mut() else {
            return Vec::new();
        };
        match self {
            WireProtocol::OpenAiChatCompletions => enforce_openai_stream_options(object),
            // Nothing to add: both report usage unconditionally. Present as
            // explicit arms rather than a catch-all so that adding a dialect is
            // a compile error here rather than a silent no-op that shows up as
            // an accounting gap weeks later.
            WireProtocol::OpenAiResponses | WireProtocol::AnthropicMessages => Vec::new(),
        }
    }

    /// How a configuration file spells this dialect.
    ///
    /// The same string `serde` writes, pinned by
    /// [`protocol_names_are_what_a_config_file_would_write`](wire_names) so the
    /// two cannot drift. It exists because an error message that has to name a
    /// dialect should name it the way the operator wrote it — `{:?}` would say
    /// `OpenAiResponses`, which appears in no file anyone can go and edit.
    pub fn wire_name(&self) -> &'static str {
        match self {
            WireProtocol::OpenAiChatCompletions => "openai_chat_completions",
            WireProtocol::OpenAiResponses => "openai_responses",
            WireProtocol::AnthropicMessages => "anthropic_messages",
        }
    }

    /// Whether a call on this dialect can report usage before the stream ends.
    ///
    /// [`WireProtocol::AnthropicMessages`] can, on `message_start`, which is
    /// why a client for it must fold both events rather than only the last.
    pub fn reports_usage_before_completion(&self) -> bool {
        matches!(self, WireProtocol::AnthropicMessages)
    }
}

/// `stream_options: { include_usage: true }`, added only when the request
/// streams and only when the caller has not spoken for itself.
///
/// Gated on `stream` because the non-streaming form reports usage on the
/// response body already, and some stricter servers reject `stream_options` on
/// a non-streaming request outright — turning an accounting improvement into a
/// failed turn.
fn enforce_openai_stream_options(object: &mut Map<String, Value>) -> Vec<&'static str> {
    if object.get("stream").and_then(Value::as_bool) != Some(true) {
        return Vec::new();
    }
    match object.get_mut("stream_options") {
        Some(Value::Object(options)) => {
            if options.contains_key("include_usage") {
                return Vec::new();
            }
            options.insert("include_usage".into(), Value::Bool(true));
            vec!["stream_options.include_usage"]
        }
        // Present but not an object: a malformed request we do not try to
        // repair. Replacing it would change what the client sent in a way that
        // is not purely additive.
        Some(_) => Vec::new(),
        None => {
            object.insert("stream_options".into(), json!({ "include_usage": true }));
            vec!["stream_options"]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streaming_openai_request_gains_include_usage() {
        let mut body = json!({ "model": "gpt-x", "stream": true });
        let added = WireProtocol::OpenAiChatCompletions.enforce_usage_reporting(&mut body);

        assert_eq!(added, vec!["stream_options"]);
        assert_eq!(body["stream_options"]["include_usage"], json!(true));
    }

    #[test]
    fn an_existing_stream_options_object_is_extended_not_replaced() {
        let mut body = json!({
            "model": "gpt-x",
            "stream": true,
            "stream_options": { "some_other_flag": 1 },
        });
        WireProtocol::OpenAiChatCompletions.enforce_usage_reporting(&mut body);

        assert_eq!(body["stream_options"]["include_usage"], json!(true));
        assert_eq!(body["stream_options"]["some_other_flag"], json!(1));
    }

    #[test]
    fn an_explicit_choice_by_the_caller_is_never_overridden() {
        let mut body = json!({
            "model": "gpt-x",
            "stream": true,
            "stream_options": { "include_usage": false },
        });
        let added = WireProtocol::OpenAiChatCompletions.enforce_usage_reporting(&mut body);

        assert!(added.is_empty());
        assert_eq!(
            body["stream_options"]["include_usage"],
            json!(false),
            "silently disagreeing with the request is worse than an unaccounted call"
        );
    }

    #[test]
    fn a_non_streaming_request_is_left_alone() {
        // The body already carries usage, and some servers reject the field.
        let mut body = json!({ "model": "gpt-x" });
        let added = WireProtocol::OpenAiChatCompletions.enforce_usage_reporting(&mut body);

        assert!(added.is_empty());
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn dialects_that_always_report_need_no_rewrite() {
        for protocol in [
            WireProtocol::OpenAiResponses,
            WireProtocol::AnthropicMessages,
        ] {
            let mut body = json!({ "stream": true });
            assert!(protocol.enforce_usage_reporting(&mut body).is_empty());
            assert_eq!(
                body,
                json!({ "stream": true }),
                "{protocol:?} was rewritten"
            );
        }
    }

    #[test]
    fn only_anthropic_reports_before_the_stream_ends() {
        assert!(WireProtocol::AnthropicMessages.reports_usage_before_completion());
        assert!(!WireProtocol::OpenAiChatCompletions.reports_usage_before_completion());
        assert!(!WireProtocol::OpenAiResponses.reports_usage_before_completion());
    }
}

#[cfg(test)]
mod wire_names {
    use super::*;

    /// The config format spells the vendor's name the way the vendor does.
    ///
    /// Pinned by a test because the failure mode is silent for a reader and
    /// loud for an operator: a derived `rename_all` produces `open_ai_*`, which
    /// only shows up as a parse error in someone's deployment.
    #[test]
    fn protocol_names_are_what_a_config_file_would_write() {
        for (protocol, expected) in [
            (
                WireProtocol::OpenAiChatCompletions,
                "openai_chat_completions",
            ),
            (WireProtocol::OpenAiResponses, "openai_responses"),
            (WireProtocol::AnthropicMessages, "anthropic_messages"),
        ] {
            let json = serde_json::to_string(&protocol).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
            assert_eq!(
                serde_json::from_str::<WireProtocol>(&json).unwrap(),
                protocol
            );
            assert_eq!(
                protocol.wire_name(),
                expected,
                "an error naming a dialect must name it the way the file spells \
                 it, or it points an operator at a word that is in no file"
            );
        }
    }
}
