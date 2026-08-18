// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a turn is allowed to *spend*.
//!
//! The third fact resolved at admission, beside the two [`control`](super)
//! already carries: [`Principal`](super::Principal) says who a turn belongs to,
//! [`TurnPolicy`](super::TurnPolicy) says which targets it may be chosen from,
//! and a [`Budget`] says how much money the project behind it has left. They are
//! three types rather than one because they answer to three different clocks — a
//! principal is fixed for the life of a key, a policy changes when an operator
//! edits a file, and a budget changes on every turn that costs anything.
//!
//! That last clock is why the budget half is split across two modules. This one
//! holds the *configuration* — the ceilings an operator writes down — and
//! [`spend`](super::spend) holds the *ledger* that those ceilings are checked
//! against. Configuration is a fact about a file; committed spend is a fact
//! about a durable counter that two processes race for.
//!
//! Three decisions are load-bearing enough to state before the types.
//!
//! **Both ceilings bind, and the tighter one wins.** A project has a limit and
//! a membership has an [`Allocation`] of it; neither shadows the other. That is
//! deliberately the opposite of the shadowing rule LiteLLM ended up with, which
//! is documented in its issue tracker as a gotcha rather than as a feature: a
//! member cap that silently lifted the project's is a limit an admin believes
//! they have and does not.
//!
//! **Shares may sum past 1.0 and that is not an error.** An [`Allocation`] is a
//! ceiling, not a slice of a partition — three members each allowed "half" the
//! project simply means no one of them may spend more than half, and the
//! project limit still binds all three together. Refusing the configuration
//! would force an admin to re-plan every share every time a member joins, which
//! is exactly the workspace-budget behavior Anthropic's own console settled on.
//! The admin view shows the sum so an over-subscription is visible; validation
//! does not refuse it.
//!
//! **[`Exhaustion`] carries the overflow valve's switch rather than the
//! deployment carrying it.** `DegradeToLocal { overflow_when_local_saturated }`
//! puts the flag inside the arm it is meaningful in, so "overflow on, but the
//! project refuses instead of degrading" is not a state anything can be in —
//! and the configuration boundary's job shrinks to rejecting the *spelling* of
//! that combination in a file, which is a message about a file rather than a
//! consistency check about a struct.

use serde::{Deserialize, Serialize};

/// The default [`Budget::warn_at`]: warn once four fifths of the limit is
/// committed.
///
/// A named constant because the configuration boundary and the runtime default
/// have to agree on it, and because a fifth of a budget is the smallest
/// remaining slice an admin can still act on — raise the limit, or tell the
/// team — before turns start degrading.
pub const DEFAULT_WARN_AT: f64 = 0.8;

/// A project's spending ceiling and what happens when it is reached.
///
/// Absent, in configuration, means *unlimited* — and unlimited is not a budget
/// with a very large limit. The engine skips the ledger entirely when no budget
/// is configured, so the open-mode path costs nothing at all, and every turn it
/// routes is recorded [`BudgetState::Unconstrained`]. A `Budget` value
/// therefore always means a real ceiling somebody wrote down.
#[derive(Debug, Clone, PartialEq)]
pub struct Budget {
    /// Dollars. Validated positive at the configuration boundary — a zero
    /// limit would refuse every turn from boot, which nobody writes on purpose
    /// and which `Exhaustion::Refuse` with a real limit expresses honestly.
    pub limit_usd: f64,
    pub window: BudgetWindow,
    pub on_exhaustion: Exhaustion,
    /// Fraction of `limit_usd` past which grants come back
    /// [`BudgetState::Warned`]. In `(0.0, 1.0]`; see [`DEFAULT_WARN_AT`].
    pub warn_at: f64,
}

impl Budget {
    /// The committed-plus-held level at which grants start warning.
    pub fn warn_level_usd(&self) -> f64 {
        self.limit_usd * self.warn_at
    }
}

/// The period a [`Budget::limit_usd`] applies over.
///
/// Two arms in v1, and the ledger is the only thing that enforces either: the
/// metrics fold cannot window yet (its watermarks cannot be pruned without
/// event-time windowing), so `measured_usd` stays lifetime and the
/// reconciliation view labels its window column as ledger-sourced. Adding a
/// third arm here without that is how the two numbers start disagreeing about
/// which month they are describing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetWindow {
    /// One ceiling for the life of the project.
    Total,
    /// A ceiling that resets at each calendar month boundary, UTC.
    Monthly,
}

/// What a project does once its budget is spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exhaustion {
    /// Keep serving, from our own fleet.
    ///
    /// The default, and nearly free: local candidates are already priced at
    /// zero dollars, so a zero grant excludes every frontier candidate and
    /// admits every local one through the ordinary admissibility predicate.
    /// No branch, no fallback, no special case.
    DegradeToLocal {
        /// Whether a turn may go back to frontier when the local pool cannot
        /// serve it — see [`TurnBudget::overflow_armed`].
        ///
        /// Lives inside this arm because it is meaningless outside it: a
        /// project that refuses on exhaustion never promised local service and
        /// so has no saturation to escape from.
        overflow_when_local_saturated: bool,
    },
    /// Refuse the turn.
    ///
    /// Terminal as
    /// [`IncompleteReason::BudgetExhausted`](crate::event::IncompleteReason::BudgetExhausted),
    /// which keeps the turn retryable — correct for a limit an admin can raise,
    /// and the difference between this and
    /// [`PolicyRefused`](crate::event::IncompleteReason::PolicyRefused), which
    /// the same turn will hit again forever.
    Refuse,
}

impl Exhaustion {
    /// The default for a configured budget that does not say.
    ///
    /// A named constructor rather than a `Default` impl, for the reason
    /// [`Principal::default_open`](super::Principal::default_open) is one:
    /// reaching for the permissive value should be a sentence a reader can
    /// find, and `..Default::default()` is the one spelling of it nobody
    /// notices in review.
    pub fn degrade_with_overflow() -> Self {
        Exhaustion::DegradeToLocal {
            overflow_when_local_saturated: true,
        }
    }

    /// Whether this project promises to keep serving from the local fleet when
    /// its budget is spent — the promise the startup check has to find capacity
    /// for.
    ///
    /// `Refuse` promises nothing, and `DegradeToLocal` with the valve armed
    /// keeps its promise on frontier, so neither needs local capacity to exist.
    /// The one arm that does is degrade-with-the-valve-off, which is exactly
    /// what the boot check refuses in a deployment with no local fleet.
    pub fn promises_local_service(&self) -> bool {
        matches!(
            self,
            Exhaustion::DegradeToLocal {
                overflow_when_local_saturated: false
            }
        )
    }
}

/// One membership's share of its project's budget.
///
/// A ceiling on top of the project's, never a replacement for it: see the
/// module note on why both bind and why shares are allowed to sum past one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Allocation {
    /// No member ceiling — this membership may spend the whole project budget.
    Pooled,
    /// An absolute ceiling in dollars.
    Capped { limit_usd: f64 },
    /// A ceiling expressed as a fraction of the project limit, so it tracks the
    /// project's limit when an admin raises it. In `(0.0, 1.0]`.
    Share { fraction: f64 },
}

impl Allocation {
    /// This membership's own ceiling, given the project's.
    ///
    /// `None` is [`Self::Pooled`] and means "no *second* ceiling", not "no
    /// ceiling": the project's limit still binds, and the ledger takes the
    /// minimum of whatever ceilings it is given.
    pub fn member_ceiling_usd(&self, project_limit_usd: f64) -> Option<f64> {
        match self {
            Allocation::Pooled => None,
            Allocation::Capped { limit_usd } => Some(*limit_usd),
            // Not clamped to the project limit. A share above the project's own
            // ceiling is already harmless — the project limit binds anyway —
            // and clamping here would quietly make two different configurations
            // read back as one in the admin view that is supposed to show the
            // over-subscription.
            Allocation::Share { fraction } => Some(project_limit_usd * fraction),
        }
    }
}

/// The budget situation one turn was dispatched under, as recorded on its
/// [`DecisionRecord`](crate::routing::DecisionRecord).
///
/// Four answers rather than a state plus a flag, because three of the four
/// combinations a flag would allow are lies. "Overflowed while unconstrained"
/// cannot happen — the valve only opens when the grant is zero — and a `bool`
/// beside a three-armed enum makes it a value somebody can construct. Here it
/// is a variant that names its own precondition.
///
/// [`Unconstrained`](Self::Unconstrained) is the [`Default`], which is what
/// makes the `#[serde(default)]` on the record's field read a pre-M3 log
/// correctly: a decision written before budgets existed was taken under no
/// budget, and that is a fact rather than a missing value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetState {
    /// No budget configured, or plenty left.
    #[default]
    Unconstrained,
    /// Past [`Budget::warn_at`] of the binding ceiling. The turn was served
    /// normally; the state is recorded so an admin can see it coming.
    Warned,
    /// Nothing left to grant. Frontier candidates cost more than zero and are
    /// therefore inadmissible; the turn served locally, or was refused.
    Exhausted,
    /// Exhausted, and served on frontier anyway because the local pool could
    /// not take the turn.
    ///
    /// **The overflow valve's mark**, and its own dashboard number: a project
    /// that spent past its limit because its own workers were full has not had
    /// the same month as one that degraded quietly. The overspend settles into
    /// committed spend like any other, so the ledger visibly exceeds its limit
    /// rather than hiding the excess.
    ///
    /// Never returned by the ledger — a [`SpendLedger`](super::spend::SpendLedger)
    /// cannot observe the fleet, so it cannot know this happened. It is
    /// produced at exactly one place, the router's admissibility resolution,
    /// and a contract test pins the ledger's half of that.
    ExhaustedOverflow,
}

impl BudgetState {
    /// Whether the budget had nothing left to grant, whatever the turn then did
    /// about it.
    pub fn is_exhausted(&self) -> bool {
        matches!(
            self,
            BudgetState::Exhausted | BudgetState::ExhaustedOverflow
        )
    }

    /// Whether this turn was re-admitted to frontier by the overflow valve.
    pub fn overflowed(&self) -> bool {
        matches!(self, BudgetState::ExhaustedOverflow)
    }

    /// The second fact a fleet-shaped routing failure has to name.
    ///
    /// When the local pool empties under load *and* the budget is spent, the
    /// blame is the fleet's — load emptied the pool, not the tenant's policy —
    /// but an operator who is told only that will go tuning workers without
    /// noticing that no frontier candidate could have taken their place. Both
    /// facts, one message. Empty for every other state, so the ordinary
    /// busy-fleet error is byte-identical to the one M2 wrote.
    pub fn saturation_note(&self) -> &'static str {
        match self {
            BudgetState::Exhausted | BudgetState::ExhaustedOverflow => {
                "; the budget is also exhausted, so no frontier candidate remained to take their place"
            }
            BudgetState::Unconstrained | BudgetState::Warned => "",
        }
    }
}

/// What the ledger granted this turn, as the router sees it.
///
/// Per-turn data on the [`RoutingContext`](crate::routing::RoutingContext),
/// beside [`FrontierHistory`](super::FrontierHistory) rather than beside
/// [`TurnPolicy`](super::TurnPolicy): a policy is resolved once at admission and
/// stands for the session, while a grant is resolved between `quote` and
/// `choose` on every single turn and is stale by the next one.
///
/// [`Unlimited`](Self::Unlimited) is a distinct arm rather than an infinite
/// ceiling because it records something an infinity would not: that the ledger
/// was never called. An unconfigured deployment does no I/O for budgets at all,
/// and the arm is what makes that visible in the type instead of being a
/// performance note somebody has to trust.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TurnBudget {
    /// No budget configured. Nothing was granted because nothing was asked.
    Unlimited,
    Granted {
        /// The most this turn may spend, in dollars. Zero when exhausted.
        ceiling_usd: f64,
        state: BudgetState,
        /// The project's exhaustion behavior, carried so the two questions
        /// below have one answer each rather than two callers each re-deriving
        /// them from configuration the router does not otherwise hold.
        on_exhaustion: Exhaustion,
    },
}

impl TurnBudget {
    /// What a turn gets once its project's limit is spent: nothing.
    ///
    /// Named because the startup promise-keeping check needs to ask *the
    /// router's own question* — "with this budget spent, is anything this key
    /// may reach still admissible?" — and the honest way to ask it is to build
    /// the budget that turn will get and call [`Self::admits`]. The dishonest
    /// way is to spell the predicate out again as `expected_cost_usd <= 0.0`
    /// at the check site, which is a second definition of admissibility that
    /// keeps compiling after this one changes.
    ///
    /// Not a value the request path ever constructs: there, a ceiling of zero
    /// comes from the ledger having nothing left to grant, and inventing one
    /// would be a second answer to what the ledger already said.
    pub fn exhausted(on_exhaustion: Exhaustion) -> Self {
        TurnBudget::Granted {
            ceiling_usd: 0.0,
            state: BudgetState::Exhausted,
            on_exhaustion,
        }
    }

    /// **The budget axis of admissibility**: can this turn afford `candidate`?
    ///
    /// The whole of the degrade-to-local behavior, and the reason it needs no
    /// branch anywhere else. Local candidates are priced at `0.0`
    /// (`roundhouse-fleet/src/local.rs`), so a zero ceiling admits every local
    /// candidate and excludes every frontier one through this one comparison.
    ///
    /// A candidate the *ledger* refuses is not unreachable — the next turn, or
    /// the next month, may well afford it — which is why this is an `admits`
    /// axis and not a `permits` one, and why a budget-excluded frontier model
    /// stays in `considered` and its counterfactual saving stays true.
    pub fn admits(&self, candidate: &crate::routing::Candidate) -> bool {
        match self {
            TurnBudget::Unlimited => true,
            TurnBudget::Granted { ceiling_usd, .. } => candidate.expected_cost_usd <= *ceiling_usd,
        }
    }

    pub fn state(&self) -> BudgetState {
        match self {
            TurnBudget::Unlimited => BudgetState::Unconstrained,
            TurnBudget::Granted { state, .. } => *state,
        }
    }

    /// Whether the overflow valve may re-admit frontier candidates for this
    /// turn.
    ///
    /// Armed only when the budget is actually spent *and* the project asked for
    /// the valve. Derived from the two fields rather than stored beside them,
    /// so "armed but not exhausted" is not a value that exists.
    pub fn overflow_armed(&self) -> bool {
        matches!(
            self,
            TurnBudget::Granted {
                state: BudgetState::Exhausted,
                on_exhaustion: Exhaustion::DegradeToLocal {
                    overflow_when_local_saturated: true
                },
                ..
            }
        )
    }

    /// Whether this turn must be refused outright rather than routed.
    ///
    /// Asked by the engine at the grant seam, before `choose` is reached: a
    /// refusal is a terminal log fact with its own
    /// [`IncompleteReason`](crate::event::IncompleteReason), which is the
    /// session layer's business and not the router's. A refusing budget
    /// therefore never reaches a [`RoutingContext`] at all — and if one ever
    /// did, [`Self::admits`] would still exclude frontier and the turn would
    /// serve locally, which is wrong but not unsafe.
    pub fn refuses(&self) -> bool {
        matches!(
            self,
            TurnBudget::Granted {
                state: BudgetState::Exhausted,
                on_exhaustion: Exhaustion::Refuse,
                ..
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{Candidate, Target};

    fn candidate(cost_usd: f64) -> Candidate {
        Candidate {
            target: if cost_usd == 0.0 {
                Target::Local {
                    worker_id: 1,
                    dp_rank: 0,
                    model: "llama".into(),
                }
            } else {
                Target::Frontier {
                    provider: "anthropic".into(),
                    model: "claude".into(),
                }
            },
            expected_prefill_tokens: 1_000.0,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 100.0,
            expected_cost_usd: cost_usd,
            quality_prior: 0.9,
            load: None,
        }
    }

    #[test]
    fn a_member_ceiling_is_absolute_or_a_fraction_of_the_project_limit() {
        assert_eq!(Allocation::Pooled.member_ceiling_usd(100.0), None);
        assert_eq!(
            Allocation::Capped { limit_usd: 25.0 }.member_ceiling_usd(100.0),
            Some(25.0)
        );
        assert_eq!(
            Allocation::Share { fraction: 0.25 }.member_ceiling_usd(100.0),
            Some(25.0)
        );
        // A share tracks the project limit rather than freezing a dollar
        // figure, which is the whole reason it is a separate arm from `Capped`.
        assert_eq!(
            Allocation::Share { fraction: 0.25 }.member_ceiling_usd(400.0),
            Some(100.0)
        );
        // Shares are ceilings, not a partition: three halves is a configuration
        // an admin may write, and the project limit is what stops all three.
        let shares = [Allocation::Share { fraction: 0.5 }; 3];
        let sum: f64 = shares
            .iter()
            .map(|a| a.member_ceiling_usd(100.0).unwrap())
            .sum();
        assert_eq!(
            sum, 150.0,
            "over-subscription is representable, not refused"
        );
    }

    #[test]
    fn a_zero_ceiling_admits_local_and_excludes_frontier_through_one_comparison() {
        let spent = TurnBudget::Granted {
            ceiling_usd: 0.0,
            state: BudgetState::Exhausted,
            on_exhaustion: Exhaustion::degrade_with_overflow(),
        };
        assert!(
            spent.admits(&candidate(0.0)),
            "local is priced at zero, so an exhausted budget still affords it"
        );
        assert!(!spent.admits(&candidate(0.01)));

        // The control: the same two candidates under a budget with room, and
        // under no budget at all.
        let funded = TurnBudget::Granted {
            ceiling_usd: 0.5,
            state: BudgetState::Unconstrained,
            on_exhaustion: Exhaustion::degrade_with_overflow(),
        };
        assert!(funded.admits(&candidate(0.01)));
        assert!(!funded.admits(&candidate(5.0)), "the ceiling is a ceiling");
        assert!(TurnBudget::Unlimited.admits(&candidate(5.0)));
    }

    #[test]
    fn the_valve_and_the_refusal_are_each_one_state_and_nothing_else() {
        let armed = TurnBudget::Granted {
            ceiling_usd: 0.0,
            state: BudgetState::Exhausted,
            on_exhaustion: Exhaustion::degrade_with_overflow(),
        };
        assert!(armed.overflow_armed());
        assert!(!armed.refuses());

        let valve_off = TurnBudget::Granted {
            ceiling_usd: 0.0,
            state: BudgetState::Exhausted,
            on_exhaustion: Exhaustion::DegradeToLocal {
                overflow_when_local_saturated: false,
            },
        };
        assert!(!valve_off.overflow_armed());
        assert!(!valve_off.refuses());

        let refusing = TurnBudget::Granted {
            ceiling_usd: 0.0,
            state: BudgetState::Exhausted,
            on_exhaustion: Exhaustion::Refuse,
        };
        assert!(refusing.refuses());
        assert!(
            !refusing.overflow_armed(),
            "a project that refuses never promised local service, so it has no \
             saturation to escape from"
        );

        // The controls: neither question can be true short of exhaustion, on
        // any exhaustion setting.
        for on_exhaustion in [
            Exhaustion::degrade_with_overflow(),
            Exhaustion::Refuse,
            Exhaustion::DegradeToLocal {
                overflow_when_local_saturated: false,
            },
        ] {
            for state in [BudgetState::Unconstrained, BudgetState::Warned] {
                let live = TurnBudget::Granted {
                    ceiling_usd: 1.0,
                    state,
                    on_exhaustion,
                };
                assert!(!live.overflow_armed());
                assert!(!live.refuses());
            }
        }
        assert!(!TurnBudget::Unlimited.overflow_armed());
        assert!(!TurnBudget::Unlimited.refuses());
        assert_eq!(TurnBudget::Unlimited.state(), BudgetState::Unconstrained);
    }

    #[test]
    fn the_named_exhausted_budget_answers_the_same_question_the_router_will() {
        // The startup check's fixture, and the reason it is a constructor
        // rather than a comparison spelled out at the check site: what a boot
        // check predicts about an exhausted project and what the router then
        // does have to be the same predicate, or a deployment boots on a
        // promise it cannot keep.
        let spent = TurnBudget::exhausted(Exhaustion::DegradeToLocal {
            overflow_when_local_saturated: false,
        });
        assert!(spent.admits(&candidate(0.0)));
        assert!(!spent.admits(&candidate(0.000_01)));
        assert_eq!(spent.state(), BudgetState::Exhausted);
        assert!(
            !spent.overflow_armed(),
            "the arm the check refuses over is the one with no valve behind it"
        );
    }

    #[test]
    fn only_a_degrade_mode_with_the_valve_off_promises_local_service() {
        // The startup check's question. A deployment with no local capacity can
        // still keep the other two promises: `Refuse` never made one, and the
        // valve serves the saturated case on frontier.
        assert!(
            Exhaustion::DegradeToLocal {
                overflow_when_local_saturated: false
            }
            .promises_local_service()
        );
        assert!(!Exhaustion::degrade_with_overflow().promises_local_service());
        assert!(!Exhaustion::Refuse.promises_local_service());
    }

    #[test]
    fn a_budget_state_round_trips_and_an_absent_one_reads_unconstrained() {
        for (state, wire) in [
            (BudgetState::Unconstrained, "\"unconstrained\""),
            (BudgetState::Warned, "\"warned\""),
            (BudgetState::Exhausted, "\"exhausted\""),
            (BudgetState::ExhaustedOverflow, "\"exhausted_overflow\""),
        ] {
            assert_eq!(serde_json::to_string(&state).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<BudgetState>(wire).unwrap(),
                state,
                "a state that did not survive the round trip would re-label \
                 every overflow in a replayed log"
            );
        }
        assert_eq!(BudgetState::default(), BudgetState::Unconstrained);

        // Overflow implies exhaustion, and that is what the fourth variant buys
        // over a flag: there is no value in this type meaning "overflowed while
        // unconstrained".
        assert!(BudgetState::ExhaustedOverflow.is_exhausted());
        assert!(BudgetState::ExhaustedOverflow.overflowed());
        assert!(BudgetState::Exhausted.is_exhausted());
        assert!(!BudgetState::Exhausted.overflowed());
        assert!(!BudgetState::Warned.is_exhausted());
    }

    #[test]
    fn only_an_exhausted_budget_adds_its_coincidence_to_a_fleet_shaped_failure() {
        assert_eq!(BudgetState::Unconstrained.saturation_note(), "");
        assert_eq!(
            BudgetState::Warned.saturation_note(),
            "",
            "a warned budget did not empty anything, so it has nothing to add"
        );
        assert!(BudgetState::Exhausted.saturation_note().contains("budget"));
    }

    #[test]
    fn the_warn_level_is_a_fraction_of_the_limit() {
        let budget = Budget {
            limit_usd: 50.0,
            window: BudgetWindow::Monthly,
            on_exhaustion: Exhaustion::degrade_with_overflow(),
            warn_at: DEFAULT_WARN_AT,
        };
        assert_eq!(budget.warn_level_usd(), 40.0);
    }
}
