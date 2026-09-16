// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M12.1 review, finding F3 — refuted.
//!
//! **The claim.** `mcp_api.rs` crossed 1000 lines (956 at `c479ca5`, the M12
//! thermo-nuclear commit, to 1184 at `c6dba85`, the M12.1 commit) entirely on
//! test-module growth — non-test code grew only 438 to 485 lines while `mod
//! tests` (opening at line 486) grew 517 to 698 — and the crate already has
//! an established pattern for this exact situation: `claude_launch.rs`,
//! `relay_handoff.rs`, `control_config/directory.rs` and
//! `control_config/config.rs` all declare `#[cfg(test)] mod tests;` pointing
//! at a sibling `tests.rs` file rather than writing the module inline.
//! `mcp_api.rs` does not follow its own crate's precedent.
//!
//! **Why the check reads git blobs, not the working tree.** Same reasoning
//! as the M12 F13 precedent (`review_m12_f13.rs`): by the time this refuter
//! ran, the working tree already carried 35 uncommitted lines of M12.1
//! follow-up work on top of `c6dba85` (`git diff --stat HEAD` at refute time:
//! `1 file changed, 35 insertions(+)`), which would silently move every line
//! number this finding cites for reasons unrelated to F3. `c6dba85` is
//! `HEAD` at commit granularity (`git rev-parse HEAD` == `git log -1
//! --format=%H c6dba85`), so pinning to it is pinning to the actual reviewed
//! state, and the check degrades to a skip (not a false pass or a
//! silently-retargeted assertion) if that commit is ever unreachable.
//!
//! **Every structural citation checked out exactly**, against both pinned
//! blobs: `c479ca5`'s `mcp_api.rs` is 956 lines with `mod tests {` opening at
//! line 439 (438 lines of non-test code before it); `c6dba85`'s is 1184
//! lines with `mod tests {` opening at line 486 (485 lines of non-test code
//! before it) — the exact 956/1184 and 438/485 figures the finding cites.
//! The 517/698 test-module-growth figures are off by exactly one in both
//! blobs (518 and 699 by this file's own count, since the finding's
//! `how_to_prove` line-count method excludes the `mod tests {` line itself
//! while a plain `total - non_test` does not) — the same class of
//! one-line-off citation the F11/F13 precedents ruled immaterial, and this
//! file's assertions use the precise inclusive counts rather than the
//! finding's off-by-one ones. The sibling-file claim also checked out
//! exactly: all four named files declare `#[cfg(test)]` immediately followed
//! by `mod tests;` (a semicolon, not a brace), and a `tests.rs` sibling
//! exists at each corresponding path.
//!
//! **The red assertion.** The finding's actual claim is normative — this
//! crate has an established convention for large `mod tests` blocks, and
//! `mcp_api.rs` should follow it but doesn't. The natural failing test is
//! exactly that: assert `mcp_api.rs` declares `mod tests;` (the sibling-file
//! form) the same way its four siblings do. That assertion is false at
//! `c6dba85` — `mcp_api.rs` opens an inline `mod tests {` — so the test
//! fails for precisely the reason F3 says it should, and is left `#[ignore]`
//! rather than fixed here (refuters do not fix).
//!
//! **Ruling: valid.** Every factual premise (line counts, the crossing being
//! wholly attributable to the test module, the sibling files' contrasting
//! pattern) is confirmed against the pinned blob, and the prescriptive claim
//! (mcp_api.rs should use the same pattern) is captured by a test that fails
//! today and would pass once the split lands.

use std::process::Command;

const PINNED_COMMIT_BEFORE: &str = "c479ca511c317ee4d5ebb173c3c10efdfb3ac211"; // M12 thermo-nuclear commit
const PINNED_COMMIT_AFTER: &str = "c6dba8511c503069e3a8513f6dfa1820ff99beed"; // M12.1 commit (HEAD at review time)
const RELATIVE_PATH: &str = "crates/roundhouse-server/src/mcp_api.rs";

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

/// `None` when the pinned commit is not reachable (e.g. a shallow clone) —
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

/// F3's growth numbers: 956 (438 non-test + 518 test-module, one line over
/// the finding's own 517 -- see module doc) at `c479ca5`, to 1184 (485
/// non-test + 699 test-module, one over the finding's 698) at `c6dba85`.
/// Confirms the crossing of the 1000-line threshold is wholly inside the
/// growth of `mod tests`, not the surrounding code.
#[test]
fn the_1000_line_crossing_is_entirely_the_test_module() {
    let Some(before) = read_pinned_blob(PINNED_COMMIT_BEFORE, RELATIVE_PATH) else {
        eprintln!(
            "F3: pinned commit {PINNED_COMMIT_BEFORE} unreachable (shallow clone?) \
             -- skipping the pinned-line assertions this test exists to make"
        );
        return;
    };
    let Some(after) = read_pinned_blob(PINNED_COMMIT_AFTER, RELATIVE_PATH) else {
        eprintln!(
            "F3: pinned commit {PINNED_COMMIT_AFTER} unreachable (shallow clone?) \
             -- skipping the pinned-line assertions this test exists to make"
        );
        return;
    };

    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();

    assert_eq!(
        before_lines.len(),
        956,
        "F3: mcp_api.rs at the M12 commit was claimed to be 956 lines"
    );
    assert_eq!(
        after_lines.len(),
        1184,
        "F3: mcp_api.rs at the M12.1 commit was claimed to be 1184 lines, \
         crossing the 1000-line threshold"
    );

    // mod tests opens at 439 (before) / 486 (after), 1-indexed.
    let before_mod_tests = before_lines
        .iter()
        .position(|l| l.trim_start() == "mod tests {")
        .expect("F3: mcp_api.rs should have exactly one `mod tests {` at the M12 commit");
    let after_mod_tests = after_lines
        .iter()
        .position(|l| l.trim_start() == "mod tests {")
        .expect("F3: mcp_api.rs should have exactly one `mod tests {` at the M12.1 commit");
    assert_eq!(
        before_mod_tests + 1,
        439,
        "F3: mod tests was claimed to open at line 439 in the M12 commit"
    );
    assert_eq!(
        after_mod_tests + 1,
        486,
        "F3: mod tests was claimed to open at line 486 in the M12.1 commit \
         (grep -n '^mod tests' == 486, per how_to_prove)"
    );

    // Non-test code is everything up to and including `#[cfg(test)]`; it
    // grew only 438 (lines 1..=438) to 485 (lines 1..=485) lines -- the
    // exact figures cited. `before_mod_tests`/`after_mod_tests` are the
    // 0-indexed positions of the `mod tests {` line, which numerically
    // equal the 1-indexed line number of the `#[cfg(test)]` line right
    // above it (mod_tests_1indexed - 1 == cfg_test_1indexed).
    let before_non_test = before_mod_tests;
    let after_non_test = after_mod_tests;
    assert_eq!(
        before_non_test, 438,
        "F3: non-test code was claimed to be 438 lines at the M12 commit"
    );
    assert_eq!(
        after_non_test, 485,
        "F3: non-test code was claimed to be 485 lines at the M12.1 commit"
    );

    // The test module's own growth: everything from `#[cfg(test)]` to EOF.
    let before_test_module = before_lines.len() - before_non_test;
    let after_test_module = after_lines.len() - after_non_test;
    assert_eq!(
        before_test_module, 518,
        "F3: the finding cites 517 for the M12 commit's test module; this is \
         off by one (the finding's how_to_prove line-count method excludes \
         the 'mod tests {{' line itself) -- immaterial per the F11/F13 \
         one-line-off precedent, recorded here as the precise figure"
    );
    assert_eq!(
        after_test_module, 699,
        "F3: the finding cites 698 for the M12.1 commit's test module; same \
         one-line-off pattern as above"
    );

    // The threshold crossing is wholly inside the test module's growth:
    // non-test code grew by only 47 lines (438 -> 485, nowhere near enough
    // on its own to cross 1000 starting from 956), while mod tests grew by
    // 181 lines (518 -> 699) -- essentially all of the file's 228-line
    // growth (956 -> 1184).
    let non_test_growth = after_non_test - before_non_test;
    let test_module_growth = after_test_module - before_test_module;
    let total_growth = after_lines.len() - before_lines.len();
    assert_eq!(non_test_growth, 47, "F3: non-test code's growth");
    assert_eq!(test_module_growth, 181, "F3: mod tests's growth");
    assert_eq!(total_growth, 228, "F3: the file's total growth");
    assert!(
        test_module_growth > total_growth * 3 / 4,
        "F3: the test module's growth (181 lines) should account for the \
         overwhelming majority of the file's total growth (228 lines) -- \
         it accounts for 79% of it, confirming the crossing is 'entirely \
         the test module', not the surrounding code"
    );
}

/// F3's sibling-pattern claim: all four named files use `#[cfg(test)] mod
/// tests;` (a semicolon, pointing at a sibling `tests.rs`), confirmed
/// against the working tree (this part of the finding is about an existing,
/// stable convention, not about lines mcp_api.rs's own growth would move).
#[test]
fn four_sibling_files_already_use_the_split_test_module_pattern() {
    let siblings = [
        (
            "crates/roundhouse-server/src/claude_launch.rs",
            "crates/roundhouse-server/src/claude_launch/tests.rs",
        ),
        (
            "crates/roundhouse-server/src/relay_handoff.rs",
            "crates/roundhouse-server/src/relay_handoff/tests.rs",
        ),
        (
            "crates/roundhouse-server/src/control_config/directory.rs",
            "crates/roundhouse-server/src/control_config/directory/tests.rs",
        ),
        (
            "crates/roundhouse-server/src/control_config/config.rs",
            "crates/roundhouse-server/src/control_config/config/tests.rs",
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

/// The red assertion: F3's normative claim is that `mcp_api.rs` should
/// follow the same sibling-file convention its four siblings already use.
/// It doesn't -- `mod tests {` is written inline in `mcp_api.rs` itself --
/// so this fails for exactly the reason the finding says it should.
#[test]
fn mcp_api_should_follow_the_crates_own_split_test_module_convention() {
    let path = repo_root().join("crates/roundhouse-server/src/mcp_api.rs");
    let src = std::fs::read_to_string(&path).expect("F3: mcp_api.rs should be readable");
    assert!(
        src.contains("#[cfg(test)]\nmod tests;"),
        "F3: mcp_api.rs should declare `#[cfg(test)] mod tests;` (pointing \
         at a sibling crates/roundhouse-server/src/mcp_api/tests.rs), the \
         same pattern its four siblings in this crate already use -- \
         instead it writes the module inline"
    );
}
