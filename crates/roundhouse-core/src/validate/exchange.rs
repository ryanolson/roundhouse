// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tool calls paired with what they returned.
//!
//! The one projection both halves of the validate loop read: the trigger asks
//! whether a session is repeating itself, and the brief renders what happened
//! for the judge. Two walks over the same items would be two answers to "what
//! did this agent just do" — the trigger would fire on a repeat the brief then
//! described differently, and nobody reading the log could tell which one was
//! wrong.
//!
//! **Pairing is by `call_id` and nothing else.** An agent runs its own tools
//! between our turns and a client may interleave several in flight, so
//! "the result after the call" is not a rule the wire guarantees. Matching on
//! the id the client itself echoes is what makes an unanswered call visible as
//! unanswered rather than silently paired with somebody else's output.

use sha2::{Digest, Sha256};

use crate::item::{Item, ItemContent};

/// One tool call and its result, if one arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchange {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    /// `None` for a call nothing has answered yet — the last call of a turn
    /// that is still running, or a steer the client has not fetched.
    pub output: Option<String>,
    /// Whether the output reads as a failure. See [`reads_as_failure`].
    pub failed: bool,
}

impl Exchange {
    /// A short, stable fingerprint of what this call was asked to do.
    pub fn argument_hash(&self) -> String {
        short_hash(&self.arguments)
    }

    /// A short, stable fingerprint of what came back, or `None` if nothing has.
    ///
    /// The quantity the no-progress signal compares, and the reason that signal
    /// is *result-aware*: the same call with a different answer is progress.
    pub fn output_hash(&self) -> Option<String> {
        self.output.as_deref().map(short_hash)
    }
}

/// Every tool call in `items`, in log order, paired with its result.
pub fn exchanges(items: &[Item]) -> Vec<Exchange> {
    let mut exchanges: Vec<Exchange> = Vec::new();
    for item in items {
        match &item.content {
            ItemContent::ToolCall {
                call_id,
                name,
                arguments,
            } => exchanges.push(Exchange {
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
                output: None,
                failed: false,
            }),
            ItemContent::ToolResult { call_id, output } => {
                // The *last* call bearing this id, not the first. A client that
                // reuses an id — Codex does not, but a flat-dialect client
                // might — should have its newest call answered, and the
                // alternative silently attributes a fresh result to a stale
                // call and reports a repeat that never happened.
                if let Some(call) = exchanges
                    .iter_mut()
                    .rev()
                    .find(|call| call.call_id == *call_id)
                {
                    call.failed = reads_as_failure(output);
                    call.output = Some(output.clone());
                }
            }
            ItemContent::Text { .. } => {}
        }
    }
    exchanges
}

/// Whether a tool result reads as a failure.
///
/// **A heuristic over text, and named as one.** The canonical [`ItemContent`]
/// carries no success flag, because the wire shape it canonicalizes from does
/// not: a `function_call_output` is a string, and whether that string is an
/// error is a convention of whichever tool produced it. Adding a flag to the
/// item would mean inventing one at the wire boundary and then storing a guess
/// as though it were a fact.
///
/// What being wrong costs, in each direction, is why a guess is acceptable
/// *here* specifically:
///
/// - A false negative leaves this signal quiet. The trigger is a conjunction of
///   a budget gate with *at least one* signal, so a missed failure streak costs
///   a validation that three other signals may still fire.
/// - A false positive cannot on its own cause anything: the gate still has to
///   pass, and past the gate the judge reads the transcript and answers about
///   the trajectory, not about this flag.
///
/// The test is deliberately narrow — a leading marker or a structured
/// `"error"`/`"success": false` field — rather than a scan for the word
/// "error" anywhere, which would flag every result that *mentions* an error it
/// had just fixed.
pub fn reads_as_failure(output: &str) -> bool {
    // The structured shapes first: a tool that answers in JSON has said so
    // explicitly, and an explicit answer beats a textual guess.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(output.trim())
        && let Some(object) = value.as_object()
    {
        if object.get("success") == Some(&serde_json::Value::Bool(false)) {
            return true;
        }
        if let Some(error) = object.get("error") {
            return !error.is_null();
        }
    }
    // Otherwise: a marker at the very start of the output. Anchored on purpose
    // — the same argument that makes a verdict parser anchored makes this one
    // anchored, and a result reading "fixed the TypeError" is not a failure.
    let head = output.trim_start().to_ascii_lowercase();
    [
        "error",
        "error:",
        "traceback",
        "exception",
        "failed",
        "fatal",
    ]
    .iter()
    .any(|marker| head.starts_with(marker))
}

/// A short, stable fingerprint of a string.
///
/// SHA-256 rather than [`std::hash`], for the reason the policy digest gives:
/// this value can travel to a judge and into a log, and a fingerprint that
/// moved between toolchain releases would make two identical calls look
/// different on replay.
pub fn short_hash(value: &str) -> String {
    hex::encode(&Sha256::digest(value.as_bytes())[..6])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ResponseId;

    fn call(call_id: &str, name: &str, arguments: &str) -> Item {
        Item::tool_call(call_id, name, arguments)
    }

    fn result(call_id: &str, output: &str) -> Item {
        Item {
            role: crate::item::Role::Tool,
            content: ItemContent::ToolResult {
                call_id: call_id.into(),
                output: output.into(),
            },
            response_id: None,
        }
    }

    #[test]
    fn calls_pair_with_their_own_results_and_an_unanswered_call_stays_unanswered() {
        let items = vec![
            call("c1", "grep", r#"{"q":"needle"}"#),
            call("c2", "read", r#"{"path":"src/lib.rs"}"#),
            // Answered out of order, which a client with two tools in flight
            // does routinely.
            result("c2", "pub mod validate;"),
            result("c1", "no matches"),
            call("c3", "grep", r#"{"q":"needle"}"#),
        ];
        let paired = exchanges(&items);
        assert_eq!(paired.len(), 3);
        assert_eq!(paired[0].output.as_deref(), Some("no matches"));
        assert_eq!(paired[1].output.as_deref(), Some("pub mod validate;"));
        assert_eq!(
            paired[2].output, None,
            "a call nothing has answered is unanswered, not paired with the \
             nearest result"
        );

        // Two calls with identical arguments hash identically, and their
        // outputs are what tells them apart.
        assert_eq!(paired[0].argument_hash(), paired[2].argument_hash());
        assert_ne!(paired[0].output_hash(), paired[2].output_hash());

        // Text items are neither, and a session with no tools at all yields
        // nothing rather than an empty-shaped call.
        assert!(exchanges(&[Item::user_text("hello")]).is_empty());
        assert!(exchanges(&[]).is_empty());

        // Provenance is irrelevant to pairing: an emitted steer is a call like
        // any other, and its fetch is a result like any other.
        let steered = vec![
            Item {
                response_id: Some(ResponseId::new("resp_1")),
                ..call("rhsteer_resp_1", "fetch_steer", "{}")
            },
            result("rhsteer_resp_1", "re-read the task"),
        ];
        assert_eq!(
            exchanges(&steered)[0].output.as_deref(),
            Some("re-read the task")
        );
    }

    #[test]
    fn a_failure_is_read_from_a_marker_or_a_structured_field_and_not_from_a_mention() {
        for failure in [
            "Error: no such file",
            "error: unresolved import",
            "  Traceback (most recent call last):",
            "FAILED tests/test_api.py::test_one",
            r#"{"success": false, "output": "nope"}"#,
            r#"{"error": "ENOENT"}"#,
        ] {
            assert!(reads_as_failure(failure), "`{failure}` reads as a failure");
        }
        for clean in [
            "no matches",
            "fixed the TypeError in the parser",
            "0 errors, 0 warnings",
            "the error handling path is now covered",
            r#"{"success": true, "output": "ok"}"#,
            r#"{"error": null}"#,
            "",
        ] {
            assert!(
                !reads_as_failure(clean),
                "`{clean}` mentions trouble at most; an unanchored scan would flag it"
            );
        }
    }

    #[test]
    fn a_fingerprint_is_stable_and_short() {
        assert_eq!(short_hash("abc"), short_hash("abc"));
        assert_ne!(short_hash("abc"), short_hash("abd"));
        // Pinned: this value travels to a judge and into a log, so a change to
        // the encoding has to be a change somebody made on purpose.
        assert_eq!(short_hash(""), "e3b0c44298fc");
        assert_eq!(short_hash("").len(), 12);
    }
}
