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
use roundhouse_server::control_config::DEFAULT_ADMISSION_CACHE_TTL_MS;
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

/// **M18, H1: the empty-Redis lineage exemption, under a real boot.**
///
/// `StoredVersion::supersedes` skips the lineage comparison at
/// `served.version == 0` — a node that has never seen a document has no
/// lineage to disagree about, so the deployment's first write is adopted
/// whichever lineage minted it, or a node booted against an empty store
/// would refuse that very first write for the life of the process. Until
/// this rung the exemption was pinned only by
/// `control_config::directory::store::tests::
/// later_means_later_in_the_same_lineage_and_anything_beats_version_zero`
/// — a unit test over the type alone, driving no real store's answer at
/// version zero. This is that exemption through a real boot: the Redis
/// store's own version-zero answer (`lineage: String::new()`, from
/// `decode_identity` in the store crate's `directory.rs`) is what a node
/// actually reads at an empty key, not a fixture's stand-in for it.
///
/// Three claims, in one deployment's lifetime:
///
/// 1. **The first write is accepted.** A node that read its own empty-store
///    boot as having claimed a lineage would refuse this `apply` as a
///    regression against a lineage nothing minted yet.
/// 2. **The next refresh past the TTL reports no regression and serves the
///    written version.** An ordinary refresh, not a proof of the
///    refresh-path exemption: node one's own `apply` in claim 1 already
///    adopted the lineage the store minted, so by the time this refresh
///    runs, `claimed.lineage` at `Managed::compiled`'s
///    `claimed.version != 0 && stored.lineage != claimed.lineage` site
///    (directory.rs, near line 1080) already equals the store's lineage —
///    the guard is never exercised at `claimed.version == 0` here, because
///    this node's own `claimed.version` is already `1`. That branch is what
///    `a_node_that_never_wrote_adopts_a_strangers_first_write_on_its_own_
///    refresh` below proves: a node that boots over an empty store and never
///    writes to it itself, refreshing past a first write another node made
///    while this one's `claimed.version` was still `0`.
/// 3. **A second node booted afterward over the same Redis agrees** — same
///    version, no divergence, and, driven by a further write from the
///    second node, no regression when the first node refreshes past it.
///    Agreement on today's version alone would also be produced by two
///    nodes that each minted their own lineage at an empty-store boot and
///    happened to be looking at the same document; only a *second* round
///    trip, refreshed without a regression, proves the two are on one
///    lineage rather than two that coincide once.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_node_booted_against_an_empty_redis_accepts_and_agrees_on_the_deployments_first_write() {
    let namespace = fresh_namespace();
    let t0 = now_ms();

    // Node 1 boots over a Redis with no `dir` key at all: the empty
    // directory, version zero, no lineage minted yet.
    let first = boot(&namespace)
        .await
        .expect("the file alone compiles, since it is what a boot would have loaded");
    assert_eq!(
        first.status().served_version,
        0,
        "a fresh namespace's directory starts at version zero"
    );

    // Claim 1: the deployment's first admin write, against that empty store.
    first
        .apply(
            DirectoryMutation::CreateProject {
                entry: roundhouse_server::control_config::ProjectEntry {
                    id: format!("h1-{}", uuid::Uuid::new_v4()),
                    name: None,
                    policy: None,
                    budget: None,
                    fair_use: None,
                    validate: None,
                    credentials: None,
                    tiers: None,
                },
            },
            t0,
        )
        .await
        .expect(
            "H1: the deployment's first write, against an empty store, must be accepted -- a \
             refusal here means the empty-Redis lineage exemption did not hold on the write path",
        );
    assert_eq!(
        first.last_regression(),
        None,
        "the node's own first write must never be recorded as a regression against itself"
    );

    // Claim 2: force a refresh well past the TTL. This is the write's own
    // node re-reading the very lineage it just minted through the store,
    // which is the shape the refresh path's version of the exemption has to
    // survive.
    let past_first_ttl = t0 + DEFAULT_ADMISSION_CACHE_TTL_MS + 5_000;
    first.plane(past_first_ttl).await;
    assert_eq!(
        first.status().served_version,
        1,
        "the refresh past the TTL must still serve the version this node itself just wrote"
    );
    assert_eq!(
        first.last_regression(),
        None,
        "H1: re-reading the store's very first lineage on a refresh must never be reported as \
         this node going backwards"
    );
    assert_eq!(
        first.status().divergence,
        None,
        "one node compiling its own write under its own inputs never diverges from itself"
    );

    // Claim 3: a second node boots afterward over the same Redis and agrees.
    let second = boot(&namespace)
        .await
        .expect("the file plus what the store now holds");
    assert_eq!(
        second.status().served_version,
        1,
        "H1: a second node booted after the first write must see the same version, or the \
         empty-Redis exemption let the two nodes diverge on the deployment's very first commit"
    );
    assert_eq!(second.last_regression(), None);
    assert_eq!(second.status().divergence, None);

    // A further write from the second node, picked up by the first node on
    // a later refresh, must not read as a regression either -- the proof
    // that the two nodes share one lineage rather than each having minted
    // its own at an empty-store boot and merely agreeing on today's version
    // by coincidence.
    second
        .apply(
            DirectoryMutation::CreateProject {
                entry: roundhouse_server::control_config::ProjectEntry {
                    id: format!("h1-second-{}", uuid::Uuid::new_v4()),
                    name: None,
                    policy: None,
                    budget: None,
                    fair_use: None,
                    validate: None,
                    credentials: None,
                    tiers: None,
                },
            },
            past_first_ttl,
        )
        .await
        .expect("a fresh project id from the second node");

    let past_second_ttl = past_first_ttl + DEFAULT_ADMISSION_CACHE_TTL_MS + 5_000;
    first.plane(past_second_ttl).await;
    assert_eq!(
        first.status().served_version,
        2,
        "the first node's refresh must pick up the second node's write"
    );
    assert_eq!(
        first.last_regression(),
        None,
        "H1: the first node refreshing past the second node's write must not read it as a \
         lineage regression -- which is what proves the two nodes agree on one lineage rather \
         than each minting its own at an empty-store boot"
    );
}

/// **M18, H1 correction: the empty-Redis lineage exemption, on the refresh
/// path itself.**
///
/// The sibling above, `a_node_booted_against_an_empty_redis_accepts_and_
/// agrees_on_the_deployments_first_write`, never drives `Managed::compiled`'s
/// own exemption (`claimed.version != 0 && stored.lineage != claimed.lineage`
/// at directory.rs, near line 1080, with the version guard skipping the
/// lineage comparison at `claimed.version == 0`): node one there always
/// mints the deployment's first lineage through its own `apply`, whose
/// separate exemption (`published.version != 0` at directory.rs ~1360) is
/// what actually fires, and by the time that test's node one ever calls
/// `plane`, its `claimed.version` is already `1`. The refresh-path guard is
/// left standing over an assertion that would pass exactly the same with it
/// deleted.
///
/// This test drives it directly: node one boots over an empty Redis and
/// then never writes to it at all, so its `claimed` state -- what a refresh
/// compares the store against -- stays at version `0`, lineage `""`, from
/// the moment it boots until the moment it refreshes. A *different* node
/// mints the deployment's first lineage in the meantime. Node one's first
/// refresh is therefore the only place in this file `claimed.version == 0`
/// reaches the refresh-path check with a `stored.lineage` that is not the
/// empty string it started with -- exactly the shape the version guard
/// exists to wave through rather than name a regression.
///
/// Two claims:
///
/// 1. **Node one's first refresh adopts the stranger's first write.** Serves
///    version 1, names no regression and no divergence. Without the
///    `claimed.version != 0 &&` guard, `"" != <the store's real lineage>`
///    would be true and this refresh would record its own adoption as a
///    lineage regression against a lineage it never claimed in the first
///    place.
/// 2. **The two nodes are now on one shared lineage, not two that merely
///    coincide.** A further write from node one, refreshed by node two,
///    still names no regression.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_node_that_never_wrote_adopts_a_strangers_first_write_on_its_own_refresh() {
    let namespace = fresh_namespace();
    let t0 = now_ms();

    // Node 1 boots over a Redis with no `dir` key: version zero, no lineage
    // minted. Unlike the sibling test, this node never calls `apply` before
    // its first refresh -- its `claimed` state stays exactly what boot left
    // it at.
    let first = boot(&namespace)
        .await
        .expect("the file alone compiles, since it is what a boot would have loaded");
    assert_eq!(
        first.status().served_version,
        0,
        "a fresh namespace's directory starts at version zero"
    );

    // Node 2 boots over the very same empty Redis and performs the
    // deployment's first admin write, through `apply`. That call exercises
    // apply's own exemption, not the one this test is about -- it is only
    // how the store comes to hold a first lineage at all, minted by a node
    // that is not the one about to refresh.
    let second = boot(&namespace)
        .await
        .expect("the file plus whatever node one has -- nothing -- written");
    second
        .apply(
            DirectoryMutation::CreateProject {
                entry: roundhouse_server::control_config::ProjectEntry {
                    id: format!("h1-refresh-{}", uuid::Uuid::new_v4()),
                    name: None,
                    policy: None,
                    budget: None,
                    fair_use: None,
                    validate: None,
                    credentials: None,
                    tiers: None,
                },
            },
            t0,
        )
        .await
        .expect("the deployment's first write, minted by node two");

    // Claim 1: node one's own first refresh, past the TTL, with
    // `claimed.version` still `0` -- the branch the sibling test never
    // reaches.
    let past_ttl = t0 + DEFAULT_ADMISSION_CACHE_TTL_MS + 5_000;
    first.plane(past_ttl).await;
    assert_eq!(
        first.status().served_version,
        1,
        "H1: a node's own first refresh must adopt what the store holds, even though a \
         different node minted the lineage it is adopting"
    );
    assert_eq!(
        first.last_regression(),
        None,
        "H1: a node's very first refresh, reading a lineage it never claimed because it has \
         never written or refreshed before, must not report that lineage as a regression -- \
         this is the refresh-path half of the empty-Redis exemption, and the guard this \
         asserts against is `claimed.version != 0 &&` at directory.rs's Managed::compiled"
    );
    assert_eq!(
        first.status().divergence,
        None,
        "both nodes compiled the same file under the same catalog and checks, so adopting the \
         other one's write is not a divergence"
    );

    // Claim 2: a later write from node one, refreshed by node two, still
    // names no regression -- the two nodes share one lineage, the one node
    // one just adopted, rather than each having minted its own and merely
    // agreeing on today's version.
    first
        .apply(
            DirectoryMutation::CreateProject {
                entry: roundhouse_server::control_config::ProjectEntry {
                    id: format!("h1-refresh-second-{}", uuid::Uuid::new_v4()),
                    name: None,
                    policy: None,
                    budget: None,
                    fair_use: None,
                    validate: None,
                    credentials: None,
                    tiers: None,
                },
            },
            past_ttl,
        )
        .await
        .expect("a fresh project id from node one, now on the lineage it just adopted");

    let past_second_ttl = past_ttl + DEFAULT_ADMISSION_CACHE_TTL_MS + 5_000;
    second.plane(past_second_ttl).await;
    assert_eq!(
        second.status().served_version,
        2,
        "node two's refresh must pick up node one's write"
    );
    assert_eq!(
        second.last_regression(),
        None,
        "H1: node two refreshing past node one's write must not read it as a lineage \
         regression -- the two nodes agree on one lineage rather than each minting its own"
    );
}
