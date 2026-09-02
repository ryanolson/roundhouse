// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a profile is allowed to be.
//!
//! The round trip and the secret refusal are the two halves R-T7 asks for, and
//! they are not independent: the refusal is only worth anything if the file it
//! refuses is the same file `save` would have written, which is what the round
//! trip pins.

use std::path::PathBuf;

use super::*;

/// A scratch directory, per the house pattern (`claude_e2e.rs`): the temp dir
/// plus a UUID, so two tests in one run never collide and a failure leaves the
/// evidence behind.
fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("topham-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("a scratch directory");
    root
}

/// An environment naming `root` as both XDG homes.
fn env_at(root: &std::path::Path) -> EnvMap {
    EnvMap::from([
        (
            "XDG_CONFIG_HOME".to_string(),
            root.join("config").display().to_string(),
        ),
        (
            "XDG_DATA_HOME".to_string(),
            root.join("data").display().to_string(),
        ),
    ])
}

fn codex_profile() -> Profile {
    Profile {
        agent: Agent::Codex,
        deployment_root: "http://127.0.0.1:8080".to_string(),
        auth: AuthKind::ForwardedLogin,
        key_env: "DEPLOYMENT_TURN_KEY".to_string(),
        topology: Topology::Chained,
        model: Some("roundhouse-local".to_string()),
        model_catalog_path: Some(PathBuf::from("/srv/roundhouse/catalog.json")),
    }
}

#[test]
fn a_profile_round_trips_through_its_file_form() {
    let profile = codex_profile();
    let text = profile.to_toml();
    assert_eq!(
        Profile::from_toml(&text, "work").expect("what `save` writes is what `load` reads"),
        profile
    );

    // The other agent, on the defaults, because the two carry different
    // fields and `skip_serializing_if` means the second file has fewer lines.
    let claude = Profile::new(Agent::Claude, "http://127.0.0.1:8080/");
    let text = claude.to_toml();
    assert_eq!(Profile::from_toml(&text, "work").unwrap(), claude);
}

/// The header comment is part of the file, and part of what it is for.
#[test]
fn the_written_file_says_no_secret_belongs_in_it() {
    let text = Profile::new(Agent::Claude, "http://127.0.0.1:8080").to_toml();
    assert!(
        text.contains("NO SECRET BELONGS IN THIS FILE"),
        "the one sentence an operator has to read before editing this file:\n{text}"
    );
}

/// Everything the profile does not say is a default, and the defaults are the
/// generators' own.
#[test]
fn an_almost_empty_profile_takes_the_generators_defaults() {
    let profile = Profile::from_toml(
        "agent = \"claude\"\ndeployment-root = \"http://127.0.0.1:8080\"\n",
        "work",
    )
    .expect("agent and root are the only required fields");
    assert_eq!(profile.auth, AuthKind::RoundhouseKey);
    assert_eq!(profile.topology, Topology::Direct);
    assert_eq!(
        profile.key_env,
        roundhouse_server::codex_launch::DEFAULT_KEY_ENV,
        "a profile that names no variable must agree with the config the codex generator would \
         have written on its own"
    );
}

/// A misspelled field is refused rather than dropped — every field here changes
/// where a client posts turns or which credential it presents.
#[test]
fn a_misspelled_field_is_refused() {
    let error = Profile::from_toml(
        "agent = \"claude\"\ndeployment-root = \"http://x\"\nkey_env = \"K\"\n",
        "work",
    )
    .expect_err("`key_env` is not `key-env`");
    assert!(
        matches!(error, ProfileError::Malformed { .. }),
        "{error:#?}"
    );
}

/// R-T7's secret refusal, in every shape a key gets into a file by.
///
/// The value is never in the message — see [`ProfileError::CarriesSecret`] —
/// so each case asserts on the *field* the refusal names, which is also the
/// only thing that tells an operator where to look.
#[test]
fn a_profile_carrying_a_key_shaped_value_is_refused_by_field() {
    let well_formed = format!("rh_turn_{:A<43}", "live");
    assert!(
        roundhouse_server::has_valid_key_shape(&well_formed),
        "the fixture must be a key this deployment would actually accept"
    );

    let cases: [(&str, String, &str); 5] = [
        (
            "the variable's field, holding a value instead of a name",
            format!(
                "agent = \"claude\"\ndeployment-root = \"http://x\"\nkey-env = \"{well_formed}\"\n"
            ),
            "key-env",
        ),
        (
            "a field this struct does not have at all",
            format!(
                "agent = \"claude\"\ndeployment-root = \"http://x\"\nturn-key = \"{well_formed}\"\n"
            ),
            "turn-key",
        ),
        (
            "a table this struct does not have",
            format!(
                "agent = \"claude\"\ndeployment-root = \"http://x\"\n\n[secrets]\nkey = \
                 \"{well_formed}\"\n"
            ),
            "secrets.key",
        ),
        (
            "a truncated paste, which is not a usable key and is still a secret",
            "agent = \"claude\"\ndeployment-root = \"http://x\"\nnote = \"rh_turn_abc\"\n"
                .to_string(),
            "note",
        ),
        (
            "the launch sentinel, which is in the key namespace on purpose",
            format!(
                "agent = \"claude\"\ndeployment-root = \"http://x\"\nnote = \"{}\"\n",
                roundhouse_server::claude_launch::ROUNDHOUSE_API_KEY_SENTINEL
            ),
            "note",
        ),
    ];

    for (what, text, field) in cases {
        let error = Profile::from_toml(&text, "work")
            .expect_err(&format!("a profile must not carry a secret in {what}"));
        let message = error.to_string();
        match error {
            ProfileError::CarriesSecret { field: named, .. } => {
                assert_eq!(named, field, "{what}");
            }
            other => panic!("{what}: expected a secret refusal, got {other:#?}"),
        }
        assert!(
            !message.contains("rh_turn_") && !message.contains("rh_admin_"),
            "the refusal must name the field and never echo the value: {message}"
        );
    }
}

/// The scan runs before deserialization, which is what lets it name a field
/// `deny_unknown_fields` would otherwise have rejected first.
#[test]
fn a_secret_in_an_unknown_field_is_reported_as_a_secret_not_as_a_typo() {
    let secret = format!("rh_admin_{:A<43}", "root");
    let error = Profile::from_toml(
        &format!("agent = \"claude\"\ndeployment-root = \"http://x\"\nadmin = \"{secret}\"\n"),
        "work",
    )
    .expect_err("an admin key in a profile is still a secret in a file");
    assert!(
        matches!(&error, ProfileError::CarriesSecret { field, .. } if field == "admin"),
        "a message about a misspelled field would send the operator to rename the field their \
         key is sitting in: {error:#?}"
    );
}

/// A field that belongs to the other agent is refused rather than ignored.
#[test]
fn a_claude_profile_may_not_carry_the_codex_fields() {
    for (field, line) in [
        ("model", "model = \"gpt-5\""),
        ("model-catalog-path", "model-catalog-path = \"/srv/c.json\""),
    ] {
        let error = Profile::from_toml(
            &format!("agent = \"claude\"\ndeployment-root = \"http://x\"\n{line}\n"),
            "work",
        )
        .expect_err("a claude profile has no model of its own -- the model rides the request body");
        assert!(
            matches!(&error, ProfileError::NotACodexField { field: named, .. } if *named == field),
            "{error:#?}"
        );
    }
}

/// A profile name becomes one path segment, so the ones that would not are
/// refused before anything is joined.
#[test]
fn a_profile_name_is_one_filename() {
    let env = env_at(&scratch("names"));
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "a/b",
        ".hidden",
        "-flag",
        "sp ace",
    ] {
        assert!(
            matches!(
                Profile::path(&env, name),
                Err(ProfileError::UnusableName { .. })
            ),
            "`{name}` must not resolve to a profile path"
        );
    }
    assert!(Profile::path(&env, "work.direct-2").is_ok());
}

#[test]
fn save_writes_where_load_and_names_look() {
    let root = scratch("save");
    let env = env_at(&root);
    let profile = codex_profile();

    let path = profile.save(&env, "work").expect("the profile is written");
    assert_eq!(
        path,
        root.join("config/topham/profiles/work.toml"),
        "the XDG rule is `XDG_CONFIG_HOME`, then this crate's own directory"
    );
    assert_eq!(Profile::load(&env, "work").unwrap(), profile);

    Profile::new(Agent::Claude, "http://127.0.0.1:8080")
        .save(&env, "alt")
        .unwrap();
    assert_eq!(Profile::names(&env).unwrap(), vec!["alt", "work"]);
}

/// A first run has no profiles directory, and the list has to render that.
#[test]
fn listing_a_machine_that_has_never_launched_anything_is_empty_not_an_error() {
    let env = env_at(&scratch("empty"));
    assert!(Profile::names(&env).unwrap().is_empty());
}

#[test]
fn loading_a_profile_that_is_not_there_names_the_path() {
    let root = scratch("missing");
    let env = env_at(&root);
    let error = Profile::load(&env, "work").expect_err("nothing was ever saved");
    match error {
        ProfileError::NotFound { path, .. } => {
            assert_eq!(path, root.join("config/topham/profiles/work.toml"))
        }
        other => panic!("{other:#?}"),
    }
}

/// The per-profile `CODEX_HOME` is under the *data* directory, not the
/// configuration one — a generated file rewritten on every launch does not
/// belong in a dotfile repository.
#[test]
fn the_codex_home_is_per_profile_and_under_the_data_directory() {
    let root = scratch("codex-home");
    let env = env_at(&root);
    assert_eq!(
        Profile::codex_home(&env, "work").unwrap(),
        root.join("data/topham/work/codex-home")
    );
    assert_ne!(
        Profile::codex_home(&env, "work").unwrap(),
        Profile::codex_home(&env, "other").unwrap(),
        "two profiles sharing one CODEX_HOME would share the auth.json a `codex login` writes"
    );
}
