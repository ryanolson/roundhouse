// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A doc/parameter-correspondence lint over `ShadowPricing`'s two chooser
//! functions, kept from review finding G06.
//!
//! G06 was a doc-merge artifact from the M10.2 edit: one combined `resolve`
//! doc block — an opening summary plus two parameter paragraphs (`observed`,
//! then `declared_baseline`) — had been *split*, leaving the
//! summary-plus-`observed` half above `reference_named` (whose own opening
//! sentence ran on, un-paragraphed, into its tail) and the rest above
//! `resolve`. So rustdoc described `reference_named` by a parameter it does not
//! take, and `resolve` no longer documented one it does.
//!
//! What is kept here is not the prose — the corrected paragraphs live in
//! `pricing.rs` and are free to be reworded — but the correspondence between a
//! doc block and the signature under it: a function may not explain a
//! parameter it has no such parameter, and `resolve` must keep explaining both
//! of its own. That is checkable against the signatures and survives an edit
//! that changes the wording, which is what makes it worth keeping while the
//! third assertion of the original triple (that the two summary *lines* differ)
//! was dropped as a pure prose pin.
//!
//! Checked by parsing the doc-comment block immediately preceding each function
//! in the committed source rather than by shelling out to `cargo doc` and
//! scraping HTML: the summary rustdoc renders is defined as the first paragraph
//! of the doc comment, which is exactly what source parsing recovers, without
//! paying for a doc build per CI run.

use std::fs;
use std::path::Path;

/// The contiguous run of `///` lines immediately above the line containing
/// `needle`, as rustdoc would see them (comment markers and exactly one
/// leading space stripped).
fn doc_block_above<'a>(lines: &[&'a str], needle: &str) -> Vec<&'a str> {
    let fn_line = lines
        .iter()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line contains {needle:?}"));

    let mut start = fn_line;
    while start > 0 {
        let candidate = lines[start - 1].trim_start();
        if candidate.starts_with("///") {
            start -= 1;
        } else {
            break;
        }
    }

    lines[start..fn_line]
        .iter()
        .map(|l| l.trim_start().trim_start_matches("///").trim_start())
        .collect()
}

#[test]
fn reference_named_doc_does_not_describe_resolves_observed_parameter() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/metrics/pricing.rs");
    let src = fs::read_to_string(&path).expect("read pricing.rs");
    let lines: Vec<&str> = src.lines().collect();

    let block = doc_block_above(&lines, "pub fn reference_named(&self, named: &str)");

    // `reference_named`'s signature carries no `observed` parameter -- only
    // `resolve` takes one (`observed: &HashMap<(String, String), TokenShape>`).
    // A doc block above `reference_named` that explains what `observed`
    // means is therefore describing a different function's parameter, which
    // is exactly the doc-merge artifact G06 reports.
    let block_text = block.join("\n");
    assert!(
        !block_text.contains("`observed`"),
        "reference_named's doc block describes `observed`, a parameter that \
         belongs to `resolve` and does not exist on reference_named's \
         signature -- this is resolve's doc, misfiled:\n{block_text}"
    );
}

/// Control: `resolve`'s own doc block, checked with the same parser, does
/// describe a parameter that actually belongs to `resolve` -- proving the
/// parser recovers real content and the failures above are not an artifact
/// of `doc_block_above` itself.
#[test]
fn resolve_doc_describes_its_own_declared_baseline_parameter() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/metrics/pricing.rs");
    let src = fs::read_to_string(&path).expect("read pricing.rs");
    let lines: Vec<&str> = src.lines().collect();

    let block = doc_block_above(&lines, "pub fn resolve(");
    let block_text = block.join("\n");
    assert!(
        block_text.contains("`declared_baseline`"),
        "resolve's doc block should describe its own declared_baseline \
         parameter:\n{block_text}"
    );
}

/// Pins the direction of the fix. `resolve` takes an `observed` parameter
/// (`observed: &HashMap<(String, String), TokenShape>`) but its current doc
/// block, having been split rather than duplicated, no longer mentions it —
/// only `declared_baseline` survived on resolve's side of the split. A fix
/// that deletes the misplaced `observed` paragraph from reference_named
/// instead of moving it back to resolve would pass the two tests above while
/// leaving resolve permanently undocumented for a parameter it actually
/// takes; this test is what rules that fix out.
#[test]
fn resolve_doc_still_describes_its_observed_parameter() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/metrics/pricing.rs");
    let src = fs::read_to_string(&path).expect("read pricing.rs");
    let lines: Vec<&str> = src.lines().collect();

    let block = doc_block_above(&lines, "pub fn resolve(");
    let block_text = block.join("\n");
    assert!(
        block_text.contains("`observed`"),
        "resolve takes an `observed` parameter but its doc block no longer \
         mentions it -- the paragraph that once did is currently misfiled \
         above reference_named instead:\n{block_text}"
    );
}
