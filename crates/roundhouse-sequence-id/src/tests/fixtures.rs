// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Claude Code captures, as raw bodies and header maps.
//!
//! **There is no canonicalizer here, on purpose.** A test copy of the server's
//! `messages_api::wire::canonicalize` used to live in this file, and the tests
//! built on it pinned the copy: making the production `is_budget_notice` return
//! `false` left them all green. Everything that needs a fixture *as items*
//! lives in `roundhouse-server/tests/sequence_identity_fixtures.rs`, which
//! calls the real one; what stays here reads only the raw JSON or the headers.

use http::{HeaderMap, HeaderName, HeaderValue};
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
