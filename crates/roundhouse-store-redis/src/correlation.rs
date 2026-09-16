// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Redis-backed [`CorrelationMaps`]: which session a client's own name for a
//! conversation belongs to, answerable from any node.
//!
//! This is what makes M12.1's F9 refusal mean what it says. Until this rung
//! the three maps lived in one process, so a client that reconnected to
//! another node kept its cache key and lost its generation, and an MCP call
//! that landed on a node which had served none of that principal's turns was
//! refused however exactly the client had named its conversation. The maps are
//! shared now; the refusal is still there, and it now means *never bound
//! anywhere* (R12, R-C2).
//!
//! | Key | Type | Holds |
//! |---|---|---|
//! | `rh:v1:corr:gen:{<namespaced cache key>}` | string | the generation a turn last committed to, in decimal |
//! | `rh:v1:corr:call:{<principal>}:<tool_use_id>` | string | `s:<session id>`, or the ambiguous marker |
//! | `rh:v1:corr:thread:{<principal>}:<thread_id>` | string | `s:<session id>` |
//!
//! # One key per binding, and the hash it beat (R-C3)
//!
//! The obvious alternative was one hash per principal with a field per
//! binding, which is the shape the fair-use ledger settled on for its buckets.
//! It loses here for one reason: **a hash field cannot expire.** The bound
//! these two families need is a staleness bound — a binding older than any
//! plausible turn is a stale guess whatever a table's size (D1, R14) — and
//! with a hash that bound needs a sweeper, which is exactly the pruning pass
//! M13 refused to own until running sums gave it a home. One key per binding
//! hands the whole of it to `PEXPIRE`: the bound is declared once
//! ([`CALL_BINDING_TTL_MS`], [`THREAD_BINDING_TTL_MS`]), re-armed on every
//! write, and enforced by the server whether or not any node is still running.
//! What it costs is that a principal's bindings cannot be read in one command
//! — and nothing reads them that way, because every question here is about
//! exactly one id.
//!
//! # The namespace and the schema version are in every key (R-C4, R-S3)
//!
//! `rh` is the default [`KeyNamespace`](crate::KeyNamespace) — a deployment
//! that names its own with `ROUNDHOUSE_REDIS_NAMESPACE` gets that one
//! instead, on every key any family in this crate writes; `v1` is this
//! family's schema version, and it is here from the first deployment rather
//! than retrofitted. The version is what makes the *value* encodings below
//! changeable: a v2 that stored a call binding as a hash would be a different
//! key space rather than a value some v1 node misreads as a session id. Every
//! key is built through [`crate::keys::build_key`] (M14.2, R-S3), which is
//! what makes "every key carries the namespace and the version" a property of
//! one function rather than a convention four families each had to remember.
//!
//! # Values are tagged, because a session id is an arbitrary string
//!
//! A call binding is either a session or the fact that two sessions claimed
//! it, and a session id is whatever the cache key made it — `acme/ada/main`,
//! `acme/ada/main#g3`, anything a client can spell. So a bare sentinel would
//! be a session id some client can mint. Bound values carry a `s:` tag and the
//! ambiguous marker carries none, which makes the two unmistakable in either
//! direction; a value with neither shape is a *foreign writer*, and it fails
//! the read loudly rather than being guessed at, exactly as a stream entry
//! this store did not write does.
//!
//! # What is not atomic here, and why that is a ruling rather than an omission
//!
//! Only the call binding is a script. The generation map is a plain `SET`
//! because a generation is a *hint* since M14.0 — where a probe starts, not
//! the answer it returns — so two nodes committing different generations for
//! one key both leave a value the other's next search merely begins from
//! (R-C2). A thread binding is a plain `SET` because the latest write is the
//! answer by contract: a thread moves on every fork, and the turn that moved
//! it is the one the client is in.
//!
//! The call binding is the one place a read-then-write would be wrong. "This
//! id is already held by a *different* session" is precisely the condition
//! that must not be evaluated against a value another node is replacing, or
//! two sessions each observe an absent key and each write itself in — leaving
//! one binding, confidently wrong, which is the M12 F14 defect with a network
//! in the middle. So the check and the write are one script.
//!
//! Passes the same
//! `correlation_maps_contract_suite!`
//! that judges `MemoryCorrelationMaps`, instantiated ignore-gated in
//! `tests/correlation_contract.rs` exactly as `tests/spend_contract.rs` does
//! for spend.

mod scripts;

use std::sync::Arc;

use async_trait::async_trait;
use redis::aio::ConnectionManager;

use roundhouse_core::control::correlation::{
    CALL_BINDING_STALENESS_MS, THREAD_BINDING_STALENESS_MS,
};
use roundhouse_core::control::{CorrelationError, CorrelationMaps, Principal};
use roundhouse_core::ids::SessionId;

use crate::keys::{self, KeyNamespace};

/// How long a tool-call binding lives here.
///
/// Re-exported from `roundhouse-core` (M14.2, R-S1) rather than defined here:
/// the memory tables now enforce the same bound themselves, so the number and
/// its reasoning have exactly one home, beside the trait both implementations
/// answer to. What stays local to this backend is the *mechanism* — `PEXPIRE`
/// — not the bound.
pub const CALL_BINDING_TTL_MS: u64 = CALL_BINDING_STALENESS_MS;

/// How long a thread binding lives here. See [`CALL_BINDING_TTL_MS`] for why
/// this moved.
pub const THREAD_BINDING_TTL_MS: u64 = THREAD_BINDING_STALENESS_MS;

/// The tag a bound value carries.
///
/// A session id is an arbitrary string — it is the client's cache key with an
/// optional `#g{n}` — so an untagged sentinel for "ambiguous" would be a
/// session id some client can mint, and a conversation named that would read
/// as a collision it never had. Tagging the *bound* case instead makes the
/// marker unspellable: no bound value can equal it, whatever the client names
/// its conversation.
const BOUND_TAG: &str = "s:";

/// What a call id two sessions of one principal have claimed holds instead of
/// a session.
///
/// Remembered rather than deleted: an id dropped from the store would read as
/// never-seen, so the next binding of the same colliding id would look like a
/// first one and start answering confidently again — the defect, one turn
/// later.
///
/// `pub` rather than `pub(crate)`: the gated integration test that reads the
/// raw stored value (`correlation_contract.rs`) needs the real constant, not
/// a copy that can drift from what ships — the same reasoning
/// [`test_support::fair_use_would_exceed_source`](crate::test_support::fair_use_would_exceed_source)
/// gives for handing out the real script rather than letting a test carry its
/// own copy. Reading it as `roundhouse_store_redis::correlation::AMBIGUOUS_MARKER`
/// needs no `test_support` wrapper the way `scripts.rs` needs none either
/// (M14.1 review, F9).
pub const AMBIGUOUS_MARKER: &str = "!ambiguous";

/// The generation key for one namespaced cache key.
///
/// The braces are a Redis Cluster hash tag. Nothing here is a multi-key
/// operation, so the tag buys no atomicity; it is here so that a future
/// operation over one conversation's correlation state — the natural shape of
/// a "forget this conversation" sweep — is expressible at all rather than
/// blocked by a layout decision nobody revisited.
///
/// Keyed by the whole namespaced string rather than by a `(project, user,
/// key)` triple, because that same string is the session id's stem: the
/// counter and the id it names must not be able to key on different things.
pub(crate) fn generation_key(namespace: &KeyNamespace, key: &str) -> String {
    keys::build_key(
        namespace,
        keys::KeyFamily::Correlation,
        &["gen", &format!("{{{key}}}")],
    )
}

/// The key one principal's binding of one tool-use id occupies.
///
/// The hash tag is on the *principal* rather than the id, so one tenant's
/// bindings share a slot: the same reason the spend ledger tags on the project.
/// Nothing needs that today — every operation here is single-key — and it
/// costs nothing to leave the door open.
pub(crate) fn call_key(namespace: &KeyNamespace, principal: &Principal, call_id: &str) -> String {
    keys::build_key(
        namespace,
        keys::KeyFamily::Correlation,
        &[
            "call",
            &format!("{{{}}}:{call_id}", principal_tag(principal)),
        ],
    )
}

/// The key one principal's binding of one client-declared thread occupies.
/// Tagged like [`call_key`], and a separate family from it — see
/// `a_call_a_thread_and_a_generation_do_not_share_a_name` in the contract.
pub(crate) fn thread_key(
    namespace: &KeyNamespace,
    principal: &Principal,
    thread_id: &str,
) -> String {
    keys::build_key(
        namespace,
        keys::KeyFamily::Correlation,
        &[
            "thread",
            &format!("{{{}}}:{thread_id}", principal_tag(principal)),
        ],
    )
}

/// One principal as a key segment that no pair of ids can spell two ways.
///
/// **Length-prefixed, and this is the one place this crate does that.** The
/// memory maps key on a `(Principal, id)` *tuple*, where a project, a user and
/// a call id cannot be confused for one another whatever they contain. Any
/// flattening into a key string can be: with a plain `proj:user:id`, the pair
/// (`ada`, `x:call_0`) and the pair (`ada:x`, `call_0`) produce one key, so one
/// member of a project could read — or make ambiguous — another member's
/// binding. That is the same class of defect M12's F14 and F15 closed inside
/// one process, re-opened by a delimiter. Prefixing each identity segment
/// with its byte length makes the encoding injective, so the partition the
/// contract asserts holds for every id a tenant can spell rather than for the
/// ids the tests happened to use.
///
/// The generation key needs none of this: its single segment is the whole
/// namespaced cache key, which is already the one string the session id is
/// built from.
fn principal_tag(principal: &Principal) -> String {
    let project = principal.project.as_str();
    let user = principal.user.as_str();
    format!("{}:{project}:{}:{user}", project.len(), user.len())
}

fn backend(error: redis::RedisError) -> CorrelationError {
    CorrelationError::Backend(anyhow::Error::new(error))
}

/// What one stored binding decodes to.
///
/// A value that is neither shape fails the read rather than being read as an
/// absence, for the reason a foreign stream entry fails a replay: a store that
/// quietly answers `None` for something it cannot parse is a store whose
/// refusals no longer distinguish "never bound" from "bound, by something
/// else, in a format this build does not know".
fn decode_binding(raw: Option<String>, key: &str) -> Result<Option<SessionId>, CorrelationError> {
    match raw {
        None => Ok(None),
        Some(value) if value == AMBIGUOUS_MARKER => Ok(None),
        Some(value) => match value.strip_prefix(BOUND_TAG) {
            Some(session) => Ok(Some(SessionId::new(session))),
            None => Err(CorrelationError::Backend(anyhow::anyhow!(
                "correlation key `{key}` holds `{value}`, which this store did \
                 not write; refusing to read it as a session or as an absence"
            ))),
        },
    }
}

fn bound_value(session: &SessionId) -> String {
    format!("{BOUND_TAG}{session}")
}

/// The two staleness bounds, as one value a test can replace.
///
/// **The seam R-C6 asks for, and it is a field rather than a `const` a test
/// edits.** Proving that a binding expires means watching one expire, and a
/// test that waited out the production bound would take six hours; a test that
/// *changed* the production bound would be asserting on a deployment nobody
/// runs. A per-handle TTL keeps the shipped numbers shipped and lets one
/// handle in one test run on milliseconds.
#[derive(Debug, Clone, Copy)]
struct BindingTtls {
    call_ms: u64,
    thread_ms: u64,
}

impl Default for BindingTtls {
    fn default() -> Self {
        Self {
            call_ms: CALL_BINDING_TTL_MS,
            thread_ms: THREAD_BINDING_TTL_MS,
        }
    }
}

/// Redis implementation of [`CorrelationMaps`].
///
/// Cheap to clone: clones share one auto-reconnecting multiplexed connection,
/// exactly like [`RedisSessionStore`](crate::RedisSessionStore).
#[derive(Clone)]
pub struct RedisCorrelationMaps {
    conn: ConnectionManager,
    scripts: Arc<scripts::Scripts>,
    ttls: BindingTtls,
    namespace: KeyNamespace,
}

impl RedisCorrelationMaps {
    /// Connect under the default namespace (`rh`) and fail fast: maps that
    /// cannot reach their Redis at startup should stop the process there,
    /// not on the first turn.
    ///
    /// Through `crate::connect_manager` (private, so not a doc-link) for the
    /// reason every other family in this crate goes through it — the outage
    /// latency this crate bounds once rather than per call site.
    pub async fn connect(url: impl AsRef<str>) -> Result<Self, CorrelationError> {
        Self::connect_namespaced(url, KeyNamespace::default()).await
    }

    /// Connect under an explicit [`KeyNamespace`] — what the composition
    /// root calls once it has read `ROUNDHOUSE_REDIS_NAMESPACE` (R-S3).
    pub async fn connect_namespaced(
        url: impl AsRef<str>,
        namespace: KeyNamespace,
    ) -> Result<Self, CorrelationError> {
        let conn = crate::connect_manager(url.as_ref())
            .await
            .map_err(backend)?;
        Ok(Self {
            conn,
            scripts: Arc::new(scripts::Scripts::new()),
            ttls: BindingTtls::default(),
            namespace,
        })
    }

    /// Shorten this handle's staleness bounds. See [`BindingTtls`].
    ///
    /// A direct `pub fn` behind the `test-support` feature, the same shape
    /// [`RedisFairUseLedger::with_bucket_ttl_ms`](crate::fair_use::RedisFairUseLedger::with_bucket_ttl_ms)
    /// already has: the gated integration test calls this lever itself rather
    /// than through a `test_support.rs` pass-through that existed only to
    /// re-export a `pub(crate)` fn an outside crate could not otherwise reach
    /// (M14.1 review, F9).
    #[cfg(feature = "test-support")]
    pub fn with_binding_ttls(mut self, call_ms: u64, thread_ms: u64) -> Self {
        self.ttls = BindingTtls { call_ms, thread_ms };
        self
    }

    /// The one GET round trip every read path here needs.
    ///
    /// `generation`, `session_of_call` and `session_of_thread` each built this
    /// same five-line block themselves before this rung, issuing the `GET`
    /// command directly — three spellings of one round trip (M14.1 review,
    /// F8), where `decode_binding` already collapsed the two binding reads'
    /// *parsing* into one function without touching the fetch above it.
    async fn get(&self, key: &str) -> Result<Option<String>, CorrelationError> {
        redis::cmd("GET")
            .arg(key)
            .query_async(&mut self.conn.clone())
            .await
            .map_err(backend)
    }
}

#[async_trait]
impl CorrelationMaps for RedisCorrelationMaps {
    async fn generation(&self, key: &str) -> Result<Option<u32>, CorrelationError> {
        let raw = self.get(&generation_key(&self.namespace, key)).await?;
        raw.map(|value| {
            value.parse::<u32>().map_err(|error| {
                CorrelationError::Backend(anyhow::anyhow!(
                    "generation key for `{key}` holds `{value}`, which is not a \
                     generation ({error}); refusing to read it as zero"
                ))
            })
        })
        .transpose()
    }

    async fn set_generation(&self, key: &str, generation: u32) -> Result<(), CorrelationError> {
        // A plain SET, and no expiry. The two binding families age out because
        // a stale binding is a stale *guess*; a generation that aged out would
        // not be a lost guess but a reset fork counter, which re-points a live
        // conversation at a log it forked away from.
        let _: () = redis::cmd("SET")
            .arg(generation_key(&self.namespace, key))
            .arg(generation)
            .query_async(&mut self.conn.clone())
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn bind_call(
        &self,
        principal: &Principal,
        call_id: &str,
        session: &SessionId,
    ) -> Result<(), CorrelationError> {
        self.scripts
            .bind_call(
                &mut self.conn.clone(),
                &call_key(&self.namespace, principal, call_id),
                &bound_value(session),
                self.ttls.call_ms,
            )
            .await
    }

    async fn session_of_call(
        &self,
        principal: &Principal,
        call_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        let key = call_key(&self.namespace, principal, call_id);
        let raw = self.get(&key).await?;
        decode_binding(raw, &key)
    }

    async fn bind_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
        session: &SessionId,
    ) -> Result<(), CorrelationError> {
        // No script: the latest write is the answer by contract, so there is
        // no condition to evaluate atomically against it.
        let _: () = redis::cmd("SET")
            .arg(thread_key(&self.namespace, principal, thread_id))
            .arg(bound_value(session))
            .arg("PX")
            .arg(self.ttls.thread_ms)
            .query_async(&mut self.conn.clone())
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn session_of_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        let key = thread_key(&self.namespace, principal, thread_id);
        let raw = self.get(&key).await?;
        decode_binding(raw, &key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ada() -> Principal {
        Principal::new("acme", "ada")
    }

    /// **R-S1's claim ("the memory tables and the Redis keys expire by the
    /// same constant") checked, not only true by construction.**
    /// `CALL_BINDING_TTL_MS`/`THREAD_BINDING_TTL_MS` are aliases of the core
    /// crate's staleness bounds today, but an alias is not a check: nothing
    /// failed before this test if a future edit replaced either alias with a
    /// hand-typed literal that happened to look right (M14.2 review, F1).
    /// `BindingTtls::default()` — what every production handle actually
    /// carries — is what is compared here, because every gated test in
    /// `correlation_contract.rs` shortens it with `with_binding_ttls` before
    /// touching Redis, so this unit test is the only place the shipped
    /// default is checked against anything at all. The live-Redis half of
    /// this proof — that the default handle's write really arms this many
    /// milliseconds on the server — is
    /// `the_default_call_and_thread_ttls_reach_redis_as_the_core_staleness_bounds`
    /// in `correlation_contract.rs`.
    #[test]
    fn the_default_ttls_equal_the_core_staleness_bounds() {
        let ttls = BindingTtls::default();
        assert_eq!(
            ttls.call_ms, CALL_BINDING_STALENESS_MS,
            "a Redis call binding must expire on exactly the bound \
             roundhouse-core enforces in memory, or the two implementations \
             disagree about what \"stale\" means"
        );
        assert_eq!(
            ttls.thread_ms, THREAD_BINDING_STALENESS_MS,
            "same for the thread binding"
        );
    }

    /// The three families are three key spaces, and every key carries the
    /// namespace and the schema version (R-C4).
    ///
    /// A unit test rather than a gated one: key strings are pure formatting,
    /// and an ignore-gated duplicate would add a dependency on infrastructure
    /// it does not use — the same reason the spend ledger's hash-tag test lives
    /// beside its key functions.
    #[test]
    fn every_key_carries_the_namespace_the_version_and_its_family() {
        let namespace = KeyNamespace::default();
        let generation = generation_key(&namespace, "acme/ada/main");
        let call = call_key(&namespace, &ada(), "toolu_1");
        let thread = thread_key(&namespace, &ada(), "thread-1");

        for key in [&generation, &call, &thread] {
            assert!(key.starts_with("rh:v1:corr:"), "{key}");
        }
        assert_eq!(generation, "rh:v1:corr:gen:{acme/ada/main}");
        assert_eq!(call, "rh:v1:corr:call:{4:acme:3:ada}:toolu_1");
        assert_eq!(thread, "rh:v1:corr:thread:{4:acme:3:ada}:thread-1");

        // A different namespace must never build the same key.
        let other = KeyNamespace::new("acme-prod").unwrap();
        assert_ne!(generation_key(&other, "acme/ada/main"), generation);
    }

    /// No pair of ids can spell one principal's key segment two ways.
    ///
    /// The defect this closes is a cross-member read inside one project: with
    /// an unprefixed `project:user:id`, the member `x` answering call
    /// `y:call_0` and the member `x:y` answering `call_0` land on one key. The
    /// contract's partition assertion uses ordinary ids and would never see
    /// it, which is exactly why the encoding is pinned here.
    #[test]
    fn a_delimiter_in_an_id_cannot_make_two_members_share_a_key() {
        let namespace = KeyNamespace::default();
        let straddling = Principal::new("acme", "x");
        let shifted = Principal::new("acme", "x:y");
        assert_ne!(
            call_key(&namespace, &straddling, "y:call_0"),
            call_key(&namespace, &shifted, "call_0")
        );
        assert_ne!(
            thread_key(&namespace, &straddling, "y:thread"),
            thread_key(&namespace, &shifted, "thread")
        );
    }

    /// A bound value and the ambiguous marker cannot be confused, whatever a
    /// client names its conversation.
    #[test]
    fn a_session_named_like_the_marker_still_decodes_as_a_session() {
        let impostor = SessionId::new(AMBIGUOUS_MARKER);
        let stored = bound_value(&impostor);
        assert_eq!(
            decode_binding(Some(stored), "k").unwrap(),
            Some(impostor),
            "the tag is on the bound case precisely so the marker is \
             unspellable by a client"
        );
        assert_eq!(
            decode_binding(Some(AMBIGUOUS_MARKER.to_string()), "k").unwrap(),
            None
        );
        assert_eq!(decode_binding(None, "k").unwrap(), None);
    }

    /// A value this store did not write fails the read rather than reading as
    /// an absence.
    #[test]
    fn a_foreign_value_is_refused_rather_than_read_as_never_bound() {
        let error = decode_binding(Some("sess_whatever".to_string()), "rh:v1:corr:call:x")
            .expect_err("an untagged value is not something this store wrote");
        let text = error.to_string();
        assert!(text.contains("rh:v1:corr:call:x"), "{text}");
    }
}
