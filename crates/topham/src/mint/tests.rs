// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Minting, against the deployment's own admin router.
//!
//! **A real router on a real loopback socket, not a double.** What a double
//! would prove is that this module can parse a body this module wrote. What
//! this proves is the thing that can actually break: the route path, the header
//! the admin key rides on, the `201`, and the field names in the response are
//! all read from `roundhouse-server`, so any of them moving fails here rather
//! than at an operator's first mint.
//!
//! The runtime shape is deliberate. [`HttpTransport`] builds its own
//! current-thread runtime, which panics if it is called from inside one — so
//! these are plain `#[test]`s that keep a multi-threaded runtime alive in the
//! background to serve, and mint from the test thread exactly as `fn main`
//! does.

use std::sync::Arc;

use roundhouse_core::control::{MemorySpendLedger, SpendLedger};
use roundhouse_core::metrics::{MetricsConfig, MetricsRecorder, ShadowPricing};
use roundhouse_core::now_ms;
use roundhouse_core::routing::{Candidate, Target};
use roundhouse_server::{
    ControlDirectory, ControlPlaneConfig, CrossChecks, MemoryDirectoryStore, admin_api,
    has_valid_key_shape,
};
use serde_json::json;

use super::*;

/// The admin secret the fixture deployment trusts.
///
/// Padded by `format!` rather than typed, in the house fixture form
/// (`tests/common`'s `admin_key`): a hand-counted tail fails as
/// `malformed_key`, which reads as this deployment refusing its own root key
/// for a reason no assertion names.
fn admin_key() -> String {
    format!("rh_admin_{:A<43}", "root")
}

const PROJECT: &str = "acme";
const USER: &str = "ada";

fn sha256_hex(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// A deployment with an admin plane and nothing else declared.
///
/// The project and the member are created *through the API* rather than
/// declared in the file, because a membership the file owns is owned by the
/// file: minting under one is refused `409`, which is the admin plane's rule
/// and not something a launcher can work around.
async fn deployment() -> axum::Router {
    let file = ControlPlaneConfig::from_json(
        &json!({
            "projects": [],
            "users": [],
            "keys": [],
            "admin_keys": [sha256_hex(&admin_key())],
        })
        .to_string(),
        "topham mint fixture",
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
        .await
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

/// One reachable target, because the boot cross-checks refuse a deployment that
/// can route nowhere.
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

/// A served deployment, and the runtime that keeps serving it.
///
/// The runtime is returned so the caller holds it: dropping it stops the
/// worker threads, and the request would then fail as "connection refused" —
/// a failure that reads like a bug in the transport rather than in the test.
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
        let app = deployment().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let addr = listener.local_addr().expect("the bound address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // Project, user and membership: what every mint needs first.
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

/// The whole subcommand's payload: a key this deployment would itself accept,
/// and the line an operator pastes.
#[test]
fn minting_over_the_real_admin_route_yields_a_key_this_deployment_accepts() {
    let served = serve();
    let minted = mint(&served.root, PROJECT, USER, &admin_key(), &HttpTransport)
        .expect("the deployment mints under a membership its own API created");

    assert!(
        has_valid_key_shape(&minted.secret),
        "a minted secret this deployment would itself refuse: {}",
        minted.secret
    );
    assert!(minted.secret.starts_with("rh_turn_"), "{}", minted.secret);
    assert!(!minted.id.is_empty());
    assert!(
        minted.secret.ends_with(&minted.display_tail),
        "the tail is how every *other* admin read identifies this key: {minted:?}"
    );

    assert_eq!(
        export_line("ROUNDHOUSE_API_KEY", &minted.secret),
        format!("export ROUNDHOUSE_API_KEY={}", minted.secret),
        "`export`, not a bare assignment: the variable has to reach the agent, which is a child \
         process"
    );
}

/// The two variables are two kinds of secret, and the deployment says so.
///
/// A key this deployment *does* know, of the wrong kind, is the case worth
/// pinning: it separates "you pasted the wrong thing" (`403`, the launcher's
/// own `ROUNDHOUSE_API_KEY` where `ROUNDHOUSE_ADMIN_KEY` belongs) from "this
/// deployment has never heard of that secret" (`401`), and an operator's next
/// move differs between them.
#[test]
fn a_turn_key_where_the_admin_key_belongs_is_refused_as_the_wrong_kind() {
    let served = serve();
    let turn_key = mint(&served.root, PROJECT, USER, &admin_key(), &HttpTransport)
        .expect("a real turn key of this deployment's")
        .secret;

    let error = mint(&served.root, PROJECT, USER, &turn_key, &HttpTransport)
        .expect_err("a turn key administers nothing, however valid it is");
    match error {
        MintError::Refused { status, .. } => assert_eq!(
            status, 403,
            "a key of the wrong kind is refused rather than narrowed"
        ),
        other => panic!("{other:#?}"),
    }

    let unknown = format!("rh_admin_{:B<43}", "nope");
    let error = mint(&served.root, PROJECT, USER, &unknown, &HttpTransport)
        .expect_err("nothing was ever minted for that secret");
    match error {
        MintError::Refused { status, .. } => assert_eq!(status, 401),
        other => panic!("{other:#?}"),
    }
}

/// A member who does not exist is a `404`, and the message says which of the
/// three plausible causes that is.
#[test]
fn minting_for_a_member_that_does_not_exist_is_refused() {
    let served = serve();
    let error = mint(
        &served.root,
        PROJECT,
        "nobody",
        &admin_key(),
        &HttpTransport,
    )
    .expect_err("there is no such membership");
    let message = error.to_string();
    assert!(
        message.contains("404") && message.contains("the project or the member not existing"),
        "{message}"
    );
}

/// The URL is derived from the served prefix, not from a second `/v1` literal.
#[test]
fn the_mint_url_is_the_admin_route_under_the_served_prefix() {
    assert_eq!(
        mint_url("http://127.0.0.1:8080/", "acme", "ada").unwrap(),
        format!("http://127.0.0.1:8080{API_PREFIX}/admin/projects/acme/members/ada/keys")
    );
}

/// A value that is not one path segment is refused rather than encoded — see
/// [`check_segment`].
#[test]
fn a_project_or_member_that_is_not_one_path_segment_is_refused() {
    for (what, project, user) in [
        ("project", "acme/sub", "ada"),
        ("user", "acme", "ada bell"),
        ("project", "", "ada"),
        ("user", "acme", "ada?x=1"),
    ] {
        let error = mint_url("http://x", project, user)
            .expect_err("the admin routes match a single segment");
        assert!(
            matches!(&error, MintError::UnusableSegment { what: named, .. } if *named == what),
            "{error:#?}"
        );
    }
}

/// A `201` whose body is not a minted key is its own error, because it is the
/// one failure that can leave a key existing that nobody holds.
#[test]
fn a_created_response_this_launcher_cannot_read_is_not_reported_as_a_refusal() {
    struct Canned;
    impl AdminTransport for Canned {
        fn post(&self, _url: &str, _admin_key: &str) -> Result<(u16, String), MintError> {
            Ok((201, "{\"id\":\"key_1\"}".to_string()))
        }
    }
    let error = mint("http://x", "acme", "ada", &admin_key(), &Canned)
        .expect_err("a body with no `secret` is not a minted key");
    match error {
        MintError::Unreadable { status, .. } => assert_eq!(status, 201),
        other => panic!("a refusal would send the operator to check their admin key: {other:#?}"),
    }
}

/// The transport is handed the route URL and the admin key, and nothing else.
///
/// The *header spelling* is proven by the real router above; what this pins is
/// the seam — that the key `mint` forwards is the admin one it was given, and
/// that the URL it posts to is the derived route rather than the deployment
/// root.
#[test]
fn the_transport_is_given_the_route_url_and_the_admin_key() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(String, String)>>);
    impl AdminTransport for Recorder {
        fn post(&self, url: &str, admin_key: &str) -> Result<(u16, String), MintError> {
            self.0
                .lock()
                .unwrap()
                .push((url.to_string(), admin_key.to_string()));
            Ok((
                201,
                json!({ "secret": "rh_turn_x", "id": "key_1", "display_tail": "n_x" }).to_string(),
            ))
        }
    }

    let recorder = Recorder::default();
    mint("http://x", "acme", "ada", &admin_key(), &recorder).expect("the canned body parses");
    let calls = recorder.0.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![(
            format!("http://x{API_PREFIX}/admin/projects/acme/members/ada/keys"),
            admin_key()
        )]
    );
}
