// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The fixtures every suite that builds a launch needs, in one place.
//!
//! Four suites were each declaring their own turn key, their own deployment
//! root, their own uuid-suffixed scratch directory and their own two-XDG-home
//! environment (F17), so a change to the *shape* of a fixture — a longer key, a
//! third directory the resolvers read — was four edits that had to agree, and
//! the way they stop agreeing is one file being left behind and its suite
//! quietly testing a launch nobody builds any more.
//!
//! **The fixture environment is deliberately spare.** It carries the turn key,
//! the two directories this crate resolves paths out of, and a `PATH`; it
//! carries no `HOME` and no `CLAUDE_CONFIG_DIR`, so a suite that does not opt
//! into a settings file reads none — including the operator's own, which is
//! exactly what a unit suite must never touch.
//!
//! `cfg(test)` and never compiled into the binary: it is a test fixture, not a
//! seam. The seams this crate really has ([`crate::launch::Launcher`],
//! [`crate::env::EnvMap`]) are in the library because production code goes
//! through them.

use std::path::{Path, PathBuf};

use crate::env::EnvMap;
use crate::relay::{RELAY_PROGRAM, RelayError};

/// A well-shaped turn key, in the house fixture form (`tests/common`'s `key`).
pub const TURN_KEY: &str = "rh_turn_liveAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// The deployment every fixture profile names.
pub const ROOT: &str = "http://127.0.0.1:8080";

/// A directory of this test's own, under the system temp root.
///
/// Uuid-suffixed rather than named for the test, because the suite runs in
/// parallel and two runs of it can overlap: a fixed name is a directory two
/// tests write the same generated `config.toml` into and one of them then reads
/// the other's.
pub fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("topham-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("a scratch directory");
    root
}

/// The operator's environment as the fixtures assume it: the key exported, the
/// two homes fixed, and whatever the case adds on top.
///
/// The directories are *fixed literals* rather than real ones because
/// resolution touches no filesystem — a case that needs the paths to exist
/// overrides `XDG_DATA_HOME` with a [`scratch`] of its own.
pub fn env(extra: &[(&str, &str)]) -> EnvMap {
    let mut env = EnvMap::from([
        ("ROUNDHOUSE_API_KEY".to_string(), TURN_KEY.to_string()),
        ("XDG_CONFIG_HOME".to_string(), "/op/config".to_string()),
        ("XDG_DATA_HOME".to_string(), "/op/data".to_string()),
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
    ]);
    for (name, value) in extra {
        env.insert((*name).to_string(), (*value).to_string());
    }
    env
}

// ---------------------------------------------------------------------------
// The dry-run double, which two suites spawn
// ---------------------------------------------------------------------------

/// What a `nemo-relay run --dry-run` prints when it resolved [`ROOT`] — the
/// report a chained launch of a fixture profile is supposed to get back.
pub const AIMED_HERE: &str = "\
agent = claude
gateway_url = http://127.0.0.1:45169
openai_base_url = https://api.openai.com/v1
anthropic_base_url = http://127.0.0.1:8080
anthropic_auth = unset
";

/// The same report with the Anthropic upstream somewhere else: what a system
/// `/etc/nemo-relay/config.toml` re-aiming the run looks like from outside.
pub const RE_AIMED: &str = "\
agent = claude
gateway_url = http://127.0.0.1:45169
openai_base_url = https://api.openai.com/v1
anthropic_base_url = http://10.0.0.9:8080
anthropic_auth = unset
";

/// A shell script standing in for `nemo-relay run --dry-run`, written into
/// `dir` under the name a `PATH` lookup would find.
///
/// It prints `report` and exits, which is the whole of the contract the
/// preflight depends on. Two things it also does, and they are the point: it
/// echoes back the argv it was given (`<dir>/argv`) and every variable it can
/// see (`<dir>/env`), so a preflight that forgot `--dry-run` or that leaked the
/// operator's environment is visible in the output rather than only in a
/// behaviour nobody checks.
///
/// **Builtins only, and the recording paths baked in absolute.** The preflight
/// clears the environment and hands the child exactly the `PATH` it was given,
/// which for a double resolved *through* that `PATH` is the double's own
/// directory alone — so `cat`, `dirname` and every other external would be this
/// fixture quietly testing that `/bin` is on a `PATH` the isolation says it is
/// not. `env` is the one exception and is allowed to fail: the suite that reads
/// its dump is the one that runs with a real `PATH`.
pub fn dry_run_double(dir: &Path, report: &str) -> PathBuf {
    let path = dir.join(RELAY_PROGRAM);
    let lines: String = report
        .lines()
        .map(|line| format!("echo '{line}'\n"))
        .collect();
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s' \"$*\" > '{argv}'\nenv > '{env}' 2>/dev/null\n{lines}",
            argv = dir.join("argv").display(),
            env = dir.join("env").display(),
        ),
    )
    .expect("the double");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("the double is executable");
    }
    path
}

/// The same double, put where the preflight's own `PATH` lookup finds it.
///
/// A directory added to `PATH` rather than an injected mock, because resolving
/// [`RELAY_PROGRAM`] through the `PATH` a caller hands down is part of what the
/// screen's precheck is being tested for. The `PATH` is *replaced*, not
/// extended: a real `nemo-relay` on the box would otherwise answer instead.
pub fn relay_double_on_path(env: &mut EnvMap, root: &Path, report: &str) -> PathBuf {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("a directory for the double");
    let double = dry_run_double(&bin, report);
    env.insert("PATH".to_string(), bin.display().to_string());
    double
}

/// Run something that spawns the double, retrying past `ETXTBSY`.
///
/// **Not a flaky-test papering-over; a property of writing an executable in a
/// multi-threaded harness.** Creating the double gives this thread a write
/// handle to it, and any *other* test thread that forks in the window before it
/// is dropped inherits that handle — so until that unrelated child reaches its
/// own `exec`, the kernel refuses to execute the file with "Text file busy".
/// Reproduced at roughly one run in eight of the relay suite.
///
/// Retried rather than avoided because the alternatives are worse: running the
/// double through `sh <script>` would stop testing the program string `--relay`
/// actually passes, and bending `preflight`'s signature to accept an
/// interpreter would be production API shaped by a test. Every other error is
/// returned untouched, so a real refusal is never retried into a pass.
pub fn past_text_file_busy<T>(
    mut attempt: impl FnMut() -> Result<T, RelayError>,
) -> Result<T, RelayError> {
    for _ in 0..1_000 {
        match attempt() {
            Err(RelayError::PreflightSpawn { source, .. })
                if source.kind() == std::io::ErrorKind::ExecutableFileBusy =>
            {
                std::thread::yield_now();
            }
            outcome => return outcome,
        }
    }
    panic!("the double stayed `Text file busy` for a thousand attempts, which is not the race");
}
