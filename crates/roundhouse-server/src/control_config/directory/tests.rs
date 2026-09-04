// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The directory's own tests: what a mutation may do, what it compiles to, and
//! how long a second node may disagree.
//!
//! Every clock here is a number passed in. Nothing sleeps, because the property
//! under test in the staleness section is *how long a stale view lasts*, and a
//! test that measured that against a real clock would be a test that fails on a
//! busy machine and passes on a quiet one.

use std::collections::HashSet;
use std::sync::Arc;

use roundhouse_core::control::{
    Allocation, BudgetWindow, DocumentStore, MemoryDocumentStore, Principal,
};
use roundhouse_core::routing::{Candidate, Target};

use super::super::budget::{AllocationConfig, BudgetConfig, OnExhaustionConfig};
use super::super::config::{ControlPlaneConfig, PolicyConfig, ProjectEntry, UserEntry};
use super::super::fixtures::{ADMIN_HASH, TURN_HASH, TURN_SECRET, bearer_headers, sample_config};
use super::super::{AuthError, KeyScope, has_valid_key_shape};
use super::*;
use crate::test_support::ScriptedDirectoryStore;

/// The one thing this deployment can route to.
///
/// Hand-built rather than quoted from a catalog: what the cross-checks read off
/// a candidate is its target identity and its quality prior, and neither is
/// something a quote would decide differently. Free, so a budget-exhausted key
/// still has somewhere to go and the budget half of the promise check stays out
/// of tests that are not about it.
fn reachable() -> Candidate {
    Candidate {
        target: Target::Frontier {
            provider: "echo".into(),
            model: "echo".into(),
        },
        expected_prefill_tokens: 1_024.0,
        matched_prefix_tokens: 0,
        expected_ttft_ms: 1.0,
        expected_cost_usd: 0.0,
        quality_prior: 0.5,
        load: None,
    }
}

/// No judge, which is why no fixture below enrols a project in the validate
/// loop: that pairing is refused at boot and would be refused here too, which
/// is the subject of its own test in `main.rs` rather than of these.
fn checks() -> CrossChecks {
    CrossChecks::new(vec![reachable()], None)
}

/// The path a refusal names. The real one comes from
/// `ROUNDHOUSE_CONTROL_PLANE`; using the variable's own name keeps a test
/// failure readable as the sentence an operator would see.
const PATH: &str = "ROUNDHOUSE_CONTROL_PLANE";

/// The file half: project `acme`, user `ada`, one turn key, one admin key.
fn file() -> ControlPlaneConfig {
    ControlPlaneConfig::from_json(sample_config(), PATH).expect("the fixture config validates")
}

/// The same file, with an explicit `admission_cache_ttl_ms`.
///
/// Spelled as JSON rather than by editing a parsed config, so the field under
/// test is exercised through the deserializer an operator's file goes through.
fn file_with_ttl(ttl_ms: u64) -> ControlPlaneConfig {
    let json = format!(
        r#"{{
          "projects": [{{ "id": "acme", "name": "Acme Corp" }}],
          "users": [{{ "id": "ada" }}],
          "keys": [{{ "project": "acme", "user": "ada", "key_sha256": "{TURN_HASH}" }}],
          "admin_keys": ["{ADMIN_HASH}"],
          "admission_cache_ttl_ms": {ttl_ms}
        }}"#
    );
    ControlPlaneConfig::from_json(&json, PATH).expect("the fixture config validates")
}

async fn directory(store: Arc<dyn DirectoryStore>, now_ms: u64) -> ControlDirectory {
    ControlDirectory::new(file(), PATH, store, checks(), now_ms)
        .await
        .expect("the file alone compiles, since it is what a boot would have loaded")
}

/// A directory over a store nobody else holds.
async fn solo(now_ms: u64) -> ControlDirectory {
    directory(
        Arc::new(DocumentDirectoryStore::over(Arc::new(
            MemoryDocumentStore::new(),
        ))),
        now_ms,
    )
    .await
}

fn project(id: &str) -> ProjectEntry {
    ProjectEntry {
        id: id.to_string(),
        name: None,
        policy: None,
        budget: None,
        fair_use: None,
        validate: None,
        credentials: None,
        tiers: None,
    }
}

fn user(id: &str) -> UserEntry {
    UserEntry { id: id.to_string() }
}

/// Project, user and membership in three writes — what every mint below needs
/// to exist first.
async fn tenancy(directory: &ControlDirectory, project_id: &str, user_id: &str, now_ms: u64) {
    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: project(project_id),
            },
            now_ms,
        )
        .await
        .expect("a fresh project id");
    directory
        .apply(
            DirectoryMutation::CreateUser {
                entry: user(user_id),
            },
            now_ms,
        )
        .await
        .expect("a fresh user id");
    directory
        .apply(
            DirectoryMutation::UpsertMembership {
                project: project_id.to_string(),
                user: user_id.to_string(),
                role: MembershipRole::Member,
                allocation: None,
                overrides: None,
            },
            now_ms,
        )
        .await
        .expect("a membership neither half declares");
}

/// What a presented secret resolves to, through the whole header seam.
async fn resolve(
    directory: &ControlDirectory,
    secret: &str,
    now_ms: u64,
) -> Result<KeyScope, AuthError> {
    directory.plane(now_ms).await.scope(&bearer_headers(secret))
}

/// The row a presented secret is refused by, or `None` if it was admitted.
///
/// A projection rather than an equality on the `Result`, because [`KeyScope`]
/// deliberately does not derive `PartialEq` — comparing two of them would be
/// comparing two resolved policies, which is not what any assertion here is
/// about. See the note on `Resolved` in the resolver's own tests.
async fn refusal(directory: &ControlDirectory, secret: &str, now_ms: u64) -> Option<AuthError> {
    resolve(directory, secret, now_ms).await.err()
}

fn principal_of(scope: Result<KeyScope, AuthError>) -> Option<Principal> {
    match scope {
        Ok(KeyScope::Turn(admission)) => Some(admission.principal),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Minting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn minting_stores_only_the_hash_and_tail() {
    let directory = solo(0).await;
    tenancy(&directory, "widgets", "bo", 1_000).await;
    let minted = directory
        .mint_turn_key("widgets", "bo", 2_000)
        .await
        .expect("the membership exists and the policy admits the catalog");

    // The secret works, which is what makes the rest of this test about a key
    // rather than about a string.
    assert_eq!(
        principal_of(resolve(&directory, &minted.secret, 2_000).await),
        Some(Principal::new("widgets", "bo"))
    );

    let view = directory.view(2_000).await;
    let record = view
        .keys
        .iter()
        .find(|key| key.key_sha256 == minted.key_sha256)
        .expect("the mint wrote a record");
    assert_eq!(record.provenance, Provenance::Admin);
    assert_eq!(record.created_at_ms, Some(2_000));
    assert_eq!(record.revoked_at_ms, None);
    assert_eq!(
        record.display_tail.as_deref(),
        Some(&minted.secret[minted.secret.len() - 4..]),
        "the tail is the last four characters of the secret, so an operator can \
         match a row against their secret manager"
    );

    // **The claim that matters**: the plaintext is in no field of the record,
    // and this asserts it against the whole rendered struct rather than against
    // the fields somebody remembered to check.
    let rendered = format!("{record:?}");
    assert!(
        !rendered.contains(&minted.secret),
        "a record must not carry the secret: {rendered}"
    );
    // And the tail on its own is not a leak: four base62 characters out of
    // forty-three.
    assert_eq!(record.display_tail.as_deref().map(str::len), Some(4));
}

/// Every minted secret passes the shape check this deployment applies to
/// presented ones.
///
/// **Two hundred draws, because the failure this catches is probabilistic.**
/// Rendering 32 bytes as base62 yields between one and forty-three digits, and
/// a value below `62^42` needs left-padding to reach the forty-three
/// characters `has_valid_key_shape` demands. Get the padding wrong and roughly
/// one mint in sixty-two is refused by its own deployment as `malformed_key` —
/// which reads to an operator like a paste error at the one moment there was
/// none. A single-draw test passes 98% of the time.
#[tokio::test]
async fn every_minted_secret_passes_the_shape_check_this_deployment_applies() {
    let mut seen: HashSet<String> = HashSet::new();
    for _ in 0..200 {
        for kind in [KeyKind::Turn, KeyKind::Admin] {
            let minted = mint_key(kind).expect("the system CSPRNG answers");
            assert!(
                has_valid_key_shape(&minted.secret),
                "a secret this deployment minted must be one it accepts: {}",
                minted.secret
            );
            assert!(minted.secret.starts_with(kind.prefix()));
            seen.insert(minted.secret);
        }
    }
    assert_eq!(
        seen.len(),
        400,
        "two draws from 32 CSPRNG bytes collided, which is not a thing that happens"
    );

    // The deterministic half of the same property: the value that needs the
    // most padding is zero, and it must still render as forty-three digits.
    assert_eq!(super::super::base62([0u8; 32]), "0".repeat(43));
    // And the largest value must not overflow into a forty-fourth.
    assert_eq!(super::super::base62([0xffu8; 32]).len(), 43);

    // The alphabet the entropy claim rests on: exactly sixty-two distinct
    // characters, every one of them acceptable to the shape check. Sixty-one or
    // sixty-three would change how many bits a forty-three-digit tail carries,
    // and nothing else in the system would notice.
    let digits: HashSet<u8> = super::super::BASE62_DIGITS.iter().copied().collect();
    assert_eq!(digits.len(), 62);
    assert!(
        super::super::BASE62_DIGITS
            .iter()
            .all(u8::is_ascii_alphanumeric)
    );
}

// ---------------------------------------------------------------------------
// Tombstones
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_revoked_key_compiles_to_a_named_refusal() {
    let directory = solo(0).await;
    tenancy(&directory, "widgets", "bo", 1_000).await;
    let minted = directory
        .mint_turn_key("widgets", "bo", 2_000)
        .await
        .unwrap();
    let id = key_id(&minted.key_sha256);

    assert!(
        principal_of(resolve(&directory, &minted.secret, 2_000).await).is_some(),
        "the probe has to work before it is revoked, or the assertion below is \
         satisfied by a key that never resolved"
    );

    directory
        .apply(DirectoryMutation::RevokeKey { id: id.clone() }, 3_000)
        .await
        .expect("an API-minted key is the API's to revoke");

    assert_eq!(
        refusal(&directory, &minted.secret, 3_000).await,
        Some(AuthError::RevokedKey),
        "revoked, and told apart from a key this deployment never had"
    );

    // The distinction, stated as a difference rather than as a single answer: a
    // well-shaped secret nobody ever issued is `unknown_key`, and an operator
    // reading a log needs those two to be different words.
    let never_issued = "rh_turn_ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ";
    assert_eq!(
        refusal(&directory, never_issued, 3_000).await,
        Some(AuthError::UnknownKey)
    );
    assert_ne!(AuthError::RevokedKey.code(), AuthError::UnknownKey.code());

    // And the row survives its own revocation, which is what a tombstone is
    // for: the operator who revoked it can still see that it existed.
    let view = directory.view(3_000).await;
    let record = view
        .keys
        .iter()
        .find(|key| key.id == id)
        .expect("still listed");
    assert_eq!(record.revoked_at_ms, Some(3_000));

    // Idempotent: the same DELETE arriving twice is not an error, because a
    // retry after a dropped response is not a mistake.
    directory
        .apply(DirectoryMutation::RevokeKey { id }, 4_000)
        .await
        .expect("revoking a revoked key is the state it is already in");
}

#[tokio::test]
async fn an_archived_projects_key_refuses_project_archived() {
    let directory = solo(0).await;
    tenancy(&directory, "widgets", "bo", 1_000).await;
    tenancy(&directory, "gadgets", "cy", 1_000).await;
    let closing = directory
        .mint_turn_key("widgets", "bo", 2_000)
        .await
        .unwrap();
    let staying = directory
        .mint_turn_key("gadgets", "cy", 2_000)
        .await
        .unwrap();

    directory
        .apply(
            DirectoryMutation::ArchiveProject {
                id: "widgets".into(),
            },
            3_000,
        )
        .await
        .expect("an API-created project is the API's to close");

    assert_eq!(
        refusal(&directory, &closing.secret, 3_000).await,
        Some(AuthError::ProjectArchived),
        "the key is intact and its project is closed, which is a different \
         remedy from a revoked key and so a different row"
    );
    // CONTROL: archiving one project closed one project.
    assert_eq!(
        principal_of(resolve(&directory, &staying.secret, 3_000).await),
        Some(Principal::new("gadgets", "cy"))
    );

    // Archived, not deleted: the row keeps the id, so nothing else can be
    // created under it and join two tenants' spend histories.
    let view = directory.view(3_000).await;
    let record = view
        .projects
        .iter()
        .find(|project| project.id() == "widgets")
        .expect("an archived project is still listed");
    assert_eq!(record.archived_at_ms, Some(3_000));
    assert!(matches!(
        directory
            .apply(
                DirectoryMutation::CreateProject {
                    entry: project("widgets")
                },
                4_000
            )
            .await,
        Err(DirectoryError::IdentityCollision {
            kind: EntityKind::Project,
            ..
        })
    ));
}

/// The direct archived-key refusal inside `compile`'s key loop, isolated from
/// its neighbour a few lines above: the unconditional exclusion of an
/// *admin-owned* archived project from `merged.projects`.
///
/// Ordinarily the two agree by construction — an admin-created project that
/// gets archived is excluded from `merged.projects` regardless of whether the
/// direct `archived.contains(...)` check also fires, which is why
/// `an_archived_projects_key_refuses_project_archived` cannot tell the two
/// apart: remove the direct check there and the key still gets refused, via
/// `merged.validate`'s `UnknownProject`, because its project is genuinely
/// gone from `merged.projects` either way.
///
/// They come apart for a project id the *file* also declares: `merged =
/// file.clone()` seeds `merged.projects` from the file first, and nothing
/// ever removes a file-declared entry from it. A hand-built, records-owned
/// `acme` row — an id no real archive request could ever reach, since the
/// file owns it and `ArchiveProject` refuses a file-owned id — therefore
/// stays present in `merged.projects` no matter what its own
/// `archived_at_ms` says. With the direct check gone, nothing else refuses
/// the key: `merged.validate` finds `acme` right there and admits it.
#[tokio::test]
async fn the_direct_archived_key_refusal_is_not_the_projects_exclusion_in_disguise() {
    let minted = mint_key(KeyKind::Turn).expect("the system CSPRNG answers");
    let mut records = DirectoryRecords {
        projects: vec![ProjectRecord {
            entry: project("acme"),
            provenance: Provenance::Admin,
            created_at_ms: Some(0),
            archived_at_ms: Some(500),
        }],
        users: vec![UserRecord {
            entry: user("bo"),
            provenance: Provenance::Admin,
            created_at_ms: Some(0),
        }],
        memberships: vec![MembershipRecord {
            project: "acme".into(),
            user: "bo".into(),
            role: Some(MembershipRole::Member),
            allocation: None,
            overrides: None,
            provenance: Provenance::Admin,
            created_at_ms: Some(0),
        }],
        keys: vec![ApiKeyRecord {
            id: key_id(&minted.key_sha256),
            key_sha256: minted.key_sha256.clone(),
            display_tail: Some(minted.display_tail.clone()),
            scope: KeyRecordScope::Turn {
                project: "acme".into(),
                user: "bo".into(),
            },
            provenance: Provenance::Admin,
            created_at_ms: Some(0),
            revoked_at_ms: None,
            // An API-minted key never carries one: no route writes a member
            // window. See `ApiKeyRecord::fair_use`.
            fair_use: None,
        }],
    };

    let plane = compile(&file(), PATH, &checks(), &records).expect(
        "`acme` is in `merged.projects` either way, and `bo`'s policy admits `reachable()`",
    );
    assert_eq!(
        plane.scope(&bearer_headers(&minted.secret)).err(),
        Some(AuthError::ProjectArchived),
        "the direct check must fire even though `acme` is never absent from \
         `merged.projects` here -- there is no `UnknownProject` fallback to \
         lean on"
    );

    // CONTROL: the same membership and key, over a records table that makes
    // no claim about `acme` at all -- so the refusal above tracks the
    // archived flag on the hand-built row and not some artifact of feeding
    // `compile` a row the real API could never produce. (Un-archiving the
    // same row instead would re-push it into `merged.projects` alongside the
    // file's own `acme` and fail on `DuplicateProject`, which is a fact about
    // this fixture rather than about the check under test.)
    records.projects.clear();
    let plane = compile(&file(), PATH, &checks(), &records).expect("still compiles");
    assert_eq!(
        principal_of(plane.scope(&bearer_headers(&minted.secret))),
        Some(Principal::new("acme", "bo"))
    );
}

// ---------------------------------------------------------------------------
// Ownership
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_config_owned_entity_refuses_mutation() {
    let directory = solo(0).await;
    // `acme`, `ada` and both hashes come from the file — see `sample_config`.
    let mutations: Vec<(&str, DirectoryMutation)> = vec![
        (
            "patching a configured project",
            DirectoryMutation::PatchProject {
                id: "acme".into(),
                patch: ProjectPatch {
                    name: Some(Some("Renamed".into())),
                    ..ProjectPatch::default()
                },
            },
        ),
        (
            "archiving a configured project",
            DirectoryMutation::ArchiveProject { id: "acme".into() },
        ),
        (
            "editing a configured membership",
            DirectoryMutation::UpsertMembership {
                project: "acme".into(),
                user: "ada".into(),
                role: MembershipRole::Owner,
                allocation: None,
                overrides: None,
            },
        ),
        (
            "removing a configured membership",
            DirectoryMutation::DeleteMembership {
                project: "acme".into(),
                user: "ada".into(),
            },
        ),
        (
            "minting a second key under a configured membership",
            DirectoryMutation::MintTurnKey {
                project: "acme".into(),
                user: "ada".into(),
                key: KeyFingerprint {
                    key_sha256: "b".repeat(64),
                    display_tail: "bbbb".into(),
                },
            },
        ),
        (
            "revoking a configured turn key",
            DirectoryMutation::RevokeKey {
                id: key_id(TURN_HASH),
            },
        ),
        (
            "revoking the configured admin key",
            DirectoryMutation::RevokeKey {
                id: key_id(ADMIN_HASH),
            },
        ),
    ];
    for (what, mutation) in mutations {
        let error = match directory.apply(mutation, 1_000).await {
            Err(error) => error,
            Ok(_) => panic!("{what}: the file owns it, so this must be refused"),
        };
        assert!(
            matches!(error, DirectoryError::ConfigOwned { .. }),
            "{what}: expected a config-owned refusal, got {error:?}"
        );
        assert!(
            error.to_string().contains("ROUNDHOUSE_CONTROL_PLANE"),
            "{what}: the refusal has to name the document that owns it, since \
             that is the remedy: {error}"
        );
    }

    // **The control, and it is what keeps the rule from being "the API may not
    // touch anything the file mentions".** Creating an entity that *references*
    // a configured one is exactly what the rule allows: a new person on a
    // configured project is a new membership, owned here, and the file has
    // nothing to say about it.
    directory
        .apply(DirectoryMutation::CreateUser { entry: user("bo") }, 1_000)
        .await
        .expect("a user id the file does not declare");
    directory
        .apply(
            DirectoryMutation::UpsertMembership {
                project: "acme".into(),
                user: "bo".into(),
                role: MembershipRole::Member,
                allocation: None,
                overrides: None,
            },
            1_000,
        )
        .await
        .expect("a membership in a configured project is a create, not an edit");
    let minted = directory
        .mint_turn_key("acme", "bo", 1_000)
        .await
        .expect("and its keys are the API's to mint");
    assert_eq!(
        principal_of(resolve(&directory, &minted.secret, 1_000).await),
        Some(Principal::new("acme", "bo"))
    );
    // The file's own key is untouched by any of it.
    assert_eq!(
        principal_of(resolve(&directory, TURN_SECRET, 1_000).await),
        Some(Principal::new("acme", "ada"))
    );
}

/// `PatchProject`'s ownership check, isolated from the coincidence that lets
/// [`a_config_owned_entity_refuses_mutation`] catch its removal today.
///
/// `acme` never has a row in `records.projects` — `CreateProject` refuses a
/// config-owned id before one could ever be inserted, see `refuse_taken` — so
/// `PatchProject`'s fallback `UnknownProject` lookup happens to redden the same
/// way a missing `self.refuse_config_project(&id)?` call would (a 404 rather
/// than the intended 409). That is a fact about the *current* shape of the
/// records table, not about the ownership check, and it would stop holding the
/// day something else legitimately puts an admin-owned row under a config id.
/// Calling `mutate` directly, on a hand-built records table that *does* hold
/// such a row, proves the ownership check runs before that row is ever
/// consulted, rather than merely being one of two paths to the same refusal.
#[tokio::test]
async fn patch_project_refuses_ownership_before_it_ever_looks_at_the_records_table() {
    let directory = solo(0).await;
    let mut records = DirectoryRecords {
        projects: vec![
            // A row that could not exist through the real API — nothing may
            // `CreateProject` under an id the file owns — built here so the
            // check under test cannot lean on that row's absence.
            ProjectRecord {
                entry: project("acme"),
                provenance: Provenance::Admin,
                created_at_ms: Some(0),
                archived_at_ms: None,
            },
            ProjectRecord {
                entry: project("widgets"),
                provenance: Provenance::Admin,
                created_at_ms: Some(0),
                archived_at_ms: None,
            },
        ],
        ..DirectoryRecords::default()
    };
    let managed = directory.managed().expect("a managed directory");

    let error = managed
        .mutate(
            &mut records,
            DirectoryMutation::PatchProject {
                id: "acme".into(),
                patch: ProjectPatch {
                    name: Some(Some("Renamed".into())),
                    ..ProjectPatch::default()
                },
            },
            1_000,
        )
        .expect_err("the file owns `acme` regardless of what the records table holds");
    assert!(
        matches!(error, DirectoryError::ConfigOwned { .. }),
        "expected a config-owned refusal, got {error:?}"
    );
    assert_eq!(
        records
            .projects
            .iter()
            .find(|record| record.id() == "acme")
            .expect("the hand-built row")
            .entry
            .name,
        None,
        "a refused patch must not have written anything"
    );

    // CONTROL: the identically-shaped row under an id the file does not own is
    // exactly as patchable as `a_config_owned_entity_refuses_mutation`'s
    // control already shows through the real API — so the refusal above is
    // about ownership and not an artifact of feeding `mutate` a hand-built
    // table.
    managed
        .mutate(
            &mut records,
            DirectoryMutation::PatchProject {
                id: "widgets".into(),
                patch: ProjectPatch {
                    name: Some(Some("Renamed".into())),
                    ..ProjectPatch::default()
                },
            },
            1_000,
        )
        .expect("an admin-owned row is the API's to patch");
    assert_eq!(
        records
            .projects
            .iter()
            .find(|record| record.id() == "widgets")
            .expect("the hand-built row")
            .entry
            .name,
        Some("Renamed".to_string())
    );
}

#[tokio::test]
async fn an_admin_create_colliding_with_config_identity_is_refused() {
    let directory = solo(0).await;

    // Against the file: reported as config-owned rather than as a bare
    // collision, because the remedy is to edit the file and a "already exists"
    // would send an operator looking for an API row that is not there.
    for (what, mutation) in [
        (
            "a project id the file declares",
            DirectoryMutation::CreateProject {
                entry: project("acme"),
            },
        ),
        (
            "a user id the file declares",
            DirectoryMutation::CreateUser { entry: user("ada") },
        ),
    ] {
        let error = directory.apply(mutation, 1_000).await.expect_err(what);
        assert!(
            matches!(error, DirectoryError::ConfigOwned { .. }),
            "{what}: got {error:?}"
        );
        assert!(error.to_string().contains("ROUNDHOUSE_CONTROL_PLANE"));
    }

    // A key hash the file declares, arriving through the mint path.
    let error = directory
        .apply(
            DirectoryMutation::MintAdminKey {
                key: KeyFingerprint {
                    key_sha256: ADMIN_HASH.to_string(),
                    display_tail: "aaaa".into(),
                },
            },
            1_000,
        )
        .await
        .expect_err("one secret must resolve to exactly one scope");
    assert!(matches!(
        error,
        DirectoryError::IdentityCollision {
            kind: EntityKind::Key,
            ..
        }
    ));

    // And against the API's own rows: a second create of an id this API already
    // owns is an ordinary collision.
    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: project("widgets"),
            },
            1_000,
        )
        .await
        .unwrap();
    assert!(matches!(
        directory
            .apply(
                DirectoryMutation::CreateProject {
                    entry: project("widgets")
                },
                1_000
            )
            .await,
        Err(DirectoryError::IdentityCollision {
            kind: EntityKind::Project,
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// Staleness
// ---------------------------------------------------------------------------

/// A second node keeps admitting a revoked key for at most one TTL.
///
/// **Two directories over one store is the multi-node simulation**, and it is
/// the only way to observe the staleness bound at all: the node that performs a
/// write swaps its own snapshot in the same call, so a single-directory test
/// can only ever show the immediate case.
///
/// The control matters as much as the probe. A refresh that dropped everything
/// and recompiled from nothing would also stop admitting the revoked key, and
/// would pass a test written only around it — while quietly taking down every
/// other key on the node.
#[tokio::test]
async fn a_stale_view_refuses_a_revoked_key_after_one_ttl() {
    let store: Arc<dyn DirectoryStore> = Arc::new(DocumentDirectoryStore::over(Arc::new(
        MemoryDocumentStore::new(),
    )));
    let writer = directory(Arc::clone(&store), 0).await;
    tenancy(&writer, "widgets", "bo", 0).await;
    let doomed = writer.mint_turn_key("widgets", "bo", 0).await.unwrap();
    let untouched = writer.mint_turn_key("widgets", "bo", 0).await.unwrap();

    // The reader compiles the same state, at the same instant.
    let reader = directory(Arc::clone(&store), 0).await;
    let ttl = DEFAULT_ADMISSION_CACHE_TTL_MS;
    assert!(principal_of(resolve(&reader, &doomed.secret, 0).await).is_some());

    writer
        .apply(
            DirectoryMutation::RevokeKey {
                id: key_id(&doomed.key_sha256),
            },
            100,
        )
        .await
        .unwrap();

    // The writing node: immediate. A write recompiles and swaps in the same
    // call, so there is no window at all on the node the operator used.
    assert_eq!(
        refusal(&writer, &doomed.secret, 100).await,
        Some(AuthError::RevokedKey)
    );

    // The reading node, inside the TTL: still admitting it. This is the
    // staleness the bound *permits*, written down so that shortening or
    // lengthening it is a decision somebody makes rather than a behavior that
    // drifts.
    assert!(
        principal_of(resolve(&reader, &doomed.secret, ttl - 1).await).is_some(),
        "inside the TTL a second node is allowed to be behind"
    );

    // And at the bound: refreshed, and refusing by name.
    assert_eq!(
        refusal(&reader, &doomed.secret, ttl).await,
        Some(AuthError::RevokedKey),
        "one TTL is the whole of the staleness window"
    );

    // CONTROL: the refresh is a recompile, not a wipe.
    assert_eq!(
        principal_of(resolve(&reader, &untouched.secret, ttl).await),
        Some(Principal::new("widgets", "bo")),
        "a key nobody revoked must survive the refresh that removed one that was"
    );
    // And the file's own key, which no admin write ever touched.
    assert_eq!(
        principal_of(resolve(&reader, TURN_SECRET, ttl).await),
        Some(Principal::new("acme", "ada"))
    );
}

/// A TTL of zero refreshes on every call, including within one millisecond.
///
/// The reason the elapsed test is `>=` and not `>`: an operator who writes zero
/// is asking for "never serve a stale answer", and `>` would give them "never
/// serve an answer stale by more than a millisecond" — which is the same
/// sentence with a footnote nobody reads.
///
/// It is a real setting rather than a curiosity: a deployment that would rather
/// pay a store read per admission than leave a leaked key working for thirty
/// seconds writes exactly this.
#[tokio::test]
async fn a_zero_ttl_refreshes_within_the_same_millisecond() {
    let store: Arc<dyn DirectoryStore> = Arc::new(DocumentDirectoryStore::over(Arc::new(
        MemoryDocumentStore::new(),
    )));
    let writer = directory(Arc::clone(&store), 7).await;
    // Two readers over the same store, built at the same instant from the same
    // file, differing in one number.
    let eager = ControlDirectory::new(file_with_ttl(0), PATH, Arc::clone(&store), checks(), 7)
        .await
        .unwrap();
    let patient = directory(Arc::clone(&store), 7).await;

    tenancy(&writer, "widgets", "bo", 7).await;
    let minted = writer.mint_turn_key("widgets", "bo", 7).await.unwrap();

    assert!(
        principal_of(resolve(&eager, &minted.secret, 7).await).is_some(),
        "a zero-TTL node picks up a write made in the same millisecond"
    );
    // CONTROL: the same store, the same instant, the default TTL. Without this
    // the assertion above would be satisfied by a store that is simply fast.
    assert_eq!(
        refusal(&patient, &minted.secret, 7).await,
        Some(AuthError::UnknownKey),
        "a default-TTL node has not looked yet, which is what the TTL means"
    );

    // And the other direction: a revocation, same millisecond.
    writer
        .apply(
            DirectoryMutation::RevokeKey {
                id: key_id(&minted.key_sha256),
            },
            7,
        )
        .await
        .unwrap();
    assert_eq!(
        refusal(&eager, &minted.secret, 7).await,
        Some(AuthError::RevokedKey)
    );
}

// ---------------------------------------------------------------------------
// What a mutation may not do
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_window_change_is_refused_naming_the_mechanism() {
    let directory = solo(0).await;
    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: ProjectEntry {
                    budget: Some(BudgetConfig {
                        limit_usd: 10.0,
                        window: BudgetWindow::Total,
                        // `Refuse` rather than a degrade mode: this deployment
                        // quotes no local candidate, so a degrade-to-local
                        // budget would be refused by a *different* check and
                        // this test would pass for the wrong reason.
                        on_exhaustion: OnExhaustionConfig::Refuse,
                        overflow_when_local_saturated: None,
                        warn_at: None,
                    }),
                    ..project("widgets")
                },
            },
            1_000,
        )
        .await
        .unwrap();

    let error = directory
        .apply(
            DirectoryMutation::PatchProject {
                id: "widgets".into(),
                patch: ProjectPatch {
                    budget: Some(Some(BudgetConfig {
                        limit_usd: 10.0,
                        window: BudgetWindow::Monthly,
                        on_exhaustion: OnExhaustionConfig::Refuse,
                        overflow_when_local_saturated: None,
                        warn_at: None,
                    })),
                    ..ProjectPatch::default()
                },
            },
            2_000,
        )
        .await
        .expect_err("moving a window either zeroes committed spend or reinterprets it");
    assert!(matches!(
        error,
        DirectoryError::WindowChangeUnsupported {
            from: BudgetWindow::Total,
            to: BudgetWindow::Monthly,
            ..
        }
    ));
    let message = error.to_string();
    assert!(
        message.contains("committed spend") && message.contains("window"),
        "the refusal has to name the mechanism, or it reads as an arbitrary \
         restriction somebody will remove: {message}"
    );

    // CONTROL: everything else about the budget moves freely. A limit is a
    // number the ledger compares against; a window is the interval it counts
    // within, and only the second one reinterprets what is already counted.
    directory
        .apply(
            DirectoryMutation::PatchProject {
                id: "widgets".into(),
                patch: ProjectPatch {
                    budget: Some(Some(BudgetConfig {
                        limit_usd: 25.0,
                        window: BudgetWindow::Total,
                        on_exhaustion: OnExhaustionConfig::Refuse,
                        overflow_when_local_saturated: None,
                        warn_at: Some(0.5),
                    })),
                    ..ProjectPatch::default()
                },
            },
            3_000,
        )
        .await
        .expect("a limit change on the same window is an ordinary edit");
    let view = directory.view(3_000).await;
    let budget = view
        .projects
        .iter()
        .find(|project| project.id() == "widgets")
        .and_then(|project| project.entry.budget.as_ref())
        .expect("the project still has a budget");
    assert_eq!(budget.limit_usd, 25.0);
}

/// CONTROL for [`explicit_json_null_on_a_populated_budget_is_refused`]: an
/// *absent* `budget` field on the wire is `ProjectPatch::default()`'s `None`,
/// and leaving a populated budget alone when the caller never mentioned it is
/// exactly the "absent means leave alone" contract the type documents. It has
/// to keep passing unchanged through the R7 fix — the double-`Option` seam is
/// wrong if it needed touching, because an absent field never reaches the
/// deserializer at all.
#[tokio::test]
async fn omitting_the_budget_field_leaves_a_populated_budget_alone() {
    let directory = solo(0).await;
    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: ProjectEntry {
                    budget: Some(BudgetConfig {
                        limit_usd: 10.0,
                        window: BudgetWindow::Total,
                        on_exhaustion: OnExhaustionConfig::Refuse,
                        overflow_when_local_saturated: None,
                        warn_at: None,
                    }),
                    ..project("widgets")
                },
            },
            1_000,
        )
        .await
        .unwrap();

    // What a `PATCH .../widgets` body of `{"name": "Widgets Inc"}` parses to:
    // `budget` is never mentioned on the wire.
    let patch: ProjectPatch =
        serde_json::from_str(r#"{"name": "Widgets Inc"}"#).expect("valid patch body");
    assert!(patch.budget.is_none(), "budget was never on the wire");

    directory
        .apply(
            DirectoryMutation::PatchProject {
                id: "widgets".into(),
                patch,
            },
            2_000,
        )
        .await
        .expect("touching only `name` is an ordinary edit");
    let view = directory.view(2_000).await;
    let budget = view
        .projects
        .iter()
        .find(|project| project.id() == "widgets")
        .and_then(|project| project.entry.budget.as_ref())
        .expect("an untouched budget is still there");
    assert_eq!(budget.limit_usd, 10.0);
}

/// R7: an explicit JSON `null` is a refusal, not a silent no-op.
///
/// [`ProjectPatch`]'s doc says omission has no spelling for "remove this
/// block" — that is the documented, deliberate half. The undocumented half was
/// what an *explicit* `null` did: a plain `Option<T>` collapsed it into the
/// same `None` an absent field produces, so `{"budget": null}` — the one JSON
/// spelling that reads like an attempt at removal — took the "leave alone"
/// branch and answered success, with the fact that the caller wrote anything
/// already destroyed before `mutate` ran.
///
/// Both halves are asserted here, because closing only the second would leave
/// a `mutate` that guesses: the seam has to preserve the distinction before
/// anything can act on it.
#[tokio::test]
async fn explicit_json_null_on_a_populated_budget_is_refused() {
    let directory = solo(0).await;
    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: ProjectEntry {
                    budget: Some(BudgetConfig {
                        limit_usd: 10.0,
                        window: BudgetWindow::Total,
                        on_exhaustion: OnExhaustionConfig::Refuse,
                        overflow_when_local_saturated: None,
                        warn_at: None,
                    }),
                    ..project("widgets")
                },
            },
            1_000,
        )
        .await
        .unwrap();

    // Exactly what a `PATCH .../widgets` body of `{"budget": null}` parses
    // to: the caller wrote the word `null` on the wire, not nothing.
    let patch: ProjectPatch =
        serde_json::from_str(r#"{"budget": null}"#).expect("valid patch body");
    // The mechanism half: deserialization keeps the distinction the outer
    // `Option` exists for. Without this, `mutate` could only guess -- which is
    // why the assertion below cannot be satisfied by changing `mutate` alone.
    assert!(
        matches!(patch.budget, Some(None)),
        "the field was on the wire (outer Some) and its value was null (inner None)"
    );
    assert_eq!(
        patch.explicit_null_axis(),
        Some("budget"),
        "and the axis names itself, so the refusal can too"
    );

    // The behavior half: a caller who explicitly nulled a populated budget is
    // told so, rather than getting an unremarked success.
    let error = directory
        .apply(
            DirectoryMutation::PatchProject {
                id: "widgets".into(),
                patch,
            },
            2_000,
        )
        .await
        .expect_err("an explicit `null` on a populated budget is refused");
    assert!(
        matches!(
            error,
            DirectoryError::NullPatchUnsupported { axis: "budget", .. }
        ),
        "{error:?}"
    );
    assert!(
        error.to_string().contains("budget"),
        "the refusal has to name the axis, or a caller who nulled one field of \
         five has to guess which: {error}"
    );

    // And nothing was written on the way to the refusal: `apply` validates
    // before it commits, so a refused patch leaves the budget exactly as it
    // was.
    let view = directory.view(2_000).await;
    let budget = view
        .projects
        .iter()
        .find(|project| project.id() == "widgets")
        .and_then(|project| project.entry.budget.as_ref())
        .expect("the refused patch removed nothing");
    assert_eq!(budget.limit_usd, 10.0);
}

/// The mirror: every axis, refused by name, and an absent axis left alone in
/// the same breath.
///
/// One test rather than five because the point is that the five behave
/// identically -- `mutate` reads them through one accessor, and a fix that
/// covered `budget` alone (the axis the finding was written against) would
/// leave `credentials` -- the one whose removal silently un-gates a project --
/// still taking the "leave alone" branch.
#[tokio::test]
async fn an_explicit_null_is_refused_on_every_axis_and_an_absent_one_is_not() {
    let directory = solo(0).await;
    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: ProjectEntry {
                    budget: Some(BudgetConfig {
                        limit_usd: 10.0,
                        window: BudgetWindow::Total,
                        on_exhaustion: OnExhaustionConfig::Refuse,
                        overflow_when_local_saturated: None,
                        warn_at: None,
                    }),
                    ..project("widgets")
                },
            },
            1_000,
        )
        .await
        .unwrap();

    for axis in ["name", "policy", "budget", "validate", "credentials"] {
        let body = format!(r#"{{"{axis}": null}}"#);
        let patch: ProjectPatch = serde_json::from_str(&body).expect("valid patch body");
        assert_eq!(patch.explicit_null_axis(), Some(axis), "{body}");
        let error = directory
            .apply(
                DirectoryMutation::PatchProject {
                    id: "widgets".into(),
                    patch,
                },
                2_000,
            )
            .await
            .expect_err(&format!("`{body}` must be refused"));
        match error {
            DirectoryError::NullPatchUnsupported { axis: named, .. } => {
                assert_eq!(named, axis, "{body}");
            }
            other => panic!("`{body}` was refused for the wrong reason: {other:?}"),
        }
    }

    // The other half, in the same test so the two cannot drift: a body that
    // never mentions `budget` leaves it exactly as it was, which is the
    // contract this type documents and the fix must not have touched.
    let patch: ProjectPatch =
        serde_json::from_str(r#"{"name": "Widgets Inc"}"#).expect("valid patch body");
    assert!(patch.explicit_null_axis().is_none(), "nothing was nulled");
    directory
        .apply(
            DirectoryMutation::PatchProject {
                id: "widgets".into(),
                patch,
            },
            3_000,
        )
        .await
        .expect("touching only `name` is an ordinary edit");
    let view = directory.view(3_000).await;
    let project = view
        .projects
        .iter()
        .find(|project| project.id() == "widgets")
        .expect("the project is still there");
    assert_eq!(project.entry.name.as_deref(), Some("Widgets Inc"));
    assert_eq!(
        project
            .entry
            .budget
            .as_ref()
            .expect("an unmentioned budget is untouched")
            .limit_usd,
        10.0
    );
}

/// A key minted at runtime is judged by the cross-checks a boot would apply.
///
/// The failure this closes: an admin plane is a way to write a configuration
/// the process refuses to *start* under, and the symptom arrives at the next
/// restart — the furthest point in time from the cause.
#[tokio::test]
async fn a_mutation_that_admits_no_model_is_refused() {
    let directory = solo(0).await;
    let narrow = ProjectEntry {
        policy: Some(PolicyConfig {
            allow: Some(vec!["nowhere/*".into()]),
            ..PolicyConfig::default()
        }),
        ..project("narrow")
    };
    // Creating the project is fine, and that is not a hole: a project with no
    // keys admits nobody, so there is no turn for the policy to fail. The check
    // is about *keys*, and it fires the moment one exists.
    directory
        .apply(DirectoryMutation::CreateProject { entry: narrow }, 1_000)
        .await
        .expect("a policy with no key under it refuses no turns");
    directory
        .apply(DirectoryMutation::CreateUser { entry: user("bo") }, 1_000)
        .await
        .unwrap();
    directory
        .apply(
            DirectoryMutation::UpsertMembership {
                project: "narrow".into(),
                user: "bo".into(),
                role: MembershipRole::Member,
                allocation: None,
                overrides: None,
            },
            1_000,
        )
        .await
        .unwrap();

    // Read at 1_000, well inside the default TTL, so this is the version this
    // node has *compiled* rather than one a refresh went and fetched. A fixture
    // edit that pushed these timestamps past the TTL would turn the assertion
    // below into a store read and stop it being about the write path at all.
    let before = directory.version(1_000).await;
    let error = directory
        .mint_turn_key("narrow", "bo", 2_000)
        .await
        .expect_err("every turn of that key's would end in policy_refused");
    match &error {
        DirectoryError::CrossCheckRefused { check, detail } => {
            assert_eq!(*check, "refuse_policies_that_admit_nothing");
            assert!(
                detail.contains("project `narrow`, user `bo`"),
                "the refusal names the key an operator would go and fix: {detail}"
            );
        }
        other => panic!("expected a cross-check refusal, got {other:?}"),
    }

    // **Nothing was written.** A refused mutation that had already committed
    // would leave the deployment holding a key it refuses to boot with.
    assert_eq!(directory.version(2_000).await, before);
    assert!(
        directory.view(2_000).await.keys.iter().all(|key| key.scope
            != KeyRecordScope::Turn {
                project: "narrow".into(),
                user: "bo".into()
            }),
        "the refused mint left no record behind"
    );

    // CONTROL: the same three writes under a policy that does name the catalog.
    tenancy(&directory, "wide", "cy", 3_000).await;
    directory
        .apply(
            DirectoryMutation::PatchProject {
                id: "wide".into(),
                patch: ProjectPatch {
                    policy: Some(Some(PolicyConfig {
                        allow: Some(vec!["echo/*".into()]),
                        ..PolicyConfig::default()
                    })),
                    ..ProjectPatch::default()
                },
            },
            3_000,
        )
        .await
        .unwrap();
    directory
        .mint_turn_key("wide", "cy", 3_000)
        .await
        .expect("a policy that names the one model this deployment has");
}

#[tokio::test]
async fn deleting_a_membership_revokes_its_minted_keys() {
    let directory = solo(0).await;
    tenancy(&directory, "widgets", "bo", 1_000).await;
    let first = directory
        .mint_turn_key("widgets", "bo", 1_000)
        .await
        .unwrap();
    let second = directory
        .mint_turn_key("widgets", "bo", 1_000)
        .await
        .unwrap();
    // A neighbour in the same project, to prove the cascade follows the
    // membership rather than the project.
    directory
        .apply(DirectoryMutation::CreateUser { entry: user("cy") }, 1_000)
        .await
        .unwrap();
    directory
        .apply(
            DirectoryMutation::UpsertMembership {
                project: "widgets".into(),
                user: "cy".into(),
                role: MembershipRole::Member,
                allocation: None,
                overrides: None,
            },
            1_000,
        )
        .await
        .unwrap();
    let neighbour = directory
        .mint_turn_key("widgets", "cy", 1_000)
        .await
        .unwrap();

    directory
        .apply(
            DirectoryMutation::DeleteMembership {
                project: "widgets".into(),
                user: "bo".into(),
            },
            2_000,
        )
        .await
        .expect("an API-created membership is the API's to remove");

    for secret in [&first.secret, &second.secret] {
        assert_eq!(
            refusal(&directory, secret, 2_000).await,
            Some(AuthError::RevokedKey),
            "a key whose membership is gone resolves to nothing, and `revoked` \
             is the answer that stays explicable to whoever removed the member"
        );
    }
    // CONTROL: the neighbour is untouched.
    assert_eq!(
        principal_of(resolve(&directory, &neighbour.secret, 2_000).await),
        Some(Principal::new("widgets", "cy"))
    );

    let view = directory.view(2_000).await;
    assert!(
        !view
            .memberships
            .iter()
            .any(|membership| membership.names("widgets", "bo")),
        "the edge itself is gone"
    );
    assert_eq!(
        view.keys
            .iter()
            .filter(|key| key.revoked_at_ms == Some(2_000))
            .count(),
        2,
        "both of that membership's keys, and no others"
    );
}

/// The cascade in [`DirectoryMutation::DeleteMembership`], isolated from
/// `compile`'s own `Inconsistent` invariant.
///
/// Without the cascade, [`deleting_a_membership_revokes_its_minted_keys`]
/// still reddens — but through a different mechanism than the one it names: the
/// membership row is removed by `mutate` regardless, and the *next* `compile`
/// then refuses to describe a live key naming a membership no record has,
/// failing the whole `apply` with `DirectoryError::Inconsistent` rather than
/// leaving an orphaned, still-admitting key. That is a real safety net — worth
/// knowing about — but it means the wrapper test's `.expect(...)` panics before
/// its own assertions run, which is a different failure than the one it is
/// meant to demonstrate, and would stop catching the cascade's removal at all
/// if that invariant were ever loosened. Calling `mutate` directly checks the
/// cascade's own effect on `records.keys` before `compile` gets anywhere near
/// it.
#[tokio::test]
async fn delete_membership_s_cascade_revokes_keys_inside_mutate_before_any_compile_runs() {
    let directory = solo(0).await;
    let key_sha256 = "b".repeat(64);
    let mut records = DirectoryRecords {
        memberships: vec![MembershipRecord {
            project: "widgets".into(),
            user: "bo".into(),
            role: Some(MembershipRole::Member),
            allocation: None,
            overrides: None,
            provenance: Provenance::Admin,
            created_at_ms: Some(0),
        }],
        keys: vec![ApiKeyRecord {
            id: key_id(&key_sha256),
            key_sha256: key_sha256.clone(),
            display_tail: Some("bbbb".into()),
            scope: KeyRecordScope::Turn {
                project: "widgets".into(),
                user: "bo".into(),
            },
            provenance: Provenance::Admin,
            created_at_ms: Some(0),
            revoked_at_ms: None,
            // An API-minted key never carries one: no route writes a member
            // window. See `ApiKeyRecord::fair_use`.
            fair_use: None,
        }],
        ..DirectoryRecords::default()
    };

    directory
        .managed()
        .expect("a managed directory")
        .mutate(
            &mut records,
            DirectoryMutation::DeleteMembership {
                project: "widgets".into(),
                user: "bo".into(),
            },
            2_000,
        )
        .expect("deleting a membership neither half declares");

    assert!(
        !records
            .memberships
            .iter()
            .any(|membership| membership.names("widgets", "bo")),
        "the edge itself must be gone"
    );
    assert_eq!(
        records.keys[0].revoked_at_ms,
        Some(2_000),
        "the cascade has to revoke the key inside `mutate`, before `compile` \
         ever runs and could fail loudly (or stop failing loudly) instead: {:?}",
        records.keys[0]
    );
}

/// Two keys of one membership cannot disagree about what it may spend.
///
/// **The property the whole compile-merge path rests on.**
/// [`ControlPlane::membership`] refuses to describe a membership whose keys
/// resolve to different budgets — an operator meets that as a boot failure and
/// an agent meets it as a control-surface refusal — and the way to produce one
/// through this API would be to store a copy of the membership's allocation on
/// each key at mint time. Then an `UpsertMembership` would update one and leave
/// the other, and the deployment would break on the *second* key rather than on
/// the edit.
///
/// So the keys carry no copy: every compile reads the membership. This is the
/// test that says so.
#[tokio::test]
async fn two_keys_of_one_membership_never_disagree_after_an_upsert() {
    let directory = solo(0).await;
    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: ProjectEntry {
                    budget: Some(BudgetConfig {
                        limit_usd: 100.0,
                        window: BudgetWindow::Total,
                        on_exhaustion: OnExhaustionConfig::Refuse,
                        overflow_when_local_saturated: None,
                        warn_at: None,
                    }),
                    ..project("widgets")
                },
            },
            1_000,
        )
        .await
        .unwrap();
    directory
        .apply(DirectoryMutation::CreateUser { entry: user("bo") }, 1_000)
        .await
        .unwrap();
    directory
        .apply(
            DirectoryMutation::UpsertMembership {
                project: "widgets".into(),
                user: "bo".into(),
                role: MembershipRole::Member,
                allocation: Some(AllocationConfig::Capped { limit_usd: 5.0 }),
                overrides: None,
            },
            1_000,
        )
        .await
        .unwrap();

    let first = directory
        .mint_turn_key("widgets", "bo", 1_000)
        .await
        .unwrap();
    // The edit lands between the two mints, which is the ordering that would
    // leave a cached copy stale.
    directory
        .apply(
            DirectoryMutation::UpsertMembership {
                project: "widgets".into(),
                user: "bo".into(),
                role: MembershipRole::Owner,
                allocation: Some(AllocationConfig::Capped { limit_usd: 7.0 }),
                overrides: None,
            },
            2_000,
        )
        .await
        .unwrap();
    let second = directory
        .mint_turn_key("widgets", "bo", 3_000)
        .await
        .unwrap();

    let plane = directory.plane(3_000).await;
    for secret in [&first.secret, &second.secret] {
        let admission = plane
            .turn_admission(&bearer_headers(secret))
            .expect("both keys resolve");
        let terms = admission.budget.expect("the project has a budget");
        assert_eq!(
            terms.allocation,
            Allocation::Capped { limit_usd: 7.0 },
            "every key of a membership carries the membership's current ceiling, \
             because there is nowhere else the ceiling is written"
        );
    }
    // And the backwards question — the one the control surface asks — has an
    // answer, which it would not if the two keys had drifted apart.
    plane
        .membership(&Principal::new("widgets", "bo"))
        .expect("one membership, one set of entitlements");
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// The file's entities are listable, and the compiled plane cannot list them.
///
/// `ControlPlane::configured` discards the config's entries as it builds its
/// lookup tables — deliberately, so no second copy of a deployment's keys is
/// live for the life of the process — which is exactly why the admin plane's
/// `GET` routes are served from here and not from the plane.
#[tokio::test]
async fn the_view_lists_both_halves_and_marks_which_is_which() {
    let directory = solo(0).await;
    tenancy(&directory, "widgets", "bo", 1_000).await;
    directory
        .mint_turn_key("widgets", "bo", 1_000)
        .await
        .unwrap();

    let view = directory.view(1_000).await;
    let owner = |provenance: Provenance| {
        view.projects
            .iter()
            .filter(|project| project.provenance == provenance)
            .map(|project| project.id().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(owner(Provenance::Config), vec!["acme".to_string()]);
    assert_eq!(owner(Provenance::Admin), vec!["widgets".to_string()]);

    // The file's memberships are implied by its keys, and two keys for one
    // person in one project are one membership — an operator rotating a secret
    // must not appear twice.
    assert_eq!(
        view.memberships
            .iter()
            .filter(|membership| membership.provenance == Provenance::Config)
            .count(),
        1
    );
    // A configured membership's role is absent rather than guessed: the file
    // has no field for one.
    assert!(
        view.memberships
            .iter()
            .filter(|membership| membership.provenance == Provenance::Config)
            .all(|membership| membership.role.is_none())
    );
    // A configured key has no tail, because this deployment has never seen its
    // plaintext. Four characters of the *hash* would look exactly like the
    // string an operator is trying to match and never match it.
    let configured: Vec<_> = view
        .keys
        .iter()
        .filter(|key| key.provenance == Provenance::Config)
        .collect();
    assert_eq!(configured.len(), 2, "one turn key and one admin key");
    assert!(configured.iter().all(|key| key.display_tail.is_none()));
    assert!(
        configured
            .iter()
            .any(|key| key.scope == KeyRecordScope::Admin)
    );
}

// ---------------------------------------------------------------------------
// One snapshot, or two versions (R2)
// ---------------------------------------------------------------------------

/// The store double this section scripts a write between two reads with is
/// [`ScriptedDirectoryStore`] (M16.0 review, F1): `arm(after, 2)` right after
/// construction lands `after` on the second [`DirectoryStore::version`] call
/// ever, which is what a directory past its TTL asks first on every refresh,
/// whether or not it goes on to load -- the injection point that makes "how
/// many times did this call refresh" observable and the other node's write
/// timeable exactly. A caller that refreshes once per snapshot therefore never
/// sees the second answer at all; one that refreshes twice sees both, and that
/// is exactly the pair-from-two-versions R2 names.
///
/// The records `widgets`/`bo` stand at before and after their turn key is
/// minted.
///
/// Produced by the ordinary mutations on a throwaway node rather than
/// hand-built, so the pair [`WriteBetweenReads`] hands out is exactly what a
/// real write on another node would have committed — a fixture that assembled
/// the "after" records itself could differ from one in the way that made the
/// test pass.
async fn staged_mint() -> (DirectoryRecords, DirectoryRecords) {
    let staged = solo(0).await;
    tenancy(&staged, "widgets", "bo", 0).await;
    // `apply` hands back the records it wrote. The upsert is repeated rather
    // than reached for through a listing because a listing is a projection, and
    // what a store holds is records.
    let before = staged
        .apply(
            DirectoryMutation::UpsertMembership {
                project: "widgets".into(),
                user: "bo".into(),
                role: MembershipRole::Member,
                allocation: None,
                overrides: None,
            },
            0,
        )
        .await
        .expect("an upsert of the membership that is already there");
    let minted = mint_key(KeyKind::Turn).expect("the process has an RNG");
    let after = staged
        .apply(
            DirectoryMutation::MintTurnKey {
                project: "widgets".into(),
                user: "bo".into(),
                key: KeyFingerprint::from(&minted),
            },
            0,
        )
        .await
        .expect("a fresh hash under an existing membership");
    ((*before).clone(), (*after).clone())
}

/// Whether a listing says bo holds a live, unrevoked turn key.
fn lists_bos_live_key(view: &DirectoryView) -> bool {
    view.keys.iter().any(|key| {
        key.revoked_at_ms.is_none()
            && key.scope
                == KeyRecordScope::Turn {
                    project: "widgets".into(),
                    user: "bo".into(),
                }
    })
}

/// The property: the two halves of one snapshot describe one version.
///
/// **Coherence, not freshness.** A snapshot taken just before a write describes
/// the state before it, and that is a correct answer — a check that demanded the
/// newest state would fail a correct implementation that happened to read one
/// instant early. So this is gated on what the listing itself reports: if the
/// listing says bo holds a live turn key, the plane beside it must resolve an
/// admission for him. One response must never say both "has a key" and "has no
/// admission" about the same membership.
fn assert_coherent(plane: &ControlPlane, view: &DirectoryView, when: &str) {
    if !lists_bos_live_key(view) {
        return;
    }
    assert!(
        plane.membership(&Principal::new("widgets", "bo")).is_ok(),
        "{when}: the listing says bo holds a live turn key and the plane handed \
         over beside it has no admission for him — one response, two versions"
    );
}

/// R2 (thermo-nuclear review, M8): `budget_view` read its plane and its listing
/// through two independent calls, each taking `Managed`'s `current` under its
/// own lock acquisition — and the old `Managed::view` was two more internally
/// (`let _ = self.plane(now_ms);` followed by a separate `read_current()`).
/// Nothing spanned any of it, so a write landing mid-render left the two answers
/// describing different directory versions: a member whose key the listing shows
/// as live, resolving against a plane that has no admission for them, which this
/// view spells as a row with no figures at all.
///
/// The handler's own comment claimed the opposite ("One snapshot for the whole
/// document"), which is why the remedy is a single
/// [`ControlDirectory::snapshot`] that hands both halves out of one guard rather
/// than a comment saying to be careful.
///
/// Reproduced at the seam the two reads share rather than over HTTP, and with
/// the other node's write timed by the store — see the doc above — so that
/// the failure is deterministic instead of a race the test hopes to lose.
#[tokio::test]
async fn budget_view_s_plane_and_view_must_describe_the_same_version() {
    let (before, after) = staged_mint().await;
    let store = Arc::new(ScriptedDirectoryStore::new(before, 1).await);
    store.arm(after, 2);
    // TTL zero, because a directory inside its TTL never reaches for the store
    // at all: with a live cache both reads answer from the same `Compiled` by
    // luck rather than by design, and this test would pass on the defect.
    let directory = ControlDirectory::new(
        file_with_ttl(0),
        PATH,
        store as Arc<dyn DirectoryStore>,
        checks(),
        0,
    )
    .await
    .expect("the file alone compiles, since it is what a boot would have loaded");

    // The other node's write lands during this call if — and only if — it
    // refreshes twice. One refresh, and this snapshot is coherently the older
    // version; two, and its listing is a version ahead of its plane.
    let (plane, view) = directory.snapshot(0).await;
    assert_coherent(
        &plane,
        &view,
        "the snapshot rendered while another node's write landed",
    );

    // And the write really did land, seen coherently by the next snapshot.
    // Without this the test would also pass on a directory that had simply
    // stopped refreshing — which is coherent, and useless.
    let (plane, view) = directory.snapshot(0).await;
    assert_coherent(&plane, &view, "the snapshot after that write");
    assert!(
        lists_bos_live_key(&view),
        "sanity: the staged write is meant to have landed by now, or the \
         assertion above never had a version mismatch to catch"
    );
}

/// CONTROL for the test above, same coherence check, minus the race: the mint
/// happens before the snapshot, so there is one unmoving version to read. It
/// must pass — and the live-key assertion is what stops it passing vacuously,
/// which is what proves the coherence check is capable of firing at all.
#[tokio::test]
async fn plane_and_view_agree_when_nothing_writes_between_them() {
    let directory = solo(0).await;
    tenancy(&directory, "widgets", "bo", 0).await;
    directory.mint_turn_key("widgets", "bo", 0).await.unwrap();

    let version_before = directory.version(0).await;
    let (plane, view) = directory.snapshot(0).await;
    assert_eq!(
        version_before,
        directory.version(0).await,
        "sanity: nothing writes across the snapshot in this control"
    );
    assert!(
        lists_bos_live_key(&view),
        "the control is only a control if the coherence check has a live key to \
         be about"
    );
    assert_coherent(&plane, &view, "control: one version, read once");
}

/// CONTROL: a membership that genuinely has no key must still read `Unknown`.
/// The fix for R2 is a shared snapshot, not a `plane()` that stops reporting
/// absent admissions — this pins that a member nobody ever minted a key for is
/// still, correctly, unresolvable.
#[tokio::test]
async fn a_membership_with_no_key_at_all_is_still_unknown_to_the_plane() {
    let directory = solo(0).await;
    tenancy(&directory, "widgets", "bo", 0).await;
    let principal = Principal::new("widgets", "bo");

    let (plane, view) = directory.snapshot(0).await;
    assert!(
        view.memberships
            .iter()
            .any(|membership| membership.names("widgets", "bo")),
        "the membership exists without ever having a key"
    );
    assert!(
        matches!(
            plane.membership(&principal),
            Err(super::super::MembershipError::Unknown(_))
        ),
        "control: genuinely no key means genuinely no admission"
    );
}

/// A reference to something neither half declares is a 404, not a compile
/// failure.
///
/// The split the error type exists for: "there is no such project" is a fact
/// about this request, and it must not arrive as the compiler's
/// `UnknownProject`, whose message is about a *file* the caller never wrote.
#[tokio::test]
async fn a_membership_naming_nothing_is_refused_before_anything_compiles() {
    let directory = solo(0).await;
    assert!(matches!(
        directory
            .apply(
                DirectoryMutation::UpsertMembership {
                    project: "nowhere".into(),
                    user: "ada".into(),
                    role: MembershipRole::Member,
                    allocation: None,
                    overrides: None,
                },
                1_000
            )
            .await,
        Err(DirectoryError::UnknownProject { .. })
    ));
    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: project("widgets"),
            },
            1_000,
        )
        .await
        .unwrap();
    assert!(matches!(
        directory
            .apply(
                DirectoryMutation::UpsertMembership {
                    project: "widgets".into(),
                    user: "nobody".into(),
                    role: MembershipRole::Member,
                    allocation: None,
                    overrides: None,
                },
                1_000
            )
            .await,
        Err(DirectoryError::UnknownUser { .. })
    ));
    assert!(matches!(
        directory
            .apply(
                DirectoryMutation::RevokeKey {
                    id: "key_0000000000000000".into()
                },
                1_000
            )
            .await,
        Err(DirectoryError::UnknownKey { .. })
    ));
    assert!(matches!(
        directory
            .apply(
                DirectoryMutation::DeleteMembership {
                    project: "widgets".into(),
                    user: "nobody".into(),
                },
                1_000
            )
            .await,
        Err(DirectoryError::UnknownMembership { .. })
    ));
}

// ---------------------------------------------------------------------------
// M16.0 — the refresh runs outside every lock (R-D2, R-D3)
// ---------------------------------------------------------------------------

/// The store double every guard below stalls, counts, moves and breaks is
/// [`ScriptedDirectoryStore`] (M16.0 review, F1) -- one wrapper over a real
/// production store shared with the coherence guard above, rather than
/// a second hand-rolled copy of the same `(records, version)` state and its
/// compare-and-set: what changed with M16.0 is that a refresh is now a pair
/// of *awaits*, so the question a double has to be able to answer moved from
/// "how many reads did this call make" to "what was another caller allowed to
/// do while one read was in flight" -- [`ScriptedDirectoryStore::block_next_load`]
/// and [`ScriptedDirectoryStore::block_next_commit`] are what answer it.
///
/// Nothing sleeps to order anything: a blocked `load` announces itself on a
/// semaphore and is released on another, so the interleavings below are decided
/// by signals rather than by how busy the machine is. The only clock is
/// [`tokio::time::timeout`], and it is a *bound* on a stall rather than an
/// ordering device — a test that stalls fails in a second instead of hanging
/// the suite, which is what this repo's bounded-run rule wants of a lock test.

/// M16.0 review, F1: a stale `expected_version` reaches
/// the production store's own compare-and-set through the double, not a
/// hand-rolled copy of it.
///
/// Before this rung, three of this crate's `DirectoryStore` doubles each
/// re-implemented the same `if state.1 != expected_version { Concurrent }`
/// guard [`DocumentDirectoryStore::commit`] performs, and two of the three --
/// `WriteBetweenReads`, formerly here, and `ArmedStore`, formerly in
/// `tests/admin_api.rs` -- never had that guard driven by any test at all:
/// every fixture that reached for a double wanted `load` or `version`
/// scripted, never `commit`. [`ScriptedDirectoryStore`] delegates `commit` to
/// the real production store instead of re-implementing it (see its own
/// doc), so this is what that delegation buys: a stale write against the
/// double is refused by the same compare-and-set
/// [`super::store::tests::commit_refuses_a_stale_expected_version`] pins
/// directly against the shipped store, not by a second copy of it that
/// could silently drop the guard while every other test here stayed green.
#[tokio::test]
async fn a_stale_commit_against_the_scripted_store_is_refused_by_the_real_compare_and_set() {
    let store = ScriptedDirectoryStore::new(DirectoryRecords::default(), 1).await;

    let first = store
        .commit(1, DirectoryRecords::default())
        .await
        .expect("the double is still at the version this write read");
    assert_eq!(
        first.version, 2,
        "the first successful commit through the double is version 2"
    );

    // The version the double is actually at has moved to 2. A second write
    // that still thinks it is 1 -- the shape of a second node that read
    // before the first node's commit landed -- must be refused, and refused
    // by the wrapped store's own guard rather than one this double forgot to
    // carry over.
    let stale = store.commit(1, DirectoryRecords::default()).await;
    assert!(
        matches!(
            stale,
            Err(StoreFailure::Concurrent {
                expected: 1,
                found: 2,
            })
        ),
        "a commit against a version the scripted double has moved past must answer \
         `Concurrent`, naming both the version it expected and the one actually found -- \
         the same refusal `commit_refuses_a_stale_expected_version` pins directly against \
         the production store: {stale:?}"
    );
    assert_eq!(
        store.version().await.unwrap().version,
        2,
        "a refused commit must not have advanced the double"
    );
}

/// The TTL every guard below runs at.
///
/// Deliberately **not** zero. Zero means "refresh on every call" — the one
/// setting under which the `refreshed_at_ms` stamp cannot be a single-flight
/// token, because every caller is past a zero TTL by definition — so a suite
/// that pinned single flight at zero would be pinning nothing. A real TTL is
/// also what a deployment runs; the number itself is arbitrary because every
/// clock here is a number passed in.
const GUARD_TTL_MS: u64 = 1_000;

/// A managed directory over a scripted store, booted at instant zero.
async fn scripted(store: Arc<ScriptedDirectoryStore>) -> Arc<ControlDirectory> {
    let directory = Arc::new(
        ControlDirectory::new(
            file_with_ttl(GUARD_TTL_MS),
            PATH,
            Arc::clone(&store) as Arc<dyn DirectoryStore>,
            checks(),
            0,
        )
        .await
        .expect("the file alone compiles, since it is what a boot would have loaded"),
    );
    store.forget_boot();
    directory
}

/// Whether a compiled plane has an admission for bo — the difference between
/// the `before` and `after` records [`staged_mint`] produces.
fn admits_bo(plane: &ControlPlane) -> bool {
    plane.membership(&Principal::new("widgets", "bo")).is_ok()
}

/// R-D2, guard 1: **a refresh in flight stalls nobody.**
///
/// The load is the expensive half of a refresh and, once the store is durable,
/// a network round trip. A refresh that held the snapshot lock across it would
/// put every concurrent admission behind that round trip — the exact trade the
/// module doc named as the thing a durable store must not be landed without.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_in_flight_does_not_stall_a_concurrent_admission() {
    let (_before, after) = staged_mint().await;
    let store = Arc::new(ScriptedDirectoryStore::new(DirectoryRecords::default(), 1).await);
    let directory = scripted(Arc::clone(&store)).await;

    // Another node's write, and a load that will not come back until this test
    // says so.
    store.set(after, 2).await;
    let release = store.block_next_load();

    let refresher = tokio::spawn({
        let directory = Arc::clone(&directory);
        async move { directory.plane(GUARD_TTL_MS).await }
    });
    store.load_in_flight().await;

    // The admission under test, taken while that load is open.
    let served = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        directory.plane(GUARD_TTL_MS),
    )
    .await
    .expect(
        "an admission taken while another caller's refresh is blocked in the store must be \
         answered from the plane this node already has, not queued behind a round trip",
    );
    assert!(
        !admits_bo(&served),
        "the refresh has not returned yet, so the plane served beside it is still the one \
         this node compiled at boot"
    );

    release.add_permits(1);
    let refreshed = refresher.await.expect("the refreshing task does not panic");
    assert!(
        admits_bo(&refreshed),
        "once the load returns, the caller that paid for it sees the new plane"
    );
}

/// R-D2, guard 2: **N callers past the TTL cost one refresh, not N.**
///
/// The stamp is the token: the first caller past the TTL writes
/// `refreshed_at_ms` and goes to the store, and every caller behind it sees a
/// fresh stamp and serves the plane this node already has. Without that, a
/// busy node answers one expired TTL with one store round trip per in-flight
/// request — which is worse the busier the node is, and worst exactly when the
/// store is already unwell.
///
/// The first caller's load is **held open** for the whole of the measurement,
/// and that is what makes the count a fact rather than a race: with the refresh
/// still in flight, every later caller is decided by the stamp alone. Letting
/// the first one finish first would let the others take the plain TTL branch
/// and count one refresh whatever the code did. The store also moves on every
/// `version` read, standing in for a neighbour committing continuously, so a
/// caller that *did* go to the store would find a reason to load.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_callers_past_the_ttl_cost_exactly_one_refresh() {
    let store = Arc::new(ScriptedDirectoryStore::new(DirectoryRecords::default(), 1).await);
    let directory = scripted(Arc::clone(&store)).await;
    store.keep_moving();

    let release = store.block_next_load();
    let refresher = tokio::spawn({
        let directory = Arc::clone(&directory);
        async move { directory.plane(GUARD_TTL_MS).await }
    });
    store.load_in_flight().await;

    let callers: Vec<_> = (0..7)
        .map(|_| {
            let directory = Arc::clone(&directory);
            tokio::spawn(async move { directory.plane(GUARD_TTL_MS).await })
        })
        .collect();
    for caller in callers {
        tokio::time::timeout(std::time::Duration::from_secs(5), caller)
            .await
            .expect("no admission queues behind the one refresh in flight")
            .expect("no caller panics");
    }

    assert_eq!(
        store.loads(),
        1,
        "eight admissions arriving on one expired TTL are one refresh, not eight"
    );
    assert_eq!(
        store.versions(),
        1,
        "and the cheap half is single-flighted too: a caller that serves the current plane \
         must not pay for a version read to find that out"
    );

    release.add_permits(1);
    refresher.await.expect("the refreshing task does not panic");
}

/// R-D2, guard 2's quiet-path sibling: **a confirmed-unchanged version still
/// stamps `refreshed_at_ms`.**
///
/// [`ControlDirectory::compiled`]'s doc promises "one cheap version read per
/// TTL when nothing is happening", and [`Compiled::refreshed_at_ms`]'s doc
/// says a confirmed-unchanged snapshot is as fresh as a rebuilt one. Nothing
/// above reaches that: guard 2 runs with [`ScriptedDirectoryStore::keep_moving`] set,
/// so the version is never unchanged, and guard 4 only exercises the
/// version-fails arm. A refactor that stamps `refreshed_at_ms` on load-or-fail
/// but not on confirmed-unchanged would leave the "quiet node" case paying a
/// version read on every admission past the TTL instead of one per TTL — and
/// every guard above stays green, because none of them holds the store still.
///
/// No blocking or spawning: single flight is not what this guard is about,
/// only whether the stamp lands on the path that finds nothing to do. The
/// clock is scripted throughout, never a sleep — each call is timed by the
/// `now_ms` passed to it, not by wall time.
#[tokio::test]
async fn a_confirmed_unchanged_version_still_stamps_the_ttl() {
    let store = Arc::new(ScriptedDirectoryStore::new(DirectoryRecords::default(), 1).await);
    let directory = scripted(Arc::clone(&store)).await;

    // First call past the TTL: the store is still at version 1, so this is
    // the confirmed-unchanged path. If the stamp lands here, the next call —
    // just past this instant but still inside the TTL window it opens — must
    // not touch the store at all.
    let _ = directory.plane(GUARD_TTL_MS).await;
    assert_eq!(
        store.versions(),
        1,
        "one caller past an expired TTL costs exactly one version read, confirmed-unchanged \
         or not"
    );

    for _ in 0..5 {
        let _ = directory.plane(GUARD_TTL_MS + 1).await;
    }
    assert_eq!(
        store.versions(),
        1,
        "five more callers inside the TTL the confirmed-unchanged refresh just opened must \
         find the stamp fresh and never reach the store — if the stamp only lands on \
         load-or-fail, each of these re-asks a version that never changed"
    );

    // A second TTL has now elapsed since the stamp: due again, and again
    // confirmed-unchanged.
    let _ = directory.plane(2 * GUARD_TTL_MS).await;
    assert_eq!(
        store.versions(),
        2,
        "a second expired TTL costs exactly one more version read — two full TTLs, two reads, \
         however many callers landed inside them"
    );
}

/// R-D2, guard 3: **a refresh publishes only if it loaded something newer.**
///
/// With the load outside the lock, two refreshes can be in flight at once and
/// they can finish out of order. The one that started first may carry the
/// older records, and a publish that only knew "I have finished" would install
/// them over the newer plane — a revocation that arrives and then un-arrives,
/// which is worse than one that arrives a TTL late.
///
/// **Scoped to the version comparison alone, and not a stand-in for the other
/// three guards.** This exercises only the `if loaded.version > current.version`
/// arithmetic — it says nothing about *when* `refreshed_at_ms` gets stamped,
/// so it stays green under a change that moves the stamp (guard 1's and
/// guard 2's subject) or merges the stamp into this same publish step (which
/// would collapse single flight into the version check this test already
/// exercises). Reading this test alone as evidence that a stamp-timing change
/// was safe would be reading a green light this test never turned on; guards
/// 1, 2 and 4 are what watch the stamp.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_that_loaded_an_older_version_does_not_replace_a_newer_one() {
    let (before, after) = staged_mint().await;
    let store = Arc::new(ScriptedDirectoryStore::new(DirectoryRecords::default(), 1).await);
    let directory = scripted(Arc::clone(&store)).await;

    // The first refresh reads version 2 — bo has a membership and no key — and
    // is held open there.
    store.set(before, 2).await;
    let release = store.block_next_load();
    let slow = tokio::spawn({
        let directory = Arc::clone(&directory);
        async move { directory.plane(GUARD_TTL_MS).await }
    });
    store.load_in_flight().await;

    // A second node commits bo's key while that read is open, and a later
    // caller — one TTL on — refreshes past it and finishes first.
    store.set(after, 3).await;
    let fast = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        directory.plane(2 * GUARD_TTL_MS),
    )
    .await
    .expect("the second refresh must not queue behind the first one's open load");
    assert!(
        admits_bo(&fast),
        "the caller that loaded version 3 sees bo's minted key"
    );

    // Now the older refresh completes. Its records are a version behind, and a
    // publish that ignored that would take bo's key away again.
    release.add_permits(1);
    let late = slow.await.expect("the slow task does not panic");
    assert!(
        admits_bo(&late),
        "the refresh that loaded version 2 finished last, and answered with the newer plane \
         rather than with its own stale one"
    );
    assert_eq!(
        directory.version(2 * GUARD_TTL_MS).await,
        3,
        "version 3 is what this node serves: an older load finishing later must not \
         overwrite it"
    );
    let served = directory.plane(2 * GUARD_TTL_MS).await;
    assert!(
        admits_bo(&served),
        "and the plane published beside that version is the one compiled from it"
    );
}

/// R-D3, guard 4: **every refresh failure backs off one TTL, `version()`
/// included.**
///
/// A failed `load` and a failed `compile` already waited a TTL before trying
/// again, and the doc called that deliberate: both are failures that *last*, so
/// retrying per request turns a degraded store into a CPU incident on the node
/// in front of it. A failed `version()` was the exception — it returned before
/// the stamp — which meant the cheapest thing to get wrong was the one thing
/// retried on every admission, and the warning it logs fired once per request
/// rather than once per TTL.
#[tokio::test]
async fn a_failed_version_read_backs_off_one_ttl_like_every_other_failure() {
    let store = Arc::new(ScriptedDirectoryStore::new(DirectoryRecords::default(), 1).await);
    let directory = scripted(Arc::clone(&store)).await;
    store.fail_versions();

    let first = directory.plane(GUARD_TTL_MS).await;
    assert!(
        !admits_bo(&first),
        "a store that cannot answer leaves this node serving the plane it has"
    );
    assert_eq!(
        store.versions(),
        1,
        "the first attempt does reach the store"
    );

    // The same instant again: the failure stamped, so this admission is inside
    // the TTL and must not re-ask.
    let _ = directory.plane(GUARD_TTL_MS).await;
    assert_eq!(
        store.versions(),
        1,
        "a version read that failed backs off one TTL, exactly as a failed load and a failed \
         compile already did — otherwise the cheapest failure is the one retried hardest"
    );

    // One TTL on, the retry is due.
    let _ = directory.plane(2 * GUARD_TTL_MS).await;
    assert_eq!(
        store.versions(),
        2,
        "and the backoff is one TTL, not a latch: the next TTL boundary tries again"
    );
}

/// R-D2, guard 5: **`apply`'s own publish is guarded by the same version rule
/// a refresh's is.**
///
/// `apply` holds a `tokio` mutex across its own load and commit, so nothing
/// else can move the *store* out from under its compare-and-set: whatever
/// version its `commit` returns is, at that instant, the true version the
/// store is at. But `apply` has no await between `commit` returning and its
/// own `write_current()` — so on this node, a concurrent *refresh* (which
/// takes no such mutex) can load that very state via the store, publish it to
/// `current`, and then a *second* write can land and get published by a later
/// refresh, all before `apply`'s own publish ever runs. `apply`'s publish must
/// see that `current` has already moved past its own version and stand down,
/// the same as [`a_refresh_that_loaded_an_older_version_does_not_replace_a_newer_one`]
/// requires of a slow refresh — dropping `apply`'s `if version >
/// current.version` guard would let it clobber that newer state with its own,
/// now-stale one.
///
/// [`ScriptedDirectoryStore::block_next_commit`] is what makes this constructible:
/// `apply`'s `commit` succeeds and mutates the store, then blocks *before
/// returning*, which is the only window in the source with no await to hook —
/// so the test opens one inside the double instead of inside `apply`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_s_publish_does_not_clobber_a_newer_version_a_concurrent_refresh_already_installed() {
    let store = Arc::new(ScriptedDirectoryStore::new(DirectoryRecords::default(), 1).await);
    let directory = scripted(Arc::clone(&store)).await;

    let release_commit = store.block_next_commit();
    let applier = tokio::spawn({
        let directory = Arc::clone(&directory);
        async move {
            directory
                .apply(
                    DirectoryMutation::CreateProject {
                        entry: project("widgets"),
                    },
                    GUARD_TTL_MS,
                )
                .await
                .expect("a fresh project id")
        }
    });
    // `apply`'s commit has already landed in the store and is now blocked
    // before returning: from here on the store answers with `apply`'s own
    // new version, but `apply` itself has not published it yet.
    store.commit_in_flight().await;

    // A refresh, past the TTL, reads that state through the store — not
    // through `apply` — and publishes it to `current` first.
    directory.plane(GUARD_TTL_MS).await;
    let applys_version = directory.version(GUARD_TTL_MS).await;

    // A second write lands directly on the store (a stand-in for another
    // node), and a later refresh picks it up and publishes it too — strictly
    // ahead of `apply`'s own publish, which is still blocked.
    let mut newer = DirectoryRecords::default();
    newer.projects.push(ProjectRecord {
        entry: project("gadgets"),
        provenance: Provenance::Admin,
        created_at_ms: Some(0),
        archived_at_ms: None,
    });
    store.set(newer, applys_version + 1).await;
    directory.plane(2 * GUARD_TTL_MS).await;
    let advanced_version = directory.version(2 * GUARD_TTL_MS).await;
    assert_eq!(
        advanced_version,
        applys_version + 1,
        "sanity: the second write really did get published ahead of `apply`"
    );

    // Only now does `apply`'s own commit return, and it races to publish a
    // version that is, by this point, already stale.
    release_commit.add_permits(1);
    applier.await.expect("the applying task does not panic");

    // Read at `apply`'s own `now_ms`, deliberately, and not a TTL further on:
    // a later read would fall past `current.refreshed_at_ms` again and
    // trigger a fresh refresh of its own, which would reload the newer state
    // straight from the store and quietly repair exactly the clobber this
    // guard exists to prevent — a self-healing read that would pass on the
    // mutation this test exists to catch. Reading immediately, at the instant
    // `apply` itself used, is what makes the assertion about `apply`'s own
    // publish rather than about the next refresh's.
    assert_eq!(
        directory.version(GUARD_TTL_MS).await,
        advanced_version,
        "apply's own publish must not overwrite a newer version a concurrent \
         refresh already installed while apply's commit was blocked"
    );
    let served = directory.view(GUARD_TTL_MS).await;
    assert!(
        served
            .projects
            .iter()
            .any(|project| project.id() == "gadgets"),
        "the newer state must still be what this node serves, not the older \
         one apply's own write would have installed"
    );
}

// ---------------------------------------------------------------------------
// F3 (M16.0 thermo-nuclear review), R-D2′: publish-by-version reads the store's
// version as monotone — which `DirectoryStore` now *requires* of every
// implementation — so a store that answers a lower version than this node has
// already compiled has regressed (a Redis `FLUSHALL`, an eviction, a restore
// from backup, a lagging failover), and `>` alone would discard it forever:
// every refresh would reload and throw away the same state each TTL, and a
// node's own successful commit would be dropped while its caller was told
// `2xx`. The rule is now: a regression is adopted and named. Both guards below
// are timed entirely by the scripted `now_ms` clock the rest of this file uses
// — this is not a concurrency race, so nothing needs a signal to order it: the
// behaviour is a single node taking one refresh, or one apply, after its store
// has moved backward. `ScriptedDirectoryStore::set` is what moves it backward, which is
// the whole of what a store-side regression looks like from this node.
// ---------------------------------------------------------------------------

/// F3, refresh path: **a version regression is adopted once, named, and then
/// converged on — not reloaded and discarded every TTL forever.**
///
/// The store boots at version 5 (a stand-in for "this node already saw a
/// newer state"), then regresses to version 2 the way a restored backup or a
/// flushed cache would. Under `>` alone the node stayed at version 5 for good,
/// re-reading and re-compiling the store's actual state once per TTL and
/// throwing it away each time with no warning — a success that changed
/// nothing, so none of the three `warn!` arms in `compiled()` fired. Under
/// R-D2′ the first refresh past the TTL adopts what the store holds and
/// records why, and the two after it find the store unchanged and cost one
/// version read each: convergence, and a bounded one.
#[tokio::test]
async fn a_version_regression_is_adopted_once_and_then_converged_on() {
    let store = Arc::new(ScriptedDirectoryStore::new(DirectoryRecords::default(), 5).await);
    let directory = scripted(Arc::clone(&store)).await;

    // What a restore from an earlier backup, or a flushed cache resurrected
    // by a lagging replica, looks like from this node's side: the store's
    // whole state stepped backward to a version this node had already moved
    // past.
    let (_before, after) = staged_mint().await;
    store.set(after, 2).await;

    let _ = directory.plane(GUARD_TTL_MS).await;
    let _ = directory.plane(2 * GUARD_TTL_MS).await;
    let _ = directory.plane(3 * GUARD_TTL_MS).await;

    assert_eq!(
        store.versions(),
        3,
        "each of the three refreshes is due and the store really did move \
         (2 != 5), so each pays for a version read"
    );
    assert_eq!(
        store.loads(),
        1,
        "F3: only the first of the three sees a version it does not already \
         hold, so only the first pays for a load -- a node that refused to \
         adopt the regression would still be at version 5, would still find \
         2 != 5 on every later refresh, and would fetch and compile the same \
         regressed state once per TTL forever"
    );

    assert_eq!(
        directory.version(3 * GUARD_TTL_MS).await,
        2,
        "F3: the store is the shared truth, so a version that goes down is \
         adopted rather than discarded -- this node converges on what the \
         store actually holds"
    );
    assert_eq!(
        directory.last_regression(),
        Some(DirectoryRegression {
            from: 5,
            to: 2,
            cause: RegressionCause::Version
        }),
        "F3: and says why it went backwards, naming both versions -- adopting \
         a regression silently would leave an operator with a node that lost \
         state and no record of when or from where"
    );
}

/// M16.1 review, F1 (R-D2″): **a counter that restarted at the very version
/// this node serves is not read as a quiet deployment.**
///
/// The ABA the version comparison above cannot see. `a_version_regression_is_
/// adopted_once_and_then_converged_on` covers a store that answers a *lower*
/// number; this covers the one that answers the *same* number for a different
/// document — an operator `DEL`, a `FLUSHDB`, a restore that did not include
/// this key, after which the next write starts the counter again at 1. The
/// refresh's first question is `version()`, and before the lineage existed the
/// answer `1` was indistinguishable from "nothing has changed": the node
/// short-circuited on `version == claimed_version` and went on serving a plane
/// the deployment had replaced — including keys it had revoked — with no line
/// in any log. What makes it reachable rather than theoretical is the TTL: a
/// young deployment flushed and re-populated inside one TTL never observes a
/// version below the one it claimed, so nothing else in this file would fire.
///
/// The control below is half of this test's evidence: the *same* two records
/// at the *same* two versions, with the lineage carried forward, must still
/// cost nothing and change nothing — otherwise the pair comparison would have
/// bought the ABA at the price of a reload per TTL on every quiet deployment.
#[tokio::test]
async fn a_counter_that_restarted_at_the_served_version_is_not_read_as_unchanged() {
    let (before, after) = staged_mint().await;
    let store = Arc::new(ScriptedDirectoryStore::new(before, 1).await);
    let directory = scripted(Arc::clone(&store)).await;
    let booted = directory.plane(0).await;
    assert!(
        !admits_bo(&booted),
        "fixture premise: this node boots serving version 1, which has no key for bo"
    );

    // The key is gone and written again from nothing, and the new counter's
    // first write lands at version 1 -- the number this node is already
    // serving -- carrying a document that revoked nothing and granted bo a
    // key. Only the lineage separates this from a deployment where nobody has
    // written anything since boot.
    store.set_in_a_new_lineage(after, 1).await;

    let plane = directory.plane(GUARD_TTL_MS).await;
    assert!(
        admits_bo(&plane),
        "F1: the store's key was replaced under this node and its counter \
         restarted at the version this node had claimed; a refresh that \
         compared numbers alone saw `1 == 1`, skipped the load outright, and \
         served the replaced plane until the process restarted"
    );
    assert_eq!(
        store.loads(),
        1,
        "and it noticed by loading once -- the short circuit is what the \
         defect was"
    );
    match directory.last_regression() {
        Some(DirectoryRegression {
            from,
            to,
            cause: RegressionCause::Lineage { .. },
        }) => {
            assert_eq!(
                (from, to),
                (1, 1),
                "the two numbers are equal, which is the whole point: this is a \
                 regression the version fields cannot show on their own"
            );
        }
        other => panic!(
            "a store whose counter restarted has regressed however the numbers compare, and \
             the cause has to say `lineage` rather than report a fall from 1 to 1: {other:?}"
        ),
    }
}

/// The control for the guard above: the same document arriving at the same
/// version *in the same lineage* is what a store is required never to do, and
/// this node still treats an unmoved identity as unmoved.
///
/// This is what proves the pair comparison did not simply turn every refresh
/// into a reload: a quiet deployment must still cost one `version()` read per
/// TTL and no load at all, which is the property `version()` exists for.
#[tokio::test]
async fn an_unmoved_identity_still_costs_one_version_read_and_no_load() {
    let (before, after) = staged_mint().await;
    let store = Arc::new(ScriptedDirectoryStore::new(before, 1).await);
    let directory = scripted(Arc::clone(&store)).await;

    // Same version, same lineage, different records -- a store violating its
    // own contract rather than a real event, and the only way to hold every
    // other variable of the guard above fixed while changing the lineage.
    store.set(after, 1).await;

    let refreshed = directory.plane(GUARD_TTL_MS).await;
    assert!(
        !admits_bo(&refreshed),
        "an identity that has not moved is not a reason to reload"
    );
    assert_eq!(store.versions(), 1, "the cheap read, once");
    assert_eq!(
        store.loads(),
        0,
        "and no load: a node that reloaded on every refresh would pay for the \
         deployment's whole tenancy once per TTL, whether or not it changed"
    );
    assert_eq!(directory.last_regression(), None);
}

/// F3, apply path: **a node's own successful write is published even after the
/// store it wrote to has regressed under it.**
///
/// The node boots caught up to the store at version 3. The store then
/// regresses to version 0 with empty records -- again, a restored backup --
/// and a fresh `apply` reads that regressed state, validates and compiles a
/// new project against it, and commits at version `0 -> 1`. `apply` returns
/// `Ok`, and as far as its caller (an admin `POST`) is concerned the write
/// happened. Under `>` alone it had not: `1 > 3` is false, the publish
/// discarded the result, and the node went on serving the state it had before
/// -- a revocation answered `2xx` and still authenticating until a restart,
/// which is the worse half of F3. The commit landed in the shared store, so it
/// is what this node serves.
#[tokio::test]
async fn apply_publishes_its_own_commit_after_a_store_regression() {
    let store = Arc::new(ScriptedDirectoryStore::new(DirectoryRecords::default(), 3).await);
    let directory = scripted(Arc::clone(&store)).await;
    assert_eq!(
        directory.version(0).await,
        3,
        "sanity: the node booted caught up to the store's version 3, inside \
         its TTL, so this reads the boot-time compile with no extra refresh"
    );

    // The store regresses to version 0 with nothing in it -- the same
    // restored-backup shape as the refresh-path guard above, this time
    // arriving between a boot and the first write this node makes.
    store.set(DirectoryRecords::default(), 0).await;

    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: project("widgets"),
            },
            2 * GUARD_TTL_MS,
        )
        .await
        .expect(
            "apply validates and commits fine against the store's own \
             (regressed) version -- the store has no way to know 0 -> 1 is a \
             regression rather than an ordinary first write",
        );

    let served = directory.view(2 * GUARD_TTL_MS).await;
    assert!(
        served
            .projects
            .iter()
            .any(|project| project.id() == "widgets"),
        "F3: apply returned Ok, so this node's own write must be what it \
         serves -- not silently discarded, forever, because the store's \
         version regressed below what this node had already seen"
    );
    assert_eq!(
        directory.last_regression(),
        Some(DirectoryRegression {
            from: 3,
            to: 0,
            cause: RegressionCause::Version
        }),
        "F3: and the regression that made this publish unconditional is \
         recorded, naming the version this node held and the one the store \
         answered"
    );
}

/// Control for the apply-path guard above: the same write, with the store
/// never regressed, does publish and is immediately visible. This is what
/// proves the assertion above is checking the right thing and is not simply
/// broken in a way that would fail regardless of F3 -- if this one failed
/// too, the fixture would be the story, not the defect.
#[tokio::test]
async fn apply_publishes_its_own_commit_when_the_store_has_not_regressed() {
    let store = Arc::new(ScriptedDirectoryStore::new(DirectoryRecords::default(), 3).await);
    let directory = scripted(Arc::clone(&store)).await;

    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: project("widgets"),
            },
            2 * GUARD_TTL_MS,
        )
        .await
        .expect("apply validates and commits fine against the store's current version");

    let served = directory.view(2 * GUARD_TTL_MS).await;
    assert!(
        served
            .projects
            .iter()
            .any(|project| project.id() == "widgets"),
        "control: with no regression, apply's own write is what this node \
         serves"
    );
    assert_eq!(
        directory.last_regression(),
        None,
        "control: and nothing is recorded as a regression -- the guards above \
         would pass just as well against an `apply` that called every publish \
         a regression, so this is what makes them about the store going \
         backwards"
    );
}

/// F4 (M16.0 review): **a claim dropped mid-load gives up the single-flight
/// token, rather than keeping it for the rest of the TTL.**
///
/// `compiled`'s window two stamps `refreshed_at_ms` *before* the two awaits
/// that follow — `store.version()` and `store.load()` — so if the claiming
/// future is dropped at either (exactly what happens when a client
/// disconnects and the handler future carrying this call is dropped
/// mid-poll), nothing is left to publish a plane. Without a give-back every
/// caller inside the same TTL found the stamp fresh in window one and was
/// served the stale plane the dead claim never refreshed — a revocation
/// landing and not taking effect for up to one whole TTL, silently: no
/// warning fires, because nothing ever reaches the `Err` arms that log one.
/// `ClaimGuard` restores the stamp on drop, which is what this pins.
///
/// `abort()` — not a drop of an unpolled future — is what proves the claim,
/// because it cancels a task that is genuinely suspended inside
/// [`ScriptedDirectoryStore::load`]'s gate, i.e. inside the `store.load().await` this
/// finding names, rather than one that never started. No sleep orders
/// anything: [`ScriptedDirectoryStore::load_in_flight`] is a semaphore signal, and the
/// bound on the second caller is a timeout, not a race against wall time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_dropped_mid_load_gives_up_the_single_flight_token() {
    let (before, after) = staged_mint().await;
    let store = Arc::new(ScriptedDirectoryStore::new(DirectoryRecords::default(), 1).await);
    let directory = scripted(Arc::clone(&store)).await;

    // The store has already moved by the time the claim reads `version()`, so
    // the claim proceeds past the version check and into `load()` — where the
    // gate below blocks it, and where a dropped handler future's cancellation
    // would land in production. `before` is bo's membership without a key, so
    // this state admits nobody: what the dead claim had in hand is not what the
    // final assertion is looking for.
    store.set(before.clone(), 2).await;
    let release = store.block_next_load();

    let claimer = tokio::spawn({
        let directory = Arc::clone(&directory);
        async move { directory.plane(GUARD_TTL_MS).await }
    });
    // Waits for `load` to have entered and be blocked on the gate -- the
    // claim has stamped `refreshed_at_ms` (a synchronous write, before either
    // await) and is now genuinely suspended inside `store.load().await`.
    store.load_in_flight().await;

    // The disconnect: hyper/axum dropping the handler future mid-load, stood
    // in for by aborting the task actually parked in that await.
    claimer.abort();
    let outcome = claimer.await;
    assert!(
        outcome.as_ref().is_err_and(|error| error.is_cancelled()),
        "this test proves nothing about a cancelled claim unless the claiming task was truly \
         cancelled while suspended inside `load()`, not merely finished or panicked: {outcome:?}"
    );

    // The dead claim's own blocked load is released (it has no task left to
    // wake, so this is inert) and the store moves on again -- to the one state
    // that admits bo, which neither the boot nor the version the dead claim was
    // carrying could produce. That is what makes the last assertion say "this
    // caller reached the store and got the newest version" rather than just
    // "this caller returned something".
    release.add_permits(1);
    store.set(after, 3).await;

    // A second caller, inside the very same TTL the dead claim stamped.
    let served = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        directory.plane(GUARD_TTL_MS),
    )
    .await
    .expect("a caller inside the same TTL must not hang behind a claim nothing is refreshing");

    assert_eq!(
        store.loads(),
        2,
        "F4: the aborted claim's own load counts as one; the guard that restores the stamp on \
         drop is what lets this second caller inside the same TTL reclaim and refresh, costing \
         a second load -- with nothing giving the stamp back this caller finds it fresh in \
         window one and never reaches the store at all"
    );
    assert!(
        admits_bo(&served),
        "F4: the plane served to the second caller should be the one compiled from the store's \
         current state, not the one this node had before the dead claim ever started"
    );
}

/// A store whose `version()` can be held open indefinitely and counted, to
/// observe `refreshed_at_ms` while the store has not yet answered anything at
/// all — not even a failure. [`ScriptedDirectoryStore`] cannot do this: its `version`
/// never awaits a gate, only its `load` does.
/// One lineage for every answer this double gives: it scripts *when* the store
/// answers, never *what* it holds, so a lineage that moved would be scripting a
/// second thing by accident.
const GATED_LINEAGE: &str = "gated-version-store";

struct GatedVersionStore {
    records: DirectoryRecords,
    version: u64,
    versions: std::sync::atomic::AtomicUsize,
    /// Signalled once a `version()` call has begun. The signal, never a sleep.
    entered: tokio::sync::Semaphore,
    /// Held by `version()` until the test releases it.
    gate: tokio::sync::Semaphore,
}

impl GatedVersionStore {
    fn new(records: DirectoryRecords, version: u64) -> Self {
        Self {
            records,
            version,
            versions: std::sync::atomic::AtomicUsize::new(0),
            entered: tokio::sync::Semaphore::new(0),
            gate: tokio::sync::Semaphore::new(0),
        }
    }

    fn versions(&self) -> usize {
        self.versions.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Returns once a `version()` call has begun and is parked on the gate.
    async fn version_in_flight(&self) {
        self.entered
            .acquire()
            .await
            .expect("the store outlives the version call it gates")
            .forget();
    }

    fn release(&self) {
        self.gate.add_permits(1);
    }
}

#[async_trait::async_trait]
impl DirectoryStore for GatedVersionStore {
    async fn load(&self) -> Result<VersionedRecords, StoreFailure> {
        // Only ever called at boot in this guard.
        Ok(VersionedRecords {
            records: self.records.clone(),
            version: self.version,
            lineage: GATED_LINEAGE.to_string(),
            compiled_under: CompiledUnder::default(),
        })
    }

    async fn commit(
        &self,
        _expected_version: u64,
        _records: DirectoryRecords,
    ) -> Result<StoredVersion, StoreFailure> {
        unimplemented!("this guard never mutates the directory")
    }

    async fn version(&self) -> Result<StoredVersion, StoreFailure> {
        self.versions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.entered.add_permits(1);
        self.gate
            .acquire()
            .await
            .expect("the gate outlives the version call waiting on it")
            .forget();
        Ok(StoredVersion {
            lineage: GATED_LINEAGE.to_string(),
            version: self.version,
        })
    }
}

/// F5 (M16.0 thermo-nuclear review): **the field doc on
/// `Compiled::refreshed_at_ms` (directory.rs:283-287) reads "When this node
/// last confirmed its snapshot against the store" — but the stamp is written
/// at claim time, before the store is asked anything at all, and a
/// concurrent caller is served as "fresh" off that stamp while the first
/// caller's own `version()` call to the store is still open and has not
/// confirmed (or denied) anything.**
///
/// This is not a behavioral defect — R-D3 (see `compiled()`'s own doc, and
/// guard 4 above) means this ordering is exactly intended: the stamp is the
/// single-flight token, not a confirmation receipt, and it must land before
/// the first fallible call so a failed `version()` backs off like every
/// other failure. But that is a distinct meaning from the field doc's own
/// words, which is the finding: a reader who took the field doc literally
/// would conclude a caller is only ever told "fresh" once the store has
/// actually confirmed the snapshot, which this guard shows is false.
///
/// The bound on the second caller is a `tokio::time::timeout`, not a sleep
/// used for ordering: if `refreshed_at_ms` really were only advanced on
/// confirmation (the field doc's wording taken at face value), the second
/// caller below would find the *old* stamp, fail its own window-one check,
/// go on to contend for window two, and block on this store's still-open
/// `version()` call right alongside the first — which would hang rather than
/// return, so the timeout is what turns that into a clean assertion failure
/// instead of a stalled test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f5_refreshed_at_ms_is_stamped_at_claim_not_at_confirmation() {
    let store = Arc::new(GatedVersionStore::new(DirectoryRecords::default(), 1));
    let directory = Arc::new(
        ControlDirectory::new(
            file_with_ttl(GUARD_TTL_MS),
            PATH,
            Arc::clone(&store) as Arc<dyn DirectoryStore>,
            checks(),
            0,
        )
        .await
        .expect("the file alone compiles, since it is what a boot would have loaded"),
    );

    // Past the TTL: this caller claims the refresh (window two stamps
    // refreshed_at_ms) and then calls into the store's version(), which this
    // store holds open until told otherwise -- the store has confirmed
    // nothing yet.
    let claimant = {
        let directory = Arc::clone(&directory);
        tokio::spawn(async move { directory.plane(GUARD_TTL_MS).await })
    };
    store.version_in_flight().await;

    let second = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        directory.plane(GUARD_TTL_MS),
    )
    .await;
    assert!(
        second.is_ok(),
        "a second caller at the same instant did not return promptly -- which is what the \
         field doc's 'last confirmed' wording predicts: refreshed_at_ms not yet advanced, so \
         this caller would itself contend for window two and block on the store's still-open \
         version() call"
    );

    // Only the first caller ever reached the store: the second was served
    // off the stamp compiled() wrote before asking the store anything, not
    // off anything the store confirmed.
    assert_eq!(
        store.versions(),
        1,
        "F5: the second caller must not have touched the store at all -- if it had, the stamp \
         was not yet 'fresh' when it looked, meaning it was written only on confirmation, which \
         would make the field doc accurate instead of stale"
    );

    store.release();
    let _ = claimant.await.expect("the claimant's task does not panic");
}

// ---------------------------------------------------------------------------
// Divergence: two nodes, one store, different inputs (M16.1, R-D9)
// ---------------------------------------------------------------------------

/// One node of a two-node deployment: its own fingerprint, its own
/// cross-checks, and a directory over the document store both nodes share.
///
/// `file_with_ttl(0)` so every `plane()` call re-asks the store — the
/// divergence check runs on a *refresh*, and a fixture that had to advance a
/// clock to reach one would be measuring the TTL rather than the check.
async fn node(
    documents: Arc<dyn DocumentStore>,
    stamp: CompiledUnder,
    checks: CrossChecks,
    now_ms: u64,
) -> ControlDirectory {
    ControlDirectory::new(
        file_with_ttl(0),
        PATH,
        Arc::new(DocumentDirectoryStore::stamped(documents, stamp)),
        checks,
        now_ms,
    )
    .await
    .expect("the file alone compiles, since it is what a boot would have loaded")
}

/// A fingerprint that declares one file and nothing else.
fn stamped_file(sha256: &str) -> CompiledUnder {
    CompiledUnder {
        file_sha256: Some(sha256.to_string()),
        ..CompiledUnder::default()
    }
}

/// The cross-checks of a node whose one reachable model has this quality
/// prior — the axis two nodes can disagree on while both hold a valid
/// deployment, which is what makes "B cannot compile what A wrote" reachable
/// without either node being misconfigured.
fn checks_at_quality(quality_prior: f64) -> CrossChecks {
    CrossChecks::new(
        vec![Candidate {
            quality_prior,
            ..reachable()
        }],
        None,
    )
}

/// **Divergence is named once per stored version, and names which input.**
///
/// Two nodes mid-rollout: the same directory, different control-plane files.
/// The one that did not write a version is the one that can see the
/// difference, so it says so — once, however many times it re-reads that
/// version — and goes on serving the plane its own file compiles. The writer
/// says nothing, which is the control that keeps this from being a check that
/// fires on everything.
#[tokio::test]
async fn a_document_written_under_another_file_is_named_once_per_version() {
    let documents: Arc<dyn DocumentStore> = Arc::new(MemoryDocumentStore::new());
    let writer = node(Arc::clone(&documents), stamped_file("aaaa"), checks(), 0).await;
    let reader = node(Arc::clone(&documents), stamped_file("bbbb"), checks(), 0).await;

    // Version zero is not a divergence, and this is the assertion that says
    // so: an empty store has no document, so nobody compiled it under
    // anything, and comparing a stamped node against the default fingerprint
    // would report every fresh deployment as divergent on its first boot.
    assert_eq!(reader.status().divergences_named, 0);
    assert_eq!(reader.status().divergence, None);

    writer
        .apply(
            DirectoryMutation::CreateProject {
                entry: project("one"),
            },
            1,
        )
        .await
        .expect("a fresh project id");

    // The reader's first sight of version 1.
    reader.plane(2).await;
    assert_eq!(
        reader.status().divergence,
        Some(DirectoryDivergence {
            version: 1,
            differs: vec![DivergentInput::File],
        }),
        "the one input these two nodes disagree about is the file, and the check must name \
         that rather than reporting `different` and leaving an operator to guess"
    );
    assert_eq!(reader.status().divergences_named, 1);
    assert_eq!(
        reader.status().served_version,
        1,
        "divergence is a fact, never a refusal: the reader compiles what it loaded and keeps \
         serving"
    );

    // Re-read the same version: nothing new to say. Here the refresh does not
    // even load — the version read answers the version this node already
    // serves, so it short-circuits — which is the cheap half of the promise
    // and is what this asserts. The expensive half, a version genuinely
    // re-loaded, is
    // [`a_version_this_node_refuses_is_recorded_beside_the_one_it_serves`],
    // and that is the test the `warned_version` guard itself is pinned by:
    // deleting the guard leaves this one green and that one red.
    reader.plane(3).await;
    reader.plane(4).await;
    assert_eq!(
        reader.status().divergences_named,
        1,
        "once per *stored version*, not once per refresh"
    );

    // A new version is a new document, written under the same foreign file --
    // and is named again, because it is a different document.
    writer
        .apply(
            DirectoryMutation::CreateProject {
                entry: project("two"),
            },
            5,
        )
        .await
        .expect("a fresh project id");
    reader.plane(6).await;
    assert_eq!(reader.status().divergences_named, 2);
    assert_eq!(reader.status().divergence.expect("named again").version, 2);

    // The control. The writer's own documents carry the writer's own
    // fingerprint, so the node that wrote them has nothing to report -- which
    // is what proves the reader's two warnings are about the file rather than
    // about any load at all.
    assert_eq!(writer.status().divergences_named, 0);
    assert_eq!(writer.status().divergence, None);
}

/// **Identical inputs are not a divergence.**
///
/// The other control, and the one that would catch a comparison written the
/// wrong way round: two nodes of one deployment share a file, a catalog and a
/// fleet, and a check that fired here would fire on every ordinary deployment
/// and be turned off within a day.
#[tokio::test]
async fn two_nodes_compiled_from_the_same_inputs_never_diverge() {
    let documents: Arc<dyn DocumentStore> = Arc::new(MemoryDocumentStore::new());
    let writer = node(Arc::clone(&documents), stamped_file("same"), checks(), 0).await;
    let reader = node(Arc::clone(&documents), stamped_file("same"), checks(), 0).await;

    writer
        .apply(
            DirectoryMutation::CreateProject {
                entry: project("one"),
            },
            1,
        )
        .await
        .expect("a fresh project id");
    reader.plane(2).await;

    assert_eq!(reader.status().served_version, 1);
    assert_eq!(reader.status().divergence, None);
    assert_eq!(reader.status().divergences_named, 0);
    assert_eq!(reader.status().refused_version, None);
}

/// **A version this node cannot compile is recorded beside the one it serves,
/// and the old plane keeps serving.**
///
/// The reader's fleet admits less than the writer's, so a policy the writer
/// can start under is one the reader's own cross-checks refuse — a real
/// rolling-upgrade shape and not a misconfiguration on either side. The reader
/// keeps the last plane that compiled, which is the only honest thing it can
/// serve, and records the version it will not: without that number a node
/// stuck behind a refusal and a node merely up to date look identical from
/// `version` alone.
///
/// It is also where "once per stored version" stops being free. A refused
/// version is never published, so the next refresh finds the store still ahead
/// and loads the *same* document again — once per TTL, forever. The
/// divergence count is asserted across two refreshes for exactly that reason.
#[tokio::test]
async fn a_version_this_node_refuses_is_recorded_beside_the_one_it_serves() {
    let documents: Arc<dyn DocumentStore> = Arc::new(MemoryDocumentStore::new());
    let writer = node(
        Arc::clone(&documents),
        stamped_file("aaaa"),
        checks_at_quality(0.9),
        0,
    )
    .await;
    let reader = node(
        Arc::clone(&documents),
        stamped_file("bbbb"),
        checks_at_quality(0.2),
        0,
    )
    .await;

    // Tenancy both nodes can compile, and a key under it, so the plane the
    // reader keeps serving is one with something in it to observe.
    tenancy(&writer, "gated", "gil", 1).await;
    writer
        .mint_turn_key("gated", "gil", 2)
        .await
        .expect("minting a turn key for a membership that exists");
    reader.plane(3).await;
    let good = reader.status().served_version;
    assert!(good > 0, "the reader compiled and published what it read");
    let named_before = reader.status().divergences_named;
    let principal = Principal::new("gated".to_string(), "gil".to_string());
    assert!(
        reader.plane(4).await.membership(&principal).is_ok(),
        "fixture premise: the reader resolves this membership before the refused write"
    );

    // A policy the writer's fleet admits and the reader's does not.
    writer
        .apply(
            DirectoryMutation::PatchProject {
                id: "gated".to_string(),
                patch: ProjectPatch {
                    policy: Some(Some(PolicyConfig {
                        min_quality: Some(0.6),
                        allow: None,
                        frontier_cadence: None,
                    })),
                    ..Default::default()
                },
            },
            5,
        )
        .await
        .expect("the writer's own cross-checks admit a policy its fleet can serve");
    let refused_at = writer.status().served_version;
    assert!(refused_at > good, "the writer published a newer version");

    reader.plane(6).await;
    let status = reader.status();
    assert_eq!(
        status.served_version, good,
        "the reader must keep serving the last plane that compiled on it, not stop \
         authenticating because a neighbour wrote something it cannot serve"
    );
    assert_eq!(
        status.refused_version,
        Some(refused_at),
        "and must record which version it will not serve, beside the one it does"
    );
    assert!(
        reader.plane(7).await.membership(&principal).is_ok(),
        "the old plane is still the served plane, keys and all"
    );

    // The reader loads the same refused version again on every refresh,
    // because it never published it. It says so once.
    let after_first = reader.status().divergences_named;
    assert_eq!(
        after_first,
        named_before + 1,
        "the refused version's own divergence is named -- the check runs on the load, before \
         the compile, precisely so a document this node cannot serve is not the one document \
         it says nothing about"
    );
    reader.plane(8).await;
    reader.plane(9).await;
    assert_eq!(
        reader.status().divergences_named,
        after_first,
        "a version loaded again is not a version seen again"
    );
    assert_eq!(reader.status().refused_version, Some(refused_at));
}

/// **F5: `divergence` reads like `refused_version` -- a fact about *now*,
/// not a scar from the last time the two disagreed.**
///
/// `refused_version` is explicitly cleared the moment a later version
/// compiles (see the publish arm of [`Managed::compiled`]), on the stated
/// reasoning that leaving it standing "would report a node as stuck long
/// after it caught up". `DivergenceState.last` is the sibling field
/// answering the same conceptual question -- is this node currently out of
/// step -- and [`Managed::note_divergence`] now clears it the same way, on
/// an agreeing load.
///
/// The reader here diverges once against the writer's foreign file, then
/// itself writes a document under its *own* fingerprint -- the case where
/// every node agrees again after a rollout completes. `warned_version` is
/// left alone by the same fix (it answers a different question -- which
/// version this node has already told the operator about, not which version
/// it currently disagrees with), so a later divergence at a *new* version
/// still warns; nothing here exercises that half, since D2's own
/// `divergences_named` coverage above already pins it.
#[tokio::test]
async fn divergence_clears_once_a_later_version_agrees() {
    let documents: Arc<dyn DocumentStore> = Arc::new(MemoryDocumentStore::new());
    let writer = node(Arc::clone(&documents), stamped_file("aaaa"), checks(), 0).await;
    let reader = node(Arc::clone(&documents), stamped_file("bbbb"), checks(), 0).await;

    writer
        .apply(
            DirectoryMutation::CreateProject {
                entry: project("one"),
            },
            1,
        )
        .await
        .expect("a fresh project id");

    // The reader sees the writer's foreign-file version and names it.
    reader.plane(2).await;
    assert_eq!(
        reader.status().divergence,
        Some(DirectoryDivergence {
            version: 1,
            differs: vec![DivergentInput::File],
        }),
        "fixture premise: the reader has named the writer's version divergent"
    );

    // The reader now writes its own document, under its own fingerprint --
    // the moment every node agrees again. `apply` also runs
    // `note_divergence` (M16.1, R-D9), and this load agrees with the
    // reader's own inputs, so `differs` is empty.
    reader
        .apply(
            DirectoryMutation::CreateProject {
                entry: project("two"),
            },
            3,
        )
        .await
        .expect("the reader's own fingerprint compiles its own write");
    assert_eq!(
        reader.status().served_version,
        2,
        "fixture premise: the reader's write published a newer version"
    );

    // A further refresh past that version, for the same reason: the fix is
    // in `note_divergence` on *any* agreeing load, not only the one made by
    // `apply` -- so a plain refresh must clear it too.
    reader.plane(4).await;

    assert_eq!(
        reader.status().divergence,
        None,
        "F5: the reader now serves a version written under its own fingerprint, agreeing with \
         every node in the fleet, so it must not go on reporting a divergence named against a \
         version it has long since moved past -- the same staleness `refused_version` is \
         explicitly cleared to avoid"
    );
}
