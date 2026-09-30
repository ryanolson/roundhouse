// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Claude Code captures through the production canonicalizer, read by the
//! sequence-identity crate's detectors and keyers.
//!
//! These lived in `roundhouse-sequence-id`'s own suite over a hand-copied
//! canonicalizer, because the crate cannot depend on the server that depends on
//! it. That made them a test of the copy: making
//! `messages_api::wire::is_budget_notice` return `false` left every one of them
//! green, so the pinned divergence numbers below described a canonicalizer that
//! shipped nowhere. They are here so the chain that breaks (or holds) where
//! they say is the chain the server actually builds.
//!
//! Only what depends on a canonicalized fixture lives here. The crate keeps the
//! tests that need no canonicalizer — literals, headers, keyed known-answer
//! vectors — because those pin the crate's own behavior and cost nothing to
//! run beside it.

use std::collections::HashSet;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use roundhouse_core::item::chain::Chain;
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::sequence::TipKey;
use roundhouse_sequence_id::{
    Detection, TipKeyer, attribution_block, detect_client, new_anchor, tools_digest,
    without_attribution_block,
};
use roundhouse_server::messages_api::wire::{CreateMessageParams, canonicalize};
use serde_json::Value;

macro_rules! fixture {
    ($name:literal) => {
        ($name, include_str!(concat!("fixtures/", $name)))
    };
}

/// The seven Messages bodies. `claude-2.1.257-mcp-wire.json` holds MCP
/// requests, not Messages bodies, and is not one of them.
const BODIES: [(&str, &str); 7] = [
    fixture!("claude-2.1.251-turn-1.json"),
    fixture!("claude-2.1.251-turn-2-continue.json"),
    fixture!("claude-2.1.257-turn-1.json"),
    fixture!("claude-2.1.257-turn-2-continue.json"),
    fixture!("claude-2.1.257-turn-3-continue.json"),
    fixture!("claude-2.1.257-mcp-turn-1.json"),
    fixture!("claude-2.1.257-mcp-turn-2-toolresult.json"),
];

/// The three header captures, two requests each.
const HEADER_CAPTURES: [(&str, &str); 3] = [
    fixture!("claude-2.1.251-headers.json"),
    fixture!("claude-2.1.257-headers.json"),
    fixture!("claude-2.1.257-mcp-headers.json"),
];

const SECRET: &[u8] = b"deployment-secret";

fn raw(name: &str) -> &'static str {
    BODIES
        .iter()
        .find(|(fixture, _)| *fixture == name)
        .unwrap_or_else(|| panic!("no body fixture {name}"))
        .1
}

fn body(name: &str) -> Value {
    serde_json::from_str(raw(name)).expect("fixture parses")
}

/// The items the server builds from a capture: production `canonicalize`, over
/// the same `CreateMessageParams` the handler parses.
fn canonical(name: &str) -> Vec<Item> {
    let params: CreateMessageParams =
        serde_json::from_str(raw(name)).expect("a captured body is a well-formed request");
    canonicalize(&params).expect("a captured body canonicalizes")
}

fn fixture_chain(name: &str) -> Chain {
    Chain::over(&canonical(name))
}

/// The client version a fixture's file name carries.
fn version_of(name: &str) -> &str {
    name.strip_prefix("claude-")
        .and_then(|rest| rest.split('-').next())
        .expect("fixture names start claude-<version>-")
}

/// Every captured header set, named `<capture>#<request>`.
fn header_sets() -> Vec<(String, HeaderMap)> {
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

#[test]
fn claude_code_is_detected_exactly_from_the_fixtures() {
    let sets = header_sets();
    for (name, _) in BODIES {
        let items = canonical(name);
        let exact = Detection::Exact {
            version: version_of(name).to_owned(),
        };
        assert_eq!(detect_client(&HeaderMap::new(), &items), exact, "{name}");
        // Headers only declare, so they never outrank the block — not even
        // when the capture is of another version.
        for (set, headers) in &sets {
            assert_eq!(detect_client(headers, &items), exact, "{name} with {set}");
        }
        let block = attribution_block(&items).expect(name);
        assert_eq!(block.entrypoint, "sdk-cli");
        assert_eq!(block.fingerprint.len(), 3);
    }
}

#[test]
fn stripping_the_attribution_block_removes_item_zero_only() {
    for (name, _) in BODIES {
        let items = canonical(name);
        let stripped = without_attribution_block(&items);
        assert_eq!(stripped, &items[1..], "{name}");
        assert_eq!(attribution_block(stripped), None, "{name}: stripped twice");
    }
    // Not exact: the same slice back, every item kept.
    let near = [
        Item {
            role: Role::Developer,
            content: ItemContent::Text {
                text: "x-anthropic-billing-header: cc_version=2.1.257.1F2; cc_entrypoint=sdk-cli;"
                    .into(),
            },
            response_id: None,
        },
        Item::user_text("hi"),
    ];
    assert_eq!(without_attribution_block(&near), &near[..]);
    assert!(without_attribution_block(&[]).is_empty());
}

#[test]
fn two_principals_never_share_a_tip_key() {
    let chain = fixture_chain("claude-2.1.257-turn-3-continue.json");
    let tools = tools_digest(body("claude-2.1.257-turn-3-continue.json").get("tools"));
    let principals = ["", "tenant-a/", "tenant-b/", "tenant-a"];
    let mut seen: HashSet<[u8; 16]> = HashSet::new();
    for principal in principals {
        let keys = TipKeyer::new(SECRET, principal).tip_keys(&chain, &tools);
        assert_eq!(keys.len(), chain.len());
        for key in keys {
            assert!(
                seen.insert(key.0),
                "a tip key crossed principals at {principal:?}"
            );
        }
    }
    // Control: one principal keys one chain the same way twice, so the
    // distinctness above is about the principal and not about randomness.
    let a = TipKeyer::new(SECRET, "tenant-a/").tip_keys(&chain, &tools);
    let b = TipKeyer::new(SECRET, "tenant-a/").tip_keys(&chain, &tools);
    assert!(a == b);
    // And a second secret is a second key space.
    let other = TipKeyer::new(b"another-secret", "tenant-a/").tip_keys(&chain, &tools);
    assert!(a.iter().zip(&other).all(|(x, y)| x != y));
}

#[test]
fn claude_fixture_divergence_is_pinned() {
    let agreed = |a: &str, b: &str| fixture_chain(a).agreed_len(&fixture_chain(b));
    // Two sessions of one version, and two versions: item 0 is the
    // attribution block, whose fingerprint differs, so nothing is shared.
    assert_eq!(
        agreed(
            "claude-2.1.257-turn-1.json",
            "claude-2.1.257-mcp-turn-1.json"
        ),
        0
    );
    assert_eq!(
        agreed("claude-2.1.251-turn-1.json", "claude-2.1.257-turn-1.json"),
        0
    );
    // One session, turn 1 to turn 2: the client rewrote system block 2 (its
    // model line) between the two, so the lineage keeps two links.
    assert_eq!(
        agreed(
            "claude-2.1.257-turn-1.json",
            "claude-2.1.257-turn-2-continue.json"
        ),
        2
    );
    assert_eq!(
        agreed(
            "claude-2.1.251-turn-1.json",
            "claude-2.1.251-turn-2-continue.json"
        ),
        2
    );
    // Turn 2 to turn 3, and the tool loop, share everything the earlier
    // request had: the budget notice was dropped, not chained.
    let turn_two = fixture_chain("claude-2.1.257-turn-2-continue.json");
    assert_eq!(turn_two.len(), 8);
    assert_eq!(
        agreed(
            "claude-2.1.257-turn-2-continue.json",
            "claude-2.1.257-turn-3-continue.json"
        ),
        8
    );
    let tool_call = fixture_chain("claude-2.1.257-mcp-turn-1.json");
    assert_eq!(tool_call.len(), 6);
    assert_eq!(
        agreed(
            "claude-2.1.257-mcp-turn-1.json",
            "claude-2.1.257-mcp-turn-2-toolresult.json"
        ),
        6
    );
}

#[test]
fn the_anchor_is_the_last_tip_of_the_first_prompt() {
    let keyer = TipKeyer::new(SECRET, "");
    let items = canonical("claude-2.1.257-turn-1.json");
    let keys = keyer.tip_keys(&Chain::over(&items), &tools_digest(None));
    let anchor = new_anchor(&keys).expect("a non-empty prompt has an anchor");
    assert_eq!(anchor.0, keys.last().unwrap().0);
    assert!(new_anchor(&[] as &[TipKey]).is_none());
}
