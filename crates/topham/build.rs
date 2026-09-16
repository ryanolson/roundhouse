// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The one thing `topham --version` can say that the package version cannot:
//! which tree this binary was built from.
//!
//! **Why a launcher needs it and the server does not** (M11.3 review F23). The
//! server is reached over a socket, so what is running is whatever answered;
//! `topham` is reached by *someone rebuilding it*, and the real-binary suites
//! drive a `target/debug/topham` an operator points them at by path. The
//! workspace version is identical in every build of every commit that does not
//! touch `Cargo.toml`, so clap's bare `version` gave a stale launcher and a
//! fresh one the same banner — and a suite whose preflight prints that banner
//! could report green for code nobody compiled. A commit hash is the smallest
//! thing that makes the two distinguishable, and it is what the suite's probe
//! compares against HEAD.
//!
//! **`unknown` and never a build failure.** A crate published to a registry, a
//! vendored tree, a source tarball and a box without `git` all build `topham`
//! perfectly well, and refusing them to protect a diagnostic would trade a
//! working launcher for a nicer version line. The suite's probe treats a banner
//! that does not name HEAD as a warning for exactly this reason.

use std::path::{Path, PathBuf};

/// How much of the object name the banner carries.
///
/// Seven, fixed, rather than `git rev-parse --short`'s shortest-unambiguous
/// answer: the suite's probe truncates HEAD to the same width, and a width that
/// grew on the first colliding prefix would make the two disagree about a
/// binary that is perfectly current.
const SHORT_LEN: usize = 7;

fn main() {
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets the manifest directory"),
    );
    println!(
        "cargo:rustc-env=TOPHAM_BUILD_COMMIT={}",
        head_commit(&manifest).unwrap_or_else(|| "unknown".to_string())
    );

    // Emitting any `rerun-if-changed` replaces cargo's default (rerun when a
    // package file changes) — which is what is wanted here, since nothing in
    // `src` can change this script's answer. What must be named is the pair
    // that can: `HEAD` itself, and the branch ref it points at, so that a
    // commit is followed by a rebuild that re-reads the hash instead of a
    // cached script output naming the previous one.
    println!("cargo:rerun-if-changed=build.rs");
    if let Some(git) = git_dir(&manifest) {
        let head = git.join("HEAD");
        println!("cargo:rerun-if-changed={}", head.display());
        if let Ok(contents) = std::fs::read_to_string(&head)
            && let Some(reference) = contents.trim().strip_prefix("ref: ")
        {
            // Absent when the ref is packed rather than loose; naming a path
            // that does not exist is how cargo is told "rerun until it does",
            // which is the right answer for a ref about to be written.
            println!("cargo:rerun-if-changed={}", git.join(reference).display());
        }
    }
}

/// The commit this tree is on, abbreviated, or `None` where there is nothing to
/// read it from.
fn head_commit(manifest: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(manifest)
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (commit.len() >= SHORT_LEN).then(|| commit[..SHORT_LEN].to_string())
}

/// The repository's `.git` directory, when it is an ordinary one.
///
/// Only the plain directory case: in a worktree or a submodule `.git` is a file
/// pointing elsewhere, and following it would be a second parser for a layout
/// this script does not otherwise depend on. The cost of not following it is a
/// build script that reruns on the package's own default rules — a hash that
/// can go one commit stale, which is precisely what the probe's warning is for.
fn git_dir(manifest: &Path) -> Option<PathBuf> {
    manifest
        .ancestors()
        .map(|ancestor| ancestor.join(".git"))
        .find(|candidate| candidate.is_dir())
}
