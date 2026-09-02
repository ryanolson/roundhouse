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
    /// Arguments this launcher generated, ahead of the operator's own.
    ///
    /// **A field of its own rather than a prefix spliced into [`Self::argv`]**
    /// (M12, R-M3), because the two are answerable to different people: this
    /// one is derived from the profile and is the launcher's to change, and the
    /// one below is the operator's text, which nothing may reorder or drop. A
    /// single list would make "the operator's argv arrived verbatim" a claim
    /// about a slice offset rather than a value a test can read.
    ///
    /// Empty for a codex launch, whose whole configuration is files.
    pub generated_argv: Vec<String>,
    /// Arguments after those — the operator's `-- <argv>`, verbatim.
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

impl LaunchPlan {
    /// Everything after the program name, in the order the child receives it.
    ///
    /// The one place the two argv fields are joined, so "generated first, the
    /// operator's last" is a property of this type rather than a convention
    /// each launcher re-implements.
    pub fn full_argv(&self) -> Vec<String> {
        self.generated_argv
            .iter()
            .chain(&self.argv)
            .cloned()
            .collect()
    }
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
    #[error(
        "the argv after `--` sets `{flag}`, which this launch also generates. Whichever of the \
         two the client applies second wins silently, and the two disagree about which MCP \
         servers this session has or what its system prompt says -- a generated registration \
         shadowed by the operator's own is a client whose control tools are simply absent, on a \
         run where every turn still answers. Drop it from the argv, or turn the corresponding \
         profile field off"
    )]
    ArgvCollidesWithGenerated { flag: String },
    #[error("could not write {path}")]
    Write {
        path: PathBuf,
        /// The cause rather than part of the sentence above: a message that
        /// inlines its own source prints it twice through
        /// [`crate::cli::error_chain`], which is the stutter F5 is about.
        #[source]
        source: std::io::Error,
    },
    #[error(
        "could not execute `{program}`. Nothing was launched -- the generated files, if any, are \
         already written and are what the next attempt will overwrite"
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

    let generated_argv = generated_argv(resolution);
    refuse_collisions(&generated_argv, &argv)?;
    let (env, files) = layered(resolution, ambient);
    Ok(LaunchPlan {
        program: resolution.profile.agent.program().to_string(),
        generated_argv,
        argv,
        env,
        files,
    })
}

/// The arguments this launcher generated for the *agent*, from the resolution
/// that derived them.
///
/// A read and not a derivation: [`crate::plan::resolve`] built them, so a dry
/// run and a launch cannot show different flags. Codex's is empty because that
/// client is configured by the files beside it — a fact about the client and
/// not an omission, which is why the arm says so rather than falling through a
/// wildcard.
pub(crate) fn generated_argv(resolution: &Resolution) -> Vec<String> {
    match &resolution.resolved {
        Resolved::Claude { leading_argv, .. } => leading_argv.clone(),
        Resolved::Codex { .. } => Vec::new(),
    }
}

/// Refuse an operator argv that names a flag this launch also generates.
///
/// **A refusal rather than a precedence rule**, for the reason
/// `ClaudeLaunchError::CollidesWithGeneratedVar` gives about the environment:
/// this launcher does not know which of two `--mcp-config`s the client applies,
/// and both orders produce a session that runs. If the operator's wins, the
/// control surface is silently absent; if the generated one wins, the servers
/// the operator asked for are. Neither is reported by anything, and the
/// operator's next move — drop one — is the same either way.
///
/// Compared on the flag *name* including an `=`-joined form, because
/// `--mcp-config=x` and `--mcp-config x` are one flag to clap and two strings
/// to a `contains`.
///
/// **Only the agent's own generated arguments are ever passed as `generated`.**
/// The chained path builds a longer list whose first half is Relay's
/// (`--agent`, `--config`), and codex takes a `--config` of its own — so
/// checking that half against the operator's tail would refuse a legitimate
/// agent flag for colliding with a flag the agent never sees.
pub(crate) fn refuse_collisions(generated: &[String], argv: &[String]) -> Result<(), LaunchError> {
    for flag in generated
        .iter()
        .filter(|argument| argument.starts_with("--") && !argument.contains(char::is_whitespace))
    {
        if argv
            .iter()
            .any(|argument| argument == flag || argument.starts_with(&format!("{flag}=")))
        {
            return Err(LaunchError::ArgvCollidesWithGenerated { flag: flag.clone() });
        }
    }
    Ok(())
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
///
/// # Written beside and renamed over, never truncated in place
///
/// Two launches of one profile write the same paths, and the first one's client
/// is by then an `exec`'d process that may still be opening its `config.toml`
/// (F21). A plain write truncates the inode that reader holds, so the client
/// reads a half-file or an empty one — and codex answers an empty config by
/// falling back to a *default openai provider*, in a child that was handed the
/// whole ambient environment, `OPENAI_API_KEY` included. A rename within a
/// directory is atomic, so the reader either has the old file whole or opens
/// the new one; nobody sees a torn one.
pub fn write_files(plan: &LaunchPlan) -> Result<(), LaunchError> {
    for (path, contents) in &plan.files {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LaunchError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let failed = |source| LaunchError::Write {
            path: path.clone(),
            source,
        };
        // Beside the target rather than in the system temp directory: `rename`
        // is only atomic within one filesystem, and a scratch root on a
        // different mount would turn this back into a copy-and-truncate.
        let staged = staging_path(path);
        std::fs::write(&staged, contents).map_err(failed)?;
        if let Err(source) = std::fs::rename(&staged, path) {
            let _ = std::fs::remove_file(&staged);
            return Err(failed(source));
        }
    }
    Ok(())
}

/// A name beside `path` that no concurrent launch is using.
///
/// The process id distinguishes two launches racing on one profile, and the
/// counter distinguishes the files within one launch — together they are what
/// keeps the staging file of one launch from being renamed over by another
/// before its own rename lands.
fn staging_path(path: &std::path::Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let staged = format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    match path.parent() {
        Some(parent) => parent.join(staged),
        None => PathBuf::from(staged),
    }
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
        // Generated first, the operator's last, through the one join on the
        // type -- see `LaunchPlan::full_argv`.
        command.args(plan.full_argv());
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
