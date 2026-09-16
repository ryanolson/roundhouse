// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Review finding F13: `CodexLaunch::new` accepted three input shapes its own
//! doc calls out as dangerous -- a relative `model_catalog_path`, a non-UTF-8
//! path, and a `base_url` with no API prefix -- and constructed a `Self` for
//! every one of them. Each produced a config that *loads*: the client starts,
//! half of it works, and the half that does not fails where the operator would
//! look at roundhouse rather than at the file they were handed.
//!
//! Ruled valid; `new` is now fallible. These tests were written against the
//! infallible signature and are rewritten here to the `Result`, which makes
//! them strictly stronger: the original could only observe "did not panic",
//! and now each case names the variant it must be refused with. That matters
//! because the three failures are diagnostically different -- a wrong catalog
//! path degrades the client to invented model metadata, a wrong base_url 404s
//! every turn while the MCP handshake still succeeds -- and one generic
//! rejection would send whoever reads it to the wrong file.
//!
//! A trailing slash is the fourth shape and is deliberately *not* refused: it
//! is what a copy-pasted address carries, it has exactly one reading, and
//! normalising it teaches nobody less than an error would. The control below
//! pins that too, so "refuse everything unusual" is not mistaken for the rule.

use roundhouse_server::API_PREFIX;
use roundhouse_server::codex_launch::{CodexLaunch, CodexLaunchError};
use std::path::PathBuf;

fn catalog() -> PathBuf {
    PathBuf::from("/srv/roundhouse/models.json")
}

fn base_url() -> String {
    format!("http://127.0.0.1:8080{API_PREFIX}")
}

/// Control: the documented-correct shape constructs, and the value it
/// constructs is the one that was handed in.
///
/// Kept live so the refusals below cannot be dismissed as a constructor that
/// refuses everything. It asserts on the `Result` itself rather than on "did
/// not panic" -- the weaker form the pre-fix version of this file used, which
/// would now pass even if the correct shape were refused.
#[test]
fn a_launch_accepts_the_documented_correct_shape() {
    let launch = CodexLaunch::new(base_url(), &catalog())
        .expect("an absolute UTF-8 catalog path and a base_url ending in the API prefix");
    assert_eq!(launch.base_url, base_url());
    assert_eq!(launch.model_catalog_path, catalog().display().to_string());
}

/// Control: a trailing slash is normalised, not refused, and the normalisation
/// reaches the emitted `base_url` rather than only the derived MCP url.
///
/// The pre-fix code stored the slash verbatim and only `mcp_endpoint` trimmed
/// it, so the provider stanza could carry `.../v1/` while the MCP url was
/// clean -- the two halves disagreeing about the same string is the shape this
/// whole finding is about.
#[test]
fn a_trailing_slash_is_normalised_rather_than_refused() {
    let launch = CodexLaunch::new(format!("{}/", base_url()), &catalog())
        .expect("a trailing slash has one unambiguous reading");
    assert_eq!(launch.base_url, base_url());
    assert!(
        launch
            .config_toml()
            .contains(&format!("base_url = \"{}\"", base_url())),
        "the normalised address is what the client is handed:\n{}",
        launch.config_toml()
    );
}

/// F13.1: a relative `model_catalog_path` is refused by name.
///
/// The field's own doc called codex's resolution rule -- against the directory
/// `config.toml` was loaded from, not the process cwd -- "correct and
/// impossible to check from here". `Path::is_absolute()` is exactly that
/// check. Left unchecked, the client finds no catalog and falls back to
/// invented model metadata instead of erroring.
#[test]
fn a_relative_catalog_path_is_refused() {
    let error = CodexLaunch::new(base_url(), &PathBuf::from("models.json"))
        .expect_err("a relative catalog path must be refused");
    assert!(
        matches!(error, CodexLaunchError::RelativeCatalogPath { .. }),
        "the refusal must name the catalog path, not something else: {error}"
    );
}

/// F13.2: a non-UTF-8 `model_catalog_path` is refused rather than lossily
/// renamed.
///
/// `Path::display().to_string()` substitutes U+FFFD for the bytes it cannot
/// decode, so the config would name a path that is *not* the file on disk,
/// with nothing anywhere recording that a substitution happened. Unix-only
/// because that is where a path can hold bytes that are not UTF-8 at all.
#[cfg(unix)]
#[test]
fn a_non_utf8_catalog_path_is_refused() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let bad_bytes = PathBuf::from(OsStr::from_bytes(b"/srv/roundhouse/models\xFF.json"));
    let error = CodexLaunch::new(base_url(), &bad_bytes)
        .expect_err("a non-UTF-8 catalog path must be refused");
    assert!(
        matches!(error, CodexLaunchError::NonUtf8CatalogPath { .. }),
        "a non-UTF-8 path is its own diagnosis, not a relative-path one: {error}"
    );
    // The absolute-ness check must not preempt it: this path *is* absolute,
    // and reporting "relative" would send the reader to the wrong problem.
    assert!(bad_bytes.is_absolute());
}

/// F13.3: a `base_url` that does not end in the prefix the Responses API is
/// actually served at is refused.
///
/// This is the one that looks healthy: `mcp_endpoint` tolerates a missing
/// prefix by design, so the MCP handshake succeeds and the client starts
/// normally -- while every turn POSTs to `{base_url}/responses`, which the
/// router does not serve. Derived from `API_PREFIX` rather than the literal
/// `/v1` so that the check and the route can never disagree about what
/// "missing the prefix" means.
#[test]
fn a_base_url_without_the_served_api_prefix_is_refused() {
    let error = CodexLaunch::new("http://127.0.0.1:8080", &catalog())
        .expect_err("a base_url missing the API prefix must be refused");
    assert!(
        matches!(error, CodexLaunchError::BaseUrlMissingApiPrefix { .. }),
        "the refusal must name the base URL: {error}"
    );
    assert!(
        error.to_string().contains(API_PREFIX),
        "the message must say which prefix is missing, since that is the whole edit: {error}"
    );
}
