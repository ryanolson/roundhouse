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
//! # Where the buckets live
//!
//! Two backing stores, and the composition root picks between them by exactly
//! the rule it picks a session store and a spend ledger by, and by nothing
//! else: with `ROUNDHOUSE_REDIS_URL` set, `RedisFairUseLedger` in
//! `roundhouse-store-redis` counts into buckets every node of the deployment
//! shares; without it, [`MemoryFairUseLedger`] counts into this process's own
//! memory and warns the first time it enforces a ceiling.
//!
//! **Whether a ceiling is configured is deliberately not part of that
//! choice.** It was, until M13's thermo-nuclear review: the boot site read it
//! once from a plane the admin API `PATCH`es at runtime, so a deployment that
//! started with a Redis and no `fair_use` block anywhere counted every
//! later-added ceiling in one node's memory for the rest of the process's
//! life, with nothing in Redis and no warning owed. The ledger follows the
//! deployment's shape, which does not change; the caution follows the ceiling,
//! which does.
//!
//! **The key layout was the one question the M10.1 deferral left open, and it
//! is answered in that crate rather than here** (M13): one hash per (scope,
//! bucket) at [`BUCKET_MS`], two integer fields, expired by Redis at the widest
//! window plus one bucket — which is what buys the pruning pass a
//! hash-per-scope layout would have needed and nothing owns. The shape of the
//! two operations was already decided by this trait, and it held:
//! `record_draw` is one script and `would_exceed` is one script, the same way
//! [`RedisSpendLedger`](super::spend::SpendLedger) expresses a grant.
//!
//! **What keeps the two honest is [`contract`]**: one list of behavioural
//! assertions, run against both. The arithmetic below — the window sum, the
//! narrowest-first check, the retry walk — is the specification, and a backend
//! that cannot reproduce it is wrong rather than different.
//!
//! **And the arithmetic is integer, in one bounded domain both backends
//! share** — see [`MAX_COUNT`] and [`DrawCounts`]. Money was never defensibly
//! an `f64` here: a ceiling accumulated by float addition disagrees with an
//! exact one at ordinary decimal boundaries (`0.70 + 0.10 < 0.80`), so the two
//! backends admitted and refused different turns at the same cap while both
//! passed the contract. The `f64` the trait speaks is now converted to
//! micro-dollars exactly once, by [`DrawCounts::of`], which both ledgers call;
//! caps convert through [`cap_micros`] and [`cap_tokens`]; and every sum, cap
//! comparison and retry walk on either side is done on those integers.
//!
//! What is still true of the memory ledger, and is what its warning is about:
//! its counters live in one process, so two nodes enforce two independent
//! ceilings and every counter resets on restart. That is honest for a
//! single-node deployment and not for any other, which is why the deployment
//! that has said "this is more than one process" — by naming a Redis — is
//! exactly the one that gets the shared buckets.

#[cfg(any(test, feature = "test-support"))]
pub mod contract;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};

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

/// The ceiling both counters saturate at, and the whole of the shared integer
/// domain: 2^53.
///
/// **The bound exists so two implementations in two languages agree
/// bit-for-bit, not because anyone draws this much.** Money and tokens are
/// counted as integers — micro-dollars and tokens — because a rolling ceiling
/// accumulated through float addition drifts differently on every node, and
/// `0.70 + 0.10` is not `0.80`. The *size* of the domain is then decided by
/// the weaker of the two arithmetics: the Redis backend sums its buckets in
/// Lua, whose only number is a double. Every integer through 2^53 is exact
/// there; a sum that leaves the domain can only be rounded further out, never
/// back under it, so a sum clamped at [`MAX_COUNT`] after *every* addition is
/// the same number the `u64` arithmetic here produces — with no overflow error
/// path on either side.
///
/// The alternative was `i64::MAX`, which is what `HINCRBY` holds: it would
/// have bought nothing, because the read side goes through `tonumber` anyway,
/// and it would have left the two backends disagreeing above 2^53 while both
/// claimed to enforce one ceiling. Nine quadrillion micro-dollars is nine
/// billion dollars and 2^53 tokens is more than any fleet serves; a draw past
/// either is a number that arrived by accident, and it is refused at the edge
/// by both ledgers rather than recorded differently by each.
pub const MAX_COUNT: u64 = 1 << 53;

/// Micro-dollars per dollar: the integer unit dollars are counted in.
const MICROS_PER_USD: f64 = 1_000_000.0;

/// Add inside the shared domain.
///
/// Clamped at [`MAX_COUNT`] after every addition rather than only on the
/// total, because that is precisely what the Lua side must do to stay exact —
/// and "the two ledgers do the same additions in the same order" is what makes
/// them one ceiling rather than two that usually agree.
fn add_count(sum: u64, add: u64) -> u64 {
    sum.saturating_add(add).min(MAX_COUNT)
}

/// One draw's two counts, already in the domain both ledgers share.
///
/// **The one conversion of the `f64` the trait speaks**, and the one place a
/// draw is refused for being outside the domain. It lives here rather than in
/// a backend because a second spelling of the rounding is a second ceiling: a
/// backend that rounded half-to-even where this rounds half away from zero
/// would refuse a different set of turns while passing every test that does
/// not sit on a boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawCounts {
    pub tokens: u64,
    pub micros: u64,
}

impl DrawCounts {
    /// Convert and bound one draw, or say why it cannot be recorded.
    ///
    /// Rounding is half away from zero, which for a non-negative draw is half
    /// up, so a draw below half a micro-dollar records as zero. That is
    /// 5 × 10⁻⁷ dollars, six orders of magnitude below the cheapest turn this
    /// fleet can serve, and it is the only information the integer domain
    /// loses.
    ///
    /// A `NaN` is refused rather than clamped for the reason
    /// `SpendError::check_amount` gives: it would not blow up, it would make
    /// every window sum `NaN` — never `>=` any cap — and the ceiling would
    /// silently stop existing.
    pub fn of(tokens: u64, usd: f64) -> Result<Self, FairUseError> {
        if !usd.is_finite() || usd < 0.0 {
            return Err(FairUseError::InvalidAmount {
                field: "usd",
                value: usd,
            });
        }
        if tokens > MAX_COUNT {
            return Err(FairUseError::OutOfDomain {
                field: "tokens",
                value: tokens,
            });
        }
        let micros = (usd * MICROS_PER_USD).round();
        if micros > MAX_COUNT as f64 {
            return Err(FairUseError::OutOfDomain {
                field: "micro-dollars",
                // A float-to-integer `as` cast saturates in Rust rather than
                // trapping, so an absurd draw is named as `u64::MAX` instead
                // of as some wrapped nonsense the operator has to decode.
                value: micros as u64,
            });
        }
        Ok(Self {
            tokens,
            micros: micros as u64,
        })
    }
}

/// A token cap in the counters' domain.
///
/// Clamped rather than refused, unlike a *draw*: a cap past the domain is one
/// no sum inside the domain can reach, and clamping it to the ceiling is that
/// same answer said once here instead of on every comparison. Refusing it
/// would turn a harmless configuration into a boot-time failure.
pub fn cap_tokens(max_tokens: Option<u64>) -> Option<u64> {
    max_tokens.map(|max| max.min(MAX_COUNT))
}

/// A dollar cap as micro-dollars, through the same rounding a draw takes.
///
/// A non-finite cap is *absent*, which is what it already meant when the
/// comparison was in floats: every comparison against a `NaN` is false and no
/// sum reaches an infinity. A negative cap clamps to zero, which is the same
/// "refuses everything" a negative cap already was.
pub fn cap_micros(max_usd: Option<f64>) -> Option<u64> {
    let max = max_usd.filter(|max| max.is_finite())?;
    Some((max * MICROS_PER_USD).round().clamp(0.0, MAX_COUNT as f64) as u64)
}

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
                if cap_tokens(limit.max_tokens).is_some_and(|max| drawn.tokens >= max) {
                    return Some((scope, *limit, FairUseQuantity::Tokens, drawn));
                }
                if cap_micros(limit.max_usd).is_some_and(|max| drawn.micros >= max) {
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
    /// Dollars as the integer micro-dollars every comparison is made in; see
    /// [`MAX_COUNT`] for why money is not an `f64` past this edge.
    pub micros: u64,
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
    /// A draw outside the counters' shared domain, refused before any ledger
    /// writes anything — the same posture as a `NaN`, and for a related
    /// reason: a count no backend can hold exactly is one the two backends
    /// would hold *differently*. See [`MAX_COUNT`].
    #[error(
        "`{field}` of {value} is past the {} a fair-use counter holds; both ledgers \
         count in integers so every node agrees exactly",
        MAX_COUNT
    )]
    OutOfDomain { field: &'static str, value: u64 },
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
    by_index: BTreeMap<u64, (u64, u64)>,
}

impl Buckets {
    fn record(&mut self, at_ms: u64, counts: DrawCounts) {
        let entry = self.by_index.entry(at_ms / BUCKET_MS).or_insert((0, 0));
        entry.0 = add_count(entry.0, counts.tokens);
        entry.1 = add_count(entry.1, counts.micros);
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
        let inside: Vec<(&u64, &(u64, u64))> = self.by_index.range(first..).collect();
        let tokens = inside
            .iter()
            .fold(0u64, |sum, (_, (tokens, _))| add_count(sum, *tokens));
        let micros = inside
            .iter()
            .fold(0u64, |sum, (_, (_, micros))| add_count(sum, *micros));
        Drawn {
            tokens,
            micros,
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
    inside: &[(&u64, &(u64, u64))],
    window: FairUseWindow,
    limit: &FairUseLimit,
) -> Option<u64> {
    let max_tokens = cap_tokens(limit.max_tokens);
    let max_micros = cap_micros(limit.max_usd);
    let over = |tokens: u64, micros: u64| {
        max_tokens.is_some_and(|max| tokens >= max) || max_micros.is_some_and(|max| micros >= max)
    };
    let mut tokens = inside
        .iter()
        .fold(0u64, |sum, (_, (tokens, _))| add_count(sum, *tokens));
    let mut micros = inside
        .iter()
        .fold(0u64, |sum, (_, (_, micros))| add_count(sum, *micros));
    if !over(tokens, micros) {
        return None;
    }
    for (index, (bucket_tokens, bucket_micros)) in inside {
        tokens = tokens.saturating_sub(*bucket_tokens);
        micros = micros.saturating_sub(*bucket_micros);
        if !over(tokens, micros) {
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
/// says so the first time it enforces one. A deployment that wants one ceiling
/// across nodes names a Redis; see the module doc for which store that selects
/// and why.
#[derive(Debug, Default)]
pub struct MemoryFairUseLedger {
    scopes: Mutex<HashMap<(ProjectId, Option<UserId>), Buckets>>,
    /// Whether the single-node caution below has been said.
    ///
    /// **Here rather than at the composition root, because here is the only
    /// place that knows both halves of what the caution is about.** The boot
    /// site knows which ledger it wired but can only guess whether a ceiling
    /// will ever exist — it reads a snapshot of a plane the admin API patches
    /// at runtime, which is precisely how M13's review found a deployment
    /// enforcing a PATCHed-in ceiling per node while owing no warning at all.
    /// This type knows it is per-process by construction, and `would_exceed`
    /// is handed the ceiling itself; the alternative — a `is_shared()` on the
    /// trait, read by the engine, plus a flag on the engine — is three
    /// spellings of one fact and a new obligation on every future backend.
    warned_single_node: AtomicBool,
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
        // The edge conversion, and it is the *same* function the Redis ledger
        // calls: a `NaN` or a count outside the domain is refused here before
        // any counter moves, and dollars become micro-dollars exactly once.
        // Two spellings of this would be two ceilings.
        let counts = DrawCounts::of(tokens, usd)?;
        let mut scopes = self.scopes.lock().await;
        // Both scopes from one call: the project's counters and this member's.
        // Two calls could record one and not the other, and a member ceiling
        // enforced against a project counter is not a member ceiling.
        for user in [None, Some(principal.user.clone())] {
            let buckets = scopes.entry((principal.project.clone(), user)).or_default();
            buckets.record(at_ms, counts);
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
        // **The honesty mechanism, at the moment it is owed rather than at
        // boot.** A ceiling everyone believes in and nothing enforces across
        // nodes is worse than no ceiling, and non-empty terms here mean this
        // process is now the whole of that ceiling — however the terms got
        // here, whether from the file this node booted from or from an admin
        // `PATCH` an hour later. Once per ledger and not per turn: a caution
        // repeated on every admitted request is one an operator filters out.
        if !self.warned_single_node.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "fair-use windows are being enforced against THIS PROCESS'S memory. Two \
                 nodes serving one project therefore enforce two independent ceilings -- a \
                 project capped at 2M tokens per 5 hours can draw 2M through each -- and \
                 every counter resets on restart. Fair use across nodes is only true with \
                 shared buckets, which is what naming a Redis selects; see \
                 roundhouse_store_redis::fair_use for the key layout it lands on"
            );
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

    // The behavioural assertions live in `contract` now and are run from here
    // by the macro below. They were this module's own unit tests until a
    // second backend existed to run them; leaving them here would have meant
    // the memory ledger was judged by one list and Redis by another, which is
    // the exact drift the suite exists to make impossible.
    crate::fair_use_ledger_contract_suite!(MemoryFairUseLedger::new());

    /// The file's spelling of a window and the refusal's are one string.
    ///
    /// Not in the contract: it asks nothing of a ledger. `serde` and
    /// `wire_name` are properties of the vocabulary itself, so a backend
    /// running this would be re-checking `roundhouse-core` against itself
    /// through an unrelated dependency.
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

    /// The edge conversion: rounding, the sub-micro-dollar draw it loses, and
    /// the domain it refuses outside of.
    ///
    /// Here rather than in the contract because it is a property of the
    /// *conversion function itself*, which both ledgers call — what each
    /// ledger then does with a converted draw, and what it does with one that
    /// does not convert, is asserted in the contract against both.
    #[test]
    fn dollars_convert_to_micro_dollars_once_at_the_edge() {
        let micros = |usd| DrawCounts::of(0, usd).unwrap().micros;
        assert_eq!(micros(0.0), 0);
        assert_eq!(micros(5.0), 5_000_000);
        assert_eq!(micros(0.35), 350_000);
        // Half away from zero, and the sub-micro-dollar draw it loses.
        assert_eq!(micros(0.0000005), 1);
        assert_eq!(micros(0.0000004), 0);
        // The boundary the whole domain exists for: exact integers, not the
        // 0.7999999999999999 an `f64` sum of the same two draws produces.
        assert_eq!(micros(0.70) + micros(0.10), micros(0.80));

        // Outside the domain: refused, not clamped, and on either field.
        assert!(matches!(
            DrawCounts::of(MAX_COUNT + 1, 0.0),
            Err(FairUseError::OutOfDomain {
                field: "tokens",
                ..
            })
        ));
        assert!(matches!(
            DrawCounts::of(0, 1e10),
            Err(FairUseError::OutOfDomain { .. })
        ));
        assert!(matches!(
            DrawCounts::of(0, f64::NAN),
            Err(FairUseError::InvalidAmount { .. })
        ));
        // And the ceiling itself is inside the domain, or "saturates at
        // MAX_COUNT" would be unreachable.
        assert_eq!(DrawCounts::of(MAX_COUNT, 0.0).unwrap().tokens, MAX_COUNT);
    }

    /// A cap converts through the same rounding a draw does, and is *clamped*
    /// into the domain where a draw is refused out of it.
    #[test]
    fn a_cap_converts_through_the_same_edge_and_clamps_rather_than_refusing() {
        assert_eq!(cap_micros(Some(5.0)), Some(5_000_000));
        assert_eq!(cap_micros(Some(0.80)), Some(800_000));
        assert_eq!(cap_tokens(Some(1_000)), Some(1_000));
        // A cap of zero is a cap, not an absence — confusing the two is the
        // difference between a window that refuses everything and one that
        // refuses nothing.
        assert_eq!(cap_micros(Some(0.0)), Some(0));
        assert_eq!(cap_tokens(Some(0)), Some(0));
        assert_eq!(cap_micros(None), None);
        assert_eq!(cap_tokens(None), None);
        // Non-finite binds on nothing, exactly as it did when the comparison
        // was in floats.
        assert_eq!(cap_micros(Some(f64::NAN)), None);
        assert_eq!(cap_micros(Some(f64::INFINITY)), None);
        // Past the domain: clamped to the ceiling a saturated sum reaches.
        assert_eq!(cap_tokens(Some(u64::MAX)), Some(MAX_COUNT));
        assert_eq!(cap_micros(Some(1e300)), Some(MAX_COUNT));
    }
}
