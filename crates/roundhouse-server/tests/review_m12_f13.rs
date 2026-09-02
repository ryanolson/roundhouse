// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M12 thermo-nuclear review, finding F13 — refuted, ruled **valid**.
//!
//! **The claim.** `Conversations` grew a third concern — the call table
//! (`calls` + `call_order` + `REMEMBERED_CALLS` eviction) — that shares the
//! struct's lock but not its stated reason for being one table:
//! `bind_call` (line 196) deliberately touches neither `latest` nor
//! `generations`, so the module's "one map cannot disagree" justification
//! (which is about `bind`/`resolve`/`fork` agreeing on one `generations`
//! map) does not cover it. The bounded insertion-ordered map is implemented
//! inline inside `bind_call`'s body rather than as its own type, so its
//! eviction logic cannot be unit tested or reused independently of
//! `Conversations`, and the invariant `call_order.len() == calls.len()` is
//! asserted only by a test that reaches into `Inner`'s private fields
//! through `Conversations::lock` (itself private, reachable only because
//! `mod tests` is a child module).
//!
//! **Why the check reads the git blob, not the working tree.** Same
//! reasoning as the M11.2b F11 and M12 F1 precedents
//! (`roundhouse-server/tests/review_m11_2b_f11.rs`,
//! `roundhouse-core/tests/review_m12_f1.rs`): sibling refuters append their
//! own guard tests to `conversations.rs`'s `mod tests`, which moves the
//! working-tree line count and line numbers for reasons unrelated to F13.
//! This test pins to the M12 commit F13 was filed against and skips its
//! line-numbered assertions (rather than silently re-targeting a moving
//! file) if that commit is unreachable.
//!
//! **Every structural citation in the finding checked out against the
//! pinned blob**, which is byte-identical to the working tree at review
//! time (no prior refuter has touched `conversations.rs`): `bind_call` is
//! defined at line 196 exactly; its 22-line body (196-217) touches only
//! `self.lock()`, `inner.calls`, and `inner.call_order` — grepping that span
//! for `latest` or `generations` finds nothing; the file declares exactly
//! three structs (`Conversations`, `Inner`, `CallSite`) and no
//! `CallSites`-shaped type exists to own the eviction invariant
//! independently; and the only assertion of
//! `call_order.len() == calls.len()` (at line 371, `assert_eq!(ordered,
//! held)`) is inside `mod tests`, reading `conversations.lock().call_order`
//! and `conversations.lock().calls` — fields private to this module, which
//! only a same-module (or descendant-module) test can name at all.
//!
//! **One inaccuracy, immaterial per the F11/F1 precedent.** The finding's
//! prose cites the invariant-asserting test as "conversations.rs:361"; line
//! 361 in the pinned blob is a comment (`// Re-binding an id already held
//! ...`), not the assertion — the actual `assert_eq!(ordered, held)` is at
//! line 371, ten lines later, inside the *same* test function the comment
//! introduces. A one-line-off pointer into the test it correctly names, not
//! a claim about a different test or a different invariant, so — exactly as
//! F11's `729` vs `730` slip did not change that ruling — this does not
//! change this one either.
//!
//! **Ruling: valid.** The finding is architectural, not behavioral: it does
//! not allege the invariant can actually desync (the existing test already
//! demonstrates it holds under duplicate-id rebinding, the one case that
//! could grow `call_order` without growing `calls`), it alleges that nothing
//! but `bind_call`'s body and a lock-reaching test stands between that
//! invariant and a regression, and that the eviction policy cannot be
//! exercised without spinning up a full `Conversations` with `Principal` and
//! `SessionId` setup unrelated to the table's own behavior. Both are true by
//! inspection of the pinned source, so there is no failing-behavior test to
//! write — the `how_to_prove` field says the same thing: proving the gap
//! needs no new test, only a same-behavior extraction. This file is the
//! pinned evidence that proof rests on. No fix applied here per the
//! refuter's mandate; the tree is unchanged apart from this file.
//!
//! **Fixed since** (M12 review fix stage): the table is a `CallTable` type
//! owning its own map, order and cap, and the invariant is asserted through
//! that type's own accessor. Everything below still reads the *pinned* blob,
//! which is why it stays green — it is the record of what the working tree
//! looked like when the finding was filed, not a description of it now.

use std::process::Command;

/// The commit F13 was filed against — M12's own commit, `HEAD` at review
/// time. Same commit as F1's pin (`review_m12_f1.rs`): both findings were
/// filed against the same M12 review pass.
const PINNED_COMMIT: &str = "302dc8a73630d3a14332f2c0e0f7e9918f683d33";
const RELATIVE_PATH: &str = "crates/roundhouse-server/src/conversations.rs";

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

/// F13's structural claim, checked against the pinned blob: `bind_call` is
/// exactly where the finding says (196), its body is the whole
/// insertion-ordered-map-with-eviction policy (touching only `calls` and
/// `call_order`, never `latest` or `generations`), and no standalone type
/// owns that policy instead.
#[test]
fn bind_call_is_the_only_place_the_call_tables_invariant_is_kept() {
    let Some(blob) = read_pinned_blob(PINNED_COMMIT, RELATIVE_PATH) else {
        eprintln!(
            "F13: pinned commit {PINNED_COMMIT} unreachable (shallow clone?) \
             -- skipping the pinned-line assertions this test exists to make"
        );
        return;
    };
    let lines: Vec<&str> = blob.lines().collect();

    assert_eq!(
        lines[195].trim(),
        "pub fn bind_call(&self, principal: &Principal, call_id: &str, session: SessionId) {",
        "F13: bind_call was claimed to open at line 196"
    );

    // The finding's mechanism: bind_call's own body is the whole eviction
    // policy, and it never touches the other two fields Conversations holds.
    let body = &lines[195..217]; // 196..=217, 0-indexed
    assert_eq!(
        body.last().map(|l| l.trim()),
        Some("}"),
        "F13: bind_call was claimed to close at line 217"
    );
    let body_text = body.join("\n");
    assert!(
        body_text.contains("inner.calls") && body_text.contains("inner.call_order"),
        "F13: bind_call's body should be the table's insert-and-evict policy"
    );
    assert!(
        !body_text.contains("latest") && !body_text.contains("generations"),
        "F13: bind_call was claimed to touch neither `latest` nor \
         `generations` -- finding this string would mean the 'one map \
         cannot disagree' justification does extend to the call table after \
         all, which is the one thing that would make this finding wrong"
    );

    // No standalone type owns the invariant: exactly the three structs the
    // finding's premise depends on, and no CallSites-shaped extraction.
    let struct_lines: Vec<&str> = lines
        .iter()
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with("struct ") || t.starts_with("pub struct ")
        })
        .copied()
        .collect();
    assert_eq!(
        struct_lines,
        vec![
            "pub struct Conversations {",
            "struct Inner {",
            "struct CallSite {"
        ],
        "F13: the call table's eviction policy has no type of its own -- \
         finding a fourth struct here would mean the extraction the finding \
         says is missing already exists"
    );
}

/// F13's evidentiary claim: the only assertion of the call table's
/// `call_order.len() == calls.len()` invariant reaches through
/// `Conversations::lock` into `Inner`'s private fields, which only a
/// same-module test can do. Also checks the finding's own line citation
/// ("conversations.rs:361") against the pinned blob, per the F11 precedent
/// that a citation slip is reported, not treated as invalidating.
#[test]
fn the_invariant_is_asserted_only_by_reaching_through_the_lock() {
    let Some(blob) = read_pinned_blob(PINNED_COMMIT, RELATIVE_PATH) else {
        eprintln!(
            "F13: pinned commit {PINNED_COMMIT} unreachable (shallow clone?) \
             -- skipping the pinned-line assertions this test exists to make"
        );
        return;
    };
    let lines: Vec<&str> = blob.lines().collect();

    // The line the finding's prose cites (361, 1-indexed) is a comment
    // inside the test that performs the assertion, not the assertion
    // itself -- a one-line-off pointer into the right test, same shape as
    // F11's 729-vs-730 slip.
    assert_eq!(
        lines[360].trim(),
        "// Re-binding an id already held must not grow the order queue past the",
        "F13: the finding's cited line 361 is a comment, not the assertion \
         it's describing -- confirming this is the citation slip, and that \
         it points at the right test rather than the wrong one"
    );

    // The actual invariant assertion, ten lines later, in the same test.
    assert_eq!(
        lines[370].trim(),
        "assert_eq!(ordered, held);",
        "F13: call_order.len() == calls.len() is actually asserted at line \
         371, inside the same test the 361 citation names"
    );
    assert!(
        lines[368].contains("conversations.lock().call_order.len()")
            && lines[369].contains("conversations.lock().calls.len()"),
        "F13: the invariant is read via conversations.lock() -- a private \
         method returning a guard over Inner's private fields, reachable \
         here only because `mod tests` is a descendant module"
    );

    // The control this finding does NOT dispute: the invariant does hold,
    // including under the one case that could break it (rebinding an id
    // already present must not double-push call_order). If this test ever
    // failed, F13's premise ("the invariant is maintained inline") would
    // still be a true description of *where* -- just not of code that
    // works, which would be a correctness finding, not this one.
    assert_eq!(
        lines[358].trim(),
        "assert_eq!(conversations.lock().call_order.len(), REMEMBERED_CALLS);",
        "F13: the cap invariant asserted just above the cited comment"
    );
}
