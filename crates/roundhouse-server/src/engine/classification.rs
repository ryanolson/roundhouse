// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The classification feature's two seams into a turn: one hook before it is
//! admitted, one after it terminates. `Engine::classifier` is read exactly
//! once by each, in the hook itself, rather than once per helper — every
//! helper below takes the runtime as a parameter instead of re-deriving it.

use std::sync::Arc;

use roundhouse_core::classify::projection::PromptCapture;
use roundhouse_core::context::Tokenizer;
use roundhouse_core::ids::ResponseId;
use roundhouse_core::item::Item;
use roundhouse_core::now_ms;
use roundhouse_core::routing::Decision;
use roundhouse_core::session::Session;
use roundhouse_core::store::SessionStore;

use crate::classify_runtime::{Capacity, ClassificationRuntime, ClassificationSource};
use crate::control_config::Admission;
use crate::engine::Engine;

/// What [`Engine::classification_before_turn`] took for this turn's own
/// classification, carried to [`Engine::classification_after_turn`].
///
/// A plain pair rather than two separate `Option`s threaded through
/// `run_turn`: the capacity and the capture it bounds are taken together and
/// used together, and two options would let a caller pass one without the
/// other, which the runtime's `prepare` would then have nothing sound to do
/// with.
pub(super) struct TurnClassification {
    capacity: Capacity,
    capture: PromptCapture,
}

impl<S: SessionStore, T: Tokenizer + Clone + 'static> Engine<S, T> {
    /// Drain whatever the classifier finished since the last turn, and take
    /// capacity plus a bounded prompt capture for this turn's own
    /// classification, if there is room.
    ///
    /// **Before `begin_turn`, on both halves.** The drain puts every
    /// delivered result at a sequence below the cutoff `plan` is about to
    /// capture, so this turn can name them; a drain after the turn's own
    /// decision could not be named by it. The capacity check is here for the
    /// same reason: acquired before the prompt is copied, so unavailable
    /// classification adds no payload allocation, and before `begin_turn`
    /// moves `input` — this is the last moment the client's own items are
    /// still separate from the committed log. A deduplicated turn still
    /// drains: the client's retry is not a reason to strand output the
    /// runtime is holding capacity for. It also still takes and then drops
    /// this turn's own capacity, through the ordinary `Capacity` drop path,
    /// because nothing downstream of the dedup short-circuit ever reaches
    /// [`Engine::classification_after_turn`] to spend it.
    ///
    /// `None` on every deployment that configured no classifier — the
    /// shipped state — and on a saturated queue, which costs one `Option`
    /// check either way.
    pub(super) async fn classification_before_turn(
        &self,
        session: &mut Session<S>,
        input: &[Item],
    ) -> Option<TurnClassification> {
        let classifier = self.classifier.as_ref()?;
        self.deliver_classifier_output(session, classifier).await;
        let capacity = classifier.capacity()?;
        Some(TurnClassification {
            capacity,
            capture: PromptCapture::of(input, &classifier.projection_caps()),
        })
    }

    /// Request classification for the turn that just ended, if it dispatched
    /// and took capacity for it, and schedule the bounded repair batch.
    ///
    /// **Gated on `self.classifier`, not on `background`.** A turn that took
    /// no capacity — a saturated queue, most often — still owes the ledger
    /// whatever repair work is outstanding; the two questions ("can this
    /// turn buy a new classification" and "does this deployment have one
    /// configured at all") are unrelated; only the second gates this method
    /// running at all.
    ///
    /// **After the terminal event, before the lease is handed back**, so the
    /// durable intent is written by the writer that already holds it, and
    /// nothing here can delay an answer.
    pub(super) async fn classification_after_turn(
        &self,
        session: &mut Session<S>,
        response_id: &ResponseId,
        admission: &Admission,
        background: Option<TurnClassification>,
        settled_decision: Option<&Decision>,
    ) {
        let Some(classifier) = self.classifier.as_ref() else {
            return;
        };
        // Only a turn that *dispatched* is classified, and the decision comes
        // from this turn's own `Completed` rather than from the fold. A
        // steered turn writes no `Routed`, so reading `last_decision()` here
        // would hand it the previous turn's admitted pool and fabricate
        // egress permission out of a decision that was never taken.
        if let (Some(decision), Some(TurnClassification { capacity, capture })) =
            (settled_decision, background)
        {
            self.request_classification(
                session,
                response_id,
                admission,
                classifier,
                decision,
                capacity,
                &capture,
            )
            .await;
        }
        // **Unconditional, unlike the request above** — a turn that steered
        // or failed still owes the ledger the same money. Spawning costs a
        // semaphore try and a task; the ledger round trip it starts belongs
        // to the executor, which is the "off the serving path" rule this
        // site is under.
        self.repair_classification_settlements(session, classifier)
            .await;
    }

    /// Commit whatever the background classifier finished since the last
    /// turn: results and settlement-repair acknowledgements together.
    ///
    /// **The engine's writer, because it is the only one there is.** A worker
    /// that opened a session to deliver its own output would take the lease
    /// from whichever turn is running and fence it; `Session::open_observed`
    /// acquires the lease, and nothing about background output is worth that.
    ///
    /// **One batched commit, not one append per record.** A session with k
    /// parked results and j parked repairs commits them in one
    /// [`Session::record_background_classification`] call rather than k + j
    /// sequential store round trips, every one of them before this turn's own
    /// `TurnStarted` — on the path to first token, on every turn that follows
    /// a classified one. An empty drain costs none at all.
    ///
    /// Append first, acknowledge second, still. A kind already in the log —
    /// its append landed on an earlier turn, its acknowledgement lost to that
    /// turn's own crash or cancellation — is acknowledged here without being
    /// appended again; that check is what lets the batch commit run
    /// unconditionally on everything else. A batch that fails to commit
    /// leaves every kind in it with the runtime, still holding whatever
    /// capacity or claim it held, for a later turn to drain: all-or-nothing,
    /// so no path here can deliver — or charge — twice.
    async fn deliver_classifier_output(
        &self,
        session: &mut Session<S>,
        classifier: &Arc<ClassificationRuntime<T>>,
    ) {
        let waiting_results = classifier.ready(session.session_id()).await;
        let waiting_repairs = classifier.ready_repairs(session.session_id()).await;
        if waiting_results.is_empty() && waiting_repairs.is_empty() {
            return;
        }

        // Each partition is (already in the log, needs appending). The engine
        // already holds every call id it is about to append -- `commit` is
        // all-or-nothing, so on `Ok` those ids are exactly what landed -- so
        // there is nothing to read back out of the committed events.
        let (settled, unsettled): (Vec<_>, Vec<_>) =
            waiting_results.into_iter().partition(|completed| {
                session
                    .state()
                    .classification_settled(&completed.record.call_id)
            });
        let mut delivered: Vec<_> = settled
            .into_iter()
            .map(|completed| completed.record.call_id.clone())
            .collect();
        let results: Vec<_> = unsettled
            .into_iter()
            .map(|completed| completed.record.clone())
            .collect();

        let (already_repaired, unrepaired): (Vec<_>, Vec<_>) =
            waiting_repairs.into_iter().partition(|answered| {
                !session
                    .state()
                    .is_settlement_unrepaired(&answered.record.call_id)
            });
        let mut written: Vec<_> = already_repaired
            .into_iter()
            .map(|answered| answered.record.call_id.clone())
            .collect();
        let repairs: Vec<_> = unrepaired
            .into_iter()
            .map(|answered| answered.record.clone())
            .collect();

        if !results.is_empty() || !repairs.is_empty() {
            // Collected before the move below: `record_background_classification`
            // takes `results` and `repairs` by value.
            let result_ids: Vec<_> = results.iter().map(|r| r.call_id.clone()).collect();
            let repair_ids: Vec<_> = repairs.iter().map(|r| r.call_id.clone()).collect();
            match session
                .record_background_classification(results, repairs)
                .await
            {
                Ok(()) => {
                    delivered.extend(result_ids);
                    written.extend(repair_ids);
                }
                Err(error) => {
                    // The usual reason is a lost lease, and the turn about to
                    // run is the better diagnosis. Neither list above gains
                    // an entry: nothing in this batch landed, so nothing in
                    // it may be acknowledged.
                    tracing::warn!(
                        %error,
                        session_id = %session.session_id(),
                        "background classifier output could not be appended; it stays \
                         with the runtime for a later turn to deliver"
                    );
                }
            }
        }
        classifier
            .acknowledge(session.session_id(), &delivered)
            .await;
        classifier
            .acknowledge_repairs(session.session_id(), &written)
            .await;
    }

    /// Schedule a bounded batch of unconfirmed settlements.
    ///
    /// The durable session principal identifies the original payer. A later
    /// turn's admission cannot substitute for a missing recorded principal.
    /// The fixed batch bounds scheduling work even when failed workers return
    /// permits during this loop. Runtime claims suppress duplicate attempts
    /// through execution and acknowledgement delivery.
    async fn repair_classification_settlements(
        &self,
        session: &Session<S>,
        classifier: &Arc<ClassificationRuntime<T>>,
    ) {
        let unrepaired = session.state().unrepaired_settlements();
        if unrepaired.len() == 0 {
            return;
        }
        let Some(principal) = session.state().principal() else {
            tracing::warn!(
                session_id = %session.session_id(),
                unrepaired = unrepaired.len(),
                "this session's log names no payer, so an unconfirmed evaluation \
                 settlement cannot be repaired; the live turn's principal is not \
                 evidence about who paid for a call that finished earlier"
            );
            return;
        };
        for settlement in classifier.repair_batch(unrepaired) {
            let Some(capacity) = classifier.capacity() else {
                // Saturated, or the runtime has stopped. The settlement stays
                // in the log, which is exactly where a later turn finds it.
                break;
            };
            classifier
                .repair(
                    capacity,
                    session.session_id().clone(),
                    principal.clone(),
                    settlement.clone(),
                )
                .await;
        }
    }

    /// Commit the intent to classify the turn that just ended, and start it.
    ///
    /// Best-effort throughout: every refusal here leaves the turn exactly as it
    /// was. The order is the durability contract and is not negotiable —
    /// capacity, then payload, then reservation, then the durable intent, and
    /// only then a worker that may open a socket. The caller acquires capacity
    /// before capturing the prompt. Each early return here releases that permit.
    #[allow(clippy::too_many_arguments)]
    async fn request_classification(
        &self,
        session: &mut Session<S>,
        response_id: &ResponseId,
        admission: &Admission,
        classifier: &Arc<ClassificationRuntime<T>>,
        decision: &Decision,
        capacity: Capacity,
        capture: &PromptCapture,
    ) {
        // **One answer per turn is structural rather than guarded here.** This
        // is reached once per `run_turn`, and the two ways a turn could arrive
        // twice both stop short of it: a client's retry of a completed turn
        // returns at the dedup short-circuit above, and a re-admitted failed
        // turn is a *different* response — `begin_turn` mints a fresh
        // `ResponseId` — so it is a new question rather than the same one asked
        // again. A membership check against the fold would be a guard that
        // never fires, and one that never fires is one nothing keeps honest.
        //
        // **Nothing below this line awaits the evaluation ledger or the
        // classifier.** The projection is a bounded render, `prepare` is a
        // serialization, and the intent append is a write the turn's own writer
        // was going to make anyway. The grant and the request both belong to the
        // worker `spawn` starts, so a slow or unreachable evaluation ledger
        // delays no response.
        let prepared = match classifier.prepare(
            ClassificationSource {
                principal: admission.principal.clone(),
                session_id: session.session_id().clone(),
                // Fresh per external attempt. A settled identity can never
                // settle again, so reusing the turn's would collide with the
                // turn's own hold; reusing an earlier call's would be refused
                // by the once-per-call rule after the first.
                call_id: ResponseId::generate(),
                source_turn_index: session.turn_index().saturating_sub(1),
                source_response_id: response_id.clone(),
            },
            capture,
            session.state().classifications(),
            session.state().prior_turns(),
            // **The policy's own resolution, never a second one.** Asking
            // `admissible` again here would answer a different question — with
            // a guessed load ceiling and without the overflow valve — and record
            // its answer as this decision's permission to send a tenant's prompt
            // to a third party.
            decision.admitted.as_deref(),
            now_ms(),
        ) {
            Ok(prepared) => prepared,
            Err(refusal) => {
                tracing::debug!(?refusal, "no turn classification for this turn");
                return;
            }
        };
        match session
            .record_classification_intent(prepared.intent.clone())
            .await
        {
            // The intent is durable. Only now may anything be sent — and the
            // grant that funds it is the worker's, so nothing this turn did
            // touched the evaluation ledger.
            Ok(()) => {
                classifier
                    .spawn(capacity, session.session_id().clone(), prepared)
                    .await
            }
            Err(error) => {
                // Nothing to hand back: no grant was opened, because opening one
                // here is exactly what this seam no longer does.
                tracing::warn!(
                    %error,
                    session_id = %session.session_id(),
                    "a classification intent could not be committed; no call was \
                     made and no evaluation budget was reserved"
                );
            }
        }
    }
}
