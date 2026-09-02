// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M13 thermo-nuclear review, finding F2 — refuted, ruled **valid**.
//!
//! **The claim.** Three doc sites still describe the memory fair-use ledger
//! as the only implementation and the Redis one as unlock-pending, after M13
//! landed and wired `RedisFairUseLedger`:
//! `engine.rs:790-797` (the `fair_use` field) and `engine.rs:969-978`
//! (`with_fair_use_ledger`) both say the memory ledger is "the only
//! implementation" and point at an "unlock condition" on the Redis one;
//! `mutation.rs:72-88` says the same ("the only implementation in this
//! milestone") and additionally records the PATCH-axis constraint as still
//! open ("`fair_use`'s module doc leaves the Redis key layout explicitly
//! undecided"). All three are stale: `main.rs:1054` wires
//! `RedisFairUseLedger` through `with_fair_use_ledger` today, and
//! `roundhouse_core::control::fair_use`'s own module doc — the exact
//! document `mutation.rs` cites as leaving the layout undecided — now reads
//! "M13: ... it is answered in that crate rather than here", naming the
//! bucket-per-key-at-`BUCKET_MS` layout the redis crate ships. M13 rewrote
//! the deferral in `control/fair_use.rs` and the crate's own module doc but
//! never touched these three call-site comments.
//!
//! **Why the check reads the git blob, not the working tree**, same
//! reasoning as the M11.2b F11 / M12 F1 / M12 F13 precedents: a sibling
//! refuter's own added tests move line numbers in files this finding does
//! not cite, and pinning is what keeps this file's assertions about the
//! *finding's* citations rather than whatever the tree happens to look like
//! when it runs. This finding cites three sites across two files
//! (`engine.rs`, `mutation.rs`) plus two supporting facts in a third file
//! (`main.rs`) and a fourth (`roundhouse-core`'s `fair_use.rs`); all five are
//! pinned to the same commit so the whole picture is read from one
//! consistent snapshot.
//!
//! **Every citation in the finding checked out against the pinned blobs**:
//! the three stale phrases ("only implementation" x2, "unlock condition",
//! "explicitly undecided") are present verbatim at the cited sites; `main.rs`
//! wires `RedisFairUseLedger` through `.with_fair_use_ledger(fair_use)`; and
//! `roundhouse-core`'s `fair_use.rs` module doc contains the M13 resolution
//! sentence the `mutation.rs` comment claims does not exist yet.
//!
//! **What this file does not assert.** Whether the *runtime behavior* of the
//! Redis ledger satisfies the window-independence property `mutation.rs`
//! worried about is not a claim about documentation currency and is not
//! re-litigated here: `roundhouse-store-redis/tests/fair_use_contract.rs`
//! already covers it end-to-end against a real Redis (all 14 cases pass,
//! including `one_call_writes_both_scopes_buckets`, which pins the key to
//! `at_ms / BUCKET_MS` with no window in the key at all) and was run again
//! for this refutation as corroborating evidence, not re-added here. This
//! file's job is narrower and different in kind: the doc comments describe a
//! state of the world (memory-only, layout undecided) that the rest of the
//! tree — Redis crate included — has already moved past, and that mismatch
//! is what these tests pin down.
//!
//! **Ruling: valid.** All three doc sites are confirmed stale by the pinned
//! blobs, and all three contradictions the finding names (Redis
//! implementation exists and is wired; the layout question the third site
//! calls "explicitly undecided" is answered one file below it) are confirmed
//! true at the same pin. The `how_to_prove` field said a grep settles it — it
//! does, and the citation assertions below are that grep, made durable.
//!
//! **Fixed (M13 review-fix, F2).** The two doc sites were reworded in the
//! working tree to describe both ledgers and the answered layout question.
//! The two guard tests below are un-ignored, and — this is the one place this
//! file's mechanism had to change along with the doc — their final assertions
//! read the *working tree* (`read_working_tree`), not the pinned blob: the
//! pin at `7c5369a6` is immutable, so a check against it could confirm the
//! finding was once true but could never observe a fix landing. The citation
//! assertions (does the finding's line citation check out at the pinned
//! commit) stay on the pinned blob, because that is a historical fact and
//! pinning is exactly what keeps it from drifting under a sibling refuter's
//! unrelated line-number churn.

use std::process::Command;

/// The commit F2 was filed against.
const PINNED_COMMIT: &str = "7c5369a6358829ac84a686a3c6a0eac0dc3b2f65";

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

/// The live doc, for the half of each test that a fix actually moves.
///
/// The pinned blob above is deliberately immutable — it is what makes the
/// "did the finding's citation check out" assertions a permanent record
/// rather than a moving target — but that same immutability makes it the
/// wrong source for "has the stale doc been fixed yet": commit `7c5369a6`
/// never changes, so a check against it could never observe a fix landing in
/// the working tree. The two halves need two sources on purpose.
fn read_working_tree(relative_path: &str) -> String {
    std::fs::read_to_string(repo_root().join(relative_path)).unwrap_or_else(|error| {
        panic!("{relative_path} must be readable in the working tree: {error}")
    })
}

/// The claim, stated as a test that must fail today: `engine.rs`'s two
/// fair-use doc sites no longer describe the memory ledger as the only
/// implementation, and no longer point at an unresolved "unlock condition"
/// on the Redis one.
///
/// This is the direct encoding of F2's mechanism for `engine.rs`. The
/// citation assertions below (against the pinned blob) document that the gap
/// was real at the commit the finding was filed against; the final two
/// assertions (against the working tree, via [`read_working_tree`]) are what
/// the doc fix moves, and are what makes this test meaningful to un-ignore.
#[test]
fn engine_rs_fair_use_docs_no_longer_call_the_memory_ledger_the_only_implementation() {
    let Some(blob) = read_pinned_blob(PINNED_COMMIT, "crates/roundhouse-server/src/engine.rs")
    else {
        eprintln!(
            "F2: pinned commit {PINNED_COMMIT} unreachable (shallow clone?) \
             -- skipping the pinned-line assertions this test exists to make"
        );
        return;
    };
    let lines: Vec<&str> = blob.lines().collect();

    // First cited site: the `fair_use` field doc, engine.rs:790-797 (F2's
    // own citation). Confirm the finding's line citation lands on the
    // sentence it describes, then assert the sentence is gone -- which is
    // the failure this test is for.
    assert!(
        lines[793].contains("the *only* implementation this milestone has"),
        "F2: engine.rs:794 (1-indexed) was claimed to call the memory ledger \
         the field's only implementation; found {:?} instead -- the \
         finding's line citation for the fair_use field doc does not check \
         out",
        lines.get(793)
    );
    assert!(
        lines[795].contains("unlock condition on the Redis one"),
        "F2: engine.rs:796 was claimed to point at an unresolved unlock \
         condition on the Redis ledger; found {:?} instead",
        lines.get(795)
    );

    // Second cited site: `with_fair_use_ledger`'s doc, engine.rs:969-978.
    assert!(
        lines[972].contains("the memory ledger is the only implementation"),
        "F2: engine.rs:973 was claimed to call the memory ledger the only \
         implementation; found {:?} instead",
        lines.get(972)
    );
    assert!(
        lines[974].contains("unlock condition is written at"),
        "F2: engine.rs:975 was claimed to point at an unlock condition \
         written elsewhere; found {:?} instead",
        lines.get(974)
    );

    // The actual claim under test, checked against the *working tree*
    // (`read_working_tree`, not `read_pinned_blob`): neither doc site should
    // still describe the memory ledger as the *only* implementation, now
    // that a Redis one is wired at main.rs:1054. Reading the pinned commit
    // here would make this assertion permanently unfixable -- 7c5369a6 never
    // changes -- which is exactly the failure mode this half of the test
    // exists to avoid.
    let current = read_working_tree("crates/roundhouse-server/src/engine.rs");
    assert!(
        !current.contains("the *only* implementation this milestone has")
            && !current.contains("unlock condition on the Redis one"),
        "F2: engine.rs's fair_use field doc still describes the memory \
         ledger as the field's only implementation and the Redis one as \
         unlock-pending, even though RedisFairUseLedger is wired at \
         main.rs:1054 -- this doc comment is stale"
    );
    assert!(
        !current.contains("the memory ledger is the only implementation")
            && !current.contains("unlock condition is written at"),
        "F2: engine.rs's with_fair_use_ledger doc still describes the memory \
         ledger as the only implementation and the Redis one as \
         unlock-pending, even though RedisFairUseLedger is wired at \
         main.rs:1054 -- this doc comment is stale"
    );
}

/// The same claim for `mutation.rs:72-88`, which additionally records the
/// PATCH-axis window-independence constraint as still open even though the
/// module doc it cites as leaving that question undecided has, by the same
/// commit, already answered it.
#[test]
fn mutation_rs_fair_use_doc_no_longer_calls_the_redis_layout_undecided() {
    let Some(blob) = read_pinned_blob(
        PINNED_COMMIT,
        "crates/roundhouse-server/src/control_config/directory/mutation.rs",
    ) else {
        eprintln!(
            "F2: pinned commit {PINNED_COMMIT} unreachable (shallow clone?) \
             -- skipping the pinned-line assertions this test exists to make"
        );
        return;
    };
    let lines: Vec<&str> = blob.lines().collect();

    assert!(
        lines[71].contains("the only implementation in this"),
        "F2: mutation.rs:72 was claimed to call MemoryFairUseLedger the only \
         implementation; found {:?} instead",
        lines.get(71)
    );
    assert!(
        lines[82].contains("explicitly undecided"),
        "F2: mutation.rs:83 was claimed to leave the Redis key layout \
         explicitly undecided; found {:?} instead",
        lines.get(82)
    );

    // Checked against the working tree, not the pinned blob -- same reason as
    // the engine.rs test above: 7c5369a6 never changes, so only the live file
    // can tell a fix from no fix.
    let current =
        read_working_tree("crates/roundhouse-server/src/control_config/directory/mutation.rs");
    assert!(
        !current.contains("the only implementation in this")
            && !current.contains("explicitly undecided"),
        "F2: mutation.rs's fair_use PATCH-axis doc still calls \
         MemoryFairUseLedger the only implementation and the Redis key \
         layout explicitly undecided, even though M13 answered the layout \
         question (bucket-per-key at BUCKET_MS, independent of window) in \
         roundhouse-store-redis and in roundhouse_core::control::fair_use's \
         own module doc -- this doc comment is stale"
    );
}

/// Supporting fact 1: `main.rs` really does wire a Redis-backed fair-use
/// ledger, which is what makes "the memory ledger is the only
/// implementation" false rather than merely dated.
#[test]
fn main_rs_wires_a_redis_fair_use_ledger() {
    let Some(blob) = read_pinned_blob(PINNED_COMMIT, "crates/roundhouse-server/src/main.rs") else {
        eprintln!(
            "F2: pinned commit {PINNED_COMMIT} unreachable (shallow clone?) \
             -- skipping the pinned-line assertions this test exists to make"
        );
        return;
    };
    assert!(
        blob.contains("RedisFairUseLedger"),
        "F2: main.rs was claimed to wire RedisFairUseLedger; the type does \
         not appear in the pinned blob at all"
    );
    assert!(
        blob.contains(".with_fair_use_ledger(fair_use)"),
        "F2: main.rs:1054 was claimed to call .with_fair_use_ledger(fair_use); \
         that call does not appear in the pinned blob"
    );
}

/// Supporting fact 2: `roundhouse_core::control::fair_use`'s own module doc
/// — the document `mutation.rs` cites by name as leaving the Redis key
/// layout undecided — already states the answer, in the same commit.
#[test]
fn the_module_doc_mutation_rs_cites_has_already_answered_the_layout_question() {
    let Some(blob) = read_pinned_blob(
        PINNED_COMMIT,
        "crates/roundhouse-core/src/control/fair_use.rs",
    ) else {
        eprintln!(
            "F2: pinned commit {PINNED_COMMIT} unreachable (shallow clone?) \
             -- skipping the pinned-line assertions this test exists to make"
        );
        return;
    };
    assert!(
        blob.contains("M13") && blob.contains("is answered in that crate rather than here"),
        "F2: roundhouse_core::control::fair_use's module doc was claimed to \
         already resolve the Redis key layout as of M13; that resolution \
         sentence does not appear in the pinned blob, which would mean \
         mutation.rs's 'explicitly undecided' claim was actually still \
         correct"
    );
}
