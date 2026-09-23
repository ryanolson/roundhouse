// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which routing decisions one frontier review covered, and what it said about
//! them.
//!
//! A review is a quality observation only for the decisions it actually saw.
//! The classic brief is a bounded summary — truncated instructions, a trailing
//! window of compacted steps — so a verdict over it says nothing exact about
//! any particular decision. This module adds a second, separately bounded
//! section that renders every turn since the previous review in full, and
//! records the exact decision set that section covered.
//!
//! **Complete or absent, never partial.** The section is left out whole, and
//! the interval is labelled unknown, in these cases:
//!
//! - the section is larger than its cap
//! - the section holds content that it cannot represent
//! - the section holds roundhouse's own control traffic, which the judge must
//!   not see
//!
//! A label for the part of an interval that fit is feedback about decisions
//! that the judge never saw. A label for an interval with a hole where a
//! control result was is the same error.
//!
//! **The label comes from the verdict, never from the action.** `map` turns an
//! off-track verdict into `Continue` under several policies, and the Shadow arm
//! discards every action, so what was delivered says nothing about what the
//! judge concluded.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ids::ResponseId;
use crate::item::{Item, ItemContent};
use crate::session::{IntervalFacts, SessionState, TurnEnd, TurnState};
use crate::validate::brief::{LINE_BREAK_MARK, QUOTE, one_line_len, quote, quoted_len};
use crate::validate::{
    ControlCallDialect, Objective, PROMPT_SEPARATOR, Verdict, is_control_call_on,
};

/// The revision of the coverage rules and the label rule.
///
/// Bumped when anything that decides a label changes: the gap derivation, the
/// fold's tracking bounds, or [`label_for`]. The fold re-derives both only for
/// reviews written under this revision, so an older review keeps the label it
/// was written with instead of being re-judged by a later build.
pub const REVIEW_RULE_REVISION: u32 = 1;

/// The default byte bound on the reviewed-turns section.
///
/// Conservative rather than measured: large enough for a short interval of
/// ordinary turns with a modest instruction block, small enough that one review
/// cannot grow the judge prompt without limit. Deployments set their own value
/// through [`ValidatorConfig`](crate::validate::ValidatorConfig).
pub const DEFAULT_INTERVAL_SECTION_BYTES: usize = 64 * 1024;

/// The heading the section opens with.
pub const INTERVAL_SECTION_HEADING: &str = "## Reviewed turns";

/// One routing decision a review covered.
///
/// Identified by the sequence of its `Routed` event, because a failover writes
/// several of those within one turn and each is a separate decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewedDecision {
    pub routed_seq: u64,
    pub turn_index: u64,
    pub response_id: ResponseId,
}

/// Why a review's coverage cannot support a quality label.
///
/// At least one of these is recorded whenever the section was omitted. The
/// list is not exhaustive: once one gap is known the section is not rendered,
/// so content checks further down never run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageGap {
    /// No decision fell inside the interval, so there is nothing to label.
    NoDecisions,
    /// The session's tracking bound was exceeded and part of the interval was
    /// not retained.
    MetadataOverflow,
    /// A covered decision's turn has no terminal event, so its output is not
    /// durable as items and cannot be shown.
    UnterminatedTurn,
    /// A covered decision recorded no objective version.
    VersionsUnavailable,
    /// Covered decisions ran under different instructions.
    InstructionsChanged,
    /// Covered decisions ran under different objectives, or under one other
    /// than the objective this review shows.
    ObjectiveChanged,
    /// Encrypted reasoning or an opaque block the section cannot render.
    UnrepresentableContent,
    /// A call to one of roundhouse's own control tools, or its result. The
    /// judge never sees their contents, because a control result can name the
    /// chosen target and its price.
    WithheldControlTraffic,
    /// The complete section exceeded its configured bound.
    Oversized,
}

/// What a review says about the decisions it covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntervalLabel {
    /// A complete review found the run on track.
    Positive,
    /// A complete review found the run off track. The covered decisions share
    /// this label; it does not claim each of them caused the problem.
    Negative,
    /// Coverage was incomplete, the judge reported missing context, or nothing
    /// was covered.
    Unknown,
}

/// Which objective a decision was made under.
///
/// Stamped on each decision because a declared objective lives in a node-local
/// store and silently falls back after a restart; comparing only the objective
/// shown at review time would miss a change in between.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObjectiveVersion {
    /// The agent declared an objective; the digest covers all of its text.
    Declared { digest: String },
    /// Nothing was declared, so the conversation's own requests stand in. The
    /// section renders those requests rather than a digest of one of them.
    Undeclared,
}

impl ObjectiveVersion {
    /// The version of `objective`.
    ///
    /// Each field is length-framed, so text moved from one field to another is
    /// a different objective rather than the same bytes.
    pub fn of(objective: &Objective) -> Self {
        let Objective::Declared {
            goal,
            plan_steps,
            done_when,
        } = objective
        else {
            return ObjectiveVersion::Undeclared;
        };
        let mut hasher = Sha256::new();
        let mut field = |text: &str| {
            hasher.update((text.len() as u64).to_le_bytes());
            hasher.update(text.as_bytes());
        };
        field(goal);
        field(&plan_steps.len().to_string());
        for step in plan_steps {
            field(step);
        }
        field(done_when);
        ObjectiveVersion::Declared {
            digest: hex::encode(hasher.finalize()),
        }
    }
}

/// One review's coverage and label, recorded on the `Judged` outcome.
///
/// Carries no target, price or routing rationale: decisions are named by log
/// position alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntervalReview {
    /// The [`REVIEW_RULE_REVISION`] this review was written under.
    pub rule_revision: u32,
    /// The previous checkpoint, exclusive.
    pub after_seq: u64,
    /// The last log sequence the review could see, captured before the call.
    pub through_seq: u64,
    /// Exactly the decisions in `(after_seq, through_seq]`, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<ReviewedDecision>,
    /// Empty when the section rendered completely.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<CoverageGap>,
    /// SHA-256 of the prepared judge prompt, as hex.
    pub prompt_digest: String,
    pub label: IntervalLabel,
}

/// The label a verdict gives an interval with `gaps`.
///
/// A judge that says it lacked context has not reviewed the interval, whatever
/// it concluded. Confidence gates nothing, as everywhere else in the loop.
pub fn label_for(gaps: &[CoverageGap], verdict: &Verdict) -> IntervalLabel {
    if !gaps.is_empty() || verdict.missing_context.is_some() {
        IntervalLabel::Unknown
    } else if verdict.on_track {
        IntervalLabel::Positive
    } else {
        IntervalLabel::Negative
    }
}

/// SHA-256 of the prompt a judge transport prepares from `system_prompt` and
/// `brief`, as hex.
///
/// Hashed over the same join the transport sends, so the digest names the
/// bytes the judge received and not a framing only this function uses.
pub fn prompt_digest(system_prompt: &str, brief: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(system_prompt.as_bytes());
    hasher.update(PROMPT_SEPARATOR.as_bytes());
    hasher.update(brief.as_bytes());
    hex::encode(hasher.finalize())
}

/// A review's coverage, captured before the judge is called.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IntervalCapture {
    after_seq: u64,
    through_seq: u64,
    decisions: Vec<ReviewedDecision>,
    gaps: Vec<CoverageGap>,
    /// The complete section, or `None` when any gap is recorded.
    pub(crate) section: Option<String>,
}

impl IntervalCapture {
    /// Capture `state`'s open interval, rendering the section within `limit`.
    ///
    /// `objective` is the one the review shows. Every covered decision must
    /// have recorded it, or the judge would be shown an objective those
    /// decisions were not made under. `dialect` is how this session's client
    /// spells roundhouse's own control calls. The capture does not show an
    /// interval that holds one.
    pub(crate) fn of(
        state: &SessionState,
        objective: &Objective,
        dialect: ControlCallDialect,
        limit: usize,
    ) -> Self {
        let facts = state.review_interval();
        let mut gaps = facts.gaps.clone();
        if gaps.is_empty()
            && facts
                .objective
                .is_some_and(|stamp| *stamp != ObjectiveVersion::of(objective))
        {
            gaps.push(CoverageGap::ObjectiveChanged);
        }
        let section = match gaps.is_empty() {
            true => match render_section(&facts, objective, dialect, limit) {
                Ok(section) => Some(section),
                Err(gap) => {
                    gaps.push(gap);
                    None
                }
            },
            // Nothing a section could show would make the label known.
            false => None,
        };
        IntervalCapture {
            after_seq: facts.after_seq,
            through_seq: state.last_seq,
            decisions: facts.decisions,
            gaps,
            section,
        }
    }

    /// Attach the verdict to the snapshot taken before the call.
    pub(crate) fn into_review(self, prompt_digest: String, verdict: &Verdict) -> IntervalReview {
        let label = label_for(&self.gaps, verdict);
        IntervalReview {
            rule_revision: REVIEW_RULE_REVISION,
            after_seq: self.after_seq,
            through_seq: self.through_seq,
            decisions: self.decisions,
            gaps: self.gaps,
            prompt_digest,
            label,
        }
    }
}

/// Render the section, or say why it cannot be shown whole.
///
/// Counted first and written second, so nothing is allocated for a section
/// that would exceed `limit` and the one allocation made is exact.
///
/// **The count comes before anything else that reads the items.** The fold
/// bounds an interval by turns and decisions, not by items, so one turn can
/// hold any number of tool calls. `limit` is the only bound on those calls.
/// Work over every item before the count is work that `limit` does not bound.
fn render_section(
    facts: &IntervalFacts<'_>,
    objective: &Objective,
    dialect: ControlCallDialect,
    limit: usize,
) -> Result<String, CoverageGap> {
    let mut count = Sink::Count { used: 0, limit };
    write_section(&mut count, facts, objective)?;
    let Sink::Count { used, .. } = count else {
        unreachable!("counted above")
    };
    if carries_control_traffic(facts, dialect) {
        return Err(CoverageGap::WithheldControlTraffic);
    }
    let mut out = String::with_capacity(used);
    write_section(&mut Sink::Write(&mut out), facts, objective)?;
    Ok(out)
}

/// Whether the rendered turns hold a call to one of roundhouse's own control
/// tools, or the result of one.
///
/// **Such an interval cannot be labelled.** A control result can carry the
/// chosen target, its rationale and the policy digest, and `explain_last_route`
/// returns exactly those. A routing fact must never reach the judge, so the
/// judge cannot see these contents. A verdict over an interval with hidden
/// contents is not a review of that interval. So the section is left out and
/// the label is unknown.
///
/// A result pairs with the latest earlier call that has its id, as
/// [`exchanges`](crate::validate::exchanges) pairs them. This function
/// examines a call inside the turns before it gets to the result of that call.
/// So only a result whose call came before the turns needs the earlier
/// history. All such results share one backward walk. The walk stops when it
/// finds every call.
///
/// The caller runs this only after the section fits. So `limit` bounds the ids
/// that it collects. The backward walk allocates nothing. It reaches the start
/// of the session only for a result that no call answers.
fn carries_control_traffic(facts: &IntervalFacts<'_>, dialect: ControlCallDialect) -> bool {
    // Calls inside the turns so far. None of them is ours, because a call that
    // is ours ends the function.
    let mut calls: HashSet<&str> = HashSet::new();
    let mut earlier: HashSet<&str> = HashSet::new();
    for item in facts.turns.iter().flat_map(|turn| turn.items) {
        match &item.content {
            ItemContent::ToolCall {
                call_id,
                name,
                namespace,
                ..
            } => {
                if is_control_call_on(name, namespace.as_deref(), dialect) {
                    return true;
                }
                calls.insert(call_id);
            }
            ItemContent::ToolResult { call_id, .. } if !calls.contains(call_id.as_str()) => {
                earlier.insert(call_id);
            }
            _ => {}
        }
    }
    for item in facts.prefix.iter().rev() {
        if earlier.is_empty() {
            break;
        }
        if let ItemContent::ToolCall {
            call_id,
            name,
            namespace,
            ..
        } = &item.content
            && earlier.remove(call_id.as_str())
            && is_control_call_on(name, namespace.as_deref(), dialect)
        {
            return true;
        }
    }
    false
}

/// Where a rendering pass goes.
enum Sink<'a> {
    Count { used: usize, limit: usize },
    Write(&'a mut String),
}

impl Sink<'_> {
    fn add(&mut self, bytes: usize) -> Result<(), CoverageGap> {
        if let Sink::Count { used, limit } = self {
            *used = used.saturating_add(bytes);
            if *used > *limit {
                return Err(CoverageGap::Oversized);
            }
        }
        Ok(())
    }

    /// Whether `bytes` more could still fit, without counting them.
    fn fits(&self, bytes: usize) -> Result<(), CoverageGap> {
        match self {
            Sink::Count { used, limit } if used.saturating_add(bytes) > *limit => {
                Err(CoverageGap::Oversized)
            }
            _ => Ok(()),
        }
    }

    /// Roundhouse's own words.
    fn text(&mut self, text: &str) -> Result<(), CoverageGap> {
        self.add(text.len())?;
        if let Sink::Write(out) = self {
            out.push_str(text);
        }
        Ok(())
    }

    /// A transcript span inside a line roundhouse wrote, with its line breaks
    /// made visible, exactly as the classic brief flattens one.
    fn flat(&mut self, text: &str) -> Result<(), CoverageGap> {
        self.add(one_line_len(text))?;
        if let Sink::Write(out) = self {
            for c in text.chars() {
                match c {
                    '\n' | '\r' => out.push_str(LINE_BREAK_MARK),
                    c => out.push(c),
                }
            }
        }
        Ok(())
    }

    /// A transcript block, every line quoted so none can open a heading.
    fn quote(&mut self, text: &str) -> Result<(), CoverageGap> {
        self.add(quoted_len(text, QUOTE))?;
        if let Sink::Write(out) = self {
            quote(text, QUOTE, out);
        }
        Ok(())
    }
}

fn write_section(
    sink: &mut Sink<'_>,
    facts: &IntervalFacts<'_>,
    objective: &Objective,
) -> Result<(), CoverageGap> {
    sink.text("\n")?;
    sink.text(INTERVAL_SECTION_HEADING)?;
    sink.text(
        "\nEvery turn since the previous review, in full and in order. Your \
         verdict applies to all of them together.\n",
    )?;

    sink.text("\n### Instructions\n")?;
    match facts.instructions {
        Some(items) if !items.is_empty() => {
            for item in items {
                match &item.content {
                    ItemContent::Text { text } => sink.quote(text)?,
                    _ => return Err(CoverageGap::UnrepresentableContent),
                }
            }
        }
        _ => sink.text("(none given)\n")?,
    }

    sink.text("\n### Objective\n")?;
    match objective {
        Objective::Declared {
            goal,
            plan_steps,
            done_when,
        } => {
            sink.quote(goal)?;
            for (index, step) in plan_steps.iter().enumerate() {
                // Bounded before the step is copied into its numbered line.
                sink.fits(step.len())?;
                sink.quote(&format!("{}. {step}", index + 1))?;
            }
            sink.fits(done_when.len())?;
            sink.quote(&format!("Done when: {done_when}"))?;
        }
        Objective::LastUserMessage(_) | Objective::Unknown => {
            sink.text("(not stated; the requests in these turns stand in for it)\n")?;
            if let Some(text) = facts.request_before {
                sink.text("The most recent request before these turns:\n")?;
                sink.quote(text)?;
            }
        }
    }

    for (index, turn) in facts.turns.iter().enumerate() {
        sink.text("\n### Turn T")?;
        sink.text(&(index + 1).to_string())?;
        sink.text(match turn.state {
            TurnState::Ended(TurnEnd::Completed) => " (answered)\n",
            TurnState::Ended(TurnEnd::Incomplete) => " (ended without a complete answer)\n",
            TurnState::InProgress => " (in progress)\n",
            TurnState::Abandoned => " (abandoned without an answer)\n",
        })?;
        for item in turn.items {
            write_item(sink, item)?;
        }
    }
    Ok(())
}

fn write_item(sink: &mut Sink<'_>, item: &Item) -> Result<(), CoverageGap> {
    let role = item.role.as_str();
    match &item.content {
        ItemContent::Text { text } => {
            sink.text(role)?;
            sink.text(":\n")?;
            sink.quote(text)
        }
        ItemContent::ToolCall {
            call_id,
            name,
            arguments,
            ..
        } => {
            sink.text(role)?;
            sink.text(" called `")?;
            sink.flat(name)?;
            sink.text("` (call `")?;
            sink.flat(call_id)?;
            sink.text("`):\n")?;
            sink.quote(arguments)
        }
        ItemContent::ToolResult { call_id, output } => {
            sink.text("result of call `")?;
            sink.flat(call_id)?;
            sink.text("`:\n")?;
            sink.quote(output)
        }
        // Plain reasoning text is shown; its signature is an opaque token that
        // says nothing a reviewer can read, so it is not.
        ItemContent::Thinking { thinking, .. } => {
            sink.text(role)?;
            sink.text(" reasoning:\n")?;
            sink.quote(thinking)
        }
        // Encrypted reasoning and blocks this build does not model cannot be
        // shown, and a review that skipped them would not be complete.
        ItemContent::RedactedThinking { .. } | ItemContent::Opaque { .. } => {
            Err(CoverageGap::UnrepresentableContent)
        }
    }
}
