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
//!
//! **M14.2 review, F7 — the hand-maintained `KEY_FUNCTIONS` list this file
//! used to check against is gone.** Its own doc said an unlisted `*_key`
//! function "is exactly the gap this test exists to close," yet the check
//! only ever walked that fixed list — a family module could add a bypassing
//! key function and, so long as nobody also remembered to extend the list,
//! this test would never see it. [`scan_key_function_names`] finds every
//! `fn \w+_key(` a family file defines instead, so there is no list left to
//! fall out of.
//!
//! **M14.2 review, F10 — the negative check no longer flags a per-family
//! version constant by name.** The old check was
//! `body.contains("SCHEMA_VERSION") || body.contains("\"v1\"")`, a bare
//! substring match that would have rejected the very escape route
//! `correlation`'s own module doc describes (a per-family constant like
//! `CORR_SCHEMA_VERSION`, which contains `SCHEMA_VERSION` as a substring of
//! its own name) as if the body had spelled out the shared constant itself.
//! There is no more shared `SCHEMA_VERSION` constant to bypass — each
//! [`KeyFamily`](../src/keys.rs)'s version lives on the variant, reached
//! only by calling `build_key` — so the negative check now looks for
//! exactly the one remaining bypass: a hand-typed version literal like
//! `"v1"`.

use std::fs;
use std::path::Path;

/// Every file a family's key functions live in — not `src/keys.rs` itself,
/// which defines `build_key` and would trivially fail its own "calls
/// `build_key`" check (a function does not call itself to build a key).
const FAMILY_FILES: &[&str] = &[
    "src/lib.rs",
    "src/spend.rs",
    "src/fair_use.rs",
    "src/correlation.rs",
    "src/directory.rs",
    "src/scripts/learning.rs",
    "src/learn.rs",
];

/// Every `fn <name>(` a family file defines outside its own `#[cfg(test)]`
/// module, where `<name>` ends in `_key` — the scan F7 says the
/// hand-maintained `KEY_FUNCTIONS` list this file used to carry never ran.
///
/// Scoped to each file's production section (everything before its own
/// `#[cfg(test)]`): this crate's test names end in `_key` too —
/// `fair_use.rs`'s `no_two_scopes_can_name_one_key`,
/// `correlation.rs`'s `a_delimiter_in_an_id_cannot_make_two_members_share_a_key`
/// — and neither is a key-building function the convention applies to.
fn scan_key_function_names(src: &str) -> Vec<String> {
    let production_src = src.split("\n#[cfg(test)]").next().unwrap_or(src);
    let mut names = Vec::new();
    let mut rest = production_src;
    while let Some(pos) = rest.find("fn ") {
        let after_fn = &rest[pos + 3..];
        let Some(name_end) = after_fn.find('(') else {
            break;
        };
        let name = after_fn[..name_end].trim();
        if !name.is_empty()
            && name.ends_with("_key")
            && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        {
            names.push(name.to_string());
        }
        rest = &after_fn[name_end..];
    }
    names
}

/// Extracts `fn <name>(...) -> String { ... }`'s body, by brace-balancing
/// from the first `{` after the signature. Good enough for this crate's key
/// functions, which are all single expressions or one `format!` call deep —
/// none nests a closure or a block past what balanced braces already track.
fn function_body(src: &str, fn_name: &str) -> String {
    let needle = format!("fn {fn_name}(");
    let start = src
        .find(&needle)
        .unwrap_or_else(|| panic!("`{needle}` not found"));
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

    for file in FAMILY_FILES {
        let path = Path::new(manifest_dir).join(file);
        let src = fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));

        for fn_name in scan_key_function_names(&src) {
            let body = function_body(&src, &fn_name);

            // The positive check: the body must actually call the builder,
            // not merely produce output that looks like it did. This is
            // what a hand-formatted `format!("{namespace}:v1:{family}:...")`
            // fails — its *string* can match build_key's output exactly
            // while its body never mentions build_key at all.
            if !body.contains("build_key(") {
                offenders.push(format!(
                    "{file}::{fn_name} does not call keys::build_key — its body is:\n{body}"
                ));
            }

            // The negative check: the body must not hand-type the version
            // literal itself instead of leaving it to the family's own
            // KeyFamily::version. Unlike the check this replaced, this does
            // not flag a per-family constant by name — there is no shared
            // constant left for a per-family one to be confused with (F10).
            if body.contains("\"v1\"") {
                offenders.push(format!(
                    "{file}::{fn_name} spells the schema version itself instead of \
                     leaving it to keys::build_key/KeyFamily::version — its body is:\n{body}"
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "F2: every family's key function must build its key through \
         keys::build_key and nothing else, so a schema-version bump at \
         KeyFamily::version reaches every key rather than missing a \
         hand-formatted one that happened to match today's shape:\n\n{}",
        offenders.join("\n\n")
    );
}

/// A file can sit in `FAMILY_FILES` and be scanned for nothing. That is the
/// present shape of `src/scripts/learning.rs`: its three `*_key` functions
/// are inlined into `IndexKeys::new`, so the row names the file as covered
/// but `scan_key_function_names` finds zero `fn ..._key(` in it, and
/// `every_key_function_calls_the_shared_builder` would pass vacuously over
/// the one family whose keys are per namespace rather than per session.
/// This asserts the scan itself is never empty for a listed file, so an
/// inert row fails loudly instead of looking like coverage.
#[test]
fn every_family_file_yields_at_least_one_key_function() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    for file in FAMILY_FILES {
        let path = Path::new(manifest_dir).join(file);
        let src = fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
        let names = scan_key_function_names(&src);
        assert!(
            !names.is_empty(),
            "{file} is listed in FAMILY_FILES but scan_key_function_names found no \
             `fn ..._key(` in it — either it builds no keys of its own (drop it from \
             FAMILY_FILES) or its key functions are inlined somewhere the scan cannot \
             see (restore them as named `fn ..._key` functions, per the key-builder \
             convention)"
        );
    }
}

/// M14.2 review, F7 correction: the module-doc table in `src/lib.rs`
/// ("Family | Version | Module") is pinned by name — extracted from
/// `KeyFamily::name`'s own match arms in `src/keys.rs` rather than
/// hand-copied a second time — so an added variant with no doc-table row
/// fails here, not only a key function with no scan coverage.
#[test]
fn every_key_family_has_a_row_in_the_module_doc_table() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    let keys_path = Path::new(manifest_dir).join("src/keys.rs");
    let keys_src =
        fs::read_to_string(&keys_path).unwrap_or_else(|e| panic!("reading {keys_path:?}: {e}"));
    let name_body = function_body(&keys_src, "name");
    let mut family_names = Vec::new();
    for line in name_body.lines() {
        if let Some(after_arrow) = line.split_once("=> \"") {
            let rest = after_arrow.1;
            if let Some(end) = rest.find('"') {
                family_names.push(rest[..end].to_string());
            }
        }
    }
    assert!(
        !family_names.is_empty(),
        "KeyFamily::name's match arms could not be parsed — has its shape changed?"
    );

    let doc_path = Path::new(manifest_dir).join("src/lib.rs");
    let doc_src =
        fs::read_to_string(&doc_path).unwrap_or_else(|e| panic!("reading {doc_path:?}: {e}"));
    for name in &family_names {
        assert!(
            doc_src.contains(&format!("`{name}`")),
            "KeyFamily's `{name}` variant has no row in lib.rs's \
             `Family | Version | Module` table"
        );
    }
}

/// Whether the key argument of one `redis.call(` is exactly `KEYS[...]`:
/// the command, a comma, then `KEYS[` with its bracket closed and nothing
/// after it but the next argument or the end of the call. `KEYS[1] .. ':x'`
/// starts like a `KEYS` entry and is a key the builder never named.
fn key_is_a_keys_entry(args: &str) -> bool {
    let Some((_, key)) = args.split_once(',') else {
        return false;
    };
    let Some(index) = key.trim_start().strip_prefix("KEYS[") else {
        return false;
    };
    let mut depth = 1;
    for (offset, ch) in index.char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    let after = index[offset + 1..].trim_start();
                    return after.starts_with(',') || after.starts_with(')');
                }
            }
            _ => {}
        }
    }
    false
}

/// The commands the learn scripts may call: the type check, the reads, and
/// the two writes the module doc allows. Each takes exactly one key.
const LEARN_SCRIPT_COMMANDS: [&str; 6] = ["TYPE", "HGET", "HMGET", "HSET", "SISMEMBER", "SADD"];

/// M7 of the online routing learner: every key of the `learn` family is built
/// by the shared builder, and its scripts name no key of their own.
///
/// Two halves. Each of the four key shapes of draft section 11.4 is a named
/// `fn *_key` in `src/learn.rs`, so the scan above holds it to `build_key`,
/// and each one names `KeyFamily::Learn`, so none borrows another family's
/// version. And every `redis.call` in `src/learn/scripts.rs` reaches its key
/// through `KEYS[...]`: a script that concatenated a key from `ARGV` would
/// write a key the builder never named, whatever the Rust side built. Its
/// command is one of [`LEARN_SCRIPT_COMMANDS`], so a `RENAME` or `COPY`
/// whose first key is `KEYS[i]` cannot write a second key taken from
/// anywhere else. The live half, that a real apply writes exactly the keys these functions
/// name, is `the_store_writes_only_the_keys_its_key_functions_name` in
/// `learn_contract.rs`.
#[test]
fn every_learn_key_is_built_by_the_shared_builder() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    assert!(
        FAMILY_FILES.contains(&"src/learn.rs"),
        "src/learn.rs must be scanned by every_key_function_calls_the_shared_builder"
    );

    let src = fs::read_to_string(Path::new(manifest_dir).join("src/learn.rs"))
        .expect("the learn family lives in src/learn.rs");
    let names = scan_key_function_names(&src);
    for shape in ["watermark_key", "quality_key", "ops_key", "seen_key"] {
        assert!(
            names.iter().any(|name| name == shape),
            "src/learn.rs has no `fn {shape}(`; found {names:?}"
        );
    }
    for name in &names {
        let body = function_body(&src, name);
        assert!(
            body.contains("build_key(") && body.contains("KeyFamily::Learn"),
            "src/learn.rs::{name} does not build its key with build_key under \
             KeyFamily::Learn:\n{body}"
        );
    }

    let scripts = fs::read_to_string(Path::new(manifest_dir).join("src/learn/scripts.rs"))
        .expect("the learn scripts live in src/learn/scripts.rs");
    let mut calls = 0;
    for (index, line) in scripts.lines().enumerate() {
        let mut rest = line;
        while let Some(at) = rest.find("redis.call(") {
            calls += 1;
            let args = &rest[at + "redis.call(".len()..];
            let command = args
                .split_once(',')
                .map(|(command, _)| command.trim().trim_matches('\''));
            assert!(
                command.is_some_and(|command| LEARN_SCRIPT_COMMANDS.contains(&command)),
                "src/learn/scripts.rs:{}: a redis.call whose command is not one of \
                 {LEARN_SCRIPT_COMMANDS:?}: {line}",
                index + 1
            );
            assert!(
                key_is_a_keys_entry(args),
                "src/learn/scripts.rs:{}: a redis.call whose key is not exactly KEYS[...]: {line}",
                index + 1
            );
            rest = args;
        }
    }
    assert!(calls > 0, "no redis.call found in src/learn/scripts.rs");
}
