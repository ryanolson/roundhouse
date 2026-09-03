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
//! **The outage posture, and why its two halves point opposite ways** (D1
//! R14, pinned here by M13.1's R-F7). A ceiling *check* that cannot reach its
//! store fails **closed**: [`Engine::fair_use_refusal`] returns the error, the
//! transport turns it into a `503` — retryable, because nothing about the
//! request was wrong and the outage clears on its own — and the turn is
//! refused. An operator configured that ceiling on purpose, and a ceiling
//! nobody can read cannot be honoured; waving the turn through would make an
//! outage of our own store the one condition under which a tenant's limit
//! quietly stops existing. A *draw* that cannot be recorded fails **open**:
//! [`Engine::record_fair_use_draw`] logs the reason and returns, because it
//! runs after the answer has been streamed and the project charged, so the
//! only thing left to lose is a counter's update — a bounded under-count for
//! the length of the outage, which is a fact about the outage, where a refused
//! turn would be a fact about nothing. Both halves are pinned by tests against
//! a store taken away mid-test; see `SeveredStore` below for why that is a
//! relay rather than a `SHUTDOWN`.
//!
//! **Where the single-node caution is said, and why not here.** A deployment
//! with no Redis counts its ceilings in one process, and an operator has to be
//! told — but the fact has two halves, and this seam holds only one of them.
//! It knows a ceiling is being enforced (`admission.fair_use` is non-empty);
//! it does not know whether the ledger behind the trait object is shared.
//! `MemoryFairUseLedger` knows both, so the warning lives there and fires the
//! first time it is asked to enforce anything. It used to be a boot-time
//! `if` in `main.rs` over a snapshot of the compiled plane, which is M13's
//! thermo-nuclear review F1: the admin plane `PATCH`es a `fair_use` block onto
//! a live project, so the snapshot was stale by exactly the deployments the
//! caution was for. The test below is the seam's end of that — one warning,
//! whatever route the ceiling arrived by.
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
    use roundhouse_core::control::spend::contract::fresh_principal;
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
    use roundhouse_store_redis::RedisFairUseLedger;
    use roundhouse_store_redis::test_support::url_from_env;

    use axum::response::IntoResponse;

    use crate::engine::{EchoLocalExecutor, EngineConfig};

    use super::*;

    /// A membership whose 5-hour window admits exactly one turn of the size the
    /// fixture below books.
    fn capped(max_tokens: u64) -> Admission {
        capped_for(Principal::new("acme", "ada"), max_tokens)
    }

    /// The same, for a membership named by the caller.
    ///
    /// The gated tests below need a principal nothing else has drawn against,
    /// because their ledger is a *real, shared* Redis whose counters outlive
    /// the process: a fixed project id would let one green run's draws decide
    /// the next run's answer.
    fn capped_for(principal: Principal, max_tokens: u64) -> Admission {
        Admission {
            principal,
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

    /// Everything `tracing::warn!` wrote during one closure, as text.
    ///
    /// The same capture point `main.rs`'s own suite keeps, and here for the
    /// same reason: nothing else in this file reads what `tracing` emits, so
    /// the single-node caution below could be deleted outright without a test
    /// going red. The serialization and the interest-cache rebuild are not
    /// tidiness — `with_default` installs a *thread-local* subscriber, and a
    /// concurrent test evaluating this callsite under the no-op global
    /// dispatcher caches "never interested" for it, which silently drops the
    /// very line the assertion is about. See `main.rs`'s copy for the full
    /// diagnosis.
    fn captured_warnings(f: impl FnOnce()) -> String {
        use std::io;
        use std::sync::Mutex;
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl io::Write for Buf {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for Buf {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
        let _serialized = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            f()
        });
        String::from_utf8(buf.0.lock().unwrap().clone()).expect("tracing output is UTF-8")
    }

    /// **A ceiling this node learned about after boot still says "one node",
    /// exactly once.**
    ///
    /// M13's thermo-nuclear review, F1: the caution used to be a boot-time
    /// `if` over a snapshot of the compiled plane, so a deployment whose
    /// operator added a `fair_use` block through the admin plane an hour later
    /// enforced it per node in silence — the snapshot the `if` read had been
    /// taken before the ceiling existed. This fixture is that deployment: an
    /// engine wired to a per-process ledger, and an admission carrying a
    /// ceiling that no boot-time read could have seen.
    ///
    /// Three assertions, and the middle one is the load-bearing one:
    ///
    /// - a membership with **no** window enforces nothing and must therefore
    ///   say nothing — a caution about a gap nobody is standing in is how a
    ///   real warning gets ignored;
    /// - the first turn judged against a real ceiling warns;
    /// - and the second does not, because a line repeated on every admitted
    ///   request is one an operator filters out and then never sees.
    #[test]
    fn a_ceiling_this_node_only_learns_of_at_enforcement_time_warns_once() {
        let engine = engine(Arc::new(MemoryFairUseLedger::new()));
        let uncapped = Admission {
            fair_use: Arc::new(FairUseTerms::default()),
            ..capped(100)
        };
        let capped = capped(100);

        // CONTROL: nothing configured, nothing enforced, nothing said.
        let quiet = captured_warnings(|| {
            futures::executor::block_on(engine.fair_use_refusal(&uncapped)).unwrap();
        });
        assert!(
            !quiet.contains("THIS PROCESS'S memory"),
            "a membership with no window reaches no ledger and is not what the caution is \
             about; said here it would be noise on every deployment that has none: {quiet}"
        );

        let first = captured_warnings(|| {
            futures::executor::block_on(engine.fair_use_refusal(&capped)).unwrap();
        });
        assert!(
            first.contains("THIS PROCESS'S memory"),
            "the first turn judged against a real ceiling through a per-process ledger must \
             say so, however late the ceiling arrived: {first}"
        );

        let second = captured_warnings(|| {
            futures::executor::block_on(engine.fair_use_refusal(&capped)).unwrap();
        });
        assert!(
            !second.contains("THIS PROCESS'S memory"),
            "and only the first: {second}"
        );
    }

    // -----------------------------------------------------------------------
    // R-F7: the outage posture, pinned against a store taken away mid-test
    // -----------------------------------------------------------------------

    /// A TCP relay in front of the real Redis, so a store can be taken away
    /// mid-test without touching a Redis this suite does not own.
    ///
    /// **Why a relay rather than a `SHUTDOWN`.** The Redis the environment
    /// names is shared with every other gated suite in this workspace, and
    /// stopping it would fail them rather than this one; starting a second
    /// server would put a binary on the test's requirements. What an outage
    /// *is*, from this process's side, is a connection that stops answering
    /// and a reconnect that is refused — which is exactly what dropping the
    /// relay produces, with the store itself untouched.
    struct SeveredStore {
        url: String,
        accept: tokio::task::JoinHandle<()>,
        pipes: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    }

    impl SeveredStore {
        async fn in_front_of(upstream: &str) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            // `redis://host:port/db`, which is the only shape
            // `ROUNDHOUSE_TEST_REDIS_URL` takes in this workspace. Parsed by
            // hand because the `redis` crate is not a dependency of this
            // crate — only of the store's — and a URL parser is not what this
            // test is about.
            let target = upstream
                .trim_start_matches("redis://")
                .split('/')
                .next()
                .expect("a redis URL names a host and a port")
                .to_string();
            let pipes: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let accept = tokio::spawn({
                let pipes = Arc::clone(&pipes);
                async move {
                    loop {
                        let Ok((mut inbound, _)) = listener.accept().await else {
                            return;
                        };
                        let target = target.clone();
                        let pipe = tokio::spawn(async move {
                            let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await
                            else {
                                return;
                            };
                            let _ =
                                tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                        });
                        pipes.lock().unwrap().push(pipe);
                    }
                }
            });
            Self {
                url: format!("redis://127.0.0.1:{port}/"),
                accept,
                pipes,
            }
        }

        /// The outage: stop listening — so a reconnect is refused — and drop
        /// every connection already open.
        fn cut(&self) {
            self.accept.abort();
            for pipe in self.pipes.lock().unwrap().drain(..) {
                pipe.abort();
            }
        }
    }

    async fn severed_ledger() -> (SeveredStore, Arc<RedisFairUseLedger>) {
        let store = SeveredStore::in_front_of(&url_from_env()).await;
        let ledger = RedisFairUseLedger::connect(&store.url)
            .await
            .expect("the relay in front of the test Redis must be reachable");
        (store, Arc::new(ledger))
    }

    /// **A ceiling that cannot be checked refuses the turn, retryably** (D1
    /// R14, folded into M13.1 as R-F7).
    ///
    /// The half of the outage posture that fails *closed*, and the reasoning
    /// is short: an operator configured this ceiling on purpose, and a
    /// ceiling nobody can read cannot be honoured. Waving the turn through
    /// would make an outage of our own store the one condition under which a
    /// tenant's rolling limit does not exist — silently, and for as long as
    /// the outage lasts.
    ///
    /// What it must *not* be is a permanent-looking failure. Nothing about
    /// the request was wrong, and the condition clears on its own, so the
    /// status is the one an agent's stack already treats as "come back":
    /// `503`, distinct from the `429` a spent window answers with, which is
    /// the control asserted alongside it. A `500` would tell a client its
    /// request was the problem.
    #[tokio::test]
    #[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
    async fn a_ceiling_that_cannot_be_checked_refuses_the_turn_retryably() {
        let (store, ledger) = severed_ledger().await;
        let engine = engine(Arc::clone(&ledger) as Arc<dyn FairUseLedger>);
        let admission = capped_for(fresh_principal("ada"), 100);

        // CONTROL: reachable and nothing drawn, so the turn is admitted and
        // the transport says nothing at all.
        assert_eq!(engine.fair_use_refusal(&admission).await.unwrap(), None);
        assert!(
            crate::http::refuse_over_fair_use(&engine, &admission)
                .await
                .is_ok()
        );

        // CONTROL: reachable and spent, which is the *other* refusal — a 429
        // naming the window. Without this the assertion below would pass for
        // a transport that answered 503 to every fair-use refusal.
        ledger
            .record_draw(&admission.principal, now_ms(), 100, 0.0)
            .await
            .unwrap();
        let spent = crate::http::refuse_over_fair_use(&engine, &admission)
            .await
            .expect_err("a spent window refuses");
        assert_eq!(
            spent.into_response().status(),
            axum::http::StatusCode::TOO_MANY_REQUESTS
        );

        store.cut();

        // The claim: the ledger cannot answer, and the turn is refused rather
        // than served.
        let error = engine
            .fair_use_refusal(&admission)
            .await
            .expect_err("a ceiling check that cannot reach its store must fail");
        assert!(
            matches!(error, EngineError::FairUse(_)),
            "and it must fail as the fair-use ledger being unreachable, which \
             is what tells an operator which store is down: {error}"
        );

        let outage = crate::http::refuse_over_fair_use(&engine, &admission)
            .await
            .expect_err("and the transport must refuse the request")
            .into_response();
        assert_eq!(
            outage.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "an unreachable ledger is a retryable outage, not a bad request \
             and not a permanent failure"
        );
        let body = axum::body::to_bytes(outage.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["error"]["code"], "fair_use_unavailable",
            "and it names the store that is down rather than the tenant"
        );
    }

    /// **A draw that cannot be recorded is logged, and the turn stands** (D1
    /// R14, R-F7).
    ///
    /// The half that fails *open*, and the asymmetry with the check above is
    /// the whole ruling. This runs after the turn's terminal event is
    /// committed and after the settle: the answer has been streamed and the
    /// project has been charged. Failing here can only lose a rolling
    /// counter's update, which under-counts by one turn for the length of the
    /// outage — a bounded, honest consequence *of* the outage. Failing the
    /// turn instead would throw away work already done and already paid for
    /// in order to report that a counter did not move.
    ///
    /// What must not happen is silence: an operator reading a window that is
    /// low has to be able to find out that draws were dropped, which is why
    /// the assertion is on the warning rather than only on the call returning.
    ///
    /// A plain `#[test]` driving its own runtime rather than `#[tokio::test]`,
    /// because `captured_warnings` installs a *thread-local* subscriber around
    /// a synchronous closure — see its own doc for why that serialization is
    /// not tidiness — and `block_on` inside the closure is what puts the
    /// ledger's I/O on the thread the subscriber is installed on.
    #[test]
    #[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
    fn a_draw_that_cannot_be_recorded_is_logged_and_the_turn_still_stands() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (store, engine, session, response_id, admission) = runtime.block_on(async {
            let (store, ledger) = severed_ledger().await;
            let engine = engine(ledger as Arc<dyn FairUseLedger>);
            let admission = capped_for(fresh_principal("ada"), 100);
            let (session, response_id) = session_with_one_completed_turn(100).await;
            (store, engine, session, response_id, admission)
        });

        // CONTROL: while the store is reachable the draw lands, and the
        // window it filled refuses the next turn. Without it, a
        // `record_fair_use_draw` that had been gutted would satisfy the claim
        // below by warning about nothing.
        let quiet = captured_warnings(|| {
            runtime.block_on(engine.record_fair_use_draw(&session, &response_id, &admission));
        });
        assert!(
            !quiet.contains("could not be recorded"),
            "a draw against a reachable ledger warns about nothing: {quiet}"
        );
        assert!(
            runtime
                .block_on(engine.fair_use_refusal(&admission))
                .unwrap()
                .is_some(),
            "the control draw must actually have reached the ledger"
        );

        store.cut();

        // The claim: the same call against a store that is gone returns, and
        // says so.
        let warnings = captured_warnings(|| {
            runtime.block_on(engine.record_fair_use_draw(&session, &response_id, &admission));
        });
        assert!(
            warnings.contains("could not be recorded"),
            "a draw that cannot reach its store must leave a warning naming \
             the project, not vanish: {warnings}"
        );
        assert!(
            warnings.contains("errs permissive"),
            "and the warning must say which way it erred, because that is the \
             half an operator reading a low window needs: {warnings}"
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
