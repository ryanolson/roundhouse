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

fn directory(store: Arc<dyn DirectoryStore>, now_ms: u64) -> ControlDirectory {
    ControlDirectory::new(file(), PATH, store, checks(), now_ms)
        .expect("the file alone compiles, since it is what a boot would have loaded")
}

/// A directory over a store nobody else holds.
fn solo(now_ms: u64) -> ControlDirectory {
    directory(Arc::new(MemoryDirectoryStore::new()), now_ms)
}

fn project(id: &str) -> ProjectEntry {
    ProjectEntry {
        id: id.to_string(),
        name: None,
        policy: None,
        budget: None,
        validate: None,
        credentials: None,
    }
}

fn user(id: &str) -> UserEntry {
    UserEntry { id: id.to_string() }
}

/// Project, user and membership in three writes — what every mint below needs
/// to exist first.
fn tenancy(directory: &ControlDirectory, project_id: &str, user_id: &str, now_ms: u64) {
    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: project(project_id),
            },
            now_ms,
        )
        .expect("a fresh project id");
    directory
        .apply(
            DirectoryMutation::CreateUser {
                entry: user(user_id),
            },
            now_ms,
        )
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
        .expect("a membership neither half declares");
}

/// What a presented secret resolves to, through the whole header seam.
fn resolve(directory: &ControlDirectory, secret: &str, now_ms: u64) -> Result<KeyScope, AuthError> {
    directory.plane(now_ms).scope(&bearer_headers(secret))
}

/// The row a presented secret is refused by, or `None` if it was admitted.
///
/// A projection rather than an equality on the `Result`, because [`KeyScope`]
/// deliberately does not derive `PartialEq` — comparing two of them would be
/// comparing two resolved policies, which is not what any assertion here is
/// about. See the note on `Resolved` in the resolver's own tests.
fn refusal(directory: &ControlDirectory, secret: &str, now_ms: u64) -> Option<AuthError> {
    resolve(directory, secret, now_ms).err()
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

#[test]
fn minting_stores_only_the_hash_and_tail() {
    let directory = solo(0);
    tenancy(&directory, "widgets", "bo", 1_000);
    let minted = directory
        .mint_turn_key("widgets", "bo", 2_000)
        .expect("the membership exists and the policy admits the catalog");

    // The secret works, which is what makes the rest of this test about a key
    // rather than about a string.
    assert_eq!(
        principal_of(resolve(&directory, &minted.secret, 2_000)),
        Some(Principal::new("widgets", "bo"))
    );

    let view = directory.view(2_000);
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
#[test]
fn every_minted_secret_passes_the_shape_check_this_deployment_applies() {
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

#[test]
fn a_revoked_key_compiles_to_a_named_refusal() {
    let directory = solo(0);
    tenancy(&directory, "widgets", "bo", 1_000);
    let minted = directory.mint_turn_key("widgets", "bo", 2_000).unwrap();
    let id = key_id(&minted.key_sha256);

    assert!(
        principal_of(resolve(&directory, &minted.secret, 2_000)).is_some(),
        "the probe has to work before it is revoked, or the assertion below is \
         satisfied by a key that never resolved"
    );

    directory
        .apply(DirectoryMutation::RevokeKey { id: id.clone() }, 3_000)
        .expect("an API-minted key is the API's to revoke");

    assert_eq!(
        refusal(&directory, &minted.secret, 3_000),
        Some(AuthError::RevokedKey),
        "revoked, and told apart from a key this deployment never had"
    );

    // The distinction, stated as a difference rather than as a single answer: a
    // well-shaped secret nobody ever issued is `unknown_key`, and an operator
    // reading a log needs those two to be different words.
    let never_issued = "rh_turn_ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ";
    assert_eq!(
        refusal(&directory, never_issued, 3_000),
        Some(AuthError::UnknownKey)
    );
    assert_ne!(AuthError::RevokedKey.code(), AuthError::UnknownKey.code());

    // And the row survives its own revocation, which is what a tombstone is
    // for: the operator who revoked it can still see that it existed.
    let view = directory.view(3_000);
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
        .expect("revoking a revoked key is the state it is already in");
}

#[test]
fn an_archived_projects_key_refuses_project_archived() {
    let directory = solo(0);
    tenancy(&directory, "widgets", "bo", 1_000);
    tenancy(&directory, "gadgets", "cy", 1_000);
    let closing = directory.mint_turn_key("widgets", "bo", 2_000).unwrap();
    let staying = directory.mint_turn_key("gadgets", "cy", 2_000).unwrap();

    directory
        .apply(
            DirectoryMutation::ArchiveProject {
                id: "widgets".into(),
            },
            3_000,
        )
        .expect("an API-created project is the API's to close");

    assert_eq!(
        refusal(&directory, &closing.secret, 3_000),
        Some(AuthError::ProjectArchived),
        "the key is intact and its project is closed, which is a different \
         remedy from a revoked key and so a different row"
    );
    // CONTROL: archiving one project closed one project.
    assert_eq!(
        principal_of(resolve(&directory, &staying.secret, 3_000)),
        Some(Principal::new("gadgets", "cy"))
    );

    // Archived, not deleted: the row keeps the id, so nothing else can be
    // created under it and join two tenants' spend histories.
    let view = directory.view(3_000);
    let record = view
        .projects
        .iter()
        .find(|project| project.id() == "widgets")
        .expect("an archived project is still listed");
    assert_eq!(record.archived_at_ms, Some(3_000));
    assert!(matches!(
        directory.apply(
            DirectoryMutation::CreateProject {
                entry: project("widgets")
            },
            4_000
        ),
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
#[test]
fn the_direct_archived_key_refusal_is_not_the_projects_exclusion_in_disguise() {
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

#[test]
fn a_config_owned_entity_refuses_mutation() {
    let directory = solo(0);
    // `acme`, `ada` and both hashes come from the file — see `sample_config`.
    let mutations: Vec<(&str, DirectoryMutation)> = vec![
        (
            "patching a configured project",
            DirectoryMutation::PatchProject {
                id: "acme".into(),
                patch: ProjectPatch {
                    name: Some("Renamed".into()),
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
        let error = match directory.apply(mutation, 1_000) {
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
        .expect("a membership in a configured project is a create, not an edit");
    let minted = directory
        .mint_turn_key("acme", "bo", 1_000)
        .expect("and its keys are the API's to mint");
    assert_eq!(
        principal_of(resolve(&directory, &minted.secret, 1_000)),
        Some(Principal::new("acme", "bo"))
    );
    // The file's own key is untouched by any of it.
    assert_eq!(
        principal_of(resolve(&directory, TURN_SECRET, 1_000)),
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
#[test]
fn patch_project_refuses_ownership_before_it_ever_looks_at_the_records_table() {
    let directory = solo(0);
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
                    name: Some("Renamed".into()),
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
                    name: Some("Renamed".into()),
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

#[test]
fn an_admin_create_colliding_with_config_identity_is_refused() {
    let directory = solo(0);

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
        let error = directory.apply(mutation, 1_000).expect_err(what);
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
        .unwrap();
    assert!(matches!(
        directory.apply(
            DirectoryMutation::CreateProject {
                entry: project("widgets")
            },
            1_000
        ),
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
#[test]
fn a_stale_view_refuses_a_revoked_key_after_one_ttl() {
    let store: Arc<dyn DirectoryStore> = Arc::new(MemoryDirectoryStore::new());
    let writer = directory(Arc::clone(&store), 0);
    tenancy(&writer, "widgets", "bo", 0);
    let doomed = writer.mint_turn_key("widgets", "bo", 0).unwrap();
    let untouched = writer.mint_turn_key("widgets", "bo", 0).unwrap();

    // The reader compiles the same state, at the same instant.
    let reader = directory(Arc::clone(&store), 0);
    let ttl = DEFAULT_ADMISSION_CACHE_TTL_MS;
    assert!(principal_of(resolve(&reader, &doomed.secret, 0)).is_some());

    writer
        .apply(
            DirectoryMutation::RevokeKey {
                id: key_id(&doomed.key_sha256),
            },
            100,
        )
        .unwrap();

    // The writing node: immediate. A write recompiles and swaps in the same
    // call, so there is no window at all on the node the operator used.
    assert_eq!(
        refusal(&writer, &doomed.secret, 100),
        Some(AuthError::RevokedKey)
    );

    // The reading node, inside the TTL: still admitting it. This is the
    // staleness the bound *permits*, written down so that shortening or
    // lengthening it is a decision somebody makes rather than a behavior that
    // drifts.
    assert!(
        principal_of(resolve(&reader, &doomed.secret, ttl - 1)).is_some(),
        "inside the TTL a second node is allowed to be behind"
    );

    // And at the bound: refreshed, and refusing by name.
    assert_eq!(
        refusal(&reader, &doomed.secret, ttl),
        Some(AuthError::RevokedKey),
        "one TTL is the whole of the staleness window"
    );

    // CONTROL: the refresh is a recompile, not a wipe.
    assert_eq!(
        principal_of(resolve(&reader, &untouched.secret, ttl)),
        Some(Principal::new("widgets", "bo")),
        "a key nobody revoked must survive the refresh that removed one that was"
    );
    // And the file's own key, which no admin write ever touched.
    assert_eq!(
        principal_of(resolve(&reader, TURN_SECRET, ttl)),
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
#[test]
fn a_zero_ttl_refreshes_within_the_same_millisecond() {
    let store: Arc<dyn DirectoryStore> = Arc::new(MemoryDirectoryStore::new());
    let writer = directory(Arc::clone(&store), 7);
    // Two readers over the same store, built at the same instant from the same
    // file, differing in one number.
    let eager =
        ControlDirectory::new(file_with_ttl(0), PATH, Arc::clone(&store), checks(), 7).unwrap();
    let patient = directory(Arc::clone(&store), 7);

    tenancy(&writer, "widgets", "bo", 7);
    let minted = writer.mint_turn_key("widgets", "bo", 7).unwrap();

    assert!(
        principal_of(resolve(&eager, &minted.secret, 7)).is_some(),
        "a zero-TTL node picks up a write made in the same millisecond"
    );
    // CONTROL: the same store, the same instant, the default TTL. Without this
    // the assertion above would be satisfied by a store that is simply fast.
    assert_eq!(
        refusal(&patient, &minted.secret, 7),
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
        .unwrap();
    assert_eq!(
        refusal(&eager, &minted.secret, 7),
        Some(AuthError::RevokedKey)
    );
}

// ---------------------------------------------------------------------------
// What a mutation may not do
// ---------------------------------------------------------------------------

#[test]
fn a_window_change_is_refused_naming_the_mechanism() {
    let directory = solo(0);
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
        .unwrap();

    let error = directory
        .apply(
            DirectoryMutation::PatchProject {
                id: "widgets".into(),
                patch: ProjectPatch {
                    budget: Some(BudgetConfig {
                        limit_usd: 10.0,
                        window: BudgetWindow::Monthly,
                        on_exhaustion: OnExhaustionConfig::Refuse,
                        overflow_when_local_saturated: None,
                        warn_at: None,
                    }),
                    ..ProjectPatch::default()
                },
            },
            2_000,
        )
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
                    budget: Some(BudgetConfig {
                        limit_usd: 25.0,
                        window: BudgetWindow::Total,
                        on_exhaustion: OnExhaustionConfig::Refuse,
                        overflow_when_local_saturated: None,
                        warn_at: Some(0.5),
                    }),
                    ..ProjectPatch::default()
                },
            },
            3_000,
        )
        .expect("a limit change on the same window is an ordinary edit");
    let view = directory.view(3_000);
    let budget = view
        .projects
        .iter()
        .find(|project| project.id() == "widgets")
        .and_then(|project| project.entry.budget.as_ref())
        .expect("the project still has a budget");
    assert_eq!(budget.limit_usd, 25.0);
}

/// A key minted at runtime is judged by the cross-checks a boot would apply.
///
/// The failure this closes: an admin plane is a way to write a configuration
/// the process refuses to *start* under, and the symptom arrives at the next
/// restart — the furthest point in time from the cause.
#[test]
fn a_mutation_that_admits_no_model_is_refused() {
    let directory = solo(0);
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
        .expect("a policy with no key under it refuses no turns");
    directory
        .apply(DirectoryMutation::CreateUser { entry: user("bo") }, 1_000)
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
        .unwrap();

    // Read at 1_000, well inside the default TTL, so this is the version this
    // node has *compiled* rather than one a refresh went and fetched. A fixture
    // edit that pushed these timestamps past the TTL would turn the assertion
    // below into a store read and stop it being about the write path at all.
    let before = directory.version(1_000);
    let error = directory
        .mint_turn_key("narrow", "bo", 2_000)
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
    assert_eq!(directory.version(2_000), before);
    assert!(
        directory.view(2_000).keys.iter().all(|key| key.scope
            != KeyRecordScope::Turn {
                project: "narrow".into(),
                user: "bo".into()
            }),
        "the refused mint left no record behind"
    );

    // CONTROL: the same three writes under a policy that does name the catalog.
    tenancy(&directory, "wide", "cy", 3_000);
    directory
        .apply(
            DirectoryMutation::PatchProject {
                id: "wide".into(),
                patch: ProjectPatch {
                    policy: Some(PolicyConfig {
                        allow: Some(vec!["echo/*".into()]),
                        ..PolicyConfig::default()
                    }),
                    ..ProjectPatch::default()
                },
            },
            3_000,
        )
        .unwrap();
    directory
        .mint_turn_key("wide", "cy", 3_000)
        .expect("a policy that names the one model this deployment has");
}

#[test]
fn deleting_a_membership_revokes_its_minted_keys() {
    let directory = solo(0);
    tenancy(&directory, "widgets", "bo", 1_000);
    let first = directory.mint_turn_key("widgets", "bo", 1_000).unwrap();
    let second = directory.mint_turn_key("widgets", "bo", 1_000).unwrap();
    // A neighbour in the same project, to prove the cascade follows the
    // membership rather than the project.
    directory
        .apply(DirectoryMutation::CreateUser { entry: user("cy") }, 1_000)
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
        .unwrap();
    let neighbour = directory.mint_turn_key("widgets", "cy", 1_000).unwrap();

    directory
        .apply(
            DirectoryMutation::DeleteMembership {
                project: "widgets".into(),
                user: "bo".into(),
            },
            2_000,
        )
        .expect("an API-created membership is the API's to remove");

    for secret in [&first.secret, &second.secret] {
        assert_eq!(
            refusal(&directory, secret, 2_000),
            Some(AuthError::RevokedKey),
            "a key whose membership is gone resolves to nothing, and `revoked` \
             is the answer that stays explicable to whoever removed the member"
        );
    }
    // CONTROL: the neighbour is untouched.
    assert_eq!(
        principal_of(resolve(&directory, &neighbour.secret, 2_000)),
        Some(Principal::new("widgets", "cy"))
    );

    let view = directory.view(2_000);
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
#[test]
fn delete_membership_s_cascade_revokes_keys_inside_mutate_before_any_compile_runs() {
    let directory = solo(0);
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
#[test]
fn two_keys_of_one_membership_never_disagree_after_an_upsert() {
    let directory = solo(0);
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
        .unwrap();
    directory
        .apply(DirectoryMutation::CreateUser { entry: user("bo") }, 1_000)
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
        .unwrap();

    let first = directory.mint_turn_key("widgets", "bo", 1_000).unwrap();
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
        .unwrap();
    let second = directory.mint_turn_key("widgets", "bo", 3_000).unwrap();

    let plane = directory.plane(3_000);
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
#[test]
fn the_view_lists_both_halves_and_marks_which_is_which() {
    let directory = solo(0);
    tenancy(&directory, "widgets", "bo", 1_000);
    directory.mint_turn_key("widgets", "bo", 1_000).unwrap();

    let view = directory.view(1_000);
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

/// A reference to something neither half declares is a 404, not a compile
/// failure.
///
/// The split the error type exists for: "there is no such project" is a fact
/// about this request, and it must not arrive as the compiler's
/// `UnknownProject`, whose message is about a *file* the caller never wrote.
#[test]
fn a_membership_naming_nothing_is_refused_before_anything_compiles() {
    let directory = solo(0);
    assert!(matches!(
        directory.apply(
            DirectoryMutation::UpsertMembership {
                project: "nowhere".into(),
                user: "ada".into(),
                role: MembershipRole::Member,
                allocation: None,
                overrides: None,
            },
            1_000
        ),
        Err(DirectoryError::UnknownProject { .. })
    ));
    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: project("widgets"),
            },
            1_000,
        )
        .unwrap();
    assert!(matches!(
        directory.apply(
            DirectoryMutation::UpsertMembership {
                project: "widgets".into(),
                user: "nobody".into(),
                role: MembershipRole::Member,
                allocation: None,
                overrides: None,
            },
            1_000
        ),
        Err(DirectoryError::UnknownUser { .. })
    ));
    assert!(matches!(
        directory.apply(
            DirectoryMutation::RevokeKey {
                id: "key_0000000000000000".into()
            },
            1_000
        ),
        Err(DirectoryError::UnknownKey { .. })
    ));
    assert!(matches!(
        directory.apply(
            DirectoryMutation::DeleteMembership {
                project: "widgets".into(),
                user: "nobody".into(),
            },
            1_000
        ),
        Err(DirectoryError::UnknownMembership { .. })
    ));
}
