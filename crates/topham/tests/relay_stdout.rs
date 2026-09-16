// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Refutes M11.3 review finding F24.
//!
//! F24's claim: the fix keeping topham's own banner off stdout during a
//! chained relay launch (`&mut std::io::stderr()` at `cli.rs:169`) is guarded
//! only by the three-binary `claude_e2e` closure test, because `cli::run`
//! hard-codes [`ExecLauncher`] for `Command::Relay`, so nothing in the
//! *default* suite drives that dispatch arm at all — `cli/tests.rs` only
//! exercises `Command::Mint`, and `relay/tests.rs` exercises `relay::run`
//! directly through an injected [`RecordingLauncher`], never through
//! `cli::run`'s hard-coded [`ExecLauncher`].
//!
//! Both halves check out by inspection (`grep -rn "Command::Relay"
//! crates/topham/src crates/topham/tests` finds only the dispatch arm itself,
//! never a call site), but the reason is worth pinning as a test rather than
//! left as a reading: [`ExecLauncher::launch`] calls `Command::exec()`, which
//! **replaces the calling process image** on success. Driving `cli::run`'s
//! `Relay` arm in-process past a passing preflight would `execve` over the
//! test binary itself — there is no in-process way to observe "what did this
//! process's own stdout carry", because the process asking the question stops
//! existing. The only way to observe it is what this file does: spawn a real
//! *child* `topham` process (via `CARGO_BIN_EXE_topham`, so cargo guarantees
//! it is freshly built) and read back what its stdout carried after it
//! exec'd.
//!
//! That does need an out-of-process integration test — F24's "more expensive
//! than the in-process unit tests" holds — but it does **not** need a real
//! `claude` or `nemo-relay` binary the way the gated `claude_e2e` closure
//! test does: a `relay_program` double distinguishing `--dry-run` (the
//! preflight) from the real invocation (what `ExecLauncher` execs into) is
//! enough to observe the same stdout/stderr split `claude_e2e` catches,
//! without `--include-ignored` or either pinned binary. So the claim is
//! right about what exists today; the "only guard needs claude, nemo-relay
//! ... --include-ignored" reads as if no cheaper guard were reachable, and
//! this file is that cheaper guard.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use topham::env::EnvMap;
use topham::profile::{Agent, Profile, Topology};

const ROOT: &str = "http://127.0.0.1:8080";
const TURN_KEY: &str = "rh_turn_liveAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// What the double prints on the real (non-`--dry-run`) invocation —
/// standing in for the agent's own machine-readable stdout, the contract
/// `relay.rs`'s module doc calls out (`claude -p --output-format json`
/// prints one document and nothing else).
const CHILD_STDOUT: &str = "F24-CHILD-STDOUT-MARKER-6f2c9a";

/// A `--dry-run` report this handoff's `verify_resolved` accepts: the
/// `anthropic_base_url` line matches `ROOT` exactly, the way
/// `relay/tests.rs`'s own `AIMED_HERE` fixture does.
const AIMED_HERE: &str = "\
agent = claude
gateway_url = http://127.0.0.1:45169
openai_base_url = https://api.openai.com/v1
anthropic_base_url = http://127.0.0.1:8080
anthropic_auth = unset
";

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "topham-relay-stdout-{tag}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).expect("a scratch directory");
    root
}

/// A `nemo-relay` double that answers two ways depending on whether
/// `--dry-run` is among its arguments: the preflight report on the dry run,
/// [`CHILD_STDOUT`] and nothing else on the real invocation `ExecLauncher`
/// execs into. Distinguishing on the argument (rather than always answering
/// the same way, as `relay/tests.rs`'s `dry_run_double` does) is what this
/// file needs and that one does not: that one is never actually exec'd into,
/// because it is driven through `RecordingLauncher`, not `ExecLauncher`.
fn double_script(dir: &Path) -> PathBuf {
    let path = dir.join("nemo-relay-double.sh");
    let mut file = std::fs::File::create(&path).expect("the double");
    write!(
        file,
        "#!/bin/sh\nif printf '%s\\n' \"$@\" | grep -qx -- --dry-run; then\ncat <<'REPORT'\n{AIMED_HERE}REPORT\nelse\nprintf '%s' '{CHILD_STDOUT}'\nfi\n"
    )
    .expect("the double's body");
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("the double is executable");
    }
    path
}

/// A saved `work` profile, chained, under `root/config` — and the `EnvMap`
/// that resolves to the same directory, for the one in-process write
/// ([`Profile::save`]) this file does before spawning the real subprocess.
fn write_profile(root: &Path) -> EnvMap {
    let mut env = EnvMap::new();
    env.insert(
        "XDG_CONFIG_HOME".to_string(),
        root.join("config").display().to_string(),
    );
    env.insert(
        "XDG_DATA_HOME".to_string(),
        root.join("data").display().to_string(),
    );
    let mut profile = Profile::new(Agent::Claude, ROOT);
    profile.topology = Topology::Chained;
    profile
        .save(&env, "work")
        .expect("the fixture profile saves");
    env
}

/// Spawn the real `topham` binary as `topham relay work --relay <double> --
/// -p hello`, with a cleared environment rebuilt from scratch — the same
/// discipline `claude_e2e.rs`'s rig uses, and for the same reason: the
/// isolation trap this environment carries an ambient OAuth token that must
/// never reach a spawned child by accident.
fn run_topham_relay(root: &Path, double: &Path) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_topham");
    let mut command = Command::new(bin);
    command.args([
        "relay",
        "work",
        "--relay",
        &double.display().to_string(),
        "--",
        "-p",
        "hello",
    ]);
    command.env_clear();
    command.env("XDG_CONFIG_HOME", root.join("config"));
    command.env("XDG_DATA_HOME", root.join("data"));
    command.env("ROUNDHOUSE_API_KEY", TURN_KEY);
    command.env("PATH", "/usr/bin:/bin");
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.output().expect("the built topham binary spawns")
}

/// F24: `topham relay`'s stdout, after the real subprocess exec's into the
/// double, is exactly the child's own output — not the banner
/// `relay::run` writes before the exec, and not the two concatenated. This
/// is the guard F24 says the default suite lacks; it lives here, in the
/// default suite, needing only a built `topham` and a shell double — no
/// `claude`, no `nemo-relay`, no `--include-ignored`.
///
/// Mutating `cli.rs:169` back to `&mut std::io::stdout()` turns this red:
/// the banner (`"relay config"`, the preflight report, `RELAY_SYSTEM_CONFIG`)
/// lands on the same pipe the exec inherits, ahead of `CHILD_STDOUT`, so the
/// equality below fails and the contains-on-stderr assertion fails too. That
/// mutation was applied and reverted by hand while ruling on F24 (never
/// committed) to confirm the red; it is not repeated as an in-tree mutation
/// test here because the whole point of the finding is that `ExecLauncher`
/// cannot be swapped out to observe the arm without truly exec'ing, i.e.
/// without this file's subprocess machinery already firing.
#[test]
fn a_chained_relay_launch_through_the_real_dispatch_puts_only_the_childs_output_on_stdout() {
    let root = scratch("stdout");
    let double = double_script(&root);
    write_profile(&root);

    let output = run_topham_relay(&root, &double);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "F24 guard: `topham relay` must exit 0 once the double reports this launch's own \
         upstream\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );

    assert_eq!(
        stdout, CHILD_STDOUT,
        "F24 guard: stdout must be exactly the child's own output, byte for byte -- a banner \
         here is a corrupted document to whatever parses stdout as JSON, not merely extra \
         output\n--- stderr\n{stderr}"
    );

    assert!(
        stderr.contains("relay config")
            && stderr.contains("anthropic_base_url = http://127.0.0.1:8080"),
        "F24 guard: the banner belongs on stderr and must still reach the operator there\n--- \
         stderr\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
