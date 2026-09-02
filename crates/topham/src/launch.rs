// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Becoming the agent: the files, the layered environment, and the `exec`.
//!
//! `topham launch <profile> [-- <argv>]` resolves the profile ([`crate::plan`]),
//! writes the codex files if there are any, and replaces itself with the client.
//! Everything that can refuse has refused by then, which is the point of doing
//! it in this order: after the `exec` this process is gone, and a launcher that
//! discovered a problem afterwards would have nowhere to report it.
//!
//! # Why the refusal is checked against the operator's own environment
//!
//! The gated e2e rig builds a hermetic environment and hands the child exactly
//! what it chose. A launcher cannot: it runs in a real session, and the whole
//! reason `ClaudeLaunch::must_be_unset` exists is that the *ambient*
//! environment is what silently changes what a launch means — a
//! `ForwardedClaudeLogin` next to an inherited `ANTHROPIC_AUTH_TOKEN` forwards
//! nothing while every request stays valid.
//!
//! So the check runs against [`EnvMap`], which is this process's own
//! environment as captured by [`crate::env::system`]. It is the one thing
//! standing between an ambient login and a launch that quietly does something
//! else, and it is why `topham launch` is a program rather than a shell
//! function that exports three variables.
//!
//! # `env_clear` and then the whole map
//!
//! The child is given the layered map in full rather than inheriting and having
//! three variables overwritten. The two are identical in effect — the map *is*
//! the inherited environment, plus the generated variables on top — and the
//! difference is that one of them is a value a test can assert on.
//! [`LaunchPlan::env`] is exactly what the child gets, so
//! "a generated variable wins over an ambient one of the same name, and an
//! unrelated ambient variable survives" is a property of a map rather than a
//! claim about `Command`'s merge order.
//!
//! # The seam
//!
//! [`Launcher`] has two implementations: [`ExecLauncher`], which really does
//! replace this process, and [`RecordingLauncher`], which keeps the plan. The
//! trait exists for the second one — there is no way to unit-test a function
//! whose success case is that it never returns.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::env::EnvMap;
use crate::plan::{PlanError, Resolution, Resolved};
use crate::profile::{Profile, Topology};

/// Where codex reads its configuration from, and the variable a launch points
/// at this profile's own copy.
pub const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// Deployment policy this launcher sets on a Claude Code child, named by
/// `claude_launch`'s own doc as belonging to whoever spawns the process.
///
/// **Not part of the generated map, deliberately, and set anyway.** The
/// generator's rule is that its output is "the hook-up, and nothing else", so
/// these live here. They are set rather than left alone because a client this
/// launcher pointed at a roundhouse deployment is not a client the operator is
/// driving by hand: an autoupdate mid-session swaps the binary whose wire
/// behaviour this deployment's dialect was verified against, and the
/// non-essential traffic is telemetry about a session that is not Anthropic's
/// to see. Both are overridable by the operator's own environment — they are
/// layered *under* nothing, so an explicit ambient value of either name is
/// overwritten; an operator who means the opposite says so in the profile's
/// argv or turns off the launcher.
const CLAUDE_DEPLOYMENT_POLICY: &[(&str, &str)] = &[
    ("DISABLE_AUTOUPDATER", "1"),
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
];

/// Everything the child process is: what to run, with what argv, in what
/// environment, after which files exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    /// The program name, resolved through `PATH` by the exec. See
    /// `Agent::program`.
    pub program: String,
    /// Arguments *after* the program name — the operator's `-- <argv>`,
    /// verbatim.
    ///
    /// Empty by default, and no default arguments are invented: an agent
    /// launched with no argv opens its own interactive session, which is what
    /// `topham launch <profile>` should mean.
    pub argv: Vec<String>,
    /// Exactly what the child's environment will be, in name order.
    pub env: BTreeMap<String, String>,
    /// Files written before the exec, absolute.
    pub files: Vec<(PathBuf, String)>,
}

/// Why a launch could not be prepared or performed.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(
        "the profile `{name}` is chained: it reaches roundhouse through a NeMo Relay, and \
         `topham launch` is the Direct entry point. Launching it here would produce a working \
         client pointed straight at the deployment -- which is a different topology from the one \
         the profile names, instrumented by nothing. Use `topham relay {name}`"
    )]
    WrongTopology { name: String },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "could not execute `{program}`: {source}. Nothing was launched -- the generated files, if \
         any, are already written and are what the next attempt will overwrite"
    )]
    Exec {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

/// Turn a resolution into the child process it describes.
///
/// The layering is here and in one place: the ambient environment first, the
/// generated variables on top. That order is what R-T7 pins — an ambient
/// `ANTHROPIC_BASE_URL` left over from some other deployment must not survive
/// into a launch this profile aimed somewhere else, and an unrelated ambient
/// variable (`PATH`, `HOME`, the operator's editor) must.
pub fn plan(
    resolution: &Resolution,
    ambient: &EnvMap,
    argv: Vec<String>,
) -> Result<LaunchPlan, LaunchError> {
    if resolution.profile.topology == Topology::Chained {
        return Err(LaunchError::WrongTopology {
            name: resolution.name.clone(),
        });
    }

    let (env, files) = layered(resolution, ambient);
    Ok(LaunchPlan {
        program: resolution.profile.agent.program().to_string(),
        argv,
        env,
        files,
    })
}

/// The child's environment and generated files, without deciding what program
/// runs.
///
/// Split out of [`plan`] for [`crate::relay`], which needs exactly this and a
/// different program: on the chained path the client is spawned by Relay rather
/// than by this process, and R-D′'s finding is that it is handed **the same
/// map** — Relay overwrites the base URL and merges into the header block
/// itself. A second copy of this layering in the relay module would be the one
/// place the two topologies could quietly stop being the same launch, which is
/// the claim the whole chained runbook rests on.
pub(crate) fn layered(
    resolution: &Resolution,
    ambient: &EnvMap,
) -> (BTreeMap<String, String>, Vec<(PathBuf, String)>) {
    let mut env = ambient.clone();
    let mut files = Vec::new();

    match &resolution.resolved {
        Resolved::Claude { env: generated, .. } => {
            for (name, value) in CLAUDE_DEPLOYMENT_POLICY {
                env.insert((*name).to_string(), (*value).to_string());
            }
            // `ClaudeEnv::vars` is the one seam that yields the turn key, and
            // this is its caller: the process that spawns the client. Anywhere
            // else it would be the defect that type exists to make visible.
            for (name, value) in generated.vars() {
                env.insert(name.to_string(), value);
            }
        }
        Resolved::Codex {
            codex_home,
            files: generated,
            ..
        } => {
            for file in generated {
                files.push((codex_home.join(&file.relative_path), file.contents.clone()));
            }
            env.insert(CODEX_HOME_ENV.to_string(), codex_home.display().to_string());
            // The generator never sees the key -- it names a variable. Putting
            // the value in the child's environment under that name is this
            // launcher's half of the same rule, and the only half that touches
            // a secret at all.
            if let Some(key) = ambient.get(&resolution.profile.key_env) {
                env.insert(resolution.profile.key_env.clone(), key.clone());
            }
        }
    }

    (env, files)
}

/// Write the generated files, creating their directories.
///
/// Overwriting rather than merging, and that is what makes a profile the source
/// of truth: a generated `config.toml` is derived from the profile on every
/// launch, so an edit made to the file instead of to the profile lasts exactly
/// one run. The alternative — refusing to overwrite — would make the second
/// launch of every profile fail.
pub fn write_files(plan: &LaunchPlan) -> Result<(), LaunchError> {
    for (path, contents) in &plan.files {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LaunchError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(path, contents).map_err(|source| LaunchError::Write {
            path: path.clone(),
            source,
        })?;
    }
    Ok(())
}

/// What actually starts the child.
///
/// `Ok(())` is unreachable for [`ExecLauncher`] — `execve` does not return on
/// success — and that is the whole reason the trait exists: a test needs a
/// second implementation for which returning is the *normal* outcome, so the
/// launch path above can be exercised without a process.
pub trait Launcher {
    fn launch(&self, plan: &LaunchPlan) -> Result<(), LaunchError>;
}

/// Replace this process with the agent.
///
/// `exec` rather than spawn-and-wait, so that the operator's shell is talking
/// to the client directly: job control, terminal resizes, `^C` and the exit
/// status are the client's rather than a supervisor's approximation of them. An
/// interactive agent behind a parent process that forwards signals imperfectly
/// is the failure this avoids, and it is not a hypothetical — a TUI client
/// under a naive supervisor loses its resize handling.
pub struct ExecLauncher;

impl Launcher for ExecLauncher {
    fn launch(&self, plan: &LaunchPlan) -> Result<(), LaunchError> {
        use std::os::unix::process::CommandExt;

        let mut command = std::process::Command::new(&plan.program);
        command.args(&plan.argv);
        // Cleared and rebuilt, so the child's environment is `plan.env` and
        // nothing else -- see the module doc.
        command.env_clear();
        command.envs(&plan.env);
        Err(LaunchError::Exec {
            program: plan.program.clone(),
            source: command.exec(),
        })
    }
}

/// A [`Launcher`] that records instead of launching.
///
/// Its `Mutex` is not about concurrency — nothing here is shared across
/// threads — it is what lets a test hold `&RecordingLauncher` while the code
/// under test holds `&dyn Launcher`.
#[derive(Debug, Default)]
pub struct RecordingLauncher {
    launched: Mutex<Vec<LaunchPlan>>,
}

impl RecordingLauncher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every plan handed to it, in order.
    pub fn launched(&self) -> Vec<LaunchPlan> {
        self.launched
            .lock()
            .expect("no panic holds this lock")
            .clone()
    }
}

impl Launcher for RecordingLauncher {
    fn launch(&self, plan: &LaunchPlan) -> Result<(), LaunchError> {
        self.launched
            .lock()
            .expect("no panic holds this lock")
            .push(plan.clone());
        Ok(())
    }
}

/// The whole subcommand: resolve, refuse, write, exec.
///
/// One function so that `topham launch`, the TUI's launch action and the e2e
/// suite take the same path — R-T6's rule is that the TUI is a front end over
/// the subcommands, and the cheapest way to hold that is for there to be one
/// function with the behaviour in it.
pub fn run(
    ambient: &EnvMap,
    name: &str,
    profile: Profile,
    argv: Vec<String>,
    launcher: &dyn Launcher,
) -> Result<LaunchPlan, LaunchError> {
    let resolution = crate::plan::resolve(ambient, name, profile)?;
    let plan = plan(&resolution, ambient, argv)?;
    write_files(&plan)?;
    launcher.launch(&plan)?;
    Ok(plan)
}

#[cfg(test)]
mod tests;
