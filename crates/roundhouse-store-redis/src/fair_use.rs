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
//! | `mark` | the scope's clock: the newest `at_ms`/`now_ms` any call has handed it |
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
//! An admitted turn now costs one `HMGET` per (scope, window) checked — the
//! window's four sum fields and the scope's mark, in one command — and no
//! bucket read at all: amortised O(1), and the steady state this ledger is
//! judged by.
//!
//! **The bounded worst case is bounded by a window's width, not by one
//! command** (M13.1 review, F7). A scope idle for almost a window and then
//! resumed reads every bucket that aged out, and those reads are chunked at
//! four hundred fields per `HMGET` because Lua's `unpack` is bounded: six
//! commands for the seven-day window's 2016 buckets, plus the one that read
//! the sum — seven, measured, not one. It is paid once, after which the sum is
//! current again. A *refusal* still walks buckets under the same chunking,
//! because "which bucket has to leave" is a question no running sum answers,
//! and a refusal is both rare and the case where the client is waiting on that
//! number rather than on a turn.
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
//! And it is the caller's clock *as the scope has seen it so far*: the `mark`
//! field is the high-water mark of every `at_ms` and `now_ms` handed to that
//! scope, and both ledgers evaluate a call at the mark rather than at whatever
//! this one supplied (M13.1 review, R-F9). Where M13.1 shipped with two
//! divergences from the memory ledger under a clock that steps backwards — a
//! draw stamped ahead of a later check, and a check behind an earlier one
//! reading state that check's own decay had deleted — there are now none: the
//! decay this backend owns on the read is irreversible, so the fix is that no
//! call can ask for the state before it. Both ledgers are deterministic
//! functions of (the draws, the mark), and `contract` asserts it of each.
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

fn window_span(window: FairUseWindow) -> scripts::WindowSpan {
    scripts::WindowSpan {
        span_ms: window.span_ms(),
        name: window.wire_name(),
    }
}

/// Every window, narrowest first, and nothing else — what a *draw* sends.
///
/// A draw has no terms: it moves the sums of every window the vocabulary
/// names, whether or not anyone has configured that window, because the
/// ceiling a later admission is judged against is read off a live control
/// plane that an admin `PATCH` can widen between the two. It therefore has no
/// caps to send, which since M13.1's review is a fact about the type rather
/// than two `ScopeCaps::absent()` per window built for a script that never
/// reads them (F3).
fn window_spans() -> [scripts::WindowSpan; FairUseWindow::ALL.len()] {
    FairUseWindow::ALL.map(window_span)
}

/// Every window, narrowest first, with the caps `terms` puts on each scope —
/// what a *check* sends, and the only argument list that carries caps at all.
fn window_caps(terms: &FairUseTerms) -> [scripts::WindowCaps; FairUseWindow::ALL.len()] {
    FairUseWindow::ALL.map(|window| scripts::WindowCaps {
        span: window_span(window),
        project: scope_caps(&terms.project, window),
        member: scope_caps(&terms.member, window),
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
    ///
    /// Through [`crate::connect_manager`] rather than
    /// `ConnectionManagerConfig::default()` — see its doc for the outage
    /// latency this crate's one `connect` bounds (M13.1 review F2): a check
    /// against a severed store must fail closed within a couple of seconds,
    /// not the crate default's ~9.45s.
    pub async fn connect(url: impl AsRef<str>) -> Result<Self, FairUseError> {
        let conn = crate::connect_manager(url.as_ref())
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
                    windows: window_spans(),
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
                    windows: window_caps(terms),
                },
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_only_tokens(window: FairUseWindow, max_tokens: u64) -> FairUseTerms {
        FairUseTerms {
            project: vec![FairUseLimit {
                window,
                max_tokens: Some(max_tokens),
                max_usd: None,
            }],
            member: Vec::new(),
        }
    }

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
        let windows = window_caps(&terms);
        let described: Vec<(&str, u64, bool, &str)> = windows
            .iter()
            .map(|window| {
                (
                    window.span.name,
                    window.span.span_ms,
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

    /// A *draw*'s argument list is every window, narrowest first, and nothing
    /// else.
    ///
    /// The control on the shape above, and the reason `record_draw` takes a
    /// window list at all: a draw made while a project had no fair-use block
    /// must still move the sums an admin `PATCH` can start enforcing a minute
    /// later, or the first turn after the patch would be judged against a
    /// window that had counted nothing.
    ///
    /// **The other half of the claim is no longer an assertion.** This was a
    /// test that the two `ScopeCaps` a draw built were dummies — a check that
    /// a field the draw path had to fill was ignored by the script that read
    /// it (M13.1 review, F3). The caps live on `WindowCaps` now and a draw
    /// sends `WindowSpan`s, so "a draw carries no caps" is a property of the
    /// type: there is nothing to assert and nothing a caller could get wrong.
    #[test]
    fn a_draw_carries_every_window_as_a_span_and_nothing_else() {
        let described: Vec<(&str, u64)> = window_spans()
            .iter()
            .map(|window| (window.name, window.span_ms))
            .collect();
        assert_eq!(
            described,
            FairUseWindow::ALL
                .iter()
                .map(|window| (window.wire_name(), window.span_ms()))
                .collect::<Vec<_>>(),
            "every window the vocabulary names reaches the draw script, in \
             ascending span order, carrying exactly the two values the \
             script's two-per-window group reads"
        );
    }

    /// Serializes every test in this module that talks to a real Redis
    /// server, the way `roundhouse-server`'s `captured_warnings` serializes
    /// tests that share tracing's global interest cache — same shape of
    /// hazard, a process-wide resource two tests can each disturb without
    /// touching the other's state directly.
    ///
    /// `would_exceed_worst_case_seven_day_decay_reads_six_bucket_chunks`
    /// below counts `HMGET` calls off `INFO commandstats`, which is
    /// server-wide: it has no way to tell its own commands from a
    /// concurrently-running neighbour's. `tests/fair_use_storage.rs`'s own
    /// invariant comment ("one measuring loop per test binary") names half
    /// of that hazard — two such counting loops racing each other — but
    /// undersells it: `a_windows_four_sum_fields_move_as_a_set` right here
    /// is not a measuring loop at all, just an ordinary test that happens to
    /// issue its own `HMGET`s against the same real server, and running it
    /// concurrently with the measuring test below inflated the count from 7
    /// to 8 or 9 depending on how much of its traffic landed inside the
    /// `RESETSTAT`-to-`INFO` window (found the hard way: `--lib
    /// --include-ignored` at default thread count failed nearly every run,
    /// `--test-threads=1` never did). The fix generalizes the invariant to
    /// what it actually needs to say: no other real-Redis test in this
    /// binary may run while a commandstats measurement is in flight, full
    /// stop, whether or not that other test is itself counting anything.
    /// Held for a test's entire body, connect through cleanup, because the
    /// neighbour's traffic can land at any point while it holds the counter
    /// uncontended, not only during whatever window this test happens to be
    /// in.
    static REAL_REDIS_TESTS_RUN_ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// M13.1 review F7's other half, held as a premise rather than assumed:
    /// **a window's four sum fields move as a set.**
    ///
    /// `decay` had an arm for a `from` with no `to` beside it. Nothing can
    /// write that state — `record_draw` writes the four fields in one `HSET`
    /// and `persist_sum` writes four or deletes four — so the arm was
    /// unreachable and is gone. That is a claim about every write this crate
    /// makes, which is why it is checked after each of the three states a
    /// window's sum passes through: freshly drawn, decayed away to nothing,
    /// and restarted by a draw that outlived everything before it.
    #[tokio::test]
    #[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
    async fn a_windows_four_sum_fields_move_as_a_set() {
        let _serialized = REAL_REDIS_TESTS_RUN_ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let url = std::env::var("ROUNDHOUSE_TEST_REDIS_URL")
            .expect("--include-ignored asks for the real backend; set ROUNDHOUSE_TEST_REDIS_URL");
        let ledger = RedisFairUseLedger::connect(&url)
            .await
            .expect("the test Redis must be reachable");
        let client = redis::Client::open(url.as_str()).expect("a valid redis url");
        let mut conn = ConnectionManager::new(client)
            .await
            .expect("the test Redis must be reachable");

        let ada = Principal::new(
            ProjectId::new("f7-sum-fields-move-as-a-set"),
            UserId::new("ada"),
        );
        let project_key = project_scope_key(&ada.project);
        let member_key = member_scope_key(&ada.project, &ada.user);
        let _: () = redis::cmd("DEL")
            .arg(&project_key)
            .arg(&member_key)
            .query_async(&mut conn)
            .await
            .expect("the test Redis must accept a DEL");

        async fn assert_whole(conn: &mut ConnectionManager, key: &str, after: &str) {
            for window in FairUseWindow::ALL {
                let (t, u, from, to) = window_sum_fields(window);
                let present: Vec<Option<String>> = redis::cmd("HMGET")
                    .arg(key)
                    .arg(&t)
                    .arg(&u)
                    .arg(&from)
                    .arg(&to)
                    .query_async(conn)
                    .await
                    .expect("HMGET on a window's sum fields must succeed");
                let count = present.iter().filter(|field| field.is_some()).count();
                assert!(
                    count == 0 || count == 4,
                    "{after}: the {} window holds {count} of its four sum \
                     fields ({present:?}). `decay` reads `from` and `to` as \
                     one fact, so a partial set is a state its dropped \
                     `to == nil` arm would have had to guard",
                    window.wire_name()
                );
            }
        }

        let five_hours = project_only_tokens(FairUseWindow::FiveHours, 1);
        ledger.record_draw(&ada, 0, 10, 0.0).await.unwrap();
        assert_whole(&mut conn, &project_key, "after a first draw").await;

        // Six hours on: the five-hour window's sum has aged out entirely and
        // is deleted, the wider two still hold the draw.
        ledger
            .would_exceed(&ada, &five_hours, 6 * 60 * 60_000)
            .await
            .unwrap();
        assert_whole(&mut conn, &project_key, "after a window decayed away").await;

        // Eight days on: past the widest window, so the draw above is pruned
        // and every sum is restarted from this one.
        let later = 8 * 24 * 60 * 60_000;
        ledger.record_draw(&ada, later, 10, 0.0).await.unwrap();
        assert_whole(&mut conn, &project_key, "after a draw past every window").await;
        ledger
            .would_exceed(&ada, &five_hours, later + 60_000)
            .await
            .unwrap();
        assert_whole(
            &mut conn,
            &project_key,
            "after a check on the restarted sum",
        )
        .await;
        assert_whole(&mut conn, &member_key, "the member scope, throughout").await;

        let _: () = redis::cmd("DEL")
            .arg(&project_key)
            .arg(&member_key)
            .query_async(&mut conn)
            .await
            .expect("cleanup DEL must succeed");
    }

    /// M13.1 review F7, closed: the module doc's worst-case cost bound is
    /// measured against the real `WOULD_EXCEED` script rather than read off
    /// the arithmetic.
    ///
    /// The doc claimed "one `HMGET` of at most that window's fields" until
    /// this test measured seven, because `scripts::decay`'s `read_buckets`
    /// chunks any range wider than `CHUNK = 400` buckets into that many
    /// separate `HMGET` calls (Lua's `unpack` is bounded by the C stack) and
    /// the 7-day window is 2016 buckets wide. The doc now states the chunked
    /// bound; this is what holds it to it.
    ///
    /// The fixture forces the worst case without waiting a week: draw once
    /// at bucket 0, draw again at bucket `2015` (still inside the first
    /// draw's own 7-day span, so the widest-window decay `record_draw` runs
    /// on every draw clamps its own `first` to 0 and leaves the sum's
    /// `from` at 0 while `to` advances to `2015`) — then check at the
    /// moment the window's start exactly reaches `2015`. That is `to >=
    /// first` (not the drop branch) and a `2015`-bucket gap, comfortably
    /// inside `(first - from) <= floor(span_ms / bucket_ms) + 1`, so
    /// `decay` takes the steady-state subtract branch and walks all `2015`
    /// aged-out buckets in one call.
    ///
    /// `CONFIG RESETSTAT` before the one `would_exceed` invocation and
    /// `INFO commandstats` after it count every `HMGET` the script actually
    /// issued: the outer read of the window's four sum fields and the scope's
    /// mark (1), plus `read_buckets`'s `ceil(2015 / 400) = 6` chunks. A bound
    /// of one would read `1`; `CHUNK` makes it `7`.
    #[tokio::test]
    #[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
    async fn would_exceed_worst_case_seven_day_decay_reads_six_bucket_chunks() {
        let _serialized = REAL_REDIS_TESTS_RUN_ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let url = std::env::var("ROUNDHOUSE_TEST_REDIS_URL")
            .expect("--include-ignored asks for the real backend; set ROUNDHOUSE_TEST_REDIS_URL");
        let client = redis::Client::open(url.as_str()).expect("a valid redis url");
        let mut conn = ConnectionManager::new(client)
            .await
            .expect("the test Redis must be reachable");
        let scripts = scripts::Scripts::new();

        let project = ProjectId::new("f7-decay-chunk-worst-case");
        let user = UserId::new("ada");
        let project_key = project_scope_key(&project);
        let member_key = member_scope_key(&project, &user);

        // A clean slate: a re-run of this test must not inherit a previous
        // run's sums or bucket fields.
        let _: () = redis::cmd("DEL")
            .arg(&project_key)
            .arg(&member_key)
            .query_async(&mut conn)
            .await
            .expect("the test Redis must accept a DEL");

        let span_ms = FairUseWindow::SevenDays.span_ms();
        let window_buckets = span_ms / BUCKET_MS;
        assert_eq!(
            window_buckets, 2016,
            "sanity: the rest of this test's arithmetic assumes a 2016-bucket \
             7-day window at today's BUCKET_MS"
        );
        let draw_index_far = window_buckets - 1; // 2015

        fn windows_absent() -> [scripts::WindowCaps; FairUseWindow::ALL.len()] {
            FairUseWindow::ALL.map(|window| scripts::WindowCaps {
                span: window_span(window),
                project: scripts::ScopeCaps::absent(),
                member: scripts::ScopeCaps::absent(),
            })
        }

        let counts = DrawCounts::of(10, 0.01).expect("a valid draw");

        scripts
            .record_draw(
                &mut conn,
                scripts::RecordDrawArgs {
                    project_key: &project_key,
                    member_key: &member_key,
                    at_ms: 1,
                    bucket_ms: BUCKET_MS,
                    counts,
                    max_count: MAX_COUNT,
                    ttl_ms: 60_000,
                    windows: window_spans(),
                },
            )
            .await
            .expect("draw 1 (bucket 0) must be recorded");

        let at_ms_far = draw_index_far * BUCKET_MS + 1;
        scripts
            .record_draw(
                &mut conn,
                scripts::RecordDrawArgs {
                    project_key: &project_key,
                    member_key: &member_key,
                    at_ms: at_ms_far,
                    bucket_ms: BUCKET_MS,
                    counts,
                    max_count: MAX_COUNT,
                    ttl_ms: 60_000,
                    windows: window_spans(),
                },
            )
            .await
            .expect("draw 2 (bucket 2015) must be recorded");

        // Sanity on the state the worst case depends on: `from` must still
        // be 0 (the widest-window decay on draw 2 saw its own `first`
        // clamped to 0, since bucket 2015 is not yet 7 days past the
        // epoch) and `to` must have advanced to 2015. If either drifted,
        // the check below would not be exercising the gap this test means
        // to force.
        let (_, _, from_field, to_field) = window_sum_fields(FairUseWindow::SevenDays);
        let (from, to): (Option<String>, Option<String>) = redis::cmd("HMGET")
            .arg(&project_key)
            .arg(&from_field)
            .arg(&to_field)
            .query_async(&mut conn)
            .await
            .expect("HMGET on the 7-day sum fields must succeed");
        assert_eq!(
            from.as_deref(),
            Some("0"),
            "F7 fixture check: the 7-day sum's `from` must still be 0"
        );
        assert_eq!(
            to.as_deref(),
            Some(draw_index_far.to_string().as_str()),
            "F7 fixture check: the 7-day sum's `to` must have advanced to \
             the second draw's bucket"
        );

        // The moment the window's start (`first`) exactly reaches
        // `draw_index_far`: `to (2015) >= first (2015)` keeps this out of
        // the drop branch, and `first - from == 2015` is still `<=
        // floor(span_ms / bucket_ms) + 1 == 2017`, so `decay` takes the
        // steady-state subtract branch and walks the full `[0, 2014]`
        // range in one `read_buckets` call.
        let now_ms = span_ms + draw_index_far * BUCKET_MS + 1;

        let mut windows = windows_absent();
        for window in windows.iter_mut() {
            if window.span.name == FairUseWindow::SevenDays.wire_name() {
                // Present only on the project scope of the 7-day window, and
                // capped far above anything this test ever draws — a
                // refusal would walk buckets a second time on its own
                // account and confound the count this test exists to take.
                window.project = scripts::ScopeCaps {
                    present: true,
                    max_tokens: "1000000000".to_string(),
                    max_micros: "1000000000".to_string(),
                };
            }
        }

        let _: () = redis::cmd("CONFIG")
            .arg("RESETSTAT")
            .query_async(&mut conn)
            .await
            .expect("CONFIG RESETSTAT must succeed on the test Redis");

        let refusal = scripts
            .would_exceed(
                &mut conn,
                scripts::WouldExceedArgs {
                    project_key: &project_key,
                    member_key: &member_key,
                    now_ms,
                    bucket_ms: BUCKET_MS,
                    max_count: MAX_COUNT,
                    windows,
                },
            )
            .await
            .expect("would_exceed must succeed");
        assert!(
            refusal.is_none(),
            "F7: the cap was set far above the draws; a refusal here means \
             the arithmetic drifted from what this test assumes, and the \
             HMGET count below would not mean what this test claims"
        );

        let info: String = redis::cmd("INFO")
            .arg("commandstats")
            .query_async(&mut conn)
            .await
            .expect("INFO commandstats must succeed");
        let hmget_calls: u64 = info
            .lines()
            .find_map(|line| line.strip_prefix("cmdstat_hmget:calls="))
            .and_then(|rest| rest.split(',').next())
            .and_then(|n| n.parse().ok())
            .expect(
                "a cmdstat_hmget line must be present in INFO commandstats \
                 after a HMGET-issuing script ran since the last RESETSTAT",
            );

        // The bound the module doc now states: the outer state read (1) plus
        // `read_buckets`'s CHUNK=400 chunking of the 2015-bucket gap
        // (ceil(2015 / 400) == 6), which is 7 — not the single HMGET the doc
        // promised before this was measured (M13.1 review, F7).
        assert_eq!(
            hmget_calls, 7,
            "F7: a would_exceed check that has 2015 buckets to age out costs \
             {hmget_calls} HMGETs — 1 outer read of the sum fields and the \
             mark, plus 6 CHUNK=400-bounded bucket-range reads — which is \
             the bound the module doc has to state"
        );

        let _: () = redis::cmd("DEL")
            .arg(&project_key)
            .arg(&member_key)
            .query_async(&mut conn)
            .await
            .expect("cleanup DEL must succeed");
    }
}
