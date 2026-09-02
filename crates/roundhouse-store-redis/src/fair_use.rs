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
//! 2017 keys rather than one `HGETALL`. That is the trade, taken with two
//! things making it cheap. The scan is *lazy and narrowest-first*, so the
//! common case — the 5-hour window, which is the narrowest and therefore the
//! first to bind — is 61 reads and the wider windows are paid for only when
//! the narrower ones had room. And the reads are inside the script, so they
//! are 2017 hash lookups in one round trip rather than 2017 round trips.
//!
//! One `HINCRBY` per field rather than one packed value: the two counters are
//! summed independently and incremented independently, and `HINCRBY` is the
//! operation that makes a concurrent draw from another node exact rather than
//! last-write-wins.
//!
//! # Micro-dollars, and what the rounding costs
//!
//! `HINCRBY` is exact; `INCRBYFLOAT` is not, and a ceiling accumulated through
//! float addition on the server would drift differently on every node. So the
//! `f64` the trait speaks is converted **once, at this edge**, to
//! micro-dollars — `(usd * 1_000_000).round()`, half away from zero, which for
//! a non-negative draw is half up. A draw below half a micro-dollar therefore
//! records as zero. That is 5 × 10⁻⁷ dollars, six orders of magnitude below
//! the cheapest turn this fleet can serve, and it is the only information this
//! ledger loses that the memory one keeps.
//!
//! The other end of the range is worth stating because it is where the two
//! backends genuinely differ: these counters are Redis integers, so a draw
//! past `i64::MAX` tokens or micro-dollars is **refused** here where
//! [`MemoryFairUseLedger`](roundhouse_core::control::MemoryFairUseLedger)
//! saturates. Nine quintillion tokens is not a turn anyone serves; a loud
//! refusal is the right answer for a number that arrived by accident, and the
//! contract suite deliberately asserts the shared middle of the range rather
//! than either end.
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

mod scripts;

use std::sync::Arc;

use async_trait::async_trait;
use redis::aio::ConnectionManager;

use roundhouse_core::control::fair_use::BUCKET_MS;
use roundhouse_core::control::{
    FairUseError, FairUseLedger, FairUseLimit, FairUseRefusal, FairUseTerms, FairUseWindow,
    Principal, ProjectId, UserId,
};

use crate::KEY_PREFIX;

/// Micro-dollars per dollar: the integer unit the `u` field counts in.
const MICROS_PER_USD: f64 = 1_000_000.0;

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

/// Dollars as an exact integer count of micro-dollars.
///
/// Refuses rather than saturating past the range a Redis integer holds; see
/// the module doc for why that is the right end of the trade.
fn micros_of(usd: f64) -> Result<i64, FairUseError> {
    let micros = (usd * MICROS_PER_USD).round();
    if micros > i64::MAX as f64 {
        return Err(FairUseError::Backend(anyhow::anyhow!(
            "a draw of ${usd} is past the {} micro-dollars a Redis counter holds; \
             the fair-use buckets count in integers so two nodes agree exactly",
            i64::MAX
        )));
    }
    Ok(micros as i64)
}

/// Tokens as the decimal string `HINCRBY` will read.
///
/// A string rather than an integer argument because it reaches the script
/// untouched by Lua's doubles — see [`scripts`].
fn tokens_arg(tokens: u64) -> Result<String, FairUseError> {
    if tokens > i64::MAX as u64 {
        return Err(FairUseError::Backend(anyhow::anyhow!(
            "a draw of {tokens} tokens is past the {} a Redis counter holds; \
             the fair-use buckets count in integers so two nodes agree exactly",
            i64::MAX
        )));
    }
    Ok(tokens.to_string())
}

/// A token cap in the script's vocabulary: `''` for "not capped".
///
/// Clamped rather than refused, unlike a *draw*: a cap past the counter's range
/// is one no sum inside that range can reach, which is what an unreachable cap
/// means in the memory ledger too. Refusing it would turn a harmless
/// configuration into a boot-time failure.
fn cap_tokens(max_tokens: Option<u64>) -> String {
    max_tokens.map_or_else(String::new, |max| max.min(i64::MAX as u64).to_string())
}

/// A dollar cap as micro-dollars, `''` for "not capped".
///
/// A non-finite cap encodes as *absent*, which is what it already means: the
/// memory ledger compares `drawn.usd >= max`, and every comparison against a
/// `NaN` is false while no finite sum reaches an infinity. Spelling it as the
/// absent sentinel is that same answer arrived at once here instead of on
/// every bucket.
fn cap_micros(max_usd: Option<f64>) -> String {
    match max_usd {
        Some(max) if max.is_finite() => (max * MICROS_PER_USD)
            .round()
            .clamp(i64::MIN as f64, i64::MAX as f64)
            .to_string(),
        // `to_string` on the clamped f64 above would print `5000000` for 5.0
        // — an integral f64 formats without a decimal point — which is what
        // `tonumber` wants. Nothing here relies on more than that.
        _ => String::new(),
    }
}

fn scope_caps(limits: &[FairUseLimit], window: FairUseWindow) -> scripts::ScopeCaps {
    match limits.iter().find(|limit| limit.window == window) {
        None => scripts::ScopeCaps::absent(),
        Some(limit) => scripts::ScopeCaps {
            present: true,
            max_tokens: cap_tokens(limit.max_tokens),
            max_micros: cap_micros(limit.max_usd),
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
        // Refused before anything reaches Redis, and for the reason the memory
        // ledger states: a `NaN` in the counters would not blow up, it would
        // make every window sum `NaN` — never `>=` any cap — and the ceiling
        // would silently stop existing. Here it would not even survive the
        // conversion, but a `Backend` error naming a Lua failure is a worse
        // answer than the trait's own `InvalidAmount`, and the contract suite
        // asserts the counters are untouched afterwards.
        if !usd.is_finite() || usd < 0.0 {
            return Err(FairUseError::InvalidAmount {
                field: "usd",
                value: usd,
            });
        }
        let tokens = tokens_arg(tokens)?;
        let micros = micros_of(usd)?.to_string();

        self.scripts
            .record_draw(
                &mut self.conn.clone(),
                scripts::RecordDrawArgs {
                    project_key: &project_bucket_prefix(&principal.project),
                    member_key: &member_bucket_prefix(&principal.project, &principal.user),
                    at_ms,
                    bucket_ms: BUCKET_MS,
                    tokens,
                    micros,
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

    /// Dollars round to the micro-dollar, half up, and the range is stated
    /// rather than silently wrapped.
    #[test]
    fn dollars_convert_to_micro_dollars_once_at_the_edge() {
        assert_eq!(micros_of(0.0).unwrap(), 0);
        assert_eq!(micros_of(5.0).unwrap(), 5_000_000);
        assert_eq!(micros_of(0.35).unwrap(), 350_000);
        // Half up, and the sub-micro-dollar draw the module doc admits is lost.
        assert_eq!(micros_of(0.0000005).unwrap(), 1);
        assert_eq!(micros_of(0.0000004).unwrap(), 0);
        // Past the counter's range: refused, not wrapped into a negative.
        assert!(micros_of(1e14).is_err());
        assert!(tokens_arg(u64::MAX).is_err());
        assert_eq!(tokens_arg(1_000_000_000_000).unwrap(), "1000000000000");
    }

    /// A cap crosses as micro-dollars too, and an absent one as the sentinel
    /// the script tests by string equality.
    #[test]
    fn a_cap_crosses_as_the_same_integers_a_draw_does() {
        assert_eq!(cap_micros(Some(5.0)), "5000000");
        assert_eq!(cap_micros(Some(0.01)), "10000");
        assert_eq!(cap_tokens(Some(1_000)), "1000");
        assert_eq!(cap_tokens(None), "");
        assert_eq!(cap_micros(None), "");
        // A cap of zero is a cap, not an absence — getting these two confused
        // is the difference between a window that refuses everything and one
        // that refuses nothing.
        assert_eq!(cap_tokens(Some(0)), "0");
        assert_eq!(cap_micros(Some(0.0)), "0");
        // A non-finite cap binds on nothing in the memory ledger, and encodes
        // as the absent sentinel here for exactly that reason.
        assert_eq!(cap_micros(Some(f64::NAN)), "");
        assert_eq!(cap_micros(Some(f64::INFINITY)), "");
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
