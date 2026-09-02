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

// ---------------------------------------------------------------------------
// M12 R-M3/R-M4: the generated argv
// ---------------------------------------------------------------------------

/// The child gets the generated arguments first and the operator's last, and
/// the two are separate values right up to the `exec`.
///
/// The order is the whole of what makes the operator's argv the last word: a
/// client resolving two of anything takes them in the order it was handed them,
/// and this launcher's own flags are the ones that should lose an argument
/// nobody refused.
#[test]
fn the_generated_argv_leads_and_the_operators_follows() {
    let plan = planned(
        &env(&[]),
        Agent::Claude,
        AuthKind::RoundhouseKey,
        &["-p", "hello"],
    );
    assert_eq!(plan.argv, vec!["-p", "hello"]);
    assert_eq!(
        plan.generated_argv,
        resolve(
            &env(&[]),
            "work",
            profile(Agent::Claude, AuthKind::RoundhouseKey)
        )
        .map(|resolution| match resolution.resolved {
            Resolved::Claude { launch, .. } => flatten_argv(&launch.leading_argv()),
            Resolved::Codex { .. } => unreachable!("this fixture is a claude profile"),
        })
        .expect("the fixture resolves"),
        "the argv a launch passes is the one the dry run resolved, not a second \
         derivation"
    );
    assert_eq!(
        plan.full_argv(),
        [plan.generated_argv.clone(), plan.argv.clone()].concat()
    );
    assert_eq!(
        plan.full_argv()[..2],
        ["--mcp-config", &plan.generated_argv[1]],
        "the registration leads: {:?}",
        plan.full_argv()
    );
}

/// A codex launch generates no argv, because that client is configured by the
/// files beside it.
#[test]
fn a_codex_launch_generates_no_argv() {
    let plan = planned(
        &env(&[]),
        Agent::Codex,
        AuthKind::RoundhouseKey,
        &["exec", "hello"],
    );
    assert!(plan.generated_argv.is_empty(), "{:?}", plan.generated_argv);
    assert_eq!(plan.full_argv(), vec!["exec", "hello"]);
}

/// R-M3: the profile's switch is what puts `--strict-mcp-config` on the argv,
/// and nothing else does.
#[test]
fn the_strict_switch_is_the_profiles_and_defaults_off() {
    let ordinary = planned(&env(&[]), Agent::Claude, AuthKind::RoundhouseKey, &[]);
    assert!(
        !ordinary
            .generated_argv
            .iter()
            .any(|a| a == "--strict-mcp-config"),
        "an operator's own MCP servers survive a launch that did not ask to \
         exclude them: {:?}",
        ordinary.generated_argv
    );

    let env = env(&[]);
    let strict = Profile {
        strict_mcp: true,
        ..profile(Agent::Claude, AuthKind::RoundhouseKey)
    };
    let resolution = resolve(&env, "work", strict).expect("the fixture resolves");
    let plan = plan(&resolution, &env, Vec::new()).expect("the fixture plans");
    assert_eq!(
        plan.generated_argv
            .iter()
            .filter(|a| *a == "--strict-mcp-config")
            .count(),
        1,
        "{:?}",
        plan.generated_argv
    );
}

/// The key is in the environment and in nothing else a launch produces.
///
/// **Both halves matter and they are opposite failures.** The argv is world-
/// readable in a process listing, so a key there is a key every other user of
/// the box has; the plan rendering is printed, pasted into issues and
/// screen-shared. The registration's `${…}` is what buys both, and this is
/// what would catch it being expanded here rather than by the client.
#[test]
fn the_key_variables_value_is_in_no_argv_and_no_rendering() {
    let env = env(&[]);
    let resolution = resolve(
        &env,
        "work",
        profile(Agent::Claude, AuthKind::RoundhouseKey),
    )
    .expect("the fixture resolves");
    let plan = plan(&resolution, &env, Vec::new()).expect("the fixture plans");
    for argument in plan.full_argv() {
        assert!(
            !argument.contains(TURN_KEY),
            "the turn key is in the child's argv, where every process listing \
             on the box can read it: {argument}"
        );
    }
    assert!(
        !resolution.render().contains(TURN_KEY),
        "{}",
        resolution.render()
    );
    // The control: the registration does name the variable, unexpanded, in
    // both places -- so the assertions above are about the value and not about
    // the registration having quietly gone missing.
    let variable = format!("${{{}}}", resolution.profile.key_env);
    assert!(
        plan.full_argv().iter().any(|a| a.contains(&variable)),
        "{:?}",
        plan.full_argv()
    );
    assert!(resolution.render().contains(&variable));
    // And the value really is somewhere: in the environment, which is the one
    // place both the header block and the registration read it from.
    assert!(
        plan.env
            .get("ANTHROPIC_CUSTOM_HEADERS")
            .is_some_and(|value| value.contains(TURN_KEY))
    );
}

/// The registration is refused along with everything else when the key
/// variable is not exported.
///
/// This is what closes R-M3's one hazard: an unexported variable would make the
/// client send the literal `${…}` as a turn key, and every control call would
/// come back `401` on a run whose inference turns all answered. The refusal
/// that already existed covers it, and this is the test that says so — if
/// resolution ever stopped requiring the key, the registration would be the
/// thing that failed silently.
#[test]
fn an_unexported_key_variable_refuses_the_launch_that_would_carry_a_literal() {
    let launcher = RecordingLauncher::new();
    let without = EnvMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
    let error = run(
        &without,
        "work",
        profile(Agent::Claude, AuthKind::RoundhouseKey),
        Vec::new(),
        &launcher,
    )
    .expect_err("a launch with no key exported is refused before anything is spawned");
    assert!(
        error.to_string().contains("ROUNDHOUSE_API_KEY"),
        "the refusal names the variable the registration would have read: {error}"
    );
    assert!(launcher.launched().is_empty());
}

/// An operator argv naming a flag this launch generates is refused, not
/// silently outranked.
#[test]
fn an_operator_flag_this_launch_also_generates_is_refused() {
    for argument in [
        "--mcp-config",
        "--mcp-config={\"mcpServers\":{}}",
        "--append-system-prompt",
    ] {
        let env = env(&[]);
        let resolution = resolve(
            &env,
            "work",
            profile(Agent::Claude, AuthKind::RoundhouseKey),
        )
        .expect("the fixture resolves");
        let error = plan(&resolution, &env, vec![argument.to_string()])
            .expect_err("two answers to one question is a refusal");
        assert!(
            matches!(error, LaunchError::ArgvCollidesWithGenerated { .. }),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("--mcp-config")
                || error.to_string().contains("--append-system-prompt"),
            "the refusal names the flag to drop: {error}"
        );
    }

    // CONTROL: an ordinary flag of the client's own is not refused, and a flag
    // that merely *contains* a generated one's name is not either -- the check
    // is on the flag, not on a substring.
    let env = env(&[]);
    let resolution = resolve(
        &env,
        "work",
        profile(Agent::Claude, AuthKind::RoundhouseKey),
    )
    .expect("the fixture resolves");
    let planned = plan(
        &resolution,
        &env,
        vec![
            "--allowedTools".to_string(),
            "mcp__roundhouse__status".to_string(),
            "--mcp-config-is-not-a-flag".to_string(),
        ],
    )
    .expect("an unrelated flag is the operator's business");
    assert_eq!(planned.argv.len(), 3);
}

/// F5, closed: a generated *value* that looks like a flag is not a flag.
///
/// The check used to be a heuristic over a flat `Vec<String>`
/// (`starts_with("--") && !contains(whitespace)`), because flattening
/// `leading_argv` at the crate boundary left nothing else to go on. It read the
/// value half of a pair as a flag name and refused the operator's identical
/// bare argument, naming a flag this launch does not pass. `GeneratedArg`
/// carries the split instead, so the case below cannot be misread rather than
/// being merely unreachable today.
///
/// Unreachable is what it is today — the signage's prose has spaces in it — and
/// that is exactly why the structure matters: the guard would have gone quiet
/// again the first time a generated value was a single token.
#[test]
fn refuse_collisions_reads_a_flag_shaped_generated_value_as_a_value() {
    let generated = vec![GeneratedArg::pair("--append-system-prompt", "--x")];
    let argv = vec!["--x".to_string()];

    refuse_collisions(&generated, &argv).expect(
        "`--x` is the value paired with `--append-system-prompt`, not a flag this launch \
         generates, so an operator passing `--x` collides with nothing",
    );

    // The control, and the half that must stay refused: the *flag* of that same
    // pair is a real collision, and an `=`-joined spelling of it is the same
    // flag to the client.
    for operator in [
        vec!["--append-system-prompt".to_string()],
        vec!["--append-system-prompt=mine".to_string()],
    ] {
        let error = refuse_collisions(&generated, &operator)
            .expect_err("two answers to one question is the refusal this check exists for");
        match &error {
            LaunchError::ArgvCollidesWithGenerated { flag } => {
                assert_eq!(flag, "--append-system-prompt")
            }
            other => panic!("expected ArgvCollidesWithGenerated, got {other:?}"),
        }
    }
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
