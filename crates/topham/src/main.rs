// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `topham`, the binary.
//!
//! Thin on purpose, and for the mirror image of the reason
//! `roundhouse-server/src/main.rs` is thin: everything worth testing is a
//! function in the library, so what is left here is argv, the environment, and
//! turning an error into an exit code.

use std::process::ExitCode;

use clap::Parser;

use topham::cli::{Cli, run};

fn main() -> ExitCode {
    let cli = Cli::parse();
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
