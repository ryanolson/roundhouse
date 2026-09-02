// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M11.2b thermo-nuclear review, finding F11 — refuted, ruled **valid**.
//!
//! **The claim.** `claude_launch.rs` lands at 1126 lines on arrival (crossing
//! the 1k-line boundary the codebase treats as a maintainability signal), and
//! the `SuppressorSite`/`OauthSuppressor`/`OAUTH_SUPPRESSORS` block
//! (lines 253-338) is a self-contained unit — defined together, consumed only
//! by `ClaudeLaunch::must_be_unset` (577) and `ClaudeLaunch::env` (593-646) —
//! that could move to a sibling module, leaving a smaller primary file. The
//! finding cites `codex_launch.rs` (1080 lines) as the house-shape precedent.
//!
//! **Why the check reads the git blob, not the working-tree file.** Sibling
//! refuters run concurrently against the same file and append their own
//! `#[ignore]`d guard tests inside `claude_launch.rs`'s `mod tests`, which
//! grows the working-tree line count turn over turn — a moving target that
//! has nothing to do with F11. F11's claim is about the file *as it landed in
//! the M11.2b commit*, so this test pins to that immutable commit
//! (`60d6b4fbacd117a5b385312978da251e26b151ce`) via `git show`, and only
//! falls back to the working tree if that exact commit is unreachable (e.g. a
//! shallow clone), in which case the pinned-count assertions are skipped
//! rather than silently re-targeted at a moving file.
//!
//! **Verified against the pinned commit:** 1126 lines total; the suppressor
//! block spans exactly 253-338 (`pub const OAUTH_SUPPRESSORS` opens at 302,
//! the literal array closes at 338, the doc comment introducing the block
//! starts at 253); `must_be_unset` is at 577 and `env`'s suppressor-consuming
//! loop (`SuppressorSite::EnvVar` / `SuppressorSite::SettingsKey` matched
//! inside the loop body) is at 616-620, inside the finding's cited 593-646
//! span; `codex_launch.rs` is 1080 lines, matching the cited precedent
//! exactly. The one inaccuracy: the finding's `grep -n '^mod tests'` line is
//! given as 729, but line 729 is `#[cfg(test)]` — the `mod tests {` line
//! itself is 730. That is a one-line citation slip (attribute vs.
//! declaration), not a defect in the substance of the claim, so it does not
//! change the ruling.
//!
//! **The fix, and what guards it.** The extraction was accepted and made:
//! `claude_launch/suppressors.rs` holds the table, `claude_launch/tests.rs`
//! the unit tests, leaving the primary file well under the boundary — the
//! same two-file shape `codex_launch` already has. So there are two live
//! guards here rather than one, and they check different eras on purpose: the
//! pinned-blob test below is the *evidence* the ruling rested on, frozen at
//! the arrival commit and unaffected by the fix, and
//! [`the_suppressor_table_now_lives_in_its_own_module`] is the *outcome*, read
//! from the working tree, which is what goes red if a later edit folds it back
//! or lets the primary file grow past the boundary again.

use std::process::Command;

/// The commit F11 was filed against. `claude_launch.rs` has had no commits
/// since (confirmed via `git log --oneline -- ...` at refutation time), so
/// this is also, as of writing, `HEAD` — but pinning the hash rather than
/// trusting `HEAD` keeps the test meaningful even after later commits touch
/// the file for unrelated reasons.
const PINNED_COMMIT: &str = "60d6b4fbacd117a5b385312978da251e26b151ce";
const RELATIVE_PATH: &str = "crates/roundhouse-server/src/claude_launch.rs";
const CODEX_LAUNCH_RELATIVE_PATH: &str = "crates/roundhouse-server/src/codex_launch.rs";

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
/// callers must skip pinned-count assertions rather than silently falling
/// back to the mutable working tree, which sibling refuters are actively
/// editing.
fn read_pinned_blob(relative_path: &str) -> Option<String> {
    let output = Command::new("git")
        .current_dir(repo_root())
        .arg("show")
        .arg(format!("{PINNED_COMMIT}:{relative_path}"))
        .output()
        .expect("git is available in this environment");
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).expect("source files are UTF-8"))
}

#[test]
fn claude_launch_landed_over_1000_lines_with_a_self_contained_suppressor_block() {
    let Some(source) = read_pinned_blob(RELATIVE_PATH) else {
        eprintln!(
            "F11: pinned commit {PINNED_COMMIT} unreachable (shallow clone?) — \
             skipping the pinned-count assertions this test exists to make"
        );
        return;
    };
    let lines: Vec<&str> = source.lines().collect();

    // The core claim: the file crosses the 1k-line boundary on arrival.
    assert_eq!(
        lines.len(),
        1126,
        "F11: claude_launch.rs was claimed to land at exactly 1126 lines"
    );
    assert!(
        lines.len() > 1000,
        "F11's headline claim: the file crosses the 1k-line maintainability boundary"
    );

    // The suppressor block: SuppressorSite's doc comment opens the unit,
    // OAUTH_SUPPRESSORS's closing `];` ends it. 1-indexed to match `grep -n`.
    let block_open = 253;
    let block_close = 338;
    assert_eq!(
        lines[block_open - 1].trim(),
        "/// Where an OAuth-suppressing input lives, because they are not all environment"
    );
    assert_eq!(lines[block_close - 1].trim(), "];");
    assert!(
        source.contains("pub enum SuppressorSite"),
        "the block claimed self-contained must actually define SuppressorSite"
    );
    assert!(
        source.contains("pub struct OauthSuppressor"),
        "the block claimed self-contained must actually define OauthSuppressor"
    );
    assert!(
        source.contains("pub const OAUTH_SUPPRESSORS"),
        "the block claimed self-contained must actually define OAUTH_SUPPRESSORS"
    );

    // The two cited consumers.
    let must_be_unset_line = lines
        .iter()
        .position(|l| l.contains("pub fn must_be_unset"))
        .map(|i| i + 1);
    assert_eq!(must_be_unset_line, Some(577));
    let env_line = lines
        .iter()
        .position(|l| l.contains("pub fn env(&self)"))
        .map(|i| i + 1);
    assert_eq!(env_line, Some(593));
    // env's body matches on SuppressorSite within the finding's cited
    // 616-646 span.
    let suppressor_site_match_line = lines
        .iter()
        .position(|l| l.contains("SuppressorSite::EnvVar =>"))
        .map(|i| i + 1)
        .expect("env matches on SuppressorSite::EnvVar somewhere in its body");
    assert!(
        (593..=646).contains(&suppressor_site_match_line),
        "F11 cites env's suppressor-consuming code at 616-646; found the \
         SuppressorSite match at {suppressor_site_match_line}"
    );
}

/// F11's one inaccuracy, asserted rather than left ignored: the finding cites
/// `grep -n '^mod tests'` at line 729, and 729 is the `#[cfg(test)]` attribute
/// — `mod tests {` is 730.
///
/// Kept live in the corrected direction because an `#[ignore]`d test enforces
/// nothing, and the fact worth keeping is the true one: a one-line
/// attribute-vs-declaration slip in an otherwise-accurate structural finding,
/// which is why it did not change the ruling.
#[test]
fn finding_f11_cited_mod_tests_line_is_off_by_one() {
    let Some(source) = read_pinned_blob(RELATIVE_PATH) else {
        eprintln!(
            "F11: pinned commit {PINNED_COMMIT} unreachable (shallow clone?) — \
             skipping the citation check this test exists to make"
        );
        return;
    };
    let lines: Vec<&str> = source.lines().collect();
    let claimed_line = 729;
    assert_eq!(
        lines[claimed_line - 1].trim(),
        "#[cfg(test)]",
        "F11 cites '^mod tests' at {claimed_line}; that line is the attribute"
    );
    assert_eq!(
        lines[claimed_line].trim(),
        "mod tests {",
        "the declaration itself is one line further down"
    );
}

/// The fix F11 asked for, read from the working tree.
///
/// Three assertions rather than a line-count one, because a bare line count
/// would pass for a file that had been split badly — the point of the
/// extraction was that the table is a *self-contained* unit, so what is checked
/// is that it moved somewhere whole and that the primary file is under the
/// boundary it crossed.
#[test]
fn the_suppressor_table_now_lives_in_its_own_module() {
    let root = repo_root();
    let primary = std::fs::read_to_string(root.join(RELATIVE_PATH))
        .expect("claude_launch.rs exists in the workspace");
    let suppressors = std::fs::read_to_string(
        root.join("crates/roundhouse-server/src/claude_launch/suppressors.rs"),
    )
    .expect("F11's extraction target exists");

    assert!(
        primary.lines().count() < 1000,
        "F11's fix is the primary file back under the 1k-line boundary; it is at {}",
        primary.lines().count()
    );
    for item in [
        "pub enum SuppressorSite",
        "pub struct OauthSuppressor",
        "pub const OAUTH_SUPPRESSORS",
    ] {
        assert!(
            suppressors.contains(item),
            "the extracted module must define {item} -- the unit only stays \
             self-contained if all of it moved"
        );
        assert!(
            !primary.contains(item),
            "{item} must not also be defined in the primary file"
        );
    }
    // And the public path a launcher reaches for is unchanged by the move,
    // which is the whole licence for calling the extraction behavior-preserving.
    assert!(
        primary.contains("pub use suppressors::"),
        "the table must still be nameable as `claude_launch::OAUTH_SUPPRESSORS`"
    );
    assert!(
        std::fs::metadata(root.join("crates/roundhouse-server/src/claude_launch/tests.rs")).is_ok(),
        "F11's other half: the unit tests move beside the table"
    );
}

/// The finding's house-shape comparison: `codex_launch.rs` as the precedent
/// for "one primary file beside its own submodule directory".
///
/// Read from the live working tree (not pinned) since the comparison is
/// about the sibling file's current *shape*, not a frozen snapshot of it. The
/// first version of this control pinned the exact line count the finding
/// quoted (1080), and M12 moved two namespace helpers out of the file for a
/// reason unrelated to F11 — which is precisely the kind of refactor a shape
/// guard must not resist. So the pin is now the shape and a band wide enough
/// to survive ordinary movement: still one primary file of the same order as
/// the launcher it was compared against, still beside a `codex_launch/`
/// submodule directory it declares.
#[test]
fn codex_launch_matches_the_cited_house_shape_precedent() {
    let root = repo_root();
    let path = root.join(CODEX_LAUNCH_RELATIVE_PATH);
    let source = std::fs::read_to_string(&path).expect("codex_launch.rs exists in the workspace");
    let line_count = source.lines().count();
    assert!(
        (600..=1300).contains(&line_count),
        "F11's precedent is a primary file of the same order as the split it justified; \
         codex_launch.rs is {line_count} lines, outside the band that keeps the comparison meaningful"
    );
    assert!(
        source.contains("pub mod skills;"),
        "the precedent's shape is a primary file that declares its submodule directory"
    );
    assert!(
        std::fs::metadata(root.join("crates/roundhouse-server/src/codex_launch/skills.rs")).is_ok(),
        "the precedent's submodule directory must exist beside the primary file"
    );
}
