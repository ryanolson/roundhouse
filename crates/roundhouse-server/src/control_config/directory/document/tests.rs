// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The codec, the schema gate, and the two answers a document store can give.
//!
//! Everything about the *compare-and-set* is pinned one crate down, by the
//! document-store contract run against both backends; what is left here is
//! what only this crate can be wrong about — the bytes, the schema number, and
//! the mapping from a store's two failures onto the two the directory's
//! callers already distinguish.

use std::sync::Arc;

use roundhouse_core::control::directory::{
    DocumentStore, DocumentVersion, MemoryDocumentStore, VersionedDocument,
};

use super::*;
use crate::control_config::budget::{AllocationConfig, BudgetConfig, OnExhaustionConfig};
use crate::control_config::config::{PolicyConfig, ProjectEntry, TiersConfig, UserEntry};
use crate::control_config::credentials::{CredentialsConfig, ProviderCredentialConfig};
use crate::control_config::fair_use::{FairUseConfig, FairUseWindowConfig};
use crate::control_config::validate::{ArmSharesConfig, ValidateConfig};
use roundhouse_core::control::policy::FrontierCadence;
use roundhouse_core::control::{BudgetCounts, BudgetWindow, CredentialMode, FairUseWindow};
use roundhouse_core::routing::PickerMode;
use roundhouse_core::validate::SteerChannel;

use super::super::records::{
    ApiKeyRecord, KeyRecordScope, MembershipRecord, MembershipRole, ProjectRecord, Provenance,
    UserRecord,
};

fn memory() -> DocumentDirectoryStore {
    DocumentDirectoryStore::over(Arc::new(MemoryDocumentStore::new()))
}

/// A `DirectoryRecords` with **every optional field populated** — the fixture
/// the byte-for-byte pin is taken over.
///
/// Every `Option` is `Some` and every collection non-empty on purpose: a
/// fixture built from defaults would round-trip through a codec that had
/// dropped half the vocabulary, because `None` in and `None` out is what a
/// dropped field looks like. The one thing deliberately *not* populated is a
/// second row per collection — one of each is enough to pin a shape, and four
/// more rows would make the literal below unreadable without pinning anything
/// new.
fn every_field_populated() -> DirectoryRecords {
    DirectoryRecords {
        projects: vec![ProjectRecord {
            entry: ProjectEntry {
                id: "acme".into(),
                name: Some("Acme Corp".into()),
                policy: Some(PolicyConfig {
                    min_quality: Some(0.5),
                    allow: Some(vec!["local/*".into()]),
                    frontier_cadence: Some(FrontierCadence {
                        max_frontier: 1,
                        per_turns: 4,
                    }),
                }),
                budget: Some(BudgetConfig {
                    limit_usd: 25.0,
                    window: BudgetWindow::Monthly,
                    on_exhaustion: OnExhaustionConfig::DegradeToLocal,
                    overflow_when_local_saturated: Some(false),
                    warn_at: Some(0.75),
                }),
                fair_use: Some(FairUseConfig {
                    windows: vec![FairUseWindowConfig {
                        window: FairUseWindow::FiveHours,
                        max_tokens: Some(1_000),
                        max_usd: Some(2.5),
                    }],
                }),
                validate: Some(ValidateConfig {
                    enabled: true,
                    channel: SteerChannel::Text,
                    arms: ArmSharesConfig {
                        live: 1,
                        shadow: 2,
                        placebo: 3,
                    },
                    placebo_rate: 0.1,
                    escalation_floor: 0.8,
                    escalation_turns: 3,
                    steer_after_interventions: 1,
                    handoff_note: Some("say why".into()),
                }),
                credentials: Some(CredentialsConfig {
                    mode: Some(CredentialMode::ProjectOnly),
                    budget_counts: Some(BudgetCounts::AllFrontierSpend),
                    providers: [(
                        "anthropic".to_string(),
                        ProviderCredentialConfig {
                            env_var: "ACME_ANTHROPIC_KEY".into(),
                            kind: Some("api_key".into()),
                        },
                    )]
                    .into_iter()
                    .collect(),
                }),
                tiers: Some(TiersConfig {
                    capable: vec!["anthropic/big".into()],
                    efficient: vec!["local/small".into()],
                    picker: PickerMode::EfficientFirst,
                    confidence_threshold: Some(0.65),
                }),
            },
            provenance: Provenance::Admin,
            created_at_ms: Some(1_700_000_000_000),
            archived_at_ms: Some(1_700_000_001_000),
        }],
        users: vec![UserRecord {
            entry: UserEntry { id: "ada".into() },
            provenance: Provenance::Admin,
            created_at_ms: Some(1_700_000_002_000),
        }],
        memberships: vec![MembershipRecord {
            project: "acme".into(),
            user: "ada".into(),
            role: Some(MembershipRole::Owner),
            allocation: Some(AllocationConfig::Capped { limit_usd: 5.0 }),
            overrides: Some(PolicyConfig {
                min_quality: Some(0.9),
                allow: Some(vec!["anthropic/*".into()]),
                frontier_cadence: None,
            }),
            provenance: Provenance::Admin,
            created_at_ms: Some(1_700_000_003_000),
        }],
        keys: vec![ApiKeyRecord {
            id: "key_0123456789abcdef".into(),
            key_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            display_tail: Some("wxyz".into()),
            scope: KeyRecordScope::Turn {
                project: "acme".into(),
                user: "ada".into(),
            },
            provenance: Provenance::Admin,
            created_at_ms: Some(1_700_000_004_000),
            revoked_at_ms: Some(1_700_000_005_000),
            fair_use: Some(FairUseConfig {
                windows: vec![FairUseWindowConfig {
                    window: FairUseWindow::SevenDays,
                    max_tokens: None,
                    max_usd: Some(9.0),
                }],
            }),
        }],
    }
}

/// The document [`every_field_populated`] writes, byte for byte.
///
/// **The literal is the point**, exactly as it is for
/// `a_pre_m11_log_record_still_deserializes` one crate down. An argument that
/// a `#[derive(Serialize)]` cannot go wrong is true right up until someone
/// reorders a field, renames a variant tag, reaches for `#[serde(untagged)]`,
/// or adds `skip_serializing_if` to a field an older build reads as required —
/// and this document is durable, so the first symptom of getting it wrong is a
/// deployment whose entire tenancy no longer loads.
const PINNED: &str = r#"{"schema":1,"records":{"projects":[{"entry":{"id":"acme","name":"Acme Corp","policy":{"min_quality":0.5,"allow":["local/*"],"frontier_cadence":{"max_frontier":1,"per_turns":4}},"budget":{"limit_usd":25.0,"window":"monthly","on_exhaustion":"degrade_to_local","overflow_when_local_saturated":false,"warn_at":0.75},"fair_use":{"windows":[{"window":"5h","max_tokens":1000,"max_usd":2.5}]},"validate":{"enabled":true,"channel":"text","arms":{"live":1,"shadow":2,"placebo":3},"placebo_rate":0.1,"escalation_floor":0.8,"escalation_turns":3,"steer_after_interventions":1,"handoff_note":"say why"},"credentials":{"mode":"project_only","budget_counts":"all_frontier_spend","providers":{"anthropic":{"env_var":"ACME_ANTHROPIC_KEY","kind":"api_key"}}},"tiers":{"capable":["anthropic/big"],"efficient":["local/small"],"picker":"efficient_first","confidence_threshold":0.65}},"provenance":"admin","created_at_ms":1700000000000,"archived_at_ms":1700000001000}],"users":[{"entry":{"id":"ada"},"provenance":"admin","created_at_ms":1700000002000}],"memberships":[{"project":"acme","user":"ada","role":"owner","allocation":{"capped":{"limit_usd":5.0}},"overrides":{"min_quality":0.9,"allow":["anthropic/*"],"frontier_cadence":null},"provenance":"admin","created_at_ms":1700000003000}],"keys":[{"id":"key_0123456789abcdef","key_sha256":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","display_tail":"wxyz","scope":{"turn":{"project":"acme","user":"ada"}},"provenance":"admin","created_at_ms":1700000004000,"revoked_at_ms":1700000005000,"fair_use":{"windows":[{"window":"7d","max_usd":9.0}]}}]},"compiled_under":{"file_sha256":null,"catalog":[],"fleet":[],"admission_cache_ttl_ms":null,"judge":null}}"#;

/// **A fully populated directory is written exactly this way, and reads back
/// exactly this way.**
///
/// Both directions, for the reason the item codec asserts both: writing proves
/// a new build does not start emitting a shape an older node cannot read, and
/// reading proves an existing document still loads. Asserted through the
/// adapter rather than on `serde_json` directly, so the envelope — the schema
/// number, the fingerprint slot, the field order — is part of what is pinned.
#[tokio::test]
async fn a_fully_populated_directory_round_trips_byte_for_byte() {
    let backing = Arc::new(MemoryDocumentStore::new());
    let store = DocumentDirectoryStore::over(Arc::clone(&backing) as Arc<dyn DocumentStore>);

    store.commit(0, every_field_populated()).await.unwrap();
    let written = backing.load().await.unwrap().document.expect("a document");
    assert_eq!(
        String::from_utf8(written).expect("the envelope is JSON and therefore UTF-8"),
        PINNED,
        "the stored document's bytes are a durable format; a reordered field \
         or a renamed tag here is a deployment whose tenancy an older node \
         can no longer read"
    );

    // And the other direction: the pinned literal loads, and writing what it
    // loaded reproduces it. A codec that dropped a field would pass the write
    // half above (it wrote what it holds) and fail here.
    let reading = holding(PINNED.as_bytes()).await;
    let loaded = reading.load().await.unwrap();
    assert_eq!(loaded.version, 1);
    let echo = Arc::new(MemoryDocumentStore::new());
    DocumentDirectoryStore::over(Arc::clone(&echo) as Arc<dyn DocumentStore>)
        .commit(0, loaded.records)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(echo.load().await.unwrap().document.unwrap()).unwrap(),
        PINNED,
        "reading the pinned document and writing it back must reproduce it"
    );
}

/// A directory over a store already holding `bytes` at version 1.
///
/// Written through the document store's own `commit` rather than by reaching
/// into its state, so a fixture cannot put the store in a shape no sequence of
/// real calls could reach.
async fn holding(bytes: &[u8]) -> DocumentDirectoryStore {
    let store = MemoryDocumentStore::new();
    store
        .commit(0, bytes.to_vec())
        .await
        .expect("a fresh store's first commit is against version 0");
    DocumentDirectoryStore::over(Arc::new(store))
}

/// **A document one schema ahead is refused, by name.**
///
/// Fail closed, and the reason is worth stating on the assertion rather than
/// only in the module doc: the alternative is a plane compiled from the half
/// of a document this build recognises, which admits and refuses the wrong
/// callers with nothing anywhere saying so. A stopped boot is loud; a plane
/// missing half its keys is not.
#[tokio::test]
async fn a_document_from_a_newer_build_is_refused_with_the_schema_in_the_reason() {
    let ahead = format!(
        r#"{{"schema":{},"records":{{}},"compiled_under":{{}}}}"#,
        DIRECTORY_DOCUMENT_SCHEMA + 1
    );
    let store = holding(ahead.as_bytes()).await;

    match store.load().await {
        Err(StoreFailure::Unavailable(reason)) => {
            assert!(
                reason.contains(&format!("schema {}", DIRECTORY_DOCUMENT_SCHEMA + 1)),
                "the reason names the schema found: {reason}"
            );
            assert!(
                reason.contains(&format!("up to schema {DIRECTORY_DOCUMENT_SCHEMA}")),
                "and the schema this build reads, so an operator can tell \
                 which node to upgrade: {reason}"
            );
        }
        other => panic!("a newer document must be refused, not read: {other:?}"),
    }

    // CONTROL: the identical document at *this* schema loads, and loads as the
    // empty directory. Without it, a `load` that refused everything would pass
    // the assertion above.
    let same =
        format!(r#"{{"schema":{DIRECTORY_DOCUMENT_SCHEMA},"records":{{}},"compiled_under":{{}}}}"#);
    let store = holding(same.as_bytes()).await;
    let loaded = store.load().await.expect("a document at this schema reads");
    assert!(loaded.records.projects.is_empty());
    assert_eq!(loaded.version, 1);
}

/// An unknown *envelope* key at a known schema is tolerated; an unknown key
/// inside a record is not.
///
/// The asymmetry the module doc argues for, driven rather than asserted in
/// prose. The first half is what lets a build add a fourth top-level key
/// without breaking the older half of a fleet mid-upgrade. The second is the
/// file vocabulary's own `deny_unknown_fields`, inherited deliberately: those
/// entries are strict because a mistyped key in an operator's file silently
/// widens a policy, and softening them here would give the file back the
/// failure mode the attribute exists to prevent — so a vocabulary change is a
/// `schema` bump, and an older node says so instead of dropping a field.
#[tokio::test]
async fn an_unknown_envelope_key_is_tolerated_and_an_unknown_record_key_is_not() {
    let extra_envelope =
        br#"{"schema":1,"records":{"users":[]},"compiled_under":{},"written_by":"a newer build"}"#;
    let store = holding(extra_envelope).await;
    let loaded = store
        .load()
        .await
        .expect("an envelope key this build does not know is not its business");
    assert!(loaded.records.users.is_empty());

    let extra_entry = br#"{"schema":1,"records":{"users":[{"entry":{"id":"ada","email":"a@b"},"provenance":"admin"}]},"compiled_under":{}}"#;
    let store = holding(extra_entry).await;
    match store.load().await {
        Err(StoreFailure::Unavailable(reason)) => {
            assert!(reason.contains("schema 1"), "{reason}");
        }
        other => panic!(
            "a config entry carrying a field this build has never heard of is \
             a vocabulary change that should have bumped the schema, and it \
             must be named rather than dropped: {other:?}"
        ),
    }
}

/// A missing optional field reads as absent rather than failing the whole
/// directory.
///
/// The other half of the compatibility rule (`#[serde(default)]` on every
/// optional field): a newer build reading a document an older build wrote must
/// not refuse it because a field it learned about last week is not there. The
/// minimal record below is what an older writer's row looks like — an entry, a
/// provenance, and nothing else.
#[tokio::test]
async fn a_record_missing_every_optional_field_still_loads() {
    let minimal = br#"{"schema":1,"records":{"projects":[{"entry":{"id":"acme"},"provenance":"config"}],"keys":[{"id":"key_x","key_sha256":"ff","scope":"admin","provenance":"admin"}]},"compiled_under":{}}"#;
    let store = holding(minimal).await;
    let loaded = store
        .load()
        .await
        .expect("an older writer's row still reads");

    let project = &loaded.records.projects[0];
    assert_eq!(project.id(), "acme");
    assert_eq!(project.provenance, Provenance::Config);
    assert_eq!(project.created_at_ms, None);
    assert!(!project.is_archived());
    assert!(project.entry.policy.is_none());

    let key = &loaded.records.keys[0];
    assert_eq!(key.scope, KeyRecordScope::Admin);
    assert_eq!(key.display_tail, None);
    assert!(!key.is_revoked());

    // The collections nothing wrote are empty rather than missing, which is
    // what makes the whole document optional field by field.
    assert!(loaded.records.users.is_empty());
    assert!(loaded.records.memberships.is_empty());
}

/// An empty store is the empty directory at version 0 — and a store that says
/// it has been written and holds nothing is not.
///
/// The second half is the one with teeth. A `None` document at a nonzero
/// version means the bytes have been lost — a hand-edited key, a partial
/// restore — and reading it as "no tenancy" would un-configure every project
/// the API ever created *and* let the next admin write commit that emptiness
/// over the top. Refusing lets the boot say where to look and lets a running
/// node keep the plane it already serves.
#[tokio::test]
async fn an_empty_store_is_the_empty_directory_and_a_lost_document_is_not() {
    let store = memory();
    let loaded = store.load().await.unwrap();
    assert_eq!(loaded.version, 0);
    assert!(loaded.records.projects.is_empty());

    struct WrittenButEmpty;
    #[async_trait]
    impl DocumentStore for WrittenButEmpty {
        async fn load(&self) -> Result<VersionedDocument, DocumentStoreError> {
            Ok(VersionedDocument {
                document: None,
                version: 7,
                lineage: "lost-document".into(),
            })
        }
        async fn commit(&self, _: u64, _: Vec<u8>) -> Result<DocumentVersion, DocumentStoreError> {
            unreachable!("this double is only ever loaded from")
        }
        async fn version(&self) -> Result<DocumentVersion, DocumentStoreError> {
            Ok(DocumentVersion {
                lineage: "lost-document".into(),
                version: 7,
            })
        }
    }

    let store = DocumentDirectoryStore::over(Arc::new(WrittenButEmpty));
    match store.load().await {
        Err(StoreFailure::Unavailable(reason)) => {
            assert!(reason.contains("version 7"), "{reason}");
        }
        other => panic!(
            "a store at a nonzero version holding no document has lost it, and \
             reading that as an empty directory would commit the emptiness on \
             the next write: {other:?}"
        ),
    }
}

/// The mirror of the case above, and refused for the mirror reason (M16.1
/// review, F4): a document at version 0.
///
/// Version 0 *is* "no document" — the contract says so, and this adapter's own
/// empty-store arm is the reading of it — so a store answering `Some(bytes)`
/// at version 0 is answering two things that cannot both be true. Only a
/// durable backend can produce the shape (a hash whose `version` field was
/// deleted, a foreign writer, a restore that landed half a key);
/// `MemoryDocumentStore` moves its document and its counter together from
/// `(None, 0)` and cannot reach it at all, which is exactly why the refusal
/// has to be here rather than in one backend — and why this test needs a
/// double to reach it.
///
/// What it costs to get this wrong is the whole of F4: the plane would be
/// compiled from tenancy whose version this node never observed, and the very
/// next admin write would `commit(0, ..)` straight over the key it came from.
#[tokio::test]
async fn a_document_at_version_zero_is_refused_rather_than_compiled() {
    struct DocumentWithoutAVersion;
    #[async_trait]
    impl DocumentStore for DocumentWithoutAVersion {
        async fn load(&self) -> Result<VersionedDocument, DocumentStoreError> {
            Ok(VersionedDocument {
                // Valid bytes, deliberately: the refusal must be about the
                // version being zero beside a document, not about a document
                // this build cannot read -- which would pass this test for the
                // wrong reason.
                document: Some(
                    serde_json::to_vec(&serde_json::json!({
                        "schema": DIRECTORY_DOCUMENT_SCHEMA,
                        "records": DirectoryRecords::default(),
                        "compiled_under": CompiledUnder::default(),
                    }))
                    .unwrap(),
                ),
                version: 0,
                lineage: String::new(),
            })
        }
        async fn commit(&self, _: u64, _: Vec<u8>) -> Result<DocumentVersion, DocumentStoreError> {
            unreachable!("this double is only ever loaded from")
        }
        async fn version(&self) -> Result<DocumentVersion, DocumentStoreError> {
            Ok(DocumentVersion {
                lineage: String::new(),
                version: 0,
            })
        }
    }

    let store = DocumentDirectoryStore::over(Arc::new(DocumentWithoutAVersion));
    match store.load().await {
        Err(StoreFailure::Unavailable(reason)) => {
            assert!(
                reason.contains("version 0"),
                "the reason has to say which impossible pair it saw: {reason}"
            );
        }
        other => panic!(
            "version zero is the empty directory, so a store holding a document at version \
             zero has a key whose version this node never read -- compiling it would serve \
             tenancy the next write clobbers: {other:?}"
        ),
    }
}

/// The two failures a document store can give arrive as the two the directory
/// already distinguishes — one for one.
///
/// Not a formality: `http.rs` answers `Concurrent` with `409` and
/// `Unavailable` with `503`, so a mapping that flattened them would turn every
/// lost race into a reported outage, and an operator retrying a `409` would be
/// told to page somebody instead.
///
/// The `Concurrent` half also proves the adapter is *delegating* the
/// compare-and-set rather than performing one of its own: the numbers in the
/// refusal are the store's, and the document behind them is untouched.
#[tokio::test]
async fn a_stale_commit_arrives_as_concurrent_and_an_outage_as_unavailable() {
    let store = memory();
    let version = store.commit(0, DirectoryRecords::default()).await.unwrap();
    assert_eq!(version.version, 1);

    let stale = store.commit(0, every_field_populated()).await;
    assert!(
        matches!(
            stale,
            Err(StoreFailure::Concurrent {
                expected: 0,
                found: 1
            })
        ),
        "a commit against a version the store has moved past must reach the \
         HTTP surface as a retryable conflict: {stale:?}"
    );
    assert!(
        store.load().await.unwrap().records.projects.is_empty(),
        "and the refused write changed nothing"
    );

    struct Down;
    #[async_trait]
    impl DocumentStore for Down {
        async fn load(&self) -> Result<VersionedDocument, DocumentStoreError> {
            Err(DocumentStoreError::Unavailable("connection refused".into()))
        }
        async fn commit(&self, _: u64, _: Vec<u8>) -> Result<DocumentVersion, DocumentStoreError> {
            Err(DocumentStoreError::Unavailable("connection refused".into()))
        }
        async fn version(&self) -> Result<DocumentVersion, DocumentStoreError> {
            Err(DocumentStoreError::Unavailable("connection refused".into()))
        }
    }

    let down = DocumentDirectoryStore::over(Arc::new(Down));
    for outcome in [
        down.load().await.err().map(|e| e.to_string()),
        down.commit(0, DirectoryRecords::default())
            .await
            .err()
            .map(|e| e.to_string()),
        down.version().await.err().map(|e| e.to_string()),
    ] {
        let reason = outcome.expect("every call to a store that is down fails");
        assert!(
            reason.contains("connection refused"),
            "the backend's own reason must survive the mapping, or an \
             operator gets `unavailable` with nothing to act on: {reason}"
        );
    }
}

/// The fingerprint the writer stamps is the fingerprint a reader loads back.
///
/// R-D7 carries the slot; R-D9 is what compares two of them. What is pinned
/// here is only that the envelope really does round-trip it — a `compiled_under`
/// dropped on write or on read would make every future divergence check
/// silently agree with itself.
#[tokio::test]
async fn the_writers_fingerprint_is_what_a_reader_loads_back() {
    let stamp = CompiledUnder {
        file_sha256: Some("abc123".into()),
        catalog: vec!["anthropic/big".into(), "local/small".into()],
        fleet: vec!["local/small".into()],
        admission_cache_ttl_ms: Some(30_000),
        judge: Some("anthropic/judge".into()),
    };
    let backing = Arc::new(MemoryDocumentStore::new());
    let writer = DocumentDirectoryStore::stamped(
        Arc::clone(&backing) as Arc<dyn DocumentStore>,
        stamp.clone(),
    );
    assert_eq!(writer.compiled_under(), &stamp);
    writer.commit(0, DirectoryRecords::default()).await.unwrap();

    // A *differently* stamped reader over the same store loads the writer's
    // fingerprint, not its own -- which is the whole of what makes a
    // divergence check able to see a difference at all.
    let reader = DocumentDirectoryStore::stamped(
        Arc::clone(&backing) as Arc<dyn DocumentStore>,
        CompiledUnder {
            file_sha256: Some("something else".into()),
            ..CompiledUnder::default()
        },
    );
    let loaded = reader.load().await.unwrap();
    assert_eq!(loaded.compiled_under, stamp);
    assert_eq!(loaded.version, 1);

    // A document written before the fingerprint existed loads as one that
    // declares nothing, rather than failing.
    let older = holding(br#"{"schema":1,"records":{}}"#).await;
    assert_eq!(
        older.load().await.unwrap().compiled_under,
        CompiledUnder::default()
    );
}

/// One project record whose `name` is `pad_bytes` copies of `'a'` and nothing
/// else populated — a single ASCII field long enough to dial the envelope's
/// encoded length to an exact byte count, since a plain ASCII letter costs
/// exactly one byte in the JSON output with no escaping to throw the count off.
fn records_padded_to(pad_bytes: usize) -> DirectoryRecords {
    DirectoryRecords {
        projects: vec![ProjectRecord {
            entry: ProjectEntry {
                id: "pad".into(),
                name: Some("a".repeat(pad_bytes)),
                policy: None,
                budget: None,
                fair_use: None,
                validate: None,
                credentials: None,
                tiers: None,
            },
            provenance: Provenance::Admin,
            created_at_ms: None,
            archived_at_ms: None,
        }],
        ..DirectoryRecords::default()
    }
}

/// The exact byte length [`DocumentDirectoryStore::commit`] would encode
/// `records` to, computed the same way `commit` does (same envelope, same
/// default fingerprint a bare [`DocumentDirectoryStore::over`] stamps) but
/// without writing anywhere — what lets the two tests below dial a document to
/// precisely the ceiling rather than merely somewhere near it.
fn encoded_len(records: &DirectoryRecords) -> usize {
    serde_json::to_vec(&DirectoryDocument {
        schema: DIRECTORY_DOCUMENT_SCHEMA,
        records: records.clone(),
        compiled_under: CompiledUnder::default(),
    })
    .expect("a directory document of plain ASCII strings always encodes")
    .len()
}

/// **M16.1 review, F6: the ceiling is enforced by the adapter, before any
/// store call, and the boundary is exact.**
///
/// `connect_manager`'s shared `RESPONSE_TIMEOUT` (`roundhouse-store-redis`,
/// `lib.rs`) is 300ms, sized for a fair-use ceiling check with its own
/// two-second budget — not for this family, whose `commit` wraps the entire
/// document in one Lua argument. A document large enough to blow that budget
/// used to surface as a bare timeout, indistinguishable from Redis being
/// down. [`DIRECTORY_DOCUMENT_CEILING_BYTES`] is the fix's other half: a
/// document at the ceiling still commits, and one byte over is refused here,
/// by this adapter, before `self.store.commit` is ever called — which this
/// test proves by checking the underlying store never saw a write for the
/// refused document, not only that the call returned an error.
#[tokio::test]
async fn a_document_at_the_ceiling_commits_and_one_byte_over_is_refused_before_any_wire() {
    let base_len = encoded_len(&records_padded_to(0));
    let pad_to_ceiling = DIRECTORY_DOCUMENT_CEILING_BYTES - base_len;

    let at_ceiling = records_padded_to(pad_to_ceiling);
    assert_eq!(
        encoded_len(&at_ceiling),
        DIRECTORY_DOCUMENT_CEILING_BYTES,
        "fixture premise: the padding must land the encoded document on the exact ceiling"
    );
    let store = memory();
    store
        .commit(0, at_ceiling)
        .await
        .expect("a document at the ceiling, not over it, must still commit");

    let backing = Arc::new(MemoryDocumentStore::new());
    let over_ceiling = DocumentDirectoryStore::over(Arc::clone(&backing) as Arc<dyn DocumentStore>);
    let one_byte_over = records_padded_to(pad_to_ceiling + 1);
    assert_eq!(
        encoded_len(&one_byte_over),
        DIRECTORY_DOCUMENT_CEILING_BYTES + 1
    );
    let error = over_ceiling
        .commit(0, one_byte_over)
        .await
        .expect_err("one byte over the ceiling must be refused");
    let reason = error.to_string();
    // M18, H2: a named variant rather than `Unavailable`, so an operator (and
    // `http.rs`'s mapping) can tell a size breach -- caused by this write --
    // from a store that is genuinely down without parsing the message.
    assert!(
        matches!(
            error,
            StoreFailure::DocumentTooLarge {
                size,
                ceiling: DIRECTORY_DOCUMENT_CEILING_BYTES,
            } if size == DIRECTORY_DOCUMENT_CEILING_BYTES + 1
        ),
        "the refusal must be typed as a size breach naming the exact size and ceiling on the \
         type, not folded into the store's generic unavailable variant: {error:?}"
    );
    assert!(
        reason.contains(&(DIRECTORY_DOCUMENT_CEILING_BYTES + 1).to_string())
            && reason.contains(&DIRECTORY_DOCUMENT_CEILING_BYTES.to_string()),
        "the reason must name both the document's actual size and the ceiling it exceeded: \
         {reason}"
    );

    // Before any wire: the underlying store this adapter wraps never saw a
    // commit for the refused document. Version 0 with no document is exactly
    // what a store nothing has ever written to answers -- proof this refusal
    // happened in the adapter's own encode-then-check step, never reaching
    // `self.store.commit`.
    let never_written = backing.load().await.unwrap();
    assert_eq!(never_written.version, 0);
    assert_eq!(never_written.document, None);
}
