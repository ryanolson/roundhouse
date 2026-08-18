// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Redis-backed [`SpendLedger`].
//!
//! One project maps to three keys, all sharing a Redis Cluster hash tag on
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
//! | `rh:{<project_id>}:budget:account` | hash | `committed` (project, current window), `member:<user_id>` per member, `window_start_ms` |
//! | `rh:{<project_id>}:budget:holds` | hash | `response_id` → packed `user`/`amount`/`expires_at_ms`, one field per live grant |
//! | `rh:{<project_id>}:budget:watermarks` | hash | `session_id` → highest settled `seq` |
//!
//! The write and read paths both live in [`spend::scripts`](scripts): every
//! trait method is exactly one Lua script, so a grant, a settle, and a
//! balance read are each one round trip regardless of how many ceilings or
//! holds they touch. See that module's doc for the two conventions worth
//! reading before editing the Lua: dollar amounts cross as strings, never Lua
//! numbers, and `now_ms` is client-supplied rather than read from
//! `redis.call('TIME')` — a deliberate departure from [`crate::scripts`]'s
//! convention, because the ledger's own contract (both here and in
//! `roundhouse_core::control::spend`) requires a TTL lapse and a monthly
//! reset to be reachable in a test without sleeping.
//!
//! Passes the same [`spend_ledger_contract_suite!`](roundhouse_core::spend_ledger_contract_suite)
//! that judges `MemorySpendLedger`, instantiated ignore-gated in
//! `tests/spend_contract.rs` exactly as `tests/contract.rs` does for the
//! session store.

mod scripts;

use std::sync::Arc;

use async_trait::async_trait;
use redis::aio::ConnectionManager;

use roundhouse_core::control::{
    Balance, BalanceQuery, BudgetTerms, BudgetWindow, Grant, GrantRequest, ProjectId, Settled,
    Settlement, SpendError, SpendLedger,
};

use crate::KEY_PREFIX;

// The braces are a Redis Cluster hash tag, on the *project* id. All three
// keys for one project hash to one slot, which is what lets `open_grant` and
// `settle_grant` check-and-debit both the project and member ceilings in one
// atomic script — see the module doc.
pub(crate) fn account_key(project: &ProjectId) -> String {
    format!("{KEY_PREFIX}:{{{project}}}:budget:account")
}

pub(crate) fn holds_key(project: &ProjectId) -> String {
    format!("{KEY_PREFIX}:{{{project}}}:budget:holds")
}

pub(crate) fn watermarks_key(project: &ProjectId) -> String {
    format!("{KEY_PREFIX}:{{{project}}}:budget:watermarks")
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

/// Refuses a non-finite or negative dollar amount before it ever reaches
/// Lua, where a `NaN` would otherwise travel silently: every comparison
/// against `NaN` is false, so a `NaN` `requested_usd` would lose every `<`
/// comparison in `OPEN_GRANT` and grant zero — a fail-open the house rule
/// forbids (no `unwrap_or(most-permissive)` on the request path). Mirrors
/// `check_amount` in `roundhouse_core::control::spend`, which is private to
/// that module for the same reason its own doc gives: the check belongs at
/// every boundary an amount enters through, not only the memory one.
fn check_amount(field: &'static str, value: f64) -> Result<(), SpendError> {
    if !value.is_finite() || value < 0.0 {
        return Err(SpendError::InvalidAmount { field, value });
    }
    Ok(())
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
}

impl RedisSpendLedger {
    /// Connect and fail fast: a ledger that cannot reach its Redis at
    /// startup should stop the process there, not on the first grant.
    pub async fn connect(url: impl AsRef<str>) -> Result<Self, SpendError> {
        let client = redis::Client::open(url.as_ref()).map_err(backend)?;
        let conn = ConnectionManager::new(client).await.map_err(backend)?;
        Ok(Self {
            conn,
            scripts: Arc::new(scripts::Scripts::new()),
        })
    }
}

#[async_trait]
impl SpendLedger for RedisSpendLedger {
    async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError> {
        check_amount("requested_usd", request.requested_usd)?;
        check_amount("limit_usd", request.terms.budget.limit_usd)?;

        let member_ceiling = member_ceiling_arg(&request.terms);
        let account = account_key(&request.principal.project);
        let holds = holds_key(&request.principal.project);
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
        check_amount("actual_usd", settlement.actual_usd)?;

        let account = account_key(&settlement.principal.project);
        let holds = holds_key(&settlement.principal.project);
        let watermarks = watermarks_key(&settlement.principal.project);
        let outcome = self
            .scripts
            .settle_grant(
                &mut self.conn.clone(),
                scripts::SettleGrantArgs {
                    account_key: &account,
                    holds_key: &holds,
                    watermarks_key: &watermarks,
                    user: settlement.principal.user.as_str(),
                    session_id: settlement.session_id.as_str(),
                    seq: settlement.seq,
                    response_id: settlement.response_id.as_str(),
                    actual_usd: settlement.actual_usd,
                    now_ms: settlement.now_ms,
                    window_mode: window_mode(settlement.terms.budget.window),
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
        check_amount("limit_usd", query.terms.budget.limit_usd)?;

        let member_ceiling = member_ceiling_arg(&query.terms);
        let account = account_key(&query.principal.project);
        let holds = holds_key(&query.principal.project);
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
        // decorative: extract the `{...}` hash tag from each of the three
        // keys and check they are the same slot-selecting substring, which
        // is what a real Redis Cluster deployment hashes on. If any of the
        // three ever drifted to a different tag, `OPEN_GRANT`/`SETTLE_GRANT`
        // would refuse to run at all on a clustered deployment (Lua scripts
        // reject multi-slot key sets) — this test catches that at build
        // time instead of at first boot against a cluster.
        fn hash_tag(key: &str) -> &str {
            let start = key.find('{').expect("every budget key carries a hash tag");
            let end = key.find('}').expect("the hash tag is closed");
            &key[start + 1..end]
        }

        let project = ProjectId::new("acme");
        let account = account_key(&project);
        let holds = holds_key(&project);
        let watermarks = watermarks_key(&project);

        let tag = hash_tag(&account);
        assert_eq!(tag, "acme", "the tag is the project id, unadorned");
        assert_eq!(hash_tag(&holds), tag);
        assert_eq!(hash_tag(&watermarks), tag);

        // The control: two different projects must land on two different
        // tags, or every project would collide onto one Redis Cluster slot.
        let other = ProjectId::new("other-project");
        assert_ne!(hash_tag(&account_key(&other)), tag);
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

    #[test]
    fn an_amount_that_is_not_a_number_of_dollars_is_refused_before_it_reaches_lua() {
        assert!(matches!(
            check_amount("requested_usd", f64::NAN),
            Err(SpendError::InvalidAmount {
                field: "requested_usd",
                ..
            })
        ));
        assert!(matches!(
            check_amount("requested_usd", f64::INFINITY),
            Err(SpendError::InvalidAmount { .. })
        ));
        assert!(matches!(
            check_amount("requested_usd", -1.0),
            Err(SpendError::InvalidAmount { .. })
        ));
        assert!(check_amount("requested_usd", 1.0).is_ok(), "the control");
    }
}
