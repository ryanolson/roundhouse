// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.1 thermo-nuclear review, finding F8 -- refuted, then fixed.
//!
//! **The claim.** `generation`, `session_of_call` and `session_of_thread`
//! (`correlation.rs:308-312`, `:361-365`, `:394-398`) each spell the same
//! five-line
//! `redis::cmd("GET").arg(..).query_async(&mut self.conn.clone()).await.map_err(backend)?`
//! round trip, and a private `get` helper would collapse the three into one.
//!
//! This is a claim about the *source text* of a file that happens to live in
//! this Redis-backed crate, not about anything Redis does differently for the
//! duplicated form versus the collapsed one -- all three call sites already
//! read the same way and would keep reading the same way through a shared
//! helper. So there is nothing here for a live Redis to distinguish, and this
//! suite runs with no `ROUNDHOUSE_TEST_REDIS_URL` and no `#[ignore]` gate: it
//! is a `cargo test -p roundhouse-store-redis --test review_m14_1_f8`
//! ordinary run. The proof is the finding's own `how_to_prove` grep, made
//! into a test that fails today for the reason F8 gives and goes green the
//! moment a shared `get` is the only place `redis::cmd("GET")` appears.

/// The red assertion, now green: three call sites (`generation`,
/// `session_of_call`, `session_of_thread`) used to each build their own
/// `redis::cmd("GET")` round trip. Now those three route through one private
/// `get`, and only that helper's body still says `redis::cmd("GET")`.
#[test]
fn the_get_round_trip_is_spelled_once_not_three_times() {
    let source = include_str!("../src/correlation.rs");
    let get_call_sites = source.matches("redis::cmd(\"GET\")").count();
    assert_eq!(
        get_call_sites, 1,
        "found {get_call_sites} call sites building their own \
         redis::cmd(\"GET\") .. query_async(&mut self.conn.clone()) .. \
         map_err(backend) round trip; a shared `get` helper collapses \
         generation's, session_of_call's and session_of_thread's identical \
         reads into exactly one"
    );
}

/// Control, and live (not ignored): the same file already shares
/// `decode_binding` between `session_of_call` and `session_of_thread` --
/// proving the "count call sites of a shared idiom" method above is not
/// tautologically failing on any repeated literal. It finds the duplication
/// that exists (three raw `GET` round trips) and does not find duplication
/// where the code has already been collapsed (`decode_binding`, called from
/// both read paths, appears once as a definition and is *shared*, not
/// re-spelled, at its two call sites).
#[test]
fn decode_binding_is_already_the_shared_helper_get_is_not() {
    let source = include_str!("../src/correlation.rs");
    let decode_binding_definitions = source.matches("fn decode_binding(").count();
    let decode_binding_call_sites = source.matches("decode_binding(raw, &key)").count();
    assert_eq!(
        decode_binding_definitions, 1,
        "decode_binding is defined once and shared, unlike the GET round trip"
    );
    assert_eq!(
        decode_binding_call_sites, 2,
        "session_of_call and session_of_thread both call the one shared \
         decode_binding rather than re-deriving its logic -- the same move \
         F8 asks for on the GET side"
    );
}
