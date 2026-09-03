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
//! | `rh:{<project_id>}:fairuse:p` | hash | everything drawn by anyone in the project |
//! | `rh:{<project_id>}:fairuse:m:<user_id>` | hash | the same, for one member |
//!
//! One hash per *scope*, and two families of fields inside it:
//!
//! | Field | Holds |
//! |---|---|
//! | `b:<index>:t`, `b:<index>:u` | one bucket's tokens and micro-dollars |
//! | `s:<window>:t`, `s:<window>:u` | that window's running sum |
//! | `s:<window>:from`, `s:<window>:to` | the oldest and newest bucket index that sum includes |
//!
//! `<index>` is `at_ms / BUCKET_MS`, the same floor division the memory
//! ledger's `BTreeMap` is keyed by; `<window>` is the window's own
//! `wire_name`, which is the same string a refusal names and a config file
//! writes. The braces are a Redis Cluster hash tag on the *project* id,
//! exactly as the spend ledger's three keys are: both scopes land in one slot,
//! which is what lets one script read a project's counters and a member's
//! together. A member ceiling checked in a different round trip from the
//! project's is two answers about one turn.
//!
//! # The layout, and the one it replaced (M13.1, R-F6)
//!
//! M13 shipped **bucket-per-key**: one hash per (scope, bucket), expired by
//! Redis at the widest window plus one bucket. It was chosen over a
//! hash-per-scope because that layout "needs a pruning pass nothing currently
//! owns", and letting Redis be the sweeper was worth paying for on the read.
//!
//! **The read is where it was measured, and the measurement is what moved
//! this rung** (M13 review, F4). A `would_exceed` that binds — refuses at the
//! narrowest window — really did cost only the 61 buckets of a five-hour
//! window. But the check only stops early by *finding* a refusal, and an
//! admitted turn is precisely the case where no window binds: the scan then
//! widens through every configured window to the widest, 2017 `HMGET`s per
//! capped scope, 4034 for a membership capped on both, about eight
//! milliseconds of blocking Lua time on the admission path of every ordinary
//! turn — ahead of every queued session-log append on the same Redis. The
//! cost was per command, not per byte, and it landed on the common case
//! rather than the rare one.
//!
//! **Running sums give the pruning an owner, which is what the M13 objection
//! was really about.** Each window's sum is maintained on write, so a ceiling
//! check compares a cap against four fields instead of a window's worth of
//! keys; the read then *decays* that sum — subtracting the buckets that have
//! aged out since `from` and advancing it — and the widest window's decay
//! deletes the bucket fields it ages out. That deletion is the pruning pass,
//! and `record_draw` performs it on every draw, so it runs whether or not any
//! membership has ever configured the widest window. What Redis's own expiry
//! still does is the other half: one `PEXPIRE` on the scope hash, re-armed on
//! every draw, so an idle scope costs nothing at all rather than one field per
//! five minutes forever.
//!
//! An admitted turn now costs one `HMGET` of four fields per (scope, window)
//! checked, amortised O(1); the bounded worst case — a scope idle for almost
//! a window and then resumed — is one `HMGET` of at most that window's fields,
//! once, after which the sum is current again. A *refusal* still walks
//! buckets, because "which bucket has to leave" is a question no running sum
//! answers, and a refusal is both rare and the case where the client is
//! waiting on that number rather than on a turn.
//!
//! **No migration, and that is a fact about this branch rather than a
//! policy.** M13's bucket-per-key layout has never been deployed — it landed
//! here days before this rung replaced it — so no live Redis holds a key of
//! that shape, and a converter would be code with no data to convert and no
//! test that could prove it right. The two layouts do not even collide: the
//! old keys were `…:fairuse:p:<index>`, this one's are `…:fairuse:p`, so a
//! stray old key from a development box is inert rather than misread.
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
//! A running sum has one hazard the old re-summing scan did not, and it is
//! closed rather than tolerated: a sum sitting *at* `MAX_COUNT` has forgotten
//! how far past it the true total went, so subtracting an aged-out bucket
//! from it would empty a window the memory ledger still holds full. The decay
//! therefore rebuilds a saturated window's sum from its own buckets instead
//! of subtracting from it — see `scripts`, where the branch and its cost live.
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
//! summed by the memory ledger's open-ended range and not, once a decay has
//! rebuilt the sum, by this one. Reaching it takes a clock that went backwards
//! between a settle and the next admission, and the divergence it causes is
//! bounded by that skew. It is stated rather than papered over: the
//! alternative — bounding the memory ledger's range to match — would change
//! the specification to fit an implementation, which is the wrong direction.
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

/// The hash holding a project's own counters.
///
/// A key per scope and nothing appended: what used to be a key suffix — the
/// bucket index — is a field name now, which is why
/// [`member_scope_key`]'s embedded user id no longer needs to be disambiguated
/// from it. Two members of one project are two keys; the project itself is a
/// third; and no spelling of a user id can make any of them collide, because
/// the only thing after `m:` is the id.
pub(crate) fn project_scope_key(project: &ProjectId) -> String {
    format!("{KEY_PREFIX}:{{{project}}}:fairuse:p")
}

pub(crate) fn member_scope_key(project: &ProjectId, user: &UserId) -> String {
    format!("{KEY_PREFIX}:{{{project}}}:fairuse:m:{user}")
}

/// One bucket's two field names, and the bucket a timestamp lands in.
///
/// **Compiled only for tests**, and that is the honest shape rather than an
/// oversight: the production path never builds a field name in Rust, because
/// the script builds it server-side from `at_ms` so a caller cannot send a
/// timestamp and an index that disagree. What these exist for is the gated
/// assertions that read the raw hashes — and they are load-bearing there
/// precisely because they are a *second* spelling: every such test writes
/// through the Lua and reads through these, so the two would have to agree by
/// accident to pass. Left ungated they would be dead code in every shipped
/// build, which is a surface that reads as supported and is not.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn bucket_fields(index: u64) -> (String, String) {
    (format!("b:{index}:t"), format!("b:{index}:u"))
}

/// One window's running-sum field names: tokens, micro-dollars, and the
/// oldest and newest bucket index the sum covers. Test-only for
/// [`bucket_fields`]'s reason.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn window_sum_fields(window: FairUseWindow) -> (String, String, String, String) {
    let name = window.wire_name();
    (
        format!("s:{name}:t"),
        format!("s:{name}:u"),
        format!("s:{name}:from"),
        format!("s:{name}:to"),
    )
}

/// Floor division, the same key the memory ledger's `BTreeMap` is keyed by.
/// Test-only for [`bucket_fields`]'s reason.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn bucket_index(at_ms: u64) -> u64 {
    at_ms / BUCKET_MS
}

/// How long an idle scope's hash lives: the widest window plus one bucket.
///
/// Derived from [`FairUseWindow::ALL`] rather than naming the 7-day window, so
/// a wider window added to that enum lengthens this by construction. Plus one
/// bucket because a bucket is included in a window until its *end* has passed
/// out of it — the staircase rule `BUCKET_MS` documents — so a TTL of exactly
/// the span would delete a hash whose newest bucket is still being summed.
///
/// This is the expiry of the whole scope, re-armed on every draw, and it is
/// what makes an idle principal cost nothing. It is *not* what prunes a busy
/// one: a scope drawn against every day never expires, and its aged-out
/// bucket fields are deleted by the widest window's decay instead. Two
/// mechanisms because there are two states — idle and busy — and only one of
/// them is something Redis can see.
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

/// Every window, narrowest first, with the caps `terms` puts on each scope.
///
/// `caps` decides what a group carries: the check hands it the membership's
/// own limits, and a draw hands it nothing at all — a draw moves every
/// window's sum whether or not anyone has configured that window, because the
/// ceiling a later admission is judged against is read off a live control
/// plane that an admin `PATCH` can widen between the two.
fn window_args(
    caps: impl Fn(FairUseWindow) -> (scripts::ScopeCaps, scripts::ScopeCaps),
) -> [scripts::WindowArgs; FairUseWindow::ALL.len()] {
    FairUseWindow::ALL.map(|window| {
        let (project, member) = caps(window);
        scripts::WindowArgs {
            span_ms: window.span_ms(),
            name: window.wire_name(),
            project,
            member,
        }
    })
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

    /// Shorten the scope hash's TTL, for the one test that has to watch a
    /// counter actually disappear.
    ///
    /// **Gated behind `test-support` and not a configuration knob.** The
    /// production expiry is derived from the window widths and must stay
    /// derived — an operator who could shorten it would be deleting counters a
    /// window is still summing, which is a ceiling that silently leaks. What
    /// the test needs is not a different policy but a shorter wait, and this
    /// is the smallest seam that gives it one: the ordinary
    /// [`bucket_ttl_ms`] still decides what [`connect`](Self::connect) builds,
    /// and a separate gated test asserts the `PTTL` a real draw arms is the
    /// derived one.
    ///
    /// Named for the bucket lifetime it bounds rather than for the key it is
    /// armed on, which is the name it has had since M13 and the one the gated
    /// suite calls: what the TTL *means* — a counter outlives the widest
    /// window by one bucket and no longer — is unchanged by the layout that
    /// now keeps every one of a scope's counters in one hash.
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
                    project_key: &project_scope_key(&principal.project),
                    member_key: &member_scope_key(&principal.project, &principal.user),
                    at_ms,
                    bucket_ms: BUCKET_MS,
                    counts,
                    max_count: MAX_COUNT,
                    ttl_ms: self.bucket_ttl_ms,
                    windows: window_args(|_| {
                        (scripts::ScopeCaps::absent(), scripts::ScopeCaps::absent())
                    }),
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
        self.scripts
            .would_exceed(
                &mut self.conn.clone(),
                scripts::WouldExceedArgs {
                    project_key: &project_scope_key(&principal.project),
                    member_key: &member_scope_key(&principal.project, &principal.user),
                    now_ms,
                    bucket_ms: BUCKET_MS,
                    max_count: MAX_COUNT,
                    windows: window_args(|window| {
                        (
                            scope_caps(&terms.project, window),
                            scope_caps(&terms.member, window),
                        )
                    }),
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
    /// if the two keys ever drifted to different tags, `WOULD_EXCEED` would
    /// refuse to run at all on a clustered deployment, and this catches it at
    /// build time rather than at first boot against a cluster.
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
        let project_key = project_scope_key(&project);
        let member_key = member_scope_key(&project, &ada);
        let tag = hash_tag(&project_key);
        assert_eq!(tag, "acme", "the tag is the project id, unadorned");
        assert_eq!(hash_tag(&member_key), tag);

        // The control: two projects must land on two tags, or every project
        // would collide onto one slot.
        let other = project_scope_key(&ProjectId::new("other"));
        assert_ne!(hash_tag(&other), tag);
    }

    /// Two members of one project, and the project itself, are three key
    /// spaces — including when a user id contains the separator.
    ///
    /// The hazard this guards shrank when the bucket index moved out of the
    /// key and into a field (M13.1): a member key's last segment is now the
    /// whole of the user id, so there is no longer a trailing numeric segment
    /// a cleverly-spelled id could impersonate. The adversarial id stays in
    /// the list because "the key is the prefix plus the id, and nothing else"
    /// is the property that keeps it true.
    #[test]
    fn no_two_scopes_can_name_one_key() {
        let project = ProjectId::new("acme");
        let mut keys = vec![
            project_scope_key(&project),
            member_scope_key(&project, &UserId::new("ada")),
            member_scope_key(&project, &UserId::new("bob")),
            member_scope_key(&project, &UserId::new("ada:1")),
        ];
        let before = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), before, "every scope is its own key");
    }

    /// A bucket's field names and a window's are two families that cannot
    /// collide, and the index is the memory ledger's floor division.
    #[test]
    fn a_field_name_says_which_family_it_belongs_to() {
        assert_eq!(bucket_index(0), 0);
        assert_eq!(bucket_index(BUCKET_MS - 1), 0);
        assert_eq!(bucket_index(BUCKET_MS), 1);
        assert_eq!(bucket_index(2 * BUCKET_MS + 17), 2);

        assert_eq!(bucket_fields(12), ("b:12:t".into(), "b:12:u".into()));
        assert_eq!(
            window_sum_fields(FairUseWindow::FiveHours),
            (
                "s:5h:t".into(),
                "s:5h:u".into(),
                "s:5h:from".into(),
                "s:5h:to".into()
            ),
            "a window's sum is filed under the same string a refusal names and \
             a config file writes, so there is no second spelling to drift"
        );

        // The families are disjoint, and every window's four are distinct:
        // one hash holds all of them, so a collision would be one counter
        // wearing another's name.
        let mut names: Vec<String> = (0..4)
            .flat_map(|i| {
                let (t, u) = bucket_fields(i);
                [t, u]
            })
            .collect();
        for window in FairUseWindow::ALL {
            let (t, u, from, to) = window_sum_fields(window);
            names.extend([t, u, from, to]);
        }
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), before, "no two counters share a field name");
    }

    /// The expiry is the widest window plus one bucket, derived rather than
    /// written down.
    #[test]
    fn a_scope_outlives_the_widest_window_by_exactly_one_bucket() {
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

    /// The three windows are handed to both scripts narrowest-first, which is
    /// the order the refusal rule *and* the pruning pass depend on: the last
    /// group is the widest, and the widest is the one whose decay deletes.
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
        let windows = window_args(|window| {
            (
                scope_caps(&terms.project, window),
                scope_caps(&terms.member, window),
            )
        });
        let described: Vec<(&str, u64, bool, &str)> = windows
            .iter()
            .map(|window| {
                (
                    window.name,
                    window.span_ms,
                    window.project.present,
                    window.project.max_tokens.as_str(),
                )
            })
            .collect();
        assert_eq!(
            described,
            vec![
                ("5h", 5 * 60 * 60_000, false, ""),
                ("24h", 24 * 60 * 60_000, true, "7"),
                ("7d", 7 * 24 * 60 * 60_000, false, ""),
            ],
            "the one configured window lands in its own slot, the two \
             unconfigured ones are marked absent rather than capped at zero, \
             and the spans ascend so the last group is the widest"
        );
    }

    /// A *draw* carries every window and caps none of them.
    ///
    /// The control on the shape above, and the reason `record_draw` takes a
    /// window list at all: a draw made while a project had no fair-use block
    /// must still move the sums an admin `PATCH` can start enforcing a minute
    /// later, or the first turn after the patch would be judged against a
    /// window that had counted nothing.
    #[test]
    fn a_draw_carries_every_window_and_no_caps() {
        let windows = window_args(|_| (scripts::ScopeCaps::absent(), scripts::ScopeCaps::absent()));
        assert_eq!(windows.len(), FairUseWindow::ALL.len());
        assert!(
            windows
                .iter()
                .all(|window| !window.project.present && !window.member.present)
        );
    }
}
