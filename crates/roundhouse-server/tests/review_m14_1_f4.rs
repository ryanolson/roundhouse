// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.1 review, finding F4 — refuted, then fixed.
//!
//! **The claim.** `correlation.rs:225-232` justifies giving
//! `MemoryCorrelationMaps` a sync inherent surface *beside* the
//! `CorrelationMaps` trait by naming a specific beneficiary: "a caller holding
//! the concrete type — `Conversations`, whose surface is synchronous and
//! whose callers this rung deliberately does not touch — therefore pays no
//! runtime for a seam it is not crossing." The finding says this is false on
//! both halves of the premise: `Conversations` (`conversations.rs:153`) holds
//! `Arc<dyn CorrelationMaps>`, not the concrete type, and every one of its
//! public methods is `pub async fn` (~40 call sites gained `.await` this
//! rung), so its surface is not synchronous either. The six inherent methods
//! consequently have no caller anywhere outside this file's own three cap
//! tests. A second, independent contradiction: the `generations` field's own
//! doc at `:240` ("how many times each key's history has failed the prefix
//! check") describes a failure counter, while the trait method that reads the
//! same field, `CorrelationMaps::generation` at `:149`, documents it as "The
//! generation `key` was last committed at" — two different data models for
//! one `HashMap<String, u32>`.
//!
//! **No Redis needed.** Every premise here is either a structural property of
//! two source files (which type a field holds, which fn signatures are
//! `async`) or a piece of doc text; none of it depends on `MemoryCorrelationMaps`
//! vs. the Redis backend, or a store round-trip. `MemoryCorrelationMaps` itself
//! is exercised in-process, no external service.
//!
//! **Every structural citation checked out, at refute time.**
//! `conversations.rs:153` declared `maps: Arc<dyn CorrelationMaps>`; all nine
//! of its correlation-surface public methods were `pub async fn`. Grepping
//! the whole tree for direct (non-`.await`) calls to the six inherent methods
//! on a `maps` binding turned up matches only inside `correlation.rs`'s own
//! `#[cfg(test)] mod tests` — every other call site awaited the trait method.
//! The `generations` field doc's "how many times ... has failed" and the
//! trait doc's "last committed at" were both present verbatim, describing the
//! same field two incompatible ways.
//!
//! **Ruling: valid — and fixed the way the beneficiary turned out false, not
//! the way the doc first read.** The doc's premise could have been resolved
//! either by making `Conversations` the concrete, synchronous type it
//! described, or by admitting the premise was false and removing the surface
//! it justified. The former would have undone `Arc<dyn CorrelationMaps>` —
//! exactly the seam R-C4 needs to swap in the Redis backend — for a caller
//! that does not exist; the latter costs nothing, since the six inherent
//! methods had no caller outside this file's own three cap tests. So
//! `MemoryCorrelationMaps`'s inherent sync surface is gone: the three cap
//! tests now call the trait methods (`.await`ed, like every other caller),
//! the `CorrelationMaps` impl reads the lock directly instead of delegating
//! to a deleted inherent method, and both contradicted docs are corrected —
//! the struct doc no longer claims a synchronous beneficiary, and the
//! `generations` field doc now says what the trait doc already said.

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

const CORRELATION_RS: &str = "crates/roundhouse-core/src/control/correlation.rs";
/// The memory implementation moved to a sibling file in M14.2's review round
/// (R-S5); the working-tree assertions read both halves.
const MEMORY_RS: &str = "crates/roundhouse-core/src/control/correlation/memory.rs";
const CONVERSATIONS_RS: &str = "crates/roundhouse-server/src/conversations.rs";
const PINNED_COMMIT_PEAK: &str = "b9d4d1244fd281a2314615dcfa5e2615bb812bbe"; // M14.1 rung itself, pre-fix

fn read(relative_path: &str) -> String {
    std::fs::read_to_string(repo_root().join(relative_path))
        .unwrap_or_else(|e| panic!("F4: {relative_path} should be readable: {e}"))
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

/// Strips the doc-comment `///` markers and collapses line wraps to single
/// spaces, so a check against a quoted sentence survives the comment being
/// re-wrapped at a different column without changing its meaning.
fn normalize_doc(src: &str) -> String {
    src.lines()
        .map(|l| l.trim().trim_start_matches("///").trim())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pins the doc text the finding quotes, at the pinned pre-fix commit rather
/// than the working tree, since the fix rewrites this doc.
#[test]
fn the_doc_named_conversations_as_holding_the_concrete_type_over_a_sync_surface() {
    let Some(blob) = read_pinned_blob(PINNED_COMMIT_PEAK, CORRELATION_RS) else {
        eprintln!(
            "F4: pinned commit {PINNED_COMMIT_PEAK} unreachable (shallow clone?) \
             -- skipping the pinned-blob assertions this test exists to make"
        );
        return;
    };
    let normalized = normalize_doc(&blob);
    assert!(
        normalized.contains(
            "A caller holding the concrete type — `Conversations`, whose surface is synchronous and whose \
             callers this rung deliberately does not touch — therefore pays no runtime"
        ),
        "F4: correlation.rs was claimed to justify the inherent sync methods by \
         naming Conversations as a caller of the concrete type with a \
         synchronous surface — that exact sentence was not found (even after \
         normalizing line wraps), so this test does not reach the claim as quoted"
    );
    assert!(
        normalized.contains(
            "Its methods are inherent as well as trait methods, and the inherent ones \
             are not `async`."
        ),
        "F4: correlation.rs was claimed to open this justification by saying \
         MemoryCorrelationMaps's methods are inherent as well as trait methods"
    );
}

/// **The fix, over the working tree.** The doc no longer claims a
/// synchronous beneficiary that does not exist -- it says the inherent
/// surface is gone because every caller (`Conversations` included) now holds
/// the maps behind `Arc<dyn CorrelationMaps>`.
#[test]
fn the_doc_no_longer_claims_a_synchronous_beneficiary() {
    let src = read(CORRELATION_RS);
    assert!(
        !src.contains("whose surface is synchronous and whose"),
        "F4: correlation.rs should no longer justify an inherent sync surface \
         by naming Conversations as synchronous -- it isn't"
    );
    assert!(
        !src.contains("pub fn generation(&self, key: &str) -> Option<u32>"),
        "F4: MemoryCorrelationMaps should no longer expose an inherent, \
         non-async `generation`"
    );
    assert!(
        !src.contains(
            "pub fn bind_call(&self, principal: &Principal, call_id: &str, session: &SessionId)"
        ),
        "F4: MemoryCorrelationMaps should no longer expose an inherent, \
         non-async `bind_call`"
    );
}

/// Half one of the contradiction: `Conversations` does not hold the concrete
/// type the doc names — it holds the trait object.
#[test]
fn conversations_holds_the_trait_object_not_the_concrete_type() {
    let src = read(CONVERSATIONS_RS);
    assert!(
        src.contains("maps: Arc<dyn CorrelationMaps>"),
        "F4: conversations.rs was claimed to declare `maps: Arc<dyn CorrelationMaps>`, \
         the trait object — not the concrete MemoryCorrelationMaps the doc's \
         justification names as the beneficiary"
    );
    assert!(
        !src.contains("maps: MemoryCorrelationMaps")
            && !src.contains("maps: Arc<MemoryCorrelationMaps>"),
        "F4: conversations.rs should not also hold the concrete type directly"
    );
}

/// Half two of the contradiction: every one of `Conversations`'s public
/// correlation-surface methods is `async`, so "whose surface is synchronous"
/// does not describe it either.
///
/// `bind` and `fork` are not in this list any more (M15, H1): both were
/// deleted once the migration to `commit` this file's own F4 finding is
/// about (M14.0) left them with no serving-path caller — a fixture that
/// still needs their exact shape now reads
/// `roundhouse_server::test_support::{bind_conversation, fork_conversation}`,
/// which are free functions and were never inherent methods this guard
/// pinned.
#[test]
fn conversations_public_correlation_surface_is_entirely_async() {
    let src = read(CONVERSATIONS_RS);
    let methods = [
        "generation",
        "commit",
        "resolve",
        "bind_call",
        "session_of_call",
        "bind_thread",
        "session_of_thread",
    ];
    for method in methods {
        let needle_async = format!("pub async fn {method}(");
        let needle_sync = format!("pub fn {method}(");
        assert!(
            src.contains(&needle_async),
            "F4: Conversations::{method} was claimed to be `pub async fn`"
        );
        assert!(
            !src.contains(&needle_sync),
            "F4: Conversations::{method} should not also exist as a plain \
             `pub fn` (synchronous) sibling"
        );
    }
}

/// **The fix's other half, over the working tree.** With the inherent sync
/// methods deleted, every call to one of the six names on a `maps` receiver,
/// across the whole tree, is `.await`ed -- including this file's own three
/// former cap tests, which now call the trait method like everyone else. No
/// direct (non-`.await`) call to any of the six should remain anywhere.
///
/// Mirrors the finding's own `how_to_prove` grep, but line-based `.await`
/// filtering alone over-reports: `.await` commonly lands on its own
/// continuation line (`maps.bind_call(...)\n    .await\n    .unwrap();`), so
/// a same-line-only check would wrongly flag every such call as "direct".
/// This instead looks for `.await` anywhere before the statement-closing `;`
/// that follows the match, which is what actually distinguishes an awaited
/// trait call from a genuine direct call to a sync method.
#[test]
fn no_call_to_the_six_names_is_direct_now_the_inherent_surface_is_gone() {
    let output = Command::new("grep")
        .current_dir(repo_root())
        .args([
            "-rn",
            r"maps\.\(bind_call\|bind_thread\|session_of_call\|session_of_thread\|set_generation\)(",
            "crates",
            "--include=*.rs",
        ])
        .output()
        .expect("grep is available in this environment");
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "F4: grep should either find matches (0) or find none (1), not error"
    );
    let stdout = String::from_utf8(output.stdout).expect("grep output is UTF-8");

    let mut file_cache: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut direct_calls: Vec<String> = Vec::new();

    for line in stdout.lines() {
        // Format is "path:lineno:content".
        let mut parts = line.splitn(3, ':');
        let path = parts.next().expect("grep -n line has a path");
        let lineno: usize = parts
            .next()
            .expect("grep -n line has a line number")
            .parse()
            .expect("grep -n line number is an integer");

        let file_lines = file_cache.entry(path.to_string()).or_insert_with(|| {
            std::fs::read_to_string(repo_root().join(path))
                .unwrap_or_else(|e| panic!("F4: {path} should be readable: {e}"))
                .lines()
                .map(str::to_string)
                .collect()
        });

        // Look from the match line up to 5 lines forward, stop at the first
        // statement-closing `;`, and check whether `.await` appears in that
        // window -- that is the whole statement the call belongs to.
        let start = lineno - 1;
        let end = (start + 5).min(file_lines.len());
        let window = file_lines[start..end].join("\n");
        let statement = match window.find(';') {
            Some(idx) => &window[..idx],
            None => &window[..],
        };

        if !statement.contains(".await") {
            direct_calls.push(format!("{path}:{lineno}"));
        }
    }

    assert!(
        direct_calls.is_empty(),
        "F4: expected no direct (non-await) calls to any of the six names now \
         that MemoryCorrelationMaps's inherent sync surface is gone -- every \
         caller, including this file's own former cap tests, should go through \
         the async trait method; found: {direct_calls:?}"
    );
}

/// The `generations` field carried two incompatible doc descriptions, at the
/// pinned pre-fix commit: the field's own comment (`:240` in the finding)
/// framed it as a failure counter, while the trait method that reads the same
/// map (`CorrelationMaps::generation`, `:149`) documented it as the committed
/// generation number.
#[test]
fn the_generations_field_doc_contradicted_the_trait_methods_doc() {
    let Some(blob) = read_pinned_blob(PINNED_COMMIT_PEAK, CORRELATION_RS) else {
        eprintln!(
            "F4: pinned commit {PINNED_COMMIT_PEAK} unreachable (shallow clone?) \
             -- skipping the pinned-blob assertions this test exists to make"
        );
        return;
    };
    assert!(
        blob.contains("How many times each *namespaced* cache key's history has failed the"),
        "F4: the generations field doc was claimed to describe a failure count"
    );
    assert!(
        blob.contains("The generation `key` was last committed at, or `None` if no node ever"),
        "F4: the CorrelationMaps::generation trait doc was claimed to describe \
         a committed generation number, not a failure count"
    );
}

/// The red assertion, now green: the two docs should agree on one data model
/// for `generations`, and they do -- the field doc now says what the trait
/// doc already said, and neither describes a failure counter any more.
#[test]
fn the_generations_field_doc_now_agrees_with_the_trait_methods_doc() {
    let src = read(CORRELATION_RS) + &read(MEMORY_RS);
    assert!(
        !src.contains("How many times each *namespaced* cache key's history has failed the"),
        "F4: the generations field doc should no longer describe a failure count"
    );
    assert!(
        src.contains("The generation each *namespaced* cache key was last committed at"),
        "F4: the generations field doc should describe the same committed \
         generation number the trait doc does"
    );
    assert!(
        src.contains("The generation `key` was last committed at, or `None` if no node ever"),
        "F4: the CorrelationMaps::generation trait doc should still describe \
         a committed generation number"
    );
}
