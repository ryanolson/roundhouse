// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The subcommands, and the dispatch under them.
//!
//! **In the library rather than in `main.rs`**, and the reason is the same one
//! that gave this crate a library target at all: [`run`] takes the environment
//! and the output stream as parameters, so a test drives the real dispatch with
//! an injected environment and reads what an operator would have seen. A
//! `fn main` that did this work could only be tested by spawning it.
//!
//! Every subcommand here is one function in another module — `plan` renders a
//! [`Resolution`](crate::plan::Resolution), `launch` calls
//! [`launch::run`], `mint` calls [`mint::mint`] — because
//! R-T6's rule (the TUI is a front end over the subcommands, never a second
//! implementation) is only enforceable if the subcommands are not
//! implementations either.

use std::io::Write;

use clap::{Parser, Subcommand};

use crate::env::EnvMap;
use crate::launch::{self, ExecLauncher, LaunchError};
use crate::mint::{self, ADMIN_KEY_ENV, MintError};
use crate::plan::{self, PlanError};
use crate::profile::{Profile, ProfileError};
use crate::relay::{self, RelayError};
use crate::tui::{self, TuiError};

/// `topham` — the operator entry point for a roundhouse deployment.
#[derive(Debug, Parser)]
#[command(name = "topham", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print what a profile resolves to, and launch nothing.
    Plan {
        /// The profile name, under `<config>/topham/profiles`.
        profile: String,
    },
    /// Replace this process with the profile's agent.
    Launch {
        profile: String,
        /// Arguments passed to the agent, after `--`.
        ///
        /// `last` rather than `trailing_var_arg`: it makes the `--` mandatory,
        /// so `topham launch work -p hello` is a parse error naming the
        /// separator rather than a silent attempt to interpret `-p` as one of
        /// this launcher's own flags.
        #[arg(last = true)]
        argv: Vec<String>,
    },
    /// Replace this process with a NeMo Relay running the profile's agent.
    Relay {
        profile: String,
        /// The Relay binary. The bare name is resolved through `PATH` by the
        /// exec, which is the same rule the agent's own program follows; a path
        /// here is how a pinned build is driven.
        #[arg(long, default_value = relay::RELAY_PROGRAM)]
        relay: String,
        /// Arguments passed to the agent, after `--`. See [`Command::Launch`].
        #[arg(last = true)]
        argv: Vec<String>,
    },
    /// Mint a turn key over the admin API and print its export line.
    Mint {
        /// The profile whose `key-env` the export line names.
        #[arg(long)]
        profile: String,
        /// The project the key is minted under.
        #[arg(long)]
        project: String,
        /// The member the key belongs to.
        #[arg(long)]
        user: String,
    },
}

/// Why a subcommand did not complete.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Launch(#[from] LaunchError),
    #[error(transparent)]
    Relay(#[from] RelayError),
    #[error(transparent)]
    Mint(#[from] MintError),
    #[error("could not write the output: {0}")]
    Output(#[from] std::io::Error),
    #[error(transparent)]
    Tui(#[from] TuiError),
}

/// Every distinct message in an error's `source` chain, outermost first.
///
/// **Deduplicated as the chain is walked**, which is the whole reason this is a
/// function rather than a loop at each printing site: half of this crate's
/// errors are `#[error(transparent)]`, which is one message and two links, so a
/// naive walk yields the same sentence three times and reads as a stutter rather
/// than as a cause.
///
/// Here rather than in `main.rs` because the TUI needs the same chain: a status
/// line that showed only the outermost message would reduce "this launch
/// forwards nothing and every request stays valid" to "could not launch", which
/// is exactly the refusal an operator most needs the generator's own words for.
pub fn error_chain(error: &dyn std::error::Error) -> Vec<String> {
    let mut messages = vec![error.to_string()];
    let mut source = error.source();
    while let Some(inner) = source {
        let message = inner.to_string();
        if messages.last() != Some(&message) {
            messages.push(message);
        }
        source = inner.source();
    }
    messages
}

/// Run one subcommand.
///
/// Returns on success for `plan` and `mint`. It does **not** return on a
/// successful `launch`: that path ends in `execve`, and this process becomes
/// the agent. The no-subcommand arm returns when the operator leaves the
/// screen, and does not return when they launch from it — for the same reason,
/// one `exec` further down.
pub fn run(cli: Cli, env: &EnvMap, out: &mut dyn Write) -> Result<(), CliError> {
    match cli.command {
        // The interactive screen, which is a front end over the arms below and
        // over nothing else (R-T6).
        None => Ok(tui::run(env)?),
        Some(Command::Plan { profile }) => {
            let loaded = Profile::load(env, &profile)?;
            let resolution = plan::resolve(env, &profile, loaded)?;
            write!(out, "{}", resolution.render())?;
            Ok(())
        }
        Some(Command::Launch { profile, argv }) => {
            let loaded = Profile::load(env, &profile)?;
            launch::run(env, &profile, loaded, argv, &ExecLauncher)?;
            Ok(())
        }
        Some(Command::Relay {
            profile,
            relay: relay_program,
            argv,
        }) => {
            let loaded = Profile::load(env, &profile)?;
            // **stderr, not `out`.** This process is about to become the agent,
            // and the agent's stdout is a contract -- `claude -p
            // --output-format json` prints one document and nothing else. See
            // `relay::run`'s doc: the banner on stdout is a corrupted document,
            // not merely extra output.
            relay::run(
                env,
                &profile,
                loaded,
                &relay_program,
                argv,
                &ExecLauncher,
                &mut std::io::stderr(),
            )?;
            Ok(())
        }
        Some(Command::Mint {
            profile,
            project,
            user,
        }) => {
            let loaded = Profile::load(env, &profile)?;
            let admin_key = env
                .get(ADMIN_KEY_ENV)
                .filter(|value| !value.trim().is_empty())
                .ok_or(MintError::AdminKeyMissing)?;
            let minted = mint::mint(
                &loaded.deployment_root,
                &project,
                &user,
                admin_key,
                &mint::HttpTransport,
            )?;
            // The id and tail first, on their own line: the export line is what
            // gets copied, and a comment on the same line would be copied with
            // it into a shell that would then treat `#` as part of the value in
            // a `.env` file.
            writeln!(out, "# minted {} (…{})", minted.id, minted.display_tail)?;
            writeln!(
                out,
                "{}",
                mint::export_line(&loaded.key_env, &minted.secret)
            )?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
