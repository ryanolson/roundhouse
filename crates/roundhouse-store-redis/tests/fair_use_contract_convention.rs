// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Refuter test for M13.1 review finding F1: `fair_use_contract.rs` grew past
//! the crate's own convention of one themed test file per concern (see
//! `recovery.rs`'s doc comment, which quarantines connection-loss tests into
//! their own binary for exactly this reason) and copy-pastes the same
//! raw-connection helper instead of sharing it from `common`.
//!
//! No live Redis is needed: both assertions are about the shape of the
//! `tests/` directory, not runtime behavior, so this is a plain `#[test]`
//! rather than one of the crate's `#[ignore]`-gated real-backend tests.
//!
//! F1 closed: the decay/prune/expiry themed tests moved to
//! `fair_use_decay.rs`, the raw-storage and commandstats-loop tests to
//! `fair_use_storage.rs`, and the one `raw_from_env` both need (alongside
//! `spend_contract.rs`) now lives once in `tests/common`, so this test runs
//! ungated rather than staying `#[ignore]`d evidence of a defect nobody
//! closed.

use std::fs;
use std::path::Path;

#[test]
fn fair_use_contract_stays_within_the_crates_sibling_file_convention() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let tests_dir = Path::new(manifest_dir).join("tests");

    let fair_use_path = tests_dir.join("fair_use_contract.rs");
    let fair_use_src = fs::read_to_string(&fair_use_path)
        .unwrap_or_else(|e| panic!("reading {fair_use_path:?}: {e}"));
    let fair_use_lines = fair_use_src.lines().count();

    // Every other file in this crate's tests/ tops out at 413 lines
    // (spend_contract.rs) — until the split this test guards added two more
    // themed siblings of fair_use_contract.rs itself, both comfortably under
    // this ceiling too. recovery.rs's doc comment is the crate's own stated
    // precedent for splitting a themed slice of gated tests into its own
    // binary with shared fixtures in tests/common.
    const CONVENTION_CEILING: usize = 900;

    // Scans this crate's tests/*.rs (not tests/common/, which is where the
    // one real definition is meant to live, uncounted): a copy pasted into a
    // sibling *.rs is exactly the defect F1 named, and the canonical
    // `tests/common::raw_from_env` living outside the scan is what makes a
    // count of zero real copies here the passing state, not one. The match
    // pattern below is, itself, source text this same scan reads back, so
    // the scan always finds one occurrence of its own search string in this
    // very file — the ceiling of 1 is calibrated to that self-match, not to
    // tolerating one real copy.
    let mut raw_from_env_copies = 0usize;
    for entry in fs::read_dir(&tests_dir).expect("tests dir exists") {
        let entry = entry.expect("readable dir entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = fs::read_to_string(&path).unwrap_or_default();
        raw_from_env_copies += src.matches("async fn raw_from_env").count();
    }

    assert!(
        fair_use_lines <= CONVENTION_CEILING && raw_from_env_copies <= 1,
        "F1: fair_use_contract.rs is {fair_use_lines} lines (convention ceiling \
         {CONVENTION_CEILING}, based on this crate's other tests/*.rs files) and \
         `raw_from_env` is defined verbatim {raw_from_env_copies} times across tests/*.rs \
         (1 of which is this guard's own source matching its own search string), \
         instead of following this crate's own convention — demonstrated by \
         recovery.rs — of a themed sibling file with shared fixtures in \
         tests/common."
    );
}
