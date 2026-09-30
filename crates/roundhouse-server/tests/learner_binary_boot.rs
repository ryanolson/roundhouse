// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The real `roundhouse` binary boots with a `shadow` project, on both
//! backends (milestone M9's "done means").
//!
//! `tests/learner_startup.rs` calls the library composition `main.rs` calls;
//! this file spawns the compiled binary through `CARGO_BIN_EXE_roundhouse`, so
//! what boots is the wiring a deployment gets, global logging included, which
//! no in-process test may initialize. The child's environment is cleared and
//! rebuilt from nothing, so no ambient credential reaches it.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TURN_HASH: &str = "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677ddfd";

/// Kills the child on drop, including on a failing assertion.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "roundhouse-learner-binary-boot-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A control-plane file with one project over the binary's own echo catalog
/// that writes a tier recipe and no learner: the composition is `stage`.
fn tiers_control_plane() -> PathBuf {
    let dir = scratch();
    let plane = serde_json::json!({
        "projects": [{
            "id": "acme",
            "tiers": { "capable": ["echo/echo"], "efficient": [] }
        }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "acme", "user": "ada", "key_sha256": TURN_HASH }]
    });
    let path = dir.join("control-plane.json");
    std::fs::write(&path, plane.to_string()).expect("the control plane writes");
    path
}

/// A control-plane file with one `shadow` project over the binary's own echo
/// catalog, its artifact beside it, and the recovery block.
fn shadow_control_plane() -> PathBuf {
    let dir = scratch();
    let artifact = dir.join("artifact.json");
    std::fs::write(
        &artifact,
        r#"{"schema_revision":1,"input_revision":1,"selector_revision":3,"stage_revision":1,"credit_revision":1,"gate":"wilson-v1","strategies":["rules","capable"],"prior":[],"manifest_digest":"00","source_commit":"test"}"#,
    )
    .expect("the artifact writes");
    let plane = serde_json::json!({
        "projects": [{
            "id": "acme",
            "tiers": { "capable": ["echo/echo"], "efficient": [] },
            "learner": {
                "mode": "shadow",
                "strategies": ["rules", "capable"],
                "artifact": artifact,
                "quality": { "floor": 0.8, "z": 1.96, "min_evidence": 5000, "min_sessions": 20 },
                "latency_limit_ms": 10000,
                "latency_min_samples": 20,
                "cache_min_samples": 20,
                "read_timeout_ms": 25,
                "apply_timeout_ms": 250
            }
        }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "acme", "user": "ada", "key_sha256": TURN_HASH }],
        "learner_recovery": {
            "sweep_interval_ms": 30000,
            "idle_after_ms": 60000,
            "max_sessions_per_sweep": 64,
            "pages_per_session_per_sweep": 4,
            "audit_sessions_per_sweep": 32,
            "read_timeout_ms": 25,
            "apply_timeout_ms": 250,
            "source_timeout_ms": 1000
        }
    });
    let path = dir.join("control-plane.json");
    std::fs::write(&path, plane.to_string()).expect("the control plane writes");
    path
}

/// Spawn the binary with only `vars` set, and collect its stdout, where
/// `tracing_subscriber::fmt()` writes every log line.
fn spawn(vars: &[(&str, &str)]) -> (ChildGuard, Arc<Mutex<Vec<String>>>) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_roundhouse"));
    command.env_clear();
    command.env("RUST_LOG", "info");
    command.env("NO_COLOR", "1");
    command.env("ROUNDHOUSE_ADDR", "127.0.0.1:0");
    command.env("PATH", "/usr/bin:/bin");
    for (name, value) in vars {
        command.env(name, value);
    }
    command.stdout(Stdio::piped());
    command.stderr(Stdio::null());
    let mut child = command.spawn().expect("the built roundhouse binary spawns");
    let stdout = child.stdout.take().expect("piped stdout");
    let lines = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&lines);
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            collected.lock().unwrap().push(strip_ansi(&line));
        }
    });
    (ChildGuard(child), lines)
}

/// Drop `\x1b[...m` sequences: the binary colors its output.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Wait, bounded, for the line that says which policy the engine routes
/// under, and return everything printed by then. `serve` logs it after
/// "roundhouse listening", once the engine is built. A binary that exits or
/// never gets there fails with what it printed.
fn wait_until_serving(guard: &mut ChildGuard, lines: &Arc<Mutex<Vec<String>>>) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        {
            let seen = lines.lock().unwrap();
            if seen.iter().any(|line| line.contains(SERVING)) {
                return seen.join("\n");
            }
        }
        if let Some(status) = guard.0.try_wait().expect("the child's status reads") {
            panic!(
                "the binary exited with {status} before serving:\n{}",
                lines.lock().unwrap().join("\n")
            );
        }
        if Instant::now() >= deadline {
            panic!(
                "the binary did not serve within 30 s:\n{}",
                lines.lock().unwrap().join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The message of the line `serve` logs with the engine's own policy name.
const SERVING: &str = "the engine routes every turn under this policy";

/// The policy the engine was built with, from its `serve` line: what a
/// turn's record names, read off the engine rather than the composition, so
/// `main.rs` wiring any other policy than the composed one shows here.
fn serving_policy(printed: &str) -> &str {
    let line = printed
        .lines()
        .find(|line| line.contains(SERVING))
        .expect("the serving line");
    let after = line
        .split("policy=")
        .nth(1)
        .unwrap_or_else(|| panic!("no policy field: {line}"));
    after.split_whitespace().next().unwrap_or_default()
}

#[test]
fn the_binary_boots_a_shadow_project_on_the_memory_backend() {
    let plane = shadow_control_plane();
    let (mut guard, lines) = spawn(&[("ROUNDHOUSE_CONTROL_PLANE", plane.to_str().unwrap())]);
    let printed = wait_until_serving(&mut guard, &lines);
    assert!(
        printed.contains("the learned router is composed over the stage router"),
        "{printed}"
    );
    assert!(
        printed.contains("learner state is in this process's memory and ends with the process"),
        "{printed}"
    );
    assert!(
        printed.contains("the learner recovery task is running"),
        "{printed}"
    );
    assert_eq!(serving_policy(&printed), "learned", "{printed}");
}

/// **No learner: the policy name is unchanged.** A file with a tier recipe
/// and no learner boots the stage router, as before M9, and runs no recovery
/// task.
#[test]
fn the_binary_boots_a_no_learner_file_under_the_stage_policy() {
    let plane = tiers_control_plane();
    let (mut guard, lines) = spawn(&[("ROUNDHOUSE_CONTROL_PLANE", plane.to_str().unwrap())]);
    let printed = wait_until_serving(&mut guard, &lines);
    assert_eq!(serving_policy(&printed), "stage", "{printed}");
    assert!(
        !printed.contains("the learner recovery task is running"),
        "{printed}"
    );
}

#[test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
fn the_binary_boots_a_shadow_project_on_the_redis_backend() {
    let url = std::env::var("ROUNDHOUSE_TEST_REDIS_URL")
        .expect("ROUNDHOUSE_TEST_REDIS_URL names the test Redis");
    let namespace = format!("m9bin{}", uuid::Uuid::new_v4().simple());
    let plane = shadow_control_plane();
    let (mut guard, lines) = spawn(&[
        ("ROUNDHOUSE_CONTROL_PLANE", plane.to_str().unwrap()),
        ("ROUNDHOUSE_REDIS_URL", &url),
        ("ROUNDHOUSE_REDIS_NAMESPACE", &namespace),
    ]);
    let printed = wait_until_serving(&mut guard, &lines);
    assert!(
        printed.contains("the learned router is composed over the stage router"),
        "{printed}"
    );
    assert!(
        printed.contains("learner state is shared in the Redis this deployment names"),
        "{printed}"
    );
    assert!(
        printed.contains("the learner recovery task is running"),
        "{printed}"
    );
    assert_eq!(serving_policy(&printed), "learned", "{printed}");
}
