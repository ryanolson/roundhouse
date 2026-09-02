// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What the child process is, and what never becomes one.
//!
//! Nothing here spawns anything: [`RecordingLauncher`] stands where the `exec`
//! is, so the assertions are about the plan a real launch would have handed the
//! kernel. That is the whole reason the seam exists — a launcher's success case
//! is that it never returns, which is not a thing a unit test can observe.
//!
//! Nothing here writes to the test process's environment either. Every refusal
//! is a function of an [`EnvMap`] built in the test, which is what keeps this
//! suite off the single-threaded `unsafe` discipline `claude_e2e.rs` has to
//! observe to mutate a real one.

use super::*;
use crate::plan::resolve;
use crate::profile::{Agent, AuthKind};
use crate::test_support::{ROOT, TURN_KEY, env, scratch};

fn profile(agent: Agent, auth: AuthKind) -> Profile {
    Profile {
        auth,
        ..Profile::new(agent, ROOT)
    }
}

fn planned(env: &EnvMap, agent: Agent, auth: AuthKind, argv: &[&str]) -> LaunchPlan {
    let resolution = resolve(env, "work", profile(agent, auth)).expect("the fixture resolves");
    plan(
        &resolution,
        env,
        argv.iter().map(|arg| arg.to_string()).collect(),
    )
    .expect("the fixture plans")
}

/// R-T7's layering, in both directions at once.
///
/// The two halves are one test because they are one property: the child's
/// environment is the operator's with the generated variables *on top*. Split
/// apart, a launcher that replaced the environment entirely would pass the
/// first half, and one that never applied the generated map would pass the
/// second.
#[test]
fn a_generated_variable_wins_and_an_unrelated_ambient_one_survives() {
    let env = env(&[
        // Left over from another deployment: the exact value a launch must not
        // inherit, because the client would post this profile's turns there.
        ("ANTHROPIC_BASE_URL", "https://api.anthropic.com"),
        // The operator's own, which has nothing to do with any of this.
        ("EDITOR", "vi"),
    ]);
    let plan = planned(&env, Agent::Claude, AuthKind::RoundhouseKey, &[]);

    assert_eq!(
        plan.env.get("ANTHROPIC_BASE_URL").map(String::as_str),
        Some(ROOT),
        "the generated map is layered over the ambient one, not under it"
    );
    assert_eq!(plan.env.get("EDITOR").map(String::as_str), Some("vi"));
    assert_eq!(
        plan.env.get("PATH").map(String::as_str),
        Some("/usr/bin:/bin")
    );
    assert_eq!(
        plan.env.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some(roundhouse_server::claude_launch::ROUNDHOUSE_API_KEY_SENTINEL),
        "a RoundhouseKey launch writes the sentinel, which is what suppresses an ambient login"
    );
    assert_eq!(
        plan.env.get("ANTHROPIC_CUSTOM_HEADERS").map(String::as_str),
        Some(format!("x-roundhouse-key: {TURN_KEY}").as_str()),
        "the one place in this crate that reveals the key is the map handed to the child"
    );
}

/// `claude_launch`'s own doc assigns these two to whoever spawns the process,
/// and here that is this launcher. See [`CLAUDE_DEPLOYMENT_POLICY`].
#[test]
fn a_claude_child_is_given_the_deployment_policy_variables() {
    let plan = planned(&env(&[]), Agent::Claude, AuthKind::RoundhouseKey, &[]);
    assert_eq!(
        plan.env.get("DISABLE_AUTOUPDATER").map(String::as_str),
        Some("1"),
        "an autoupdate mid-session swaps the binary whose wire behaviour this deployment's \
         dialect was verified against"
    );
    assert_eq!(
        plan.env
            .get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC")
            .map(String::as_str),
        Some("1")
    );
}

/// The argv is the operator's, verbatim, and nothing is invented.
#[test]
fn the_argv_is_passed_through_and_the_program_is_the_agents() {
    let plan = planned(
        &env(&[]),
        Agent::Claude,
        AuthKind::RoundhouseKey,
        &["-p", "hello --not-a-topham-flag"],
    );
    assert_eq!(plan.program, "claude");
    assert_eq!(plan.argv, vec!["-p", "hello --not-a-topham-flag"]);

    let bare = planned(&env(&[]), Agent::Claude, AuthKind::RoundhouseKey, &[]);
    assert!(
        bare.argv.is_empty(),
        "`topham launch <profile>` opens the agent's own interactive session, so there is no \
         default argument to invent"
    );
}

/// R-T4: the refusal happens before anything is spawned or written.
///
/// The launcher is asserted *not* to have been called, which is the assertion
/// that would still fail if the check were moved after the exec — where it
/// would be unreachable.
#[test]
fn an_ambient_suppressor_refuses_before_the_launcher_is_reached() {
    let env = env(&[("CLAUDE_CODE_USE_BEDROCK", "1")]);
    let launcher = RecordingLauncher::new();
    let error = run(
        &env,
        "work",
        profile(Agent::Claude, AuthKind::RoundhouseKey),
        Vec::new(),
        &launcher,
    )
    .expect_err("a client that selects Bedrock never reads the base URL at all");
    assert!(
        error.to_string().contains("CLAUDE_CODE_USE_BEDROCK"),
        "{error}"
    );
    assert!(
        launcher.launched().is_empty(),
        "nothing may be spawned once a refusal is known"
    );
}

/// A chained profile is refused by the Direct entry point, by name.
#[test]
fn launch_refuses_a_chained_profile_and_names_the_other_subcommand() {
    let launcher = RecordingLauncher::new();
    let profile = Profile {
        topology: Topology::Chained,
        ..profile(Agent::Claude, AuthKind::RoundhouseKey)
    };
    let error = run(&env(&[]), "work", profile, Vec::new(), &launcher)
        .expect_err("a chained profile is instrumented by a Relay this subcommand does not start");
    assert!(
        matches!(error, LaunchError::WrongTopology { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("topham relay work"), "{error}");
    assert!(launcher.launched().is_empty());
}

/// The codex half: two files under the profile's own `CODEX_HOME`, the variable
/// that points the client at them, and the key the generated config names.
#[test]
fn a_codex_launch_writes_its_two_files_and_points_the_client_at_them() {
    let root = scratch("codex-launch");
    let mut env = env(&[]);
    env.insert(
        "XDG_DATA_HOME".to_string(),
        root.join("data").display().to_string(),
    );

    let launcher = RecordingLauncher::new();
    run(
        &env,
        "work",
        profile(Agent::Codex, AuthKind::RoundhouseKey),
        vec!["exec".to_string()],
        &launcher,
    )
    .expect("a codex launch");

    let home = root.join("data/topham/work/codex-home");
    let config = std::fs::read_to_string(home.join("config.toml")).expect("the generated config");
    let catalog =
        std::fs::read_to_string(home.join("model-catalog.json")).expect("the generated catalog");
    assert!(config.contains("[model_providers.roundhouse]"), "{config}");
    assert!(
        config.contains("env_key = \"ROUNDHOUSE_API_KEY\""),
        "the generated config names the variable rather than holding a key:\n{config}"
    );
    assert!(
        !config.contains(TURN_KEY) && !catalog.contains(TURN_KEY),
        "no secret may reach a file -- that rule is `codex_launch`'s and this is its launcher"
    );

    let [plan] = launcher.launched().try_into().expect("exactly one launch");
    assert_eq!(plan.program, "codex");
    assert_eq!(plan.argv, vec!["exec"]);
    assert_eq!(
        plan.env.get(CODEX_HOME_ENV).map(String::as_str),
        Some(home.display().to_string().as_str()),
        "per profile, so two profiles never share the auth.json a `codex login` writes"
    );
    assert_eq!(
        plan.env.get("ROUNDHOUSE_API_KEY").map(String::as_str),
        Some(TURN_KEY),
        "the generator never sees the key; putting it in the child's environment under the name \
         the config gives is this launcher's half of that rule"
    );
}

/// Writing is idempotent, because a profile is the source of truth.
#[test]
fn a_second_launch_overwrites_what_the_first_one_generated() {
    let root = scratch("codex-rewrite");
    let mut env = env(&[]);
    env.insert(
        "XDG_DATA_HOME".to_string(),
        root.join("data").display().to_string(),
    );
    let home = root.join("data/topham/work/codex-home");
    let launcher = RecordingLauncher::new();

    run(
        &env,
        "work",
        profile(Agent::Codex, AuthKind::RoundhouseKey),
        Vec::new(),
        &launcher,
    )
    .unwrap();
    std::fs::write(home.join("config.toml"), "edited by hand\n").unwrap();
    run(
        &env,
        "work",
        profile(Agent::Codex, AuthKind::RoundhouseKey),
        Vec::new(),
        &launcher,
    )
    .unwrap();

    let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(
        config.contains("[model_providers.roundhouse]"),
        "an edit to a generated file lasts one run: the profile is what is edited instead"
    );
}

/// F21: a concurrent second launch must not truncate a `config.toml` a
/// just-exec'd client may still have open. An atomic write-then-rename
/// replaces the inode; an in-place `O_TRUNC` write keeps it. Two sequential
/// `run()`s stand in for the race here -- the property under test is what
/// `write_files` *does* to the file, not the interleaving -- and is enough to
/// tell the two mechanisms apart.
#[test]
fn a_second_launch_replaces_the_config_file_rather_than_truncating_it_in_place() {
    use std::os::unix::fs::MetadataExt;

    let root = scratch("codex-inode");
    let mut env = env(&[]);
    env.insert(
        "XDG_DATA_HOME".to_string(),
        root.join("data").display().to_string(),
    );
    let home = root.join("data/topham/work/codex-home");
    let launcher = RecordingLauncher::new();

    run(
        &env,
        "work",
        profile(Agent::Codex, AuthKind::RoundhouseKey),
        Vec::new(),
        &launcher,
    )
    .unwrap();
    let first_ino = std::fs::metadata(home.join("config.toml"))
        .expect("the first launch's config")
        .ino();

    run(
        &env,
        "work",
        profile(Agent::Codex, AuthKind::RoundhouseKey),
        Vec::new(),
        &launcher,
    )
    .unwrap();
    let second_ino = std::fs::metadata(home.join("config.toml"))
        .expect("the second launch's config")
        .ino();

    assert_ne!(
        first_ino, second_ino,
        "an atomic write-then-rename replaces the inode a concurrent reader's fd points at; an \
         in-place O_TRUNC write leaves the first launch's just-exec'd client reading a file that \
         is being truncated out from under it"
    );
}

/// A Claude launch writes nothing, and that is the shape of that client's
/// whole redirect surface.
#[test]
fn a_claude_launch_writes_no_files_at_all() {
    let plan = planned(&env(&[]), Agent::Claude, AuthKind::RoundhouseKey, &[]);
    assert!(plan.files.is_empty());
    assert!(
        !plan.env.contains_key(CODEX_HOME_ENV),
        "nothing about a Claude launch involves a CODEX_HOME"
    );
}
