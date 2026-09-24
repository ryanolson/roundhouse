// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What `plan` reads before it prices anything: the turn's own features, the
//! objective it is decided under, the classification window it may name, and
//! whether the local quote is worth asking for. None of it needs a candidate,
//! a quote or a dispatch — moved here so `plan` itself reads as price, admit,
//! decide, dispatch, rather than carrying roughly ninety lines of capture that
//! has nothing to do with any of those.

use roundhouse_core::classify::ClassificationWindow;
use roundhouse_core::classify::projection::PROJECTION_REVISION;
use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::TurnPolicy;
use roundhouse_core::routing::{
    FEATURE_EXTRACTOR_REVISION, LocalFeatures, LocalQuoteSkip, Target, TurnSignals,
};
use roundhouse_core::session::Session;
use roundhouse_core::store::SessionStore;
use roundhouse_core::validate::{ControlCallDialect, Objective, ObjectiveVersion, exchanges};

use crate::control_config::Admission;
use crate::engine::{ClientDeclarations, Engine};

/// What may be said about this turn, taken from the client's own items while
/// they are still a separate thing. The committed log is one flat list, and
/// reconstructing "what arrived on this turn" out of it afterwards would be a
/// second answer to a question the caller holds right here.
pub(super) struct SelectionInputs {
    pub features: LocalFeatures,
    pub objective: ObjectiveVersion,
    /// `None` on every deployment that configured no classifier — the shipped
    /// state — and costs one `Option` check.
    pub classifications: Option<ClassificationWindow>,
}

impl<S: SessionStore, T: Tokenizer + Clone + 'static> Engine<S, T> {
    /// Capture the cutoff with the features. Recomputing it during failover
    /// would include the preceding dispatch in a later attempt's snapshot.
    ///
    /// Computed once from committed input and retained in the route record.
    /// Later turns can use a changed extractor without rewriting this turn's
    /// evidence. No model call is required.
    pub(super) fn selection_inputs(
        &self,
        session: &Session<S>,
        turn_index: u64,
    ) -> SelectionInputs {
        let dialect = ControlCallDialect::of_session_key(session.session_id().as_str());
        let signals = TurnSignals::from_exchanges(&exchanges(&session.state().items), dialect);
        let features = LocalFeatures {
            extractor_revision: FEATURE_EXTRACTOR_REVISION,
            dialect,
            signals,
            turn_index,
            observed_through_seq: session.last_seq(),
        };
        // The objective this turn is decided under, read the way the
        // validator's brief reads it, so a later review can show that it
        // applied. An undeclared objective stamps as undeclared without
        // copying the request that stands in for it.
        let objective = ObjectiveVersion::of(
            &self
                .declared_objective(session.session_id())
                .unwrap_or(Objective::Unknown),
        );
        // Which background classifications this decision can see, named at
        // the same cutoff the features were taken at. A result that lands
        // while this turn is in flight has a higher sequence and is
        // therefore absent here — the no-backdating rule expressed as a
        // filter rather than as a convention somebody has to remember.
        //
        // **Bounded to the window a projection would actually carry.**
        // Naming every classification a session ever produced on every
        // `Routed` makes the log grow with the square of the turn count; the
        // window records how many were available beyond the ones it names,
        // so what it leaves out is a number rather than a silence.
        let classifications = self.classifier.as_ref().map(|classifier| {
            ClassificationWindow::of(
                PROJECTION_REVISION,
                features.observed_through_seq,
                classifier.projection_caps().max_prior_classifications,
                session
                    .state()
                    .classifications_through(features.observed_through_seq),
            )
        });
        SelectionInputs {
            features,
            objective,
            classifications,
        }
    }

    /// Whether the local quote is worth asking for, decided without an HTTP
    /// round trip. `None` means ask it; `Some` names why the answer would
    /// have been thrown away, and that name is what reaches the log — see
    /// [`roundhouse_core::routing::DecisionRecord::local_quote_skipped`].
    ///
    /// **The local quote is a decision, not a lookup.** `price` sends this
    /// turn's block and sequence hashes to the selector and waits for what it
    /// is still holding, which makes it an HTTP round-trip on the path to
    /// first token — and Dynamo can hold a KV cache for longer than any
    /// frontier TTL, so the answer is worth having whenever it can move the
    /// route. It is worth nothing when it cannot: a coding agent declares a
    /// toolbox on nearly every turn, this function's tool arm excludes local
    /// on exactly those turns before a quote is ever asked for, and a
    /// selector that is down was failing turns that were always going to a
    /// hosted model.
    pub(super) fn local_quote_skip(
        &self,
        declarations: &ClientDeclarations,
        admission: &Admission,
    ) -> Option<LocalQuoteSkip> {
        self.fleet.as_ref().and_then(|_| {
            local_quote_can_matter(
                declarations.declares_tools(),
                &admission.policy,
                &Target::local_policy_identity(&self.config.local_model),
                self.config.local_quality_prior,
            )
            .err()
        })
    }
}

/// Whether pricing the local fleet can still change this turn's route.
///
/// `Ok(())` means ask it; `Err` names why the answer would have been thrown
/// away — see [`Engine::local_quote_skip`]. `plan` reads this `Err` directly
/// as `local_quote_skipped`, and a tool-declaring turn is withheld from local
/// exactly when it is `Some(LocalQuoteSkip::ToolsDeclared)`: no local
/// candidate is ever quoted for one, so there is nothing downstream left to
/// filter back out. A free function over values the call site
/// already holds, so the decision to skip an HTTP call is testable without a
/// session, a fleet or a clock.
///
/// Both refusals are *reachability* facts in the sense
/// [`TurnPolicy::permits`] means: the same answer on every turn that looks
/// like this one. The two knobs that are deliberately not consulted are the
/// budget and the cadence, because both make a local route more likely — a
/// squeezed budget is precisely when the fleet's answer decides the turn, so
/// skipping the quote there would drop the call exactly where it earned its
/// latency.
///
/// The policy asked is the one resolved at admission, before the validator's
/// escalation narrows it. That is safe in the only direction that matters:
/// an escalation raises the quality floor and never lowers it, so a policy
/// that already names no local target still names none afterwards.
fn local_quote_can_matter(
    declares_tools: bool,
    policy: &TurnPolicy,
    local_policy_identity: &str,
    local_quality_prior: f64,
) -> Result<(), LocalQuoteSkip> {
    // **M11.2a's F2, and it is a routing fact rather than a dispatch one.**
    // [`LocalExecutor::execute`] takes prompt token ids and an output cap and
    // nothing else — this build has no way to tell a locally served model
    // about a toolbox at all — and [`LocalExecution::text`] is a plain
    // `String`, structurally incapable of carrying a call back. So a turn
    // that declares tools and lands local is answered in prose, reports
    // `end_turn` as if it had finished normally, and signals the loss
    // nowhere: the client's agent loop simply stops working, which is the
    // one failure shape this codebase treats as worse than an error.
    //
    // **Checked before the policy arm below, and the order is load-bearing,
    // not tidy.** A candidate that could never have served this
    // turn must not sit in `considered` either, or the dashboard prices a
    // counterfactual saving against a target the turn could not have used —
    // checking policy first would report `PolicyAdmitsNoLocal` for a
    // tool-declaring turn a hosted-only policy also excludes, and `plan`
    // would then annotate the decision as a policy exclusion, dropping
    // `TOOL_TURN_EXCLUDES_LOCAL` from a record it belongs on.
    //
    // The alternative deliberately not taken: rendering a textual toolbox
    // into the local prompt and parsing calls back out of the model's prose.
    // That is a real design with a real cost — a second, weaker tool
    // protocol whose failures look like bad answers — and it belongs to
    // whichever milestone decides local models should be agentic, not to a
    // review fix.
    if declares_tools {
        return Err(LocalQuoteSkip::ToolsDeclared);
    }
    if !policy.permits_identity(local_policy_identity, local_quality_prior) {
        return Err(LocalQuoteSkip::PolicyAdmitsNoLocal);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use roundhouse_core::control::TargetFilter;

    /// The predicate that decides whether an HTTP round-trip is worth making,
    /// asked without an engine, a session or a fleet — which is the reason it
    /// is a free function over plain values rather than a method.
    #[test]
    fn local_quote_can_matter_names_why_it_cannot() {
        let open = TurnPolicy::unrestricted();
        let identity = Target::local_policy_identity("llama");

        assert_eq!(local_quote_can_matter(false, &open, &identity, 0.6), Ok(()));
        assert_eq!(
            local_quote_can_matter(true, &open, &identity, 0.6),
            Err(LocalQuoteSkip::ToolsDeclared),
            "a toolbox makes every local candidate unreachable, so the quote \
             would be discarded on arrival"
        );

        let hosted_only = TurnPolicy {
            allow: TargetFilter::parse(["anthropic/*"]).unwrap(),
            ..TurnPolicy::unrestricted()
        };
        assert_eq!(
            local_quote_can_matter(false, &hosted_only, &identity, 0.6),
            Err(LocalQuoteSkip::PolicyAdmitsNoLocal)
        );
        // **ORDERING.** Tools must win over policy: a
        // tool-declaring turn under a policy that also excludes every local
        // target still has to name the tool reason, because that is the name
        // `plan` reads to refuse or annotate the turn — a policy reason here
        // would drop `TOOL_TURN_EXCLUDES_LOCAL` from a decision it belongs on.
        assert_eq!(
            local_quote_can_matter(true, &hosted_only, &identity, 0.6),
            Err(LocalQuoteSkip::ToolsDeclared),
            "the tool arm must be checked before the policy arm"
        );

        let discerning = TurnPolicy {
            min_quality: 0.9,
            ..TurnPolicy::unrestricted()
        };
        assert_eq!(
            local_quote_can_matter(false, &discerning, &identity, 0.6),
            Err(LocalQuoteSkip::PolicyAdmitsNoLocal),
            "a floor above the configured local prior is the same refusal by \
             the other axis: the prior is configuration, so the answer is known \
             before the selector is asked"
        );
        assert_eq!(
            local_quote_can_matter(false, &discerning, &identity, 0.95),
            Ok(()),
            "and a fleet that clears the floor is still asked"
        );
    }
}
