// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which session a client's resent history belongs to.
//!
//! # What this is, and why it is neither dialect's
//!
//! Both wire dialects this deployment serves re-send the *whole* conversation
//! on every turn, because neither has anywhere to keep a cursor. Against an
//! append-only log that resent history is not input: it is a claim about what
//! the session already contains. This module checks the claim and admits only
//! the part the session does not have — which is what keeps one client
//! conversation on one Roundhouse session, and therefore on one accumulated
//! warm prefix.
//!
//! The two dialects resolve a *different name* for the conversation —
//! [`responses_api`](crate::responses_api) reads `prompt_cache_key`,
//! [`messages_api`](crate::messages_api) a header or `metadata.user_id` — and
//! then ask exactly this question of it. A second copy would have been a
//! second answer to "does the client's history still agree with ours", and the
//! two would agree only until one of them learned something: the search rule,
//! the stamp-blind comparison in [`same_item`], and the retry-shaped empty
//! suffix are each a decision that has to hold for a conversation *whichever*
//! dialect it was opened on — a chained Relay serves one and dispatches the
//! other.
//!
//! # Probe, then commit — and what commit-as-you-go cost
//!
//! A cache key does not name one session but a family of them: generation
//! zero, plus a `#g{n}` for every time a client edited its own history out
//! from under the log ([`Conversations`] owns the naming). Admission is a
//! *search* over that family for the claim's home, and the search reads
//! before it writes anything down.
//!
//! The first draft of this rung did the opposite — it forked
//! [`Conversations`] first and asked questions afterwards, one generation per
//! attempt — and the M14.0 review found the same defect wearing six faces,
//! every one of them a consequence of mutating the table before anything was
//! known:
//!
//! - the request that ran out of attempts left the key's counter advanced, so
//!   a verbatim retry resumed past the bound and was admitted whole — the
//!   refusal stopped one request rather than the loop it named;
//! - it also left `latest` and the key's binding on the final *disagreeing*
//!   generation, one no turn ever ran on, which the MCP surface then answered
//!   unnamed calls with;
//! - the refusal's count was the constant rather than a tally, so it
//!   under-reported the generations that had actually disagreed;
//! - the search only ever went *up* from this node's counter, so a node that
//!   had served a divergent turn in between forked past an older, still
//!   agreeing generation and re-appended its whole history — one claim, two
//!   homes, differing by which node answered;
//! - an *empty* generation another node had just created and was mid-turn on
//!   read exactly like a fresh one, so the claim was admitted onto a session
//!   this node could not write to and the turn died in-stream on the lease;
//! - and the same admission step was spelled twice, once for generation zero
//!   and once inside the loop, with only one of the two consuming the store's
//!   already-existed answer.
//!
//! So: [`probe`] asks one generation one question and writes nothing;
//! [`bind_prefix`] searches with it and calls
//! [`Conversations::commit`](crate::conversations::Conversations::commit)
//! exactly once, after the home is known. A refusal commits nothing at all,
//! which is what makes a verbatim retry probe the same generations and be
//! refused identically.
//!
//! # The home of a claim is the longest generation that agrees with it
//!
//! Two generations can both agree — an older one the client is resuming and a
//! newer one that has since grown past it — and continuing the shorter would
//! append, a second time, history the longer already holds. So of the
//! generations the search probes, the claim lands on the one that agrees and
//! holds the most of it; only when none agrees is a fresh generation minted,
//! and a claim landing there is taken whole because there is nothing there to
//! disagree with.
//!
//! Which generations it probes is the other half, and it is decided by cost:
//!
//! 1. **This node's current generation, alone.** An unedited conversation is
//!    where this node last left it, and finding it there is one read. It wins
//!    outright rather than being weighed — the alternative is reading a whole
//!    family on every ordinary turn.
//! 2. **Upward, until the claim finds a generation it continues or the store
//!    has never held one.** This is the restart direction: a counter that
//!    re-derived at zero is behind a store that remembers more. The walk
//!    stops at its answer because generations are minted in order — nothing
//!    above one the store has never held can itself be held — so reading
//!    further would only ask the same question of slots that provably cannot
//!    answer it differently; and since probing no longer creates the slot it
//!    stops at (see "Probe, then commit" above), a walk that finds no
//!    agreement costs exactly the reads it made, not a session minted and
//!    then abandoned.
//! 3. **Downward, to zero.** The resume direction: a node that served one
//!    divergent turn in between judges a claim continuing an older generation
//!    against a newer one, and without this walk it would take the claim whole
//!    onto a generation of its own. These generations all exist, so probing
//!    them creates nothing, and the walk runs to the bottom rather than
//!    stopping at its first answer — this is where two agreeing generations
//!    genuinely compete.
//!
//! Each walk is bounded ([`MAX_PREFIX_PROBES`]); a claim that finds no home
//! anywhere both of them reach is refused, with the number of generations
//! actually read back on the wire — split into how many disagreed and how
//! many were another writer's (M15, H4), because folding the two into one
//! tally is how a refusal that probed nine busy generations once reported
//! zero of anything.
//!
//! Two consequences worth stating rather than discovering:
//!
//! - **A restart forks nothing.** A fresh process's counter re-derives at
//!   zero, so its first claim is judged against a generation some earlier
//!   process may already have forked away from; the search simply walks up to
//!   the generation that agrees and continues it with the delta. The cost is
//!   one extra read of the generation it walked past — not a fork, and not the
//!   warm prefix that treating a re-derived generation as empty would have
//!   thrown away.
//! - **An empty generation is only ours if nobody else is writing it.** A log
//!   with no items agrees with every claim trivially, which is right for a
//!   slot a previous request created and never used, and wrong for the slot
//!   another node created one instruction ago and holds the writer lease on.
//!   [`SessionStore::is_leased`] is the only thing that tells those apart, and
//!   it is asked exactly when the log is empty.

use std::cmp::Reverse;
use std::collections::HashSet;

use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::Principal;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::item::{Item, ItemContent};
use roundhouse_core::session::{ConfigurationCursor, turn_configuration_len};
use roundhouse_core::store::{SessionStore, StoreError};

use crate::control_config::ControlPlane;
use crate::conversations::{Conversations, bound_session};
use crate::engine::Engine;
use crate::http::{ApiError, READ_BATCH, store_error};

/// How far the search may probe in one direction before giving up on the key.
///
/// Chosen small and deliberately not zero: the ordinary restart case this
/// search exists to admit (R13) needs exactly one extra generation checked —
/// the one a prior process already forked to — so one is not enough to tell
/// "the store remembers a fork this process forgot" from "a client that will
/// never agree" apart. Eight is a client that rewrote its own history eight
/// times over the course of resending *one* turn's claim; nothing sane does
/// that, and a deployment whose clients legitimately diverge this often within
/// a single request has a different, larger problem this refusal is not trying
/// to solve.
///
/// It bounds *probes*, not writes, and that is the honest justification: since
/// the search commits nothing until it has an answer, an unbounded version
/// would not grow the store — it would spend unbounded reads walking a family
/// of generations one request has no business walking. The bound applies to
/// each direction separately rather than to the search as a whole, so a key
/// with a long history behind this node's counter cannot spend the allowance
/// the restart case ahead of it needs.
const MAX_PREFIX_PROBES: u32 = 8;

/// Resolve a cache key to the session holding its history, and to the part of
/// `claimed` that session does not have yet.
///
/// The read is unleased and therefore a snapshot: a second request on the same
/// cache key arriving before the first has appended would compute its delta
/// against a prefix that is about to grow. Serializing turns within a
/// conversation is the client's job — these APIs have no other way to order
/// them, since a turn's input is defined by the one before it — and the
/// engine's per-session gate keeps the log itself consistent regardless.
///
/// **A free function, `pub(crate)`, because prefix admission is what the two
/// dialects share and not what distinguishes them** — see the module doc for
/// what the shared answer is and why it is searched for rather than assumed.
///
/// The client's key is namespaced by [`ControlPlane::qualify`] rather than by a
/// convention spelled here, because the id this mints is the id the native
/// surface's namespace check will later be asked about: minting and checking
/// are one function pair, and two spellings of the convention is how a
/// namespace stops being one. The plane is the handler's snapshot rather than a
/// fresh read: a session id minted under one compiled plane and checked under
/// another is a session created and immediately unreachable.
pub(crate) async fn bind_prefix<S, T>(
    engine: &Engine<S, T>,
    store: &S,
    conversations: &Conversations,
    plane: &ControlPlane,
    principal: &Principal,
    cache_key: &str,
    claimed: Vec<Item>,
) -> Result<(SessionId, Vec<Item>), ApiError>
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    // Computed once and used for both the generation counter and the session
    // id, so the two cannot key on different strings. See [`Conversations`].
    let key = plane.qualify(principal, cache_key);
    let hint = conversations.generation(&key).await;

    let outcome = match search(store, &key, hint, &claimed).await? {
        // **A hint that ran the search off its bound is stale, and is
        // refreshed before anything is refused** (review M14.1, F2; R-C2″).
        // The memo is where a probe starts and never what it concludes, so a
        // node whose memo sat at generation zero while another node forked the
        // key nine times walked 1..=8 and refused a claim the store placed one
        // read away — and, since a refusal commits nothing, refused every
        // retry identically while a memo-less node served it at once. Asking
        // the store for a fresh hint and searching once more from it costs one
        // read on the refusal path alone; the ordinary turn is unchanged.
        Search::Exhausted { disagreed, busy } => {
            let refreshed = conversations.generation_refreshed(&key).await;
            if refreshed == hint {
                Search::Exhausted { disagreed, busy }
            } else {
                search(store, &key, refreshed, &claimed).await?
            }
        }
        decided => decided,
    };

    match outcome {
        Search::Lands { generation, delta } => Ok((
            conversations.commit(principal, &key, generation).await,
            delta,
        )),
        // The claim opens a generation of its own and is taken whole: there is
        // nothing recorded there to disagree with. The honest cost, paid once
        // per genuine divergence and not per restart, is that the new session
        // starts with no history — the routing ledger no longer knows any
        // provider is warm for it and the next turn is priced cold. That is
        // the conservative direction: a ledger claiming a warm prefix for a
        // conversation that just changed shape would be claiming a cache hit
        // nobody can serve.
        Search::Fresh { generation } => {
            let session_id = open_fresh(engine, conversations, principal, &key, generation).await?;
            Ok((session_id, claimed))
        }
        // Every generation a search from a fresh hint could reach disagreed
        // or was busy, and it never found a free slot. Refuse loudly, naming
        // the key and both tallies — and commit nothing, so a verbatim retry
        // probes the same generations and is refused in exactly the same way
        // rather than resuming past the bound.
        Search::Exhausted { disagreed, busy } => {
            Err(ApiError::prefix_admission_exhausted(&key, disagreed, busy))
        }
    }
}

/// What one pass over a key's family concluded, having written nothing down.
///
/// A value rather than three exits taken inside the walk itself, because a
/// pass is no longer necessarily the last word: one that ran off its bound
/// from a stale hint is run again from a fresh one (F2 above), and a walk that
/// committed or minted as it went could not be re-run at all — which is the
/// same reason [`probe`] writes nothing.
enum Search {
    /// `generation` agrees with the claim, and `delta` is the part of the
    /// claim it does not already hold.
    Lands { generation: u32, delta: Vec<Item> },
    /// Nothing agreed, and `generation` is the key's first free slot — not yet
    /// created, since a probe that asked about it left it as it found it.
    Fresh { generation: u32 },
    /// Every generation both walks reached was either disagreeing or busy,
    /// and neither walk reached a free slot. `disagreed` and `busy` are what
    /// the walks actually read rather than what they were allowed to read —
    /// kept apart (M15, H4) because a run of disagreements and a run of busy
    /// slots are different facts about the deployment, and folding them into
    /// one count is how a refusal that probed nine busy generations reported
    /// zero of anything.
    Exhausted { disagreed: u32, busy: u32 },
}

/// One pass of the search over `key`'s family, starting from `current`.
///
/// `current` is a *hint* and not a fact — this node's memo, or the store's own
/// answer when the memo ran the first pass off its bound — which is what makes
/// running this twice on one request meaningful rather than wasteful.
async fn search<S: SessionStore>(
    store: &S,
    key: &str,
    current: u32,
    claimed: &[Item],
) -> Result<Search, ApiError> {
    // Generations found agreeing, kept rather than counted from the bound:
    // the refusal above reports what the search actually read, not what it
    // was allowed to read. `disagreed` and `busy` are the same discipline for
    // the two ways a generation can fail to be a home — kept apart (M15, H4)
    // rather than folded into one tally, so a refusal reporting all-busy does
    // not read as a refusal that probed nothing.
    let mut homes: Vec<Home> = Vec::new();
    let mut disagreed = 0u32;
    let mut busy = 0u32;

    // The common case, and the reason it is asked first and alone: a
    // conversation nobody has edited is at the generation this node last
    // committed, and finding it there costs exactly one read.
    match probe(store, &bound_session(key, current), claimed).await? {
        Probe::Home { delta, .. } => {
            return Ok(Search::Lands {
                generation: current,
                delta,
            });
        }
        // A generation the store has never held has nothing above it either:
        // this node's counter only ever names a generation this node actually
        // committed a claim to — found agreeing, or opened fresh — so a
        // missing one means no node has bound this key this far out. That is
        // the first turn of a fresh key, and it costs no read at all beyond
        // the existence check: the claim has nothing anywhere to disagree
        // with, so [`open_fresh`] creates the generation and takes it whole.
        Probe::Fresh => {
            return Ok(Search::Fresh {
                generation: current,
            });
        }
        Probe::Disagrees => disagreed += 1,
        Probe::Busy => busy += 1,
    }

    // Upward, one generation at a time, until the claim finds a generation it
    // continues or the store has never heard of one — that generation being
    // this key's first free slot, and nothing past it existing to find.
    //
    // The walk stops at the first free slot because generations are minted in
    // order: nothing above one the store has never held can itself be held,
    // so reading further would only ask "not here" of slots that provably
    // cannot answer otherwise. And because probing no longer creates that
    // slot merely by asking about it — see [`probe`] — the free slot this
    // walk finds when nothing upward agrees costs exactly the one read that
    // found it. It is only opened, by [`open_fresh`], if the downward walk
    // below finds nothing better; a home found there instead leaves this slot
    // exactly as it found it — nonexistent.
    let mut fresh = None;
    for step in 1..=MAX_PREFIX_PROBES {
        let generation = current.saturating_add(step);
        match probe(store, &bound_session(key, generation), claimed).await? {
            Probe::Fresh => {
                fresh = Some(generation);
                break;
            }
            Probe::Home { held, delta } => {
                homes.push(Home {
                    generation,
                    held,
                    delta,
                });
                break;
            }
            Probe::Disagrees => disagreed += 1,
            Probe::Busy => busy += 1,
        }
    }

    // Downward, for the older generation a resumed claim is still continuing.
    // This is the direction the first draft did not have, and its absence is
    // what let one claimed history have two homes depending on whether the
    // node answering it had served a divergent turn in between. It runs to the
    // bottom rather than stopping at its first answer, because every
    // generation below the current one already exists — probing them mints
    // nothing — and it is here that two agreeing generations genuinely
    // compete.
    for step in 1..=MAX_PREFIX_PROBES {
        let Some(generation) = current.checked_sub(step) else {
            break;
        };
        match probe(store, &bound_session(key, generation), claimed).await? {
            Probe::Home { held, delta } => homes.push(Home {
                generation,
                held,
                delta,
            }),
            Probe::Disagrees => disagreed += 1,
            // A hole below this node's counter is a shape the invariant above
            // says the store cannot be in; the walk stops rather than
            // pretending the run of existing generations continues past it.
            Probe::Fresh => break,
            Probe::Busy => busy += 1,
        }
    }

    // The longest agreeing generation, because two can agree at once and
    // continuing the shorter would append a second copy of what the longer
    // already holds. Ties break to the lower generation so that two nodes
    // searching the same family from different counters reach the same answer.
    if let Some(home) = homes
        .into_iter()
        .max_by_key(|home| (home.held, Reverse(home.generation)))
    {
        return Ok(Search::Lands {
            generation: home.generation,
            delta: home.delta,
        });
    }

    if let Some(generation) = fresh {
        return Ok(Search::Fresh { generation });
    }

    Ok(Search::Exhausted { disagreed, busy })
}

/// Create the generation the search decided is the claim's home, then commit
/// it — in that order, and the only place in this search either happens.
///
/// **The one write, made exactly once, only after the home is known.** Every
/// other generation the search visited was asked about through [`probe`] and
/// left exactly as it was found; this function exists so that fact stays
/// true regardless of which of `bind_prefix`'s two `Fresh` sites calls it —
/// the ordinary first-turn-of-a-key case and the "nothing agreed anywhere"
/// fallback are one call site, not two chances for one of them to create
/// before commit. See the module doc's "Probe, then commit" section for what
/// the earlier, per-attempt version of this cost.
async fn open_fresh<S, T>(
    engine: &Engine<S, T>,
    conversations: &Conversations,
    principal: &Principal,
    key: &str,
    generation: u32,
) -> Result<SessionId, ApiError>
where
    S: SessionStore,
    T: Tokenizer + Clone + Send + Sync + 'static,
{
    engine
        .create_session(&bound_session(key, generation))
        .await
        .map_err(|error| ApiError::internal("engine_error", error.to_string()))?;
    Ok(conversations.commit(principal, key, generation).await)
}

/// One generation that agrees with the claim, and how much of the claim it
/// already holds.
///
/// `held` is the stored history's own length rather than the delta's, because
/// the question the search asks of two agreeing generations is which of them
/// holds *more* of the conversation — a generation whose log is longer is the
/// one continuing it would not duplicate.
struct Home {
    generation: u32,
    held: usize,
    delta: Vec<Item>,
}

/// What one generation has to say about a claim, asked without writing
/// anything down.
///
/// Four answers and not `Option<Vec<Item>>`, because the two that are not
/// "agrees" are not the same refusal: a generation whose log disagrees is one
/// the search may never land on, while one another node is mid-turn on is a
/// slot that is simply not ours — the claim would be admitted onto a session
/// this node cannot write to and the turn would die on the lease, in-stream,
/// after admission had already reported success.
enum Probe {
    /// The store has never heard of this generation: the first free slot in
    /// the key's family. `probe` does not create it — [`open_fresh`] does,
    /// and only once the search has decided this is where the claim belongs.
    Fresh,
    /// This generation's log agrees with the claim, and `delta` is what the
    /// turn may append to it.
    Home { held: usize, delta: Vec<Item> },
    /// Its log and the claim disagree somewhere they overlap.
    Disagrees,
    /// Nothing recorded, but another writer holds the lease: another node's
    /// fresh slot, one instruction into its first turn.
    Busy,
}

/// Ask one generation whether it is the claim's home, writing nothing down.
///
/// **The one existence check, spelled once**, and the only consumer of
/// [`SessionStore::last_seq`] in this search: the same question, asked the
/// same way, for this node's current generation and for every generation the
/// search walks to, so the two cannot drift apart.
///
/// Existence is asked through `last_seq` rather than
/// [`SessionStore::create_session`] — the idiom
/// [`mcp_api::named_session`](crate::mcp_api) already reads existence by —
/// because `create_session` is create-if-missing: calling it here, as the
/// first fix pass did, wrote a session into the store on every generation
/// merely asked about, including the ones the search went on to reject.
/// R13's own rule is "commit nothing until the home is known", and a probe
/// that can mint a session is committing before it knows anything. Creating
/// the generation the search actually lands on is [`open_fresh`]'s job, done
/// once, after the search is over.
async fn probe<S: SessionStore>(
    store: &S,
    session_id: &SessionId,
    claimed: &[Item],
) -> Result<Probe, ApiError> {
    // `SessionNotFound` is the honest spelling of "fresh": nothing this node
    // or any other has ever bound to this generation. Any other store error
    // is a real failure and must not be read as an absent session — see
    // [`store_error`] for the same distinction made the same way at every
    // other call into a [`SessionStore`] in this module.
    match store.last_seq(session_id).await {
        Err(StoreError::SessionNotFound(_)) => return Ok(Probe::Fresh),
        Err(other) => return Err(store_error(session_id, other)),
        Ok(_) => {}
    }

    let stored = stored_conversation(store, session_id).await?;
    // Asked only of a log with nothing in it, which is the only shape a
    // just-created generation and a generation another node is one instruction
    // into its first turn on can share. Anything already recorded here settles
    // the question by agreeing or disagreeing, exactly as it always did, and
    // pays no second round trip for the lease.
    if stored.items.is_empty()
        && store
            .is_leased(session_id)
            .await
            .map_err(|error| store_error(session_id, error))?
    {
        return Ok(Probe::Busy);
    }

    Ok(match admit(&stored, claimed) {
        Some(delta) => Probe::Home {
            held: stored.history().len(),
            delta,
        },
        None => Probe::Disagrees,
    })
}

/// The session as prefix admission sees it: its turn configuration, then the
/// history a client's claim is checked against.
struct StoredConversation {
    /// Configuration run first, then history — the same order and the same
    /// placement rule [`SessionState`](roundhouse_core::session::SessionState)
    /// folds with, because the two have to describe one session.
    items: Vec<Item>,
    configuration_len: usize,
}

impl StoredConversation {
    fn configuration(&self) -> &[Item] {
        &self.items[..self.configuration_len]
    }

    fn history(&self) -> &[Item] {
        &self.items[self.configuration_len..]
    }
}

/// One entry of the log that this projection cares about.
///
/// Turn starts are kept because the configuration run is a *per turn* fact —
/// see [`ConfigurationCursor`] — so the projection cannot be a filter over
/// items alone.
enum Entry {
    TurnStarted,
    Item(Item),
}

/// The session's committed conversation, projected from the log.
///
/// A projection rather than a [`Session`](roundhouse_core::session::Session):
/// opening one takes the lease, and a read that took the lease would evict the
/// turn it is about to start.
///
/// Three things the raw item stream does not say are resolved here, and all
/// three are review findings:
///
/// **A partial committed by a response that never completed is provisional**
/// (M11.1's F2). [`Session::mark_incomplete`](roundhouse_core::session::Session)
/// commits the bytes a dying dispatch had already produced so a successor can
/// resume from them — but the client's SSE layer threw that answer away the
/// moment it read the `error` frame, so the history it resends next has a hole
/// exactly where the partial sits. Compared strictly, the client's own honest
/// retry disagrees with us at that item and forks the session it has been using
/// all along, losing the routing history and the warm prefix on the turn a
/// transient upstream failure already cost it once. So an item stamped by a
/// response the log records as incomplete is left out of what a claim is
/// checked against: the client may resend it, in which case it is re-admitted
/// as ordinary unstamped history, or it may not, in which case the turn it
/// belonged to is simply regenerated on the same session. Divergence anywhere
/// else still forks, because a provisional item is the only item on the log
/// this deployment knows the client may never have seen.
///
/// The rejected alternative was to drop the partial from the *session's own*
/// fold as well, so the retry regenerates from a prompt that never saw it. That
/// is a larger change than the finding: it contradicts `mark_incomplete`'s own
/// documented reason for committing a partial at all, and it costs the
/// guaranteed cache hit that partial represents on the target that produced it.
/// What is written here is the admission half only — the retry still continues
/// from the partial, and the *next* turn no longer forks over it.
///
/// **An item stamped by a response that never terminated at all is
/// provisional too, once the turn that stamped it is over** (M11.2a's F3).
/// F2's rule reads the `ResponseIncomplete` beside a partial, and so covers
/// exactly the failures that got as far as writing one. The failure that does
/// not is the one `append_emitted`'s own doc calls the thing it "must never
/// leave behind": an emitted item — a durably committed *tool call*, since
/// M11.2 the only item a turn commits before its terminal — and no terminal
/// event whatever, because the very append that would have written one is what
/// failed. To a strict comparison that item is ordinary, permanently confirmed
/// history; to the client it never existed, because its own stream threw before
/// the block closed. The client's honest retry then disagrees with us at that
/// item and forks the session a transient store failure already cost it once —
/// the identical outcome F2 exists to prevent, reopened by a new item class
/// that fix does not cover.
///
/// **The liveness guard is the whole of the difference between the two rules.**
/// A `ResponseIncomplete` is proof the turn is over; "no terminal event" is not
/// — it is also what a turn *in flight* looks like, one that has streamed a
/// tool call and is still working. So the widened set applies only to a session
/// nobody is writing: [`SessionStore::is_leased`] answers that, and it is asked
/// after the read rather than before, with the log's tail re-checked for
/// settlement, because the dangerous race is a terminal event landing between
/// this projection's last batch and the question — a turn that finished
/// normally, read as an orphan. A leased or still-growing session falls back to
/// F2's rule exactly, which is the pre-M11.2a behaviour.
///
/// The two rejected alternatives, for the reader who would reopen this: giving
/// `Session::complete`'s failure path a terminal fallback of its own is the fix
/// closest to `append_emitted`'s stated invariant, but the reason that append
/// failed is usually a lost lease, so the fallback would fail for the same
/// reason and the orphan would survive it — an invariant enforced only when
/// nothing went wrong. Reconciling orphans in a later turn (writing the missing
/// `ResponseIncomplete` from a successor's lease) is a real repair rather than
/// a reading, and it would put an event in the log describing a turn this
/// process never ran; the append-only discipline F2's fix was careful to keep
/// says a supersession is a *reading* of what was recorded.
///
/// **A turn's leading configuration run replaces the session's** (M11.1's F7),
/// through exactly the cursor
/// [`SessionState`](roundhouse_core::session::SessionState) folds with, so the
/// prompt the engine rebuilds and the prefix a claim is checked against cannot
/// disagree about where the system prompt is.
async fn stored_conversation<S: SessionStore>(
    store: &S,
    session_id: &SessionId,
) -> Result<StoredConversation, ApiError> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut provisional: HashSet<ResponseId> = HashSet::new();
    // Every response the log mentions, and every response it *ends*. The
    // difference between the two — over a settled, unleased session — is the
    // orphan set F3 is about.
    let mut stamped: HashSet<ResponseId> = HashSet::new();
    let mut terminated: HashSet<ResponseId> = HashSet::new();
    let mut cursor = 0u64;
    loop {
        let batch = store
            .read_events(session_id, cursor, READ_BATCH)
            .await
            .map_err(|error| store_error(session_id, error))?;
        let Some(last) = batch.last() else { break };
        cursor = last.seq;
        for event in batch {
            match event.kind {
                SessionEventKind::ItemAppended { item } => {
                    if let Some(response_id) = &item.response_id {
                        stamped.insert(response_id.clone());
                    }
                    entries.push(Entry::Item(item));
                }
                SessionEventKind::TurnStarted { .. } => entries.push(Entry::TurnStarted),
                SessionEventKind::ResponseIncomplete { response_id, .. } => {
                    terminated.insert(response_id.clone());
                    provisional.insert(response_id);
                }
                SessionEventKind::ResponseCompleted { response_id, .. } => {
                    terminated.insert(response_id);
                }
                _ => {}
            }
        }
    }

    // **The orphan half of the rule, and both of its preconditions.**
    //
    // Asked only when there is something to ask about: a session all of whose
    // responses terminated — every session, until one of them does not — has an
    // empty orphan set, and this projection then costs exactly the reads it
    // cost before F3 rather than two store round trips on every turn.
    //
    // The lease is asked first because it is the cheaper refusal, and the tail
    // second because it closes the race the lease alone cannot: a turn whose
    // terminal event landed after the loop above ended would answer "nobody is
    // writing" truthfully and still have committed the very event that makes
    // its items ordinary history. A log that grew under us is a log this
    // projection has not finished reading, and superseding out of a stale
    // snapshot is exactly the fork this rule exists to prevent — so the
    // conservative arm keeps F2's narrower set and the next turn tries again
    // against a settled log.
    let orphans: Vec<ResponseId> = stamped.difference(&terminated).cloned().collect();
    if !orphans.is_empty() {
        let idle = !store
            .is_leased(session_id)
            .await
            .map_err(|error| store_error(session_id, error))?;
        if idle
            && store
                .last_seq(session_id)
                .await
                .map_err(|error| store_error(session_id, error))?
                == cursor
        {
            provisional.extend(orphans);
        }
    }

    // Collected first and resolved second, because a response is only known to
    // be incomplete by an event that arrives *after* the item it stamped: a
    // single streaming pass would have to admit the partial before learning it
    // was provisional.
    let mut items = Vec::with_capacity(entries.len());
    let mut configuration = ConfigurationCursor::default();
    for entry in entries {
        match entry {
            Entry::TurnStarted => configuration.turn_started(),
            Entry::Item(item) => {
                if item
                    .response_id
                    .as_ref()
                    .is_some_and(|id| provisional.contains(id))
                {
                    continue;
                }
                configuration.append(&mut items, item);
            }
        }
    }
    Ok(StoredConversation {
        configuration_len: configuration.len(),
        items,
    })
}

/// What this turn may append, or `None` when the claim is not a continuation
/// of this session at all.
///
/// **The configuration run and the history are admitted under different
/// rules, and that asymmetry is finding F7's ruling** (M11.1 thermo-nuclear
/// review). History is checked strictly and forks on any disagreement: it is a
/// claim about what already happened, and two sides that disagree about that
/// are not in one conversation. The leading configuration run is not a claim
/// about what happened — it is the instruction block the client re-derives from
/// its own environment on every single invocation, so it moves for reasons that
/// have nothing to do with the conversation: the date rolls over, the user
/// changes directory or branch, a beta flag drops out of the header, the client
/// self-updates overnight. Forking on that defeats warm-prefix caching for
/// every real session, on precisely the turn it would first have paid off —
/// which is what the milestone's own captured `--continue` pair does today.
///
/// So a changed configuration run is *recorded*, never forked on: the new run
/// is admitted as this turn's input and replaces the stored one at the head.
/// Three consequences, taken deliberately:
///
/// - The turn id is still a hash of the whole canonicalized conversation, the
///   new configuration included (it is computed by the handler, over the items
///   passed in here). A byte-identical retry therefore still deduplicates onto
///   the response it already paid for, and a turn whose system prompt changed
///   is a new turn — which it is.
/// - The provider-side cache the old configuration was holding is lost. That
///   loss is the client's own doing and needs no compensation here: it rewrote
///   the bytes at the front of its own prompt.
/// - An *interior* system message is history, not configuration, and is
///   compared strictly like everything else. Where the split is decided —
///   once, at canonicalization, by position — is
///   [`is_turn_configuration`](roundhouse_core::session::is_turn_configuration).
///
/// A claim carrying **no** configuration at all against a session that holds
/// some leaves the stored run in place. That is "this request said nothing
/// about the instructions", not "the instructions are now empty": an empty run
/// has no items to append and so nothing to record, and forking over it would
/// punish exactly the bare `curl` the anonymous arm exists to serve.
fn admit(stored: &StoredConversation, claimed: &[Item]) -> Option<Vec<Item>> {
    let claimed_configuration_len = turn_configuration_len(claimed);
    let (claimed_configuration, claimed_history) = claimed.split_at(claimed_configuration_len);
    let suffix = suffix_after(stored.history(), claimed_history)?;

    let mut delta = Vec::with_capacity(claimed_configuration.len() + suffix.len());
    if !claimed_configuration.is_empty()
        && !same_items(stored.configuration(), claimed_configuration)
    {
        delta.extend_from_slice(claimed_configuration);
    }
    delta.extend(suffix);
    Some(delta)
}

/// The part of `claimed` that `stored` does not already contain.
///
/// `None` when the two disagree anywhere they overlap. A `claimed` shorter than
/// `stored` is not a disagreement but the ordinary retry: the client is
/// re-sending a turn whose answer we already appended and it never saw, and the
/// empty suffix it yields is exactly right — the turn id will deduplicate it
/// onto the response that answer belongs to.
fn suffix_after(stored: &[Item], claimed: &[Item]) -> Option<Vec<Item>> {
    let overlap = stored.len().min(claimed.len());
    stored[..overlap]
        .iter()
        .zip(&claimed[..overlap])
        .all(|(stored, claimed)| same_item(stored, claimed))
        .then(|| claimed[overlap..].to_vec())
}

/// Item equality as this surface sees it: role and content, never the response
/// stamp, and asymmetrically on one field of one variant.
///
/// Assistant history comes back as the model's own words with no id attached —
/// the client has no field to put one in — so comparing stamps would fail the
/// prefix check on every turn after the first. That is the first stated
/// exception; [`same_namespace`] is the second and the only other one.
fn same_item(stored: &Item, claimed: &Item) -> bool {
    stored.role == claimed.role && same_content(&stored.content, &claimed.content)
}

/// Content equality, deferring to the derived `PartialEq` on everything but a
/// tool call.
///
/// A `match` on the pair rather than a field-by-field comparison of every
/// variant, so a variant added later is compared structurally by default: the
/// safe answer for a shape nobody has thought about is "these differ", because
/// a false *agreement* silently admits a claim that is not the stored
/// conversation, while a false disagreement forks — visibly, into a generation
/// that then continues.
fn same_content(stored: &ItemContent, claimed: &ItemContent) -> bool {
    match (stored, claimed) {
        (
            ItemContent::ToolCall {
                call_id: stored_id,
                name: stored_name,
                arguments: stored_arguments,
                namespace: stored_namespace,
            },
            ItemContent::ToolCall {
                call_id: claimed_id,
                name: claimed_name,
                arguments: claimed_arguments,
                namespace: claimed_namespace,
            },
        ) => {
            stored_id == claimed_id
                && stored_name == claimed_name
                && stored_arguments == claimed_arguments
                && same_namespace(stored_namespace.as_deref(), claimed_namespace.as_deref())
        }
        _ => stored == claimed,
    }
}

/// The namespace rule, and **the single load-bearing edit of M17** (R-N8).
///
/// Neither blind nor symmetric, and each half is a decision:
///
/// - **Not blind.** A comparison that ignored the field would never notice a
///   client changing which MCP server a tool name came from — which is a
///   genuinely different call, dispatched to a different server, and should
///   fork exactly as a changed name does. `same_item` is already blind to
///   `response_id`; that is blindness to a stamp *we* wrote, not to something
///   the client said.
/// - **Not symmetric.** A stored `None` is a record written before the field
///   existed, or by the Messages surface where the flat spelling is the
///   namespace, so it agrees with any claim: a conversation whose early turns
///   predate M17 and whose next request canonicalizes with a namespace
///   continues instead of forking on the change. A stored `Some` is the
///   client's own word for where the call went, and requires equality — an
///   absent claim included, because "the client stopped sending the field" is
///   not evidence that it means the same call.
///
/// The asymmetry is what makes the rung forward-only rather than a migration:
/// every straddling conversation keeps its generation, and no stored byte was
/// touched to achieve it.
fn same_namespace(stored: Option<&str>, claimed: Option<&str>) -> bool {
    match stored {
        None => true,
        Some(_) => stored == claimed,
    }
}

/// [`same_item`], run over two runs of the same length.
///
/// Stamp-blind for the same reason, and length-sensitive on purpose: a
/// configuration run that gained or lost a block is a changed run, not a
/// prefix-matching one.
fn same_items(stored: &[Item], claimed: &[Item]) -> bool {
    stored.len() == claimed.len() && stored.iter().zip(claimed).all(|(a, b)| same_item(a, b))
}

#[cfg(test)]
mod tests;
