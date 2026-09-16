// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M15 thermo-nuclear review, finding F6 — refuted.
//!
//! **The claim.** H1 removed `Conversations::bind` and `Conversations::fork`
//! (folding both into `Conversations::commit`), and H3 added a cargo-doc
//! warning-count guard to catch dangling doc references left behind by such
//! removals. F6 said the guard was blind to *prose* mentions of the dead
//! names — backtick-quoted method names in a doc comment's running text, not
//! intra-doc links `[`like this`]` — and that six such mentions survived at
//! the time of the finding: `roundhouse-mcp/src/store.rs:61` (in the very
//! file the H3 rung edited), `tests/common/e2e.rs:474`,
//! `tests/codex_e2e.rs:751` and `:2838`,
//! `tests/messages_api_surface.rs` (line 1839 at finding time; the H1/H2
//! fixture migrations that landed first in this same fix rung shifted it to
//! 1812 without touching the sentence itself), plus a sixth, unrelated
//! staleness at `src/http.rs:304` where the 409 doc's opening line still said
//! "disagreed with all `attempts` generations" after H4 split that count into
//! disagreed-vs-busy.
//!
//! **Fixed below.** All five `Conversations::fork` prose mentions now name
//! `Conversations::commit`, the method that does the rebind since H1, and
//! `http.rs:304`'s summary line now says "disagreed with, or found busy,
//! every one" of `attempts`, matching the disagreed-vs-busy split H4 gave
//! the tally two paragraphs below it. `cargo doc`'s warning count could not
//! have caught any of the six — rustdoc lints unresolvable *intra-doc
//! links*, not backtick prose — which is why this file checks the prose
//! directly rather than through that guard.
//!
//! **The `how_to_prove` shape guard was imprecise, and stays fixed here.**
//! Read literally — `grep` for `Conversations::(bind|fork)` outside
//! `test_support.rs` and the review files — the pattern also matches
//! `Conversations::bind_call` and `Conversations::bind_thread`, two methods
//! H1 did *not* remove and whose doc references (`conversations.rs:28,34`,
//! `mcp_surface.rs:344,2017`) are accurate today and must stay unflagged.
//! [`mentions_dead_conversations_method`] is the tightened version: it
//! matches `Conversations::bind`/`Conversations::fork` only when not
//! immediately followed by a word character, so `bind_call` and
//! `bind_thread` never trip it while every real dead-prose mention still
//! does.
//!
//! This crate is `roundhouse-server`, but the dead prose spanned it and
//! `roundhouse-mcp`; the workspace-relative reads below reach across the
//! crate boundary the same way the finding's own citations did.

use std::fs;
use std::path::{Path, PathBuf};

/// Repo root, derived from this crate's `CARGO_MANIFEST_DIR`
/// (`crates/roundhouse-server`) rather than hard-coded, so the test does not
/// depend on the working directory `cargo test` was invoked from.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/roundhouse-server has two ancestors up to the repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn line(src: &str, n: usize) -> &str {
    src.lines()
        .nth(n - 1)
        .unwrap_or_else(|| panic!("file has fewer than {n} lines"))
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The tightened `how_to_prove` shape: matches `Conversations::bind` or
/// `Conversations::fork` only when the character right after `bind`/`fork`
/// is not a word character, so `Conversations::bind_call` and
/// `Conversations::bind_thread` — live methods H1 did not remove — never
/// trip it while a real dead-prose mention of `bind`/`fork` on its own
/// always does. The finding's own grep, `Conversations::(bind|fork)`, had no
/// such boundary and over-matched both live methods (the refuter's
/// correction).
fn mentions_dead_conversations_method(s: &str) -> bool {
    for needle in ["Conversations::bind", "Conversations::fork"] {
        let mut rest = s;
        while let Some(idx) = rest.find(needle) {
            let after = &rest[idx + needle.len()..];
            match after.chars().next() {
                Some(c) if is_word_char(c) => rest = after,
                _ => return true,
            }
        }
    }
    false
}

/// The five dead-prose sites plus the sixth, unrelated staleness at
/// `http.rs:304`, asserted fixed. This is F6's regression guard: it failed
/// on the working tree before the fix, because H1's removal of
/// `Conversations::bind`/`::fork` was never reflected in these six
/// backtick-quoted (link-free, so doc-warning-blind) mentions, and it stays
/// live so a reintroduced `Conversations::fork`/`::bind` mention (not
/// `bind_call`/`bind_thread`, which [`mentions_dead_conversations_method`]
/// does not flag) or a re-widened 409 doc trips it again.
#[test]
fn f6_dead_conversations_fork_bind_prose_and_stale_409_doc_do_not_survive_h1_h4() {
    let store_rs = read("crates/roundhouse-mcp/src/store.rs");
    assert!(
        !mentions_dead_conversations_method(line(&store_rs, 61)),
        "store.rs:61 still opens a section on a removed `Conversations` \
         method:\n{}",
        line(&store_rs, 61)
    );

    let e2e_rs = read("crates/roundhouse-server/tests/common/e2e.rs");
    assert!(
        !mentions_dead_conversations_method(line(&e2e_rs, 474)),
        "tests/common/e2e.rs:474 still names a removed `Conversations` \
         method:\n{}",
        line(&e2e_rs, 474)
    );

    let codex_e2e_rs = read("crates/roundhouse-server/tests/codex_e2e.rs");
    assert!(
        !mentions_dead_conversations_method(line(&codex_e2e_rs, 751)),
        "tests/codex_e2e.rs:751 still names a removed `Conversations` \
         method:\n{}",
        line(&codex_e2e_rs, 751)
    );
    assert!(
        !mentions_dead_conversations_method(line(&codex_e2e_rs, 2838)),
        "tests/codex_e2e.rs:2838 still names a removed `Conversations` \
         method, directly above the test the rung migrated to \
         `fork_conversation`:\n{}",
        line(&codex_e2e_rs, 2838)
    );

    // H1/H2's fixture migrations, which landed earlier in this same fix
    // rung, shifted this file's line count without touching the sentence
    // F6 named: 1839 at finding time, 1812 today.
    let messages_api_surface_rs = read("crates/roundhouse-server/tests/messages_api_surface.rs");
    assert!(
        !mentions_dead_conversations_method(line(&messages_api_surface_rs, 1812)),
        "tests/messages_api_surface.rs:1812 still names a removed \
         `Conversations` method:\n{}",
        line(&messages_api_surface_rs, 1812)
    );

    let http_rs = read("crates/roundhouse-server/src/http.rs");
    assert!(
        !line(&http_rs, 304).contains("disagreed with all `attempts` generations"),
        "http.rs:304 still defines the 409 as disagreeing with *all* \
         `attempts` generations, which stopped being accurate once H4 split \
         the tally into disagreed-vs-busy (the doc's own later paragraphs \
         say so):\n{}",
        line(&http_rs, 304)
    );
}

/// Control: [`mentions_dead_conversations_method`] must not flag
/// `Conversations::bind_call` or `Conversations::bind_thread` — two methods
/// H1 left alone, whose doc mentions (`conversations.rs:28,34`) are live and
/// accurate. This is the guard the finding's own `how_to_prove` grep lacked:
/// run unanchored, `Conversations::(bind|fork)` matches inside `bind_call`
/// and `bind_thread` too, over-reporting nine lines instead of six with two
/// false positives among them (the refuter's correction). The tightened
/// word-boundary check below must not repeat that over-match.
#[test]
fn f6_tightened_guard_does_not_flag_live_bind_call_and_bind_thread() {
    let conversations_rs = read("crates/roundhouse-server/src/conversations.rs");
    assert!(
        line(&conversations_rs, 28).contains("Conversations::bind_call"),
        "expected conversations.rs:28 to document the live \
         `Conversations::bind_call`:\n{}",
        line(&conversations_rs, 28)
    );
    assert!(
        line(&conversations_rs, 34).contains("Conversations::bind_thread"),
        "expected conversations.rs:34 to document the live \
         `Conversations::bind_thread`:\n{}",
        line(&conversations_rs, 34)
    );

    assert!(
        !mentions_dead_conversations_method(line(&conversations_rs, 28)),
        "conversations.rs:28 must not trip the tightened guard: \
         `Conversations::bind_call` is live and accurate, and the whole \
         point of the word boundary is to leave it unflagged:\n{}",
        line(&conversations_rs, 28)
    );
    assert!(
        !mentions_dead_conversations_method(line(&conversations_rs, 34)),
        "conversations.rs:34 must not trip the tightened guard: \
         `Conversations::bind_thread` is live and accurate, and the whole \
         point of the word boundary is to leave it unflagged:\n{}",
        line(&conversations_rs, 34)
    );
}
