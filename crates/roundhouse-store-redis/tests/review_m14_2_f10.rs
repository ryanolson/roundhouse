// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.2 thermo-nuclear review, finding F10 — refuted.
//!
//! **The claim.** `keys::build_key` (`src/keys.rs`) takes no per-family
//! version parameter — only a namespace, a family tag, and parts — so the
//! only version any key carries is the crate-wide `SCHEMA_VERSION`
//! constant. `correlation.rs`'s own module doc promises that a v2 of that
//! family's *value* encoding "would be a different key space" with its own
//! constant, the way `CORR_SCHEMA_VERSION` reads. But
//! `key_builder_convention.rs`'s negative check —
//! `body.contains("SCHEMA_VERSION") || body.contains("\"v1\"")` — is a bare
//! substring match over the whole function body, so it flags any per-family
//! constant whose name merely *contains* `SCHEMA_VERSION` as a byte
//! sequence, `CORR_SCHEMA_VERSION` included, as if the body had spelled out
//! the shared constant itself.
//!
//! No live Redis is needed: like the guard test it exercises, this is a
//! shape-of-a-string check — the same one `key_builder_convention.rs`
//! itself runs with no backing store.
//!
//! **The proof.** [`the_negative_checks_substring_match_flags_a_documented_per_family_constant`]
//! reproduces the guard's negative check verbatim against a function body
//! shaped exactly like the escape route `keys.rs`'s own module doc names —
//! `keys::build_key_versioned(ns, "corr", CORR_SCHEMA_VERSION, ...)` — and
//! asserts the check does *not* flag it. It fails, because
//! `"...CORR_SCHEMA_VERSION...".contains("SCHEMA_VERSION")` is `true`: the
//! guard cannot tell "spells out the shared constant" from "names a
//! differently-scoped constant that happens to share a suffix".
//! [`build_key_takes_no_per_family_version_parameter`] is the passing
//! control proving the other half of the mechanism directly from
//! `keys.rs`'s own source: `build_key`'s parameter list is exactly
//! `(namespace, family, parts)` — no version parameter exists for a family
//! to pass one through even if the guard let it.
//!
//! **Ruling: valid.** Both premises held exactly as F10 stated them: the
//! shared builder had no version parameter, and the convention guard's
//! by-name substring check would have rejected the very escape route the
//! correlation module doc describes, on nothing more than a shared
//! substring of the constant's name.
//!
//! **Closed.** `build_key` now takes a [`KeyFamily`](../src/keys.rs) whose
//! own [`KeyFamily::version`] supplies the version, so there is no more
//! per-family *constant* for a name collision to happen to — the escape
//! route is a match arm, not an identifier a substring scan could ever see.
//! `key_builder_convention.rs`'s negative check dropped the `SCHEMA_VERSION`
//! half of its old substring match accordingly, keeping only the literal
//! `"v1"` check `negative_check_flags` below still mirrors.

use std::fs;
use std::path::Path;

/// Verbatim copy of `key_builder_convention.rs`'s (fixed) negative check,
/// isolated so it can be exercised against a body that never has to compile
/// — the guard itself never compiles the bodies it inspects either, it only
/// reads their source text.
fn negative_check_flags(body: &str) -> bool {
    body.contains("\"v1\"")
}

#[test]
fn the_negative_checks_substring_match_flags_a_documented_per_family_constant() {
    // Shaped exactly as `keys.rs`'s own module doc used to describe the
    // escape route: a family reaching for its own version constant instead
    // of the shared one, still through a `build_key`-family function. Kept
    // as a passing regression guard now that the escape route no longer
    // needs a constant at all — `KeyFamily::version`'s own match arm plays
    // the same role without a name a substring scan could collide with.
    let documented_escape_route_body = r#"{
        keys::build_key_versioned(namespace, "corr", CORR_SCHEMA_VERSION, parts)
    }"#;

    assert!(
        !negative_check_flags(documented_escape_route_body),
        "the guard's negative check must not flag a body that names its own \
         per-family version constant (CORR_SCHEMA_VERSION) merely because \
         that name contains \"SCHEMA_VERSION\" as a substring"
    );
}

/// Passing control: `build_key` really has no version parameter today, so
/// the only version any family can carry through it is the one crate-wide
/// `SCHEMA_VERSION` — confirmed by reading `keys.rs`'s own source rather
/// than by re-asserting the claim.
#[test]
fn build_key_takes_no_per_family_version_parameter() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = Path::new(manifest_dir).join("src/keys.rs");
    let src = fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));

    let needle = "pub(crate) fn build_key(";
    let start = src
        .find(needle)
        .unwrap_or_else(|| panic!("`{needle}` not found — has build_key been renamed?"));
    // Scan from just past the opening paren of build_key's own parameter
    // list — not from `start`, which still includes the closing paren of
    // `pub(crate)` itself and would end the signature before it began.
    let params_start = start + needle.len();
    let sig_end = src[params_start..]
        .find(')')
        .unwrap_or_else(|| panic!("build_key's signature has no closing paren"));
    let signature = &src[params_start..params_start + sig_end];

    assert!(
        signature.contains("namespace")
            && signature.contains("family")
            && signature.contains("parts"),
        "build_key's signature changed shape; update this control: {signature}"
    );
    assert!(
        !signature.to_lowercase().contains("version"),
        "build_key gained a version parameter — F10's premise (no per-family \
         version input) no longer holds: {signature}"
    );
}

/// Direct restatement of the finding's own `how_to_prove`, and — unlike the
/// finding's assertion, which named the fact by way of showing an old check
/// would trip on it — kept here with the true polarity as a standing
/// control: `CORR_SCHEMA_VERSION` containing `SCHEMA_VERSION` as a substring
/// is a fact about the two identifiers' spelling that no code change makes
/// false. What the fix above changed is that the guard's negative check no
/// longer looks for `SCHEMA_VERSION` at all — see
/// `the_negative_checks_substring_match_flags_a_documented_per_family_constant`
/// — so this substring relationship, still true, no longer has anything to
/// collide with.
#[test]
fn a_per_family_constant_name_can_contain_the_shared_constants_name() {
    assert!(
        "CORR_SCHEMA_VERSION".contains("SCHEMA_VERSION"),
        "this is the substring relationship the old by-name check confused \
         with the shared constant itself — the fixed check no longer checks \
         for SCHEMA_VERSION as a substring, so this fact staying true is no \
         longer a hazard"
    );
}
