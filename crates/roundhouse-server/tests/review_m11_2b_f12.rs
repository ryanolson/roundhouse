// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M11.2b thermo-nuclear review, finding F12 -- refuter test.
//!
//! **The claim.** `messages_api_surface.rs`'s own module doc says of the
//! pinned client captures: "Only `metadata.user_id`'s `device_id` is edited,
//! to a placeholder of the same shape; everything else is verbatim." F12
//! says that is false for the current (2.1.257) line: the three
//! `claude-2.1.257-turn-*.json` fixtures carry `metadata.user_id` with
//! Python `json.dumps`-style `": "` / `", "` separators, while the client
//! itself emits `JSON.stringify`'s compact form -- which the 2.1.251
//! fixtures (`claude-2.1.251-turn-{1,2-continue}.json`) do preserve. If
//! true, the redaction step that stamped in the placeholder `device_id`
//! round-tripped the *whole* `user_id` string through a re-serializer for
//! the 2.1.257 captures, rather than substituting only the 64 hex
//! characters in place -- so "everything else is verbatim" does not hold
//! for the currently pinned line.
//!
//! **How this is checked.** `metadata.user_id` is itself a JSON document
//! encoded as a string (`wire.rs:301`'s `serde_json::from_str` is what every
//! test that reads it uses, and that call is blind to whitespace inside the
//! string it parses -- which is exactly why nothing before this test
//! noticed). This suite parses each fixture, pulls that string out, and
//! checks it for the two separator shapes `json.dumps`'s default
//! (`separators=(", ", ": ")`) adds and `JSON.stringify` never does.

use serde_json::Value;

fn user_id(raw: &str) -> String {
    let v: Value = serde_json::from_str(raw).expect("fixture is well-formed JSON");
    v["metadata"]["user_id"]
        .as_str()
        .expect("metadata.user_id is a string")
        .to_string()
}

const PRIOR_TURN_1: &str = include_str!("fixtures/claude-2.1.251-turn-1.json");
const PRIOR_TURN_2: &str = include_str!("fixtures/claude-2.1.251-turn-2-continue.json");
const CURRENT_TURN_1: &str = include_str!("fixtures/claude-2.1.257-turn-1.json");
const CURRENT_TURN_2: &str = include_str!("fixtures/claude-2.1.257-turn-2-continue.json");
const CURRENT_TURN_3: &str = include_str!("fixtures/claude-2.1.257-turn-3-continue.json");

/// **CONTROL.** The prior (2.1.251) line's `user_id` is compact --
/// `JSON.stringify`'s shape, no space after `:` or `,`.
///
/// Kept live so the assertion below cannot be dismissed as a check that
/// would fail for any fixture regardless of content: this proves the
/// compact form is what an unedited capture on this harness actually looks
/// like, which is what makes the 2.1.257 fixtures' spaced form a change
/// rather than a property every capture already had.
#[test]
fn f12_control_2_1_251_user_id_is_compact() {
    for (name, raw) in [("turn-1", PRIOR_TURN_1), ("turn-2-continue", PRIOR_TURN_2)] {
        let uid = user_id(raw);
        assert!(
            !uid.contains("\": \"") && !uid.contains("\", \""),
            "control failed: 2.1.251 {name}'s metadata.user_id is not \
             compact -- the baseline this test relies on does not hold: {uid}"
        );
    }
}

/// **F12, closed.** The current (2.1.257) line's `user_id` is compact too,
/// because the redaction now substitutes `device_id`'s 64 hex characters in
/// place in the string the client sent rather than parsing and re-dumping
/// it. Held here rather than only in the generator script: the script lives
/// in a scratch capture rig that does not ship, so this is the only place
/// the "everything else is verbatim" claim is enforced against the fixtures
/// the suite actually reads.
#[test]
fn f12_current_2_1_257_user_id_is_compact() {
    for (name, raw) in [
        ("turn-1", CURRENT_TURN_1),
        ("turn-2-continue", CURRENT_TURN_2),
        ("turn-3-continue", CURRENT_TURN_3),
    ] {
        let uid = user_id(raw);
        assert!(
            !uid.contains("\": \"") && !uid.contains("\", \""),
            "F12: 2.1.257 {name}'s metadata.user_id was re-serialized with \
             json.dumps-style spaced separators instead of having only \
             device_id substituted in place -- \"everything else is \
             verbatim\" does not hold for this fixture: {uid}"
        );
    }
}
