// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Claude Code captures, as canonical items and header maps.
//!
//! **The canonicalizer here is a test copy of the server's
//! `messages_api::wire::canonicalize`**, reduced to what the captures hold, and
//! that is a deliberate cost: the crate cannot depend on the server that
//! depends on it. The copy keeps the three rules that decide where a chain
//! breaks — one item per block, the ephemeral budget notice dropped, the
//! leading system run marked `Developer` — and
//! `claude_fixture_divergence_is_pinned` would disagree with the evidence
//! document's measured positions if any of the three drifted.

use http::{HeaderMap, HeaderName, HeaderValue};
use roundhouse_core::item::{Item, ItemContent, Role};
use serde_json::Value;

macro_rules! fixture {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../roundhouse-server/tests/fixtures/",
                $name
            )),
        )
    };
}

/// The seven Messages bodies. `claude-2.1.257-mcp-wire.json` holds MCP
/// requests, not Messages bodies, and is not one of them.
pub const BODIES: [(&str, &str); 7] = [
    fixture!("claude-2.1.251-turn-1.json"),
    fixture!("claude-2.1.251-turn-2-continue.json"),
    fixture!("claude-2.1.257-turn-1.json"),
    fixture!("claude-2.1.257-turn-2-continue.json"),
    fixture!("claude-2.1.257-turn-3-continue.json"),
    fixture!("claude-2.1.257-mcp-turn-1.json"),
    fixture!("claude-2.1.257-mcp-turn-2-toolresult.json"),
];

/// The three header captures, two requests each.
pub const HEADER_CAPTURES: [(&str, &str); 3] = [
    fixture!("claude-2.1.251-headers.json"),
    fixture!("claude-2.1.257-headers.json"),
    fixture!("claude-2.1.257-mcp-headers.json"),
];

pub fn body(name: &str) -> Value {
    let (_, raw) = BODIES
        .iter()
        .find(|(fixture, _)| *fixture == name)
        .unwrap_or_else(|| panic!("no body fixture {name}"));
    serde_json::from_str(raw).expect("fixture parses")
}

/// The client version a fixture's file name carries.
pub fn version_of(name: &str) -> &str {
    name.strip_prefix("claude-")
        .and_then(|rest| rest.split('-').next())
        .expect("fixture names start claude-<version>-")
}

pub fn user_id(body: &Value) -> Option<&str> {
    body.get("metadata")?.get("user_id")?.as_str()
}

/// Every captured header set, named `<capture>#<request>`.
pub fn header_sets() -> Vec<(String, HeaderMap)> {
    let mut sets = Vec::new();
    for (name, raw) in HEADER_CAPTURES {
        let requests: Vec<Value> = serde_json::from_str(raw).expect("capture parses");
        for (index, request) in requests.iter().enumerate() {
            let mut headers = HeaderMap::new();
            for (key, value) in request["headers"].as_object().expect("headers object") {
                headers.insert(
                    HeaderName::from_bytes(key.as_bytes()).expect("captured name"),
                    HeaderValue::from_str(value.as_str().expect("string value"))
                        .expect("captured value"),
                );
            }
            sets.push((format!("{name}#{index}"), headers));
        }
    }
    sets
}

pub fn canonical(body: &Value) -> Vec<Item> {
    let mut items = Vec::new();
    match body.get("system") {
        Some(Value::String(text)) if !text.is_empty() => items.push(Item::system_text(text)),
        Some(Value::Array(blocks)) => {
            items.extend(blocks.iter().map(|block| block_item(Role::System, block)))
        }
        _ => {}
    }
    for message in body["messages"].as_array().expect("messages") {
        let role = match message["role"].as_str() {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            Some("system") => Role::System,
            other => panic!("role {other:?} in a fixture"),
        };
        if role == Role::System && is_budget_notice(&message["content"]) {
            continue;
        }
        match &message["content"] {
            Value::String(text) => items.push(Item {
                role,
                content: ItemContent::Text { text: text.clone() },
                response_id: None,
            }),
            Value::Array(blocks) => {
                items.extend(blocks.iter().map(|block| block_item(role, block)))
            }
            other => panic!("content {other} in a fixture"),
        }
    }
    for item in &mut items {
        if item.role != Role::System {
            break;
        }
        item.role = Role::Developer;
    }
    items
}

fn is_budget_notice(content: &Value) -> bool {
    let text = match content {
        Value::String(text) => text.as_str(),
        Value::Array(blocks) if blocks.len() == 1 && blocks[0]["type"] == "text" => {
            blocks[0]["text"].as_str().unwrap_or_default()
        }
        _ => return false,
    };
    text.trim()
        .strip_prefix("<total_tokens>")
        .and_then(|rest| rest.strip_suffix("</total_tokens>"))
        .is_some_and(|inner| !inner.contains('<'))
}

fn block_item(role: Role, block: &Value) -> Item {
    let text = |field: &str| block[field].as_str().expect("string field").to_owned();
    let (role, content) = match block["type"].as_str().expect("typed block") {
        "text" => (role, ItemContent::Text { text: text("text") }),
        "tool_use" => (
            Role::Assistant,
            ItemContent::ToolCall {
                call_id: text("id"),
                name: text("name"),
                arguments: block["input"].to_string(),
                namespace: None,
            },
        ),
        "tool_result" => (
            Role::Tool,
            ItemContent::ToolResult {
                call_id: text("tool_use_id"),
                output: match block.get("content") {
                    Some(Value::String(text)) => text.clone(),
                    None | Some(Value::Null) => String::new(),
                    Some(other) => other.to_string(),
                },
            },
        ),
        other => (
            role,
            ItemContent::Opaque {
                block_type: other.to_owned(),
                block: block.clone(),
            },
        ),
    };
    Item {
        role,
        content,
        response_id: None,
    }
}
