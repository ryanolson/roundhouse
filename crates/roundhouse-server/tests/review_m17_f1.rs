// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The documentation does not carry two stale sentences about control-call
//! recognition.
//!
//! The sentences were "nothing renders a tool call outbound any more" and "a
//! third party's MCP server offering its own `status` is exempted too" (with
//! "the Responses wire has dropped the namespace into a field canonicalization
//! discards"). Both describe a tree that no longer exists:
//! `responses_api::wire::function_call_item` re-emits the namespace on the
//! outbound projection, and `ControlCallDialect::CodexResponses::recognises`
//! matches a foreign `Some` namespace to `false` rather than exempting it.
//!
//! **Contract.** These sentences must not appear anywhere in the documentation:
//! `README.md` or any `*.md` under `docs/src/`. The two tests that call
//! `function_call_item` and `is_control_call_on` are controls: they exercise the
//! real behavior directly and establish that the sentences are false rather than
//! merely rephrased, so the absence checks cannot pass for want of a target. The
//! absence checks read the whole documentation corpus, so moving a sentence
//! between chapters does not hide it.

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

/// Every `*.md` file under `dir`, recursively, in path order so a failure
/// message is stable.
fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            markdown_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            out.push(path);
        }
    }
}

/// The whole documentation corpus, concatenated: `README.md` plus every `*.md`
/// under `docs/src/`. Read at run time so a chapter added later is covered
/// without editing this file.
fn read_docs() -> String {
    let root = repo_root();
    let mut files = vec![root.join("README.md")];
    markdown_files(&root.join("docs/src"), &mut files);
    files
        .iter()
        .map(|path| {
            fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    // field intact, which is the opposite of the stale "nothing renders a tool
    // call outbound any more".
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
    // code (and the documentation's description of it) did.
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
fn the_docs_do_not_claim_nothing_renders_a_tool_call_outbound() {
    let docs = unwrapped(&read_docs());
    assert!(
        !docs.contains("nothing renders a tool call outbound any more"),
        "the documentation still asserts, in the plain indicative, that nothing \
         renders a tool call outbound any more; the outbound projection \
         already does (F1)",
    );
}

#[test]
fn the_docs_do_not_claim_third_party_status_is_exempted() {
    let docs = unwrapped(&read_docs());
    assert!(
        !docs.contains("is exempted too"),
        "the documentation still asserts that a third party's MCP server offering \
         its own `status` is exempted from control-call recognition; the \
         CodexResponses arm rejects a foreign namespace outright (F1)",
    );
    assert!(
        !docs.contains("canonicalization discards"),
        "the documentation still asserts the Responses wire drops the namespace at \
         canonicalization; canonical_item reads it via optional_str instead (F1)",
    );
}
