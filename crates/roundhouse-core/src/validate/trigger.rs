// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! When to ask, and — far more often — when not to.
//!
//! A budget gate **conjoined with** a signal, never a cadence alone. The
//! cascade literature is unanimous that validating on elapsed time has
//! *negative* expected benefit: "check more when things look fine" pays the
//! disruption cost of every unnecessary interruption to buy nothing, and the
//! measured effect reverses. So a cadence that has come due is permission to
//! ask, and evidence of trouble is the reason to.
//!
//! Everything here is computable from the session's own projection with no
//! model call and no clock beyond the one the log carries. That is not a
//! performance property, it is the reason the trigger can be *tested*: a
//! trigger that needed the wall clock or a network would be a trigger whose
//! behavior a suite could only approximate.
//!
//! ## What the gate is, and why each half is where it is
//!
//! The **gate** (all must hold) is about budget and hysteresis — the parts a
//! deployment sets and then stops thinking about:
//!
//! - `tokens_since_last_validation >= T`. Tokens, not turns: roundhouse prices
//!   every turn exactly, so a validator budgeted as a fraction of the spend
//!   since the last check is self-scaling — a session of one-line questions
//!   pays for far fewer checks than a session grinding through a large
//!   codebase, without anybody tuning a per-workload cadence.
//! - Never turn 0. There is no trajectory to judge before there is a
//!   trajectory.
//! - The cooldown has elapsed.
//! - `consecutive_interventions < cap`.
//! - **The claimed suffix does not fulfil an open steer.** The hysteresis that
//!   stops a steer re-triggering the validation that emitted it: the turn that
//!   answers a correction looks, to every signal here, exactly like the turn
//!   that provoked it.
//! - A per-session cap on how many validations one conversation may buy.
//!
//! The **signals** (at least one) are about evidence, and they are behind a
//! trait rather than a match so that a fifth — prompt-stability collapse,
//! which is computable with no model call and orthogonal to these four —
//! slots in additively without the gate being touched.
//!
//! Two candidate signals are excluded from v1 *by evidence*, and the exclusions
//! are as load-bearing as the inclusions. Model-judged semantic drift is the
//! thing being triggered, and its first flag lands at a median 83–84% of
//! trajectory elapsed, which makes it an autopsy. Confidence thresholds are
//! miscalibrated, need per-pair per-domain retuning, and carry no formal bound.

use serde::{Deserialize, Serialize};

use crate::session::SessionState;
use crate::validate::exchange::{Exchange, exchanges};

/// Which of the trigger's observations fired.
///
/// A closed vocabulary rather than a free string, because a dashboard groups
/// on it and an operator tuning the trigger has to be able to ask "how often
/// does this one fire, and how often was it right".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    /// The same call, the same arguments, and the same answer, repeatedly.
    NoProgressRepeat,
    /// Two tools alternating with nothing else between them.
    PingPong,
    /// Consecutive tool calls all returning failures.
    ToolFailureStreak,
    /// A turn far outside this session's own trailing distribution.
    CostAnomaly,
}

/// One observation, and the sentence that states it as a fact.
///
/// The sentence is built here rather than at render time because it is the
/// signal that knows *what it saw* — the count, the tool name, the window. A
/// brief that re-derived the wording from the kind alone would either lose the
/// numbers or need the evidence a second time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalFired {
    pub kind: SignalKind,
    /// A statement of what was observed, in the indicative.
    ///
    /// **Never a suggestion.** "This call has produced identical output four
    /// times" is a fact the judge weighs; "this looks like a loop, consider
    /// escalating" is roundhouse asking the judge to agree with it, and a judge
    /// that agrees with the trigger is an expensive way to re-read the trigger.
    pub fact: String,
}

/// Why a validation was consulted, recorded beside its outcome.
///
/// Kept whole in the log rather than reduced to a boolean, because the arm
/// comparison the whole design exists for is "did acting on *this kind* of
/// evidence help", and a log that recorded only "the trigger fired" cannot
/// answer it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerRecord {
    pub turn_index: u64,
    pub tokens_since_last_validation: u64,
    /// Non-empty by construction: [`Trigger::evaluate`] is the only producer
    /// and it returns `None` when nothing fired. A cadence alone never
    /// reaches here.
    pub signals: Vec<SignalFired>,
}

impl TriggerRecord {
    pub fn new(
        turn_index: u64,
        tokens_since_last_validation: u64,
        signals: Vec<SignalFired>,
    ) -> Self {
        Self {
            turn_index,
            tokens_since_last_validation,
            signals,
        }
    }

    /// The observed facts, for the brief and for a rendered directive.
    pub fn facts(&self) -> impl Iterator<Item = &str> {
        self.signals.iter().map(|signal| signal.fact.as_str())
    }
}

/// What the signals read.
///
/// Prepared once and shared, so four signals are four questions about one
/// picture rather than four walks that could disagree about it. Also what makes
/// a signal testable on its own: a case is a handful of exchanges, not a
/// synthetic event log.
pub struct Evidence<'a> {
    pub exchanges: Vec<Exchange>,
    /// Billable tokens per completed turn, oldest first.
    pub turn_tokens: &'a [u64],
}

impl<'a> Evidence<'a> {
    pub fn of(state: &'a SessionState) -> Self {
        Self {
            exchanges: exchanges(&state.items),
            turn_tokens: state.recent_turn_tokens(),
        }
    }
}

/// One piece of evidence that a session is in trouble.
///
/// A trait with one method rather than an enum with four arms, and the
/// difference is what the plan calls out: a deployment that wants the
/// prompt-stability signal, or an experiment that wants a signal disabled,
/// changes the *list* and not the gate. An enum would put every future signal
/// inside the one function every existing signal already passes through.
pub trait Signal: Send + Sync {
    fn kind(&self) -> SignalKind;
    /// The fact this signal found, or `None` when it is quiet.
    fn detect(&self, evidence: &Evidence<'_>) -> Option<String>;
}

/// The same call, the same arguments, and the same answer, repeatedly.
///
/// **Result-aware, and that is the whole of it.** The same input with a
/// *different* output is progress: a poll that returns a changing status, a
/// test run whose failures are shrinking, a directory listing after a write.
/// A repeat detector that compared only `(name, arguments)` would fire on all
/// of those, and firing on progress is how a validator becomes a tax.
#[derive(Debug, Clone, Copy)]
pub struct NoProgressRepeat {
    pub occurrences: usize,
    /// How many trailing exchanges to look across.
    ///
    /// A window rather than strict adjacency, because the most common form of
    /// this failure has the agent editing something unrelated between attempts
    /// — the repeat is what stays the same, not what is next to what.
    pub window: usize,
}

impl Signal for NoProgressRepeat {
    fn kind(&self) -> SignalKind {
        SignalKind::NoProgressRepeat
    }

    fn detect(&self, evidence: &Evidence<'_>) -> Option<String> {
        let window: Vec<&Exchange> = evidence
            .exchanges
            .iter()
            .rev()
            .take(self.window)
            .rev()
            .collect();
        let latest = window.last()?;
        // An unanswered call is not a repeat of anything yet: the output is
        // half the comparison and it has not arrived.
        let output = latest.output_hash()?;
        let identical = window
            .iter()
            .filter(|call| {
                call.name == latest.name
                    && call.arguments == latest.arguments
                    && call.output_hash().as_deref() == Some(output.as_str())
            })
            .count();
        (identical >= self.occurrences).then(|| {
            format!(
                "the call `{}` has produced identical output {identical} times in the \
                 last {} tool calls",
                latest.name,
                window.len()
            )
        })
    }
}

/// Two tools alternating with nothing else between them.
#[derive(Debug, Clone, Copy)]
pub struct PingPong {
    /// How many complete A-B cycles the alternation must run for.
    pub cycles: usize,
}

impl Signal for PingPong {
    fn kind(&self) -> SignalKind {
        SignalKind::PingPong
    }

    fn detect(&self, evidence: &Evidence<'_>) -> Option<String> {
        let span = self.cycles.checked_mul(2)?;
        if span < 4 || evidence.exchanges.len() < span {
            return None;
        }
        let names: Vec<&str> = evidence.exchanges[evidence.exchanges.len() - span..]
            .iter()
            .map(|call| call.name.as_str())
            .collect();
        let (first, second) = (names[0], names[1]);
        if first == second {
            return None;
        }
        let alternates = names
            .iter()
            .enumerate()
            .all(|(index, name)| *name == if index % 2 == 0 { first } else { second });
        alternates.then(|| {
            format!("the last {span} tool calls alternate between `{first}` and `{second}`")
        })
    }
}

/// Consecutive tool calls all returning failures.
#[derive(Debug, Clone, Copy)]
pub struct ToolFailureStreak {
    pub length: usize,
}

impl Signal for ToolFailureStreak {
    fn kind(&self) -> SignalKind {
        SignalKind::ToolFailureStreak
    }

    fn detect(&self, evidence: &Evidence<'_>) -> Option<String> {
        if self.length == 0 || evidence.exchanges.len() < self.length {
            return None;
        }
        let tail = &evidence.exchanges[evidence.exchanges.len() - self.length..];
        // Answered *and* failed. An unanswered call is not a failure — the most
        // recent call of a turn still in flight would otherwise end every
        // streak in a fire.
        tail.iter()
            .all(|call| call.output.is_some() && call.failed)
            .then(|| format!("the last {} tool calls all returned failures", self.length))
    }
}

/// A turn far outside this session's own trailing distribution.
///
/// **Against the session's own history and nobody else's.** A deployment-wide
/// threshold would fire on every session whose work is simply larger than
/// average, which is a description of the workloads worth serving. This is the
/// signal no published monitor has, because no published monitor knows what
/// each turn cost at the moment it is deciding whether to look.
///
/// Computed on **billable tokens** rather than on dollars, and that is a
/// deliberate narrowing rather than an approximation of one. The metrics fold
/// keeps prices out of accumulation on purpose — prices are configuration and
/// they change, tokens are facts — and a trigger that priced a turn would have
/// to pick a rate card, which is the one input a replay cannot reproduce. A
/// rate card is linear in tokens, so an outlier in tokens against the same
/// target is an outlier in dollars; where the target changed, the two diverge,
/// and mixing them is exactly the laundering a price-free fold refuses.
#[derive(Debug, Clone, Copy)]
pub struct CostAnomaly {
    /// Prior turns needed before a distribution is a distribution.
    pub min_samples: usize,
    /// How many times the trailing median counts as anomalous.
    pub multiple: f64,
}

impl Signal for CostAnomaly {
    fn kind(&self) -> SignalKind {
        SignalKind::CostAnomaly
    }

    fn detect(&self, evidence: &Evidence<'_>) -> Option<String> {
        let (latest, prior) = evidence.turn_tokens.split_last()?;
        if prior.len() < self.min_samples {
            return None;
        }
        // The median, not the mean: one earlier spike must not raise the bar
        // so far that the next one is invisible, which is precisely the shape
        // a session in trouble produces.
        let mut sorted = prior.to_vec();
        sorted.sort_unstable();
        let median = sorted[sorted.len() / 2];
        if median == 0 {
            return None;
        }
        ((*latest as f64) >= self.multiple * median as f64).then(|| {
            format!(
                "the most recent turn billed {latest} tokens against this session's \
                 trailing median of {median}"
            )
        })
    }
}

/// The signals a deployment gets without choosing any.
///
/// Thresholds are configuration, not measurement, and are stated at one site
/// rather than as literals inside each signal so that a deployment tuning them
/// has one place to look. They are chosen from the cost of being wrong: every
/// one of them is conjoined with a budget gate, so a signal that is slightly
/// too eager buys a check the gate had already paid for, while one that is too
/// shy buys nothing at all.
pub fn default_signals() -> Vec<Box<dyn Signal>> {
    vec![
        Box::new(NoProgressRepeat {
            occurrences: 3,
            window: 8,
        }),
        Box::new(PingPong { cycles: 3 }),
        Box::new(ToolFailureStreak { length: 3 }),
        Box::new(CostAnomaly {
            min_samples: 4,
            multiple: 3.0,
        }),
    ]
}

/// The budget half of the trigger.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TriggerConfig {
    /// Tokens a session must bill between one validation and the next.
    pub tokens_between_validations: u64,
    /// Wall-clock quiet period, measured on the log's own timestamps.
    pub cooldown_ms: u64,
    /// How many consecutive intervening validations before the trigger stops.
    pub max_consecutive_interventions: u32,
    /// How many validations one conversation may buy in its lifetime.
    ///
    /// The per-session half of the review budget. The other half — how many
    /// consults one *node* may have outstanding, and how many failures it
    /// tolerates before it stops asking — cannot live here, because it is a
    /// fact about concurrency and this type is a projection of one log. See
    /// [`ReviewBudget`](crate::validate::ReviewBudget).
    pub max_validations_per_session: u32,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            tokens_between_validations: 20_000,
            cooldown_ms: 60_000,
            max_consecutive_interventions: 2,
            max_validations_per_session: 8,
        }
    }
}

/// The trigger: a gate, a list of signals, and nothing else.
pub struct Trigger {
    config: TriggerConfig,
    signals: Vec<Box<dyn Signal>>,
}

impl Trigger {
    pub fn new(config: TriggerConfig, signals: Vec<Box<dyn Signal>>) -> Self {
        Self { config, signals }
    }

    pub fn config(&self) -> &TriggerConfig {
        &self.config
    }

    /// Whether this turn is worth asking about, and what fired.
    ///
    /// **Pure, and takes no clock.** The cooldown is measured against
    /// [`SessionState::last_event_at_ms`], which is the timestamp the store
    /// stamped on the turn's own admission — already in the log by the time
    /// this runs. Taking `now` as an argument would make a replay's answer
    /// depend on when the replay happened, and a trigger whose answer moved on
    /// replay would put the fold and the log into disagreement about whether a
    /// session was ever validated.
    ///
    /// **Takes no [`TurnPolicy`](crate::control::TurnPolicy) either**, and its
    /// absence is deliberate rather than an omission: nothing in the gate reads
    /// one, and a parameter nothing reads is a claim ("whether to validate is
    /// subject to the policy") that nothing enforces — the same argument the
    /// interjection seam makes about supplying a field before an occupant
    /// consults it. The policy's real consumer is the action map, which needs
    /// it as the ceiling every narrowing composes through.
    pub fn evaluate(&self, state: &SessionState) -> Option<TriggerRecord> {
        if !self.gate_open(state) {
            return None;
        }
        let evidence = Evidence::of(state);
        let signals: Vec<SignalFired> = self
            .signals
            .iter()
            .filter_map(|signal| {
                signal.detect(&evidence).map(|fact| SignalFired {
                    kind: signal.kind(),
                    fact,
                })
            })
            .collect();
        // The conjunction, and the one line the whole module is about: an open
        // gate with nothing behind it is not a reason to spend money on a
        // judge.
        if signals.is_empty() {
            return None;
        }
        Some(TriggerRecord::new(
            state.turn_index,
            state.tokens_since_last_validation(),
            signals,
        ))
    }

    /// Every budget and hysteresis condition, all of which must hold.
    fn gate_open(&self, state: &SessionState) -> bool {
        // Turn 0 has no trajectory. The turn index is incremented by the
        // admission of the turn being decided, so the first turn of a session
        // reads 1 here.
        if state.turn_index <= 1 {
            return false;
        }
        if state.this_turn_fulfilled_a_steer() {
            return false;
        }
        if state.consecutive_interventions() >= self.config.max_consecutive_interventions {
            return false;
        }
        if state.validations_run() >= self.config.max_validations_per_session {
            return false;
        }
        if state.tokens_since_last_validation() < self.config.tokens_between_validations {
            return false;
        }
        match state.last_validation_at_ms() {
            // Saturating: a store whose clock stepped backwards between two
            // events would otherwise wrap and read as an elapsed eternity.
            Some(last) => state.last_event_at_ms().saturating_sub(last) >= self.config.cooldown_ms,
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{Item, ItemContent, Role};

    fn call(call_id: &str, name: &str, arguments: &str) -> Item {
        Item::tool_call(call_id, name, arguments)
    }

    fn result(call_id: &str, output: &str) -> Item {
        Item {
            role: Role::Tool,
            content: ItemContent::ToolResult {
                call_id: call_id.into(),
                output: output.into(),
            },
            response_id: None,
        }
    }

    /// A session whose gate is wide open, so a test can be about one thing.
    ///
    /// The fields are set directly rather than through a fold of synthetic
    /// events, which is what keeps these tests about the *trigger*: the fold
    /// that produces them is pinned separately, in `session::tests`, and a
    /// trigger test that had to build a log would fail for either reason.
    fn wide_open(items: Vec<Item>) -> SessionState {
        let mut state = SessionState::default();
        state.items = items;
        state.turn_index = 9;
        state.tokens_since_last_validation = 200_000;
        state.last_event_at_ms = 5_000_000;
        state
    }

    /// The four identical failing runs every gate test needs behind it.
    fn stuck_items() -> Vec<Item> {
        let mut items = Vec::new();
        for n in 0..4 {
            items.push(call(&format!("s{n}"), "pytest", r#"{"path":"tests/"}"#));
            items.push(result(&format!("s{n}"), "ImportError: no module named app"));
        }
        items
    }

    fn evidence_of<'a>(items: &'a [Item], turn_tokens: &'a [u64]) -> Evidence<'a> {
        Evidence {
            exchanges: exchanges(items),
            turn_tokens,
        }
    }

    /// The sharpest of the four, and the one a naive implementation gets wrong.
    #[test]
    fn a_repeat_with_a_different_output_does_not_fire() {
        let signal = NoProgressRepeat {
            occurrences: 3,
            window: 8,
        };

        // Probe: the same call, the same arguments, the same answer, four
        // times. This is the pattern the literature calls the most common way
        // an agent run dies.
        let mut stuck = Vec::new();
        for n in 0..4 {
            stuck.push(call(&format!("c{n}"), "pytest", r#"{"path":"tests/"}"#));
            stuck.push(result(&format!("c{n}"), "ImportError: no module named app"));
        }
        let fact = signal
            .detect(&evidence_of(&stuck, &[]))
            .expect("four identical results is the loop");
        assert!(fact.contains("identical output 4 times"), "{fact}");
        assert!(
            !fact.contains("consider") && !fact.contains("should"),
            "a fact, never a suggestion: {fact}"
        );

        // Control: the identical call four times with an answer that keeps
        // changing. This is a poll, a shrinking test failure, a directory
        // listing after a write — progress, and a validator that taxed it
        // would be a validator nobody leaves on.
        let mut progressing = Vec::new();
        for n in 0..4 {
            progressing.push(call(&format!("c{n}"), "pytest", r#"{"path":"tests/"}"#));
            progressing.push(result(&format!("c{n}"), &format!("{} failed", 4 - n)));
        }
        assert_eq!(
            signal.detect(&evidence_of(&progressing, &[])),
            None,
            "same input, different output, is progress and must not fire"
        );

        // Control: the same *output* from different calls is not a repeat
        // either — four greps for four different needles all missing is
        // exploration.
        let mut exploring = Vec::new();
        for n in 0..4 {
            exploring.push(call(
                &format!("c{n}"),
                "grep",
                &format!(r#"{{"q":"needle-{n}"}}"#),
            ));
            exploring.push(result(&format!("c{n}"), "no matches"));
        }
        assert_eq!(signal.detect(&evidence_of(&exploring, &[])), None);

        // Control: three identical calls of which the last is unanswered. Half
        // the comparison has not arrived, so there is nothing to be identical
        // to yet.
        let mut in_flight = stuck.clone();
        in_flight.truncate(6);
        in_flight.push(call("c9", "pytest", r#"{"path":"tests/"}"#));
        assert_eq!(
            signal.detect(&evidence_of(&in_flight, &[])),
            None,
            "an unanswered call is not yet a repeat of anything"
        );
    }

    #[test]
    fn the_other_three_signals_fire_on_their_pattern_and_are_quiet_otherwise() {
        // Ping-pong: strict alternation of two names, and nothing else.
        let ping_pong = PingPong { cycles: 3 };
        let mut alternating = Vec::new();
        for n in 0..6 {
            let name = if n % 2 == 0 { "read" } else { "edit" };
            alternating.push(call(&format!("c{n}"), name, "{}"));
            alternating.push(result(&format!("c{n}"), "ok"));
        }
        assert!(
            ping_pong
                .detect(&evidence_of(&alternating, &[]))
                .is_some_and(|fact| fact.contains("`read`") && fact.contains("`edit`"))
        );
        // Control: a third tool in the middle breaks it. Trying something
        // different is adaptation, which is the behavior this must not punish.
        let mut adapting = alternating.clone();
        adapting[4] = call("c2", "search", "{}");
        assert_eq!(ping_pong.detect(&evidence_of(&adapting, &[])), None);
        // Control: too short to be a cycle.
        assert_eq!(ping_pong.detect(&evidence_of(&alternating[..4], &[])), None);

        // Failure streak: answered and failed, three deep.
        let streak = ToolFailureStreak { length: 3 };
        let mut failing = Vec::new();
        for n in 0..3 {
            failing.push(call(&format!("c{n}"), "cargo", "{}"));
            failing.push(result(&format!("c{n}"), "error: unresolved import"));
        }
        assert!(streak.detect(&evidence_of(&failing, &[])).is_some());
        // Control: one success at the end ends the streak.
        let mut recovered = failing.clone();
        recovered.pop();
        recovered.push(result("c2", "Finished dev profile"));
        assert_eq!(streak.detect(&evidence_of(&recovered, &[])), None);
        // Control: an unanswered last call is not a failure.
        let mut in_flight = failing.clone();
        in_flight.pop();
        assert_eq!(streak.detect(&evidence_of(&in_flight, &[])), None);

        // Cost anomaly, against the session's own trailing distribution.
        let anomaly = CostAnomaly {
            min_samples: 4,
            multiple: 3.0,
        };
        assert!(
            anomaly
                .detect(&evidence_of(&[], &[1_000, 1_200, 900, 1_100, 30_000]))
                .is_some_and(|fact| fact.contains("30000") && fact.contains("1100"))
        );
        // Control: the same large turn in a session whose turns are all large.
        assert_eq!(
            anomaly.detect(&evidence_of(&[], &[28_000, 31_000, 29_000, 30_000, 30_000])),
            None,
            "a big session is not an anomalous one; the distribution is its own"
        );
        // Control: too few prior turns to have a distribution at all.
        assert_eq!(anomaly.detect(&evidence_of(&[], &[1_000, 30_000])), None);
        // Control: one earlier spike must not raise the bar out of reach —
        // which is what a mean would do and a median does not.
        assert!(
            anomaly
                .detect(&evidence_of(
                    &[],
                    &[1_000, 90_000, 1_100, 900, 1_000, 30_000]
                ))
                .is_some()
        );
    }

    /// The whole point of the conjunction, as a test.
    #[test]
    fn the_cadence_alone_never_fires_without_a_signal() {
        let trigger = Trigger::new(TriggerConfig::default(), default_signals());

        // A long, expensive, entirely healthy session: the gate is wide open
        // and nothing is wrong. "Validate more when things look fine" is the
        // behavior with measured *negative* benefit, so this must be silent.
        let mut healthy_items = Vec::new();
        for n in 0..6 {
            healthy_items.push(call(&format!("c{n}"), "edit", &format!(r#"{{"n":{n}}}"#)));
            healthy_items.push(result(&format!("c{n}"), &format!("wrote {n} lines")));
        }
        let healthy = wide_open(healthy_items.clone());
        assert_eq!(
            trigger.evaluate(&healthy),
            None,
            "a cadence that has come due is permission to ask, never a reason to"
        );

        // The control that makes the assertion above about the *signal* rather
        // than about the gate: one signal turned on, everything else identical,
        // and it fires.
        let mut items = healthy_items;
        items.extend(stuck_items());
        let stuck = wide_open(items);
        let fired = trigger
            .evaluate(&stuck)
            .expect("an open gate plus evidence is the one case that consults");
        assert_eq!(fired.signals.len(), 1);
        assert_eq!(fired.signals[0].kind, SignalKind::NoProgressRepeat);
        assert_eq!(fired.turn_index, stuck.turn_index);
    }

    /// The hysteresis, which is the difference between a validator and a loop.
    #[test]
    fn a_turn_fulfilling_an_open_steer_never_fires() {
        let trigger = Trigger::new(TriggerConfig::default(), default_signals());

        // A session that would fire on its own evidence.
        let stuck = wide_open(stuck_items());
        assert!(
            trigger.evaluate(&stuck).is_some(),
            "the control: this session's evidence is what makes the next \
             assertion about the steer and not about the evidence"
        );

        // The same session, on the turn whose input answered the steer we
        // emitted. Every signal still reads exactly as it did — the correction
        // has not had a chance to change anything yet — so without this rule a
        // steer re-triggers the validation that emitted it, forever.
        let mut fulfilling = wide_open(stuck_items());
        fulfilling.steer_fulfilled_on_turn = Some(fulfilling.turn_index);
        assert!(fulfilling.this_turn_fulfilled_a_steer());
        assert_eq!(trigger.evaluate(&fulfilling), None);

        // The control on the *other* side of the rule: a steer fulfilled on an
        // earlier turn does not disable validation for the rest of the session.
        let mut earlier = wide_open(stuck_items());
        earlier.steer_fulfilled_on_turn = Some(earlier.turn_index - 1);
        assert!(trigger.evaluate(&earlier).is_some());
    }

    #[test]
    fn every_arm_of_the_gate_closes_it_on_its_own() {
        let trigger = Trigger::new(TriggerConfig::default(), default_signals());
        let config = TriggerConfig::default();

        // The base case: open, with evidence.
        assert!(trigger.evaluate(&wide_open(stuck_items())).is_some());

        // Turn 0 and turn 1: there is no trajectory to judge before there is a
        // trajectory.
        for index in [0, 1] {
            let mut early = wide_open(stuck_items());
            early.turn_index = index;
            assert_eq!(
                trigger.evaluate(&early),
                None,
                "turn {index} has no history"
            );
        }

        // The token gate: one token short is closed, exactly at the threshold
        // is open. Tokens rather than turns is the self-scaling half of the
        // design, so the boundary is worth pinning.
        for (tokens, open) in [
            (config.tokens_between_validations - 1, false),
            (config.tokens_between_validations, true),
        ] {
            let mut state = wide_open(stuck_items());
            state.tokens_since_last_validation = tokens;
            assert_eq!(trigger.evaluate(&state).is_some(), open, "{tokens} tokens");
        }

        // The cooldown, measured on the log's own timestamps rather than on a
        // clock this function was handed.
        for (elapsed, open) in [(config.cooldown_ms - 1, false), (config.cooldown_ms, true)] {
            let mut state = wide_open(stuck_items());
            state.last_validation_at_ms = Some(1_000_000);
            state.last_event_at_ms = 1_000_000 + elapsed;
            assert_eq!(
                trigger.evaluate(&state).is_some(),
                open,
                "{elapsed}ms elapsed"
            );
        }

        // The intervention cap: a session this deployment has already
        // interrupted to the cap is one it stops interrupting.
        for (interventions, open) in [
            (config.max_consecutive_interventions, false),
            (config.max_consecutive_interventions - 1, true),
        ] {
            let mut state = wide_open(stuck_items());
            state.consecutive_interventions = interventions;
            assert_eq!(trigger.evaluate(&state).is_some(), open);
        }

        // The per-session review cap: the log-derived half of the review
        // budget.
        for (run, open) in [
            (config.max_validations_per_session, false),
            (config.max_validations_per_session - 1, true),
        ] {
            let mut state = wide_open(stuck_items());
            state.validations_run = run;
            assert_eq!(trigger.evaluate(&state).is_some(), open);
        }
    }
}
