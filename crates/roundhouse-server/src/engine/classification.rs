// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use roundhouse_core::classify::projection::PromptCapture;
use roundhouse_core::context::Tokenizer;
use roundhouse_core::ids::ResponseId;
use roundhouse_core::now_ms;
use roundhouse_core::routing::Decision;
use roundhouse_core::session::Session;
use roundhouse_core::store::SessionStore;

use crate::classify_runtime::Capacity;
use crate::control_config::Admission;
use crate::engine::Engine;

impl<S: SessionStore, T: Tokenizer + Clone + 'static> Engine<S, T> {
    /// Commit whatever the background classifier finished since the last turn.
    ///
    /// **The engine's writer, because it is the only one there is.** A worker
    /// that opened a session to deliver its own result would take the lease from
    /// whichever turn is running and fence it; `Session::open_observed` acquires
    /// the lease, and nothing about a background result is worth that.
    ///
    /// Append first, acknowledge second. A result whose append fails stays with
    /// the runtime for a later drain — still holding its admission permit, still
    /// costing nothing further, and emphatically not re-purchased. A result whose
    /// append succeeded and whose acknowledgement was lost is offered again and
    /// refused here by the log's own record of it, so no path delivers twice.
    pub(super) async fn deliver_classifications(&self, session: &mut Session<S>) {
        let Some(classifier) = &self.classifier else {
            return;
        };
        let waiting = classifier.ready(session.session_id()).await;
        if waiting.is_empty() {
            return;
        }
        let mut delivered = Vec::with_capacity(waiting.len());
        for completed in waiting {
            let call_id = completed.record.call_id.clone();
            if session.state().classification_settled(&call_id) {
                // Already in the log: the append landed and the acknowledgement
                // did not. Acknowledged now, appended never.
                delivered.push(call_id);
                continue;
            }
            match session
                .record_classification(completed.record.clone())
                .await
            {
                Ok(()) => delivered.push(call_id),
                Err(error) => {
                    // The usual reason is a lost lease, and the turn about to run
                    // is the better diagnosis. Stop rather than continue: a
                    // writer that cannot append one event will not append the
                    // next.
                    tracing::warn!(
                        %error,
                        session_id = %session.session_id(),
                        "a classification result could not be appended; it stays with \
                         the runtime for a later turn to deliver"
                    );
                    break;
                }
            }
        }
        classifier
            .acknowledge(session.session_id(), &delivered)
            .await;
    }

    /// Commit the acknowledgements the repair workers produced.
    ///
    /// Append first, acknowledge second — the same order and the same reason as
    /// [`Self::deliver_classifications`]. An acknowledgement whose append fails
    /// stays with the runtime, and even if it is lost entirely the log still
    /// reads the settlement as unrepaired, so a later turn drives it again and
    /// the ledger deduplicates. **No path here can charge twice**, and no path
    /// here reaches the classifier at all.
    pub(super) async fn deliver_settlement_repairs(&self, session: &mut Session<S>) {
        let Some(classifier) = &self.classifier else {
            return;
        };
        let waiting = classifier.ready_repairs(session.session_id()).await;
        if waiting.is_empty() {
            return;
        }
        let mut written = Vec::with_capacity(waiting.len());
        for answered in waiting {
            let call_id = answered.record.call_id.clone();
            match session
                .record_classification_settlement_repair(answered.record.clone())
                .await
            {
                Ok(()) => written.push(call_id),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        session_id = %session.session_id(),
                        "an evaluation settlement repair could not be appended; \
                         the settlement stays unconfirmed in the log and a later \
                         turn will drive it again"
                    );
                    break;
                }
            }
        }
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
    pub(super) async fn repair_classification_settlements(&self, session: &Session<S>) {
        let Some(classifier) = &self.classifier else {
            return;
        };
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
    pub(super) async fn request_classification(
        &self,
        session: &mut Session<S>,
        response_id: &ResponseId,
        admission: &Admission,
        decision: &Decision,
        capacity: Capacity,
        capture: &PromptCapture,
    ) {
        let Some(classifier) = &self.classifier else {
            return;
        };
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
        let projection = match classifier.projection(
            capture,
            session.state().classifications(),
            session.state().prior_turns(),
        ) {
            Ok(projection) => projection,
            Err(refusal) => {
                tracing::debug!(?refusal, "no turn classification for this turn");
                return;
            }
        };
        let prepared = match classifier.prepare(
            admission.principal.clone(),
            session.session_id().clone(),
            // Fresh per external attempt. A settled identity can never settle
            // again, so reusing the turn's would collide with the turn's own
            // hold; reusing an earlier call's would be refused by the
            // once-per-call rule after the first.
            ResponseId::generate(),
            session.turn_index().saturating_sub(1),
            response_id.clone(),
            &projection,
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
