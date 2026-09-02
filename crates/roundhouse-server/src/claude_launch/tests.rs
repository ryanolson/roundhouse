// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! [`claude_launch`](super)'s unit tests, in their own file for the reason the
//! suppressor table is in one: the module's tests are as long as its code, and
//! a generator whose whole output is a map is tested by asserting the map, so
//! they grow with the client rather than with the mechanism.

use super::*;

const TURN_KEY: &str = "rh_turn_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const ADMIN_KEY: &str = "rh_admin_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const ROOT: &str = "http://127.0.0.1:8080";

fn launch() -> ClaudeLaunch {
    ClaudeLaunch::new(ROOT, TURN_KEY).expect("the documented-correct shape constructs")
}

fn env_of(launch: &ClaudeLaunch) -> BTreeMap<String, String> {
    launch
        .env()
        .expect("the documented-correct shape renders")
        .vars()
        .map(|(name, value)| (name.to_string(), value))
        .collect()
}

/// **The snapshot M11.3's launcher will consume**, for both kinds.
///
/// Written as one exhaustive map per kind rather than as a handful of
/// `contains` assertions, because the property that matters to the launcher
/// is that the map is *complete and closed*: a fourth variable appearing
/// here is a change to what a launch means, and one disappearing is a client
/// that silently falls back to `api.anthropic.com` or to an ambient login.
#[test]
fn each_auth_kind_renders_one_exact_environment() {
    assert_eq!(
        env_of(&launch()),
        BTreeMap::from([
            (BASE_URL_ENV.to_string(), ROOT.to_string()),
            (
                CUSTOM_HEADERS_ENV.to_string(),
                format!("{TURN_KEY_HEADER}: {TURN_KEY}")
            ),
            (
                API_KEY_ENV.to_string(),
                ROUNDHOUSE_API_KEY_SENTINEL.to_string()
            ),
        ]),
    );
    assert_eq!(
        env_of(&launch().forwarding_claude_login()),
        BTreeMap::from([
            (BASE_URL_ENV.to_string(), ROOT.to_string()),
            (
                CUSTOM_HEADERS_ENV.to_string(),
                format!("{TURN_KEY_HEADER}: {TURN_KEY}")
            ),
        ]),
        "a forwarded login must carry no API key: any resolved value suppresses \
         the very login it exists to forward"
    );
}

/// The header block is in the syntax §1.6 says the client parses.
///
/// Re-derived here by running the client's own regex rather than by
/// asserting the string this module just built, which would be a tautology.
/// The parse is `^\s*(.*?)\s*:\s*(.*?)\s*$` per line, non-greedy, so the
/// first colon wins and whitespace is trimmed on both halves.
#[test]
fn the_custom_header_block_parses_the_way_the_client_parses_it() {
    let rendered = env_of(&launch())[CUSTOM_HEADERS_ENV].clone();
    let lines: Vec<&str> = rendered
        .split(['\n', '\r'])
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "one header, one line: {rendered:?}");
    let (name, value) = lines[0]
        .split_once(':')
        .expect("the client splits on the first colon");
    assert_eq!(name.trim(), TURN_KEY_HEADER);
    assert_eq!(value.trim(), TURN_KEY);
    // And the value survives the client's own header-safety check
    // (v2.1.227+ rejects non-HTTP-safe characters): every byte is visible
    // ASCII, so nothing here can be split into a second header.
    assert!(
        rendered.bytes().all(|b| (0x20..=0x7e).contains(&b)),
        "an HTTP-unsafe byte in {rendered:?} is a header the client refuses to send"
    );
}

/// The turn key is in the environment and nowhere a reader can reach.
///
/// The structural half is that this module renders no file at all; this is
/// the behavioural half, and it is aimed at the two things a launcher will
/// actually do with these types — print them while debugging, and log which
/// variables it set.
#[test]
fn the_turn_key_rides_the_environment_and_no_rendering_of_it() {
    let launch = launch();
    let env = launch.env().expect("renders");
    assert!(
        format!("{env:?}").contains(TURN_KEY_HEADER),
        "a redacted map must still say which header carries the key"
    );
    for rendered in [format!("{env:?}"), format!("{launch:?}")] {
        assert!(
            !rendered.contains(TURN_KEY) && !rendered.contains("rh_turn_"),
            "a Debug of the launch or its environment must redact the key:\n{rendered}"
        );
    }
    assert!(
        !env.names().any(|name| name.contains(TURN_KEY)),
        "the names half of the seam must be free of the value half"
    );
    // The one seam that does yield it, so the redaction above is a
    // redaction rather than a key that was never there.
    assert_eq!(
        env.get(CUSTOM_HEADERS_ENV),
        Some(format!("{TURN_KEY_HEADER}: {TURN_KEY}"))
    );
}

/// Every one of §1.3's five suppressing inputs is refused by name under a
/// forwarded login.
///
/// One assertion per input rather than one over the list, because what the
/// test is for is that no arm was skipped — a loop over
/// [`OAUTH_SUPPRESSORS`] would pass against an implementation that also
/// looped over it and got the list wrong.
#[test]
fn a_forwarded_login_refuses_each_input_that_would_suppress_it() {
    let refuse = |launch: ClaudeLaunch| {
        launch
            .forwarding_claude_login()
            .env()
            .expect_err("a suppressor beside a forwarded login is refused")
    };
    for name in [
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
        API_KEY_ENV,
    ] {
        let error = refuse(launch().also_launching_with(name, "anything"));
        // By identity in the carried list, not by substring in a rendered
        // message: one real suppressor's name is a prefix of a plausible
        // spelling that names no suppressor at all, so a `contains` check
        // answers a different question than this test is asking (F9).
        assert!(
            matches!(&error, ClaudeLaunchError::OauthSuppressorsPresent { suppressors }
                if suppressors.iter().any(|s| s.name == name)),
            "`{name}` must be refused by name, got: {error}"
        );
    }
    for name in [
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        // These three are refused one level harder: they change which
        // provider the client selects at all, so the base URL is never read
        // and the turn never reaches this deployment.
        let error = refuse(launch().also_launching_with(name, "1"));
        assert_eq!(
            error,
            ClaudeLaunchError::RedirectDefeated { name },
            "`{name}` defeats the redirect, not only the login"
        );
    }
    // The settings key, which is the one a launcher cannot fix by clearing
    // the child's environment — hence its own site and its own input.
    let error = refuse(launch().with_settings_api_key_helper());
    assert!(
        matches!(&error, ClaudeLaunchError::OauthSuppressorsPresent { suppressors }
            if suppressors.iter().any(|s| s.name == "apiKeyHelper")),
        "the settings key must be refused too, got: {error}"
    );

    // CONTROL: an unrelated variable beside a forwarded login is not a
    // refusal. Without this the rule above is indistinguishable from
    // "refuse every extra variable", which would make the type useless to a
    // launcher that has to set `PATH`.
    let fine = launch()
        .forwarding_claude_login()
        .also_launching_with("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env()
        .expect("an unrelated variable is not a suppressor");
    assert_eq!(
        fine.get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"),
        Some("1".to_string())
    );
}

/// F9: which suppressor fired is answerable by identity, not by searching a
/// rendered message.
///
/// The distinction is live rather than hypothetical:
/// `CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR` is a real suppressor whose name has
/// `CLAUDE_CODE_API_KEY` — which names no suppressor at all — as a literal
/// prefix. While the variant carried a backtick-joined `String`, flagging
/// the former made a `.contains(..)` check (the style the sibling test above
/// used) read the latter as present.
#[test]
fn oauth_suppressors_present_carries_which_suppressor_fired() {
    let error = launch()
        .forwarding_claude_login()
        .also_launching_with("CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR", "anything")
        .env()
        .expect_err("a suppressor beside a forwarded login is refused");
    let suppressors = match &error {
        ClaudeLaunchError::OauthSuppressorsPresent { suppressors } => suppressors,
        other => panic!("expected OauthSuppressorsPresent, got {other}"),
    };
    assert!(
        suppressors
            .iter()
            .any(|s| s.name == "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR"),
        "the suppressor that actually fired must be in the list"
    );
    assert!(
        !suppressors.iter().any(|s| s.name == "CLAUDE_CODE_API_KEY"),
        "`CLAUDE_CODE_API_KEY` is not a suppressor -- only a textual prefix of \
         the one that fired -- and the carried list must not read as naming it"
    );
    // The list is what the variant holds; the message is rendered from it,
    // so the prose an operator sees still names what fired.
    assert!(
        error
            .to_string()
            .contains("`CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR`"),
        "Display must still name the suppressor: {error}"
    );
}

/// The redirect-defeating three are refused under **both** kinds.
///
/// The ruling names them among the five a forwarded login refuses; they are
/// refused for the bring-your-own-key launch as well, and the reason is a
/// different one. §1.3's `I7()` picks the provider before any credential is
/// resolved, and a non-`firstParty` answer means [`BASE_URL_ENV`] is not read
/// at all — so the sentinel does its job perfectly and the client still
/// never reaches roundhouse. That failure is silent on both sides: the agent
/// answers, from somebody else's serving plane, and no roundhouse log has a
/// row for the turn that did not arrive.
#[test]
fn a_cloud_provider_selector_is_refused_even_under_a_roundhouse_key() {
    for name in [
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        assert_eq!(
            launch()
                .also_launching_with(name, "1")
                .env()
                .expect_err("a cloud selector makes the base URL unread"),
            ClaudeLaunchError::RedirectDefeated { name }
        );
    }
    // CONTROL: an input this table does not name is not refused under this
    // kind either. Without it the rule above is indistinguishable from
    // "refuse every extra variable", which would make the type useless to a
    // launcher that has to set `PATH`. The *credential* inputs are refused
    // here too, for a reason of their own — see
    // [`a_credential_input_is_refused_beside_the_sentinel_too`].
    assert!(
        launch()
            .also_launching_with("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env()
            .is_ok(),
        "a variable that suppresses nothing is not a suppressor under either kind"
    );
}

/// F2: the three inputs that resolve to a credential of the operator's own are
/// refused beside the sentinel, not only beside a forwarded login.
///
/// The sentinel decides what §1.3's `VV()` resolves for the *API-key* arm and
/// nothing else, so each of these still reaches `Authorization` — and
/// roundhouse's edge records what arrives there as the forwarded seat. A
/// `RoundhouseKey` launch next to one of them is a forwarded-login launch in
/// fact, on a credential the operator never chose to hand over, and every turn
/// answers while it happens.
#[test]
fn a_credential_input_is_refused_beside_the_sentinel_too() {
    for name in [
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    ] {
        let error = launch()
            .also_launching_with(name, "anything")
            .env()
            .expect_err("an operator credential rides past the sentinel onto the wire");
        // By identity in the carried list rather than by substring, for the
        // reason F9 gives one variant up.
        assert!(
            matches!(&error, ClaudeLaunchError::CredentialBesideTheSentinel { suppressors }
                if suppressors.iter().any(|s| s.name == name)),
            "`{name}` must be refused by name under RoundhouseKey, got: {error}"
        );
    }
    let error = launch()
        .with_settings_api_key_helper()
        .env()
        .expect_err("the helper's output is presented exactly like the token is");
    assert!(
        matches!(&error, ClaudeLaunchError::CredentialBesideTheSentinel { suppressors }
            if suppressors.iter().any(|s| s.name == "apiKeyHelper")),
        "the settings key must be refused under this kind too, got: {error}"
    );

    // CONTROL, and the row that makes this a per-input rule rather than
    // "every credential input": `API_KEY_ENV` is the one the generated map
    // writes the sentinel over, so it is refused as a collision with what the
    // launch itself sets and never as an operator credential riding past it.
    assert_eq!(
        launch()
            .also_launching_with(API_KEY_ENV, "sk-ant-something")
            .env()
            .expect_err("the generator writes that variable itself"),
        ClaudeLaunchError::CollidesWithGeneratedVar {
            name: API_KEY_ENV.to_string()
        }
    );
}

/// F16: no `also_launching_with` value reaches a `Debug`, on either type.
///
/// The turn key was the only value the redaction covered, and it is the one
/// value this module *knows* is a secret. A launcher passes variables this
/// module cannot classify — a proxy URL with credentials in it is the
/// ordinary case, and every suppressing input this module *does* know is now
/// refused rather than carried — and a launcher that prints either type while
/// debugging, which this module's own doc invites, put that value in the log.
/// The name still shows, because "which variables did the launch set" is what
/// a `Debug` is read for.
#[test]
fn no_declared_value_survives_a_debug_of_the_launch_or_its_environment() {
    let secret = "http://op:hunter2@proxy.internal:3128";
    let launch = launch().also_launching_with("HTTPS_PROXY", secret);
    let env = launch
        .env()
        .expect("a variable this module does not classify is carried, not refused");
    for rendered in [format!("{env:?}"), format!("{launch:?}")] {
        assert!(
            !rendered.contains(secret),
            "a Debug printed the also_launching_with credential in plaintext: {rendered:?}"
        );
        assert!(
            rendered.contains("HTTPS_PROXY"),
            "the redaction must keep the variable name: {rendered:?}"
        );
    }
    // The seam that does yield it, so the above is a redaction rather than a
    // value the launch quietly dropped.
    assert_eq!(env.get("HTTPS_PROXY"), Some(secret.to_string()));
}

/// The list a launcher enforces matches the refusals the generator makes.
///
/// Two lists that agree today and would part company on the edit that adds a
/// sixth input, which is exactly the drift that makes an enforcement
/// promise false without making any test red.
#[test]
fn must_be_unset_names_what_the_generator_would_refuse() {
    let forwarded: Vec<&str> = launch()
        .forwarding_claude_login()
        .must_be_unset()
        .iter()
        .map(|suppressor| suppressor.name)
        .collect();
    assert_eq!(forwarded.len(), OAUTH_SUPPRESSORS.len() - 1);
    assert!(forwarded.contains(&API_KEY_ENV));
    assert!(forwarded.contains(&"apiKeyHelper"));
    assert!(
        !forwarded.contains(&"CLAUDE_CODE_REMOTE"),
        "the one entry that defeats the other kind's sentinel is not this kind's \
         problem: it leaves the login this launch forwards exactly where it is"
    );

    let byok: Vec<&str> = launch()
        .must_be_unset()
        .iter()
        .map(|suppressor| suppressor.name)
        .collect();
    assert_eq!(
        byok,
        vec![
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
            "CLAUDE_CODE_REMOTE",
            "apiKeyHelper",
        ],
        "a bring-your-own-key launch must not ask a launcher to unset the \
         variable the generator is about to write -- but must ask for the one \
         that turns that variable off, and for the three that put a second \
         credential on the wire beside it (F2)"
    );
    assert!(
        !byok.contains(&API_KEY_ENV),
        "the sentinel's own variable is the one exception, and it is the whole \
         reason this list is per-input"
    );
    // And every name a launcher is asked to unset is one it *can*: the
    // settings key is the exception, and it is marked rather than mixed in.
    for suppressor in launch().forwarding_claude_login().must_be_unset() {
        assert_eq!(
            suppressor.site == SuppressorSite::SettingsKey,
            suppressor.name == "apiKeyHelper"
        );
    }
}

/// M11.2b F3: `CLAUDE_CODE_REMOTE` is the one input documented (§1.3's
/// `VV()`, client-surface.md:98-102) to defeat the `ANTHROPIC_API_KEY`
/// suppressor specifically — `!$6(CLAUDE_CODE_REMOTE)` gates only that arm,
/// every other suppressor in `VV()` is unconditional — yet it is absent from
/// [`OAUTH_SUPPRESSORS`], so a `RoundhouseKey` launch neither refuses it nor
/// lists it in [`ClaudeLaunch::must_be_unset`].
#[test]
fn claude_code_remote_defeats_the_roundhouse_key_sentinel_specifically() {
    assert_eq!(
        launch()
            .also_launching_with("CLAUDE_CODE_REMOTE", "true")
            .env()
            .expect_err("it defeats the sentinel this kind depends on"),
        ClaudeLaunchError::SentinelDefeated {
            name: "CLAUDE_CODE_REMOTE"
        },
    );
    assert!(
        launch()
            .must_be_unset()
            .iter()
            .any(|s| s.name == "CLAUDE_CODE_REMOTE"),
        "must_be_unset() under RoundhouseKey must name CLAUDE_CODE_REMOTE, not only the \
         three cloud selectors"
    );

    // CONTROL, and the half that makes the entry a *kind*-specific refusal
    // rather than one more unconditional one: a forwarded login has no
    // sentinel to defeat, so the same variable is admitted there and is
    // absent from the list that launch's launcher enforces.
    let forwarded = launch().forwarding_claude_login();
    assert!(
        !forwarded
            .must_be_unset()
            .iter()
            .any(|s| s.name == "CLAUDE_CODE_REMOTE"),
        "a forwarded login must not be asked to unset a variable that only \
         un-suppresses the login it is forwarding"
    );
    assert_eq!(
        forwarded
            .also_launching_with("CLAUDE_CODE_REMOTE", "true")
            .env()
            .expect("harmless under the kind with no sentinel")
            .get("CLAUDE_CODE_REMOTE"),
        Some("true".to_string())
    );
}

/// A base URL that already carries the served API prefix is refused.
///
/// The exact inverse of the codex sibling's `BaseUrlMissingApiPrefix`, and
/// asserted against [`API_PREFIX`] rather than against a second `"/v1"`
/// literal so that moving the served prefix moves this refusal with it.
#[test]
fn a_base_url_that_already_carries_the_api_prefix_is_refused() {
    let with_prefix = format!("https://rh.example.com{API_PREFIX}");
    assert_eq!(
        ClaudeLaunch::new(&with_prefix, TURN_KEY).expect_err("the SDK appends it itself"),
        ClaudeLaunchError::BaseUrlCarriesApiPrefix {
            base_url: with_prefix,
        }
    );
    // A trailing slash is normalised, on both the accepted and the refused
    // shape — it is what a copy-pasted address carries.
    assert_eq!(
        ClaudeLaunch::new("https://rh.example.com/", TURN_KEY)
            .expect("a trailing slash is not a mistake")
            .base_url(),
        "https://rh.example.com"
    );
    assert!(
        ClaudeLaunch::new(format!("https://rh.example.com{API_PREFIX}/"), TURN_KEY).is_err(),
        "normalising the slash must not smuggle the prefix past the refusal"
    );
    assert!(matches!(
        ClaudeLaunch::new("   ", TURN_KEY),
        Err(ClaudeLaunchError::BaseUrlIsEmpty)
    ));
    // And the URL the client will actually assemble is the one this refusal
    // is protecting, spelled once so a reader can check the two against
    // each other.
    assert_eq!(
        launch().messages_url(),
        format!("{ROOT}{API_PREFIX}/{MESSAGES_PATH}")
    );
}

/// Only a turn key builds a launch, and no refusal quotes the value.
///
/// The admin key gets a row of its own rather than being one more wrong
/// string: it is a real secret of this deployment's, an operator plausibly
/// has one to hand, and it authenticates on every surface except the one
/// this launch is for — where `turn_admission` refuses it as the wrong
/// *kind* of key on every turn, after the client has already started. Told
/// only "not a turn key", that operator checks their paste.
#[test]
fn only_a_turn_key_builds_a_launch_and_no_refusal_quotes_it() {
    assert_eq!(
        ClaudeLaunch::new(ROOT, ADMIN_KEY).expect_err("an admin key is not a turn key"),
        ClaudeLaunchError::AdminKeyIsNotATurnKey,
    );
    // A JWT and a provider key are refused by the *shape* check rather than
    // by `Secret::api_key`'s OAuth check — asserted because the two read
    // alike from outside and only one of them names the fix.
    for wrong in [
        "rh_turn_tooshort",
        "sk-ant-api03-somebody-elses",
        "eyJhbGciOiJub25lIn0.e30.x",
        "",
    ] {
        let error = ClaudeLaunch::new(ROOT, wrong).expect_err("not a turn key");
        assert_eq!(error, ClaudeLaunchError::NotATurnKey);
        // The refusal must not carry the value into whatever logs it. The
        // likeliest wrong value here is a live credential of somebody
        // else's, which is exactly the shape of the third case.
        assert!(
            wrong.is_empty() || !error.to_string().contains(wrong),
            "a refusal quoted the rejected credential: {error}"
        );
    }
}

/// A variable the generated map already names is a refusal, not an
/// overwrite.
///
/// Both directions of the collision are silent: an operator's own
/// `ANTHROPIC_BASE_URL` aims the client somewhere else, and their own
/// `ANTHROPIC_CUSTOM_HEADERS` drops the turn key — after which every turn is
/// a `401` from a deployment the operator can see is running.
#[test]
fn a_variable_the_generated_map_already_names_is_refused() {
    for name in [BASE_URL_ENV, CUSTOM_HEADERS_ENV] {
        assert_eq!(
            launch()
                .also_launching_with(name, "https://elsewhere.example.com")
                .env()
                .expect_err("the generated map already names it"),
            ClaudeLaunchError::CollidesWithGeneratedVar {
                name: name.to_string(),
            }
        );
    }
    // `ANTHROPIC_API_KEY` collides under the kind that writes it and is a
    // *suppressor* refusal under the kind that does not — two different
    // sentences for two different mistakes, which is the whole reason the
    // error type has more than one variant.
    assert!(matches!(
        launch()
            .also_launching_with(API_KEY_ENV, "sk-ant-api03-mine")
            .env(),
        Err(ClaudeLaunchError::CollidesWithGeneratedVar { .. })
    ));
    assert!(matches!(
        launch()
            .forwarding_claude_login()
            .also_launching_with(API_KEY_ENV, "sk-ant-api03-mine")
            .env(),
        Err(ClaudeLaunchError::OauthSuppressorsPresent { .. })
    ));
}

/// The sentinel is a roundhouse value that can never be a roundhouse key.
///
/// The tripwire for the one line this module and `control_config` share. If
/// the constant ever became key-shaped, `presented_key` would take it out of
/// an `Authorization` header and hand it to the resolver, where a hash
/// collision is the only thing between a published literal and a membership.
/// If it left the `rh_` namespace, the same header would be answered as "no
/// key presented" and an operator chasing a `401` would be told to add a
/// header they had already set.
#[test]
fn the_api_key_sentinel_is_namespaced_and_is_not_key_shaped() {
    assert!(ROUNDHOUSE_API_KEY_SENTINEL.starts_with("rh_"));
    assert!(
        !has_valid_key_shape(ROUNDHOUSE_API_KEY_SENTINEL),
        "the sentinel must never resolve as a key"
    );
    assert!(
        !ROUNDHOUSE_API_KEY_SENTINEL.is_empty(),
        "an empty value resolves no source"
    );
}

// ---------------------------------------------------------------------------
// M12 R-M3: the control surface's registration, and the signage beside it
// ---------------------------------------------------------------------------

/// The MCP half of the same 2.1.257 capture the fixtures above come from
/// (§5.8): five requests, `initialize` through `tools/call`, made by the real
/// client against a stub registered exactly the way this module generates one.
///
/// Read by the test below rather than transcribed, because the two facts it is
/// consulted for — the path the client posts to and the header name that
/// survives to the wire — are the two a generated registration can get wrong
/// while still parsing.
const MCP_WIRE: &str = include_str!("../../tests/fixtures/claude-2.1.257-mcp-wire.json");

/// R-M3: the generated registration is the config form the captured client
/// honoured, and it carries a variable rather than a key.
///
/// **Pinned as a whole string *and* read field by field**, because the two fail
/// differently: the byte pin catches a shape change nobody meant, and the field
/// assertions say which field a deliberate change moved. The whole-string pin
/// is what a launcher's snapshot then renders, so it moving is a change to what
/// an operator reads in a dry run as much as to what the client is handed.
#[test]
fn the_mcp_registration_is_the_config_form_the_captured_client_honoured() {
    let registration = launch().mcp_registration();
    assert_eq!(
        registration,
        r#"{"mcpServers":{"roundhouse":{"headers":{"x-roundhouse-key":"${ROUNDHOUSE_API_KEY}"},"type":"http","url":"http://127.0.0.1:8080/mcp"}}}"#
    );

    let parsed: serde_json::Value =
        serde_json::from_str(&registration).expect("the registration is JSON the client parses");
    let servers = parsed["mcpServers"]
        .as_object()
        .expect("a `mcpServers` object");
    assert_eq!(
        servers.keys().collect::<Vec<_>>(),
        vec![mcp_server_name()],
        "one server, named so its tools come back under this deployment's own \
         namespace: {registration}"
    );
    let server = &servers[mcp_server_name()];
    assert_eq!(server["type"].as_str(), Some("http"));
    assert_eq!(
        server["url"].as_str(),
        Some(launch().mcp_url().as_str()),
        "the registration and `mcp_url` are one derivation"
    );
    assert_eq!(
        server["headers"]
            .as_object()
            .expect("a headers object")
            .len(),
        1,
        "one header: every additional one is a name that overrides whatever the \
         client would otherwise send under it"
    );
    assert_eq!(
        server["headers"][TURN_KEY_HEADER].as_str(),
        Some(format!("${{{DEFAULT_KEY_ENV}}}").as_str()),
        "the key rides `${{VAR}}`, which the client expands out of the \
         environment this module's own map put the key in"
    );

    // The two facts the capture settles, against the capture. A registration
    // that named the wrong path or the wrong header parses perfectly and
    // produces a client that reaches nothing.
    let wire: serde_json::Value = serde_json::from_str(MCP_WIRE).expect("the capture is JSON");
    let requests = wire.as_array().expect("a list of requests");
    assert!(
        requests.len() >= 5,
        "the capture is the whole handshake: {}",
        requests.len()
    );
    for request in requests {
        assert_eq!(
            request["path"].as_str(),
            Some(MCP_MOUNT_PATH),
            "the real client posted every MCP request to the path this \
             registration has to name"
        );
        assert!(
            request["headers"][TURN_KEY_HEADER].is_string(),
            "the header the registration declares reached the wire on request \
             {}: {request}",
            request["n"]
        );
    }
}

/// The registration names a variable, so no rendering of it can hold the key.
///
/// Asserted over the whole generated argv rather than over the JSON alone: the
/// signage is in there too, and the argv is what a launcher prints, logs and
/// puts in a process listing that any other user of the box can read.
#[test]
fn no_generated_argument_carries_the_turn_key() {
    let launch = launch();
    for argument in launch.leading_argv() {
        assert!(
            !argument.contains(TURN_KEY),
            "the turn key is in the generated argv: {argument}"
        );
    }
    // The control: the key *is* somewhere, and it is the one place the module
    // doc says it is.
    assert!(
        env_of(&launch)[CUSTOM_HEADERS_ENV].contains(TURN_KEY),
        "the header block is where the key rides"
    );
}

/// The variable named is the launcher's own, and naming another moves nothing
/// else.
///
/// The failure this prevents is silent and total: a launcher that exports the
/// key under one name while the registration reads another generates `${…}`
/// expanding to nothing, and every control call arrives with an empty turn key
/// — a `401` on the control surface only, while every inference turn keeps
/// answering.
#[test]
fn the_registration_reads_the_variable_the_launcher_exports() {
    let renamed = launch().with_key_env("DEPLOY_TURN_KEY");
    assert_eq!(renamed.key_env(), "DEPLOY_TURN_KEY");
    assert_eq!(
        renamed.mcp_registration(),
        launch()
            .mcp_registration()
            .replace(DEFAULT_KEY_ENV, "DEPLOY_TURN_KEY"),
        "the variable is the only thing that moves"
    );
    // And the environment map is untouched by it: the header block holds the
    // key itself, because that variable is not expanded (§1.6).
    assert_eq!(env_of(&renamed), env_of(&launch()));
}

/// R-M3: the generated argv is the registration, the signage, and — only when
/// asked — the strict flag.
#[test]
fn the_generated_argv_is_two_flags_and_a_third_only_when_the_profile_asks() {
    let launch = launch();
    assert_eq!(
        launch.leading_argv(),
        vec![
            "--mcp-config".to_string(),
            launch.mcp_registration(),
            "--append-system-prompt".to_string(),
            signage(),
        ],
        "off by default: `--strict-mcp-config` drops an operator's own servers \
         wholesale, which is a deployment decision rather than part of hooking up"
    );

    let strict = launch.clone().with_strict_mcp_config(true);
    assert_eq!(
        strict.leading_argv(),
        vec![
            "--mcp-config".to_string(),
            strict.mcp_registration(),
            "--strict-mcp-config".to_string(),
            "--append-system-prompt".to_string(),
            signage(),
        ]
    );
    assert_eq!(
        launch.clone().with_strict_mcp_config(false).leading_argv(),
        launch.leading_argv(),
        "and asking for it off is asking for the default"
    );
}

/// The MCP route is a sibling of the turn route, not a child of it.
///
/// A registration carrying [`API_PREFIX`] points at a path nothing serves, and
/// the client reports that as a server it could not reach rather than as a
/// configuration mistake — the failure `codex_launch::mcp_endpoint` exists to
/// prevent on the other client, reached here from the other direction because
/// this launch's `base_url` is already the root.
#[test]
fn the_mcp_url_is_mounted_at_the_root_beside_the_turn_route() {
    let launch = launch();
    assert_eq!(launch.mcp_url(), format!("{ROOT}{MCP_MOUNT_PATH}"));
    assert_eq!(
        launch.messages_url(),
        format!("{ROOT}{API_PREFIX}/messages")
    );
    assert!(
        !launch.mcp_url().contains(API_PREFIX),
        "{}",
        launch.mcp_url()
    );
}
