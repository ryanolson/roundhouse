// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Canonical conversation items.
//!
//! This is the provider-neutral form the session stores. Both the local Dynamo
//! path and the frontier path render *from* this; neither renders *to* it. That
//! direction matters: it is what lets one session be served by an OSS model on
//! one turn and a frontier model on the next without the history changing shape.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::ids::ResponseId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::Developer => "developer",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// The payload of an item.
///
/// Text plus the two tool shapes an agentic loop cannot do without, and — since
/// M11.1 — the three shapes the Anthropic Messages surface resends that none of
/// those three can hold. Images and audio still slot in as further variants
/// without disturbing the session or routing layers.
///
/// **The three new variants are additive and nothing above them moved.** The
/// durable log holds records written before they existed, and the tag values
/// `text`, `tool_call` and `tool_result` still mean exactly what they meant;
/// `a_pre_m11_log_record_still_deserializes` pins that against literal stored
/// JSON rather than against an argument. The alternative — widening
/// [`Self::ToolResult`] with the `is_error` flag the Messages wire carries, or
/// folding thinking into `Text` — would have changed a shape every existing
/// record is written in, and a log that no longer reads is not recoverable by a
/// rollback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ItemContent {
    Text {
        text: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        call_id: String,
        output: String,
    },
    /// Extended thinking, with the signature that makes it resendable.
    ///
    /// The signature is carried rather than dropped because dropping it does
    /// not merely lose provenance: an upstream rejects a resent thinking block
    /// whose signature is missing or altered, so a conversation that passed
    /// through here without it stops being continuable at all. It is a separate
    /// field rather than part of the text for the same reason the wire keeps it
    /// separate — the model reads one and validates the other.
    Thinking {
        thinking: String,
        signature: String,
    },
    /// Thinking the provider encrypted, which nothing here can read.
    ///
    /// Stored because the client resends it and the prefix has to match, not
    /// because anything downstream inspects it. `data` is opaque by
    /// construction: treating it as text and, say, scanning it for tool ids
    /// would be reading ciphertext as prose.
    RedactedThinking {
        data: String,
    },
    /// A content block this build does not model, kept verbatim.
    ///
    /// The opaque-first ruling (plan R5): images, documents, server-tool calls
    /// and their results, container uploads — a dozen block types today and a
    /// thirteenth next quarter — all ride through as the JSON the client sent.
    /// A typed variant per shape is future work and would buy something real
    /// (an image the router could price, a server-tool result the validate loop
    /// could pair), but each one is a decision about *semantics*, and until
    /// somebody makes it the honest reading of a block is "the client's bytes".
    /// The cost of guessing instead is paid at the prefix check: a block
    /// flattened into text canonicalizes differently the day the flattening
    /// changes, and every warm session forks at once.
    Opaque {
        /// The block's own `type`, lifted out so a reader — a refusal message,
        /// an operator grepping a log — can say *which* block this is without
        /// re-parsing `block`. Never derived from `block` at read time: the two
        /// are written together by the one canonicalization that refuses a
        /// block with no type at all.
        block_type: String,
        /// The whole block, as a parsed value rather than as the client's bytes.
        ///
        /// **Parsed, deliberately, and this is the variant's load-bearing
        /// choice.** Keeping the raw text would round-trip byte-exactly, but a
        /// chained NeMo Relay re-serializes every intercepted body through an
        /// alphabetizing `serde_json::Map` (synergy ruling S3, guard 1), so the
        /// bytes a client sent and the bytes that reach us differ by key order
        /// on the very next turn — and a prefix check over raw text would fork
        /// every session behind a Relay, silently, while every turn still
        /// answered. Two values parsed from differently ordered JSON compare
        /// equal, and `serde_json`'s default map is a `BTreeMap`, so
        /// [`ItemContent::render`] re-serializes in one canonical key order for
        /// any given value. Order-insensitivity and render determinism come
        /// from the same decision.
        block: Value,
    },
}

/// The one spelling of a tool call's arguments in this log.
///
/// **A canonical form, and it is what stops every tool-using session forking on
/// its second turn (M11.2).** The argument string reaches this log from two
/// directions that do not agree byte for byte on their own:
///
/// - The *model* produces it, and produces whatever it likes — keys in the order
///   it thought of them, spaces after the colons.
/// - The *client* sends the same call back on the next turn as history, and the
///   Messages wire carries it as a JSON **object**, so canonicalizing that
///   resend means serializing a parsed value: `serde_json`'s map is a
///   `BTreeMap`, so the result is compact and key-sorted. A chained NeMo Relay
///   re-serializes intercepted bodies through the same alphabetizing map
///   (synergy S3, guard 1), so nothing upstream of here can preserve the model's
///   spacing anyway.
///
/// Storing the model's bytes and comparing them against that resend fails on the
/// very first tool call with more than one key — `{"pattern": …, "path": …}`
/// against `{"path":…,"pattern":…}` — and prefix admission then forks the
/// conversation into a fresh session, silently, while every turn still answers.
/// So an emitted call is stored in the form its own resend will canonicalize to,
/// and the serve projections put *that* string on the wire, which is what makes
/// the round trip closed rather than merely likely.
///
/// A string that is not JSON at all passes through unchanged. It is not
/// representable on either dialect's wire — the Messages `input` is an object
/// and the client's accumulator throws on fragments that do not parse — so this
/// is the honest fallback for a corrupt log rather than a supported shape, and
/// silently replacing it with `{}` would hide the corruption.
pub fn canonical_arguments(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => value.to_string(),
        Err(_) => raw.to_string(),
    }
}

impl ItemContent {
    /// The text used for prompt rendering and token accounting.
    ///
    /// Tool calls and results are flattened to a stable textual form so the
    /// token buffer stays append-only. The exact rendering is a placeholder
    /// pending per-model chat templates; what matters here is determinism,
    /// because a rendering that varies between turns would invalidate every
    /// cached block after the first divergence.
    pub fn render(&self) -> String {
        match self {
            ItemContent::Text { text } => text.clone(),
            ItemContent::ToolCall {
                call_id,
                name,
                arguments,
            } => format!("<tool_call id=\"{call_id}\" name=\"{name}\">{arguments}</tool_call>"),
            ItemContent::ToolResult { call_id, output } => {
                format!("<tool_result id=\"{call_id}\">{output}</tool_result>")
            }
            // The signature rides the render, and it is not free: it is a few
            // hundred base64 characters that the turn id needs and no model
            // does. Excluding it would make two conversations that differ only
            // in their signatures hash to one turn id, and the turn id is what
            // makes a client's retry replay instead of paying twice — so the
            // collision would be a *second billed answer* attributed to the
            // first. The waste is bounded and visible; the collision would be
            // neither. The day per-model chat templates land (see this
            // function's own note), the prompt encoding and the identity
            // encoding part company and this is the line that splits.
            ItemContent::Thinking {
                thinking,
                signature,
            } => format!("<thinking signature=\"{signature}\">{thinking}</thinking>"),
            ItemContent::RedactedThinking { data } => {
                format!("<redacted_thinking>{data}</redacted_thinking>")
            }
            // **A digest of the block, never the block.** This is the one
            // variant whose body is unbounded: a pasted screenshot arrives as a
            // `source.data` of roughly 1.35 base64 characters per image byte,
            // so rendering it verbatim put a megabyte of base64 into all three
            // things this function feeds at once — the prompt the provider is
            // sent, the string [`crate::context`] tokenizes and the turn is
            // billed for, and the input to `turn_id_for`. A 1 MB paste was
            // therefore quoted, priced and dispatched as ~1.35M tokens of prose
            // that no model can read back as a picture (M11.1 review, F5).
            //
            // What the digest keeps is everything the three readers need:
            // *deterministic*, because `Display` for a `Value` is compact JSON
            // over a `BTreeMap`, so the key order is the sorted one for every
            // value alike — which is also what makes it order-insensitive, and
            // a body re-encoded by a chained Relay digests identically;
            // *identity-preserving*, because a block that changes changes its
            // digest, so turn ids and prefix admission still move when the
            // block moves; and *honest about size*, which the verbatim render
            // was not. What it drops is the payload, which no reader of this
            // string could use — a typed content-block path from
            // [`ItemContent`] through the dispatch is what would let a model
            // actually see the image, and that is the future work R5 names.
            //
            // `block_type` stays in the tag rather than inside the digest so
            // two blocks whose bodies match but whose types differ cannot
            // render alike, and so a refusal or a log line still says *which*
            // block this is without a second parse.
            //
            // **Safe to change here and only here**: `Opaque` is new in M11.1,
            // so no production log carries a turn id derived from the old
            // render, and the stored `Value` is untouched — serde is not on
            // this path, and a session that replays gets the same items it
            // always did.
            ItemContent::Opaque { block_type, block } => {
                let digest = hex::encode(Sha256::digest(block.to_string().as_bytes()));
                format!("<block type=\"{block_type}\" sha256=\"{digest}\">")
            }
        }
    }
}

/// One entry in the canonical conversation log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub role: Role,
    pub content: ItemContent,
    /// Set on assistant items so `previous_response_id` can resolve to the
    /// exact prefix a client is continuing from, and the provenance stamp on
    /// anything this deployment produced. Client input always canonicalizes with
    /// `None` and only the emission act (`Session::complete_with_item`) sets it,
    /// so a stamped item in the log is one *we* wrote and a client cannot forge
    /// one — which is what the wire projection's "may this go out on this
    /// response" check reads.
    ///
    /// **What that no longer distinguishes, since M10.0.** While the steer was a
    /// synthetic tool call, the stamp was also a free discriminator for *which*
    /// item was the correction: a `ToolCall` bearing a response id could only be
    /// ours, so the session fold read the shape and knew. A steer is assistant
    /// text now — the same shape every dispatched turn's answer has — so nothing
    /// about an item says it is a correction, and
    /// `SessionState::steered_on_turn` is folded from `ValidationDecided`
    /// instead. That is deliberate: it is also what keeps a resent history
    /// carrying the guidance admitting as an ordinary prefix, with no exclusion
    /// rule anywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<ResponseId>,
}

impl Item {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: ItemContent::Text { text: text.into() },
            response_id: None,
        }
    }

    pub fn system_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: ItemContent::Text { text: text.into() },
            response_id: None,
        }
    }

    pub fn assistant_text(text: impl Into<String>, response_id: ResponseId) -> Self {
        Self {
            role: Role::Assistant,
            content: ItemContent::Text { text: text.into() },
            response_id: Some(response_id),
        }
    }

    /// A tool call, with no provenance.
    ///
    /// `response_id` is deliberately `None`, and it is the constructor's whole
    /// point: a call built here is just a call. Only
    /// [`Session::complete_with_item`](crate::session::Session::complete_with_item)
    /// stamps a response onto one, which is what lets a stamped `ToolCall` in
    /// the log mean "this deployment emitted it" rather than "somebody set a
    /// field". The input path cannot produce a stamp — the wire layer's
    /// canonicalization sets `None` on everything a client sends — so the
    /// provenance marker is not something a client can forge.
    ///
    /// The name is the bare one. A namespace belongs to a client dialect and
    /// lives in the wire projection: canonicalization ignores it on the way
    /// in, so a namespaced resend and a flat one arrive as this same item, and
    /// the log keeps one spelling per tool.
    pub fn tool_call(
        call_id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: ItemContent::ToolCall {
                call_id: call_id.into(),
                name: name.into(),
                arguments: arguments.into(),
            },
            response_id: None,
        }
    }

    /// Deterministic prompt rendering for a single item.
    pub fn render(&self) -> String {
        format!("<|{}|>{}", self.role.as_str(), self.content.render())
    }

    /// What this item says to a human reader, and nothing else.
    ///
    /// Empty for everything that is not plain text, which is the whole
    /// distinction from [`Self::render`]: `render` produces the *prompt*
    /// encoding — role prefix, `<tool_call>` tags — and a caller that
    /// concatenated that into a transcript a person or an agent reads would
    /// find scaffolding in it, or worse, a tool call it might act on.
    ///
    /// Its caller is the interjection seam's completion: outcome C commits
    /// guidance text and outcome B commits a synthetic call, and the turn's
    /// reported text is the guidance for one and nothing for the other. A
    /// `match` at that site would work today and would answer wrongly the day
    /// a third completion shape is added, because the *default* it would have
    /// to pick is the unsafe one.
    ///
    /// **Thinking is not spoken output**, and that is the one arm here worth
    /// arguing about. A thinking block is text, it is the assistant's, and a
    /// caller reaching for "what did the model say" could plausibly want it —
    /// but the callers are the interjection seam's completion and the validate
    /// loop's signals, and both ask this question in order to *judge the
    /// answer*. Reading reasoning as answer text would make a turn that
    /// deliberated at length and then said nothing look like a turn that
    /// answered, which is precisely the failure the no-progress and
    /// empty-answer signals exist to catch. Redacted thinking is ciphertext and
    /// an opaque block is a shape nobody has read; neither is prose either.
    pub fn spoken_text(&self) -> &str {
        match &self.content {
            ItemContent::Text { text } => text,
            ItemContent::ToolCall { .. }
            | ItemContent::ToolResult { .. }
            | ItemContent::Thinking { .. }
            | ItemContent::RedactedThinking { .. }
            | ItemContent::Opaque { .. } => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_is_stable_across_calls() {
        let item = Item::user_text("hello");
        assert_eq!(item.render(), item.render());
        assert_eq!(item.render(), "<|user|>hello");
    }

    #[test]
    fn tool_items_render_deterministically() {
        let call = Item {
            role: Role::Assistant,
            content: ItemContent::ToolCall {
                call_id: "c1".into(),
                name: "grep".into(),
                arguments: "{\"q\":\"x\"}".into(),
            },
            response_id: None,
        };
        assert_eq!(call.render(), call.render());
        assert!(call.render().contains("name=\"grep\""));
    }

    fn item(content: ItemContent) -> Item {
        Item {
            role: Role::Assistant,
            content,
            response_id: None,
        }
    }

    /// **A record written before M11.1 still reads, and still writes back the
    /// same way.**
    ///
    /// The literals are the point. An argument that three added variants cannot
    /// disturb three existing ones is true of every additive change right up
    /// until someone reorders a field, renames a tag, or reaches for
    /// `#[serde(untagged)]` — and the log is durable, so the first symptom of
    /// getting it wrong is a session that can no longer be replayed at all.
    /// Both directions are asserted: reading proves an old record still
    /// deserializes, writing proves a *new* build does not start emitting a
    /// shape an older build could not read back.
    #[test]
    fn a_pre_m11_log_record_still_deserializes() {
        for stored in [
            r#"{"role":"user","content":{"type":"text","text":"hello"}}"#,
            r#"{"role":"assistant","content":{"type":"text","text":"hi"},"response_id":"resp_1"}"#,
            r#"{"role":"assistant","content":{"type":"tool_call","call_id":"c1","name":"grep","arguments":"{}"}}"#,
            r#"{"role":"tool","content":{"type":"tool_result","call_id":"c1","output":"3 hits"}}"#,
        ] {
            let item: Item = serde_json::from_str(stored).unwrap_or_else(|error| {
                panic!("pre-M11.1 record must still read: {stored} ({error})")
            });
            assert_eq!(
                serde_json::to_string(&item).expect("an item serializes"),
                stored,
                "a new build must write the old shape byte for byte"
            );
        }
    }

    /// **The three M11.1 variants' own shipped tags, pinned against literal
    /// JSON — the same discipline `a_pre_m11_log_record_still_deserializes`
    /// applies to the three shapes that predate them.**
    ///
    /// "Three added variants don't disturb three existing ones" stops being
    /// true the moment someone renames a tag, and that argument does not stop
    /// applying to Thinking, RedactedThinking and Opaque themselves the instant
    /// they ship: a record already durably stored under today's tag spelling
    /// (`"thinking"`, `"redacted_thinking"`, `"opaque"`) has to keep reading
    /// tomorrow, not only today. Pinned against literals rather than
    /// round-tripped through this build's own encoder, for the reason the
    /// pre-M11.1 test is: `the_new_variants_round_trip_through_the_log_encoding`
    /// encodes and decodes with the *same* code in one call, so it is
    /// self-consistent by construction and cannot see a tag drift — it would
    /// stay green even if every one of these three tags were renamed at once.
    #[test]
    fn the_m11_1_variants_shipped_tags_still_read() {
        for stored in [
            r#"{"role":"assistant","content":{"type":"thinking","thinking":"step one","signature":"sig"},"response_id":"resp_1"}"#,
            r#"{"role":"assistant","content":{"type":"redacted_thinking","data":"opaque"},"response_id":"resp_1"}"#,
            r#"{"role":"assistant","content":{"type":"opaque","block_type":"image","block":{"type":"image"}},"response_id":"resp_1"}"#,
        ] {
            let item: Item = serde_json::from_str(stored).unwrap_or_else(|error| {
                panic!("a record already stored under today's M11.1 tags must still read: {stored} ({error})")
            });
            assert_eq!(
                serde_json::to_string(&item).expect("an item serializes"),
                stored,
                "a later build must write the same shape it reads, byte for byte"
            );
        }
    }

    /// The renders of the three pre-M11.1 shapes, pinned as literals.
    ///
    /// Turn ids are FNV over exactly these strings and a client's retry is
    /// deduplicated by hashing to the same one, so a render that moved would
    /// orphan every in-flight retry in the fleet. `responses_api::wire` pins the
    /// resulting hash; this pins the input to it, so a change that moves the
    /// hash says *which* rendering moved instead of only that one did.
    #[test]
    fn the_pre_m11_renders_are_pinned() {
        assert_eq!(Item::user_text("hello").render(), "<|user|>hello");
        assert_eq!(
            item(ItemContent::ToolCall {
                call_id: "c1".into(),
                name: "grep".into(),
                arguments: "{\"q\":\"x\"}".into(),
            })
            .render(),
            "<|assistant|><tool_call id=\"c1\" name=\"grep\">{\"q\":\"x\"}</tool_call>"
        );
        assert_eq!(
            item(ItemContent::ToolResult {
                call_id: "c1".into(),
                output: "3 hits".into(),
            })
            .render(),
            "<|assistant|><tool_result id=\"c1\">3 hits</tool_result>"
        );
    }

    /// Every new variant renders, renders the same way twice, and renders
    /// differently from the others.
    ///
    /// "Injective enough" is the standard `render` has always held itself to —
    /// a `Text` item whose text is literally `<tool_call …>` collides with a
    /// real call, and has since M0 — so what is asserted is the property the
    /// turn id actually needs: no two *shapes* collapse, and no field a variant
    /// carries is dropped on the floor where two values differing only in it
    /// would hash alike.
    #[test]
    fn the_new_variants_render_deterministically_and_distinctly() {
        let renders: Vec<String> = [
            ItemContent::Thinking {
                thinking: "step one".into(),
                signature: "sig_a".into(),
            },
            // Same reasoning, different signature: a different block upstream,
            // and it must be a different render or two conversations collide on
            // one turn id.
            ItemContent::Thinking {
                thinking: "step one".into(),
                signature: "sig_b".into(),
            },
            ItemContent::RedactedThinking {
                data: "step one".into(),
            },
            ItemContent::Opaque {
                block_type: "image".into(),
                block: serde_json::json!({ "type": "image", "source": { "type": "base64" } }),
            },
            // The same body under a different block type. The type is in the
            // tag precisely so this pair does not collapse.
            ItemContent::Opaque {
                block_type: "document".into(),
                block: serde_json::json!({ "type": "image", "source": { "type": "base64" } }),
            },
            ItemContent::Text {
                text: "step one".into(),
            },
        ]
        .iter()
        .map(|content| {
            let rendered = content.render();
            // Eight calls, not two. `Value`'s own `Display` is deterministic
            // (a `BTreeMap` underneath — see `ItemContent::render`'s doc), so
            // this is redundant against the shipped implementation, but it is
            // the direct guard for that property and it must actually hold
            // one on its own: an implementation that rebuilt the rendered
            // string from a fresh `HashMap` per call (a regression this
            // module's own review history has seen — a chained Relay
            // re-encodes intercepted bodies, so key order is not something a
            // future edit gets to assume away) would have its default
            // per-thread `RandomState` seed only one increment apart between
            // two back-to-back calls, which for a handful of keys lands on
            // the same iteration order often enough that two calls alone
            // pass by chance a third of the time. Eight independent calls
            // agreeing by that same chance is far less likely, without
            // asserting anything about `HashMap` internals directly.
            for _ in 0..8 {
                assert_eq!(rendered, content.render(), "render must be a function");
            }
            rendered
        })
        .collect();

        for (i, left) in renders.iter().enumerate() {
            for right in &renders[i + 1..] {
                assert_ne!(left, right, "two distinct blocks rendered alike");
            }
        }
    }

    /// **Key order in an opaque block changes nothing.**
    ///
    /// The guard for synergy ruling S3's first chain hazard: a chained NeMo
    /// Relay re-serializes intercepted bodies through an alphabetizing
    /// `serde_json::Map`, so the second turn of a conversation arrives with its
    /// object keys in a different order than the first. Storing the client's
    /// raw bytes would make that a prefix disagreement — every session behind a
    /// Relay forking on turn two, while every turn still answered.
    #[test]
    fn an_opaque_block_is_insensitive_to_key_order() {
        let sent = r#"{"type":"image","source":{"type":"base64","data":"AA"},"index":2}"#;
        let relayed = r#"{"index":2,"source":{"data":"AA","type":"base64"},"type":"image"}"#;

        let block = |json: &str| ItemContent::Opaque {
            block_type: "image".into(),
            block: serde_json::from_str(json).expect("the fixture is JSON"),
        };
        assert_eq!(
            block(sent),
            block(relayed),
            "prefix admission compares content, and a re-encoded body must compare equal"
        );
        assert_eq!(
            block(sent).render(),
            block(relayed).render(),
            "and the turn id is over the render, so it must agree too"
        );
    }

    /// **An opaque block renders as a digest, never as its payload.**
    ///
    /// The guard for M11.1's F5. `render` is the prompt encoding, the
    /// token-count encoding and the identity encoding at once, and an opaque
    /// block is the only variant whose body has no bound — a pasted screenshot
    /// is base64 in `source.data`. Rendering it verbatim billed and dispatched
    /// the image as prose at roughly one token per base64 character.
    ///
    /// Three assertions, because the fix has to keep two properties while
    /// dropping one: the payload is *gone* from the string, the string's length
    /// does not move with the payload's, and the identity still does — a
    /// digest that ignored the body would make every image in a conversation
    /// the same block and hand a client somebody else's cached answer.
    #[test]
    fn an_opaque_block_renders_as_a_digest_rather_than_its_bytes() {
        let image = |data: &str| ItemContent::Opaque {
            block_type: "image".into(),
            block: serde_json::json!({
                "type": "image",
                "source": { "type": "base64", "media_type": "image/png", "data": data },
            }),
        };
        let payload = "A".repeat(4096);
        let rendered = image(&payload).render();

        assert!(
            !rendered.contains(&payload),
            "the payload is in the prompt, the token count and the turn id: {rendered}"
        );
        assert_eq!(
            rendered.len(),
            image("AA").render().len(),
            "a render whose length moves with the payload is one the tokenizer \
             bills for the payload: {rendered}"
        );
        assert_ne!(
            rendered,
            image(&format!("{payload}B")).render(),
            "two different blocks must render differently, or prefix admission \
             and `turn_id_for` stop moving when the block moves"
        );
    }

    /// None of the three is answer text.
    ///
    /// The control is the arm that *is*: without it a `spoken_text` that
    /// returned `""` unconditionally would pass, and the claim would be
    /// tautological.
    #[test]
    fn thinking_is_never_spoken_output() {
        for content in [
            ItemContent::Thinking {
                thinking: "the user probably wants X".into(),
                signature: "sig".into(),
            },
            ItemContent::RedactedThinking {
                data: "opaque".into(),
            },
            ItemContent::Opaque {
                block_type: "image".into(),
                block: serde_json::json!({ "type": "image" }),
            },
        ] {
            assert_eq!(
                item(content).spoken_text(),
                "",
                "the validate loop's signals must not read reasoning as an answer"
            );
        }
        assert_eq!(
            item(ItemContent::Text {
                text: "the answer".into()
            })
            .spoken_text(),
            "the answer"
        );
    }

    /// The three new variants round-trip through the durable log's encoding.
    #[test]
    fn the_new_variants_round_trip_through_the_log_encoding() {
        for content in [
            ItemContent::Thinking {
                thinking: "step one".into(),
                signature: "sig".into(),
            },
            ItemContent::RedactedThinking {
                data: "opaque".into(),
            },
            ItemContent::Opaque {
                block_type: "server_tool_use".into(),
                block: serde_json::json!({
                    "type": "server_tool_use",
                    "id": "srvtoolu_1",
                    "name": "web_search",
                    "input": { "query": "rust" },
                }),
            },
        ] {
            let original = item(content);
            let encoded = serde_json::to_string(&original).expect("an item serializes");
            let decoded: Item = serde_json::from_str(&encoded).expect("what we wrote, we can read");
            assert_eq!(decoded, original, "round trip changed the item: {encoded}");
        }
    }
}
