// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What one session event costs to move, and why the selection snapshot is not
//! part of it.
//!
//! **An enum is as large as its largest variant on every value of it.** A
//! `SessionEventKind` is moved by value into the store contract, back out of it
//! on replay, and through the metrics fold once per event, whichever kind it
//! actually is — so an `OutputTextDelta` carrying twelve bytes of text pays
//! whatever a routed decision's evidence costs. `clippy::large_enum_variant` is
//! the lint that measures that, and this file is its executable half: the lint
//! names a crate that failed to build, these assertions name the field that
//! moved and the rule that decided where it went.
//!
//! The wire is not this file's subject and is deliberately untouched by the
//! indirection these assertions require — see `routing_selection_serde.rs`,
//! which owns the JSON shape, the absent-field read of a historical log, and
//! the whole-snapshot round trip.

use std::mem::{size_of, size_of_val};

use roundhouse_core::classify::ClassificationIntent;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::routing::{DecisionRecord, SelectionSnapshot};

/// Clippy's `enum-variant-size-threshold`, which this workspace does not
/// override in a `clippy.toml`: `large_enum_variant` fires when the largest
/// variant exceeds the second largest by more than this many bytes. The crate
/// builds under `-D warnings`, so crossing it is a build failure and not a note.
const CLIPPY_ENUM_VARIANT_SIZE_THRESHOLD: usize = 200;

/// **The claim.** A decision record names its selection snapshot; it does not
/// carry it.
///
/// The threshold is the representation and not a number picked to pass: a
/// `DecisionRecord` smaller than the `SelectionSnapshot` it refers to is a
/// statement only indirection can make true. Trimming some unrelated field
/// cannot satisfy it, and putting the snapshot back inline cannot survive it —
/// which is the property a byte-count assertion normally lacks.
#[test]
fn a_decision_record_names_its_selection_snapshot_rather_than_carrying_it() {
    assert!(
        size_of::<DecisionRecord>() < size_of::<SelectionSnapshot>(),
        "a decision record ({} bytes) must stay smaller than the selection \
         snapshot it refers to ({} bytes); inline, the snapshot is the majority \
         of the record and every event of every kind pays for it",
        size_of::<DecisionRecord>(),
        size_of::<SelectionSnapshot>(),
    );
}

/// **The claim.** The selection field of a decision that recorded no evidence
/// is one pointer wide.
///
/// Measured through a real [`DecisionRecord`], and through the field itself
/// rather than by naming `Option<Box<SelectionSnapshot>>` here. Spelling the
/// type in the test would assert something about a type this file wrote down —
/// it would hold just as well on a build where the production field had gone
/// back to inline, which is the one thing worth catching. The value arrives the
/// way a replay produces it: a log entry from before the field existed,
/// deserialized, so `selection` takes its serde default rather than whatever a
/// constructor here would have chosen.
///
/// **A width, not an allocation count**, and the name claims only what is
/// measured. Width is *why* the absent case is free — a null pointer is nothing
/// to store and nothing to clone — but no call to the allocator is counted
/// anywhere in this file, and that half of the argument lives on the field's own
/// doc comment as reasoning rather than here as evidence. What the assertion
/// does rule out is every shape that would break it: a field the snapshot's
/// width means it went back inline, and a field a word wider than a pointer
/// means a discriminant is sitting beside the pointer instead of in its null
/// niche.
#[test]
fn the_selection_field_of_a_decision_without_evidence_is_one_pointer_wide() {
    // Seven fields, which is every field a decision record had before any of
    // the widenings; each one since arrives by serde default, `selection`
    // included. So this is exactly the record the claim is about, and it is
    // built by the same path a replay builds it by.
    let historical: DecisionRecord = serde_json::from_str(
        r#"{
            "chosen": {"kind":"frontier","provider":"anthropic","model":"claude"},
            "rationale": "test",
            "policy": "affinity",
            "isl_tokens": 4096,
            "expected_prefill_tokens": 4096.0,
            "expected_cost_usd": 0.02,
            "considered": []
        }"#,
    )
    .expect("a record from before the selection field existed still reads");
    assert!(
        historical.selection.is_none(),
        "the width below is a statement about an absent selection only if this \
         one is absent"
    );

    assert_eq!(
        size_of_val(&historical.selection),
        size_of::<*const SelectionSnapshot>(),
        "the selection field of a decision with no evidence to record is {} \
         bytes against a pointer's {}",
        size_of_val(&historical.selection),
        size_of::<*const SelectionSnapshot>(),
    );
}

/// **The claim.** The event kind stays inside the lint's headroom over its
/// second-largest variant.
///
/// `ClassificationRequested` is that variant, and it is a single
/// [`ClassificationIntent`] — so the intent's size is the comparison's floor.
/// The enum's own size stands in for the largest variant's payload, which the
/// lint compares and `size_of` cannot name directly: an enum is at least as
/// large as any variant it holds, so passing here implies the lint's comparison
/// passes. The substitution is strict in the safe direction — it can fail while
/// the lint would still be quiet, never the reverse.
#[test]
fn the_event_kind_stays_inside_the_lint_headroom_over_its_second_largest_variant() {
    let ceiling = size_of::<ClassificationIntent>() + CLIPPY_ENUM_VARIANT_SIZE_THRESHOLD;
    assert!(
        size_of::<SessionEventKind>() <= ceiling,
        "SessionEventKind is {} bytes against a ceiling of {}: the second \
         largest variant carries a {}-byte ClassificationIntent, and clippy \
         allows the largest {CLIPPY_ENUM_VARIANT_SIZE_THRESHOLD} bytes over it",
        size_of::<SessionEventKind>(),
        ceiling,
        size_of::<ClassificationIntent>(),
    );
}
