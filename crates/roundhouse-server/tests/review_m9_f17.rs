// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Refuting evidence for review finding F17: `codex_launch`'s module doc
//! claims to be "what an operator hands a Codex client", but the workspace has
//! no shipped way to obtain the two files it generates -- no CLI subcommand in
//! `main.rs` (a bare `async fn main()`, no arg parsing at all), no HTTP route
//! among the five routers `main.rs` merges (`http`, `metrics_api`,
//! `admin_api`, `mcp_api`, `responses_api`). The only place in the workspace
//! that ever constructs a [`CodexLaunch`] is a test rig
//! (`tests/codex_e2e.rs:556`).
//!
//! This is a structural claim about *which files call the constructor*, not a
//! runtime behavior, so the test is a source scan rather than a client call:
//! it greps every non-test `.rs` file in this crate's `src/` (excluding
//! `codex_launch.rs` itself, which necessarily calls its own constructor from
//! its doc-tests/unit tests) for a call site of `CodexLaunch::new(`, skipping
//! lines that are doc comments (`///` or `//!`) since a doc comment citing the
//! type is not a production caller.

use std::fs;
use std::path::{Path, PathBuf};

/// Every `.rs` file anywhere under `src/`, found by walking the directory
/// rather than hand-listing it -- a new production module (or one nested a
/// level deeper, like `admin_api/mod.rs`) is covered automatically, which a
/// hard-coded or non-recursive list would silently miss.
fn src_files() -> Vec<PathBuf> {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    let mut stack = vec![src_dir.clone()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        {
            let path = entry
                .unwrap_or_else(|e| panic!("reading entry in {}: {e}", dir.display()))
                .path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// True if `line` is a real call site rather than a doc-comment mention: a
/// doc comment (`///` or `//!`, after trimming leading whitespace) is prose
/// about the type, not code the compiler executes.
fn is_production_call(line: &str) -> bool {
    let trimmed = line.trim_start();
    !trimmed.starts_with("///") && !trimmed.starts_with("//!") && line.contains("CodexLaunch::new(")
}

/// Control: `codex_launch.rs` itself must construct `CodexLaunch` (its own
/// unit tests do, at minimum) -- kept live so the scan machinery is proven to
/// find a call site it is pointed at, and the failing test below cannot be
/// dismissed as a scanner that finds nothing anywhere.
#[test]
fn a_codex_launch_rs_itself_constructs_the_type_it_defines() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/codex_launch.rs");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let hits: Vec<&str> = text.lines().filter(|l| is_production_call(l)).collect();
    assert!(
        !hits.is_empty(),
        "expected codex_launch.rs's own tests to construct CodexLaunch"
    );
}

/// F17: no file in `src/` *other than* `codex_launch.rs` -- i.e. no route
/// handler, no `main.rs` composition root, no other module -- ever
/// constructs a `CodexLaunch`. If this passes, some production module already
/// wires the generator to a surface an operator can reach, contradicting the
/// finding; if it fails, the finding's core claim (no shipped distribution
/// channel) is empirically confirmed for this revision.
#[test]
#[ignore = "F17: valid -- no production module (main.rs has no CLI subcommand \
            at all, and none of the five merged routers exposes a codex-config \
            route) ever constructs a CodexLaunch; only codex_launch.rs's own \
            unit tests and tests/codex_e2e.rs do. Removing this ignore is the \
            first step of the fix, not a cleanup after it."]
fn a_production_module_other_than_codex_launch_constructs_a_codex_launch() {
    let scanned: Vec<PathBuf> = src_files()
        .into_iter()
        .filter(|path| path.file_name().and_then(|n| n.to_str()) != Some("codex_launch.rs"))
        // Repo house rule: never read or edit a `*credential*` file. It is
        // also a no-op for this scan -- `credentials.rs` mints and stores
        // tenant secrets, nothing about launching a Codex client -- so
        // skipping it costs the test no coverage.
        .filter(|path| {
            !path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .contains("credential")
        })
        .collect();

    let hits: Vec<String> = scanned
        .iter()
        .flat_map(|path| {
            let text = fs::read_to_string(path).unwrap_or_default();
            let file_label = path.display().to_string();
            text.lines()
                .filter(|l| is_production_call(l))
                .map(|l| format!("{file_label}: {}", l.trim()))
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(
        !hits.is_empty(),
        "F17: no production module other than codex_launch.rs itself constructs a \
         CodexLaunch -- the module's opening doc (\"What an operator hands a Codex \
         client\") describes a distribution channel that does not exist. Scanned \
         files: {scanned:?}"
    );
}
