// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use roundhouse_core::item::{Item, ItemContent, Role};

use super::fixtures::header_sets;
use super::labels::headers;
use crate::{
    AttributionBlock, DeclaredClient, Detection, attribution_block, detect_client,
    without_attribution_block,
};

const EXACT: &str = "x-anthropic-billing-header: cc_version=2.1.257.1f2; cc_entrypoint=sdk-cli;";

fn developer(text: &str) -> Item {
    Item {
        role: Role::Developer,
        content: ItemContent::Text { text: text.into() },
        response_id: None,
    }
}

/// What the headers alone declare: a request whose prompt carries no
/// attribution block, so nothing can be exact.
fn detect(pairs: &[(&'static str, &str)]) -> Detection {
    detect_client(&headers(pairs), &[Item::user_text("hi")])
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
fn a_user_agent_alone_is_declared_not_exact() {
    let no_block = [Item::user_text("hi")];
    for (set, captured) in header_sets() {
        let version = set
            .strip_prefix("claude-")
            .and_then(|rest| rest.split('-').next())
            .unwrap();
        assert_eq!(
            detect_client(&captured, &no_block),
            Detection::Declared(DeclaredClient::ClaudeCode {
                version: Some(version.to_owned()),
            }),
            "{set}"
        );
    }
    let declared = detect;
    assert_eq!(
        declared(&[("x-app", "cli")]),
        Detection::Declared(DeclaredClient::ClaudeCode { version: None })
    );
    let codex = Detection::Declared(DeclaredClient::Codex);
    assert_eq!(declared(&[("originator", "codex_cli_rs")]), codex);
    assert_eq!(
        declared(&[("user-agent", "codex_cli_rs/0.146.0 (Linux)")]),
        codex
    );
    assert_eq!(declared(&[("x-codex-window-id", "t:0")]), codex);
    let nothing = Detection::NoSignal;
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

/// Mutation 12f: `all_of(part, is_digit)` -> `part.bytes().all(is_digit)`
/// lets an empty version component through.
#[test]
fn an_empty_version_component_is_not_an_attribution_block() {
    for version in ["2..257", ".1.257", "2.1.", "2.1..1f2"] {
        let text =
            format!("x-anthropic-billing-header: cc_version={version}.1f2; cc_entrypoint=sdk-cli;");
        let item = Item {
            role: Role::Developer,
            content: ItemContent::Text { text: text.clone() },
            response_id: None,
        };
        assert_eq!(attribution_block(&[item]), None, "{text:?}");
    }
}

/// Mutations 13e, 13g, 13h: `x-app` is `cli` exactly, an empty `claude-cli/`
/// version is no version, and an `originator` must start `codex_`.
#[test]
fn a_declaration_must_match_its_literal() {
    let nothing = Detection::NoSignal;
    assert_eq!(detect(&[("x-app", "web")]), nothing);
    assert_eq!(detect(&[("x-app", "")]), nothing);
    assert_eq!(detect(&[("originator", "acme_cli")]), nothing);
    assert_eq!(detect(&[("originator", "codex")]), nothing);
    let versionless = Detection::Declared(DeclaredClient::ClaudeCode { version: None });
    assert_eq!(detect(&[("user-agent", "claude-cli/")]), versionless);
    assert_eq!(
        detect(&[("user-agent", "claude-cli/ (external)")]),
        versionless
    );
}

/// **The block outranks every header**, because it is the client's own
/// per-request fingerprint and a header is only a claim — so a Codex
/// `originator` beside a Claude Code block is still exactly Claude Code, and
/// only the exact case strips item 0.
#[test]
fn an_exact_block_outranks_a_declared_header_and_alone_is_stripped() {
    let items = [developer(EXACT), Item::user_text("hi")];
    assert_eq!(
        detect_client(&headers(&[("originator", "codex_cli_rs")]), &items),
        Detection::Exact {
            version: "2.1.257".into()
        }
    );
    assert_eq!(without_attribution_block(&items), &items[1..]);
    let unblocked = [developer("You are Claude Code."), Item::user_text("hi")];
    assert_eq!(without_attribution_block(&unblocked), &unblocked[..]);
}
