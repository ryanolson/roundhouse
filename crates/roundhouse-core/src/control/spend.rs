// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The spend ledger: the number cheap enough to check before every turn.
//!
//! **This is not the answer to "what did this project spend."** That number is
//! `measured_usd`, folded from session logs by [`metrics`](crate::metrics) and
//! priced at snapshot from the live rate card. This is `committed_usd`, an
//! enforcement counter — and naming the two differently and never summing them
//! is the design, not an accident of having two implementations. A reconciliation
//! view shows them side by side with their difference; anything that added them
//! would report double a project's bill.
//!
//! The mechanism is an authorization hold, the same shape a card terminal uses.
//! [`SpendLedger::open_grant`] reserves the dearest admissible frontier
//! candidate's expected cost under the turn's `ResponseId` with a TTL;
//! [`SpendLedger::settle_grant`] releases the hold and applies what was actually
//! spent. Two properties make that safe to run against a durable store shared by
//! several processes:
//!
//! - **A grant is atomic across both ceilings.** `min(requested,
//!   project_remaining, member_remaining)` is computed and reserved in one
//!   operation, so two concurrent sessions under one membership cannot both read
//!   the same remaining balance and jointly overspend. The turn gate is
//!   per-*session*, so that race exists in a single process — it is not a
//!   multi-node problem deferred until multi-node.
//!   `concurrent_grants_cannot_jointly_exceed_the_limit` in
//!   [`contract`] is the proof.
//! - **A settle is idempotent by `(session_id, seq)`**, through a per-session
//!   watermark — the same rule [`MetricsFold`](crate::metrics::MetricsFold)
//!   states for itself, and for the same reason: a session replays its own log
//!   on every open, so a settle that were not idempotent would be applied again
//!   on every turn of the session that produced it.
//!
//! ## The crash story, and what it does not cover
//!
//! A process that dies between grant and settle leaves a hold. Holds carry a
//! TTL and are expired lazily by whatever call comes next, so a leaked hold
//! self-heals within one TTL with no sweeper and no cross-session index. A
//! *settle* lost to a crash is re-driven by the replay every session already
//! performs when it is next opened, through the same idempotent operation.
//!
//! The one unrepairable case is a session that is never opened again: its last
//! turn's spend stays unsettled forever. That is bounded by one turn per dead
//! session, and it is visible — as drift between `committed_usd` and
//! `measured_usd` on the reconciliation view. A wrong number with its own
//! dashboard line is a bug report; a quiet wrong number is the failure mode this
//! repo exists to avoid.
//!
//! ## Two honest limitations
//!
//! **A grant is an admission ceiling, not a bound on realized spend.** It is
//! computed from `expected_output_tokens`, so a reasoning-heavy turn can settle
//! above its hold. Settling above the hold *overcommits* rather than capping,
//! because realized spend is a fact and a ledger that clamped it would be
//! reporting a number the provider will not agree with. The overshoot lands in
//! `committed_usd` and the next grant sees it.
//!
//! **The window is enforced here and nowhere else.** [`BudgetWindow::Monthly`]
//! resets this counter at a calendar boundary, evaluated lazily against the
//! `now_ms` each call supplies — there is no background task and nothing here
//! reads a clock of its own, which is also what lets the contract suite drive a
//! month boundary without waiting for one. The metrics fold cannot window yet,
//! so `measured_usd` stays lifetime and the reconciliation view has to say which
//! of its two columns is windowed.

#[cfg(any(test, feature = "test-support"))]
pub mod contract;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::control::budget::{
    Allocation, Budget, BudgetState, BudgetWindow, Exhaustion, TurnBudget,
};
use crate::control::{Principal, ProjectId, UserId};
use crate::ids::{ResponseId, SessionId};

/// The ceilings one grant is judged against.
///
/// Travels *with* each request rather than being held by the ledger, and that
/// is deliberate. A ledger caching configuration would be a second copy of the
/// control-plane file, stale from the moment an operator edits it, and every
/// backend would need its own reload path; a Redis implementation would have to
/// keep budgets in Redis, where nothing else about the control plane lives. Both
/// ceilings arrive as arguments to one atomic operation instead, which is
/// exactly the shape a Lua script wants.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetTerms {
    pub budget: Budget,
    pub allocation: Allocation,
}

impl BudgetTerms {
    /// This membership's own ceiling, or `None` when it draws on the project's
    /// pool without a second one.
    pub fn member_ceiling_usd(&self) -> Option<f64> {
        self.allocation.member_ceiling_usd(self.budget.limit_usd)
    }
}

/// A request to reserve budget for one turn.
#[derive(Debug, Clone, PartialEq)]
pub struct GrantRequest {
    pub principal: Principal,
    pub session_id: SessionId,
    /// The turn this hold belongs to, and the key the settle releases it by.
    pub response_id: ResponseId,
    /// The dearest admissible frontier candidate's expected cost. Asking for
    /// the dearest rather than the chosen one is what makes the grant a
    /// *ceiling the choice is then made under* instead of a rubber stamp on a
    /// choice already taken.
    pub requested_usd: f64,
    /// How long the hold survives without a settle. The turn deadline plus
    /// slack: long enough that a slow turn is not charged twice, short enough
    /// that a dead process's hold clears without a sweeper.
    pub ttl_ms: u64,
    pub terms: BudgetTerms,
    /// Supplied rather than read from a clock inside the ledger, so a window
    /// boundary and a lapsed TTL are both reachable in a test without waiting
    /// for one.
    pub now_ms: u64,
}

/// What one turn may spend.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grant {
    /// `min(requested, project_remaining, member_remaining)`. Zero is an
    /// ordinary answer, not an error: an exhausted project degrades to local
    /// through the admissibility predicate rather than failing here.
    pub granted_usd: f64,
    /// Never [`BudgetState::ExhaustedOverflow`] — a ledger cannot observe the
    /// fleet, so it cannot know whether the local pool could serve. That
    /// variant is produced at exactly one place, and it is not here.
    pub state: BudgetState,
}

impl Grant {
    /// The router's view of this grant.
    ///
    /// The one conversion between the ledger's answer and the per-turn datum
    /// [`RoutingContext`](crate::routing::RoutingContext) carries, so the
    /// ceiling the router enforces and the ceiling the ledger reserved cannot
    /// be two different numbers.
    pub fn turn_budget(&self, on_exhaustion: Exhaustion) -> TurnBudget {
        TurnBudget::Granted {
            ceiling_usd: self.granted_usd,
            state: self.state,
            on_exhaustion,
        }
    }
}

/// What a turn actually spent.
#[derive(Debug, Clone, PartialEq)]
pub struct Settlement {
    pub principal: Principal,
    pub session_id: SessionId,
    /// The log sequence number of the terminal event this settles. The other
    /// half of the idempotency key: a settle at or below the session's
    /// watermark has already been applied and does nothing.
    pub seq: u64,
    pub response_id: ResponseId,
    /// Priced from the terminal [`Usage`](crate::event::Usage). Zero for a
    /// local dispatch, and zero for a dispatch that never reached a provider —
    /// but never zero because a price could not be found, which is an error
    /// rather than a free turn.
    pub actual_usd: f64,
    pub terms: BudgetTerms,
    pub now_ms: u64,
}

/// The outcome of applying one settlement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settled {
    /// `false` when `(session_id, seq)` was at or below the watermark — the
    /// settle had already been applied and this call changed nothing. The
    /// ordinary answer on a replay.
    pub applied: bool,
    /// Hold returned to the pool: `hold - actual`, floored at zero. Zero when
    /// the turn settled above its hold, which overcommits rather than capping.
    pub released_usd: f64,
    /// The project's committed spend after this call.
    pub committed_usd: f64,
}

/// A read of one membership's position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Balance {
    /// Realized spend for the project, in the current window. May exceed the
    /// limit: an overshooting settle and an overflow dispatch both land here
    /// rather than being clamped out of sight.
    pub committed_usd: f64,
    /// Reserved but not yet settled, across every live hold in the project.
    pub held_usd: f64,
    /// `(limit - committed - held)`, floored at zero.
    pub project_remaining_usd: f64,
    pub member_committed_usd: f64,
    /// `None` when the membership is [`Allocation::Pooled`] — there is no
    /// second ceiling, which is not the same as a ceiling of zero.
    pub member_remaining_usd: Option<f64>,
    pub state: BudgetState,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BalanceQuery {
    pub principal: Principal,
    pub terms: BudgetTerms,
    pub now_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SpendError {
    /// An amount that is not a number of dollars.
    ///
    /// Refused rather than clamped, because every clamp here is a fail-open:
    /// a `NaN` price silently treated as zero is unpriced frontier traffic
    /// booked as free, which is the one accounting lie this whole module exists
    /// to make impossible.
    #[error("`{field}` must be a finite, non-negative number of dollars, got {value}")]
    InvalidAmount { field: &'static str, value: f64 },
    #[error("ledger backend failure: {0}")]
    Backend(#[from] anyhow::Error),
}

fn check_amount(field: &'static str, value: f64) -> Result<(), SpendError> {
    if !value.is_finite() || value < 0.0 {
        return Err(SpendError::InvalidAmount { field, value });
    }
    Ok(())
}

/// Durable committed spend, with authorization holds.
///
/// Deliberately small, for the reason [`SessionStore`](crate::store::SessionStore)
/// is: three operations, all of which a Redis implementation can express as one
/// atomic script each. Everything the control plane wants to *say* about budgets
/// — windows, allocations, warn thresholds — arrives as [`BudgetTerms`] on the
/// call rather than as state a backend has to hold and reload.
///
/// What the two implementations must agree on is executable rather than prose:
/// [`contract`] holds these guarantees as a generic suite, and every backend —
/// including the in-memory one — is judged by that identical suite.
#[async_trait]
pub trait SpendLedger: Send + Sync + 'static {
    /// Reserve up to `requested_usd` for one turn.
    ///
    /// Grants `min(requested, project_remaining, member_remaining)` and holds
    /// it under the request's `ResponseId` until it is settled or its TTL
    /// lapses. Both ceilings are read and the hold placed in one atomic step,
    /// or concurrent grants under one membership can jointly exceed the limit.
    ///
    /// Opening a second grant under a `ResponseId` that already holds one
    /// replaces it: a turn has one hold. The engine never does this — a
    /// deduplicated retry short-circuits before `plan` — but a backend that
    /// accumulated holds per response would leak the whole difference.
    async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError>;

    /// Release the hold and apply what was actually spent.
    ///
    /// Idempotent by `(session_id, seq)`: a settlement at or below the
    /// session's watermark is a no-op and reports itself as one.
    async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError>;

    /// Read one membership's position, project and member ceilings both.
    async fn balance(&self, query: BalanceQuery) -> Result<Balance, SpendError>;
}

// ---------------------------------------------------------------------------
// Window arithmetic
// ---------------------------------------------------------------------------

const MS_PER_DAY: u64 = 86_400_000;

/// The epoch-millisecond start of the UTC calendar month containing `now_ms`.
///
/// Hand-rolled from Hinnant's civil-calendar algorithm rather than pulling in a
/// date crate: the whole requirement is "which month is this, and when did it
/// start", it is exact for every representable timestamp, and a dependency
/// whose timezone database can change would make a budget window's boundary
/// move under a deployment between two releases.
fn month_start_ms(now_ms: u64) -> u64 {
    let days = (now_ms / MS_PER_DAY) as i64;
    let (year, month, _day) = civil_from_days(days);
    (days_from_civil(year, month, 1) as u64) * MS_PER_DAY
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The instant the window containing `now_ms` began.
///
/// [`BudgetWindow::Total`] has no boundary, so it reports the epoch and never
/// resets.
fn window_start_ms(window: BudgetWindow, now_ms: u64) -> u64 {
    match window {
        BudgetWindow::Total => 0,
        BudgetWindow::Monthly => month_start_ms(now_ms),
    }
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

/// A reservation waiting for its turn to settle.
#[derive(Debug, Clone)]
struct Hold {
    user: UserId,
    amount_usd: f64,
    expires_at_ms: u64,
}

#[derive(Debug, Default)]
struct ProjectAccount {
    committed_usd: f64,
    member_committed_usd: HashMap<UserId, f64>,
    holds: HashMap<ResponseId, Hold>,
    /// Highest settled `seq` per session. The idempotency key's other half.
    ///
    /// Deliberately *not* cleared at a window boundary: a window bounds what a
    /// project may spend, not which settles it has already seen, and clearing
    /// these would double-charge every session that replayed across a month
    /// boundary.
    watermarks: HashMap<SessionId, u64>,
    /// Which window `committed_usd` belongs to, so a reset can be evaluated
    /// lazily on the next access instead of by a background task.
    window_started_ms: u64,
}

impl ProjectAccount {
    /// Drop lapsed holds and, if the window has rolled over, the previous
    /// window's committed spend.
    ///
    /// Every operation begins here. Lazily rather than on a timer, because the
    /// only observer of either fact is the next call: a hold nobody is asking
    /// about and a window nobody is spending in are indistinguishable from
    /// having already been cleaned up.
    fn settle_time(&mut self, window: BudgetWindow, now_ms: u64) {
        self.holds.retain(|_, hold| hold.expires_at_ms > now_ms);
        let current = window_start_ms(window, now_ms);
        if current > self.window_started_ms {
            self.committed_usd = 0.0;
            self.member_committed_usd.clear();
            self.window_started_ms = current;
        }
    }

    fn held_usd(&self) -> f64 {
        self.holds.values().map(|hold| hold.amount_usd).sum()
    }

    fn member_held_usd(&self, user: &UserId) -> f64 {
        self.holds
            .values()
            .filter(|hold| &hold.user == user)
            .map(|hold| hold.amount_usd)
            .sum()
    }

    fn member_committed(&self, user: &UserId) -> f64 {
        self.member_committed_usd.get(user).copied().unwrap_or(0.0)
    }
}

/// Non-durable [`SpendLedger`] for tests and single-process runs.
///
/// One lock over the whole ledger, which is what makes every operation atomic
/// in the sense the contract requires. A finer-grained scheme would buy nothing
/// here — the critical sections are a few float additions — and would reproduce
/// exactly the race the Redis implementation uses one script to avoid.
#[derive(Default, Clone)]
pub struct MemorySpendLedger {
    projects: Arc<Mutex<HashMap<ProjectId, ProjectAccount>>>,
}

impl MemorySpendLedger {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The remaining balance on both ceilings, and which of them binds.
struct Remaining {
    project: f64,
    /// `None` for a pooled membership: no second ceiling.
    member: Option<f64>,
}

impl Remaining {
    /// The tighter of the two ceilings — the amount actually available.
    fn effective(&self) -> f64 {
        match self.member {
            Some(member) => self.project.min(member),
            None => self.project,
        }
    }
}

fn remaining(account: &ProjectAccount, user: &UserId, terms: &BudgetTerms) -> Remaining {
    let project = (terms.budget.limit_usd - account.committed_usd - account.held_usd()).max(0.0);
    let member = terms.member_ceiling_usd().map(|ceiling| {
        (ceiling - account.member_committed(user) - account.member_held_usd(user)).max(0.0)
    });
    Remaining { project, member }
}

/// The state a grant or a read reports.
///
/// **Exhaustion is judged on `available` — what there was to hand out — while a
/// warning is judged on the position `account` is in now.** Two bases in one
/// function, and they are deliberate rather than sloppy: `Exhausted` has to mean
/// "this turn got nothing", because the router reads it as "ceiling zero" and it
/// is what arms [`TurnBudget::overflow_armed`] and triggers
/// [`TurnBudget::refuses`]. Judged instead on what the grant *left behind*, a
/// turn that took the entire remaining budget would come back exhausted — and so
/// would open the valve while holding money, or be refused while holding money.
/// A warning is the opposite: it is a statement about the turn *after* this one,
/// so it is read off the position this grant leaves.
///
/// A read passes its own current remaining as `available`, which collapses the
/// two bases into the one they agree on.
fn state_for(
    account: &ProjectAccount,
    user: &UserId,
    terms: &BudgetTerms,
    available: f64,
) -> BudgetState {
    if available <= 0.0 {
        return BudgetState::Exhausted;
    }
    // Warn on whichever ceiling is closer to its own edge: a member three
    // quarters through a share nobody else is near is the one who needs telling.
    let project_used = account.committed_usd + account.held_usd();
    let mut warned = project_used >= terms.budget.warn_level_usd();
    if let Some(ceiling) = terms.member_ceiling_usd() {
        let member_used = account.member_committed(user) + account.member_held_usd(user);
        warned |= member_used >= ceiling * terms.budget.warn_at;
    }
    if warned {
        BudgetState::Warned
    } else {
        BudgetState::Unconstrained
    }
}

#[async_trait]
impl SpendLedger for MemorySpendLedger {
    async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError> {
        check_amount("requested_usd", request.requested_usd)?;
        check_amount("limit_usd", request.terms.budget.limit_usd)?;

        let mut projects = self.projects.lock().await;
        let account = projects
            .entry(request.principal.project.clone())
            .or_insert_with(|| ProjectAccount {
                window_started_ms: window_start_ms(request.terms.budget.window, request.now_ms),
                ..Default::default()
            });
        account.settle_time(request.terms.budget.window, request.now_ms);

        // A turn has one hold. Replacing rather than accumulating is what makes
        // a re-grant under the same response id — which the engine avoids and a
        // buggy caller could still reach — cost the difference rather than the
        // whole amount again.
        account.holds.remove(&request.response_id);

        // Read before the hold is placed, and kept: this is both the ceiling
        // and the number `Exhausted` is judged on, and the two must not drift.
        let available = remaining(account, &request.principal.user, &request.terms).effective();
        let granted = request.requested_usd.min(available).max(0.0);
        if granted > 0.0 {
            account.holds.insert(
                request.response_id.clone(),
                Hold {
                    user: request.principal.user.clone(),
                    amount_usd: granted,
                    expires_at_ms: request.now_ms.saturating_add(request.ttl_ms),
                },
            );
        }

        // The account is read *after* the hold, so the warn threshold sees this
        // turn's own reservation — but exhaustion is still judged on what was
        // available to grant. See `state_for`.
        let state = state_for(account, &request.principal.user, &request.terms, available);
        Ok(Grant {
            granted_usd: granted,
            state,
        })
    }

    async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError> {
        check_amount("actual_usd", settlement.actual_usd)?;

        let mut projects = self.projects.lock().await;
        let account = projects
            .entry(settlement.principal.project.clone())
            .or_insert_with(|| ProjectAccount {
                window_started_ms: window_start_ms(
                    settlement.terms.budget.window,
                    settlement.now_ms,
                ),
                ..Default::default()
            });
        account.settle_time(settlement.terms.budget.window, settlement.now_ms);

        let watermark = account
            .watermarks
            .get(&settlement.session_id)
            .copied()
            .unwrap_or(0);
        if settlement.seq <= watermark {
            // The replay case, and the ordinary one: every open of a session
            // re-drives its terminal events through here.
            return Ok(Settled {
                applied: false,
                released_usd: 0.0,
                committed_usd: account.committed_usd,
            });
        }
        account
            .watermarks
            .insert(settlement.session_id.clone(), settlement.seq);

        let hold = account.holds.remove(&settlement.response_id);
        let held = hold.as_ref().map_or(0.0, |hold| hold.amount_usd);
        // Floored at zero: settling above the hold overcommits — realized spend
        // is a fact — and there is nothing left to give back.
        let released = (held - settlement.actual_usd).max(0.0);

        account.committed_usd += settlement.actual_usd;
        *account
            .member_committed_usd
            .entry(settlement.principal.user.clone())
            .or_insert(0.0) += settlement.actual_usd;

        Ok(Settled {
            applied: true,
            released_usd: released,
            committed_usd: account.committed_usd,
        })
    }

    async fn balance(&self, query: BalanceQuery) -> Result<Balance, SpendError> {
        check_amount("limit_usd", query.terms.budget.limit_usd)?;

        let mut projects = self.projects.lock().await;
        let account = projects
            .entry(query.principal.project.clone())
            .or_insert_with(|| ProjectAccount {
                window_started_ms: window_start_ms(query.terms.budget.window, query.now_ms),
                ..Default::default()
            });
        account.settle_time(query.terms.budget.window, query.now_ms);

        let left = remaining(account, &query.principal.user, &query.terms);
        let state = state_for(
            account,
            &query.principal.user,
            &query.terms,
            left.effective(),
        );
        Ok(Balance {
            committed_usd: account.committed_usd,
            held_usd: account.held_usd(),
            project_remaining_usd: left.project,
            member_committed_usd: account.member_committed(&query.principal.user),
            member_remaining_usd: left.member,
            state,
        })
    }
}

#[cfg(test)]
mod tests {
    //! The memory ledger's conformance run, plus the arithmetic the contract
    //! suite cannot reach through the trait.
    //!
    //! The assertions live in [`contract`](super::contract) and the macro is
    //! the list; this module only points it at [`MemorySpendLedger`].

    use super::*;

    crate::spend_ledger_contract_suite!(MemorySpendLedger::new());

    #[test]
    fn a_month_boundary_is_computed_without_a_calendar_dependency() {
        // 2026-08-18T00:00:00Z and 2026-08-01T00:00:00Z, from `date -u -d`.
        let mid_august = 1_787_011_200_000u64;
        let august_first = 1_785_542_400_000u64;
        assert_eq!(month_start_ms(mid_august), august_first);
        assert_eq!(
            month_start_ms(august_first),
            august_first,
            "the boundary instant belongs to the window it opens"
        );
        // The one arithmetic edge a hand-rolled calendar gets wrong: a leap day.
        // 2028-02-29T12:00:00Z falls in the February that has one.
        let leap_day = 1_835_438_400_000u64;
        let february_2028 = 1_832_976_000_000u64;
        assert_eq!(month_start_ms(leap_day), february_2028);
        assert_eq!(
            civil_from_days((leap_day / MS_PER_DAY) as i64),
            (2028, 2, 29)
        );

        // `Total` has no boundary to roll over, which is what makes it never
        // reset.
        assert_eq!(window_start_ms(BudgetWindow::Total, mid_august), 0);
        assert_eq!(
            window_start_ms(BudgetWindow::Monthly, mid_august),
            august_first
        );
    }

    #[tokio::test]
    async fn an_amount_that_is_not_a_number_of_dollars_is_refused_rather_than_clamped() {
        let ledger = MemorySpendLedger::new();
        let terms = BudgetTerms {
            budget: Budget {
                limit_usd: 10.0,
                window: BudgetWindow::Total,
                on_exhaustion: Exhaustion::degrade_with_overflow(),
                warn_at: 0.8,
            },
            allocation: Allocation::Pooled,
        };
        let request = GrantRequest {
            principal: Principal::new("acme", "ada"),
            session_id: SessionId::new("s"),
            response_id: ResponseId::new("r"),
            requested_usd: f64::NAN,
            ttl_ms: 1_000,
            terms: terms.clone(),
            now_ms: 0,
        };
        assert!(matches!(
            ledger.open_grant(request.clone()).await,
            Err(SpendError::InvalidAmount {
                field: "requested_usd",
                ..
            })
        ));
        assert!(
            ledger
                .open_grant(GrantRequest {
                    requested_usd: 1.0,
                    ..request
                })
                .await
                .is_ok(),
            "the control: a real amount is granted"
        );

        // A settle that cannot be priced must not arrive here as zero, so a
        // non-number is refused on this side too.
        assert!(matches!(
            ledger
                .settle_grant(Settlement {
                    principal: Principal::new("acme", "ada"),
                    session_id: SessionId::new("s"),
                    seq: 1,
                    response_id: ResponseId::new("r"),
                    actual_usd: f64::INFINITY,
                    terms,
                    now_ms: 0,
                })
                .await,
            Err(SpendError::InvalidAmount {
                field: "actual_usd",
                ..
            })
        ));
    }

    #[test]
    fn a_grant_converts_into_the_ceiling_the_router_enforces() {
        // One conversion, so the number the ledger reserved and the number the
        // router compares against cannot drift apart.
        let grant = Grant {
            granted_usd: 0.0,
            state: BudgetState::Exhausted,
        };
        let budget = grant.turn_budget(Exhaustion::degrade_with_overflow());
        assert_eq!(budget.state(), BudgetState::Exhausted);
        assert!(budget.overflow_armed());
        assert!(!budget.refuses());
        assert!(
            grant.turn_budget(Exhaustion::Refuse).refuses(),
            "the same grant under a refusing project"
        );
    }
}
