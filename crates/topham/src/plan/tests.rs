// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a dry run says, for both agents and both auth kinds.
//!
//! **Whole-output snapshots rather than a handful of `contains` assertions.**
//! The thing being pinned is not that some line mentions the base URL; it is
//! that the operator-facing rendering of a launch does not acquire, lose or
//! reorder anything without somebody looking at the diff. A `contains` suite
//! passes a render that silently dropped the must-be-unset table, which is the
//! section whose absence costs the most.
//!
//! Two values are substituted into the expected text rather than typed:
//! the turn key's fingerprint (computed from the same [`Secret`] the generator
//! renders, so this file does not restate a digest) and nothing else. The
//! directories are *fixed literals* on purpose — resolution touches no
//! filesystem, so the fixture can name a path that does not exist and the
//! snapshot stays byte-stable across machines.

use roundhouse_core::control::Secret;

use super::*;

/// A well-shaped turn key, in the house fixture form (`tests/common`'s `key`).
const TURN_KEY: &str = "rh_turn_liveAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const ROOT: &str = "http://127.0.0.1:8080";

/// An environment with the key exported and the two homes fixed.
fn env() -> EnvMap {
    EnvMap::from([
        ("ROUNDHOUSE_API_KEY".to_string(), TURN_KEY.to_string()),
        ("XDG_CONFIG_HOME".to_string(), "/op/config".to_string()),
        ("XDG_DATA_HOME".to_string(), "/op/data".to_string()),
    ])
}

/// What the generator renders the key as, read from the type that renders it.
fn redacted_key() -> String {
    Secret::api_key(TURN_KEY)
        .expect("the fixture key is api-key shaped")
        .to_string()
}

fn profile(agent: Agent, auth: AuthKind) -> Profile {
    Profile {
        auth,
        ..Profile::new(agent, ROOT)
    }
}

fn render(agent: Agent, auth: AuthKind) -> String {
    resolve(&env(), "work", profile(agent, auth))
        .expect("the fixture resolves")
        .render()
}

#[test]
fn a_claude_bring_your_own_key_launch_renders() {
    let expected = format!(
        "\
profile         : work
agent           : claude
topology        : direct
auth            : roundhouse-key
deployment root : http://127.0.0.1:8080
turn key        : read from $ROUNDHOUSE_API_KEY (<set>)
messages url    : http://127.0.0.1:8080/v1/messages

environment handed to the client (the generator's own Debug):
    ClaudeEnv {{
        vars: {{
            \"ANTHROPIC_API_KEY\": \"rh_sentinel_not_a_credential\",
            \"ANTHROPIC_BASE_URL\": \"http://127.0.0.1:8080\",
            \"ANTHROPIC_CUSTOM_HEADERS\": \"x-roundhouse-key: {key}\",
        }},
    }}

must be unset when this launch runs:
    CLAUDE_CODE_USE_BEDROCK (environment) -- the client goes to another cloud and never reads the base URL
    CLAUDE_CODE_USE_VERTEX (environment) -- the client goes to another cloud and never reads the base URL
    CLAUDE_CODE_USE_FOUNDRY (environment) -- the client goes to another cloud and never reads the base URL
    CLAUDE_CODE_REMOTE (environment) -- an ambient login stops being suppressed and reaches this deployment

files written by `topham launch`:
    (none) -- Claude Code's whole redirect surface is environment

notes:
    - an interactive session asks once. With `-p` the client always uses the API key; interactively it asks the user to approve it overriding their subscription, and until they do that session is on the subscription.
",
        key = redacted_key()
    );
    assert_eq!(render(Agent::Claude, AuthKind::RoundhouseKey), expected);
}

#[test]
fn a_claude_forwarded_login_launch_renders() {
    let expected = format!(
        "\
profile         : work
agent           : claude
topology        : direct
auth            : forwarded-login
deployment root : http://127.0.0.1:8080
turn key        : read from $ROUNDHOUSE_API_KEY (<set>)
messages url    : http://127.0.0.1:8080/v1/messages

environment handed to the client (the generator's own Debug):
    ClaudeEnv {{
        vars: {{
            \"ANTHROPIC_BASE_URL\": \"http://127.0.0.1:8080\",
            \"ANTHROPIC_CUSTOM_HEADERS\": \"x-roundhouse-key: {key}\",
        }},
    }}

must be unset when this launch runs:
    CLAUDE_CODE_USE_BEDROCK (environment) -- the client goes to another cloud and never reads the base URL
    CLAUDE_CODE_USE_VERTEX (environment) -- the client goes to another cloud and never reads the base URL
    CLAUDE_CODE_USE_FOUNDRY (environment) -- the client goes to another cloud and never reads the base URL
    ANTHROPIC_AUTH_TOKEN (environment) -- the login this profile forwards is suppressed; every request still answers
    CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR (environment) -- the login this profile forwards is suppressed; every request still answers
    ANTHROPIC_API_KEY (environment) -- the login this profile forwards is suppressed; every request still answers
    apiKeyHelper (settings key) -- the login this profile forwards is suppressed; every request still answers

files written by `topham launch`:
    (none) -- Claude Code's whole redirect surface is environment

notes:
    - the precondition is a completed `claude` login, not this profile. Without one the client presents no credential and roundhouse degrades the turn to local-only, which nothing in the run reports.
    - one entry above lives in the client's settings file rather than the environment. This launcher reads no settings file -- which one the client resolves is its own layered search -- so that entry is stated, not enforced.
",
        key = redacted_key()
    );
    assert_eq!(render(Agent::Claude, AuthKind::ForwardedLogin), expected);
}

#[test]
fn a_codex_bring_your_own_key_launch_renders() {
    let expected = "\
profile         : work
agent           : codex
topology        : direct
auth            : roundhouse-key
deployment root : http://127.0.0.1:8080
turn key        : read from $ROUNDHOUSE_API_KEY (<set>)
responses url   : http://127.0.0.1:8080/v1/responses
mcp url         : http://127.0.0.1:8080/mcp
model slug      : roundhouse-local

environment handed to the client:
    CODEX_HOME = \"/op/data/topham/work/codex-home\"
    ROUNDHOUSE_API_KEY = \"<set>\"

must be unset when this launch runs:
    (none) -- codex resolves its credential from the config file below, which names the
    variable rather than reading an ambient one

files written by `topham launch`:
    /op/data/topham/work/codex-home/config.toml
    /op/data/topham/work/codex-home/model-catalog.json

notes:
    - the generated config names the key variable rather than holding a key, so the files above are safe to read, diff and keep.
";
    assert_eq!(render(Agent::Codex, AuthKind::RoundhouseKey), expected);
}

#[test]
fn a_codex_forwarded_login_launch_renders() {
    let expected = "\
profile         : work
agent           : codex
topology        : direct
auth            : forwarded-login
deployment root : http://127.0.0.1:8080
turn key        : read from $ROUNDHOUSE_API_KEY (<set>)
responses url   : http://127.0.0.1:8080/v1/responses
mcp url         : http://127.0.0.1:8080/mcp
model slug      : roundhouse-local

environment handed to the client:
    CODEX_HOME = \"/op/data/topham/work/codex-home\"
    ROUNDHOUSE_API_KEY = \"<set>\"

must be unset when this launch runs:
    (none) -- codex resolves its credential from the config file below, which names the
    variable rather than reading an ambient one

files written by `topham launch`:
    /op/data/topham/work/codex-home/config.toml
    /op/data/topham/work/codex-home/model-catalog.json

notes:
    - run `codex login` against this profile's CODEX_HOME (/op/data/topham/work/codex-home) first. The stanza selects a code path; the Authorization header comes from the auth.json that login writes and from nothing else.
";
    assert_eq!(render(Agent::Codex, AuthKind::ForwardedLogin), expected);
}

/// The redaction is the generator's, and this is what it is for.
///
/// Separate from the snapshots because a snapshot proves the output *is* a
/// string, and this proves the string is not the key: the two fail for
/// different reasons and a reader of the failure should not have to diff two
/// hundred characters to tell which.
#[test]
fn no_rendering_carries_the_turn_key() {
    for agent in [Agent::Claude, Agent::Codex] {
        for auth in [AuthKind::RoundhouseKey, AuthKind::ForwardedLogin] {
            let rendered = render(agent, auth);
            assert!(
                !rendered.contains(TURN_KEY),
                "`topham plan` is printed into terminals, pasted into issues and screen-shared: \
                 {agent:?}/{auth:?} rendered the key in the clear"
            );
        }
    }
}

/// A chained profile renders, and says which subcommand it belongs to.
#[test]
fn a_chained_profile_plans_and_names_the_chained_entry_point() {
    let profile = Profile {
        topology: Topology::Chained,
        ..profile(Agent::Claude, AuthKind::RoundhouseKey)
    };
    let rendered = resolve(&env(), "work", profile)
        .expect("a chained profile resolves -- only `launch` refuses it")
        .render();
    assert!(rendered.contains("topology        : chained"), "{rendered}");
    assert!(rendered.contains("`topham relay`"), "{rendered}");
}

/// R-T4's refusal, through the generator's own table rather than a copy of it.
#[test]
fn an_ambient_suppressor_refuses_the_resolution_by_name() {
    let mut env = env();
    env.insert(
        "ANTHROPIC_AUTH_TOKEN".to_string(),
        "sk-somebody".to_string(),
    );

    // The kind that forwards a login is the one this variable defeats.
    let error = resolve(
        &env,
        "work",
        profile(Agent::Claude, AuthKind::ForwardedLogin),
    )
    .expect_err("an ambient auth token turns the forwarding off silently");
    let message = error.to_string();
    assert!(
        message.contains("`ANTHROPIC_AUTH_TOKEN`"),
        "the refusal has to name the variable an operator must unset: {message}"
    );
    assert!(
        !message.contains("sk-somebody"),
        "and must not echo the credential it found: {message}"
    );

    // The other kind sets its own API key and is unaffected by this one, which
    // is what makes the check kind-dependent rather than a blanket scan.
    resolve(
        &env,
        "work",
        profile(Agent::Claude, AuthKind::RoundhouseKey),
    )
    .expect("`ANTHROPIC_AUTH_TOKEN` suppresses a login a RoundhouseKey launch does not use");
}

/// The other direction of the same table: `CLAUDE_CODE_REMOTE` is refused under
/// the kind that sets the sentinel, and admitted under the kind that does not.
#[test]
fn the_remote_variable_is_refused_under_exactly_one_kind() {
    let mut env = env();
    env.insert("CLAUDE_CODE_REMOTE".to_string(), "true".to_string());

    let error = resolve(
        &env,
        "work",
        profile(Agent::Claude, AuthKind::RoundhouseKey),
    )
    .expect_err("under CCR the sentinel suppresses nothing and the ambient login is presented");
    assert!(error.to_string().contains("CLAUDE_CODE_REMOTE"), "{error}");

    resolve(
        &env,
        "work",
        profile(Agent::Claude, AuthKind::ForwardedLogin),
    )
    .expect("a forwarded-login launch has no sentinel for this variable to defeat");
}

/// Resolution refuses a profile whose key is not exported, for both agents —
/// see the module doc on why `plan` refuses what `launch` refuses.
#[test]
fn an_unexported_turn_key_is_refused_and_names_the_variable() {
    let env = EnvMap::from([("XDG_DATA_HOME".to_string(), "/op/data".to_string())]);
    for agent in [Agent::Claude, Agent::Codex] {
        let error = resolve(&env, "work", profile(agent, AuthKind::RoundhouseKey))
            .expect_err("nothing exported the key");
        assert!(
            matches!(&error, PlanError::TurnKeyMissing { key_env } if key_env == "ROUNDHOUSE_API_KEY"),
            "{error:#?}"
        );
    }

    // An exported-but-empty variable is the same case: the shell that set it
    // meant to set it, and a client handed an empty key presents nothing.
    let mut env = env;
    env.insert("ROUNDHOUSE_API_KEY".to_string(), "  ".to_string());
    assert!(matches!(
        resolve(
            &env,
            "work",
            profile(Agent::Claude, AuthKind::RoundhouseKey)
        ),
        Err(PlanError::TurnKeyMissing { .. })
    ));
}

/// One profile field, two derivations — and each generator refuses the other's
/// shape, which is what makes storing one root safe.
#[test]
fn the_two_generators_are_handed_the_url_shape_each_one_needs() {
    let claude = resolve(
        &env(),
        "work",
        profile(Agent::Claude, AuthKind::RoundhouseKey),
    )
    .unwrap();
    let Resolved::Claude { launch, .. } = &claude.resolved else {
        panic!("a claude profile resolves to a claude launch");
    };
    assert_eq!(launch.base_url(), ROOT, "the deployment root, with no /v1");

    let codex = resolve(
        &env(),
        "work",
        profile(Agent::Codex, AuthKind::RoundhouseKey),
    )
    .unwrap();
    let Resolved::Codex { launch, .. } = &codex.resolved else {
        panic!("a codex profile resolves to a codex launch");
    };
    assert_eq!(
        launch.base_url,
        format!("{ROOT}{API_PREFIX}"),
        "codex is configured with the URL it posts to, which carries the API prefix"
    );

    // And a profile whose root already carries the prefix is refused by the
    // generator that cannot use it, rather than producing /v1/v1/messages.
    let mut wrong = profile(Agent::Claude, AuthKind::RoundhouseKey);
    wrong.deployment_root = format!("{ROOT}{API_PREFIX}");
    assert!(matches!(
        resolve(&env(), "work", wrong),
        Err(PlanError::Claude(
            roundhouse_server::ClaudeLaunchError::BaseUrlCarriesApiPrefix { .. }
        ))
    ));
}

/// A trailing slash is what a copy-pasted address carries, and it has one
/// meaning — so it is normalised rather than refused, on both arms.
#[test]
fn a_trailing_slash_on_the_deployment_root_is_normalised() {
    let mut profile = profile(Agent::Codex, AuthKind::RoundhouseKey);
    profile.deployment_root = format!("{ROOT}/");
    let resolved = resolve(&env(), "work", profile).expect("a trailing slash is not an error");
    let Resolved::Codex { launch, .. } = &resolved.resolved else {
        unreachable!()
    };
    assert_eq!(launch.base_url, format!("{ROOT}{API_PREFIX}"));
}

/// `LaunchValue::Declared`'s redaction, exercised the only way it can be.
///
/// **Not through `resolve()`.** `resolve()`'s own loop only ever calls
/// `also_launching_with` on a name already present in `must_be_unset()` for
/// the current auth kind, and `ClaudeLaunch::env` refuses *every* suppressor
/// it finds declared that way -- see `an_ambient_suppressor_refuses_the_resolution_by_name`
/// and `the_remote_variable_is_refused_under_exactly_one_kind` above, which
/// pin that direction for the two `Defeats` arms that return early and the
/// one that accumulates into `offending`. So a `Declared` entry can never
/// survive into a *successful* `resolve()`, which means no fixture built by
/// resolving a profile -- however the ambient environment is shaped -- can
/// ever exercise this rendering. The only route to one is
/// `also_launching_with` on a name that is *not* a suppressor (the same
/// route `claude_launch`'s own suite uses), built here directly.
#[test]
fn a_declared_non_suppressor_variable_is_redacted_in_the_rendered_environment() {
    let launch = roundhouse_server::ClaudeLaunch::new(ROOT, TURN_KEY)
        .expect("the fixture key resolves")
        .also_launching_with(
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            "a-value-that-must-never-be-printed",
        );
    let must_be_unset = launch.must_be_unset();
    let env = launch
        .env()
        .expect("a declared variable that is not a suppressor does not refuse the launch");
    let resolution = Resolution {
        name: "work".to_string(),
        profile: profile(Agent::Claude, AuthKind::RoundhouseKey),
        resolved: Resolved::Claude {
            launch,
            env,
            must_be_unset,
        },
    };
    let rendered = resolution.render();
    assert!(
        rendered.contains("\"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC\": \"<set>\""),
        "a declared variable's value must render redacted like every other secret-shaped field \
         in this output: {rendered}"
    );
    assert!(
        !rendered.contains("a-value-that-must-never-be-printed"),
        "`topham plan` is printed into terminals, pasted into issues and screen-shared: {rendered}"
    );
}

/// The catalog defaults to the profile's own `CODEX_HOME`, and an operator's
/// own path is used verbatim.
#[test]
fn the_catalog_path_defaults_beside_the_generated_config() {
    let resolved = resolve(
        &env(),
        "work",
        profile(Agent::Codex, AuthKind::RoundhouseKey),
    )
    .unwrap();
    let Resolved::Codex { launch, files, .. } = &resolved.resolved else {
        unreachable!()
    };
    assert_eq!(
        launch.model_catalog_path,
        "/op/data/topham/work/codex-home/model-catalog.json"
    );
    assert!(
        launch.config_toml().contains(&launch.model_catalog_path),
        "the generated config must name the catalog the launch writes"
    );
    assert_eq!(
        files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect::<Vec<_>>(),
        vec!["config.toml", "model-catalog.json"]
    );

    let mine = Profile {
        model_catalog_path: Some(std::path::PathBuf::from("/srv/catalog.json")),
        ..profile(Agent::Codex, AuthKind::RoundhouseKey)
    };
    let resolved = resolve(&env(), "work", mine).unwrap();
    let Resolved::Codex { launch, .. } = &resolved.resolved else {
        unreachable!()
    };
    assert_eq!(launch.model_catalog_path, "/srv/catalog.json");
}
