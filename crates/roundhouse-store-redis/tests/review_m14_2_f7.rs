// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.2 thermo-nuclear review, finding F7 — refuted.
//!
//! **The claim.** `keys.rs`'s `build_key(namespace, family: &str, parts)`
//! has no closed family type, so the four names R-S3 calls a closed set
//! (`sess`, `spend`, `fairuse`, `corr`) are really just string literals
//! typed at eleven call sites. Worse, `key_builder_convention.rs`'s own
//! module doc says a `*_key` function absent from its hand-written
//! `KEY_FUNCTIONS` list "is exactly the gap this test exists to close," yet
//! the loop at lines 76-104 iterates only that fixed list — it never scans
//! `src/*.rs` for `fn \w+_key` — so a family that adds a key function
//! without also adding a `KEY_FUNCTIONS` row is invisible to it regardless
//! of what the function's body does.
//!
//! **Verified directly against the real crate first, then reverted (no
//! diff kept — the source-tree edit could not be left behind).** Appending
//! ```ignore
//! pub(crate) fn rogue_key(namespace: &KeyNamespace) -> String {
//!     format!("{namespace}:v1:spend:rogue")
//! }
//! ```
//! to `src/spend.rs` and running
//! `cargo test -p roundhouse-store-redis --test key_builder_convention`
//! reports `test every_key_function_calls_the_shared_builder ... ok` —
//! the hand-formatted, `build_key`-bypassing function is never flagged,
//! because the check never learns it exists. `src/spend.rs` was restored
//! byte-for-byte afterward (`git diff` empty) before this test was written,
//! so the gap this file proves has to stand on a self-contained fixture
//! rather than on that transient edit.
//!
//! **This test reproduces the same algorithm** — the exact `fn \w+_key(`
//! scan the finding says is missing — against a fixture standing in for a
//! family module, so the demonstration does not depend on editing real
//! crate source. No Redis is needed: `build_key` is a pure string builder
//! and `key_builder_convention.rs` is a shape-of-the-source check, not a
//! runtime one — the claim never touches the store.
//!
//! **Closed.** `key_builder_convention.rs`'s `KEY_FUNCTIONS` hand list is
//! gone; it now runs the same `scan_key_function_names` this file already
//! had (`M14.2 review, F7`) over each family file directly, so a function
//! this scan finds is a function the check inspects — there is no second
//! list to fall out of. The test below, once red because
//! `FIXTURE_KEY_FUNCTIONS` (the stand-in for the old hand list) omitted
//! `rogue_key`, now proves the *positive* half of that: scanning finds
//! `rogue_key` and the shared "calls `build_key`" check — the same one
//! `key_builder_convention.rs` runs — flags it.

/// A full scan for every key-building function a module defines: every
/// `fn <name>(` where `<name>` ends in `_key`. This is the check the
/// finding says `key_builder_convention.rs` never performs — its own loop
/// walks a fixed list instead. No regex dependency, matching the crate's
/// own hand-rolled `function_body` extraction in `key_builder_convention.rs`.
fn scan_key_function_names(src: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = src;
    while let Some(pos) = rest.find("fn ") {
        let after_fn = &rest[pos + 3..];
        let Some(name_end) = after_fn.find('(') else {
            break;
        };
        let name = after_fn[..name_end].trim();
        if !name.is_empty()
            && name.ends_with("_key")
            && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        {
            names.push(name.to_string());
        }
        rest = &after_fn[name_end..];
    }
    names
}

/// A fixture standing in for one family module: two legitimate key
/// functions that call `build_key`, plus a `rogue_key` that hand-formats
/// its own key string — exactly the pattern verified against the real
/// crate above, reproduced here so it survives without a source edit.
const FIXTURE_SRC: &str = r#"
pub(crate) fn account_key(namespace: &KeyNamespace, project: &ProjectId) -> String {
    keys::build_key(namespace, "spend", &[&format!("{{{project}}}"), "account"])
}

pub(crate) fn holds_key(namespace: &KeyNamespace, project: &ProjectId) -> String {
    keys::build_key(namespace, "spend", &[&format!("{{{project}}}"), "holds"])
}

pub(crate) fn rogue_key(namespace: &KeyNamespace) -> String {
    format!("{namespace}:v1:spend:rogue")
}
"#;

/// What a maintainer following `key_builder_convention.rs`'s own pattern
/// would have hand-typed into `KEY_FUNCTIONS` for this fixture *before*
/// `rogue_key` was added — nothing in the check forces them to revisit it
/// afterward, which is the entire mechanism the finding describes. Kept as
/// the pre-fix baseline the passing control below still checks; the fixed
/// check no longer consults a list like this one at all.
const FIXTURE_KEY_FUNCTIONS: &[&str] = &["account_key", "holds_key"];

/// Extracts `fn <name>(...) -> String { ... }`'s body, by brace-balancing
/// from the first `{` after the signature — the same extractor
/// `key_builder_convention.rs` runs over real crate source, reproduced here
/// so this file's fixture-based proof needs no crate source of its own.
fn function_body(src: &str, fn_name: &str) -> String {
    let needle = format!("fn {fn_name}(");
    let start = src
        .find(&needle)
        .unwrap_or_else(|| panic!("`{needle}` not found"));
    let brace_start = start
        + src[start..]
            .find('{')
            .unwrap_or_else(|| panic!("`{needle}` has no body"));
    let mut depth = 0i32;
    let mut end = brace_start;
    for (offset, ch) in src[brace_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = brace_start + offset + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    src[brace_start..end].to_string()
}

/// Control: the scanner itself is correct, and `key_builder_convention.rs`'s
/// check is sound *when its list is complete* — the finding does not
/// dispute that part. Run against source with no rogue function, the scan
/// and the hand list agree exactly.
#[test]
fn scan_matches_the_hand_list_when_the_hand_list_is_complete() {
    let clean_src = FIXTURE_SRC.split("pub(crate) fn rogue_key").next().unwrap();
    assert_eq!(scan_key_function_names(clean_src), FIXTURE_KEY_FUNCTIONS);
}

/// F7, was red, now closed: `key_builder_convention.rs`'s own doc said an
/// unlisted `*_key` function "is exactly the gap this test exists to
/// close" — so its check must cover every `_key` function the source
/// actually defines, not just the ones a maintainer remembered to list.
/// Before the fix it did not: `rogue_key` is defined in the fixture,
/// hand-formats a key, bypasses `build_key` entirely, and was not in
/// `FIXTURE_KEY_FUNCTIONS` — the same shape as the hand-maintained
/// `KEY_FUNCTIONS` the real `key_builder_convention.rs` used to carry.
///
/// The fixed guard scans instead of consulting a list, so this now checks
/// the *positive* half directly: scanning finds `rogue_key` regardless of
/// any list, and the shared "calls `build_key`" check — the same
/// `body.contains("build_key(")` `key_builder_convention.rs` runs — flags
/// it precisely because scanning, not a list, decided which functions get
/// checked.
#[test]
fn every_scanned_key_function_is_checked_there_is_no_list_left_to_fall_out_of() {
    let scanned = scan_key_function_names(FIXTURE_SRC);
    assert_eq!(
        scanned,
        vec!["account_key", "holds_key", "rogue_key"],
        "the scan must find every _key function the fixture defines, hand \
         list or not — rogue_key included, which FIXTURE_KEY_FUNCTIONS \
         above never named"
    );

    let offenders: Vec<&str> = scanned
        .iter()
        .map(String::as_str)
        .filter(|name| !function_body(FIXTURE_SRC, name).contains("build_key("))
        .collect();
    assert_eq!(
        offenders,
        vec!["rogue_key"],
        "F7: scanning every defined _key function rather than a hand-\
         maintained subset is what makes rogue_key — which hand-formats its \
         key and never calls build_key — visible to the check at all"
    );
}
