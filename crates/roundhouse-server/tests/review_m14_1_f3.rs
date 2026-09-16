// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.1 review, finding F3 — refuted, then fixed.
//!
//! **The claim.** The M14.1 rung pushed `conversations.rs` from 961 to 1031
//! lines, all of it in `mod tests` (opening at line 561/562), against the
//! crate's established convention of moving large test modules to a sibling
//! `tests.rs` — the same convention the M12.1 F3 precedent
//! (`review_m12_1_f3.rs`) proved against `mcp_api.rs`. The finding further
//! claims two hand-rolled `CorrelationMaps` test doubles inside that module,
//! `CountingMaps` and `OutageMaps`, each implement all six trait methods and
//! differ only in whether they count reads (delegating to a real
//! `MemoryCorrelationMaps`) or unconditionally return `Err`.
//!
//! **Every structural citation checked out exactly, at refute time.**
//! `becbb4f`'s (pre-M14.1) `conversations.rs` was 961 lines; `b9d4d12`'s
//! (the M14.1 rung itself) was 1031, with `#[cfg(test)]` at line 561 and
//! `mod tests {` at 562. Both `CountingMaps` and `OutageMaps` implemented
//! all six `CorrelationMaps` methods; `CountingMaps` incremented an
//! `AtomicUsize` then delegated to a wrapped `MemoryCorrelationMaps` in
//! every method, while `OutageMaps` returned `Err(outage())`
//! unconditionally in every method. `ls prefix_admission mcp_api` showed a
//! `tests.rs` sibling in each, and no `conversations/` sibling directory for
//! `conversations.rs`.
//!
//! **Ruling: valid — and fixed.** `conversations.rs` now declares
//! `#[cfg(test)]\nmod tests;` pointing at `conversations/tests.rs`, the same
//! pattern `prefix_admission.rs` and `mcp_api.rs` use, and `CountingMaps` /
//! `OutageMaps` are one `Double { inner, reads, outage }` whose `outage`
//! flag is the one axis the finding said they differed on. The pinned
//! before/after blob figures below are kept as the historical record of the
//! growth this fixes, and the rest of the suite checks the split and the
//! merged double directly against the working tree.

use std::process::Command;

const PINNED_COMMIT_BEFORE: &str = "becbb4fdf6477a94023efb61ea0e649a2280fc83"; // M13.1 thermo-nuclear-review commit, pre-M14.1
const PINNED_COMMIT_PEAK: &str = "b9d4d1244fd281a2314615dcfa5e2615bb812bbe"; // M14.1 rung itself, pre-fix
const RELATIVE_PATH: &str = "crates/roundhouse-server/src/conversations.rs";
const TESTS_SIBLING_PATH: &str = "crates/roundhouse-server/src/conversations/tests.rs";

fn repo_root() -> std::path::PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    // manifest_dir is crates/roundhouse-server; the workspace root is two
    // levels up.
    std::path::Path::new(manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/roundhouse-server has a workspace root two levels up")
        .to_path_buf()
}

/// `None` when the pinned commit is not reachable (e.g. a shallow clone) --
/// callers must skip pinned-line assertions rather than silently falling
/// back to the mutable working tree.
fn read_pinned_blob(commit: &str, relative_path: &str) -> Option<String> {
    let output = Command::new("git")
        .current_dir(repo_root())
        .arg("show")
        .arg(format!("{commit}:{relative_path}"))
        .output()
        .expect("git is available in this environment");
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).expect("source files are UTF-8"))
}

/// F3's growth numbers, kept as the historical record rather than asserted
/// against the working tree: 961 lines at the pre-M14.1 commit `becbb4f`, to
/// 1031 lines at the M14.1 rung's own commit `b9d4d12`, with `mod tests {`
/// opening at line 562 (`#[cfg(test)]` at 561) in that peak -- the exact
/// figures the finding cited, from the commit the finding was filed against.
#[test]
fn the_1000_line_crossing_was_the_m14_1_rungs_own_growth() {
    let (Some(before), Some(peak)) = (
        read_pinned_blob(PINNED_COMMIT_BEFORE, RELATIVE_PATH),
        read_pinned_blob(PINNED_COMMIT_PEAK, RELATIVE_PATH),
    ) else {
        eprintln!(
            "F3: a pinned commit is unreachable (shallow clone?) -- skipping \
             the pinned-line assertions this test exists to make"
        );
        return;
    };
    let before_lines: Vec<&str> = before.lines().collect();
    assert_eq!(
        before_lines.len(),
        961,
        "F3: conversations.rs at the pre-M14.1 commit was claimed to be 961 lines"
    );

    let peak_lines: Vec<&str> = peak.lines().collect();
    assert_eq!(
        peak_lines.len(),
        1031,
        "F3: conversations.rs at the M14.1 rung's own commit was claimed to be \
         1031 lines, crossing the 1000-line threshold"
    );
    let cfg_test_pos = peak_lines
        .iter()
        .position(|l| l.trim() == "#[cfg(test)]")
        .expect("F3: the pinned peak should have a #[cfg(test)] attribute");
    let mod_tests_pos = peak_lines
        .iter()
        .position(|l| l.trim() == "mod tests {")
        .expect("F3: the pinned peak should have exactly one `mod tests {`");
    assert!(
        cfg_test_pos + 1 >= 561,
        "F3: #[cfg(test)] was claimed to be at line 561, past the module's \
         own half of the file"
    );
    assert_eq!(
        mod_tests_pos,
        cfg_test_pos + 1,
        "F3: mod tests {{ was claimed to open right after the attribute, \
         inline rather than as a `mod tests;` pointing at a sibling"
    );
}

/// **The fix, over the working tree.** `conversations.rs` no longer carries
/// the test module inline: it declares `#[cfg(test)]\nmod tests;` pointing
/// at a sibling `conversations/tests.rs`, the file has shrunk well below the
/// M14.1 rung's own 1031-line peak, and there is exactly one `mod tests`
/// declaration.
#[test]
fn conversations_rs_no_longer_carries_the_test_module_inline() {
    let src = std::fs::read_to_string(repo_root().join(RELATIVE_PATH))
        .expect("F3: conversations.rs should be readable");
    let lines: Vec<&str> = src.lines().collect();
    assert!(
        lines.len() < 1031,
        "F3: conversations.rs should have shrunk once its test module moved \
         to a sibling file -- it is {} lines, not below the M14.1 rung's own \
         1031-line peak",
        lines.len()
    );
    assert!(
        !src.contains("mod tests {"),
        "F3: conversations.rs should no longer write `mod tests {{ .. }}` inline"
    );
    assert_eq!(
        src.matches("mod tests;").count(),
        1,
        "F3: conversations.rs should declare `mod tests;` exactly once"
    );
    assert!(
        repo_root().join(TESTS_SIBLING_PATH).is_file(),
        "F3: conversations.rs declares `mod tests;` but {TESTS_SIBLING_PATH} \
         does not exist"
    );
}

/// F3's sibling-pattern claim, extended past the M12.1 precedent's four
/// examples to the two the finding itself cites: `prefix_admission.rs` and
/// `mcp_api.rs` both use `#[cfg(test)] mod tests;` (a semicolon, pointing at
/// a sibling `tests.rs`), the pattern `conversations.rs` was expected to
/// follow.
#[test]
fn cited_sibling_modules_already_use_the_split_test_module_pattern() {
    let siblings = [
        (
            "crates/roundhouse-server/src/prefix_admission.rs",
            "crates/roundhouse-server/src/prefix_admission/tests.rs",
        ),
        (
            "crates/roundhouse-server/src/mcp_api.rs",
            "crates/roundhouse-server/src/mcp_api/tests.rs",
        ),
    ];

    for (parent, sibling) in siblings {
        let parent_path = repo_root().join(parent);
        let sibling_path = repo_root().join(sibling);
        let parent_src = std::fs::read_to_string(&parent_path)
            .unwrap_or_else(|e| panic!("F3: {parent} should be readable: {e}"));

        assert!(
            parent_src.contains("#[cfg(test)]\nmod tests;"),
            "F3: {parent} was claimed to declare `#[cfg(test)] mod tests;` \
             pointing at a sibling file"
        );
        assert!(
            sibling_path.is_file(),
            "F3: {parent} declares `mod tests;` but {sibling} does not exist"
        );
    }
}

/// F3's claim about the two test doubles, at the pinned peak commit: they
/// each implemented all six `CorrelationMaps` methods, and differed only in
/// whether they counted (delegated, incrementing a counter) or failed
/// (unconditional `Err`) -- not in any other behavioral dimension. Read from
/// `PINNED_COMMIT_PEAK` rather than the working tree, since the fix below
/// merges them into one `Double` and this is a claim about the pre-fix shape.
#[test]
fn the_two_test_doubles_differed_only_in_counting_versus_failing() {
    let Some(src) = read_pinned_blob(PINNED_COMMIT_PEAK, RELATIVE_PATH) else {
        eprintln!(
            "F3: pinned commit {PINNED_COMMIT_PEAK} unreachable (shallow clone?) \
             -- skipping the pinned-line assertions this test exists to make"
        );
        return;
    };

    let counting_impl_start = src
        .find("impl CorrelationMaps for CountingMaps")
        .expect("F3: CountingMaps should implement CorrelationMaps");
    let outage_impl_start = src
        .find("impl CorrelationMaps for OutageMaps")
        .expect("F3: OutageMaps should implement CorrelationMaps");
    assert!(
        outage_impl_start > counting_impl_start,
        "F3: expected CountingMaps's impl block to precede OutageMaps's"
    );

    let counting_block = &src[counting_impl_start..outage_impl_start];
    let outage_block = &src[outage_impl_start..];

    let methods = [
        "generation",
        "set_generation",
        "bind_call",
        "session_of_call",
        "bind_thread",
        "session_of_thread",
    ];
    for method in methods {
        assert!(
            counting_block.contains(&format!("async fn {method}")),
            "F3: CountingMaps was claimed to implement all six trait methods, \
             missing {method}"
        );
        assert!(
            outage_block.contains(&format!("async fn {method}")),
            "F3: OutageMaps was claimed to implement all six trait methods, \
             missing {method}"
        );
    }

    // CountingMaps increments a counter then delegates to a wrapped real
    // map in every method; it never itself returns Err.
    assert!(
        counting_block.contains("reads.fetch_add(1"),
        "F3: CountingMaps was claimed to count reads via fetch_add"
    );
    assert!(
        !counting_block.contains("Err(outage())"),
        "F3: CountingMaps was claimed to always delegate, never fail"
    );

    // OutageMaps returns Err(outage()) unconditionally in every method and
    // never delegates to a real map or counts anything.
    let outage_err_count = outage_block.matches("Err(outage())").count();
    assert_eq!(
        outage_err_count, 6,
        "F3: OutageMaps was claimed to return Err(outage()) in exactly its \
         six trait methods"
    );
    assert!(
        !outage_block.contains("reads.fetch_add"),
        "F3: OutageMaps was claimed to never count reads"
    );
}

/// The red assertion, now green: F3's normative claim was that
/// `conversations.rs` should follow the same sibling-file convention
/// `prefix_admission.rs` and `mcp_api.rs` already use, and that the two
/// doubles should be one. Both are true now: `mod tests;` points at
/// `conversations/tests.rs`, and that sibling defines a single `Double`
/// whose `outage` field is the one axis `CountingMaps`/`OutageMaps` used to
/// differ on -- counting reads exactly where the old `CountingMaps` did, and
/// refusing every method exactly where the old `OutageMaps` did, gated by
/// one flag rather than duplicated across two structs.
#[test]
fn conversations_follows_the_crates_own_split_test_module_convention() {
    let path = repo_root().join(RELATIVE_PATH);
    let src = std::fs::read_to_string(&path).expect("F3: conversations.rs should be readable");
    assert!(
        src.contains("#[cfg(test)]\nmod tests;"),
        "F3: conversations.rs should declare `#[cfg(test)] mod tests;` \
         (pointing at a sibling crates/roundhouse-server/src/conversations/tests.rs), \
         the same pattern prefix_admission.rs and mcp_api.rs already use -- \
         instead it writes the module inline"
    );

    let sibling = std::fs::read_to_string(repo_root().join(TESTS_SIBLING_PATH))
        .expect("F3: conversations/tests.rs should be readable");
    assert!(
        !sibling.contains("struct CountingMaps") && !sibling.contains("struct OutageMaps"),
        "F3: CountingMaps and OutageMaps should have been merged into one double"
    );
    let double_impl_start = sibling
        .find("impl CorrelationMaps for Double")
        .expect("F3: conversations/tests.rs should define one Double implementing CorrelationMaps");
    let double_block = &sibling[double_impl_start..];

    let methods = [
        "generation",
        "set_generation",
        "bind_call",
        "session_of_call",
        "bind_thread",
        "session_of_thread",
    ];
    for method in methods {
        assert!(
            double_block.contains(&format!("async fn {method}")),
            "F3: Double should implement all six trait methods, missing {method}"
        );
    }
    assert!(
        double_block.contains("reads.fetch_add(1"),
        "F3: Double should count generation reads the way CountingMaps did"
    );
    assert_eq!(
        double_block.matches("Err(outage())").count(),
        6,
        "F3: Double should refuse every one of its six methods behind the \
         outage flag, the way OutageMaps unconditionally did"
    );
}
