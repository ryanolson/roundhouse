// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The money half of a turn: reserve, settle, repair.
//!
//! Split from the turn's execution because it answers to a different clock and
//! a different store. Everything in [`super`] is about getting an answer out of
//! a model within a deadline; everything here is about a durable counter two
//! processes race for, which no deadline may bound and which must be right
//! whether or not the process that opened the grant survived to close it.
//!
//! **The one rule this module exists to keep: a settle reads the log and
//! nothing else.** What a turn is charged is a fact about the turn — its usage,
//! the rate card that was in force when it was routed, who paid, whether any of
//! it was roundhouse's money to price, and whether a budget was in force to
//! charge it against at all — and all five travel in the session's own log, so
//! the process that ran the turn and a successor replaying its log arrive at the
//! same number by construction rather than by both consulting files that may
//! have been edited in between. See
//! [`DecisionRecord::rate_card`](roundhouse_core::routing::DecisionRecord::rate_card),
//! [`DecisionRecord::billing`](roundhouse_core::routing::DecisionRecord::billing)
//! and
//! [`DecisionRecord::budget_draw`](roundhouse_core::routing::DecisionRecord::budget_draw).
//!
//! The rule has no exception now, and it had two until recently. The
//! billed-or-accounted half was read from the live `Admission`, which made a
//! repaired settle disagree with the settle it replaced the moment a project's
//! credential mode was edited — and left the dashboard, which reads only the
//! log, unable to agree with either. The budget itself was read there too,
//! which was worse in kind rather than in degree: a project handed a budget it
//! had never had absorbed the turns that predated the change, because the
//! successor asked the plane in front of it whether they had been budgeted and
//! was told yes about turns that were over.

use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::{GrantRequest, SettledSpend, Settlement, TurnBudget};
use roundhouse_core::ids::ResponseId;
use roundhouse_core::now_ms;
use roundhouse_core::routing::{Candidate, Target};
use roundhouse_core::session::{Session, TerminalSettlement};
use roundhouse_core::store::SessionStore;

use crate::control_config::Admission;
use crate::engine::{Engine, EngineError};

/// How long a grant's hold outlives the turn that took it.
///
/// The turn deadline plus slack, and both halves matter. Shorter than the
/// deadline and a slow turn's own hold would lapse underneath it, to be
/// re-granted to the next turn while the first is still running — the budget
/// spent twice. Much longer and a dead process's reservation would strand its
/// project's money for the difference. There is no sweeper: the next call to
/// touch the project is what expires it.
pub(crate) const GRANT_TTL_SLACK_MS: u64 = 30_000;

impl<S: SessionStore, T: Tokenizer + Clone> Engine<S, T> {
    /// Reserve what this turn may spend, or discover there is nothing to
    /// reserve.
    ///
    /// **An admission with no budget configured never touches the ledger.**
    /// That is the whole of the open-mode cost promise, and it is a skipped
    /// call rather than a very large ceiling: [`TurnBudget::Unlimited`] is a
    /// distinct arm precisely so that "no budget was configured" and "a budget
    /// granted a great deal" cannot be confused for one another by anything
    /// downstream, including a reader of this function.
    ///
    /// The hold is keyed by `ResponseId`, so a turn has exactly one — and a
    /// deduplicated retry never arrives here at all, because
    /// [`Session::begin_turn`] short-circuits before `plan` is reached. A retry
    /// that did open a second grant would reserve a turn's budget against a
    /// turn that is not going to happen, and a client reconnecting through a
    /// flaky link could exhaust a project without spending a cent at any
    /// provider.
    ///
    /// **This is where a ledger outage fails a turn**, and deliberately the
    /// only place: it is inside the dispatch, so the response is already open
    /// and the failure terminates it with a reason a client can read. The
    /// repair below runs before any of that exists and therefore cannot fail a
    /// turn at all — see [`Engine::repair_settle`].
    ///
    /// **A budget over pass-through traffic is not the ceiling it looks like,
    /// and this is the seam where the difference shows.** A grant is opened for
    /// every budgeted turn, whatever the credential mode, and the ceiling it
    /// returns bounds *this* turn: a candidate dearer than what the ledger
    /// granted is inadmissible, so an operator's dollar figure does change
    /// routing. What it never does on a forwarded seat is accumulate — the
    /// settle commits [`SettledSpend::AccountedNotBilled`], the committed total
    /// stays at zero, and the exhaustion arm and the warn threshold are
    /// therefore unreachable. The setting reads as "this project may spend $200"
    /// and behaves as "no single turn of this project may be quoted above $200".
    /// See [`Billing::is_billable`] for the boot-knowable form of
    /// that, and for the one path that still commits on such a project: a
    /// `validate` block's judge authenticates on this deployment's own
    /// transport and its spend is real.
    ///
    /// [`Billing::is_billable`]: roundhouse_core::control::Billing::is_billable
    pub(super) async fn open_grant(
        &self,
        session: &Session<S>,
        response_id: &ResponseId,
        candidates: &[Candidate],
        admission: &Admission,
    ) -> Result<TurnBudget, EngineError> {
        let Some(terms) = &admission.budget else {
            return Ok(TurnBudget::Unlimited);
        };
        let grant = self
            .spend
            .open_grant(GrantRequest {
                principal: admission.principal.clone(),
                session_id: session.session_id().clone(),
                response_id: response_id.clone(),
                requested_usd: admission
                    .policy
                    .dearest_admissible_frontier_usd(candidates, &session.state().frontier_history),
                ttl_ms: self.config.turn_deadline_ms + GRANT_TTL_SLACK_MS,
                terms: terms.clone(),
                now_ms: now_ms(),
            })
            .await?;
        Ok(grant.turn_budget(terms.budget.on_exhaustion))
    }

    /// Apply this session's most recent terminal event to the spend ledger.
    ///
    /// **One function called at two moments, because it is one operation.**
    /// Just after a session is opened it is the *repair*: a process that died
    /// between its log commit and its settle left exactly one turn's spend
    /// unapplied — the last one, since turns are serialized and each settles
    /// before the next is admitted — and re-driving it there rides on the
    /// replay [`Session::open_observed`] has already performed. Just after this
    /// turn's own commit it is the *settle*, and by then the log's last
    /// terminal event is this turn's. Both go through the same operation, which
    /// is idempotent by `(session, seq)`: the repair costs one no-op call when
    /// there was nothing to repair, and the settle costs one no-op call when
    /// the repair already did it.
    ///
    /// One function rather than two because the alternative is two pieces of
    /// code that must agree forever about how a terminal event is priced — and
    /// the day they stop agreeing, a repaired settle charges a different number
    /// than the settle it replaced, which is drift nobody can see without
    /// reading both. That is now more than a convention: both moments price
    /// from [`settled_cost_usd`], which reads the log and has no catalog to
    /// consult, so there is no *input* left for them to disagree about either.
    ///
    /// Not bounded by the turn deadline, deliberately: that deadline exists to
    /// stop a hung *provider* from holding a lease open, and the ledger is this
    /// deployment's own store — the same reasoning that keeps the session
    /// store's appends out from under it.
    ///
    /// **What the live admission still decides is where a charge lands, never
    /// what it is.** The account and the window come from the project's current
    /// terms, because an account is a thing that exists now and a window is a
    /// period that is open now; the dollars come from the log alone. The
    /// early return below is that distinction and not the old one: an
    /// unbudgeted project has no account to talk to, which is the whole of the
    /// open-mode cost promise, while whether *this turn* is charged is
    /// [`TerminalSettlement::budget_draw`]'s answer and no longer this line's.
    pub(super) async fn settle(
        &self,
        session: &Session<S>,
        admission: &Admission,
    ) -> Result<(), EngineError> {
        let Some(terms) = &admission.budget else {
            return Ok(());
        };
        let Some(settlement) = session.state().last_settlement() else {
            return Ok(());
        };
        self.spend
            .settle_grant(Settlement {
                principal: admission.principal.clone(),
                session_id: session.session_id().clone(),
                seq: settlement.seq,
                response_id: settlement.response_id.clone(),
                actual_usd: match settlement.budget_draw {
                    // The log says this turn was decided when the project had
                    // no budget. It owes this window nothing — the window did
                    // not exist when the turn ran — and the settle still runs
                    // rather than returning, because a settle is also what
                    // releases a hold, and a turn that reached no provider
                    // reaches this arm with a grant outstanding.
                    None => 0.0,
                    // The two credential axes meet here and only here — see
                    // [`BudgetCounts::drawn_usd`], which is the one place
                    // `payer` and the accounting-honesty rule are applied, so
                    // two callers cannot apply them in two orders. The basis
                    // comes off the log beside the payer it is applied to: read
                    // from the live admission, it moved a finished turn's
                    // charge the moment an admin switched the project between
                    // the two bases.
                    Some(counts) => {
                        counts.drawn_usd(settlement.payer, self.settled_spend(settlement)?)
                    }
                },
                window: terms.budget.window,
                now_ms: now_ms(),
            })
            .await?;
        Ok(())
    }

    /// What this turn cost, and whether roundhouse may name the number.
    ///
    /// **Both halves come out of the log, and the exception this function used
    /// to carry is gone.** It read the forwarded half from the live
    /// `Admission` on the argument that a project's credential mode is
    /// configuration like the `window` beside it. That argument was wrong in a
    /// way the window's is not: a `DecisionRecord` already records the payer
    /// resolved from that mode, so the mode's consequence for *this turn* was
    /// half in the log and half in a file an operator edits. A project switched
    /// between a stored key and pass-through therefore re-priced every turn a
    /// successor repaired — and the metrics fold, reading the same log, had no
    /// way to agree with either answer.
    ///
    /// [`DecisionRecord::billing`] is that fact, written where the credential
    /// resolves and read here. One decision, one source, and the dashboard
    /// reads it too.
    ///
    /// [`DecisionRecord::billing`]: roundhouse_core::routing::DecisionRecord::billing
    fn settled_spend(&self, settlement: &TerminalSettlement) -> Result<SettledSpend, EngineError> {
        // A seat is a subscription, not a metered rate card: the catalog's
        // per-token price describes what *roundhouse* would have paid on its
        // own key, which is a counterfactual and not a bill. The tokens still
        // land in the fold, which is the honest half.
        Ok(settlement.billing.settled(settled_cost_usd(settlement)?))
    }

    /// The same settle, driven on a log this process did not write — and
    /// **contained, because nothing this early may fail a turn.**
    ///
    /// It runs after the lease is taken and before the turn is admitted, so a
    /// `?` here returns while holding the lease, with no response open to
    /// terminate and nothing to tell the client but a bare error. Worse, it
    /// returns for a reason that is a property of the *log* rather than of the
    /// turn: the same repair is attempted on every open, so one settle this
    /// process cannot apply would fail every turn of that session, forever, on
    /// a deployment whose fleet is perfectly healthy. That was the shape this
    /// containment was written for, and it bricked sessions over a model an
    /// operator had merely stopped offering.
    ///
    /// So a failed repair is a warning and a skip. What that costs is bounded
    /// and visible: one turn's spend stays uncommitted, its hold lapses on the
    /// grant TTL, and the gap shows up as the drift between ledger and log that
    /// the reconciliation view exists to surface. What it does not cost is the
    /// session.
    ///
    /// **A ledger that is genuinely down still fails the turn**, one step
    /// later and in the right place: [`Engine::open_grant`] calls the same
    /// ledger inside the dispatch, where there is a response to terminate and
    /// an `IncompleteReason` to terminate it with. Swallowing the error here
    /// therefore hides nothing — it only declines to be the thing that reports
    /// it.
    pub(super) async fn repair_settle(&self, session: &Session<S>, admission: &Admission) {
        if let Err(error) = self.settle(session, admission).await {
            tracing::warn!(
                session_id = %session.session_id(),
                seq = session.state().last_settlement().map(|s| s.seq),
                %error,
                "the log's last settle could not be repaired; leaving it \
                 unapplied rather than failing the turn"
            );
        }
    }
}

/// What one terminated response is to be charged, in dollars.
///
/// Priced from the usage the log actually holds and never from what was
/// expected, which is what makes an estimate consume budget: a provider that
/// reports nothing gets [`Engine::estimated_usage`] standing in for it, and
/// that estimate is charged exactly as a measurement would be. The
/// alternative — writing an unreported call off as free — would let a provider
/// with unreliable accounting spend a project's whole month without moving its
/// committed total by a cent.
///
/// **Priced from the card the log holds, too**, which is the half that took a
/// bricked session to learn. A free function with no `&self` is the shape that
/// says it: there is no catalog in scope here, so the live one cannot be
/// consulted by accident, and the repair and the settle cannot drift apart
/// because they are reading the same recorded fact.
///
/// Three ways to reach zero, each a statement rather than a fallback: a local
/// dispatch bills capacity and not dollars, a response that carries no target
/// never reached a provider, and a genuinely free rate card is free. A frontier
/// dispatch whose decision recorded no card at all is none of those — see
/// [`EngineError::UnpricedSettlement`].
///
/// **One turn at a time, and the metrics rollup agrees with it by
/// construction.** What this returns is summed into `Account::committed_usd`,
/// while `/v1/metrics` prices the same turns out of its own fold — two routes
/// to one number, and the admin reconciliation view publishes their difference
/// as evidence about *settlement*. They stayed equal only while pricing was
/// linear in tokens, which M11.0's measured cache-write split ended: the fold
/// now accumulates each turn's own split (`routing::PooledUsage`) instead of
/// summing tokens and pricing once, so a drift is again a settle that failed,
/// a restart, or a turn still in flight, and never an artifact of where the
/// arithmetic happened (M11.0 review F2).
pub(super) fn settled_cost_usd(settlement: &TerminalSettlement) -> Result<f64, EngineError> {
    match (&settlement.target, &settlement.rate_card) {
        // Order matters: a local dispatch is free whatever a card says, and a
        // turn that routed nowhere owes nothing whatever it would have cost.
        (None, _) | (Some(Target::Local { .. }), _) => Ok(0.0),
        (Some(_), Some(card)) => Ok(card.price(&settlement.usage)),
        (Some(target), None) => Err(EngineError::UnpricedSettlement(target.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roundhouse_core::control::{Billing, BudgetCounts, Payer};
    use roundhouse_core::event::{Accounting, Usage};
    use roundhouse_core::ids::ResponseId;
    use roundhouse_core::routing::ProviderPricing;

    /// A million output tokens, so a price in dollars reads back as the rate
    /// that produced it.
    fn one_mtok_out() -> Usage {
        Usage {
            input_tokens: 0,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 1_000_000,
            reasoning_tokens: 0,
            accounting: Accounting::Reported,
        }
    }

    fn card(output_per_mtok_usd: f64) -> ProviderPricing {
        ProviderPricing {
            input_per_mtok_usd: 0.0,
            cached_input_per_mtok_usd: 0.0,
            cache_write_per_mtok_usd: 0.0,
            output_per_mtok_usd,
        }
    }

    fn settlement(
        target: Option<Target>,
        rate_card: Option<ProviderPricing>,
    ) -> TerminalSettlement {
        TerminalSettlement {
            response_id: ResponseId::new("resp_1"),
            seq: 7,
            target,
            rate_card,
            // This function's subject is the *price*, which is a question about
            // the card and the target. Who paid is the axis `BudgetCounts`
            // reads, one seam out, and it is asserted there — as is whether the
            // price may be claimed at all, which `Billing` decides, and whether
            // any budget draws on it at all, which `budget_draw` decides.
            payer: Payer::Deployment,
            billing: Billing::Billed,
            budget_draw: Some(BudgetCounts::AllFrontierSpend),
            usage: one_mtok_out(),
            // Never an input to a settle, which is exactly what this fixture's
            // subject is; see the field's own note on why it rides here at all.
            provider_reported_cost_usd: None,
        }
    }

    fn hosted() -> Target {
        Target::Frontier {
            provider: "anthropic".into(),
            model: "claude".into(),
        }
    }

    #[test]
    fn a_settle_is_priced_by_the_card_the_log_holds_and_by_nothing_else() {
        assert_eq!(
            settled_cost_usd(&settlement(Some(hosted()), Some(card(12.0)))).unwrap(),
            12.0
        );
        // The same turn, the same usage, a different recorded card: the number
        // moves with the log and there is nothing else in scope for it to move
        // with. This function takes no `&self` precisely so that the live
        // catalog is not reachable from here even by accident.
        assert_eq!(
            settled_cost_usd(&settlement(Some(hosted()), Some(card(6.0)))).unwrap(),
            6.0
        );
    }

    #[test]
    fn the_three_zeroes_are_statements_and_the_fourth_case_is_an_error() {
        let worker = Target::Local {
            worker_id: 7,
            dp_rank: 0,
            model: "llama".into(),
        };
        assert_eq!(
            settled_cost_usd(&settlement(Some(worker), Some(card(12.0)))).unwrap(),
            0.0,
            "a local dispatch bills capacity and not dollars, whatever card \
             happens to sit beside it"
        );
        assert_eq!(
            settled_cost_usd(&settlement(None, None)).unwrap(),
            0.0,
            "a turn that reached no provider owes nobody anything"
        );
        assert_eq!(
            settled_cost_usd(&settlement(Some(hosted()), Some(card(0.0)))).unwrap(),
            0.0,
            "and a genuinely free rate card is free"
        );

        // The fourth: a hosted dispatch whose decision recorded no card. Free
        // is the one answer this must not give — unpriced frontier traffic
        // booked as a saving is the accounting lie the ledger exists to
        // prevent — so it is loud, and the repair seam is what decides that a
        // pre-M3 log is drift rather than a dead session.
        assert!(matches!(
            settled_cost_usd(&settlement(Some(hosted()), None)),
            Err(EngineError::UnpricedSettlement(target)) if target == hosted()
        ));
    }
}

/// R3 and R4 (M8 thermo-nuclear review): **the live `Admission` cannot move
/// what a finished turn is charged**, which are two findings and one guard.
///
/// The module doc above once claimed a settle was priced from four
/// log-derived inputs and "nothing else" while `Engine::settle` read two
/// more off the caller's `Admission`: whether a budget existed at all (R3),
/// and which [`BudgetCounts`](roundhouse_core::control::BudgetCounts) basis
/// the draw applied (R4). Both fed `Settlement::actual_usd` exactly where
/// `payer` and `billing` used to before this module closed that hole for
/// them.
///
/// R3 was reachable through the admin plane the day it was found — a
/// `None -> Some` budget PATCH, and the next repair absorbed a turn that
/// predated the budget — and its end-to-end proof lives in
/// `tests/admin_api.rs`. R4 was latent, because nothing could make
/// `budget_counts` vary between two settles of one turn while it came from
/// file config alone; it goes live the day a stored user-tier credential is
/// editable through admin-plane CRUD.
///
/// The tests below hold both closed at this seam, where an `Admission`
/// handed to `settle()` twice with different contents is indistinguishable
/// from one `Admission` an operator edited in between. Each varies only the
/// live admission and asserts the charge does not move, and each has a live
/// control that varies the *log* and asserts it does — because a guard that
/// only proved the charge is insensitive to the admission would also pass on
/// a settle that had stopped reading anything at all.
#[cfg(test)]
mod the_live_admission_cannot_move_a_finished_turns_charge {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use roundhouse_core::context::ByteTokenizer;
    use roundhouse_core::control::{
        Allocation, Balance, BalanceQuery, Billing, Budget, BudgetCounts, BudgetTerms,
        BudgetWindow, DEFAULT_WARN_AT, Exhaustion, FairUseTerms, Grant, Payer, Principal, Settled,
        SpendError, SpendLedger, TurnCredentials, TurnPolicy,
    };
    use roundhouse_core::event::{Accounting, Usage};
    use roundhouse_core::ids::{SessionId, TurnId};
    use roundhouse_core::item::Item;
    use roundhouse_core::routing::{AffinityPolicy, CacheLedger, DecisionRecord, ProviderPricing};
    use roundhouse_core::store::MemoryStore;
    use roundhouse_fleet::{EchoFrontierClient, StaticFrontierCatalog};

    use crate::engine::{EchoLocalExecutor, EngineConfig};

    use super::*;

    /// Records what `settle()` sends rather than modeling a real ledger's
    /// state. A real ledger's idempotency-by-`(session, seq)` would hide the
    /// second call's amount behind `applied: false` — which is exactly the
    /// "drift nobody can see" this defect is about, not a property to model
    /// away in the harness that is supposed to surface it.
    #[derive(Default)]
    struct RecordingLedger {
        actual_usd_seen: Mutex<Vec<f64>>,
    }

    #[async_trait]
    impl SpendLedger for RecordingLedger {
        async fn open_grant(&self, _request: GrantRequest) -> Result<Grant, SpendError> {
            unreachable!("R4 exercises settle(), which never opens a grant")
        }

        async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError> {
            self.actual_usd_seen
                .lock()
                .unwrap()
                .push(settlement.actual_usd);
            Ok(Settled {
                applied: true,
                released_usd: 0.0,
                committed_usd: settlement.actual_usd,
            })
        }

        async fn balance(&self, _query: BalanceQuery) -> Result<Balance, SpendError> {
            unreachable!("R4 exercises settle(), which never reads a balance")
        }
    }

    fn frontier_card() -> ProviderPricing {
        ProviderPricing {
            input_per_mtok_usd: 0.0,
            cached_input_per_mtok_usd: 0.0,
            cache_write_per_mtok_usd: 0.0,
            output_per_mtok_usd: 12.0,
        }
    }

    /// A million output tokens against [`frontier_card`], so the settled
    /// price reads back as the rate that produced it: $12.00 flat.
    fn frontier_usage() -> Usage {
        Usage {
            input_tokens: 0,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 1_000_000,
            reasoning_tokens: 0,
            accounting: Accounting::Reported,
        }
    }

    fn budgeted_admission(budget_counts: BudgetCounts) -> Admission {
        Admission {
            principal: Principal::new("acme", "ada"),
            policy: Arc::new(TurnPolicy::unrestricted()),
            budget: Some(BudgetTerms {
                budget: Budget {
                    limit_usd: 1_000.0,
                    window: BudgetWindow::Total,
                    on_exhaustion: Exhaustion::degrade_with_overflow(),
                    warn_at: DEFAULT_WARN_AT,
                },
                allocation: Allocation::Pooled,
            }),
            // These tests are about the *settle*, which fair use does not
            // touch: draws are recorded one seam out, in `run_turn`'s tail,
            // precisely so that a project with windows and no budget is
            // counted at all.
            fair_use: Arc::new(FairUseTerms::default()),
            validation: None,
            credentials: TurnCredentials::unrestricted(),
            budget_counts,
            tiers: None,
        }
    }

    fn engine_over(spend: Arc<dyn SpendLedger>) -> Engine<MemoryStore, ByteTokenizer> {
        Engine::new(
            Arc::new(MemoryStore::new()),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local")),
            StaticFrontierCatalog::new(vec![]),
            Arc::new(EchoFrontierClient::new("frontier")),
            Arc::new(AffinityPolicy::new()),
            EngineConfig::default(),
        )
        .with_spend_ledger(spend)
    }

    /// One logged turn, billed to a member's own credential
    /// (`Payer::User`) and priced by [`frontier_card`] — a
    /// `TerminalSettlement` whose five documented inputs (usage, rate_card,
    /// payer, billing, budget_draw) are fixed the instant this function
    /// returns. Nothing below mutates the log again; every call to `settle()`
    /// against this session reads the identical settlement.
    ///
    /// `logged` is the whole subject: it is the budget situation *the log
    /// records*, which the tests below vary independently of the live
    /// admission's, because the two agreeing is exactly what a settle must
    /// not depend on.
    async fn session_with_one_user_paid_turn(logged: Option<BudgetCounts>) -> Session<MemoryStore> {
        let store = Arc::new(MemoryStore::new());
        let session_id = SessionId::generate();
        store.create_session(&session_id, "affinity").await.unwrap();
        let mut session = Session::open(store, session_id, "node-a", 30_000, CacheLedger::new())
            .await
            .unwrap();

        session
            .record_created("affinity", &Principal::new("acme", "ada"), None)
            .await
            .unwrap();
        let admission = session
            .begin_turn(TurnId::new("t1"), vec![Item::user_text("hi")])
            .await
            .unwrap();
        let response_id = admission.response_id().clone();

        session
            .record_routing(
                &response_id,
                DecisionRecord {
                    chosen: Target::Frontier {
                        provider: "anthropic".into(),
                        model: "claude".into(),
                    },
                    rationale: "test".into(),
                    policy: "affinity".into(),
                    isl_tokens: 100,
                    expected_prefill_tokens: 100.0,
                    expected_cost_usd: 0.0,
                    considered: Vec::new(),
                    turn_policy_digest: String::new(),
                    budget_state: Default::default(),
                    rate_card: Some(frontier_card()),
                    // The axis `BudgetCounts::ProjectPaidOnly` reads: a
                    // member's own credential, not the project's.
                    payer: Payer::User,
                    billing: Billing::Billed,
                    budget_draw: logged,
                    withheld_providers: Vec::new(),
                    declared_baseline: None,
                    attempts: Vec::new(),
                },
            )
            .await
            .unwrap();
        session
            .complete(&response_id, Some("hi"), frontier_usage(), None, None)
            .await
            .unwrap();

        session
    }

    /// Every `actual_usd` one settle of that session sent the ledger, under a
    /// log that says `logged` and a live admission that says `live`.
    ///
    /// A `Vec` rather than the one figure, because "how many times the ledger
    /// was called" is half of what these tests assert: a settle that draws
    /// nothing and a settle that never happened commit the same dollars and
    /// differ in whether the turn's hold is released.
    async fn settled_usd(logged: Option<BudgetCounts>, live: BudgetCounts) -> Vec<f64> {
        let session = session_with_one_user_paid_turn(logged).await;
        let ledger = Arc::new(RecordingLedger::default());
        engine_over(ledger.clone())
            .settle(&session, &budgeted_admission(live))
            .await
            .unwrap();
        ledger.actual_usd_seen.lock().unwrap().clone()
    }

    /// **R4's guard.** One identical logged `TerminalSettlement`, settled
    /// twice under live admissions that disagree about `budget_counts`,
    /// commits the same dollars both times — because the basis the draw is
    /// applied on comes off the log, where the turn recorded it, and the
    /// caller's `Admission` has no say in it.
    ///
    /// This is the assertion that used to fail: `AllFrontierSpend` drew the
    /// full $12.00 the log prices and `ProjectPaidOnly` drew $0.00 for the
    /// same finished turn, because `payer: Payer::User` zeroes it under that
    /// arm. Its partner below is what stops it passing vacuously.
    #[tokio::test]
    async fn a_settle_does_not_move_with_the_live_admissions_budget_counts() {
        let under_all = settled_usd(
            Some(BudgetCounts::AllFrontierSpend),
            BudgetCounts::AllFrontierSpend,
        )
        .await;
        let under_project_paid = settled_usd(
            Some(BudgetCounts::AllFrontierSpend),
            BudgetCounts::ProjectPaidOnly,
        )
        .await;
        assert_eq!(
            under_all, under_project_paid,
            "one logged TerminalSettlement settled twice: actual_usd must not \
             move with admission.budget_counts, which is a fact about the \
             plane in front of this process and not about the turn"
        );
        assert_eq!(
            under_all,
            vec![12.0],
            "and the figure is the one the log's own basis produces: \
             user-paid frontier spend draws in full under AllFrontierSpend"
        );
    }

    /// **The partner that keeps the guard above honest.** Same turn, same
    /// live admission, one thing different: the log says the project drew on
    /// `ProjectPaidOnly` when this turn ran, so the member's own credential
    /// draws nothing — and the live `AllFrontierSpend` admission does not
    /// override it.
    ///
    /// Without this, a settle that had stopped reading the basis at all —
    /// hard-coding `AllFrontierSpend`, say — would satisfy the test above
    /// perfectly. The pair together say the number moves with the log and
    /// with nothing else.
    #[tokio::test]
    async fn a_settle_draws_on_the_basis_the_log_recorded() {
        assert_eq!(
            settled_usd(
                Some(BudgetCounts::ProjectPaidOnly),
                BudgetCounts::AllFrontierSpend,
            )
            .await,
            vec![0.0],
            "the log recorded a project that does not meter its members' own \
             keys, so this turn draws nothing however the live plane reads now"
        );
    }

    /// **R3's guard at this seam**, where its end-to-end proof in
    /// `tests/admin_api.rs` shows the same thing through the admin plane: a
    /// turn the log records as having run under no budget draws nothing from
    /// the budget a project acquired afterwards.
    ///
    /// The call still reaches the ledger, and that is the second assertion
    /// rather than an accident. A settle is also what releases the hold a
    /// grant took out, and a turn that terminated before recording any
    /// decision arrives here with exactly this absent basis — skipping it
    /// would strand that turn's reservation for a whole grant TTL.
    #[tokio::test]
    async fn a_turn_logged_under_no_budget_draws_nothing_from_one_added_later() {
        assert_eq!(
            settled_usd(None, BudgetCounts::AllFrontierSpend).await,
            vec![0.0],
            "the budget was not there when the turn ran, so the turn owes \
             this window nothing -- and the ledger was still called once, \
             which is what releases a hold"
        );
    }

    /// **The harness control.** Same log, same admission, two settles — only
    /// the recording ledger differs. If this went red, every mismatch above
    /// would be something about the fixture (nondeterministic pricing,
    /// mutable session state, ledger flakiness) rather than about the inputs
    /// the tests vary on purpose.
    #[tokio::test]
    async fn control_identical_settlement_agrees_under_one_fixed_budget_counts() {
        let session = session_with_one_user_paid_turn(Some(BudgetCounts::AllFrontierSpend)).await;
        let admission = budgeted_admission(BudgetCounts::AllFrontierSpend);

        let ledger_a = Arc::new(RecordingLedger::default());
        engine_over(ledger_a.clone())
            .settle(&session, &admission)
            .await
            .unwrap();

        let ledger_b = Arc::new(RecordingLedger::default());
        engine_over(ledger_b.clone())
            .settle(&session, &admission)
            .await
            .unwrap();

        let committed_a = ledger_a.actual_usd_seen.lock().unwrap()[0];
        let committed_b = ledger_b.actual_usd_seen.lock().unwrap()[0];
        assert_eq!(
            committed_a, committed_b,
            "one fixed budget_counts must settle one logged turn the same \
             way twice"
        );
        assert_eq!(
            committed_a, 12.0,
            "user-paid frontier spend under AllFrontierSpend draws the full \
             logged price"
        );
    }
}
