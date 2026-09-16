// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a turn is allowed to do.
//!
//! [`control`](super) says *who* a turn belongs to; this says what that
//! membership may spend it on. The two are deliberately separate types
//! resolved together at admission, because a principal is a fact about a
//! request and a [`TurnPolicy`] is a fact about a deployment's configuration —
//! the same principal serves a different policy the moment an operator edits
//! the file.
//!
//! The split that matters more is the one against the router.
//! [`routing::policy`](crate::routing::policy) decides *how* a target is
//! chosen; this decides *which targets may be chosen from*. Keeping them apart
//! is what stops every deployment-authored [`RoutingPolicy`] from
//! re-implementing tenancy, and it means "a policy that ignored its
//! constraints" is a thing that can be tested once, centrally, rather than
//! once per policy. [`TurnPolicy::admits`] is that one place.
//!
//! Three properties are load-bearing enough to state before the types.
//!
//! **[`TurnPolicy::unrestricted`] must route byte-identically to a deployment
//! that has never heard of tenancy.** An open-mode deployment resolves every
//! request to it, so if it changed a single decision, turning the control
//! plane on would silently re-route every existing workload. It is pinned by
//! test, not by care.
//!
//! **Narrowing is the only composition.** [`TurnPolicy::narrow`] is total and
//! can only shrink the admissible set, which is what will make the MCP
//! overlays safe to hand to an agent later and what makes a per-key override
//! safe to hand to a project owner now. The allow-list axis cannot be widened
//! *structurally* — see [`TargetFilter`] — while the two numeric axes can, so
//! those are clamped here and rejected at the configuration boundary. Both
//! halves are deliberate: clamping keeps the runtime total, and rejecting
//! keeps an operator from believing a file that does not mean what it says.
//!
//! **The cadence counts dispatches, not successes.** See [`FrontierCadence`].

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::routing::{Candidate, Target};

/// The version tag every canonical policy string opens with, and therefore
/// part of every digest in every log.
///
/// **The rule: when the golden pins in
/// `the_digest_of_two_known_policies_is_pinned_to_a_literal` fail, the
/// encoding changed and this constant gets bumped** — before, not instead of,
/// updating the literals. A log spans the change; two encodings sharing one
/// version tag makes a fingerprint from either side of it unreadable, which is
/// the one property the digest exists to have. Editing the pinned literals
/// alone is the shortcut that costs it.
const DIGEST_VERSION: &str = "v1";

/// Patterns naming the targets a principal may be routed to.
///
/// # Identity
///
/// A target is matched by the spelling [`Target::policy_identity`] gives it —
/// `provider/model` for a hosted model, `local/model` for one of our own
/// workers. Both halves of the fleet are therefore nameable by one filter, and
/// `local/*` is a sentence an operator can write.
///
/// # Dialect
///
/// The only metacharacter is `*`, which matches any run of characters
/// *including* `/`. That is what makes a bare `*` mean "everything" rather
/// than "everything without a slash", which under this identity spelling would
/// be nothing at all. Richer glob syntax (`**`, `?`, character classes, brace
/// alternation) is rejected at parse time rather than silently treated as
/// literal text: a filter that quietly matched nothing would route every turn
/// local and look exactly like a cost win.
///
/// # Why layers
///
/// An empty filter admits everything. Narrowing *appends a layer*, and a
/// target must satisfy every layer, so composition is exact set intersection
/// and a narrowing step cannot widen — not by policy, by construction. The
/// alternative, merging two pattern lists into one, has no correct answer:
/// glob intersection is not expressible as a glob, so any single-list merge
/// either over-admits or invents a subset test that is wrong at the edges.
///
/// The cost of that choice is that a filter which admits *nothing* is
/// representable (two disjoint layers), and cannot be detected here — the
/// catalog it would be checked against is a different file. That check belongs
/// where both are loaded, and the process refuses to serve without it.
/// # Not deserializable, on purpose
///
/// There is no `Deserialize` impl and no `TryFrom<Vec<String>>`. Both existed
/// and neither was ever used: the configuration boundary reads raw
/// `Vec<String>` into its own `PolicyConfig` and calls [`Self::parse`]
/// explicitly, precisely so a malformed pattern is refused *naming the entry
/// it came from* rather than as a bare serde error naming a JSON path. A
/// `Deserialize` impl here is therefore not a convenience but a second door
/// into the same room, and the one behind it produces the worse error — so it
/// is gone, and this paragraph is here so it does not come back as an
/// obvious-looking addition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetFilter {
    /// Conjunction of disjunctions: every layer must admit, and within a layer
    /// any pattern does.
    layers: Vec<Vec<String>>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FilterError {
    #[error("a target pattern may not be empty; use `*` to allow everything")]
    EmptyPattern,
    #[error(
        "target pattern `{pattern}` uses `{token}`, which this dialect does not support: the only metacharacter is `*`, and it already crosses `/`"
    )]
    Unsupported {
        pattern: String,
        token: &'static str,
    },
    /// A pattern carrying a character the digest encoding reserves.
    ///
    /// Separate from [`Self::Unsupported`] because the reason is different and
    /// an operator has to be told which one applies: `**` is syntax this
    /// matcher does not implement, while a comma is syntax the *fingerprint*
    /// cannot represent. See [`TurnPolicy::canonical`] — the two are one
    /// edit apart and have to stay that way.
    ///
    /// [`TurnPolicy::canonical`]: TargetFilter::canonical
    #[error(
        "target pattern `{pattern}` contains {token}, which the policy digest's canonical \
         encoding uses to separate patterns and layers: one pattern carrying it would encode \
         as two, and would fingerprint identically to a filter that really has two"
    )]
    Unencodable {
        pattern: String,
        token: &'static str,
    },
}

impl TargetFilter {
    /// The absent-filter value: every target is nameable.
    pub fn allow_all() -> Self {
        Self { layers: Vec::new() }
    }

    /// One layer of alternatives, validated.
    ///
    /// The only constructor that takes strings, so there is exactly one place
    /// a malformed pattern can enter — which is what lets the configuration
    /// boundary report the offending entry by name instead of discovering the
    /// problem at the first turn.
    pub fn parse<I, S>(patterns: I) -> Result<Self, FilterError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let layer = patterns
            .into_iter()
            .map(|pattern| {
                let pattern = pattern.into();
                if pattern.is_empty() {
                    return Err(FilterError::EmptyPattern);
                }
                // Two rejection lists, refusing for two different reasons.
                //
                // These four are dialects this matcher does not implement.
                // Taken literally instead, a filter would quietly match
                // nothing, route every turn local, and look exactly like a
                // cost win.
                for token in ["**", "?", "[", "{"] {
                    if pattern.contains(token) {
                        return Err(FilterError::Unsupported { pattern, token });
                    }
                }
                // These are the characters `TargetFilter::canonical` builds
                // the digest input out of: it joins a layer's patterns with
                // `,` and the layers with `;`. A pattern carrying either
                // encodes as two patterns, so `["a,b"]` — which admits
                // nothing — and `["a", "b"]` — which admits both — produce
                // the identical canonical string and the identical
                // fingerprint. Whitespace and control characters go with them
                // because they cannot appear in a `policy_identity` either
                // (both halves are `provider/model`) and because a pattern
                // that differs from another only by a trailing newline is a
                // second spelling of one policy, which is exactly what
                // canonicalization exists to rule out.
                //
                // Refused here rather than escaped there: escaping keeps the
                // collision reachable through any later encoding change, and
                // would have to be gotten right inside the one function whose
                // output is pinned by golden digest.
                if let Some(offender) = pattern
                    .chars()
                    .find(|c| matches!(c, ',' | ';') || c.is_whitespace() || c.is_control())
                {
                    let token = match offender {
                        ',' => "a comma",
                        ';' => "a semicolon",
                        c if c.is_whitespace() => "whitespace",
                        _ => "a control character",
                    };
                    return Err(FilterError::Unencodable { pattern, token });
                }
                Ok(pattern)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(if layer.is_empty() {
            Self::allow_all()
        } else {
            Self {
                layers: vec![layer],
            }
        })
    }

    /// Whether this filter names `target`.
    pub fn matches(&self, target: &Target) -> bool {
        let identity = target.policy_identity();
        self.layers
            .iter()
            .all(|layer| layer.iter().any(|pattern| glob_match(pattern, &identity)))
    }

    /// Intersect with `other`: the result admits a target only if both did.
    fn intersect(&self, other: &TargetFilter) -> TargetFilter {
        let mut layers = self.layers.clone();
        layers.extend(other.layers.iter().cloned());
        TargetFilter { layers }
    }

    /// Canonical spelling, for the digest.
    ///
    /// Sorted at both levels because both levels are order-independent — a
    /// conjunction of disjunctions means the same thing however it is written
    /// — so two policies that differ only in the order an operator listed
    /// their patterns must not read as different policies in the audit trail.
    ///
    /// **The two separators below are the reason [`Self::parse`] refuses `,`
    /// and `;` inside a pattern**, and its second rejection list names this
    /// function for the same reason this comment names that one. Nothing else
    /// makes the encoding injective: a pattern free to contain a separator
    /// turns one layer into two and hands two policies with different
    /// admissible sets the same fingerprint. Changing a separator here means
    /// changing the rejection list there, in the same edit.
    fn canonical(&self) -> String {
        let mut layers: Vec<String> = self
            .layers
            .iter()
            .map(|layer| {
                let mut patterns = layer.clone();
                patterns.sort();
                patterns.join(",")
            })
            .collect();
        layers.sort();
        layers.join(";")
    }

    /// The redundancy-free spelling, for asking whether two filters *mean* the
    /// same thing.
    ///
    /// **Deliberately not [`Self::canonical`], and the difference is the whole
    /// point.** `canonical` is the digest's input: it answers "how was this
    /// policy written", which is exactly right for a fingerprint stamped on a
    /// `DecisionRecord` — an operator who edits a file wants the audit trail to
    /// show that they did — and exactly wrong for deciding whether two keys
    /// disagree. It also carries a golden pin and a `DIGEST_VERSION`, so
    /// teaching it to dedupe would renumber every policy in every log to fix a
    /// comparison it is not the one making.
    ///
    /// Three redundancies are removed, and each is a theorem about
    /// [`Self::matches`] rather than a guess about operator intent:
    ///
    /// - **A repeated pattern inside a layer.** A layer is a disjunction, so
    ///   `any` over it is unmoved by a duplicate.
    /// - **A repeated layer.** Layers conjoin, so a second copy of a layer
    ///   admits exactly what the first already did. This is the one that turns
    ///   a key restating its project's `allow` — the shape of a secret rotation
    ///   where the operator writes down the policy already in force — into two
    ///   identical layers where the inheriting key has one.
    /// - **A layer naming `*`.** `*` crosses `/` (see [`Self::parse`]'s
    ///   rejection list, which is why `**` is not a dialect here), so such a
    ///   layer admits every `policy_identity` and constrains nothing.
    ///
    /// Nothing else is folded. Two patterns where one subsumes the other —
    /// `local/*` and `local/llama` in one layer — are left alone: deciding that
    /// needs glob subsumption, and a comparison that is *conservative* errs
    /// toward reporting an ambiguity an operator can resolve by hand, which is
    /// the safe direction for a check whose failure mode is a wrong answer
    /// about what a key may do.
    fn comparison_form(&self) -> Vec<Vec<String>> {
        let mut layers: Vec<Vec<String>> = self
            .layers
            .iter()
            .filter(|layer| !layer.iter().any(|pattern| pattern == "*"))
            .map(|layer| {
                let mut patterns = layer.clone();
                patterns.sort();
                patterns.dedup();
                patterns
            })
            .collect();
        layers.sort();
        layers.dedup();
        layers
    }
}

impl fmt::Display for TargetFilter {
    /// The layered intersection, spelled the way the filter reads: alternatives
    /// within a layer separated by `|` and parenthesized, layers joined by
    /// ` & ` because every one of them must admit.
    ///
    /// For the operator staring at a startup refusal. A
    /// [`TurnPolicy::digest`] tells them that two keys differ; it never tells
    /// them which pattern they mistyped, and the pattern is the entire content
    /// of the mistake. Deliberately *not* [`Self::canonical`], whose whole job
    /// is to be a stable digest input — sorting and separators there answer to
    /// the audit trail, and a rendering that answered to both would be one
    /// change away from silently moving every digest in a deployment's log.
    ///
    /// The absent filter renders as `*`, which is what it means and what an
    /// operator would have written for it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.layers.is_empty() {
            return f.write_str("*");
        }
        for (index, layer) in self.layers.iter().enumerate() {
            if index > 0 {
                f.write_str(" & ")?;
            }
            match layer.as_slice() {
                [only] => f.write_str(only)?,
                many => write!(f, "({})", many.join("|"))?,
            }
        }
        Ok(())
    }
}

/// Match `value` against a pattern whose only metacharacter is `*`.
///
/// The textbook two-pointer walk with one backtrack point rather than a
/// regex: it is linear in practice, allocates the two `char` vectors and
/// nothing else, and — the reason that matters here — it has no dialect. (The
/// vectors buy indexing by character rather than by byte, so a multi-byte
/// pattern cannot be split mid-character by the backtrack; a byte walk would
/// be allocation-free and wrong for any non-ASCII model name.) A regex crate
/// would silently give `.`,
/// `+` and anchoring meanings the documented dialect does not have, and the
/// first operator to write `anthropic/claude.*` would get a filter that means
/// something other than what it reads as.
fn glob_match(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    let (mut p, mut v) = (0usize, 0usize);
    // Where to resume from if the current `*` turns out to have consumed too
    // little.
    let mut star: Option<(usize, usize)> = None;
    while v < value.len() {
        if p < pattern.len() && pattern[p] == value[v] {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, v));
            p += 1;
        } else if let Some((star_p, star_v)) = star {
            // `*` crosses `/` like any other character; that is what makes a
            // bare `*` mean everything under this identity spelling.
            p = star_p + 1;
            v = star_v + 1;
            star = Some((star_p, star_v + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// How often a session may reach for a hosted model.
///
/// At most `max_frontier` frontier dispatches in any window of `per_turns`
/// consecutive routed turns of one session. This is the user-facing "how often
/// frontier versus local" knob, and it is enforced as *admissibility*: when the
/// trailing window is spent, frontier candidates stop being admissible and the
/// turn serves locally. That is deliberately not the same thing as a filter
/// admitting nothing (see [`TurnPolicy::admits`]).
///
/// **"Serves locally" is a promise about the fleet, and this type cannot keep
/// it alone.** A deployment with no local capacity has nowhere for a spent
/// window to go, and the second turn of every rationed session would fail
/// exactly as an empty filter does — the opposite of the paragraph above. The
/// composition root is where a cadence and a fleet are both visible, so that
/// is where the promise is checked: a configured key whose policy carries a
/// cadence and leaves nothing under [`TurnPolicy::admits_when_spent`] stops
/// the process at startup rather than being discovered one turn at a time.
/// The paragraph above is therefore true of any deployment that booted.
///
/// **The counting basis is dispatch, not success.** A [`Routed`] event whose
/// target was a frontier model spends a ration even if the dispatch then
/// failed. The alternative — count only turns that completed — makes a
/// provider outage silently multiply the frontier spend of every session
/// retrying through it, which is exactly when a cadence knob is supposed to
/// hold.
///
/// [`Routed`]: crate::event::SessionEventKind::Routed
/// The one policy type that *is* deserializable, because it is the one whose
/// config shape is identical to its runtime shape — two required integers,
/// nothing to convert and no entry name an error would want to carry that the
/// enclosing `PolicyConfig` does not already supply.
///
/// `deny_unknown_fields` because serde's does not recurse: the attribute on
/// `PolicyConfig` guards the three policy axes — [`TurnPolicy::min_quality`],
/// [`TurnPolicy::allow`] and this one — and stops at their boundary, so
/// without this a stale or misspelled key *inside* a cadence object was
/// accepted and dropped, and an operator got a cadence they did not write with
/// nothing to tell them a line had been ignored.
/// `Serialize` since M16.1 (R-D7) for the reason every other config shape in
/// this workspace gained it in that rung: a project's policy is now written
/// *back* — into the durable admin directory's document — as well as read out
/// of an operator's file, and a cadence with no `Serialize` is a policy that
/// cannot be stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontierCadence {
    pub max_frontier: u32,
    pub per_turns: u32,
}

impl FrontierCadence {
    /// Whether one more frontier dispatch fits in the trailing window.
    pub fn admits_another(&self, history: &FrontierHistory) -> bool {
        history.frontier_in_last(self.per_turns) < self.max_frontier
    }

    /// Whether `self` permits strictly less frontier traffic than `other`.
    ///
    /// Rate first, by cross-multiplication rather than by dividing: integer
    /// division would make `1 per 3` and `1 per 2` both rate zero and so
    /// indistinguishable, and floats would put a rounding decision underneath
    /// a comparison an operator has to be able to predict.
    ///
    /// Equal rates are broken by window size, the shorter window winning:
    /// `1 per 2` and `5 per 10` allow the same traffic in the long run, but
    /// only the first also forbids a burst of five.
    fn is_tighter_than(&self, other: &FrontierCadence) -> bool {
        let mine = self.max_frontier as u64 * other.per_turns as u64;
        let theirs = other.max_frontier as u64 * self.per_turns as u64;
        (mine, self.per_turns) < (theirs, other.per_turns)
    }
}

/// Frontier dispatches, grouped by the routed turn that made them.
///
/// A projection of the session's [`Routed`] events rather than a counter
/// incremented beside them: a successor process that replays a log has to
/// arrive at the same window as the process it replaced, and a counter that
/// lived anywhere but in the fold would not. Nothing here is serialized — the
/// shape below is derived from the log on every open, so changing it costs no
/// migration and breaks no stored bytes.
///
/// **One entry per routed *turn*, holding that turn's count of frontier
/// reaches** — not one entry per dispatch, which is what review finding G05
/// was. Since M10 a turn may write more than one `Routed` (a decision that fell
/// forward to a fallback writes one per attempt), so a flat per-dispatch vector
/// made `per_turns` mean "the last N dispatches": on any failover turn the
/// vector grew faster than turns elapsed, the window walked *fewer* real turns
/// into the past than the cadence promised, and an older hosted turn aged out
/// early — relaxing the ration exactly on the sessions that had been failing
/// over hardest. The numerator's per-dispatch counting is deliberate and
/// survives here unchanged; only the window boundary moves.
///
/// [`Routed`]: crate::event::SessionEventKind::Routed
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrontierHistory {
    turns: Vec<TurnReaches>,
}

/// One routed turn, and how many of its dispatches went to a hosted model.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnReaches {
    /// The session's own turn index, which is what makes grouping a fact about
    /// the log rather than a guess: two `Routed` events belong to one turn
    /// exactly when the fold's `turn_index` had not moved between them. Keyed
    /// on equality with the previous entry rather than on a "new turn" signal,
    /// so a log whose `TurnStarted` is missing degrades to one group rather
    /// than to a panic.
    turn_index: u64,
    frontier: u32,
}

impl FrontierHistory {
    /// Fold one routing decision in, under the turn that made it.
    ///
    /// Crate-visible: the only honest producer is the session projection, and
    /// a history assembled by hand somewhere else would be a second answer to
    /// a question the log already answers.
    pub(crate) fn record(&mut self, target: &Target, turn_index: u64) {
        let reach = u32::from(!target.is_local());
        match self.turns.last_mut() {
            Some(last) if last.turn_index == turn_index => last.frontier += reach,
            _ => self.turns.push(TurnReaches {
                turn_index,
                frontier: reach,
            }),
        }
    }

    /// Frontier dispatches made during the last `turns` routed turns.
    ///
    /// A window longer than the session simply sees the whole session, which
    /// is the right reading: a cadence of "two per twenty" on a session five
    /// turns old has spent whatever those five turns spent.
    ///
    /// A failover turn contributes every reach it made — two dead hosted
    /// attempts are two reaches — but it occupies exactly one slot of the
    /// window, because `per_turns` is a promise about turns.
    pub fn frontier_in_last(&self, turns: u32) -> u32 {
        self.turns
            .iter()
            .rev()
            .take(turns as usize)
            .map(|turn| turn.frontier)
            .sum()
    }
}

/// What a principal's turns may do, resolved once at admission.
///
/// Immutable for the turn. Everything that consults it does so through
/// [`Self::admits`], and the candidate set is filtered by the same policy
/// before the router ever sees it — see the note there on why both exist.
/// Not deserializable, for the reason [`TargetFilter`] is not: the
/// configuration boundary reads its own `PolicyConfig` shape and converts,
/// because only it can name the entry an error belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnPolicy {
    /// Lowest [`Candidate::quality_prior`] this principal may be routed to.
    ///
    /// `0.0` is unrestricted rather than a floor that happens to admit
    /// everything, and the two are the same thing only because
    /// `quality_prior` is defined on `0.0..=1.0`. Stated so a reader does not
    /// go looking for an `Option`.
    pub min_quality: f64,
    pub allow: TargetFilter,
    pub frontier_cadence: Option<FrontierCadence>,
}

// No `Default` impl, and its absence is deliberate. There was one, returning
// `unrestricted()`, and its only reason to exist was the `#[serde(default)]`
// attribute above — which is gone with the `Deserialize` derive. Reaching for
// the most permissive policy in the system should be a sentence a reader can
// find, the same argument `Principal::default_open` is written out for; a
// `..Default::default()` that quietly widens a policy is the one spelling of
// it nobody would notice in review.

impl TurnPolicy {
    /// The policy an open-mode deployment, and any principal with no
    /// configured policy, resolves to.
    ///
    /// Routing under this value is byte-identical to routing before the
    /// control plane existed. That is a compatibility guarantee with a test
    /// behind it, not an intention.
    pub fn unrestricted() -> Self {
        Self {
            min_quality: 0.0,
            allow: TargetFilter::allow_all(),
            frontier_cadence: None,
        }
    }

    /// Whether `candidate` is one this principal may *ever* be routed to.
    ///
    /// The history-independent half: the quality floor and the allow filter,
    /// the two axes whose answer is the same on every turn of every session.
    /// A candidate this refuses is unreachable for as long as the policy
    /// stands, which is what makes it the right question for the two callers
    /// that are not choosing a target:
    ///
    /// - the engine's pre-`choose` filter, which decides what belongs in
    ///   `DecisionRecord::considered` — a cadence-rationed model belongs
    ///   there, because the counterfactual against it is true;
    /// - the startup cross-check, which asks whether a key can route anywhere
    ///   at all before the process agrees to serve it.
    ///
    /// Both used to spell this as `admits(candidate, &FrontierHistory::default())`
    /// — the same answer, obtained by fabricating a session history and
    /// leaving a paragraph of comment to explain which question the fabrication
    /// stood for. Naming the question is the fix; the fabricated history is
    /// what a reader had to decode.
    pub fn permits(&self, candidate: &Candidate) -> bool {
        candidate.quality_prior >= self.min_quality && self.allow.matches(&candidate.target)
    }

    /// The highest [`Candidate::quality_prior`] this policy still permits, or
    /// `None` when it permits none of them.
    ///
    /// **The ceiling a best-effort narrowing is clamped to.** Every other
    /// narrowing in this system is an ask somebody made and may be refused on:
    /// an `allow` filter that matches nothing empties the pool and fails the
    /// turn, and that is the operator's configured intent. The validator's
    /// escalation is the one narrowing *nobody asked for* — this deployment
    /// invented it, on a turn a client is already waiting for — so it must
    /// never be the reason a turn fails. Clamping its floor to this value turns
    /// "raise the bar past every candidate" into "take the best candidate there
    /// is", which is what escalating meant in the first place.
    ///
    /// [`Self::permits`] and not [`Self::admits`], matching the engine's own
    /// pre-`choose` filter: a cadence-rationed model is not unreachable, it is
    /// unavailable *this* turn, and a ceiling computed against a spent ration
    /// would collapse an escalation for the rest of a session because one turn
    /// had used its frontier allowance.
    ///
    /// `None` rather than `0.0` for a pool this policy permits nothing from,
    /// because those are opposite answers: no candidate at all is a refusal the
    /// caller has to leave standing, and a floor of zero is one every candidate
    /// meets.
    pub fn reachable_quality_ceiling(&self, candidates: &[Candidate]) -> Option<f64> {
        candidates
            .iter()
            .filter(|candidate| self.permits(candidate))
            .map(|candidate| candidate.quality_prior)
            .reduce(f64::max)
    }

    /// Whether `candidate` is one this principal may be routed to *on this
    /// turn*, given what the session has already spent.
    ///
    /// **The policy-axes half of admissibility**, and all three of them: the
    /// two [`Self::permits`] answers plus the cadence. The router asks it
    /// through
    /// [`RoutingContext::admissible`](crate::routing::RoutingContext::admissible),
    /// which conjoins it with the budget — the one axis that is not a policy's
    /// to answer for, because a policy is resolved once at admission and a
    /// grant is opened on every turn. Every
    /// [`RoutingPolicy`](crate::routing::RoutingPolicy) reaches it that way and
    /// none re-derives it, including the escalation audit branch, which would
    /// otherwise escalate straight past a quality floor it never asked about.
    ///
    /// A `bool` rather than a reason: nothing downstream branches on *why* a
    /// candidate was refused, and a reason nobody reads is a field that goes
    /// stale.
    pub fn admits(&self, candidate: &Candidate, history: &FrontierHistory) -> bool {
        self.permits(candidate) && self.cadence_allows(candidate, history)
    }

    /// What the dearest hosted candidate this policy admits *this turn* would
    /// cost — the amount the spend ledger is asked to reserve.
    ///
    /// The dearest and not the one about to be chosen, because a grant has to
    /// be a ceiling the choice is then made *under*. Reserving the chosen
    /// candidate's price would be a rubber stamp applied after the decision it
    /// was supposed to constrain, and every turn would be affordable by
    /// construction.
    ///
    /// Hosted only, because a local candidate is priced at zero and asking for
    /// zero more dollars is asking for nothing. Zero is therefore also the
    /// honest answer for a turn with no hosted option at all — a local-only
    /// key requests nothing and receives a ceiling that still admits every
    /// local candidate through the same comparison.
    ///
    /// A fold over [`Self::admits`] rather than a second opinion about
    /// admissibility: what the ledger reserves and what the router may then
    /// spend it on have to be the same set, or the grant is a number about a
    /// different question.
    pub fn dearest_admissible_frontier_usd(
        &self,
        candidates: &[Candidate],
        history: &FrontierHistory,
    ) -> f64 {
        candidates
            .iter()
            .filter(|candidate| !candidate.target.is_local())
            .filter(|candidate| self.admits(candidate, history))
            .map(|candidate| candidate.expected_cost_usd)
            .fold(0.0, f64::max)
    }

    /// Whether `candidate` survives a *fully spent* cadence window.
    ///
    /// The question a deployment has to answer before it boots, and the one
    /// neither of the two above asks: [`Self::permits`] ignores the cadence
    /// and [`Self::admits`] needs a session that does not exist yet. What is
    /// left when the ration is gone is every permitted local target and, if
    /// there is no cadence to spend, everything [`Self::permits`] allows.
    ///
    /// Stated as a function rather than as `admits(candidate, &spent_window)`
    /// because there is no honest way for a caller outside this crate to build
    /// a spent window: [`FrontierHistory::record`] is deliberately
    /// crate-private, since the only truthful producer is the session
    /// projection. A history assembled by hand to ask a question is a second
    /// answer to something the log already answers — so the question moved
    /// here instead.
    pub fn admits_when_spent(&self, candidate: &Candidate) -> bool {
        self.permits(candidate) && (candidate.target.is_local() || self.frontier_cadence.is_none())
    }

    /// The cadence axis alone: does one more dispatch to `candidate` fit?
    ///
    /// The cadence is a frontier knob and only a frontier knob. That asymmetry
    /// is the whole behavior: when the window is spent the hosted options go
    /// inadmissible and the local ones do not, so the turn *serves*, locally,
    /// instead of failing. Contrast an `allow` filter that matches nothing,
    /// which leaves no admissible candidate at all and must fail the turn — a
    /// filter that quietly routed everything local would look exactly like a
    /// cost win.
    fn cadence_allows(&self, candidate: &Candidate, history: &FrontierHistory) -> bool {
        candidate.target.is_local()
            || match &self.frontier_cadence {
                Some(cadence) => cadence.admits_another(history),
                None => true,
            }
    }

    /// Apply a narrowing overlay. Total, and can only shrink.
    ///
    /// The single composition operator: project policy narrowed by a key's
    /// overrides today, narrowed again by an MCP overlay later. An override
    /// that would widen an axis is clamped to the base rather than honored,
    /// so no caller can hand this a value that grows the admissible set.
    ///
    /// Clamping is the runtime half. The other half lives at the configuration
    /// boundary, which *rejects* a file whose override is outright wider —
    /// silently obeying an operator's file less than it says is how a
    /// deployment ends up believing a limit it does not have.
    pub fn narrow(&self, overrides: &PolicyOverrides) -> TurnPolicy {
        TurnPolicy {
            // `max`, not "the override wins": a lower floor is not a
            // narrowing, so it is not applied.
            min_quality: overrides
                .min_quality
                .map_or(self.min_quality, |floor| self.min_quality.max(floor)),
            // Layered, not replaced. See `TargetFilter`: this is the axis
            // where widening is impossible rather than merely refused.
            allow: match &overrides.allow {
                Some(allow) => self.allow.intersect(allow),
                None => self.allow.clone(),
            },
            frontier_cadence: match (self.frontier_cadence, overrides.frontier_cadence) {
                (Some(base), Some(over)) if over.is_tighter_than(&base) => Some(over),
                (Some(base), _) => Some(base),
                (None, over) => over,
            },
        }
    }

    /// A short, stable fingerprint of this policy, for the audit trail.
    ///
    /// Recorded on every [`DecisionRecord`](crate::routing::DecisionRecord) so
    /// that a policy change shows up on the very next routing event with no
    /// side channel able to disagree with it.
    ///
    /// **Determinism is the requirement, not brevity.** Replaying a log has to
    /// reproduce the digest a different process wrote, possibly on a different
    /// machine and a later build, or the audit trail is decorative. So: a
    /// canonical string with no map iteration in it, order-independent axes
    /// sorted, the float taken by its bits rather than by a formatting
    /// decision, and SHA-256 rather than [`std::hash`], whose output is
    /// explicitly not stable across releases.
    ///
    /// The canonical string opens with [`DIGEST_VERSION`], and the rule that
    /// makes that tag worth carrying is stated there: golden digests are
    /// pinned by test, and when they move the constant moves with them.
    pub fn digest(&self) -> String {
        let cadence = match &self.frontier_cadence {
            Some(cadence) => format!("{}/{}", cadence.max_frontier, cadence.per_turns),
            None => "none".to_string(),
        };
        // The floor goes in by its bits rather than by a formatted decimal:
        // `{:.6}` would be readable and would also make 0.8500001 and 0.85 the
        // same policy in the audit trail, which is a collision nobody would
        // ever look for. `-0.0` is folded into `0.0` so the unrestricted
        // policy has exactly one spelling.
        let floor = if self.min_quality == 0.0 {
            0.0f64
        } else {
            self.min_quality
        };
        let canonical = format!(
            "{DIGEST_VERSION}\nmin_quality={:016x}\nallow={}\ncadence={cadence}\n",
            floor.to_bits(),
            self.allow.canonical(),
        );
        let full = Sha256::digest(canonical.as_bytes());
        // A quarter of the hash: this is a fingerprint an operator reads off a log
        // line beside a decision, not a signature. 64 bits is far past the
        // point where two policies in one deployment collide.
        hex::encode(&full[..8])
    }

    /// Whether `other` admits exactly the turns this policy does, however
    /// either of them was spelled.
    ///
    /// **The question [`Self::digest`] is not the answer to.** A digest
    /// fingerprints a *spelling* — that is its job on a `DecisionRecord`, where
    /// an operator who rewrote a policy should see the audit trail change — so
    /// comparing two digests asks "were these written the same way", and two
    /// keys can spell one entitlement two ways: a key inheriting its project's
    /// `allow` and a key restating it as an override intersect to one layer and
    /// to two identical layers respectively. Comparing digests reports that as
    /// a disagreement, which turns a secret rotation into a boot failure. The
    /// caller is
    /// [`ControlPlane::membership`](../../../roundhouse_server/control_config/enum.ControlPlane.html),
    /// and the failure it must not produce is refusing to say what a key may do
    /// when both keys say the same thing.
    ///
    /// Conservative on every axis it cannot prove: the floor is compared by its
    /// bits (the same fold `digest` applies, so `-0.0` and `0.0` are one
    /// policy), the cadence structurally, and the filter through
    /// [`TargetFilter::comparison_form`], which removes only redundancies that
    /// are theorems about [`TargetFilter::matches`]. Anything subtler reads as
    /// disagreement, and disagreement is the arm an operator can fix by hand.
    pub fn admits_the_same_as(&self, other: &Self) -> bool {
        let floor = |policy: &Self| {
            if policy.min_quality == 0.0 {
                0.0f64.to_bits()
            } else {
                policy.min_quality.to_bits()
            }
        };
        floor(self) == floor(other)
            && self.frontier_cadence == other.frontier_cadence
            && self.allow.comparison_form() == other.allow.comparison_form()
    }

    /// Axes on which `overrides` is *wider* than this policy — the ones a
    /// configuration boundary must refuse rather than let [`Self::narrow`]
    /// quietly clamp.
    ///
    /// The other half of the rule stated on [`Self::narrow`]: clamping keeps
    /// the runtime total, and this keeps an operator-authored file from
    /// meaning less than it says.
    ///
    /// `allow` is absent from the answer on purpose and not by oversight:
    /// narrowing appends a layer, so an `allow` override cannot widen anything
    /// and there is nothing to reject. See [`TargetFilter`].
    pub fn widenings_of(&self, overrides: &PolicyOverrides) -> Vec<&'static str> {
        let mut wider = Vec::new();
        if overrides
            .min_quality
            .is_some_and(|floor| floor < self.min_quality)
        {
            wider.push("min_quality");
        }
        if let (Some(base), Some(over)) = (self.frontier_cadence, overrides.frontier_cadence)
            && !over.is_tighter_than(&base)
            && over != base
        {
            wider.push("frontier_cadence");
        }
        wider
    }
}

/// A narrowing overlay: the axes an override may tighten, each optional.
///
/// Absent means "do not touch this axis", which is why it is a separate type
/// from [`TurnPolicy`] rather than a `TurnPolicy` with sentinel values. A
/// sentinel here would be `min_quality: 0.0`, which is a real policy — the
/// unrestricted one — and an override that accidentally spelled it would read
/// as "lower the floor to nothing" rather than "leave it alone".
/// Not deserializable, for the reason [`TargetFilter`] is not: an override
/// arrives as the configuration boundary's own `PolicyConfig` and is converted
/// there, where an error can name the key it belongs to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PolicyOverrides {
    pub min_quality: Option<f64>,
    pub allow: Option<TargetFilter>,
    pub frontier_cadence: Option<FrontierCadence>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(target: Target, quality_prior: f64) -> Candidate {
        Candidate {
            target,
            expected_prefill_tokens: 1_000.0,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 100.0,
            expected_cost_usd: 0.0,
            quality_prior,
            load: None,
        }
    }

    fn frontier(provider: &str, model: &str) -> Target {
        Target::Frontier {
            provider: provider.into(),
            model: model.into(),
        }
    }

    fn local(model: &str) -> Target {
        Target::Local {
            worker_id: 7,
            dp_rank: 0,
            model: model.into(),
        }
    }

    fn filter(patterns: &[&str]) -> TargetFilter {
        TargetFilter::parse(patterns.iter().copied()).expect("well-formed patterns")
    }

    /// One dispatch per turn, which is what every caller of this helper means:
    /// the failover case has its own test below and builds its history by hand.
    fn history(turns: &[bool]) -> FrontierHistory {
        let mut history = FrontierHistory::default();
        for (turn_index, &to_frontier) in turns.iter().enumerate() {
            history.record(
                &if to_frontier {
                    frontier("anthropic", "claude")
                } else {
                    local("llama")
                },
                turn_index as u64,
            );
        }
        history
    }

    /// The grant request is a fold over `admits`, so every axis that takes a
    /// candidate away takes its price out of the request too.
    ///
    /// The failure this pins is a grant that is *too large*, which has no
    /// visible symptom at all: it reserves budget for a model this turn could
    /// never have been sent to, and every over-reservation is money another
    /// concurrent session is refused. Both directions matter — asking for too
    /// little would put a ceiling under a candidate the router is about to be
    /// offered, and degrade a turn nobody meant to degrade.
    #[test]
    fn the_grant_request_is_the_dearest_hosted_candidate_the_policy_admits() {
        let cheap = Candidate {
            expected_cost_usd: 0.25,
            ..candidate(frontier("anthropic", "claude"), 0.9)
        };
        let dear = Candidate {
            expected_cost_usd: 4.0,
            ..candidate(frontier("openai", "gpt-5"), 0.95)
        };
        let worker = candidate(local("llama"), 0.6);
        let pool = [worker.clone(), cheap.clone(), dear.clone()];
        let empty = FrontierHistory::default();

        assert_eq!(
            TurnPolicy::unrestricted().dearest_admissible_frontier_usd(&pool, &empty),
            4.0,
            "the ceiling has to cover the dearest option, or the router is \
             offered a candidate the ledger never reserved for"
        );

        // Each axis in turn. A filter and a floor are reachability, so they
        // remove a price permanently; a spent cadence removes it for this turn
        // only — and the request is a this-turn number, so it goes either way.
        let filtered = TurnPolicy {
            allow: filter(&["anthropic/*", "local/*"]),
            ..TurnPolicy::unrestricted()
        };
        assert_eq!(
            filtered.dearest_admissible_frontier_usd(&pool, &empty),
            0.25,
            "a filtered-out model is not something this key can spend on"
        );
        let floored = TurnPolicy {
            min_quality: 0.94,
            ..TurnPolicy::unrestricted()
        };
        assert_eq!(floored.dearest_admissible_frontier_usd(&pool, &empty), 4.0);
        let rationed = TurnPolicy {
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 1,
                per_turns: 3,
            }),
            ..TurnPolicy::unrestricted()
        };
        assert_eq!(
            rationed.dearest_admissible_frontier_usd(&pool, &history(&[true])),
            0.0,
            "a spent ration leaves this turn nothing hosted to pay for"
        );
        assert_eq!(
            rationed.dearest_admissible_frontier_usd(&pool, &empty),
            4.0,
            "the control: the same cadence with the window open reserves in full"
        );

        // Local is priced at zero and asking for zero more dollars is asking
        // for nothing, so a local-only pool requests nothing — which still
        // yields a ceiling every local candidate clears.
        assert_eq!(
            TurnPolicy::unrestricted().dearest_admissible_frontier_usd(&[worker], &empty),
            0.0
        );
        assert_eq!(
            TurnPolicy::unrestricted().dearest_admissible_frontier_usd(&[], &empty),
            0.0,
            "and an empty pool is a request for nothing rather than a panic"
        );
    }

    #[test]
    fn a_target_filter_matches_frontier_and_local_identities() {
        let claude = frontier("anthropic", "claude-opus-4");
        let gpt = frontier("openai", "gpt-5");
        let llama = local("llama-3.1-8b");

        // A bare star has to cross `/`, or it would match neither spelling.
        for target in [&claude, &gpt, &llama] {
            assert!(filter(&["*"]).matches(target), "`*` admits {target:?}");
        }

        let anthropic_only = filter(&["anthropic/*"]);
        assert!(anthropic_only.matches(&claude));
        assert!(!anthropic_only.matches(&gpt));
        assert!(
            !anthropic_only.matches(&llama),
            "a provider filter must not admit a local worker whose model happens to sort near it"
        );

        let local_only = filter(&["local/*"]);
        assert!(local_only.matches(&llama));
        assert!(!local_only.matches(&claude));

        // Exact, prefix, infix, and a near miss.
        assert!(filter(&["anthropic/claude-opus-4"]).matches(&claude));
        assert!(!filter(&["anthropic/claude-opus-5"]).matches(&claude));
        assert!(filter(&["anthropic/claude-*-4"]).matches(&claude));
        assert!(filter(&["*/claude-opus-4"]).matches(&claude));
        assert!(
            !filter(&["anthropic/claude"]).matches(&claude),
            "an exact pattern is exact: no implicit prefix matching"
        );

        // Any pattern in one layer admits.
        let either = filter(&["anthropic/*", "local/*"]);
        assert!(either.matches(&claude));
        assert!(either.matches(&llama));
        assert!(!either.matches(&gpt));

        // The absent filter.
        assert!(TargetFilter::allow_all().matches(&gpt));
        assert!(
            TargetFilter::parse(Vec::<String>::new())
                .unwrap()
                .matches(&gpt),
            "an empty list is an absent filter, not a filter that admits nothing"
        );
    }

    #[test]
    fn a_pattern_dialect_nobody_supports_is_refused_rather_than_taken_literally() {
        assert_eq!(
            TargetFilter::parse(["anthropic/**"]).unwrap_err(),
            FilterError::Unsupported {
                pattern: "anthropic/**".into(),
                token: "**",
            }
        );
        assert!(TargetFilter::parse(["anthropic/claude-?"]).is_err());
        assert!(TargetFilter::parse(["anthropic/{a,b}"]).is_err());
        assert_eq!(
            TargetFilter::parse([""]).unwrap_err(),
            FilterError::EmptyPattern
        );

        // The characters `canonical()` separates on, refused for a different
        // reason from the four above — not "this dialect does not support it"
        // but "this dialect cannot encode it". See the collision test below.
        for (pattern, token) in [
            ("anthropic/*,local/*", ","),
            ("anthropic/*;local/*", ";"),
            ("anthropic/ claude", " "),
            ("anthropic/claude\n", "whitespace"),
            ("anthropic/\tclaude", "whitespace"),
        ] {
            assert!(
                TargetFilter::parse([pattern]).is_err(),
                "`{pattern}` carries `{token}`, which the digest encoding uses"
            );
        }
    }

    #[test]
    fn a_pattern_may_not_carry_the_characters_the_digest_encoding_separates_on() {
        // A proven collision, not a hypothetical. `canonical()` joins the
        // patterns of a layer with "," and the layers with ";", and `parse`
        // used to accept both characters inside a pattern — so
        // `["anthropic/*,local/*"]`, one literal pattern matching *nothing*,
        // and `["anthropic/*", "local/*"]`, two patterns admitting *both*,
        // canonicalized to the identical string and fingerprinted identically
        // as `e1ce5cdcb9eb30b7`. Two policies with opposite admissible sets
        // sharing one digest is the audit trail reporting a constraint that
        // was never in force.
        //
        // Escaping in `canonical()` was the alternative and is the worse one:
        // it would keep the collision reachable through any future encoding
        // change and would have to be gotten right in a function whose output
        // is pinned by golden digest. Refusing the characters at the one
        // constructor keeps the encoding total by construction.
        for hostile in ["anthropic/*,local/*", "anthropic/*;local/*"] {
            assert!(
                TargetFilter::parse([hostile]).is_err(),
                "`{hostile}` is one pattern that encodes as two, and would collide"
            );
        }

        // The control, and it is what makes the assertion above about the
        // *separator* rather than about strictness: the same two patterns
        // written as a real layer parse, and digest apart from a filter that
        // names only one of them.
        let both = TurnPolicy {
            allow: filter(&["anthropic/*", "local/*"]),
            ..TurnPolicy::unrestricted()
        };
        let one = TurnPolicy {
            allow: filter(&["anthropic/*"]),
            ..TurnPolicy::unrestricted()
        };
        assert_ne!(both.digest(), one.digest());
        assert_eq!(
            both.digest(),
            "e1ce5cdcb9eb30b7",
            "the disputed fingerprint still belongs to this policy -- the fix \
             was to make it unreachable from the other side, not to renumber \
             every filter in every existing log"
        );
    }

    #[test]
    fn a_quality_floor_refuses_a_candidate_below_it() {
        // The default policy picks the cheapest warm option, which here is the
        // small local model. A floor above its prior must exclude it outright
        // rather than merely deprioritize it.
        let cheap = candidate(local("llama-3.1-8b"), 0.6);
        let strong = candidate(frontier("anthropic", "claude-opus-4"), 0.95);
        let policy = TurnPolicy {
            min_quality: 0.9,
            ..TurnPolicy::unrestricted()
        };
        let empty = FrontierHistory::default();

        assert!(!policy.admits(&cheap, &empty));
        assert!(policy.admits(&strong, &empty));
        assert!(
            TurnPolicy::unrestricted().admits(&cheap, &empty),
            "the control: the same candidate is admissible with no floor"
        );
    }

    #[test]
    fn an_unrestricted_policy_admits_every_candidate_whatever_the_history() {
        let spent = history(&[true; 32]);
        for quality in [0.0, 0.5, 1.0] {
            for target in [local("llama"), frontier("anthropic", "claude")] {
                assert!(
                    TurnPolicy::unrestricted().admits(&candidate(target, quality), &spent),
                    "unrestricted is the absent-policy value and must never refuse"
                );
            }
        }
    }

    #[test]
    fn a_cadence_window_exhausts_frontier_and_recovers() {
        let policy = TurnPolicy {
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 1,
                per_turns: 3,
            }),
            ..TurnPolicy::unrestricted()
        };
        let hosted = candidate(frontier("anthropic", "claude"), 0.95);
        let own = candidate(local("llama"), 0.6);

        assert!(policy.admits(&hosted, &history(&[])));
        assert!(
            policy.admits(&hosted, &history(&[false, false])),
            "local turns do not spend the ration"
        );
        assert!(
            !policy.admits(&hosted, &history(&[true])),
            "one frontier dispatch fills a window of one"
        );
        assert!(
            policy.admits(&own, &history(&[true])),
            "the cadence is a frontier knob: local stays admissible, which is what makes the turn serve rather than fail"
        );
        assert!(!policy.admits(&hosted, &history(&[true, false])));
        assert!(
            !policy.admits(&hosted, &history(&[true, false, false])),
            "three turns back is still inside a window of three"
        );
        assert!(
            policy.admits(&hosted, &history(&[true, false, false, false])),
            "the window slides: the spent dispatch has fallen out of the last three turns"
        );

        // A frontier dispatch spends the ration even when it produced nothing:
        // the history is folded from `Routed`, which is written before the
        // dispatch is attempted.
        let mut failed = FrontierHistory::default();
        failed.record(&frontier("anthropic", "claude"), 0);
        assert!(
            !policy.admits(&hosted, &failed),
            "a failed frontier dispatch still spent the ration"
        );

        // The control: with no cadence configured, none of the above binds.
        assert!(
            TurnPolicy::unrestricted().admits(&hosted, &history(&[true, true, true])),
            "no cadence means no window"
        );
    }

    /// M10 review G05: a turn that falls forward writes one `Routed` per
    /// dispatch (`Session::fold`'s comment on `frontier_history` says so
    /// explicitly), so `dispatches` can hold more entries than the session has
    /// routed turns. `frontier_in_last` reads `turns` as an index into that
    /// vector, one entry per element, with no notion of a turn boundary — so a
    /// failover turn's extra entries eat into the window exactly as if they
    /// were extra turns, and an older hosted turn ages out before `per_turns`
    /// real turns have actually elapsed.
    ///
    /// Fixed by grouping the fold per turn: `record` now takes the session's
    /// own `turn_index`, which is the only honest turn boundary available —
    /// `Routed` carries no turn id, and the fold that writes it is the one
    /// place the index is already known.
    #[test]
    fn a_failover_turn_does_not_shorten_the_cadence_window() {
        let mut history = FrontierHistory::default();

        // Turn 1: reaches the hosted model once.
        history.record(&frontier("anthropic", "claude"), 1);
        // Turn 2: local.
        history.record(&local("llama"), 2);
        // Turn 3: falls forward twice before landing locally — three
        // `Routed` events folded from one routed turn.
        history.record(&frontier("anthropic", "claude"), 3);
        history.record(&frontier("openai", "gpt-5"), 3);
        history.record(&local("llama"), 3);
        // Turns 4 through 10: local. Ten routed turns total.
        for turn in 4..=10 {
            history.record(&local("llama"), turn);
        }

        // Ten real turns have elapsed, so a ten-turn trailing window spans
        // the whole session and turn 1's hosted dispatch is still inside it:
        // 1 reach from turn 1, plus the 2 frontier attempts turn 3 made
        // before it landed locally.
        assert_eq!(
            history.frontier_in_last(10),
            3,
            "turn 1's reach must still be inside a ten-turn window when the \
             session is exactly ten turns old; the per-dispatch fold treated \
             each of turn 3's three dispatches as its own turn, so `take(10)` \
             stopped two dispatches short of turn 1 and returned 2 — turn 1 \
             had aged out three turns early"
        );

        // The other half of the same window, and the reason the fix is a
        // grouping rather than a de-duplication: turn 3's two dead hosted
        // attempts are still two reaches. A cadence rations reaching for a
        // hosted model, and a dispatch that failed on the way out reached.
        assert_eq!(
            history.frontier_in_last(8),
            2,
            "an eight-turn window spans turns 3..=10, which made two reaches \
             in one turn"
        );
    }

    #[test]
    fn permits_ignores_the_cadence_and_admits_when_spent_is_what_survives_it() {
        // The three questions, told apart on one policy. `permits` is the
        // history-independent pair, `admits` adds this session's spend, and
        // `admits_when_spent` is what is left when the ration is gone — the
        // question the startup check asks and the only one of the three a
        // caller outside this crate could not otherwise spell, since there is
        // no honest way to build a spent `FrontierHistory`.
        let policy = TurnPolicy {
            min_quality: 0.5,
            allow: filter(&["anthropic/*", "local/*"]),
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 1,
                per_turns: 3,
            }),
        };
        let hosted = candidate(frontier("anthropic", "claude"), 0.95);
        let own = candidate(local("llama"), 0.6);
        let excluded = candidate(frontier("openai", "gpt-5"), 0.99);
        let too_cheap = candidate(local("tiny"), 0.1);

        assert!(
            policy.permits(&hosted),
            "the cadence is not permits' business"
        );
        assert!(policy.permits(&own));
        assert!(!policy.permits(&excluded), "the filter is");
        assert!(!policy.permits(&too_cheap), "and so is the floor");

        assert!(policy.admits(&hosted, &history(&[])));
        assert!(
            !policy.admits(&hosted, &history(&[true])),
            "a spent window is the difference between admits and permits"
        );

        assert!(
            !policy.admits_when_spent(&hosted),
            "a spent window leaves no hosted target"
        );
        assert!(
            policy.admits_when_spent(&own),
            "and leaves every permitted local one -- which is what makes the \
             turn serve rather than fail"
        );
        assert!(!policy.admits_when_spent(&excluded));

        // With no cadence there is no window to spend, so the question
        // degenerates to `permits` rather than to "local only".
        let no_cadence = TurnPolicy {
            frontier_cadence: None,
            ..policy.clone()
        };
        assert!(
            no_cadence.admits_when_spent(&hosted),
            "nothing to spend, nothing to run out of"
        );
        assert!(!no_cadence.admits_when_spent(&excluded));
    }

    #[test]
    fn a_filter_displays_as_the_intersection_an_operator_wrote() {
        // For the startup refusal: a digest says two keys differ, never which
        // pattern was mistyped.
        assert_eq!(TargetFilter::allow_all().to_string(), "*");
        assert_eq!(filter(&["anthropic/*"]).to_string(), "anthropic/*");
        assert_eq!(
            filter(&["anthropic/*", "local/*"]).to_string(),
            "(anthropic/*|local/*)"
        );
        // Narrowing appends a layer, and every layer must admit -- so the
        // rendering has to show a conjunction and not a longer list.
        assert_eq!(
            filter(&["anthropic/*", "local/*"])
                .intersect(&filter(&["local/*"]))
                .to_string(),
            "(anthropic/*|local/*) & local/*"
        );
    }

    #[test]
    fn the_reachable_ceiling_is_the_best_a_policy_permits_and_none_when_it_permits_nothing() {
        let policy = TurnPolicy {
            min_quality: 0.5,
            allow: filter(&["anthropic/*", "local/*"]),
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 1,
                per_turns: 3,
            }),
        };
        let hosted = candidate(frontier("anthropic", "claude"), 0.95);
        let own = candidate(local("llama"), 0.6);
        let excluded = candidate(frontier("openai", "gpt-5"), 0.99);
        let too_cheap = candidate(local("tiny"), 0.1);

        assert_eq!(
            policy.reachable_quality_ceiling(&[
                own.clone(),
                hosted.clone(),
                excluded.clone(),
                too_cheap.clone()
            ]),
            Some(0.95),
            "the best of what is permitted, and the 0.99 the filter excludes is \
             not a candidate an escalation could ever be clamped onto"
        );
        // The cadence is deliberately not consulted: a spent ration makes a
        // model unavailable this turn, not unreachable for the session, and a
        // ceiling that fell when the window filled would silently un-escalate.
        assert_eq!(
            policy.reachable_quality_ceiling(std::slice::from_ref(&hosted)),
            Some(0.95)
        );
        assert_eq!(
            policy.reachable_quality_ceiling(std::slice::from_ref(&own)),
            Some(0.6),
            "a modest pool has a modest ceiling, which is the whole point: the \
             clamp lands here rather than emptying the set"
        );

        // The refusal that must stay a refusal. `None` and `Some(0.0)` are
        // opposite answers, and a caller that read one as the other would route
        // a turn its key admits nowhere.
        assert_eq!(
            policy.reachable_quality_ceiling(&[excluded, too_cheap]),
            None
        );
        assert_eq!(policy.reachable_quality_ceiling(&[]), None);
    }

    #[test]
    fn narrow_can_only_tighten() {
        let base = TurnPolicy {
            min_quality: 0.5,
            allow: filter(&["anthropic/*", "local/*"]),
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 2,
                per_turns: 4,
            }),
        };
        let empty = FrontierHistory::default();

        // Probe: every axis tightened.
        let tightened = base.narrow(&PolicyOverrides {
            min_quality: Some(0.8),
            allow: Some(filter(&["anthropic/*"])),
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 1,
                per_turns: 4,
            }),
        });
        assert_eq!(tightened.min_quality, 0.8);
        assert_eq!(
            tightened.frontier_cadence,
            Some(FrontierCadence {
                max_frontier: 1,
                per_turns: 4
            })
        );
        assert!(!tightened.allow.matches(&local("llama")));
        assert!(tightened.allow.matches(&frontier("anthropic", "claude")));

        // Control: every axis widened, every axis clamped to the base.
        let widened = base.narrow(&PolicyOverrides {
            min_quality: Some(0.1),
            allow: Some(filter(&["*"])),
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 4,
                per_turns: 4,
            }),
        });
        assert_eq!(
            widened.min_quality, 0.5,
            "a lower floor is not a narrowing, so the base floor stands"
        );
        assert_eq!(
            widened.frontier_cadence, base.frontier_cadence,
            "a looser cadence is not a narrowing"
        );
        assert!(
            !widened.allow.matches(&frontier("openai", "gpt-5")),
            "`*` layered onto a filter cannot re-admit what the filter excluded"
        );

        // An overlay that touches nothing is the identity.
        let untouched = base.narrow(&PolicyOverrides::default());
        assert_eq!(untouched.min_quality, base.min_quality);
        assert_eq!(untouched.frontier_cadence, base.frontier_cadence);
        assert!(untouched.allow.matches(&local("llama")));
        assert!(!untouched.allow.matches(&frontier("openai", "gpt-5")));

        // Narrowing an unrestricted base is how a project with no policy still
        // gets its key's overrides.
        let from_nothing = TurnPolicy::unrestricted().narrow(&PolicyOverrides {
            min_quality: Some(0.7),
            allow: Some(filter(&["local/*"])),
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 1,
                per_turns: 10,
            }),
        });
        assert_eq!(from_nothing.min_quality, 0.7);
        assert!(!from_nothing.admits(&candidate(frontier("anthropic", "claude"), 0.95), &empty));

        // Equal-rate cadences are ordered by window: the burst-limiting one wins.
        let bursty = TurnPolicy {
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 5,
                per_turns: 10,
            }),
            ..TurnPolicy::unrestricted()
        };
        assert_eq!(
            bursty
                .narrow(&PolicyOverrides {
                    frontier_cadence: Some(FrontierCadence {
                        max_frontier: 1,
                        per_turns: 2
                    }),
                    ..PolicyOverrides::default()
                })
                .frontier_cadence,
            Some(FrontierCadence {
                max_frontier: 1,
                per_turns: 2
            }),
            "same long-run rate, but only one of them forbids a burst"
        );
    }

    #[test]
    fn a_widening_override_is_named_so_a_config_boundary_can_refuse_it() {
        let base = TurnPolicy {
            min_quality: 0.5,
            allow: filter(&["anthropic/*"]),
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 1,
                per_turns: 4,
            }),
        };

        assert_eq!(
            base.widenings_of(&PolicyOverrides {
                min_quality: Some(0.1),
                allow: Some(filter(&["*"])),
                frontier_cadence: Some(FrontierCadence {
                    max_frontier: 3,
                    per_turns: 4
                }),
            }),
            vec!["min_quality", "frontier_cadence"],
            "the allow axis cannot widen, so it is never reported"
        );
        assert!(
            base.widenings_of(&PolicyOverrides {
                min_quality: Some(0.5),
                allow: Some(filter(&["*"])),
                frontier_cadence: Some(FrontierCadence {
                    max_frontier: 1,
                    per_turns: 4
                }),
            })
            .is_empty(),
            "an override equal to the base narrows nothing but widens nothing either"
        );
        assert!(
            TurnPolicy::unrestricted()
                .widenings_of(&PolicyOverrides {
                    min_quality: Some(0.9),
                    frontier_cadence: Some(FrontierCadence {
                        max_frontier: 1,
                        per_turns: 100
                    }),
                    ..PolicyOverrides::default()
                })
                .is_empty(),
            "anything narrows an unrestricted base"
        );
    }

    #[test]
    fn the_policy_digest_is_deterministic() {
        let policy = TurnPolicy {
            min_quality: 0.85,
            allow: filter(&["anthropic/*", "local/*"]),
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 1,
                per_turns: 4,
            }),
        };
        let digest = policy.digest();
        assert!(!digest.is_empty());
        assert_eq!(digest, policy.clone().digest(), "same policy, same digest");

        // Order-independent axes are canonicalized: an operator listing the
        // same alternatives the other way round has not changed the policy.
        let reordered = TurnPolicy {
            allow: filter(&["local/*", "anthropic/*"]),
            ..policy.clone()
        };
        assert_eq!(reordered.digest(), digest);

        // And every axis is actually in the fingerprint.
        for different in [
            TurnPolicy {
                min_quality: 0.86,
                ..policy.clone()
            },
            TurnPolicy {
                allow: filter(&["anthropic/*"]),
                ..policy.clone()
            },
            TurnPolicy {
                frontier_cadence: None,
                ..policy.clone()
            },
        ] {
            assert_ne!(
                different.digest(),
                digest,
                "a policy change has to move the digest, or the audit trail cannot see it"
            );
        }
        assert_ne!(
            TurnPolicy::unrestricted().digest(),
            String::new(),
            "the unrestricted policy has a real digest; the empty string is reserved for pre-M2 logs"
        );
    }

    #[test]
    fn the_digest_of_two_known_policies_is_pinned_to_a_literal() {
        // The determinism claim, finally with a test that can fail. Everything
        // in `the_policy_digest_is_deterministic` compares one run of
        // `digest()` against another run of `digest()`, so it holds just as
        // well after a change to `canonical()` that renumbers every policy in
        // every log this deployment has ever written. These two literals are
        // the thing that notices.
        //
        // **When this fails, the encoding changed.** That is not a test to
        // update: bump `DIGEST_VERSION` so old and new fingerprints are
        // distinguishable in a log that spans the change, and then update
        // these literals to match. Editing the literal alone silently merges
        // two encodings under one version.
        assert_eq!(
            TurnPolicy::unrestricted().digest(),
            "4ec325a715649c8e",
            "the policy every open-mode deployment routes under"
        );
        assert_eq!(
            TurnPolicy {
                min_quality: 0.85,
                allow: filter(&["anthropic/*", "local/*"]),
                frontier_cadence: Some(FrontierCadence {
                    max_frontier: 1,
                    per_turns: 4,
                }),
            }
            .digest(),
            "5719acfed66d8f90",
            "every axis populated, so every axis is pinned"
        );
        assert!(
            TurnPolicy::unrestricted().digest().len() == 16,
            "a quarter of the hash, hex-encoded"
        );
    }

    #[test]
    fn two_spellings_of_one_entitlement_compare_equal_while_still_fingerprinting_apart() {
        // The two questions kept separate, in one test because keeping them
        // separate is the whole claim. A digest answers "was this written the
        // same way" and is stamped on every `DecisionRecord`; `admits_the_same_as`
        // answers "may these two keys do the same things" and is what
        // `ControlPlane::membership` compares before refusing to say what a
        // membership may do. Conflating them made a secret rotation — where an
        // operator copies the policy already in force onto the new key — read as
        // two keys with different entitlements and stopped the boot.

        // A project's filter, and a key that restates it: `narrow` appends a
        // layer, so the restatement is two identical layers where the inheriting
        // key has one.
        let inherited = TurnPolicy {
            allow: filter(&["local/*"]),
            ..TurnPolicy::unrestricted()
        };
        let restated = inherited.narrow(&PolicyOverrides {
            allow: Some(filter(&["local/*"])),
            ..PolicyOverrides::default()
        });
        assert!(
            inherited.admits_the_same_as(&restated),
            "a layer conjoined with itself admits exactly what it admitted"
        );
        // The control that makes the assertion above about *meaning*: the two
        // still fingerprint apart, because they were written differently and the
        // audit trail is entitled to say so. A comparison implemented by
        // teaching `canonical` to dedupe would have renumbered every policy in
        // every existing log.
        assert_ne!(inherited.digest(), restated.digest());
        assert_eq!(
            inherited.digest(),
            "96cd6326af5e364b",
            "pinned so that the tempting shortcut fails here: teaching \
             `canonical` to dedupe would make the two spellings above compare \
             equal *and* renumber this filter in every log that already carries \
             it. See `the_digest_of_two_known_policies_is_pinned_to_a_literal` \
             on what a moved fingerprint costs"
        );

        // A layer naming `*` constrains nothing, so a key that spells out "may
        // reach everything" agrees with a project that narrowed nothing.
        assert!(TurnPolicy::unrestricted().admits_the_same_as(&TurnPolicy {
            allow: filter(&["*"]),
            ..TurnPolicy::unrestricted()
        }));

        // The controls: policies that really differ, on each axis, must not
        // compare equal — or the check would be satisfied by returning `true`.
        for different in [
            TurnPolicy {
                allow: filter(&["local/*"]),
                ..TurnPolicy::unrestricted()
            },
            TurnPolicy {
                min_quality: 0.9,
                ..TurnPolicy::unrestricted()
            },
            TurnPolicy {
                frontier_cadence: Some(FrontierCadence {
                    max_frontier: 1,
                    per_turns: 4,
                }),
                ..TurnPolicy::unrestricted()
            },
        ] {
            assert!(
                !TurnPolicy::unrestricted().admits_the_same_as(&different),
                "{different:?} admits a different set from the unrestricted policy"
            );
        }

        // And nothing subtler is folded: subsumption inside a layer is left
        // alone, so a comparison that cannot prove agreement reports
        // disagreement — the arm an operator can fix by hand.
        assert!(
            !TurnPolicy {
                allow: filter(&["local/*"]),
                ..TurnPolicy::unrestricted()
            }
            .admits_the_same_as(&TurnPolicy {
                allow: filter(&["local/*", "local/llama"]),
                ..TurnPolicy::unrestricted()
            }),
            "conservative on anything it would have to reason about globs to \
             decide"
        );
    }
}
