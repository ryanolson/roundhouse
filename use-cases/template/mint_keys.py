#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Mint roundhouse turn/admin keys for this use case demo.

Roundhouse stores only sha256(secret) in the control plane; the client presents
the secret. This script mints the secrets, writes the hashes into
control-plane.json in place, and saves the secrets to keys.local.json (gitignored)
for run.py to read.

A key is `rh_turn_<43 base62>` / `rh_admin_<43 base62>` -- 43 ASCII-alphanumeric
characters after the role prefix, which is the shape roundhouse validates
structurally (see control_config: has_valid_key_shape).

Usage:
    python use-cases/TODO_NAME/mint_keys.py
"""
from __future__ import annotations

import hashlib
import json
import secrets
import string
from pathlib import Path

HERE = Path(__file__).resolve().parent
CONTROL_PLANE = HERE / "control-plane.json"
SECRETS_OUT = HERE / "keys.local.json"

ALPHABET = string.ascii_letters + string.digits  # base62, matches the shape check


def mint(prefix: str) -> str:
    body = "".join(secrets.choice(ALPHABET) for _ in range(43))
    return f"{prefix}{body}"


def sha256_hex(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def main() -> None:
    plane = json.loads(CONTROL_PLANE.read_text(encoding="utf-8"))
    minted: dict[str, str] = {}

    # One turn key per (project, user) membership.
    for key in plane.get("keys", []):
        secret = mint("rh_turn_")
        key["key_sha256"] = sha256_hex(secret)
        label = f"{key['project']}/{key['user']}"
        minted[label] = secret

    # One admin key (handy for the metrics/control surfaces; optional for the demo).
    admin_secret = mint("rh_admin_")
    plane["admin_keys"] = [sha256_hex(admin_secret)]
    minted["__admin__"] = admin_secret

    CONTROL_PLANE.write_text(json.dumps(plane, indent=2) + "\n", encoding="utf-8")
    SECRETS_OUT.write_text(json.dumps(minted, indent=2) + "\n", encoding="utf-8")

    print(f"Patched {CONTROL_PLANE.name} with {len(plane.get('keys', []))} turn-key hash(es) + admin hash.")
    print(f"Wrote secrets to {SECRETS_OUT.name} (gitignored; do not commit).")
    for label, secret in minted.items():
        who = "admin" if label == "__admin__" else label
        print(f"  {who:24s} {secret[:12]}...")


if __name__ == "__main__":
    main()
