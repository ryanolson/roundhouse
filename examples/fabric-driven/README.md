<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# The Fabric-driven topology, end to end

A NeMo Fabric consumer runs Codex through Fabric's Codex adapter against a
real roundhouse, using the `FabricConfig` roundhouse emits for the deployment.
This is the third topology `agent-docs/synergies/nemo-fabric.md` rules
(Codex-SDK app-server → Fabric → roundhouse), exercised rather than read.

What the run proves, and what it does not:

- The document `roundhouse_server::fabric_config` emits plans against
  Fabric's real Codex adapter with `doctor` at `warn` for two benign
  reasons (no resolution strategy, no environment block).
- Two ordered turns on one Fabric runtime land in **one** roundhouse session:
  the app-server sends `prompt_cache_key` equal to its thread id, roundhouse
  names the session by it, and `GET /v1/sessions/{thread_id}/trajectory` and
  `/atof` answer 200 for exactly that id. Prefix admission is what makes the
  second turn a turn and not a fork.
- The app-server completes the MCP handshake against `/mcp` with the bearer
  carried by the `bearer_token_env_var` override, so the secret never enters
  the thread config.
- The zero `cached_input` in roundhouse's metrics is the offline echo stub's
  constant (`EchoFrontierClient` reports no cache reads); it says nothing
  about admission. A deployment with a catalog and a real upstream reports the
  provider's figure.
- Not proved here: anything about a routed frontier answer (the stub echoes),
  and the three `codex-cli 0.146.0` rulings the ruling lists as unverified on
  the app-server.

## Run it

Build the server and the artifact writer, start roundhouse in offline Open
mode, emit the four artifacts, then drive Fabric:

```bash
cargo build -p roundhouse-server --bin roundhouse --example fabric_launch
ROUNDHOUSE_ADDR=127.0.0.1:8080 ./target/debug/roundhouse &
./target/debug/examples/fabric_launch http://127.0.0.1:8080/v1 /abs/out

python3 -m venv .venv && .venv/bin/pip install "nemo-fabric[codex]"
ROUNDHOUSE_API_KEY=anything ADAPTER_PYTHON=$PWD/.venv/bin/python \
    .venv/bin/python examples/fabric-driven/run.py /abs/out
```

`ROUNDHOUSE_API_KEY` must be set to *something*: Fabric refuses to start a
custom provider whose `api_key_env` is unset, and roundhouse in Open mode
admits any value. With `ROUNDHOUSE_CONTROL_PLANE` configured, set it to a
minted `rh_turn_…` key instead.

The versions the recorded run used (2026-09-10): roundhouse at the commit
that added `fabric_config`; `nemo-fabric 0.2.0`, `nemo-fabric-adapters-codex
0.2.0`, `openai-codex 0.144.4` (the app-server the adapter pins) from PyPI;
`nemo-relay-cli-bin 0.7.3` installed but Relay not enabled. The PyPI runtime
is the `0.2.0` stable the ruling names; the four config structs the artifact
uses are byte-identical between it and the `6d9ebc3` rev roundhouse pins.

Expected tail of the output:

```text
TURN1 status: succeeded
TURN1 thread_id: <uuid>
TURN2 status: succeeded
TURN2 thread_id: <same uuid>
METRICS after: sessions=+1 turns=+2 calls=+2 …
SESSION trajectory: HTTP 200, … bytes
TRAJECTORY steps: 7 final_metrics: {'total_prompt_tokens': …, 'total_cached_tokens': 0, …}
SESSION atof: HTTP 200, … bytes
E2E RESULT: PASS
```
