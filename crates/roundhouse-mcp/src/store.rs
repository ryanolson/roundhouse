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
//! # Expiry is one sweep, and it is what bounds the whole store
//!
//! Every family is keyed by something a *session* owns, and a node serves
//! unboundedly many sessions over its life — so without expiry the four maps
//! only ever grow, and [`ControlSurface::init_session`](crate::ControlSurface::init_session)
//! in particular is a write a model can call in a loop. [`RETENTION_MS`] is
//! therefore enforced by a sweep that runs on the writes that carry a clock,
//! against the timestamp each record already stores. One retention for four
//! families rather than four is the other half of what "one store" bought: a
//! per-family lifecycle is M8's business, where a key's records get a
//! reconciliation view to be tuned against.
//!
//! The sweep is rate-limited by `SWEEP_INTERVAL_MS` rather than run on every
//! write. A sweep is `O(n)` over four maps and an insert is `O(1)`; running one
//! per insert would make the cost of holding state quadratic in the number of
//! writes, and what this needs is a *bound*, not a deadline.
//!
//! # A leak the sweep bounds rather than closes
//!
//! `Conversations::fork` rebinds a client's cache key to a fresh `SessionId`
//! when the client's resent history disagrees with the log — a client editing
//! its own history mid-session. Every family here is keyed by the *pre-fork*
//! id, so the agent's standing narrowing silently stops applying (the engine
//! asks for the new id and finds nothing) and the old records are orphaned.
//! Nothing migrates them: a cross-crate rebind hook would put this crate's four
//! maps into the server's conversation table, and the widening it would cause —
//! `scope=session` narrowing surviving a history rewrite — is a decision for
//! the milestone that gives overlays a durable identity of their own (M8), not
//! a hook bolted on here. What M5 guarantees instead is that the orphan is
//! bounded: the sweep collects it like any other aged record.
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

use crate::overlay::{ModeNarrowing, SessionOverlay, TimedOverlay};
use crate::surface::SteerOutcome;

/// How long a record outlives the write that made it.
///
/// One day, and the same day for all four families. The number is chosen from
/// the consequence of getting it wrong in each direction rather than from a
/// measurement: too short and a steer is swept while the turn that was told to
/// fetch it is still running, which is `fetch_steer` refusing a correction the
/// log says was emitted; too long and a node's state is bounded by how many
/// conversations it has *ever* served. A day sits far above any single agent
/// turn and far below a node's uptime. It is deliberately not tunable — a knob
/// here would be a per-family lifecycle in disguise, which is M8's.
pub const RETENTION_MS: u64 = 24 * 60 * 60 * 1_000;

/// How often the sweep is allowed to run.
///
/// See the module note: the sweep bounds the store, and running one per insert
/// would make holding state quadratic in the number of writes for no bound the
/// minute-granular version does not already give.
const SWEEP_INTERVAL_MS: u64 = 60_000;

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

/// What an agent said it did about the correction it was given.
///
/// **All that is left of `SteerRecord`, and the deletion is M10.0's.** The old
/// record carried the *guidance* as well — deposited here when the synthetic
/// call was emitted, because the call itself carried only an id. The guidance is
/// a conversation item now, folded by the session's own projection, so keeping a
/// copy here would be a second source of truth for one string, node-local and
/// lost on restart. What remains is the one fact the log genuinely does not
/// have: what the agent says it did next.
///
/// Keyed by session rather than by steer id for the same reason: there is no
/// steer id any more. One report per conversation, overwritten by a later one —
/// an agent that reports twice has changed its mind, and the newer answer is the
/// one M6's arm evaluation wants.
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeRecord {
    /// Which conversation the report is about.
    pub session: SessionId,
    /// Who reported. Kept even though the session is resolved per-principal at
    /// the seam, because a record that cannot say whose it is cannot be swept,
    /// joined, or audited on its own.
    pub principal: Principal,
    pub outcome: SteerOutcome,
    pub note: Option<String>,
    pub reported_at_ms: u64,
}

/// Node-local control-plane state, keyed by the session it belongs to.
#[derive(Debug, Default)]
pub struct ControlStore {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    overlays: HashMap<SessionId, OverlayEntry>,
    intents: HashMap<SessionId, IntentRecord>,
    outcomes: HashMap<SessionId, OutcomeRecord>,
    bindings: HashMap<BindingId, SessionBinding>,
    /// The wall clock the next sweep is due at. Zero, so the first write of a
    /// process's life sweeps and the interval is measured from there.
    next_sweep_at_ms: u64,
}

/// An overlay together with the clock reading that installed it.
///
/// The stamp is here rather than on [`SessionOverlay`] because it is a fact
/// about *this store's* copy and not about what the agent asked for: an overlay
/// crossing the seam into the engine is a narrowing, and a narrowing carrying a
/// node's local clock would be one more thing two nodes could disagree about.
#[derive(Debug, Clone)]
struct OverlayEntry {
    overlay: SessionOverlay,
    written_at_ms: u64,
}

impl Inner {
    /// Drop every record older than [`RETENTION_MS`], at most once per
    /// [`SWEEP_INTERVAL_MS`].
    ///
    /// Called with the clock of the write that is about to happen, so the
    /// record being written is never the one collected.
    fn sweep(&mut self, now_ms: u64) {
        if now_ms < self.next_sweep_at_ms {
            return;
        }
        self.next_sweep_at_ms = now_ms.saturating_add(SWEEP_INTERVAL_MS);
        let cutoff = now_ms.saturating_sub(RETENTION_MS);
        self.overlays
            .retain(|_, entry| entry.written_at_ms >= cutoff);
        self.intents
            .retain(|_, intent| intent.declared_at_ms >= cutoff);
        self.outcomes
            .retain(|_, report| report.reported_at_ms >= cutoff);
        self.bindings
            .retain(|_, binding| binding.minted_at_ms >= cutoff);
    }
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
        self.lock()
            .overlays
            .get(session)
            .map(|entry| entry.overlay.clone())
    }

    /// Install the mode axis, leaving every other axis exactly as it is.
    ///
    /// **Per axis, and inside one lock, because the alternative loses a turn's
    /// ration.** The surface has to decide whether an ask leaves anything
    /// routable, and that decision is an `await` — so a whole-snapshot write
    /// would read the overlay, go away, and write back a picture of a moment
    /// that has passed. The engine consumes the same entry at the start of
    /// every turn it goes on to route:
    /// an interleaving in which a turn spends this session's last ration inside
    /// that gap gets the spent axis handed back with its count intact, and two
    /// overlay tool calls in flight at once each drop the other's axis.
    /// Read-modify-write of one axis under one lock is what makes both
    /// unrepresentable rather than unlikely.
    ///
    /// Returns the overlay in force *after* the write, which is what the
    /// answering tool renders: what the agent is told it has must be what a
    /// later reader finds, and the two are the same thing only if one of them
    /// is read back.
    pub fn set_mode_axis(
        &self,
        session: &SessionId,
        mode: Option<TimedOverlay<ModeNarrowing>>,
        now_ms: u64,
    ) -> SessionOverlay {
        self.mutate_axis(session, now_ms, |overlay| overlay.mode = mode)
    }

    /// Install the quality-floor axis, leaving every other axis as it is.
    ///
    /// See [`Self::set_mode_axis`] for why an axis and not a snapshot.
    pub fn set_floor_axis(
        &self,
        session: &SessionId,
        floor: Option<TimedOverlay<f64>>,
        now_ms: u64,
    ) -> SessionOverlay {
        self.mutate_axis(session, now_ms, |overlay| overlay.floor = floor)
    }

    /// One axis moved under one lock, and the resulting overlay.
    fn mutate_axis(
        &self,
        session: &SessionId,
        now_ms: u64,
        apply: impl FnOnce(&mut SessionOverlay),
    ) -> SessionOverlay {
        let mut inner = self.lock();
        inner.sweep(now_ms);
        let entry = inner
            .overlays
            .entry(session.clone())
            .or_insert(OverlayEntry {
                overlay: SessionOverlay::default(),
                written_at_ms: now_ms,
            });
        apply(&mut entry.overlay);
        entry.written_at_ms = now_ms;
        let settled = entry.overlay.clone();
        if settled.is_empty() {
            // Stored as an absence rather than as a record with two `None`s, so
            // "this session has an overlay" is one question with one answer.
            inner.overlays.remove(session);
        }
        settled
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
        let entry = inner.overlays.get_mut(session)?;
        // Read before spending: the turn about to run is routed under the
        // ration it is spending, not under what is left after it.
        let overrides = entry.overlay.overrides();
        entry.overlay.consume();
        if entry.overlay.is_empty() {
            inner.overlays.remove(session);
        }
        Some(overrides)
    }

    /// Record what the agent says it is doing. Replaces any earlier statement.
    pub fn set_intent(&self, session: &SessionId, intent: IntentRecord) {
        let mut inner = self.lock();
        inner.sweep(intent.declared_at_ms);
        inner.intents.insert(session.clone(), intent);
    }

    /// The standing intent for a session.
    ///
    /// Public as of M6, which is the change its own doc said should widen it:
    /// the engine reads this at the interjection seam and hands it to the
    /// validator as [`Objective::Declared`], where a stated goal turns the
    /// judge's question from "infer the goal, then judge drift against your
    /// inference" into "here is the goal, name the divergence". Until there
    /// was a reader in another crate this was `pub(crate)`, because a public
    /// API with nothing behind it is a promise nothing keeps.
    ///
    /// [`Objective::Declared`]: roundhouse_core::validate::Objective::Declared
    pub fn intent(&self, session: &SessionId) -> Option<IntentRecord> {
        self.lock().intents.get(session).cloned()
    }

    /// Record what an agent said it did about a correction.
    ///
    /// **Never refuses, and that is the tool's own promise kept in the store.**
    /// `report_outcome` is advisory in the strongest sense — not reporting is
    /// never an error and never blocks a turn — so a report against a
    /// conversation roundhouse has not steered is filed rather than rejected.
    /// The alternative would make the tool's answer depend on whether a
    /// validation had happened, which is a fact the agent cannot see and would
    /// have to guess at.
    ///
    /// The caller has already resolved `session` through
    /// [`ControlReads::resolve_session`](crate::reads::ControlReads::resolve_session),
    /// which is where the tenancy boundary is; this stores what it is given.
    pub fn record_outcome(
        &self,
        principal: &Principal,
        session: &SessionId,
        outcome: SteerOutcome,
        note: Option<String>,
        now_ms: u64,
    ) -> OutcomeRecord {
        let record = OutcomeRecord {
            session: session.clone(),
            principal: principal.clone(),
            outcome,
            note,
            reported_at_ms: now_ms,
        };
        let mut inner = self.lock();
        inner.sweep(now_ms);
        inner.outcomes.insert(session.clone(), record.clone());
        record
    }

    /// What this conversation's agent last said it did, if it said anything.
    pub fn outcome_for(&self, session: &SessionId) -> Option<OutcomeRecord> {
        self.lock().outcomes.get(session).cloned()
    }

    /// The binding id for `(principal, session)`, minting one if it has none.
    ///
    /// **Idempotent, because the one write behind it is model-callable.**
    /// `init_session` is a tool, an agent can call a tool in a loop, and a
    /// version of this that minted unconditionally turned that loop into
    /// unbounded growth of a map with no key the loop could collide on. Its
    /// answer is also a better one: a conversation has *one* id, so a client
    /// that called the tool twice appends the same token twice rather than two
    /// tokens whose ordering a later reader has to adjudicate.
    ///
    /// The lookup is a scan rather than a second map keyed by owner. Two maps
    /// are two things to keep in step and two to sweep, and this scan runs once
    /// per `init_session` — once per conversation in the intended use — over a
    /// map the sweep keeps bounded.
    pub fn bind_session(
        &self,
        principal: &Principal,
        session: &SessionId,
        now_ms: u64,
    ) -> BindingId {
        let mut inner = self.lock();
        inner.sweep(now_ms);
        if let Some((id, _)) = inner
            .bindings
            .iter()
            .find(|(_, binding)| &binding.principal == principal && &binding.session == session)
        {
            return id.clone();
        }
        let id = BindingId::generate();
        inner.bindings.insert(
            id.clone(),
            SessionBinding {
                principal: principal.clone(),
                session: session.clone(),
                minted_at_ms: now_ms,
            },
        );
        id
    }

    /// What a binding id stands for, when it stands for *this* caller's
    /// conversation.
    ///
    /// **The tenancy check is the whole of the method.** A binding id is a
    /// token that travels through a model's context, and a context is where
    /// text of unknown authorship arrives: an issue body, a summarized web
    /// page, another agent's transcript. Resolving whatever id turned up and
    /// answering with its record would make a pasted token authoritative over
    /// the log it was pasted into. Matching on both halves of the record — the
    /// principal *and* the session the caller already holds — is what makes a
    /// foreign id inert rather than merely unlikely to be useful.
    pub fn binding(
        &self,
        principal: &Principal,
        session: &SessionId,
        id: &BindingId,
    ) -> Option<SessionBinding> {
        self.lock()
            .bindings
            .get(id)
            .filter(|binding| &binding.principal == principal && &binding.session == session)
            .cloned()
    }

    /// The binding this caller's own conversation proves, out of its items.
    ///
    /// The join [`binding_in_items`] only scans for: every `rhb_…` token in log
    /// order is tried, and the first that resolves *for this caller* wins.
    /// Trying them all rather than only the first is what stops a token pasted
    /// ahead of the agent's own from denying the join as well as failing it.
    ///
    /// **This is the read side of the correlation trick, and in M5 it has no
    /// production caller.** `mcp_api::resolve_session` answers "which
    /// conversation is this?" from the client's `prompt_cache_key` and from
    /// `Conversations::latest`, never from a binding — which is why the tool
    /// that mints an id is honest about recording it rather than about using
    /// it. M7 is where the read lands, per the plan's §3: it is the milestone
    /// that gives a request an identity resolved from the log rather than from
    /// a header the client cannot set.
    pub fn binding_in_log(
        &self,
        principal: &Principal,
        session: &SessionId,
        items: &[Item],
    ) -> Option<SessionBinding> {
        let inner = self.lock();
        binding_ids_in_items(items).into_iter().find_map(|id| {
            inner
                .bindings
                .get(&id)
                .filter(|binding| &binding.principal == principal && &binding.session == session)
                .cloned()
        })
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

/// Every session-binding id a conversation's items mention, in log order.
///
/// **A lexical scan, and nothing more.** `init_session` mints an id and returns
/// it in its tool output; the client appends that output to its conversation;
/// the next turn resends the history, and the id lands in the session log as an
/// ordinary item. This finds the tokens — it does not decide what they mean.
/// Every item in a log is text of *some* provenance, and the ones an agent
/// pastes in from an issue body or a summarized page are as scannable as the
/// ones this deployment wrote. Turning a token into a binding is
/// [`ControlStore::binding_in_log`]'s job, and the tenancy check it applies is
/// the reason this function may safely be as credulous as it is.
///
/// Scans the rendered text of every item rather than only tool results, because
/// which item kind the output arrives as is the client's decision and not ours:
/// Codex appends it as a tool result, another client may fold it into a user
/// turn or a summary, and a scan that guessed wrong would find nothing while
/// looking like it worked.
pub fn binding_ids_in_items(items: &[Item]) -> Vec<BindingId> {
    items
        .iter()
        .flat_map(|item| {
            let text = match &item.content {
                ItemContent::Text { text } => text.clone(),
                other => other.render(),
            };
            binding_ids_in_text(&text)
        })
        .collect()
}

/// The first id a conversation mentions, whoever it belongs to.
///
/// Kept as the cheap "did the id reach the log at all?" question — which is
/// what the end-to-end test of the correlation trick asks, and what an operator
/// debugging a client that summarizes too hard wants. It resolves nothing: see
/// [`ControlStore::binding_in_log`] for the join that applies the tenancy
/// check, and [`binding_ids_in_items`] for why the distinction matters.
pub fn binding_in_items(items: &[Item]) -> Option<BindingId> {
    binding_ids_in_items(items).into_iter().next()
}

/// Every `rhb_…` token in `text`, in order.
///
/// A hand-rolled scan rather than a regex, for the reason the policy filter is
/// hand-rolled: the token shape is fixed by [`BindingId::generate`] — the
/// prefix followed by exactly 32 lowercase hex digits — and a scan that
/// accepted anything looser would match a truncated id a client summarized, and
/// resolve to a binding that is not the one minted.
fn binding_ids_in_text(text: &str) -> Vec<BindingId> {
    const HEX_LEN: usize = 32;
    let mut found = Vec::new();
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
            found.push(BindingId::new(format!(
                "{BINDING_PREFIX}{}",
                &digits[..HEX_LEN]
            )));
        }
        // Past the prefix only: a hex run holds no `r`, so no id can hide
        // inside another's tail, and advancing past the whole token would skip
        // a second prefix that overlapped a rejected one.
        rest = &rest[at + BINDING_PREFIX.len()..];
    }
    found
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

    /// **T4's replacement for the two steer-store tests.**
    ///
    /// What is gone with them is named in the report: an unknown `steer_id` and
    /// another tenant's `steer_id` used to have to read identically, or the tool
    /// was an enumeration oracle. There is no id to enumerate any more — both
    /// steer tools name a conversation and resolve it through
    /// `ControlReads::resolve_session`, so the oracle question is answered by
    /// `ForeignConversation` at a door every other session tool already uses,
    /// and this store never sees a call it should have refused.
    ///
    /// What is left here is the one fact the log does not have, and the two
    /// properties the tool's own description promises about it: a report is
    /// never refused, and a second report replaces the first.
    #[test]
    fn a_report_is_filed_for_any_conversation_and_the_newer_one_wins() {
        let store = ControlStore::new();

        // A conversation roundhouse has not steered. Filed, not refused: the
        // descriptor promises reporting is never an error, and an agent cannot
        // see whether a validation happened, so a refusal here would make the
        // tool's answer depend on a fact it would have to guess at.
        let filed = store.record_outcome(
            &principal(),
            &session(),
            SteerOutcome::NotApplicable,
            None,
            1_700_000_000_000,
        );
        assert_eq!(filed.outcome, SteerOutcome::NotApplicable);
        assert_eq!(store.outcome_for(&session()).unwrap(), filed);

        // A second report is the agent changing its mind, and M6's arm
        // evaluation wants the newer answer -- so it replaces rather than
        // accumulating under a key that no longer exists to distinguish them.
        let revised = store.record_outcome(
            &principal(),
            &session(),
            SteerOutcome::Applied,
            Some("re-read the task and started over".into()),
            1_700_000_001_000,
        );
        assert_eq!(store.outcome_for(&session()).unwrap(), revised);
        assert_eq!(
            store.outcome_for(&session()).unwrap().note.as_deref(),
            Some("re-read the task and started over")
        );

        // The control: a conversation nobody reported on has no record, so the
        // assertions above are about the writes and not about a default.
        assert!(
            store
                .outcome_for(&SessionId::new("acme/ada/other"))
                .is_none()
        );
    }

    #[test]
    fn reading_an_overlay_does_not_spend_it_and_consuming_it_does() {
        let store = ControlStore::new();
        store.set_mode_axis(
            &session(),
            Some(TimedOverlay {
                ask: ModeNarrowing {
                    mode: PreferMode::Local,
                    allow: None,
                },
                remaining_turns: Some(1),
                reason: "cheap".into(),
            }),
            1,
        );

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
    fn writing_one_axis_leaves_the_other_exactly_as_it_was() {
        // The other half of why a write is per axis rather than per snapshot:
        // two overlay tool calls in flight at once each read the same overlay,
        // and a snapshot writer would have each of them publish a picture
        // missing the other's axis. Here the two writes are sequential, which
        // is the case a snapshot writer *also* gets wrong the moment anything
        // ran between them.
        let store = ControlStore::new();
        store.set_mode_axis(
            &session(),
            Some(TimedOverlay {
                ask: ModeNarrowing {
                    mode: PreferMode::Local,
                    allow: None,
                },
                remaining_turns: Some(3),
                reason: "bulk edits".into(),
            }),
            1,
        );
        let both = store.set_floor_axis(
            &session(),
            Some(TimedOverlay {
                ask: 0.9,
                remaining_turns: Some(2),
                reason: "this one is subtle".into(),
            }),
            2,
        );
        assert_eq!(both.mode.as_ref().unwrap().remaining_turns, Some(3));
        assert_eq!(both.floor.as_ref().unwrap().ask, 0.9);

        // Releasing an axis releases that axis. `prefer auto` must not take a
        // quality floor with it.
        let released = store.set_mode_axis(&session(), None, 3);
        assert!(released.mode.is_none());
        assert_eq!(released.floor.as_ref().unwrap().ask, 0.9);

        // And the last axis leaving is an absence, not a record of two `None`s.
        assert!(store.set_floor_axis(&session(), None, 4).is_empty());
        assert!(store.overlay(&session()).is_none());
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
    fn a_binding_id_pasted_into_another_tenants_log_resolves_to_nothing() {
        // The token travels through a model's context, and a context is where
        // text of unknown authorship arrives — an issue body, a summarized
        // page. Mallory's id, placed at the head of Ada's conversation, must
        // not answer the question "which conversation is this?" for Ada.
        let store = ControlStore::new();
        let mallory = Principal::new("evil", "mal");
        let mallorys_session = SessionId::new("evil/mal/main");
        let mallorys_id = store.bind_session(&mallory, &mallorys_session, 1);
        let adas_id = store.bind_session(&principal(), &session(), 2);

        let adas_log = vec![
            Item::user_text(format!("the issue body said: {mallorys_id}")),
            Item::user_text(format!("and my own id is {adas_id}")),
        ];
        let resolved = store
            .binding_in_log(&principal(), &session(), &adas_log)
            .expect("ada's own binding is still found behind the pasted one");
        assert_eq!(resolved.principal, principal());
        assert_eq!(resolved.session, session());
        assert!(
            store
                .binding(&principal(), &session(), &mallorys_id)
                .is_none(),
            "and the pasted id resolves to nothing when asked for directly"
        );

        // The controls: the scan itself still sees both tokens — the check is a
        // tenancy check and not a blinder — and Mallory reads her own binding.
        assert_eq!(
            binding_ids_in_items(&adas_log),
            vec![mallorys_id.clone(), adas_id],
        );
        assert!(
            store
                .binding(&mallory, &mallorys_session, &mallorys_id)
                .is_some()
        );
        // And a log holding *only* a foreign id proves nothing rather than
        // proving somebody else's conversation.
        assert!(
            store
                .binding_in_log(
                    &principal(),
                    &session(),
                    &[Item::user_text(mallorys_id.to_string())]
                )
                .is_none()
        );
    }

    #[test]
    fn an_aged_record_is_swept_by_a_later_write_and_a_fresh_one_is_not() {
        // The store's own module doc names expiry as the requirement one shared
        // store buys; without it every family grows for the process's life, and
        // `init_session` is a write a model can call in a loop.
        const DAY_MS: u64 = 86_400_000;
        let store = ControlStore::new();
        let old_id = store.bind_session(&principal(), &session(), 0);
        store.set_intent(
            &session(),
            IntentRecord {
                goal: "ship the parser".into(),
                plan_steps: Vec::new(),
                done_when: "cargo test is green".into(),
                declared_at_ms: 0,
            },
        );
        store.record_outcome(&principal(), &session(), SteerOutcome::Applied, None, 0);
        store.set_mode_axis(
            &session(),
            Some(TimedOverlay {
                ask: ModeNarrowing {
                    mode: PreferMode::Local,
                    allow: None,
                },
                remaining_turns: None,
                reason: "cheap".into(),
            }),
            0,
        );

        // A month later, one write of any family sweeps all four. Written
        // against a *different* conversation on purpose: outcomes are keyed by
        // session, so re-reporting on this one would overwrite the aged record
        // and prove nothing about the sweep.
        let other = SessionId::new("acme/ada/sess_2");
        store.record_outcome(
            &principal(),
            &other,
            SteerOutcome::Applied,
            None,
            30 * DAY_MS,
        );

        assert!(store.outcome_for(&session()).is_none());
        assert!(store.intent(&session()).is_none());
        assert!(store.overlay(&session()).is_none());
        assert!(store.binding(&principal(), &session(), &old_id).is_none());

        // The control: the record the sweeping write itself carried is not the
        // one collected, and neither is anything written after it.
        assert!(store.outcome_for(&other).is_some());
        let fresh_id = store.bind_session(&principal(), &session(), 30 * DAY_MS);
        assert!(store.binding(&principal(), &session(), &fresh_id).is_some());
    }

    #[test]
    fn init_session_called_twice_answers_with_one_binding() {
        // `init_session` is a tool, and a model can call a tool in a loop. A
        // mint per call is that loop turned into unbounded growth of a map with
        // no key the loop collides on.
        let store = ControlStore::new();
        let first = store.bind_session(&principal(), &session(), 1);
        let second = store.bind_session(&principal(), &session(), 2);
        assert_eq!(
            first, second,
            "one conversation has one id, however many times the tool is called"
        );
        assert_eq!(
            store.binding(&principal(), &session(), &first).unwrap(),
            SessionBinding {
                principal: principal(),
                session: session(),
                minted_at_ms: 1,
            },
            "and the record is the one the first call wrote, timestamp included"
        );

        // The controls: another conversation of the same key, and another key
        // on the same conversation id, each get their own.
        let other_session = store.bind_session(&principal(), &SessionId::new("acme/ada/sess_2"), 3);
        assert_ne!(other_session, first);
        let other_key = store.bind_session(&Principal::new("acme", "bob"), &session(), 4);
        assert_ne!(other_key, first);
    }

    #[test]
    fn a_binding_id_is_found_in_whatever_item_kind_the_client_appended_it_as() {
        let store = ControlStore::new();
        let id = store.bind_session(&principal(), &session(), 7);
        assert_eq!(
            store
                .binding(&principal(), &session(), &id)
                .unwrap()
                .session,
            session()
        );

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
        assert_eq!(
            store
                .binding_in_log(&principal(), &session(), &as_result)
                .map(|binding| binding.session),
            Some(session())
        );

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

        // Two ids in one conversation is no longer a client that called the
        // tool twice — that answers with one id — but a client that carried
        // somebody else's token in. The scan reports both, in log order.
        let elsewhere = store.bind_session(&Principal::new("evil", "mal"), &session(), 8);
        let both = vec![
            Item::assistant_text(elsewhere.to_string(), ResponseId::new("resp_1")),
            Item::assistant_text(id.to_string(), ResponseId::new("resp_2")),
        ];
        assert_eq!(binding_ids_in_items(&both), vec![elsewhere, id]);
    }
}
