// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The engine's two ends of the fair-use seam: ask before, record after.
//!
//! Its own file rather than lines inside [`spend`](super::spend), and the
//! reason is the one the addendum gives for the seam existing at all: fair use
//! is *not* the budget ladder. It takes no hold, releases nothing, is not
//! idempotent by `(session, seq)`, and cannot fail a turn from inside the
//! dispatch. Sharing a file with the settle would invite the next person to
//! share its arithmetic, which is exactly the merge the M8 window hazard says
//! not to make.
//!
//! **Two calls, at two moments, and neither is where the budget's are.**
//!
//! - [`Engine::fair_use_refusal`] runs at the *transport's* admission, before
//!   the session is bound and long before a grant. That placement is what makes
//!   `a_refused_turn_took_no_grant_and_left_no_hold` a property of the control
//!   flow rather than a claim: there is no grant call between the key lookup
//!   and this one. It is also the only place a `429` is still expressible —
//!   both surfaces spawn `run_turn` into a task and answer with a stream, so an
//!   error raised inside the turn becomes a terminal log event and never a
//!   status code.
//! - [`Engine::record_fair_use_draw`] runs in `run_turn`'s tail, beside the
//!   settle and **independent of it**. `Engine::settle` returns early for a
//!   membership with no budget, and a project with fair-use windows and no
//!   dollar ceiling is precisely the shape the addendum ships (budgets
//!   unlimited, windows real) — a draw hung off the settle would record nothing
//!   for exactly the projects fair use governs.
//!
//! **What a crash costs, stated rather than discovered.** A process that dies
//! between the log commit and the draw loses that turn's draw: unlike a settle,
//! there is no watermark to re-drive it from, because the counters are rolling
//! sums rather than a per-session position. The loss is bounded by one turn per
//! dead process and it errs in the *permissive* direction, which is the right
//! direction for a limit whose whole purpose is to approximate a lab's session
//! ceiling rather than to be a bill.
//!
//! **The other direction is the one that had to be closed by a check.** A draw
//! is not idempotent — there is no `(session, seq)` key to make it so, because
//! rolling sums have no position to compare against — so a draw that ran twice
//! over one turn's usage would refuse a *later* turn that had room, which errs
//! restrictive and is the direction a ceiling must never err in silently. The
//! reachable path was real: a dispatch failure terminates its response
//! best-effort (`let _ = session.mark_incomplete(...)`), so a lost lease leaves
//! this turn with no terminal event and `last_settlement()` answering with the
//! *previous* turn's. [`Engine::record_fair_use_draw`] therefore checks that the
//! settlement it found belongs to the response it was called for, and draws
//! nothing when it does not.
//!
//! **A judge's side call is not a second draw.** Where the interjection seam
//! answers a turn, the judge's tokens *are* the turn's booked usage and are
//! counted once here; where the turn is dispatched, they are booked under the
//! judge's own model row and are not the tenant's answer. Adding them again
//! would make the validate loop eat the ceiling it exists to protect.

use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::FairUseRefusal;
use roundhouse_core::ids::ResponseId;
use roundhouse_core::now_ms;
use roundhouse_core::session::Session;
use roundhouse_core::store::SessionStore;

use crate::control_config::Admission;
use crate::engine::spend::settled_cost_usd;
use crate::engine::{Engine, EngineError};

impl<S: SessionStore, T: Tokenizer + Clone> Engine<S, T> {
    /// Whether a turn admitted for this membership right now would exceed a
    /// rolling window, and which.
    ///
    /// **Called by the transports at admission, never from inside `run_turn`.**
    /// A turn that is refused here never reaches the log, so the refusal is not
    /// a session event — it is a `429` with a window and a retry time, which is
    /// what a client can actually act on, plus a structured log line at the
    /// gate for the operator who wants to see the ceiling working. Terminating
    /// a response instead would put the refusal in the log and cost the client
    /// the status code — and the status code is what a client's error handling
    /// branches on before it reads anything else.
    ///
    /// **The status code alone is not enough, and G10 is the record of why.**
    /// This doc used to call it "the half an agent's HTTP stack already knows
    /// how to back off on"; for the one agent this product exists to serve that
    /// was false. codex does not retry a `429` at all (`retry_429` is hardcoded
    /// `false` at every `RetryConfig` construction site in the pinned tree) and
    /// reads exactly one machine-readable shape: `error.type ==
    /// "usage_limit_reached"` with `error.resets_at` in unix seconds. The body
    /// therefore carries those two spellings beside our own — see
    /// `refuse_over_fair_use` in [`http`](crate::http), which also says why they
    /// ride on this refusal and on no other.
    ///
    /// A membership with no windows short-circuits before the ledger is
    /// touched, so the shipped posture — every project, until an operator
    /// writes one — costs a `Vec::is_empty()` per turn.
    pub async fn fair_use_refusal(
        &self,
        admission: &Admission,
    ) -> Result<Option<FairUseRefusal>, EngineError> {
        if admission.fair_use.is_empty() {
            return Ok(None);
        }
        let refusal = self
            .fair_use
            .would_exceed(&admission.principal, &admission.fair_use, now_ms())
            .await?;
        if let Some(refusal) = &refusal {
            // A log fact, deliberately structured rather than prose: an
            // operator watching a benchmark run needs to be able to count these
            // per window, and a refusal nobody can count is an anecdote.
            tracing::info!(
                project = %admission.principal.project,
                user = %admission.principal.user,
                scope = refusal.scope.wire_name(),
                window = refusal.window.wire_name(),
                quantity = refusal.quantity.wire_name(),
                retry_at_ms = refusal.retry_at_ms,
                "fair use: refusing this turn; no grant was taken and no hold was left"
            );
        }
        Ok(refusal)
    }

    /// Add this turn's booked usage to the membership's rolling counters.
    ///
    /// **Booked usage and not quoted**, which is what makes a window count what
    /// happened rather than what was expected — including an estimate stood in
    /// for a provider that reported nothing, exactly as the settle charges one.
    /// The consequence is stated on [`FairUseLedger`]: the turn that crosses a
    /// cap is served and the next is refused, because reserving up front would
    /// need the hold machinery this seam exists to stay out of.
    ///
    /// Contained rather than fallible, and for a sharper reason than
    /// `repair_settle`'s: this runs *after* the turn's terminal event is
    /// committed and after the settle, so there is nothing left to fail. A `?`
    /// here would throw away an answer the client has already been streamed and
    /// the ledger has already been charged for, in order to report that a
    /// rolling counter did not move — which is a worse outcome than the counter
    /// not moving.
    ///
    /// [`FairUseLedger`]: roundhouse_core::control::FairUseLedger
    pub(super) async fn record_fair_use_draw(
        &self,
        session: &Session<S>,
        response_id: &ResponseId,
        admission: &Admission,
    ) {
        if admission.fair_use.is_empty() {
            return;
        }
        let Some(settlement) = session.state().last_settlement() else {
            return;
        };
        // **This turn's settlement, or nothing.** A failed dispatch terminates
        // its response best-effort — the usual cause is a lost lease — so a turn
        // can reach this line having written no terminal event of its own, and
        // `last_settlement()` then answers with the previous turn's. Drawing
        // that would add one turn's usage to the window twice, which refuses a
        // later turn that had room: the restrictive direction, and the one a
        // ceiling must never err in by accident. The settle is immune to the
        // same shape because it is idempotent by `(session, seq)`; a rolling sum
        // has no position to be idempotent against, so the check is explicit.
        if settlement.response_id != *response_id {
            tracing::debug!(
                %response_id,
                found = %settlement.response_id,
                "fair use: this turn wrote no terminal event, so there is nothing of its \
                 own to draw; the log's last settlement belongs to an earlier turn that has \
                 already been counted"
            );
            return;
        }
        // Priced the same way the settle prices it, through the same function
        // reading the same log — a second pricing rule here would let the
        // dollars a window counts drift from the dollars a project is charged,
        // and the two disagreeing is worse than either being wrong.
        //
        // A turn that reached no provider prices at zero and still draws its
        // tokens, which is right: a steered turn's judge call is real usage on
        // a real provider even though the turn itself dispatched nowhere.
        //
        // `UnpricedSettlement` — a frontier dispatch whose decision recorded no
        // rate card — is where the two seams deliberately part. The settle
        // treats it as a defect, because booking unpriced frontier traffic as
        // free is the one accounting lie the ledger exists to prevent. Here it
        // is a warning and a zero: the tokens still land, so the window is not
        // blind, and abandoning the whole draw over a missing price would lose
        // the token count too — which would let an unpriced turn spend a
        // rolling ceiling for nothing. Stated rather than hidden, because the
        // disagreement is the design.
        let usd = match settled_cost_usd(settlement) {
            Ok(usd) => usd,
            Err(error) => {
                tracing::warn!(
                    project = %admission.principal.project,
                    %error,
                    "fair use: this turn has no recorded rate card, so its tokens count \
                     against the rolling window and its dollars do not"
                );
                0.0
            }
        };
        if let Err(error) = self
            .fair_use
            .record_draw(
                &admission.principal,
                now_ms(),
                settlement.usage.total(),
                usd,
            )
            .await
        {
            tracing::warn!(
                project = %admission.principal.project,
                %error,
                "a fair-use draw could not be recorded; this turn is missing from the \
                 rolling windows, which errs permissive -- see Engine::record_fair_use_draw"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use roundhouse_core::context::ByteTokenizer;
    use roundhouse_core::control::{
        FairUseLedger, FairUseLimit, FairUseTerms, FairUseWindow, MemoryFairUseLedger, Principal,
        TurnCredentials, TurnPolicy,
    };
    use roundhouse_core::event::{Accounting, Usage};
    use roundhouse_core::ids::{SessionId, TurnId};
    use roundhouse_core::item::Item;
    use roundhouse_core::routing::{AffinityPolicy, CacheLedger};
    use roundhouse_core::session::Session;
    use roundhouse_core::store::{MemoryStore, SessionStore};
    use roundhouse_fleet::{EchoFrontierClient, StaticFrontierCatalog};

    use crate::engine::{EchoLocalExecutor, EngineConfig};

    use super::*;

    /// A membership whose 5-hour window admits exactly one turn of the size the
    /// fixture below books.
    fn capped(max_tokens: u64) -> Admission {
        Admission {
            principal: Principal::new("acme", "ada"),
            policy: Arc::new(TurnPolicy::unrestricted()),
            budget: None,
            fair_use: Arc::new(FairUseTerms {
                project: vec![FairUseLimit {
                    window: FairUseWindow::FiveHours,
                    max_tokens: Some(max_tokens),
                    max_usd: None,
                }],
                member: Vec::new(),
            }),
            validation: None,
            credentials: TurnCredentials::unrestricted(),
            budget_counts: Default::default(),
            tiers: None,
        }
    }

    fn engine(fair_use: Arc<dyn FairUseLedger>) -> Engine<MemoryStore, ByteTokenizer> {
        Engine::new(
            Arc::new(MemoryStore::new()),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local")),
            StaticFrontierCatalog::new(vec![]),
            Arc::new(EchoFrontierClient::new("frontier")),
            Arc::new(AffinityPolicy::new()),
            EngineConfig::default(),
        )
        .with_fair_use_ledger(fair_use)
    }

    /// A session holding exactly one completed turn that booked `tokens`.
    async fn session_with_one_completed_turn(
        tokens: u64,
    ) -> (Session<MemoryStore>, roundhouse_core::ids::ResponseId) {
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
        let admitted = session
            .begin_turn(TurnId::new("t1"), vec![Item::user_text("hi")])
            .await
            .unwrap();
        let response_id = admitted.response_id().clone();
        session
            .complete(
                &response_id,
                Some("hi"),
                Usage {
                    input_tokens: tokens,
                    cached_input_tokens: 0,
                    cache_write_tokens: 0,
                    output_tokens: 0,
                    reasoning_tokens: 0,
                    accounting: Accounting::Reported,
                },
                None,
                None,
            )
            .await
            .unwrap();
        (session, response_id)
    }

    /// **The guard.** A turn that wrote no terminal event of its own draws
    /// nothing, rather than drawing whatever the log's last settlement happens
    /// to be.
    ///
    /// Reachable: `run_turn` terminates a failed dispatch best-effort, so a lost
    /// lease leaves this turn with no `ResponseCompleted` or `ResponseIncomplete`
    /// and `last_settlement()` answering with the previous turn's. Drawing that
    /// counts one turn's usage twice — and a rolling sum has no `(session, seq)`
    /// watermark to make the second call a no-op, which is exactly how the
    /// settle beside it is immune to the same shape.
    ///
    /// The direction matters: a double draw refuses a *later* turn that had
    /// room. Erring permissive on a lost draw is the deliberate trade; erring
    /// restrictive on a duplicate one is a ceiling that closes early for a
    /// reason nobody can see.
    #[tokio::test]
    async fn a_turn_that_wrote_no_terminal_event_draws_nothing_of_its_predecessors() {
        let ledger = Arc::new(MemoryFairUseLedger::new());
        let engine = engine(Arc::clone(&ledger) as Arc<dyn FairUseLedger>);
        let admission = capped(100);
        let (session, response_id) = session_with_one_completed_turn(100).await;

        // The turn that really did settle. One draw, and the window is now full.
        engine
            .record_fair_use_draw(&session, &response_id, &admission)
            .await;
        assert!(
            engine.fair_use_refusal(&admission).await.unwrap().is_some(),
            "one turn of 100 tokens fills a 100-token window"
        );

        // PROBE: a *second* turn whose own terminal append was lost. It calls
        // through with its own response id and finds the first turn's
        // settlement — which has already been counted.
        let ghost = roundhouse_core::ids::ResponseId::new("resp_never_committed");
        engine
            .record_fair_use_draw(&session, &ghost, &admission)
            .await;

        // A generous window that one turn's usage fits inside and two do not:
        // if the ghost drew, this is over.
        let roomy = capped(150);
        assert_eq!(
            engine.fair_use_refusal(&roomy).await.unwrap(),
            None,
            "the ghost turn must have drawn nothing; a second draw over the \
             same 100 tokens would put this 150-token window over and refuse a \
             turn that had room"
        );
    }

    /// The control that stops the guard above being "draws never happen".
    ///
    /// Same engine, same admission, a settlement whose response id *does* match
    /// — and the tokens land. Without this, a `record_fair_use_draw` that had
    /// been gutted to an early `return` would satisfy every assertion above.
    #[tokio::test]
    async fn control_a_turn_that_settled_draws_its_own_usage() {
        let ledger = Arc::new(MemoryFairUseLedger::new());
        let engine = engine(Arc::clone(&ledger) as Arc<dyn FairUseLedger>);
        let admission = capped(100);
        let (session, response_id) = session_with_one_completed_turn(100).await;

        assert_eq!(
            engine.fair_use_refusal(&admission).await.unwrap(),
            None,
            "nothing drawn yet"
        );
        engine
            .record_fair_use_draw(&session, &response_id, &admission)
            .await;
        assert!(
            engine.fair_use_refusal(&admission).await.unwrap().is_some(),
            "the matching settlement's usage must reach the window"
        );
    }
}
