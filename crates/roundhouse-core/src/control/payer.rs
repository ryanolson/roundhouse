// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Who paid for a turn, what that draws, and when roundhouse may name a price.
//!
//! Three types and one question. They live together because the honesty rule
//! that binds them is one rule, and a file boundary between the payer and what
//! the payer's turn may be charged is a file boundary between a decision and
//! the thing it decides.
//!
//! **The rule: never a price roundhouse did not pay.** A turn served through a
//! forwarded seat consumes tokens roundhouse can measure and dollars it cannot
//! name — the seat is a subscription, not a metered rate card, and applying the
//! catalog's per-token price to it would invent a bill nobody issued. So
//! [`SettledSpend`] has two arms, the second of which carries no number, and
//! every path that turns a settled turn into a ledger draw goes through
//! [`BudgetCounts::drawn_usd`].
//!
//! The rule is *decided* once, as [`Billing`], where the credential resolves and
//! before the turn runs — and it is written into the log there, beside
//! [`Payer`]. Everything downstream reads that recorded answer: the settle, the
//! repair a successor drives from the log alone, and the metrics fold behind the
//! dashboard. Two of those three used to answer the question for themselves —
//! the settle from the live admission and the dashboard not at all — and an
//! operator editing one line of the control plane was enough to make the ledger
//! and the dashboard report different money for the same turn.
//!
//! This is the same discipline `spend.rs` states for `NaN`: the failure mode
//! worth engineering against is not a missing number, it is a *confident wrong*
//! one.

use serde::{Deserialize, Serialize};

use super::credential::TurnCredentials;

/// Whose money a turn's dispatch spends.
///
/// Recorded on the [`DecisionRecord`](crate::routing::DecisionRecord) because
/// it is decided where the credential resolves — before `choose()`, at the same
/// seam the candidate set is filtered — and a fact decided there and read at
/// settle time has to travel in the log or the two answers can disagree.
///
/// The default is [`Deployment`](Self::Deployment) and it is the correct
/// reading of a log written before M7 rather than a placeholder: those turns
/// really were paid for with the deployment's own key. Same treatment, and same
/// reason, as
/// [`DecisionRecord::budget_state`](crate::routing::DecisionRecord::budget_state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Payer {
    /// The deployment's own credential, or its own workers.
    ///
    /// Local dispatches are always this: local capacity is the deployment's,
    /// which is literally true and is why a locally routed turn needs no
    /// credential at all.
    #[default]
    Deployment,
    /// A credential attached to the project.
    Project,
    /// A member's own credential — a stored key, or the seat a pass-through
    /// turn forwards. Both are the member's to be billed for, and the
    /// difference between them is whether roundhouse can put a number on it.
    User,
}

/// Whether user-paid spend draws down the project's budget.
///
/// The axis is narrower than the names suggest and the doc has to say so:
/// **what it decides is whether a *member's own* credential draws the project's
/// ceiling.** Deployment-paid spend draws under both, because that is exactly
/// the spend a project budget has metered since M3 — excluding it would zero
/// every existing project's meter the day BYOK is turned on, which is a silent
/// change to what an operator's limit means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetCounts {
    /// Every frontier dollar draws the project ceiling, whoever's key paid.
    ///
    /// The default, because a budget is usually a statement about how much this
    /// project may spend on frontier models at all, not about which card it was
    /// billed to.
    #[default]
    AllFrontierSpend,
    /// Only spend on a credential the project did not have to bring itself.
    ///
    /// For the deployment where a member's key is their own affair: their turns
    /// are attributed and measured, and they do not consume a ceiling somebody
    /// else is managing.
    ProjectPaidOnly,
}

impl BudgetCounts {
    /// What one settled turn draws from the project's ledger.
    ///
    /// **The one place the two axes meet**, so `payer` and the honesty rule
    /// cannot be applied in two different orders by two callers. A settle
    /// passes the result as `Settlement::actual_usd`; zero is an ordinary
    /// answer there and means what it says.
    pub fn drawn_usd(&self, payer: Payer, spend: SettledSpend) -> f64 {
        let SettledSpend::Billed { usd } = spend else {
            // Accounted, not billed: there is no price to draw with. Measured
            // tokens still land in the fold, which is the honest half.
            return 0.0;
        };
        match (self, payer) {
            (BudgetCounts::ProjectPaidOnly, Payer::User) => 0.0,
            _ => usd,
        }
    }
}

/// What a settled turn cost, and whether roundhouse may say so.
///
/// Not an `Option<f64>`: `None` reads as "we failed to price this", which is
/// the error case `EngineError::UnpricedSettlement` already covers and which
/// must stay loud. This is the opposite — a turn priced correctly at *no dollar
/// claim*, because the money was a subscription seat roundhouse never held a
/// rate card for. Two states that both spell `0.0` and mean opposite things is
/// exactly the confusion `committed_usd` and `measured_usd` are kept apart to
/// avoid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SettledSpend {
    /// A price produced by the rate card recorded on the decision.
    Billed { usd: f64 },
    /// Tokens roundhouse measured under a seat it did not price.
    AccountedNotBilled,
}

/// Whether roundhouse may put a price on a turn at all.
///
/// [`SettledSpend`] without the number, which is exactly the half that is
/// knowable *before* the turn runs — and therefore the half that belongs in the
/// log. Recorded on the
/// [`DecisionRecord`](crate::routing::DecisionRecord) beside [`Payer`], for the
/// same reason: a fact decided where the credential resolves and read again at
/// settle time has to travel in the log, or the process that ran the turn, the
/// successor that repairs it and the dashboard that reports it can reach three
/// different answers.
///
/// One decision with one spelling. `SettledSpend::of(credential, usd)` used to
/// be a second, and the engine's settle a third, asked of the *live* admission
/// rather than of the turn: a project switched between a stored key and
/// pass-through re-priced every turn a successor repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    /// A rate card roundhouse holds applied, so the catalog's price is a bill.
    ///
    /// The default, and the correct reading of a log written before this field
    /// existed rather than a placeholder: pass-through did not exist then, so
    /// every one of those turns really was billable. Same treatment, and same
    /// reason, as [`Payer::Deployment`].
    #[default]
    Billed,
    /// Tokens roundhouse measured under a seat it holds no rate card for.
    ///
    /// A pass-through turn. The tokens are real and are counted; the dollars
    /// are a subscription's and are not roundhouse's to name.
    AccountedNotBilled,
}

impl Billing {
    /// The rule, applied where the credential resolves.
    ///
    /// **The admission's mode decides, not the one dispatch's credential**, and
    /// the difference shows on a locally-served turn of a pass-through project.
    /// That turn touches no credential at all — [`TurnCredential::Absent`] —
    /// but the hosted call it displaced would have been billed to the caller's
    /// seat, so crediting this deployment with having saved that money is the
    /// same invented number in the other direction. One question, asked of the
    /// project: *is any of this roundhouse's money?*
    ///
    /// [`TurnCredentials::is_forwarding`] is the whole of it, and it is
    /// deliberately true whether or not a credential was presented: a turn under
    /// a pass-through project is a pass-through turn even when it degraded to
    /// local for want of one.
    pub fn of(credentials: &TurnCredentials) -> Self {
        match credentials.is_forwarding() {
            true => Billing::AccountedNotBilled,
            false => Billing::Billed,
        }
    }

    /// What a turn the rate card prices at `usd` may be settled for.
    ///
    /// The one place a recorded [`Billing`] becomes a [`SettledSpend`], so the
    /// two types cannot come to disagree about which arm a turn is in.
    pub fn settled(self, usd: f64) -> SettledSpend {
        match self {
            Billing::Billed => SettledSpend::Billed { usd },
            Billing::AccountedNotBilled => SettledSpend::AccountedNotBilled,
        }
    }

    /// Whether this turn's money is roundhouse's to name.
    ///
    /// One predicate for the two questions that turn on it, because they are
    /// one question: what a ledger may draw, and what a *saving* may be claimed
    /// against. A counterfactual is only a saving if the money it stands in for
    /// would have been ours.
    ///
    /// **It is also the boot-knowable half of "is this budget inert?".** A
    /// project that forwards its callers' seats bills nothing for its turns, so
    /// a dollar budget over them never commits: it can neither exhaust nor
    /// warn, and every figure it produces stays zero however much traffic the
    /// project serves. What such a budget still does is bound each turn
    /// *individually* — the grant is opened before the choice and a candidate
    /// dearer than the ceiling is inadmissible — so the setting is not ignored,
    /// it is a per-turn price cap that never accumulates. An operator who wrote
    /// "$200 a month" wrote something else.
    ///
    /// That is not the *whole* of the question, which is why it is a predicate
    /// here rather than a boot refusal: a project enrolled in the validate loop
    /// pays for its judge on the deployment's own transport — `TurnCredential::Absent`,
    /// never a forwarded seat — and that spend does commit against the same
    /// budget. So a pass-through project **with** a `validate` block has a live
    /// ceiling, and only one without is provably inert.
    pub fn is_billable(self) -> bool {
        matches!(self, Billing::Billed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::budget::{Allocation, Budget, BudgetWindow, DEFAULT_WARN_AT, Exhaustion};
    use crate::control::credential::Secret;
    use crate::control::spend::{
        BudgetTerms, GrantRequest, MemorySpendLedger, Settlement, SpendLedger,
    };
    use crate::control::{Principal, ProjectId};
    use crate::ids::{ResponseId, SessionId};

    fn terms() -> BudgetTerms {
        BudgetTerms {
            budget: Budget {
                limit_usd: 100.0,
                window: BudgetWindow::Total,
                on_exhaustion: Exhaustion::degrade_with_overflow(),
                warn_at: DEFAULT_WARN_AT,
            },
            allocation: Allocation::Pooled,
        }
    }

    /// Settle one user-paid turn under `counts` and report the project's
    /// committed total.
    ///
    /// The whole seam in one function: price the turn, ask `BudgetCounts` what
    /// that draws, hand the answer to the ledger as `actual_usd`. Stage 2 wires
    /// exactly this between `settled_cost_usd` and `Settlement`.
    async fn committed_after_a_user_paid_turn(counts: BudgetCounts, spend: SettledSpend) -> f64 {
        let ledger = MemorySpendLedger::new();
        let principal = Principal::new(ProjectId::new("acme"), "ada");
        let response_id = ResponseId::new("resp_1");
        let session_id = SessionId::new("acme/ada/s1");

        ledger
            .open_grant(GrantRequest {
                principal: principal.clone(),
                session_id: session_id.clone(),
                response_id: response_id.clone(),
                requested_usd: 4.0,
                ttl_ms: 60_000,
                terms: terms(),
                now_ms: 1_000,
            })
            .await
            .unwrap();

        ledger
            .settle_grant(Settlement {
                principal,
                session_id,
                seq: 3,
                response_id,
                actual_usd: counts.drawn_usd(Payer::User, spend),
                window: BudgetWindow::Total,
                now_ms: 2_000,
            })
            .await
            .unwrap()
            .committed_usd
    }

    #[tokio::test]
    async fn user_paid_spend_draws_the_project_budget_under_all_frontier_spend() {
        let billed = SettledSpend::Billed { usd: 4.0 };

        // PROBE: the default. A member's own key still spends the project's
        // ceiling, because the ceiling is a statement about how much frontier
        // traffic this project may generate at all.
        assert_eq!(
            committed_after_a_user_paid_turn(BudgetCounts::AllFrontierSpend, billed).await,
            4.0
        );

        // CONTROL, and the other direction asserted rather than assumed: under
        // `ProjectPaidOnly` the identical turn draws nothing. Both directions
        // matter -- a knob that only ever tightens is indistinguishable from a
        // knob that does nothing.
        assert_eq!(
            committed_after_a_user_paid_turn(BudgetCounts::ProjectPaidOnly, billed).await,
            0.0
        );

        // CONTROL: the axis is *who paid*, not *which mode*. Deployment- and
        // project-paid spend draws under both, which is what keeps a pre-M7
        // project's meter meaning the same thing after BYOK is switched on.
        for counts in [
            BudgetCounts::AllFrontierSpend,
            BudgetCounts::ProjectPaidOnly,
        ] {
            for payer in [Payer::Deployment, Payer::Project] {
                assert_eq!(
                    counts.drawn_usd(payer, billed),
                    4.0,
                    "{counts:?} must not exempt {payer:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_forwarded_seat_is_accounted_and_never_billed() {
        // The accounting honesty rule, at the ledger: a pass-through turn draws
        // nothing under either setting, because roundhouse holds no rate card
        // for a subscription seat and a price it did not pay is a price it may
        // not claim.
        for counts in [
            BudgetCounts::AllFrontierSpend,
            BudgetCounts::ProjectPaidOnly,
        ] {
            assert_eq!(
                committed_after_a_user_paid_turn(counts, SettledSpend::AccountedNotBilled).await,
                0.0,
                "{counts:?} must not invent a dollar figure for a forwarded seat"
            );
        }

        // Which arm applies is the admission's answer and nobody else's, and it
        // is decided once — before the turn runs — as a `Billing`.
        let stored = TurnCredentials::configured(
            crate::control::CredentialMode::PreferUser,
            [(
                "openai".to_string(),
                Secret::api_key("sk-live-AAAA").unwrap(),
            )]
            .into_iter()
            .collect(),
            Default::default(),
            Default::default(),
        )
        .expect("an ordinary stored key");
        assert_eq!(Billing::of(&stored), Billing::Billed);
        assert_eq!(
            Billing::of(&stored).settled(4.0),
            SettledSpend::Billed { usd: 4.0 }
        );

        let forwarding =
            TurnCredentials::forwarding(crate::control::PresentedCredential::captured(|name| {
                match name {
                    "authorization" => Some("Bearer eyJhbGciOiJub25lIn0.e30.seat".to_string()),
                    _ => None,
                }
            }));
        assert_eq!(
            Billing::of(&forwarding).settled(4.0),
            SettledSpend::AccountedNotBilled,
            "the catalog price of a forwarded turn is a counterfactual, not a bill"
        );
        // And the same project on a turn where no seat arrived at all: still
        // pass-through, still unpriceable. The alternative reading -- deciding
        // per dispatch, off `TurnCredential::is_forwarded` -- would price the
        // local turn that degrade produces as if this deployment had chosen to
        // save the seat's money.
        assert_eq!(
            Billing::of(&TurnCredentials::forwarding(None)),
            Billing::AccountedNotBilled
        );

        // The control that keeps the rule about *forwarding* rather than about
        // absence: an ungated deployment resolves no credential either, and its
        // turns are billed exactly as they were before M7.
        assert_eq!(
            Billing::of(&TurnCredentials::unrestricted()).settled(4.0),
            SettledSpend::Billed { usd: 4.0 }
        );
    }

    /// What a dollar budget over pass-through traffic can and cannot do.
    ///
    /// The combination is accepted by the configuration boundary and reads like
    /// a monthly ceiling. It is not one: nothing a pass-through turn settles
    /// ever commits, so the ledger's total stays at zero and the exhaustion arm
    /// an operator chose is unreachable. Pinned here rather than left as prose,
    /// because "this setting quietly means something else" is the shape of
    /// finding worth a test.
    #[tokio::test]
    async fn a_dollar_budget_over_forwarded_traffic_can_never_commit() {
        let seat = TurnCredentials::forwarding(None);
        assert!(
            !Billing::of(&seat).is_billable(),
            "a project whose turns are all seat-forwarded has an inert ceiling"
        );

        // The ledger says the same thing, over as many turns as anyone cares to
        // serve: `AccountedNotBilled` draws nothing under either `BudgetCounts`,
        // so the committed total a warn threshold and an exhaustion arm both
        // read never moves.
        for counts in [
            BudgetCounts::AllFrontierSpend,
            BudgetCounts::ProjectPaidOnly,
        ] {
            assert_eq!(
                committed_after_a_user_paid_turn(counts, Billing::of(&seat).settled(4.0)).await,
                0.0
            );
        }

        // CONTROL: the same budget on a project that bills is a real ceiling.
        assert!(
            Billing::of(&TurnCredentials::unrestricted()).is_billable(),
            "the check must be about the mode, not about budgets generally"
        );
        assert_eq!(
            committed_after_a_user_paid_turn(
                BudgetCounts::AllFrontierSpend,
                Billing::of(&TurnCredentials::unrestricted()).settled(4.0)
            )
            .await,
            4.0
        );
    }
}
