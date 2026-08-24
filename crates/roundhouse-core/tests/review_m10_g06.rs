// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Validation test for review finding G06: "reference_named's rustdoc is
//! resolve's".
//!
//! The claim: a doc-merge artifact from the M10.2 edit put `resolve`'s
//! rustdoc opening paragraph above `ShadowPricing::reference_named`, so the
//! summary rustdoc renders for `reference_named` is not about
//! `reference_named` at all.
//!
//! This is checked by parsing the doc-comment block immediately preceding
//! each function in the committed source, rather than by shelling out to
//! `cargo doc` and scraping HTML — the summary line rustdoc renders is
//! defined as the first paragraph of the doc comment, and that is exactly
//! what source parsing recovers, without paying for a doc build per CI run.
//!
//! The full mechanism, corrected from the raw claim: the doc comment was not
//! duplicated, it was *split*. One combined `resolve` doc block — an opening
//! summary plus two parameter paragraphs (`observed`, then
//! `declared_baseline`) — now has its summary-plus-`observed` half sitting
//! above `reference_named` (with `reference_named`'s own real opening
//! sentence run on, un-paragraphed, into the tail of that misplaced block)
//! and its summary-plus-`declared_baseline` half above `resolve` itself.
//! `resolve` still takes an `observed` parameter, so a fix that only deletes
//! the misplaced paragraph — rather than moving it back — would silently
//! leave `resolve` undocumented for a parameter it actually has. The fourth
//! test below pins that: it fails today for the same root cause and only
//! passes once `observed` is restored to `resolve`'s doc, which is what
//! forces "move" over "delete" as the fix.

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

/// rustdoc's summary is the doc comment's first paragraph: everything up to
/// the first blank (`///` with nothing after it) line.
fn first_paragraph<'a>(block: &[&'a str]) -> Vec<&'a str> {
    block
        .iter()
        .take_while(|l| !l.is_empty())
        .copied()
        .collect()
}

#[test]
#[ignore = "G06: reference_named's doc block opens with resolve's \"Choose \
            the correlary...\" paragraph, which explains the `observed` \
            parameter that only resolve takes; reference_named's own \
            opening sentence (\"The reference model a client's `model` \
            field names...\") is demoted to a buried second sentence. Fix \
            by MOVING the `observed` paragraph back onto resolve's doc \
            block (see the sibling ignored test on resolve's own doc: \
            resolve still takes `observed` and must keep documenting it) \
            and hoisting reference_named's real opening sentence to the \
            top of its own doc block. Removing this ignore is step one of \
            that fix."]
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

#[test]
#[ignore = "G06: rustdoc's module-index summary for reference_named is its \
            doc comment's first paragraph, and that paragraph is currently \
            resolve's, byte-for-byte identical to resolve's own summary. \
            Fixed by the same edit as the sibling ignored test in this \
            file."]
fn reference_named_and_resolve_do_not_share_a_summary_line() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/metrics/pricing.rs");
    let src = fs::read_to_string(&path).expect("read pricing.rs");
    let lines: Vec<&str> = src.lines().collect();

    let named_block = doc_block_above(&lines, "pub fn reference_named(&self, named: &str)");
    let resolve_block = doc_block_above(&lines, "pub fn resolve(");

    let named_summary = first_paragraph(&named_block).join(" ");
    let resolve_summary = first_paragraph(&resolve_block).join(" ");

    // The rustdoc summary line is the reader's one-line description of the
    // function in the module index. Two functions that do different things
    // -- one looks a name up in a list, the other runs the full
    // declare/infer precedence -- rendering the identical summary is the
    // module-index-level symptom of the same merge artifact.
    assert_ne!(
        named_summary, resolve_summary,
        "reference_named and resolve render the identical rustdoc summary \
         line ({named_summary:?}); the module index would describe both \
         functions the same way"
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
#[ignore = "G06: resolve's doc block is missing its own `observed` \
            parameter explanation -- it migrated onto reference_named's doc \
            block instead of staying on resolve's. The fix must MOVE that \
            paragraph back, not delete it: deleting it would silence this \
            test's sibling assertions while leaving resolve undocumented \
            for a parameter it still takes."]
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
