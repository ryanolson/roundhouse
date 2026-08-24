// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Canonical conversation items.
//!
//! This is the provider-neutral form the session stores. Both the local Dynamo
//! path and the frontier path render *from* this; neither renders *to* it. That
//! direction matters: it is what lets one session be served by an OSS model on
//! one turn and a frontier model on the next without the history changing shape.

use serde::{Deserialize, Serialize};

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
/// Deliberately small for the walking skeleton: text plus the two tool shapes
/// an agentic loop cannot do without. Images and audio slot in as further
/// variants without disturbing the session or routing layers.
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
    pub fn spoken_text(&self) -> &str {
        match &self.content {
            ItemContent::Text { text } => text,
            ItemContent::ToolCall { .. } | ItemContent::ToolResult { .. } => "",
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
}
