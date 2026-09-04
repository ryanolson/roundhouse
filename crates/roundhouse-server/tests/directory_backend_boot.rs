// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M16.1, R-D8: the composition root's own choice of directory backend, and
//! the boot it is allowed to stop.
//!
//! The fifth family in the shape the other four already have. `shared_backend`
//! has one match and one rule — `ROUNDHOUSE_REDIS_URL` — and
//! [`fair_use_backend_boot.rs`](../tests/fair_use_backend_boot.rs) and
//! [`correlation_backend_boot.rs`](../tests/correlation_backend_boot.rs) exist
//! because two earlier reviews found that rule silently detached from the
//! wiring it claimed to follow, in a `[[bin]]` nothing could call. This file
//! is the directory's, and it calls [`open`] rather than re-typing what
//! `open` does: a mutation that wired `MemoryDocumentStore` into the `Shared`
//! arm has to make something here go red, or the whole exercise is a copy of
//! the composition root agreeing with itself.
//!
//! Two claims, and they are the two halves of R-D8:
//!
//! 1. **Tenancy outlives the process.** A project created and archived through
//!    a directory over the Redis `open` built is still archived — its id still
//!    refused, `identity_collision` — for a *second* directory opened over the
//!    same Redis after the first is dropped. That is a restart, in every way
//!    that matters to `ProjectRecord::archived_at_ms`: the tombstone that keeps
//!    a closed id retired either survived or it did not.
//!    `tests/admin_api.rs::recreating_an_archived_project_after_a_restart_inherits_its_spend`
//!    is the same claim over the admin HTTP surface with an in-memory document
//!    store; this is it against the store a deployment actually runs.
//!
//! 2. **A directory the store cannot read stops the boot.** A `dir` key some
//!    other writer owns is refused rather than read as the empty directory,
//!    and the refusal reaches the boot as a typed
//!    [`StoreFailure::Unavailable`] naming the key. Reading it as empty is the
//!    failure this fails closed against: it would authenticate every request
//!    against a plane missing every project, member and key the admin plane
//!    ever created, and the next admin write would commit that emptiness on
//!    top of whatever is really there.
//!
//! **A fresh [`KeyNamespace`] per test**, not a fresh project id inside one.
//! This family has a single key for the whole deployment (R-D6), so there is
//! nothing inside it to make fresh — the isolation moves outward, exactly as
//! it does in the store crate's own `directory_contract.rs`.
//!
//! Gated like every Redis-touching suite in this tree: `#[ignore]`, opted into
//! with `--include-ignored`, and a missing `ROUNDHOUSE_TEST_REDIS_URL` fails
//! loudly rather than skipping quietly.

use std::sync::Arc;

use roundhouse_core::now_ms;
use roundhouse_fleet::{StaticFrontierCatalog, WireProtocol};
use roundhouse_server::control_config::boot_directory;
use roundhouse_server::control_config::crosscheck::CrossChecks;
use roundhouse_server::shared_backend::open;
use roundhouse_server::test_support::frontier_spec;
use roundhouse_server::{ControlDirectory, DirectoryError, DirectoryMutation};
use roundhouse_store_redis::KeyNamespace;
use roundhouse_store_redis::test_support::{directory_records_key, url_from_env};

mod common;
use common::{admin_key, control_plane, sha256_hex};

/// The file half: an admin plane and nothing else, so every project below is
/// `Provenance::Admin` and therefore something the API — and its
/// tombstone — actually owns. A file-declared project is projected from the
/// file on every boot and would survive a restart whatever the store did,
/// which is the one shape that would make this suite pass for the wrong
/// reason.
fn boot_file() -> roundhouse_server::control_config::ControlPlaneFile {
    let config = control_plane(
        serde_json::json!({
            "projects": [],
            "users": [],
            "keys": [],
            "admin_keys": [sha256_hex(&admin_key("root"))],
        }),
        "R-D8 fixture",
    );
    roundhouse_server::control_config::ControlPlaneFile {
        config,
        path: "ROUNDHOUSE_CONTROL_PLANE".to_string(),
        // No real file backs this fixture, so no real bytes to hash; neither
        // test in this file asserts on the fingerprint, only on whether the
        // boot fails closed.
        sha256: sha256_hex("R-D8 fixture"),
    }
}

fn reachable() -> Vec<roundhouse_core::routing::Candidate> {
    vec![roundhouse_core::routing::Candidate {
        target: roundhouse_core::routing::Target::Frontier {
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

/// A namespace nothing else in this run shares. `KeyNamespace::new` refuses
/// `:`, braces and whitespace, and a hyphenated UUID contains none of them.
fn fresh_namespace() -> KeyNamespace {
    KeyNamespace::new(format!("dirboot-{}", uuid::Uuid::new_v4()))
        .expect("a hyphenated UUID contains no character the namespace rejects")
}

/// One node's whole boot, exactly as `main.rs` performs it: `open` chooses the
/// backends, and the directory is built **by `control_config::boot_directory`,
/// the same function `main.rs` calls** — not from a handle or a match this
/// file re-derives itself.
///
/// That is the entire point of the file. Connecting a `RedisDocumentStore`
/// and matching on the file here by hand would prove that the Redis store
/// works — which the store crate's own contract suite already proves — and
/// would say nothing about whether the composition root reaches for it, or
/// about whether a fallback slipped into the fail-closed decision
/// `boot_directory` makes (M16.1 review, F1): calling the function main.rs
/// calls, rather than a copy of it, is what makes a mutation of that decision
/// something this test can catch at all.
async fn boot(namespace: &KeyNamespace) -> Result<Arc<ControlDirectory>, DirectoryError> {
    let backends = open(Some(&url_from_env()), namespace)
        .await
        .expect("the test Redis named by ROUNDHOUSE_TEST_REDIS_URL must be reachable");
    boot_directory(
        Some(boot_file()),
        Arc::clone(backends.directory()),
        // Empty: neither test that calls this helper reads the fingerprint's
        // catalog axis, only whether the boot succeeds or is refused --
        // `directory_boot_fingerprint_catalog_half_never_reflects_the_real_catalog`
        // below builds its own catalog and calls `boot_directory` directly for
        // that reason.
        &StaticFrontierCatalog::default(),
        CrossChecks::new(reachable(), None),
        now_ms(),
    )
    .await
}

/// **The unlock condition, end to end and through the composition root.**
///
/// 1. Boot a node over the Redis this deployment names, taking its document
///    store from [`open`].
/// 2. Create a project through the same `DirectoryMutation` the admin plane's
///    `create_project` route applies, and archive it through the same one
///    `DELETE /v1/admin/projects/{id}` applies.
/// 3. Drop that node entirely — every handle, every in-memory copy of the
///    records.
/// 4. Boot a second node over the same Redis and offer it the same project id.
///
/// It must be refused `identity_collision`. Before this rung the directory was
/// rebuilt from nothing on every boot no matter what `ROUNDHOUSE_REDIS_URL`
/// said, so step 4 succeeded — and the new tenant inherited the old one's rows
/// in the durable spend ledger, which had survived.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn an_archived_project_stays_archived_across_a_restart_of_the_wired_directory() {
    let namespace = fresh_namespace();
    let project = format!("shutco-{}", uuid::Uuid::new_v4());

    {
        let first = boot(&namespace)
            .await
            .expect("the file alone compiles, since it is what a boot would have loaded");
        first
            .apply(
                DirectoryMutation::CreateProject {
                    entry: roundhouse_server::control_config::ProjectEntry {
                        id: project.clone(),
                        name: None,
                        policy: None,
                        budget: None,
                        fair_use: None,
                        validate: None,
                        credentials: None,
                        tiers: None,
                    },
                },
                now_ms(),
            )
            .await
            .expect("a fresh project id in a fresh namespace");
        first
            .apply(
                DirectoryMutation::ArchiveProject {
                    id: project.clone(),
                },
                now_ms(),
            )
            .await
            .expect("archiving a project this node just created");
    }
    // The first node is gone here: the block above owns every reference to it,
    // so nothing that follows can be reading its memory by accident.

    let second = boot(&namespace)
        .await
        .expect("the second boot compiles the file plus whatever the store held");
    let refused = second
        .apply(
            DirectoryMutation::CreateProject {
                entry: roundhouse_server::control_config::ProjectEntry {
                    id: project.clone(),
                    name: None,
                    policy: None,
                    budget: None,
                    fair_use: None,
                    validate: None,
                    credentials: None,
                    tiers: None,
                },
            },
            now_ms(),
        )
        .await
        .expect_err(
            "R-D8: the tombstone of a project archived before the restart must still hold its \
             id. If this create succeeds, the directory the composition root wired is not the \
             one ROUNDHOUSE_REDIS_URL chose, and a new tenant is about to be joined to the \
             archived one's spend history in the ledger that did survive",
        );
    assert!(
        matches!(refused, DirectoryError::IdentityCollision { .. }),
        "the refusal must be the identity one an operator can act on, not a store error: \
         {refused}"
    );

    // The control that keeps the assertion above from passing for the wrong
    // reason: an id nothing ever archived is still free in this namespace, so
    // the refusal is about the tombstone rather than about the second boot
    // refusing every create.
    second
        .apply(
            DirectoryMutation::CreateProject {
                entry: roundhouse_server::control_config::ProjectEntry {
                    id: format!("{project}-never-archived"),
                    name: None,
                    policy: None,
                    budget: None,
                    fair_use: None,
                    validate: None,
                    credentials: None,
                    tiers: None,
                },
            },
            now_ms(),
        )
        .await
        .expect("an id no boot has ever taken is still free after the restart");
}

/// **A directory the store cannot read stops the boot, naming the key.**
///
/// The `dir` key is set to a plain string — a hand-edit, a key reused from
/// something else, a foreign writer sharing the namespace. Redis then answers
/// `WRONGTYPE` to the `HMGET` `load` performs, the store maps it to
/// `DocumentStoreError::Unavailable`, the adapter maps that one-for-one onto
/// `StoreFailure::Unavailable`, and `ControlDirectory::new` — whose first load
/// *is* the boot check — refuses to exist.
///
/// Fail closed is the ruling and the alternative is the reason for it: a node
/// that read an unreadable directory as the empty one would serve a plane with
/// no admin-created project, member or key in it, authenticate every request
/// against that, and then commit the emptiness over the top on the first admin
/// write.
///
/// The reason must name the key, because the key is the thing an operator goes
/// and looks at; the store crate pins the same refusal one layer down in
/// `a_key_this_store_did_not_write_is_refused_rather_than_overwritten`, and
/// this is that refusal arriving where it stops a process.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_directory_key_this_deployment_cannot_read_refuses_the_boot() {
    let namespace = fresh_namespace();
    let key = directory_records_key(&namespace);

    // A raw client, because no roundhouse store handle can write this and a
    // store that could would be a store that could do it by accident.
    let mut raw = redis::Client::open(url_from_env().as_str())
        .expect("a client over the test Redis URL")
        .get_multiplexed_async_connection()
        .await
        .expect("the test Redis named by ROUNDHOUSE_TEST_REDIS_URL must be reachable");
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg("not a hash this deployment wrote")
        .query_async(&mut raw)
        .await
        .expect("the fixture writes the foreign key it is about");

    // `expect_err` needs `Debug` on the success half, and a whole directory is
    // not something to derive that on for one test's sake -- so the success
    // arm is discarded here explicitly, with the message the assertion would
    // have carried.
    let refused = match boot(&namespace).await {
        Ok(_) => panic!(
            "R-D8: a Redis that serves the other four families and cannot answer for the \
             directory must stop the boot. Starting anyway serves a plane compiled from the \
             file alone -- every admin-created project, member and key silently absent -- \
             and the next admin write commits that emptiness over the top"
        ),
        Err(refused) => refused,
    };
    let reason = refused.to_string();
    assert!(
        matches!(refused, DirectoryError::Store(_)),
        "the refusal must be typed as a store failure rather than as a bad change, since \
         nothing about a caller's request produced it: {reason}"
    );
    assert!(
        reason.contains(&key),
        "the reason must name the key an operator goes and looks at: {reason}"
    );

    // The control, in the same namespace: with the foreign key gone the very
    // same boot succeeds, so the refusal above is about what is at that key
    // and not about the namespace, the file or the fixture.
    let _: () = redis::cmd("DEL")
        .arg(&key)
        .query_async(&mut raw)
        .await
        .expect("removing the fixture's own key");
    boot(&namespace)
        .await
        .expect("with nothing foreign at the key, the same boot is the empty directory");
}

/// **M16.1 review, F2: the boot composition threads the real catalog into
/// the stored fingerprint.**
///
/// `StaticFrontierCatalog::identities` (`roundhouse-fleet/src/frontier.rs`)
/// is the real computation -- sort, dedup, `"{provider}/{model}"` -- and it
/// used to be a private `fn` inside the `[[bin]] roundhouse` target that only
/// a unit test living beside it could reach; this file's own `boot()` helper,
/// like every boot in this suite, handed `boot_directory` a hand-written
/// `Vec::new()` for the catalog axis because there was no way to thread a
/// different value through it. Moving the computation into the library (this
/// finding's fix) is what lets this suite build a real
/// `StaticFrontierCatalog`, hand it to `boot_directory` -- the very function
/// `main.rs::serve` calls -- and check that what lands in Redis is what that
/// catalog actually identifies, rather than asserting the absence of a
/// function.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn directory_boot_fingerprint_catalog_half_never_reflects_the_real_catalog() {
    let namespace = fresh_namespace();

    // A catalog that names the same target `reachable()` (above) already
    // uses, plus a duplicate and a second provider, so dedup and sort both
    // have something to do -- not a strawman invented for the test.
    let catalog = StaticFrontierCatalog::new(vec![
        frontier_spec("anthropic", "claude", WireProtocol::AnthropicMessages),
        frontier_spec("anthropic", "claude", WireProtocol::AnthropicMessages),
        frontier_spec(
            "openrouter",
            "capable-m",
            WireProtocol::OpenAiChatCompletions,
        ),
    ]);
    let real_catalog_identities = catalog.identities();

    // Exactly the boot this file performs everywhere else: `open` chooses the
    // backend, `boot_directory` is the same function `main.rs::serve` calls,
    // and the catalog is the real one built above rather than `boot()`'s own
    // `StaticFrontierCatalog::default()`.
    let backends = open(Some(&url_from_env()), &namespace)
        .await
        .expect("the test Redis named by ROUNDHOUSE_TEST_REDIS_URL must be reachable");
    let directory = boot_directory(
        Some(boot_file()),
        Arc::clone(backends.directory()),
        &catalog,
        CrossChecks::new(reachable(), None),
        now_ms(),
    )
    .await
    .expect("the file alone compiles, since it is what a boot would have loaded");

    // `Managed::new` only loads and compiles on boot -- it commits nothing
    // (see `directory.rs`, `note_divergence`'s version-zero skip, which is
    // exactly for a store nothing has written to yet). A mutation is the
    // first thing that stamps `compiled_under` into Redis, so one is applied
    // here purely to produce the document this test reads back.
    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: roundhouse_server::control_config::ProjectEntry {
                    id: format!("f2-fingerprint-probe-{}", uuid::Uuid::new_v4()),
                    name: None,
                    policy: None,
                    budget: None,
                    fair_use: None,
                    validate: None,
                    credentials: None,
                    tiers: None,
                },
            },
            now_ms(),
        )
        .await
        .expect("a fresh project id in a fresh namespace commits and stamps the fingerprint");

    // Read the envelope this commit just wrote back out of Redis, raw --
    // bypassing the typed directory entirely, so this checks what actually
    // landed rather than an in-process copy.
    let stored = backends
        .directory()
        .load()
        .await
        .expect("the document boot_directory just committed")
        .document
        .expect("a document exists once boot_directory has written one");
    let envelope: serde_json::Value =
        serde_json::from_slice(&stored).expect("boot_directory writes valid JSON (R-D7)");
    let stored_catalog: Vec<String> = envelope["compiled_under"]["catalog"]
        .as_array()
        .expect("CompiledUnder::catalog is #[serde(default)] and always written")
        .iter()
        .map(|v| {
            v.as_str()
                .expect("catalog identities are strings")
                .to_string()
        })
        .collect();

    assert_eq!(
        stored_catalog, real_catalog_identities,
        "F2: the directory-boot integration suite's stored fingerprint must carry the real \
         catalog identities -- boot_directory is fed the real StaticFrontierCatalog here, so a \
         mutation to StaticFrontierCatalog::identities's sort/dedup/format is visible to this \
         suite."
    );
}
