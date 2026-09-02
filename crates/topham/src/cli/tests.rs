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
use crate::profile::{Agent, Profile};

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

/// A scratch directory, per the house pattern: the temp dir plus a UUID, so
/// two tests in one run never collide.
fn scratch(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("topham-cli-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("a scratch directory");
    root
}

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
    let root = scratch("mint");
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
