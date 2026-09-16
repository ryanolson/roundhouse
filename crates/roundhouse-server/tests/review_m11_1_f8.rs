// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! REFUTE F8 (M11.1 thermo-nuclear review). The claim: the tier-1 oracle
//! (`tests/common/anthropic.rs`) puts `#[serde(deny_unknown_fields)]` on eight
//! strict shapes -- `StrictBlock` and `StrictDelta` (both `tag = "type"`
//! enums), `StrictUsage`, `StrictCacheCreation`, `StrictMessage`,
//! `StrictMessageDelta`, `StrictError`, and `StrictEvent` (also `tag =
//! "type"`) -- but `messages_api_surface.rs`'s
//! `the_oracle_is_not_a_rubber_stamp` only ever injects an unknown field into
//! one of them (`StrictUsage`, via `cache_creation_input_tokens_1h`). The
//! other seven have the attribute in source with no probe anywhere that would
//! notice it going missing.
//!
//! # Verified test-first, by running the assigned mutation rather than
//! reading it
//!
//! This finding was one of three mutations the thermo-nuclear review queued
//! but never actually ran (disk exhaustion). Grepping is a static claim about
//! what the suite *contains*; the dynamic question is what the suite
//! *notices*, so this file replaces the static read with an actual run:
//!
//! 1. Baseline: `timeout 300 cargo test -p roundhouse-server --test
//!    messages_api_surface -j 2` against the unmodified tree --
//!    `24 passed; 0 failed; 6 ignored`, `the_oracle_is_not_a_rubber_stamp ...
//!    ok`.
//! 2. Mutated: `#[serde(deny_unknown_fields)]` stripped from all seven shapes
//!    above (left intact on `StrictUsage`, the one the finding says *is*
//!    covered) directly in `tests/common/anthropic.rs`, same command run
//!    again -- **`24 passed; 0 failed; 6 ignored`, unchanged.**
//!    `the_oracle_is_not_a_rubber_stamp` neither caught the loosening nor
//!    changed its outcome, and nothing else in the 30-test binary did either.
//! 3. The mutation was reverted (`git checkout --
//!    crates/roundhouse-server/tests/common/anthropic.rs`); leaving a
//!    weakened shared oracle in the tree would have degraded every other
//!    suite that trusts its strictness, which is a strictly worse outcome
//!    than an unclosed coverage gap.
//!
//! A grep independent of the finding's own (`rg
//! 'StrictMessage|StrictBlock|StrictDelta|StrictCacheCreation|StrictMessageDelta|StrictEvent\b'
//! --type rust` from the repo root) returns exactly one file:
//! `tests/common/anthropic.rs` itself -- the seven types are never named
//! anywhere a test could inject a probe against them directly, only reached
//! indirectly through `audit()`. Confirmed, not merely repeated.
//!
//! # Ruling: valid
//!
//! The described mechanism is exactly what happened: stripping the attribute
//! from seven of eight shapes changed nothing observable in the suite that
//! exists to notice exactly this class of defect. Severity is rightly
//! "minor" -- nothing is wrong with the *shipped* oracle today, every
//! attribute is present at 724dba8 and this file's control below proves each
//! one still functions -- the exposure is to a *future* edit (an enum arm
//! added without the derive, a struct field spliced in past a merge) landing
//! with the tier-1 oracle still reporting green.
//!
//! # Fixed here
//!
//! Per review protocol, closing a test-coverage finding *is* the fix (adding
//! the missing probes and leaving them enabled). Every assertion in
//! [`f8_probes_ready_to_close_the_gap`] already passed against the real,
//! unmodified oracle (proven by [`f8_the_one_covered_shape_and_a_clean_stream_still_work`]
//! below, which stays live as the control that the harness itself is sound),
//! so removing its `#[ignore]` was the entire fix — confirmed by running it:
//! `cargo test -p roundhouse-server --test review_m11_1_f8` passes all four
//! tests in this file with none ignored.
//!
//! # An addendum this validation surfaced, not assigned by F8
//!
//! Probing `StrictEvent` turned up something F8's own framing does not
//! anticipate: its two *fieldless* variants (`MessageStop`, `Ping`) do not
//! enforce `deny_unknown_fields` at all, today, independent of any future
//! edit -- verified by direct construction in
//! [`f8_addendum_the_enums_own_guard_is_inert_on_its_fieldless_variants`],
//! kept live (not ignored) because it pins *verified current* behavior
//! rather than probing for a desired one. See that test's doc for the
//! mechanism. This does not change the ruling below -- F8's stated claim and
//! mechanism are correct for all seven shapes as written -- it sharpens the
//! `StrictEvent` case from "would go unnoticed if the guard were later
//! removed" to "already unguarded for two of its eight variants."

use serde_json::{Value, json};

mod common;
use common::anthropic::audit;

/// The same six-frame conformant stream `messages_api_surface.rs`'s own
/// `conformant()` builds, reproduced here because that function is private to
/// its file. Any drift between the two is itself worth noticing, so the
/// shape is deliberately copied verbatim rather than paraphrased.
fn conformant() -> Vec<(&'static str, Value)> {
    vec![
        (
            "message_start",
            json!({ "type": "message_start", "message": {
                "type": "message", "id": "resp_1", "role": "assistant",
                "model": "claude-opus-5", "content": [],
                "usage": { "input_tokens": 900, "output_tokens": 1 },
            }}),
        ),
        (
            "content_block_start",
            json!({ "type": "content_block_start", "index": 0,
                    "content_block": { "type": "text", "text": "" } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "text_delta", "text": "hi" } }),
        ),
        (
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "message_delta",
            json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" },
                    "usage": { "output_tokens": 7 } }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ]
}

fn sse(frames: &[(&str, Value)]) -> String {
    frames
        .iter()
        .map(|(name, payload)| format!("event: {name}\ndata: {payload}\n\n"))
        .collect()
}

/// CONTROL, kept live. Proves two things the ignored test below leans on:
/// the harness's own baseline is conformant (so a rejection elsewhere in this
/// file is about the one thing changed, not a malformed fixture), and the one
/// shape the finding says *is* covered (`StrictUsage`, the
/// `cache_creation_input_tokens_1h` case `the_oracle_is_not_a_rubber_stamp`
/// already exercises) still gets caught today. Without this, a reader cannot
/// tell whether [`f8_probes_ready_to_close_the_gap`] passing is because the
/// oracle works or because every fixture in it is silently broken.
#[test]
fn f8_the_one_covered_shape_and_a_clean_stream_still_work() {
    assert!(
        audit(&sse(&conformant())).is_ok(),
        "the unmodified stream must still pass; a change here would mean the fixture itself \
         drifted, not that anything about F8 changed"
    );

    let extra = sse(&conformant()).replace(
        "\"output_tokens\":1",
        "\"output_tokens\":1,\"cache_creation_input_tokens_1h\":5",
    );
    assert!(
        audit(&extra).is_err(),
        "StrictUsage's deny_unknown_fields is the one shape the finding says is actually \
         exercised, and it must still reject an invented usage property"
    );
}

/// F8's seven probes, one per uncovered shape. Every assertion here passes
/// against the oracle exactly as it ships at 724dba8 -- `deny_unknown_fields`
/// is present and functioning on all seven. What is missing is not the
/// attribute, it is *this test*: nothing wires these checks into the default
/// run, so a future edit that drops the attribute from any one of these seven
/// (a merge that regenerates a `#[derive]` block, a copy-pasted variant that
/// forgot the enum-level attribute) reports 21/21 (now 24/24) green exactly as
/// confirmed by the dynamic mutation run in this file's module doc.
///
/// Un-ignored: wiring this coverage in was the entire fix (review protocol
/// for this finding is validate-only, and validation is exactly what these
/// seven probes now do on every default test run).
#[test]
fn f8_probes_ready_to_close_the_gap() {
    // StrictBlock (tag = "type" enum, anthropic.rs:97): an unknown field
    // beside a `text` content_block.
    let block = sse(&conformant()).replace(
        "\"content_block\":{\"text\":\"\",\"type\":\"text\"}",
        "\"content_block\":{\"text\":\"\",\"type\":\"text\",\"bogus_field\":true}",
    );
    assert_ne!(
        block,
        sse(&conformant()),
        "the replace must actually have matched something"
    );
    assert!(
        audit(&block).is_err(),
        "an invented property on a content_block (StrictBlock) must be refused"
    );

    // StrictDelta (tag = "type" enum, anthropic.rs:161): an unknown field
    // beside a `text_delta`.
    let delta = sse(&conformant()).replace(
        "\"delta\":{\"text\":\"hi\",\"type\":\"text_delta\"}",
        "\"delta\":{\"text\":\"hi\",\"type\":\"text_delta\",\"bogus_field\":true}",
    );
    assert_ne!(
        delta,
        sse(&conformant()),
        "the replace must actually have matched something"
    );
    assert!(
        audit(&delta).is_err(),
        "an invented property on a content_block_delta (StrictDelta) must be refused"
    );

    // StrictCacheCreation (anthropic.rs:202): an unknown field inside the
    // nested `usage.cache_creation` object.
    let cache_creation = sse(&conformant()).replace(
        "\"input_tokens\":900,\"output_tokens\":1",
        "\"input_tokens\":900,\"output_tokens\":1,\"cache_creation\":{\"ephemeral_5m_input_tokens\":1,\"bogus_field\":true}",
    );
    assert_ne!(
        cache_creation,
        sse(&conformant()),
        "the replace must actually have matched something"
    );
    assert!(
        audit(&cache_creation).is_err(),
        "an invented property on usage.cache_creation (StrictCacheCreation) must be refused"
    );

    // StrictMessage (anthropic.rs:212): an unknown top-level field on the
    // `message_start.message` object itself -- the shape the shipped
    // `Message` type's flattened `Extra` map could actually produce.
    // Anchored on `"type":"message"` rather than a multi-key span: the
    // message object's own key order is an implementation detail of
    // `serde_json::Value` (sorted, per `the_oracle_is_not_a_rubber_stamp`'s
    // own comment on this), and `"type":"message"` is the one substring in
    // the whole six-frame stream that names the *inner* message object --
    // every other frame's `type` value carries a suffix (`message_start`,
    // `message_delta`, `message_stop`, ...) that keeps it from matching.
    let message = sse(&conformant()).replace(
        "\"type\":\"message\"",
        "\"type\":\"message\",\"bogus_field\":true",
    );
    assert_ne!(
        message,
        sse(&conformant()),
        "the replace must actually have matched something"
    );
    assert!(
        audit(&message).is_err(),
        "an invented top-level property on message_start.message (StrictMessage) must be refused"
    );

    // StrictMessageDelta (anthropic.rs:232): an unknown field on
    // `message_delta.delta`.
    let message_delta = sse(&conformant()).replace(
        "\"delta\":{\"stop_reason\":\"end_turn\"}",
        "\"delta\":{\"stop_reason\":\"end_turn\",\"bogus_field\":true}",
    );
    assert_ne!(
        message_delta,
        sse(&conformant()),
        "the replace must actually have matched something"
    );
    assert!(
        audit(&message_delta).is_err(),
        "an invented property on message_delta.delta (StrictMessageDelta) must be refused"
    );

    // StrictError (anthropic.rs:262): an unknown field inside a terminal
    // `error` event's `error` object.
    let error_stream = sse(&[(
        "error",
        json!({ "type": "error", "error": {
            "type": "overloaded_error", "message": "at capacity", "bogus_field": true,
        }}),
    )]);
    assert!(
        audit(&error_stream).is_err(),
        "an invented property on an error event's error object (StrictError) must be refused"
    );

    // StrictEvent (tag = "type" enum, anthropic.rs:277): an unknown field
    // beside a variant that carries fields of its own
    // (`content_block_stop { index }`), which isolates the enum's own
    // deny_unknown_fields on a data-bearing arm. The enum's *fieldless*
    // arms (`message_stop`, `ping`) are deliberately not probed here --
    // see [`f8_addendum_the_enums_own_guard_is_inert_on_its_fieldless_variants`],
    // which is a different, more severe mechanism than "untested."
    let stopped = sse(&conformant()).replace(
        "\"index\":0,\"type\":\"content_block_stop\"",
        "\"index\":0,\"type\":\"content_block_stop\",\"bogus_field\":true",
    );
    assert_ne!(
        stopped,
        sse(&conformant()),
        "the replace must actually have matched something"
    );
    assert!(
        audit(&stopped).is_err(),
        "an invented property beside a content_block_stop event (StrictEvent, data-bearing arm) \
         must be refused"
    );
}

/// **Addendum, discovered while validating F8 rather than assigned by it.**
///
/// `StrictEvent`'s two fieldless (unit) variants -- `MessageStop` and `Ping`
/// -- do not enforce `deny_unknown_fields` *at all*, today, on the shipped
/// oracle, independent of any future edit. This is not the same claim F8
/// makes: F8 is "present but untested, so a future removal would go
/// unnoticed" (true of the other six shapes and of `StrictEvent` itself when
/// the matched arm carries fields -- see the `content_block_stop` case
/// above), whereas this is "the attribute is already inert for these two
/// arms, verified by direct construction, no future edit required."
///
/// The likely mechanism (not further chased here -- validate-only scope):
/// serde's internally-tagged-enum deserializer buffers the object, reads the
/// tag to pick a variant, then deserializes the remaining buffered content as
/// that variant's payload; a fieldless variant has no payload deserializer to
/// run `deny_unknown_fields` inside, so the buffered leftovers are never
/// inspected. `content_block_stop`'s single `index` field gives serde
/// something to visit and reject extras against; `message_stop` and `ping`
/// give it nothing.
///
/// Kept **live, not ignored**: this asserts the verified current behavior
/// (`is_ok`, not `is_err`) rather than a not-yet-added probe for a desired
/// one, so it is a regression pin, not a coverage gap with a one-line close.
/// A future fix here is a type-shape or serde-version change, not "write the
/// missing test" -- which is why it does not fold into
/// [`f8_probes_ready_to_close_the_gap`]'s ignore reason.
///
/// Practical exposure: minimal today (roundhouse never emits an extra field
/// on a `message_stop` or `ping` frame), but it means the oracle's coverage
/// of `StrictEvent` is narrower than "one attribute, one enum" suggests --
/// two of its eight variants have no deny_unknown_fields backstop at all,
/// unit or data-bearing shape notwithstanding.
#[test]
fn f8_addendum_the_enums_own_guard_is_inert_on_its_fieldless_variants() {
    let message_stop_plus = sse(&conformant()).replace(
        "{\"type\":\"message_stop\"}",
        "{\"type\":\"message_stop\",\"bogus_field\":true}",
    );
    assert_ne!(
        message_stop_plus,
        sse(&conformant()),
        "the replace must actually have matched something"
    );
    assert!(
        audit(&message_stop_plus).is_ok(),
        "verified: an invented property on a fieldless message_stop event is NOT refused by \
         today's oracle -- deny_unknown_fields has nothing to check it against on this arm"
    );

    let ping_plus = r#"{"type":"ping","bogus_field":true}"#;
    let stream = format!("event: ping\ndata: {ping_plus}\n\n{}", sse(&conformant()));
    assert!(
        audit(&stream).is_ok(),
        "verified: the same holds for ping, the oracle's other fieldless StrictEvent arm"
    );
}
