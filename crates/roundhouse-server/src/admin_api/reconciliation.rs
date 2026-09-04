// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `GET /v1/admin/projects/{p}/budget`: what the ledger says, what the log says,
//! and the gap between them.
//!
//! Its own module rather than a section of [`super`], because it is the one read
//! on the admin plane and it has a whole vocabulary the CRUD routes do not
//! share — a stamp per column, four named reasons a committed figure is not a
//! plain ledger figure (three of them absences and one of them not), and
//! a rule about which terms a balance may be read under that is load-bearing
//! enough to have its own paragraph. The routes next door are about *changing*
//! tenancy; this is about not lying while describing it.
//!
//! # The one rule everything else follows from
//!
//! **Nothing here constructs [`BudgetTerms`].** [`SpendLedger::balance`] is not
//! a pure read: it rolls the account's window over if the window it was given
//! has lapsed. A sweep with the wrong [`BudgetWindow`] would therefore zero a
//! project's committed spend permanently — a reporting endpoint quietly handing
//! a month's budget back.
//!
//! So a balance is read under one of exactly two sets of terms, and this module
//! authors neither. Ordinarily they are the ones the membership's own
//! [`Admission`](crate::Admission) carries. For a membership whose keys have all
//! been revoked there is no admission left to carry any — and its spend is still
//! in the ledger, still binding the project's ceiling — so the terms come from
//! [`ControlDirectory::membership_terms`](crate::control_config::ControlDirectory::membership_terms),
//! which pairs the project block with the membership's allocation through the
//! same function the compiler pairs them with for a live key. Same bytes, same
//! window, no second authority. Where neither source has terms there is no
//! figure, and the row says which of the reasons applies rather than reporting a
//! zero.

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
use crate::control_config::{ApiKeyRecord, DirectoryView, KeyRecordScope, MembershipError};
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
#[derive(Debug, Clone, Serialize)]
struct Stamp {
    /// `ledger`, `process-fold`, `revoked_keys`, `unenforced`, `no_keys` or
    /// `archived`.
    ///
    /// The last three are why the dollars beside them are `null`.
    /// `revoked_keys` is not one of them: it carries a real figure, read from
    /// the ledger over a real window, and says that the membership it belongs to
    /// can no longer spend against it. See [`Basis`].
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
        Self::figure("ledger", window, window_start_ms)
    }

    /// The ledger's stamp for a membership with no live key left.
    ///
    /// The same account, the same terms, the same window as [`Self::ledger`] —
    /// the figure is not a different kind of number and must not be labelled as
    /// though it were. What the label adds is the fact the row would otherwise
    /// lose: nobody can spend against this position any more. See
    /// [`Basis::RevokedKeys`].
    fn revoked_keys(window: BudgetWindow, window_start_ms: u64) -> Self {
        Self::figure(Basis::RevokedKeys.as_str(), window, window_start_ms)
    }

    /// A stamp with dollars behind it, whatever the label says about whose.
    fn figure(basis: &'static str, window: BudgetWindow, window_start_ms: u64) -> Self {
        Self {
            basis,
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

    /// The stamp on the provider-reported column.
    ///
    /// Process-lifetime like [`Self::measured`], and for the same reason: the
    /// figure is folded in memory from the log this process has read. What the
    /// basis adds is that nobody here produced the number — it is the upstream's
    /// own arithmetic, which is the entire reason it is worth putting beside
    /// ours.
    fn provider_reported() -> Self {
        Self {
            basis: "provider-reported",
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

/// Why a row's committed column reads the way it does.
///
/// Three of these are reasons there is no figure at all, and they are three
/// answers rather than one because an operator's next action differs for each
/// and `null` alone tells them nothing. **Never `0.0` for any of those three:**
/// a project that has spent nothing and a project nothing is counting look
/// identical as a zero, and the second one is the state somebody needs to fix.
///
/// [`Self::RevokedKeys`] is the fourth and is not one of them — it labels a row
/// that *does* carry dollars. It is here rather than beside `ledger` as a bare
/// string because it exists for the same reason the other three do: it is the
/// answer to "why does this row not say `ledger`", and the whole point of this
/// enum is that every such answer is a named one.
enum Basis {
    /// No budget is configured, so the engine never calls the ledger for this
    /// membership and there is no position to read.
    Unenforced,
    /// The membership exists and has *never* had a key, so it has no admission —
    /// and therefore no [`BudgetTerms`] to read a balance under, and no turn it
    /// could have spent anything on. Distinct from [`Self::Unenforced`], which
    /// means the opposite thing: there, spending is happening and nothing is
    /// counting it.
    ///
    /// Never-had-a-key and no-longer-has-one are the same absence in the
    /// compiled plane and completely different facts to an operator, which is
    /// what [`Self::RevokedKeys`] exists to separate.
    NoKeys,
    /// The project is archived. Its keys are refused and it is left out of the
    /// compiled plane entirely, so there is no admission to take terms from — but
    /// its spend history outlives it, which is the reason archiving is not
    /// deletion, so the measured column is still real. A row with `measured_usd`
    /// and no `committed_usd` looks wrong and is not.
    Archived,
    /// Every key this membership had has been revoked, and the ledger still
    /// holds what it spent before that.
    ///
    /// **The one basis with dollars behind it that is not `ledger`.** A revoked
    /// hash is gone from the compiled plane's admissions, so this membership
    /// resolves to nothing there — exactly like [`Self::NoKeys`] — while the
    /// ledger goes on counting its committed spend against the project's ceiling
    /// and any hold it left open goes on binding it. Reporting `no_keys` for it
    /// would blank a figure this deployment still has, and blank it in the one
    /// view whose purpose is that its numbers are not invented; reporting
    /// `ledger` would hide that nobody can spend against the position any more,
    /// which is the operator's actual question when the row stops moving.
    ///
    /// The terms are derived from the directory's own rows rather than from an
    /// admission — see the module doc — and every dollar figure here is real.
    RevokedKeys,
}

impl Basis {
    fn as_str(&self) -> &'static str {
        match self {
            Basis::Unenforced => "unenforced",
            Basis::NoKeys => "no_keys",
            Basis::Archived => "archived",
            Basis::RevokedKeys => "revoked_keys",
        }
    }
}

/// `GET /v1/admin/projects/{p}/budget` — what the ledger says, what the log
/// says, and the gap.
///
/// **No total field, and there never will be one.** The figures are not
/// addends of anything: `committed_usd` is what a project was charged against
/// its ceiling, `measured_usd` is what this process measured it spending, and
/// `provider_reported_usd` is what the upstreams themselves billed — a sum of
/// any of them is a number with no referent that a dashboard would print
/// anyway. What *is* published is the difference of the first two, which does
/// have a referent. The third stays out of that difference deliberately: it is
/// the only column here this deployment did not compute, and folding it in
/// would turn the cross-check into a self-check.
///
/// `drift_usd` is `committed - measured`, unclamped in both directions. Negative
/// means the fold saw spend the ledger did not, for one of three causes, two of
/// them real problems and one of them not:
///
/// - a settle that failed and was logged as a warning;
/// - a process restarted between the dispatch and the settle;
/// - **or, ordinarily and by design, nothing wrong at all:** the engine always
///   writes a turn's terminal usage event to the log *before* it settles the
///   ledger (see the engine's own "money after the log, always" rule), so any
///   turn currently between those two steps has already been measured but not
///   yet committed. `held_usd` is the discriminator — it is nonzero for
///   exactly the turns in that window, so a reader can tell "still settling"
///   from "actually lost" by checking whether anything is held before
///   escalating.
///
/// Clamping it at zero, or "repairing" it here, would hide the only evidence
/// of the two genuine failures this number exists to surface.
///
/// **The list is three long because a fourth cause was closed rather than
/// documented.** Until M11.0 review finding F2, `measured_usd` priced a
/// *summed* `Usage` while `committed_usd` accrued one turn at a time, and
/// `ProviderPricing::price` had stopped being additive over such a sum — so a
/// project whose traffic mixed measured and unmeasured cache writes drifted
/// permanently, with nothing held, no failed settle and no restart. The metrics
/// rollup now accumulates each turn's own pricing decision
/// (`routing::PooledUsage`), which makes the three causes above exhaustive by
/// construction instead of by assertion.
///
/// `seat_tokens` is the dollar-free column: traffic served under a forwarded
/// subscription seat is measured in tokens and priced nowhere, because the seat
/// is a subscription and this deployment has no per-token figure it may
/// honestly name for it. Without the column a pass-through project reads as
/// having done almost nothing.
#[derive(Debug, Serialize)]
struct BudgetViewDto {
    project: String,
    /// Realized spend in the current budget window, from the ledger.
    ///
    /// `null` exactly when `committed.basis` is one of the three that say there
    /// is no position to read — `unenforced`, `no_keys`, `archived`. A
    /// `revoked_keys` stamp carries a real figure: see [`Basis::RevokedKeys`].
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
    ///
    /// **`provider_reported_usd` is not in it, and never will be.** The whole
    /// use of the column below is to be a number this deployment did not
    /// produce; folding it into the difference would make the drift a
    /// comparison of our arithmetic with itself, and the one disagreement the
    /// view exists to show would read as agreement.
    drift_usd: Option<f64>,
    /// What the providers themselves billed for this project's calls, over the
    /// same process lifetime `measured_usd` covers.
    ///
    /// **A third number, not a third addend.** `committed_usd` is what the
    /// ledger charged, `measured_usd` is what this process priced from the
    /// catalog, and this is what the upstream said — three answers to one
    /// question from three machines, published side by side under their own
    /// stamps rather than reconciled into one figure nobody could check.
    ///
    /// `null` when no call in this scope reported a price, which is most
    /// deployments: a provider that bills nothing and a provider that says
    /// nothing are different facts, and only the first is a figure.
    provider_reported_usd: Option<f64>,
    provider_reported: Stamp,
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
    /// This member's share of what the providers themselves billed. Same rule
    /// as the project's: published, never summed into `drift_usd`.
    provider_reported_usd: Option<f64>,
    provider_reported: Stamp,
    seat_tokens: TokenBreakdown,
    /// This member's `Share` fraction, if their allocation is one.
    allocation_share: Option<f64>,
}

/// How one row's committed figure is going to be produced.
///
/// Decided before any ledger call, so that the two arms which *have* terms read
/// their balance and stamp their window in one place rather than two. The second
/// copy of that code is where the next hand-built [`BudgetTerms`] would appear,
/// and the module doc's first rule is that there is not one.
enum Position {
    /// Terms to read a balance under, and whether they came from a live
    /// admission (`ledger`) or were derived for a membership whose keys are all
    /// revoked (`revoked_keys`). The figures are equally real either way; the
    /// flag decides the label, and which row the project inherits its own from.
    Figure { terms: BudgetTerms, live: bool },
    /// No figure, and which of the reasons applies. See [`Basis`].
    Absent(Basis),
}

/// Whether this membership has keys and every one of them is revoked.
///
/// Asked of the listing rather than of the plane, because the plane cannot
/// answer it: a revoked hash is not in the admissions table and neither is a
/// hash that was never minted, so "no admission" is where the two cases become
/// one. The rows keep the tombstone — revocation is never a delete — which is
/// what makes the distinction recoverable at all.
///
/// Both halves of the condition matter. Some key: a membership with none has
/// never spent and there is nothing to report. All revoked: a membership with a
/// live key resolves to an admission and never reaches this question, so the
/// mixed case can only be a listing and a plane describing two different
/// versions — which [`ControlDirectory::snapshot`](crate::control_config::ControlDirectory::snapshot)
/// exists to prevent, and which this deliberately does not paper over.
fn keys_all_revoked(view: &DirectoryView, project: &str, user: &str) -> bool {
    let minted: Vec<&ApiKeyRecord> = view
        .keys
        .iter()
        .filter(|key| {
            matches!(
                &key.scope,
                KeyRecordScope::Turn { project: p, user: u } if p == project && u == user
            )
        })
        .collect();
    !minted.is_empty() && minted.iter().all(|key| key.is_revoked())
}

/// **Cost, written down so a future change to it is a decision and not a
/// surprise:** this issues one [`SpendLedger::balance`] call per budgeted
/// membership inside the loop below, so one `GET` performs N ledger
/// round-trips, linear in the project's member count — and `balance` is not
/// free the way a read normally is, since it rolls a lapsed window over (see
/// the module doc), so this is N *mutating* round-trips, not N cache hits.
/// Every figure produced is still correct: the rollovers are idempotent and
/// order-independent (see `project_committed`'s own comment below), so this
/// is a cost, not a defect. M8 has no pagination or rate limiting on the admin
/// surface by design, so nothing here bounds N today.
///
/// A single project-scoped ledger read — one round-trip regardless of member
/// count — is deferred rather than built: it needs a new [`SpendLedger`]
/// method, coverage in its contract suite so the Rust and Redis-Lua
/// implementations are held to the same answer, and the Lua script itself.
/// That is real work for a milestone with no reported latency complaint
/// against this endpoint yet, not a one-line hoist — the per-member loop
/// still has to run for the member rows regardless, so a project-scoped call
/// would sit *beside* it rather than replace it.
pub(super) async fn budget_view(
    State(state): State<AdminState>,
    Path(project): Path<String>,
) -> Result<Response, ApiError> {
    let at_ms = now_ms();
    // One snapshot for the whole document, and one call because that is the only
    // way to have one: the memberships listed and the terms their balances are
    // read under have to come from one compiled plane, and two calls would be two
    // lock acquisitions with a write free to land between them — a member with a
    // live key resolving to no admission, reported here as a row with no figures.
    let (plane, view) = state.directory.snapshot(at_ms).await;
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
    //
    // The fourth element is whether that row's terms came from a live
    // admission, and it is what breaks the tie for the *label*: a row read under
    // a live key outranks one read for a revoked-only membership, because the
    // project's ceiling is being enforced against something as long as one key
    // is live. `revoked_keys` reaches the project row only when no key in the
    // project does — which is the state that would otherwise report `no_keys`
    // over a ledger holding money.
    let mut project_committed: Option<(f64, f64, Stamp, bool)> = None;
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
        // at all, and does it carry a budget — are what this row's basis is
        // chosen between.
        let admission = match plane.membership(&principal) {
            Ok(admission) => Some(admission),
            // No *live* key names this membership. That is not the same as
            // never having had one, and the difference is money: a revoked key
            // leaves no admission behind and leaves its spend in the ledger.
            // Which of the two this is is settled below, off the rows.
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
        let position = match (archived, admission) {
            (true, _) => Position::Absent(Basis::Archived),
            (false, Some(admission)) => match admission.budget {
                None => Position::Absent(Basis::Unenforced),
                Some(terms) => Position::Figure { terms, live: true },
            },
            // No admission, which is two facts wearing one shape: a membership
            // nobody ever minted a key for, and one whose keys have all been
            // revoked *after* it spent money. Only the rows can tell them apart
            // — the compiled plane has forgotten the second — and getting it
            // wrong blanks a balance the ledger is still enforcing.
            (false, None) => match keys_all_revoked(&view, &project, &membership.user) {
                false => Position::Absent(Basis::NoKeys),
                true => match state.directory.membership_terms(record, membership)? {
                    Some(terms) => Position::Figure { terms, live: false },
                    // The project has no budget, so nothing ever counted this
                    // membership's spend and there is no position a revoked key
                    // could have left behind. `unenforced` is the honest reason
                    // for the absent figure; `revoked_keys` is kept for the row
                    // that actually carries dollars.
                    None => Position::Absent(Basis::Unenforced),
                },
            },
        };

        let (committed, remaining, stamp, share) = match position {
            Position::Absent(basis) => {
                unenforced_seen |= matches!(basis, Basis::Unenforced);
                (None, None, Stamp::absent(basis.as_str()), None)
            }
            Position::Figure { terms, live } => {
                let share = match terms.allocation {
                    Allocation::Share { fraction } => Some(fraction),
                    _ => None,
                };
                let balance = read_balance(state.spend.as_ref(), &principal, &terms, at_ms).await?;
                let window_start_ms = window_start_ms(&terms, at_ms);
                let stamp = match live {
                    true => Stamp::ledger(terms.budget.window, window_start_ms),
                    false => Stamp::revoked_keys(terms.budget.window, window_start_ms),
                };
                // First row wins, except that a live row displaces a
                // revoked-only one — see `project_committed`'s declaration.
                if project_committed
                    .as_ref()
                    .is_none_or(|(.., from_live)| live && !from_live)
                {
                    project_committed = Some((
                        dollars(balance.committed_usd),
                        dollars(balance.held_usd),
                        stamp.clone(),
                        live,
                    ));
                }
                (
                    Some(dollars(balance.member_committed_usd)),
                    balance.member_remaining_usd.map(dollars),
                    stamp,
                    share,
                )
            }
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
            provider_reported_usd: measured.savings.provider_reported_usd.map(dollars),
            provider_reported: Stamp::provider_reported(),
            seat_tokens: measured.seat_tokens,
            allocation_share: share,
        });
    }

    let measured_usd = dollars(project_measured.savings.frontier_spend_usd);
    let (committed_usd, held_usd, committed) = match project_committed {
        Some((committed, held, stamp, _)) => (Some(committed), Some(held), stamp),
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
        provider_reported_usd: project_measured.savings.provider_reported_usd.map(dollars),
        provider_reported: Stamp::provider_reported(),
        seat_tokens: project_measured.seat_tokens,
        members,
        allocation_share_sum: (!shares.is_empty()).then(|| shares.iter().sum()),
    })
    .into_response())
}

/// One membership's position, under terms this module did not write.
///
/// **This is the sharp edge of the whole view.** [`SpendLedger::balance`] is not
/// a pure read: it rolls the window over if the account's stored window has
/// lapsed. Handing it terms assembled here rather than the ones the engine
/// spends under would let a sweep with the wrong [`BudgetWindow`] roll a
/// project's window and zero its committed spend permanently — a reporting
/// endpoint silently handing a month's budget back.
///
/// So `terms` is either the membership's own admission's, verbatim, or — for a
/// membership whose keys are all revoked, which has no admission left — the
/// pairing of its project's budget block with its allocation that
/// [`ControlDirectory::membership_terms`](crate::control_config::ControlDirectory::membership_terms)
/// derives through the compiler's own function. The second is not a relaxation
/// of the rule: it is the same bytes the first would have carried, produced by
/// the same code, which is exactly why deriving them is safe and assembling them
/// here would not be.
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
