#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Capture sanitized Claude Code Messages requests on a loopback mock."""

import json
import os
import subprocess
import threading
import uuid
from argparse import ArgumentParser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def cache_control(value):
    if not isinstance(value, dict):
        return None
    return {key: value[key] for key in ("type", "ttl") if key in value}


def content_types(value):
    if isinstance(value, str):
        return ["text"]
    if not isinstance(value, list):
        return []
    return [block.get("type", "unknown") for block in value if isinstance(block, dict)]


def user_id_summary(value, root_session_id, request_session_id):
    if not isinstance(value, str):
        return {"present": False, "encoding": "absent_or_non_string"}
    try:
        decoded = json.loads(value)
    except json.JSONDecodeError:
        return {
            "present": True,
            "encoding": "opaque_string",
            "contains_root_session_id": root_session_id in value,
            "contains_request_header_session_id": request_session_id in value,
        }
    if not isinstance(decoded, dict):
        return {
            "present": True,
            "encoding": "json_non_object",
            "contains_root_session_id": root_session_id in value,
            "contains_request_header_session_id": request_session_id in value,
        }
    return {
        "present": True,
        "encoding": "json_encoded_string_object",
        "json_key_names": sorted(decoded),
        "root_session_id_key_names": sorted(
            key for key, item in decoded.items() if item == root_session_id
        ),
        "request_header_session_id_key_names": sorted(
            key for key, item in decoded.items() if item == request_session_id
        ),
    }


def body_summary(body, root_session_id, request_session_id):
    metadata = body.get("metadata") if isinstance(body.get("metadata"), dict) else {}
    user_id = metadata.get("user_id")
    system = body.get("system")
    messages = body.get("messages") if isinstance(body.get("messages"), list) else []
    breakpoints = []

    def add_breakpoint(location, value):
        control = cache_control(value)
        if control is not None:
            breakpoints.append({"location": location, "cache_control": control})

    add_breakpoint("request", body.get("cache_control"))
    if isinstance(system, list):
        for index, block in enumerate(system):
            if isinstance(block, dict):
                add_breakpoint(f"system[{index}]", block.get("cache_control"))
    for message_index, message in enumerate(messages):
        content = message.get("content") if isinstance(message, dict) else None
        if isinstance(content, list):
            for block_index, block in enumerate(content):
                if isinstance(block, dict):
                    add_breakpoint(
                        f"messages[{message_index}].content[{block_index}]",
                        block.get("cache_control"),
                    )
    return {
        "top_level_keys": sorted(body),
        "model": body.get("model"),
        "stream": body.get("stream"),
        "message_roles": [message.get("role") for message in messages if isinstance(message, dict)],
        "message_content_types": [
            content_types(message.get("content")) for message in messages if isinstance(message, dict)
        ],
        "system_content_types": content_types(system),
        "metadata_keys": sorted(metadata),
        "metadata_user_id": user_id_summary(user_id, root_session_id, request_session_id),
        "parent_tool_use_id_present": isinstance(body.get("parent_tool_use_id"), str),
        "context_management_edit_types": [
            edit.get("type")
            for edit in body.get("context_management", {}).get("edits", [])
            if isinstance(edit, dict)
        ]
        if isinstance(body.get("context_management"), dict)
        else [],
        "cache_breakpoints": breakpoints,
        "tool_names": [tool.get("name") for tool in body.get("tools", []) if isinstance(tool, dict)],
    }


class CaptureHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    fixtures = []
    root_session_id = None
    task_mode = False
    agent_call_issued = False

    def log_message(self, _format, *_args):
        return

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw_body = self.rfile.read(length)
        try:
            body = json.loads(raw_body)
        except json.JSONDecodeError:
            body = {}
        request_session_id = self.headers.get("x-claude-code-session-id", "")
        fixture = {
            "request": {
                "method": self.command,
                "path": self.path,
                "authorization_present": any(
                    header.lower() in {"authorization", "x-api-key", "proxy-authorization"}
                    for header in self.headers
                ),
                "non_sensitive_header_names": sorted(
                    header.lower()
                    for header in self.headers
                    if header.lower() not in {"authorization", "x-api-key", "proxy-authorization", "cookie"}
                ),
                "headers": {
                    header: self.headers[header]
                    for header in ("anthropic-version", "anthropic-beta", "user-agent", "accept", "content-type")
                    if header in self.headers
                },
                "identity_header_matches_root_session_id": {
                    header: self.headers.get(header) == type(self).root_session_id
                    for header in (
                        "session-id",
                        "thread-id",
                        "x-session-id",
                        "x-thread-id",
                        "x-claude-code-session-id",
                        "x-client-request-id",
                    )
                    if header in self.headers
                },
                "body": body_summary(body, type(self).root_session_id, request_session_id),
            }
        }
        request_number = len(type(self).fixtures)
        type(self).fixtures.append(fixture)
        response = {
            "id": "msg_roundhouse_wire_fixture",
            "type": "message",
            "role": "assistant",
            "model": body.get("model", "claude-sonnet-4-5"),
            "content": [{"type": "text", "text": "WIRE_FIXTURE_OK"}],
            "stop_reason": "end_turn",
            "stop_sequence": None,
            "usage": {
                "input_tokens": 1,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
                "output_tokens": 1,
            },
        }
        if type(self).task_mode and not type(self).agent_call_issued and "Agent" in fixture["request"]["body"]["tool_names"]:
            response["content"] = [
                {
                    "type": "tool_use",
                    "id": "toolu_roundhouse_wire_fixture",
                    "name": "Agent",
                    "input": {
                        "description": "Run the synthetic child fixture.",
                        "prompt": "Return exactly CHILD_FIXTURE_OK.",
                        "subagent_type": "general-purpose",
                    },
                }
            ]
            response["stop_reason"] = "tool_use"
            type(self).agent_call_issued = True
        encoded = json.dumps(response).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


def main():
    parser = ArgumentParser()
    parser.add_argument("--agent-tool", action="store_true", help="Ask the mock to invoke one built-in Agent subagent.")
    arguments = parser.parse_args()
    synthetic_session_id = str(uuid.uuid4())
    CaptureHandler.root_session_id = synthetic_session_id
    CaptureHandler.task_mode = arguments.agent_tool
    server = ThreadingHTTPServer(("127.0.0.1", 0), CaptureHandler)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    environment = os.environ.copy()
    environment["ANTHROPIC_BASE_URL"] = f"http://127.0.0.1:{server.server_port}"
    command = [
        "timeout",
        "90",
        "claude",
        "-p",
        "--safe-mode",
        "--no-session-persistence",
        "--tools",
        "Agent" if arguments.agent_tool else "",
        "--permission-mode",
        "dontAsk",
        "--permission-prompts",
        "none",
        "--output-format",
        "json",
        "--max-budget-usd",
        "0.01",
        "--session-id",
        synthetic_session_id,
        "Return exactly WIRE_FIXTURE_OK.",
    ]
    try:
        result = subprocess.run(command, env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    finally:
        server.shutdown()
        server.server_close()
    print(
        json.dumps(
            {
                "claude_exit_code": result.returncode,
                "mode": "agent_tool" if arguments.agent_tool else "single_turn",
                "requests": CaptureHandler.fixtures,
            },
            indent=2,
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
