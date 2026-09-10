// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Write every launch artifact for one deployment into one directory.
//!
//! An *example*, not the deferred operator entry point: it exists so the
//! Fabric-driven topology can be exercised end to end against a real
//! roundhouse without a CLI subcommand or admin route that the plan has not
//! yet designed. It writes the two files an unmodified `codex` reads
//! (`config.toml`, `models.json`), the generated skills, and the
//! `FabricConfig` a Fabric consumer reads (`fabric.json`) — all from one
//! [`CodexLaunch`], so the four documents describe one deployment.
//!
//! ```text
//! cargo run -p roundhouse-server --example fabric_launch -- http://127.0.0.1:8080/v1 /abs/out
//! ```

use std::fs;
use std::path::PathBuf;

use roundhouse_server::codex_launch::{CodexLaunch, skill_files};
use roundhouse_server::fabric_config_json;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(base_url), Some(out)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: fabric_launch <base_url ending in /v1> <absolute output dir>");
    };
    let out = PathBuf::from(out);
    fs::create_dir_all(&out)?;
    let catalog = out.join("models.json");
    let launch = CodexLaunch::new(base_url, &catalog)?;

    fs::write(out.join("config.toml"), launch.config_toml())?;
    fs::write(&catalog, launch.model_catalog_json())?;
    for file in skill_files() {
        let path = out.join(&file.relative_path);
        fs::create_dir_all(path.parent().expect("a skill file has a directory"))?;
        fs::write(path, file.contents)?;
    }
    fs::write(
        out.join("fabric.json"),
        fabric_config_json(&launch, Some(&out))?,
    )?;
    println!(
        "wrote config.toml, models.json, skills/ and fabric.json under {}",
        out.display()
    );
    Ok(())
}
