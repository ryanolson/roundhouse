// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `topham`, the binary.
//!
//! Thin on purpose, and for the mirror image of the reason
//! `roundhouse-server/src/main.rs` is thin: everything worth testing is a
//! function in the library, so what is left here is argv, the environment, and
//! turning an error into an exit code.

use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches};

use topham::cli::{Cli, run};

/// What `topham --version` answers with.
///
/// The package version *plus the commit the binary was built from*, which is
/// the whole of M11.3 review F23: an operator drives a `target/debug/topham` by
/// path, and the workspace version is the same string in every build of every
/// commit that does not touch `Cargo.toml` — so a launcher nobody rebuilt was
/// indistinguishable from a current one, including to the real-binary suites
/// whose preflight prints this line. The hash comes from `build.rs`, which
/// answers `unknown` rather than failing where there is no checkout to read.
///
/// Applied here rather than as `#[command(version = ...)]` on [`Cli`] because
/// the identifier is a fact about *this binary*: the library target is linked
/// into tests and into anything that ever embeds the parser, and a version
/// string those carry would be naming a build they are not.
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("TOPHAM_BUILD_COMMIT"),
    ")"
);

fn main() -> ExitCode {
    // `Cli::parse()` with the version overridden: `get_matches` still handles
    // `--help`, `--version` and a parse error by exiting the way the derive
    // does, so the only difference is which string `--version` prints.
    let matches = Cli::command().version(VERSION).get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    // Read once, here, and passed down as a value -- see `topham::env`.
    let environment = topham::env::system();
    let mut out = std::io::stdout();
    match run(cli, &environment, &mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The whole chain, not just the outermost message. Half of this
            // crate's errors wrap a generator's refusal, and the generator's is
            // the one that says what the client would have done -- printing
            // only the wrapper would reduce "this launch forwards nothing and
            // every request stays valid" to "could not launch". The walk (and
            // its repeat-dropping) is `cli::error_chain`, shared with the TUI's
            // status line.
            let mut chain = topham::cli::error_chain(&error).into_iter();
            if let Some(first) = chain.next() {
                eprintln!("topham: {first}");
            }
            for cause in chain {
                eprintln!("  caused by: {cause}");
            }
            ExitCode::FAILURE
        }
    }
}
