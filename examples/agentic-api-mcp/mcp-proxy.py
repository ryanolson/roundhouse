#!/usr/bin/env python3
"""A pass-through reverse proxy in front of roundhouse's /mcp, which writes down
what crossed it.

This exists because the assertion the example has to make -- *agentic-api opened
an MCP session against roundhouse and carried the request's `authorization` onto
that leg* -- is a statement about the wire, and roundhouse's own logs do not
print a caller's bearer (correctly). Reading a JSON-RPC transcript off the socket
is the only evidence that does not depend on trusting either side's account of
itself.

The secret never lands in the capture file. What is recorded is
`sha256(secret)`, which is exactly the form `control-plane.json` already carries,
so `run.sh` can prove the presented key *is* the configured one by comparing two
hashes rather than by writing a credential to disk.

Streaming matters: roundhouse's transport may answer `text/event-stream`, and a
proxy that buffered the body to rewrite `Content-Length` would either truncate
an open stream or hang on one. Bodies are forwarded in whatever pieces they
arrive in; a response without an upstream `Content-Length` is delimited by
closing the connection, which is what an SSE reply wants anyway.
"""

import argparse
import hashlib
import http.client
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

STATE = {"capture": None, "lock": threading.Lock(), "target": None}

# Hop-by-hop headers belong to the connection, not the message: forwarding them
# is what makes a proxy that "works" against a JSON reply hang against an SSE one.
HOP_BY_HOP = {"connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
              "te", "trailers", "transfer-encoding", "upgrade", "host", "content-length"}


def record(payload):
    path = STATE["capture"]
    if not path:
        return
    with STATE["lock"]:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(json.dumps({"t": time.time(), **payload}) + "\n")


def credential_view(value):
    """A bearer as evidence rather than as a secret."""
    if not value:
        return None
    scheme, _, secret = value.partition(" ")
    return {
        "scheme": scheme,
        "secret_len": len(secret),
        "secret_prefix": secret[:8],
        "secret_sha256": hashlib.sha256(secret.encode("utf-8")).hexdigest(),
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        print(f"[mcp-proxy] {fmt % args}", flush=True)

    def _forward(self, method):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""

        target = STATE["target"]
        headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP_BY_HOP}
        headers["Host"] = target.netloc
        if body:
            headers["Content-Length"] = str(len(body))

        try:
            request_json = json.loads(body) if body else None
        except json.JSONDecodeError:
            request_json = None
        rpc = request_json if isinstance(request_json, dict) else {}

        entry = {
            "method": method,
            "path": self.path,
            "rpc_method": rpc.get("method"),
            "rpc_id": rpc.get("id"),
            "tool": (rpc.get("params") or {}).get("name") if isinstance(rpc.get("params"), dict) else None,
            "authorization": credential_view(self.headers.get("Authorization")),
            "mcp_session_id": self.headers.get("mcp-session-id"),
            "accept": self.headers.get("Accept"),
            "request_body": request_json,
        }

        connection = http.client.HTTPConnection(target.hostname, target.port or 80, timeout=60)
        try:
            connection.request(method, self.path, body=body or None, headers=headers)
            upstream = connection.getresponse()
            passthrough = [(k, v) for k, v in upstream.getheaders() if k.lower() not in HOP_BY_HOP]
            upstream_length = upstream.getheader("Content-Length")

            self.send_response(upstream.status)
            for key, value in passthrough:
                self.send_header(key, value)
            if upstream_length is not None:
                self.send_header("Content-Length", upstream_length)
            else:
                self.send_header("Connection", "close")
                self.close_connection = True
            self.end_headers()

            collected = bytearray()
            while True:
                chunk = upstream.read1(65536)
                if not chunk:
                    break
                collected.extend(chunk)
                self.wfile.write(chunk)
                self.wfile.flush()

            entry["status"] = upstream.status
            entry["response_content_type"] = upstream.getheader("Content-Type")
            entry["response_body"] = collected.decode("utf-8", "replace")
        except Exception as error:  # noqa: BLE001 -- the failure itself is the evidence
            entry["status"] = None
            entry["error"] = f"{type(error).__name__}: {error}"
            try:
                self.send_error(502, "proxy error")
            except Exception:  # noqa: BLE001
                pass
        finally:
            connection.close()
            record(entry)

    def do_GET(self):
        self._forward("GET")

    def do_POST(self):
        self._forward("POST")

    def do_DELETE(self):
        self._forward("DELETE")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8090)
    parser.add_argument("--target", default="http://127.0.0.1:8080")
    parser.add_argument("--capture", default=None, help="append every exchange here as JSONL")
    args = parser.parse_args()

    STATE["capture"] = args.capture
    STATE["target"] = urlparse(args.target)

    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"[mcp-proxy] http://{args.host}:{args.port} -> {args.target}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
