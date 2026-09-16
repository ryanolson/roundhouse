// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Refutes M11.3 review finding F25.
//!
//! F25 claims: `topham` with stdout redirected to a file/pipe but a
//! controlling tty present (i.e. stdin still a tty) opens the full TUI
//! instead of refusing, because `crossterm`'s `enable_raw_mode` and
//! `terminal::size` fall back to `/dev/tty` when stdin is not itself a tty --
//! so `try_init` only fails when *no* controlling terminal exists at all,
//! not merely when stdout is redirected. That contradicts the stronger claim
//! at `agent-docs/PLAN-anthropic-messages.md:736-737`: "a piped `topham` now
//! refuses naming the subcommands and writes nothing to stdout".
//!
//! **Why this needs a real pty and a real subprocess, not a unit test**:
//! `tui::run`'s refusal path is exercised in-process nowhere in this crate's
//! existing suite (`tui/tests.rs` tests `update`/`apply` as pure functions,
//! never `run` itself -- see that module's own doc on why `event_loop` is
//! the untested remainder). `cargo test` itself gives every test process
//! pipes, not a tty, on stdin -- so a plain `Command::new(topham_bin)` run
//! under `cargo test` exercises the *documented* piped-refusal path (no
//! controlling terminal at all) but can never reach the scenario F25
//! describes (a controlling terminal that exists but is not what stdout
//! points at). Proving F25 needs a subprocess whose stdin is a genuine
//! pty and whose stdout is a genuine file -- `f25_pty_helper.py` builds
//! exactly that via `pty.fork()`, since `std::process::Command` can hand a
//! child pipes or inherited fds but never a *new* controlling terminal.

use std::path::PathBuf;
use std::process::Command;

/// A scratch directory, per the house pattern: the temp dir plus a UUID.
fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("topham-f25-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("a scratch directory");
    root
}

/// Runs `topham` (no subcommand) inside a real pty via the Python helper,
/// with a controlling terminal on stdin and stdout/stderr redirected to real
/// files. Returns `(stdout bytes, stderr bytes, helper's own stdout line)`.
///
/// The helper self-bounds: it kills the pty child and reports `TIMED_OUT`
/// after 8s if `q` never lands, so a hang in `topham` itself surfaces as a
/// prompt assertion failure here rather than as this test never returning
/// -- the crate-wide `timeout 300` wrapper this suite always runs under
/// (see CLAUDE.md) is the outer bound.
fn run_topham_under_pty() -> (Vec<u8>, Vec<u8>, String) {
    let bin = env!("CARGO_BIN_EXE_topham");
    let helper = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/f25_pty_helper.py");
    let root = scratch("piped-refusal");
    let out_path = root.join("out.txt");
    let err_path = root.join("err.txt");
    let config_home = root.join("config");
    let data_home = root.join("data");

    let output = Command::new("python3")
        .arg(helper)
        .arg(bin)
        .arg(&out_path)
        .arg(&err_path)
        .arg(&config_home)
        .arg(&data_home)
        .output()
        .expect(
            "python3 with the `pty` module is required to allocate a real controlling terminal \
             for this scenario -- see the module doc for why std::process::Command alone cannot",
        );

    let helper_stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        !helper_stdout.contains("TIMED_OUT"),
        "the pty helper had to kill `topham` after 8s without it exiting on its own -- it did \
         not merely fail to refuse, it hung with the alternate screen open: helper stderr = {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let out_bytes = std::fs::read(&out_path).unwrap_or_default();
    let err_bytes = std::fs::read(&err_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&root);
    (out_bytes, err_bytes, helper_stdout)
}

/// Control: this pty scaffolding is not simply broken in a way that makes
/// every assertion vacuous.
///
/// "stdout is empty" is now the *passing* answer, so emptiness can no longer
/// double as evidence that the harness ran anything. What proves it here is
/// that the child exited on its own with a code of its own choosing -- not
/// `127`, which is what the helper's own `os._exit` after a failed `execv`
/// reports -- and that it wrote something to *one* of the two streams.
#[test]
fn control_the_pty_helper_actually_launches_topham() {
    let (stdout, stderr, helper_stdout) = run_topham_under_pty();
    assert!(
        helper_stdout.starts_with("EXIT="),
        "control failed: the helper did not report an exit at all: {helper_stdout}"
    );
    assert_ne!(
        helper_stdout, "EXIT=127",
        "control failed: the helper could not exec topham, so neither file means anything"
    );
    assert!(
        !stdout.is_empty() || !stderr.is_empty(),
        "control failed: topham wrote nothing to either stream under the pty -- the helper is \
         not exercising the binary the way this check assumes (helper reported: {helper_stdout})"
    );
}

/// F25's claim, proven directly: with a controlling tty present but stdout
/// redirected to a real file, `topham` (no subcommand) still opens the full
/// TUI and writes ratatui's alternate-screen escape sequences to that file,
/// rather than refusing per `TuiError::Terminal` and writing nothing --
/// contradicting `agent-docs/PLAN-anthropic-messages.md:736-737`'s "a piped
/// `topham` now refuses ... and writes nothing to stdout".
///
/// Failed until the fix: `out.txt` held 47 bytes beginning with the
/// `EnterAlternateScreen` sequence `\x1b[?1049h`, exactly as F25's own
/// mechanism section reports. `run` now asks *stdout* whether it is a terminal
/// before `try_init` gets the chance to answer with `/dev/tty`.
#[test]
fn f25_piped_stdout_with_a_controlling_tty_still_writes_nothing_to_stdout() {
    let (stdout, stderr, _helper_stdout) = run_topham_under_pty();

    assert!(
        stdout.is_empty(),
        "F25: expected the piped-refusal contract (nothing on stdout) to hold even though a \
         controlling tty is present, but topham wrote {} bytes to stdout, starting with {:?}",
        stdout.len(),
        String::from_utf8_lossy(&stdout[..stdout.len().min(16)])
    );
    let stderr_text = String::from_utf8_lossy(&stderr);
    assert!(
        stderr_text.contains("topham plan") || stderr_text.contains("topham launch"),
        "F25: expected a refusal on stderr naming the plan/launch/relay subcommands, got: {} \
         bytes = {stderr_text:?}",
        stderr.len()
    );
}
