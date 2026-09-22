// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Redis-backed [`SpendLedger`].
//!
//! One project maps to four keys, all sharing a Redis Cluster hash tag on
//! the *project id* (not the session id [`crate`]'s own three keys share) —
//! the colocation is what makes "both ceilings bind" an atomic property
//! rather than an optimization: a project ceiling and a member ceiling that
//! lived in different slots could not be read-and-debited by one script, and
//! two grants racing across two separate round trips is precisely the
//! overspend `concurrent_grants_cannot_jointly_exceed_the_limit` exists to
//! close.
//!
//! | Key | Type | Holds |
//! |---|---|---|
//! | `rh:v1:spend:{<project_id>}:account` | hash | `committed` (project, current window), `member:<user_id>` per member, `window_start_ms` |
//! | `rh:v1:spend:{<project_id>}:holds` | hash | `response_id` → packed `user`/`amount`/`expires_at_ms`, one field per live grant |
//! | `rh:v1:spend:{<project_id>}:watermarks` | hash | `session_id` → highest settled `seq` |
//! | `rh:v1:spend:{<project_id>}:settled_calls` | set | `response_id` of every evaluation call that has settled |
//!
//! **`settled_calls` is the durable copy of once-only settlement's storage
//! cost** — the memory backend's equivalent set dies with its process, this one
//! does not. One member per settled evaluation call, never expired and never
//! swept, because any expiry is a window in which a duplicate charges a project
//! twice; `SettlementKey::OncePerCall` states the tradeoff and this is where
//! the bytes live. The script asks the set for membership and never reads it
//! back, so the cost is memory on the Redis until there is a compaction
//! protocol that can prove an identity will never be re-settled.
//!
//! `rh:v1:spend` is this family's [`crate::keys::build_key`] prefix under the
//! default namespace — see [`crate`]'s module doc for the table of every
//! family and [`crate::keys`] for the builder (R-S3).
//!
//! The write and read paths both live in `spend::scripts`: every
//! trait method is exactly one Lua script, so a grant, a settle, and a
//! balance read are each one round trip regardless of how many ceilings or
//! holds they touch. See that module's doc for the two conventions worth
//! reading before editing the Lua: dollar amounts cross as strings, never Lua
//! numbers, and `now_ms` is client-supplied rather than read from
//! `redis.call('TIME')` — a deliberate departure from `crate::scripts`'s
//! convention, because the ledger's own contract (both here and in
//! `roundhouse_core::control::spend`) requires a TTL lapse and a monthly
//! reset to be reachable in a test without sleeping.
//!
//! Passes the same `spend_ledger_contract_suite!`
//! that judges `MemorySpendLedger`, instantiated ignore-gated in
//! `tests/spend_contract.rs` exactly as `tests/contract.rs` does for the
//! session store.

mod scripts;

use std::sync::Arc;

use async_trait::async_trait;
use redis::aio::ConnectionManager;

use roundhouse_core::control::{
    Balance, BalanceQuery, BudgetTerms, BudgetWindow, Grant, GrantRequest, ProjectId, Settled,
    Settlement, SettlementKey, SpendError, SpendLedger,
};

use crate::keys::{self, KeyNamespace};

/// Which pool of money a ledger handle counts.
///
/// **A segment inside the spend family, not a second namespace.** Deriving an
/// evaluation namespace by decorating the deployment's own — `tenant` giving
/// `tenant-eval` — made one deployment's evaluation ledger the *same keys* as a
/// second deployment legitimately named `tenant-eval`: two tenants sharing one
/// counter, discovered only by the one whose turns started being refused.
/// Reserving the decorated name would have been a rule no operator could find
/// out about. The purpose belongs inside the namespace it is a purpose of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpendPurpose {
    /// What turns spend. Writes the keys this crate has always written.
    #[default]
    Serving,
    /// What this deployment's own evaluation calls spend.
    Evaluation,
}

impl SpendPurpose {
    /// The segment this purpose adds, after the hash tag and before the leaf.
    ///
    /// **Empty for [`Self::Serving`], and that is the compatibility promise**:
    /// every key a serving ledger builds is byte-identical to the key it built
    /// before this type existed, so no deployment's committed spend moves.
    fn segments(self) -> &'static [&'static str] {
        match self {
            Self::Serving => &[],
            Self::Evaluation => &["eval"],
        }
    }
}

/// Shared key parts. Each key function calls [`keys::build_key`] directly,
/// as required by the key-builder convention.
///
/// The hash tag stays **first**, ahead of the purpose segment: Redis Cluster
/// hashes a key on its first `{...}` pair, and a purpose written ahead of the
/// tag would put one project's four keys on different slots and break the
/// check-and-debit script's atomicity.
fn spend_key_parts<'a>(tag: &'a str, purpose: SpendPurpose, leaf: &'a str) -> Vec<&'a str> {
    let mut parts: Vec<&str> = vec![tag];
    parts.extend_from_slice(purpose.segments());
    parts.push(leaf);
    parts
}

/// [`account_key`] for a caller outside this crate that needs to assert the key
/// layout — the composition root's own test that two deployments do not collide.
pub fn account_key_for_test(
    namespace: &KeyNamespace,
    purpose: SpendPurpose,
    project: &ProjectId,
) -> String {
    account_key(namespace, purpose, project)
}

/// [`holds_key`], for the same reason.
pub fn holds_key_for_test(
    namespace: &KeyNamespace,
    purpose: SpendPurpose,
    project: &ProjectId,
) -> String {
    holds_key(namespace, purpose, project)
}

pub(crate) fn account_key(
    namespace: &KeyNamespace,
    purpose: SpendPurpose,
    project: &ProjectId,
) -> String {
    let tag = format!("{{{project}}}");
    keys::build_key(
        namespace,
        keys::KeyFamily::Spend,
        &spend_key_parts(&tag, purpose, "account"),
    )
}

pub(crate) fn holds_key(
    namespace: &KeyNamespace,
    purpose: SpendPurpose,
    project: &ProjectId,
) -> String {
    let tag = format!("{{{project}}}");
    keys::build_key(
        namespace,
        keys::KeyFamily::Spend,
        &spend_key_parts(&tag, purpose, "holds"),
    )
}

pub(crate) fn watermarks_key(
    namespace: &KeyNamespace,
    purpose: SpendPurpose,
    project: &ProjectId,
) -> String {
    let tag = format!("{{{project}}}");
    keys::build_key(
        namespace,
        keys::KeyFamily::Spend,
        &spend_key_parts(&tag, purpose, "watermarks"),
    )
}

pub(crate) fn settled_calls_key(
    namespace: &KeyNamespace,
    purpose: SpendPurpose,
    project: &ProjectId,
) -> String {
    let tag = format!("{{{project}}}");
    keys::build_key(
        namespace,
        keys::KeyFamily::Spend,
        &spend_key_parts(&tag, purpose, "settled_calls"),
    )
}

fn window_mode(window: BudgetWindow) -> &'static str {
    match window {
        BudgetWindow::Total => "total",
        BudgetWindow::Monthly => "monthly",
    }
}

/// `''` means [`Allocation::Pooled`](roundhouse_core::control::Allocation::Pooled)
/// — no second ceiling — which the script tests for by string equality
/// before ever calling `tonumber` on it.
fn member_ceiling_arg(terms: &BudgetTerms) -> String {
    terms
        .member_ceiling_usd()
        .map(|ceiling| ceiling.to_string())
        .unwrap_or_default()
}

fn backend(error: redis::RedisError) -> SpendError {
    SpendError::Backend(anyhow::Error::new(error))
}

/// Redis implementation of [`SpendLedger`].
///
/// Cheap to clone: clones share one auto-reconnecting multiplexed
/// connection, exactly like [`RedisSessionStore`](crate::RedisSessionStore).
#[derive(Clone)]
pub struct RedisSpendLedger {
    conn: ConnectionManager,
    scripts: Arc<scripts::Scripts>,
    namespace: KeyNamespace,
    purpose: SpendPurpose,
}

impl RedisSpendLedger {
    /// Connect under the default namespace (`rh`) and fail fast: a ledger
    /// that cannot reach its Redis at startup should stop the process
    /// there, not on the first grant.
    ///
    /// Through [`crate::connect_manager`] rather than
    /// `ConnectionManagerConfig::default()` — see its doc for the outage
    /// latency this crate's one `connect` bounds (M13.1 review F2).
    pub async fn connect(url: impl AsRef<str>) -> Result<Self, SpendError> {
        Self::connect_namespaced(url, KeyNamespace::default()).await
    }

    /// Connect under an explicit [`KeyNamespace`] — what the composition
    /// root calls once it has read `ROUNDHOUSE_REDIS_NAMESPACE` (R-S3).
    pub async fn connect_namespaced(
        url: impl AsRef<str>,
        namespace: KeyNamespace,
    ) -> Result<Self, SpendError> {
        Self::connect_for(url, namespace, SpendPurpose::Serving).await
    }

    /// Connect under an explicit namespace *and* purpose.
    ///
    /// The evaluation ledger is this, with [`SpendPurpose::Evaluation`]: the
    /// same deployment namespace as everything else, and its own keys inside it.
    /// See [`SpendPurpose`] for what deriving a namespace instead collided with.
    pub async fn connect_for(
        url: impl AsRef<str>,
        namespace: KeyNamespace,
        purpose: SpendPurpose,
    ) -> Result<Self, SpendError> {
        let conn = crate::connect_manager(url.as_ref())
            .await
            .map_err(backend)?;
        Ok(Self {
            conn,
            scripts: Arc::new(scripts::Scripts::new()),
            namespace,
            purpose,
        })
    }
}

#[async_trait]
impl SpendLedger for RedisSpendLedger {
    async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError> {
        SpendError::check_amount("requested_usd", request.requested_usd)?;
        SpendError::check_amount("limit_usd", request.terms.budget.limit_usd)?;

        let member_ceiling = member_ceiling_arg(&request.terms);
        let account = account_key(&self.namespace, self.purpose, &request.principal.project);
        let holds = holds_key(&self.namespace, self.purpose, &request.principal.project);
        let outcome = self
            .scripts
            .open_grant(
                &mut self.conn.clone(),
                scripts::OpenGrantArgs {
                    account_key: &account,
                    holds_key: &holds,
                    user: request.principal.user.as_str(),
                    response_id: request.response_id.as_str(),
                    requested_usd: request.requested_usd,
                    ttl_ms: request.ttl_ms,
                    now_ms: request.now_ms,
                    limit_usd: request.terms.budget.limit_usd,
                    member_ceiling_arg: &member_ceiling,
                    warn_at: request.terms.budget.warn_at,
                    window_mode: window_mode(request.terms.budget.window),
                },
            )
            .await?;
        Ok(Grant {
            granted_usd: outcome.granted_usd,
            state: outcome.state,
        })
    }

    async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError> {
        SpendError::check_amount("actual_usd", settlement.actual_usd)?;

        let account = account_key(&self.namespace, self.purpose, &settlement.principal.project);
        let holds = holds_key(&self.namespace, self.purpose, &settlement.principal.project);
        let watermarks =
            watermarks_key(&self.namespace, self.purpose, &settlement.principal.project);
        let settled_calls =
            settled_calls_key(&self.namespace, self.purpose, &settlement.principal.project);
        // One script for both modes, the unused half of the key travelling as a
        // sentinel: a second script would be a second copy of the window roll,
        // the hold release and the commit, which are identical either way.
        let (key_mode, session_id, seq) = match &settlement.key {
            SettlementKey::SessionWatermark { session_id, seq } => {
                ("watermark", session_id.as_str(), *seq)
            }
            SettlementKey::OncePerCall => ("call", "", 0),
        };
        let outcome = self
            .scripts
            .settle_grant(
                &mut self.conn.clone(),
                scripts::SettleGrantArgs {
                    account_key: &account,
                    holds_key: &holds,
                    watermarks_key: &watermarks,
                    settled_calls_key: &settled_calls,
                    user: settlement.principal.user.as_str(),
                    key_mode,
                    session_id,
                    seq,
                    response_id: settlement.response_id.as_str(),
                    actual_usd: settlement.actual_usd,
                    now_ms: settlement.now_ms,
                    window_mode: window_mode(settlement.window),
                },
            )
            .await?;
        Ok(match outcome {
            scripts::SettleOutcome::Applied {
                committed_usd,
                released_usd,
            } => Settled {
                applied: true,
                released_usd,
                committed_usd,
            },
            scripts::SettleOutcome::NoOp { committed_usd } => Settled {
                applied: false,
                released_usd: 0.0,
                committed_usd,
            },
        })
    }

    async fn balance(&self, query: BalanceQuery) -> Result<Balance, SpendError> {
        SpendError::check_amount("limit_usd", query.terms.budget.limit_usd)?;

        let member_ceiling = member_ceiling_arg(&query.terms);
        let account = account_key(&self.namespace, self.purpose, &query.principal.project);
        let holds = holds_key(&self.namespace, self.purpose, &query.principal.project);
        let outcome = self
            .scripts
            .balance(
                &mut self.conn.clone(),
                scripts::BalanceArgs {
                    account_key: &account,
                    holds_key: &holds,
                    user: query.principal.user.as_str(),
                    now_ms: query.now_ms,
                    limit_usd: query.terms.budget.limit_usd,
                    member_ceiling_arg: &member_ceiling,
                    warn_at: query.terms.budget.warn_at,
                    window_mode: window_mode(query.terms.budget.window),
                },
            )
            .await?;
        Ok(Balance {
            committed_usd: outcome.committed_usd,
            held_usd: outcome.held_usd,
            project_remaining_usd: outcome.project_remaining_usd,
            member_committed_usd: outcome.member_committed_usd,
            member_remaining_usd: outcome.member_remaining_usd,
            state: outcome.state,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_project_and_member_keys_share_one_hash_tag() {
        // The property the module doc claims is load-bearing, not
        // decorative: extract the `{...}` hash tag from each of the four
        // keys and check they are the same slot-selecting substring, which
        // is what a real Redis Cluster deployment hashes on. If any of the
        // four ever drifted to a different tag, `OPEN_GRANT`/`SETTLE_GRANT`
        // would refuse to run at all on a clustered deployment (Lua scripts
        // reject multi-slot key sets) — this test catches that at build
        // time instead of at first boot against a cluster. `SETTLE_GRANT`
        // names all four at once, so it is the script this binds hardest.
        fn hash_tag(key: &str) -> &str {
            let start = key.find('{').expect("every budget key carries a hash tag");
            let end = key.find('}').expect("the hash tag is closed");
            &key[start + 1..end]
        }

        let namespace = KeyNamespace::default();
        let project = ProjectId::new("acme");
        let account = account_key(&namespace, SpendPurpose::Serving, &project);
        let holds = holds_key(&namespace, SpendPurpose::Serving, &project);
        let watermarks = watermarks_key(&namespace, SpendPurpose::Serving, &project);
        let settled_calls = settled_calls_key(&namespace, SpendPurpose::Serving, &project);

        let tag = hash_tag(&account);
        assert_eq!(tag, "acme", "the tag is the project id, unadorned");
        assert_eq!(hash_tag(&holds), tag);
        assert_eq!(hash_tag(&watermarks), tag);
        assert_eq!(hash_tag(&settled_calls), tag);

        // The control: two different projects must land on two different
        // tags, or every project would collide onto one Redis Cluster slot.
        let other = ProjectId::new("other-project");
        assert_ne!(
            hash_tag(&account_key(&namespace, SpendPurpose::Serving, &other)),
            tag
        );
    }

    /// Every family's keys carry the namespace, the schema version and the
    /// family name (M14.2, R-S3) — the shape [`correlation`](crate::correlation)'s
    /// own key test already pins, asserted here for this family too.
    #[test]
    fn every_key_carries_the_namespace_the_version_and_the_family() {
        let namespace = KeyNamespace::default();
        let project = ProjectId::new("acme");
        assert_eq!(
            account_key(&namespace, SpendPurpose::Serving, &project),
            "rh:v1:spend:{acme}:account"
        );
        assert_eq!(
            holds_key(&namespace, SpendPurpose::Serving, &project),
            "rh:v1:spend:{acme}:holds"
        );
        assert_eq!(
            watermarks_key(&namespace, SpendPurpose::Serving, &project),
            "rh:v1:spend:{acme}:watermarks"
        );
        assert_eq!(
            settled_calls_key(&namespace, SpendPurpose::Serving, &project),
            "rh:v1:spend:{acme}:settled_calls"
        );

        let other = KeyNamespace::new("acme-prod").unwrap();
        assert_ne!(
            account_key(&namespace, SpendPurpose::Serving, &project),
            account_key(&other, SpendPurpose::Serving, &project),
            "two namespaces must never build the same key"
        );
    }

    #[test]
    fn a_pooled_allocation_sends_the_empty_ceiling_sentinel() {
        use roundhouse_core::control::{Allocation, Budget, Exhaustion};

        let pooled = BudgetTerms {
            budget: Budget {
                limit_usd: 10.0,
                window: BudgetWindow::Total,
                on_exhaustion: Exhaustion::degrade_with_overflow(),
                warn_at: 0.8,
            },
            allocation: Allocation::Pooled,
        };
        assert_eq!(member_ceiling_arg(&pooled), "");

        let capped = BudgetTerms {
            allocation: Allocation::Capped { limit_usd: 5.0 },
            ..pooled
        };
        assert_eq!(member_ceiling_arg(&capped), "5");
    }

    // `an_amount_that_is_not_a_number_of_dollars_is_refused_before_it_reaches_lua`
    // used to sit here, and its twin in `roundhouse_core::control::spend`'s own
    // test module: two private tests of one rule, each only ever run against
    // one backend. It is `a_non_finite_request_is_refused_through_the_trait` in
    // the shared contract suite now, which judges every backend through the
    // trait rather than judging one helper through its own copy of the check.
}
