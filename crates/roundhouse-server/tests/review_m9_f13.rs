// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Refuting evidence for review finding F13: `CodexLaunch::new` accepts three
//! input shapes its own doc calls out as dangerous (a relative
//! `model_catalog_path`, a non-UTF-8 path, a `base_url` with no `/v1`) and
//! constructs a `Self` unconditionally for every one of them -- there is no
//! validating entry point anywhere in `codex_launch.rs` to refuse them.

use roundhouse_server::codex_launch::CodexLaunch;
use std::panic::catch_unwind;
use std::path::PathBuf;

/// Control: the documented-correct shape must construct without panicking.
/// Kept live (not ignored) so the failing test below cannot be dismissed as
/// tautological -- this proves the harness can observe a *successful*
/// construction too.
#[test]
fn a_launch_accepts_the_documented_correct_shape() {
    let result = catch_unwind(|| {
        CodexLaunch::new(
            "http://127.0.0.1:8080/v1",
            &PathBuf::from("/srv/roundhouse/models.json"),
        )
    });
    assert!(
        result.is_ok(),
        "the documented-correct shape (absolute catalog path, UTF-8, base_url ending in /v1) \
         must construct cleanly"
    );
}

/// F13: none of the three inputs the module's own doc comments call out as
/// dangerous are refused. `CodexLaunch::new` returns `Self` unconditionally --
/// there is no `Result`, no panic, no validation path at all -- so every one
/// of these `catch_unwind`s comes back `Ok`, and every assertion below fails.
#[test]
#[ignore = "F13: CodexLaunch::new has no validation path at all (infallible `-> Self`); a relative \
            model_catalog_path, a non-UTF-8 path (silently lossy via Path::display(), confirmed to \
            substitute U+FFFD and mismatch the file on disk), and a base_url missing /v1 (written \
            verbatim into the provider stanza while mcp_url() is computed correctly, confirmed to \
            desync the two) are all accepted and silently produce a broken client. Fix: a fallible \
            constructor (or validate() step) in codex_launch.rs that checks Path::is_absolute(), \
            require a valid Unicode path (Path::to_str().is_some()), and require base_url ends with \
            /v1 before accepting it."]
fn a_launch_refuses_the_three_inputs_whose_config_would_be_silently_wrong() {
    // 1. Relative model_catalog_path. The field doc says codex resolves this
    // against the directory config.toml was loaded from and calls that
    // "correct and impossible to check from here" -- but `Path::is_absolute()`
    // is exactly that check, and `CodexLaunch::new` never calls it.
    let relative = catch_unwind(|| {
        CodexLaunch::new("http://127.0.0.1:8080/v1", &PathBuf::from("models.json"))
    });
    assert!(
        relative.is_err(),
        "a relative model_catalog_path must be refused: codex resolves it against config.toml's \
         directory, not the process cwd, and Path::is_absolute() catches this before the client \
         ever sees a stanza pointing at a file that isn't there"
    );

    // 2. Non-UTF-8 path. `model_catalog_path.display().to_string()` is lossy:
    // it silently substitutes replacement characters, naming a file that will
    // not exist on disk, with nothing logged about the substitution.
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let bad_bytes = PathBuf::from(OsStr::from_bytes(b"/srv/roundhouse/models\xFF.json"));
        let non_utf8 = catch_unwind(|| CodexLaunch::new("http://127.0.0.1:8080/v1", &bad_bytes));
        assert!(
            non_utf8.is_err(),
            "a non-UTF-8 model_catalog_path must be refused: Path::display() lossily renames it \
             to a path that will not exist on disk instead of surfacing the encoding problem"
        );
    }

    // 3. base_url missing the /v1 suffix. mcp_endpoint() tolerates this by
    // design (it strips /v1 if present, falls through otherwise), but the same
    // base_url is written verbatim into [model_providers.roundhouse].base_url,
    // and responses_api.rs mounts the turn surface only at the literal
    // "/v1/responses". A client built from this stanza posts to
    // "{base_url}/responses", which 404s on every turn while the MCP handshake
    // still succeeds -- silent in the dangerous direction, per the module's
    // own framing of what silent-wrong looks like here.
    let no_v1 = catch_unwind(|| {
        CodexLaunch::new(
            "http://127.0.0.1:8080",
            &PathBuf::from("/srv/roundhouse/models.json"),
        )
    });
    assert!(
        no_v1.is_err(),
        "a base_url with no /v1 suffix must be refused: it is accepted gracefully for the MCP \
         half but the identical string is sent verbatim into the provider stanza, breaking every \
         turn against a router that only serves /v1/responses"
    );
}
