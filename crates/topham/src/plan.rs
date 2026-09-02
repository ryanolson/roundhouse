// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Resolving a profile against an environment — and printing what that resolved
//! to without spawning anything.
//!
//! [`resolve`] is the single place a [`Profile`] becomes a launch. `topham
//! plan` renders it, `topham launch` executes it, and the TUI's plan pane shows
//! the same rendering: three surfaces over one resolution, so a dry run cannot
//! describe a launch different from the one that then happens. That is the
//! whole of R-T6's "the TUI is a front end over the subcommands", pulled one
//! layer down to where it can be enforced by there being nothing else to call.
//!
//! # Why `plan` refuses the same things `launch` does
//!
//! It would be friendlier to render a profile whose turn key is not exported —
//! an operator reading a profile before minting anything wants to see what it
//! means. It is refused anyway, because the alternative is a second resolution
//! path: one that runs when the key is missing and one that runs when it is
//! there, and only the second is the one `launch` takes. A dry run that
//! resolves differently from the real thing is worse than no dry run, because
//! it is believed.
//!
//! # What the rendering is allowed to print
//!
//! Every secret in the output goes through the generator's own `Debug`.
//! `ClaudeEnv`'s renders the turn key as `redacted:<fingerprint>` and any
//! variable the launcher declared as `"<set>"`; `CodexLaunch`'s holds no secret
//! at all, because that generator names a variable and never reads one. This
//! module therefore does not implement redaction — it *borrows* it, which is
//! what keeps a future field added to either generator from being printed in
//! the clear by a launcher that had its own copy of the rules.

use std::path::PathBuf;

use roundhouse_server::claude_launch::{OauthSuppressor, SuppressorSite};
use roundhouse_server::codex_launch::{CodexLaunchError, GeneratedFile};
use roundhouse_server::{API_PREFIX, ClaudeEnv, ClaudeLaunch, ClaudeLaunchError, CodexLaunch};

use crate::env::EnvMap;
use crate::profile::{Agent, AuthKind, Profile, ProfileError, Topology};

/// The generated codex config, as codex reads it out of its `CODEX_HOME`.
const CODEX_CONFIG_FILE: &str = "config.toml";

/// The generated model catalog, beside it.
///
/// A file in the same directory rather than one under a `models/` subtree: the
/// config names it by absolute path, so the only thing the layout has to be is
/// stable across launches — the same profile writing the same two paths every
/// time is what lets an operator diff a generated config against the last one.
const CODEX_CATALOG_FILE: &str = "model-catalog.json";

/// What a redacted value renders as, matching the generator's own spelling.
///
/// Restated rather than imported because `claude_launch`'s copy is private to
/// the enum whose `Debug` prints it. Deliberately the *same* string: a plan
/// pane that spelled a redaction two ways would read as two different states of
/// the same variable, which is the one thing an operator scanning this output
/// is looking for.
const REDACTED_VALUE: &str = "<set>";

/// A profile, resolved against an environment into the launch it names.
#[derive(Debug)]
pub struct Resolution {
    /// The profile's name, so every message can say which one.
    pub name: String,
    pub profile: Profile,
    pub resolved: Resolved,
}

/// The agent-shaped half of a [`Resolution`].
///
/// An enum rather than a struct with optional fields because the two agents do
/// not have a common shape: Claude Code's launch *is* an environment map and
/// codex's is two files plus a `CODEX_HOME`. A struct that held both would have
/// four fields of which two are always empty, and every reader would have to
/// know which two.
#[derive(Debug)]
pub enum Resolved {
    Claude {
        launch: ClaudeLaunch,
        env: ClaudeEnv,
        /// What must not be set when this launch runs, from the generator's own
        /// table.
        must_be_unset: Vec<&'static OauthSuppressor>,
    },
    Codex {
        launch: CodexLaunch,
        /// The `CODEX_HOME` this profile's client is pointed at.
        codex_home: PathBuf,
        /// The two files, relative to that home, exactly as the client will
        /// find them.
        files: Vec<GeneratedFile>,
    },
}

/// Why a profile could not be resolved into a launch.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(
        "the turn key is not exported: `{key_env}` is unset, and this profile names it as where \
         the key is read from. Launching anyway would reach roundhouse with no credential -- \
         which roundhouse *admits*, degrading the turn to local-only routing rather than refusing \
         it, so every turn would answer and no frontier route would ever happen. Mint one \
         (`topham mint --profile ...`) and export it"
    )]
    TurnKeyMissing { key_env: String },
    #[error(transparent)]
    Claude(#[from] ClaudeLaunchError),
    #[error(transparent)]
    Codex(#[from] CodexLaunchError),
    #[error(transparent)]
    Profile(#[from] ProfileError),
}

/// Resolve `profile` against `env`: the one path from a saved profile to a
/// launch.
///
/// The suppressor check is done **by handing the ambient variables to the
/// generator**, not by re-implementing its table here. Every name in
/// [`ClaudeLaunch::must_be_unset`] that is set in `env` is declared through
/// `also_launching_with`, and `ClaudeLaunch::env` is what refuses — so the
/// refusal an operator sees is the generator's, with its explanation of what
/// that particular variable does to their launch, and a suppressor added
/// upstream is enforced here the day it lands rather than the day somebody
/// remembers to copy it.
pub fn resolve(env: &EnvMap, name: &str, profile: Profile) -> Result<Resolution, PlanError> {
    let turn_key = env
        .get(&profile.key_env)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| PlanError::TurnKeyMissing {
            key_env: profile.key_env.clone(),
        })?;
    let root = profile.deployment_root.trim_end_matches('/');

    let resolved = match profile.agent {
        Agent::Claude => {
            let mut launch = ClaudeLaunch::new(root, turn_key)?;
            if profile.auth == AuthKind::ForwardedLogin {
                launch = launch.forwarding_claude_login();
            }
            let must_be_unset = launch.must_be_unset();
            for suppressor in &must_be_unset {
                if suppressor.site != SuppressorSite::EnvVar {
                    continue;
                }
                if let Some(value) = env.get(suppressor.name) {
                    launch = launch.also_launching_with(suppressor.name, value);
                }
            }
            let generated = launch.env()?;
            Resolved::Claude {
                launch,
                env: generated,
                must_be_unset,
            }
        }
        Agent::Codex => {
            let codex_home = Profile::codex_home(env, name)?;
            let catalog = profile
                .model_catalog_path
                .clone()
                .unwrap_or_else(|| codex_home.join(CODEX_CATALOG_FILE));
            // The **Responses prefix** here where the Claude arm passes the
            // bare root, and each generator refuses the other's shape by name.
            // One field in the profile, two derivations, so a profile cannot
            // name two deployments.
            let mut launch = CodexLaunch::new(format!("{root}{API_PREFIX}"), &catalog)?
                .with_key_env(&profile.key_env)
                .with_model(profile.model_slug());
            if profile.auth == AuthKind::ForwardedLogin {
                launch = launch.forwarding_openai_login();
            }
            let files = vec![
                GeneratedFile {
                    relative_path: CODEX_CONFIG_FILE.to_string(),
                    contents: launch.config_toml(),
                },
                GeneratedFile {
                    relative_path: CODEX_CATALOG_FILE.to_string(),
                    contents: launch.model_catalog_json(),
                },
            ];
            Resolved::Codex {
                launch,
                codex_home,
                files,
            }
        }
    };

    Ok(Resolution {
        name: name.to_string(),
        profile,
        resolved,
    })
}

impl Resolution {
    /// The whole resolution as `topham plan` prints it.
    ///
    /// Deterministic to the byte, which is what makes it snapshot-testable and
    /// diffable between two profiles: no timestamps, no absolute paths this
    /// crate did not derive from the environment it was handed, and every map
    /// rendered in name order because [`EnvMap`] and the generators' own maps
    /// are `BTreeMap`s.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let field = |out: &mut String, name: &str, value: &str| {
            out.push_str(&format!("{name:<16}: {value}\n"));
        };
        field(&mut out, "profile", &self.name);
        field(&mut out, "agent", self.profile.agent.as_str());
        field(&mut out, "topology", self.profile.topology.as_str());
        field(&mut out, "auth", self.profile.auth.as_str());
        field(&mut out, "deployment root", &self.profile.deployment_root);
        field(
            &mut out,
            "turn key",
            &format!("read from ${} ({REDACTED_VALUE})", self.profile.key_env),
        );

        match &self.resolved {
            Resolved::Claude {
                launch,
                env,
                must_be_unset,
            } => {
                field(&mut out, "messages url", &launch.messages_url());
                out.push('\n');
                out.push_str("environment handed to the client (the generator's own Debug):\n");
                out.push_str(&indent(&format!("{env:#?}")));
                out.push('\n');
                out.push_str("must be unset when this launch runs:\n");
                out.push_str(&render_suppressors(must_be_unset));
                out.push('\n');
                out.push_str("files written by `topham launch`:\n");
                out.push_str(&indent(
                    "(none) -- Claude Code's whole redirect surface is environment",
                ));
            }
            Resolved::Codex {
                launch,
                codex_home,
                files,
            } => {
                field(
                    &mut out,
                    "responses url",
                    &format!("{}/responses", launch.base_url),
                );
                field(&mut out, "mcp url", &launch.mcp_url());
                field(&mut out, "model slug", &launch.model);
                out.push('\n');
                out.push_str("environment handed to the client:\n");
                out.push_str(&indent(&format!(
                    "CODEX_HOME = {:?}\n{} = {REDACTED_VALUE:?}",
                    codex_home.display().to_string(),
                    launch.key_env,
                )));
                out.push('\n');
                out.push_str("must be unset when this launch runs:\n");
                out.push_str(&indent(
                    "(none) -- codex resolves its credential from the config file below, which \
                     names the\nvariable rather than reading an ambient one",
                ));
                out.push('\n');
                out.push_str("files written by `topham launch`:\n");
                // Paths, not contents. The generated `config.toml` is sixty
                // lines of the generator's own operator-facing comments, and a
                // dry run that printed it whole would bury the six lines above
                // that are the actual decision. `topham launch` writes them;
                // reading them is `cat`'s job.
                out.push_str(&indent(
                    &files
                        .iter()
                        .map(|file| codex_home.join(&file.relative_path).display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ));
            }
        }

        out.push('\n');
        out.push_str("notes:\n");
        out.push_str(&indent(
            &self
                .notes()
                .iter()
                .map(|note| format!("- {note}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        out
    }

    /// The limits of this launch that no refusal can close.
    ///
    /// Each of these is something an operator can only learn from a document or
    /// from a day of debugging, and each is a property of the *client* rather
    /// than of roundhouse — which is why they are printed rather than enforced:
    /// there is nothing here to fix.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        match (&self.resolved, self.profile.auth) {
            (Resolved::Claude { .. }, AuthKind::RoundhouseKey) => {
                notes.push(
                    "an interactive session asks once. With `-p` the client always uses the API \
                     key; interactively it asks the user to approve it overriding their \
                     subscription, and until they do that session is on the subscription."
                        .to_string(),
                );
            }
            (Resolved::Claude { .. }, AuthKind::ForwardedLogin) => {
                notes.push(
                    "the precondition is a completed `claude` login, not this profile. Without \
                     one the client presents no credential and roundhouse degrades the turn to \
                     local-only, which nothing in the run reports."
                        .to_string(),
                );
            }
            (Resolved::Codex { codex_home, .. }, AuthKind::ForwardedLogin) => {
                notes.push(format!(
                    "run `codex login` against this profile's CODEX_HOME ({}) first. The stanza \
                     selects a code path; the Authorization header comes from the auth.json that \
                     login writes and from nothing else.",
                    codex_home.display()
                ));
            }
            (Resolved::Codex { .. }, AuthKind::RoundhouseKey) => {
                notes.push(
                    "the generated config names the key variable rather than holding a key, so \
                     the files above are safe to read, diff and keep."
                        .to_string(),
                );
            }
        }
        if let Resolved::Claude { must_be_unset, .. } = &self.resolved
            && must_be_unset
                .iter()
                .any(|suppressor| suppressor.site == SuppressorSite::SettingsKey)
        {
            notes.push(
                "one entry above lives in the client's settings file rather than the \
                 environment. This launcher reads no settings file -- which one the client \
                 resolves is its own layered search -- so that entry is stated, not enforced."
                    .to_string(),
            );
        }
        if self.profile.topology == Topology::Chained {
            notes.push(
                "this profile is chained: `topham launch` is the Direct entry point and refuses \
                 it. `topham relay` runs the same generated environment behind a NeMo Relay."
                    .to_string(),
            );
        }
        // The chained codex limit, stated where an operator reads it rather
        // than only in `relay`'s module doc. It is the difference between a
        // deployment that routes and one that quietly answers every turn
        // locally, and nothing in a running session says which happened.
        if self.profile.topology == Topology::Chained
            && matches!(self.resolved, Resolved::Codex { .. })
        {
            notes.push(
                "chained codex is unproven, and the observed reason is specific: Relay splices \
                 `--config model_provider=\"nemo-relay-openai\"` onto codex's argv, and a \
                 codex `--config` override outranks the generated config.toml below. So the \
                 turn-key header that config names is not what the client presents, roundhouse \
                 sees a credential-less turn, admits it and degrades to local-only routing. \
                 Chained Claude Code is unaffected -- Relay merges into that client's header \
                 block instead of replacing its provider -- and is proven end to end."
                    .to_string(),
            );
        }
        notes
    }
}

/// The must-be-unset table, one line each, naming what each entry defeats.
fn render_suppressors(suppressors: &[&'static OauthSuppressor]) -> String {
    if suppressors.is_empty() {
        return indent("(none)");
    }
    indent(
        &suppressors
            .iter()
            .map(|suppressor| {
                let site = match suppressor.site {
                    SuppressorSite::EnvVar => "environment",
                    SuppressorSite::SettingsKey => "settings key",
                };
                format!("{} ({site}) -- {}", suppressor.name, defeats(suppressor))
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// What one suppressor costs, in the operator's terms rather than the client's.
///
/// A sentence per `Defeats` arm rather than the enum's `Debug`: `TheRedirect`
/// tells a reader nothing, and the whole reason this table is printed is that
/// each entry's consequence is different — one sends the client to another
/// cloud, one un-suppresses a subscription login, one turns off the forwarding
/// the profile exists for.
fn defeats(suppressor: &OauthSuppressor) -> &'static str {
    use roundhouse_server::claude_launch::Defeats;
    match suppressor.defeats {
        Defeats::TheRedirect => "the client goes to another cloud and never reads the base URL",
        Defeats::TheApiKeySentinel => {
            "an ambient login stops being suppressed and reaches this deployment"
        }
        Defeats::TheSubscriptionLogin => {
            "the login this profile forwards is suppressed; every request still answers"
        }
    }
}

/// Four spaces on every line, including the blank ones a `{:#?}` leaves.
fn indent(text: &str) -> String {
    text.lines()
        .map(|line| match line.is_empty() {
            true => String::new(),
            false => format!("    {line}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[cfg(test)]
mod tests;
