// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The operator entry point: what turns a deployment plus a decision into a
//! running agent.
//!
//! Roundhouse has had, since M9, two generators that say exactly what an
//! unmodified client needs in order to drive it —
//! [`codex_launch`](roundhouse_server::codex_launch) writes the files codex
//! reads and [`claude_launch`](roundhouse_server::claude_launch) builds the map
//! Claude Code reads — and no way for a person to run either. Both README
//! deferrals name that gap in the same words ("no CLI subcommand or admin route
//! produces these files"), and this crate closes it.
//!
//! # Why a separate binary rather than a subcommand of `roundhouse`
//!
//! The server binary's `main.rs` parses no flags. That is a rule with a
//! reason — every knob it has is a validated configuration file, so a
//! deployment is a document under review rather than an argv nobody keeps —
//! and a `roundhouse launch` subcommand would end it, because a launcher's
//! whole surface *is* flags. Two binaries keep both properties: the parser
//! lives here, `main.rs` keeps its rule, and the dependency runs upwards
//! (`topham` → `roundhouse-server`) which is the direction that lets a launcher
//! read the generators and the constants without the server ever knowing a
//! launcher exists.
//!
//! There is a second, blunter reason. `topham launch` **replaces itself** with
//! the agent (`execve`), so the process an operator started is the client they
//! wanted. A subcommand of the server binary could not do that without a server
//! binary that sometimes turns into `claude`.
//!
//! # Why a profile names things and never holds one
//!
//! A [`Profile`] carries an agent, a deployment root, an auth
//! kind, the **name** of the variable the turn key arrives in, a topology, and
//! for codex a slug and a catalog path. It carries no secret, and
//! [`Profile::from_toml`](profile::Profile::from_toml) refuses a file that
//! looks like it does, naming the field.
//!
//! That is inherited rather than chosen. `codex_launch`'s rule is "a secret
//! rides the environment and never a file", and `ClaudeEnv` enforces the same
//! thing by being un-`Serialize`-able. A profile is exactly the kind of file
//! that ends up in a dotfile repository, and a `rh_turn_…` copied into one is
//! indistinguishable from a live key by everything downstream of it. So the
//! profile names `ROUNDHOUSE_API_KEY` and the operator's environment holds the
//! value — which is also why [`mint`] prints an export line and writes nothing.
//!
//! # What a wrong profile costs
//!
//! Every refusal in this crate exists because the wrong answer *runs*. That is
//! the shape of the whole problem space, and it is why `plan` exists at all:
//!
//! - A base URL carrying `/v1` builds a client that posts to `/v1/v1/messages`
//!   and reports it as an upstream connection error. Refused by
//!   [`ClaudeLaunch::new`](roundhouse_server::ClaudeLaunch::new).
//! - A `ForwardedClaudeLogin` profile launched next to an ambient
//!   `CLAUDE_CODE_USE_VERTEX` forwards nothing, and every turn still answers.
//!   Refused by [`launch`], which checks
//!   [`ClaudeLaunch::must_be_unset`](roundhouse_server::ClaudeLaunch::must_be_unset)
//!   against the operator's own environment before anything is spawned.
//! - A `RoundhouseKey` profile launched with no key exported reaches roundhouse
//!   with no credential, which roundhouse *admits* and degrades to local-only.
//!   Nothing anywhere reports it; the run just never routes to a frontier
//!   model. Refused by [`plan::resolve`].
//!
//! None of those is visible from the client's side, and none of them is
//! visible from roundhouse's side either — which is the argument for a dry run
//! that prints the resolution rather than a launcher that is merely careful.
//!
//! # The screen, and why it is not a second launcher
//!
//! `topham` with no subcommand opens an interactive screen ([`tui`]): the
//! profile list, an editor for the fields above, a plan pane, and launch/relay
//! actions. It is a **front end over the subcommands** — the pane holds
//! [`Resolution::render`](plan::Resolution::render), the editor's save goes
//! through [`Profile::from_toml`](profile::Profile::from_toml)'s own refusals,
//! and the two actions call [`launch::run`] and [`relay::run`] once the
//! terminal has been restored.
//!
//! The rule that keeps it that way is that every action is a subcommand a
//! script can run, and the reason it matters is not tidiness: a screen with its
//! own resolution path would be a second answer to "what does this profile
//! mean", shown to the operator who is about to launch it and believed. The
//! state transitions are pure functions over key events, so the whole of it is
//! tested without a terminal.
//!
//! # The seams, and why they are seams
//!
//! Two things this crate does are not testable in-process as written: reading
//! the environment, and becoming another program. Both are values rather than
//! calls, so the tests are ordinary:
//!
//! - the environment is captured once into an [`env::EnvMap`], so every refusal
//!   is a pure function of a map a test can build. Nothing here calls
//!   `std::env::var` below [`env::system`], and nothing sets a variable at all
//!   — the process's own environment is read, never written, which is what
//!   keeps this crate's suite free of the single-threaded `unsafe` discipline
//!   the gated e2e suites need.
//! - the exec is behind [`launch::Launcher`], whose recording implementation
//!   lets a test assert on the exact argv and environment a real launch would
//!   have handed the child.

pub mod cli;
pub mod env;
pub mod launch;
pub mod mint;
pub mod plan;
pub mod profile;
pub mod relay;
pub mod tui;

/// Fixtures shared by the suites, compiled only under `cfg(test)`.
#[cfg(test)]
mod test_support;

pub use env::EnvMap;
pub use profile::{Agent, AuthKind, Profile, ProfileError, Topology};
