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

    let cases: [(&str, String, &str); 6] = [
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
            "a key rather than a value, which cannot be echoed to name itself",
            format!(
                "agent = \"claude\"\ndeployment-root = \"http://x\"\n\n[secrets]\n{well_formed} = \
                 \"whatever\"\n"
            ),
            "secrets.<a key>",
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

/// F1 (M11.3 thermo-nuclear review): a paste that fails to *parse* as TOML at
/// all — a bare secret, a `.env` line, or a `topham mint` export line — must
/// not echo the pasted value back through [`ProfileError::Malformed`]. The
/// scan in [`find_secret`] only runs after `toml::from_str` succeeds
/// (`from_toml`, above), so these three shapes never reach it; the parser's
/// own error is what a caller sees, and `toml`'s `Display` inlines the
/// offending source line under a caret.
///
/// `error_chain` is `pub(crate)` reachable via `crate::cli`, and is exactly
/// what `main.rs` and the TUI print — the same text this test inspects.
///
/// **The refusal is `CarriesSecret`, not `Malformed`**, which is a correction
/// to the shape this test asserted when it was written to fail: the scan now
/// runs on the raw text, so none of these three pastes reaches the parser at
/// all. Refusing them as parse failures with a sanitised message would have
/// left the leak one unanticipated paste shape away — the message would have
/// to keep being right about text nobody has seen yet — where refusing them as
/// what they are means no parse error is ever constructed from a document
/// carrying a key.
#[test]
fn a_paste_that_fails_to_parse_does_not_echo_the_secret() {
    let secret = format!("rh_turn_{:A<43}", "live");
    assert!(
        roundhouse_server::has_valid_key_shape(&secret),
        "the fixture must be a key this deployment would actually accept"
    );

    let pastes: [(&str, String); 3] = [
        ("a bare key pasted with no `=` at all", secret.clone()),
        (
            "a `.env`-style line",
            format!("DEPLOYMENT_TURN_KEY={secret}"),
        ),
        (
            "a `topham mint` export line, pasted whole",
            format!("export DEPLOYMENT_TURN_KEY={secret}"),
        ),
    ];

    for (what, text) in pastes {
        let error = Profile::from_toml(&text, "work").expect_err(&format!("{what} is not TOML"));
        assert!(
            matches!(error, ProfileError::CarriesSecret { .. }),
            "{what}: a paste carrying a key is refused as the secret it is, before the parser \
             sees it, got {error:#?}"
        );
        let chain = crate::cli::error_chain(&error).join("\n");
        assert!(
            !chain.contains(&secret),
            "{what}: the parse-failure message must never echo the pasted secret:\n{chain}"
        );
    }
}

/// F1's second half, and the one that outlives the scan: a parse failure
/// reports *what* the parser objected to and *where*, and never the text it
/// objected to.
///
/// The scan above is what refuses a paste this crate can recognise; this is
/// what a paste shape nobody has thought of yet costs. Without it the guard is
/// only as good as the three prefixes [`looks_like_a_secret`] knows.
#[test]
fn a_parse_failure_reports_the_position_and_not_the_line() {
    let error = Profile::from_toml(
        "agent = \"claude\"\ndeployment-root = whatever-the-operator-pasted\n",
        "work",
    )
    .expect_err("a bare word is not a TOML value");
    let chain = crate::cli::error_chain(&error).join("\n");
    assert!(
        !chain.contains("whatever-the-operator-pasted"),
        "the offending line must not be quoted back -- that excerpt is how a pasted credential \
         got out (F1):\n{chain}"
    );
    assert!(
        chain.contains("line 2, column 19"),
        "and the position must survive, or the message sends nobody anywhere:\n{chain}"
    );
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

/// F6 (M11.3 thermo-nuclear review): `looks_like_a_secret` only checks
/// `starts_with` the minted prefix (or exact sentinel equality) on the
/// *trimmed whole value*, so a key embedded partway through a larger string
/// -- exactly the `export ROUNDHOUSE_API_KEY=rh_turn_...` line `topham mint`
/// tells operators to paste -- never matches `starts_with` and sails through
/// `find_secret` untouched. The module doc at :27-32 promises "anything
/// wearing a minted prefix is refused"; this is a live key that does, and is
/// not.
///
/// It failed on the finding — `from_toml` returned `Ok`, not
/// `Err(CarriesSecret)` — and is live now that the scan reads the whole value
/// rather than only its front.
#[test]
fn a_key_embedded_in_a_larger_value_is_still_refused() {
    let key = format!("rh_turn_{:A<43}", "live");
    assert!(
        roundhouse_server::has_valid_key_shape(&key),
        "the fixture must be a key this deployment would actually accept"
    );

    // Exactly what `topham mint` tells an operator to paste, pasted into the
    // field that documents itself as naming a variable, not holding one.
    let text = format!(
        "agent = \"claude\"\ndeployment-root = \"http://x\"\nkey-env = \"export \
         ROUNDHOUSE_API_KEY={key}\"\n"
    );
    let error = Profile::from_toml(&text, "work")
        .expect_err("an export line carrying a live key is still a secret in the file");
    assert!(
        matches!(&error, ProfileError::CarriesSecret { field, .. } if field == "key-env"),
        "{error:#?}"
    );
}

/// F15 (M11.3 review): the per-profile root is a *named* accessor, and both
/// generated directories are joins onto it.
///
/// The finding was that `relay::scratch_dir` reached the root by walking up
/// from `codex_home` with `.parent().expect(..)`. The refutation showed the
/// panic is unreachable through this crate's validated inputs — and that this
/// is the worse half: a change to the codex layout leaves `.parent()` returning
/// `Some` of the *wrong* directory, so two profiles could quietly come to share
/// one Relay upstream with nothing going red. Deriving both from one accessor
/// makes that disagreement unrepresentable rather than merely unlikely.
#[test]
fn the_scratch_root_is_the_one_place_the_per_profile_directory_is_spelled() {
    let root = scratch("scratch-root");
    let env = env_at(&root);
    assert_eq!(
        Profile::scratch_root(&env, "work").unwrap(),
        root.join("data/topham/work")
    );
    assert_eq!(
        Profile::codex_home(&env, "work").unwrap().parent(),
        Some(Profile::scratch_root(&env, "work").unwrap().as_path()),
        "the codex home is a join onto the root, not a directory that happens to sit near it"
    );
    // The name check belongs to the root, so a traversal cannot reach a
    // directory outside it through either derived path.
    assert!(Profile::scratch_root(&env, "../elsewhere").is_err());
}
