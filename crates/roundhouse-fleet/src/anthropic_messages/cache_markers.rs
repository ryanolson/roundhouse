// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Where a request's `cache_control` breakpoints go — a pure function of three
//! inputs, extracted from [`super::AnthropicMessagesClient::body`] so the
//! policy has one owner and its tests do not have to build a whole quote and
//! scrape JSON back out of it to ask where a marker landed.
//!
//! **The breakpoint goes on the penultimate block, and nowhere else.**
//!
//! Anthropic caches nothing without an explicit `cache_control` marker —
//! unlike the Responses API, where `prompt_cache_key` steers a request to a
//! node that caches on its own. So a client that sent no breakpoint would get
//! a 0% hit rate on every turn, and the router would keep pricing the target
//! on a `CacheModel::Deterministic` prediction that nothing could fulfil.
//! Routing on a predicted cache hit and then prompting in a way that defeats
//! it is the failure `frontier.rs`'s module doc names first.
//!
//! Penultimate rather than last. A breakpoint caches everything *up to and
//! including* the block it sits on, and the penultimate block ends the prefix
//! the previous turn already sent, so its marker reads that turn's entry and
//! extends it. The final segment is this turn's new input; marking it too
//! would pay the write premium now for a read on the next turn, which resends
//! it byte for byte. Whether that trade pays is for live cache measurement to
//! decide, and until it does the final segment stays unmarked and the router
//! prices it as uncached on the next turn (see the last paragraph).
//!
//! Fewer than two segments means there is no stable prefix to name yet: one
//! block is the whole prompt, which is entirely this turn's input.
//!
//! And no breakpoint at all when the forwarded tools have already spent the
//! request's allowance. **Yielding is the right way round**: the tools' own
//! breakpoint caches the client's twenty-four-tool preamble, which is the
//! largest stable block in the request and is warm on the provider's side
//! either way, while ours caches a prefix we could re-mark on the next turn.
//! Dropping theirs to keep ours would trade a bigger discount for a smaller
//! one; sending both is a 400 that costs the turn.
//!
//! **And the same yield, one slot later, decides the second marker.** With two
//! free slots a request marks the penultimate block *and* the block the
//! previous request to this target marked; with one, the penultimate marker
//! takes it; with none, neither is sent.
//!
//! **A second marker, back where the previous request wrote its entry.**
//!
//! Anthropic's cache lookup examines at most [`CACHE_LOOKBACK_BLOCKS`] block
//! positions back from a marker, counting the marker itself. A session that
//! appends that many items between two turns therefore puts the penultimate
//! marker out of the previous write's reach — the prefix bytes are still
//! byte-identical and the turn still reads nothing, which is the one failure
//! mode a breakpoint strategy exists to avoid. Marking the earlier block as
//! well puts the old entry back inside a window, so the long tail is read
//! rather than re-prefilled.
//!
//! **The penultimate marker wins the last free slot**, which is why
//! [`plan`] derives `previous` from `penultimate` rather than beside it. A
//! lone marker at the earlier block reads this turn's cache and then moves
//! nothing forward, so every later turn pays plain input on an ever-longer
//! tail; a lone penultimate marker pays one write now and makes every later
//! turn a hit. One turn of saving against every turn after it is not a close
//! call.
//!
//! Dropped rather than sent when it is not strictly earlier than the
//! penultimate block — a previous marker that is not earlier means the
//! conversation stopped being append-only, and a marker placed there names a
//! block that is not the one that was written. Dropped too when the gap is
//! inside the window, because the penultimate marker already reaches the old
//! entry and a second one would only pay a second write.
//!
//! **Where the previous request marked is a recorded fact, not a guess.**
//! [`plan`] takes the previous dispatch's placement as the ledger recorded it
//! ([`PreviousMarker`]), and that placement came from this same function: the
//! engine asks [`super::marker_placement`] before it writes the `Routed`
//! decision, and the body sent is built from the same quote. A count alone
//! could not say whether that earlier request had a free slot for its marker —
//! a history that rode four tool markers renders the same count as one that
//! rode none — and reaching back for an entry never written was
//! fleet-redis-3. Only a ledger record from before placements were recorded
//! still carries just a count; for that one [`plan`] infers the block with
//! [`penultimate`], the rule it applies to the current request.
//!
//! **The last marker also bounds what the router predicts is warm.** The
//! provider caches through the last marker it is sent and no further, so the
//! final segment is billed as a write on the next turn. The ledger reads the
//! recorded placement for that too, rather than predicting the whole previous
//! request (P2).

use serde_json::Value;

use super::CacheLifetime;
use super::wire::CacheControl;
use crate::frontier::PreviousMarker;

/// **Anthropic's documented cap, and a hard 400 on the fifth.** It matters
/// here because roundhouse is not the only author of this request: the tool
/// definitions are forwarded from the client with their own breakpoints
/// intact — Claude Code marks its last tool, which is how it caches a
/// twenty-four-entry preamble — and the block breakpoint this client adds is
/// therefore never the request's only one. A constant rather than a literal
/// so the number and the reason it exists sit together; if Anthropic raises
/// it, this is the line that moves.
pub(super) const MAX_CACHE_BREAKPOINTS: usize = 4;

/// How many block positions back from a `cache_control` marker the provider
/// looks for a cache entry, counting the marker's own block.
///
/// **The number that makes one breakpoint per request insufficient.**
/// Anthropic documents this bound
/// (platform.claude.com/docs/en/build-with-claude/prompt-caching), and it is
/// what turns a long append into a total miss: a request whose only marker
/// sits this far past the previous request's marker reaches nothing, even
/// though the blocks in between are byte-identical to what was cached. A
/// constant rather than a literal so the number and the consequence sit
/// together; if Anthropic widens the window, this is the line that moves.
pub(super) const CACHE_LOOKBACK_BLOCKS: usize = 20;

/// The penultimate block of a prompt with `segment_count` segments, if one
/// exists.
///
/// The one formula both markers in [`plan`] are derived from — the current
/// request's own breakpoint, and (from a *different* segment count, when the
/// ledger recorded no placement) the block the previous dispatch would have
/// marked. One
/// function rather than the arithmetic written out twice is what keeps the
/// two placements from drifting apart -- a duplicated copy of this formula
/// once did exactly that, when a second copy lived in `roundhouse-server`'s
/// `engine.rs`.
pub(super) fn penultimate(segment_count: usize) -> Option<usize> {
    segment_count.checked_sub(2)
}

/// Where one request's `cache_control` markers go, resolved by [`plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct MarkerPlan {
    /// This request's own breakpoint — the penultimate block, when the
    /// forwarded tools left a slot for it.
    pub(super) penultimate: Option<usize>,
    /// The earlier block a long append reaches back for, when the ledger
    /// remembers one inside the lookback window and a slot remains for it
    /// too.
    pub(super) previous: Option<usize>,
}

impl MarkerPlan {
    /// The furthest block this plan marks — where the cached prefix ends.
    pub(super) fn last(&self) -> Option<usize> {
        self.penultimate.max(self.previous)
    }

    /// Whether block `index` carries a marker under this plan.
    pub(super) fn contains(&self, index: usize) -> bool {
        Some(index) == self.penultimate || Some(index) == self.previous
    }
}

/// Resolve where this request's `cache_control` markers go.
///
/// `segment_count` is this request's own item count. `riding` is how many
/// breakpoints the forwarded tools already carry — see [`breakpoints_in`].
/// `previous` is where the *previous* dispatch to this target put its own
/// marker, as the ledger recorded it; see this module's doc for the one kind
/// of record from which it is still inferred.
pub(super) fn plan(segment_count: usize, riding: usize, previous: PreviousMarker) -> MarkerPlan {
    let breakpoint = match riding < MAX_CACHE_BREAKPOINTS {
        true => penultimate(segment_count),
        false => None,
    };
    let previous = match breakpoint {
        Some(current) if riding + 2 <= MAX_CACHE_BREAKPOINTS => match previous {
            PreviousMarker::Unmarked => None,
            PreviousMarker::Block(index) => Some(index),
            PreviousMarker::Inferred { segment_count } => penultimate(segment_count),
        }
        .filter(|prev| *prev < current && current - *prev >= CACHE_LOOKBACK_BLOCKS),
        _ => None,
    };
    MarkerPlan {
        penultimate: breakpoint,
        previous,
    }
}

/// How many `cache_control` breakpoints the forwarded tools already carry.
///
/// **Counted structurally rather than by scanning the JSON text**, because a
/// tool whose *description* mentions `cache_control` is an ordinary tool and a
/// text scan would read it as a breakpoint and silently drop roundhouse's own —
/// costing the prefix discount on every turn for a substring in a doc string.
///
/// Only the array's top level is counted, which is where the wire puts them: a
/// tool definition's `input_schema` is the tool's own argument schema and
/// nothing inside it is a cache breakpoint, so descending would count a
/// property a client happened to name `cache_control`.
pub(super) fn breakpoints_in(tools: Option<&Value>) -> usize {
    tools
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| {
                    entry
                        .get("cache_control")
                        .is_some_and(|control| !control.is_null())
                })
                .count()
        })
        .unwrap_or(0)
}

/// Rewrite every forwarded tool marker to the target's own cache lifetime.
///
/// **The lifetime only.** The marker's `type`, any field this build has never
/// named, the definition around it and the marker's position all stay as the
/// client sent them — so [`breakpoints_in`] counts the same before and after
/// and the block allowance is unmoved.
///
/// Top level only, the bound [`breakpoints_in`] counts on: a property inside an
/// `input_schema` that a client happened to name `cache_control` is that tool's
/// own argument vocabulary, and rewriting it would edit the toolbox.
///
/// A marker this build cannot restate — a `null`, a string, a missing `type` —
/// is left exactly as sent: it is a 400 at the provider whatever lifetime is
/// written into it, and filling in the missing parts would be roundhouse
/// authoring a breakpoint the client did not.
pub(super) fn normalize_marker_lifetimes(tools: &mut Value, lifetime: CacheLifetime) {
    let Some(entries) = tools.as_array_mut() else {
        return;
    };
    for marker in entries
        .iter_mut()
        .filter_map(|entry| entry.get_mut("cache_control"))
    {
        let Ok(mut control) = serde_json::from_value::<CacheControl>(marker.clone()) else {
            continue;
        };
        control.ttl = lifetime.wire().map(str::to_string);
        *marker = serde_json::to_value(control).expect("a cache breakpoint serializes");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Table over `plan`.** Each row is `(segment_count, riding, previous)
    /// -> MarkerPlan`, and together they are the placement policy — the essay
    /// above justifies each column, this proves the arithmetic.
    ///
    /// `Inferred { segment_count: n }` rows are the ledger records that carry
    /// only a count, so they also pin the `n - 2` inference. Rows and what
    /// each pins:
    /// - `(31, 0, Inferred(6))`: both markers, the ordinary long-append case.
    /// - `(31, 3, Inferred(6))`: one free slot — the penultimate wins it.
    /// - `(31, 4, Inferred(6))` and `(31, 4, Unmarked)`: no free slot —
    ///   neither marker is placed, and the previous placement makes no
    ///   difference once the current request has no breakpoint of its own.
    /// - `(9, 0, Inferred(6))`: a three-item append (gap 3) is well inside the
    ///   lookback window — no second marker is worth the write.
    /// - `(6, 0, Inferred(6|7|42))`: a previous count that is not *strictly
    ///   before* this request's own penultimate block names no reachable
    ///   write — including the degenerate `42`, a conversation that stopped
    ///   being append-only.
    /// - `(26, 0, Inferred(6))` / `(25, 0, Inferred(6))`: the lookback
    ///   boundary itself. Gap 20 (`24 - 4`) is included, gap 19 (`23 - 4`) is
    ///   not — changing [`CACHE_LOOKBACK_BLOCKS`] to 19 or 21 flips one of
    ///   these rows, which is what makes this table a second C2 guard beside
    ///   `a_long_append_keeps_the_previous_cache_write_inside_a_lookback_window`.
    /// - `(6, 0, Inferred(0|1))`: a previous count too small to have had its
    ///   own penultimate block at all.
    /// - `(1, 0, Unmarked)` / `(0, 0, Unmarked)`: no stable prefix exists yet
    ///   to mark.
    /// - `(31, 0, Block(4))` / `(26, 0, Block(4))` / `(25, 0, Block(4))`: a
    ///   recorded placement is used as the block itself, not as a count — the
    ///   same boundary as the inferred rows, reached without the `n - 2`.
    /// - `(31, 0, Unmarked)`: a previous request recorded as marking nothing
    ///   gets no reach-back marker however long the append (fleet-redis-3).
    /// - `(31, 0, Block(29))`: a recorded block not strictly before this
    ///   request's own is dropped, as an inferred one is.
    #[test]
    fn plan_places_markers_by_riding_count_and_previous_placement() {
        use PreviousMarker::{Block, Inferred, Unmarked};
        let both = |penultimate, previous| MarkerPlan {
            penultimate: Some(penultimate),
            previous: Some(previous),
        };
        let only = |penultimate| MarkerPlan {
            penultimate: Some(penultimate),
            previous: None,
        };
        let inferred = |segment_count| Inferred { segment_count };
        for (segment_count, riding, previous, expected) in [
            (31, 0, inferred(6), both(29, 4)),
            (31, 3, inferred(6), only(29)),
            (31, 4, inferred(6), MarkerPlan::default()),
            (31, 4, Unmarked, MarkerPlan::default()),
            (9, 0, inferred(6), only(7)),
            (6, 0, inferred(6), only(4)),
            (6, 0, inferred(7), only(4)),
            (6, 0, inferred(42), only(4)),
            (26, 0, inferred(6), both(24, 4)),
            (25, 0, inferred(6), only(23)),
            (6, 0, inferred(0), only(4)),
            (6, 0, inferred(1), only(4)),
            (1, 0, Unmarked, MarkerPlan::default()),
            (0, 0, Unmarked, MarkerPlan::default()),
            (31, 0, Block(4), both(29, 4)),
            (26, 0, Block(4), both(24, 4)),
            (25, 0, Block(4), only(23)),
            (31, 0, Unmarked, only(29)),
            (31, 0, Block(29), only(29)),
        ] {
            assert_eq!(
                plan(segment_count, riding, previous),
                expected,
                "plan({segment_count}, {riding}, {previous:?})"
            );
        }
    }

    /// **CONTROL.** [`MarkerPlan::contains`] agrees with the two fields it
    /// reads, including the degenerate all-`None` plan a spent allowance
    /// produces.
    #[test]
    fn marker_plan_contains_answers_from_both_fields() {
        let plan = MarkerPlan {
            penultimate: Some(4),
            previous: Some(29),
        };
        assert!(plan.contains(4));
        assert!(plan.contains(29));
        assert!(!plan.contains(5));

        assert!(!MarkerPlan::default().contains(0));
    }

    /// [`MarkerPlan::last`] is the furthest marked block, whichever field
    /// holds it — the end of the prefix the provider caches.
    #[test]
    fn marker_plan_last_is_the_furthest_marked_block() {
        let plan = MarkerPlan {
            penultimate: Some(29),
            previous: Some(4),
        };
        assert_eq!(plan.last(), Some(29));
        assert_eq!(
            MarkerPlan {
                penultimate: Some(7),
                previous: None
            }
            .last(),
            Some(7)
        );
        assert_eq!(MarkerPlan::default().last(), None);
    }
}
