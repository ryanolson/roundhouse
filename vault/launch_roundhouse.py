#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Launch roundhouse-server with secrets from Vault.

Roundhouse reads credentials from environment variables named by the control
plane (credentials.providers.nvidia.env_var -> INFERENCE_API_KEY). This
launcher resolves INFERENCE_API_KEY through the `vault` package (Vault if
VAULT_TOKEN is set, else .env / environment fallback), exports the ROUNDHOUSE_*
configuration, and execs the server. The key is never written to disk or logged.

Usage (from the repo root):
    python vault/launch_roundhouse.py \
        --catalog use-cases/cache-aware-routing/catalog.json \
        --control-plane use-cases/cache-aware-routing/control-plane.json

Override via env:
    ROUNDHOUSE_ADDR=0.0.0.0:8080
    ROUNDHOUSE_OPENAI_API_BASE=https://inference-api.nvidia.com/v1
    ROUNDHOUSE_BIN=/path/to/prebuilt/roundhouse-server   # skip `cargo run`
    INFERENCE_KEY_NAME=INFERENCE_API_KEY                  # vault secret name to fetch
"""
from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT))

from vault import require_secret  # noqa: E402
from vault.env_loader import ensure_vault_config  # noqa: E402

# The env var name the control plane's credentials block resolves for provider
# `nvidia`. Keep these two in sync.
INFERENCE_ENV_VAR = "INFERENCE_API_KEY"


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Launch roundhouse-server with Vault-resolved secrets.")
    p.add_argument("--catalog", type=Path, help="Path to catalog.json (relative to repo root)")
    p.add_argument("--control-plane", dest="control_plane", type=Path, help="Path to control-plane.json")
    return p.parse_args()


def resolve_paths(args: argparse.Namespace) -> tuple[Path, Path]:
    catalog = REPO_ROOT / args.catalog if args.catalog else None
    control_plane = REPO_ROOT / args.control_plane if args.control_plane else None
    if catalog is None or control_plane is None:
        sys.exit(
            "Provide --catalog and --control-plane, e.g.:\n"
            "  python vault/launch_roundhouse.py \\\n"
            "      --catalog use-cases/cache-aware-routing/catalog.json \\\n"
            "      --control-plane use-cases/cache-aware-routing/control-plane.json"
        )
    return catalog, control_plane


def preflight(catalog: Path, control_plane: Path) -> None:
    for path in (catalog, control_plane):
        if not path.exists():
            sys.exit(f"missing {path} -- are you running from the repo root?")
    if "REPLACE-with-exact" in catalog.read_text(encoding="utf-8"):
        print(
            f"WARNING: {catalog.name} still has the placeholder model id. Set "
            "models[0].model to the exact NVIDIA model id or every turn will fail.",
            file=sys.stderr,
        )
    if "REPLACE-run" in control_plane.read_text(encoding="utf-8"):
        sys.exit(
            f"{control_plane.name} still has placeholder key hashes -- run "
            f"`python {control_plane.parent}/mint_keys.py` first."
        )


def build_env(catalog: Path, control_plane: Path) -> dict:
    ensure_vault_config()
    secret_name = os.environ.get("INFERENCE_KEY_NAME", INFERENCE_ENV_VAR)
    api_key = require_secret(secret_name)

    env = dict(os.environ)
    env[INFERENCE_ENV_VAR] = api_key
    env.setdefault("ROUNDHOUSE_ADDR", "127.0.0.1:8080")
    env.setdefault("ROUNDHOUSE_FRONTIER_UPSTREAM", "openai_responses")
    env.setdefault("ROUNDHOUSE_OPENAI_API_BASE", "https://inference-api.nvidia.com/v1")
    env["ROUNDHOUSE_CATALOG"] = str(catalog)
    env["ROUNDHOUSE_CONTROL_PLANE"] = str(control_plane)
    return env


def command() -> list[str]:
    prebuilt = os.environ.get("ROUNDHOUSE_BIN")
    if prebuilt:
        return [prebuilt]
    return ["cargo", "run", "--release", "-p", "roundhouse-server"]


def main() -> None:
    args = parse_args()
    catalog, control_plane = resolve_paths(args)
    preflight(catalog, control_plane)
    env = build_env(catalog, control_plane)
    cmd = command()
    print(f"INFERENCE_API_KEY resolved ({len(env[INFERENCE_ENV_VAR])} chars); launching:")
    print(f"  ROUNDHOUSE_ADDR              = {env['ROUNDHOUSE_ADDR']}")
    print(f"  ROUNDHOUSE_OPENAI_API_BASE   = {env['ROUNDHOUSE_OPENAI_API_BASE']}")
    print(f"  ROUNDHOUSE_FRONTIER_UPSTREAM = {env['ROUNDHOUSE_FRONTIER_UPSTREAM']}")
    print(f"  ROUNDHOUSE_CATALOG           = {env['ROUNDHOUSE_CATALOG']}")
    print(f"  ROUNDHOUSE_CONTROL_PLANE     = {env['ROUNDHOUSE_CONTROL_PLANE']}")
    print(f"  $ {' '.join(cmd)}\n")
    try:
        raise SystemExit(subprocess.run(cmd, env=env, cwd=str(REPO_ROOT)).returncode)
    except FileNotFoundError:
        sys.exit(f"could not run {cmd[0]!r}; set ROUNDHOUSE_BIN to a prebuilt binary or install cargo.")


if __name__ == "__main__":
    main()
