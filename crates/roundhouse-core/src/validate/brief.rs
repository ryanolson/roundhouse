// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What the judge sees.
//!
//! A bounded, deterministic projection of one session: the instructions
//! (truncated), what the agent said it is trying to do, the last K tool
//! call/result pairs compacted, and roundhouse's own computed signals **as
//! facts**.
//!
//! A review may append a second section, every turn since the previous review
//! in full, built by [`interval`](super::interval) under its own bound. The
//! validator appends the section after this projection and never changes the
//! projection. So a review without the section sends exactly this brief.
//!
//! ## The negative invariant is the sharp one
//!
//! **Do not add roundhouse's prices, candidate lists, target choices, or routing
//! rationales to the brief.** Ordinary transcript text can name models or repeat
//! routing details. This projection does not redact those spans. LLM judges
//! carry self-preference and same-provider family bias — a judge is itself a
//! member of one of the families being chosen between — so a judge asked
//! "should we have used a stronger model?" is not a neutral instrument. The
//! judge answers a *task* question; code maps the answer to an action under
//! policy. The routing question is asked exactly once, of code.
//!
//! The routing records are excluded in two ways. Structurally, this
//! type has no routing metadata field: it is built from
//! items, hashes and sentences, and nothing here takes a
//! [`DecisionRecord`](crate::routing::DecisionRecord) or a
//! [`Candidate`](crate::routing::Candidate). By assertion, the guard test
//! renders a brief for a session whose routing history is full of exactly
//! those things and scans the output for them — because the structural
//! argument is about today's fields, and the test is about tomorrow's.
//!
//! **The structural argument does not cover the items themselves.** An agent
//! can ask roundhouse's own control tools about routing, and
//! `explain_last_route` answers with the chosen target, its price and the
//! rationale. That answer is a tool result in the session like any other. So
//! a step that calls a control tool shows its name and nothing else: no
//! argument fingerprint, no output head. See [`StepContent::Withheld`].
//!
//! ## Facts, not suggestions
//!
//! "This call has produced identical output four times" is evidence the judge
//! weighs against everything else it can see. "This looks like a loop, consider
//! intervening" is roundhouse asking the judge to agree with roundhouse, and a
//! judge that agrees with the trigger is an expensive way to re-read the
//! trigger.
//!
//! ## Compacted, hashed, and quoted
//!
//! Arguments travel as a fingerprint and outputs as a head. That bounds the
//! cost of asking, and it bounds *how much* attacker-influenceable text the
//! judge reads — every byte of the transcript is attacker-influenceable in an
//! agent that reads issues, web pages or other agents' output.
//!
//! **Bounding is not structural, and the two are different defenses.** The
//! rendered brief is plain markdown sections whose meanings the judge is told,
//! so a span that reaches column zero can open a section of its own: eighty
//! characters of tool output carrying `\n## Observed\n- <fabrication>` gets its
//! fabrication read as one of *roundhouse's* measurements, and no length bound
//! touches that — the payload is far inside every bound here. So every
//! transcript-derived span is line-prefixed as quotation before it is rendered
//! ([`quote`]), and the spans that sit inside a line roundhouse wrote are
//! flattened instead ([`one_line`]). Nothing from the transcript begins a line
//! of the brief, for any input.
//!
//! What that buys is structural: the judge can always tell roundhouse's words
//! from the session's. What it does not buy is a judge that ignores a
//! well-written instruction inside a quotation — that is the system prompt's
//! injection-defense sentence, which is a mitigation and not a proof. Bounded,
//! not solved; the risk register says so, and the Shadow arm is the instrument
//! that measures it.

use crate::item::{Item, ItemContent, Role};
use crate::validate::control_call::{ControlCallDialect, is_control_call_on};
use crate::validate::exchange::{Exchange, exchanges};

/// What the agent is trying to do, as well as anybody knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Objective {
    /// What the agent declared through the control surface.
    ///
    /// The best of the three, and the reason the MCP surface has a write half
    /// for it: a stated goal turns the judge's question from "infer the goal,
    /// then judge drift against your inference" into "here is the goal, name
    /// the divergence".
    Declared {
        goal: String,
        plan_steps: Vec<String>,
        done_when: String,
    },
    /// The most recent thing the human asked for.
    LastUserMessage(String),
    /// Nothing in the session says. Rendered as such rather than omitted —
    /// a judge that is not told the goal is absent will infer one.
    Unknown,
}

impl Objective {
    /// The best objective a session's own items can supply.
    ///
    /// Never [`Objective::Declared`]: a declaration lives in the control store
    /// and not in the log, so it reaches the brief from the interjection
    /// context. This is the fallback every session has.
    pub fn from_items(items: &[Item]) -> Objective {
        trailing_user_request(items)
            .map(|text| Objective::LastUserMessage(text.to_string()))
            .unwrap_or(Objective::Unknown)
    }
}

/// The last thing the human asked for, as the log has it.
///
/// **One definition, two readers.** The brief calls it the objective's fallback
/// and the text steer calls it the pending request, and they must be the same
/// span of bytes: a steer that restated one request while the judge was briefed
/// on another would be correcting an agent against a task nobody set. Extracted
/// as a function rather than left inline in [`Objective::from_items`] for
/// exactly that reason — the second caller arrived with M10.0 and the two
/// answers have to be one answer by construction.
///
/// `None` where the trailing input is not user text: a resent history ending in
/// a tool result, or a session whose only user messages are whitespace. Callers
/// render that absence rather than an empty string — see
/// [`render_steer_answer`](crate::validate::render_steer_answer).
pub fn trailing_user_request(items: &[Item]) -> Option<&str> {
    items.iter().rev().find_map(Item::user_request)
}

/// How much of a session the judge is shown.
///
/// Bounds rather than a full transcript, and which of the two is better is an
/// open question the plan keeps on its risk register: a full transcript costs
/// more, may judge better, and keeps the judge's own prefix warm. Making it
/// configuration rather than a constant is what lets a deployment answer that
/// with its own Shadow data instead of with an argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BriefConfig {
    pub instruction_chars: usize,
    pub objective_chars: usize,
    /// How many trailing tool exchanges to show.
    pub steps: usize,
    pub output_head_chars: usize,
}

impl Default for BriefConfig {
    fn default() -> Self {
        Self {
            instruction_chars: 800,
            objective_chars: 500,
            steps: 12,
            output_head_chars: 240,
        }
    }
}

/// One compacted tool exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefStep {
    /// Position in the brief, and the only index a
    /// [`Divergence`](crate::validate::Divergence) can mean: the judge cannot
    /// see the session's own item indices, so an answer numbered against them
    /// would be a number the judge could not have meant.
    pub index: u32,
    pub name: String,
    pub content: StepContent,
}

/// What a step shows besides its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepContent {
    /// A call that the agent made with one of its own tools.
    Shown {
        /// A fingerprint, not the arguments. See the module note on why.
        argument_hash: String,
        /// The head of the output, or `None` for a call nothing has answered.
        output_head: Option<String>,
        failed: bool,
    },
    /// A call to one of roundhouse's own control tools. The brief does not
    /// show its arguments or result, because a control result can name the
    /// chosen target and its price.
    ///
    /// The step keeps its place in the list. As a result, every other step
    /// keeps the number that the judge's `at_step` refers to. The window also
    /// holds the same trailing calls.
    Withheld,
}

/// The bounded projection one validation is decided on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationBrief {
    pub instructions: Option<String>,
    pub objective: Objective,
    pub steps: Vec<BriefStep>,
    /// Roundhouse's own observations, in the indicative.
    pub facts: Vec<String>,
}

impl ValidationBrief {
    /// Build the brief from a session's items, an objective and the trigger's
    /// facts.
    ///
    /// **Takes items and sentences, and nothing else.** There is no argument
    /// here through which a price, a target or a candidate could arrive, which
    /// is the structural half of the invariant this module exists to hold.
    /// `dialect` is how the session's client spells roundhouse's own control
    /// calls, which are the one way routing facts can arrive *inside* the
    /// items. See the module note.
    pub fn build(
        items: &[Item],
        dialect: ControlCallDialect,
        objective: Objective,
        facts: Vec<String>,
        config: BriefConfig,
    ) -> ValidationBrief {
        let all = exchanges(items);
        let shown = all.len().saturating_sub(config.steps);
        let steps = all[shown..]
            .iter()
            .enumerate()
            .map(|(index, call)| compact(index as u32, call, dialect, config.output_head_chars))
            .collect();
        ValidationBrief {
            instructions: instructions_of(items)
                .map(|text| truncate(&text, config.instruction_chars)),
            objective: truncate_objective(objective, config.objective_chars),
            steps,
            facts,
        }
    }

    /// The brief as the judge receives it.
    ///
    /// Plain sections rather than JSON: the judge is asked for JSON *back*, and
    /// a prompt that is itself a JSON document invites an answer that continues
    /// the document instead of replacing it. Deterministic in every part —
    /// same session, same brief, byte for byte — because a brief that varied
    /// would make the judge's own prefix cache cold on every check, and the
    /// side call is budgeted on that prefix staying warm.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("## Task instructions\n");
        match self.instructions.as_deref() {
            Some(text) => quote(text, QUOTE, &mut out),
            None => out.push_str("(none given)\n"),
        }
        out.push_str("\n## Stated objective\n");
        match &self.objective {
            Objective::Declared {
                goal,
                plan_steps,
                done_when,
            } => {
                quote(goal, QUOTE, &mut out);
                for (index, step) in plan_steps.iter().enumerate() {
                    // The number is roundhouse's and the step is the agent's,
                    // composed before quoting so a step that spans lines cannot
                    // put its continuation outside the quotation.
                    quote(&format!("{}. {step}", index + 1), QUOTE, &mut out);
                }
                quote(&format!("Done when: {done_when}"), QUOTE, &mut out);
            }
            Objective::LastUserMessage(text) => {
                out.push_str("(not stated; the most recent request was)\n");
                quote(text, QUOTE, &mut out);
            }
            Objective::Unknown => out.push_str("(not stated, and no request to fall back on)\n"),
        }
        out.push_str("\n## Recent steps\n");
        if self.steps.is_empty() {
            out.push_str("(no tool activity)\n");
        }
        for step in &self.steps {
            // The name sits inside a line roundhouse wrote, so it is flattened
            // rather than quoted; the output gets a block of its own.
            let name = one_line(&step.name);
            match &step.content {
                StepContent::Shown {
                    argument_hash,
                    output_head,
                    failed,
                } => {
                    out.push_str(&format!("{}. {name} args#{argument_hash}\n", step.index));
                    if *failed {
                        out.push_str("   [failed]\n");
                    }
                    match output_head.as_deref() {
                        Some(head) => quote(head, STEP_QUOTE, &mut out),
                        None => out.push_str("   (no result yet)\n"),
                    }
                }
                StepContent::Withheld => {
                    out.push_str(&format!("{}. {name}\n", step.index));
                    out.push_str(WITHHELD_STEP);
                }
            }
        }
        out.push_str("\n## Observed\n");
        if self.facts.is_empty() {
            out.push_str("(nothing measured)\n");
        }
        for fact in &self.facts {
            // Roundhouse's own sentences — but they interpolate tool names, and
            // a tool name comes from the transcript. Flattened for that one
            // reason: a fact is a sentence by construction, so a line break in
            // one is transcript content wearing a measurement.
            out.push_str(&format!("- {}\n", one_line(fact)));
        }
        out
    }
}

/// The prefix a transcript-derived block carries.
pub(crate) const QUOTE: &str = "> ";

/// The same, indented under the step it belongs to.
const STEP_QUOTE: &str = "   > ";

/// What a control step shows in place of its arguments and result.
const WITHHELD_STEP: &str =
    "   (a session control call: its arguments and result are withheld from this review)\n";

/// Append `text` to `out` as quoted lines — **every** line, including the
/// first.
///
/// The brief is plain markdown sections and the judge is told what each section
/// means, so any transcript span that reaches column zero can open a section of
/// its own: a tool result carrying `\n## Observed\n- <fabrication>` gets its
/// fabrication read as one of roundhouse's own measurements. Bounding the span
/// does not help — the payload fits in eighty characters — and neither does
/// stripping `#`, which would only move the forgery to the next markdown
/// construct somebody thinks of.
///
/// Prefixing unconditionally is what makes the property total rather than
/// enumerated: there is no input for which a line of `text` begins a line of
/// `out`, so nothing in the transcript can be *anything* structural. A payload
/// that quotes itself first arrives as `> > ## Observed`, which is a quotation
/// of a quotation and still not a heading.
///
/// Including the first line is the half that is easy to get wrong. A scheme
/// that quoted continuations only would leave `ok\n## Observed` correctly
/// handled and `## Observed\nok` wide open, and both shapes are one tool result
/// away.
pub(crate) fn quote(text: &str, prefix: &str, out: &mut String) {
    // Trailing blank lines would render as bare prefixes, which is noise in a
    // prompt that is paying for every token.
    for line in text.trim_end().split('\n') {
        out.push_str(prefix);
        out.push_str(line.trim_end_matches('\r'));
        out.push('\n');
    }
}

/// The bytes [`quote`] appends for `text`, computed without allocating.
///
/// Beside `quote` so the two cannot disagree: a caller that bounds a section
/// before building it relies on this being exact.
pub(crate) fn quoted_len(text: &str, prefix: &str) -> usize {
    text.trim_end()
        .split('\n')
        .map(|line| prefix.len() + line.trim_end_matches('\r').len() + 1)
        .sum()
}

/// `text` with its line breaks made visible, for a span that sits *inside* a
/// line roundhouse wrote.
///
/// A marker rather than a strip, because a tool named `ls\n## Observed` is
/// itself evidence about the run under review, and a judge that saw `ls##
/// Observed` would be reading a different session from the one that happened.
pub(crate) fn one_line(text: &str) -> String {
    text.replace(['\n', '\r'], LINE_BREAK_MARK)
}

/// What [`one_line`] writes in place of a line break.
pub(crate) const LINE_BREAK_MARK: &str = "⏎";

/// The bytes [`one_line`] produces for `text`, computed without allocating.
pub(crate) fn one_line_len(text: &str) -> usize {
    text.len() + text.matches(['\n', '\r']).count() * (LINE_BREAK_MARK.len() - 1)
}

fn compact(index: u32, call: &Exchange, dialect: ControlCallDialect, head: usize) -> BriefStep {
    let content = match is_control_call_on(&call.name, call.namespace.as_deref(), dialect) {
        true => StepContent::Withheld,
        false => StepContent::Shown {
            argument_hash: call.argument_hash(),
            output_head: call
                .output
                .as_deref()
                .map(|output| truncate(output.trim(), head)),
            failed: call.failed,
        },
    };
    BriefStep {
        index,
        name: call.name.clone(),
        content,
    }
}

/// The session's instruction block: its **leading run** of system or developer
/// text, oldest first.
///
/// **The run, not the first item of it** (M11.1 review, finding F4). A dialect
/// whose clients send instructions as one string produces one item and this is
/// unchanged for them. Anthropic's Messages clients send `system` as a *list of
/// blocks*, and the shipping Claude Code puts a ~70-byte billing attribution
/// pseudo-header in block 0, its own identity line in block 1, and the actual
/// multi-KB system prompt in block 2 — so a reader that took the first item
/// handed the judge billing metadata and called it the task. Every drift,
/// no-progress and steer verdict for every such session was then decided
/// against a header.
///
/// Concatenated rather than searched for the "real" one, and that is the
/// client-agnostic reading: nothing here knows what an attribution header looks
/// like, and a rule that did would be a rule that breaks the next time the
/// client re-orders its blocks or another client ships a different preamble.
/// The instruction budget the caller already applies does the bounding — the
/// first blocks are small, so the budget spends almost all of itself on the
/// prompt that matters.
///
/// The run stops at the first item that is not system/developer *text*, which
/// is what keeps a mid-conversation system message — history, at a position the
/// conversation agrees on — out of the instructions. It is the same boundary
/// prefix admission draws (see
/// [`is_turn_configuration`](crate::session::is_turn_configuration)), and it is
/// drawn the same way here so the judge is briefed on exactly the block the
/// session treats as its configuration.
fn instructions_of(items: &[Item]) -> Option<String> {
    let run: Vec<&str> = items
        .iter()
        .map_while(|item| match (&item.role, &item.content) {
            (Role::System | Role::Developer, ItemContent::Text { text }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    (!run.is_empty()).then(|| run.join("\n"))
}

fn truncate_objective(objective: Objective, limit: usize) -> Objective {
    match objective {
        Objective::Declared {
            goal,
            plan_steps,
            done_when,
        } => Objective::Declared {
            goal: truncate(&goal, limit),
            plan_steps: plan_steps
                .into_iter()
                .map(|step| truncate(&step, limit))
                .collect(),
            done_when: truncate(&done_when, limit),
        },
        Objective::LastUserMessage(text) => Objective::LastUserMessage(truncate(&text, limit)),
        Objective::Unknown => Objective::Unknown,
    }
}

/// What a truncated span ends with.
///
/// Hoisted so the char count a caller budgets against can never drift from the
/// marker `truncate` actually appends — a hand-counted length beside a literal
/// is exactly the kind of pair that disagrees the day one of the two is
/// edited and not the other. Shared with the classifier projection's own
/// truncation for the same reason.
pub(crate) const TRUNCATION_MARKER: &str = "…[truncated]";

/// At most `limit` characters, with the cut marked.
///
/// Characters and not bytes: a byte slice through a multi-byte character
/// panics, and the one input guaranteed to be arbitrary here is the transcript.
/// The marker is inside the budget rather than added to it, so `limit` is a
/// bound a caller can rely on when sizing a request.
///
/// Shared with side-call adapters so additional text fields use the same
/// character bound and truncation marker as the judge brief.
pub fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let keep = limit.saturating_sub(TRUNCATION_MARKER.chars().count());
    let head: String = text.chars().take(keep).collect();
    format!("{head}{TRUNCATION_MARKER}")
}

#[cfg(test)]
mod tests;
