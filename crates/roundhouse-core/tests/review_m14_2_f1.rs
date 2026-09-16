// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.2 thermo-nuclear review, finding F1 — refuted.
//!
//! **The claim.** The M14.2 rung pushed `correlation.rs` from 632 to 1029
//! lines (+397), crossing the skill's presumptive 1000-line blocker, while
//! the crate already had two house decompositions available and used
//! neither: `correlation/contract.rs` already exists as a sibling module in
//! the same directory, and `roundhouse-server/src/conversations.rs` already
//! keeps its test module out-of-line as `mod tests;` pointing at a sibling
//! `conversations/tests.rs`. The result is one file holding the trait, both
//! backends' tables, and their tests together.
//!
//! **Every citation checked out exactly.** `git show 2d79bb6` (the M14.1
//! review commit, immediately prior to the M14.2 rung) has `correlation.rs`
//! at 632 lines with `mod tests {` already inline at line 502 — so the
//! out-of-line convention was already being skipped before M14.2, and M14.2
//! grew the same inline module rather than splitting it. At `94d0904` (the
//! M14.2 rung's own commit, `HEAD` at review time) the file is 1029 lines:
//! `pub trait CorrelationMaps` at 203, `pub struct MemoryCorrelationMaps` at
//! 286 opening the implementation, and `#[cfg(test)]`/`mod tests {` at
//! 735/736 running to the end of the file (294 lines of tests). `ls
//! crates/roundhouse-core/src/control/correlation/` shows `contract.rs`
//! (407 lines) already living as a sibling, wired in via `pub mod contract;`
//! at line 102 of `correlation.rs` itself — so the sibling-module idiom is
//! not just available elsewhere in the workspace, it is already in use one
//! `mod` declaration away in the very file that grew past 1000 lines inline.
//! `conversations.rs:775` is `mod tests;` (a semicolon, not a brace),
//! pointing at `conversations/tests.rs`, confirming the out-of-line test
//! convention the finding cites as the second unused option.
//!
//! **No Redis is involved.** This finding is about static file layout —
//! line counts and module wiring — not runtime behavior against either
//! backend, so the assertions below read git blobs and the working tree
//! directly; no server, no store, no `ROUNDHOUSE_TEST_REDIS_URL` needed.
//!
//! **Ruling: valid, and closed.** The split landed: the memory
//! implementation moved to `correlation/memory.rs`, the tests to
//! `correlation/tests.rs`, and `correlation.rs` is back under the threshold
//! with `mod tests;` rather than an inline module. The two controls below
//! still read the pinned commits, so the finding's premises stay checked
//! against the revision they were about;
//! [`correlation_rs_follows_its_own_sibling_test_convention`] is no longer
//! ignored and is what stops the file growing back.
//!
//! [`the_1000_line_crossing_was_the_m14_2_rungs_own_growth`]
//! and [`contract_rs_already_exists_as_the_sibling_the_finding_names`] are
//! passing controls: they confirm the finding's factual premises against the
//! pinned commits.

use std::process::Command;

/// The M14.2 rung's own commit — `HEAD` at review time.
const PINNED_COMMIT_PEAK: &str = "94d09049197dc00604ddd6d3c85cf7ef15bb4e38";
/// The M14.1 thermo-nuclear review commit, immediately prior to the M14.2 rung.
const PINNED_COMMIT_BEFORE: &str = "2d79bb6";
const RELATIVE_PATH: &str = "crates/roundhouse-core/src/control/correlation.rs";
const CONTRACT_SIBLING_PATH: &str = "crates/roundhouse-core/src/control/correlation/contract.rs";
const CONVERSATIONS_PATH: &str = "crates/roundhouse-server/src/conversations.rs";

fn repo_root() -> std::path::PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    // manifest_dir is crates/roundhouse-core; the workspace root is two
    // levels up.
    std::path::Path::new(manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/roundhouse-core has a workspace root two levels up")
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

/// F1's growth numbers, read from pinned git blobs rather than the working
/// tree: 632 lines at the pre-M14.2 commit `2d79bb6`, already with `mod
/// tests {` inline at that point, to 1029 lines at the M14.2 rung's own
/// commit `94d0904` -- the exact figures the finding cited.
#[test]
fn the_1000_line_crossing_was_the_m14_2_rungs_own_growth() {
    let (Some(before), Some(peak)) = (
        read_pinned_blob(PINNED_COMMIT_BEFORE, RELATIVE_PATH),
        read_pinned_blob(PINNED_COMMIT_PEAK, RELATIVE_PATH),
    ) else {
        eprintln!(
            "F1: a pinned commit is unreachable (shallow clone?) -- skipping \
             the pinned-line assertions this test exists to make"
        );
        return;
    };

    let before_lines: Vec<&str> = before.lines().collect();
    assert_eq!(
        before_lines.len(),
        632,
        "F1: correlation.rs at the pre-M14.2 commit was claimed to be 632 lines"
    );
    assert!(
        before_lines.iter().any(|l| l.trim() == "mod tests {"),
        "F1: correlation.rs before M14.2 was claimed to already have `mod \
         tests {{` inline (not yet split), which is why growing it further \
         inline rather than splitting it is the finding's complaint"
    );

    let peak_lines: Vec<&str> = peak.lines().collect();
    assert_eq!(
        peak_lines.len(),
        1029,
        "F1: correlation.rs at the M14.2 rung's own commit was claimed to be \
         1029 lines, crossing the 1000-line threshold"
    );

    let trait_pos = peak_lines
        .iter()
        .position(|l| l.trim_start().starts_with("pub trait CorrelationMaps"))
        .expect("F1: the pinned peak should declare the CorrelationMaps trait");
    let struct_pos = peak_lines
        .iter()
        .position(|l| {
            l.trim_start()
                .starts_with("pub struct MemoryCorrelationMaps")
        })
        .expect("F1: the pinned peak should declare MemoryCorrelationMaps");
    // There are earlier `#[cfg(test)]` attributes in the file (on individual
    // test-only helper methods inside the table impls) -- the one that
    // matters here is the one immediately gating the top-level `mod tests {`.
    let mod_tests_pos = peak_lines
        .iter()
        .position(|l| l.trim() == "mod tests {")
        .expect("F1: the pinned peak should have exactly one `mod tests {`");

    assert!(
        trait_pos < struct_pos,
        "F1: the trait was claimed to precede the memory implementation"
    );
    assert!(
        struct_pos + 1 >= 286,
        "F1: MemoryCorrelationMaps was claimed to open the implementation \
         half of the file, around line 286"
    );
    assert_eq!(
        peak_lines[mod_tests_pos - 1].trim(),
        "#[cfg(test)]",
        "F1: mod tests {{ was claimed to open right after its own \
         #[cfg(test)] attribute, inline rather than as a `mod tests;` \
         pointing at a sibling"
    );
    assert!(
        mod_tests_pos + 1 >= 735,
        "F1: the inline test module was claimed to open around line 736, \
         holding both backends' trait and tables above it in one file"
    );
}

/// F1's first unused-decomposition claim: `correlation/contract.rs` already
/// exists as a sibling module and is already wired into `correlation.rs` via
/// `pub mod contract;` -- so the sibling-module idiom was not merely
/// available somewhere in the workspace, it was one `mod` declaration away
/// in the very file the rung grew past 1000 lines.
#[test]
fn contract_rs_already_exists_as_the_sibling_the_finding_names() {
    let contract_path = repo_root().join(CONTRACT_SIBLING_PATH);
    assert!(
        contract_path.is_file(),
        "F1: correlation/contract.rs was claimed to already exist as a sibling module"
    );
    // The 407 lines are the *pinned* file's, not the working tree's: F4's fix
    // rewrote this same file (the shared staleness assertion), and a control
    // over a historical premise has to read the revision the premise was
    // about or it stops being a control and becomes a tripwire on unrelated
    // work.
    if let Some(pinned_contract) = read_pinned_blob(PINNED_COMMIT_PEAK, CONTRACT_SIBLING_PATH) {
        assert_eq!(
            pinned_contract.lines().count(),
            407,
            "F1: correlation/contract.rs was claimed to be 407 lines at the \
             M14.2 rung's own commit"
        );
    }

    let correlation_src = std::fs::read_to_string(repo_root().join(RELATIVE_PATH))
        .expect("F1: correlation.rs should be readable");
    assert!(
        correlation_src.contains("pub mod contract;"),
        "F1: correlation.rs was claimed to already wire in the sibling via \
         `pub mod contract;`"
    );

    // The second unused decomposition: conversations.rs (a different crate,
    // roundhouse-server) already keeps its test module out-of-line.
    let conversations_src = std::fs::read_to_string(repo_root().join(CONVERSATIONS_PATH))
        .expect("F1: conversations.rs should be readable");
    assert!(
        conversations_src.contains("mod tests;"),
        "F1: conversations.rs was claimed to declare `mod tests;` (a \
         semicolon, pointing at a sibling file) rather than an inline module"
    );
    assert!(
        !conversations_src.contains("mod tests {"),
        "F1: conversations.rs was claimed to no longer write `mod tests {{ \
         .. }}` inline -- both house idioms the finding cites should already \
         be live in the workspace"
    );
}

/// F1's normative claim, now the guard on the split that closed it:
/// `correlation.rs` uses the idioms already live in this workspace --
/// `mod tests;` pointing at a sibling `correlation/tests.rs` the way
/// `conversations.rs` does, and the file itself back under the 1000-line
/// presumptive threshold. This was the red assertion the refute pass left
/// ignored; it is the reason the split cannot quietly grow back.
#[test]
fn correlation_rs_follows_its_own_sibling_test_convention() {
    let src = std::fs::read_to_string(repo_root().join(RELATIVE_PATH))
        .expect("F1: correlation.rs should be readable");
    let lines: Vec<&str> = src.lines().collect();

    assert!(
        lines.len() < 1000,
        "F1: correlation.rs should be under the 1000-line presumptive \
         threshold once split -- it is {} lines",
        lines.len()
    );
    assert!(
        !src.contains("mod tests {"),
        "F1: correlation.rs should no longer write `mod tests {{ .. }}` inline, \
         the way conversations.rs already does not"
    );
    assert_eq!(
        src.matches("mod tests;").count(),
        1,
        "F1: correlation.rs should declare `mod tests;` exactly once, \
         pointing at a sibling test module"
    );
}
