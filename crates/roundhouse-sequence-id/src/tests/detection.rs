// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use http::HeaderMap;
use roundhouse_core::item::{Item, ItemContent, Role};

use super::fixtures::{BODIES, body, canonical, header_sets, version_of};
use super::labels::headers;
use crate::{
    AttributionBlock, Client, Confidence, Detection, RequestView, Surface, attribution_block,
    detect_client, without_attribution_block,
};

const EXACT: &str = "x-anthropic-billing-header: cc_version=2.1.257.1f2; cc_entrypoint=sdk-cli;";

fn developer(text: &str) -> Item {
    Item {
        role: Role::Developer,
        content: ItemContent::Text { text: text.into() },
        response_id: None,
    }
}

fn detect(headers: &HeaderMap, items: &[Item]) -> Detection {
    detect_client(&RequestView {
        surface: Surface::AnthropicMessages,
        headers,
        items,
        tools: None,
        metadata_user_id: None,
        prompt_cache_key: None,
    })
}

#[test]
fn the_attribution_block_is_detected_only_on_exact_match() {
    let items = [developer(EXACT), Item::user_text("hi")];
    assert_eq!(
        attribution_block(&items),
        Some(AttributionBlock {
            version: "2.1.257",
            fingerprint: "1f2",
            entrypoint: "sdk-cli",
        })
    );
    let near_misses = [
        // A changed prefix.
        "x-anthropic-billing-header:cc_version=2.1.257.1f2; cc_entrypoint=sdk-cli;",
        "X-anthropic-billing-header: cc_version=2.1.257.1f2; cc_entrypoint=sdk-cli;",
        // The closing `;` missing, or something after it.
        "x-anthropic-billing-header: cc_version=2.1.257.1f2; cc_entrypoint=sdk-cli",
        "x-anthropic-billing-header: cc_version=2.1.257.1f2; cc_entrypoint=sdk-cli;\n",
        // An extra field, in either place.
        "x-anthropic-billing-header: cc_version=2.1.257.1f2; cc_entrypoint=sdk-cli; cc_workload=x;",
        "x-anthropic-billing-header: cc_version=2.1.257.1f2; cc_extra=1; cc_entrypoint=sdk-cli;",
        // A fingerprint that is not three lowercase hex digits.
        "x-anthropic-billing-header: cc_version=2.1.257.1g2; cc_entrypoint=sdk-cli;",
        "x-anthropic-billing-header: cc_version=2.1.257.1F2; cc_entrypoint=sdk-cli;",
        "x-anthropic-billing-header: cc_version=2.1.257.1f23; cc_entrypoint=sdk-cli;",
        // A version that is not three dotted numbers.
        "x-anthropic-billing-header: cc_version=2.1.1f2; cc_entrypoint=sdk-cli;",
        "x-anthropic-billing-header: cc_version=2.1.x.1f2; cc_entrypoint=sdk-cli;",
        // An entrypoint outside `[a-z0-9_-]+`.
        "x-anthropic-billing-header: cc_version=2.1.257.1f2; cc_entrypoint=;",
        "x-anthropic-billing-header: cc_version=2.1.257.1f2; cc_entrypoint=SDK;",
    ];
    for text in near_misses {
        assert_eq!(attribution_block(&[developer(text)]), None, "{text:?}");
    }
    // The exact text, but not at item 0, or not as configuration.
    assert_eq!(
        attribution_block(&[Item::user_text("hi"), developer(EXACT)]),
        None
    );
    assert_eq!(attribution_block(&[Item::user_text(EXACT)]), None);
    assert_eq!(attribution_block(&[Item::system_text(EXACT)]), None);
    assert_eq!(attribution_block(&[]), None);
}

#[test]
fn claude_code_is_detected_exactly_from_the_fixtures() {
    let sets = header_sets();
    for (name, _) in BODIES {
        let items = canonical(&body(name));
        let exact = Detection {
            client: Client::ClaudeCode {
                version: Some(version_of(name).to_owned()),
            },
            confidence: Confidence::Exact,
        };
        assert_eq!(detect(&HeaderMap::new(), &items), exact, "{name}");
        // Headers only declare, so they never outrank the block — not even
        // when the capture is of another version.
        for (set, headers) in &sets {
            assert_eq!(detect(headers, &items), exact, "{name} with {set}");
        }
        let block = attribution_block(&items).expect(name);
        assert_eq!(block.entrypoint, "sdk-cli");
        assert_eq!(block.fingerprint.len(), 3);
    }
}

#[test]
fn a_user_agent_alone_is_declared_not_exact() {
    let no_block = [Item::user_text("hi")];
    for (set, captured) in header_sets() {
        let version = set
            .strip_prefix("claude-")
            .and_then(|rest| rest.split('-').next())
            .unwrap();
        assert_eq!(
            detect(&captured, &no_block),
            Detection {
                client: Client::ClaudeCode {
                    version: Some(version.to_owned()),
                },
                confidence: Confidence::Declared,
            },
            "{set}"
        );
    }
    let declared = |pairs: &[(&'static str, &str)]| detect(&headers(pairs), &no_block);
    assert_eq!(
        declared(&[("x-app", "cli")]),
        Detection {
            client: Client::ClaudeCode { version: None },
            confidence: Confidence::Declared,
        }
    );
    let codex = Detection {
        client: Client::Codex,
        confidence: Confidence::Declared,
    };
    assert_eq!(declared(&[("originator", "codex_cli_rs")]), codex);
    assert_eq!(
        declared(&[("user-agent", "codex_cli_rs/0.146.0 (Linux)")]),
        codex
    );
    assert_eq!(declared(&[("x-codex-window-id", "t:0")]), codex);
    let nothing = Detection {
        client: Client::Unknown,
        confidence: Confidence::NoSignal,
    };
    assert_eq!(declared(&[]), nothing);
    assert_eq!(declared(&[("user-agent", "curl/8.0")]), nothing);
    // Two declarations naming different clients cancel rather than pick.
    assert_eq!(
        declared(&[
            ("user-agent", "claude-cli/2.1.257 (external, cli)"),
            ("originator", "codex_cli_rs")
        ]),
        nothing
    );
}

#[test]
fn stripping_the_attribution_block_removes_item_zero_only() {
    for (name, _) in BODIES {
        let items = canonical(&body(name));
        let stripped = without_attribution_block(&items);
        assert_eq!(stripped, &items[1..], "{name}");
        assert_eq!(attribution_block(stripped), None, "{name}: stripped twice");
    }
    // Not exact: the same slice back, every item kept.
    let near = [
        developer(&EXACT.replace("1f2", "1F2")),
        Item::user_text("hi"),
    ];
    assert_eq!(without_attribution_block(&near), &near[..]);
    assert!(without_attribution_block(&[]).is_empty());
}
