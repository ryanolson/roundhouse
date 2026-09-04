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
use std::sync::{Arc, Mutex};

use roundhouse_core::control::{Allocation, BudgetWindow, Principal};
use roundhouse_core::routing::{Candidate, Target};

use super::super::budget::{AllocationConfig, BudgetConfig, OnExhaustionConfig};
use super::super::config::{ControlPlaneConfig, PolicyConfig, ProjectEntry, UserEntry};
use super::super::fixtures::{ADMIN_HASH, TURN_HASH, TURN_SECRET, bearer_headers, sample_config};
use super::super::{AuthError, KeyScope, has_valid_key_shape};
use super::*;

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
    directory(Arc::new(MemoryDirectoryStore::new()), now_ms).await
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
    let store: Arc<dyn DirectoryStore> = Arc::new(MemoryDirectoryStore::new());
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
    let store: Arc<dyn DirectoryStore> = Arc::new(MemoryDirectoryStore::new());
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

/// A store that lets another node's write land *between* two reads.
///
/// The defect R2 names needs a write to arrive after a directory has read its
/// plane and before it reads its records. Arranging that with a second thread
/// would be a test that passes on a busy machine and fails on a quiet one, so it
/// is arranged here instead: a directory past its TTL reaches for its store once
/// per refresh, which makes "how many times did this call refresh" observable,
/// and makes the store the one place the other node's write can be timed
/// exactly. The first read answers the version this was built with; the second
/// hands over the records the other node wrote, one version on.
///
/// [`DirectoryStore::version`] is the injection point because every refresh asks
/// it first, whether or not it goes on to load. A caller that refreshes once per
/// snapshot therefore never sees the second answer at all; one that refreshes
/// twice sees both, and that is exactly the pair-from-two-versions this is here
/// to catch.
struct WriteBetweenReads {
    state: Mutex<(DirectoryRecords, u64)>,
    /// What the other node wrote, handed over on the second read.
    pending: Mutex<Option<DirectoryRecords>>,
    reads: Mutex<u64>,
}

impl WriteBetweenReads {
    fn new(before: DirectoryRecords, after: DirectoryRecords) -> Self {
        Self {
            state: Mutex::new((before, 1)),
            pending: Mutex::new(Some(after)),
            reads: Mutex::new(0),
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, (DirectoryRecords, u64)> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

#[async_trait::async_trait]
impl DirectoryStore for WriteBetweenReads {
    async fn load(&self) -> Result<VersionedRecords, StoreFailure> {
        let state = self.locked();
        Ok(VersionedRecords {
            records: state.0.clone(),
            version: state.1,
        })
    }

    async fn commit(
        &self,
        expected_version: u64,
        records: DirectoryRecords,
    ) -> Result<u64, StoreFailure> {
        let mut state = self.locked();
        if state.1 != expected_version {
            return Err(StoreFailure::Concurrent {
                expected: expected_version,
                found: state.1,
            });
        }
        state.0 = records;
        state.1 += 1;
        Ok(state.1)
    }

    async fn version(&self) -> Result<u64, StoreFailure> {
        let mut reads = self.reads.lock().unwrap_or_else(|error| error.into_inner());
        *reads += 1;
        if *reads == 2
            && let Some(after) = self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
        {
            let mut state = self.locked();
            state.0 = after;
            state.1 += 1;
        }
        Ok(self.locked().1)
    }
}

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
/// the other node's write timed by the store — see [`WriteBetweenReads`] — so
/// that the failure is deterministic instead of a race the test hopes to lose.
#[tokio::test]
async fn budget_view_s_plane_and_view_must_describe_the_same_version() {
    let (before, after) = staged_mint().await;
    let store = Arc::new(WriteBetweenReads::new(before, after));
    // TTL zero, because a directory inside its TTL never reaches for the store
    // at all: with a live cache both reads answer from the same `Compiled` by
    // luck rather than by design, and this test would pass on the defect.
    let directory = ControlDirectory::new(file_with_ttl(0), PATH, store, checks(), 0)
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

/// A store a test can stall, count, move and break.
///
/// One double for all four guards below rather than four, extending
/// [`WriteBetweenReads`]'s pattern rather than inventing a second: what changed
/// with M16.0 is that a refresh is now a pair of *awaits*, so the question a
/// double has to be able to answer moved from "how many reads did this call
/// make" to "what was another caller allowed to do while one read was in
/// flight". [`WriteBetweenReads`] scripts a write between two reads and is
/// exactly right for the coherence property it was written for; it has no way
/// to hold a read open, which is the whole subject here.
///
/// Nothing sleeps to order anything: a blocked `load` announces itself on a
/// semaphore and is released on another, so the interleavings below are decided
/// by signals rather than by how busy the machine is. The only clock is
/// [`tokio::time::timeout`], and it is a *bound* on a stall rather than an
/// ordering device — a test that stalls fails in a second instead of hanging
/// the suite, which is what this repo's bounded-run rule wants of a lock test.
struct ScriptedStore {
    state: Mutex<(DirectoryRecords, u64)>,
    /// Every [`DirectoryStore::version`] and [`DirectoryStore::load`] call.
    /// Counted separately because the two answer different questions: one is
    /// the cheap half of a refresh and the other is the expensive half, and
    /// "how many callers paid anything at all" is the single-flight property.
    versions: std::sync::atomic::AtomicUsize,
    loads: std::sync::atomic::AtomicUsize,
    /// When set, every `version` read moves the store on one — a stand-in for
    /// a neighbour node committing continuously, which is what makes "did this
    /// caller refresh" observable without any blocking at all.
    moving: std::sync::atomic::AtomicBool,
    /// When set, `version` answers [`StoreFailure::Unavailable`].
    version_fails: std::sync::atomic::AtomicBool,
    /// Taken by the *next* `load` to arrive, which then waits on it. One load
    /// at a time, so a test can hold one refresh open and let a later one run
    /// to completion past it.
    gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// A permit per `load` that has begun, so a test can wait for one to be in
    /// flight instead of guessing.
    entered: tokio::sync::Semaphore,
    /// Taken by the *next* `commit` to arrive, held **after** the store's
    /// state has already been mutated and **before** the call returns to its
    /// caller. Where `gate` opens a window before a read, this one opens a
    /// window after a write has already landed -- the only place `apply`'s
    /// own commit-to-publish race (guard 5, below) can be driven under test
    /// control, since nothing else in `apply` awaits between `commit`
    /// returning and its own publish.
    commit_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// A permit per `commit` that has already mutated the store and is
    /// waiting on `commit_gate`.
    commit_entered: tokio::sync::Semaphore,
}

impl ScriptedStore {
    fn new(records: DirectoryRecords, version: u64) -> Self {
        Self {
            state: Mutex::new((records, version)),
            versions: std::sync::atomic::AtomicUsize::new(0),
            loads: std::sync::atomic::AtomicUsize::new(0),
            moving: std::sync::atomic::AtomicBool::new(false),
            version_fails: std::sync::atomic::AtomicBool::new(false),
            gate: Mutex::new(None),
            entered: tokio::sync::Semaphore::new(0),
            commit_gate: Mutex::new(None),
            commit_entered: tokio::sync::Semaphore::new(0),
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, (DirectoryRecords, u64)> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// What another node just committed.
    fn set(&self, records: DirectoryRecords, version: u64) {
        let mut state = self.locked();
        state.0 = records;
        state.1 = version;
    }

    /// Forget the store traffic the boot itself made.
    ///
    /// `ControlDirectory::new` loads once to compile what it starts serving, and
    /// every count below is about what a *refresh* costs — so the boot's read is
    /// subtracted here rather than added to each guard's expected number, where
    /// it would read as an unexplained off-by-one.
    fn forget_boot(&self) {
        self.versions.store(0, std::sync::atomic::Ordering::SeqCst);
        self.loads.store(0, std::sync::atomic::Ordering::SeqCst);
        while let Ok(stale) = self.entered.try_acquire() {
            stale.forget();
        }
    }

    fn versions(&self) -> usize {
        self.versions.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn loads(&self) -> usize {
        self.loads.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A neighbour that never stops writing.
    fn keep_moving(&self) {
        self.moving.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn fail_versions(&self) {
        self.version_fails
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Hold the next `load` open. The returned handle releases it.
    ///
    /// Drains the in-flight signal first, because construction already read
    /// the store once: a test that waited on a permit left over from
    /// `ControlDirectory::new` would be told a refresh was in flight before one
    /// had started, and would then race the very caller it means to observe.
    fn block_next_load(&self) -> Arc<tokio::sync::Semaphore> {
        while let Ok(stale) = self.entered.try_acquire() {
            stale.forget();
        }
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *self.gate.lock().unwrap_or_else(|error| error.into_inner()) = Some(Arc::clone(&gate));
        gate
    }

    /// Returns once a `load` has begun. The signal, never a sleep.
    async fn load_in_flight(&self) {
        self.entered
            .acquire()
            .await
            .expect("the store outlives the loads it counts")
            .forget();
    }

    /// Hold the next `commit` open, *after* it has mutated the store's
    /// state. The returned handle releases it.
    fn block_next_commit(&self) -> Arc<tokio::sync::Semaphore> {
        while let Ok(stale) = self.commit_entered.try_acquire() {
            stale.forget();
        }
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *self
            .commit_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::clone(&gate));
        gate
    }

    /// Returns once a `commit` has already mutated the store and is waiting
    /// on the gate `block_next_commit` armed. The signal, never a sleep.
    async fn commit_in_flight(&self) {
        self.commit_entered
            .acquire()
            .await
            .expect("the store outlives the commits it gates")
            .forget();
    }
}

#[async_trait::async_trait]
impl DirectoryStore for ScriptedStore {
    async fn load(&self) -> Result<VersionedRecords, StoreFailure> {
        self.loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let gate = self
            .gate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        // Read before the gate, never after. A load held open is a request
        // already in flight, and what comes back is what the store held when it
        // was made — not what a commit that landed while it was open changed it
        // to. Reading after the gate would make every blocked load answer with
        // the newest records, which is the one thing the out-of-order publish
        // guard needs it not to do.
        let answer = {
            let state = self.locked();
            VersionedRecords {
                records: state.0.clone(),
                version: state.1,
            }
        };
        self.entered.add_permits(1);
        if let Some(gate) = gate {
            gate.acquire()
                .await
                .expect("the gate outlives the load waiting on it")
                .forget();
        }
        Ok(answer)
    }

    async fn commit(
        &self,
        expected_version: u64,
        records: DirectoryRecords,
    ) -> Result<u64, StoreFailure> {
        let new_version = {
            let mut state = self.locked();
            if state.1 != expected_version {
                return Err(StoreFailure::Concurrent {
                    expected: expected_version,
                    found: state.1,
                });
            }
            state.0 = records;
            state.1 += 1;
            state.1
        };
        // The store's state is already advanced by the time this gate is
        // taken -- a caller that reads the store from here on (a concurrent
        // refresh's `version`/`load`) sees this commit's own result, not a
        // stale one. That is what makes this the seam guard 5 needs: `apply`
        // has no await between `commit` returning and its own publish, so
        // this is the only place a test can hold a *successful* commit open
        // long enough for another writer to publish past it first.
        let gate = self
            .commit_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        self.commit_entered.add_permits(1);
        if let Some(gate) = gate {
            gate.acquire()
                .await
                .expect("the gate outlives the commit waiting on it")
                .forget();
        }
        Ok(new_version)
    }

    async fn version(&self) -> Result<u64, StoreFailure> {
        self.versions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.version_fails.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreFailure::Unavailable(
                "the scripted store is refusing version reads".into(),
            ));
        }
        let mut state = self.locked();
        if self.moving.load(std::sync::atomic::Ordering::SeqCst) {
            state.1 += 1;
        }
        Ok(state.1)
    }
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
async fn scripted(store: Arc<ScriptedStore>) -> Arc<ControlDirectory> {
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
    let store = Arc::new(ScriptedStore::new(DirectoryRecords::default(), 1));
    let directory = scripted(Arc::clone(&store)).await;

    // Another node's write, and a load that will not come back until this test
    // says so.
    store.set(after, 2);
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
    let store = Arc::new(ScriptedStore::new(DirectoryRecords::default(), 1));
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
    let store = Arc::new(ScriptedStore::new(DirectoryRecords::default(), 1));
    let directory = scripted(Arc::clone(&store)).await;

    // The first refresh reads version 2 — bo has a membership and no key — and
    // is held open there.
    store.set(before, 2);
    let release = store.block_next_load();
    let slow = tokio::spawn({
        let directory = Arc::clone(&directory);
        async move { directory.plane(GUARD_TTL_MS).await }
    });
    store.load_in_flight().await;

    // A second node commits bo's key while that read is open, and a later
    // caller — one TTL on — refreshes past it and finishes first.
    store.set(after, 3);
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
    let store = Arc::new(ScriptedStore::new(DirectoryRecords::default(), 1));
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
/// [`ScriptedStore::block_next_commit`] is what makes this constructible:
/// `apply`'s `commit` succeeds and mutates the store, then blocks *before
/// returning*, which is the only window in the source with no await to hook —
/// so the test opens one inside the double instead of inside `apply`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_s_publish_does_not_clobber_a_newer_version_a_concurrent_refresh_already_installed() {
    let store = Arc::new(ScriptedStore::new(DirectoryRecords::default(), 1));
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
    store.set(newer, applys_version + 1);
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
