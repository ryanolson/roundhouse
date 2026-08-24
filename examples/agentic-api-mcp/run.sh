#!/usr/bin/env bash
# Drive the whole four-process topology and assert on what crossed each wire.
#
#   curl -> agentic-api gateway -> fixture upstream (the "model")
#                    |
#                    +-> mcp-proxy -> roundhouse /mcp
#
# Everything is loopback, bounded by `timeout`, and torn down by the EXIT trap.
# The run leaves its whole transcript in $WORK, which is printed at the end.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "${HERE}/../.." && pwd)"

RH_HOST=${RH_HOST:-127.0.0.1}
RH_PORT=${RH_PORT:-8080}
PROXY_PORT=${PROXY_PORT:-8090}
UPSTREAM_PORT=${UPSTREAM_PORT:-8000}
GATEWAY_PORT=${GATEWAY_PORT:-3000}

WORK=${WORK:-$(mktemp -d -t agentic-api-mcp-XXXXXX)}
mkdir -p "${WORK}"

# ---------------------------------------------------------------------------
# Binaries
# ---------------------------------------------------------------------------
# agentic-api pins `channel = "1.98.0"` in its own rust-toolchain.toml and this
# repository pins 1.96.1, so the two are built by different toolchains into
# different target directories and neither is a workspace member of the other.
# There is no way to derive AGENTIC_BIN from this tree; it has to be told.
if [[ -z "${AGENTIC_BIN:-}" ]]; then
  cat >&2 <<'MSG'
AGENTIC_BIN is unset. Point it at the directory holding a built `agentic` and
`agentic-server` pair (`agentic serve` spawns its sibling by path), e.g.

  git clone https://github.com/vllm-project/agentic-api && cd agentic-api
  CARGO_TARGET_DIR=/tmp/agentic-target rustup run 1.98.0 cargo build -p agentic-server
  export AGENTIC_BIN=/tmp/agentic-target/debug
MSG
  exit 2
fi
for binary in agentic agentic-server; do
  if [[ ! -x "${AGENTIC_BIN}/${binary}" ]]; then
    echo "AGENTIC_BIN=${AGENTIC_BIN} has no executable ${binary}" >&2
    exit 2
  fi
done

if [[ -z "${ROUNDHOUSE_BIN:-}" ]]; then
  echo "== building roundhouse-server (set ROUNDHOUSE_BIN to skip)"
  ( cd "${REPO}" && timeout 900 cargo build -p roundhouse-server )
  ROUNDHOUSE_BIN="${REPO}/target/debug/roundhouse"
fi
[[ -x "${ROUNDHOUSE_BIN}" ]] || { echo "no roundhouse binary at ${ROUNDHOUSE_BIN}" >&2; exit 2; }

# ---------------------------------------------------------------------------
# Teardown
# ---------------------------------------------------------------------------
PIDS=()
cleanup() {
  local status=$?
  for pid in "${PIDS[@]:-}"; do
    [[ -n "${pid}" ]] && kill "${pid}" 2>/dev/null || true
  done
  # `agentic serve` is a supervisor: killing it leaves the agentic-server child
  # holding the gateway port, and the next run then fails to bind for a reason
  # that looks like anything but this.
  pkill -f "agentic-server .*--gateway-port ${GATEWAY_PORT}" 2>/dev/null || true
  wait 2>/dev/null || true
  exit "${status}"
}
trap cleanup EXIT INT TERM

wait_for_port() {
  local host=$1 port=$2 name=$3 tries=${4:-120}
  for _ in $(seq 1 "${tries}"); do
    if (exec 3<>"/dev/tcp/${host}/${port}") 2>/dev/null; then
      exec 3<&- 3>&- 2>/dev/null || true
      return 0
    fi
    sleep 0.5
  done
  echo "${name} never came up on ${host}:${port}" >&2
  return 1
}

# ---------------------------------------------------------------------------
# The four processes
# ---------------------------------------------------------------------------
echo "== work dir ${WORK}"

python3 "${HERE}/fixture-upstream.py" \
  --port "${UPSTREAM_PORT}" \
  --capture "${WORK}/upstream-capture.jsonl" \
  >"${WORK}/fixture-upstream.log" 2>&1 &
PIDS+=($!)

ROUNDHOUSE_CONTROL_PLANE="${HERE}/control-plane.json" \
ROUNDHOUSE_ADDR="${RH_HOST}:${RH_PORT}" \
RUST_LOG="${RUST_LOG:-info,roundhouse_server=debug,rmcp=debug}" \
  "${ROUNDHOUSE_BIN}" >"${WORK}/roundhouse.log" 2>&1 &
PIDS+=($!)

python3 "${HERE}/mcp-proxy.py" \
  --port "${PROXY_PORT}" \
  --target "http://${RH_HOST}:${RH_PORT}" \
  --capture "${WORK}/mcp-capture.jsonl" \
  >"${WORK}/mcp-proxy.log" 2>&1 &
PIDS+=($!)

wait_for_port 127.0.0.1 "${UPSTREAM_PORT}" "fixture upstream"
wait_for_port "${RH_HOST}" "${RH_PORT}" "roundhouse"
wait_for_port 127.0.0.1 "${PROXY_PORT}" "mcp proxy"

"${AGENTIC_BIN}/agentic" serve \
  --upstream "http://127.0.0.1:${UPSTREAM_PORT}" \
  --skip-llm-ready-check \
  --gateway-port "${GATEWAY_PORT}" \
  --database-url "sqlite://${WORK}/agentic.db" \
  --no-color \
  >"${WORK}/agentic.log" 2>&1 &
PIDS+=($!)
wait_for_port 127.0.0.1 "${GATEWAY_PORT}" "agentic-api gateway"

# The shipped request.json names the default proxy port; rewriting it here is
# what keeps the ports overridable without keeping two copies of the request in
# sync. The effective body is written into $WORK so the run's evidence is the
# request that was actually sent.
python3 - "${HERE}/request.json" "${WORK}/request.json" "${PROXY_PORT}" <<'PY'
import json, sys
source, destination, port = sys.argv[1], sys.argv[2], sys.argv[3]
body = json.load(open(source, encoding="utf-8"))
for tool in body.get("tools", []):
    if tool.get("type") == "mcp" and tool.get("server_url"):
        tool["server_url"] = f"http://127.0.0.1:{port}/mcp"
json.dump(body, open(destination, "w", encoding="utf-8"), indent=2)
PY

# ---------------------------------------------------------------------------
# The turn
# ---------------------------------------------------------------------------
echo "== POST /v1/responses"
set +e
timeout 180 curl -sS -N \
  -X POST "http://127.0.0.1:${GATEWAY_PORT}/v1/responses" \
  -H 'Content-Type: application/json' \
  -H 'Accept: text/event-stream' \
  --max-time 150 \
  -w '\n[curl] http_status=%{http_code}\n' \
  --data @"${WORK}/request.json" >"${WORK}/sse.txt" 2>"${WORK}/curl.err"
CURL_STATUS=$?
set -e
echo "[curl] exit=${CURL_STATUS}"
sed -n '1,200p' "${WORK}/sse.txt"

# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------
set +e
python3 - "${WORK}" "${HERE}/control-plane.json" "${CURL_STATUS}" <<'PY'
import json, sys, pathlib

work, plane_path, curl_status = pathlib.Path(sys.argv[1]), sys.argv[2], int(sys.argv[3])
plane = json.loads(pathlib.Path(plane_path).read_text(encoding="utf-8"))
expected_sha = plane["keys"][0]["key_sha256"]


def jsonl(name):
    path = work / name
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


mcp = jsonl("mcp-capture.jsonl")
upstream = jsonl("upstream-capture.jsonl")
sse = (work / "sse.txt").read_text(encoding="utf-8") if (work / "sse.txt").exists() else ""

results = []


def check(name, ok, detail=""):
    results.append((name, bool(ok), detail))


def rpc_result(entry):
    """The JSON-RPC result of one captured exchange, JSON or SSE-framed."""
    body = entry.get("response_body") or ""
    for line in body.splitlines():
        if line.startswith("data: "):
            try:
                return json.loads(line[6:])
            except json.JSONDecodeError:
                continue
    try:
        return json.loads(body)
    except json.JSONDecodeError:
        return None


methods = [entry.get("rpc_method") for entry in mcp]
check("i.a  roundhouse /mcp received `initialize`", "initialize" in methods, f"methods={methods}")
check("i.b  roundhouse /mcp received `tools/list`", "tools/list" in methods, "")

calls = [entry for entry in mcp if entry.get("rpc_method") == "tools/call"]
status_calls = [entry for entry in calls if entry.get("tool") == "status"]
check("i.c  roundhouse /mcp received `tools/call` for `status`", status_calls,
      f"tools called={[entry.get('tool') for entry in calls]}")

bearers = [entry.get("authorization") for entry in mcp]
carried = [b for b in bearers if b and b.get("secret_sha256") == expected_sha]
check("i.d  every /mcp request carried the configured turn key",
      mcp and len(carried) == len(mcp),
      f"{len(carried)}/{len(mcp)} requests, expected sha {expected_sha[:12]}...")

listed = None
for entry in mcp:
    if entry.get("rpc_method") == "tools/list":
        payload = rpc_result(entry) or {}
        listed = [tool["name"] for tool in payload.get("result", {}).get("tools", [])]
check("i.e  tools/list answered with roundhouse's control tools",
      listed and "status" in listed, f"tools={listed}")

turn1 = next((entry for entry in upstream if entry.get("kind") == "upstream_request" and entry.get("turn") == 1), None)
forwarded = (turn1 or {}).get("forwarded_tool_names") or []
chosen = (turn1 or {}).get("chosen_tool")
check("ii.a agentic-api forwarded our tools under `mcp__<label>__<tool>`",
      chosen and chosen.startswith("mcp__") and chosen.endswith("__status"),
      f"chosen={chosen} all={forwarded}")

check("ii.b the gateway forced parallel_tool_calls=false upstream (needs #197)",
      turn1 is not None and turn1.get("parallel_tool_calls") is False,
      f"upstream parallel_tool_calls={None if turn1 is None else turn1.get('parallel_tool_calls')}")

tool_text = None
tool_is_error = None
if status_calls:
    payload = rpc_result(status_calls[-1]) or {}
    result = payload.get("result") or {}
    tool_is_error = result.get("isError")
    content = result.get("content") or []
    tool_text = content[0].get("text") if content else None

final_text = []
for line in sse.splitlines():
    if not line.startswith("data: "):
        continue
    try:
        event = json.loads(line[6:])
    except json.JSONDecodeError:
        continue
    if event.get("type") == "response.output_text.done":
        final_text.append(event.get("text", ""))
answer = final_text[-1] if final_text else ""

check("iii.a the client's final text carries the tool's own output",
      tool_text is not None and tool_text in answer,
      f"tool_text={tool_text!r}")
check("iii.b the gateway answered 200 and curl exited 0",
      curl_status == 0 and "http_status=200" in sse,
      f"curl exit={curl_status}")

print()
print("=" * 78)
for name, ok, detail in results:
    print(f"{'PASS' if ok else 'FAIL'}  {name}")
    if detail:
        print(f"        {detail}")
print("-" * 78)
print(f"status tool isError={tool_is_error}")
print(f"status tool output : {tool_text}")
print(f"model's final text : {answer}")
print("=" * 78)

sys.exit(0 if all(ok for _, ok, _ in results) else 1)
PY
CHECK_STATUS=$?
set -e

echo
echo "== transcript in ${WORK}"
ls -la "${WORK}"
exit "${CHECK_STATUS}"
