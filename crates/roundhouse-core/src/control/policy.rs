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

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::routing::{Candidate, Target};

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
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "Vec<String>")]
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
                for token in ["**", "?", "[", "{"] {
                    if pattern.contains(token) {
                        return Err(FilterError::Unsupported { pattern, token });
                    }
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
}

/// Match `value` against a pattern whose only metacharacter is `*`.
///
/// The textbook two-pointer walk with one backtrack point rather than a
/// regex: it is linear in practice, allocation-free, and — the reason that
/// matters here — it has no dialect. A regex crate would silently give `.`,
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

impl TryFrom<Vec<String>> for TargetFilter {
    type Error = FilterError;

    fn try_from(patterns: Vec<String>) -> Result<Self, Self::Error> {
        Self::parse(patterns)
    }
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
/// **The counting basis is dispatch, not success.** A [`Routed`] event whose
/// target was a frontier model spends a ration even if the dispatch then
/// failed. The alternative — count only turns that completed — makes a
/// provider outage silently multiply the frontier spend of every session
/// retrying through it, which is exactly when a cadence knob is supposed to
/// hold.
///
/// [`Routed`]: crate::event::SessionEventKind::Routed
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

/// Frontier dispatches per routed turn, in log order.
///
/// A projection of the session's [`Routed`] events rather than a counter
/// incremented beside them: a successor process that replays a log has to
/// arrive at the same window as the process it replaced, and a counter that
/// lived anywhere but in the fold would not. One `bool` per routed turn, which
/// is strictly smaller than the conversation item that turn already put in the
/// same projection.
///
/// [`Routed`]: crate::event::SessionEventKind::Routed
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrontierHistory {
    dispatches: Vec<bool>,
}

impl FrontierHistory {
    /// Fold one routing decision in.
    ///
    /// Crate-visible: the only honest producer is the session projection, and
    /// a history assembled by hand somewhere else would be a second answer to
    /// a question the log already answers.
    pub(crate) fn record(&mut self, target: &Target) {
        self.dispatches.push(!target.is_local());
    }

    /// Frontier dispatches among the last `turns` routed turns.
    ///
    /// A window longer than the session simply sees the whole session, which
    /// is the right reading: a cadence of "two per twenty" on a session five
    /// turns old has spent whatever those five turns spent.
    pub fn frontier_in_last(&self, turns: u32) -> u32 {
        let window = (turns as usize).min(self.dispatches.len());
        self.dispatches[self.dispatches.len() - window..]
            .iter()
            .filter(|to_frontier| **to_frontier)
            .count() as u32
    }
}

/// What a principal's turns may do, resolved once at admission.
///
/// Immutable for the turn. Everything that consults it does so through
/// [`Self::admits`], and the candidate set is filtered by the same policy
/// before the router ever sees it — see the note there on why both exist.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
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

impl Default for TurnPolicy {
    fn default() -> Self {
        Self::unrestricted()
    }
}

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

    /// Whether `candidate` is one this principal may be routed to.
    ///
    /// **The one admissibility question in the system.** Every
    /// [`RoutingPolicy`](crate::routing::RoutingPolicy) consults this and none
    /// re-derives it, including the escalation audit branch, which would
    /// otherwise escalate straight past a quality floor it never asked about.
    ///
    /// A `bool` rather than a reason: nothing downstream branches on *why* a
    /// candidate was refused, and a reason nobody reads is a field that goes
    /// stale.
    pub fn admits(&self, candidate: &Candidate, history: &FrontierHistory) -> bool {
        if candidate.quality_prior < self.min_quality {
            return false;
        }
        if !self.allow.matches(&candidate.target) {
            return false;
        }
        // The cadence is a frontier knob and only a frontier knob. That
        // asymmetry is the whole behavior: when the window is spent the hosted
        // options go inadmissible and the local ones do not, so the turn
        // *serves*, locally, instead of failing. Contrast an `allow` filter
        // that matches nothing, which leaves no admissible candidate at all
        // and must fail the turn — a filter that quietly routed everything
        // local would look exactly like a cost win.
        if !candidate.target.is_local()
            && let Some(cadence) = &self.frontier_cadence
            && !cadence.admits_another(history)
        {
            return false;
        }
        true
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
            "v1\nmin_quality={:016x}\nallow={}\ncadence={cadence}\n",
            floor.to_bits(),
            self.allow.canonical(),
        );
        let full = Sha256::digest(canonical.as_bytes());
        // Half the hash: this is a fingerprint an operator reads off a log
        // line beside a decision, not a signature. 64 bits is far past the
        // point where two policies in one deployment collide.
        hex::encode(&full[..8])
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
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
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

    fn history(dispatches: &[bool]) -> FrontierHistory {
        let mut history = FrontierHistory::default();
        for &to_frontier in dispatches {
            history.record(&if to_frontier {
                frontier("anthropic", "claude")
            } else {
                local("llama")
            });
        }
        history
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
        failed.record(&frontier("anthropic", "claude"));
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
}
