// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The tier-1 conformance oracle: a deliberately strict Messages reader.
//!
//! # Why this exists rather than an SDK
//!
//! No Anthropic-published strict parser exists in any language. Both official
//! SDKs are non-validating on purpose — the TypeScript one bare-casts and the
//! Python one uses Pydantic's `construct` — so spawning either would be a
//! *sequencing* oracle at best and would agree with any field soup we sent it.
//! The strict community crates are worse than nothing: `claudius` closed its
//! enums in 2025 and now rejects three `stop_reason` values and six of the
//! spec's twelve response blocks, and `adk-anthropic` models a usage field that
//! does not exist on the wire. So the codex-oracle pattern is mirrored with
//! different provenance (plan R6): a reader written here, from the pinned spec's
//! own vocabulary, whose polarity is the **opposite** of the shipped module's.
//!
//! `roundhouse-fleet`'s wire types are open by design — untagged fallbacks,
//! `#[serde(flatten)] extra`, an open `stop_reason` — because a client that
//! refuses a value newer than itself refuses correct traffic. Everything here is
//! closed: `deny_unknown_fields` on every struct, no fallback variant on any
//! enum, and no `extra` anywhere. The two must disagree for this to be worth
//! running. A field roundhouse invents, a `stop_reason` it misspells, or an
//! event name that does not match its payload's `type` is caught here and
//! nowhere else, because the shipping client would carry all three in silence.
//!
//! # And why it is narrower than the API on purpose
//!
//! [`StrictBlock`] knows five block types, not the spec's twelve. This surface
//! emits exactly one of them, and the day it emits a `server_tool_use` or a
//! `web_search_tool_result` the first thing that has to change is this enum —
//! which is a review of a new emitted shape rather than a shape that shipped
//! because a parser was permissive. Narrower-than-the-spec is the correct
//! direction for an oracle; it is the wrong direction for the shipped types, and
//! that is the whole distinction between the two files.
//!
//! # What the sequencing rules are drawn from
//!
//! [`StreamOracle`] encodes what Claude Code's own consumer enforces, which is
//! stricter than the documented contract in some places and looser in others
//! (`agent-docs/research/claude-code-client-surface.md` §3.2–§3.4):
//!
//! - Dispatch is on the SSE `event:` name. A frame without one matches no branch
//!   and is **silently dropped** — the stream then ends having consumed nothing
//!   and the client re-issues the entire turn non-streaming, at full price. That
//!   is why [`split_frames`] counts nameless frames rather than skipping them the
//!   way the Responses suites' reader does.
//! - The accumulator throws on five conditions, four of which are ordering
//!   (`Content block not found` twice, `Message not found`, and the three
//!   block-type disagreements). Each is a rule below.
//! - The usage merge guards input and cache counts with `> 0` but takes
//!   `output_tokens` with `??`, so an explicit `"output_tokens": 0` in a
//!   `message_delta` overwrites a real count and bills the turn as free. That is
//!   the single most expensive frame this surface could emit, and
//!   [`MergedUsage::merge`] reproduces the client's arithmetic exactly so a test
//!   asserts against what the client would compute rather than against what we
//!   meant.

use std::collections::HashMap;

use serde::Deserialize;

// ---------------------------------------------------------------------------
// The strict vocabulary
// ---------------------------------------------------------------------------

/// Why the model stopped, closed over the pinned spec's seven values.
///
/// No `Other` arm, deliberately: the shipped [`StopReason`](roundhouse_fleet::anthropic_messages::wire::StopReason)
/// has one because a value newer than the build must be carried rather than
/// refused, and this one must not, because a value *we* emit that the spec does
/// not name is a bug we would otherwise ship. The seven are
/// `spec_pin.json`'s `vocabulary.stop_reason` verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrictStopReason {
    EndTurn,
    MaxTokens,
    ModelContextWindowExceeded,
    PauseTurn,
    Refusal,
    StopSequence,
    ToolUse,
}

/// The only role a response message may claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrictRole {
    Assistant,
}

/// A content block, closed over the five shapes this deployment can produce or
/// store.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StrictBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: serde_json::Value,
        #[serde(default)]
        is_error: bool,
    },
}

impl StrictBlock {
    /// Which delta types may be applied to this block.
    ///
    /// The client's three block-type throws, read as a table. A `text_delta` on
    /// a thinking block is `Error("Content block is not a text block")`, and a
    /// thrown accumulator is a lost turn rather than a dropped frame.
    fn accepts(&self, delta: &StrictDelta) -> bool {
        matches!(
            (self, delta),
            (StrictBlock::Text { .. }, StrictDelta::TextDelta { .. })
                | (
                    StrictBlock::Thinking { .. },
                    StrictDelta::ThinkingDelta { .. } | StrictDelta::SignatureDelta { .. }
                )
                | (
                    StrictBlock::ToolUse { .. },
                    StrictDelta::InputJsonDelta { .. }
                )
        )
    }

    fn wire_name(&self) -> &'static str {
        match self {
            StrictBlock::Text { .. } => "text",
            StrictBlock::Thinking { .. } => "thinking",
            StrictBlock::RedactedThinking { .. } => "redacted_thinking",
            StrictBlock::ToolUse { .. } => "tool_use",
            StrictBlock::ToolResult { .. } => "tool_result",
        }
    }
}

/// One `content_block_delta` payload, closed over the pinned four.
///
/// `spec_pin.json`'s `vocabulary.content_block_delta_variants` exactly.
/// `citations_delta` is absent because the spec's member list does not name it
/// and this surface does not emit it; if it ever does, this enum is where that
/// gets noticed.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StrictDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
}

/// The usage object, closed over the pinned property set.
///
/// Every field defaulted rather than required, and that is not laxity: the
/// pinned spec puts `additionalProperties: false` only on *request* schemas, so
/// a response object omitting a nullable field is conformant. What this type
/// refuses is a property the spec has never named — which is the failure mode
/// that actually happened in the wild (`adk-anthropic`'s
/// `cache_creation_input_tokens_1h`, a field invented by a crate and reported to
/// nobody).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrictUsage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation: Option<StrictCacheCreation>,
    #[serde(default)]
    pub server_tool_use: Option<serde_json::Value>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub output_tokens_details: Option<serde_json::Value>,
    #[serde(default)]
    pub inference_geo: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrictCacheCreation {
    #[serde(default)]
    pub ephemeral_5m_input_tokens: Option<u64>,
    #[serde(default)]
    pub ephemeral_1h_input_tokens: Option<u64>,
}

/// The `Message` skeleton, closed over the pinned `message_properties`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrictMessage {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub role: StrictRole,
    pub model: String,
    pub content: Vec<StrictBlock>,
    #[serde(default)]
    pub stop_reason: Option<StrictStopReason>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
    pub usage: StrictUsage,
    #[serde(default)]
    pub container: Option<serde_json::Value>,
    #[serde(default)]
    pub stop_details: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrictMessageDelta {
    #[serde(default)]
    pub stop_reason: Option<StrictStopReason>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

/// The error vocabulary Anthropic publishes.
///
/// Closed, and the closure is load-bearing rather than tidy: `overloaded_error`
/// is the one value Claude Code retries a mid-stream failure on, so a typo in it
/// is a turn that ends where it should have resumed, and a *correct* spelling
/// applied to a permanent fault is an agent burning its whole retry budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrictErrorKind {
    InvalidRequestError,
    AuthenticationError,
    BillingError,
    PermissionError,
    NotFoundError,
    RequestTooLarge,
    RateLimitError,
    TimeoutError,
    ApiError,
    OverloadedError,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrictError {
    #[serde(rename = "type")]
    pub kind: StrictErrorKind,
    pub message: String,
}

/// One stream event.
///
/// `ping` and `error` are here even though the pinned spec's
/// `message_stream_event_members` names only six: the spec does not describe the
/// SSE transport at all, and both are real on the wire and documented in
/// Anthropic's streaming guide. That gap is itself worth stating — an oracle
/// generated from the spec alone would reject a conformant stream.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StrictEvent {
    MessageStart {
        message: StrictMessage,
    },
    ContentBlockStart {
        index: u64,
        content_block: StrictBlock,
    },
    ContentBlockDelta {
        index: u64,
        delta: StrictDelta,
    },
    ContentBlockStop {
        index: u64,
    },
    MessageDelta {
        delta: StrictMessageDelta,
        #[serde(default)]
        usage: Option<StrictUsage>,
    },
    MessageStop,
    Ping,
    Error {
        error: StrictError,
    },
}

impl StrictEvent {
    /// The `event:` name this payload must have arrived under.
    pub fn wire_name(&self) -> &'static str {
        match self {
            StrictEvent::MessageStart { .. } => "message_start",
            StrictEvent::ContentBlockStart { .. } => "content_block_start",
            StrictEvent::ContentBlockDelta { .. } => "content_block_delta",
            StrictEvent::ContentBlockStop { .. } => "content_block_stop",
            StrictEvent::MessageDelta { .. } => "message_delta",
            StrictEvent::MessageStop => "message_stop",
            StrictEvent::Ping => "ping",
            StrictEvent::Error { .. } => "error",
        }
    }
}

// ---------------------------------------------------------------------------
// The client's usage merge
// ---------------------------------------------------------------------------

/// What the client's accumulator holds after merging every `usage` it saw.
///
/// `p91` from the bundle, reproduced field for field (§3.4). The point of
/// reproducing it rather than asserting on our frames directly is that the
/// *merge* is where a correct-looking pair of frames can still bill wrongly: a
/// `message_start` reporting the input counts and a `message_delta` reporting
/// `output_tokens: 0` are each individually plausible and together report a free
/// turn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MergedUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl MergedUsage {
    fn merge(&mut self, reported: &StrictUsage) {
        // `> 0` on the three input axes: a frame that omits them or sends zero
        // leaves the accumulated value standing, which is what makes the
        // documented split reporting safe.
        if let Some(value) = reported.input_tokens.filter(|value| *value > 0) {
            self.input_tokens = value;
        }
        if let Some(value) = reported
            .cache_creation_input_tokens
            .filter(|value| *value > 0)
        {
            self.cache_creation_input_tokens = value;
        }
        if let Some(value) = reported.cache_read_input_tokens.filter(|value| *value > 0) {
            self.cache_read_input_tokens = value;
        }
        // `??` on the output axis: present-and-zero *replaces*. The asymmetry is
        // the client's, not ours, and it is the reason a terminal frame must
        // omit the whole `usage` object rather than report a zero.
        if let Some(value) = reported.output_tokens {
            self.output_tokens = value;
        }
    }

    /// What Anthropic's billing semantics say the prompt cost in total.
    ///
    /// The three input axes are disjoint on this wire, so a client sums them.
    /// Stated here because it is the identity the serve surface's inversion has
    /// to preserve: roundhouse nests cached and written input inside its own
    /// `input_tokens`, and getting the direction wrong reports a warm turn as
    /// nearly two cold ones.
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }
}

// ---------------------------------------------------------------------------
// The sequencing oracle
// ---------------------------------------------------------------------------

/// What a conformant stream left behind.
#[derive(Debug, Clone, PartialEq)]
pub struct Accumulated {
    pub message_id: String,
    pub model: String,
    /// The text assembled from `text_delta`s, which is the whole of what a user
    /// sees.
    pub text: String,
    pub stop_reason: Option<StrictStopReason>,
    pub usage: MergedUsage,
    /// Set when the stream ended in an `error` event, which is terminal and
    /// legal on its own.
    pub error: Option<StrictError>,
    /// How many content blocks reached a `content_block_stop`.
    ///
    /// Zero is a specific, expensive failure and not merely an empty answer:
    /// "stream completed with `message_start` but no content blocks completed"
    /// is one of the two conditions that make Claude Code re-issue the entire
    /// turn without streaming (§3.6).
    pub completed_blocks: usize,
}

/// A strict consumer of one stream.
#[derive(Debug, Default)]
pub struct StreamOracle {
    started: Option<(String, String)>,
    open: HashMap<u64, StrictBlock>,
    seen_indices: Vec<u64>,
    completed_blocks: usize,
    text: String,
    stop_reason: Option<StrictStopReason>,
    usage: MergedUsage,
    error: Option<StrictError>,
    terminated: bool,
}

impl StreamOracle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one frame, as the wire delivered it.
    ///
    /// `name` is the SSE `event:` line and `data` the JSON beside it. Both are
    /// taken because their agreement is one of the rules: the client dispatches
    /// on the name and every downstream reader — including roundhouse's own
    /// dispatch decoder, in the chained topology — reads the payload's `type`,
    /// so a frame where the two disagree is a frame two readers understand
    /// differently.
    pub fn accept(&mut self, name: &str, data: &str) -> Result<(), String> {
        if self.terminated {
            return Err(format!(
                "frame `{name}` arrived after the stream's terminal event; nothing may follow it \
                 in either direction"
            ));
        }
        let event: StrictEvent = serde_json::from_str(data)
            .map_err(|error| format!("`{name}` is not a conformant payload: {error}\n{data}"))?;
        if event.wire_name() != name {
            return Err(format!(
                "the `event:` line says `{name}` and the payload says `{}`; one of the two is \
                 lying about what frame this is",
                event.wire_name()
            ));
        }
        match event {
            // Allowed anywhere, including before `message_start`: the client
            // skips it at the SSE layer and never shows it to the accumulator.
            StrictEvent::Ping => Ok(()),
            StrictEvent::MessageStart { message } => self.begin(message),
            StrictEvent::ContentBlockStart {
                index,
                content_block,
            } => self.open_block(index, content_block),
            StrictEvent::ContentBlockDelta { index, delta } => self.apply(index, delta),
            StrictEvent::ContentBlockStop { index } => self.close_block(index),
            StrictEvent::MessageDelta { delta, usage } => self.finish_message(delta, usage),
            StrictEvent::MessageStop => {
                if self.started.is_none() {
                    return Err("`message_stop` before any `message_start`".into());
                }
                self.terminated = true;
                Ok(())
            }
            StrictEvent::Error { error } => {
                self.error = Some(error);
                self.terminated = true;
                Ok(())
            }
        }
    }

    fn begin(&mut self, message: StrictMessage) -> Result<(), String> {
        if self.started.is_some() {
            return Err("a second `message_start`: the client announces one message".into());
        }
        if message.kind != "message" {
            return Err(format!(
                "`message.type` is `{}`, not `message`",
                message.kind
            ));
        }
        if !message.content.is_empty() {
            return Err(
                "`message_start` must carry empty `content`; the blocks arrive as their own \
                 events"
                    .into(),
            );
        }
        self.usage.merge(&message.usage);
        self.started = Some((message.id.clone(), message.model.clone()));
        Ok(())
    }

    fn open_block(&mut self, index: u64, block: StrictBlock) -> Result<(), String> {
        if self.started.is_none() {
            return Err(format!(
                "`content_block_start` at index {index} before any `message_start`"
            ));
        }
        if self.open.contains_key(&index) {
            return Err(format!("index {index} is already an open content block"));
        }
        if let StrictBlock::Text { text } = &block
            && !text.is_empty()
        {
            return Err(format!(
                "the text block opened at index {index} is seeded with `{text}`; the accumulator \
                 appends every delta to it, so a non-empty seed is the answer said twice"
            ));
        }
        self.seen_indices.push(index);
        self.open.insert(index, block);
        Ok(())
    }

    fn apply(&mut self, index: u64, delta: StrictDelta) -> Result<(), String> {
        // `RangeError("Content block not found")` — a thrown accumulator, not a
        // dropped frame.
        let block = self.open.get(&index).ok_or_else(|| {
            format!("`content_block_delta` at index {index}, which no `content_block_start` opened")
        })?;
        if !block.accepts(&delta) {
            return Err(format!(
                "a {delta:?} applied to a `{}` block at index {index}",
                block.wire_name()
            ));
        }
        if let StrictDelta::TextDelta { text } = &delta {
            self.text.push_str(text);
        }
        Ok(())
    }

    fn close_block(&mut self, index: u64) -> Result<(), String> {
        if self.started.is_none() {
            return Err(format!(
                "`content_block_stop` at index {index} before any `message_start`"
            ));
        }
        if self.open.remove(&index).is_none() {
            return Err(format!(
                "`content_block_stop` at index {index}, which is not an open block"
            ));
        }
        self.completed_blocks += 1;
        Ok(())
    }

    fn finish_message(
        &mut self,
        delta: StrictMessageDelta,
        usage: Option<StrictUsage>,
    ) -> Result<(), String> {
        if self.started.is_none() {
            return Err("`message_delta` before any `message_start`".into());
        }
        if let Some(reason) = delta.stop_reason {
            self.stop_reason = Some(reason);
        }
        if let Some(usage) = usage {
            // **The most expensive single frame this surface could emit.** The
            // client merges this field with `??`, so an explicit zero replaces a
            // real count and the turn is billed as free — and a free turn is the
            // one accounting mistake that reads as a saving rather than as a
            // fault. Omitting the whole `usage` object is the documented-safe
            // way to say "no output count in this frame".
            if usage.output_tokens == Some(0) {
                return Err(
                    "`message_delta` reports `output_tokens: 0`; the client's `??` merge would \
                     overwrite the real count and bill this turn as free"
                        .into(),
                );
            }
            self.usage.merge(&usage);
        }
        Ok(())
    }

    /// What the stream added up to, or why it was not conformant.
    pub fn finish(self) -> Result<Accumulated, String> {
        let Some((message_id, model)) = self.started else {
            // "Stream completed without receiving message_start event" — the
            // first of the two non-streaming-fallback triggers.
            return Err(
                "the stream ended without a `message_start`; the client re-issues the whole turn \
                 non-streaming for this, at full price"
                    .into(),
            );
        };
        if !self.open.is_empty() {
            let mut indices: Vec<u64> = self.open.keys().copied().collect();
            indices.sort_unstable();
            return Err(format!(
                "the stream ended with content blocks still open at {indices:?}"
            ));
        }
        if self.error.is_none() {
            if !self.terminated {
                return Err("the stream ended without a `message_stop`".into());
            }
            if self.completed_blocks == 0 {
                // "Stream completed with message_start but no content blocks
                // completed" — the second trigger.
                return Err(
                    "the stream completed no content block; the client re-issues the whole turn \
                     non-streaming for this, at full price"
                        .into(),
                );
            }
            if self.stop_reason.is_none() {
                return Err("the stream ended with no `stop_reason`".into());
            }
        }
        Ok(Accumulated {
            message_id,
            model,
            text: self.text,
            stop_reason: self.stop_reason,
            usage: self.usage,
            error: self.error,
            completed_blocks: self.completed_blocks,
        })
    }
}

// ---------------------------------------------------------------------------
// Reading an SSE body
// ---------------------------------------------------------------------------

/// One frame as the wire carried it.
#[derive(Debug, Clone, PartialEq)]
pub struct RawFrame {
    /// `None` when the frame carried no `event:` line — the failure that costs a
    /// whole extra turn, so it is represented rather than filtered.
    pub name: Option<String>,
    pub data: Option<String>,
}

/// Split an SSE body into frames, keeping the ones a client would drop.
///
/// Deliberately not `common::codex::frames`, which returns only frames that had
/// both lines. On this dialect a frame missing its `event:` line is not noise to
/// skip: it is the specific defect that makes Claude Code consume nothing and
/// re-issue the turn, so the oracle has to be able to see one.
pub fn split_frames(body: &str) -> Vec<RawFrame> {
    body.split("\n\n")
        .filter(|raw| !raw.trim().is_empty())
        .map(|raw| {
            let mut name = None;
            let mut data = None;
            for line in raw.lines() {
                let Some((field, value)) =
                    line.split_once(':').filter(|(field, _)| !field.is_empty())
                else {
                    // A line beginning with `:` is an SSE comment. Counted as
                    // nothing here on purpose: it keeps a direct connection
                    // alive and is discarded by a chained Relay's re-encoder, so
                    // a stream that relied on one would be alive on one topology
                    // and dead on the other.
                    continue;
                };
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "event" => name = Some(value.to_string()),
                    "data" => data = Some(value.to_string()),
                    _ => {}
                }
            }
            RawFrame { name, data }
        })
        .collect()
}

/// Drive a whole SSE body through the oracle.
///
/// The one entry point a suite needs: it enforces the framing rules
/// [`split_frames`] can see and the sequencing rules [`StreamOracle`] can, and
/// returns what a conformant client would have accumulated.
pub fn audit(body: &str) -> Result<Accumulated, String> {
    let mut oracle = StreamOracle::new();
    for (position, frame) in split_frames(body).into_iter().enumerate() {
        let Some(name) = frame.name else {
            return Err(format!(
                "frame {position} carries no `event:` line; Claude Code dispatches on the name, so \
                 this frame is dropped in silence and the turn is re-issued non-streaming"
            ));
        };
        let Some(data) = frame.data else {
            return Err(format!(
                "frame {position} (`{name}`) carries no `data:` line; a chained NeMo Relay's \
                 re-encoder discards it entirely"
            ));
        };
        oracle
            .accept(&name, &data)
            .map_err(|error| format!("frame {position}: {error}"))?;
    }
    oracle.finish()
}
