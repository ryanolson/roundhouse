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
    ///
    /// Hashed over [`tool_output_body`] rather than the stored string: codex
    /// stamps a fresh wall time on every result, so hashing the whole thing
    /// gives four different fingerprints to four identical answers and the
    /// no-progress signal can never fire against a real client (F04). Output
    /// nothing wrapped hashes exactly as it did before, so folding an existing
    /// log produces the same fingerprints it always did.
    pub fn output_hash(&self) -> Option<String> {
        self.output
            .as_deref()
            .map(|output| short_hash(tool_output_body(output)))
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

/// The tool's own answer, with a codex wrapper header removed if it wrote one.
///
/// **Why this lives here and not at the wire boundary.** Codex prepends a
/// header to every tool result before echoing it back as a
/// `function_call_output`: `Wall time: N.NNNN seconds\nOutput:` for an MCP call
/// (`core/src/tools/context.rs:124-126`) and a `Chunk ID:` / `Wall time:` /
/// `Process exited with code N` / `Output:` block for an exec call
/// (`context.rs:446-465`) — read at both codex revisions this repo tracks,
/// `e363b08` (the binary) and `6344a65` (the Cargo pin), which agree verbatim.
/// Stripping it on the way *in* would be the obvious fix and is the wrong one:
/// the stored item has to stay the client's verbatim bytes or prefix admission
/// stops recognising the history codex resends, and every prefix probe would
/// then miss on a session that had ever run a tool. So the log keeps what the
/// client sent and the two *derived* questions — is this a failure, is this the
/// same answer as last time — ask them of the body.
///
/// **Recognition is deliberately strict, and bails to the whole string.** The
/// header must start at byte 0 (an indented one is somebody else's format),
/// every line up to the terminator must be one of codex's own sections, and the
/// literal `Output:` line must arrive within [`MAX_HEADER_LINES`]. Anything
/// else returns the input unchanged, which is what keeps a non-codex result —
/// and therefore every fixture and every already-folded log — byte-identical.
/// A looser scan for `Output:` anywhere would decapitate a ten-thousand-line
/// build log that happens to contain the word.
///
/// **Three known-inert cases, named because a silent return is what this
/// function does when it does not recognise something** — the same reason the
/// pinned sentinels below are on the vigilance list:
///
/// - *Content-items results.* Codex puts the header in its own content item
///   rather than joined by a newline when the tool answered with structured
///   parts (`context.rs:127-137`), and this surface stores that form as its JSON
///   encoding (`responses_api::wire::output_text`, deliberately — flattening
///   would make two different outputs canonicalize identically). The string then
///   opens `[{"type":…` and nothing is stripped. Narrow in practice: codex
///   collapses a lone text part back to the text form and roundhouse's own eight
///   tools all answer with one, so the exposed case is a third-party MCP server
///   returning multi-part or image content.
/// - *A seventh section upstream* makes the bound reject a header that is
///   otherwise codex's.
/// - *Truncated exec output* opens `Warning: truncated output (original token
///   count: N)` (`context.rs:430-441`) — inside the body, so a truncated failure
///   still reads clean to [`reads_as_failure`]'s anchored markers.
///
/// All three degrade the way the pre-fix code degraded — a signal stays quiet —
/// rather than producing a wrong answer, which is why they are documented here
/// instead of guessed at.
pub fn tool_output_body(output: &str) -> &str {
    // The two section prefixes codex can lead with. Matched as prefixes and
    // never parsed: the seconds are formatted `{:.4}` today, and a recogniser
    // that insisted on four decimals would be a second place to update the
    // moment upstream changes its format string.
    if !(output.starts_with("Wall time: ") || output.starts_with("Chunk ID: ")) {
        return output;
    }
    let mut consumed = 0usize;
    for (index, line) in output.split_inclusive('\n').enumerate() {
        if index >= MAX_HEADER_LINES {
            return output;
        }
        consumed += line.len();
        let line = line.strip_suffix('\n').unwrap_or(line);
        if line == "Output:" {
            return &output[consumed..];
        }
        if !is_header_section(line) {
            return output;
        }
    }
    // Ran out of input inside what looked like a header: no body was ever
    // reached, so there is nothing to strip and the caller gets what arrived.
    output
}

/// Codex's longest header: `Chunk ID`, `Wall time`, `Process exited`,
/// `Process running`, `Original token count`, then `Output:`
/// (`core/src/tools/context.rs:446-465` @ `e363b08`). A bound rather than an
/// open scan so a result that merely *opens* like a header cannot lose its
/// first ten lines to a stray `Output:` further down.
const MAX_HEADER_LINES: usize = 6;

fn is_header_section(line: &str) -> bool {
    [
        "Wall time: ",
        "Chunk ID: ",
        "Process exited with code ",
        "Process running with session ID ",
        "Original token count: ",
    ]
    .iter()
    .any(|section| line.starts_with(section))
}

/// Whether a tool result says the call never reached the tool at all.
///
/// The three texts codex substitutes for an answer, all read at `e363b08` and
/// confirmed identical at the Cargo pin `6344a65`:
///
/// - `"user cancelled MCP tool call"` — the operator cancelled the approval
///   prompt (`core/src/mcp_tool_call.rs:280`).
/// - `"user rejected MCP tool call"` — the approval was declined and no custom
///   message was supplied (`mcp_tool_call.rs:267`).
/// - `"aborted"` — codex synthesising a missing output for a call whose turn
///   was dropped (`core/src/context_manager/normalize.rs:58,93,112`).
///
/// **Equality on the trimmed body, never a prefix or a substring.** `"aborted"`
/// is short enough that a `contains` test would fire on a real correction
/// reading "aborted the migration, now re-read the task" — and being wrong in
/// that direction re-steers an agent that complied and writes a false fact into
/// the log. Wrong in the other direction is F05 itself: a declined steer read
/// as answered. A false positive here costs one redundant validation that the
/// `consecutive_interventions` ladder already bounds; a false negative costs
/// the correction entirely.
///
/// **Pinned to a revision, so it is on the vigilance list.** These are upstream
/// message literals with no wire-level status beside them — codex reports a
/// declined call as an ordinary `function_call_output` — so a codex bump has to
/// re-read them the way any other pinned-source claim is re-read.
pub fn is_undelivered_tool_result(output: &str) -> bool {
    matches!(
        tool_output_body(output).trim(),
        "user cancelled MCP tool call" | "user rejected MCP tool call" | "aborted"
    )
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
///
/// **Asked of [`tool_output_body`], not of the stored string.** The anchoring
/// below is what makes the narrow test safe, and codex's header sits ahead of
/// the marker on every real result — so against a real client the anchored
/// check was not a probabilistic miss but a hard never, and the failure streak
/// signal was dead (F04). Stripping here rather than at each call site means a
/// future reader of this function cannot reintroduce the gap by forgetting to.
pub fn reads_as_failure(output: &str) -> bool {
    let output = tool_output_body(output);
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

    /// The half of F04's fix that nothing else can prove: output codex did not
    /// write must hash exactly as it hashed before the stripper existed, or
    /// every already-folded session silently changes fingerprints and a repeat
    /// that fired yesterday stops firing today.
    #[test]
    fn output_no_codex_wrapper_touched_hashes_exactly_as_the_raw_string_does() {
        for untouched in [
            "no matches",
            "",
            r#"{"success": true, "output": "ok"}"#,
            // Contains the terminator, but not as a header codex wrote.
            "build log\nOutput:\nfine",
            // Opens like a header but never terminates within the bound —
            // the shape a ten-thousand-line log would take.
            "Wall time: forever\nline\nline\nline\nline\nline\nline\nOutput:\nnope",
            // Indented: codex writes the header itself, flush left.
            "  Wall time: 0.1 seconds\nOutput:\nindented",
        ] {
            let paired = exchanges(&[call("c1", "t", "{}"), result("c1", untouched)]);
            assert_eq!(
                paired[0].output_hash(),
                Some(short_hash(untouched)),
                "`{untouched}` is not codex-wrapped, so its fingerprint must be \
                 the raw string's and existing logs must fold identically"
            );
        }
        // Pinned, for the same reason `a_fingerprint_is_stable_and_short` pins
        // one: this value reaches a log, so a change to it is a change made on
        // purpose.
        assert_eq!(short_hash("no matches"), "0ed6af34915f");
    }

    /// The stripper itself: what it takes off, and where it refuses to.
    #[test]
    fn a_codex_header_is_recognised_up_to_its_output_line_and_nothing_else_is() {
        // The MCP wrapper (context.rs:124-126).
        assert_eq!(
            tool_output_body("Wall time: 0.0421 seconds\nOutput:\nImportError"),
            "ImportError"
        );
        // The exec wrapper, every section present (context.rs:446-465).
        assert_eq!(
            tool_output_body(
                "Chunk ID: c-1\nWall time: 0.0421 seconds\nProcess exited with code 1\n\
                 Process running with session ID s-1\nOriginal token count: 42\nOutput:\n\
                 Error: build failed"
            ),
            "Error: build failed"
        );
        // An empty MCP result is the header alone: a body of nothing, not a
        // refusal to strip.
        assert_eq!(tool_output_body("Wall time: 0.0421 seconds\nOutput:"), "");
        // Multi-line bodies keep every line, including one that looks like a
        // second header.
        assert_eq!(
            tool_output_body("Wall time: 0.0 seconds\nOutput:\nfirst\nWall time: fake"),
            "first\nWall time: fake"
        );
        // A line that is not one of codex's sections ends recognition, because
        // whatever wrote it was not codex.
        assert_eq!(
            tool_output_body("Wall time: 0.1 seconds\nElapsed: 3\nOutput:\nbody"),
            "Wall time: 0.1 seconds\nElapsed: 3\nOutput:\nbody"
        );
        // The wall time is never parsed: the failure-streak fixture formats it
        // to two decimals and upstream to four, and both are codex's header.
        assert_eq!(tool_output_body("Wall time: 0.01 seconds\nOutput:\nx"), "x");
    }

    /// F05: the three texts codex substitutes for an answer.
    #[test]
    fn a_cancellation_reads_as_undelivered_and_a_directive_that_mentions_one_does_not() {
        for undelivered in [
            "user cancelled MCP tool call",
            "user rejected MCP tool call",
            "aborted",
            // As it actually arrives: codex wraps the cancellation text the
            // same way it wraps a real answer.
            "Wall time: 0.0000 seconds\nOutput:\nuser cancelled MCP tool call",
        ] {
            assert!(
                is_undelivered_tool_result(undelivered),
                "`{undelivered}` is codex saying the call never ran"
            );
        }
        for delivered in [
            "re-read the task",
            // The reason this is equality and not `contains`: a real
            // correction is allowed to mention what was aborted.
            "aborted the migration, now re-read the task",
            "the user cancelled MCP tool call earlier; try the other approach",
            "",
        ] {
            assert!(
                !is_undelivered_tool_result(delivered),
                "`{delivered}` is a directive the agent received"
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
