// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M12 thermo-nuclear review, finding F1 — refuted, ruled **valid**.
//!
//! **The claim.** `exchange.rs` grew from 846 to 1066 lines in M12, and the
//! growth — `CONTROL_TOOL_NAMESPACE`/`CONTROL_TOOL_DELIMITER`/`CONTROL_TOOL_NAMES`,
//! `is_control_call`, `is_flat_control_call`, and their four tests, ~250 lines —
//! is a cohesive "is this tool name ours" unit with nothing to do with folding
//! items into `Exchange`s. Its only in-file (non-test) caller is
//! `task_exchanges`; `validate/mod.rs` already re-exports every name
//! individually, so a split module (`validate/control_call.rs`) would be
//! invisible to consumers.
//!
//! **Why the check reads the git blob, not the working tree.** Same reasoning
//! as the M11.2b F11 precedent this follows
//! (`roundhouse-server/tests/review_m11_2b_f11.rs`): sibling refuters append
//! their own guard tests to `exchange.rs`'s `mod tests`, which moves the
//! working-tree line count for reasons unrelated to F1. This test pins to the
//! commit F1 was filed against and falls back to skipping (not silently
//! re-targeting the moving file) if that commit is unreachable.
//!
//! **Every citation in the finding checked out exactly**, against the pinned
//! blob: 846 -> 1066 lines across the M12 commit and its parent; the three
//! consts at lines 172, 187, 207; `is_control_call` at 252; `is_flat_control_call`
//! at 263; `task_exchanges` at 284 as the only in-file production caller of
//! `is_control_call` (the four other call sites are all inside `mod tests`);
//! the four recognizer tests occupying exactly 924-1066, none of them
//! exercising `exchanges`, `exec_exit_code`, `tool_output_body`, or
//! `reads_as_failure`; and `validate/mod.rs`'s `pub use exchange::{ ... }`
//! already naming every recognizer export individually, which is what makes
//! `pub use control_call::{ ... }` a no-op for every consumer outside this
//! module.
//!
//! **Ruling: valid, and fixed.** No inaccuracy was found in the mechanism,
//! unlike F11's one-line citation slip. The recognizer now lives in
//! `validate/control_call.rs` and `exchange.rs` is back under the boundary.
//!
//! **What survives of the refuter's audit, and what was retired.** The pinned
//! growth measurement below stays: it is the record of *why* the split
//! happened, it reads a frozen blob, and nothing in the working tree can make
//! it lie. The other three assertions described the pre-fix arrangement of
//! `exchange.rs` — the recognizer's exact line numbers, the 924..=1066 test
//! span, the `pub use exchange::{ … }` block — and every one of them is now a
//! statement about a file that no longer looks like that. They are replaced by
//! guards on the tree as it *is*, which is the half that keeps the fix from
//! being undone: a recognizer creeping back into `exchange.rs` fails
//! [`the_recognizer_lives_in_its_own_module`], and re-exports reached through a
//! glob fail [`the_split_stays_invisible_to_consumers`].

use std::process::Command;

/// The commit F1 was filed against — M12's own commit, `HEAD` at review time.
const PINNED_COMMIT: &str = "302dc8a73630d3a14332f2c0e0f7e9918f683d33";
/// The parent of `PINNED_COMMIT`, i.e. exchange.rs immediately before M12.
const PARENT_COMMIT: &str = "b8e8ddd";
const RELATIVE_PATH: &str = "crates/roundhouse-core/src/validate/exchange.rs";
const MOD_RELATIVE_PATH: &str = "crates/roundhouse-core/src/validate/mod.rs";

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

/// `None` when the pinned commit is not reachable (e.g. a shallow clone) —
/// callers must skip pinned-count assertions rather than silently falling
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

#[test]
fn exchange_rs_grew_from_846_to_1066_lines_across_m12() {
    let (Some(before), Some(after)) = (
        read_pinned_blob(PARENT_COMMIT, RELATIVE_PATH),
        read_pinned_blob(PINNED_COMMIT, RELATIVE_PATH),
    ) else {
        eprintln!(
            "F1: pinned commits {PARENT_COMMIT}/{PINNED_COMMIT} unreachable \
             (shallow clone?) -- skipping the pinned-count assertions this \
             test exists to make"
        );
        return;
    };
    assert_eq!(
        before.lines().count(),
        846,
        "F1: exchange.rs was claimed to be 846 lines immediately before M12"
    );
    assert_eq!(
        after.lines().count(),
        1066,
        "F1: exchange.rs was claimed to land at exactly 1066 lines after M12"
    );
}

const CONTROL_CALL_RELATIVE_PATH: &str = "crates/roundhouse-core/src/validate/control_call.rs";

fn working_tree(relative_path: &str) -> String {
    std::fs::read_to_string(repo_root().join(relative_path))
        .unwrap_or_else(|error| panic!("{relative_path} is readable: {error}"))
}

/// The fix, as a property of the tree rather than of a diff nobody re-reads.
///
/// Asserted on *definitions* (`pub const`, `pub fn`) and not on mentions, so a
/// doc comment in `exchange.rs` that points at the recognizer — which is the
/// useful kind of cross-reference — does not read as the recognizer moving
/// back.
#[test]
fn the_recognizer_lives_in_its_own_module() {
    let exchange = working_tree(RELATIVE_PATH);
    let control_call = working_tree(CONTROL_CALL_RELATIVE_PATH);

    for definition in [
        "pub const CONTROL_TOOL_NAMESPACE",
        "pub const CONTROL_TOOL_DELIMITER",
        "pub const CONTROL_TOOL_NAMES",
        "pub fn is_control_call",
        "pub fn is_flat_control_call",
        "pub fn task_exchanges",
    ] {
        assert!(
            control_call.contains(definition),
            "F1: `{definition}` belongs in validate/control_call.rs"
        );
        assert!(
            !exchange.contains(definition),
            "F1: `{definition}` is the recognizer's, not the fold's -- \
             exchange.rs pairs calls with their results and nothing else"
        );
    }
}

/// The boundary the finding was filed over, measured on the tree.
///
/// A ceiling rather than an exact count: the point is that the fold's own file
/// stays a fold, and pinning its length to the digit would go red for every
/// honest test added to it — which is how a size guard teaches people to delete
/// tests.
#[test]
fn exchange_rs_is_back_under_the_boundary() {
    let lines = working_tree(RELATIVE_PATH).lines().count();
    assert!(
        lines <= 900,
        "F1: exchange.rs is {lines} lines; it crossed 1000 in M12 by carrying \
         the control-call recognizer, which now has its own module"
    );
}

/// The split cost consumers nothing, which is what made it mechanical.
#[test]
fn the_split_stays_invisible_to_consumers() {
    let module = working_tree(MOD_RELATIVE_PATH);
    for name in [
        "CONTROL_TOOL_DELIMITER",
        "CONTROL_TOOL_NAMES",
        "CONTROL_TOOL_NAMESPACE",
        "is_control_call",
        "task_exchanges",
    ] {
        assert!(
            module.contains(name),
            "F1: {name} must still be named individually in validate/mod.rs, so \
             the module it is defined in stays this crate's business"
        );
    }
    for glob in ["pub use exchange::*", "pub use control_call::*"] {
        assert!(
            !module.contains(glob),
            "a glob re-export would make the split module's boundary visible to \
             consumers, which is not what F1 licensed"
        );
    }
}
