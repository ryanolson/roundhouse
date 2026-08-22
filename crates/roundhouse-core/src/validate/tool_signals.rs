// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What the last few tool results and calls say about a session, as numbers.
//!
//! A curated error table over tool output, plus a handful of counts over tool
//! *names* — no model call, no clock, no I/O. The trigger's four original
//! signals each answer one question about a shape (a repeat, an alternation, a
//! streak, an outlier); this answers "how bad does the recent output look and
//! what kind of work is the agent actually doing", which is the vocabulary the
//! M6 trigger did not have and the `Signal` seam was left open for.
//!
//! # Attribution
//!
//! Ported from NVIDIA Switchyard (Apache-2.0), rev
//! `053a61e2c43ba15f0772952ec3b3060c24b317f2`:
//! `crates/libsy/src/algorithms/util/tool_signals.rs`.
//!
//! **Taken:** the twelve portable fields of `ToolSignals`, the
//! `ERROR_PATTERNS` severity table and its trace-mined anchoring note, the five
//! tool-name tables, the three bash-command pattern lists and their
//! first-match-wins order, the test-outcome markers with
//! `has_nonzero_failure_count`'s whitespace rule, and `DEFAULT_RECENT_WINDOW`.
//!
//! **Deliberately not taken**, each for a stated reason rather than for effort:
//!
//! - `pick_tier` and `score_signal`, upstream's two scorers. They answer *which
//!   model tier should run next*, and [`SignalFired::fact`](super::SignalFired)
//!   forbids that output shape outright — "never a suggestion". A judge handed
//!   a tier recommendation is an expensive way to re-read the recommender. If
//!   they are ever wanted here they belong beside `routing/policy.rs`, where
//!   the routing question is already asked exactly once, of code.
//! - `turn_depth`, upstream's `messages.len()`. [`Evidence`]
//!   carries exchanges rather than messages, and upstream itself calls the
//!   count "wire-format dependent … approximate across request origins" — so
//!   the honest roundhouse quantity is a count of exchanges, which every signal
//!   here already has.
//! - `compacted`, upstream's scan for Claude Code's compaction preamble. Dead
//!   three ways here: [`exchanges`](super::exchanges) drops text items so the
//!   scan has no input, the marker is Claude Code's and not codex's, and the
//!   self-latching premise fails because roundhouse forks a compacted
//!   conversation onto a fresh empty session — there is no prefix left to latch
//!   onto.
//! - The `Request` type and everything around it. Upstream reads
//!   `llm_request.messages` and walks `ContentBlock`s; this reads
//!   [`Exchange`], which is roundhouse's own projection of the same facts.
//!
//! Both trees carry the same `SPDX-FileCopyrightText` line and the same
//! licence, so what is owed is provenance and revision, not a third-party
//! copyright notice. The revision is the half that rots, which is why a test
//! below pins it rather than prose asking a reader to keep it fresh.
//!
//! # The codex exec header is half signal and half noise
//!
//! The one place this port is not a transcription, and getting it backwards
//! would leave a defect behind a passing test. Upstream's `exit_nonzero` row
//! matches the bare substring `exited with code` with no digit constraint, over
//! the whole result text. Codex writes `Process exited with code {n}` as a
//! header section on **every** exec result, successes included, so:
//!
//! - matching the stored string scores [`SOFT`] on every shell call a session
//!   ever makes, which pins `no_error_streak` at zero forever;
//! - matching only [`tool_output_body`] loses the exit status entirely, because
//!   that line is one of the sections the stripper removes.
//!
//! So the rule is **exit code from the header, text patterns over the body**:
//! the substrings run over [`tool_output_body`] and the exit status comes from
//! [`exec_exit_code`] as a structured fact. A non-zero code contributes [`SOFT`]
//! the way upstream's row does, which is what stops the split from
//! reintroducing, one layer up, the exact blindness it was written to fix — a
//! result that exited 1 with nothing on stdout must not score 0.0 here any more
//! than it may read as clean to
//! [`reads_as_failure`](super::exchange::reads_as_failure).
//!
//! Verified against codex rather than assumed: `shell_command` declares
//! `command` as a *string* (`core/src/tools/handlers/shell_spec.rs:157-166` @
//! `e363b08`), which is the shape upstream's `command_of` reads, so the bash
//! pattern tables land on codex traffic unmodified. The tool-name tables
//! already carry codex's own spellings — `shell_command`, `local_shell_call`,
//! and `update_plan`, which upstream annotates as codex's `todowrite`.

use serde_json::Value;

use crate::validate::exchange::{Exchange, exec_exit_code, tool_output_body};
use crate::validate::trigger::{Evidence, Signal, SignalKind};

// ─── severity constants ──────────────────────────────────────────────────────

/// A plain non-zero exit with no recognisable exception behind it.
pub const SOFT: f32 = 0.3;
/// A named failure: a traceback, an import error, a timeout, a missing file.
pub const HARD: f32 = 0.7;
/// The two that end a session rather than interrupt it.
pub const CRITICAL: f32 = 1.0;

/// How many trailing tool results the windowed fields are computed over.
///
/// Upstream's value and upstream's argument for it: a short horizon captures
/// what the agent is doing *now* while keeping the signal sticky, so an error
/// persists through a couple of recovery results instead of flickering off the
/// moment one clean result lands.
pub const DEFAULT_RECENT_WINDOW: usize = 3;

/// `(name, severity, lower-cased substrings — any hit fires the pattern)`.
///
/// Copied verbatim from upstream, ordering and all, because the table is the
/// asset: it is trace-mined rather than reasoned, and an editorial improvement
/// made on the way across would be an unmeasured heuristic wearing a measured
/// one's provenance.
static ERROR_PATTERNS: &[(&str, f32, &[&str])] = &[
    (
        "oom",
        CRITICAL,
        &["out of memory", "memoryerror", "cannot allocate memory"],
    ),
    (
        "connection_refused",
        CRITICAL,
        &[
            "connection refused",
            "connectionrefusederror",
            "econnrefused",
        ],
    ),
    ("traceback", HARD, &["traceback (most recent call last)"]),
    (
        "import_error",
        HARD,
        &["modulenotfounderror:", "importerror:", "no module named "],
    ),
    (
        "cmd_not_found",
        HARD,
        &["command not found", "not found\n", "/usr/bin/env: "],
    ),
    ("assertion", HARD, &["assertionerror"]),
    ("value_error", HARD, &["valueerror:"]),
    ("syntax_error", HARD, &["syntaxerror:"]),
    (
        "timeout",
        HARD,
        &[
            "timed out",
            "timeouterror",
            "timeout expired",
            "deadline exceeded",
        ],
    ),
    (
        "no_such_file",
        HARD,
        &[
            "filenotfounderror:",
            "no such file or directory",
            // Upstream's provenance, kept because it is the number that makes
            // the row defensible: anchored as "file does not exist" rather than
            // a bare "does not exist", which fires on `ls` output and on prose
            // — trace-mined across 1006 local trajectories at 22 true / 2 false
            // positives.
            "file does not exist",
        ],
    ),
    // SOFT: a plain non-zero exit with no recognisable exception traceback.
    // These substrings run over the **body** only; the exit status codex states
    // in its header arrives through [`exec_exit_code`] instead. See the module
    // doc — running them over the stored string scores every successful exec.
    (
        "exit_nonzero",
        SOFT,
        &[
            "exit code 1",
            "exit code 2",
            "exit status 1",
            "returned non-zero",
            "exited with code",
        ],
    ),
];

static EDIT_TOOL_NAMES: &[&str] = &[
    "edit",
    "multiedit",
    "notebookedit",
    "str_replace",
    "str_replace_based_edit_tool",
    "text_editor",
    "patch", // hermes's str_replace-style edit tool
];

static WRITE_TOOL_NAMES: &[&str] = &["write", "create_file", "new_file", "write_file"];

// Bash subcommand patterns, lower-cased; the command is lower-cased before
// matching. Bucketed into write/edit counts alongside the dedicated tools,
// because `cat > f` is a write however the harness spells the tool.
static BASH_WRITE_PATTERNS: &[&str] = &[
    "cat >",
    "cat >>",
    "echo >",
    "echo >>",
    "tee ",
    "printf >",
    "printf >>",
    "> /",
    ">> /",
    "<< 'eof'",
    "<<eof",
    "<<'eof'",
    "<< eof",
];

static BASH_EDIT_PATTERNS: &[&str] = &[
    "sed -i",
    "sed --in-place",
    "awk -i inplace",
    "awk 'inplace=1'",
    "patch ",
    "patch -p",
    "perl -i",
    "perl -p -i",
    "perl -pi",
];

// Read-like inspections. Reached only when no write or edit pattern fired:
// redirection and in-place editing deliberately trump a read-like operand, so
// `grep foo > out` is a write and not a read.
static BASH_READ_PATTERNS: &[&str] = &[
    "cat /", "cat ./", "cat ../", "grep ", "ls ", "ls -", "find ", "head ", "tail ", "wc ",
    "diff ", "which ", "ps ", "df ", "du ", "stat ", "file ", "less ", "more ",
];

static READ_TOOL_NAMES: &[&str] = &["read", "view", "read_file", "search_files"];

// Planning / scratchpad calls — investigative, non-producing activity.
// `update_plan` is codex's equivalent of `todowrite`.
static PLAN_TOOL_NAMES: &[&str] = &["todowrite", "todo_write", "todo", "update_plan"];

// Tool names whose category comes from their command rather than their name.
// `bash` is claude-code's; `shell_command` is codex's; `shell` and
// `local_shell_call` are seen on OpenAI-derived harnesses; `terminal` is
// hermes's.
static BASH_TOOL_NAMES: &[&str] = &[
    "bash",
    "shell_command",
    "shell",
    "local_shell_call",
    "terminal",
];

// Upstream's stated bias, kept: prefer false negatives. There it routes the
// picker to a cheaper tier, so a false positive drops tier on unfinished work.
// Here it is a field rather than a signal, and the same bias is still the safe
// one — a wrongly-settled session is one nobody looks at.
static TEST_PASS_PHRASES: &[&str] = &[
    " passed",
    "passed in",
    "tests passed",
    "all tests passed",
    "test ok",
    "test result: ok",
    "passed.\n",
    "tests pass",
    "\nok ", // go test; newline-anchored to avoid "...lookup..." mid-text
    "✓ ",
];

// Literal failure phrases that cannot appear inside a clean run. Phrases that
// pair with a count ("failed", "errors") are handled by
// `has_nonzero_failure_count` instead, so `0 failed` is not a false negative.
static TEST_FAILURE_LITERAL: &[&str] = &["✗ ", "fatal:", "assertionerror", "error:"];

// Count-prefixed failure keywords: trip only when a non-zero integer precedes
// the keyword modulo whitespace, so cargo's "0 failed" and go's "0 errors" on a
// clean run are not misread.
static NUMERIC_FAILURE_KEYWORDS: &[&str] = &["failed", "failure", "failures", "errors", "error"];

// ─── output type ─────────────────────────────────────────────────────────────

/// What the recent tool traffic says, as twelve numbers.
///
/// Every field is a fact about the session's own log, computable with no model
/// call — which is what lets a signal built on it be *tested* rather than
/// approximated, the same property the trigger's other four have.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolSignals {
    /// Max severity across the recent window of tool results: `0.0` clean,
    /// [`SOFT`], [`HARD`], [`CRITICAL`]. Windowed rather than last-only so an
    /// error persists through the recovery results instead of clearing the
    /// instant one clean result lands.
    pub severity: f32,
    /// Consecutive clean tool results counting back from the most recent. `0`
    /// if the last one carried any severity at all.
    pub no_error_streak: u32,
    /// Edit-style calls in the whole session.
    pub edit_count: u32,
    /// Write-style calls in the whole session.
    pub write_count: u32,
    /// Read-type calls (read tools plus read-like shell) in the whole session.
    pub read_count: u32,
    /// Planning / todo calls in the whole session — investigative activity,
    /// which is what distinguishes exploring from spinning.
    pub todowrite_count: u32,
    /// Edit-type calls within the recent window.
    pub recent_edit_count: u32,
    /// Write-type calls within the recent window.
    pub recent_write_count: u32,
    /// Read-type calls within the recent window.
    pub recent_read_count: u32,
    /// Planning calls within the recent window.
    pub recent_todowrite_count: u32,
    /// Consecutive trailing calls in the `Other` category — nothing that reads,
    /// writes, edits or plans. Upstream's build-pit proxy.
    pub pure_bash_streak: u32,
    /// A result in the recent window looks like a passing test run.
    ///
    /// **A field, not a signal, and the distinction is the seam's.** Every
    /// [`Signal`] states that a session is in *trouble*; the
    /// gate is a budget and the signals are evidence, and there is no slot for
    /// "the turn is settling, spend less". A tests-passed signal would have to
    /// either fire on good news — which the brief renders as a fact about
    /// trouble — or recommend an action, which
    /// [`SignalFired::fact`](super::SignalFired) forbids. It is carried because
    /// it is cheap and because whatever consumes it (a future policy input, a
    /// dashboard) will want it computed the same way everything else here is.
    pub tests_passed: bool,
}

impl ToolSignals {
    /// The signals over a session's exchanges, at [`DEFAULT_RECENT_WINDOW`].
    pub fn from_exchanges(exchanges: &[Exchange]) -> Self {
        Self::with_window(exchanges, DEFAULT_RECENT_WINDOW)
    }

    /// The signals over a session's exchanges, at an explicit window.
    pub fn with_window(exchanges: &[Exchange], window: usize) -> Self {
        // Results and calls are two different sequences and are windowed
        // separately, which is upstream's shape: a call still in flight has no
        // result to classify, and an exchange whose tool answered with nothing
        // is still a result — the empty body of a command that exited non-zero
        // and printed nothing is exactly the case the severity split exists
        // for, so it is kept in the window rather than dropped.
        let severities = recent_severities(exchanges, window);
        let severity = severities
            .iter()
            .fold(0.0f32, |worst, result| worst.max(result.severity));

        let mut counts = Counts::default();
        // The asymmetry is upstream's and is copied rather than tidied:
        // `max(1)` on the result window (a window of zero would still classify
        // the newest result), no `max(1)` on the call window (a window of zero
        // means no recent counts). Making them agree would be an improvement to
        // a ported heuristic, which is how a port stops being one.
        let recent_start = exchanges.len().saturating_sub(window);
        let mut streak_open = true;
        for (index, exchange) in exchanges.iter().enumerate().rev() {
            let category = classify_tool_call(&exchange.name, command_of(&exchange.arguments));
            if streak_open {
                if category == ToolCategory::Other {
                    counts.pure_bash_streak += 1;
                } else {
                    streak_open = false;
                }
            }
            counts.add(category, index >= recent_start);
        }

        Self {
            severity,
            no_error_streak: no_error_streak(exchanges),
            edit_count: counts.edit,
            write_count: counts.write,
            read_count: counts.read,
            todowrite_count: counts.todowrite,
            recent_edit_count: counts.recent_edit,
            recent_write_count: counts.recent_write,
            recent_read_count: counts.recent_read,
            recent_todowrite_count: counts.recent_todowrite,
            pure_bash_streak: counts.pure_bash_streak,
            tests_passed: detect_tests_passed(exchanges, window),
        }
    }
}

/// One tool result's severity and the pattern names that produced it.
///
/// Public because a signal that says only "severity 0.7" is a number and not a
/// fact: the sentence a judge reads has to name what was seen. This is the same
/// derivation [`ToolSignals::severity`] is the maximum of — one walk with two
/// consumers, rather than two walks that could disagree about one window.
#[derive(Clone, Debug, PartialEq)]
pub struct ResultSeverity {
    pub severity: f32,
    /// Which `ERROR_PATTERNS` rows matched, in table order.
    pub patterns: Vec<&'static str>,
}

/// The per-result severities over the last `window` answered tool calls,
/// oldest first.
pub fn recent_severities(exchanges: &[Exchange], window: usize) -> Vec<ResultSeverity> {
    let answered: Vec<&Exchange> = exchanges
        .iter()
        .filter(|exchange| exchange.output.is_some())
        .collect();
    let start = answered.len().saturating_sub(window.max(1));
    answered[start..]
        .iter()
        .map(|exchange| {
            let output = exchange.output.as_deref().unwrap_or_default();
            classify_result(output)
        })
        .collect()
}

/// One result's severity, read the only way that is right in both directions.
///
/// The exit status comes from the header as a structured fact and the pattern
/// table runs over the body. See the module doc for why each naive alternative
/// is wrong.
pub fn classify_result(output: &str) -> ResultSeverity {
    let (mut severity, mut patterns) = classify_body(tool_output_body(output));
    if exec_exit_code(output).is_some_and(|code| code != 0) {
        // Upstream's own row for this, reached through the structured fact
        // instead of through a substring. Named identically so a dashboard
        // grouping on pattern names cannot tell the two routes apart — they are
        // the same observation, arriving by the only channel that carries it.
        if !patterns.contains(&"exit_nonzero") {
            patterns.push("exit_nonzero");
        }
        severity = severity.max(SOFT);
    }
    ResultSeverity { severity, patterns }
}

/// The pattern table over one piece of text, with no header handling at all.
///
/// Exposed for the callers that have already stripped, and kept separate from
/// [`classify_result`] so the table can be tested as a table.
pub fn classify_body(text: &str) -> (f32, Vec<&'static str>) {
    let lower = text.to_lowercase();
    let mut patterns = Vec::new();
    let mut severity = 0.0f32;
    for (name, sev, substrings) in ERROR_PATTERNS {
        if substrings.iter().any(|sub| lower.contains(sub)) {
            patterns.push(*name);
            severity = severity.max(*sev);
        }
    }
    (severity, patterns)
}

/// Consecutive clean results counting back from the most recent.
fn no_error_streak(exchanges: &[Exchange]) -> u32 {
    let mut streak = 0u32;
    for exchange in exchanges.iter().rev() {
        let Some(output) = exchange.output.as_deref() else {
            // A call still in flight is not a clean result and not a dirty one.
            // Skipping it keeps the streak about answers, the way
            // `ToolFailureStreak` refuses to count an unanswered call.
            continue;
        };
        if classify_result(output).severity > 0.0 {
            break;
        }
        streak += 1;
    }
    streak
}

fn detect_tests_passed(exchanges: &[Exchange], window: usize) -> bool {
    recent_bodies(exchanges, window).iter().any(|body| {
        let lower = body.to_lowercase();
        TEST_PASS_PHRASES.iter().any(|p| lower.contains(p))
            && !TEST_FAILURE_LITERAL.iter().any(|p| lower.contains(p))
            && !has_nonzero_failure_count(&lower)
    })
}

/// The stripped bodies of the last `window` answered calls, oldest first.
fn recent_bodies(exchanges: &[Exchange], window: usize) -> Vec<&str> {
    let answered: Vec<&str> = exchanges
        .iter()
        .filter_map(|exchange| exchange.output.as_deref().map(tool_output_body))
        .collect();
    let start = answered.len().saturating_sub(window.max(1));
    answered[start..].to_vec()
}

/// Whether `lower` contains a failure keyword preceded, modulo whitespace, by a
/// non-zero integer.
///
/// The whitespace rule is what makes `1 failed`, `1\nfailed` and `1  failed`
/// all trip while cargo's `0 failed`, go's `0 errors` and a mid-word `errored`
/// do not. Copied rather than rewritten with a regex: the crate is not a
/// dependency here, and the loop is the measured behaviour.
fn has_nonzero_failure_count(lower: &str) -> bool {
    for keyword in NUMERIC_FAILURE_KEYWORDS {
        let mut cursor = 0usize;
        while let Some(relative) = lower[cursor..].find(keyword) {
            let start = cursor + relative;
            let end = start + keyword.len();
            // A word boundary *after* the keyword, so "errored" is not a
            // failure-count site.
            let boundary_after = lower[end..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_alphanumeric());
            if boundary_after {
                let trimmed = lower[..start].trim_end_matches(char::is_whitespace);
                let digits: String = trimmed
                    .chars()
                    .rev()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if !digits.is_empty() && digits.chars().any(|digit| digit != '0') {
                    return true;
                }
            }
            cursor = end;
        }
    }
    false
}

/// What kind of work one tool call is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolCategory {
    Write,
    Edit,
    Read,
    Plan,
    Other,
}

/// Cumulative and windowed counters, filled in one pass.
#[derive(Default)]
struct Counts {
    write: u32,
    edit: u32,
    read: u32,
    todowrite: u32,
    recent_write: u32,
    recent_edit: u32,
    recent_read: u32,
    recent_todowrite: u32,
    pure_bash_streak: u32,
}

impl Counts {
    fn add(&mut self, category: ToolCategory, recent: bool) {
        let (total, windowed) = match category {
            ToolCategory::Write => (&mut self.write, &mut self.recent_write),
            ToolCategory::Edit => (&mut self.edit, &mut self.recent_edit),
            ToolCategory::Read => (&mut self.read, &mut self.recent_read),
            ToolCategory::Plan => (&mut self.todowrite, &mut self.recent_todowrite),
            // Counted only by the streak: "not one of the four" is the
            // definition of the build pit and has no total of its own upstream.
            ToolCategory::Other => return,
        };
        *total += 1;
        if recent {
            *windowed += 1;
        }
    }
}

fn classify_tool_call(name: &str, command: Option<String>) -> ToolCategory {
    let lower = name.to_lowercase();
    if WRITE_TOOL_NAMES.contains(&lower.as_str()) {
        return ToolCategory::Write;
    }
    if EDIT_TOOL_NAMES.contains(&lower.as_str()) {
        return ToolCategory::Edit;
    }
    if READ_TOOL_NAMES.contains(&lower.as_str()) {
        return ToolCategory::Read;
    }
    if PLAN_TOOL_NAMES.contains(&lower.as_str()) {
        return ToolCategory::Plan;
    }
    if BASH_TOOL_NAMES.contains(&lower.as_str())
        && let Some(command) = command
    {
        // Write/edit redirection trumps a read-like operand, first match wins.
        if BASH_WRITE_PATTERNS.iter().any(|p| command.contains(p)) {
            return ToolCategory::Write;
        }
        if BASH_EDIT_PATTERNS.iter().any(|p| command.contains(p)) {
            return ToolCategory::Edit;
        }
        if BASH_READ_PATTERNS.iter().any(|p| command.contains(p)) {
            return ToolCategory::Read;
        }
    }
    ToolCategory::Other
}

/// The shell command a call carries, lower-cased, when it carries one.
///
/// [`Exchange::arguments`] is the client's verbatim JSON *string* rather than a
/// parsed value — the log stores what arrived — so the port owes a parse
/// upstream did not. **Unparseable arguments yield `None`**, which lands the
/// call in `Other`: a call whose arguments this cannot read is a call whose
/// intent this does not know, and guessing a category from the tool name alone
/// is how a `bash` call that wrote a file gets counted as investigation.
fn command_of(arguments: &str) -> Option<String> {
    serde_json::from_str::<Value>(arguments)
        .ok()?
        .get("command")?
        .as_str()
        .map(str::to_lowercase)
}

// ─── the two signals this port earns ─────────────────────────────────────────

/// How bad a windowed result has to look before [`ErrorSeverity`] says so.
///
/// [`HARD`] rather than [`SOFT`], and the difference is what the signal is for:
/// [`SOFT`] is a bare non-zero exit, which an agent doing ordinary work
/// produces constantly — a `grep` that missed, a `test` that was false. Firing
/// on those would make this the noisiest signal in the set and the first one an
/// operator turns off. [`HARD`] is a *named* failure: a traceback, an import
/// error, a timeout, a missing file.
pub const ERROR_SEVERITY_THRESHOLD: f32 = HARD;

/// How many consecutive uncategorised calls are a build pit.
///
/// Four rather than three, deliberately above [`DEFAULT_RECENT_WINDOW`]: three
/// shell calls in a row is a build, a test run and a git status, which is a
/// description of working. The pattern worth naming is the one where nothing
/// has been read, written or edited for longer than a normal compile-run-check
/// cycle.
pub const PURE_BASH_STREAK_LENGTH: usize = 4;

/// A named failure in the recent tool output.
///
/// **Orthogonal to [`ToolFailureStreak`](super::ToolFailureStreak), and both
/// are kept.** They disagree in both directions, which is why neither replaces
/// the other:
///
/// - the streak asks [`reads_as_failure`](super::exchange::reads_as_failure),
///   which is anchored and structured — it catches `{"success": false}` and a
///   leading `Error:` that no pattern row matches, and it needs *consecutive*
///   failures;
/// - this asks the ported table, which is unanchored — it catches a
///   `traceback (most recent call last)` a thousand lines into an otherwise
///   chatty result, which the anchored check cannot see, and it fires on **one
///   bad result in the last three** rather than needing a run of them.
///
/// The genuinely new part is the window. A session that fails, half-recovers,
/// fails again is invisible to a streak and obvious here.
#[derive(Debug, Clone, Copy)]
pub struct ErrorSeverity {
    /// Severity at or above which this fires. See [`ERROR_SEVERITY_THRESHOLD`].
    pub threshold: f32,
    /// How many trailing answered calls to look across.
    pub window: usize,
}

impl Default for ErrorSeverity {
    fn default() -> Self {
        Self {
            threshold: ERROR_SEVERITY_THRESHOLD,
            window: DEFAULT_RECENT_WINDOW,
        }
    }
}

impl Signal for ErrorSeverity {
    fn kind(&self) -> SignalKind {
        SignalKind::ErrorSeverity
    }

    fn detect(&self, evidence: &Evidence<'_>) -> Option<String> {
        // The same derivation `ToolSignals::severity` is the maximum of, called
        // directly rather than through the struct: the sentence has to name
        // *what* was seen, and a single `f32` cannot say "traceback".
        let recent = recent_severities(&evidence.exchanges, self.window);
        if recent.is_empty() {
            return None;
        }
        let bad: Vec<&ResultSeverity> = recent
            .iter()
            .filter(|result| result.severity >= self.threshold)
            .collect();
        if bad.is_empty() {
            return None;
        }
        // Table order, deduplicated: two tracebacks in one window are one kind
        // of trouble, and a judge reading the same word twice learns nothing.
        let mut named: Vec<&str> = Vec::new();
        for pattern in bad.iter().flat_map(|result| result.patterns.iter()) {
            if !named.contains(pattern) {
                named.push(pattern);
            }
        }
        Some(format!(
            "{} of the last {} tool results carried a recognised failure ({})",
            bad.len(),
            recent.len(),
            named.join(", ")
        ))
    }
}

/// The build pit: consecutive calls that neither read, wrote, edited nor
/// planned.
///
/// Upstream's `pure_bash_streak`, as the one thing it is good for here. A run
/// of shell calls with no file touched between them is the shape of an agent
/// re-running a build and reading its output, and it is invisible to every
/// other signal in the set: the commands differ so it is not a repeat, two
/// names do not alternate so it is not ping-pong, and a build that fails to
/// compile answers with a body the anchored failure check often reads as clean.
#[derive(Debug, Clone, Copy)]
pub struct PureBashStreak {
    /// Consecutive uncategorised calls before this fires. See
    /// [`PURE_BASH_STREAK_LENGTH`].
    pub length: usize,
}

impl Default for PureBashStreak {
    fn default() -> Self {
        Self {
            length: PURE_BASH_STREAK_LENGTH,
        }
    }
}

impl Signal for PureBashStreak {
    fn kind(&self) -> SignalKind {
        SignalKind::PureBashStreak
    }

    fn detect(&self, evidence: &Evidence<'_>) -> Option<String> {
        if self.length == 0 {
            return None;
        }
        // Builds the whole struct to read one field, deliberately: the
        // alternative is a second copy of `classify_tool_call`'s walk that
        // could disagree with the ported one about what an `Other` call is, and
        // the streak is the field most likely to drift if it had its own
        // classifier. The cost is bounded on both sides — signals run only past
        // a gate that needs 20k billed tokens, and the walk is one pass with a
        // JSON parse per call over a session's exchanges.
        let streak = ToolSignals::from_exchanges(&evidence.exchanges).pure_bash_streak as usize;
        (streak >= self.length).then(|| {
            format!(
                "the last {streak} tool calls were shell or unrecognised tools, with no file \
                 read, written or edited among them"
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{Item, ItemContent, Role};
    use crate::validate::exchange::exchanges;

    /// The module's own source, so the attribution test reads what a reader
    /// would read rather than a constant that could be deleted with it.
    const SOURCE: &str = include_str!("tool_signals.rs");

    /// [`SOURCE`] up to (not including) `mod tests` itself.
    ///
    /// Asserting against the whole file is a tautology: this module's own
    /// assertions retype the rev, the path, and each refused name, so
    /// `SOURCE.contains(...)` is trivially satisfied by the test finding its
    /// own literal, no matter what the doc comment two hundred lines above
    /// says — confirmed by mutating one character of the rev in the doc
    /// comment and watching the test stay green, then deleting the entire
    /// `# Attribution` block with the same result. Slicing off `mod tests`
    /// closes that: every string below is checked only against the doc
    /// comment and code that precede the test module, so a mutation there
    /// (and only there) is what makes this go red.
    fn doc_and_code() -> &'static str {
        SOURCE.split("\n#[cfg(test)]").next().unwrap()
    }

    fn call(call_id: &str, name: &str, arguments: &str) -> Item {
        Item::tool_call(call_id, name, arguments)
    }

    fn result(call_id: &str, output: &str) -> Item {
        Item {
            role: Role::Tool,
            content: ItemContent::ToolResult {
                call_id: call_id.into(),
                output: output.into(),
            },
            response_id: None,
        }
    }

    /// A session of `(name, arguments, output)` triples, in order.
    fn session(calls: &[(&str, &str, Option<&str>)]) -> Vec<Exchange> {
        let mut items = Vec::new();
        for (index, (name, arguments, output)) in calls.iter().enumerate() {
            let id = format!("c{index}");
            items.push(call(&id, name, arguments));
            if let Some(output) = output {
                items.push(result(&id, output));
            }
        }
        exchanges(&items)
    }

    fn shell(command: &str) -> String {
        serde_json::json!({ "command": command }).to_string()
    }

    /// R4: provenance and revision, pinned so an edit of the prose around them
    /// cannot quietly retire either.
    ///
    /// Reads the file rather than a constant, for `prompt.rs`'s reason: the
    /// revision is the half that rots, and a citation nobody can observe going
    /// stale is a citation that will.
    #[test]
    fn the_attribution_names_the_source_the_revision_and_what_was_not_taken() {
        let doc_and_code = doc_and_code();
        assert!(doc_and_code.contains("NVIDIA Switchyard"));
        assert!(doc_and_code.contains("Apache-2.0"));
        assert!(doc_and_code.contains("053a61e2c43ba15f0772952ec3b3060c24b317f2"));
        assert!(doc_and_code.contains("crates/libsy/src/algorithms/util/tool_signals.rs"));
        // What was deliberately left behind, each named where a reader looking
        // for it will look. Checked against `doc_and_code`, not `SOURCE` — see
        // its doc comment for why the whole-file version is a tautology.
        for refused in [
            "pick_tier",
            "score_signal",
            "turn_depth",
            "compacted",
            "Request",
        ] {
            assert!(
                doc_and_code.contains(refused),
                "the attribution has to say that `{refused}` was not taken, or \
                 the next reader ports it a second time"
            );
        }
    }

    /// Every row of the error table, with a positive apiece.
    ///
    /// A table copied from another repository is exactly the kind of asset that
    /// rots silently — a row deleted in a merge changes no behaviour any other
    /// test observes — so each one is named here.
    #[test]
    fn every_error_pattern_row_has_a_positive_and_the_severities_rank() {
        for (text, name, severity) in [
            ("fatal: Out of memory", "oom", CRITICAL),
            ("MemoryError", "oom", CRITICAL),
            ("cannot allocate memory", "oom", CRITICAL),
            (
                "dial tcp: connection refused",
                "connection_refused",
                CRITICAL,
            ),
            (
                "ConnectionRefusedError: [Errno 111]",
                "connection_refused",
                CRITICAL,
            ),
            (
                "connect ECONNREFUSED 127.0.0.1:8080",
                "connection_refused",
                CRITICAL,
            ),
            ("Traceback (most recent call last):", "traceback", HARD),
            ("ModuleNotFoundError: no app", "import_error", HARD),
            ("ImportError: cannot import name", "import_error", HARD),
            ("no module named app", "import_error", HARD),
            ("bash: cargoo: command not found", "cmd_not_found", HARD),
            ("zsh: pytest: not found\n", "cmd_not_found", HARD),
            (
                "/usr/bin/env: 'python3.9': No such file",
                "cmd_not_found",
                HARD,
            ),
            ("AssertionError", "assertion", HARD),
            ("ValueError: invalid literal", "value_error", HARD),
            ("SyntaxError: unexpected EOF", "syntax_error", HARD),
            ("the request timed out", "timeout", HARD),
            ("TimeoutError", "timeout", HARD),
            ("timeout expired", "timeout", HARD),
            ("context deadline exceeded", "timeout", HARD),
            ("FileNotFoundError: config.toml", "no_such_file", HARD),
            (
                "open /etc/nope: no such file or directory",
                "no_such_file",
                HARD,
            ),
            ("file does not exist", "no_such_file", HARD),
            ("make: *** exit code 1", "exit_nonzero", SOFT),
            ("exit code 2", "exit_nonzero", SOFT),
            ("git returned exit status 1", "exit_nonzero", SOFT),
            ("subprocess returned non-zero", "exit_nonzero", SOFT),
            ("the child exited with code 3", "exit_nonzero", SOFT),
        ] {
            let (found, patterns) = classify_body(text);
            assert!(
                patterns.contains(&name),
                "`{text}` is the `{name}` row's own example and matched {patterns:?}"
            );
            assert!(
                found >= severity,
                "`{text}` scored {found}, below `{name}`'s {severity}"
            );
        }

        // The anchoring the trace-mined comment is about: a bare "does not
        // exist" is `ls` output and prose, and firing on it was measured worse.
        assert_eq!(classify_body("that option does not exist yet").0, 0.0);
        // Upstream's numbers, pinned: the three severities are a scale a
        // threshold is compared against, so moving one silently re-tunes every
        // signal built on it.
        assert_eq!((SOFT, HARD, CRITICAL), (0.3, 0.7, 1.0));
        let (severity, patterns) = classify_body("exit code 1\nMemoryError");
        assert_eq!(severity, CRITICAL);
        assert!(patterns.contains(&"oom") && patterns.contains(&"exit_nonzero"));
        // Case-insensitive, which is what makes one lower-cased table enough.
        assert_eq!(classify_body("TRACEBACK (MOST RECENT CALL LAST):").0, HARD);
        assert_eq!(classify_body("nothing wrong here").0, 0.0);
    }

    /// The split rule, as the two cases that make both naive readings wrong.
    #[test]
    fn the_exit_code_comes_from_the_header_and_the_patterns_from_the_body() {
        // Exit 0 with the substring in the header: upstream's unanchored row
        // would score SOFT on this, which is every successful shell call a
        // session ever makes.
        let clean =
            "Chunk ID: 1\nWall time: 0.4212 seconds\nProcess exited with code 0\nOutput:\nall good";
        assert_eq!(
            classify_result(clean),
            ResultSeverity {
                severity: 0.0,
                patterns: Vec::new()
            }
        );

        // Exit 1 with nothing on stdout: reading only the body loses it
        // entirely, which is the blindness this port must not re-create one
        // layer up from `reads_as_failure`.
        let silent =
            "Chunk ID: 1\nWall time: 0.0210 seconds\nProcess exited with code 1\nOutput:\n";
        assert_eq!(classify_result(silent).severity, SOFT);
        assert!(classify_result(silent).patterns.contains(&"exit_nonzero"));

        // A real exception in the body still outranks the soft exit, and the
        // pattern list names both.
        let hard = "Chunk ID: 1\nWall time: 3.1400 seconds\nProcess exited with code 101\nOutput:\nTraceback (most recent call last):\n";
        let classified = classify_result(hard);
        assert_eq!(classified.severity, HARD);
        assert!(classified.patterns.contains(&"traceback"));
        assert!(classified.patterns.contains(&"exit_nonzero"));

        // A body that merely prints the phrase is still the tool's own text,
        // and the row still matches it — the split moves where the *header* is
        // read, it does not weaken the table.
        assert_eq!(
            classify_result("Wall time: 0.1 seconds\nOutput:\nmake: exited with code 4").severity,
            SOFT
        );
    }

    /// The window, on both of the sequences it applies to.
    #[test]
    fn the_window_bounds_severity_and_the_recent_counts_but_not_the_totals() {
        // An old critical error and three clean results after it: the severity
        // has decayed out of a window of three, and is still visible at five.
        let calls = session(&[
            ("bash", &shell("./run"), Some("MemoryError")),
            ("read", "{}", Some("ok")),
            ("read", "{}", Some("ok")),
            ("read", "{}", Some("ok")),
        ]);
        assert_eq!(ToolSignals::with_window(&calls, 3).severity, 0.0);
        assert_eq!(ToolSignals::with_window(&calls, 5).severity, CRITICAL);
        // The streak counts back from the most recent and is not windowed.
        assert_eq!(ToolSignals::with_window(&calls, 3).no_error_streak, 3);

        // Totals count everything; the recent counts see only the tail.
        let mixed = session(&[
            ("edit", "{}", Some("ok")),
            ("edit", "{}", Some("ok")),
            ("write", "{}", Some("ok")),
            ("read", "{}", Some("ok")),
            ("update_plan", "{}", Some("ok")),
        ]);
        let signals = ToolSignals::with_window(&mixed, 3);
        assert_eq!((signals.edit_count, signals.recent_edit_count), (2, 0));
        assert_eq!((signals.write_count, signals.recent_write_count), (1, 1));
        assert_eq!((signals.read_count, signals.recent_read_count), (1, 1));
        assert_eq!(
            (signals.todowrite_count, signals.recent_todowrite_count),
            (1, 1)
        );
        // The default is the window the fields are documented against.
        assert_eq!(
            ToolSignals::from_exchanges(&mixed),
            ToolSignals::with_window(&mixed, DEFAULT_RECENT_WINDOW)
        );
        assert_eq!(DEFAULT_RECENT_WINDOW, 3);

        // Nothing at all is all zeroes rather than a panic, which is what lets
        // a caller read the fields unconditionally.
        assert_eq!(ToolSignals::from_exchanges(&[]), ToolSignals::default());
    }

    /// Every tool-name table and every bash pattern list, with a positive each.
    #[test]
    fn each_tool_table_and_bash_pattern_lands_in_its_category() {
        for (name, arguments) in [
            ("write", "{}"),
            ("create_file", "{}"),
            ("new_file", "{}"),
            ("write_file", "{}"),
            ("Write", "{}"), // matched on the lower-cased name
            ("bash", &shell("cat > f")),
            ("bash", &shell("cat >> f")),
            ("bash", &shell("echo > f")),
            ("bash", &shell("echo >> f")),
            ("bash", &shell("tee f")),
            ("bash", &shell("printf > f")),
            ("bash", &shell("printf >> f")),
            ("shell_command", &shell("x > /tmp/f")),
            ("shell", &shell("x >> /tmp/f")),
            ("local_shell_call", &shell("cat << 'EOF'")),
            ("terminal", &shell("cat <<EOF")),
            ("bash", &shell("cat <<'EOF'")),
            ("bash", &shell("cat << EOF")),
        ] {
            let signals = ToolSignals::from_exchanges(&session(&[(name, arguments, Some("ok"))]));
            assert_eq!(signals.write_count, 1, "`{name}` {arguments} is a write");
        }

        for (name, arguments) in [
            ("edit", "{}"),
            ("multiedit", "{}"),
            ("notebookedit", "{}"),
            ("str_replace", "{}"),
            ("str_replace_based_edit_tool", "{}"),
            ("text_editor", "{}"),
            ("patch", "{}"),
            ("bash", &shell("sed -i s/a/b/ f")),
            ("bash", &shell("sed --in-place s/a/b/ f")),
            ("bash", &shell("awk -i inplace '{print}' f")),
            ("bash", &shell("awk 'inplace=1' f")),
            ("bash", &shell("patch f < d")),
            ("bash", &shell("patch -p1 < d")),
            ("bash", &shell("perl -i -pe s/a/b/ f")),
            ("bash", &shell("perl -p -i -e s/a/b/ f")),
            ("bash", &shell("perl -pi -e s/a/b/ f")),
        ] {
            let signals = ToolSignals::from_exchanges(&session(&[(name, arguments, Some("ok"))]));
            assert_eq!(signals.edit_count, 1, "`{name}` {arguments} is an edit");
        }

        for (name, arguments) in [
            ("read", "{}"),
            ("view", "{}"),
            ("read_file", "{}"),
            ("search_files", "{}"),
            ("bash", &shell("cat /etc/hosts")),
            ("bash", &shell("cat ./f")),
            ("bash", &shell("cat ../f")),
            ("bash", &shell("grep needle f")),
            ("bash", &shell("ls src")),
            ("bash", &shell("ls -la")),
            ("bash", &shell("find . -name x")),
            ("bash", &shell("head f")),
            ("bash", &shell("tail f")),
            ("bash", &shell("wc -l f")),
            ("bash", &shell("diff a b")),
            ("bash", &shell("which cargo")),
            ("bash", &shell("ps aux")),
            ("bash", &shell("df -h")),
            ("bash", &shell("du -sh .")),
            ("bash", &shell("stat f")),
            ("bash", &shell("file f")),
            ("bash", &shell("less f")),
            ("bash", &shell("more f")),
        ] {
            let signals = ToolSignals::from_exchanges(&session(&[(name, arguments, Some("ok"))]));
            assert_eq!(signals.read_count, 1, "`{name}` {arguments} is a read");
        }

        for name in ["todowrite", "todo_write", "todo", "update_plan"] {
            let signals = ToolSignals::from_exchanges(&session(&[(name, "{}", Some("ok"))]));
            assert_eq!(signals.todowrite_count, 1, "`{name}` is planning");
        }

        // First match wins, and the order is the ruling: redirection beats the
        // read-like operand, so `grep … > out` is a write and not a read.
        let redirecting = ToolSignals::from_exchanges(&session(&[(
            "bash",
            &shell("grep x f > /tmp/o"),
            Some("ok"),
        )]));
        assert_eq!((redirecting.write_count, redirecting.read_count), (1, 0));
        let in_place = ToolSignals::from_exchanges(&session(&[(
            "bash",
            &shell("sed -i s/a/b/ f; ls"),
            Some("ok"),
        )]));
        assert_eq!((in_place.edit_count, in_place.read_count), (1, 0));

        // Everything else is `Other`: an unrecognised tool, a shell call whose
        // command matches nothing, and a shell call whose arguments do not
        // parse at all.
        for (name, arguments) in [
            ("cargo", "{}"),
            ("bash", &shell("cargo build")),
            ("bash", "not json"),
            ("bash", r#"{"cmd":"ls"}"#),
        ] {
            let signals = ToolSignals::from_exchanges(&session(&[(name, arguments, Some("ok"))]));
            assert_eq!(
                (
                    signals.write_count,
                    signals.edit_count,
                    signals.read_count,
                    signals.todowrite_count,
                    signals.pure_bash_streak
                ),
                (0, 0, 0, 0, 1),
                "`{name}` {arguments} is uncategorised work"
            );
        }
    }

    /// The build pit: consecutive uncategorised calls, counting back from the
    /// end and ending at the first thing that read, wrote, edited or planned.
    #[test]
    fn the_pure_bash_streak_is_trailing_and_any_real_work_ends_it() {
        let pit = session(&[
            ("read", "{}", Some("ok")),
            ("bash", &shell("cargo build"), Some("err")),
            ("bash", &shell("cargo build"), Some("err")),
            ("bash", &shell("cargo build"), Some("err")),
            ("bash", &shell("cargo build"), Some("err")),
        ]);
        assert_eq!(ToolSignals::from_exchanges(&pit).pure_bash_streak, 4);

        // One edit at the end and the streak is zero: the agent is producing
        // again, which is the whole distinction.
        let mut working = pit.clone();
        working.push(Exchange {
            call_id: "c9".into(),
            name: "edit".into(),
            arguments: "{}".into(),
            output: Some("ok".into()),
            failed: false,
        });
        assert_eq!(ToolSignals::from_exchanges(&working).pure_bash_streak, 0);

        // An edit in the middle bounds it rather than clearing it.
        let interrupted = session(&[
            ("bash", &shell("cargo build"), Some("err")),
            ("edit", "{}", Some("ok")),
            ("bash", &shell("cargo build"), Some("err")),
            ("bash", &shell("cargo build"), Some("err")),
        ]);
        assert_eq!(
            ToolSignals::from_exchanges(&interrupted).pure_bash_streak,
            2
        );

        // An unanswered call still counts: the streak is about what the agent
        // is *doing*, and a call in flight has already been made.
        let in_flight = session(&[
            ("bash", &shell("cargo build"), Some("err")),
            ("bash", &shell("cargo build"), None),
        ]);
        assert_eq!(ToolSignals::from_exchanges(&in_flight).pure_bash_streak, 2);
    }

    /// `tests_passed`, including the whitespace rule that keeps `0 failed` from
    /// vetoing a clean run.
    #[test]
    fn a_passing_run_is_recognised_and_a_counted_failure_vetoes_it() {
        for passing in [
            "test result: ok. 42 passed; 0 failed; 0 ignored",
            "42 passed in 3.10s",
            "all tests passed",
            "tests passed",
            "test ok",
            "passed.\n",
            "tests pass",
            "\nok  github.com/x/y 0.3s",
            "✓ renders the header",
            // Through codex's exec wrapper, which the checked non-finding says
            // changes nothing: every digit in the header is followed by
            // ` seconds`, a newline or `Output:`, never by a failure keyword.
            "Chunk ID: 1\nWall time: 1.0000 seconds\nProcess exited with code 0\nOutput:\ntest result: ok. 42 passed; 0 failed; 0 ignored",
        ] {
            let calls = session(&[("bash", &shell("cargo test"), Some(passing))]);
            assert!(
                ToolSignals::from_exchanges(&calls).tests_passed,
                "`{passing}` is a passing run"
            );
        }

        for not_passing in [
            "3 passed; 1 failed",
            "12 passed, 2 failures",
            "10 passed; 3 errors",
            "ok 4 passed\nerror: link failed",
            "✗ renders the header",
            "fatal: not a git repository",
            "1 passed\nAssertionError",
            "compiling",
        ] {
            let calls = session(&[("bash", &shell("cargo test"), Some(not_passing))]);
            assert!(
                !ToolSignals::from_exchanges(&calls).tests_passed,
                "`{not_passing}` is not a clean run"
            );
        }

        // The whitespace rule, directly: a zero count never trips, a non-zero
        // one always does across any whitespace, and a keyword inside a word is
        // not a count site.
        assert!(!has_nonzero_failure_count("0 failed"));
        assert!(!has_nonzero_failure_count("0 errors"));
        assert!(!has_nonzero_failure_count("the run errored"));
        assert!(has_nonzero_failure_count("1 failed"));
        assert!(has_nonzero_failure_count("1\nfailed"));
        assert!(has_nonzero_failure_count("1  failed"));
        assert!(has_nonzero_failure_count("10 failures"));
    }

    fn evidence(exchanges: Vec<Exchange>) -> Evidence<'static> {
        Evidence {
            exchanges,
            turn_tokens: &[],
        }
    }

    /// `ErrorSeverity`: what makes it fire, what keeps it quiet, and the two
    /// places it disagrees with the failure streak.
    #[test]
    fn error_severity_fires_on_one_hard_result_in_the_window_and_not_on_soft_noise() {
        let signal = ErrorSeverity::default();

        // One hard error among three results. A streak would need all three.
        let fact = signal
            .detect(&evidence(session(&[
                ("bash", &shell("cargo build"), Some("ok")),
                (
                    "bash",
                    &shell("pytest"),
                    Some("collecting ...\nTraceback (most recent call last):\n  File x"),
                ),
                ("read", "{}", Some("fn main() {}")),
            ])))
            .expect("a traceback in the window is a named failure");
        assert!(fact.contains("1 of the last 3"), "{fact}");
        assert!(fact.contains("traceback"), "{fact}");
        // The `SignalFired::fact` rule: an observation, never advice.
        assert!(
            !fact.contains("consider") && !fact.contains("should"),
            "a fact, never a suggestion: {fact}"
        );

        // Quiet on soft noise: a `grep` that missed and a `test` that was false
        // are a working agent, and firing on them is how a signal gets turned
        // off.
        assert_eq!(
            signal.detect(&evidence(session(&[(
                "shell_command",
                &shell("grep needle f"),
                Some(
                    "Chunk ID: 1\nWall time: 0.0100 seconds\nProcess exited with code 1\nOutput:\n"
                ),
            )]))),
            None,
            "a bare non-zero exit is SOFT and below the threshold on purpose"
        );

        // Quiet on a clean session, and on one with no results at all.
        assert_eq!(
            signal.detect(&evidence(session(&[("read", "{}", Some("fn main() {}"))]))),
            None
        );
        assert_eq!(
            signal.detect(&evidence(session(&[("read", "{}", None)]))),
            None
        );
        assert_eq!(signal.detect(&evidence(Vec::new())), None);

        // The window: the same traceback four results back has decayed out.
        assert_eq!(
            signal.detect(&evidence(session(&[
                (
                    "bash",
                    &shell("pytest"),
                    Some("Traceback (most recent call last):")
                ),
                ("edit", "{}", Some("ok")),
                ("edit", "{}", Some("ok")),
                ("edit", "{}", Some("ok")),
            ]))),
            None
        );

        // The disagreement that is the reason both signals are kept: a
        // mid-body traceback that `reads_as_failure`'s anchored markers cannot
        // see, so the streak is quiet on three consecutive ones and this is not.
        let chatty = session(&[
            (
                "bash",
                &shell("pytest"),
                Some("collected 3 items\nTraceback (most recent call last):"),
            ),
            (
                "bash",
                &shell("pytest"),
                Some("collected 4 items\nTraceback (most recent call last):"),
            ),
            (
                "bash",
                &shell("pytest"),
                Some("collected 5 items\nTraceback (most recent call last):"),
            ),
        ]);
        assert!(chatty.iter().all(|exchange| !exchange.failed));
        assert!(signal.detect(&evidence(chatty)).is_some());
    }

    /// The signal's wording and the struct's number are one derivation.
    ///
    /// Two walks over one window is how a brief comes to say something the
    /// field disagrees with, which is the failure `Evidence` itself was
    /// introduced to prevent.
    #[test]
    fn the_signal_and_the_field_never_disagree_about_the_window() {
        for calls in [
            session(&[(
                "bash",
                &shell("pytest"),
                Some("Traceback (most recent call last):"),
            )]),
            session(&[
                ("bash", &shell("run"), Some("MemoryError")),
                ("read", "{}", Some("ok")),
            ]),
            session(&[("read", "{}", Some("ok"))]),
        ] {
            let windowed = recent_severities(&calls, DEFAULT_RECENT_WINDOW)
                .iter()
                .fold(0.0f32, |worst, result| worst.max(result.severity));
            assert_eq!(ToolSignals::from_exchanges(&calls).severity, windowed);
            assert_eq!(
                ErrorSeverity::default().detect(&evidence(calls)).is_some(),
                windowed >= ERROR_SEVERITY_THRESHOLD
            );
        }
    }

    /// `PureBashStreak`: the build pit, and the edit that ends it.
    #[test]
    fn the_build_pit_fires_at_four_and_one_file_touched_clears_it() {
        let signal = PureBashStreak::default();
        assert_eq!(PURE_BASH_STREAK_LENGTH, 4);

        let pit = session(&[
            ("bash", &shell("cargo build"), Some("error[E0433]")),
            ("bash", &shell("cargo build"), Some("error[E0433]")),
            ("bash", &shell("cargo build"), Some("error[E0433]")),
            ("bash", &shell("cargo build"), Some("error[E0433]")),
        ]);
        let fact = signal
            .detect(&evidence(pit.clone()))
            .expect("four builds with nothing edited between them is the pit");
        assert!(fact.contains("the last 4 tool calls"), "{fact}");
        assert!(
            !fact.contains("consider") && !fact.contains("should"),
            "a fact, never a suggestion: {fact}"
        );

        // Three is a compile-run-check cycle, which is working.
        assert_eq!(signal.detect(&evidence(pit[..3].to_vec())), None);

        // Four builds *with* an edit at the end: the agent is producing again.
        let mut recovered = pit.clone();
        recovered.push(Exchange {
            call_id: "c9".into(),
            name: "edit".into(),
            arguments: "{}".into(),
            output: Some("ok".into()),
            failed: false,
        });
        assert_eq!(signal.detect(&evidence(recovered)), None);

        // A read counts as touching a file too — the pit is about producing
        // nothing, not about running shells.
        let mut reading = pit;
        reading.push(Exchange {
            call_id: "c9".into(),
            name: "bash".into(),
            arguments: shell("grep needle src/lib.rs"),
            output: Some("ok".into()),
            failed: false,
        });
        assert_eq!(signal.detect(&evidence(reading)), None);

        // A length of zero is off rather than always-on, the way
        // `ToolFailureStreak` treats it.
        assert_eq!(
            PureBashStreak { length: 0 }.detect(&evidence(session(&[(
                "bash",
                &shell("cargo build"),
                Some("ok")
            )]))),
            None
        );
    }

    /// The clean streak, which is the field a windowed severity cannot express.
    #[test]
    fn the_clean_streak_counts_back_to_the_first_result_with_any_severity() {
        let recovering = session(&[
            ("bash", &shell("cargo build"), Some("SyntaxError: bad")),
            ("edit", "{}", Some("ok")),
            ("bash", &shell("cargo build"), Some("Finished dev profile")),
        ]);
        assert_eq!(ToolSignals::from_exchanges(&recovering).no_error_streak, 2);

        let broken = session(&[
            ("edit", "{}", Some("ok")),
            ("bash", &shell("cargo build"), Some("SyntaxError: bad")),
        ]);
        assert_eq!(ToolSignals::from_exchanges(&broken).no_error_streak, 0);

        // An unanswered call is skipped rather than counted or breaking, which
        // is the one place this deviates from upstream — upstream walks result
        // texts and has no in-flight case. Mirrors `ToolFailureStreak`'s
        // refusal to read a call still in flight as a failure: the last call of
        // a running turn would otherwise reset the streak on every turn.
        let in_flight = session(&[
            ("edit", "{}", Some("ok")),
            ("edit", "{}", Some("ok")),
            ("bash", &shell("cargo build"), None),
        ]);
        assert_eq!(ToolSignals::from_exchanges(&in_flight).no_error_streak, 2);

        // A soft exit is severity too: the streak is "nothing at all went
        // wrong", not "nothing serious went wrong".
        let soft = session(&[(
            "shell_command",
            &shell("./ci"),
            Some("Chunk ID: 1\nWall time: 0.1000 seconds\nProcess exited with code 1\nOutput:\n"),
        )]);
        assert_eq!(ToolSignals::from_exchanges(&soft).no_error_streak, 0);
    }
}
