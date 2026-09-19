// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The report both backends fill in, and the driver that fills it.

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::{Accounting, SessionEventKind};
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::WireProtocol;
use roundhouse_server::{Admission, Engine, TurnInput};

/// How many items turn two appends before dispatching.
///
/// Twenty is the smallest append the provider's lookback window misses, so a
/// probe that appended nineteen would pass on a client that had never placed
/// the second marker at all.
const APPENDED_ITEMS: usize = 20;

/// One paragraph of the first prefix.
///
/// Repeated [`PREFIX_ITEMS`] times, to `MIN_CACHED_WORDS`. Providers write a
/// cache entry only above some minimum prefix length, so a probe whose first
/// turn fell below the one its model uses would report a zero read for a reason
/// that has nothing to do with the marker.
const PREFIX_PARAGRAPH: &str = "The cache affinity probe sends a long stable prefix and then appends to it, so the second request can be checked for a read against the entry the first one wrote. ";
/// How many times each item repeats it, and how many items there are.
///
/// Sized so the marked prefix clears `MIN_CACHED_WORDS` with room to spare —
/// `the_marked_prefix_is_long_enough_to_be_worth_caching` is what holds the two
/// in step. Repeating one paragraph rather than generating unique text keeps
/// the request large and boring: what this probe needs is length, and a long
/// prefix that also varies would be measuring two things.
const PARAGRAPH_REPEATS: usize = 14;
const PREFIX_ITEMS: usize = 24;

/// What one turn's `ResponseCompleted` said about the cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct TurnCache {
    /// Wire `cache_read_input_tokens`.
    pub(super) read: u64,
    /// Wire `cache_creation_input_tokens`.
    pub(super) written: u64,
    pub(super) input: u64,
    /// Whether the provider sent these counts, or roundhouse estimated them.
    ///
    /// An estimated record carries `cached_input_tokens: 0` because nothing
    /// observable bears on what a remote cache did — so treating it as an
    /// observed zero would manufacture the probe's own negative result.
    pub(super) reported: bool,
}

/// Both turns, kept apart so a zero cannot be read as the wrong zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProbeReport {
    pub(super) first: TurnCache,
    pub(super) second: TurnCache,
}

impl ProbeReport {
    /// The line the live run records in the ruling.
    ///
    /// States the verdict rather than leaving a reader to derive it: the
    /// interesting failure is a second-turn zero, and it means nothing until
    /// the first turn's write is known.
    pub(super) fn render(&self) -> String {
        // Cache activity of any kind on turn one, read or write. A turn that
        // read a warm prefix did something this run, so its absence of a write
        // is not an absence of evidence.
        let first_activity = self.first.read > 0 || self.first.written > 0;
        let unreported = match (self.first.reported, self.second.reported) {
            (false, false) => Some("turn 1 and turn 2 carry"),
            (false, true) => Some("turn 1 carries"),
            (true, false) => Some("turn 2 carries"),
            (true, true) => None,
        };
        let verdict = match (unreported, self.second.read, first_activity) {
            // Which turn is missing provenance, named — the other turn's
            // counters came from the provider and stay observations. The
            // `reported` flags below say which is which.
            (Some(which), _, _) => format!(
                "UNREPORTED: {which} no provider usage; read the rows below against their own reported flag"
            ),
            // A read happened. Where the entry came from is not in the usage
            // object, so the line says what was seen on each turn and infers
            // no origin.
            (_, read, activity) if read > 0 => format!(
                "READ: turn 2 read {read} cached tokens; {}",
                match (self.first.written, activity) {
                    (0, false) => "no write and no read was observed on turn 1",
                    (0, true) => "no write was observed on turn 1, which did read a cached prefix",
                    _ => "a write was observed on turn 1",
                }
            ),
            (_, _, true) => {
                "NO READ: turn 1 showed cache activity and turn 2 read nothing".to_string()
            }
            (_, _, false) => {
                "INCONCLUSIVE: no cache activity was observed on either turn".to_string()
            }
        };
        format!(
            "{verdict}\n  turn 1: written={} read={} input={} reported={}\n  turn 2: written={} read={} input={} reported={}",
            self.first.written,
            self.first.read,
            self.first.input,
            self.first.reported,
            self.second.written,
            self.second.read,
            self.second.input,
            self.second.reported,
        )
    }
}

/// One session, two turns, an append of [`APPENDED_ITEMS`] between them.
///
/// Hard-bounded at two dispatches: there is no loop and no retry here, because
/// the live form of this function spends real money and a bound that depends on
/// a condition is a bound that can fail to hold.
pub(super) async fn two_turn_probe(
    engine: &Engine<MemoryStore, ByteTokenizer>,
    store: &MemoryStore,
    admission: &Admission,
) -> Result<ProbeReport, roundhouse_server::EngineError> {
    let session_id = SessionId::generate();
    engine
        .create_session(&session_id)
        .await
        .expect("the session opens");

    // The nonce goes first, inside the first item, so two runs diverge inside
    // the opening block. A probe whose prefix matched yesterday's run would
    // read an entry it did not write and report somebody else's hit.
    let body = PREFIX_PARAGRAPH.repeat(PARAGRAPH_REPEATS);
    let prefix: Vec<Item> = (0..PREFIX_ITEMS)
        .map(|n| match n {
            0 => Item::user_text(format!("probe run {session_id} paragraph 0 {body}")),
            _ => Item::user_text(format!("paragraph {n} {body}")),
        })
        .collect();
    engine
        .run_turn(
            &session_id,
            TurnId::new("probe-1"),
            input(prefix),
            admission,
        )
        .await?;

    let appended: Vec<Item> = (0..APPENDED_ITEMS)
        .map(|n| Item::user_text(format!("appended item {n}")))
        .collect();
    engine
        .run_turn(
            &session_id,
            TurnId::new("probe-2"),
            input(appended),
            admission,
        )
        .await?;

    let turns = completed_usage(store, &session_id).await;
    assert_eq!(
        turns.len(),
        2,
        "the probe dispatches exactly twice; a third would be spend nobody bounded"
    );
    Ok(ProbeReport {
        first: turns[0],
        second: turns[1],
    })
}

pub(super) fn input(items: Vec<Item>) -> TurnInput {
    TurnInput {
        items,
        declared_baseline: None,
        // A small cap, because the answer is not what this probe measures and
        // an unbounded one would let a live run spend on output it discards.
        output_token_cap: Some(16),
        tools: None,
        tool_choice: None,
        tools_dialect: Some(WireProtocol::AnthropicMessages),
    }
}

/// Every `ResponseCompleted` usage in the log, in order.
async fn completed_usage(store: &MemoryStore, session_id: &SessionId) -> Vec<TurnCache> {
    store
        .read_events(session_id, 0, 10_000)
        .await
        .expect("an in-memory log reads")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ResponseCompleted { usage, .. } => Some(TurnCache {
                read: usage.cached_input_tokens,
                written: usage.cache_write_tokens,
                input: usage.input_tokens,
                reported: usage.accounting == Accounting::Reported,
            }),
            _ => None,
        })
        .collect()
}
