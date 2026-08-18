// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The interjection seam: answering an admitted turn instead of running it.
//!
//! One question, asked once per admitted turn: is this turn dispatched as the
//! client asked, or completed here carrying something we produced instead? The
//! second answer is the steered turn — the client gets a synthetic tool call
//! back, runs it, and returns its output as ordinary conversation on the next
//! turn.
//!
//! **The contract with the engine**, which is what makes the answers to that
//! question comparable across milestones:
//!
//! - Consulted *after* the dedup short-circuit, so an identical retry of a
//!   steered turn replays the log and never reaches here. Whatever the
//!   occupant costs — in M6, a paid call to a judge — it is spent once per
//!   turn id, not once per attempt.
//! - Consulted *before* the turn is planned. A held turn should not spend a
//!   fleet round trip to price options it will not use, no `Routed` is
//!   recorded for it, and no grant is opened. Nothing about a steered turn
//!   reaches the cache ledger, because nothing was dispatched.
//! - Consulted with the session's projection, never with the candidate list.
//!   A decision that could see what the turn *would have cost* is a decision
//!   that can be argued into or out of by price.
//! - The interjector decides; the engine writes. The log has exactly one
//!   writer and it is the session that holds the lease, so an occupant that
//!   needs facts recorded — M6's side-call and verdict events — returns them
//!   rather than committing them.
//!
//! **A steered turn completes.** [`Interjection::Complete`] is not a refusal
//! and there is deliberately no incomplete arm: only a completion registers in
//! the session's completed turns, so an incomplete steered turn would re-enter
//! this seam on every retry and never settle, and `response.incomplete` reads
//! as an error in the client rather than as a call to run.
//!
//! **What M6 replaces:** [`production_default`] — and nothing else here. The
//! trigger, the arms, the judge side-call and the verdict-to-action map all
//! land as an occupant of this trait, whose `consider` runs the trigger
//! against the same [`InterjectionContext`] and returns [`Interjection`]. The
//! variants may grow (a side-call record for the engine to commit); the seam's
//! position and its two answers are what M6 inherits rather than re-decides.

use std::sync::Arc;

use async_trait::async_trait;

use crate::control::TurnPolicy;
use crate::event::Usage;
use crate::ids::ResponseId;
use crate::item::Item;
use crate::session::SessionState;

/// What the engine is to do with a turn it has already admitted.
#[derive(Debug, Clone, PartialEq)]
pub enum Interjection {
    /// Run the turn as the client asked. Nothing observable happened.
    Proceed,
    /// Complete the turn carrying `item`, without dispatching it.
    ///
    /// `item` is committed by
    /// [`Session::complete_with_item`](crate::session::Session::complete_with_item),
    /// which stamps this response's id onto it — so the item is built here
    /// without provenance and cannot claim any.
    ///
    /// `usage` is what the decision itself cost, and it is reported to the
    /// client as this turn's usage. Reporting an empty usage instead would
    /// make the deployment's own dashboard exceed what clients were told they
    /// spent, which is the one direction an accounting error must never run.
    Complete { item: Item, usage: Usage },
}

/// Everything a decision may see.
///
/// A named struct rather than a tuple of arguments because M6 adds to it —
/// and because each field here answers a different question, which a reader
/// should be able to see without opening the occupant.
pub struct InterjectionContext<'a> {
    /// The conversation and its projections, as the log has them: the items,
    /// the turn index, the frontier history, the steers still outstanding.
    /// Everything a trigger can compute without a model call is in here.
    pub state: &'a SessionState,
    /// What this principal is allowed to do with the turn. A decision that
    /// narrows routing may not widen past this, and a decision to steer is
    /// still subject to it.
    pub policy: &'a TurnPolicy,
    /// The response this turn has already opened.
    ///
    /// Load-bearing rather than incidental: the steer's call id is minted from
    /// it, which is what makes two concurrent steers unable to collide and a
    /// steer that no emitted call named impossible to fetch.
    pub response_id: &'a ResponseId,
}

/// Consulted once per admitted turn, under the contract in the module docs.
///
/// Asynchronous because the occupant this seam exists for makes a network call
/// — M6's judge — and a seam shaped for the no-op would have to change shape to
/// admit it, which is the one thing a seam is for. The return type has no error
/// arm on purpose: an occupant that cannot reach its judge releases the turn
/// (`Proceed`) rather than failing it, because the checker must never break the
/// checked, and a `Result` here would make failing the turn expressible.
#[async_trait]
pub trait Interjector: Send + Sync + 'static {
    async fn consider(&self, context: &InterjectionContext<'_>) -> Interjection;
}

/// The occupant that ships, and the reason this seam needs no `Option`.
///
/// An `Option<Arc<dyn Interjector>>` would put a `None` check on every turn's
/// path and leave "absent" and "present but silent" as two spellings of the
/// same behavior. A default occupant that decides `Proceed` collapses them:
/// the engine always consults, and until M6 the answer is always the same one.
struct NoInterjection;

#[async_trait]
impl Interjector for NoInterjection {
    async fn consider(&self, _context: &InterjectionContext<'_>) -> Interjection {
        Interjection::Proceed
    }
}

/// The interjector an engine runs with unless a caller replaces it.
///
/// The only constructor: the no-op type itself is private, so "what does an
/// unconfigured deployment interject?" has exactly one answer to read.
pub fn production_default() -> Arc<dyn Interjector> {
    Arc::new(NoInterjection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_noop_interjector_is_the_production_default_and_decides_nothing() {
        let interjector = production_default();
        let policy = TurnPolicy::unrestricted();
        let response_id = ResponseId::new("resp_1");

        let mut state = SessionState::default();
        assert_eq!(
            interjector
                .consider(&InterjectionContext {
                    state: &state,
                    policy: &policy,
                    response_id: &response_id,
                })
                .await,
            Interjection::Proceed,
            "the seam ships installed and empty: until M6 there is nothing \
             here to decide anything"
        );

        // The two states an occupant would most want to act on: a session with
        // history behind it, and one with a steer already outstanding. The
        // production default acts on neither.
        state.items.push(Item::user_text("still going in circles"));
        state.turn_index = 12;
        state
            .open_steers
            .insert("rhsteer_resp_0".into(), 1_700_000_000_000);
        assert_eq!(
            interjector
                .consider(&InterjectionContext {
                    state: &state,
                    policy: &policy,
                    response_id: &response_id,
                })
                .await,
            Interjection::Proceed,
            "no state makes the default interject, which is what makes it the \
             production occupant rather than a disabled feature"
        );
    }
}
