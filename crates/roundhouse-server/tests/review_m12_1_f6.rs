// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M12.1 review, finding F6 — refuted, then fixed.
//!
//! **The claim.** `codex_e2e.rs`'s
//! `a_real_codex_binary_is_correlated_by_the_thread_id_it_stamps` (line 1571)
//! carried a bespoke `#[ignore]` reason distinct from the uniform "needs the
//! real codex binary" string every other ignored test in the file carries, and
//! — unlike what that uniform reason promises — a real codex binary alone was
//! not enough to turn it green: the test also needed an upstream that emits a
//! namespaced `function_call`, and no such fixture exists in this tree. So on
//! any box with a real codex, running the suite's own sanctioned invocation
//! (`--include-ignored`) failed this one test while its neighbours passed,
//! silently turning "thirty-five pass" into "N pass, one fails on purpose."
//!
//! **Why refutation could not spawn a real `codex` to prove it directly.**
//! This sandbox has no codex binary on `PATH` or named by
//! `ROUNDHOUSE_TEST_CODEX_BIN` (confirmed: `which codex` and a filesystem
//! search both come up empty), and `codex_e2e.rs`'s own `Rig::start` panics
//! loudly rather than skip when one is missing (`common::e2e::version_probe`,
//! "`--include-ignored` asks for the real binary... failed"). The refuter
//! instead reached the same failure by construction, at the one seam the
//! finding's own mechanism turns on: `Rig::start`/`start_as` hard-code the
//! turn's upstream as `Arc::new(EchoFrontierClient::new(ANSWER))`
//! (codex_e2e.rs:511) with no parameter, builder method, or alternate
//! constructor to swap it for anything else — `common::ScriptedTurns`, the
//! fixture the finding's own doc comment names as what *would* close this, was
//! never wired into `Rig` at all. And `EchoFrontierClient::execute`
//! (roundhouse-fleet/src/frontier.rs:1106-1115) unconditionally returns
//! `FrontierChunk::whole_response(text, ...)` — it has no code path that ever
//! returns `FrontierChunk::ToolCall`. A codex process can only dispatch a
//! `tools/call` for a tool call the model's turn actually asked for; if the
//! engine never commits an `ItemContent::ToolCall` for the turn, a real client
//! — however faithful — has nothing to call, and `calls.len()` is 0, not 1.
//! Driving the exact turn shape the target test drives (one user message, one
//! declared tool, `WireProtocol::OpenAiResponses`) through the production
//! `Engine`, wired the way `Rig::start_as` always wires it, confirmed 0
//! committed `ItemContent::ToolCall` items — a direct reproduction of
//! codex_e2e.rs:1583's `assert_eq!(calls.len(), 1, ...)` failing, with no
//! codex process, no HTTP, no MCP surface involved.
//!
//! **Ruling: valid**, with one correction to the finding's framing. The
//! finding said the missing fixture left condition (2) merely *unbuilt* —
//! implying a future `ScriptedTurns`-based `Rig` variant would close it. The
//! refuter's demonstration showed something narrower and harder: as
//! `Rig::start`/`start_as` were actually written, there was no parameter or
//! seam through which any upstream *other than* the hard-coded
//! `EchoFrontierClient` could reach this test, so `Rig`'s API itself — not
//! just the fixture — would need to change. That did not weaken the finding;
//! it was the same defect one layer further down, reinforcing that F6's
//! prescription (pull the assertions out of a live, `--include-ignored`-swept
//! test until the fixture — and the `Rig` seam to plug it in — both exist) was
//! the right fix. Both unlock conditions (a `ScriptedTurns` upstream emitting
//! the namespaced `function_call`; a `Rig` constructor that accepts an
//! upstream) are open work, not built here.
//!
//! **The fix.** `a_real_codex_binary_is_correlated_by_the_thread_id_it_stamps`
//! no longer carries `#[tokio::test]`/`#[ignore]` at all — it is a plain,
//! `#[allow(dead_code)]` async fn in `codex_e2e.rs` now, so
//! `--include-ignored` cannot see it and cannot fail on it "by design". The
//! refuter's engine-level demonstration above (an ignored test asserting the
//! same 0-not-1 fact) is retired along with it: once the target function is
//! not a test, re-proving that it would fail is no longer informative. What
//! stands in its place is a live guard against the same defect returning —
//! [`every_ignore_in_codex_e2e_carries_a_known_real_binary_reason`] — plus the
//! original control, [`the_uniform_ignore_reason_is_shared_by_multiple_other_tests_in_the_file`],
//! updated to check the post-fix source rather than the retired string.

/// **The guard.** Every `#[ignore = "..."]` left in `codex_e2e.rs` must be one
/// of the file's two known "needs a real binary, and nothing else is wrong"
/// reasons. A bespoke reason appearing here — one naming a second, permanent
/// condition the way the retired test's did — is exactly the shape F6 found:
/// a test that fails by design under the suite's own sanctioned invocation,
/// indistinguishable from a uniform "needs a binary" ignore by anyone reading
/// `cargo test ... -- --include-ignored` output. This is what stops that
/// shape from quietly returning the next time someone writes a real-binary
/// test that cannot pass even with the binary.
#[test]
fn every_ignore_in_codex_e2e_carries_a_known_real_binary_reason() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/codex_e2e.rs");
    let source = std::fs::read_to_string(&path).expect("codex_e2e.rs is a sibling test file");

    const CODEX_ONLY: &str = "needs the real codex binary: --features e2e-codex -- \
         --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH";
    const CODEX_AND_TOPHAM: &str = "needs the real codex and topham binaries: --features \
         e2e-codex -- --include-ignored; ROUNDHOUSE_TEST_CODEX_BIN overrides PATH and \
         ROUNDHOUSE_TEST_TOPHAM_BIN names a built topham";

    let mut found = 0usize;
    for line in source.lines() {
        let Some(rest) = line.strip_prefix("#[ignore = \"") else {
            continue;
        };
        let reason = rest.strip_suffix("\"]").unwrap_or_else(|| {
            panic!(
                "F6: codex_e2e.rs has a multi-line or malformed #[ignore] attribute this \
                 single-line scan cannot parse: {line:?}. Either keep #[ignore] reasons on \
                 one line or extend this guard to reassemble the continuation."
            )
        });
        found += 1;
        assert!(
            reason == CODEX_ONLY || reason == CODEX_AND_TOPHAM,
            "F6: codex_e2e.rs has an #[ignore] reason that is neither of the file's known \
             real-binary reasons. A run with the binary(ies) it names would still fail on \
             purpose, the exact defect F6 found. Reason: {reason:?}"
        );
    }

    assert!(
        found >= 9,
        "F6: expected at least 9 #[ignore] sites in codex_e2e.rs (this file had 10 before \
         the fix, one of which -- the target test -- is no longer a #[test] at all); found \
         {found}"
    );
}

/// Control: the uniform reason string really is shared by the file's other
/// real-binary tests, confirming the guard above is checking a real, existing
/// convention rather than one this file invents. Also confirms the fix
/// actually landed: the retired test's function still exists (renamed to
/// nothing -- same name, same place) but no `#[ignore]` immediately precedes
/// it any more.
#[test]
fn the_uniform_ignore_reason_is_shared_by_multiple_other_tests_in_the_file() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/codex_e2e.rs");
    let source = std::fs::read_to_string(&path).expect("codex_e2e.rs is a sibling test file");

    const UNIFORM: &str = "needs the real codex binary: --features e2e-codex -- --include-ignored; \
         ROUNDHOUSE_TEST_CODEX_BIN overrides PATH";

    let uniform_count = source.matches(UNIFORM).count();
    assert!(
        uniform_count >= 5,
        "F6: expected the uniform reason string to appear on several ignored tests in \
         codex_e2e.rs (control for 'every other #[ignore] ... carries the identical reason \
         string'), found it {uniform_count} time(s)"
    );

    let target_fn = "fn a_real_codex_binary_is_correlated_by_the_thread_id_it_stamps";
    let lines: Vec<&str> = source.lines().collect();
    let target_line = lines
        .iter()
        .position(|l| l.contains(target_fn))
        .unwrap_or_else(|| panic!("F6: sanity check -- {target_fn} should still exist"));
    assert_eq!(
        lines.iter().filter(|l| l.contains(target_fn)).count(),
        1,
        "F6: sanity check -- the target function should exist exactly once in codex_e2e.rs"
    );

    // Post-fix: nothing immediately above the target function's signature is
    // an #[ignore] attribute any more -- it was pulled out of the ignored-test
    // pool entirely (codex_e2e.rs:1580, #[allow(dead_code, ...)]) rather than
    // kept alive under a bespoke reason. This is the fact the guard above
    // exists to keep true.
    let preceding = lines[..target_line]
        .iter()
        .rev()
        .find(|l| l.starts_with('#'))
        .copied()
        .unwrap_or("");
    assert!(
        !preceding.starts_with("#[ignore"),
        "F6: the target function should no longer be directly preceded by an #[ignore] \
         attribute -- the fix pulls it out of the ignored-test pool rather than giving it \
         a bespoke reason. Found: {preceding:?}"
    );
}
