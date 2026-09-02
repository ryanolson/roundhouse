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
//!
//! # Why the client's settings files are read
//!
//! Claude Code applies a settings file's `env` block by **replacing** the value
//! it inherited, so a `settings.json` left behind by something else — a
//! persistent NeMo Relay install writes exactly this, `env.ANTHROPIC_BASE_URL`
//! pointed at its gateway — silently outranks the environment this launcher
//! generated, and the client that starts is not the launch the profile names.
//! An ambient-only sweep cannot see that (F3), so [`resolve`] reads the three
//! files the client's own search would find and refuses one that names a
//! generated variable or a suppressor.
//!
//! **The administrator's managed-policy file is not among them.** It is
//! deliberately outside the operator's control — a refusal naming it would tell
//! them to edit a file they may not write — and its path is platform-specific
//! in a way no capture in this phase verified, so a launcher that guessed at it
//! would refuse against a file that does not exist there. Its layer is stated
//! in the plan's notes rather than enforced, the same way a settings key was
//! before this launcher read any file at all.
//!
//! # What a profile may hold, and where that is checked
//!
//! A profile reaching [`resolve`] has not necessarily been through
//! [`Profile::from_toml`] — the TUI's editor, a test and any future constructor
//! all build one in process — and the Claude arm reads neither of the two
//! fields only a codex profile has. So resolution validates the profile it is
//! handed, through the file boundary's own code rather than a second copy of
//! its rules (F20). The structural fix, an agent-shaped enum in which a Claude
//! profile cannot hold a slug at all, is deferred: it changes the profile
//! file's schema and every reader of it, which is a larger change than the
//! defect.

use std::path::{Path, PathBuf};

use roundhouse_server::claude_launch::{OauthSuppressor, REDACTED_VALUE, SuppressorSite};
use roundhouse_server::codex_launch::{CodexLaunchError, GeneratedFile};
use roundhouse_server::{
    API_PREFIX, ClaudeAuthKind, ClaudeEnv, ClaudeLaunch, ClaudeLaunchError, CodexLaunch,
};

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

/// The client's settings file, in each of the directories it searches.
const SETTINGS_FILE: &str = "settings.json";

/// The project-local override beside it, which the client reads after
/// [`SETTINGS_FILE`] and which is the one a `.gitignore` usually covers — so it
/// is the copy an operator is least likely to remember is there.
const SETTINGS_LOCAL_FILE: &str = "settings.local.json";

/// The directory both project-local settings files live in, relative to the
/// directory the client is started in.
const PROJECT_SETTINGS_DIR: &str = ".claude";

/// What a settings `env` entry is declared to the generator as.
///
/// The generator is asked whether a *name* is admissible; none of its refusals
/// read the value, and holding the real one would put an operator's
/// `ANTHROPIC_AUTH_TOKEN` — which is exactly what a settings `env` block is
/// used for — inside a struct this module prints. See [`SettingsFile::env`].
const A_SETTINGS_VALUE: &str = "<from the settings file>";

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
        /// The settings files the launched client will read, as this launcher
        /// found them.
        ///
        /// Held because the rendering shows them: an operator whose launch was
        /// refused by one of these files needs to know which files were
        /// searched at all, and an operator whose launch was *not* refused
        /// needs to know that the search happened. What must not be set when
        /// this launch runs is not held beside it — that is
        /// [`ClaudeLaunch::must_be_unset`] on the launch above, and a second
        /// home for a derived value is a second thing that can be stale (F16).
        settings: Vec<SettingsFile>,
        /// The arguments this launcher puts before the operator's own — the
        /// MCP registration and the signage (M12, R-M3/R-M4).
        ///
        /// **Resolved here rather than built in [`crate::launch`]**, for the
        /// reason [`resolve`] exists at all: `plan`, `launch` and the screen
        /// are three surfaces over one resolution, and an argv derived on the
        /// launch path only would be one a dry run could not show. It is held
        /// as the generator's own rendering rather than as its inputs so that
        /// what is printed is what is passed.
        leading_argv: Vec<String>,
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
    #[error(
        "the settings file {path} sets `{what}`, and the launched client reads that file for \
         itself: an `env` entry there *replaces* the value this launcher exports, and an \
         `apiKeyHelper` resolves a credential the generated environment cannot suppress. So the \
         client that started would not be the launch this profile names, and every turn would \
         still answer. Remove that key, or point the profile at what the file already says"
    )]
    SettingsDefeats {
        path: PathBuf,
        /// The key as the file spells it — `env.<NAME>` or `apiKeyHelper`.
        what: String,
        /// The generator's own account of what that name does to this launch,
        /// as the cause rather than inlined: see [`crate::cli::error_chain`].
        #[source]
        source: ClaudeLaunchError,
    },
    #[error(
        "could not read the settings file {path}, which the launched client will read for itself: \
         {why}. Refused rather than skipped: a file this launcher cannot parse is one whose `env` \
         block it cannot check, and that block silently outranks everything this launch exports"
    )]
    SettingsUnreadable {
        path: PathBuf,
        /// Rendered rather than kept as a cause, so nothing here can quote the
        /// file's own text back — a settings file is where a credential lives.
        why: String,
    },
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
    // **Through the file boundary's own code**, rather than a second copy of
    // its cross-field rules here (F20, and the module doc): a profile built in
    // process has never been through `from_toml`, and the Claude arm below
    // reads neither of the fields only a codex profile has, so an unvalidated
    // one is dropped in silence where a loaded one is refused by name. The
    // round trip costs a serialize and a parse of six short fields, and buys
    // there being exactly one place that knows which field belongs to which
    // agent.
    let profile = Profile::from_toml(&profile.to_toml(), name)?;
    let turn_key = env
        .get(&profile.key_env)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| PlanError::TurnKeyMissing {
            key_env: profile.key_env.clone(),
        })?;
    let root = profile.deployment_root.trim_end_matches('/');

    let resolved = match profile.agent {
        Agent::Claude => {
            let mut launch = ClaudeLaunch::new(root, turn_key)?
                // The profile's own variable, not the generator's default: the
                // registration tells the client to expand `${…}` and the key is
                // exported under whatever `key-env` names, so a mismatch here
                // produces control calls carrying an empty key while every
                // inference turn still answers.
                .with_key_env(&profile.key_env)
                .with_strict_mcp_config(profile.strict_mcp);
            if profile.auth == AuthKind::ForwardedLogin {
                launch = launch.forwarding_claude_login();
            }
            for suppressor in launch.must_be_unset() {
                if suppressor.site != SuppressorSite::EnvVar {
                    continue;
                }
                if let Some(value) = env.get(suppressor.name) {
                    launch = launch.also_launching_with(suppressor.name, value);
                }
            }
            // The ambient refusal first, so that a settings file is only ever
            // blamed for something a launch this environment already admits.
            let generated = launch.env()?;
            let settings = read_settings(env, working_directory().as_deref())?;
            for file in &settings {
                file.refuse_what_it_defeats(&launch)?;
            }
            Resolved::Claude {
                leading_argv: launch.leading_argv(),
                launch,
                env: generated,
                settings,
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
            let mut files = vec![GeneratedFile {
                relative_path: CODEX_CONFIG_FILE.to_string(),
                contents: launch.config_toml(),
            }];
            // **Only when the profile did not name a catalog of its own** (F8).
            // The config above points at `catalog`, so generating a second file
            // under the default name when that path is somewhere else writes a
            // catalog nothing reads -- and reports it as one of the files this
            // launch wrote, which is the half that misleads.
            if profile.model_catalog_path.is_none() {
                files.push(GeneratedFile {
                    relative_path: CODEX_CATALOG_FILE.to_string(),
                    contents: launch.model_catalog_json(),
                });
            }
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

/// One of the client's settings files, as far as a launch depends on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsFile {
    /// Where it was found, so a refusal can name the file to edit.
    pub path: PathBuf,
    /// The **names** in the file's `env` block, never the values.
    ///
    /// A settings `env` block is the usual home of an operator's
    /// `ANTHROPIC_AUTH_TOKEN`, and this struct is printed by `topham plan` and
    /// by the screen's plan pane. Every refusal below is a question about a
    /// name, so the values are read and dropped rather than carried into a type
    /// whose next field would be the one that leaks — the argument
    /// `ClaudeLaunch` makes for its own private fields.
    pub env: Vec<String>,
    /// Whether the file defines `apiKeyHelper`, which resolves a credential no
    /// generated environment can suppress.
    pub api_key_helper: bool,
}

impl SettingsFile {
    /// Refuse this launch if this file would change what it means.
    ///
    /// The question is put to the **generator**, by declaring each key the way
    /// an ambient variable is declared and reading its answer, so the table of
    /// what defeats what stays in the one place that owns it: a name this
    /// launch already writes comes back as a collision, and a suppressor comes
    /// back with the sentence describing what it costs this auth kind.
    fn refuse_what_it_defeats(&self, launch: &ClaudeLaunch) -> Result<(), PlanError> {
        let defeated = |what: String, source: ClaudeLaunchError| PlanError::SettingsDefeats {
            path: self.path.clone(),
            what,
            source,
        };
        for name in &self.env {
            if let Err(source) = launch
                .clone()
                .also_launching_with(name, A_SETTINGS_VALUE)
                .env()
            {
                return Err(defeated(format!("env.{name}"), source));
            }
        }
        if self.api_key_helper
            && let Err(source) = launch.clone().with_settings_api_key_helper().env()
        {
            return Err(defeated("apiKeyHelper".to_string(), source));
        }
        Ok(())
    }
}

/// The directory the client will be started in, for the project-local settings
/// files that are resolved against it.
///
/// The one ambient read in this crate that is not a variable, and it is here
/// rather than in [`crate::env`] for the reason that module exists: it is read
/// once, at the top of a resolution, and handed on as a value. `None` when the
/// directory cannot be read at all (deleted out from under the process), which
/// is treated as "no project settings" rather than as a refusal — the client
/// started there would have the same problem, and about that this launcher has
/// nothing to add.
fn working_directory() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

/// The settings files the launched client will read, in the order it reads
/// them.
///
/// `CLAUDE_CONFIG_DIR` relocates the user file wholesale — the same rule the
/// e2e rig relies on to keep a run out of an operator's real one — and
/// `$HOME/.claude` is where it lands otherwise. The two project-local files sit
/// under the directory the client is started in, which is this process's own:
/// `exec` does not change it.
fn settings_paths(env: &EnvMap, working_directory: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let absolute = |name: &str| {
        env.get(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let user = absolute("CLAUDE_CONFIG_DIR")
        .or_else(|| absolute("HOME").map(|home| home.join(PROJECT_SETTINGS_DIR)));
    if let Some(directory) = user {
        paths.push(directory.join(SETTINGS_FILE));
    }
    if let Some(directory) = working_directory {
        paths.push(directory.join(PROJECT_SETTINGS_DIR).join(SETTINGS_FILE));
        paths.push(
            directory
                .join(PROJECT_SETTINGS_DIR)
                .join(SETTINGS_LOCAL_FILE),
        );
    }
    paths
}

/// Read every settings file that exists, skipping the ones that do not.
fn read_settings(
    env: &EnvMap,
    working_directory: Option<&Path>,
) -> Result<Vec<SettingsFile>, PlanError> {
    settings_paths(env, working_directory)
        .into_iter()
        .filter_map(|path| read_settings_file(&path).transpose())
        .collect()
}

/// One file: absent, readable, or a refusal.
///
/// A missing file is the ordinary case and not an error — most launches have
/// none of the three. Anything else is refused, including a file whose JSON
/// does not parse: the client applies what it can read out of that file, and a
/// launcher that shrugged at a parse failure would be admitting exactly the
/// `env` block it cannot see.
fn read_settings_file(path: &Path) -> Result<Option<SettingsFile>, PlanError> {
    let unreadable = |why: String| PlanError::SettingsUnreadable {
        path: path.to_path_buf(),
        why,
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(error.to_string())),
    };
    let document: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| unreadable(error.to_string()))?;
    Ok(Some(SettingsFile {
        path: path.to_path_buf(),
        env: document
            .get("env")
            .and_then(serde_json::Value::as_object)
            .map(|block| block.keys().cloned().collect())
            .unwrap_or_default(),
        // Present *and* not null: a settings file that writes `"apiKeyHelper":
        // null` to turn an inherited one off is not a file that defines one.
        api_key_helper: document
            .get("apiKeyHelper")
            .is_some_and(|value| !value.is_null()),
    }))
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
                settings,
                leading_argv,
            } => {
                field(&mut out, "messages url", &launch.messages_url());
                out.push('\n');
                out.push_str("environment handed to the client (the generator's own Debug):\n");
                out.push_str(&indent(&format!("{env:#?}")));
                out.push('\n');
                out.push_str("argv prepended to the operator's own:\n");
                out.push_str(&render_leading_argv(leading_argv));
                out.push('\n');
                out.push_str("must be unset when this launch runs:\n");
                out.push_str(&render_suppressors(&launch.must_be_unset(), launch.auth()));
                out.push('\n');
                out.push_str("settings files the client will read:\n");
                out.push_str(&render_settings(settings));
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
        // The limit R-M3's registration cannot close, and the one an operator
        // is likeliest to read as "the control surface is broken": the flag
        // makes the tools *exist* for the client, and the client's own
        // permission layer decides whether it may call one.
        if matches!(self.resolved, Resolved::Claude { .. }) {
            notes.push(
                "the registration above is what makes roundhouse's control tools exist for this \
                 client, not what makes it call one. Headless (`-p`), the client synthesises a \
                 permission refusal for an `mcp__roundhouse__*` tool unless its own argv names it \
                 -- `--allowedTools mcp__roundhouse__status` and so on; interactively it asks the \
                 operator. Neither is something this launcher can decide for it."
                    .to_string(),
            );
        }
        if let Resolved::Claude { launch, .. } = &self.resolved
            && launch
                .must_be_unset()
                .iter()
                .any(|suppressor| suppressor.site == SuppressorSite::SettingsKey)
        {
            notes.push(
                "one entry above lives in the client's settings file rather than the \
                 environment. The three files listed above are read and refused; an \
                 administrator's managed-policy file is not read, and outranks all of them, so \
                 that one layer is stated rather than enforced."
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

/// The generated argv, one argument per line, with the key variable
/// **unexpanded**.
///
/// Unexpanded because that is what is passed: the `${…}` in the registration is
/// the client's to expand, and a dry run that showed the key there would be
/// showing something the launch never carries — as well as printing a
/// credential on a screen an operator screen-shares.
///
/// **The signage is named and not printed**, which is the same call `topham
/// plan` already makes about codex's generated `config.toml`: it is prose long
/// enough to bury the six lines above it that are the actual decision, it is a
/// pure function of the tool list, and `claude_launch::signage`'s own tests are
/// what pin its contents. What an operator needs from a plan is *that* an
/// appended system prompt is being passed and roughly how much of one; reading
/// it is `topham plan | ...`'s job, and the argument itself is one flag away.
fn render_leading_argv(argv: &[String]) -> String {
    if argv.is_empty() {
        return indent("(none)");
    }
    let mut lines = Vec::new();
    let mut arguments = argv.iter();
    while let Some(argument) = arguments.next() {
        if argument == APPEND_SYSTEM_PROMPT {
            let text = arguments.next().map(String::as_str).unwrap_or_default();
            lines.push(format!(
                "{argument} <the control-tool signage, {} characters>",
                text.chars().count()
            ));
            continue;
        }
        lines.push(argument.clone());
    }
    indent(&lines.join("\n"))
}

/// The flag whose value this rendering summarises rather than prints.
///
/// Matched by value rather than by position because the argv is the generator's
/// and may grow: a positional rule ("the last two") would silently start
/// printing the whole signage the day an argument is added after it.
const APPEND_SYSTEM_PROMPT: &str = "--append-system-prompt";

/// The must-be-unset table, one line each, naming what each entry defeats.
fn render_suppressors(suppressors: &[&'static OauthSuppressor], auth: ClaudeAuthKind) -> String {
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
                format!(
                    "{} ({site}) -- {}",
                    suppressor.name,
                    defeats(suppressor, auth)
                )
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
///
/// One arm needs the auth kind as well as the row, and it is the same asymmetry
/// the generator splits two errors over: a row that resolves to a bearer
/// credential *replaces* the login a forwarding profile exists to forward, and
/// *joins* the turn key a bring-your-own-key profile promised was the only one.
/// Printing the forwarding sentence beside a `RoundhouseKey` launch would
/// describe a login that launch does not have.
fn defeats(suppressor: &OauthSuppressor, auth: ClaudeAuthKind) -> &'static str {
    use roundhouse_server::claude_launch::Defeats;
    match suppressor.defeats {
        Defeats::TheRedirect => "the client goes to another cloud and never reads the base URL",
        Defeats::TheApiKeySentinel => {
            "an ambient login stops being suppressed and reaches this deployment"
        }
        Defeats::TheSubscriptionLogin if auth == ClaudeAuthKind::RoundhouseKey => {
            "a second credential rides beside the sentinel and the edge reads it as the seat"
        }
        Defeats::TheSubscriptionLogin => {
            "the login this profile forwards is suppressed; every request still answers"
        }
    }
}

/// The settings files, one line each, naming what in them a launch depends on.
///
/// Names and never values — see [`SettingsFile::env`] — and the files that were
/// *searched* rather than only the ones that exist would be the friendlier
/// output and the wrong one: a plan that listed three paths an operator has
/// never created reads as three things to go and check.
fn render_settings(settings: &[SettingsFile]) -> String {
    if settings.is_empty() {
        return indent(
            "(none found) -- an administrator's managed-policy file is not among the three \
             searched;\nsee the notes",
        );
    }
    indent(
        &settings
            .iter()
            .map(|file| {
                let mut keys = file
                    .env
                    .iter()
                    .map(|name| format!("env.{name}"))
                    .collect::<Vec<_>>();
                if file.api_key_helper {
                    keys.push("apiKeyHelper".to_string());
                }
                let keys = match keys.is_empty() {
                    true => "nothing this launch depends on".to_string(),
                    false => keys.join(", "),
                };
                format!("{} -- {keys}", file.path.display())
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
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
