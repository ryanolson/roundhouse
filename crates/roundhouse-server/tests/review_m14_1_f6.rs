// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.1 review, finding F6 — refuted, then fixed.
//!
//! **The claim.** `correlation_maps_contract_suite!`
//! (`crates/roundhouse-core/src/control/correlation/contract.rs:382-417`) is
//! the fourth verbatim copy of one macro's recursion plumbing: its two
//! public arms (`ignore = $reason, $make` and `$make`), its `@list`
//! dispatch, its `@tests` recursion (both the multi-name and the
//! empty-list base case), and the "captured at depth one" comment are the
//! same ~22 lines as `spend_ledger_contract_suite!`
//! (`control/spend/contract.rs:885-923`), `fair_use_ledger_contract_suite!`
//! (`control/fair_use/contract.rs:608-648`), and `store_contract_suite!`
//! (`store/contract.rs:495-533`), differing only in the macro's own name,
//! the fully-qualified path the generated test calls into, and the local
//! `let` binding name. The finding's fix: one `#[doc(hidden)]
//! macro_rules! __contract_suite` in `roundhouse-core` that each family
//! macro delegates to, keeping only its public arms and name list.
//!
//! **No Redis needed.** The claim is about `macro_rules!` source text
//! duplicated across four files at compile time, not about behavior any of
//! the four contract suites exercise against a store — `MemoryStore` /
//! `MemoryCorrelationMaps` and the Redis backends they gate are identical
//! either way. Nothing here touches a store, in-memory or Redis.
//!
//! **Every structural citation checked out exactly, at refute time.** All
//! four macros opened and closed at the finding's cited lines
//! (`correlation/contract.rs` 382/417, `spend/contract.rs` 885/924 — the
//! finding said 885-923, one line short of the closing `}` the block
//! actually needed; the 885-923 span was otherwise exact —
//! `fair_use/contract.rs` 608/649, `store/contract.rs` 495/533). The
//! "captured at depth one" comment was present byte-for-byte in all four.
//! Substituting each macro's own name, module path and binding name for a
//! placeholder over the `@tests` recursion arms made all four textually
//! identical.
//!
//! **Ruling: valid — and fixed.** One `#[doc(hidden)] macro_rules!
//! __contract_suite` now lives in `roundhouse-core`'s own
//! `contract_macro.rs` (not inside any of the four family files — the whole
//! point is that it is no longer copied into each), and each family macro's
//! `@list` arm delegates to it directly, carrying only its own binding name,
//! its own fully-qualified module path and its own test-name list. The
//! recursive `@tests` arms are gone from all four family files; the pinned
//! blob below is what proves they were identical before this rung removed
//! them.

use std::collections::HashSet;
use std::process::Command;

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

const PINNED_COMMIT_PEAK: &str = "b9d4d1244fd281a2314615dcfa5e2615bb812bbe"; // M14.1 rung itself, pre-fix
const CONTRACT_MACRO_RS: &str = "crates/roundhouse-core/src/contract_macro.rs";

fn read(relative_path: &str) -> String {
    std::fs::read_to_string(repo_root().join(relative_path))
        .unwrap_or_else(|e| panic!("F6: {relative_path} should be readable: {e}"))
}

/// `None` when the pinned commit is not reachable (e.g. a shallow clone) --
/// callers must skip pinned-blob assertions rather than silently falling back
/// to the mutable working tree.
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

/// One family macro's identifying facts: its source file, its own name, the
/// fully-qualified module path its generated tests call into, and the local
/// `let` binding name its `@tests` arm introduces.
struct Family {
    path: &'static str,
    macro_name: &'static str,
    module_path: &'static str,
    binding: &'static str,
}

const FAMILIES: [Family; 4] = [
    Family {
        path: "crates/roundhouse-core/src/control/correlation/contract.rs",
        macro_name: "correlation_maps_contract_suite",
        module_path: "control::correlation::contract",
        binding: "maps",
    },
    Family {
        path: "crates/roundhouse-core/src/control/spend/contract.rs",
        macro_name: "spend_ledger_contract_suite",
        module_path: "control::spend::contract",
        binding: "ledger",
    },
    Family {
        path: "crates/roundhouse-core/src/control/fair_use/contract.rs",
        macro_name: "fair_use_ledger_contract_suite",
        module_path: "control::fair_use::contract",
        binding: "ledger",
    },
    Family {
        path: "crates/roundhouse-core/src/store/contract.rs",
        macro_name: "store_contract_suite",
        module_path: "store::contract",
        binding: "store",
    },
];

/// Extracts the whole `macro_rules! <name> { ... }` block by brace counting
/// from the `macro_rules!` keyword, so this does not depend on guessing
/// exact line numbers the way the finding's own `sed` ranges do.
fn extract_macro_rules_block(src: &str, macro_name: &str) -> String {
    let needle = format!("macro_rules! {macro_name} {{");
    let start = src
        .find(&needle)
        .unwrap_or_else(|| panic!("F6: expected `{needle}` in the source"));
    let body_start = start + needle.len() - 1; // at the opening `{`
    let bytes = src.as_bytes();
    let mut depth = 0i32;
    let mut end = body_start;
    for (offset, byte) in bytes[body_start..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = body_start + offset + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(
        end > body_start,
        "F6: unbalanced braces in {macro_name}'s macro_rules! block"
    );
    src[start..end].to_string()
}

/// Normalizes a family's macro block to a placeholder form: its own name,
/// module path, and let-binding are each replaced with a fixed token, so
/// two families' blocks can be compared for structural (not textual)
/// equality regardless of their own identifiers.
fn normalize(block: &str, family: &Family) -> String {
    block
        .replace(family.macro_name, "MACRO")
        .replace(family.module_path, "MODULE")
        .replace(
            &format!("let {} = $make;", family.binding),
            "let X = $make;",
        )
        .replace(&format!("(&{})", family.binding), "(&X)")
}

/// Isolates just the `@tests` recursion arms (the two-arm block: the
/// recursive per-name case and the empty-list base case, plus the preceding
/// "captured at depth one" comment) from a normalized macro block, which is
/// the part of the finding's claim that has no per-family list of test
/// names inside it — the whole thing should be comparable byte-for-byte.
fn tests_arms_only(normalized_block: &str) -> &str {
    let start = normalized_block
        .find("// One test per recursion step")
        .expect("F6: expected the depth-one comment to precede the @tests arms");
    &normalized_block[start..]
}

/// Sanity check: all four macros exist at the names and files the finding
/// cites, and each is reachable via `macro_rules! <name> {`.
#[test]
fn all_four_family_macros_exist_as_named() {
    for family in &FAMILIES {
        let src = read(family.path);
        assert!(
            src.contains(&format!("macro_rules! {} {{", family.macro_name)),
            "F6: expected macro_rules! {} in {}",
            family.macro_name,
            family.path
        );
    }
}

/// The "captured at depth one" comment was byte-identical across all four
/// files at the pinned pre-fix commit, as the finding's title claims. Read
/// from `PINNED_COMMIT_PEAK` rather than the working tree, since the fix
/// moves this comment to the one shared helper it now describes.
#[test]
fn the_captured_at_depth_one_comment_was_byte_identical_across_all_four() {
    const COMMENT: &str = "// One test per recursion step rather than one repetition over the names:\n    \
                            // the attribute group is captured at depth one, and macro_rules cannot\n    \
                            // re-expand it inside a second repetition.";
    for family in &FAMILIES {
        let Some(blob) = read_pinned_blob(PINNED_COMMIT_PEAK, family.path) else {
            eprintln!(
                "F6: pinned commit {PINNED_COMMIT_PEAK} unreachable (shallow clone?) \
                 -- skipping the pinned-blob assertions this test exists to make"
            );
            return;
        };
        assert!(
            blob.contains(COMMENT),
            "F6: expected the exact 'captured at depth one' comment in {} at {PINNED_COMMIT_PEAK}",
            family.path
        );
    }
}

/// The finding's core structural claim, at the pinned pre-fix commit:
/// normalized for macro name, module path, and binding name, the `@tests`
/// recursion arms (which carry no per-family test-name list — that lives
/// only in `@list`) were identical across all six pairs among the four
/// families.
#[test]
fn the_tests_recursion_arms_were_identical_across_all_four_families() {
    let mut normalized_arms: Vec<(String, String)> = Vec::new();
    for family in &FAMILIES {
        let Some(blob) = read_pinned_blob(PINNED_COMMIT_PEAK, family.path) else {
            eprintln!(
                "F6: pinned commit {PINNED_COMMIT_PEAK} unreachable (shallow clone?) \
                 -- skipping the pinned-blob assertions this test exists to make"
            );
            return;
        };
        let block = extract_macro_rules_block(&blob, family.macro_name);
        let normalized = normalize(&block, family);
        let arms = tests_arms_only(&normalized).to_string();
        normalized_arms.push((family.macro_name.to_string(), arms));
    }

    let distinct: HashSet<&String> = normalized_arms.iter().map(|(_, arms)| arms).collect();
    assert_eq!(
        distinct.len(),
        1,
        "F6: expected the @tests recursion arms to be identical (modulo macro \
         name / module path / binding name) across all four families, found \
         {} distinct variants among {:?}",
        distinct.len(),
        normalized_arms
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
    );
}

/// The finding's broader claim, at the pinned pre-fix commit: the two public
/// arms and the `@list` dispatch line (up to, not including, the per-family
/// test-name list) were also identical modulo the same three substitutions.
#[test]
fn the_public_arms_and_list_dispatch_were_identical_across_all_four_families() {
    fn public_prefix(normalized_block: &str) -> &str {
        let end = normalized_block
            .find("// The single list.")
            .expect("F6: expected the 'single list' comment after the public arms")
            + "// The single list. Both public arms land here, so gated and ungated\n    \
               // backends cannot drift apart in coverage.\n    \
               (@list $attrs:tt $make:expr) => {\n        \
               $crate::MACRO!(@tests $attrs $make;\n"
                .len();
        &normalized_block[..end]
    }

    let mut prefixes: Vec<String> = Vec::new();
    for family in &FAMILIES {
        let Some(blob) = read_pinned_blob(PINNED_COMMIT_PEAK, family.path) else {
            eprintln!(
                "F6: pinned commit {PINNED_COMMIT_PEAK} unreachable (shallow clone?) \
                 -- skipping the pinned-blob assertions this test exists to make"
            );
            return;
        };
        let block = extract_macro_rules_block(&blob, family.macro_name);
        let normalized = normalize(&block, family);
        prefixes.push(public_prefix(&normalized).to_string());
    }

    let distinct: HashSet<&String> = prefixes.iter().collect();
    assert_eq!(
        distinct.len(),
        1,
        "F6: expected the two public arms and the @list dispatch line to be \
         identical (modulo macro name) across all four families, found {} \
         distinct variants",
        distinct.len()
    );
}

/// **The fix, over the working tree.** None of the four family files carries
/// `@tests` recursion arms any more -- each `@list` arm is a straight-line
/// delegation to the one shared helper.
#[test]
fn no_family_file_carries_tests_recursion_arms_any_more() {
    for family in &FAMILIES {
        let src = read(family.path);
        assert!(
            !src.contains("(@tests"),
            "F6: {} should no longer define its own @tests recursion arms",
            family.path
        );
    }
}

/// The red assertion, now green: the finding's fix is that this plumbing
/// should live once, in a `#[doc(hidden)] macro_rules! __contract_suite` in
/// `roundhouse-core`, that each family macro delegates to. It does now: the
/// helper is defined in `contract_macro.rs` -- deliberately not in any of
/// the four family files, since the whole point is that it stops being
/// copied into each -- and every family's `@list` arm names it.
#[test]
fn the_four_family_macros_delegate_to_a_shared_helper_macro() {
    let helper_src = read(CONTRACT_MACRO_RS);
    assert!(
        helper_src.contains("#[doc(hidden)]")
            && helper_src.contains("macro_rules! __contract_suite"),
        "F6: expected a #[doc(hidden)] macro_rules! __contract_suite helper in {CONTRACT_MACRO_RS}"
    );

    let mut delegating_families: Vec<&str> = Vec::new();
    for family in &FAMILIES {
        let src = read(family.path);
        assert!(
            !src.contains("macro_rules! __contract_suite"),
            "F6: {} should not redefine the shared helper -- it should only call it",
            family.path
        );
        if src.contains("$crate::__contract_suite!(") {
            delegating_families.push(family.macro_name);
        }
    }
    assert_eq!(
        delegating_families.len(),
        FAMILIES.len(),
        "F6: expected all four family macros to delegate to the shared helper, \
         found only: {delegating_families:?}"
    );
}
