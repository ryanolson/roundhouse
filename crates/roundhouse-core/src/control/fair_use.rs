// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Fair-use windows: the rolling ceilings a frontier lab's own session limits
//! are shaped like.
//!
//! **A separate seam from [`budget`](super::budget), deliberately, and the
//! reason is a hazard this repo has already been bitten by.** A budget is a
//! ledger of committed dollars with an authorization hold and a *calendar*
//! window, and its arithmetic is load-bearing enough that the admin plane
//! refuses to `PATCH` a project's `BudgetWindow` at all — reading a `balance()`
//! under the wrong window destroys committed spend. Fair use asks a different
//! question ("has this principal drawn too much in the last five hours?") over
//! a *rolling* window with no hold, no settle, and no money to lose. Expressing
//! it as a fourth `BudgetWindow` variant would put a rolling window inside the
//! type whose calendar arithmetic that refusal protects, which is exactly the
//! shape of the M8 hazard. So: its own trait, its own store, its own draws.
//!
//! The 2026-08-24 addendum to `PLAN-frontier-selection.md` is what this
//! implements. Budgets are *unlimited* for the benchmark projects this phase
//! runs — the reconciliation view already reports an unenforced basis honestly
//! — and what replaces a hard dollar cap in the architecture is a rolling
//! 5-hour / 24-hour / 7-day window per project and per member, each optionally
//! capping tokens, dollars, or both. **The enforcement is real and the ceilings
//! are absent by default**: a project that writes no `fair_use` block never
//! reaches this module at all, exactly as a project with no `budget` never
//! reaches the spend ledger.
//!
//! # What a refusal is
//!
//! Admission-time, before any grant is taken, so a refused turn leaves no hold
//! to lapse. It is a `429` naming the window and the earliest time that window
//! could have room — retryable, like every other refusal here, because a
//! rolling window clears on its own and telling a client *when* is the
//! difference between a backoff and a poll.
//!
//! # What is deferred, and what would unblock it
//!
//! [`MemoryFairUseLedger`] is the only backing store in this milestone, which
//! means the buckets live in one process and two nodes enforce two independent
//! ceilings. That is honest for M10.1 — this phase's benchmark runs on one node
//! — and it is the same shape as the M2 choice between
//! [`MemoryStore`](crate::store::MemoryStore) and Redis, and as the admin
//! directory's own deferral.
//!
//! **The unlock condition, so the next person does not have to re-derive it:**
//! fair use across nodes is only true with *shared* buckets, so the Redis
//! implementation is wanted the moment a second node serves the same project.
//! The shape is already decided by this trait — `record_draw` is a
//! read-modify-write on one bucket and `would_exceed` is a suffix sum over a
//! bounded number of them, which is one Lua script each, the same way
//! `RedisSpendLedger` expresses a grant. What is *not* decided is the key
//! layout, and it is the only interesting question: bucket-per-key costs one
//! `INCRBY` and a `MGET` of at most 2016 keys for the 7-day window, while a
//! hash-per-scope costs one `HINCRBY` and one `HGETALL` but needs a pruning
//! pass nothing currently owns. Until that lands, the boot warning at the
//! composition root says single-node enforcement out loud — the same honesty
//! mechanism the directory store uses, and for the same reason: a ceiling
//! everyone believes in and nothing enforces is worse than no ceiling.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::control::{Principal, ProjectId, UserId};

/// How wide a time bucket is.
///
/// **This is the whole of the staircase error, and it is one-sided by
/// construction.** Draws are accumulated per bucket rather than per turn, and a
/// window sums whole buckets — including the one that only partially overlaps
/// the window's trailing edge. So a draw made 5 hours and 4 minutes ago can
/// still count against a 5-hour window: the cap refuses *slightly early* and
/// never late, by at most one bucket width.
///
/// Five minutes on a five-hour window is 1.7%, which is the smallest window
/// this module offers and therefore the worst case. The alternative — keeping
/// every draw and scanning them per turn — buys exactness at the cost of an
/// unbounded per-principal list scanned on the admission path of every turn,
/// for a difference no operator can perceive in a limit whose whole purpose is
/// to be approximately like a frontier lab's. Erring early rather than late is
/// the deliberate half: a cap that leaks is a cap nobody trusts.
pub const BUCKET_MS: u64 = 5 * 60_000;

/// A rolling window an operator may cap.
///
/// Three fixed spans rather than an arbitrary duration, and they are the three
/// the frontier labs' own session limits use. A free-form duration would be a
/// knob whose only honest default is "whatever the lab you are mimicking uses",
/// and it would let an operator write a window shorter than [`BUCKET_MS`],
/// where the staircase error stops being a rounding error and becomes the whole
/// measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FairUseWindow {
    #[serde(rename = "5h")]
    FiveHours,
    #[serde(rename = "24h")]
    TwentyFourHours,
    #[serde(rename = "7d")]
    SevenDays,
}

impl FairUseWindow {
    /// Every window, narrowest first — which is also the order they are
    /// checked in; see `FairUseTerms::exceeded_by`, which relies on it.
    pub const ALL: [FairUseWindow; 3] = [
        FairUseWindow::FiveHours,
        FairUseWindow::TwentyFourHours,
        FairUseWindow::SevenDays,
    ];

    pub fn span_ms(self) -> u64 {
        match self {
            FairUseWindow::FiveHours => 5 * 60 * 60_000,
            FairUseWindow::TwentyFourHours => 24 * 60 * 60_000,
            FairUseWindow::SevenDays => 7 * 24 * 60 * 60_000,
        }
    }

    /// How a configuration file spells this window, and how a refusal names it.
    ///
    /// Pinned by a test against what `serde` writes, for the reason
    /// `WireProtocol::wire_name` in `roundhouse-fleet` gives for its own: a
    /// refusal that named `FiveHours` would point an operator at a word that
    /// appears in no file they can edit.
    pub fn wire_name(self) -> &'static str {
        match self {
            FairUseWindow::FiveHours => "5h",
            FairUseWindow::TwentyFourHours => "24h",
            FairUseWindow::SevenDays => "7d",
        }
    }
}

/// One capped window: how much may be drawn inside it.
///
/// Both caps optional and at least one required — enforced where the config is
/// judged, because an entry naming neither is a window that refuses nothing
/// while reading like a limit. Both present means both bind, and the first one
/// to be exceeded is what the refusal names.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FairUseLimit {
    pub window: FairUseWindow,
    pub max_tokens: Option<u64>,
    pub max_usd: Option<f64>,
}

/// Which ceiling refused a turn.
///
/// Two scopes rather than one merged list, and the distinction is the whole
/// content of "a member ceiling binds even when the project has room". A
/// refusal that could not say which one was hit would send an operator to raise
/// the project's limit when the member's is what stopped the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FairUseScope {
    Project,
    Member,
}

impl FairUseScope {
    pub fn wire_name(self) -> &'static str {
        match self {
            FairUseScope::Project => "project",
            FairUseScope::Member => "member",
        }
    }
}

/// Which quantity a window ran out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FairUseQuantity {
    Tokens,
    Usd,
}

impl FairUseQuantity {
    pub fn wire_name(self) -> &'static str {
        match self {
            FairUseQuantity::Tokens => "tokens",
            FairUseQuantity::Usd => "usd",
        }
    }
}

/// The fair-use ceilings in force for one membership's turn.
///
/// **Two named lists and not one set.** They are two ceilings that both bind,
/// mirroring the budget ladder's project limit and member allocation, and a
/// merged list could not express "the project has room and the member does
/// not" — which is the case the whole member tier exists for.
///
/// `None` for either half is *no ceiling at that scope*, not a ceiling of zero,
/// the same distinction [`Allocation::Pooled`](super::budget::Allocation) draws
/// and for the same reason: getting it backwards is the difference between a
/// member who may draw everything and one who may draw nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FairUseTerms {
    pub project: Vec<FairUseLimit>,
    pub member: Vec<FairUseLimit>,
}

impl FairUseTerms {
    /// Whether this membership has any fair-use ceiling at all.
    ///
    /// The engine's early return reads this rather than `Option<FairUseTerms>`
    /// being `None`, so a project that declared an empty `windows` list and one
    /// that declared no block are one state downstream rather than two
    /// spellings of it.
    pub fn is_empty(&self) -> bool {
        self.project.is_empty() && self.member.is_empty()
    }

    /// The narrowest window that refuses this turn, if any.
    ///
    /// **Windows ascending, and within each window the project before the
    /// member.** Narrowest first because the narrowest is the soonest to clear,
    /// so its retry time is the one a client can actually act on — naming a
    /// 7-day window when a 5-hour one is also spent would send an agent away
    /// for a week it does not have to wait. Project before member inside one
    /// window is arbitrary and stated so a reader does not look for a reason;
    /// what is *not* arbitrary is that the member list is consulted at all when
    /// the project's passes, which is the ladder's own rule.
    ///
    /// `draws` answers "what has this scope drawn inside this window", which is
    /// the only thing a ledger has to supply.
    fn exceeded_by(
        &self,
        mut draws: impl FnMut(FairUseScope, FairUseWindow) -> Drawn,
    ) -> Option<(FairUseScope, FairUseLimit, FairUseQuantity, Drawn)> {
        for window in FairUseWindow::ALL {
            for (scope, limits) in [
                (FairUseScope::Project, &self.project),
                (FairUseScope::Member, &self.member),
            ] {
                let Some(limit) = limits.iter().find(|limit| limit.window == window) else {
                    continue;
                };
                let drawn = draws(scope, window);
                // Tokens before dollars where both are capped, because a
                // token cap is the one an agent can reason about: it is the
                // quantity in its own context window.
                if limit.max_tokens.is_some_and(|max| drawn.tokens >= max) {
                    return Some((scope, *limit, FairUseQuantity::Tokens, drawn));
                }
                if limit.max_usd.is_some_and(|max| drawn.usd >= max) {
                    return Some((scope, *limit, FairUseQuantity::Usd, drawn));
                }
            }
        }
        None
    }
}

/// What one scope has drawn inside one window.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Drawn {
    pub tokens: u64,
    pub usd: f64,
    /// The earliest time this window could have room again, or `None` where it
    /// is not over any cap.
    ///
    /// Computed by the ledger because only the ledger knows *when* each draw
    /// landed. See [`MemoryFairUseLedger`] for the arithmetic and for the one
    /// honest caveat: it is the earliest time the *current* draws could clear,
    /// not a promise that nothing lands in between.
    pub retry_at_ms: Option<u64>,
}

/// A turn refused because a rolling window is spent.
///
/// Every field is on the wire, because a client that is told only "429" can do
/// nothing but poll — and an agent loop that polls a spent 7-day window is the
/// failure this whole mechanism exists to make rare.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FairUseRefusal {
    pub scope: FairUseScope,
    pub window: FairUseWindow,
    pub quantity: FairUseQuantity,
    /// The earliest epoch-millisecond time at which the named window's current
    /// draws will have aged out far enough to be under the cap.
    pub retry_at_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum FairUseError {
    #[error("`{field}` must be a finite, non-negative number, got {value}")]
    InvalidAmount { field: &'static str, value: f64 },
    #[error("fair-use ledger backend failure: {0}")]
    Backend(#[from] anyhow::Error),
}

/// Rolling per-principal draw counters, checked before a turn is granted.
///
/// Deliberately two operations, for the reason [`SpendLedger`] is three: both
/// are expressible as one atomic step in a store that has one, and everything
/// the control plane wants to *say* about fair use arrives as [`FairUseTerms`]
/// on the call rather than as configuration a backend has to hold and reload.
///
/// **Draws are recorded at settle, from the turn's booked usage**, so what a
/// window counts is what actually happened rather than what was quoted. The
/// consequence is deliberate and worth stating: the turn that crosses a cap is
/// served, and the *next* one is refused. Reserving against the cap up front
/// would need a hold, a TTL and a release — the whole budget machinery this
/// seam exists to stay out of — to bound an overshoot of one turn.
///
/// [`SpendLedger`]: super::spend::SpendLedger
#[async_trait]
pub trait FairUseLedger: Send + Sync + 'static {
    /// Add one turn's booked usage to this principal's rolling counters.
    ///
    /// `at_ms` is supplied rather than read from a clock inside the ledger, for
    /// the reason [`GrantRequest::now_ms`](super::spend::GrantRequest) is: a
    /// window boundary has to be reachable in a test without waiting for one,
    /// and `windows_roll_rather_than_reset` is untestable otherwise.
    ///
    /// The [`Principal`] carries the project, so the ledger updates both the
    /// project's counters and the member's from one call — two arguments for
    /// one fact would be two things able to disagree.
    async fn record_draw(
        &self,
        principal: &Principal,
        at_ms: u64,
        tokens: u64,
        usd: f64,
    ) -> Result<(), FairUseError>;

    /// The narrowest window that would refuse a turn admitted at `now_ms`, if
    /// any.
    async fn would_exceed(
        &self,
        principal: &Principal,
        terms: &FairUseTerms,
        now_ms: u64,
    ) -> Result<Option<FairUseRefusal>, FairUseError>;
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

/// One scope's draws, bucketed.
///
/// A `BTreeMap` keyed by bucket index rather than a ring buffer: the map is
/// ordered, so a window sum is a range query and the retry calculation is a
/// walk from the oldest bucket forward, and it holds only buckets that saw a
/// draw — an idle principal costs one empty map rather than 2016 zeroes.
#[derive(Debug, Default)]
struct Buckets {
    by_index: BTreeMap<u64, (u64, f64)>,
}

impl Buckets {
    fn record(&mut self, at_ms: u64, tokens: u64, usd: f64) {
        let entry = self.by_index.entry(at_ms / BUCKET_MS).or_insert((0, 0.0));
        entry.0 = entry.0.saturating_add(tokens);
        entry.1 += usd;
    }

    /// Drop everything older than the widest window, so an idle-then-busy
    /// principal's map does not grow without bound.
    ///
    /// Here rather than in a sweeper for the reason the spend ledger expires
    /// holds lazily: the next call to touch this principal is what prunes it,
    /// which needs no background task and no cross-principal index. A principal
    /// that is never touched again holds at most one 7-day window of buckets —
    /// 2016 of them, a few tens of kilobytes — until the process ends.
    fn prune(&mut self, now_ms: u64) {
        let widest = FairUseWindow::SevenDays.span_ms();
        let oldest = now_ms.saturating_sub(widest) / BUCKET_MS;
        self.by_index = self.by_index.split_off(&oldest);
    }

    /// The first bucket index inside `window` at `now_ms`.
    ///
    /// Floor division, which is what includes the partially-overlapping
    /// trailing bucket whole — the staircase error [`BUCKET_MS`] documents, and
    /// the direction that refuses early rather than late.
    fn first_index(window: FairUseWindow, now_ms: u64) -> u64 {
        now_ms.saturating_sub(window.span_ms()) / BUCKET_MS
    }

    fn drawn(&self, window: FairUseWindow, now_ms: u64, limit: &FairUseLimit) -> Drawn {
        let first = Self::first_index(window, now_ms);
        let inside: Vec<(&u64, &(u64, f64))> = self.by_index.range(first..).collect();
        let tokens = inside
            .iter()
            .fold(0u64, |sum, (_, (tokens, _))| sum.saturating_add(*tokens));
        let usd = inside.iter().fold(0.0, |sum, (_, (_, usd))| sum + usd);
        Drawn {
            tokens,
            usd,
            retry_at_ms: earliest_retry_ms(&inside, window, limit),
        }
    }
}

/// The earliest time the window's *current* draws will be under both caps.
///
/// Walk the buckets oldest-first, dropping each in turn, until what remains is
/// under every cap the limit names; the answer is the moment that last-dropped
/// bucket falls out of the window, which is its own end plus the window's span
/// (the inclusion rule in [`Buckets::first_index`] keeps a bucket until its
/// *end* has passed out of the window).
///
/// **What this is not is a promise.** It is the earliest time these draws could
/// clear, and a turn served in between pushes it out again — which is honest
/// and is what a client needs: a `retry_at_ms` that was a guarantee would have
/// to reserve the future, and this seam takes no holds. Stated on the wire as
/// "earliest", never as "at".
///
/// `None` when nothing is over a cap, which is what makes the refusal path and
/// the ordinary path one function rather than two that could disagree about
/// which buckets are inside the window.
fn earliest_retry_ms(
    inside: &[(&u64, &(u64, f64))],
    window: FairUseWindow,
    limit: &FairUseLimit,
) -> Option<u64> {
    let over = |tokens: u64, usd: f64| {
        limit.max_tokens.is_some_and(|max| tokens >= max)
            || limit.max_usd.is_some_and(|max| usd >= max)
    };
    let mut tokens = inside
        .iter()
        .fold(0u64, |sum, (_, (tokens, _))| sum.saturating_add(*tokens));
    let mut usd = inside.iter().fold(0.0, |sum, (_, (_, usd))| sum + usd);
    if !over(tokens, usd) {
        return None;
    }
    for (index, (bucket_tokens, bucket_usd)) in inside {
        tokens = tokens.saturating_sub(*bucket_tokens);
        usd -= bucket_usd;
        if !over(tokens, usd) {
            // This bucket's end, pushed out of the window by its full span.
            return Some((**index + 1) * BUCKET_MS + window.span_ms());
        }
    }
    // Every bucket dropped and still over: only reachable with a cap of zero
    // or below, which the config boundary refuses — a window that can never
    // have room is a filter wearing a limit's clothes. Answered honestly
    // rather than with a panic, because this type is public and a
    // hand-assembled `FairUseLimit` carries the obligation itself.
    None
}

/// Rolling draw counters in this process's memory.
///
/// Enforces every ceiling correctly for the scope it covers — one node — and
/// says so at boot. See the module doc for the unlock condition.
#[derive(Debug, Default)]
pub struct MemoryFairUseLedger {
    scopes: Mutex<HashMap<(ProjectId, Option<UserId>), Buckets>>,
}

impl MemoryFairUseLedger {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl FairUseLedger for MemoryFairUseLedger {
    async fn record_draw(
        &self,
        principal: &Principal,
        at_ms: u64,
        tokens: u64,
        usd: f64,
    ) -> Result<(), FairUseError> {
        // Refused rather than clamped, for the reason `SpendError::check_amount`
        // gives: a `NaN` loses every comparison it is part of, so a `NaN` that
        // reached these counters would not blow up — it would make the window
        // sum `NaN`, which is never `>=` any cap, and the ceiling would
        // silently stop existing.
        if !usd.is_finite() || usd < 0.0 {
            return Err(FairUseError::InvalidAmount {
                field: "usd",
                value: usd,
            });
        }
        let mut scopes = self.scopes.lock().await;
        // Both scopes from one call: the project's counters and this member's.
        // Two calls could record one and not the other, and a member ceiling
        // enforced against a project counter is not a member ceiling.
        for user in [None, Some(principal.user.clone())] {
            let buckets = scopes.entry((principal.project.clone(), user)).or_default();
            buckets.record(at_ms, tokens, usd);
            buckets.prune(at_ms);
        }
        Ok(())
    }

    async fn would_exceed(
        &self,
        principal: &Principal,
        terms: &FairUseTerms,
        now_ms: u64,
    ) -> Result<Option<FairUseRefusal>, FairUseError> {
        if terms.is_empty() {
            return Ok(None);
        }
        let scopes = self.scopes.lock().await;
        let empty = Buckets::default();
        let refused = terms.exceeded_by(|scope, window| {
            let key = (
                principal.project.clone(),
                match scope {
                    FairUseScope::Project => None,
                    FairUseScope::Member => Some(principal.user.clone()),
                },
            );
            let buckets = scopes.get(&key).unwrap_or(&empty);
            let limit = match scope {
                FairUseScope::Project => &terms.project,
                FairUseScope::Member => &terms.member,
            }
            .iter()
            .find(|limit| limit.window == window)
            .copied()
            .expect("exceeded_by only asks about a window it found a limit for");
            buckets.drawn(window, now_ms, &limit)
        });
        Ok(
            refused.map(|(scope, limit, quantity, drawn)| FairUseRefusal {
                scope,
                window: limit.window,
                quantity,
                // Present whenever the limit was exceeded, which is the only way
                // this arm is reached — `drawn` computes both from one walk of the
                // same buckets, so the sum that refused and the retry time that
                // explains it cannot disagree about which draws are inside.
                retry_at_ms: drawn.retry_at_ms.unwrap_or(now_ms),
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = 60_000;
    const HOUR: u64 = 60 * MINUTE;

    fn ada() -> Principal {
        Principal::new("acme", "ada")
    }

    fn bob() -> Principal {
        Principal::new("acme", "bob")
    }

    fn tokens(window: FairUseWindow, max: u64) -> FairUseLimit {
        FairUseLimit {
            window,
            max_tokens: Some(max),
            max_usd: None,
        }
    }

    fn project_only(limits: Vec<FairUseLimit>) -> FairUseTerms {
        FairUseTerms {
            project: limits,
            member: Vec::new(),
        }
    }

    async fn refused(
        ledger: &MemoryFairUseLedger,
        principal: &Principal,
        terms: &FairUseTerms,
        now_ms: u64,
    ) -> Option<FairUseRefusal> {
        ledger.would_exceed(principal, terms, now_ms).await.unwrap()
    }

    /// **The claim.** A turn over the 5-hour window is refused, and the refusal
    /// carries a time a client can wait until.
    #[tokio::test]
    async fn a_turn_over_the_5h_window_is_refused_with_the_earliest_retry_time() {
        let ledger = MemoryFairUseLedger::new();
        let terms = project_only(vec![tokens(FairUseWindow::FiveHours, 1_000)]);

        // One draw at t=0 that lands exactly on the cap.
        ledger.record_draw(&ada(), 0, 1_000, 0.0).await.unwrap();

        let hit = refused(&ledger, &ada(), &terms, HOUR)
            .await
            .expect("the window is spent");
        assert_eq!(hit.window, FairUseWindow::FiveHours);
        assert_eq!(hit.scope, FairUseScope::Project);
        assert_eq!(hit.quantity, FairUseQuantity::Tokens);
        // The draw sits in bucket 0, whose end is BUCKET_MS; it leaves a
        // 5-hour window BUCKET_MS + 5h after the epoch.
        assert_eq!(hit.retry_at_ms, BUCKET_MS + 5 * HOUR);

        // CONTROL: the same ledger, the same draw, one turn earlier — under the
        // cap, so nothing is refused. Without this, a `would_exceed` that
        // refused unconditionally would pass the assertion above.
        let generous = project_only(vec![tokens(FairUseWindow::FiveHours, 1_001)]);
        assert_eq!(refused(&ledger, &ada(), &generous, HOUR).await, None);
    }

    /// The window rolls: a draw that has aged past the span stops counting, and
    /// the identical request that was refused is served.
    #[tokio::test]
    async fn windows_roll_rather_than_reset() {
        let ledger = MemoryFairUseLedger::new();
        let terms = project_only(vec![tokens(FairUseWindow::FiveHours, 1_000)]);
        ledger.record_draw(&ada(), 0, 1_000, 0.0).await.unwrap();

        // Just inside: still refused. This is the assertion a *calendar* window
        // would fail — a 5-hour window anchored to a clock boundary would have
        // reset at some fixed hour regardless of when the draw landed.
        assert!(refused(&ledger, &ada(), &terms, 5 * HOUR).await.is_some());

        // Past the retry time the refusal named, and the same request is
        // served. Asserting *at* the named time rather than at some later
        // round number is what makes `retry_at_ms` a number rather than a
        // gesture.
        assert_eq!(
            refused(&ledger, &ada(), &terms, BUCKET_MS + 5 * HOUR).await,
            None
        );
    }

    /// **The member ceiling binds even when the project has room.**
    ///
    /// The project here has *no* fair-use limit at all, so nothing about the
    /// project's counters can be what refuses the turn — which is what stops
    /// this passing for the wrong reason on a ledger that merged the two
    /// scopes.
    #[tokio::test]
    async fn the_member_window_binds_even_when_the_projects_has_room() {
        let ledger = MemoryFairUseLedger::new();
        let terms = FairUseTerms {
            project: vec![tokens(FairUseWindow::FiveHours, 1_000_000)],
            member: vec![tokens(FairUseWindow::FiveHours, 100)],
        };

        ledger.record_draw(&ada(), 0, 100, 0.0).await.unwrap();

        let hit = refused(&ledger, &ada(), &terms, HOUR)
            .await
            .expect("ada is over her own ceiling");
        assert_eq!(hit.scope, FairUseScope::Member);
        assert_eq!(
            hit.window,
            FairUseWindow::FiveHours,
            "and the refusal names the member's window, because raising the \
             project's would change nothing"
        );

        // CONTROL: the other member of the same project, under the identical
        // terms, at the identical instant. `bob` has drawn nothing, so he is
        // served — which is what makes the refusal above about `ada`'s own
        // counters rather than about the project's.
        assert_eq!(refused(&ledger, &bob(), &terms, HOUR).await, None);

        // And the project's own counters really did move: `ada`'s draw is in
        // the project total too, so the two scopes are two counters over one
        // draw rather than one counter read twice.
        let tight_project = project_only(vec![tokens(FairUseWindow::FiveHours, 100)]);
        assert_eq!(
            refused(&ledger, &bob(), &tight_project, HOUR)
                .await
                .map(|refusal| refusal.scope),
            Some(FairUseScope::Project),
            "bob has drawn nothing of his own and is still refused by the \
             project's window, which ada filled"
        );
    }

    /// **The narrowest window is checked first and is what the refusal names.**
    ///
    /// Every window is over its cap here, so the answer is entirely about
    /// order. Naming the 7-day one would send an agent away for a week when the
    /// 5-hour one clears first.
    #[tokio::test]
    async fn the_smallest_window_is_checked_first_and_named_in_the_refusal() {
        let ledger = MemoryFairUseLedger::new();
        let terms = project_only(vec![
            tokens(FairUseWindow::SevenDays, 10),
            tokens(FairUseWindow::TwentyFourHours, 10),
            tokens(FairUseWindow::FiveHours, 10),
        ]);
        ledger.record_draw(&ada(), 0, 50, 0.0).await.unwrap();

        let hit = refused(&ledger, &ada(), &terms, HOUR)
            .await
            .expect("every window is spent");
        assert_eq!(hit.window, FairUseWindow::FiveHours);
        assert!(hit.retry_at_ms < BUCKET_MS + 24 * HOUR);

        // CONTROL: the 5-hour window rolled off, so the next-narrowest is what
        // answers. Without this, a `would_exceed` that always returned the
        // first element of a hard-coded list would satisfy the assertion above.
        let hit = refused(&ledger, &ada(), &terms, BUCKET_MS + 6 * HOUR)
            .await
            .expect("the wider windows are still spent");
        assert_eq!(hit.window, FairUseWindow::TwentyFourHours);
    }

    /// A dollar cap and a token cap on one window both bind.
    #[tokio::test]
    async fn either_cap_can_be_the_one_that_refuses() {
        let ledger = MemoryFairUseLedger::new();
        let terms = project_only(vec![FairUseLimit {
            window: FairUseWindow::FiveHours,
            max_tokens: Some(1_000_000),
            max_usd: Some(5.0),
        }]);
        ledger.record_draw(&ada(), 0, 10, 5.0).await.unwrap();

        assert_eq!(
            refused(&ledger, &ada(), &terms, HOUR)
                .await
                .map(|refusal| refusal.quantity),
            Some(FairUseQuantity::Usd),
            "ten tokens is nowhere near the token cap; the dollars are what ran out"
        );
    }

    /// A membership with no fair-use block reaches no counter and is never
    /// refused — the shipped posture, and the one every project has until an
    /// operator writes a window down.
    #[tokio::test]
    async fn a_membership_with_no_windows_is_never_refused() {
        let ledger = MemoryFairUseLedger::new();
        ledger
            .record_draw(&ada(), 0, u64::MAX, 1_000_000.0)
            .await
            .unwrap();
        assert_eq!(
            refused(&ledger, &ada(), &FairUseTerms::default(), HOUR).await,
            None
        );
    }

    /// A `NaN` cannot enter the counters.
    ///
    /// It would not blow up: it would make every window sum `NaN`, which is
    /// never `>=` any cap, and the ceiling would silently stop existing — the
    /// same fail-open `SpendError::check_amount` exists to prevent one seam
    /// over.
    #[tokio::test]
    async fn a_nonfinite_draw_is_refused_rather_than_silently_disabling_the_cap() {
        let ledger = MemoryFairUseLedger::new();
        assert!(ledger.record_draw(&ada(), 0, 1, f64::NAN).await.is_err());
        assert!(ledger.record_draw(&ada(), 0, 1, -1.0).await.is_err());

        // CONTROL, and it is the load-bearing half: the counters are untouched,
        // so a refused draw is refused rather than half-applied.
        let terms = project_only(vec![FairUseLimit {
            window: FairUseWindow::FiveHours,
            max_tokens: None,
            max_usd: Some(0.01),
        }]);
        assert_eq!(refused(&ledger, &ada(), &terms, HOUR).await, None);
    }

    /// The file's spelling of a window and the refusal's are one string.
    #[test]
    fn window_names_are_what_a_config_file_would_write() {
        for (window, expected) in [
            (FairUseWindow::FiveHours, "5h"),
            (FairUseWindow::TwentyFourHours, "24h"),
            (FairUseWindow::SevenDays, "7d"),
        ] {
            let json = serde_json::to_string(&window).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
            assert_eq!(
                serde_json::from_str::<FairUseWindow>(&json).unwrap(),
                window
            );
            assert_eq!(window.wire_name(), expected);
        }
        // And they are in ascending order, which `exceeded_by` relies on for
        // "narrowest first" to mean what it says.
        let spans: Vec<u64> = FairUseWindow::ALL.iter().map(|w| w.span_ms()).collect();
        assert!(spans.windows(2).all(|pair| pair[0] < pair[1]));
    }

    /// The staircase error is bounded and one-sided: early, never late.
    #[tokio::test]
    async fn a_window_refuses_early_rather_than_late() {
        let ledger = MemoryFairUseLedger::new();
        let terms = project_only(vec![tokens(FairUseWindow::FiveHours, 10)]);
        // A draw at the very start of a bucket. Its bucket is included until
        // the bucket's *end* leaves the window, so it counts for up to one
        // bucket width longer than a per-draw ledger would say.
        ledger.record_draw(&ada(), 0, 10, 0.0).await.unwrap();

        assert!(
            refused(&ledger, &ada(), &terms, 5 * HOUR + BUCKET_MS - 1)
                .await
                .is_some(),
            "still counted a whisker before the bucket ages out -- early"
        );
        assert_eq!(
            refused(&ledger, &ada(), &terms, 5 * HOUR + BUCKET_MS).await,
            None,
            "and never later than one bucket width past the span"
        );
    }
}
