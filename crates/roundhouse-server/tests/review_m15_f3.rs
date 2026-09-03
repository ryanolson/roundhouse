// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M15 thermo-nuclear review, finding F3 — refuted, ruled valid, fixed, and
//! kept here as the guard.
//!
//! **The fix.** Every site the finding names now calls `engine_over_echo`
//! (`messages_api_surface.rs`'s eight, `metrics_end_to_end.rs`'s one,
//! `src/prefix_admission/tests.rs`'s one) or delegates through
//! `single_model_catalog(frontier_spec(..))` (`tests/common/mod.rs::
//! frontier_catalog` and `src/prefix_admission/tests.rs::catalog`). Both
//! guards below are live rather than ignored.
//!
//! **The claim.** H2 (`test_support.rs`'s `engine_over_echo` and
//! `single_model_catalog`) retired the fixtures literally named
//! `fn engine`/`fn engine_over`/`fn catalog`/`fn catalog_for`/`fn spec`, but
//! left the *shape* those fixtures built duplicated by hand in the files
//! that already import the helper: eight sites in `messages_api_surface.rs`,
//! one in `metrics_end_to_end.rs`, and one in
//! `src/prefix_admission/tests.rs` still spell
//! `Engine::new(store, ByteTokenizer, Arc::new(EchoLocalExecutor::new("local
//! answer")), <catalog>, <frontier>, Arc::new(AffinityPolicy::new()),
//! <config>)` out by hand — `engine_over_echo`'s exact seven-argument body,
//! reduced to a four-argument call. Separately, `single_model_catalog(...)`
//! in `prefix_admission/tests.rs` builds the identical one-model catalog
//! `tests/common/mod.rs::frontier_catalog` still builds by hand-rolling
//! `FrontierModelSpec` itself, even though `single_model_catalog` is
//! reachable from that file (`admin_api.rs`, `provider_registry.rs` and
//! `review_m11_2a_f4.rs` already import it from there).
//!
//! **Every citation checked out against the working tree.** All eight
//! `Engine::new(` sites in `messages_api_surface.rs`
//! (lines 264, 328, 374, 452, 573, 766, 1126, 3086), the one in
//! `metrics_end_to_end.rs:286`, and the one in
//! `src/prefix_admission/tests.rs:233` pass
//! `Arc::new(EchoLocalExecutor::new("local answer"))` as the executor and
//! `Arc::new(AffinityPolicy::new())` as the policy — `engine_over_echo`'s
//! fixed two arguments — with only the store, catalog, frontier client and
//! config varying, which is exactly what `engine_over_echo` already takes as
//! parameters. None of the ten needed a different constructor shape (the
//! `metrics_end_to_end.rs` site's non-default `EngineConfig` is not an
//! obstacle: `engine_over_echo` takes `config` as a parameter for exactly
//! this reason, per its own doc comment). `frontier_catalog` in
//! `tests/common/mod.rs` builds a `FrontierModelSpec` whose eight fields
//! equal, one for one, what
//! `single_model_catalog("anthropic", "claude", AnthropicMessages, 0.95,
//! {3.0, 0.3, 3.75, 15.0}, 350.0, 0.002)` already produces — the exact call
//! `prefix_admission/tests.rs::catalog()` makes.
//!
//! One correction to the finding's own citation: `prefix_admission/tests.rs`
//! is `crates/roundhouse-server/src/prefix_admission/tests.rs` (a unit-test
//! sibling of the `prefix_admission` module, `mod tests;` at
//! `prefix_admission.rs:819`), not a file under `tests/` as the finding's
//! prose ("34 more times under tests/") implies. The line numbers (186, 233)
//! and content the finding cites are otherwise exact.
//!
//! **Ruling: valid.** The two guard tests below assert the postcondition H2
//! was for — no hand-rolled duplicate of `engine_over_echo`'s or
//! `single_model_catalog`'s exact shape remains outside the helpers
//! themselves — and both fail today for the reason the finding says they
//! should: the duplication is real and un-folded in the very files that
//! already import the helper that would absorb it. The control test proves
//! the failures are not because the helpers are shape-incompatible with
//! these call sites: built with the values each duplicate already uses, the
//! helpers reproduce those values exactly.
//!
//! **Not asserted here.** Whether every one of the 34 count the finding
//! gives is exact (`AffinityPolicy::new()` alone, `EchoLocalExecutor::new(...)`
//! alone, etc. also appear outside `Engine::new(` call sites for unrelated
//! reasons) is not re-derived; the ten sites named above are individually
//! confirmed, which is enough to prove H2 left this file's residual
//! duplication in place, the mechanism the finding is about.

use std::path::PathBuf;

use roundhouse_core::routing::CacheModel;
use roundhouse_fleet::WireProtocol;
use roundhouse_server::test_support::{frontier_spec, single_model_catalog};

fn repo_root() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    // manifest_dir is crates/roundhouse-server; the workspace root is two
    // levels up.
    PathBuf::from(manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/roundhouse-server has a workspace root two levels up")
        .to_path_buf()
}

fn read(relative_path: &str) -> String {
    std::fs::read_to_string(repo_root().join(relative_path))
        .unwrap_or_else(|e| panic!("{relative_path} should be readable: {e}"))
}

/// True when `content` contains an `Engine::new(` call site whose body (the
/// text up to the next line starting a new statement at the same or lower
/// indent — approximated here by scanning the next 8 lines, generous enough
/// for every site this file cites) passes both of `engine_over_echo`'s fixed
/// arguments: the echo executor answering `"local answer"`, and a plain
/// `AffinityPolicy::new()`. That is exactly `engine_over_echo`'s shape
/// reduced to a hand-written literal.
fn hand_rolled_engine_over_echo_shape_count(content: &str) -> usize {
    let lines: Vec<&str> = content.lines().collect();
    let mut count = 0;
    for (i, line) in lines.iter().enumerate() {
        if !line.contains("Engine::new(") {
            continue;
        }
        let window = lines[i..(i + 8).min(lines.len())].join("\n");
        if window.contains(r#"EchoLocalExecutor::new("local answer")"#)
            && window.contains("AffinityPolicy::new()")
        {
            count += 1;
        }
    }
    count
}

/// F3's first half: the eight `messages_api_surface.rs` sites, the one
/// `metrics_end_to_end.rs` site, and the one `src/prefix_admission/tests.rs`
/// site all still hand-roll `engine_over_echo`'s exact shape, in files that
/// already `use roundhouse_server::test_support::engine_over_echo` (or, for
/// `prefix_admission/tests.rs`, could reach it as `crate::test_support::
/// engine_over_echo` exactly as its neighbor `catalog()` already reaches
/// `crate::test_support::single_model_catalog`).
///
/// Fails today because H2 folded only the fixtures literally named
/// `fn engine`/`fn engine_over`, not every call site with that fixture's
/// shape — the mechanism the finding names.
#[test]
fn no_hand_rolled_engine_over_echo_duplicates_remain_in_the_h2_named_files() {
    let messages_api_surface = read("crates/roundhouse-server/tests/messages_api_surface.rs");
    let metrics_end_to_end = read("crates/roundhouse-server/tests/metrics_end_to_end.rs");
    let prefix_admission_tests = read("crates/roundhouse-server/src/prefix_admission/tests.rs");

    let counts = [
        (
            "messages_api_surface.rs",
            hand_rolled_engine_over_echo_shape_count(&messages_api_surface),
        ),
        (
            "metrics_end_to_end.rs",
            hand_rolled_engine_over_echo_shape_count(&metrics_end_to_end),
        ),
        (
            "src/prefix_admission/tests.rs",
            hand_rolled_engine_over_echo_shape_count(&prefix_admission_tests),
        ),
    ];
    for (file, count) in counts {
        assert_eq!(
            count, 0,
            "{file} still hand-rolls engine_over_echo's exact shape at {count} \
             site(s); H2 should have folded every one of them, not just the \
             fixtures literally named fn engine/fn engine_over"
        );
    }
}

/// F3's second half: `tests/common/mod.rs::frontier_catalog` builds a
/// `FrontierModelSpec` field-for-field identical to what
/// `single_model_catalog` already produces for the same arguments, rather
/// than delegating to it — even though the helper is reachable from that
/// file (three other integration suites already import it from there).
///
/// Fails today: `frontier_catalog`'s body is a hand-rolled
/// `StaticFrontierCatalog::new(vec![FrontierModelSpec { .. }])` literal, not
/// a call to `single_model_catalog`.
#[test]
fn frontier_catalog_delegates_to_single_model_catalog_rather_than_hand_rolling_it() {
    let common_mod = read("crates/roundhouse-server/tests/common/mod.rs");
    let frontier_catalog_body = common_mod
        .split("pub fn frontier_catalog()")
        .nth(1)
        .expect("tests/common/mod.rs defines frontier_catalog")
        .split("\n}\n")
        .next()
        .expect("frontier_catalog's body is closed by a top-level '}'");

    assert!(
        frontier_catalog_body.contains("single_model_catalog("),
        "frontier_catalog's body does not call single_model_catalog — it \
         still hand-rolls the FrontierModelSpec literal directly:\n{frontier_catalog_body}"
    );
}

/// Control for both tests above: `single_model_catalog`, called with the
/// exact values `tests/common/mod.rs::frontier_catalog` and
/// `prefix_admission/tests.rs::catalog()` already use, produces a spec
/// field-for-field identical to what those two hand-rolled literals encode.
/// This is what proves the two failures above are a real fold-through-a-
/// shared-helper omission, not a case where the helper cannot actually
/// reproduce what the duplicate spells out.
#[test]
fn single_model_catalog_reproduces_the_values_the_duplicates_hand_roll() {
    let built = single_model_catalog(frontier_spec(
        "anthropic",
        "claude",
        WireProtocol::AnthropicMessages,
    ));
    let spec = &built.models()[0];

    assert_eq!(spec.provider, "anthropic");
    assert_eq!(spec.model, "claude");
    assert_eq!(spec.wire_protocol, WireProtocol::AnthropicMessages);
    assert_eq!(
        spec.cache_model,
        CacheModel::Deterministic { ttl_ms: 5 * 60_000 }
    );
    assert_eq!(spec.pricing.input_per_mtok_usd, 3.0);
    assert_eq!(spec.pricing.cached_input_per_mtok_usd, 0.3);
    assert_eq!(spec.pricing.cache_write_per_mtok_usd, 3.75);
    assert_eq!(spec.pricing.output_per_mtok_usd, 15.0);
    assert_eq!(spec.quality_prior, 0.95);
    assert_eq!(spec.base_ttft_ms, 350.0);
    assert_eq!(spec.ttft_ms_per_uncached_token, 0.002);
}
