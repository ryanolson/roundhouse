// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `topham relay <profile>` — the chained topology, from an operator's shell.
//!
//! The same launch as [`crate::launch`] with a NeMo Relay in the middle, and
//! **deliberately the same map**: R-D′'s finding is that Relay overwrites the
//! base URL and merges into the header block, so one generated environment
//! serves both topologies and the client is told the identical thing either way.
//! What differs is the program this process becomes — `nemo-relay run --agent
//! <agent> --config <toml> -- <argv>` rather than the agent — and one file
//! written beside the codex ones.
//!
//! That file is rendered by
//! [`relay_handoff`](roundhouse_server::relay_handoff), not here (R-T5). The
//! `claude_e2e` rig writes the same rendering, which is what makes the gated
//! chained tests evidence about this subcommand and not only about a rig: a
//! launcher with its own copy of the four lines would be a launcher no test in
//! the tree has ever run.
//!
//! # Two ways a chained launch is aimed somewhere else, and two refusals
//!
//! Relay resolves its upstream from layers, and two of them are outside the
//! `--config` this subcommand passes:
//!
//! 1. **The system layer.** `--config` replaces only the *user* layer;
//!    `/etc/nemo-relay/config.toml` is folded in **after** it and wins on any
//!    key both name (Relay evidence §2.4). Caught by [`preflight`], which asks
//!    Relay itself what it resolved — with the environment cleared, the rig's
//!    rule (M11.2b review F7), so what is left resolving is the explicit config
//!    plus exactly the layer an operator cannot see.
//! 2. **The environment layer**, which sits *above* `--config` (§2.4). The
//!    preflight cannot catch this one **because the preflight clears it**, and
//!    clearing it is right: a probe carrying `NEMO_RELAY_ANTHROPIC_BASE_URL`
//!    would be checking a resolution the operator's own launch does not have.
//!    So it is checked directly against the captured environment, the way
//!    `must_be_unset` is — see [`RelayError::UpstreamOverriddenByEnv`].
//!
//! Neither is hypothetical and both are silent: a re-aimed chained launch
//! forwards this deployment's turn key to whatever the winning layer named, and
//! every request still answers.
//!
//! # Why the launch itself is *not* isolated
//!
//! The rig points Relay's four XDG variables at its own scratch, because a rig
//! must resolve nothing it did not write. A launcher must do the opposite: an
//! operator's `plugins.toml` — observability exporters, pricing, PII — lives in
//! their XDG config directory, and a launcher that redirected it would silently
//! turn off the instrumentation that is the entire reason to run chained. The
//! user *config* layer is replaced by `--config` regardless, so what inheriting
//! buys is exactly the operator's plugins and nothing that can re-aim the
//! upstream.
//!
//! # The codex half, and what could not be verified
//!
//! The rendering is symmetric — `[upstream] openai_base_url` and
//! `[agents.codex]`, with the base URL carrying `API_PREFIX` because Relay's
//! OpenAI upstream is the prefixed one — and a real 0.8.2 `--dry-run --agent
//! codex` accepts it and echoes it back, without a codex binary present.
//!
//! What that dry-run also shows, and what no test in this tree can close, is
//! that **Relay replaces codex's provider selection on the argv**: it splices
//! `--config model_provider="nemo-relay-openai"` plus a `model_providers`
//! table of its own, and codex's `--config` overrides outrank the `config.toml`
//! this launcher generated. So on the chained codex path the client presents
//! Relay's `x-nemo-relay-proxy-token` and **not** the generated
//! `env_http_headers` turn-key header — which Relay then strips as its own
//! (Relay evidence §A.13) — and roundhouse sees a credential-less turn, admits
//! it, and degrades to local-only routing with nothing reporting it.
//!
//! Stated rather than solved, and stated where an operator reads it
//! ([`crate::plan`]'s notes) rather than only here. It is not refused because
//! the remedy is a decision this launcher should not make on its own: Relay's
//! `[upstream] openai_auth_header` would carry the turn key instead, which is
//! the fallback wiring `claude_launch`'s runbook records as deliberately
//! untested and which hazard 4 can clear out from under a second config layer.
//! Chained **Claude Code** has none of this problem and is proven end to end in
//! `claude_e2e`, because Relay merges into that client's header block rather
//! than replacing its provider.

use std::path::{Path, PathBuf};

use roundhouse_server::relay_handoff::{RELAY_SYSTEM_CONFIG, RelayHandoff, UpstreamReAimed};

use crate::env::EnvMap;
use crate::launch::{LaunchError, LaunchPlan, Launcher};
use crate::plan::{PlanError, Resolution};
use crate::profile::{Agent, Profile, ProfileError, Topology};

/// The program a chained launch becomes, resolved through `PATH` by the exec.
///
/// The bare name for [`Agent::program`]'s reason: an operator with two Relay
/// builds has already answered which one they mean, in their `PATH`. `topham
/// relay --relay <path>` overrides it, which is also how the gated suite points
/// at its pinned binary.
pub const RELAY_PROGRAM: &str = "nemo-relay";

/// The generated Relay configuration, in the profile's own scratch.
const CONFIG_FILE: &str = "relay-config.toml";

/// Where the preflight's cleared environment is pointed.
///
/// A directory of its own rather than the profile's scratch root: the probe
/// writes Relay's bootstrap state under every XDG variable it is given, and
/// mixing that into the directory holding the generated config would make a
/// `ls` of the profile's scratch unreadable.
const PREFLIGHT_DIR: &str = "preflight-home";

/// Why a chained launch did not happen.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(transparent)]
    Launch(#[from] LaunchError),
    #[error(transparent)]
    Handoff(#[from] roundhouse_server::RelayHandoffError),
    #[error(
        "the profile `{name}` is direct: it points the client straight at the deployment, and \
         `topham relay` puts a NeMo Relay in the middle. Running it here would produce a chained \
         turn the profile does not describe -- a second hop, a second re-encode, and Relay's \
         instrumentation on a session nobody asked to instrument. Use `topham launch {name}`, or \
         set `topology = \"chained\"`"
    )]
    WrongTopology { name: String },
    #[error(
        "`{name}` is set to `{value}`, and Relay's environment layer outranks the `--config` this \
         launch passes. Every turn would be forwarded there instead of to `{wanted}`, carrying \
         this deployment's turn key, and the run would look entirely healthy. The preflight \
         cannot catch this one: it clears the environment on purpose, so that what it checks is \
         the system config layer an operator cannot see. Unset `{name}`, or set it to `{wanted}`"
    )]
    UpstreamOverriddenByEnv {
        name: &'static str,
        value: String,
        wanted: String,
    },
    #[error(
        "could not run the preflight `{program}`. Nothing was launched. Set `--relay` to a real \
         nemo-relay binary, or put one on PATH"
    )]
    PreflightSpawn {
        program: String,
        /// The cause rather than part of the sentence: a message that inlined
        /// its own source printed it twice through
        /// [`crate::cli::error_chain`], which is the stutter F5 is about.
        #[source]
        source: std::io::Error,
    },
    #[error(
        "the preflight `{program}` refused this launch ({status}) and produced no report, so what \
         Relay would have resolved is unknown. Nothing was launched. Relay's own diagnostic:\n\
         {diagnostic}"
    )]
    PreflightRefused {
        program: String,
        /// How it exited, spelled the way the platform spells it.
        status: String,
        /// What Relay wrote to stderr, which is the only account of *why* it
        /// refused — and is a `String` rather than a cause because Relay's
        /// words are not an error type this crate can hold.
        diagnostic: String,
    },
    /// Transparent, so the report Relay resolved is the whole message.
    ///
    /// It carried a `"preflight: "` prefix until F5: the prefix made this
    /// link's `Display` a *superset* of its source's rather than equal to it,
    /// which is the one shape [`crate::cli::error_chain`] cannot collapse, so
    /// the re-aim paragraph printed twice.
    #[error(transparent)]
    ReAimed(#[from] UpstreamReAimed),
}

/// Everything a chained launch is, before anything is written or spawned.
#[derive(Debug)]
pub struct RelayLaunch {
    /// The rendering, and the source of the config file and the argv.
    pub handoff: RelayHandoff,
    /// Where the generated config was — or will be — written.
    pub config: PathBuf,
    /// The isolated `HOME`/XDG root the preflight is spawned under.
    pub preflight_home: PathBuf,
    /// The child process, with Relay as the program.
    pub plan: LaunchPlan,
}

/// Resolve a chained launch: the handoff, the scratch paths, and the child.
///
/// Pure apart from the two path resolutions, so the argv layering and the two
/// refusals above are testable without a Relay binary or a filesystem write.
pub fn plan(
    resolution: &Resolution,
    ambient: &EnvMap,
    relay_program: &str,
    argv: Vec<String>,
) -> Result<RelayLaunch, RelayError> {
    if resolution.profile.topology != Topology::Chained {
        return Err(RelayError::WrongTopology {
            name: resolution.name.clone(),
        });
    }

    let root = &resolution.profile.deployment_root;
    // The *client's* program, not this launcher's: what Relay is told to run is
    // the agent, and `[agents.<name>] command` is the only place a chained
    // launch names it. Relay resolves it through the `PATH` it inherits, which
    // is the operator's -- the same answer `topham launch` would have reached.
    let command = resolution.profile.agent.program();
    let handoff = match resolution.profile.agent {
        Agent::Claude => RelayHandoff::for_claude(root, command)?,
        Agent::Codex => RelayHandoff::for_codex(root, command)?,
    };

    let overriding = handoff.agent().upstream_env_var();
    if let Some(value) = ambient.get(overriding).filter(|value| !value.is_empty())
        && value != handoff.upstream_base_url()
    {
        return Err(RelayError::UpstreamOverriddenByEnv {
            name: overriding,
            value: value.clone(),
            wanted: handoff.upstream_base_url().to_string(),
        });
    }

    let scratch = scratch_dir(ambient, &resolution.name)?;
    let config = scratch.join(CONFIG_FILE);
    let preflight_home = scratch.join(PREFLIGHT_DIR);

    // The same layering `topham launch` performs, from the same function: on
    // this path the map is handed to Relay, which hands it to the client after
    // overwriting the base URL and merging its own header in.
    let (env, mut files) = crate::launch::layered(resolution, ambient);
    files.push((config.clone(), handoff.config_toml()));

    // Relay's own arguments and the agent's generated ones are one list, and it
    // is the launcher's: `run --agent <x> --config <f> --` ends with the
    // separator, so everything after it is the *client's* argv, and the MCP
    // registration and signage belong there ahead of the operator's own.
    //
    // **The chained client is registered against the deployment directly**,
    // which is the one thing the chained runbook does not carry through Relay:
    // Relay proxies the Anthropic route and nothing else, so the `url` in the
    // generated registration reaches roundhouse's `/mcp` without a hop. Stated
    // rather than proven — the plan's own "left open" — and it is here rather
    // than absent because a chained launch that silently had no control surface
    // would be the same client as a direct one in every respect an operator can
    // see.
    let agent_args = crate::launch::generated_args(resolution);
    crate::launch::refuse_collisions(&agent_args, &argv)?;
    let mut generated_argv = handoff.run_argv(&config);
    generated_argv.extend(crate::launch::flatten_argv(&agent_args));

    Ok(RelayLaunch {
        handoff,
        config,
        preflight_home,
        plan: LaunchPlan {
            program: relay_program.to_string(),
            generated_argv,
            argv,
            env,
            files,
        },
    })
}

/// `<data>/topham/<name>/relay` — this profile's chained scratch.
///
/// Beside the generated `CODEX_HOME` and under the same per-profile root, for
/// the reason that root exists: two profiles sharing one Relay config would be
/// one profile's upstream silently applied to the other's launch, and the
/// preflight would agree with itself both times.
///
/// A join onto [`Profile::scratch_root`] rather than a walk up from
/// `codex_home` (M11.3 review F15): the walk's `.parent()` keeps returning
/// `Some` when the codex layout moves, just of the wrong directory, so the one
/// thing it could not catch is the drift it was standing in for.
pub fn scratch_dir(env: &EnvMap, name: &str) -> Result<PathBuf, ProfileError> {
    Ok(Profile::scratch_root(env, name)?.join("relay"))
}

/// Ask Relay what it resolved, with the ambient environment cleared.
///
/// Returns the report, so a caller can print it: a chained launch that *did*
/// land is worth showing, and it is the one place an operator sees the gateway
/// URL Relay chose.
///
/// **`--dry-run` spawns no agent**, which is what makes this affordable before
/// every launch rather than a thing an operator remembers to do.
///
/// # A Relay that failed is not a Relay that re-aimed
///
/// The exit status is checked before the report is read (F11). A Relay that
/// refuses outright — a config it will not load, a `--dry-run` this version
/// does not have, a version refusal — prints nothing on stdout, and a
/// stdout-only reading of that is indistinguishable from a healthy report with
/// the upstream line missing: the operator is told a system
/// `/etc/nemo-relay/config.toml` re-aimed their run, while Relay's actual
/// complaint, which was on stderr, is thrown away. So the re-aim diagnosis is
/// made **only** on a zero exit, and a non-zero one carries Relay's own words.
pub fn preflight(
    program: &str,
    handoff: &RelayHandoff,
    config: &Path,
    home: &Path,
    path: &str,
) -> Result<String, RelayError> {
    let mut command = std::process::Command::new(program);
    command.args(handoff.preflight_argv(config));
    command.env_clear();
    for (name, value) in RelayHandoff::preflight_env(home, path) {
        command.env(name, value);
    }
    let output = command
        .output()
        .map_err(|source| RelayError::PreflightSpawn {
            program: program.to_string(),
            source,
        })?;
    if !output.status.success() {
        let diagnostic = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(RelayError::PreflightRefused {
            program: program.to_string(),
            status: output.status.to_string(),
            diagnostic: match diagnostic.is_empty() {
                // Said rather than left blank: "it failed and said nothing" is
                // a different next step from "it failed and said this".
                true => "(nothing on stderr)".to_string(),
                false => diagnostic,
            },
        });
    }
    let report = String::from_utf8_lossy(&output.stdout).into_owned();
    handoff.verify_resolved(&report)?;
    Ok(report)
}

/// Everything a chained launch does **except** become the client: resolve,
/// refuse, write the config, and ask Relay what it resolved.
///
/// Every refusal `topham relay` owns lives in here — the topology, the ambient
/// upstream override, the unexported key and the settings files `plan::resolve`
/// reads, a Relay that cannot be spawned, and a system config layer that
/// re-aimed the run. What is left outside is the banner and the `exec`.
///
/// **It is a function because there are two callers, and they must not
/// diverge** (M11.3 review F22). The interactive screen has to run these
/// refusals *before* it tears the terminal down, so it composed the same five
/// steps itself — and two hand-written compositions can each be correct and
/// still answer differently: a different order, a preflight home somewhere
/// else, one side passing `--relay` and the other the bare name. A screen
/// refusal that is merely plausible is one nobody can tell is wrong, so the
/// screen calls this instead.
///
/// Returning the report as well as the launch is what lets [`run`] print the
/// banner without asking Relay twice.
pub fn dry_run(
    ambient: &EnvMap,
    name: &str,
    profile: Profile,
    relay_program: &str,
    argv: Vec<String>,
) -> Result<(RelayLaunch, String), RelayError> {
    let resolution = crate::plan::resolve(ambient, name, profile)?;
    let launch = plan(&resolution, ambient, relay_program, argv)?;

    crate::launch::write_files(&launch.plan)?;
    std::fs::create_dir_all(&launch.preflight_home).map_err(|source| LaunchError::Write {
        path: launch.preflight_home.clone(),
        source,
    })?;

    let path = ambient.get("PATH").cloned().unwrap_or_default();
    let report = preflight(
        relay_program,
        &launch.handoff,
        &launch.config,
        &launch.preflight_home,
        &path,
    )?;
    Ok((launch, report))
}

/// The whole subcommand: [`dry_run`], then the banner, then the exec.
///
/// The order is the one [`crate::launch::run`] uses and for the same reason,
/// with the preflight inserted where it can still refuse: after the config
/// exists, because the preflight's whole job is to resolve *that file*, and
/// before the exec, because after it this process is gone.
///
/// # `diagnostics` is stderr, and that is a contract rather than a preference
///
/// This process becomes the agent, and the agent's stdout is a machine-readable
/// stream: `claude -p --output-format json` prints exactly one JSON document
/// and nothing else. A banner written to stdout is therefore not "extra output"
/// but a **corrupted document**, and the corruption arrives at whatever was
/// parsing it — a script, a pipe into `jq`, the gated closure test that found
/// this — as a client that failed, when the turn in fact completed.
///
/// The parameter stays rather than becoming a hard-coded `eprint!` because it
/// is the seam the unit tests read the banner through. Both real callers
/// (`topham relay` and the interactive screen) pass stderr.
pub fn run(
    ambient: &EnvMap,
    name: &str,
    profile: Profile,
    relay_program: &str,
    argv: Vec<String>,
    launcher: &dyn Launcher,
    diagnostics: &mut dyn std::io::Write,
) -> Result<RelayLaunch, RelayError> {
    let (launch, report) = dry_run(ambient, name, profile, relay_program, argv)?;

    // Printed **before** the exec, because after it this process is gone and
    // Relay's own output owns the terminal. An operator who later wonders which
    // upstream a session was on has it in their scrollback.
    //
    // **On `diagnostics`, which every caller makes stderr** -- see this
    // function's doc. Found by the gated closure test, not by reading: a
    // `topham relay <p> -- -p … --output-format json` printed this banner and
    // Relay's preflight report onto the stdout the client then contracts to own
    // alone, and the run failed at the JSON parse while the turn itself had
    // completed perfectly.
    let _ = write!(diagnostics, "{}", banner(&launch, &report));

    launcher.launch(&launch.plan)?;
    Ok(launch)
}

/// What `topham relay` prints before it execs.
///
/// The resolved report rather than a summary of it: the gateway URL, the two
/// upstreams and both `*_auth` lines are Relay's own words, and a launcher that
/// paraphrased them would be a second thing to keep true. The one sentence
/// added is which file was written, because that is the thing not in the report.
pub fn banner(launch: &RelayLaunch, report: &str) -> String {
    format!(
        "relay config  : {}\nsystem layer  : {RELAY_SYSTEM_CONFIG} (checked, not re-aiming)\n\n\
         {report}",
        launch.config.display()
    )
}

#[cfg(test)]
mod tests;
