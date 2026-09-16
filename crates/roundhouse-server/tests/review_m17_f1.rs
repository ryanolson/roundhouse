// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M17 thermo-nuclear review, finding F1 — confirmed.
//!
//! **The claim.** README.md's "Control tools from Claude Code" section
//! (around line 702) says "nothing renders a tool call outbound any more",
//! and the "Escalation" section (around line 333) says the Responses wire
//! "has dropped the namespace into a field canonicalization discards" and
//! that "a third party's MCP server offering its own `status` is exempted
//! too". Both sentences are pre-M17: `responses_api::wire::function_call_item`
//! now re-emits the namespace on the outbound projection, and
//! `ControlCallDialect::CodexResponses::recognises` no longer exempts a
//! foreign `Some` namespace — it is matched to `false`, not swallowed. The
//! README states both in the plain indicative, with no dated bracket noting
//! they describe the base tree rather than HEAD.
//!
//! **This test.** The two `assert!`s tagged CONTROL below exercise the real
//! HEAD behavior directly (no mutation, no doc-parsing needed to prove the
//! code side) and pass today — that is what establishes the two README
//! sentences are stale rather than merely re-phrased. The two `assert!`s
//! tagged CLAIM read README.md itself and fail today, because both stale
//! sentences are still present verbatim. Marked `#[ignore]` per the fix
//! contract: an ignored test enforces nothing, and removing the ignore is
//! the first step of the actual fix (a dated bracketed note per the
//! finding's own `how_to_prove`, not a rewrite).

use std::fs;
use std::path::{Path, PathBuf};

use roundhouse_core::validate::{CONTROL_TOOL_NAMESPACE, ControlCallDialect, is_control_call_on};
use roundhouse_server::responses_api::wire::function_call_item;

/// Repo root, derived from this crate's `CARGO_MANIFEST_DIR`
/// (`crates/roundhouse-server`) rather than hard-coded, so the test does not
/// depend on the working directory `cargo test` was invoked from.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/roundhouse-server has two ancestors up to the repo root")
        .to_path_buf()
}

fn read_readme() -> String {
    let path = repo_root().join("README.md");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Markdown hand-wraps at ~80 columns, so a sentence this test greps for can
/// straddle a line break in the source file even though it reads as one run
/// of text. Collapse newlines to spaces before searching so the search is
/// insensitive to exactly where the wrap happens to fall today.
fn unwrapped(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn head_outbound_projection_now_renders_the_namespace() {
    // CONTROL (passes): since M17, `function_call_item` — the Responses
    // outbound projection — re-emits a carried namespace rather than
    // dropping it. Something *does* render a tool call outbound with the
    // field intact, which is the opposite of README.md's "nothing renders a
    // tool call outbound any more".
    let item = function_call_item("call_1", "status", Some(CONTROL_TOOL_NAMESPACE), "{}");
    assert_eq!(
        item.get("namespace").and_then(|v| v.as_str()),
        Some(CONTROL_TOOL_NAMESPACE),
        "function_call_item should re-emit the namespace on the outbound \
         projection (M17); this is the code half of F1's contradiction",
    );
}

#[test]
fn head_no_longer_exempts_a_foreign_namespace_status_call() {
    // CONTROL (passes): a third party's own `status` tool, under a foreign
    // namespace, is recognised as *not* ours — `Some(_) => false` — rather
    // than being swallowed by the bare-name fallback the way the pre-M17
    // code (and the README's description of it) did.
    assert!(
        !is_control_call_on(
            "status",
            Some("mcp__someone_else"),
            ControlCallDialect::CodexResponses,
        ),
        "control_call.rs's CodexResponses arm should reject a foreign \
         `Some` namespace outright (M17, R-N9); this is the code half of \
         F1's contradiction",
    );
}

#[test]
fn readme_no_longer_claims_nothing_renders_a_tool_call_outbound() {
    let readme = unwrapped(&read_readme());
    assert!(
        !readme.contains("nothing renders a tool call outbound any more"),
        "README.md still asserts, in the plain indicative, that nothing \
         renders a tool call outbound any more; HEAD's outbound projection \
         already does (F1)",
    );
}

#[test]
fn readme_no_longer_claims_third_party_status_is_exempted() {
    let readme = unwrapped(&read_readme());
    assert!(
        !readme.contains("is exempted too"),
        "README.md still asserts that a third party's MCP server offering \
         its own `status` is exempted from control-call recognition; HEAD's \
         CodexResponses arm rejects a foreign namespace outright since M17 \
         (F1)",
    );
    assert!(
        !readme.contains("canonicalization discards"),
        "README.md still asserts the Responses wire drops the namespace at \
         canonicalization; HEAD's canonical_item reads it via optional_str \
         instead (F1)",
    );
}
