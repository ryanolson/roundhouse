// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The escalation handoff note: the second steering surface, and the cheap one.
//!
//! **What separates this from the steer next door.** [`verdict`] answers a held
//! turn — the client asked for work, and roundhouse hands back guidance instead
//! of an answer. This decorates a turn that *is* being served: a signal-driven
//! escalation has already moved the quality floor, the turn is on its way to a
//! provider, and one sentence rides along telling whoever answers it why the
//! preceding steps are not to be trusted. Outcome B costs the agent a turn; this
//! costs it a paragraph.
//!
//! [`verdict`]: crate::validate::verdict
//!
//! # Three properties, and each one is a failure mode somebody already found
//!
//! **The note rides the forwarded request only, never the stored log.** The
//! conversation the caller replays, the items the prefix check hashes, and the
//! bytes a successor rebuilds are all identical whether a deployment configured
//! a note or not — which is what makes the feature safe to turn on mid-session.
//! Switchyard states the same property for the same reason
//! (`crates/libsy/src/algorithms/util/stage.rs:436-438@053a61e`, "Stateless: a
//! note is not persisted"), and there it also has a second consequence: notes
//! cannot accumulate, because there is nowhere for an old one to accumulate
//! *in*.
//!
//! **It narrates only a switch that happened, for the reason claimed.**
//! Switchyard gates its note behind `only_on_wrong_signal_escalation`, default
//! `true`, and spells out what an ungated one does: it "can tell the capable
//! model the efficient one was stalling when it wasn't" (`:452-455`, `:479-481`)
//! — only the signal-driven decision sources qualify, and an ambiguous
//! fall-open carries nothing. Roundhouse's analogue is structural rather than
//! configured: the only thing that can put an escalation in
//! [`SessionState`](crate::session::SessionState) is a `ValidationDecided`
//! carrying `SteerAction::Escalate` under an arm that acts, which is a judge
//! that read the trajectory and located a divergence. Every *other* narrowing
//! on a turn — the membership's own floor, the agent's MCP overlay — reaches
//! routing without going anywhere near this fold, so it carries no note by
//! construction and not by a check somebody has to remember.
//!
//! **It says only what this deployment can verify.** The upstream wording is
//! worth copying and its first clause is not: "A weaker model was handling this
//! task … so control was escalated to you, a stronger model"
//! (`dev-server/config.toml:50@053a61e`) asserts that the target changed and
//! that the new one is stronger. Roundhouse cannot promise either. An escalation
//! is *best-effort* narrowing — the floor is clamped to what the quoted pool can
//! reach (`engine::control::escalate_within_reach`), so on a modest pool the
//! turn may be served by the very model that was already handling it. A note
//! claiming otherwise would be exactly the lie the gate exists to prevent,
//! arriving through the wording instead of through the gating. So
//! [`EXAMPLE_HANDOFF_NOTE`] states the observation and the instruction and
//! nothing about who is answering.
//!
//! # Where the marker comes from
//!
//! [`HANDOFF_MARKER`] is roundhouse's, not the operator's, and it is prepended
//! here rather than written into the config value. The note is appended to the
//! *user's* message, so an unmarked one is indistinguishable from something the
//! user wrote — which is the same injection-boundary argument
//! [`render_steer_answer`](crate::validate::render_steer_answer) rests on, with
//! the roles reversed: there roundhouse quotes the user so its own lines are
//! identifiable, here it marks its own line for the same reason.

/// The prefix every handoff note carries.
///
/// One literal, so a deployment grepping its provider logs for "did roundhouse
/// decorate this request" has one pattern, and so a model that has learned the
/// convention sees it in the same place every time.
pub const HANDOFF_MARKER: &str = "[roundhouse-guidance]";

/// The note a deployment turning this on can start from.
///
/// **Documentation, not a default.** No note is configured unless a project
/// writes one, and this is not silently substituted for a missing value: R2
/// ships the surface off, and a deployment that has not decided what to say to
/// an escalated turn has not decided to decorate one. It is a `pub const` rather
/// than a doc-comment example so the tests that assert the rendering can assert
/// it against the string an operator would actually copy.
///
/// Adapted from Switchyard's production wording
/// (`dev-server/config.toml:50@053a61e`), with the clause about who is answering
/// removed — see the module note on why roundhouse cannot make that claim.
pub const EXAMPLE_HANDOFF_NOTE: &str = "A review of this session's recent steps found signs of stalling, looping, or \
     repeated errors on the work leading up to this request. Re-examine the \
     current state of the task directly, and do not simply repeat the previous \
     approach.";

/// `prompt`, with `note` appended as roundhouse's own trailing line.
///
/// **On this wire, "the end of the prompt" and "the end of the trailing user
/// message" are the same position**, and that is worth stating because the
/// ruling is phrased in terms of the second. The Responses client wraps the
/// whole rendered conversation in a single `input_text` content block on one
/// user message (`roundhouse-fleet/src/openai_responses.rs`, `body`), so there
/// is exactly one message to be the trailing one. The day a client sends a
/// structured array instead, this function takes the array and the callers do
/// not change — which is why it exists at all rather than being two `push_str`
/// calls at the call site.
///
/// Takes the prompt by value: the undecorated path is every turn of every
/// deployment, and it must not pay for a clone that the decorated path is going
/// to make anyway.
pub fn append_handoff_note(prompt: String, note: &str) -> String {
    let mut decorated = String::with_capacity(prompt.len() + note.len() + HANDOFF_MARKER.len() + 4);
    decorated.push_str(&prompt);
    // A blank line, so the note reads as a separate paragraph rather than as the
    // last sentence of whatever the user was saying. The marker is what makes it
    // attributable; the break is what makes it legible.
    decorated.push_str("\n\n");
    decorated.push_str(HANDOFF_MARKER);
    decorated.push(' ');
    decorated.push_str(note);
    decorated
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rendering, pinned: the request is untouched and roundhouse's line is
    /// attributable.
    ///
    /// Both halves matter and they fail differently. A note that mangled the
    /// request would corrupt the very turn it is trying to help; an unmarked
    /// note would arrive as another sentence of the user's own message, which is
    /// the injection boundary the module note describes.
    #[test]
    fn the_note_is_appended_whole_marked_and_leaves_the_request_untouched() {
        let decorated = append_handoff_note(
            "<|user|>rename the config key and update the tests".to_string(),
            EXAMPLE_HANDOFF_NOTE,
        );
        assert_eq!(
            decorated,
            format!(
                "<|user|>rename the config key and update the tests\n\n\
                 {HANDOFF_MARKER} {EXAMPLE_HANDOFF_NOTE}"
            )
        );
        assert!(
            decorated.starts_with("<|user|>rename the config key and update the tests"),
            "the forwarded request must be a prefix of the decorated one: nothing \
             the caller wrote may move"
        );

        // The control that makes the marker assertion about attribution rather
        // than about a literal being present somewhere: the request itself does
        // not carry it, so its presence in the result can only have come from
        // here.
        assert_eq!(
            decorated.matches(HANDOFF_MARKER).count(),
            1,
            "exactly one line in the forwarded request is roundhouse's"
        );
    }

    /// The example note promises nothing this deployment cannot check.
    ///
    /// A wording test, and deliberately so: the shipped example is what most
    /// deployments will paste, and the one way this surface tells a lie is by
    /// asserting something about the model now answering — that it is stronger,
    /// or that the previous one was weaker. Roundhouse's escalation is clamped
    /// to what the pool can reach, so on a modest pool the same model answers
    /// and the claim would be false. The upstream string this was adapted from
    /// makes exactly that claim; this is the guard that keeps the adaptation
    /// from being quietly reverted to it.
    #[test]
    fn the_example_note_claims_nothing_about_which_model_is_answering() {
        for forbidden in ["stronger", "weaker", "escalated to you", "control was"] {
            assert!(
                !EXAMPLE_HANDOFF_NOTE.contains(forbidden),
                "the example note must not claim `{forbidden}`: an escalation is \
                 best-effort narrowing, and on a pool that cannot reach the floor \
                 the turn is served by the model that was already handling it"
            );
        }
        // The control: it does say the two things roundhouse *does* know — that
        // a review found trouble in the preceding steps, and what to do about it.
        assert!(EXAMPLE_HANDOFF_NOTE.contains("review of this session's recent steps"));
        assert!(EXAMPLE_HANDOFF_NOTE.contains("do not simply repeat the previous approach"));
    }
}
