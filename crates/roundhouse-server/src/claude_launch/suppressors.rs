// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The table of inputs that change how a launched Claude Code client
//! authenticates, and the one question a launch asks of it.
//!
//! Its own module because it is the half of
//! [`claude_launch`](super) that is *evidence* rather than mechanism: every row
//! is a fact about `§1.3`'s `VV()` in
//! `agent-docs/research/claude-code-client-surface.md`, re-read whenever the
//! client line moves, while the generator around it is unchanged by any of that.
//! Keeping them apart means a client-surface re-capture edits one file whose
//! whole content is claims about the client, and the review that matters is
//! "does the table still match the capture" rather than a diff threaded through
//! a builder.

use super::{API_KEY_ENV, ClaudeAuthKind};

/// Where an OAuth-suppressing input lives, because they are not all environment
/// variables.
///
/// The distinction is load-bearing for a launcher: four of the five §1.3 names
/// can be cleared by not passing them to the child process, and `apiKeyHelper`
/// cannot — it is a settings key, read out of a file the client finds on its
/// own. A list that flattened the two would promise an enforcement that half of
/// it cannot deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressorSite {
    /// An environment variable the launcher controls.
    EnvVar,
    /// A key in the settings file the client resolves for itself. A launcher can
    /// refuse to run, or point an isolated `CLAUDE_CONFIG_DIR` at a file without
    /// it; it cannot unset this in the child's environment.
    SettingsKey,
}

/// What a suppressing input actually defeats — which is the same question as
/// "which launch has to refuse it".
///
/// A field on the table rather than a match arm in
/// [`ClaudeLaunch::env`](super::ClaudeLaunch::env), because the two auth kinds
/// do not stand in a subset relation and a rule written as code kept assuming
/// they did: [`Defeats::TheApiKeySentinel`] is harmless under exactly the kind
/// that refuses everything else, so a check spelled "the forwarded login refuses
/// all of them" silently had no place to put it (F3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Defeats {
    /// The subscription login, and only under
    /// [`ClaudeAuthKind::ForwardedClaudeLogin`] — the kind that has one to
    /// protect. The bring-your-own-key launch suppresses that login on purpose.
    TheSubscriptionLogin,
    /// The *redirect*, under both kinds: the three cloud-provider selectors make
    /// `I7()` return something other than `firstParty`, so
    /// [`BASE_URL_ENV`](super::BASE_URL_ENV) is never read at all.
    ///
    /// Worse than the login failure and refused harder, because a client under
    /// `CLAUDE_CODE_USE_VERTEX` never reaches roundhouse and nothing on either
    /// side reports it.
    TheRedirect,
    /// The [`ROUNDHOUSE_API_KEY_SENTINEL`](super::ROUNDHOUSE_API_KEY_SENTINEL),
    /// and only under [`ClaudeAuthKind::RoundhouseKey`] — the mirror image of
    /// [`Self::TheSubscriptionLogin`]. §1.3's `VV()` guards its [`API_KEY_ENV`]
    /// arm with `!$6(CLAUDE_CODE_REMOTE)` and nothing else, so this is the one
    /// input that turns the sentinel off while leaving every other suppressor
    /// working; a forwarded login is unharmed by it.
    TheApiKeySentinel,
}

/// One input that changes how a launched client authenticates, and what it
/// costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OauthSuppressor {
    /// The variable or settings key, spelled exactly as the client reads it.
    pub name: &'static str,
    /// Where it lives. See [`SuppressorSite`].
    pub site: SuppressorSite,
    /// What it defeats, and therefore which launch refuses it. See [`Defeats`].
    pub defeats: Defeats,
}

impl OauthSuppressor {
    /// Whether a launch of this `auth` kind must refuse this input.
    ///
    /// The one place the kind-to-input mapping is written, read by both
    /// [`ClaudeLaunch::must_be_unset`](super::ClaudeLaunch::must_be_unset) (what
    /// a launcher enforces) and [`ClaudeLaunch::env`](super::ClaudeLaunch::env)
    /// (what the generator refuses). Two copies of it were the drift F3 found:
    /// the enforcement list and the refusal agreed only because every entry
    /// happened to belong to the same two buckets.
    pub fn refused_under(&self, auth: ClaudeAuthKind) -> bool {
        match self.defeats {
            Defeats::TheRedirect => true,
            Defeats::TheSubscriptionLogin => auth == ClaudeAuthKind::ForwardedClaudeLogin,
            Defeats::TheApiKeySentinel => auth == ClaudeAuthKind::RoundhouseKey,
        }
    }
}

/// The inputs §1.3's `VV()` names, in the order it reads them.
///
/// Refused **by presence**, not by truthiness, and the honesty is deliberate:
/// the three cloud selectors are read through a truth function (`$6`) whose body
/// the evidence does not reproduce, so a rule that admitted
/// `CLAUDE_CODE_USE_BEDROCK=0` would be guessing at somebody else's parser. The
/// fail-closed reading costs an operator one deletion and buys never being wrong
/// about it.
///
/// `pub` so a launcher can enumerate the whole table rather than only the subset
/// [`ClaudeLaunch::must_be_unset`](super::ClaudeLaunch::must_be_unset) returns
/// for one kind — an operator diagnosing "why is my seat not forwarding" needs
/// the list itself, and a second copy of it in the launcher crate is the drift
/// this constant exists to prevent.
///
/// The table also carries the one input that runs the other way —
/// `CLAUDE_CODE_REMOTE`, which *un*-suppresses — because a launcher's question
/// is "what must not be set", and the answer differs by kind rather than by
/// direction. See [`Defeats::TheApiKeySentinel`].
pub const OAUTH_SUPPRESSORS: &[OauthSuppressor] = &[
    OauthSuppressor {
        name: "CLAUDE_CODE_USE_BEDROCK",
        site: SuppressorSite::EnvVar,
        defeats: Defeats::TheRedirect,
    },
    OauthSuppressor {
        name: "CLAUDE_CODE_USE_VERTEX",
        site: SuppressorSite::EnvVar,
        defeats: Defeats::TheRedirect,
    },
    OauthSuppressor {
        name: "CLAUDE_CODE_USE_FOUNDRY",
        site: SuppressorSite::EnvVar,
        defeats: Defeats::TheRedirect,
    },
    OauthSuppressor {
        name: "ANTHROPIC_AUTH_TOKEN",
        site: SuppressorSite::EnvVar,
        defeats: Defeats::TheSubscriptionLogin,
    },
    OauthSuppressor {
        name: "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
        site: SuppressorSite::EnvVar,
        defeats: Defeats::TheSubscriptionLogin,
    },
    OauthSuppressor {
        name: API_KEY_ENV,
        site: SuppressorSite::EnvVar,
        defeats: Defeats::TheSubscriptionLogin,
    },
    // Read immediately after `API_KEY_ENV` because it is the guard on that
    // arm and on no other: it is not a suppressor of the login but of *our
    // suppression of it*, which is why it is refused under the kind that
    // refuses nothing else in this list.
    OauthSuppressor {
        name: "CLAUDE_CODE_REMOTE",
        site: SuppressorSite::EnvVar,
        defeats: Defeats::TheApiKeySentinel,
    },
    OauthSuppressor {
        name: "apiKeyHelper",
        site: SuppressorSite::SettingsKey,
        defeats: Defeats::TheSubscriptionLogin,
    },
];

/// A suppressor list as an error message wants to read it.
///
/// Free-standing rather than a `Display` on the list, because the *only* caller
/// is the one error variant: giving [`OauthSuppressor`] a `Display` would make
/// this rendering the natural thing to compare against, which is exactly the
/// substring check F9 is about.
pub(super) fn quoted_names(suppressors: &[&'static OauthSuppressor]) -> String {
    suppressors
        .iter()
        .map(|suppressor| format!("`{}`", suppressor.name))
        .collect::<Vec<_>>()
        .join(", ")
}
