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
//! nothing else.** What a turn is charged is a fact about the turn — its usage
//! and the rate card that was in force when it was routed — and both travel in
//! the session's own log, so the process that ran the turn and a successor
//! replaying its log arrive at the same number by construction rather than by
//! both consulting a catalog file that may have been edited in between. See
//! [`DecisionRecord::rate_card`](roundhouse_core::routing::DecisionRecord::rate_card).

use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::{GrantRequest, Settlement, TurnBudget};
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
                actual_usd: settled_cost_usd(settlement)?,
                window: terms.budget.window,
                now_ms: now_ms(),
            })
            .await?;
        Ok(())
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
    use roundhouse_core::event::{Accounting, Usage};
    use roundhouse_core::ids::ResponseId;
    use roundhouse_core::routing::ProviderPricing;

    /// A million output tokens, so a price in dollars reads back as the rate
    /// that produced it.
    fn one_mtok_out() -> Usage {
        Usage {
            input_tokens: 0,
            cached_input_tokens: 0,
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
            usage: one_mtok_out(),
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
