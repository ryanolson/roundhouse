// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `learner-calibrate` — the offline calibrator of the online routing learner
//! (milestone M10).
//!
//! ```text
//! learner-calibrate <manifest.json> <out-dir>
//! ```
//!
//! Reads the project's learner sessions from the source marks, replays their
//! logs, and writes the calibration artifact, its sidecar, the promotion
//! report and the resolved input manifest into `<out-dir>`. It only reads the
//! stores. The manifest format and the files are documented on
//! `roundhouse_server::learner_calibrate`, the calibration itself on
//! `roundhouse_core::routing::learn::offline`, and both in the README.
//!
//! Thin on purpose: everything a test needs to check is in the library, so
//! this file parses two arguments and prints where the files went.

use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use roundhouse_server::learner_calibrate::{host, run};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [manifest, out_dir] = args.as_slice() else {
        eprintln!("usage: learner-calibrate <manifest.json> <out-dir>");
        return ExitCode::from(2);
    };
    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    match run(
        Path::new(manifest),
        Path::new(out_dir),
        created_at_ms,
        &host(),
    )
    .await
    {
        Ok(written) => {
            println!("epoch {}", written.epoch);
            for path in [
                &written.artifact,
                &written.sidecar,
                &written.report,
                &written.input_manifest,
            ] {
                println!("wrote {}", path.display());
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("learner-calibrate: {error:#}");
            ExitCode::FAILURE
        }
    }
}
