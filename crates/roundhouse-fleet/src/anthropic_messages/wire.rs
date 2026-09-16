// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Anthropic Messages vocabulary, hand-written and pinned to a spec snapshot.
//!
//! **Why hand-written at all.** There is no official Anthropic Rust SDK, and no
//! community crate is adoptable for a shipped path: the best-shaped one is
//! client-direction only, the bidirectional ones carry 2025-era closed enums
//! that reject correct 2026 traffic, and one invents a usage field that does not
//! exist on the wire — which would mis-report the single counter this product is
//! judged on. Generating from the spec is no better: its
//! `additionalProperties: false` sits entirely on *request* schemas, so a
//! faithful generation would make the serve surface fail closed on the day a
//! client sends a field newer than our snapshot, and it still would not produce
//! the two SSE events the spec omits. (Ruling R1 of
//! `agent-docs/PLAN-anthropic-messages.md`; evidence
//! `agent-docs/research/anthropic-messages-wire-crates.md` §1, §3, §5.)
//!
//! **The polarity, everywhere: typed where roundhouse reads or originates, open
//! everywhere else.** Typed are the stream events, [`Usage`] with the full
//! `cache_creation` breakdown, [`StopReason`] as an *open* enum, the content
//! blocks that map onto conversation items, and [`CacheControl`] with the real
//! 5m/1h vocabulary. Open is everything else: every struct here carries a
//! `#[serde(flatten)]` extras map, unknown content blocks ride through
//! [`ContentBlock::Opaque`] verbatim, unknown deltas through
//! [`BlockDelta::Other`], and an eighth `stop_reason` becomes
//! [`StopReason::Other`] rather than a parse error. **There is no
//! `deny_unknown_fields` in this module and there must never be one** — these
//! types are shared with the serve surface, which is a pass-through, and
//! refusing an unknown field there is the pass-through-fatal condition.
//!
//! **Both directions on everything.** Every type derives `Serialize` as well as
//! `Deserialize` even though the dispatch client only ever deserializes: the
//! `/v1/messages` serve surface emits this same vocabulary, and two parallel
//! definitions of one wire format are two things that can disagree about what
//! Anthropic said.
//!
//! **Two events here are not in the spec, and that is a fact about the spec.**
//! `ping` and mid-stream `error` appear nowhere in the 2.4 MB OpenAPI document —
//! the literal string `"ping"` occurs zero times, and `MessageStreamEvent` is a
//! union of exactly six members — yet both are real on the wire and Claude Code
//! dispatches on both (evidence doc §3.1, client surface §3.2). They are typed
//! here and pinned by this module's own tests rather than by [`SPEC_PIN_JSON`],
//! so that their *absence* from a future spec diff is never read as a removal.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// Fields this build does not name, carried verbatim.
///
/// A type alias rather than the bare `Map` at a dozen field declarations,
/// because the name is the argument: these are not "unknown" fields to be
/// tolerated, they are the forward-compatibility contract R1 rests on.
pub type Extra = Map<String, Value>;

/// The recorded spec pin: URL, our own body sha256, the opaque upstream content
/// address, the SDK revision that named it, the fetch date, and the vocabulary.
///
/// Read by this module's pinning tests and by the client's body test. It is also
/// the file the `anthropic-spec-sync` skill rewrites: its shape is the contract
/// between that loop and these tests, so a field is added to it rather than
/// renamed.
pub const SPEC_PIN_JSON: &str = include_str!("spec_pin.json");

// ---------------------------------------------------------------------------
// The pinned vocabulary, as constants this module's own code and tests use.
//
// These are not documentation. Each one is compared against `spec_pin.json` by a
// test below, so an upstream rename turns the suite red with the old and new
// spelling both named in the failure.
// ---------------------------------------------------------------------------

/// The six stream events the spec's `MessageStreamEvent` union names.
pub const SPEC_STREAM_EVENTS: [&str; 6] = [
    "message_start",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "message_delta",
    "message_stop",
];

/// The two events that are real on the wire and absent from the spec.
///
/// Kept separate from [`SPEC_STREAM_EVENTS`] rather than merged, because the
/// pinning test compares that list against the spec snapshot: merging them would
/// make the test fail for the one reason that is not an upstream move.
pub const TRANSPORT_STREAM_EVENTS: [&str; 2] = ["ping", "error"];

/// The four `content_block_delta` payload types the spec names.
pub const SPEC_BLOCK_DELTAS: [&str; 4] = [
    "text_delta",
    "input_json_delta",
    "thinking_delta",
    "signature_delta",
];

/// The seven `stop_reason` values the spec names, in the spelling it uses.
///
/// An eighth is not an error — see [`StopReason::Other`]. This list exists so
/// that an upstream *rename* of one of the seven is loud, which is the failure a
/// closed enum hides by turning every unrecognised value into the same parse
/// error.
pub const SPEC_STOP_REASONS: [&str; 7] = [
    "end_turn",
    "max_tokens",
    "stop_sequence",
    "tool_use",
    "pause_turn",
    "refusal",
    "model_context_window_exceeded",
];

/// The `usage` properties this module reads by name.
///
/// A strict subset of the spec's nine: `inference_geo`, `output_tokens_details`,
/// `server_tool_use` and `service_tier` are carried in [`Usage::extra`] and read
/// by nothing here. Naming the subset rather than the whole set is deliberate —
/// this is the list whose *renaming* would silently zero a counter.
pub const USAGE_PROPERTIES_READ: [&str; 5] = [
    "input_tokens",
    "output_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
    "cache_creation",
];

/// The `cache_creation` breakdown's field names.
///
/// Roundhouse is the first Rust implementation to model these at all: the crate
/// survey found none, and the one crate that tried invented
/// `cache_creation_input_tokens_1h`, which does not exist (evidence doc §5.4).
pub const SPEC_CACHE_CREATION_FIELDS: [&str; 2] =
    ["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"];

/// The `cache_control.ttl` vocabulary.
pub const SPEC_CACHE_CONTROL_TTLS: [&str; 2] = [CACHE_TTL_5M, CACHE_TTL_1H];

/// The response content blocks this module gives a typed arm, as
/// `(spec schema name, wire `type` value)`.
///
/// Four of the spec's twelve. The other eight ride through
/// [`ContentBlock::Opaque`] byte-for-byte, which is what lets a resent history
/// containing a server-tool result round-trip prefix admission unchanged.
pub const TYPED_RESPONSE_CONTENT_BLOCKS: [(&str, &str); 4] = [
    ("ResponseTextBlock", "text"),
    ("ResponseThinkingBlock", "thinking"),
    ("ResponseRedactedThinkingBlock", "redacted_thinking"),
    ("ResponseToolUseBlock", "tool_use"),
];

/// The eight response content blocks [`ContentBlock::Opaque`] carries.
///
/// Written out rather than derived as "the rest", because the point of the test
/// that reads it is that the *count* is twelve: a thirteenth block type upstream
/// has to be classified by a person into this list or the typed one, and a
/// derived complement would classify it silently.
pub const OPAQUE_RESPONSE_CONTENT_BLOCKS: [&str; 8] = [
    "ResponseServerToolUseBlock",
    "ResponseWebSearchToolResultBlock",
    "ResponseWebFetchToolResultBlock",
    "ResponseCodeExecutionToolResultBlock",
    "ResponseBashCodeExecutionToolResultBlock",
    "ResponseTextEditorCodeExecutionToolResultBlock",
    "ResponseToolSearchToolResultBlock",
    "ResponseContainerUploadBlock",
];

/// The only `cache_control` type the API has ever had.
pub const CACHE_CONTROL_EPHEMERAL: &str = "ephemeral";
/// The default cache lifetime, and what an omitted `ttl` means.
pub const CACHE_TTL_5M: &str = "5m";
/// The extended cache lifetime, gated behind `extended-cache-ttl-2025-04-11`.
pub const CACHE_TTL_1H: &str = "1h";

/// The `type` a [`Message`] carries.
pub const MESSAGE_TYPE: &str = "message";

// ---------------------------------------------------------------------------
// Stream events
// ---------------------------------------------------------------------------

/// One frame of a Messages SSE stream.
///
/// **No catch-all arm, deliberately, and the openness lives one layer up.** The
/// decoder dispatches on the SSE `event:` name and skips a name it does not
/// know *before* it ever asks serde to parse the payload, so a ninth event type
/// never reaches this enum. Putting a catch-all here as well would make the same
/// forward-compatibility promise twice and let the two drift; putting it *only*
/// here would mean parsing every unknown frame in order to throw it away.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// The prelude: a [`Message`] with empty `content`, carrying the input-side
    /// usage. Half of this dialect's split accounting.
    MessageStart {
        message: Message,
        #[serde(flatten)]
        extra: Extra,
    },
    ContentBlockStart {
        index: u64,
        content_block: ContentBlock,
        #[serde(flatten)]
        extra: Extra,
    },
    ContentBlockDelta {
        index: u64,
        delta: BlockDelta,
        #[serde(flatten)]
        extra: Extra,
    },
    ContentBlockStop {
        index: u64,
        #[serde(flatten)]
        extra: Extra,
    },
    /// The terminal metadata: why generation stopped, and the *cumulative*
    /// output count. The other half of the split accounting.
    MessageDelta {
        delta: MessageDeltaBody,
        /// `Option` and not a defaulted [`Usage`], because "the frame reported
        /// no counts" and "the frame reported zero" are different facts and only
        /// the second may overwrite what `message_start` said.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(flatten)]
        extra: Extra,
    },
    MessageStop {
        #[serde(flatten)]
        extra: Extra,
    },
    /// A keepalive. Absent from the spec; real on the wire (see the module doc).
    Ping {
        #[serde(flatten)]
        extra: Extra,
    },
    /// A mid-stream failure. Absent from the spec; real on the wire.
    Error {
        error: ApiError,
        #[serde(flatten)]
        extra: Extra,
    },
}

/// The `Message` skeleton `message_start` carries, and the shape a non-streaming
/// response returns whole.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    #[serde(rename = "type", default = "message_type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Empty on `message_start`; the accumulated blocks on a whole response.
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(flatten)]
    pub extra: Extra,
}

fn message_type() -> String {
    MESSAGE_TYPE.to_string()
}

/// What a `message_delta` says about how the turn ended.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageDeltaBody {
    /// `Option` because the wire sends an explicit `null` here on every
    /// non-final delta, and because a value we have never seen must arrive as
    /// [`StopReason::Other`] rather than as a failed turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// The body of a mid-stream `error` event, and of an error response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    /// `overloaded_error`, `rate_limit_error`, `api_error`, … Open, because the
    /// set grows and because the one value that matters operationally
    /// (`overloaded_error`, the only mid-stream error Claude Code retries) is
    /// matched on as a string wherever it matters.
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub message: String,
    #[serde(flatten)]
    pub extra: Extra,
}

// ---------------------------------------------------------------------------
// Content blocks and deltas
// ---------------------------------------------------------------------------

/// One block of message content, in either direction.
///
/// [`Self::Opaque`] is the whole forward-compatibility story for this type: it
/// keeps the original object and re-serializes it byte-for-byte, so a history
/// containing a web-search result or a container upload survives a round trip
/// through roundhouse unchanged. A closed enum here is exactly the defect that
/// disqualified `claudius` — six of the spec's twelve response blocks missing,
/// each one a correct 2026 response it would refuse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        #[serde(default)]
        text: String,
        /// Present only on the request side; a response block never carries one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(flatten)]
        extra: Extra,
    },
    ToolUse {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        /// Arbitrary JSON by definition — it is the tool's own argument schema.
        #[serde(default)]
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(flatten)]
        extra: Extra,
    },
    /// Request-side only, and typed anyway: it is one of the two shapes an
    /// agentic loop cannot do without, and the serve surface has to map it onto
    /// [`ItemContent::ToolResult`](roundhouse_core::item::ItemContent).
    ToolResult {
        #[serde(default)]
        tool_use_id: String,
        /// A string or an array of blocks, per the API. Kept as JSON rather than
        /// normalised to one of them, because normalising would rewrite what a
        /// client sent.
        #[serde(default)]
        content: Value,
        #[serde(default)]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(flatten)]
        extra: Extra,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        /// The cryptographic signature that lets extended thinking be resent.
        /// Dropping it on a round trip invalidates the block upstream, which is
        /// why it is typed rather than left to `extra`.
        #[serde(default)]
        signature: String,
        #[serde(flatten)]
        extra: Extra,
    },
    RedactedThinking {
        #[serde(default)]
        data: String,
        #[serde(flatten)]
        extra: Extra,
    },
    /// Any block type this build does not name, kept verbatim.
    #[serde(untagged)]
    Opaque(Value),
}

impl ContentBlock {
    /// A plain text block, the only shape this client originates.
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text {
            text: text.into(),
            cache_control: None,
            extra: Extra::new(),
        }
    }

    /// The wire `type` of this block.
    ///
    /// `None` for an [`Self::Opaque`] whose object has no string `type` at all —
    /// which is not a block the API can have sent, but is a shape a hostile or
    /// broken upstream can, and this returning `None` is how a caller says so
    /// without a panic.
    pub fn block_type(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { .. } => Some("text"),
            ContentBlock::ToolUse { .. } => Some("tool_use"),
            ContentBlock::ToolResult { .. } => Some("tool_result"),
            ContentBlock::Thinking { .. } => Some("thinking"),
            ContentBlock::RedactedThinking { .. } => Some("redacted_thinking"),
            ContentBlock::Opaque(value) => value.get("type").and_then(Value::as_str),
        }
    }
}

/// One `content_block_delta` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockDelta {
    TextDelta {
        #[serde(default)]
        text: String,
        #[serde(flatten)]
        extra: Extra,
    },
    InputJsonDelta {
        /// A *fragment* of JSON, not JSON. Concatenating every fragment of one
        /// block yields the tool's arguments; parsing one alone does not.
        #[serde(default)]
        partial_json: String,
        #[serde(flatten)]
        extra: Extra,
    },
    ThinkingDelta {
        #[serde(default)]
        thinking: String,
        #[serde(flatten)]
        extra: Extra,
    },
    SignatureDelta {
        #[serde(default)]
        signature: String,
        #[serde(flatten)]
        extra: Extra,
    },
    /// A delta type this build does not name — `citations_delta` today, and
    /// whatever the next beta adds. Kept verbatim so the serve surface can relay
    /// it, and skipped by the dispatch decoder.
    #[serde(untagged)]
    Other(Value),
}

// ---------------------------------------------------------------------------
// Stop reasons
// ---------------------------------------------------------------------------

/// Why the model stopped.
///
/// **Open by construction, and this is the arm with the most cautionary
/// evidence behind it.** NeMo Relay's codec maps this to three values and drops
/// the rest; `claudius` closed the enum in 2025 and now rejects `pause_turn`,
/// `refusal` and `model_context_window_exceeded` — three values the spec names
/// today. An eighth value is not a hypothetical: two of the seven arrived after
/// the crates that closed the enum shipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    PauseTurn,
    Refusal,
    ModelContextWindowExceeded,
    /// A value newer than this build. Carried, never refused.
    #[serde(untagged)]
    Other(String),
}

impl StopReason {
    /// How the wire spells this reason.
    ///
    /// Pinned against the spec snapshot by a test, for the same reason
    /// [`WireProtocol::wire_name`](crate::usage::WireProtocol::wire_name) is: a
    /// derived `rename_all` is one refactor away from spelling a value the API
    /// has never sent, and the failure shows up as a silently unmatched arm
    /// rather than as a compile error.
    pub fn as_wire(&self) -> &str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::MaxTokens => "max_tokens",
            StopReason::StopSequence => "stop_sequence",
            StopReason::ToolUse => "tool_use",
            StopReason::PauseTurn => "pause_turn",
            StopReason::Refusal => "refusal",
            StopReason::ModelContextWindowExceeded => "model_context_window_exceeded",
            StopReason::Other(value) => value,
        }
    }
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

/// What a call cost, in Anthropic's own axes.
///
/// **These axes are not roundhouse's.** On this wire `input_tokens` *excludes*
/// both cache reads and cache writes, so the three counters are disjoint
/// addends; roundhouse's [`Usage`](roundhouse_core::event::Usage) is
/// OpenAI-shaped, where `input_tokens` is the total and the cached count is a
/// component of it. The conversion happens once, in the stream decoder, and the
/// comment there is the one that matters — this type deliberately keeps the
/// provider's own meaning so that the conversion has exactly one site.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, deserialize_with = "count")]
    pub input_tokens: u64,
    #[serde(default, deserialize_with = "count")]
    pub output_tokens: u64,
    /// Tokens written to the cache this call, at whatever TTL.
    #[serde(default, deserialize_with = "count")]
    pub cache_creation_input_tokens: u64,
    /// Tokens served from the cache. The quantity the whole system maximizes.
    #[serde(default, deserialize_with = "count")]
    pub cache_read_input_tokens: u64,
    /// The 5m/1h split of `cache_creation_input_tokens`.
    ///
    /// Typed here and carried nowhere downstream yet, deliberately: roundhouse's
    /// ledger has one cache-write rate, so the total is what pricing can use.
    /// The breakdown is parsed anyway because it is the field no other Rust
    /// implementation models, and because the day the ledger prices two TTLs
    /// separately, the data has to have been there all along rather than
    /// starting from the day someone noticed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Usage {
    /// Whether this object reported any input-side count at all.
    ///
    /// The discriminator between "the upstream told us nothing" and "the
    /// upstream told us the prompt was free". Zero input tokens on a call that
    /// carried a prompt is not a saving, it is a missing measurement — and the
    /// difference decides whether a `Done` is emitted at all.
    pub fn reported_any_input(&self) -> bool {
        self.input_tokens > 0
            || self.cache_read_input_tokens > 0
            || self.cache_creation_input_tokens > 0
    }
}

/// The 5m/1h breakdown of a cache write.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheCreation {
    #[serde(default, deserialize_with = "count")]
    pub ephemeral_5m_input_tokens: u64,
    #[serde(default, deserialize_with = "count")]
    pub ephemeral_1h_input_tokens: u64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A token count that tolerates an explicit `null`.
///
/// Anthropic's own client null-guards these fields before comparing them
/// (`input_tokens !== null && input_tokens > 0`, client surface §3.4), which is
/// the strongest available evidence that `null` reaches the wire on some
/// beta shapes. Reading it as zero rather than failing the parse is the open
/// direction, and it is the *safe* open direction here: an absent count already
/// reads as zero everywhere downstream, so this adds no new way to be wrong.
fn count<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    Ok(Option::<u64>::deserialize(deserializer)?.unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Cache control
// ---------------------------------------------------------------------------

/// A prompt-cache breakpoint.
///
/// **Anthropic caches nothing without one.** Unlike the Responses API, where a
/// `prompt_cache_key` steers a request at a node that caches automatically,
/// Anthropic's cache is opt-in per request and marked on a specific block: no
/// `cache_control`, no cache, no discount, on every turn. That is why this type
/// exists at all and why the client places one — see the placement site in
/// `super::body`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheControl {
    /// `ephemeral` is the only value the API has ever had. A `String` rather
    /// than a one-variant enum, because a second value would arrive as traffic
    /// before it arrives as a type here.
    #[serde(rename = "type")]
    pub kind: String,
    /// `5m` (the default) or `1h`. `None` means the field is omitted, which is
    /// *not* the same as sending `"5m"`: the extended-TTL beta must be enabled
    /// for the field to be accepted at all, so an omitted `ttl` is the only form
    /// that is valid without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl CacheControl {
    /// A breakpoint with no explicit TTL: five minutes, and valid without any
    /// beta header.
    pub fn ephemeral() -> Self {
        Self {
            kind: CACHE_CONTROL_EPHEMERAL.to_string(),
            ttl: None,
            extra: Extra::new(),
        }
    }

    /// A breakpoint at a named TTL. Requires `extended-cache-ttl-2025-04-11`
    /// upstream for anything but the default, which is why the caller has to ask
    /// for it by name rather than getting one from [`Self::ephemeral`].
    pub fn ephemeral_for(ttl: impl Into<String>) -> Self {
        Self {
            ttl: Some(ttl.into()),
            ..Self::ephemeral()
        }
    }
}

#[cfg(test)]
pub(crate) mod pin {
    //! The spec snapshot, parsed, for the tests in this module and in the
    //! client beside it.

    use serde_json::Value;

    /// The pin as JSON.
    ///
    /// `expect` rather than a `Result`: this file is compiled into the binary,
    /// so a parse failure is a broken build artefact and not a runtime
    /// condition anything could handle.
    pub(crate) fn spec_pin() -> Value {
        serde_json::from_str(super::SPEC_PIN_JSON).expect("the pinned spec vocabulary is JSON")
    }

    /// One string array out of the pin's `vocabulary` object.
    pub(crate) fn vocabulary(pin: &Value, key: &str) -> Vec<String> {
        strings(&pin["vocabulary"][key])
    }

    /// A JSON array of strings, as strings.
    ///
    /// Panics rather than returning an option: a pin whose shape does not match
    /// is a broken fixture, and every caller is a test that would otherwise
    /// report the wrong failure.
    pub(crate) fn strings(value: &Value) -> Vec<String> {
        value
            .as_array()
            .unwrap_or_else(|| panic!("expected an array of strings, found {value}"))
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .unwrap_or_else(|| panic!("expected a string, found {value}"))
                    .to_string()
            })
            .collect()
    }

    /// A sorted, de-duplicated copy, so a comparison is about membership rather
    /// than about the order two lists happen to be written in.
    pub(crate) fn set(values: impl IntoIterator<Item = String>) -> Vec<String> {
        let mut values: Vec<String> = values.into_iter().collect();
        values.sort();
        values.dedup();
        values
    }
}

#[cfg(test)]
mod tests {
    use super::pin::{set, spec_pin, vocabulary};
    use super::*;

    /// `MessageStartEvent` → `message_start`.
    ///
    /// A derivation rather than a second hand-written list: if this test simply
    /// compared two literals somebody typed, it would pass on the day one of
    /// them was updated to match a rename that the *code* had not followed.
    fn event_wire_name(schema: &str) -> String {
        snake_case(schema.strip_suffix("Event").unwrap_or(schema))
    }

    /// `TextContentBlockDelta` → `text_delta`.
    fn delta_wire_name(schema: &str) -> String {
        let stem = schema.strip_suffix("ContentBlockDelta").unwrap_or(schema);
        format!("{}_delta", snake_case(stem))
    }

    fn snake_case(camel: &str) -> String {
        let mut out = String::new();
        for (index, character) in camel.char_indices() {
            if character.is_ascii_uppercase() {
                if index > 0 {
                    out.push('_');
                }
                out.push(character.to_ascii_lowercase());
            } else {
                out.push(character);
            }
        }
        out
    }

    #[test]
    fn the_pin_identifies_the_snapshot_three_ways_and_conflates_none_of_them() {
        let pin = spec_pin();
        // Three identifiers, and the skill that refreshes this file exists
        // partly because they were conflated once: the 64-hex hash inside the
        // URL and `.stats.yml`'s 32-hex `openapi_spec_hash` are opaque Stainless
        // content addresses, and neither is the sha256 of the body. A pin that
        // recorded one of them as "the sha256" would make a broken download
        // indistinguishable from an upstream move.
        let url = pin["spec_url"].as_str().expect("the pin names a URL");
        let body_sha = pin["spec_sha256"].as_str().expect("our own body sha256");
        let upstream = pin["openapi_spec_hash"]
            .as_str()
            .expect("the upstream content address");
        assert_eq!(body_sha.len(), 64, "a sha256 is 64 hex characters");
        assert_eq!(upstream.len(), 32, "Stainless's address is 32 hex");
        assert!(
            !url.contains(body_sha),
            "the URL's embedded hash is not our body sha256; if it ever is, one \
             of the two was copied from the other"
        );
        assert!(!url.contains(upstream));
        assert!(
            pin["source_sdk_rev"]
                .as_str()
                .is_some_and(|rev| rev.len() == 40),
            "the pin names the anthropic-sdk-typescript revision whose \
             `.stats.yml` pointed at this spec, because that is the only way to \
             discover the *next* one"
        );
        assert!(pin["fetched"].as_str().is_some());
    }

    #[test]
    fn the_six_spec_stream_events_are_the_ones_this_module_types() {
        let pin = spec_pin();
        let from_spec = set(vocabulary(&pin, "message_stream_event_members")
            .iter()
            .map(|schema| event_wire_name(schema)));
        let ours = set(SPEC_STREAM_EVENTS.iter().map(|name| name.to_string()));
        assert_eq!(ours, from_spec);

        // Every one of them parses into its own arm, so the list above is a
        // claim about this module's code rather than about a constant.
        for name in SPEC_STREAM_EVENTS {
            let frame = minimal_event(name);
            let event: StreamEvent = serde_json::from_value(frame.clone())
                .unwrap_or_else(|error| panic!("`{name}` must parse: {error} in {frame}"));
            assert_eq!(
                serde_json::to_value(&event).unwrap()["type"],
                Value::String(name.to_string()),
                "`{name}` must round-trip to its own wire name"
            );
        }
    }

    /// **`ping` and `error` are real and the spec does not know them.**
    ///
    /// Pinned here rather than in `spec_pin.json` on purpose: the sync skill
    /// diffs that file against the spec, and a `ping` entry there would show up
    /// as a phantom removal on every single run. The evidence is
    /// `research/anthropic-messages-wire-crates.md` §3.1 — the literal string
    /// `"ping"` occurs zero times in the 2.4 MB document — and
    /// `research/claude-code-client-surface.md` §3.2, where the real client
    /// dispatches on both names.
    #[test]
    fn the_two_transport_events_the_spec_omits_are_typed_anyway() {
        let pin = spec_pin();
        let spec_events = vocabulary(&pin, "message_stream_event_members");
        for absent in TRANSPORT_STREAM_EVENTS {
            assert!(
                !spec_events
                    .iter()
                    .any(|schema| event_wire_name(schema) == absent),
                "`{absent}` has appeared in the spec's stream-event union; move \
                 it out of TRANSPORT_STREAM_EVENTS and into SPEC_STREAM_EVENTS"
            );
        }

        assert!(matches!(
            serde_json::from_value::<StreamEvent>(serde_json::json!({ "type": "ping" })).unwrap(),
            StreamEvent::Ping { .. }
        ));
        let error: StreamEvent = serde_json::from_value(serde_json::json!({
            "type": "error",
            "error": { "type": "overloaded_error", "message": "Overloaded" },
        }))
        .unwrap();
        let StreamEvent::Error { error, .. } = error else {
            panic!("an error frame must parse into the error arm")
        };
        // `overloaded_error` is load-bearing beyond diagnostics: it is the only
        // mid-stream error Claude Code will retry, so the serve surface has to
        // be able to spell it and the dispatch client has to be able to read it.
        assert_eq!(error.kind, "overloaded_error");
        assert_eq!(error.message, "Overloaded");
    }

    #[test]
    fn the_four_spec_delta_variants_are_the_ones_this_module_types() {
        let pin = spec_pin();
        let from_spec = set(vocabulary(&pin, "content_block_delta_variants")
            .iter()
            .map(|schema| delta_wire_name(schema)));
        assert_eq!(
            set(SPEC_BLOCK_DELTAS.iter().map(|name| name.to_string())),
            from_spec
        );

        for name in SPEC_BLOCK_DELTAS {
            let delta: BlockDelta =
                serde_json::from_value(serde_json::json!({ "type": name })).unwrap();
            assert!(
                !matches!(delta, BlockDelta::Other(_)),
                "`{name}` fell through to the catch-all, so a rename upstream \
                 would silently stop producing text"
            );
        }

        // The catch-all keeps what it could not name, rather than flattening it
        // to a marker: the serve surface relays these.
        let unknown = serde_json::json!({ "type": "citations_delta", "citation": { "x": 1 } });
        let delta: BlockDelta = serde_json::from_value(unknown.clone()).unwrap();
        assert_eq!(delta, BlockDelta::Other(unknown.clone()));
        assert_eq!(serde_json::to_value(&delta).unwrap(), unknown);
    }

    #[test]
    fn the_seven_spec_stop_reasons_round_trip_and_an_eighth_is_not_an_error() {
        let pin = spec_pin();
        assert_eq!(
            set(SPEC_STOP_REASONS.iter().map(|value| value.to_string())),
            set(vocabulary(&pin, "stop_reason")),
        );

        for spelling in SPEC_STOP_REASONS {
            let json = Value::String(spelling.to_string());
            let reason: StopReason = serde_json::from_value(json.clone()).unwrap();
            assert!(
                !matches!(reason, StopReason::Other(_)),
                "`{spelling}` must have its own arm; falling into `Other` is how \
                 a value the engine has to act on becomes a string nobody matches"
            );
            assert_eq!(reason.as_wire(), spelling);
            assert_eq!(serde_json::to_value(&reason).unwrap(), json);
        }

        // PROBE: the eighth value. `claudius` returns a parse error here and
        // therefore fails a correct turn; Relay maps it to a default and
        // therefore reports the wrong reason. Both are worse than carrying it.
        let future: StopReason = serde_json::from_value(Value::String("dreaming".into())).unwrap();
        assert_eq!(future, StopReason::Other("dreaming".into()));
        assert_eq!(future.as_wire(), "dreaming");
        assert_eq!(
            serde_json::to_value(&future).unwrap(),
            Value::String("dreaming".into()),
            "an unknown reason must go back out the way it came in, so a serve \
             surface relaying it does not invent a different ending"
        );
    }

    #[test]
    fn the_usage_properties_this_module_reads_are_spelled_the_way_the_spec_does() {
        let pin = spec_pin();
        let spec = vocabulary(&pin, "usage_properties");
        for property in USAGE_PROPERTIES_READ {
            assert!(
                spec.iter().any(|name| name == property),
                "`{property}` is not a property of the spec's Usage; a counter \
                 read by a name the API does not use reports zero forever"
            );
        }
        // The four the spec has and this module deliberately does not read, so
        // that "unread" stays a decision rather than an oversight. If one of
        // these disappears from the spec nothing here breaks — which is correct,
        // and is why they are asserted as *known* rather than as required.
        for carried in [
            "inference_geo",
            "output_tokens_details",
            "server_tool_use",
            "service_tier",
        ] {
            assert!(spec.iter().any(|name| name == carried));
        }

        assert_eq!(
            set(SPEC_CACHE_CREATION_FIELDS.iter().map(|f| f.to_string())),
            set(vocabulary(&pin, "cache_creation_fields")),
        );
    }

    #[test]
    fn a_usage_object_parses_the_cache_creation_breakdown_no_other_crate_models() {
        let usage: Usage = serde_json::from_value(serde_json::json!({
            "input_tokens": 12,
            "output_tokens": 34,
            "cache_creation_input_tokens": 500,
            "cache_read_input_tokens": 9_000,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 200,
                "ephemeral_1h_input_tokens": 300,
            },
            // The four properties this module carries and does not read.
            "service_tier": "standard",
            "server_tool_use": { "web_search_requests": 2 },
        }))
        .unwrap();

        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.cache_read_input_tokens, 9_000);
        assert_eq!(usage.cache_creation_input_tokens, 500);
        let breakdown = usage.cache_creation.clone().expect("the breakdown parsed");
        assert_eq!(breakdown.ephemeral_5m_input_tokens, 200);
        assert_eq!(breakdown.ephemeral_1h_input_tokens, 300);
        assert_eq!(
            breakdown.ephemeral_5m_input_tokens + breakdown.ephemeral_1h_input_tokens,
            usage.cache_creation_input_tokens,
            "the breakdown sums to the total on a well-formed object; asserted \
             on the fixture rather than enforced in code, because an upstream \
             that disagrees is reporting something and refusing the turn over \
             an accounting extra would be worse than recording both"
        );
        assert_eq!(usage.extra["service_tier"], serde_json::json!("standard"));

        // The whole object goes back out the way it came, extras included:
        // the serve surface re-emits this.
        assert_eq!(
            serde_json::to_value(&usage).unwrap()["server_tool_use"]["web_search_requests"],
            serde_json::json!(2)
        );
    }

    #[test]
    fn a_null_count_reads_as_zero_rather_than_failing_the_turn() {
        // The official client guards `input_tokens !== null` before comparing,
        // which is evidence enough that null reaches the wire. A parse error
        // here would fail a turn that was served correctly.
        let usage: Usage = serde_json::from_value(serde_json::json!({
            "input_tokens": null,
            "output_tokens": 7,
            "cache_read_input_tokens": null,
        }))
        .unwrap();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 7);
        assert!(!usage.reported_any_input());

        // CONTROL: the same shape with counts present reports them, so the
        // assertion above is about `null` and not about the fields being
        // ignored.
        let counted: Usage =
            serde_json::from_value(serde_json::json!({ "input_tokens": 3 })).unwrap();
        assert!(counted.reported_any_input());
    }

    #[test]
    fn the_twelve_spec_response_blocks_are_each_either_typed_or_carried_verbatim() {
        let pin = spec_pin();
        let spec = set(vocabulary(&pin, "response_content_block_members"));
        assert_eq!(spec.len(), 12, "the spec's response ContentBlock union");

        let ours = set(TYPED_RESPONSE_CONTENT_BLOCKS
            .iter()
            .map(|(schema, _)| schema.to_string())
            .chain(OPAQUE_RESPONSE_CONTENT_BLOCKS.iter().map(|s| s.to_string())));
        assert_eq!(
            ours, spec,
            "every response block the spec names must be classified: given a \
             typed arm, or explicitly listed as one the Opaque arm carries. A \
             thirteenth block upstream turns this red, which is the point"
        );

        // Each typed schema name maps to a wire `type` this module actually
        // parses into a named arm.
        for (schema, wire) in TYPED_RESPONSE_CONTENT_BLOCKS {
            let block: ContentBlock =
                serde_json::from_value(serde_json::json!({ "type": wire })).unwrap();
            assert!(
                !matches!(block, ContentBlock::Opaque(_)),
                "`{schema}` claims a typed arm for `{wire}` and did not get one"
            );
            assert_eq!(block.block_type(), Some(wire));
        }

        // `tool_result` is typed here and is *not* in that union, which is not
        // an inconsistency: it is a request-side block, and the serve surface
        // has to read one to map it onto a conversation item.
        let result: ContentBlock = serde_json::from_value(serde_json::json!({
            "type": "tool_result", "tool_use_id": "call_1", "content": "42",
        }))
        .unwrap();
        assert_eq!(result.block_type(), Some("tool_result"));
        assert!(
            !spec.iter().any(|name| name == "ResponseToolResultBlock"),
            "if the response union grows a tool_result block, this module's \
             typed arm needs re-checking against it rather than assuming"
        );
    }

    #[test]
    fn an_unknown_block_type_survives_a_round_trip_byte_for_byte() {
        // PROBE: the shape `claudius` rejects and Relay's codec flattens. A
        // resent history containing one of these has to admit as the same
        // prefix it was stored as, so anything less than verbatim is a fork.
        let original = serde_json::json!({
            "type": "web_search_tool_result",
            "tool_use_id": "srvtoolu_1",
            "content": [{ "type": "web_search_result", "url": "https://example.test" }],
        });
        let block: ContentBlock = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(block, ContentBlock::Opaque(original.clone()));
        assert_eq!(block.block_type(), Some("web_search_tool_result"));
        assert_eq!(serde_json::to_value(&block).unwrap(), original);

        // CONTROL: a named type does *not* land in the catch-all, so the
        // assertion above is about novelty and not about the enum being inert.
        let text: ContentBlock =
            serde_json::from_value(serde_json::json!({ "type": "text", "text": "hi" })).unwrap();
        assert_eq!(text, ContentBlock::text("hi"));
    }

    #[test]
    fn an_unknown_field_on_a_known_shape_rides_through_rather_than_failing() {
        // The pass-through condition, stated as a test: the serve surface
        // shares these types, and `deny_unknown_fields` anywhere here would
        // make roundhouse refuse a request Anthropic itself would have served.
        let frame = serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "future_field": 1 },
            "usage": { "output_tokens": 42, "future_counter": 9 },
            "future_top_level": true,
        });
        let event: StreamEvent = serde_json::from_value(frame.clone()).unwrap();
        let StreamEvent::MessageDelta {
            delta,
            usage,
            extra,
        } = &event
        else {
            panic!("a message_delta must parse")
        };
        assert_eq!(delta.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(delta.extra["future_field"], serde_json::json!(1));
        assert_eq!(usage.as_ref().unwrap().output_tokens, 42);
        assert_eq!(extra["future_top_level"], serde_json::json!(true));

        // And every unnamed field is still there on the way out. Not asserted
        // as whole-value equality, because a re-serialized `Usage` also names
        // the counters the frame omitted -- which is correct for the accounting
        // type and would make an equality assertion here a test of serde's
        // defaults rather than of the pass-through.
        let out = serde_json::to_value(&event).unwrap();
        assert_eq!(out["future_top_level"], serde_json::json!(true));
        assert_eq!(out["delta"]["future_field"], serde_json::json!(1));
        assert_eq!(out["usage"]["future_counter"], serde_json::json!(9));
        assert_eq!(out["usage"]["output_tokens"], serde_json::json!(42));
    }

    #[test]
    fn cache_control_spells_the_ttl_vocabulary_the_spec_pins() {
        let pin = spec_pin();
        assert_eq!(
            set(SPEC_CACHE_CONTROL_TTLS.iter().map(|t| t.to_string())),
            set(vocabulary(&pin, "cache_control_ttl")),
        );

        // The default breakpoint omits `ttl` entirely, and that is not the same
        // as sending "5m": the field itself requires the extended-TTL beta.
        let default = serde_json::to_value(CacheControl::ephemeral()).unwrap();
        assert_eq!(default, serde_json::json!({ "type": "ephemeral" }));
        assert_eq!(
            serde_json::to_value(CacheControl::ephemeral_for(CACHE_TTL_1H)).unwrap(),
            serde_json::json!({ "type": "ephemeral", "ttl": "1h" })
        );
    }

    /// A minimal well-formed frame for each event name, so the round-trip test
    /// above is exercising the arms rather than one hand-written example.
    fn minimal_event(name: &str) -> Value {
        match name {
            "message_start" => serde_json::json!({
                "type": "message_start",
                "message": {
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "model": "claude-x",
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": { "input_tokens": 1, "output_tokens": 0 },
                },
            }),
            "content_block_start" => serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" },
            }),
            "content_block_delta" => serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "hi" },
            }),
            "content_block_stop" => serde_json::json!({ "type": "content_block_stop", "index": 0 }),
            "message_delta" => serde_json::json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                "usage": { "output_tokens": 3 },
            }),
            "message_stop" => serde_json::json!({ "type": "message_stop" }),
            other => panic!("no fixture for `{other}`"),
        }
    }
}
