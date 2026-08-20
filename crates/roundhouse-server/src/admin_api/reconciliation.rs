// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `GET /v1/admin/projects/{p}/budget`: what the ledger says, what the log says,
//! and the gap between them.
//!
//! Its own module rather than a section of [`super`], because it is the one read
//! on the admin plane and it has a whole vocabulary the CRUD routes do not
//! share — a stamp per column, four reasons a dollar figure may be absent, and
//! a rule about which terms a balance may be read under that is load-bearing
//! enough to have its own paragraph. The routes next door are about *changing*
//! tenancy; this is about not lying while describing it.
//!
//! # The one rule everything else follows from
//!
//! **Nothing here constructs [`BudgetTerms`].** Every balance is read under the
//! terms the membership's own [`Admission`](crate::Admission) carries, because
//! [`SpendLedger::balance`] is not a pure read: it rolls the account's window
//! over if the window it was given has lapsed. A sweep with the wrong
//! [`BudgetWindow`] would therefore zero a project's committed spend
//! permanently — a reporting endpoint quietly handing a month's budget back.
//! Where no admission exists there is no figure, and the row says which of the
//! four reasons applies rather than reporting a zero.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use roundhouse_core::control::{
    Allocation, BalanceQuery, BudgetTerms, BudgetWindow, Principal, PrincipalKey, ProjectId,
    SpendLedger,
};
use roundhouse_core::metrics::TokenBreakdown;
use roundhouse_core::now_ms;

use super::{AdminState, find_project};
use crate::control_config::MembershipError;
use crate::http::ApiError;

/// Where a dollar figure came from, and over what period.
///
/// **Every column is stamped, and that is the whole design of this view.** The
/// two numbers it puts side by side are produced by different machinery over
/// different periods — the ledger counts committed spend *within a budget
/// window*, the metrics fold counts what this process has measured *since it
/// started* — and a reader who does not know that will read their difference as
/// an accounting error. So the difference is published, and so is the reason it
/// is not one.
#[derive(Debug, Serialize)]
struct Stamp {
    /// `ledger`, `process-fold`, `unenforced`, `no_keys` or `archived`. The last
    /// three are why the dollars beside them are `null`.
    basis: &'static str,
    /// `total`, `monthly` or `lifetime`.
    window: &'static str,
    /// When the window this figure covers began. `null` for a lifetime figure,
    /// which has no boundary, and for every basis with no dollars.
    window_start_ms: Option<u64>,
}

impl Stamp {
    /// The ledger's stamp: a real window, with the instant it began.
    fn ledger(window: BudgetWindow, window_start_ms: u64) -> Self {
        Self {
            basis: "ledger",
            window: match window {
                BudgetWindow::Total => "total",
                BudgetWindow::Monthly => "monthly",
            },
            window_start_ms: Some(window_start_ms),
        }
    }

    /// **Lifetime, and only ever lifetime.** The fold's watermarks cannot be
    /// pruned without event-time windowing, so it has no way to answer "this
    /// month" — and the honest thing is to say so in the document rather than to
    /// label a lifetime number with the ledger's window.
    ///
    /// It is also *process*-lifetime: the fold is in memory and starts empty
    /// after a restart, which is why the basis names the process and not the
    /// log.
    fn measured() -> Self {
        Self {
            basis: "process-fold",
            window: "lifetime",
            window_start_ms: None,
        }
    }

    /// The stamp on a row with no committed figure. See [`Basis`].
    fn absent(basis: &'static str) -> Self {
        Self {
            basis,
            window: "none",
            window_start_ms: None,
        }
    }
}

/// Why a row has no committed spend to report.
///
/// Three answers and not one, because an operator's next action differs for each
/// and `null` alone tells them nothing. **Never `0.0` for any of them:** a
/// project that has spent nothing and a project nothing is counting look
/// identical as a zero, and the second one is the state somebody needs to fix.
enum Basis {
    /// No budget is configured, so the engine never calls the ledger for this
    /// membership and there is no position to read.
    Unenforced,
    /// The membership exists and has no key, so it has no admission — and
    /// therefore no [`BudgetTerms`] to read a balance under, and no turn it could
    /// have spent anything on. Distinct from [`Self::Unenforced`], which means
    /// the opposite thing: there, spending is happening and nothing is counting
    /// it.
    NoKeys,
    /// The project is archived. Its keys are refused and it is left out of the
    /// compiled plane entirely, so there is no admission to take terms from — but
    /// its spend history outlives it, which is the reason archiving is not
    /// deletion, so the measured column is still real. A row with `measured_usd`
    /// and no `committed_usd` looks wrong and is not.
    Archived,
}

impl Basis {
    fn as_str(&self) -> &'static str {
        match self {
            Basis::Unenforced => "unenforced",
            Basis::NoKeys => "no_keys",
            Basis::Archived => "archived",
        }
    }
}

/// `GET /v1/admin/projects/{p}/budget` — what the ledger says, what the log
/// says, and the gap.
///
/// **No total field, and there never will be one.** The two figures are not
/// addends of anything: `committed_usd` is what a project was charged against
/// its ceiling and `measured_usd` is what this process measured it spending, and
/// a sum of the two is a number with no referent that a dashboard would print
/// anyway. What *is* published is their difference, which does have a referent.
///
/// `drift_usd` is `committed - measured`, unclamped in both directions. Negative
/// means the fold saw spend the ledger did not — a settle that failed and was
/// logged as a warning, a process restarted between the dispatch and the
/// settle — and that is precisely the number somebody is looking for when they
/// come to this endpoint. Clamping it at zero, or "repairing" it here, would
/// hide the only evidence of the failure it exists to surface.
///
/// `seat_tokens` is the dollar-free column: traffic served under a forwarded
/// subscription seat is measured in tokens and priced nowhere, because the seat
/// is a subscription and this deployment has no per-token figure it may
/// honestly name for it. Without the column a pass-through project reads as
/// having done almost nothing.
#[derive(Debug, Serialize)]
struct BudgetViewDto {
    project: String,
    /// Realized spend in the current budget window, from the ledger, `null`
    /// whenever `committed.basis` is not `ledger`.
    committed_usd: Option<f64>,
    /// Reserved and not yet settled, across every live hold in the project.
    /// `null` under the same condition `committed_usd` is.
    held_usd: Option<f64>,
    committed: Stamp,
    /// What this process has measured the project's principals spending on
    /// hosted providers, over the process's lifetime. Never `null`: the fold
    /// always has an answer, even if it is zero.
    measured_usd: f64,
    measured: Stamp,
    /// `committed_usd - measured_usd`, or `null` exactly when `committed_usd` is.
    drift_usd: Option<f64>,
    seat_tokens: TokenBreakdown,
    members: Vec<MemberBudgetDto>,
    /// The sum of every member's `Share` allocation, or `null` where no member
    /// has one.
    ///
    /// **Reported and never refused.** Shares are allowed to sum past 1.0 — the
    /// project's own limit binds regardless, so over-subscription is a real
    /// arrangement an operator may want (five people who will not all spend at
    /// once) rather than a mistake. What they are owed is being able to see it,
    /// which is what this number is for. `Capped` allocations are absent because
    /// dollars and fractions do not sum to anything a reader could use.
    allocation_share_sum: Option<f64>,
}

/// One member's row, under the same discipline as the project's.
///
/// **No `held_usd`.** The ledger's holds are a project-wide figure and there is
/// no per-member decomposition of them; dividing the project's, or repeating it
/// on every row, would be an invented number in the one view whose entire
/// purpose is that its numbers are not invented.
#[derive(Debug, Serialize)]
struct MemberBudgetDto {
    user: String,
    provenance: String,
    member_committed_usd: Option<f64>,
    /// What is left of this member's own ceiling. `null` for a pooled
    /// membership, which has no *second* ceiling — not a ceiling of zero.
    member_remaining_usd: Option<f64>,
    committed: Stamp,
    measured_usd: f64,
    measured: Stamp,
    drift_usd: Option<f64>,
    seat_tokens: TokenBreakdown,
    /// This member's `Share` fraction, if their allocation is one.
    allocation_share: Option<f64>,
}

pub(super) async fn budget_view(
    State(state): State<AdminState>,
    Path(project): Path<String>,
) -> Result<Response, ApiError> {
    let at_ms = now_ms();
    // One snapshot for the whole document: the memberships listed and the terms
    // their balances are read under have to come from one compiled plane, or a
    // write landing mid-render would produce a view assembled from two.
    let plane = state.directory.plane(at_ms);
    let view = state.directory.view(at_ms);
    let record = find_project(&view, &project)?;
    let archived = record.is_archived();

    let project_id = ProjectId::new(project.clone());
    let project_measured =
        state
            .metrics
            .snapshot_for_project(&project_id, &state.metrics_config, at_ms);

    let mut members: Vec<MemberBudgetDto> = Vec::new();
    let mut shares: Vec<f64> = Vec::new();
    // The project's own two figures, taken from whichever member row first read
    // a balance. Safe, and the sentence is worth writing down: `BudgetTerms`
    // pairs the *project's* budget with one member's allocation, and
    // `Balance::committed_usd` and `Balance::held_usd` are project-wide — only
    // the `member_*` fields vary with the allocation. So every member of a
    // budgeted project reads the same two numbers here, and reading them from
    // the first is not a choice about which member is representative.
    let mut project_committed: Option<(f64, f64, Stamp)> = None;
    let mut unenforced_seen = false;

    for membership in view
        .memberships
        .iter()
        .filter(|membership| membership.project == project)
    {
        let principal = Principal::new(project.clone(), membership.user.clone());
        let measured = state.metrics.snapshot_for(
            &PrincipalKey::from(&principal),
            &state.metrics_config,
            at_ms,
        );
        let measured_usd = dollars(measured.savings.frontier_spend_usd);

        // The admission, resolved backwards through the one function that
        // refuses rather than guessing when two keys disagree. Reaching into
        // `configured_admissions()` and filtering by principal would have picked
        // whichever of them the hash map yielded first.
        //
        // Asked once, because the two things read off it — is there an admission
        // at all, and does it carry a budget — are the two answers this row's
        // basis is chosen between.
        let admission = match plane.membership(&principal) {
            Ok(admission) => Some(admission),
            // No key names this membership, so it has no admission — and has
            // therefore never spent anything either, since a turn needs a key.
            Err(MembershipError::Unknown(_)) => None,
            // Unreachable on a deployment that booted — the cross-check refuses
            // it at startup and again after every admin write — and reported
            // rather than papered over, because the alternative is telling an
            // operator a member's spend under a policy that member's own key may
            // not have.
            Err(error @ MembershipError::Ambiguous(_)) => {
                return Err(ApiError::internal(
                    "ambiguous_membership",
                    error.to_string(),
                ));
            }
        };

        // The archived arm is first because an archived project's memberships
        // resolve to nothing too, and `no_keys` would be the wrong reason for
        // the right `null`.
        let (committed, remaining, stamp, share) = match (archived, admission) {
            (true, _) => (None, None, Stamp::absent(Basis::Archived.as_str()), None),
            (false, None) => (None, None, Stamp::absent(Basis::NoKeys.as_str()), None),
            (false, Some(admission)) => match admission.budget {
                None => {
                    unenforced_seen = true;
                    (None, None, Stamp::absent(Basis::Unenforced.as_str()), None)
                }
                Some(terms) => {
                    let share = match terms.allocation {
                        Allocation::Share { fraction } => Some(fraction),
                        _ => None,
                    };
                    let balance =
                        read_balance(state.spend.as_ref(), &principal, &terms, at_ms).await?;
                    let stamp = Stamp::ledger(terms.budget.window, window_start_ms(&terms, at_ms));
                    if project_committed.is_none() {
                        project_committed = Some((
                            dollars(balance.committed_usd),
                            dollars(balance.held_usd),
                            Stamp::ledger(terms.budget.window, window_start_ms(&terms, at_ms)),
                        ));
                    }
                    (
                        Some(dollars(balance.member_committed_usd)),
                        balance.member_remaining_usd.map(dollars),
                        stamp,
                        share,
                    )
                }
            },
        };
        if let Some(fraction) = share {
            shares.push(fraction);
        }
        members.push(MemberBudgetDto {
            user: membership.user.clone(),
            provenance: membership.provenance.to_string(),
            member_committed_usd: committed,
            member_remaining_usd: remaining,
            drift_usd: committed.map(|committed| dollars(committed - measured_usd)),
            committed: stamp,
            measured_usd,
            measured: Stamp::measured(),
            seat_tokens: measured.seat_tokens,
            allocation_share: share,
        });
    }

    let measured_usd = dollars(project_measured.savings.frontier_spend_usd);
    let (committed_usd, held_usd, committed) = match project_committed {
        Some((committed, held, stamp)) => (Some(committed), Some(held), stamp),
        // The project's own basis follows from its members': archived first,
        // then "somebody is spending and nothing counts it", then "nobody can
        // spend at all". Ordered so the most actionable answer wins where a
        // project has some of each.
        None if archived => (None, None, Stamp::absent(Basis::Archived.as_str())),
        None if unenforced_seen => (None, None, Stamp::absent(Basis::Unenforced.as_str())),
        None => (None, None, Stamp::absent(Basis::NoKeys.as_str())),
    };

    Ok(axum::Json(BudgetViewDto {
        project,
        committed_usd,
        held_usd,
        committed,
        measured_usd,
        measured: Stamp::measured(),
        drift_usd: committed_usd.map(|committed| dollars(committed - measured_usd)),
        seat_tokens: project_measured.seat_tokens,
        members,
        allocation_share_sum: (!shares.is_empty()).then(|| shares.iter().sum()),
    })
    .into_response())
}

/// One membership's position, under the terms its own admission carries.
///
/// **Verbatim, and this is the sharp edge of the whole view.**
/// [`SpendLedger::balance`] is not a pure read: it rolls the window over if the
/// account's stored window has lapsed. Handing it terms assembled here rather
/// than the ones the engine spends under would let a sweep with the wrong
/// [`BudgetWindow`] roll a project's window and zero its committed spend
/// permanently — a reporting endpoint silently handing a month's budget back.
async fn read_balance(
    spend: &dyn SpendLedger,
    principal: &Principal,
    terms: &BudgetTerms,
    now_ms: u64,
) -> Result<roundhouse_core::control::Balance, ApiError> {
    spend
        .balance(BalanceQuery {
            principal: principal.clone(),
            terms: terms.clone(),
            now_ms,
        })
        .await
        // Surfaced, never zeroed. A view that answered `0.0` for a ledger it
        // could not reach would report an unspent budget to whoever was about to
        // decide whether to raise it.
        .map_err(|error| ApiError::internal("ledger_error", error.to_string()))
}

/// When the window a committed figure belongs to began.
///
/// Derived here rather than read off the [`Balance`](roundhouse_core::control::Balance),
/// which does not carry it: [`BudgetWindow::Total`] has no boundary and reports
/// the epoch, and a monthly window began at the start of the current UTC
/// calendar month.
fn window_start_ms(terms: &BudgetTerms, now_ms: u64) -> u64 {
    roundhouse_core::control::window_start_ms(terms.budget.window, now_ms)
}

/// A dollar figure with negative zero flattened.
///
/// The in-memory ledger sums an empty hold list to `-0.0`, which is `0.0` by
/// every comparison and `-0` in every JSON document — a number no operator will
/// read as "nothing held".
fn dollars(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}
