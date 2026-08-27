#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Steps 1-3 of the anthropic-spec-sync skill: discover, fetch, diff.

Discovers the current Anthropic OpenAPI spec via anthropic-sdk-typescript's
.stats.yml, verifies the content-addressed download, extracts the pinned
vocabulary, and prints a structured diff against the recorded pin. It never
writes into the repository: updating the pin fixture and fixing the code are
the skill-driver's job (steps 4-6), because those steps are judgement, and
this script exists precisely so the judgement starts from a mechanical diff
rather than a 2.4 MB YAML read.

Usage:
  spec_sync.py --pin <spec_pin.json> [--workdir DIR]     # normal run
  spec_sync.py --pinned-sha <sha256> [--workdir DIR]     # pre-M11.0: pin from the evidence doc
  spec_sync.py --diff-only <old.yml> <new.yml>           # offline re-diff

Exit codes: 0 = spec unmoved, 2 = spec moved (diff printed), 1 = error.
"""

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path

SDK_REPO = "https://github.com/anthropics/anthropic-sdk-typescript"


def sh(cmd, cwd=None):
    return subprocess.run(cmd, cwd=cwd, check=True, capture_output=True, text=True).stdout


def discover(workdir: Path):
    """Shallow-clone the SDK, return (sdk_rev, spec_url, spec_hash_field)."""
    clone = workdir / "anthropic-sdk-typescript"
    if not clone.exists():
        sh(["git", "clone", "--depth", "1", SDK_REPO, str(clone)])
    rev = sh(["git", "rev-parse", "HEAD"], cwd=clone).strip()
    stats = (clone / ".stats.yml").read_text()
    url = re.search(r"openapi_spec_url:\s*(\S+)", stats).group(1)
    spec_hash = re.search(r"openapi_spec_hash:\s*(\S+)", stats)
    return rev, url, spec_hash.group(1) if spec_hash else None


def fetch(url: str, workdir: Path, pinned_body_sha=None) -> Path:
    """Download; record the body sha256 we compute ourselves.

    Three identifiers exist and none may be conflated (verified 2026-08-27):
    the 64-hex hash inside the URL filename and .stats.yml's 32-hex
    openapi_spec_hash are both opaque Stainless-internal content addresses —
    neither is a hash of the raw body (sha256 and md5 of the body match
    neither). Move detection is therefore URL comparison; integrity is OUR
    recorded body sha256, checkable only when re-downloading the SAME URL
    (the storage is immutable, so a changed body under an unchanged URL is a
    broken download or a compromised mirror — refuse it).
    """
    out = workdir / "spec-current.yml"
    with urllib.request.urlopen(url) as r:
        body = r.read()
    got = hashlib.sha256(body).hexdigest()
    if pinned_body_sha and got != pinned_body_sha:
        sys.exit(
            f"error: same URL as the pin but body sha256 {got} != pinned {pinned_body_sha} — "
            f"the storage is content-addressed and immutable, so this is a broken download; do not proceed"
        )
    out.write_bytes(body)
    print(f"fetched {len(body)} bytes, body sha256 {got}")
    return out


def load_spec(path: Path):
    import yaml  # pyyaml; install if absent — the spec is YAML, 2.4 MB

    return yaml.safe_load(path.read_text())


def names(schema, key="properties"):
    return sorted((schema or {}).get(key, {}).keys())


def refs(members):
    out = []
    for m in members or []:
        r = m.get("$ref", "")
        out.append(r.rsplit("/", 1)[-1] if r else str(m)[:60])
    return sorted(out)


def extract_vocabulary(spec) -> dict:
    """The exact set the wire module's pinning tests read. Keep in lockstep
    with crates/roundhouse-fleet/src/anthropic_messages/spec_pin.json."""
    s = spec["components"]["schemas"]

    def enum_of(name, prop=None):
        sch = s.get(name, {})
        if prop:
            sch = sch.get("properties", {}).get(prop, {})
        if "enum" in sch:
            return sorted(sch["enum"])
        for alt in sch.get("anyOf", []):
            if "enum" in alt:
                return sorted(alt["enum"])
        return []

    create, beta_create = s.get("CreateMessageParams", {}), s.get("BetaCreateMessageParams", {})
    return {
        "stop_reason": enum_of("StopReason") or enum_of("Message", "stop_reason"),
        "usage_properties": names(s.get("Usage")),
        "cache_creation_fields": names(s.get("CacheCreation")),
        "message_stream_event_members": refs(s.get("MessageStreamEvent", {}).get("oneOf")),
        "content_block_delta_variants": sorted(
            k for k in s if k.endswith("ContentBlockDelta") and not k.startswith("Beta")
        ),
        "response_content_block_members": refs(s.get("ContentBlock", {}).get("oneOf")),
        "create_message_params": {
            "properties": names(create),
            "required": sorted(create.get("required", [])),
            "additional_properties_false": create.get("additionalProperties") is False,
        },
        "beta_create_message_params": {
            "properties": names(beta_create),
            "additional_properties_false": beta_create.get("additionalProperties") is False,
        },
        "cache_control_ttl": enum_of("CacheControlEphemeral", "ttl"),
        "anthropic_beta_named_values": enum_of("AnthropicBeta"),
        "anthropic_beta_is_open": any(
            alt == {"type": "string"} for alt in s.get("AnthropicBeta", {}).get("anyOf", [])
        ),
        "message_properties": names(s.get("Message")),
        "path_count": len(spec.get("paths", {})),
        "beta_path_count": sum(1 for p in spec.get("paths", {}) if "beta=true" in p),
    }


def diff(old: dict, new: dict, prefix=""):
    moved = False
    for k in sorted(set(old) | set(new)):
        o, n = old.get(k), new.get(k)
        if isinstance(o, dict) and isinstance(n, dict):
            moved |= diff(o, n, prefix=f"{prefix}{k}.")
        elif isinstance(o, list) and isinstance(n, list):
            added, removed = sorted(set(n) - set(o)), sorted(set(o) - set(n))
            if added:
                print(f"  + {prefix}{k}: {added}")
            if removed:
                print(f"  - {prefix}{k}: {removed}   <-- REMOVAL: breaking until ruled otherwise")
            moved |= bool(added or removed)
        elif o != n:
            print(f"  ~ {prefix}{k}: {o!r} -> {n!r}")
            moved = True
    return moved


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pin", type=Path, help="spec_pin.json to diff against")
    ap.add_argument("--pinned-sha", help="pinned body sha256 when no fixture exists yet (pre-M11.0)")
    ap.add_argument("--pinned-url", help="pinned openapi_spec_url when no fixture exists yet (pre-M11.0)")
    ap.add_argument("--workdir", type=Path, default=None)
    ap.add_argument("--diff-only", nargs=2, type=Path, metavar=("OLD_YML", "NEW_YML"))
    args = ap.parse_args()

    if args.diff_only:
        old_v = extract_vocabulary(load_spec(args.diff_only[0]))
        new_v = extract_vocabulary(load_spec(args.diff_only[1]))
        print("vocabulary diff:")
        sys.exit(2 if diff(old_v, new_v) else 0)

    workdir = args.workdir or Path(tempfile.mkdtemp(prefix="anthropic-spec-sync-"))
    workdir.mkdir(parents=True, exist_ok=True)
    print(f"workdir: {workdir}")

    rev, url, _ = discover(workdir)
    print(f"anthropic-sdk-typescript @ {rev}\nopenapi_spec_url: {url}")

    pin = json.loads(args.pin.read_text()) if args.pin else None
    pinned_body_sha = pin["spec_sha256"] if pin else args.pinned_sha
    pinned_url = pin.get("spec_url") if pin else args.pinned_url
    if not pinned_body_sha and not pinned_url:
        sys.exit("error: give --pin, or --pinned-sha/--pinned-url; the skill says where the pin lives")

    if pinned_url and url == pinned_url:
        print("spec unmoved: .stats.yml still names the pinned URL — record a dated re-verification and stop")
        sys.exit(0)
    if pinned_url is None:
        # Body-sha-only pin (pre-M11.0): download and compare our own hash.
        probe = fetch(url, workdir)
        got = hashlib.sha256(probe.read_bytes()).hexdigest()
        if got == pinned_body_sha:
            print(f"spec unmoved: body sha256 matches pinned {pinned_body_sha[:12]}… — record a dated re-verification and stop")
            sys.exit(0)
        spec_path = probe
    else:
        spec_path = fetch(url, workdir)
    new_v = extract_vocabulary(load_spec(spec_path))
    (workdir / "vocabulary-current.json").write_text(json.dumps(new_v, indent=2))
    print(f"current vocabulary written: {workdir}/vocabulary-current.json")

    if pin and "vocabulary" in pin:
        print("vocabulary diff (pinned -> current):")
        moved = diff(pin["vocabulary"], new_v)
        if not moved:
            print("  (URL moved but the pinned vocabulary is unchanged — an additive-elsewhere spec churn; still update the pin's sha/rev/date)")
    else:
        print("no pinned vocabulary to diff (pre-M11.0 run) — read vocabulary-current.json against the evidence doc's §3.2")
    print(f"\nnext (skill steps 4-6): update the pin fixture with sha/rev/date/vocabulary, run\n"
          f"  timeout 300 cargo test -p roundhouse-fleet anthropic_messages\n"
          f"and treat every red pinning test as the worklist; then the dated addendum.")
    sys.exit(2)


if __name__ == "__main__":
    main()
