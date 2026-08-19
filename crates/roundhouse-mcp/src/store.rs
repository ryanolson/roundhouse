// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The node-local control store: overlays, intents, steer payloads, bindings.
//!
//! # In-process, deliberately, and on a stated precedent
//!
//! Every family here is a `HashMap` behind a `Mutex` in one process. That is
//! the same posture `responses_api.rs` takes for its generations map, which
//! documents itself as "process state standing in for a durable mapping the
//! Redis store will own", and it is taken here for the same reason: the durable
//! shape belongs to the admin plane (M8), which is where a key's records get a
//! lifecycle, a reconciliation view and a migration. Building the durable store
//! now would fix that shape before the plane that owns it exists.
//!
//! What the choice costs, said plainly rather than left for a reader to
//! discover: an overlay does not survive a process restart, and in a multi-node
//! deployment it applies only on the node that took the MCP call. Both are
//! acceptable *because of what an overlay is* — a narrowing, so losing one
//! widens back to the deployment's ceiling and never past it, and the audit
//! trail shows the change through `turn_policy_digest` either way. A steer
//! payload lost to a restart is the one real hole, and it is bounded the same
//! way: the log holds the emitted call, so `fetch_steer` refuses rather than
//! inventing, and the turn continues.
//!
//! # One store, four families
//!
//! Overlays, intents and steer payloads are the three the plan names; session
//! bindings are the fourth, and they are here rather than in a store of their
//! own for the reason the other three share one — they are all node-local
//! process state keyed by something a session owns, they all become rows in the
//! same durable store in M8, and three lookalike `Mutex<HashMap<…>>` types
//! would be three places to remember to lock, three to expire and three to
//! migrate.
//!
//! # The log is the truth; this is a projection
//!
//! A steer payload is deposited *after* the log commit that emitted its call —
//! see the engine's interjection seam. The ordering is what makes the failure
//! mode benign: a crash between the two leaves a call in the log with no
//! payload here, and `fetch_steer` refuses an id it cannot resolve. The
//! opposite ordering would leave a payload for a call that was never emitted,
//! which is a steer an agent can fetch and answer against a session that never
//! asked.

use std::collections::HashMap;
use std::sync::Mutex;

use roundhouse_core::control::Principal;
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::{Item, ItemContent};

use crate::overlay::SessionOverlay;
use crate::surface::{SteerOutcome, SurfaceError};

/// The prefix every minted session-binding id carries.
///
/// Public because it is half of a wire contract with a projection that runs in
/// another crate: [`binding_in_items`] scans a session's items for this token,
/// and the engine's own binding lookup will use the same scanner. A second
/// spelling of the prefix is a join that silently finds nothing.
pub const BINDING_PREFIX: &str = "rhb_";

/// An opaque handle correlating an MCP connection to a conversation.
///
/// Opaque on purpose: it is minted by us, echoed by the client into its own
/// history, and read back out of a log. Anything derivable from it — a session
/// id, a principal — would be a tenancy fact travelling through a model's
/// context, where it can be summarized, quoted, or handed to another tool.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BindingId(String);

impl BindingId {
    /// Mint a fresh one.
    pub fn generate() -> Self {
        Self(format!("{BINDING_PREFIX}{}", uuid::Uuid::new_v4().simple()))
    }

    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BindingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a minted binding id stands for.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionBinding {
    pub principal: Principal,
    pub session: SessionId,
    pub minted_at_ms: u64,
}

/// What the agent said it is trying to do.
///
/// Stored and — in M5 — read by nothing but this crate's own tests. Its
/// consumer is M6's validator brief, where a stated goal turns the judge's
/// question from "infer the goal, then judge drift" into "here is the goal,
/// name the divergence". The write half ships now because the tool that
/// produces it is part of this surface, and a brief assembled in M6 against
/// sessions that never had the tool is a brief with nothing to read.
#[derive(Debug, Clone, PartialEq)]
pub struct IntentRecord {
    pub goal: String,
    pub plan_steps: Vec<String>,
    pub done_when: String,
    pub declared_at_ms: u64,
}

/// A corrective payload, committed when its synthetic call was emitted.
///
/// `steer_id` is the `call_id` the engine put in the log — one id, not two, so
/// the tool an agent calls and the item its client resends name the same thing
/// and a projection can join them without a mapping table.
#[derive(Debug, Clone, PartialEq)]
pub struct SteerRecord {
    pub steer_id: String,
    pub session: SessionId,
    /// Who the steer belongs to. Compared against the caller on every fetch:
    /// the id travels through a model's context, and a context is a place ids
    /// get copied between conversations.
    pub principal: Principal,
    pub guidance: String,
    pub emitted_at_ms: u64,
    /// What the agent said it did about the steer, if it said anything.
    ///
    /// Advisory in the strongest sense: nothing reads it to decide anything,
    /// its absence is never an error, and M6's arm evaluation is the first
    /// consumer that will.
    pub outcome: Option<SteerOutcome>,
    pub outcome_note: Option<String>,
}

/// Node-local control-plane state, keyed by the session it belongs to.
#[derive(Debug, Default)]
pub struct ControlStore {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    overlays: HashMap<SessionId, SessionOverlay>,
    intents: HashMap<SessionId, IntentRecord>,
    steers: HashMap<String, SteerRecord>,
    bindings: HashMap<BindingId, SessionBinding>,
}

impl ControlStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The overlay in force, without spending any of it.
    ///
    /// What `status` and the two overlay writers render. Distinct from
    /// [`Self::consume_overlay`] because reading an overlay must not be able to
    /// expire it — a chatty agent calling `status` three times a turn would
    /// otherwise burn a three-turn preference before its first turn ran.
    pub fn overlay(&self, session: &SessionId) -> Option<SessionOverlay> {
        self.lock().overlays.get(session).cloned()
    }

    /// Install the overlay `session` will be routed under.
    ///
    /// An empty overlay is stored as an absence rather than as a record with
    /// two `None`s, so "this session has an overlay" is one question with one
    /// answer.
    pub fn set_overlay(&self, session: &SessionId, overlay: SessionOverlay) {
        let mut inner = self.lock();
        if overlay.is_empty() {
            inner.overlays.remove(session);
        } else {
            inner.overlays.insert(session.clone(), overlay);
        }
    }

    /// The narrowing this turn is routed under, spending one turn of it.
    ///
    /// Called once per turn by the engine, at the seam where the admission
    /// policy is resolved. Returns the overrides rather than the record because
    /// that is all the engine needs — `ceiling.narrow(&overrides)` — and
    /// handing it the record would invite a second place that decides what an
    /// overlay means.
    pub fn consume_overlay(
        &self,
        session: &SessionId,
    ) -> Option<roundhouse_core::control::PolicyOverrides> {
        let mut inner = self.lock();
        let overlay = inner.overlays.get_mut(session)?;
        // Read before spending: the turn about to run is routed under the
        // ration it is spending, not under what is left after it.
        let overrides = overlay.overrides();
        overlay.consume();
        if overlay.is_empty() {
            inner.overlays.remove(session);
        }
        Some(overrides)
    }

    /// Record what the agent says it is doing. Replaces any earlier statement.
    pub fn set_intent(&self, session: &SessionId, intent: IntentRecord) {
        self.lock().intents.insert(session.clone(), intent);
    }

    /// The standing intent for a session.
    ///
    /// `pub(crate)` because M6's brief is its only real consumer and it lives
    /// in another crate: exporting the reader now would be a public API with
    /// nothing behind it, and the M6 change that needs it is the change that
    /// should widen it. This crate's own tests are what keep the write half
    /// honest in the meantime.
    pub(crate) fn intent(&self, session: &SessionId) -> Option<IntentRecord> {
        self.lock().intents.get(session).cloned()
    }

    /// Commit a steer payload, after the log commit that emitted its call.
    ///
    /// The engine's interjection seam is the only honest caller. See the module
    /// note on why the ordering is log-first.
    pub fn deposit_steer(&self, record: SteerRecord) {
        self.lock().steers.insert(record.steer_id.clone(), record);
    }

    /// Read a steer this principal owns.
    ///
    /// An id that does not exist and an id belonging to somebody else produce
    /// the identical error, naming neither the other principal nor the other
    /// session. See [`SurfaceError::UnknownSteer`] for why telling them apart
    /// would make the tool an enumeration oracle.
    pub fn steer_for(
        &self,
        principal: &Principal,
        steer_id: &str,
    ) -> Result<SteerRecord, SurfaceError> {
        self.lock()
            .steers
            .get(steer_id)
            .filter(|record| &record.principal == principal)
            .cloned()
            .ok_or_else(|| SurfaceError::UnknownSteer {
                steer_id: steer_id.to_string(),
            })
    }

    /// Attach an advisory outcome to a steer this principal owns.
    pub fn record_outcome(
        &self,
        principal: &Principal,
        steer_id: &str,
        outcome: SteerOutcome,
        note: Option<String>,
    ) -> Result<SteerRecord, SurfaceError> {
        let mut inner = self.lock();
        let record = inner
            .steers
            .get_mut(steer_id)
            .filter(|record| &record.principal == principal)
            .ok_or_else(|| SurfaceError::UnknownSteer {
                steer_id: steer_id.to_string(),
            })?;
        record.outcome = Some(outcome);
        record.outcome_note = note;
        Ok(record.clone())
    }

    /// Mint a binding id for `session` and record what it stands for.
    pub fn bind_session(
        &self,
        principal: &Principal,
        session: &SessionId,
        now_ms: u64,
    ) -> BindingId {
        let id = BindingId::generate();
        self.lock().bindings.insert(
            id.clone(),
            SessionBinding {
                principal: principal.clone(),
                session: session.clone(),
                minted_at_ms: now_ms,
            },
        );
        id
    }

    /// What a binding id stands for, if this node minted it.
    pub fn binding(&self, id: &BindingId) -> Option<SessionBinding> {
        self.lock().bindings.get(id).cloned()
    }

    /// The lock, in one place.
    ///
    /// A poisoned mutex here means a handler panicked mid-write. Recovering the
    /// guard rather than propagating the panic is deliberate: every family in
    /// this store is a projection whose loss degrades to the deployment's
    /// ceiling, and killing every later MCP call over one poisoned overlay is a
    /// worse outcome than serving the next one from possibly-stale state.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Find a session-binding id in a session's conversation items.
///
/// **The correlation trick's second half.** `init_session` mints an id and
/// returns it in its tool output; the client appends that output to its
/// conversation; the next turn resends the history, and the id lands in the
/// session log as an ordinary item. This is what reads it back out — so the
/// question "which wire session made that MCP call?" is answered from the log
/// and from nothing else.
///
/// Scans the rendered text of every item rather than only tool results, because
/// which item kind the output arrives as is the client's decision and not ours:
/// Codex appends it as a tool result, another client may fold it into a user
/// turn or a summary, and a scan that guessed wrong would find nothing while
/// looking like it worked.
///
/// Returns the **first** id in log order. A conversation holding two is a
/// client that called `init_session` twice, and the first is the one whose
/// binding the earlier turns were made under.
pub fn binding_in_items(items: &[Item]) -> Option<BindingId> {
    items.iter().find_map(|item| {
        let text = match &item.content {
            ItemContent::Text { text } => text.clone(),
            other => other.render(),
        };
        binding_in_text(&text)
    })
}

/// The first `rhb_…` token in `text`, if there is one.
///
/// A hand-rolled scan rather than a regex, for the reason the policy filter is
/// hand-rolled: the token shape is fixed by [`BindingId::generate`] — the
/// prefix followed by exactly 32 lowercase hex digits — and a scan that
/// accepted anything looser would match a truncated id a client summarized, and
/// resolve to a binding that is not the one minted.
fn binding_in_text(text: &str) -> Option<BindingId> {
    const HEX_LEN: usize = 32;
    let mut rest = text;
    while let Some(at) = rest.find(BINDING_PREFIX) {
        let after = &rest[at + BINDING_PREFIX.len()..];
        // Lowercase hex only, and an uppercase digit *ends* the token rather
        // than being skipped over. Skipping would let `rhb_ABC…` shift the
        // remaining characters left and resolve to an id nobody minted — the
        // exact failure a looser scan is supposed to prevent.
        let digits: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || matches!(c, 'a'..='f'))
            .collect();
        if digits.len() >= HEX_LEN {
            return Some(BindingId::new(format!(
                "{BINDING_PREFIX}{}",
                &digits[..HEX_LEN]
            )));
        }
        rest = &rest[at + BINDING_PREFIX.len()..];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::{ModeNarrowing, PreferMode, TimedOverlay};
    use roundhouse_core::ids::ResponseId;

    fn principal() -> Principal {
        Principal::new("acme", "ada")
    }

    fn session() -> SessionId {
        SessionId::new("acme/ada/sess_1")
    }

    fn steer(id: &str, owner: Principal) -> SteerRecord {
        SteerRecord {
            steer_id: id.into(),
            session: session(),
            principal: owner,
            guidance: "you are editing a file the task did not name".into(),
            emitted_at_ms: 1_700_000_000_000,
            outcome: None,
            outcome_note: None,
        }
    }

    #[test]
    fn a_steer_belonging_to_another_principal_reads_the_same_as_one_that_does_not_exist() {
        let store = ControlStore::new();
        store.deposit_steer(steer("fc_1", Principal::new("other", "bob")));

        let foreign = store
            .steer_for(&principal(), "fc_1")
            .expect_err("another key's steer is not readable");
        let missing = store
            .steer_for(&principal(), "fc_nope")
            .expect_err("an id nobody minted is not readable");
        assert_eq!(
            foreign.to_string().replace("fc_1", "ID"),
            missing.to_string().replace("fc_nope", "ID"),
            "telling the two apart would make the tool an enumeration oracle"
        );
        assert!(
            !foreign.to_string().contains("other") && !foreign.to_string().contains("bob"),
            "and the refusal must name nothing about the tenant that does own it"
        );

        // The control: the owner reads it.
        store.deposit_steer(steer("fc_2", principal()));
        assert_eq!(
            store.steer_for(&principal(), "fc_2").unwrap().steer_id,
            "fc_2"
        );
    }

    #[test]
    fn an_outcome_for_an_unknown_steer_is_refused_and_stores_nothing() {
        let store = ControlStore::new();
        assert!(
            store
                .record_outcome(&principal(), "fc_nope", SteerOutcome::Applied, None)
                .is_err()
        );
        assert!(store.steer_for(&principal(), "fc_nope").is_err());

        // The control: a real steer takes its outcome, and keeps it.
        store.deposit_steer(steer("fc_1", principal()));
        let updated = store
            .record_outcome(
                &principal(),
                "fc_1",
                SteerOutcome::Rejected,
                Some("already handled".into()),
            )
            .expect("the owner may report");
        assert_eq!(updated.outcome, Some(SteerOutcome::Rejected));
        assert_eq!(
            store.steer_for(&principal(), "fc_1").unwrap().outcome,
            Some(SteerOutcome::Rejected),
            "the write is durable within the process, not just in the answer"
        );
    }

    #[test]
    fn reading_an_overlay_does_not_spend_it_and_consuming_it_does() {
        let store = ControlStore::new();
        let overlay = SessionOverlay {
            mode: Some(TimedOverlay {
                ask: ModeNarrowing {
                    mode: PreferMode::Local,
                    allow: None,
                },
                remaining_turns: Some(1),
                reason: "cheap".into(),
            }),
            floor: None,
        };
        store.set_overlay(&session(), overlay);

        for _ in 0..3 {
            assert!(
                store.overlay(&session()).is_some(),
                "a chatty agent calling status must not burn its own preference"
            );
        }
        assert!(store.consume_overlay(&session()).is_some());
        assert!(
            store.overlay(&session()).is_none(),
            "one turn's ration, one turn"
        );
        assert!(
            store.consume_overlay(&session()).is_none(),
            "and a spent overlay is an absence, not a record of zeroes"
        );
    }

    #[test]
    fn an_intent_is_stored_and_replaced_rather_than_accumulated() {
        let store = ControlStore::new();
        assert!(store.intent(&session()).is_none());
        for goal in ["ship the parser", "ship the parser and its tests"] {
            store.set_intent(
                &session(),
                IntentRecord {
                    goal: goal.into(),
                    plan_steps: vec!["read the spec".into()],
                    done_when: "cargo test is green".into(),
                    declared_at_ms: 1,
                },
            );
        }
        assert_eq!(
            store.intent(&session()).unwrap().goal,
            "ship the parser and its tests",
            "the newer sentence is the one the agent meant"
        );
    }

    #[test]
    fn a_binding_id_is_found_in_whatever_item_kind_the_client_appended_it_as() {
        let store = ControlStore::new();
        let id = store.bind_session(&principal(), &session(), 7);
        assert_eq!(store.binding(&id).unwrap().session, session());

        // Codex appends a tool output as a `ToolResult`. Another client may
        // fold it into text. Both have to join.
        let as_result = vec![Item {
            role: roundhouse_core::item::Role::User,
            content: ItemContent::ToolResult {
                call_id: "call_9".into(),
                output: format!("{{\n  \"session_binding_id\": \"{id}\"\n}}"),
            },
            response_id: None,
        }];
        assert_eq!(binding_in_items(&as_result), Some(id.clone()));

        let as_text = vec![Item::user_text(format!("earlier I got {id} from the tool"))];
        assert_eq!(binding_in_items(&as_text), Some(id.clone()));

        // The controls: a conversation with no id, and a truncated one, both
        // resolve to nothing rather than to a near-miss binding.
        assert_eq!(binding_in_items(&[Item::user_text("no id here")]), None);
        let truncated = &id.as_str()[..BINDING_PREFIX.len() + 8];
        assert_eq!(binding_in_items(&[Item::user_text(truncated)]), None);
        // And a token whose hex run is interrupted rather than merely short.
        // An uppercase digit has to *end* the token: scanning past it would
        // shift the rest left and yield an id nobody ever minted.
        let interrupted = format!("{BINDING_PREFIX}A{}", &id.as_str()[BINDING_PREFIX.len()..]);
        assert_eq!(binding_in_items(&[Item::user_text(interrupted)]), None);

        // And the first id wins when a client called the tool twice.
        let second = store.bind_session(&principal(), &session(), 8);
        let both = vec![
            Item::assistant_text(id.to_string(), ResponseId::new("resp_1")),
            Item::assistant_text(second.to_string(), ResponseId::new("resp_2")),
        ];
        assert_eq!(binding_in_items(&both), Some(id));
    }
}
