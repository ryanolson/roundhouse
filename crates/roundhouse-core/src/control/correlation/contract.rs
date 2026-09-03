// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The [`CorrelationMaps`] contract as executable assertions.
//!
//! Every guarantee the trait documents lives here as a test any backend must
//! pass unchanged, exactly as
//! [`fair_use::contract`](crate::control::fair_use::contract) and
//! [`spend::contract`](crate::control::spend::contract) do for the two ledgers
//! one seam over. These assertions were `Conversations`' own unit tests in
//! `roundhouse-server` until a second backend existed to run them; moving them
//! here is what makes "an ambiguous call is remembered wherever it is
//! remembered" a checked property rather than a claim — and, as with spend, it
//! matters more here than for a pure-Rust seam, because the two
//! implementations are written in two languages: Rust over a `HashMap`, Lua
//! over one key per binding.
//!
//! **[`MemoryCorrelationMaps`](super::MemoryCorrelationMaps) is the
//! specification.** Where a backend's own representation cannot reproduce an
//! assertion here, the backend is wrong.
//!
//! Every test mints a fresh `Principal` and a fresh cache key rather than
//! assuming an empty store, so one shared backend instance — one real Redis —
//! can host the whole suite with no cross-test interference.
//!
//! **What is deliberately not asserted here is each backend's *bound*.** The
//! memory maps evict by capacity per principal and the Redis maps expire by
//! TTL per key, and R-C3 makes that difference deliberate: a shared store has
//! no natural place to keep an eviction queue. Asserting a cap would fail a
//! backend that expires by time, and asserting an expiry would fail one that
//! does not; each is pinned beside the implementation that owns it. What is
//! shared — and is here — is what a *lost* binding costs, which is the same
//! `None` an unknown id answers with.
//!
//! The [`correlation_maps_contract_suite!`](crate::correlation_maps_contract_suite)
//! macro is the single list of these tests. A backend instantiates the whole
//! suite with one macro call, so it gets every test or none of them.

use super::CorrelationMaps;
use crate::ids::SessionId;

/// A membership nothing else in the suite shares.
///
/// Borrowed from the spend contract rather than copied, exactly as the
/// fair-use contract borrows it: it mints a `Principal` over a random project,
/// which is the isolation one shared Redis makes mandatory, and a second
/// deliberately-identical copy is one edit away from two suites colliding.
use crate::control::spend::contract::fresh_principal;

/// A namespaced cache key nothing else in the suite shares.
///
/// Shaped like the real thing — `{project}/{user}/{name}` — because that shape
/// is what a backend has to key on: a store that mangled a `/` would pass a
/// suite written over opaque tokens and fail on the first real conversation.
pub fn fresh_key(name: &str) -> String {
    let principal = fresh_principal("ada");
    format!("{}/{}/{name}", principal.project, principal.user)
}

fn session(name: &str) -> SessionId {
    SessionId::new(name)
}

/// **The claim.** A committed generation is read back, and a key nothing has
/// committed is absent rather than zero.
///
/// The absent half is the whole of R-C2's widening of M12.1's F9: zero is a
/// real session id in the shared store the moment any node mints it, so a map
/// that answered zero for an unknown key would hand a reader a log another
/// node may already have forked away from, with a 200 on it.
pub async fn a_committed_generation_is_read_back_and_an_uncommitted_key_is_absent<
    M: CorrelationMaps,
>(
    maps: &M,
) {
    let key = fresh_key("main");
    assert_eq!(
        maps.generation(&key).await.unwrap(),
        None,
        "a key nothing has ever committed must say so rather than answer the \
         generation a first turn would have minted"
    );

    maps.set_generation(&key, 0).await.unwrap();
    assert_eq!(
        maps.generation(&key).await.unwrap(),
        Some(0),
        "and generation zero committed is a different answer from no \
         generation at all — the two are the same number and not the same fact"
    );

    maps.set_generation(&key, 3).await.unwrap();
    assert_eq!(maps.generation(&key).await.unwrap(), Some(3));

    // CONTROL: a second key is untouched by the first key's commits, which is
    // what makes the map a map rather than a counter.
    assert_eq!(maps.generation(&fresh_key("other")).await.unwrap(), None);
}

/// A generation is *set*, in either direction, never advanced.
///
/// The backward step is the one with teeth and it is M14.0's: prefix admission
/// searches a key's family downward as well as upward, so a claim that
/// continues an older generation commits to that older generation. A map that
/// implemented this as `max` — or as an `INCR` — would silently strand every
/// resumed conversation one generation above the log it is actually in.
pub async fn a_generation_is_set_rather_than_advanced_so_a_backward_search_can_commit<
    M: CorrelationMaps,
>(
    maps: &M,
) {
    let key = fresh_key("main");
    maps.set_generation(&key, 7).await.unwrap();
    maps.set_generation(&key, 2).await.unwrap();
    assert_eq!(
        maps.generation(&key).await.unwrap(),
        Some(2),
        "the search that walked back to generation 2 committed there, and the \
         map has to hold what was committed rather than the high-water mark"
    );
}

/// **The claim.** A tool-use id names one session exactly, and only for the
/// principal it was emitted for.
///
/// The three cases are one test because they are one rule: a binding answers
/// its own caller, and answers *nothing* — indistinguishably — to anyone else
/// or for any id nothing ever emitted.
pub async fn an_emitted_call_names_its_session_and_only_for_its_own_principal<
    M: CorrelationMaps,
>(
    maps: &M,
) {
    let ada = fresh_principal("ada");
    let bob = fresh_principal("bob");
    let subagent = session("acme/ada/sub");
    maps.bind_call(&ada, "toolu_sub", &subagent).await.unwrap();

    assert_eq!(
        maps.session_of_call(&ada, "toolu_sub").await.unwrap(),
        Some(subagent),
        "the session that emitted the call is the session the answer to it \
         concerns"
    );
    assert_eq!(
        maps.session_of_call(&bob, "toolu_sub").await.unwrap(),
        None,
        "another tenant presenting the id learns nothing from it"
    );
    assert_eq!(
        maps.session_of_call(&ada, "toolu_never_emitted")
            .await
            .unwrap(),
        None,
        "and an id nothing ever emitted answers exactly as a foreign one does"
    );
}

/// One call id bound by two principals is two bindings, not a collision.
///
/// The partition, asserted from the other side: the ambiguity rule below is
/// about two sessions of *one* principal, and a backend that flattened
/// `(principal, id)` into one key badly — or keyed on the id alone — would
/// make two tenants' identical `call_0` un-answer each other, which is the
/// F14 defect wearing the F15 costume.
pub async fn one_call_id_bound_by_two_principals_is_two_bindings<M: CorrelationMaps>(maps: &M) {
    let ada = fresh_principal("ada");
    let bob = fresh_principal("bob");
    let adas = session("acme/ada/main");
    let bobs = session("globex/bob/main");

    maps.bind_call(&ada, "call_0", &adas).await.unwrap();
    maps.bind_call(&bob, "call_0", &bobs).await.unwrap();

    assert_eq!(
        maps.session_of_call(&ada, "call_0").await.unwrap(),
        Some(adas)
    );
    assert_eq!(
        maps.session_of_call(&bob, "call_0").await.unwrap(),
        Some(bobs)
    );
}

/// **The claim.** A colliding call id from two sessions of one principal is
/// remembered as ambiguous rather than resolved to whichever session bound it
/// last — and it *stays* ambiguous.
///
/// A frontier backend's tool-call ids are globally unique, so this never
/// happens on the routes M12 was built against. A local backend that numbers
/// calls per response (`call_0`, `call_1`, …) can hand the same id to two
/// concurrent conversations of one principal, and a plain overwrite answers
/// the first conversation's still-open `tools/call` with a confident 200 about
/// the second's session.
///
/// The tail is the half that makes "remembered" the right word: an id dropped
/// from the map would read as never-seen, so the *next* binding would look
/// like a first one and start answering confidently again — the defect, one
/// turn later.
pub async fn a_colliding_call_id_is_remembered_as_ambiguous<M: CorrelationMaps>(maps: &M) {
    let ada = fresh_principal("ada");
    let first = session("acme/ada/first");
    let second = session("acme/ada/second");

    maps.bind_call(&ada, "call_0", &first).await.unwrap();
    maps.bind_call(&ada, "call_0", &second).await.unwrap();

    assert_eq!(
        maps.session_of_call(&ada, "call_0").await.unwrap(),
        None,
        "an id bound to two different sessions of one principal names neither, \
         so it must answer exactly as an unknown id does rather than \
         confidently resolving to the second conversation's session while the \
         first conversation's tools/call is still answering it"
    );

    maps.bind_call(&ada, "call_0", &first).await.unwrap();
    assert_eq!(
        maps.session_of_call(&ada, "call_0").await.unwrap(),
        None,
        "and a third binding does not clear the ambiguity: a map that forgot \
         the collision would answer the next claimant confidently"
    );
}

/// The control the ambiguity rule turns on: only a *different* session
/// claiming a held id makes it ambiguous.
///
/// Without this, the cheapest way to satisfy the rule above — treat every
/// re-bind as a collision — would silently un-answer every id a resend or a
/// dedup replay binds twice, which is the ordinary case rather than the
/// pathological one.
pub async fn re_binding_a_call_to_the_session_that_holds_it_changes_nothing<M: CorrelationMaps>(
    maps: &M,
) {
    let ada = fresh_principal("ada");
    let held = session("acme/ada/main");

    maps.bind_call(&ada, "toolu_replayed", &held).await.unwrap();
    maps.bind_call(&ada, "toolu_replayed", &held).await.unwrap();

    assert_eq!(
        maps.session_of_call(&ada, "toolu_replayed").await.unwrap(),
        Some(held),
        "one call seen twice is one call, and the binding it already had is \
         still exactly right"
    );
}

/// **The claim.** A thread rebinds where a call collides, and the two rules
/// are asserted side by side because the contrast is the decision.
///
/// A tool-call id names one emission for ever; a thread id names a
/// conversation, and every fork moves a conversation to a new session. A map
/// that remembered a thread as ambiguous would un-answer every thread the
/// moment its client compacted — the ordinary case, not the pathological one.
pub async fn a_thread_rebinds_where_a_call_collides<M: CorrelationMaps>(maps: &M) {
    let ada = fresh_principal("ada");
    let before = session("acme/ada/main");
    let after = session("acme/ada/main#g1");

    maps.bind_thread(&ada, "thread-parent", &before)
        .await
        .unwrap();
    maps.bind_thread(&ada, "thread-parent", &after)
        .await
        .unwrap();
    assert_eq!(
        maps.session_of_thread(&ada, "thread-parent").await.unwrap(),
        Some(after.clone()),
        "a thread that forked is in the session it forked *to*: the latest \
         binding wins"
    );

    // The contrast, on the same two sessions and the same map: a call id
    // rebound the same way is refused instead.
    maps.bind_call(&ada, "toolu_x", &before).await.unwrap();
    maps.bind_call(&ada, "toolu_x", &after).await.unwrap();
    assert_eq!(maps.session_of_call(&ada, "toolu_x").await.unwrap(), None);
}

/// A thread binding is partitioned by principal, and a thread nobody declared
/// is absent.
///
/// The same two halves the call rule has, asserted separately because the two
/// families are two key spaces: a backend that partitioned one and not the
/// other would pass half a suite.
pub async fn a_thread_binding_is_partitioned_by_principal<M: CorrelationMaps>(maps: &M) {
    let ada = fresh_principal("ada");
    let bob = fresh_principal("bob");
    let adas = session("acme/ada/main");
    let bobs = session("globex/bob/main");

    maps.bind_thread(&ada, "thread-shared-name", &adas)
        .await
        .unwrap();
    maps.bind_thread(&bob, "thread-shared-name", &bobs)
        .await
        .unwrap();

    assert_eq!(
        maps.session_of_thread(&ada, "thread-shared-name")
            .await
            .unwrap(),
        Some(adas),
        "two tenants naming one thread own two threads; one map for both \
         would answer each with the other's session"
    );
    assert_eq!(
        maps.session_of_thread(&bob, "thread-shared-name")
            .await
            .unwrap(),
        Some(bobs)
    );
    assert_eq!(
        maps.session_of_thread(&ada, "thread-never-seen")
            .await
            .unwrap(),
        None,
        "and a thread nothing ever served answers exactly as a foreign one does"
    );
}

/// The three families are three key spaces: a name bound in one is not
/// readable through another.
///
/// Here rather than left implicit because both durable families flatten a
/// `(principal, id)` pair into one key, and a flattening that put two families
/// in one space would make a thread id resolvable as a call id — a confident
/// answer about a session the caller never asked about, which is the whole
/// class of defect this seam exists to close.
pub async fn a_call_a_thread_and_a_generation_do_not_share_a_name<M: CorrelationMaps>(maps: &M) {
    let ada = fresh_principal("ada");
    let bound = session("acme/ada/main");
    let name = "same-string-three-ways";

    maps.bind_call(&ada, name, &bound).await.unwrap();
    assert_eq!(
        maps.session_of_thread(&ada, name).await.unwrap(),
        None,
        "a call id is not a thread id"
    );

    let key = fresh_key(name);
    maps.set_generation(&key, 4).await.unwrap();
    assert_eq!(
        maps.session_of_call(&ada, &key).await.unwrap(),
        None,
        "and a cache key is not a call id"
    );
    assert_eq!(maps.generation(&key).await.unwrap(), Some(4));
}

/// Instantiate the whole conformance suite against one backend.
///
/// The single list of contract tests, in the same idiom and for the same
/// reason as
/// [`fair_use_ledger_contract_suite!`](crate::fair_use_ledger_contract_suite):
/// a backend gets the entire suite in one call, so there is no per-test wiring
/// step where one test can be forgotten for one backend while the others keep
/// enforcing it.
///
/// `$make` is evaluated inside each generated test, so every test gets a fresh
/// handle and a backend whose construction is async passes an `.await`
/// expression. The optional `ignore = "…"` prefix stamps that reason as
/// `#[ignore]` on every generated test — how an infrastructure-gated backend
/// applies its gate suite-wide.
///
/// ```ignore
/// roundhouse_core::correlation_maps_contract_suite!(MemoryCorrelationMaps::new());
///
/// roundhouse_core::correlation_maps_contract_suite!(
///     ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored",
///     connect_from_env().await
/// );
/// ```
///
/// Only usable where the `contract` module is compiled: this crate's own
/// tests, or a dependent with the `test-support` feature on its
/// dev-dependency.
#[macro_export]
macro_rules! correlation_maps_contract_suite {
    (ignore = $reason:literal, $make:expr $(,)?) => {
        $crate::correlation_maps_contract_suite!(@list (#[ignore = $reason]) $make);
    };
    ($make:expr $(,)?) => {
        $crate::correlation_maps_contract_suite!(@list () $make);
    };
    // The single list. Both public arms land here, so gated and ungated
    // backends cannot drift apart in coverage. The recursion that turns this
    // list into one `#[tokio::test]` per name is
    // [`__contract_suite!`](crate::__contract_suite), shared with the other
    // three families (M14.1 review, F6).
    (@list $attrs:tt $make:expr) => {
        $crate::__contract_suite!(maps, $crate::control::correlation::contract, $attrs, $make;
            a_committed_generation_is_read_back_and_an_uncommitted_key_is_absent,
            a_generation_is_set_rather_than_advanced_so_a_backward_search_can_commit,
            an_emitted_call_names_its_session_and_only_for_its_own_principal,
            one_call_id_bound_by_two_principals_is_two_bindings,
            a_colliding_call_id_is_remembered_as_ambiguous,
            re_binding_a_call_to_the_session_that_holds_it_changes_nothing,
            a_thread_rebinds_where_a_call_collides,
            a_thread_binding_is_partitioned_by_principal,
            a_call_a_thread_and_a_generation_do_not_share_a_name,
        );
    };
}
