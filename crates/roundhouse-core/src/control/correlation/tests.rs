// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! [`correlation`](super)'s unit tests, in their own file for the reason
//! `conversations/tests.rs` one crate over already is: the trait, a backend
//! and their tests in one file is the file a reader has to read all of to
//! find any of it (M14.2 review, F1).
//!
//! The behavioural assertions themselves live in [`contract`](super::contract)
//! and run from here by the macro below, for the reason the fair-use ledger's
//! do: leaving them here would judge the memory maps by one list and Redis by
//! another, which is the exact drift the suite exists to make impossible.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::contract::AdvancePastTheBound;
use super::{
    AgedTable, CALL_BINDING_STALENESS_MS, CorrelationMaps, MemoryCorrelationMaps, REMEMBERED_CALLS,
    REMEMBERED_THREADS, THREAD_BINDING_STALENESS_MS,
};
use crate::control::spend::contract::fresh_principal;
use crate::ids::SessionId;

crate::correlation_maps_contract_suite!(MemoryCorrelationMaps::new(), aged = aged_memory_maps(),);

/// A shared, settable clock a test can move without sleeping — the memory
/// side's half of R-S4's "clock seam each implementation already has for
/// tests", the Redis side's being its per-handle TTL lever.
fn scripted_clock() -> (impl Fn() -> u64 + Send + Sync + 'static, Arc<AtomicU64>) {
    let now = Arc::new(AtomicU64::new(0));
    let read = Arc::clone(&now);
    (move || read.load(Ordering::Relaxed), now)
}

/// This backend's answer to the contract's "advance past the bound" hook
/// (M14.2 review, F4): move the scripted clock past the wider of the two
/// bounds, which is no wait at all. The Redis instantiation answers the same
/// hook by shortening its per-handle TTL and waiting that out — an
/// instantiation may wait out an expiry it owns, and the shared assertion
/// itself never sleeps.
fn aged_memory_maps() -> (MemoryCorrelationMaps, AdvancePastTheBound) {
    let (clock, now) = scripted_clock();
    let maps = MemoryCorrelationMaps::new().with_clock(clock);
    let advance: AdvancePastTheBound = Box::new(move || {
        now.fetch_add(
            CALL_BINDING_STALENESS_MS.max(THREAD_BINDING_STALENESS_MS) + 1,
            Ordering::Relaxed,
        );
        Box::pin(std::future::ready(()))
    });
    (maps, advance)
}

// ---------------------------------------------------------------------------
// The bounded, aged table itself (M14.2 review, F2 — R-S5)
// ---------------------------------------------------------------------------

/// A table with both bounds and a clock a test owns.
fn aged_table<V>(capacity: usize, bound_ms: u64) -> (AgedTable<V>, Arc<AtomicU64>) {
    let (clock, now) = scripted_clock();
    (
        AgedTable::new(capacity, Some(bound_ms)).with_clock(clock),
        now,
    )
}

/// **R-S1 on the type that owns it: a read past the bound answers absent and
/// drops the entry**, rather than answering absent and leaving it for a sweep
/// that may not come.
#[test]
fn a_read_past_the_bound_is_absent_and_takes_the_entry_with_it() {
    let (mut table, now) = aged_table::<&str>(8, 1_000);
    table.write("k", |_| "v");

    // CONTROL: at the bound exactly, not past it — `is_stale` is a strict
    // `>`, and the two implementations have to agree on which side of the
    // bound the bound itself is.
    now.store(1_000, Ordering::Relaxed);
    assert_eq!(table.get("k"), Some(&"v"));
    assert_eq!((table.len(), table.queue_len()), (1, 1));

    now.store(1_001, Ordering::Relaxed);
    assert_eq!(table.get("k"), None);
    assert_eq!(
        (table.len(), table.queue_len()),
        (0, 0),
        "the read that answered absent must have dropped the entry and its \
         queue position together"
    );
}

/// **M14.2 review, F3, on the type: a write past the bound is a first
/// write.** `update` sees `None`, so a caller cannot resolve a new claim
/// against a binding that R-S1 says is already absent.
#[test]
fn a_write_past_the_bound_shows_the_update_no_held_value() {
    let (mut table, now) = aged_table::<&str>(8, 1_000);
    table.write("k", |held| {
        assert!(held.is_none(), "control: nothing is held on a first write");
        "first"
    });

    // CONTROL: inside the bound, the held value is shown to the update.
    now.store(1_000, Ordering::Relaxed);
    table.write("k", |held| {
        assert_eq!(held, Some("first"), "still live at the bound exactly");
        "second"
    });

    now.store(2_001, Ordering::Relaxed);
    table.write("k", |held| {
        assert!(
            held.is_none(),
            "F3: the entry aged out before this write, so this write is a \
             first write — the same thing it is against a Redis key that has \
             already expired"
        );
        "third"
    });
    assert_eq!(table.get("k"), Some(&"third"));
}

/// **M14.2 review, F8: a write moves its entry to the queue's tail.** A
/// rebind that kept its original position leaves a fresh entry at the head,
/// where it stops the age sweep before it reaches the stale entries behind it
/// — and is then the very entry the capacity cap pops.
#[test]
fn a_write_moves_its_entry_to_the_tail_so_a_fresh_head_never_shields_stale_ones() {
    let (mut table, now) = aged_table::<u32>(4, 1_000);
    for n in 0..4 {
        table.write(&format!("k{n}"), |_| n);
    }
    assert_eq!(table.len(), 4, "setup: the table is exactly at its cap");

    // Past the bound, rebind the entry that is currently at the head, then
    // write one unrelated key.
    now.store(1_001, Ordering::Relaxed);
    table.write("k0", |_| 100);
    table.write("k-new", |_| 200);

    assert_eq!(
        table.get("k0"),
        Some(&100),
        "F8: k0 was rebound one write ago — if its refreshed entry had stayed \
         at the head it would have shielded every stale entry behind it from \
         the age sweep, and the cap would then have evicted it as the head"
    );
    assert_eq!(
        table.len(),
        2,
        "and the stale entries the head would have shielded are gone: only \
         the rebound entry and the new one are left"
    );
}

/// A rebind spends no second queue position, whatever else it does — the
/// invariant that keeps the cap from evicting a key that is still live.
#[test]
fn a_rebind_does_not_spend_a_second_queue_position() {
    let (mut table, _now) = aged_table::<u32>(4, 1_000);
    for _ in 0..10 {
        table.write("hot", |held| held.unwrap_or(0) + 1);
    }
    assert_eq!((table.len(), table.queue_len()), (1, 1));
    assert_eq!(table.get("hot"), Some(&10));
}

/// **M14.2 review, F9: the cap evicts oldest-first among *evictable* entries
/// and never a pinned one.** The pinned entry here is the oldest in the
/// table, so a cap that ignored the pin would take it first.
#[test]
fn the_cap_evicts_the_oldest_evictable_entry_and_steps_over_a_pinned_one() {
    let mut table: AgedTable<bool> = AgedTable::new(3, None).with_pinned(|pinned| *pinned);
    table.write("pinned", |_| true);
    table.write("a", |_| false);
    table.write("b", |_| false);
    table.write("c", |_| false);

    assert_eq!(table.len(), 3);
    assert_eq!(
        table.get("pinned"),
        Some(&true),
        "F9: the pinned entry is the oldest in the table, and a cap that \
         evicted oldest-first without asking would have taken exactly it"
    );
    assert_eq!(
        table.get("a"),
        None,
        "the oldest *evictable* entry is the one the cap took"
    );
    assert_eq!(table.get("c"), Some(&false));

    // CONTROL: unpinning is a write, and the entry is then ordinary. It is
    // also now the newest, so filling the table past its cap takes the older
    // evictable entries first and leaves it.
    table.write("pinned", |_| false);
    table.write("d", |_| false);
    table.write("e", |_| false);
    assert_eq!(
        table.get("pinned"),
        Some(&false),
        "an unpinned entry is evicted by ordinary age-of-write, and this one \
         is the third newest of three"
    );
    assert_eq!(table.len(), 3);
}

/// A table with no staleness bound ages nothing out, however far the clock
/// moves — R-S2's memo, whose entries are hints a probe corrects rather than
/// answers a caller trusts.
#[test]
fn a_table_with_no_staleness_bound_never_ages_anything_out() {
    let (clock, now) = scripted_clock();
    let mut table: AgedTable<&str> = AgedTable::new(8, None).with_clock(clock);
    table.write("k", |_| "v");

    now.store(u64::MAX, Ordering::Relaxed);
    assert_eq!(table.get("k"), Some(&"v"));
    assert_eq!(table.len(), 1);
}

// ---------------------------------------------------------------------------
// The two per-principal tables over it
// ---------------------------------------------------------------------------

/// The remembered-calls cap evicts oldest-first, and a re-binding does not
/// spend a queue slot.
///
/// Not in the contract: it is a claim about *this* backend's bound, and a
/// backend that expires by time instead — as the Redis one does — has no
/// queue to assert on. What losing an entry costs is the fallback the
/// contract's own "an unknown id answers `None`" already pins.
#[tokio::test]
async fn the_call_table_is_capped_and_forgets_its_oldest_bindings_first() {
    let maps = MemoryCorrelationMaps::new();
    let ada = fresh_principal("ada");
    let session = SessionId::new("acme/ada/main");
    for n in 0..=REMEMBERED_CALLS {
        maps.bind_call(&ada, &format!("toolu_{n}"), &session)
            .await
            .unwrap();
    }

    assert_eq!(
        maps.session_of_call(&ada, "toolu_0").await.unwrap(),
        None,
        "the oldest binding is the one the cap gives up"
    );
    assert_eq!(
        maps.session_of_call(&ada, &format!("toolu_{REMEMBERED_CALLS}"))
            .await
            .unwrap(),
        Some(session),
        "and the newest is kept, which is the one a live tool loop is \
         about to answer"
    );
    assert_eq!(
        maps.lock().calls.sizes(&ada),
        (REMEMBERED_CALLS, REMEMBERED_CALLS)
    );

    // Re-binding an id already held must not grow the order queue past the
    // map, or the cap evicts a key that is still live and the two halves
    // drift apart.
    maps.bind_call(&ada, "toolu_1", &SessionId::new("acme/ada/other"))
        .await
        .unwrap();
    let (held, ordered) = maps.lock().calls.sizes(&ada);
    assert_eq!(ordered, held);
}

/// The remembered-calls cap is per principal, so a co-tenant's tool
/// traffic cannot evict a *different* principal's binding (M12 review,
/// F15) — the half the oldest-first test above does not cover, that one
/// being the control that a tenant still ages out its own oldest entry.
#[tokio::test]
async fn a_co_tenants_call_traffic_does_not_evict_another_principals_call_binding() {
    let maps = MemoryCorrelationMaps::new();
    let ada = fresh_principal("ada");
    let bob = fresh_principal("bob");
    let subagent = SessionId::new("acme/ada/sub");
    maps.bind_call(&ada, "toolu_ada_sub", &subagent)
        .await
        .unwrap();

    let bobs = SessionId::new("globex/bob/main");
    for n in 0..REMEMBERED_CALLS {
        maps.bind_call(&bob, &format!("toolu_bob_{n}"), &bobs)
            .await
            .unwrap();
    }

    assert_eq!(
        maps.session_of_call(&ada, "toolu_ada_sub").await.unwrap(),
        Some(subagent),
        "a principal's own call binding must survive another tenant's \
         tool traffic; a node-wide cap makes it fall through to the same \
         None a foreign id would answer with"
    );
}

/// The thread cap evicts oldest-first, and a rebinding does not spend a
/// queue slot.
///
/// The second half is the one with teeth: rebinding is the *ordinary* case
/// here (every fork rebinds), so a `bind` that pushed a second order entry
/// would evict live threads at a rate set by how often clients compact.
#[tokio::test]
async fn the_thread_table_is_capped_and_a_rebinding_does_not_grow_its_queue() {
    let maps = MemoryCorrelationMaps::new();
    let ada = fresh_principal("ada");
    let session = SessionId::new("acme/ada/main");
    for n in 0..=REMEMBERED_THREADS {
        maps.bind_thread(&ada, &format!("thread-{n}"), &session)
            .await
            .unwrap();
    }

    assert_eq!(
        maps.session_of_thread(&ada, "thread-0").await.unwrap(),
        None,
        "the oldest binding is the one the cap gives up"
    );
    assert_eq!(
        maps.session_of_thread(&ada, &format!("thread-{REMEMBERED_THREADS}"))
            .await
            .unwrap(),
        Some(session),
        "and the newest is kept, which is the thread a live tool loop is \
         about to answer"
    );
    assert_eq!(
        maps.lock().threads.sizes(&ada),
        (REMEMBERED_THREADS, REMEMBERED_THREADS)
    );

    let forked = SessionId::new("acme/ada/main#g1");
    maps.bind_thread(&ada, "thread-1", &forked).await.unwrap();
    let (held, ordered) = maps.lock().threads.sizes(&ada);
    assert_eq!(ordered, held);
    assert_eq!(
        maps.session_of_thread(&ada, "thread-1").await.unwrap(),
        Some(forked)
    );
}

// ---------------------------------------------------------------------------
// M14.2, R-S1: age, under a scripted clock rather than a sleep
// ---------------------------------------------------------------------------

/// **The claim R-S1 states in the module doc, proved rather than only
/// documented:** a binding older than its family's staleness bound
/// answers exactly as one nothing ever wrote does, on both families, and
/// a binding well inside the bound is untouched by the same clock advance.
#[tokio::test]
async fn a_binding_older_than_the_bound_is_absent_under_a_scripted_clock() {
    let (clock, now) = scripted_clock();
    let maps = MemoryCorrelationMaps::new().with_clock(clock);
    let ada = fresh_principal("ada");
    let session = SessionId::new("acme/ada/main");

    maps.bind_call(&ada, "toolu_ages_out", &session)
        .await
        .unwrap();
    maps.bind_thread(&ada, "thread-ages-out", &session)
        .await
        .unwrap();

    // CONTROL: well inside both bounds, both bindings still answer.
    now.store(60_000, Ordering::Relaxed);
    assert_eq!(
        maps.session_of_call(&ada, "toolu_ages_out").await.unwrap(),
        Some(session.clone())
    );
    assert_eq!(
        maps.session_of_thread(&ada, "thread-ages-out")
            .await
            .unwrap(),
        Some(session.clone())
    );

    // Past the call bound, short of the (much wider) thread bound: the
    // call binding is gone and the thread binding is not, which is the
    // proof the two ages are independent rather than one clock tripping
    // both at once.
    now.store(CALL_BINDING_STALENESS_MS + 1, Ordering::Relaxed);
    assert_eq!(
        maps.session_of_call(&ada, "toolu_ages_out").await.unwrap(),
        None,
        "a binding older than the bound answers exactly as an id nothing \
         ever emitted does"
    );
    assert_eq!(
        maps.session_of_thread(&ada, "thread-ages-out")
            .await
            .unwrap(),
        Some(session.clone()),
        "the thread bound is wider, and this clock has not reached it yet"
    );

    // Past both bounds: the thread binding is gone too.
    now.store(THREAD_BINDING_STALENESS_MS + 1, Ordering::Relaxed);
    assert_eq!(
        maps.session_of_thread(&ada, "thread-ages-out")
            .await
            .unwrap(),
        None
    );
}

/// **Age and count are independent bounds, neither waiting on the
/// other.** A table well under its capacity cap still ages out an idle
/// entry on the next write to a *different* key, and a table with
/// nothing stale still evicts at the cap.
#[tokio::test]
async fn a_write_sweeps_aged_out_entries_from_the_head_independently_of_the_cap() {
    let (clock, now) = scripted_clock();
    let maps = MemoryCorrelationMaps::new().with_clock(clock);
    let ada = fresh_principal("ada");
    let first = SessionId::new("acme/ada/first");
    let second = SessionId::new("acme/ada/second");

    maps.bind_call(&ada, "toolu_stale", &first).await.unwrap();

    // Advance well past the call bound, then bind a second, unrelated
    // call. The table holds two entries, nowhere near REMEMBERED_CALLS —
    // the sweep that drops the first one is the age sweep, not the cap.
    now.store(CALL_BINDING_STALENESS_MS + 1, Ordering::Relaxed);
    maps.bind_call(&ada, "toolu_fresh", &second).await.unwrap();

    let (held, ordered) = maps.lock().calls.sizes(&ada);
    assert_eq!(
        (held, ordered),
        (1, 1),
        "the write that bound the fresh id must have swept the aged-out \
         one from the queue's head, or the table only shrinks when a \
         reader happens to ask about the stale key"
    );
    assert_eq!(
        maps.session_of_call(&ada, "toolu_fresh").await.unwrap(),
        Some(second)
    );
}

/// **M14.2 review, F8, through the whole handle: a thread rebound one write
/// ago is not the cap's next victim**, and the stale entries a fresh head
/// would have shielded are swept.
#[tokio::test]
async fn a_rebound_thread_is_not_evicted_while_stale_entries_stay_resident() {
    let (clock, now) = scripted_clock();
    let maps = MemoryCorrelationMaps::new().with_clock(clock);
    let ada = fresh_principal("ada");
    let session = SessionId::new("acme/ada/main");

    // thread-hot lands at the queue's head; REMEMBERED_THREADS - 1
    // fillers follow it, all written at t=0, so the table is exactly at
    // its cap and every entry — thread-hot included — is the same age.
    maps.bind_thread(&ada, "thread-hot", &session)
        .await
        .unwrap();
    for n in 0..REMEMBERED_THREADS - 1 {
        maps.bind_thread(&ada, &format!("thread-filler-{n}"), &session)
            .await
            .unwrap();
    }
    assert_eq!(
        maps.lock().threads.sizes(&ada),
        (REMEMBERED_THREADS, REMEMBERED_THREADS),
        "setup: the table must be exactly at its cap before the clock \
         moves, or what follows tests the age sweep and the cap \
         conflated rather than the head-shielding interaction between \
         them"
    );

    // Advance well past the staleness bound, then rebind thread-hot — a
    // fork, the module doc's ordinary case.
    now.store(THREAD_BINDING_STALENESS_MS + 1, Ordering::Relaxed);
    let forked = SessionId::new("acme/ada/main#g1");
    maps.bind_thread(&ada, "thread-hot", &forked).await.unwrap();

    // One more, unrelated bind — which would push the table over its cap if
    // the fillers were still there.
    maps.bind_thread(&ada, "thread-new", &session)
        .await
        .unwrap();

    assert_eq!(
        maps.session_of_thread(&ada, "thread-hot").await.unwrap(),
        Some(forked),
        "F8: thread-hot was rebound one write ago — a rebind that kept its \
         original queue position would leave it at the head of a queue of \
         nothing but stale fillers, where it stops the age sweep dead and \
         is then the one entry the count cap pops"
    );
    assert_eq!(
        maps.lock().threads.sizes(&ada),
        (2, 2),
        "and the stale fillers the old head shielded are swept, rather than \
         sitting resident while the table evicts live bindings around them"
    );
}

// ---------------------------------------------------------------------------
// M14.2 review, F3: the bound on the write path
// ---------------------------------------------------------------------------

/// **Control: a second session claiming an id that is still inside the
/// staleness bound is genuinely ambiguous, and the map is right to say
/// so.** Proves the test below is not just returning `None` for an
/// unrelated reason — at exactly the bound (not yet stale, since the
/// staleness check is a strict `>`) two live claims on one id really are
/// unanswerable.
#[tokio::test]
async fn a_second_bind_still_inside_the_bound_is_a_real_collision() {
    let (clock, now) = scripted_clock();
    let maps = MemoryCorrelationMaps::new().with_clock(clock);
    let ada = fresh_principal("ada");
    let first = SessionId::new("acme/ada/first");
    let second = SessionId::new("acme/ada/second");

    maps.bind_call(&ada, "call_0", &first).await.unwrap();
    now.store(CALL_BINDING_STALENESS_MS, Ordering::Relaxed);
    maps.bind_call(&ada, "call_0", &second).await.unwrap();

    assert_eq!(
        maps.session_of_call(&ada, "call_0").await.unwrap(),
        None,
        "still inside the bound, so this is two sessions genuinely \
         fighting over one id — Ambiguous is the right answer"
    );
}

/// **F3: a second session claiming an id whose previous binding is
/// already past [`CALL_BINDING_STALENESS_MS`] is a first bind, not a
/// collision** — R-S1 says a binding older than the bound is absent, and an
/// absent binding is exactly what a first bind sees. The Redis
/// implementation has always read this way, its key having expired by then;
/// the contract now asserts it over both.
#[tokio::test]
async fn a_second_bind_past_the_staleness_bound_is_a_first_bind_not_a_collision() {
    let (clock, now) = scripted_clock();
    let maps = MemoryCorrelationMaps::new().with_clock(clock);
    let ada = fresh_principal("ada");
    let first = SessionId::new("acme/ada/first");
    let second = SessionId::new("acme/ada/second");

    maps.bind_call(&ada, "call_0", &first).await.unwrap();
    now.store(CALL_BINDING_STALENESS_MS + 1, Ordering::Relaxed);
    maps.bind_call(&ada, "call_0", &second).await.unwrap();

    assert_eq!(
        maps.session_of_call(&ada, "call_0").await.unwrap(),
        Some(second),
        "F3: the previous binding was already past the staleness bound \
         when the second session claimed the id, so R-S1 says it was \
         absent — this must read as a first bind, the way Redis's \
         expired key already does, not as a collision with a binding \
         that no longer exists"
    );
}

// ---------------------------------------------------------------------------
// M14.2 review, F2: one bounded table, and what the wrappers keep
// ---------------------------------------------------------------------------

/// Reads the memory backend's own source, so the checks below run against
/// what is actually on disk rather than a copy pasted into the test.
fn memory_source() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = std::path::Path::new(manifest_dir).join("src/control/correlation/memory.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {path:?}: {error}"))
}

/// The substring of `src` starting at (and including) `start`, ending just
/// before `end`'s next occurrence at or after `start`. Panics with the
/// anchors named if either is not found, so a drifted anchor fails loudly
/// rather than silently comparing the wrong text.
fn slice_between<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
    let start_pos = src
        .find(start)
        .unwrap_or_else(|| panic!("start anchor not found: {start:?}"));
    let end_pos = src[start_pos..]
        .find(end)
        .map(|p| p + start_pos)
        .unwrap_or_else(|| panic!("end anchor not found after start: {end:?}"));
    &src[start_pos..end_pos]
}

/// [`slice_between`], but `start` is searched for only after `after`'s
/// occurrence — for picking out the *second* copy of a signature that is
/// textually identical in both `impl` blocks, by anchoring on something that
/// precedes only one of them.
fn slice_after<'a>(src: &'a str, after: &str, start: &str, end: &str) -> &'a str {
    let after_pos = src
        .find(after)
        .unwrap_or_else(|| panic!("context anchor not found: {after:?}"));
    slice_between(&src[after_pos..], start, end)
}

/// F2's own `how_to_prove` normalization — the family's value name, key name
/// and its two constants renamed away — plus dropping whitespace and braces,
/// so what is compared is the logic and not `rustfmt`'s line-length
/// tie-breaking.
fn normalize(block: &str, key: &str, bound_const: &str, cap_const: &str) -> String {
    block
        .replace(key, "K")
        .replace(bound_const, "B")
        .replace(cap_const, "C")
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '{' && *c != '}')
        .collect()
}

fn normalize_calls(block: &str) -> String {
    normalize(
        block,
        "call_id",
        "CALL_BINDING_STALENESS_MS",
        "REMEMBERED_CALLS",
    )
}

fn normalize_threads(block: &str) -> String {
    normalize(
        block,
        "thread_id",
        "THREAD_BINDING_STALENESS_MS",
        "REMEMBERED_THREADS",
    )
}

/// **F2, closed: neither table spells a sweep, a queue or a cap loop of its
/// own.** The duplication the finding named — two byte-identical age-then-cap
/// walks and an O(n) drop on the read path, with a third copy one crate over
/// — is gone into [`AgedTable`], and this is what would go red if a later
/// edit grew one back rather than changing the shared type.
#[test]
fn f2_neither_per_principal_table_carries_a_bounded_map_of_its_own() {
    let src = memory_source();
    let code: String = src
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    for spelled in ["VecDeque", "pop_front", "while ", "written_at", "retain("] {
        assert!(
            !code.contains(spelled),
            "F2: {spelled:?} is a bounded map's own machinery, and there is \
             one bounded map now — memory.rs holds the two value types and \
             their update rules, and AgedTable holds every sweep"
        );
    }
    assert_eq!(
        code.matches("AgedTable::new(").count(),
        2,
        "F2: exactly two instantiations here, one per family"
    );
}

/// **F2, the other half: each family names its cap and its bound exactly
/// once.** The finding counted each bound constant spelled twice, which is
/// how one of the two moves and the other does not.
#[test]
fn f2_each_family_names_its_cap_and_its_bound_exactly_once() {
    let code: String = memory_source()
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    for named in [
        "REMEMBERED_CALLS",
        "CALL_BINDING_STALENESS_MS",
        "REMEMBERED_THREADS",
        "THREAD_BINDING_STALENESS_MS",
    ] {
        assert_eq!(
            code.matches(named).count(),
            2,
            "F2: {named} should appear exactly twice in memory.rs — once in \
             the `use` that brings it in and once where its table is built"
        );
    }
}

/// **The refute pass's correction to F2, kept as a checked claim.** F2's
/// `how_to_prove` said the two tables differ only inside `bind`'s
/// collision arm; they also differ in `session_of`, because a call has a
/// third state (ambiguous) that a thread rebinding has no equivalent of. So
/// the shared type carries the sweep and the wrappers carry *two* variation
/// points, not one — and everything else about them is the same text after
/// mechanical renaming, which this asserts in both directions.
#[test]
fn f2_the_two_tables_differ_only_at_the_collision_arm_and_the_three_state_read() {
    let src = memory_source();

    let calls_table = normalize_calls(slice_between(
        &src,
        "    fn table(&mut self, principal: &Principal, clock: &Clock) -> &mut AgedTable<CallSite> {",
        "\n    fn bind(",
    ));
    let threads_table = normalize_threads(slice_between(
        &src,
        "    fn table(&mut self, principal: &Principal, clock: &Clock) -> &mut AgedTable<SessionId> {",
        "\n    fn bind(",
    ));
    assert_eq!(
        calls_table.replace("CallSite", "V"),
        threads_table.replace("SessionId", "V"),
        "the two tables are built the same way — the cap, the bound and the \
         shared clock — and only the value type differs"
    );

    let calls_sizes = normalize_calls(slice_between(
        &src,
        "    pub(super) fn sizes(&self, principal: &Principal) -> (usize, usize) {",
        "\n}",
    ));
    let threads_sizes = normalize_threads(slice_after(
        &src,
        "pub(super) struct ThreadTable {",
        "    pub(super) fn sizes(&self, principal: &Principal) -> (usize, usize) {",
        "\n}",
    ));
    assert_eq!(
        calls_sizes.replace("calls", "E").replace("threads", "E"),
        threads_sizes.replace("calls", "E").replace("threads", "E"),
        "and the size accessors are the same accessor"
    );

    let calls_read = normalize_calls(slice_between(
        &src,
        "    fn session_of(&mut self, principal: &Principal, call_id: &str) -> Option<SessionId> {",
        "\n\n    /// How many bindings",
    ));
    let threads_read = normalize_threads(slice_between(
        &src,
        "    fn session_of(&mut self, principal: &Principal, thread_id: &str) -> Option<SessionId> {",
        "\n\n    /// How many bindings",
    ));
    assert_ne!(
        calls_read, threads_read,
        "F2 correction: `session_of` is the second variation point, not a \
         copy — a call id resolves through a three-state match (bound, or \
         ambiguous and therefore unanswerable) where a thread id is the \
         newest write and nothing else. A shared type that hooked only \
         `bind`'s collision arm, as F2's how_to_prove described, would still \
         have had to hook this"
    );
}

/// **A spelling guard for R-S4's methodology — through the clock seam,
/// never by waiting out a real timer.** A test author who drops
/// `with_clock`/`scripted_clock` for a real, awaited timer does not fail an
/// assertion here: the test just gets slow, and the only thing that
/// notices today is the workspace's bounded-timeout house rule, which
/// reports an opaque `exit 124` that names neither the timer nor the test.
/// Scanning this file's own source for known real-wait spellings is what
/// `fair_use_contract_convention.rs` does for its sibling-file convention,
/// aimed here instead at the seam-vs-timer one.
///
/// **This scans for the spellings [`banned_wait_spellings`] names, not for
/// "a real wait" in general** (M14.2 review, F11): a single three-segment
/// path was the whole check until the refute stage found a real wait that
/// evaded it by import shape alone (`f11_...` below), which is the reason
/// this now checks several spellings instead of trusting one to stand for
/// the rest.
///
/// **Every spelling is assembled at runtime, deliberately**, so this doc
/// comment can describe it in prose without the scan tripping over its own
/// description the way `fair_use_contract_convention.rs`'s scan has to
/// special-case its one unavoidable self-match. There is no legitimate
/// reason for a real, awaited or busy-spun timer to appear anywhere in this
/// file, this doc comment included.
///
/// Scoped to this file — the one the staleness tests live in since the
/// module was split (M14.2 review, F1) — rather than the whole crate:
/// `session.rs` waits on a real timer on purpose (a background poll loop and
/// its test), and a crate-wide ban would be a false positive on a use these
/// staleness tests have nothing to do with.
#[test]
fn this_files_staleness_tests_move_time_through_the_seam_not_a_real_wait() {
    let src = own_source();
    let hit = banned_wait_spellings()
        .into_iter()
        .find(|spelling| src.contains(spelling.as_str()));
    assert!(
        hit.is_none(),
        "F3: this module ages a binding out by moving `with_clock`'s \
         scripted clock forward, never by waiting out a real one — a \
         test that waited instead would still pass every assertion, \
         only slower, and nothing but the workspace's bounded-timeout \
         habit would notice, as a bare exit 124 that points at no timer \
         and no test. Found: {hit:?}"
    );
}

/// Every spelling of a real, awaited or busy-spun timer the guard above
/// refuses to see in this file (M14.2 review, F11). Broadened from the
/// original single contiguous three-segment Tokio wait path, which a split
/// `use` of its module plus a bare call to the wait function cleared
/// untouched — see `f11_...` below, which reproduces exactly that mutation
/// and checks it against this list rather than the old one-literal check.
///
/// Each entry is assembled from segments, not written contiguously, for
/// the same self-trip reason [`this_files_staleness_tests_move_time_through_the_seam_not_a_real_wait`]'s
/// doc comment gives: this function's own source must not itself contain
/// what it bans, or the guard it backs would flag its own definition.
fn banned_wait_spellings() -> [String; 4] {
    [
        format!("{}(", "sleep"),
        ["thread", "sleep"].join("::"),
        ["std", "time", "Instant"].join("::"),
        ["tokio", "time"].join("::"),
    ]
}

/// This file's own source — what the guard above scans, and what the
/// finding below mutates a copy of.
fn own_source() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = std::path::Path::new(manifest_dir).join("src/control/correlation/tests.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {path:?}: {error}"))
}

// -------------------------------------------------------------------
// M14.2 thermo-nuclear review, F11 — was: the guard above banned one exact
// spelling of a real wait, not the seam-vs-timer convention generally.
// Closed by broadening what it scans for; kept here as the regression
// guard against the one evasion the refute stage actually found.
// -------------------------------------------------------------------

/// **F11, closed.** The guard used to scan for one contiguous three-segment
/// path. A real, awaited timer reached the way most authors actually write
/// it — importing the wait function and its duration type together off the
/// module path, then calling the bare wait function — never spelled that
/// exact path contiguously anywhere in the file, so it cleared the old ban
/// untouched. This reproduces the review's `how_to_prove` (append the
/// split-import form the way a test author naturally would, rather than
/// leaving a permanent mutation in the tracked file) and checks the result
/// against [`banned_wait_spellings`] — the same broadened list the guard
/// above now runs. Like the guard's own doc comment, this one is worded to
/// avoid spelling any banned pattern out contiguously, so the guard does
/// not trip over its own description of the finding.
#[test]
fn f11_split_tokio_time_sleep_import_is_caught_by_the_broadened_scan() {
    let real_src = own_source();

    // The how_to_prove reproduction: a test author drops in a real,
    // awaited wait via the split-import spelling — `use
    // tokio::{time-module}::{wait-fn, duration-type};` followed by a
    // bare call to the wait function — instead of the one contiguous
    // path the old guard scanned for. Built from `segments` rather than
    // written out, for the same self-trip reason the guard's own doc
    // comment gives.
    let segments = ["tokio", "time", "sleep"];
    let split_import = format!(
        "use {}::{}::{{{}, Duration}};",
        segments[0], segments[1], segments[2]
    );
    let mutated_src = format!(
        "{real_src}\n// hypothetical addition a test author might make:\n\
         {split_import}\n\
         async fn hypothetical_staleness_test() {{ {}(Duration::from_millis(1)).await; }}\n",
        segments[2]
    );

    let hit = banned_wait_spellings()
        .into_iter()
        .find(|spelling| mutated_src.contains(spelling.as_str()));
    assert!(
        hit.is_some(),
        "F11: a real tokio wait introduced via a split import plus a bare \
         wait call should trip the broadened scan even though it never \
         spells the old single three-segment path contiguously"
    );
}
