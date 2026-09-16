// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.1 thermo-nuclear review, finding F9 -- confirmed, then fixed.
//!
//! **The claim.** The fair-use family's TTL test seam is one layer: a
//! `#[cfg(feature = "test-support")] pub fn with_bucket_ttl_ms` directly on
//! `RedisFairUseLedger` (`fair_use.rs:333-336`), called straight from the
//! gated test (`fair_use_decay.rs:457`). The correlation family's equivalent
//! seam is three layers for the same lever -- a `BindingTtls` struct
//! (`correlation.rs:251-264`), a `pub(crate) fn with_binding_ttls`
//! (`correlation.rs:299-302`) too private for an integration test (a
//! separate crate) to call directly, and a `test_support.rs` pass-through
//! (`:117-123`) that exists only to re-export it -- plus
//! `correlation_ambiguous_marker() -> &'static str` (`test_support.rs:142-144`)
//! handing out a `const` that `scripts.rs`'s `bind_call` (`:79-85`) takes as a
//! parameter from its only caller instead of reading `super::AMBIGUOUS_MARKER`
//! directly.
//!
//! This is a claim about the *source text and visibility* of files that
//! happen to live in this Redis-backed crate, not about anything a live
//! Redis does differently for the three-layer seam versus a one-layer seam --
//! both would arm the same `PX` on the same key either way. So there is
//! nothing here for a live Redis to distinguish, and this suite runs with no
//! `ROUNDHOUSE_TEST_REDIS_URL` and no `#[ignore]` gate for that reason,
//! exactly as `review_m14_1_f8.rs` reasons for its own source-text claim.
//!
//! **Ruling: valid -- and fixed.** `with_binding_ttls` is now `#[cfg(feature
//! = "test-support")] pub fn` directly on `RedisCorrelationMaps`, the exact
//! shape `with_bucket_ttl_ms` has on `RedisFairUseLedger`;
//! `correlation_contract.rs` calls it directly
//! (`.with_binding_ttls(80, 80)`), the way `fair_use_decay.rs` calls
//! `.with_bucket_ttl_ms(120)` directly; `test_support.rs`'s pass-through
//! (`correlation_with_binding_ttls`) is gone. `scripts.rs`'s `bind_call` now
//! reads `super::AMBIGUOUS_MARKER` itself instead of taking it as a
//! parameter from its one caller, and the const-returning
//! `correlation_ambiguous_marker` is gone from `test_support.rs` along with
//! it -- `AMBIGUOUS_MARKER` widened from `pub(crate)` to `pub` is what lets
//! the one gated integration test that reads the raw stored value name the
//! real constant directly, the same "hand out the real thing, not a copy"
//! reasoning `fair_use_would_exceed_source` already uses for the fair-use
//! script's own text.

/// The red assertion, now green: correlation's TTL lever used to be wrapped a
/// second time in `test_support.rs` (`correlation_with_binding_ttls`), on top
/// of being `pub(crate)` rather than `pub` on `RedisCorrelationMaps` --
/// neither of which was true of fair_use's `with_bucket_ttl_ms`. Now
/// `with_binding_ttls` is `#[cfg(feature = "test-support")] pub fn`, called
/// directly from `tests/correlation_contract.rs`, and `test_support.rs`
/// carries no mention of it -- the same shape fair_use's lever already had.
#[test]
fn the_correlation_ttl_lever_is_reached_as_directly_as_fair_uses() {
    let correlation = include_str!("../src/correlation.rs");
    let test_support = include_str!("../src/test_support.rs");
    let correlation_contract = include_str!("correlation_contract.rs");

    assert_eq!(
        correlation
            .matches("#[cfg(feature = \"test-support\")]\n    pub fn with_binding_ttls")
            .count(),
        1,
        "with_binding_ttls should be the one directly-callable pub seam \
         correlation exposes, the same shape fair_use's with_bucket_ttl_ms has"
    );
    assert!(
        !correlation.contains("pub(crate) fn with_binding_ttls"),
        "with_binding_ttls should no longer be pub(crate) -- an integration \
         test (a separate crate) needs to reach it directly"
    );

    assert!(
        correlation_contract.contains(".with_binding_ttls("),
        "correlation_contract.rs should call .with_binding_ttls(..) directly \
         on the connected maps, the same way fair_use_decay.rs calls \
         .with_bucket_ttl_ms(..) directly"
    );

    assert_eq!(
        test_support.matches("with_binding_ttls").count(),
        0,
        "test_support.rs should carry no mention of the correlation TTL \
         lever now that it is reached directly -- exactly as it carries no \
         mention of fair_use's lever"
    );
}

/// The red assertion, now green: `bind_call`'s only caller used to have
/// `AMBIGUOUS_MARKER` in scope and pass it in as a plain `&str` argument.
/// `scripts.rs` now names the constant directly and `bind_call` takes one
/// fewer parameter for it.
#[test]
fn bind_call_reads_the_ambiguous_marker_constant_directly() {
    let scripts = include_str!("../src/correlation/scripts.rs");
    assert!(
        scripts.contains("super::AMBIGUOUS_MARKER"),
        "scripts.rs should read super::AMBIGUOUS_MARKER itself rather than \
         taking it as a parameter from its one caller in correlation.rs"
    );
    assert!(
        !scripts.contains("ambiguous_marker: &str"),
        "bind_call should no longer take the marker as a parameter now that \
         it reads the constant directly"
    );
}

/// The red assertion, now green: `test_support.rs` used to hand out a
/// function that did nothing but return `AMBIGUOUS_MARKER`. It no longer
/// does -- the constant itself is `pub` now, so the one gated integration
/// test that needs it (`correlation_contract.rs`) names it directly, the
/// same "no copy to drift" reasoning the crate already uses for the
/// fair-use script's own text.
#[test]
fn test_support_no_longer_wraps_the_ambiguous_marker_constant() {
    let test_support = include_str!("../src/test_support.rs");
    let correlation = include_str!("../src/correlation.rs");
    let correlation_contract = include_str!("correlation_contract.rs");

    assert!(
        !test_support.contains("correlation_ambiguous_marker"),
        "test_support.rs should no longer hand out a function that just \
         returns AMBIGUOUS_MARKER"
    );
    assert!(
        correlation.contains("pub const AMBIGUOUS_MARKER"),
        "AMBIGUOUS_MARKER should be pub, so a gated integration test can \
         name the real constant instead of a copy or a wrapper"
    );
    assert!(
        correlation_contract.contains("correlation::AMBIGUOUS_MARKER")
            || correlation_contract.contains("AMBIGUOUS_MARKER"),
        "correlation_contract.rs should read the real constant directly"
    );
}

/// Control, and live (not ignored): the same counting method finds no
/// asymmetry on the fair_use side, proving the two assertions above are not
/// tautologically failing on any mention of a TTL-shaped identifier --
/// fair_use's lever really is one direct `pub fn`, called once, with nothing
/// in `test_support.rs` standing in front of it.
#[test]
fn the_fair_use_ttl_lever_is_already_reached_directly_with_no_wrapper() {
    let fair_use = include_str!("../src/fair_use.rs");
    let fair_use_decay = include_str!("fair_use_decay.rs");
    let test_support = include_str!("../src/test_support.rs");

    assert_eq!(
        fair_use
            .matches("#[cfg(feature = \"test-support\")]\n    pub fn with_bucket_ttl_ms")
            .count(),
        1,
        "with_bucket_ttl_ms should be the one directly-callable pub seam \
         fair_use exposes"
    );
    assert!(
        fair_use_decay.contains(".with_bucket_ttl_ms(120)"),
        "the gated fair_use test calls the lever directly, with no \
         test_support pass-through in between"
    );
    assert_eq!(
        test_support.matches("with_bucket_ttl_ms").count(),
        0,
        "test_support.rs carries no wrapper for fair_use's lever -- the \
         asymmetry F9 names is specific to correlation, not a pattern this \
         crate uses everywhere"
    );
}
