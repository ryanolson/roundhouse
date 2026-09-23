// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The bounded thing a classifier is allowed to see.
//!
//! **Prior metadata and classifications, plus the current user prompt. Nothing
//! else.** Not the history, not old raw tool outputs, and not this deployment's
//! or the client's system instructions — the egress ruling (T6) permits a
//! classification call at all only on that basis, so the boundary is a type here
//! rather than a habit at a call site.
//!
//! Three absences are stated rather than left to be inferred, because each one
//! is a different fact about the turn and a classifier that could not tell them
//! apart would answer the wrong question:
//!
//! - **tool continuation** — the agent is driving its own loop and the user said
//!   nothing this turn. The tool *results* are not sent; that the turn is a
//!   continuation is.
//! - **no user text** — the turn carried neither user text nor a tool result.
//! - **omitted context** — how many items and how many earlier classifications
//!   were deliberately left out, as counts.
//!
//! An omission with a number beside it is a fact a reader can act on. A silent
//! one reads as "there was nothing there", which is the single most expensive
//! confusion this projection can produce: a truncated turn that looks simple.
//!
//! ## Rejected rather than cut
//!
//! The rendered projection is refused past [`ProjectionCaps::max_total_bytes`]
//! rather than sliced. Slicing would cut the line-prefix quoting that contains a
//! hostile prompt, and a half-quoted forgery is exactly what the quoting exists
//! to prevent. The prompt itself *is* truncated, at a cap of its own and with the
//! truncation stated in the rendered text.

use serde::{Deserialize, Serialize};

use crate::item::{Item, ItemContent, Role};
use crate::validate::brief::{QUOTE, TRUNCATION_MARKER, quote};

use super::{AvailableClassification, ClassificationAxis, PriorTurnMetadata, TAXONOMY_VERSION};

/// The revision of the rendering below and of what it includes.
///
/// Moves when the *content* of a projection changes, so a record written under
/// one revision is never read as though it had been asked the other's question.
///
/// **Bumped to 2** when the whitespace-only-prompt fix (core-session-5)
/// changed what a turn like `[user_text("  \n"), tool_result(..)]` renders as
/// — `origin` moves from `user_text` to `tool_continuation` and the `prompt:`
/// section disappears. Safe to bump: this is a label on new records and
/// nothing reads it to drop, re-key, or refuse a stored classification —
/// `ClassifierIdentity::projection_revision` and `ClassificationWindow::revision`
/// are recorded and never compared against the constant.
pub const PROJECTION_REVISION: u32 = 2;

/// What this deployment is willing to send.
///
/// No [`Default`]: a deployment that has not chosen these has not decided how
/// much of a prompt it is willing to hand a third party.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionCaps {
    /// The most earlier classifications to carry. The newest survive.
    pub max_prior_classifications: usize,
    /// The most prior-turn local metadata records to carry — this
    /// deployment's own read of earlier turns (see
    /// [`PriorTurnMetadata`]), not the classifications above. The newest
    /// survive.
    ///
    /// A cap of its own rather than a reuse of `max_prior_classifications`:
    /// the two lists are unrelated context with unrelated sizes, and a reader
    /// bounding one by the other's name would be reading the wrong doc
    /// comment to find out what it does.
    pub max_prior_turns: usize,
    /// The most of the current prompt to send, in characters.
    pub max_prompt_chars: usize,
    /// The whole rendered projection. Over this, no call happens.
    pub max_total_bytes: usize,
}

/// Where this turn's text came from, or that there was none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptOrigin {
    /// The client sent user text.
    UserText,
    /// Every readable item was a tool result: the agent is continuing its own
    /// loop, and the user said nothing on this turn.
    ToolContinuation,
    /// Neither user text nor a tool result.
    NoUserText,
}

impl PromptOrigin {
    fn label(self) -> &'static str {
        match self {
            Self::UserText => "user_text",
            Self::ToolContinuation => "tool_continuation",
            Self::NoUserText => "no_user_text",
        }
    }
}

/// The current turn's own contribution, bounded at capture.
///
/// Taken from the turn's input items **before** they are committed, which is the
/// only moment this deployment holds "what the client sent on this turn" as a
/// separate thing: the committed log is one flat item list, and reconstructing a
/// turn boundary out of it is a second answer to a question the caller already
/// has.
///
/// ## The input is not always one turn's worth
///
/// `prefix_admission::bind_prefix` answers a fresh session — or a client that
/// rewrote its own history — with the **whole claimed conversation** as the
/// turn's input. So "every user message in the input" is not the current prompt;
/// on an import it is every prompt the session ever had, and sending them as one
/// would put months-old requests in front of a classifier as though the user had
/// just typed them, and would do it at import size rather than at prompt size.
///
/// The boundary is the last assistant item. Everything after it is what this
/// turn contributed; everything before it is history somebody else's turns
/// produced, and is counted rather than read.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptCapture {
    pub origin: PromptOrigin,
    /// The user text of *this* turn, bounded by
    /// [`ProjectionCaps::max_prompt_chars`]. Empty for both absent origins.
    pub text: String,
    pub truncated: bool,
    /// Items of this turn's own contribution that the projection does not
    /// represent: system and developer instructions, tool results, thinking,
    /// and opaque blocks.
    pub omitted_items: usize,
    /// Items belonging to conversation *before* this turn, which an import or a
    /// rewritten history puts in the same input.
    ///
    /// Counted and never read. A non-zero value here on a session's first turn
    /// is the ordinary shape of a client presenting its own history.
    pub imported_history_items: usize,
}

impl PromptCapture {
    /// What may be said about one turn's input.
    ///
    /// **Allocation is bounded by [`ProjectionCaps::max_prompt_chars`], not by
    /// the input.** The first version concatenated every user message and then
    /// truncated the result, so a 200k-token history import built a 200k-token
    /// string on the turn path to throw away all but two thousand characters of
    /// it. Text is accumulated up to the cap and the remainder is not copied.
    pub fn of(input: &[Item], caps: &ProjectionCaps) -> Self {
        // This turn's own contribution: everything after the last thing the
        // model said. See the type's own note on why the whole input is not it.
        let boundary = input
            .iter()
            .rposition(|item| item.role == Role::Assistant)
            .map_or(0, |index| index + 1);
        let current = &input[boundary..];

        // The marker costs from the same budget `truncate` charges it to, so a
        // capture and a truncated string of the same cap are the same length.
        let keep = caps
            .max_prompt_chars
            .saturating_sub(TRUNCATION_MARKER.chars().count());
        let mut text = String::new();
        let mut taken = 0usize;
        let mut overflowed = false;
        let mut tool_results = 0usize;
        let mut omitted = 0usize;

        for item in current {
            // `is_user_request` is the one predicate `trailing_user_request`
            // and the review fold also apply, so a turn's own contribution is
            // read as a request here exactly when it would be read as one
            // anywhere else — including trimming whitespace-only text, which
            // a bare `Role::User` match does not.
            if item.is_user_request() {
                let ItemContent::Text { text: said } = &item.content else {
                    unreachable!("Item::is_user_request guarantees ItemContent::Text")
                };
                if !text.is_empty() {
                    match taken < keep {
                        true => {
                            text.push('\n');
                            taken += 1;
                        }
                        false => overflowed = true,
                    }
                }
                for ch in said.chars() {
                    if taken >= keep {
                        // Everything from here is dropped, and the loop stops
                        // *reading* it rather than copying it to drop later.
                        // That is the whole allocation bound: what this
                        // builds is the size of the cap, whatever the size of
                        // the input.
                        overflowed = true;
                        break;
                    }
                    text.push(ch);
                    taken += 1;
                }
                continue;
            }
            match &item.content {
                ItemContent::ToolResult { .. } => {
                    // Counted, never sent. A tool result is the raw output of
                    // somebody else's program and is exactly what the egress
                    // ruling excludes.
                    tool_results += 1;
                    omitted += 1;
                }
                // Blank user text is not a request — `is_user_request` above
                // said so — and it is not counted as omitted either: the turn
                // contributed nothing readable, which is a different fact
                // from the instructions and tool output this projection
                // deliberately withholds and counts.
                ItemContent::Text { .. } if item.role == Role::User => {}
                _ => omitted += 1,
            }
        }
        if overflowed {
            text.push_str(TRUNCATION_MARKER);
        }

        let origin = match (text.is_empty(), tool_results > 0) {
            (false, _) => PromptOrigin::UserText,
            (true, true) => PromptOrigin::ToolContinuation,
            (true, false) => PromptOrigin::NoUserText,
        };
        Self {
            origin,
            text,
            truncated: overflowed,
            omitted_items: omitted,
            imported_history_items: boundary,
        }
    }
}

/// A projection that does not fit, and by how much.
///
/// Carries lengths and never content: a refusal that quoted the prompt back
/// would put the transcript in whatever log caught it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the rendered projection is {actual_bytes} bytes, over the configured {limit_bytes}")]
pub struct ProjectionTooLarge {
    pub limit_bytes: usize,
    pub actual_bytes: usize,
}

/// One turn, as much of it as may leave the deployment.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnProjection {
    /// [`PROJECTION_REVISION`] as of the process that built this.
    pub revision: u32,
    pub rendered: String,
    pub origin: PromptOrigin,
    pub prompt_truncated: bool,
    pub omitted_items: usize,
    pub prior_included: usize,
    pub prior_omitted: usize,
    /// Prior-turn local metadata records carried, and left out.
    pub local_included: usize,
    pub local_omitted: usize,
    /// Items of earlier conversation the client presented alongside this turn.
    pub imported_history_items: usize,
}

/// Render the current turn against the classifications already available.
///
/// `prior` arrives in log order, oldest first; the newest
/// [`ProjectionCaps::max_prior_classifications`] survive and the rest are
/// reported as a count.
pub fn project(
    capture: &PromptCapture,
    prior: &[AvailableClassification],
    local: &[PriorTurnMetadata],
    caps: &ProjectionCaps,
) -> Result<TurnProjection, ProjectionTooLarge> {
    let included = prior.len().min(caps.max_prior_classifications);
    let omitted = prior.len() - included;
    let kept = &prior[prior.len() - included..];
    let local_included = local.len().min(caps.max_prior_turns);
    let local_kept = &local[local.len() - local_included..];

    let mut rendered = String::new();
    rendered.push_str("# Turn classification\n");
    rendered.push_str(&format!("taxonomy: {TAXONOMY_VERSION}\n"));
    rendered.push_str(&format!("projection: {PROJECTION_REVISION}\n\n"));

    rendered.push_str("## Earlier turns in this session\n");
    if kept.is_empty() {
        rendered.push_str("no classifications available\n");
    }
    for available in kept {
        let labels = &available.classification;
        rendered.push_str(&format!(
            "turn {}: intent={} complexity={} context={}\n",
            available.reference.source_turn_index,
            labels.intent.value.label(),
            labels.complexity.value.label(),
            labels.context_dependence.value.label(),
        ));
    }
    if omitted > 0 {
        rendered.push_str(&format!("{omitted} earlier classifications omitted\n"));
    }

    // **This deployment's own read of the same turns, which is the other half
    // of the permitted prior context.** Counts and one flag, from records
    // already written — a tool-continuation turn has no prompt of its own, and
    // without this its projection would describe nothing at all.
    //
    // `tests_passed_heuristic` is rendered under that name deliberately. It is a
    // guess about what a tool result's text looked like, and a classifier told
    // "tests passed" would read a verdict where there is only a pattern match.
    rendered.push_str("\n## This deployment's own read of those turns\n");
    if local_kept.is_empty() {
        rendered.push_str("no earlier turns recorded\n");
    }
    for turn in local_kept {
        rendered.push_str(&format!(
            "turn {}: depth={} edits={} reads={} worst_tool_severity={:.2} \
             tests_passed_heuristic={} extractor={}\n",
            turn.turn_index,
            turn.turn_depth,
            turn.edit_count,
            turn.read_count,
            turn.severity,
            turn.tests_passed_heuristic,
            turn.extractor_revision,
        ));
    }
    if local.len() > local_included {
        rendered.push_str(&format!(
            "{} earlier turn records omitted\n",
            local.len() - local_included
        ));
    }

    rendered.push_str("\n## This turn\n");
    rendered.push_str(&format!("origin: {}\n", capture.origin.label()));
    if capture.imported_history_items > 0 {
        // The client presented earlier conversation with this turn — a fresh
        // session's import, or a rewritten history. Counted so the absence of
        // those exchanges reads as a boundary rather than as a short session.
        rendered.push_str(&format!(
            "{} items of earlier conversation presented with this turn and not read\n",
            capture.imported_history_items
        ));
    }
    if capture.omitted_items > 0 {
        rendered.push_str(&format!(
            "{} items omitted from this turn's input\n",
            capture.omitted_items
        ));
    }
    if capture.truncated {
        rendered.push_str("the prompt below is truncated\n");
    }
    match capture.origin {
        PromptOrigin::UserText => {
            rendered.push_str("prompt:\n");
            quote(&capture.text, QUOTE, &mut rendered);
        }
        // Stated rather than left as an empty section. "The user said nothing
        // this turn" is a fact about the turn, and a classifier shown a blank
        // prompt would read it as a trivial request instead.
        PromptOrigin::ToolContinuation => {
            rendered.push_str("no prompt: the agent is continuing its own tool loop\n");
        }
        PromptOrigin::NoUserText => {
            rendered.push_str("no prompt: this turn carried no user text\n");
        }
    }

    if rendered.len() > caps.max_total_bytes {
        return Err(ProjectionTooLarge {
            limit_bytes: caps.max_total_bytes,
            actual_bytes: rendered.len(),
        });
    }
    Ok(TurnProjection {
        revision: PROJECTION_REVISION,
        rendered,
        origin: capture.origin,
        prompt_truncated: capture.truncated,
        omitted_items: capture.omitted_items,
        prior_included: included,
        prior_omitted: omitted,
        local_included,
        local_omitted: local.len() - local_included,
        imported_history_items: capture.imported_history_items,
    })
}
