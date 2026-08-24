#!/usr/bin/env python3
"""A GPU-free Responses-API upstream, scripted to call one MCP tool and quote it.

agentic-api's gateway loop needs a model that (turn 1) emits a `function_call`
naming a tool it owns and (turn 2) says something about the output it gets back.
Nothing else about the model matters to this example, so nothing else is
simulated: this fixture is a decision table over the request body, not an
inference server.

Why not `scripts/container-fixture-server.py` from agentic-api's own tree: it
422s unless the last input item is exactly the one prompt it was recorded for,
`model == "gpt-4o"`, and `stream is False` (`scripts/container-fixture-server.py:44-57`
@ e35fbb2), so it cannot serve a streaming tool-calling turn at all.

Two deliberate choices, both about not asserting what the run is supposed to
prove:

* **The tool name is read off the request, never hardcoded.** agentic-api
  flattens a request-declared MCP tool to `mcp__<server_label>__<tool>` before
  forwarding it upstream (`crates/agentic-server-core/src/tool/mcp/handler.rs:379-401`
  @ e35fbb2). This fixture picks the forwarded name out of the `tools` array it
  actually received. If the flattening ever changes, the run still works and the
  capture file shows the new name -- which is the evidence, rather than a
  constant in this file agreeing with a constant in that one.
* **Turns are counted, not sniffed.** Round two's input is whatever the gateway
  chose to feed back, and guessing its item shape in advance would make the
  fixture fail for a reason that has nothing to do with roundhouse. Every body
  is written to the capture file regardless, so what actually arrived is on the
  record either way.
"""

import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

STATE = {"turn": 0, "capture": None, "lock": threading.Lock()}


def record(kind, payload):
    """Append one event to the capture file, if one was asked for."""
    path = STATE["capture"]
    if not path:
        return
    with STATE["lock"]:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(json.dumps({"t": time.time(), "kind": kind, **payload}) + "\n")


def sse(events):
    """Render Responses-API SSE frames.

    `data: ` prefixed lines are the only ones agentic-api reads
    (`crates/agentic-server-core/src/events/normalize.rs:12-14` @ e35fbb2); the
    `event:` line is here because a real upstream sends one and a fixture that
    omits it would be proving something narrower than it claims.
    """
    body = []
    for index, event in enumerate(events):
        event = dict(event, sequence_number=index)
        body.append(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n")
    return "".join(body).encode("utf-8")


def mcp_tool_name(tools, want_suffix):
    """The forwarded name of the tool this fixture is scripted to call.

    Prefers an exact `mcp__*__<suffix>`, falls back to any name ending in the
    suffix, so the fixture survives a change to the prefix without silently
    calling some other tool.
    """
    names = [tool.get("name", "") for tool in tools or [] if tool.get("type") == "function"]
    exact = [name for name in names if name.startswith("mcp__") and name.endswith(f"__{want_suffix}")]
    if exact:
        return exact[0], names
    loose = [name for name in names if name.endswith(want_suffix)]
    if loose:
        return loose[0], names
    return None, names


def function_call_turn(tool_name, response_id):
    call = {
        "id": "fc_fixture_1",
        "type": "function_call",
        "name": tool_name,
        "call_id": "call_fixture_1",
        "arguments": "{}",
        "status": "completed",
    }
    return [
        {"type": "response.created", "response": {"id": response_id, "status": "in_progress"}},
        {"type": "response.in_progress", "response": {"id": response_id, "status": "in_progress"}},
        {
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {**call, "arguments": "", "status": "in_progress"},
        },
        {
            "type": "response.function_call_arguments.delta",
            "item_id": call["id"],
            "call_id": call["call_id"],
            "output_index": 0,
            "delta": "{}",
        },
        {
            "type": "response.function_call_arguments.done",
            "item_id": call["id"],
            "call_id": call["call_id"],
            "name": tool_name,
            "output_index": 0,
            "arguments": "{}",
        },
        {"type": "response.output_item.done", "output_index": 0, "item": call},
        {
            "type": "response.completed",
            "response": {
                "id": response_id,
                "status": "completed",
                "output": [call],
                "usage": {
                    "input_tokens": 64,
                    "input_tokens_details": {"cached_tokens": 0},
                    "output_tokens": 8,
                    "output_tokens_details": {"reasoning_tokens": 0},
                    "total_tokens": 72,
                },
            },
        },
    ]


def text_turn(text, response_id):
    item = {
        "id": "msg_fixture_1",
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    }
    return [
        {"type": "response.created", "response": {"id": response_id, "status": "in_progress"}},
        {"type": "response.in_progress", "response": {"id": response_id, "status": "in_progress"}},
        {
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {**item, "content": [], "status": "in_progress"},
        },
        {
            "type": "response.output_text.delta",
            "item_id": item["id"],
            "output_index": 0,
            "content_index": 0,
            "delta": text,
        },
        {
            "type": "response.output_text.done",
            "item_id": item["id"],
            "output_index": 0,
            "content_index": 0,
            "text": text,
        },
        {"type": "response.output_item.done", "output_index": 0, "item": item},
        {
            "type": "response.completed",
            "response": {
                "id": response_id,
                "status": "completed",
                "output": [item],
                "usage": {
                    "input_tokens": 128,
                    "input_tokens_details": {"cached_tokens": 0},
                    "output_tokens": 32,
                    "output_tokens_details": {"reasoning_tokens": 0},
                    "total_tokens": 160,
                },
            },
        },
    ]


def tool_outputs(body):
    """Every tool result the gateway fed back into this turn's input."""
    found = []
    for item in body.get("input") or []:
        if isinstance(item, dict) and item.get("type") == "function_call_output":
            found.append(item.get("output", ""))
    return found


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        print(f"[fixture-upstream] {fmt % args}", flush=True)

    def _json(self, status, payload):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        # Both probes are answered even though `--skip-llm-ready-check` should
        # mean neither is asked for: `/v1/models` is proxied on demand rather
        # than at startup, and a 404 there would look like a fixture bug from
        # inside agentic-api's logs.
        if self.path.startswith("/health"):
            self._json(200, {"status": "healthy"})
        elif self.path.startswith("/v1/models"):
            self._json(200, {"object": "list", "data": [{"id": "fixture", "object": "model"}]})
        else:
            self._json(404, {"error": f"no route {self.path}"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length)
        try:
            body = json.loads(raw or b"{}")
        except json.JSONDecodeError as error:
            self._json(400, {"error": f"unparseable body: {error}"})
            return

        if not self.path.startswith("/v1/responses"):
            self._json(404, {"error": f"no route {self.path}"})
            return

        with STATE["lock"]:
            STATE["turn"] += 1
            turn = STATE["turn"]

        tool_name, names = mcp_tool_name(body.get("tools"), STATE["want_tool"])
        record(
            "upstream_request",
            {
                "turn": turn,
                "path": self.path,
                "forwarded_tool_names": names,
                "chosen_tool": tool_name,
                "parallel_tool_calls": body.get("parallel_tool_calls"),
                "body": body,
            },
        )

        response_id = f"resp_fixture_{turn}"
        outputs = tool_outputs(body)
        if turn == 1:
            if tool_name is None:
                # A 400 here is the honest answer: without a forwarded tool the
                # fixture has nothing to call, and pretending otherwise would
                # turn a real finding into a confusing timeout downstream.
                self._json(
                    400,
                    {"error": {"message": f"no MCP-flattened tool in the forwarded tools array: {names}"}},
                )
                return
            events = function_call_turn(tool_name, response_id)
        elif outputs:
            events = text_turn(f"{STATE['echo_prefix']}{outputs[-1]}", response_id)
        else:
            events = text_turn(
                f"{STATE['echo_prefix']}<no function_call_output in round {turn}'s input>",
                response_id,
            )

        payload = sse(events)
        record("upstream_response", {"turn": turn, "events": [event["type"] for event in events]})
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        # Written in frame-sized pieces with a flush between: the gateway's
        # per-chunk read timeout is real, and a fixture that only ever delivers
        # one buffer would not exercise the path a real upstream takes.
        for frame in payload.split(b"\n\n"):
            if not frame:
                continue
            self.wfile.write(frame + b"\n\n")
            self.wfile.flush()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8000)
    parser.add_argument("--capture", default=None, help="append every request and reply here as JSONL")
    parser.add_argument("--tool-suffix", default="status", help="which roundhouse tool to call on turn 1")
    parser.add_argument("--echo-prefix", default="ROUNDHOUSE_STATUS=")
    args = parser.parse_args()

    STATE["capture"] = args.capture
    STATE["want_tool"] = args.tool_suffix
    STATE["echo_prefix"] = args.echo_prefix

    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"[fixture-upstream] listening on http://{args.host}:{args.port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
