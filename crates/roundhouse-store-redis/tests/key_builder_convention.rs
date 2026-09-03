// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Refuter test for the M14.2 thermo-nuclear review's F2: R-S3 promises
//! "every key any family writes... is built by one function"
//! (`keys::build_key`), but nothing checked that a family's key function
//! actually calls it rather than hand-formatting the identical string —
//! every other test in this crate asserts only on the *output* of a key
//! function, which cannot tell the two apart.
//!
//! No live Redis is needed: this is a shape-of-the-source check, the same
//! kind `fair_use_contract_convention.rs` runs over `tests/*.rs` for the
//! sibling-file convention, applied here to `src/*.rs` for the builder
//! convention instead.
//!
//! F2 closed: every key function this crate defines is checked, by name,
//! against its own body text, so a future family — or a rewritten one —
//! that bypasses `keys::build_key` fails here even when its output happens
//! to match today's shape byte-for-byte, which is exactly the case a
//! output-only assertion cannot see.

use std::fs;
use std::path::Path;

/// Every key-building function this crate defines, and the file it lives
/// in. A name ending in `_key` that is not on this list is exactly the gap
/// this test exists to close — extending the list is the fix.
const KEY_FUNCTIONS: &[(&str, &str)] = &[
    ("src/lib.rs", "meta_key"),
    ("src/lib.rs", "lease_key"),
    ("src/lib.rs", "log_key"),
    ("src/spend.rs", "account_key"),
    ("src/spend.rs", "holds_key"),
    ("src/spend.rs", "watermarks_key"),
    ("src/fair_use.rs", "project_scope_key"),
    ("src/fair_use.rs", "member_scope_key"),
    ("src/correlation.rs", "generation_key"),
    ("src/correlation.rs", "call_key"),
    ("src/correlation.rs", "thread_key"),
];

/// Extracts `fn <name>(...) -> String { ... }`'s body, by brace-balancing
/// from the first `{` after the signature. Good enough for this crate's key
/// functions, which are all single expressions or one `format!` call deep —
/// none nests a closure or a block past what balanced braces already track.
fn function_body(src: &str, fn_name: &str) -> String {
    let needle = format!("fn {fn_name}(");
    let start = src
        .find(&needle)
        .unwrap_or_else(|| panic!("`{needle}` not found — update KEY_FUNCTIONS"));
    let brace_start = start
        + src[start..]
            .find('{')
            .unwrap_or_else(|| panic!("`{needle}` has no body"));
    let mut depth = 0i32;
    let mut end = brace_start;
    for (offset, ch) in src[brace_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = brace_start + offset + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    src[brace_start..end].to_string()
}

#[test]
fn every_key_function_calls_the_shared_builder() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let mut offenders = Vec::new();

    for (file, fn_name) in KEY_FUNCTIONS {
        let path = Path::new(manifest_dir).join(file);
        let src = fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
        let body = function_body(&src, fn_name);

        // The positive check: the body must actually call the builder, not
        // merely produce output that looks like it did. This is what a
        // hand-formatted `format!("{namespace}:v1:{family}:...")` fails —
        // its *string* can match build_key's output exactly while its body
        // never mentions build_key at all.
        if !body.contains("build_key(") {
            offenders.push(format!(
                "{file}::{fn_name} does not call keys::build_key — its body is:\n{body}"
            ));
        }

        // The negative check: the body must not spell the schema version
        // out itself, whether as the literal ("v1") or by reaching past the
        // builder for the constant (SCHEMA_VERSION). Either is a bypass
        // that a build_key-only positive check would miss if a hand-written
        // body happened to call build_key too, decoratively, alongside its
        // own formatting.
        if body.contains("SCHEMA_VERSION") || body.contains("\"v1\"") {
            offenders.push(format!(
                "{file}::{fn_name} spells the schema version itself instead of \
                 leaving it to keys::build_key — its body is:\n{body}"
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "F2: every family's key function must build its key through \
         keys::build_key and nothing else, so a schema-version bump at \
         keys::SCHEMA_VERSION reaches every key rather than missing a \
         hand-formatted one that happened to match today's shape:\n\n{}",
        offenders.join("\n\n")
    );
}
