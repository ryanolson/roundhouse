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
//!
//! # What M10.0 removed from here, and what it deliberately kept (T3)
//!
//! `is_undelivered_tool_result` is gone. It recognised the three texts codex
//! substitutes for an answer — `"user cancelled MCP tool call"`, `"user
//! rejected MCP tool call"`, `"aborted"` — and it existed for exactly one
//! caller: the session fold's steer-fulfilment branch, which had to tell a
//! correction the agent *read* from one it declined at an approval prompt (F05).
//! The steer is assistant text now, delivered as the turn's own answer, so
//! there is no dispatch to decline and no undelivered case to classify. Keeping
//! the classifier for a caller that no longer exists would have been dead code
//! wearing pinned-source knowledge.
//!
//! **Where that knowledge went, checked rather than assumed.** Of the three
//! literals, only the `"aborted"` one is in `research/codex-0.146.0-vs-pin-
//! vigilance.md` (claim 10, `ensure_call_outputs_present` synthesising an output
//! for an unanswered call); the two approval-prompt texts are recorded in
//! `PLAN-agentic-control-plane.md` (§ F05) and in git history, and nowhere on
//! the vigilance list. That is a smaller loss than it looks — the vigilance list
//! exists so a codex bump re-reads the claims *this build still depends on*, and
//! after M10.0 no build path depends on those two — but it is stated here rather
//! than implied, because "it is on the vigilance list" was the tempting thing to
//! write and it would have been false.
//!
//! [`tool_output_body`] stays, and it is the half that was ever load-bearing
//! beyond steering: [`ErrorSeverity`](crate::validate::ErrorSeverity) and
//! [`exec_exit_code`] both read codex's result header, and the header is a fact
//! about every exec result rather than about a cancelled steer. So the codex
//! sentinel this module owes the vigilance list is the *header grammar*, not the
//! cancellation literals — and that is what is still here to re-read when the
//! pin moves.

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
    match codex_header(output) {
        Some(header) => &output[header.body_start..],
        None => output,
    }
}

/// The exit status codex's exec wrapper reported, when it reported one.
///
/// **A structured fact read from the header, and the other half of a rule the
/// body cannot carry.** Codex writes `Process exited with code {exit_code}` for
/// every exec call it ran to completion — *including the ones that succeeded*
/// (`response_text`, `core/src/tools/context.rs:443-470` @ `e363b08`, identical
/// at the Cargo pin `6344a65`) — and [`tool_output_body`] strips that section
/// along with the rest of the wrapper. So the two obvious readings are both
/// wrong, and wrong in opposite directions:
///
/// - Ask the **whole string** whether it contains `exited with code` and every
///   exec result reads as a failure, exit 0 included. That is the shape
///   Switchyard's `exit_nonzero` row has upstream, where the header does not
///   exist; ported naively it would pin an error severity on for the life of
///   any session that ran a shell.
/// - Ask only the **body** and the exit status disappears, because the line
///   that carries it is not in the body. That is [`reads_as_failure`]'s state
///   before this accessor existed: blind to a `grep` with no match, a `test`
///   that was false, a `diff` that found differences — silent non-zero exits,
///   the most common failure shape in an agentic coding loop.
///
/// The split this function exists to make: **exit code from the header, text
/// patterns over the body.** Note it is the *inverse* of F04's remedy rather
/// than another instance of it — there the header suppressed an anchored
/// matcher, here it would manufacture a match for an unanchored one.
///
/// `None` means the header had no such section, which is a different answer
/// from `Some(0)` and deliberately not collapsed with it: an MCP result carries
/// `Wall time:` / `Output:` and no exit status at all (`context.rs:118-138`),
/// and inventing a success for it would claim a fact codex never stated. A
/// section whose number does not parse as an [`i32`] is also `None` — the value
/// is unusable, and guessing at it is how a wrapper-format change would become
/// a silent misread rather than a quiet one.
pub fn exec_exit_code(output: &str) -> Option<i32> {
    codex_header(output)?.exit_code
}

/// What one walk of a codex wrapper found.
struct CodexHeader {
    /// Byte offset of the first byte after the `Output:` line.
    body_start: usize,
    exit_code: Option<i32>,
}

/// The one recogniser both derived questions ask.
///
/// Deliberately a single walk rather than two scanners: an exit-code reader
/// that looked for `Process exited with code` *anywhere* would find it in a
/// build log that printed the phrase, which is F04's mistake with the sign
/// flipped. The recogniser that decides where the body starts is the same one
/// that decides whether a `Process exited` line is codex's or the tool's.
///
/// Returns `None` for anything that is not codex's wrapper, which is what keeps
/// every non-codex result — and therefore every fixture and every already
/// folded log — byte-identical.
fn codex_header(output: &str) -> Option<CodexHeader> {
    // The two section prefixes codex can lead with. Matched as prefixes and
    // never parsed: the seconds are formatted `{:.4}` today, and a recogniser
    // that insisted on four decimals would be a second place to update the
    // moment upstream changes its format string.
    if !(output.starts_with("Wall time: ") || output.starts_with("Chunk ID: ")) {
        return None;
    }
    let mut consumed = 0usize;
    let mut exit_code = None;
    for (index, line) in output.split_inclusive('\n').enumerate() {
        if index >= MAX_HEADER_LINES {
            return None;
        }
        consumed += line.len();
        let line = line.strip_suffix('\n').unwrap_or(line);
        if line == "Output:" {
            return Some(CodexHeader {
                body_start: consumed,
                exit_code,
            });
        }
        if let Some(code) = line.strip_prefix(EXIT_SECTION) {
            exit_code = code.parse().ok();
        }
        if !is_header_section(line) {
            return None;
        }
    }
    // Ran out of input inside what looked like a header: no body was ever
    // reached, so there is nothing to strip and no header to have read.
    None
}

/// The section prefix, named once so the recogniser and the parser cannot drift.
const EXIT_SECTION: &str = "Process exited with code ";

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
        EXIT_SECTION,
        "Process running with session ID ",
        "Original token count: ",
    ]
    .iter()
    .any(|section| line.starts_with(section))
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
/// **That first bullet is about occasional misses and does not cover a
/// systematic one**, which is the judgement the exit-code finding forced and is
/// recorded here so nobody re-derives it. "Some failures go unseen, and the
/// disjunction absorbs it" is an argument that stops holding the moment the
/// unseen set is defined by a *shape* rather than by chance: a silent non-zero
/// exit is not one failure in ten, it is every `grep` with no match, every
/// false `test`, every `diff` that found differences — against a codex client
/// the streak signal was not quiet but dead, exactly the class of defect F04
/// was. So the missing structured fact is read rather than tolerated, and the
/// tolerance above is left standing for what it was written about.
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
    // The structured fact before any text at all: a process that exited
    // non-zero has said it failed in the one place the body cannot, and
    // [`exec_exit_code`] documents why reading it from the header rather than
    // from the string is the only reading that is right in both directions.
    if exec_exit_code(output).is_some_and(|code| code != 0) {
        return true;
    }
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

    /// The accessor itself: where an exit code may be read from, and where it
    /// may not.
    #[test]
    fn an_exit_code_is_read_from_the_header_and_never_from_the_body() {
        // Present, zero and non-zero, with and without the optional sections
        // around it. `Some(0)` rather than `None`: "codex said zero" and "codex
        // said nothing" are different answers and the caller needs both.
        assert_eq!(
            exec_exit_code(
                "Chunk ID: 1\nWall time: 0.4212 seconds\nProcess exited with code 0\nOutput:\nall good"
            ),
            Some(0)
        );
        assert_eq!(
            exec_exit_code(
                "Wall time: 0.0210 seconds\nProcess exited with code 101\nOutput:\nerror: could not compile `foo`"
            ),
            Some(101)
        );
        assert_eq!(
            exec_exit_code(
                "Chunk ID: c-1\nWall time: 0.0421 seconds\nProcess exited with code 1\n\
                 Process running with session ID s-1\nOriginal token count: 42\nOutput:\nx"
            ),
            Some(1)
        );

        // Absent: an MCP result has no exit status, and a header that never
        // terminates was never a header.
        assert_eq!(
            exec_exit_code("Wall time: 0.0421 seconds\nOutput:\nfine"),
            None
        );
        assert_eq!(
            exec_exit_code("Wall time: 0.1 seconds\nElapsed: 3\nOutput:\nbody"),
            None
        );

        // Never from the body. Each of these is a tool *printing* the phrase —
        // a build log, a shell transcript, a quoted error — and reading it as
        // roundhouse's own fact is the mistake this accessor exists to make
        // impossible.
        for body_only in [
            "Process exited with code 1",
            "the child process exited with code 3, retrying",
            "Wall time: 0.0421 seconds\nOutput:\nProcess exited with code 9",
            "  Chunk ID: 1\nWall time: 0.1 seconds\nProcess exited with code 4\nOutput:\n",
        ] {
            assert_eq!(
                exec_exit_code(body_only),
                None,
                "`{body_only}` is a string that mentions an exit code, not a header codex wrote"
            );
        }

        // Unparseable is `None`, not a guess: the value is unusable, and a
        // wrapper-format change should degrade to quiet rather than to wrong.
        assert_eq!(
            exec_exit_code("Wall time: 0.1 seconds\nProcess exited with code SIGKILL\nOutput:\n"),
            None
        );
    }

    /// R2's non-regression requirement: a non-codex output is untouched by the
    /// accessor's existence — same body, same fingerprint, same verdict.
    #[test]
    fn a_plain_string_is_unaffected_by_the_exit_code_split() {
        for plain in [
            "no matches",
            "",
            "error: unresolved import",
            "0 errors, 0 warnings",
        ] {
            assert_eq!(tool_output_body(plain), plain);
            assert_eq!(exec_exit_code(plain), None);
            assert_eq!(short_hash(tool_output_body(plain)), short_hash(plain));
        }
        assert!(reads_as_failure("error: unresolved import"));
        assert!(!reads_as_failure("0 errors, 0 warnings"));
    }

    /// The exit code is a structured fact and the body is text, and reading
    /// either one through the other's rules is a defect.
    ///
    /// The claim under test (round-3 Switchyard re-read): a codex **exec**
    /// result that exited non-zero with empty or non-error-shaped stdout reads
    /// as clean, because the one section that says otherwise is the section
    /// [`tool_output_body`] strips. Every string below is codex's real header
    /// shape (`response_text`, `core/src/tools/context.rs:443-470` @ `e363b08`,
    /// identical at the pin `6344a65`).
    #[test]
    fn a_nonzero_exec_exit_reads_as_a_failure_even_when_stdout_says_nothing() {
        // A `test -f`, a `grep` with no match, a `diff` that found differences:
        // the most common failure shape in an agentic coding loop is a silent
        // non-zero exit, and none of them writes a marker to stdout.
        for silent_failure in [
            "Chunk ID: 1\nWall time: 0.0210 seconds\nProcess exited with code 1\nOutput:\n",
            "Chunk ID: 1\nWall time: 0.0210 seconds\nProcess exited with code 1\nOutput:",
            "Wall time: 0.0210 seconds\nProcess exited with code 2\nOutput:\nsrc/lib.rs\n",
            // A signal death, which codex reports as a plain code like any
            // other because its `exit_code` is an `Option<i32>` it formats
            // unconditionally.
            "Chunk ID: 7\nWall time: 9.9000 seconds\nProcess exited with code 137\nOutput:\n",
        ] {
            assert!(
                reads_as_failure(silent_failure),
                "`{silent_failure}` exited non-zero; the header is the only \
                 place that says so and it must be read as a fact"
            );
        }

        // The other half of the split, and the reason this is not "stop
        // stripping": codex writes the section on *success* too, so a body-side
        // substring test would read every exec result as a failure.
        for clean in [
            "Chunk ID: 1\nWall time: 0.4212 seconds\nProcess exited with code 0\nOutput:\nall good",
            "Chunk ID: 1\nWall time: 0.4212 seconds\nProcess exited with code 0\nOutput:\n",
            // An MCP result never carries the section at all, so there is no
            // exit status to lose and nothing to invent.
            "Wall time: 0.0421 seconds\nOutput:\nfine",
        ] {
            assert!(
                !reads_as_failure(clean),
                "`{clean}` exited zero; the `exited with code` text in the \
                 header is codex's bookkeeping, not the tool's verdict"
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
