// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a chained launch is, and the two ways it refuses to become one.
//!
//! The preflight tests **do** spawn a process, unlike everything else in this
//! crate, and that is deliberate: what is under test is the spawn — the argv, the
//! cleared environment, and the reading of what came back. A trait double in
//! place of the child would leave the one thing that can be wrong (Relay is
//! invoked with a `--dry-run` it does not understand, or with an environment
//! that re-aims it) untested by construction. The child is a shell script that
//! prints a Relay report, so the suite still needs no Relay binary.

use std::io::Write as _;
use std::path::PathBuf;

use roundhouse_server::relay_handoff::RelayAgent;

use super::*;
use crate::launch::RecordingLauncher;
use crate::plan::resolve;
use crate::profile::{Agent, Topology};
use crate::test_support::{
    AIMED_HERE, RE_AIMED, ROOT, dry_run_double, env, past_text_file_busy, scratch,
};

fn chained(agent: Agent) -> Profile {
    Profile {
        topology: Topology::Chained,
        ..Profile::new(agent, ROOT)
    }
}

fn planned(env: &EnvMap, profile: Profile, argv: &[&str]) -> RelayLaunch {
    let resolution = resolve(env, "work", profile).expect("the fixture resolves");
    plan(
        &resolution,
        env,
        RELAY_PROGRAM,
        argv.iter().map(|arg| arg.to_string()).collect(),
    )
    .expect("the fixture plans")
}

/// The chained child is Relay running the agent — and the client's own argv
/// arrives after the separator, untouched.
///
/// The `--` is asserted by position rather than by `contains`: Relay's
/// `RunCommand::command` is `#[arg(last = true)]`, so a launch missing it has
/// clap reading the agent's flags as Relay's and failing before the gateway
/// binds.
#[test]
fn the_chained_child_is_relay_running_the_agent_with_the_operators_argv_after_the_separator() {
    let launch = planned(&env(&[]), chained(Agent::Claude), &["-p", "hello"]);
    assert_eq!(launch.plan.program, RELAY_PROGRAM);
    assert_eq!(
        launch.plan.argv,
        [
            "run",
            "--agent",
            "claude",
            "--config",
            &launch.config.display().to_string(),
            "--",
            "-p",
            "hello"
        ]
    );
}

/// R-D′ as a value: the environment Relay is handed is the one `topham launch`
/// would have handed the client directly.
///
/// Compared against `launch::layered`'s own output rather than against a
/// transcribed list, so a variable added to the generated map is in both or in
/// neither — which is the property, not the current contents.
#[test]
fn the_chained_environment_is_the_direct_one() {
    let env = env(&[("EDITOR", "vi")]);
    let resolution = resolve(&env, "work", chained(Agent::Claude)).expect("resolves");
    let (direct, _) = crate::launch::layered(&resolution, &env);
    let launch = planned(&env, chained(Agent::Claude), &[]);
    assert_eq!(launch.plan.env, direct);
}

/// The generated config is written into the profile's own scratch, and it is
/// the library's rendering rather than anything spelled in this crate.
#[test]
fn the_config_written_is_the_shared_rendering_in_the_profiles_scratch() {
    let launch = planned(&env(&[]), chained(Agent::Claude), &[]);
    assert_eq!(
        launch.config,
        PathBuf::from("/op/data/topham/work/relay/relay-config.toml")
    );
    let (path, contents) = launch
        .plan
        .files
        .iter()
        .find(|(path, _)| path == &launch.config)
        .expect("the relay config is one of the files a launch writes");
    assert_eq!(path, &launch.config);
    assert_eq!(
        contents,
        &RelayHandoff::for_claude(ROOT, "claude")
            .unwrap()
            .config_toml()
    );
}

/// The codex half of the rendering, and the two files that still go with it.
///
/// The generated `config.toml` and catalog are written even though Relay
/// overrides codex's provider selection on the argv (see the module doc): they
/// are what a `topham launch` of the same profile would write, and a chained
/// scratch that differed from the direct one would make the two topologies
/// impossible to compare by hand.
#[test]
fn a_chained_codex_profile_renders_the_prefixed_openai_upstream() {
    let launch = planned(&env(&[]), chained(Agent::Codex), &[]);
    assert_eq!(launch.plan.argv[2], "codex");
    let contents = &launch
        .plan
        .files
        .iter()
        .find(|(path, _)| path == &launch.config)
        .expect("the relay config")
        .1;
    assert!(
        contents.contains("openai_base_url = \"http://127.0.0.1:8080/v1\""),
        "{contents}"
    );
    assert!(contents.contains("[agents.codex]"), "{contents}");
    assert_eq!(launch.plan.files.len(), 3, "{:?}", launch.plan.files);
}

/// A Direct profile is refused here for the mirror of the reason a Chained one
/// is refused by `topham launch`: the topology is a property of the profile, and
/// running the other one produces a working session that is not the one the
/// profile describes.
#[test]
fn a_direct_profile_is_refused_naming_the_other_subcommand() {
    let env = env(&[]);
    let resolution = resolve(&env, "work", Profile::new(Agent::Claude, ROOT)).expect("resolves");
    let error = plan(&resolution, &env, RELAY_PROGRAM, Vec::new()).unwrap_err();
    assert!(
        matches!(error, RelayError::WrongTopology { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("topham launch work"), "{error}");
}

/// The refusal the preflight structurally cannot make.
///
/// `NEMO_RELAY_ANTHROPIC_BASE_URL` layers *above* `--config`, and the preflight
/// clears the environment on purpose — so an ambient one would re-aim the real
/// launch while the probe reported everything fine. Checked against the captured
/// map instead, the same shape as the client's own `must_be_unset`.
#[test]
fn an_ambient_relay_upstream_variable_is_refused_before_anything_is_written() {
    let env = env(&[("NEMO_RELAY_ANTHROPIC_BASE_URL", "http://10.0.0.9:8080")]);
    let resolution = resolve(&env, "work", chained(Agent::Claude)).expect("resolves");
    let error = plan(&resolution, &env, RELAY_PROGRAM, Vec::new()).unwrap_err();
    match error {
        RelayError::UpstreamOverriddenByEnv { name, wanted, .. } => {
            assert_eq!(name, "NEMO_RELAY_ANTHROPIC_BASE_URL");
            assert_eq!(wanted, ROOT);
        }
        other => panic!("{other:?}"),
    }
}

/// …and the same variable set to the value this launch already wants is not a
/// refusal. Without this control the rule above would read as "any Relay
/// variable is fatal", which would refuse an operator who exported the right
/// thing.
#[test]
fn the_same_upstream_in_the_environment_is_not_a_refusal() {
    let env = env(&[("NEMO_RELAY_ANTHROPIC_BASE_URL", ROOT)]);
    let resolution = resolve(&env, "work", chained(Agent::Claude)).expect("resolves");
    assert!(plan(&resolution, &env, RELAY_PROGRAM, Vec::new()).is_ok());
}

/// A codex profile reads the *other* variable — the one whose name a Claude
/// profile would ignore.
#[test]
fn a_codex_profile_watches_the_openai_upstream_variable() {
    let env = env(&[("NEMO_RELAY_OPENAI_BASE_URL", "http://10.0.0.9:8080/v1")]);
    let resolution = resolve(&env, "work", chained(Agent::Codex)).expect("resolves");
    let error = plan(&resolution, &env, RELAY_PROGRAM, Vec::new()).unwrap_err();
    match error {
        RelayError::UpstreamOverriddenByEnv { name, wanted, .. } => {
            assert_eq!(name, "NEMO_RELAY_OPENAI_BASE_URL");
            assert_eq!(wanted, format!("{ROOT}/v1"));
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The preflight, against a dry-run double
// ---------------------------------------------------------------------------

/// The control: a double that reports this launch's own upstream is accepted,
/// and the argv it saw is the `--dry-run` form.
///
/// Without this control the refusal below could pass on a preflight that
/// refuses everything, which would be a launcher nobody could use.
#[test]
fn a_preflight_that_resolves_this_upstream_passes_and_ran_a_dry_run() {
    let dir = scratch("preflight-ok");
    let double = dry_run_double(&dir, AIMED_HERE);
    let handoff = RelayHandoff::for_claude(ROOT, "claude").unwrap();
    let config = dir.join("relay-config.toml");
    std::fs::write(&config, handoff.config_toml()).expect("the config");

    let report = past_text_file_busy(|| {
        preflight(
            &double.display().to_string(),
            &handoff,
            &config,
            &dir,
            "/usr/bin:/bin",
        )
    })
    .expect("the double reports this launch's upstream");
    assert!(report.contains("anthropic_base_url = http://127.0.0.1:8080"));

    let argv = std::fs::read_to_string(dir.join("argv")).expect("the double recorded its argv");
    assert!(argv.contains("--dry-run"), "{argv}");
    assert!(argv.contains("--agent claude"), "{argv}");
    assert!(
        !argv.contains(" -- "),
        "a preflight must not carry an agent argv: {argv}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// **The refusal.** A double that reports a different upstream is a system
/// Relay layer re-aiming the run, and the message has to name the file — an
/// operator cannot find `/etc/nemo-relay/config.toml` from "the preflight
/// failed".
#[test]
fn a_preflight_that_resolves_elsewhere_refuses_naming_the_system_config() {
    let dir = scratch("preflight-reaimed");
    let double = dry_run_double(&dir, RE_AIMED);
    let handoff = RelayHandoff::for_claude(ROOT, "claude").unwrap();
    let config = dir.join("relay-config.toml");
    std::fs::write(&config, handoff.config_toml()).expect("the config");

    let error = past_text_file_busy(|| {
        preflight(
            &double.display().to_string(),
            &handoff,
            &config,
            &dir,
            "/usr/bin:/bin",
        )
    })
    .unwrap_err();
    assert!(matches!(error, RelayError::ReAimed(_)), "{error:?}");
    let message = error.to_string();
    assert!(message.contains(RELAY_SYSTEM_CONFIG), "{message}");
    assert!(message.contains("http://10.0.0.9:8080"), "{message}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The isolation, read off the child rather than asserted about the code.
///
/// F7's finding was that a probe inheriting the operator's environment checks a
/// resolution the real launch does not have. The double dumps its own `env`, so
/// a dropped `env_clear()` shows up as a variable this test set in its own
/// process and never passed as isolation.
#[test]
fn the_preflight_child_sees_only_path_home_and_the_four_xdg_variables() {
    let dir = scratch("preflight-isolation");
    let double = dry_run_double(&dir, AIMED_HERE);
    let handoff = RelayHandoff::for_claude(ROOT, "claude").unwrap();
    let config = dir.join("relay-config.toml");
    std::fs::write(&config, handoff.config_toml()).expect("the config");

    past_text_file_busy(|| {
        preflight(
            &double.display().to_string(),
            &handoff,
            &config,
            &dir,
            "/usr/bin:/bin",
        )
    })
    .expect("the control double");

    let seen = std::fs::read_to_string(dir.join("env")).expect("the double recorded its env");
    let mut names: Vec<&str> = seen
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, _)| name)
        // `sh` exports its own bookkeeping (PWD, SHLVL, `_`), which is the
        // shell's rather than an inherited variable. Naming them here rather
        // than loosening the assertion keeps a *real* leak visible.
        .filter(|name| !matches!(*name, "PWD" | "SHLVL" | "_"))
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "HOME",
            "PATH",
            "XDG_CACHE_HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME"
        ],
        "F7: the preflight leaked the operator's environment, so what it \
         resolved is not what the launch will:\n{seen}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A double standing in for a Relay that refuses to run at all: bad config,
/// an unrecognized `--dry-run`, a version refusal. It writes its diagnostic to
/// stderr — the way a real CLI failure would — and exits non-zero, printing
/// nothing on stdout.
fn failing_dry_run_double(dir: &Path, stderr_message: &str, exit_code: i32) -> PathBuf {
    let path = dir.join("nemo-relay-failing.sh");
    let mut file = std::fs::File::create(&path).expect("the double");
    write!(
        file,
        "#!/bin/sh\necho '{stderr_message}' >&2\nexit {exit_code}\n"
    )
    .expect("the double's body");
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("the double is executable");
    }
    path
}

/// F11: a Relay that fails outright — non-zero exit, its diagnostic on
/// stderr, nothing on stdout — must not be reported as a system layer
/// re-aiming the run, and Relay's own words must reach the operator.
///
/// `preflight` reads only `output.stdout` and never inspects `output.status`,
/// so an empty stdout here reads exactly like a healthy Relay whose
/// `--dry-run` report happened to omit the upstream line: `verify_resolved`
/// returns `UpstreamReAimed { resolved: "(absent)", .. }`, the system-config
/// paragraph is printed, and the actual failure (this double's stderr) is
/// thrown away entirely.
///
/// The second assertion pins a compounding defect in the same code path:
/// `RelayError::ReAimed`'s own Display is `"preflight: {0}"`, which is a
/// *different* string from the source `UpstreamReAimed`'s Display even
/// though both carry the same underlying report — so `cli::error_chain`'s
/// consecutive-dedupe (`messages.last() != Some(&message)`) does not
/// collapse them, and the misreported message prints twice.
#[test]
fn a_failing_relay_reports_its_own_diagnostic_not_a_system_reaim() {
    let dir = scratch("preflight-failing");
    let stderr_message = "nemo-relay: unrecognized argument --dry-run";
    let double = failing_dry_run_double(&dir, stderr_message, 2);
    let handoff = RelayHandoff::for_claude(ROOT, "claude").unwrap();
    let config = dir.join("relay-config.toml");
    std::fs::write(&config, handoff.config_toml()).expect("the config");

    let error = past_text_file_busy(|| {
        preflight(
            &double.display().to_string(),
            &handoff,
            &config,
            &dir,
            "/usr/bin:/bin",
        )
    })
    .unwrap_err();

    assert!(
        !matches!(error, RelayError::ReAimed(_)),
        "F11: a Relay that exited non-zero with empty stdout is misreported as a system \
         Relay layer re-aiming the run: {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains(stderr_message),
        "F11: Relay's own diagnostic on stderr was discarded -- got: {message}"
    );

    let chain = crate::cli::error_chain(&error);
    assert_eq!(
        chain.len(),
        1,
        "F11: RelayError::ReAimed's 'preflight: {{0}}' prefix defeats error_chain's \
         consecutive dedupe, so the misreported message prints twice: {chain:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The whole subcommand against the double: the config lands, the preflight
/// runs, the banner names both, and the recorded plan is what would have been
/// exec'd.
#[test]
fn the_subcommand_writes_preflights_and_hands_the_launcher_the_relay_command() {
    let dir = scratch("relay-run");
    let double = dry_run_double(&dir, AIMED_HERE);
    let data_home = dir.display().to_string();
    let env = env(&[("XDG_DATA_HOME", &data_home), ("PATH", "/usr/bin:/bin")]);
    let launcher = RecordingLauncher::new();
    let mut out = Vec::new();

    let launch = past_text_file_busy(|| {
        run(
            &env,
            "work",
            chained(Agent::Claude),
            &double.display().to_string(),
            vec!["-p".into(), "hello".into()],
            &launcher,
            &mut out,
        )
    })
    .expect("the double reports this launch's upstream");

    assert_eq!(
        std::fs::read_to_string(&launch.config).expect("the config was written"),
        RelayHandoff::for_claude(ROOT, "claude")
            .unwrap()
            .config_toml()
    );
    let banner = String::from_utf8(out).expect("utf-8");
    assert!(
        banner.contains(&launch.config.display().to_string()),
        "{banner}"
    );
    assert!(banner.contains(RELAY_SYSTEM_CONFIG), "{banner}");
    assert!(banner.contains("gateway_url"), "{banner}");

    let launched = launcher.launched();
    assert_eq!(launched.len(), 1);
    assert_eq!(launched[0].program, double.display().to_string());
    assert_eq!(launched[0].argv, launch.plan.argv);
    assert_eq!(
        launched[0].argv.last().map(String::as_str),
        Some("hello"),
        "{:?}",
        launched[0].argv
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Nothing is exec'd when the preflight refuses — the launcher records no plan
/// at all. The write already happened by then, which is deliberate and stated:
/// the preflight resolves *that file*, so the file has to exist first.
#[test]
fn a_refused_preflight_launches_nothing() {
    let dir = scratch("relay-refused");
    let double = dry_run_double(&dir, RE_AIMED);
    let data_home = dir.display().to_string();
    let env = env(&[("XDG_DATA_HOME", &data_home), ("PATH", "/usr/bin:/bin")]);
    let launcher = RecordingLauncher::new();
    let mut out = Vec::new();

    let error = past_text_file_busy(|| {
        run(
            &env,
            "work",
            chained(Agent::Claude),
            &double.display().to_string(),
            Vec::new(),
            &launcher,
            &mut out,
        )
    })
    .unwrap_err();
    assert!(matches!(error, RelayError::ReAimed(_)), "{error:?}");
    assert!(launcher.launched().is_empty());
    assert!(out.is_empty(), "no banner for a launch that did not happen");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The plan's chained-codex note, which is the only place an operator is told
/// that Relay replaces codex's provider. Asserted because it is the deliverable
/// for a path no test in this tree can prove.
#[test]
fn a_chained_codex_plan_states_the_provider_override() {
    let env = env(&[]);
    let resolution = resolve(&env, "work", chained(Agent::Codex)).expect("resolves");
    let notes = resolution.notes().join("\n");
    assert!(notes.contains("nemo-relay-openai"), "{notes}");
    assert!(notes.contains("local-only"), "{notes}");

    // …and the Claude side does not carry it, because the finding is not about
    // that client.
    let resolution = resolve(&env, "work", chained(Agent::Claude)).expect("resolves");
    assert!(
        !resolution.notes().join("\n").contains("nemo-relay-openai"),
        "{:?}",
        resolution.notes()
    );
}

/// The argv builders are the agent's, not this module's — the one place a
/// chained launch could disagree with the rig about how Relay is invoked.
#[test]
fn the_launch_argv_comes_from_the_shared_agent_form() {
    let launch = planned(&env(&[]), chained(Agent::Claude), &["-p", "x"]);
    let expected = RelayAgent::Claude.run_argv(&launch.config);
    assert_eq!(&launch.plan.argv[..expected.len()], expected.as_slice());
}

/// F15's claim: `scratch_dir`'s `.parent().expect("codex_home is
/// <data>/topham/<name>/codex-home")` is the only thing standing between a
/// `profile.rs` layout change and a caught defect, so that change is "caught
/// by nothing except the panic at relay.rs:239".
///
/// It is not reachable at all through this crate's own API, which is the
/// correction: `Profile::codex_home` is `data_home().join("topham").join(name)
/// .join("codex-home")` — three non-empty `.join()`s onto a base
/// `check_name` and `env::data_home` never let through empty — so the
/// resulting path always has at least four components and `.parent()` always
/// returns `Some`. This drives `XDG_DATA_HOME` and the profile name down to
/// the shortest strings the crate's own validation still accepts (`"/"`,
/// `"a"`) and shows the `.expect()` still does not fire — a real layout
/// change (say, `codex_home` dropping the `<name>` segment) would not trip
/// this panic either; `.parent()` would still return `Some`, just of the
/// *wrong* directory, silently. The panic path F15 points at is not what
/// would catch that drift.
///
/// The walk itself is gone now — `scratch_dir` joins onto
/// [`Profile::scratch_root`] — so what this test still holds is the shortest
/// input the crate accepts resolving to a whole path and not to a panic or a
/// bare root, which is the property the removed `.expect()` was standing in for.
#[test]
fn scratch_dir_does_not_panic_even_at_the_shortest_path_the_crate_accepts() {
    let env = EnvMap::from([("XDG_DATA_HOME".to_string(), "/".to_string())]);
    let outcome = std::panic::catch_unwind(|| scratch_dir(&env, "a"));
    let dir = outcome
        .expect(
            "F15: resolving the chained scratch panicked on the shortest data home \
                 and profile name this crate's own validation can produce",
        )
        .expect("scratch_dir resolves data_home");
    assert_eq!(dir, PathBuf::from("/topham/a/relay"));
}

/// The relationship the `.expect()` string only *describes*: the chained
/// scratch and the generated `CODEX_HOME` are siblings under one per-profile
/// root, so two profiles can never share a Relay config.
///
/// This is what F15's refutation showed was actually at risk. The panic the
/// finding pointed at is unreachable through the crate's own validated inputs;
/// what a change to `Profile::codex_home`'s layout really does is leave
/// `.parent()` returning `Some` of the *wrong* directory, silently. The
/// remaining half of the fix — [`Profile::scratch_root`], a named per-profile
/// root both sides now derive from, so the two cannot disagree by construction
/// — has landed; this is what goes red if either side is ever re-derived by
/// hand instead.
#[test]
fn the_relay_scratch_is_a_sibling_of_the_codex_home_under_one_profile_root() {
    let env = env(&[]);
    let codex_home = Profile::codex_home(&env, "work").expect("the fixture resolves a data home");
    let relay = scratch_dir(&env, "work").expect("and the scratch beside it");

    assert_eq!(
        relay.parent(),
        codex_home.parent(),
        "one profile, one root: the chained scratch and the codex home are siblings"
    );
    assert_eq!(
        relay.parent().and_then(|root| root.file_name()),
        Some(std::ffi::OsStr::new("work")),
        "and that root is named for the profile, which is what keeps two profiles from \
         sharing one Relay upstream"
    );
}
