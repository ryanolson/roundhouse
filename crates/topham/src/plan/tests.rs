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

use roundhouse_server::claude_launch::signage;

use super::*;
use crate::test_support::{ROOT, TURN_KEY, env, scratch};

/// What the generator renders the key as, read from the type that renders it.
fn redacted_key() -> String {
    Secret::api_key(TURN_KEY)
        .expect("the fixture key is api-key shaped")
        .to_string()
}

/// What the generated registration renders as, read from the generator rather
/// than transcribed.
///
/// The JSON's own shape is pinned byte-for-byte in `claude_launch`'s suite,
/// which is where a change to it should be argued with; what these snapshots
/// are for is that the plan *shows* it, unexpanded, in the right place.
fn registration() -> String {
    ClaudeLaunch::new(ROOT, TURN_KEY)
        .expect("the fixture key resolves")
        .mcp_registration()
}

fn profile(agent: Agent, auth: AuthKind) -> Profile {
    Profile {
        auth,
        ..Profile::new(agent, ROOT)
    }
}

fn render(agent: Agent, auth: AuthKind) -> String {
    resolve(&env(&[]), "work", profile(agent, auth))
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

argv prepended to the operator's own:
    --mcp-config
    {registration}
    --append-system-prompt <the control-tool signage, {signage} characters>

must be unset when this launch runs:
    CLAUDE_CODE_USE_BEDROCK (environment) -- the client goes to another cloud and never reads the base URL
    CLAUDE_CODE_USE_VERTEX (environment) -- the client goes to another cloud and never reads the base URL
    CLAUDE_CODE_USE_FOUNDRY (environment) -- the client goes to another cloud and never reads the base URL
    ANTHROPIC_AUTH_TOKEN (environment) -- a second credential rides beside the sentinel and the edge reads it as the seat
    CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR (environment) -- a second credential rides beside the sentinel and the edge reads it as the seat
    CLAUDE_CODE_REMOTE (environment) -- an ambient login stops being suppressed and reaches this deployment
    apiKeyHelper (settings key) -- a second credential rides beside the sentinel and the edge reads it as the seat

settings files the client will read:
    (none found) -- an administrator's managed-policy file is not among the three searched;
    see the notes

files written by `topham launch`:
    (none) -- Claude Code's whole redirect surface is environment

notes:
    - an interactive session asks once. With `-p` the client always uses the API key; interactively it asks the user to approve it overriding their subscription, and until they do that session is on the subscription.
    - the registration above is what makes roundhouse's control tools exist for this client, not what makes it call one. Headless (`-p`), the client synthesises a permission refusal for an `mcp__roundhouse__*` tool unless its own argv names it -- `--allowedTools mcp__roundhouse__status` and so on; interactively it asks the operator. Neither is something this launcher can decide for it.
    - one entry above lives in the client's settings file rather than the environment. The three files listed above are read and refused; an administrator's managed-policy file is not read, and outranks all of them, so that one layer is stated rather than enforced.
",
        key = redacted_key(),
        registration = registration(),
        signage = signage().chars().count(),
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

argv prepended to the operator's own:
    --mcp-config
    {registration}
    --append-system-prompt <the control-tool signage, {signage} characters>

must be unset when this launch runs:
    CLAUDE_CODE_USE_BEDROCK (environment) -- the client goes to another cloud and never reads the base URL
    CLAUDE_CODE_USE_VERTEX (environment) -- the client goes to another cloud and never reads the base URL
    CLAUDE_CODE_USE_FOUNDRY (environment) -- the client goes to another cloud and never reads the base URL
    ANTHROPIC_AUTH_TOKEN (environment) -- the login this profile forwards is suppressed; every request still answers
    CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR (environment) -- the login this profile forwards is suppressed; every request still answers
    ANTHROPIC_API_KEY (environment) -- the login this profile forwards is suppressed; every request still answers
    apiKeyHelper (settings key) -- the login this profile forwards is suppressed; every request still answers

settings files the client will read:
    (none found) -- an administrator's managed-policy file is not among the three searched;
    see the notes

files written by `topham launch`:
    (none) -- Claude Code's whole redirect surface is environment

notes:
    - the precondition is a completed `claude` login, not this profile. Without one the client presents no credential and roundhouse degrades the turn to local-only, which nothing in the run reports.
    - the registration above is what makes roundhouse's control tools exist for this client, not what makes it call one. Headless (`-p`), the client synthesises a permission refusal for an `mcp__roundhouse__*` tool unless its own argv names it -- `--allowedTools mcp__roundhouse__status` and so on; interactively it asks the operator. Neither is something this launcher can decide for it.
    - one entry above lives in the client's settings file rather than the environment. The three files listed above are read and refused; an administrator's managed-policy file is not read, and outranks all of them, so that one layer is stated rather than enforced.
",
        key = redacted_key(),
        registration = registration(),
        signage = signage().chars().count(),
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
    let rendered = resolve(&env(&[]), "work", profile)
        .expect("a chained profile resolves -- only `launch` refuses it")
        .render();
    assert!(rendered.contains("topology        : chained"), "{rendered}");
    assert!(rendered.contains("`topham relay`"), "{rendered}");
}

/// R-T4's refusal, through the generator's own table rather than a copy of it.
#[test]
fn an_ambient_suppressor_refuses_the_resolution_by_name() {
    let mut env = env(&[]);
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

    // The other kind refuses it too, for its own reason: the sentinel does not
    // stand between an `ANTHROPIC_AUTH_TOKEN` and `Authorization`, so a launch
    // that promised a turn key and nothing else would present a second
    // credential the edge reads as the seat (F2). Same variable, two costs --
    // which is what the message has to say, and why this is not one blanket
    // scan.
    let error = resolve(
        &env,
        "work",
        profile(Agent::Claude, AuthKind::RoundhouseKey),
    )
    .expect_err("`ANTHROPIC_AUTH_TOKEN` rides past the sentinel onto Authorization");
    let message = error.to_string();
    assert!(
        message.contains("`ANTHROPIC_AUTH_TOKEN`"),
        "the refusal has to name the variable an operator must unset: {message}"
    );
    assert!(
        !message.contains("sk-somebody"),
        "and must not echo the credential it found: {message}"
    );
}

/// F2: an ambient `ANTHROPIC_AUTH_TOKEN` (or `apiKeyHelper` output) sits
/// outside `must_be_unset()` for `RoundhouseKey` — `refused_under` only fires
/// that suppressor under `ForwardedClaudeLogin` (`Defeats::TheSubscriptionLogin`).
/// `resolve` therefore never calls `also_launching_with` for it under
/// `RoundhouseKey`, `ClaudeLaunch::env` never sees it to refuse, and
/// `launch::layered` (topham/src/launch.rs:166) starts from the *whole*
/// ambient map, so the token rides into the child untouched. A profile whose
/// `kind` promises "a roundhouse turn key and nothing else" is admitted next
/// to a credential that promise never mentioned.
#[test]
fn f2_an_ambient_auth_token_is_refused_under_roundhouse_key_too() {
    let mut env = env(&[]);
    env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), "gw-token".to_string());

    let error = resolve(
        &env,
        "work",
        profile(Agent::Claude, AuthKind::RoundhouseKey),
    )
    .expect_err(
        "an ambient ANTHROPIC_AUTH_TOKEN rides untouched into a RoundhouseKey launch's child \
         environment today, where the client presents it on Authorization instead of the \
         sentinel; a RoundhouseKey profile promises a turn key and nothing else, so this must \
         refuse and name the variable",
    );
    assert!(
        error.to_string().contains("ANTHROPIC_AUTH_TOKEN"),
        "{error}"
    );
}

/// The other direction of the same table: `CLAUDE_CODE_REMOTE` is refused under
/// the kind that sets the sentinel, and admitted under the kind that does not.
#[test]
fn the_remote_variable_is_refused_under_exactly_one_kind() {
    let mut env = env(&[]);
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
        &env(&[]),
        "work",
        profile(Agent::Claude, AuthKind::RoundhouseKey),
    )
    .unwrap();
    let Resolved::Claude { launch, .. } = &claude.resolved else {
        panic!("a claude profile resolves to a claude launch");
    };
    assert_eq!(launch.base_url(), ROOT, "the deployment root, with no /v1");

    let codex = resolve(
        &env(&[]),
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
        resolve(&env(&[]), "work", wrong),
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
    let resolved = resolve(&env(&[]), "work", profile).expect("a trailing slash is not an error");
    let Resolved::Codex { launch, .. } = &resolved.resolved else {
        unreachable!()
    };
    assert_eq!(launch.base_url, format!("{ROOT}{API_PREFIX}"));
}

/// F16: `Resolved::Claude::must_be_unset` is claimed to be a cached copy of
/// `launch.must_be_unset()` that render/notes could recompute on demand
/// instead of storing.
///
/// This was a structural claim about where a derived value lives, so it never
/// had a red-to-green form: the cached field agreed with the launch beside it
/// on every input. What survives the fix is the property the cache was
/// pretending to be — the rendered table *is* the launch's own
/// `must_be_unset()`, for both kinds — which is what the two former readers of
/// the field now compute directly.
#[test]
fn f16_the_rendered_table_is_the_launchs_own_must_be_unset() {
    for auth in [AuthKind::RoundhouseKey, AuthKind::ForwardedLogin] {
        let resolution = resolve(&env(&[]), "work", profile(Agent::Claude, auth))
            .expect("the fixture profile resolves");
        let Resolved::Claude { launch, .. } = &resolution.resolved else {
            panic!("a claude profile resolves to a claude launch");
        };
        let rendered = resolution.render();
        for suppressor in launch.must_be_unset() {
            assert!(
                rendered.contains(suppressor.name),
                "the table an operator reads must be the generator's own, entry for entry \
                 (auth = {auth:?}, missing {}): {rendered}",
                suppressor.name
            );
        }
    }
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
    let env = launch
        .env()
        .expect("a declared variable that is not a suppressor does not refuse the launch");
    let resolution = Resolution {
        name: "work".to_string(),
        profile: profile(Agent::Claude, AuthKind::RoundhouseKey),
        resolved: Resolved::Claude {
            launch,
            env,
            settings: Vec::new(),
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
        &env(&[]),
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
    let resolved = resolve(&env(&[]), "work", mine).unwrap();
    let Resolved::Codex { launch, .. } = &resolved.resolved else {
        unreachable!()
    };
    assert_eq!(launch.model_catalog_path, "/srv/catalog.json");
}

/// F8 (M11.3 thermo-nuclear review): when a profile names its own
/// `model_catalog_path`, `config.toml` is generated to reference that path
/// (proven above by `the_catalog_path_defaults_beside_the_generated_config`),
/// but `resolve`'s `files` push is unconditional -- it always writes a
/// `GeneratedFile` at the fixed `CODEX_CATALOG_FILE` name
/// (`model-catalog.json`), which `launch::layered` then joins onto
/// `codex_home` regardless of what the operator named. Nothing reads that
/// file: the config points elsewhere. This is a second, stray write topham
/// makes and misreports as one of the "files written by topham launch".
///
#[test]
fn f8_a_named_catalog_path_does_not_also_write_the_default_named_file() {
    let mine = Profile {
        model_catalog_path: Some(std::path::PathBuf::from("/srv/catalog.json")),
        ..profile(Agent::Codex, AuthKind::RoundhouseKey)
    };
    let resolved = resolve(&env(&[]), "work", mine).unwrap();
    let Resolved::Codex { launch, files, .. } = &resolved.resolved else {
        unreachable!()
    };
    assert_eq!(launch.model_catalog_path, "/srv/catalog.json");
    assert!(
        !files
            .iter()
            .any(|f| f.relative_path == "model-catalog.json"),
        "the profile named its own catalog path, so no file should be written under the fixed \
         default name -- nothing reads it, and topham launch reports it as a file it wrote: {files:?}"
    );
}

/// F6 (M11.3 thermo-nuclear review): `profile.rs`'s secret scan does not
/// catch a key embedded in a larger `key-env` value (see
/// `profile::tests::a_key_embedded_in_a_larger_value_is_still_refused`), so a
/// profile built directly here -- bypassing that admission gap on purpose,
/// the way a saved profile would have reached this point -- carries the
/// whole `export ROUNDHOUSE_API_KEY=rh_turn_...` line as `key_env`. `resolve`
/// looks that string up as a *variable name*, finds nothing, and
/// `PlanError::TurnKeyMissing`'s `Display` interpolates `key_env` verbatim:
/// the live key rides out through the exact error `topham plan` and the TUI
/// print on every unexported-key run.
///
/// F6's substring scan and F20's round trip together close this: `resolve`
/// now re-parses every profile through `Profile::from_toml` before it ever
/// reads `key_env`, so the embedded key is caught as `CarriesSecret` and
/// `TurnKeyMissing` never gets a turn at formatting it.
#[test]
fn f6_an_embedded_key_does_not_survive_into_the_missing_key_error() {
    let key = format!("rh_turn_{:A<43}", "live");
    let profile = Profile {
        key_env: format!("export ROUNDHOUSE_API_KEY={key}"),
        ..profile(Agent::Claude, AuthKind::RoundhouseKey)
    };
    let error = resolve(&env(&[]), "work", profile)
        .expect_err("the malformed variable name is never set, so the key is not exported");
    let message = error.to_string();
    assert!(
        !message.contains(&key),
        "`topham plan` is printed into terminals, pasted into issues and screen-shared: {message}"
    );
    // The *mechanism*, not only the absence: a message that happened not to
    // carry the key because the variable name was elided would pass the
    // assertion above while leaving the profile admitted. What closes F6 is
    // that the round trip refuses the profile outright, before `key_env` is
    // ever looked up.
    assert!(
        matches!(
            &error,
            PlanError::Profile(ProfileError::CarriesSecret { field, .. }) if field == "key-env"
        ),
        "{error:#?}"
    );
}

/// F14, now that the constant is `pub` and there is one of it: the spelling
/// this module renders is the generator's own, so the guard is the import
/// rather than an assertion. What is left to check is that the rendering
/// actually uses it — a plan whose redaction read `<redacted>` while the
/// generator's `Debug` said `<set>` would read as two states of one variable.
#[test]
fn f14_the_rendered_redaction_is_the_generators_own_spelling() {
    assert_eq!(
        REDACTED_VALUE,
        roundhouse_server::claude_launch::REDACTED_VALUE
    );
    assert!(
        render(Agent::Claude, AuthKind::RoundhouseKey)
            .contains(&format!("read from $ROUNDHOUSE_API_KEY ({REDACTED_VALUE})")),
        "the turn-key line renders the generator's spelling"
    );
}

/// F20: a profile built **in process** — by the screen's editor, by a test, by
/// any future constructor — has not been through `Profile::from_toml`, and the
/// Claude arm of `resolve` reads neither of the two fields only a codex profile
/// has. Until this, that meant the field was dropped in silence exactly where a
/// file-loaded profile would have been refused by name.
#[test]
fn f20_an_in_process_claude_profile_with_a_codex_model_field_is_refused() {
    let profile = Profile {
        model: Some("a-codex-slug".to_string()),
        ..Profile::new(Agent::Claude, ROOT)
    };
    let error = resolve(&env(&[]), "work", profile).expect_err(
        "a Claude profile carrying a codex-only `model` field must be refused the same way \
         Profile::validate refuses it for a file-loaded profile, not silently accepted",
    );
    assert!(
        error.to_string().contains("`model`"),
        "and the refusal names the field, as the file boundary's does: {error}"
    );
}

/// F6 (M12 thermo-nuclear review), closed: `render()` derives the leading argv
/// from `launch`, and `Resolved::Claude` no longer holds a copy of it.
///
/// The field it held was a pure derivation of its sibling — exactly the "second
/// home for a derived value is a second thing that can be stale" the `settings`
/// field's own doc two lines above it refuses under F16 — and a `Resolution`
/// built with the two out of sync rendered `(none)` while the launch's own
/// answer was four arguments long. That construction is now unspellable, which
/// is the fix: this test builds the resolution the same way the redaction test
/// above does and asserts the rendering is the launch's.
#[test]
fn f6_render_must_derive_leading_argv_from_launch_not_a_separately_held_copy() {
    let launch = ClaudeLaunch::new(ROOT, TURN_KEY).expect("the fixture key resolves");
    let env = launch
        .env()
        .expect("the fixture launch is not refused by its own environment");
    let true_leading_argv = launch.leading_argv();
    assert!(
        !true_leading_argv.is_empty(),
        "the launch's own derivation is non-empty for this fixture, so an empty cached copy is \
         visibly wrong and not just a different valid rendering"
    );
    let resolution = Resolution {
        name: "work".to_string(),
        profile: profile(Agent::Claude, AuthKind::RoundhouseKey),
        resolved: Resolved::Claude {
            launch,
            env,
            settings: Vec::new(),
        },
    };
    let rendered = resolution.render();
    assert!(
        rendered.contains("--mcp-config"),
        "render() must show what `launch` actually derives ({true_leading_argv:?}): {rendered}"
    );
}

/// F9 (M12 thermo-nuclear review): a `key-env` that names a variable the
/// launcher itself writes into the child environment.
///
/// `resolve` reads the turn key out of `env[profile.key_env]` and stamps the
/// registration with `.with_key_env(&profile.key_env)` (plan.rs), so the
/// generated `--mcp-config` tells the client to expand `${ANTHROPIC_API_KEY}`.
/// Nothing checks that name against the three variables
/// [`ClaudeLaunch::env`] itself is about to write (`ANTHROPIC_BASE_URL`,
/// `ANTHROPIC_CUSTOM_HEADERS`, `ANTHROPIC_API_KEY` under a roundhouse-key
/// launch) -- `CollidesWithGeneratedVar` only fires for names declared through
/// `also_launching_with`. If admitted, the child's `ANTHROPIC_API_KEY` is
/// last written by the `RoundhouseKey` sentinel (`launch::layered` overlays
/// `generated.vars()` over `ambient.clone()`), so the value the ambient
/// environment held -- the real turn key `resolve` itself read as
/// `turn_key` -- is gone from the map `plan` renders and `topham launch`
/// would hand the child.
#[test]
fn f9_key_env_naming_a_generated_variable_is_admitted_and_the_sentinel_wins() {
    let ambient = env(&[("ANTHROPIC_API_KEY", TURN_KEY)]);
    let profile = Profile {
        key_env: "ANTHROPIC_API_KEY".to_string(),
        ..profile(Agent::Claude, AuthKind::RoundhouseKey)
    };
    match resolve(&ambient, "work", profile) {
        Err(error) => {
            // The defect would also be closed by a refusal at resolve time --
            // `plan`, and therefore `topham launch`, never producing a
            // control surface that 401s. Either remedy is acceptable; this
            // arm exists so the test still means something if the fix takes
            // this shape instead of the other.
            let message = error.to_string();
            assert!(
                message.contains("ANTHROPIC_API_KEY"),
                "a refusal here should name the colliding variable, the way \
                 CollidesWithGeneratedVar does for an also_launching_with clash: {message}"
            );
        }
        Ok(resolution) => {
            let Resolved::Claude { env: generated, .. } = &resolution.resolved else {
                panic!("a Claude profile resolves to Resolved::Claude");
            };
            assert_eq!(
                generated.get("ANTHROPIC_API_KEY").as_deref(),
                Some(TURN_KEY),
                "resolve admitted a key-env that collides with a generated variable, and the \
                 plan's own environment now holds the RoundhouseKey sentinel instead of the real \
                 turn key `resolve` read from the ambient environment -- every mcp__roundhouse__* \
                 control call the launched client makes will present the sentinel and be rejected \
                 as MalformedKey, while the plan renders this profile as healthy"
            );
        }
    }

    // The launcher's own two, which are not in the generator's map at all but
    // are written over the ambient environment just the same
    // (`launch::CLAUDE_DEPLOYMENT_POLICY`): the same silent failure, one layer
    // out.
    let policy = Profile {
        key_env: "DISABLE_AUTOUPDATER".to_string(),
        auth: AuthKind::RoundhouseKey,
        ..Profile::new(Agent::Claude, ROOT)
    };
    let error = resolve(&env(&[("DISABLE_AUTOUPDATER", TURN_KEY)]), "work", policy).expect_err(
        "the launcher overwrites this one too, so `${DISABLE_AUTOUPDATER}` expands to `1`",
    );
    assert!(error.to_string().contains("DISABLE_AUTOUPDATER"), "{error}");

    // The control: a name this launch does not write resolves, so what is
    // refused above is the collision and not `key-env` being honoured at all.
    let ordinary = Profile {
        key_env: "DEPLOY_TURN_KEY".to_string(),
        auth: AuthKind::RoundhouseKey,
        ..Profile::new(Agent::Claude, ROOT)
    };
    resolve(&env(&[("DEPLOY_TURN_KEY", TURN_KEY)]), "work", ordinary)
        .expect("a key variable of this deployment's own is what the profile is for");
}

/// F10 (M12 thermo-nuclear review): `notes()` dispatches on
/// `Resolved::Claude` in four separate patterns across three constructs --
/// two arms of the leading `match (&self.resolved, self.profile.auth)`, a
/// standalone `if matches!(self.resolved, Resolved::Claude { .. })` that
/// binds nothing from the variant, and the `if let Resolved::Claude { launch,
/// .. }` that immediately follows it. The standalone `if` could fold into
/// that `if let`, leaving three patterns instead of four.
///
/// This is a structural claim about `notes()`'s source, not about what it
/// returns -- folding the `if` into the `if let` changes no observable
/// output, so a behavioral test on `notes()` cannot fail for this reason.
/// The finding's own `how_to_prove` says as much: a grep count plus the
/// existing render snapshots is the whole proof. This test is that grep
/// count, made an assertion: it locates `notes()`'s body in the checked-in
/// source and counts `Resolved::Claude` occurrences in it.
#[test]
fn f10_notes_folds_the_standalone_claude_check_into_the_if_let_after_it() {
    let source = include_str!("../plan.rs");
    let start = source
        .find("pub fn notes(&self) -> Vec<String> {")
        .expect("notes() is still declared in plan.rs under this signature");
    let body = &source[start..];
    let end = body
        .find("\n        notes\n    }\n}")
        .expect("notes()'s body still ends by returning the accumulated `notes` vec");
    let body = &body[..end];

    let claude_patterns = body.matches("Resolved::Claude").count();
    assert_eq!(
        claude_patterns, 3,
        "notes() matches on Resolved::Claude {claude_patterns} times; folding the standalone \
         `if matches!(self.resolved, Resolved::Claude {{ .. }})` block into the `if let \
         Resolved::Claude {{ launch, .. }}` that immediately follows it -- which binds nothing \
         the standalone check needs and would still gate the same two notes on the same two \
         conditions -- would bring this to 3 (the two AuthKind match arms plus one merged `if \
         let`), not the 4 found today"
    );
}

/// F3 (M11.3 thermo-nuclear review): the settings files, which the client
/// reads for itself and this launcher had never looked at.
///
/// Driven through `CLAUDE_CONFIG_DIR` because that is the one of the three
/// paths an environment map can move — the two project-local files are
/// resolved against the directory the client is started in, which a test
/// cannot change without changing it for every other test in the process.
/// `settings_paths` is where that half is pinned, below.
mod settings {
    use super::*;

    /// A `CLAUDE_CONFIG_DIR` of this test's own, with `settings.json` in it.
    fn with_settings(tag: &str, contents: serde_json::Value) -> (PathBuf, EnvMap) {
        let directory = scratch(tag);
        std::fs::write(directory.join("settings.json"), contents.to_string())
            .expect("the settings file this test plants");
        let env = env(&[("CLAUDE_CONFIG_DIR", &directory.display().to_string())]);
        (directory, env)
    }

    fn resolved(env: &EnvMap) -> Result<Resolution, PlanError> {
        resolve(env, "work", profile(Agent::Claude, AuthKind::RoundhouseKey))
    }

    /// The one an operator actually meets: a persistent NeMo Relay install
    /// leaves `env.ANTHROPIC_BASE_URL` behind, the client applies it over the
    /// value this launch exported, and every turn goes somewhere else.
    #[test]
    fn an_env_block_naming_a_generated_variable_refuses_the_launch() {
        let (directory, env) = with_settings(
            "settings-base-url",
            serde_json::json!({ "env": { "ANTHROPIC_BASE_URL": "http://127.0.0.1:1" } }),
        );
        let error = resolved(&env).expect_err(
            "a settings `env` entry replaces the value this launch exports, so the client that \
             started would not be this launch",
        );
        let message = error.to_string();
        assert!(
            message.contains(&directory.join("settings.json").display().to_string()),
            "the refusal has to name the file to edit: {message}"
        );
        assert!(message.contains("env.ANTHROPIC_BASE_URL"), "{message}");
        assert!(
            crate::cli::error_chain(&error)
                .iter()
                .any(|line| line.contains("already names")),
            "the generator's own account of the collision is the cause: {:#?}",
            crate::cli::error_chain(&error)
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The other half of the same sweep: a suppressor set in the settings file
    /// is a suppressor, and the ambient-only check never saw it.
    #[test]
    fn an_env_block_naming_a_suppressor_refuses_the_launch() {
        let (directory, env) = with_settings(
            "settings-vertex",
            serde_json::json!({ "env": { "CLAUDE_CODE_USE_VERTEX": "1" } }),
        );
        let error = resolved(&env).expect_err("the client never reads the base URL at all");
        assert!(
            error.to_string().contains("env.CLAUDE_CODE_USE_VERTEX"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// `apiKeyHelper` is the entry that is not a variable, fed to the
    /// generator through the seam it has for exactly this.
    #[test]
    fn an_api_key_helper_refuses_the_launch_by_name() {
        let (directory, env) = with_settings(
            "settings-helper",
            serde_json::json!({ "apiKeyHelper": "/usr/local/bin/get-key.sh" }),
        );
        let error = resolved(&env)
            .expect_err("a helper's output reaches Authorization past the sentinel entirely");
        let message = error.to_string();
        assert!(message.contains("apiKeyHelper"), "{message}");
        assert!(
            !message.contains("get-key.sh"),
            "the refusal names the key, not what the file set it to: {message}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The control that keeps the three above from reading as "any settings
    /// file is fatal": an empty one resolves, and is reported as read.
    #[test]
    fn an_empty_settings_file_resolves_and_is_listed_as_read() {
        let (directory, env) = with_settings("settings-empty", serde_json::json!({}));
        let rendered = resolved(&env)
            .expect("an empty settings file changes nothing about this launch")
            .render();
        assert!(
            rendered.contains(&directory.join("settings.json").display().to_string()),
            "a file that was read is reported even when it refused nothing: {rendered}"
        );
        assert!(
            rendered.contains("nothing this launch depends on"),
            "{rendered}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// An `env` block naming something this launch does not care about is
    /// admitted — and rendered by name, never by value: a settings `env` block
    /// is where an operator's own token lives.
    #[test]
    fn an_unrelated_env_entry_is_admitted_and_rendered_without_its_value() {
        let (directory, env) = with_settings(
            "settings-unrelated",
            serde_json::json!({ "env": { "EDITOR": "a-value-that-must-never-be-printed" } }),
        );
        let rendered = resolved(&env)
            .expect("a settings variable this launch does not write is not a refusal")
            .render();
        assert!(rendered.contains("env.EDITOR"), "{rendered}");
        assert!(
            !rendered.contains("a-value-that-must-never-be-printed"),
            "`topham plan` is printed into terminals, pasted into issues and screen-shared: \
             {rendered}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A file the launcher cannot parse is refused rather than skipped: its
    /// `env` block is exactly what cannot be checked, and it outranks
    /// everything this launch exports.
    #[test]
    fn a_settings_file_that_is_not_json_is_refused_naming_the_file() {
        let directory = scratch("settings-garbage");
        let path = directory.join("settings.json");
        std::fs::write(&path, "{ this is not json").expect("the file this test plants");
        let env = env(&[("CLAUDE_CONFIG_DIR", &directory.display().to_string())]);

        let error = resolved(&env).expect_err("an unparsable settings file is refused");
        assert!(
            matches!(error, PlanError::SettingsUnreadable { .. }),
            "{error:?}"
        );
        assert!(
            error.to_string().contains(&path.display().to_string()),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The three paths, in the order the client reads them — and the fallback
    /// that puts the user file under `$HOME/.claude` when `CLAUDE_CONFIG_DIR`
    /// does not move it.
    #[test]
    fn the_three_paths_are_the_ones_the_client_searches() {
        let relocated = env(&[("CLAUDE_CONFIG_DIR", "/op/elsewhere"), ("HOME", "/op/home")]);
        assert_eq!(
            settings_paths(&relocated, Some(Path::new("/work/repo"))),
            [
                PathBuf::from("/op/elsewhere/settings.json"),
                PathBuf::from("/work/repo/.claude/settings.json"),
                PathBuf::from("/work/repo/.claude/settings.local.json"),
            ]
        );

        let plain = env(&[("HOME", "/op/home")]);
        assert_eq!(
            settings_paths(&plain, None),
            [PathBuf::from("/op/home/.claude/settings.json")]
        );

        assert!(
            settings_paths(&env(&[]), None).is_empty(),
            "with neither variable there is nowhere to look, and a launch reads no file at all"
        );
    }
}
