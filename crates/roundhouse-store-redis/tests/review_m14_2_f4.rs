// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.2 thermo-nuclear review, finding F4 — refuted, then closed.
//!
//! **The claim.** `correlation/contract.rs`'s module doc still said the
//! memory maps were bounded by *capacity* and the Redis maps by *TTL*, and
//! gave that split as the reason no staleness assertion lived at the contract
//! level: "asserting a cap would fail a backend that expires by time, and
//! asserting an expiry would fail one that does not." M14.2 (R-S1) put both
//! binding families' *age* bound on the same core constants
//! ([`CALL_BINDING_STALENESS_MS`], [`THREAD_BINDING_STALENESS_MS`]), so the
//! stated reason for keeping expiry unshared no longer described what the two
//! implementations did. R-S4 promised "the contract's staleness assertions
//! over both implementations"; the assertion instead existed twice, through
//! two unrelated seams, and `contract.rs`'s own macro list had no staleness
//! entry.
//!
//! **Ruling: valid, and closed.** The contract now carries
//! `a_binding_past_its_staleness_bound_is_absent_and_the_next_write_is_a_first_write`,
//! written once and generated for every instantiation by
//! `correlation_maps_contract_suite!`, taking an `AdvancePastTheBound` hook
//! from each: the memory maps move a scripted clock, these maps shorten their
//! per-handle TTL and wait it out. The shared text never sleeps; an
//! instantiation may wait out an expiry it owns. `contract.rs`'s module doc
//! says so where it used to say the opposite.
//!
//! The static control below reads the *pinned* blob rather than the working
//! tree: it is a control over what the file said at the M14.2 rung's own
//! commit, and reading it from a tree the fix has since rewritten would turn
//! a control into a tripwire. The behavioral control is what a grep cannot
//! show — one assertion, both backends, one run — and it needs a real Redis.

mod common;

use std::process::Command;

use roundhouse_core::control::correlation::contract::{
    AdvancePastTheBound,
    a_binding_past_its_staleness_bound_is_absent_and_the_next_write_is_a_first_write,
};
use roundhouse_core::control::correlation::{
    CALL_BINDING_STALENESS_MS, MemoryCorrelationMaps, THREAD_BINDING_STALENESS_MS,
};
use roundhouse_store_redis::RedisCorrelationMaps;
use roundhouse_store_redis::test_support::url_from_env;

const CONTRACT_RELATIVE_PATH: &str = "crates/roundhouse-core/src/control/correlation/contract.rs";
/// The M14.2 rung's own commit — `HEAD` at review time, immediately after
/// R-S1 unified both binding families on the same core staleness constants.
const PINNED_COMMIT_PEAK: &str = "94d09049197dc00604ddd6d3c85cf7ef15bb4e38";
/// The M14.1 thermo-nuclear review commit, immediately prior to the M14.2 rung.
const PINNED_COMMIT_BEFORE: &str = "2d79bb6";

fn repo_root() -> std::path::PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    // manifest_dir is crates/roundhouse-store-redis; the workspace root is
    // two levels up.
    std::path::Path::new(manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/roundhouse-store-redis has a workspace root two levels up")
        .to_path_buf()
}

/// `None` when the pinned commit is not reachable (e.g. a shallow clone) —
/// callers skip the pinned assertions rather than silently falling back to
/// the mutable working tree.
fn read_pinned_blob(commit: &str, relative_path: &str) -> Option<String> {
    let output = Command::new("git")
        .current_dir(repo_root())
        .arg("show")
        .arg(format!("{commit}:{relative_path}"))
        .output()
        .expect("git is available in this environment");
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).expect("source files are UTF-8"))
}

fn contract_source() -> String {
    let path = repo_root().join(CONTRACT_RELATIVE_PATH);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {path:?}: {error}"))
}

/// The names in the suite macro's single `@list`, whatever revision the
/// source came from.
fn listed_contract_tests(src: &str) -> Vec<String> {
    let list_start = src
        .find("(@list $attrs:tt $make:expr) => {")
        .expect("F4: the macro's @list arm should exist");
    src[list_start..]
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed.ends_with(',')
                && !trimmed.starts_with("//")
                && trimmed.chars().next().is_some_and(|c| c.is_alphabetic())
        })
        .map(|line| line.trim().trim_end_matches(',').to_string())
        .collect()
}

/// Passing control, over the pinned blob: at the M14.2 rung's own commit
/// `contract.rs` carried exactly the doc language the finding quotes, was
/// untouched by that rung's diff, and its macro list was the nine names the
/// finding says it was — none of them about staleness. All static, no Redis.
#[test]
fn contract_rs_doc_and_diff_match_what_f4_cites() {
    let Some(pinned) = read_pinned_blob(PINNED_COMMIT_PEAK, CONTRACT_RELATIVE_PATH) else {
        eprintln!(
            "F4: the pinned commit is unreachable (shallow clone?) — skipping \
             the pinned-source assertions this control exists to make"
        );
        return;
    };

    assert!(
        pinned.contains("memory maps evict by capacity per principal"),
        "F4: contract.rs's module doc was claimed to still say memory evicts \
         by capacity"
    );
    assert!(
        pinned.contains("the Redis maps expire by"),
        "F4: contract.rs's module doc was claimed to still say Redis expires \
         by TTL"
    );
    assert!(
        pinned.contains("asserting an expiry would fail one that"),
        "F4: contract.rs was claimed to still give \"asserting an expiry \
         would fail one that does not\" as the reason no shared staleness \
         assertion exists"
    );
    assert_eq!(
        pinned.matches("stale").count(),
        0,
        "F4: contract.rs was claimed to mention staleness zero times, \
         despite both implementations now sharing a staleness bound"
    );

    // git diff 2d79bb6..94d0904 --stat lists no contract.rs: the file the
    // M14.2 rung shipped is byte-identical to the one the prior review saw.
    let diff = Command::new("git")
        .current_dir(repo_root())
        .args([
            "diff",
            &format!("{PINNED_COMMIT_BEFORE}..{PINNED_COMMIT_PEAK}"),
            "--stat",
            "--",
            CONTRACT_RELATIVE_PATH,
        ])
        .output()
        .expect("git is available in this environment");
    assert!(
        diff.status.success(),
        "F4: git diff over the pinned commits should succeed"
    );
    assert!(
        String::from_utf8(diff.stdout).unwrap().trim().is_empty(),
        "F4: contract.rs was claimed to be unchanged by the M14.2 diff \
         (2d79bb6..94d0904) — a nonempty --stat here means it did change and \
         the finding's premise is wrong"
    );

    let names = listed_contract_tests(&pinned);
    assert_eq!(
        names.len(),
        9,
        "F4: the macro's contract-suite list was claimed to name exactly \
         nine tests at that commit; found {names:?}"
    );
    assert!(
        names.iter().all(|name| !name.contains("stale")),
        "F4: none of the nine were claimed to mention staleness; found {names:?}"
    );
}

/// The assertion that was missing: the contract carries a staleness test, it
/// is generated by the suite macro rather than written per backend, and no
/// instantiation can opt out of it — the `aged` hook is a required argument,
/// so a backend that skipped it would not compile.
///
/// This is the F4 red assertion, un-ignored: it fails the moment the shared
/// assertion is deleted, or quietly demoted back to something one backend
/// writes for itself.
#[test]
fn the_contract_suite_carries_the_staleness_assertion_for_every_instantiation() {
    let src = contract_source();
    const SHARED: &str =
        "a_binding_past_its_staleness_bound_is_absent_and_the_next_write_is_a_first_write";

    assert!(
        src.contains(&format!("pub async fn {SHARED}")),
        "F4: the staleness assertion should be a contract function, written \
         once beside the other nine"
    );
    assert!(
        src.contains("(@staleness ($(#[$attr:meta])*) $aged:expr) => {"),
        "F4: and generated by the suite macro, so an instantiation gets it \
         by instantiating rather than by remembering to"
    );
    assert!(
        src.contains("pub type AdvancePastTheBound"),
        "F4: through a hook each instantiation supplies — the memory maps \
         move a scripted clock, the Redis maps wait out a shortened TTL — so \
         the shared text never sleeps"
    );

    // Both public arms of the macro require the hook. A backend that passed
    // only `$make` matches no arm at all, which is the strongest form of
    // "no instantiation can skip this one".
    let public_arms = src.matches("aged = $aged:expr $(,)?) => {").count();
    assert_eq!(
        public_arms, 2,
        "F4: both the gated and the ungated arm should require the hook — an \
         optional one is an assertion the next backend forgets"
    );

    // And each instantiation in the tree really does hand it one.
    for instantiation in [
        "crates/roundhouse-core/src/control/correlation/tests.rs",
        "crates/roundhouse-store-redis/tests/correlation_contract.rs",
    ] {
        let src = std::fs::read_to_string(repo_root().join(instantiation))
            .unwrap_or_else(|error| panic!("reading {instantiation}: {error}"));
        assert!(
            src.contains("aged = "),
            "F4: {instantiation} instantiates the suite and must hand it an \
             advance-past-the-bound hook"
        );
    }
}

/// The behavioral half, over a real Redis: the *shipped* contract assertion —
/// one text, no per-backend special-casing — run against both
/// implementations from one test. That a single shared assertion works on
/// both is what falsifies the doc's old reason for keeping expiry unshared
/// ("asserting an expiry would fail one that does not"), and it is the one
/// thing a source grep cannot show.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn one_shared_staleness_assertion_passes_on_both_backends() {
    // Memory: moved past the wider of the two bounds through a scripted
    // clock — no real wait.
    let now = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1_000));
    let clock_now = std::sync::Arc::clone(&now);
    let memory = MemoryCorrelationMaps::new()
        .with_clock(move || clock_now.load(std::sync::atomic::Ordering::SeqCst));
    let advance: AdvancePastTheBound = Box::new(move || {
        now.fetch_add(
            CALL_BINDING_STALENESS_MS.max(THREAD_BINDING_STALENESS_MS) + 1,
            std::sync::atomic::Ordering::SeqCst,
        );
        Box::pin(std::future::ready(()))
    });
    a_binding_past_its_staleness_bound_is_absent_and_the_next_write_is_a_first_write(
        &memory, advance,
    )
    .await;

    // Redis: shortened with with_binding_ttls to the same small bound on
    // both families, then a genuine wait past it — the identical assertion.
    let redis = RedisCorrelationMaps::connect(url_from_env())
        .await
        .expect("Redis named by ROUNDHOUSE_TEST_REDIS_URL must be reachable")
        .with_binding_ttls(80, 80);
    let advance: AdvancePastTheBound = Box::new(|| {
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        })
    });
    a_binding_past_its_staleness_bound_is_absent_and_the_next_write_is_a_first_write(
        &redis, advance,
    )
    .await;
}
