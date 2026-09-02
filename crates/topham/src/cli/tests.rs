// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The dispatch itself, not just the functions it calls.
//!
//! Every subcommand below `run` already has its own suite —
//! [`launch::run`], [`relay::run`] and [`plan::resolve`] are exercised
//! directly by `launch/tests.rs`, `relay/tests.rs` and `plan/tests.rs` — but
//! nothing anywhere had driven `run` itself before this file, which is the
//! one place that ever assembles a subcommand's *inputs* the way an operator's
//! shell does: a profile loaded from disk, an admin key read from the
//! environment, and an argv matched to an enum variant. A generator getting
//! something right in isolation says nothing about the ten lines of wiring
//! around it.
//!
//! # Why `Command::Mint` gets a real admin router
//!
//! The one property this file exists to pin is a negative: `topham mint`
//! prints the secret and **touches nothing else on disk**. A profile is the
//! one file in this crate's whole vocabulary a secret must never reach (see
//! `crate::profile`'s module doc), and the dispatch arm that calls `mint` is
//! also the one arm that already holds an open `Profile` when the secret
//! comes back — the shortest possible distance between a live key and the
//! file that must never carry one. Proving "the file is unchanged" needs a
//! secret that was actually minted, which needs a real router for the reason
//! `mint/tests.rs` gives: a double would only prove this file can parse a
//! body it wrote itself.

use std::sync::Arc;

use roundhouse_core::control::{MemorySpendLedger, SpendLedger};
use roundhouse_core::metrics::{MetricsConfig, MetricsRecorder, ShadowPricing};
use roundhouse_core::now_ms;
use roundhouse_core::routing::{Candidate, Target};
use roundhouse_server::{
    ControlDirectory, ControlPlaneConfig, CrossChecks, MemoryDirectoryStore, admin_api,
};
use serde_json::json;

use super::*;
use crate::mint::ADMIN_KEY_ENV;
use crate::profile::{Agent, Profile};
use crate::test_support::scratch;

const PROJECT: &str = "acme";
const USER: &str = "ada";

fn admin_key() -> String {
    format!("rh_admin_{:A<43}", "root")
}

fn sha256_hex(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(secret.as_bytes()))
}

fn deployment() -> axum::Router {
    let file = ControlPlaneConfig::from_json(
        &json!({
            "projects": [],
            "users": [],
            "keys": [],
            "admin_keys": [sha256_hex(&admin_key())],
        })
        .to_string(),
        "topham cli fixture",
    )
    .expect("the fixture control plane compiles");
    let directory = Arc::new(
        ControlDirectory::new(
            file,
            "ROUNDHOUSE_CONTROL_PLANE",
            Arc::new(MemoryDirectoryStore::new()),
            CrossChecks::new(reachable(), None),
            now_ms(),
        )
        .expect("a file that compiles at boot compiles here"),
    );
    let ledger: Arc<dyn SpendLedger> = Arc::new(MemorySpendLedger::new());
    admin_api::admin_router(
        directory,
        ledger,
        Arc::new(MetricsRecorder::new()),
        Arc::new(MetricsConfig::new(ShadowPricing::new(Vec::new()))),
    )
}

fn reachable() -> Vec<Candidate> {
    vec![Candidate {
        target: Target::Frontier {
            provider: "anthropic".into(),
            model: "claude".into(),
        },
        expected_prefill_tokens: 1_024.0,
        matched_prefix_tokens: 0,
        expected_ttft_ms: 1.0,
        expected_cost_usd: 0.0,
        quality_prior: 0.95,
        load: None,
    }]
}

/// A served deployment, kept alive by the runtime the caller holds.
struct Served {
    root: String,
    _runtime: tokio::runtime::Runtime,
}

fn serve() -> Served {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("a runtime");
    let root = runtime.block_on(async {
        let app = deployment();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let addr = listener.local_addr().expect("the bound address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = reqwest::Client::new();
        let root = format!("http://{addr}");
        for (method, path, body) in [
            (
                reqwest::Method::POST,
                "/v1/admin/projects".to_string(),
                json!({ "id": PROJECT, "name": "Acme Corp" }),
            ),
            (
                reqwest::Method::POST,
                "/v1/admin/users".to_string(),
                json!({ "id": USER }),
            ),
            (
                reqwest::Method::PUT,
                format!("/v1/admin/projects/{PROJECT}/members/{USER}"),
                json!({ "role": "member" }),
            ),
        ] {
            let response = client
                .request(method, format!("{root}{path}"))
                .header("authorization", format!("Bearer {}", admin_key()))
                .json(&body)
                .send()
                .await
                .expect("the admin plane answers");
            assert!(
                response.status().is_success(),
                "{path}: {}",
                response.text().await.unwrap_or_default()
            );
        }
        root
    });
    Served {
        root,
        _runtime: runtime,
    }
}

/// The environment this suite runs against: two directories under a scratch
/// root and nothing else.
///
/// Its own rather than [`crate::test_support::env`], and that is the half of
/// F17 the refutation kept: this suite mints against a real router on an
/// ephemeral port, so it has no fixed deployment root and no turn key to
/// export — the shared fixture's whole content is things this file would have
/// to override.
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

/// R-T3's whole promise, pinned at the dispatch layer rather than only in
/// `mint::mint`'s own suite: minting through `topham mint` prints the export
/// line and leaves the profile it was minted for byte-for-byte unchanged.
///
/// This is the test the mutation in the M11.3 refute round would have failed:
/// a `Command::Mint` arm that appended the freshly minted secret to the
/// profile's own file passed every other test in this crate, because nothing
/// before this file ever drove `run` far enough to read the file back.
#[test]
fn minting_through_the_dispatch_prints_the_key_and_leaves_the_profile_file_untouched() {
    let served = serve();
    let root = scratch("cli-mint");
    let env = {
        let mut env = env_at(&root);
        env.insert(ADMIN_KEY_ENV.to_string(), admin_key());
        env
    };

    let profile = Profile::new(Agent::Claude, &served.root);
    let path = profile
        .save(&env, "work")
        .expect("the fixture profile saves");
    let before = std::fs::read(&path).expect("the just-written profile reads back");

    let cli = Cli {
        command: Some(Command::Mint {
            profile: "work".to_string(),
            project: PROJECT.to_string(),
            user: USER.to_string(),
        }),
    };
    let mut out = Vec::new();
    run(cli, &env, &mut out).expect("the deployment mints under a membership its own API created");
    let printed = String::from_utf8(out).expect("the export line is UTF-8");

    assert!(
        printed.contains(&format!("export {}=rh_turn_", profile.key_env)),
        "the export line is the whole point of the subcommand: {printed}"
    );

    let after = std::fs::read(&path).expect("the profile still reads back");
    assert_eq!(
        before, after,
        "`topham mint` writes nothing -- the secret it printed above must not have reached the \
         profile file it read `work` out of"
    );

    // And the file that would refuse to load if it did carry one: the surest
    // check that "unchanged" really means "still refusable", not merely
    // "byte-identical to a copy this test also forgot to update".
    Profile::load(&env, "work").expect("a profile `mint` did not touch still loads clean");
}

/// F5 (M11.3 thermo-nuclear review): `error_chain`'s dedup arm compares
/// `messages.last() != Some(&message)`, which only ever catches an *equal*
/// repeat. `ProfileError::Malformed`'s own `#[error(...)]` inlines
/// `{source}` (see `profile.rs`), so its message already contains the toml
/// parser's message as a substring -- outer ⊃ inner, never outer == inner --
/// and the equality check never fires. `CliError::Profile` is
/// `#[error(transparent)]`, so `CliError::from(profile_error).to_string()`
/// forwards to the same inlined text, and `.source()` forwards to the
/// same toml error one level down: nothing here is the "transparent wrapper
/// adds a chain link" case the function's doc blames.
///
/// Fixed at the variant rather than in the walk: nothing that carries a
/// `#[source]` inlines it in its own message any more, so the chain is one
/// message per layer and the equality dedup that could never fire is gone.
#[test]
fn f5_malformed_toml_reports_the_parsers_complaint_exactly_once() {
    let profile_error =
        Profile::from_toml("this is not [ valid toml", "work").expect_err("garbage is not TOML");
    assert!(
        matches!(profile_error, ProfileError::Malformed { .. }),
        "{profile_error:#?}"
    );

    let cli_error = CliError::from(profile_error);
    let chain = error_chain(&cli_error);

    assert_eq!(
        chain.len(),
        1,
        "the outer message already embeds the toml parser's own text via `{{source}}` in its own \
         Display, so the second chain entry is the same sentence again, not a second cause: \
         {chain:#?}"
    );
}

/// Control for F5, in the same file so the contrast is visible in one diff:
/// three genuinely transparent wrappers over a source-less leaf really do
/// collapse to one message, because there each link's `Display` is *only*
/// the wrapped error's `Display` -- nothing is inlined a second time -- so
/// the equality check the function actually implements is exactly right for
/// this shape. This is the case `error_chain`'s doc comment is describing;
/// F5's claim is that it is not the case that ever needs dedup in this
/// crate.
#[test]
fn transparent_wrappers_over_a_source_less_leaf_collapse_to_one_message() {
    #[derive(Debug, thiserror::Error)]
    #[error("the leaf broke")]
    struct Leaf;

    #[derive(Debug, thiserror::Error)]
    #[error(transparent)]
    struct Middle(#[from] Leaf);

    #[derive(Debug, thiserror::Error)]
    #[error(transparent)]
    struct Outer(#[from] Middle);

    let wrapped = Outer(Middle(Leaf));

    let chain = error_chain(&wrapped);
    assert_eq!(
        chain.len(),
        1,
        "three purely transparent wrappers over a source-less leaf must collapse to the leaf's \
         one message: {chain:#?}"
    );
    assert_eq!(chain[0], "the leaf broke");
}

/// F19 (M11.3 thermo-nuclear review, maintainability lens): `Plan`, `Launch`
/// and `Relay` are each one call into another module -- `plan::resolve`,
/// `launch::run`, `relay::run` -- so this module's own doc comment ("every
/// subcommand here is one function in another module") holds for them.
/// `Mint`'s arm additionally reads `ADMIN_KEY_ENV`, validates it non-empty,
/// and formats its two output lines inline in `run` (`cli.rs:178-199`),
/// because `mint.rs` owns no entry point for that contract -- only
/// `mint_url`/`mint`/`export_line` and the constant itself.
///
/// Now that `mint::run` owns it, the contract is driven where it lives: no
/// `Cli`, no dispatch, no router — an empty admin key is refused by the module
/// that knows what an admin key is for.
#[test]
fn f19_mint_owns_the_admin_key_read_and_refuses_a_blank_one() {
    /// A transport that fails the test if it is ever reached: the point is
    /// that the refusal happens *before* a request is built.
    struct NeverPosts;
    impl mint::AdminTransport for NeverPosts {
        fn post(&self, url: &str, _admin_key: &str) -> Result<(u16, String), MintError> {
            panic!("no admin key means no request at all, and this one reached {url}");
        }
    }

    let profile = Profile::new(Agent::Claude, "http://127.0.0.1:8080");
    for admin_key in ["", "   "] {
        let env = EnvMap::from([(ADMIN_KEY_ENV.to_string(), admin_key.to_string())]);
        let error = mint::run(&env, &profile, PROJECT, USER, &NeverPosts, &mut Vec::new())
            .expect_err("an exported-but-blank admin key is a missing one");
        assert!(matches!(error, MintError::AdminKeyMissing), "{error:?}");
    }

    assert!(
        matches!(
            mint::run(
                &EnvMap::new(),
                &profile,
                PROJECT,
                USER,
                &NeverPosts,
                &mut Vec::new()
            ),
            Err(MintError::AdminKeyMissing)
        ),
        "and so is an unset one"
    );
}

/// The print order, which is the other half of what moved: the comment line
/// first and the export line alone on its own, so that copying the second one
/// into a shell carries no `#`.
#[test]
fn f19_the_export_line_is_printed_alone_under_the_minted_key_comment() {
    let served = serve();
    let root = scratch("cli-mint-run");
    let mut env = env_at(&root);
    env.insert(ADMIN_KEY_ENV.to_string(), admin_key());
    let profile = Profile::new(Agent::Claude, &served.root);

    let mut out = Vec::new();
    mint::run(
        &env,
        &profile,
        PROJECT,
        USER,
        &mint::HttpTransport,
        &mut out,
    )
    .expect("the deployment mints under a membership its own API created");

    let printed = String::from_utf8(out).expect("the two lines are UTF-8");
    let lines = printed.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "{printed}");
    assert!(lines[0].starts_with("# minted "), "{printed}");
    assert!(
        lines[1].starts_with(&format!("export {}=rh_turn_", profile.key_env)),
        "{printed}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// F5's other direction, as a property over the errors this crate really
/// builds: no message in a chain repeats what the next one says. An outer
/// message that inlined its own `{source}` would fail this without ever being
/// *equal* to it, which is the shape the old equality dedup could not see.
#[test]
fn f5_no_message_in_a_chain_contains_the_one_below_it() {
    let io = || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");
    let chains: Vec<CliError> = vec![
        CliError::Output(io()),
        LaunchError::Write {
            path: std::path::PathBuf::from("/op/data/topham/work/codex-home/config.toml"),
            source: io(),
        }
        .into(),
        LaunchError::Exec {
            program: "claude".to_string(),
            source: io(),
        }
        .into(),
        RelayError::PreflightSpawn {
            program: "nemo-relay".to_string(),
            source: io(),
        }
        .into(),
        MintError::Unreachable {
            url: "http://127.0.0.1:8080/v1/admin".to_string(),
            source: Box::new(io()),
        }
        .into(),
        // The two variants the first pass left behind, because each lived in a
        // file another stage owned: both carried a `#[source]` *and* spelled it
        // into their own sentence.
        ProfileError::Io {
            path: std::path::PathBuf::from("/op/config/topham/profiles/work.toml"),
            source: io(),
        }
        .into(),
        TuiError::Terminal(io()).into(),
    ];

    for error in &chains {
        let chain = error_chain(error);
        for pair in chain.windows(2) {
            assert!(
                !pair[0].contains(&pair[1]),
                "an error's own message must not restate its cause -- the chain prints both: \
                 {chain:#?}"
            );
        }
    }
}
