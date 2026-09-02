// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Redis-backed [`FairUseLedger`]: the rolling ceilings, counted where every
//! node can see them.
//!
//! This closes the deferral `roundhouse_core::control::fair_use` wrote down.
//! The shape of the two operations was decided by the trait — one script each
//! — and the one open question was the key layout. It is answered here.
//!
//! | Key | Type | Holds |
//! |---|---|---|
//! | `rh:{<project_id>}:fairuse:p:<bucket>` | hash | `t` tokens, `u` micro-dollars, drawn by anyone in the project inside that bucket |
//! | `rh:{<project_id>}:fairuse:m:<user_id>:<bucket>` | hash | the same two counters for one member |
//!
//! `<bucket>` is `at_ms / BUCKET_MS`, the same floor division the memory
//! ledger's `BTreeMap` is keyed by. The braces are a Redis Cluster hash tag on
//! the *project* id, exactly as the spend ledger's three keys are: both scopes
//! and every one of their buckets land in one slot, which is what lets one
//! script read a project's and a member's counters together. A member ceiling
//! checked in a different round trip from the project's is two answers about
//! one turn.
//!
//! # The layout, and the one it beat
//!
//! The deferral named two candidates. **Hash-per-scope** — one hash per
//! (project; member) with a field per bucket — is one `HINCRBY` and one
//! `HGETALL`, which is the cheaper pair of round trips, and it was rejected on
//! the sentence the deferral itself wrote: it *needs a pruning pass nothing
//! currently owns*. A hash grows a field per five minutes forever, so a
//! project that ran a benchmark in March and nothing since keeps its March
//! fields until somebody sweeps them, and the somebody does not exist: this
//! repo has no background task, deliberately — the spend ledger's whole crash
//! story is that a leaked hold self-heals lazily on the next call rather than
//! by a sweeper. Pruning inside `record_draw` would make every draw walk the
//! whole hash to find what to delete, which is the cost the layout was chosen
//! to avoid.
//!
//! **Bucket-per-key makes Redis the sweeper.** A `PEXPIRE` at the widest
//! window plus one bucket is set on every write, so a bucket outside every
//! window deletes itself and an idle scope costs nothing at all — not one key,
//! not one field. What it costs instead is reads: a 7-day window is at most
//! 2017 keys rather than one `HGETALL`.
//!
//! **That cost lands on the common case, not the rare one (M13 review,
//! F4).** The scan is lazy and narrowest-first, which only makes the 5-hour
//! window's 61 reads the whole story when a window *binds* — refuses — before
//! a wider one is asked. But `would_exceed`'s admission path only stops early
//! by finding a refusal, and an admitted turn is exactly the case where no
//! window ever binds: it is the turn a fleet serves most of the time, and it
//! widens through every configured window to the widest one. A membership
//! capped on all three windows on both scopes therefore costs up to 2017
//! reads per scope — 4034 total, measured at roughly 8ms of blocking Lua time
//! per admission on a real Redis — on the ordinary, non-refusing path, not
//! 61. What the lazy scan still buys is that those reads are inside one
//! script rather than one round trip apiece, and that a *refusal* at the
//! narrowest window really does cost only 61: the saving is real, it is just
//! not the common case. Replacing the scan itself — running sums maintained
//! on write, the bucket walk reserved for a refusal's retry time — is M13.1's
//! rung, not this one's.
//!
//! Two fields rather than one packed value: the two counters are summed
//! independently and updated independently. They are read, added and written
//! back *inside* the one script, which Redis runs indivisibly, so a concurrent
//! draw from another node is exact rather than last-write-wins — and, unlike
//! the `HINCRBY` pair this started as, there is no command that can fail
//! part-way and leave one scope moved and the other not.
//!
//! # The integer domain, which is not this crate's to define
//!
//! Both counters are integers — tokens, and dollars as micro-dollars —
//! because a ceiling accumulated through float addition drifts differently on
//! every node. The conversion, the rounding it uses and the bound it enforces
//! all live in `roundhouse_core::control::fair_use`: [`DrawCounts::of`] is
//! what turns the trait's `f64` into the two counts, [`cap_micros`] and
//! [`cap_tokens`] convert a limit through the same rounding, and
//! [`MAX_COUNT`] is the ceiling both fields saturate at. This crate calls
//! them; it does not have opinions of its own about money.
//!
//! **The M13 addendum recorded a divergence here, and it is closed.** That
//! text said these counters were `i64`, so a draw past `i64::MAX` was refused
//! where the memory ledger saturated, and it asserted the contract covered
//! only the shared middle of the range. Both halves are gone: the domain is
//! `MAX_COUNT` on *both* sides, a draw past it is refused by both before
//! anything is written, a sum at it saturates in both, and the contract suite
//! asserts each of those against both backends. The bound is 2^53 rather than
//! `i64::MAX` for the reason [`MAX_COUNT`] gives — the window sum is computed
//! in Lua, whose only number is a double, so a wider write-side range would
//! have been thrown away at the one comparison that decides a refusal.
//!
//! # What the clock is
//!
//! The caller's, never `redis.call('TIME')` — see `scripts`, which states the
//! rule and the `RedisSpendLedger` precedent it follows.
//!
//! # The one place the two representations cannot be made identical
//!
//! A window's sum runs over the buckets from `first_index(window, now_ms)`
//! through `now_ms / BUCKET_MS`; keys cannot be enumerated forward without
//! bound, so a draw stamped *after* the `now_ms` a later check supplies is
//! summed by the memory ledger's open-ended range and not by this one. Reaching
//! it takes a clock that went backwards between a settle and the next
//! admission by more than the remainder of a bucket, and the divergence it
//! causes is bounded by that skew. It is stated rather than papered over: the
//! alternative — bounding the memory ledger's range to match — would change the
//! specification to fit an implementation, which is the wrong direction.
//!
//! Passes the same
//! `fair_use_ledger_contract_suite!`
//! that judges the memory ledger, instantiated ignore-gated in
//! `tests/fair_use_contract.rs` exactly as `tests/spend_contract.rs` does for
//! spend.

// `pub(crate)` for one reason: `test_support` re-exports the `would_exceed`
// script's text so the gated test that must hand it an argument list
// `WouldExceedArgs` cannot express invokes the real script rather than a copy.
pub(crate) mod scripts;

use std::sync::Arc;

use async_trait::async_trait;
use redis::aio::ConnectionManager;

use roundhouse_core::control::fair_use::{
    BUCKET_MS, DrawCounts, MAX_COUNT, cap_micros, cap_tokens,
};
use roundhouse_core::control::{
    FairUseError, FairUseLedger, FairUseLimit, FairUseRefusal, FairUseTerms, FairUseWindow,
    Principal, ProjectId, UserId,
};

use crate::KEY_PREFIX;

/// The bucket-key prefix for a project's own counters.
///
/// The bucket index is appended as the final `:`-separated segment and is
/// always decimal digits, which is what keeps
/// [`member_bucket_prefix`]'s embedded user id unambiguous: splitting a key at
/// its last colon recovers exactly (prefix, index), so no two (scope, bucket)
/// pairs can name one key however a user id is spelled.
pub(crate) fn project_bucket_prefix(project: &ProjectId) -> String {
    format!("{KEY_PREFIX}:{{{project}}}:fairuse:p")
}

pub(crate) fn member_bucket_prefix(project: &ProjectId, user: &UserId) -> String {
    format!("{KEY_PREFIX}:{{{project}}}:fairuse:m:{user}")
}

/// One bucket's key, and the bucket a timestamp lands in.
///
/// **Compiled only for tests**, and that is the honest shape rather than an
/// oversight: the production path never builds a bucket key in Rust, because
/// the script builds it server-side from `at_ms` so a caller cannot send a
/// timestamp and an index that disagree. What these exist for is the gated
/// assertions that read the raw hashes — and they are load-bearing there
/// precisely because they are a *second* spelling: every such test writes
/// through the Lua and reads through these, so the two would have to agree by
/// accident to pass. Left ungated they would be dead code in every shipped
/// build, which is a surface that reads as supported and is not.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn bucket_key(prefix: &str, index: u64) -> String {
    format!("{prefix}:{index}")
}

/// Floor division, the same key the memory ledger's `BTreeMap` is keyed by.
/// Test-only for [`bucket_key`]'s reason.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn bucket_index(at_ms: u64) -> u64 {
    at_ms / BUCKET_MS
}

/// How long a bucket lives: the widest window plus one bucket.
///
/// Derived from [`FairUseWindow::ALL`] rather than naming the 7-day window, so
/// a wider window added to that enum lengthens this by construction. Plus one
/// bucket because a bucket is included in a window until its *end* has passed
/// out of it — the staircase rule `BUCKET_MS` documents — so a TTL of exactly
/// the span would delete a bucket that is still being summed.
pub(crate) fn bucket_ttl_ms() -> u64 {
    FairUseWindow::ALL
        .iter()
        .map(|window| window.span_ms())
        .max()
        .expect("FairUseWindow::ALL is never empty")
        + BUCKET_MS
}

/// A cap in the script's vocabulary: the decimal digits, or `''` for "not
/// capped on this quantity".
///
/// The only conversion left in this crate, and it is encoding rather than
/// arithmetic: the cap itself was already put into the counters' domain by
/// [`cap_tokens`]/[`cap_micros`] in core, which is what stops this backend
/// clamping a cap somewhere the memory ledger does not. `''` is a sentinel the
/// script tests by string equality, so it can never be confused with a cap of
/// zero.
fn cap_arg(cap: Option<u64>) -> String {
    cap.map_or_else(String::new, |max| max.to_string())
}

fn scope_caps(limits: &[FairUseLimit], window: FairUseWindow) -> scripts::ScopeCaps {
    match limits.iter().find(|limit| limit.window == window) {
        None => scripts::ScopeCaps::absent(),
        Some(limit) => scripts::ScopeCaps {
            present: true,
            max_tokens: cap_arg(cap_tokens(limit.max_tokens)),
            max_micros: cap_arg(cap_micros(limit.max_usd)),
        },
    }
}

/// Redis implementation of [`FairUseLedger`].
///
/// Cheap to clone: clones share one auto-reconnecting multiplexed connection,
/// exactly like [`RedisSessionStore`](crate::RedisSessionStore) and
/// [`RedisSpendLedger`](crate::RedisSpendLedger).
#[derive(Clone)]
pub struct RedisFairUseLedger {
    conn: ConnectionManager,
    scripts: Arc<scripts::Scripts>,
    /// A field rather than a constant read at the call site, so the
    /// test-support seam below can shorten it without the production path
    /// having a knob. See [`Self::with_bucket_ttl_ms`].
    bucket_ttl_ms: u64,
}

impl RedisFairUseLedger {
    /// Connect and fail fast: a ledger that cannot reach its Redis at startup
    /// should stop the process there, not on the first refused turn.
    pub async fn connect(url: impl AsRef<str>) -> Result<Self, FairUseError> {
        let client = redis::Client::open(url.as_ref())
            .map_err(|error| FairUseError::Backend(anyhow::Error::new(error)))?;
        let conn = ConnectionManager::new(client)
            .await
            .map_err(|error| FairUseError::Backend(anyhow::Error::new(error)))?;
        Ok(Self {
            conn,
            scripts: Arc::new(scripts::Scripts::new()),
            bucket_ttl_ms: bucket_ttl_ms(),
        })
    }

    /// Shorten the bucket TTL, for the one test that has to watch a bucket
    /// actually disappear.
    ///
    /// **Gated behind `test-support` and not a configuration knob.** The
    /// production expiry is derived from the window widths and must stay
    /// derived — an operator who could shorten it would be deleting buckets a
    /// window is still summing, which is a ceiling that silently leaks. What
    /// the test needs is not a different policy but a shorter wait, and this
    /// is the smallest seam that gives it one: the ordinary
    /// `bucket_ttl_ms()` still decides what
    /// [`connect`](Self::connect) builds, and a separate gated test asserts
    /// the `PTTL` a real draw arms is the derived one.
    #[cfg(feature = "test-support")]
    pub fn with_bucket_ttl_ms(mut self, bucket_ttl_ms: u64) -> Self {
        self.bucket_ttl_ms = bucket_ttl_ms;
        self
    }
}

#[async_trait]
impl FairUseLedger for RedisFairUseLedger {
    async fn record_draw(
        &self,
        principal: &Principal,
        at_ms: u64,
        tokens: u64,
        usd: f64,
    ) -> Result<(), FairUseError> {
        // The edge conversion, in core, shared with the memory ledger: a
        // `NaN` or a count outside the domain is refused before anything
        // reaches Redis, so a draw the two backends could not record
        // identically is recorded by neither. A second copy of this rule here
        // would be a second ceiling.
        let counts = DrawCounts::of(tokens, usd)?;

        self.scripts
            .record_draw(
                &mut self.conn.clone(),
                scripts::RecordDrawArgs {
                    project_key: &project_bucket_prefix(&principal.project),
                    member_key: &member_bucket_prefix(&principal.project, &principal.user),
                    at_ms,
                    bucket_ms: BUCKET_MS,
                    counts,
                    max_count: MAX_COUNT,
                    ttl_ms: self.bucket_ttl_ms,
                },
            )
            .await
    }

    async fn would_exceed(
        &self,
        principal: &Principal,
        terms: &FairUseTerms,
        now_ms: u64,
    ) -> Result<Option<FairUseRefusal>, FairUseError> {
        // The early return the memory ledger makes too, and it is not an
        // optimization: a membership with no fair-use block must reach no
        // counter at all, which is what makes the shipped posture — every
        // project, until an operator writes a window down — cost nothing.
        if terms.is_empty() {
            return Ok(None);
        }
        let windows = FairUseWindow::ALL.map(|window| scripts::WindowArgs {
            span_ms: window.span_ms(),
            name: window.wire_name(),
            project: scope_caps(&terms.project, window),
            member: scope_caps(&terms.member, window),
        });
        self.scripts
            .would_exceed(
                &mut self.conn.clone(),
                scripts::WouldExceedArgs {
                    project_key: &project_bucket_prefix(&principal.project),
                    member_key: &member_bucket_prefix(&principal.project, &principal.user),
                    now_ms,
                    bucket_ms: BUCKET_MS,
                    max_count: MAX_COUNT,
                    windows,
                },
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the module doc claims is load-bearing: both scopes' keys
    /// hash to one Redis Cluster slot, so one script may touch them.
    ///
    /// The same assertion `the_project_and_member_keys_share_one_hash_tag`
    /// makes for the spend ledger, and it earns its place for the same reason:
    /// if the two prefixes ever drifted to different tags, `WOULD_EXCEED`
    /// would refuse to run at all on a clustered deployment, and this catches
    /// it at build time rather than at first boot against a cluster.
    #[test]
    fn both_scopes_hash_to_one_slot_and_two_projects_do_not() {
        fn hash_tag(key: &str) -> &str {
            let start = key
                .find('{')
                .expect("every fair-use key carries a hash tag");
            let end = key.find('}').expect("the hash tag is closed");
            &key[start + 1..end]
        }

        let project = ProjectId::new("acme");
        let ada = UserId::new("ada");
        let project_prefix = project_bucket_prefix(&project);
        let member_prefix = member_bucket_prefix(&project, &ada);
        let tag = hash_tag(&project_prefix);
        assert_eq!(tag, "acme", "the tag is the project id, unadorned");
        assert_eq!(hash_tag(&member_prefix), tag);
        // And the bucket key is the prefix plus a segment, so it inherits the
        // tag rather than having one of its own to get wrong.
        let one_bucket = bucket_key(&project_prefix, 12);
        assert_eq!(hash_tag(&one_bucket), tag);

        // The control: two projects must land on two tags, or every project
        // would collide onto one slot.
        let other = project_bucket_prefix(&ProjectId::new("other"));
        assert_ne!(hash_tag(&other), tag);
    }

    /// Two members of one project, and the project itself, are three key
    /// spaces — including when a user id contains the separator.
    #[test]
    fn no_two_scopes_or_buckets_can_name_one_key() {
        let project = ProjectId::new("acme");
        let mut keys = vec![
            bucket_key(&project_bucket_prefix(&project), 1),
            bucket_key(&member_bucket_prefix(&project, &UserId::new("ada")), 1),
            bucket_key(&member_bucket_prefix(&project, &UserId::new("bob")), 1),
            bucket_key(&member_bucket_prefix(&project, &UserId::new("ada")), 2),
            // The adversarial pair: a user id that itself ends in what looks
            // like a bucket segment. The index is always the final segment and
            // always digits, so splitting at the last colon still recovers the
            // right (scope, bucket) — which is why these two are different
            // keys rather than one.
            bucket_key(&member_bucket_prefix(&project, &UserId::new("ada:1")), 2),
            bucket_key(&member_bucket_prefix(&project, &UserId::new("ada")), 12),
        ];
        let before = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(
            keys.len(),
            before,
            "every scope-and-bucket pair is its own key"
        );
    }

    /// The bucket index is the memory ledger's floor division, and the
    /// boundary belongs to the bucket it opens.
    #[test]
    fn a_bucket_index_is_the_same_floor_division_the_memory_ledger_uses() {
        assert_eq!(bucket_index(0), 0);
        assert_eq!(bucket_index(BUCKET_MS - 1), 0);
        assert_eq!(bucket_index(BUCKET_MS), 1);
        assert_eq!(bucket_index(2 * BUCKET_MS + 17), 2);
    }

    /// The expiry is the widest window plus one bucket, derived rather than
    /// written down.
    #[test]
    fn a_bucket_outlives_the_widest_window_by_exactly_one_bucket() {
        assert_eq!(
            bucket_ttl_ms(),
            FairUseWindow::SevenDays.span_ms() + BUCKET_MS
        );
        // The control on the derivation: it is the *widest* window, not the
        // first or the last named.
        assert!(
            FairUseWindow::ALL
                .iter()
                .all(|window| bucket_ttl_ms() > window.span_ms())
        );
    }

    /// A count crosses as an integer `ARGV`, and that is byte-identical to the
    /// decimal `String` this used to build.
    ///
    /// The live half of M13 review finding F7. `tokens_arg` existed because a
    /// `String` was said to reach the script "untouched by Lua's doubles" —
    /// true of *any* integer argument, and so not a reason to prefer a
    /// `String`. The pinned `redis` crate formats an integer `ARGV` with
    /// `itoa`, the same decimal ASCII bytes `to_string()` produces, so
    /// removing the plumbing changed nothing on the wire. What actually keeps
    /// Lua's doubles exact is the domain bound, which is asserted in core
    /// beside `MAX_COUNT` and in the contract against both backends.
    #[test]
    fn a_count_crosses_as_the_same_bytes_a_decimal_string_would_have() {
        use redis::ToRedisArgs;
        let value: u64 = 9_007_199_254_740_993; // past 2^53, the case that matters
        assert_eq!(
            value.to_redis_args(),
            value.to_string().to_redis_args(),
            "an integer ARGV and its decimal-string twin must serialize to \
             the same RESP bytes, or dropping the String plumbing would have \
             changed the wire rather than just the code"
        );
    }

    /// A cap crosses as the digits core put it in the domain as, and an absent
    /// one as the sentinel the script tests by string equality.
    ///
    /// The clamping and the rounding are core's and are asserted there; what
    /// is asserted here is the *encoding*, which is this crate's half.
    #[test]
    fn a_cap_crosses_as_the_same_integers_a_draw_does() {
        let micros = |max| cap_arg(cap_micros(Some(max)));
        assert_eq!(micros(5.0), "5000000");
        assert_eq!(micros(0.01), "10000");
        assert_eq!(cap_arg(cap_tokens(Some(1_000))), "1000");
        assert_eq!(cap_arg(cap_tokens(None)), "");
        assert_eq!(cap_arg(cap_micros(None)), "");
        // A cap of zero is a cap, not an absence — getting these two confused
        // is the difference between a window that refuses everything and one
        // that refuses nothing.
        assert_eq!(cap_arg(cap_tokens(Some(0))), "0");
        assert_eq!(micros(0.0), "0");
        // A non-finite cap binds on nothing in the memory ledger, and encodes
        // as the absent sentinel here for exactly that reason.
        assert_eq!(micros(f64::NAN), "");
        assert_eq!(micros(f64::INFINITY), "");
        // An unreachable cap arrives already clamped to the domain's ceiling,
        // which is what a saturated sum reaches — so it stays reachable.
        assert_eq!(cap_arg(cap_tokens(Some(u64::MAX))), MAX_COUNT.to_string());
    }

    /// The three windows are handed to the script narrowest-first, which is
    /// the order the whole refusal rule depends on.
    #[test]
    fn the_windows_reach_the_script_narrowest_first() {
        let terms = FairUseTerms {
            project: vec![FairUseLimit {
                window: FairUseWindow::TwentyFourHours,
                max_tokens: Some(7),
                max_usd: None,
            }],
            member: Vec::new(),
        };
        let windows: Vec<(&str, bool, String)> = FairUseWindow::ALL
            .iter()
            .map(|window| {
                let caps = scope_caps(&terms.project, *window);
                (window.wire_name(), caps.present, caps.max_tokens)
            })
            .collect();
        assert_eq!(
            windows,
            vec![
                ("5h", false, String::new()),
                ("24h", true, "7".to_string()),
                ("7d", false, String::new()),
            ],
            "the one configured window lands in its own slot, and the two \
             unconfigured ones are marked absent rather than capped at zero"
        );
    }
}
