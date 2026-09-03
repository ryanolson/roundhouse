// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which session a client's own name for a conversation resolves to.
//!
//! # Why this is a shared thing rather than a field of one surface
//!
//! It used to live inside [`responses_api`](crate::responses_api)'s handler
//! state, because that surface was the only thing that named a conversation.
//! The MCP control surface names one too — `prefer`, `status` and
//! `declare_intent` all take an optional `conversation`, spelled as the client's
//! own `prompt_cache_key` — and it has to arrive at the *same* session id, or an
//! agent narrows the routing of a session no turn will ever run in. Two maps
//! would agree only while nothing had forked; one map cannot disagree at all.
//!
//! # Three questions, one set of maps, and why they belong together
//!
//! A client names a conversation in one of three ways, and all three are
//! answered from the same state:
//!
//! - **By cache key.** `{project}/{user}/{key}` at generation zero, plus a
//!   `#g{n}` suffix once a client has edited its own history out from under a
//!   session. Only the generation map knows what `n` is.
//! - **By the tool-use id of the call it is answering** (M12, R-M2). Claude
//!   Code puts `_meta["claudecode/toolUseId"]` on every `tools/call` it makes,
//!   and that id is one *roundhouse emitted* — so the session that emitted it is
//!   knowable exactly, with no guess and no race, for as long as one session
//!   claims it. See [`Conversations::bind_call`].
//! - **By the thread the client says the call belongs to** (M12.1 review,
//!   R-M9). Codex stamps `_meta.threadId` on every `tools/call`, and the same
//!   id rides the turn that opened the thread in the `x-codex-turn-metadata`
//!   header — so the session a thread is in is knowable exactly, and knowable
//!   *per thread* where the cache key is shared by a whole agent family. See
//!   [`Conversations::bind_thread`].
//! - **Not at all.** The MCP surface's `conversation` argument is optional, and
//!   omitted it means the principal's most recent conversation — which is only
//!   knowable by having watched the turns go past, which is what
//!   [`Conversations::commit`] does on every request either wire surface
//!   serves.
//!
//! # Where the three maps live, since M14.1
//!
//! Not here. Generations, call bindings and thread bindings are
//! [`CorrelationMaps`] in `roundhouse-core`, with an in-process implementation
//! and a Redis one, and this type is the *conversation vocabulary* over them:
//! the `#g{n}` naming convention, the reader-versus-turn distinction, `latest`,
//! and the node-local memo below.
//!
//! What that buys is the thing M12.1's F9 could only half-promise. This type
//! held a `HashMap` behind a `Mutex` in one process, so a client that
//! reconnected to another node kept its cache key and lost its generation, and
//! an MCP call on a node that had served none of this principal's turns was
//! refused rather than answered. With a durable map behind it, "never bound on
//! *this node*" becomes "never bound *anywhere*" — the same refusal with the
//! scope it always should have had (R12, R-C2).
//!
//! # The memo is the turn path's, and what a reader may take from it
//!
//! R-C2 asks for a write-through cache so the common turn stays a local
//! lookup, and [`Conversations::generation`] is where that cache lives: a
//! node's first touch of a key reads the store, every later turn on the same
//! key reads the memo, and [`Conversations::commit`] writes both. That is legal
//! for exactly one reason and it is M14.0's: **a generation is where a search
//! starts, not the answer it returns.** A memo that has gone stale — another
//! node forked the key in between — costs the probe a walk of one or two extra
//! reads and lands the turn in the same place, because prefix admission checks
//! whatever it starts from against the log before committing to it.
//!
//! **No reader is cached, and that is the same rule and not an exception.**
//! [`Conversations::resolve`], [`Conversations::session_of_call`] and
//! [`Conversations::session_of_thread`] answer an MCP control call *directly*:
//! nothing downstream checks their answer against a log, so a stale one is
//! served to the agent with a 200 on it. That is precisely M12.1 F9's defect
//! — "a log another node has already forked away from, quietly" — and a
//! per-node memo would reintroduce it one fork later:
//!
//! - a memoised **generation** goes stale the moment another node's turn forks
//!   the key, and `resolve` would then narrow the routing of the pre-fork
//!   session;
//! - a memoised **call binding** cannot see the ambiguous marker another node's
//!   colliding claim wrote, so the node that bound the id first would keep
//!   answering confidently for it — M12's F14 with a network in the middle;
//! - a memoised **thread binding** goes stale on any fork served elsewhere, and
//!   a thread is *defined* as the session its own latest turn decided (M12.1
//!   review, F2), which is the turn this node did not serve.
//!
//! So the memo covers the one family whose consumer re-checks it, and the
//! readers pay a store round trip they were already paying a `last_seq` beside.
//! `the_node_memo_does_not_answer_a_reader` and
//! `a_binding_another_node_moved_is_read_from_the_store` below are what hold
//! that line.
//!
//! **The one exception is a memo entry whose write the store refused**, and it
//! is the rule rather than a hole in it (M14.1 review, F7). Every case above is
//! the memo being *behind* the store; a refused write is the store being behind
//! the memo, because the generation this node committed exists nowhere else.
//! Reading over it hands a control call the session the client was just moved
//! off — with a 200 on it, from the very node that moved them, while an
//! unnamed call on the same node answers the new one from `latest`. So
//! [`Conversations::resolve`] answers such an entry from the memo, the next
//! write on the key retries it, and the entry stops being the answer the
//! moment one lands.
//!
//! **`latest` stays node-local and stays a guess**, and that is a decision
//! rather than an omission (R12). It answers "which conversation is this agent
//! working in", which is knowable only by having watched turns arrive; two
//! nodes serving one agent would each write their own answer to a shared map,
//! and whichever wrote last would speak for both. A guess that is honest about
//! its scope beats a shared one that is not.
//!
//! **A key no node holds a binding for is still `None`, never generation zero**
//! (M12.1 review, F9). Zero is a real session id in the shared store the moment
//! any node mints it, so a reader that minted one would hand back a pre-fork
//! log with a 200 on it. What survives a restart is a *turn* re-binding its own
//! cache key — [`Conversations::generation`] still starts an unknown key's
//! search at generation zero, so the common never-forked conversation re-binds
//! to the same log — not a reader guessing at one.
//!
//! **What a re-derived generation costs is one read, not a fork.** A counter
//! that starts at zero says nothing about which generations the shared store
//! holds, so prefix admission searches the key's family for the one that
//! agrees with the claim and lands the turn there — see
//! [`prefix_admission`](crate::prefix_admission), which is the one home of
//! that rule and of what the alternative cost. An agreeing restart therefore
//! forks nothing and loses no warm prefix; it pays one extra read of the
//! generation it walked past.
//!
//! # What a correlation-store failure does, by which half is asking
//!
//! The turn path **degrades and says so**; the reader path **refuses**.
//!
//! A turn asks for a hint and records where it landed. A hint that could not be
//! loaded is still a hint — the probe checks it — and a commit that could not
//! be written costs *another* node's next probe a walk of the same bounded
//! size, this node's own readers nothing (F7, above), and the write itself
//! only until the next commit on that key.
//! Failing the turn instead would make the serving path depend on the
//! correlation store for something nothing trusts unchecked, and on a
//! deployment where both live in one Redis the *session log* fails loudly on
//! its own a few lines later, which is the failure an operator has to act on.
//! Every degradation is logged at warn, because "the maps are unreachable" is
//! not something to learn from a latency graph.
//!
//! A reader has no such check. Answering `None` for an unreachable store would
//! spell "no conversation of yours" — the caller then falls through to
//! `latest`, a plausible answer about the wrong conversation, which is the one
//! failure the whole correlation ruling exists to remove. So the three readers
//! return the error, and `mcp_api` renders it as an internal fault rather than
//! as a tenancy answer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use roundhouse_core::control::correlation::AgedTable;
use roundhouse_core::control::{
    CorrelationError, CorrelationMaps, MemoryCorrelationMaps, Principal,
};
use roundhouse_core::ids::SessionId;

/// How many keys this node's generation memo holds at once.
///
/// **A cache's cap, not the correlation store's staleness bound** (M14.2,
/// R-S2), and the distinction is the whole of the ruling: [`CorrelationMaps`]'
/// call and thread bindings age out because a *wrong* answer there is served
/// to an agent with a 200 on it, where a wrong entry here is never served —
/// [`Conversations::generation`] hands the memo out only as a hint a probe
/// starts from and corrects, never as an answer a caller trusts unchecked
/// (see the module doc's "no reader is cached" section). So an entry going
/// *stale* costs nothing this cap does not already cost by going *missing*:
/// either way the next touch pays one store read. Capacity is therefore the
/// only bound worth having, oldest-first — which is
/// [`AgedTable`](roundhouse_core::control::correlation::AgedTable) with no
/// staleness bound, the same type the two binding families one seam over are
/// instantiations of (M14.2 review, F2).
///
/// The M14.1 doc this replaces predicted the opposite — that this memo would
/// gain the two binding families' staleness bound — and that prediction was
/// wrong, not merely stale: aging out a *correct* hint early would trade a
/// free local lookup for a needless store read, which is a cost with nothing
/// bought for it.
const GENERATION_MEMO_CAP: usize = 4096;

/// The binding from a client's names to the sessions holding them.
pub struct Conversations {
    /// The three families, wherever this deployment keeps them.
    ///
    /// Behind the trait rather than as the concrete memory maps, because
    /// which implementation is here is the composition root's one decision
    /// (R-C4) and every method below reads identically either way.
    maps: Arc<dyn CorrelationMaps>,
    /// This node's memo of the generation map. See the module doc for why the
    /// turn path may hold one, and which one entry a reader must.
    ///
    /// Bounded by [`GENERATION_MEMO_CAP`], oldest-first — a capacity cap and
    /// not a staleness bound; see the constant's own doc for why those are
    /// different questions here.
    generations: Mutex<GenerationMemo>,
    /// Whether this node is currently carrying a commit the store refused —
    /// so [`Self::commit`] warns once per outage rather than once per turn,
    /// the shape the engine's fair-use seam already uses (M13.1 review, F4).
    /// A per-turn line about a condition that lasts as long as the outage
    /// trains an operator to filter exactly the line they need.
    generation_write_failing: AtomicBool,
    /// The session each principal most recently drove a turn on. Node-local by
    /// contract — see the module doc.
    latest: Mutex<HashMap<Principal, SessionId>>,
}

/// One key's entry in this node's memo of the generation map.
///
/// `Option<u32>` rather than `u32`, so the memo distinguishes three states
/// where the map itself has two: not read through yet (no entry at all), read
/// and absent, read and committed at *n*. Collapsing the middle one into zero
/// would be F9's defect stored locally — the node would stop asking the store
/// about a key it had only ever heard silence about.
#[derive(Clone, Copy, Debug)]
struct Memo {
    generation: Option<u32>,
    /// **This node committed this generation and the store did not take the
    /// write** (review M14.1, F7). A dirty entry is the one thing a store read
    /// cannot know: the value here is newer than anything the maps can answer,
    /// so [`Conversations::resolve`] serves it and
    /// [`Conversations::generation_refreshed`] retries the write rather than
    /// reading over it. Cleared by the first write that lands — every
    /// [`Conversations::commit`] is that retry, since a turn on this key
    /// writes the key again anyway.
    dirty: bool,
}

/// This node's whole memo of the generation map: one bounded table, holding
/// entries the cap may not take while they are dirty.
///
/// A named wrapper rather than the table bare, so the two things that are
/// *this* memo's — its cap and which of its entries are pinned — are named
/// once, here, and every call site below reads and writes without repeating
/// either.
#[derive(Debug)]
struct GenerationMemo {
    /// Bounded by count alone: no staleness bound, for
    /// [`GENERATION_MEMO_CAP`]'s reason.
    ///
    /// **Pinned while dirty** (M14.2 review, F9). A clean entry is a copy of
    /// what the store holds, so evicting it costs the next touch one read and
    /// nothing else — the whole of what R-S2 rests on. A *dirty* entry is not
    /// a copy of anything: it is this node's only record of a commit the
    /// store refused, and evicting it makes [`Conversations::resolve`] fall
    /// back to the store's older generation and hand a client the session
    /// this node moved it off — M14.1's F7, re-opened one eviction later. It
    /// stops being pinned the moment a write lands, which
    /// [`Conversations::write_generation`] retries on the next commit of the
    /// same key; so the pinned population is bounded by the keys whose
    /// commits one store outage refused, and it drains on that retry rather
    /// than accumulating.
    entries: AgedTable<Memo>,
}

impl Default for GenerationMemo {
    fn default() -> Self {
        Self {
            entries: AgedTable::new(GENERATION_MEMO_CAP, None).with_pinned(|memo| memo.dirty),
        }
    }
}

impl GenerationMemo {
    /// `&mut self` because the table's read path is also where an aged-out
    /// entry would be dropped — this table has no staleness bound, so nothing
    /// is dropped here, but the seam belongs to the shared type rather than
    /// to this wrapper.
    fn get(&mut self, key: &str) -> Option<Memo> {
        self.entries.get(key).copied()
    }

    /// Insert or overwrite `key`'s entry, evicting the oldest *evictable* key
    /// once this pushes the memo past [`GENERATION_MEMO_CAP`].
    ///
    /// Overwriting moves the entry to the queue's tail rather than leaving it
    /// where it was, so repeated touches of one hot key keep it resident
    /// without ever spending a second queue slot.
    fn set(&mut self, key: &str, memo: Memo) {
        self.entries.write(key, |_| memo);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

impl std::fmt::Debug for Conversations {
    /// Hand-written because [`CorrelationMaps`] is a trait object: what a
    /// reader of a debug line wants is the shape of this node's own state,
    /// which is the two maps below.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Conversations")
            .field("generations", &self.generations)
            .field("latest", &self.latest)
            .finish_non_exhaustive()
    }
}

impl Default for Conversations {
    fn default() -> Self {
        Self::new()
    }
}

impl Conversations {
    /// Over maps that live and die with this process.
    ///
    /// What a deployment that named no Redis gets, and what every test that is
    /// not about the durable seam gets.
    pub fn new() -> Self {
        Self::over(Arc::new(MemoryCorrelationMaps::new()))
    }

    /// Over whichever maps the composition root chose (R-C4).
    pub fn over(maps: Arc<dyn CorrelationMaps>) -> Self {
        Self {
            maps,
            generations: Mutex::new(GenerationMemo::default()),
            generation_write_failing: AtomicBool::new(false),
            latest: Mutex::new(HashMap::new()),
        }
    }

    /// Which generation of `key` a turn was last committed to, or zero for a
    /// key no node has ever committed.
    ///
    /// Where [`Self::resolve`] refuses an unbound key, this answers zero for
    /// one, and the difference is who is asking. `resolve` answers a *reader* —
    /// an MCP call naming a conversation — and a reader that guesses hands back
    /// a log another node has already forked away from (M12.1 review, F9). This
    /// answers prefix admission, which is not guessing but choosing where to
    /// start a search it will check against the store anyway: zero is the
    /// generation a never-bound key would be at if it exists, and a search that
    /// begins there confirms or abandons it in one probe.
    ///
    /// **The node's first touch of a key reads the store; every later turn on
    /// it reads the memo** (R-C2). What that memo can be wrong about, and why
    /// being wrong about it is bounded rather than dangerous, is the module
    /// doc's; what it buys is one saved round trip on every turn after the
    /// first of a conversation, which on the turn path is the number that
    /// matters.
    ///
    /// Deliberately a read: it records nothing in the *store* and moves
    /// nothing, so a request that goes on to be refused leaves the maps exactly
    /// as it found them. See [`Self::commit`].
    pub async fn generation(&self, key: &str) -> u32 {
        if let Some(memo) = self.memoised(key) {
            return memo.generation.unwrap_or(0);
        }
        match self.maps.generation(key).await {
            Ok(found) => {
                self.memoise(key, found, false);
                found.unwrap_or(0)
            }
            // Degraded, not refused — see the module doc. Deliberately *not*
            // memoised: an unreachable store is a transient this node should
            // ask about again on the next turn, where a remembered absence
            // would make one outage the answer for the process's life.
            Err(error) => {
                tracing::warn!(
                    %error,
                    key,
                    "the correlation maps could not be read for this cache key; the \
                     prefix search starts at generation zero and walks, which costs \
                     reads rather than correctness"
                );
                0
            }
        }
    }

    /// What the *store* holds for `key` now, re-priming the memo with it.
    ///
    /// **The refusal path's question, and only its** (review M14.1, F2;
    /// R-C2″). [`Self::generation`] hands out a hint and a hint is where a
    /// probe starts, so being wrong about it normally costs a walk of one or
    /// two extra reads. Past [`prefix_admission`](crate::prefix_admission)'s
    /// probe bound it stops costing reads and starts costing correctness: a
    /// memo at generation zero while another node forked the key nine times
    /// runs the walk off its bound, and a refusal commits nothing, so the memo
    /// stays where it was and every retry on this node is refused identically
    /// — while a node holding no memo at all serves the same claim in one
    /// read. So a search that ran off its bound asks this before refusing, and
    /// only a search that ran off its bound *from a fresh hint* refuses.
    ///
    /// Not the turn path's question: the common turn's whole benefit is that
    /// it costs no store read, and this read is paid where a request was about
    /// to be refused outright.
    ///
    /// **A dirty entry is retried rather than read over** (F7). Its value is
    /// this node's own commit that the store never took, which is newer than
    /// anything a read can return; reading over it would replace a known
    /// generation with the superseded one and hand the next probe a hint that
    /// walks backwards.
    pub async fn generation_refreshed(&self, key: &str) -> u32 {
        if let Some(Memo {
            generation: Some(generation),
            dirty: true,
        }) = self.memoised(key)
        {
            self.write_generation(key, generation).await;
            return generation;
        }
        match self.maps.generation(key).await {
            Ok(found) => {
                self.memoise(key, found, false);
                found.unwrap_or(0)
            }
            // Degraded for [`Self::generation`]'s reason and with its answer:
            // an unreachable store leaves the hint this node already had,
            // which is what the caller was going to use anyway.
            Err(error) => {
                tracing::warn!(
                    %error,
                    key,
                    "the correlation maps could not be re-read for this cache key; the \
                     prefix search is refused from the hint this node already held"
                );
                self.memoised(key)
                    .and_then(|memo| memo.generation)
                    .unwrap_or(0)
            }
        }
    }

    /// Record that `principal`'s turn on `key` is landing on `generation`.
    ///
    /// **The one write prefix admission makes, and it happens after the answer
    /// is known** (M14.0 review). Its predecessors — a `bind` on the way in and
    /// a `fork` per attempt, both since removed (M15, H1: neither has had a
    /// serving-path caller since M14.0) — wrote as they searched, so a request
    /// that ended in a refusal still left the counter advanced and `latest`
    /// naming a generation no turn had run on: the refusal stopped one request
    /// while the retry behind it resumed past the bound, and an unnamed MCP
    /// call in between was answered with a dead session. Committing once, at
    /// the end, is what makes a refusal cost nothing.
    ///
    /// The counter is *set* rather than incremented, because the search that
    /// chose `generation` may have walked backwards to an older generation the
    /// claim still continues — see [`prefix_admission`](crate::prefix_admission).
    ///
    /// Written through to the store and to this node's memo, in that order.
    /// **The memo is written even when the store write failed**, and that is
    /// deliberate: this node's turn did land on `generation`, so this node's
    /// next probe should start there. What the lost write costs another node
    /// is its next probe walking to find it, which is the bounded cost R-C2
    /// already accepts for two nodes committing different generations.
    ///
    /// What it must not cost is *this* node disagreeing with itself (review
    /// M14.1, F7): the entry is marked dirty, and a dirty entry is what
    /// [`Self::resolve`] answers a named conversation from, so the control
    /// surface cannot hand back the generation this very node just moved the
    /// client off. The next commit is the retry — a turn on this key writes
    /// the key again anyway — and the warn is once per outage, not per turn.
    ///
    /// `latest` moves on every call, whatever generation it lands on: it
    /// answers "which conversation is this agent working in", and an agent
    /// whose turn is about to open is working in it whether or not the turn
    /// goes on to succeed.
    ///
    /// # What moving off a generation costs the control plane, stated rather than hidden
    ///
    /// `ControlStore`'s four families — overlay, intent, steer payload, session
    /// binding — are keyed by the `SessionId` this commits *away from*, and
    /// nothing migrates them. The visible consequence is one: an agent that
    /// asked for `scope=session` narrowing, on a client that then edited its own
    /// history mid-session, silently stops being narrowed — the engine asks the
    /// post-fork id for an overlay and finds none. The trigger is specific and
    /// worth naming, because it is not "a fork happens": a fork happens when a
    /// *client rewrites history it already sent*, which is a compaction or a
    /// user editing a message, not an ordinary turn.
    ///
    /// Not migrated on purpose. Carrying an overlay across a fork means a
    /// narrowing surviving a history rewrite that the agent which asked for it
    /// may no longer be running, and deciding that is a decision about what an
    /// overlay's identity *is* — which belongs to the milestone that gives it a
    /// durable one (M8), not to a rebind hook reaching across two crates. What
    /// holds in the meantime is a bound rather than a fix: the orphaned records
    /// are collected by `ControlStore`'s retention sweep like any other aged
    /// state, so a forking client leaks for a day and not for the process's
    /// life.
    pub async fn commit(&self, principal: &Principal, key: &str, generation: u32) -> SessionId {
        self.write_generation(key, generation).await;
        self.mark_latest(principal, bound_session(key, generation))
    }

    /// Write `generation` through to the store, and memoise it either way —
    /// dirty when the store refused.
    ///
    /// One helper because [`Self::commit`] and
    /// [`Self::generation_refreshed`]'s retry are the same write with the same
    /// bookkeeping, and a second copy is how one of them would forget to clear
    /// the flag and leave this node answering from a memo the store had long
    /// since caught up with.
    async fn write_generation(&self, key: &str, generation: u32) {
        let lost = self.maps.set_generation(key, generation).await;
        if let Err(error) = &lost {
            // Once per outage rather than once per turn, the shape the engine's
            // fair-use seam already uses: the condition lasts as long as the
            // store is unreachable, and a line per request is a line an
            // operator learns to filter.
            if !self.generation_write_failing.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    %error,
                    key,
                    generation,
                    "this turn's generation could not be recorded in the correlation maps; \
                     this node serves it from its own memo and retries the write on this \
                     key's next commit, and another node's next probe walks to find it"
                );
            }
        } else {
            self.generation_write_failing
                .store(false, Ordering::Relaxed);
        }
        self.memoise(key, Some(generation), lost.is_err());
    }

    /// The session `key` names now, without claiming to be using it, or `None`
    /// for a key no node holds a binding for.
    ///
    /// What a *reader* asks — the MCP surface resolving an explicit
    /// `conversation` argument. Distinct from [`Self::commit`] because an agent
    /// asking `status` about a conversation must not thereby make that
    /// conversation its most recent one: the two tools that take the argument
    /// and the tool that omits it would then disagree about what "most recent"
    /// means, in an order the agent chose.
    ///
    /// **`None` and not generation zero for an unbound key** (M12.1 review,
    /// F9). Zero is what a turn's [`Self::commit`] would mint, but a reader
    /// minting it is not the same act as a turn minting it: the store is
    /// shared across
    /// nodes, so a generation-zero id exists there whenever *any* node ever
    /// created it, and answering with one hands the caller a superseded log
    /// that another node has already forked away from — quietly, with a 200 on
    /// it. A reader that does not know refuses; see the module doc.
    ///
    /// **Read from the store and never from this node's memo**, which is the
    /// same rule seen from the reader's side: the memo is a probe's starting
    /// point and a probe corrects it, where this answer is served to an agent
    /// unchecked. A node that had committed generation 2 and then watched
    /// another node fork to 3 would otherwise narrow the routing of a session
    /// its client has left.
    ///
    /// **With one exception, and it is the same rule and not a hole in it**
    /// (review M14.1, F7): a memo entry this node could not write through. The
    /// store cannot be more current than that entry — it is a commit the store
    /// refused — so reading it there hands back the generation this very node
    /// moved the client *off*, with a 200 on it, while an unnamed call on the
    /// same node answers the new session from `latest`. Staleness is what the
    /// rule above is about, and a dirty entry is the one state where the memo
    /// is the fresher of the two. It stops being the answer the moment a write
    /// lands; see [`Self::write_generation`].
    pub async fn resolve(&self, key: &str) -> Result<Option<SessionId>, CorrelationError> {
        if let Some(Memo {
            generation: Some(generation),
            dirty: true,
        }) = self.memoised(key)
        {
            return Ok(Some(bound_session(key, generation)));
        }
        Ok(self
            .maps
            .generation(key)
            .await?
            .map(|generation| bound_session(key, generation)))
    }

    /// The last session this principal drove a turn on, on this node.
    pub fn latest(&self, principal: &Principal) -> Option<SessionId> {
        self.lock_latest().get(principal).cloned()
    }

    /// Remember that `session` emitted the tool call `call_id`, for `principal`.
    ///
    /// **Recorded at the moment the call is streamed to the client**, because
    /// that is the only moment both halves are in one place: the surface knows
    /// which session it is following, and the item it is about to project
    /// carries the id the client will quote back. A later reconstruction —
    /// scanning logs for a `call_id` — would need to know which log to scan,
    /// which is the question this answers.
    ///
    /// Deliberately *not* a claim that the principal is now working in this
    /// conversation: [`Self::latest`] is unmoved. An agent whose subagent runs a
    /// tool must not thereby redirect its parent's next unnamed MCP call, which
    /// is the very race R-M2 exists to remove.
    ///
    /// An id two of this principal's sessions have bound is remembered as
    /// *ambiguous* rather than resolved in favour of the later writer — see
    /// [`CorrelationMaps::bind_call`] for why an id is not a name on every
    /// backend.
    ///
    /// A store that refused the write degrades to the pre-R-M2 answer for that
    /// one id — the MCP call quoting it falls back to `latest` — and says so.
    pub async fn bind_call(&self, principal: &Principal, call_id: &str, session: SessionId) {
        if let Err(error) = self.maps.bind_call(principal, call_id, &session).await {
            tracing::warn!(
                %error,
                call_id,
                "a tool call could not be bound to its session; a control call \
                 answering it falls back to this principal's most recent conversation"
            );
        }
    }

    /// The session that emitted `call_id`, if it was emitted *for this
    /// principal*.
    ///
    /// **A foreign id, an unknown one and an ambiguous one all answer the same
    /// way, and that is the decision.** Returning something distinguishable for
    /// "this id exists but is somebody else's" would make the argument an
    /// enumeration oracle — the reasoning `mcp_api::resolve_session` already
    /// collapses a foreign cache key under — and it would buy nothing: the
    /// caller has no use for another tenant's session, so both answers lead to
    /// the same next step, which is to fall back to the caller's own most
    /// recent conversation. An ambiguous id joins them for the same reason
    /// turned around: nothing distinguishes *which* of the two claiming
    /// sessions the caller meant, so there is no answer to give that is better
    /// than the fallback.
    ///
    /// A store that could not be *reached* is none of those three and is
    /// returned as itself — see the module doc's last section.
    pub async fn session_of_call(
        &self,
        principal: &Principal,
        call_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        self.maps.session_of_call(principal, call_id).await
    }

    /// Remember that `principal`'s thread `thread_id` is in `session`.
    ///
    /// **Written by the turn ingest, from the client's own header**, because
    /// that is the one moment both halves are in one place: the surface has
    /// just decided which session this turn's history belongs to, and the
    /// request that carried the history also carried the thread it came from.
    /// Nothing later can reconstruct the pairing — the thread id is not in the
    /// log, and the cache key that is in it is the whole agent family's rather
    /// than this thread's.
    ///
    /// Rebinding is ordinary and the latest write wins: a fork moves a thread
    /// to a new session, and the thread is then in the new one.
    ///
    /// Deliberately *not* a claim about [`Self::latest`], for
    /// [`Self::bind_call`]'s reason turned around: the ingest that calls this
    /// has already moved `latest` for the turn it is serving, and moving it a
    /// second time from a subagent's header is how the parent's next unnamed
    /// call gets redirected.
    pub async fn bind_thread(&self, principal: &Principal, thread_id: &str, session: SessionId) {
        if let Err(error) = self.maps.bind_thread(principal, thread_id, &session).await {
            tracing::warn!(
                %error,
                thread_id,
                "a thread could not be bound to the session its turn landed in; a \
                 control call from that thread falls back to the named path and then \
                 to this principal's most recent conversation"
            );
        }
    }

    /// The session `principal`'s thread `thread_id` is in, if any node served a
    /// turn of it.
    ///
    /// A foreign thread id and an unknown one answer alike, for the reason
    /// [`Self::session_of_call`] spells out at length: the caller has no use
    /// for another tenant's session, so distinguishing the two would buy an
    /// enumeration oracle and nothing else.
    pub async fn session_of_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        self.maps.session_of_thread(principal, thread_id).await
    }

    /// What this node last read or wrote for `key`, if it has touched it *and*
    /// the cap has not since evicted it.
    ///
    /// An eviction is silent here on purpose: the caller's next touch of
    /// `key` reads through the store exactly as a never-touched key would,
    /// which is the whole of what [`GENERATION_MEMO_CAP`]'s doc promises —
    /// one extra store read, and nothing else.
    fn memoised(&self, key: &str) -> Option<Memo> {
        self.lock_generations().get(key)
    }

    /// Remember what the store said, or what this node just committed —
    /// `dirty` when the store refused that commit.
    fn memoise(&self, key: &str, generation: Option<u32>, dirty: bool) {
        self.lock_generations().set(key, Memo { generation, dirty });
    }

    /// Record that `principal` is working in `session`, and hand it back.
    ///
    /// [`Self::commit`]'s own helper, named separately so the two things one
    /// commit does — write the generation, then move `latest` — read as two
    /// steps rather than one write with a side effect buried inside it. `bind`
    /// and `fork` were the other two callers this once served; both were
    /// removed in M15 (H1) once M14.0 moved every serving-path write onto
    /// `commit` alone.
    fn mark_latest(&self, principal: &Principal, session: SessionId) -> SessionId {
        self.lock_latest()
            .insert(principal.clone(), session.clone());
        session
    }

    /// The locks, in one place each.
    ///
    /// Recovering a poisoned guard rather than propagating the panic: a lost
    /// `latest` is one MCP call that has to name its conversation and a lost
    /// memo is one extra store read, and failing every later request over one
    /// poisoned map is a worse outcome than serving the next one from
    /// possibly-stale state.
    fn lock_latest(&self) -> std::sync::MutexGuard<'_, HashMap<Principal, SessionId>> {
        self.latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_generations(&self) -> std::sync::MutexGuard<'_, GenerationMemo> {
        self.generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// This node's session id for a namespaced cache key at a given generation.
///
/// Generation zero is the key verbatim, so a session survives a process
/// restart that loses the generation map: the common case is a conversation
/// that never forked, and it re-binds to the same log.
///
/// **Public because the convention has to have exactly one home.** Prefix
/// admission searches a key's generations by name, and every test that seeds
/// or asserts on one names it the same way — a second spelling of `#g{n}`
/// anywhere is a second convention, and the two would agree only until one of
/// them moved.
pub fn bound_session(key: &str, generation: u32) -> SessionId {
    match generation {
        0 => SessionId::new(key),
        n => SessionId::new(format!("{key}#g{n}")),
    }
}

#[cfg(test)]
mod tests;
