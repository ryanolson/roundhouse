#!/usr/bin/env python3
"""Capture SGLang routed-expert responses as Rhino observation JSONL."""

from __future__ import annotations

import argparse
import base64
import json
import struct
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any
from urllib.request import Request, urlopen


DEFAULT_PROMPTS = [
    "Explain why the sky is blue to a curious ten year old.",
    "Write a short proof that the square root of two is irrational.",
    "Compare optimistic and pessimistic concurrency control.",
    "Translate 'measure twice, cut once' into French and explain the idiom.",
    "Design a three day strength training plan for a beginner.",
    "What caused the fall of the Western Roman Empire? Give several factors.",
    "Implement binary search and state its loop invariant.",
    "Describe a quiet forest immediately after a summer storm.",
    "Solve 3x + 7 = 31 and show each step.",
    "Explain the difference between precision and recall with an example.",
    "Draft a polite email declining a meeting due to a deadline.",
    "How does photosynthesis store solar energy in chemical bonds?",
    "Give an argument for and against congestion pricing in cities.",
    "Summarize the plot structure of a classic detective story.",
    "List practical ways to make a Python data pipeline reproducible.",
    "Invent a riddle whose answer is a clock.",
]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:30000/generate")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--checkpoint", required=True)
    parser.add_argument("--total-layers", type=int, required=True)
    parser.add_argument("--dense-layers", type=int, default=0)
    parser.add_argument("--num-experts", type=int, required=True)
    parser.add_argument("--top-k", type=int, required=True)
    parser.add_argument("--rows", type=int, default=8)
    parser.add_argument("--decode-steps", type=int, default=128)
    parser.add_argument("--timeout", type=float, default=1800.0)
    parser.add_argument("--server-revision", default="unknown")
    parser.add_argument("--server-args", default="")
    parser.add_argument("--prompts-json", type=Path)
    parser.add_argument("--self-test", action="store_true")
    return parser.parse_args()


def post_json(url: str, payload: dict[str, Any], timeout: float) -> dict[str, Any]:
    request = Request(
        url,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urlopen(request, timeout=timeout) as response:
        body = json.load(response)
    if "error" in body:
        raise RuntimeError(f"SGLang request failed: {body['error']}")
    return body


def decode_routes(response: dict[str, Any], total_layers: int, top_k: int) -> list[list[list[int]]]:
    encoded = response.get("meta_info", {}).get("routed_experts")
    if not encoded:
        raise ValueError("SGLang response is missing meta_info.routed_experts")
    raw = base64.b64decode(encoded)
    if len(raw) % 4:
        raise ValueError("routed_experts payload is not int32-aligned")
    values = struct.unpack(f"={len(raw) // 4}i", raw)
    stride = total_layers * top_k
    if not values or len(values) % stride:
        raise ValueError(
            f"routed_experts has {len(values)} values, not a multiple of {stride}"
        )
    return [
        [
            list(values[token * stride + layer * top_k : token * stride + (layer + 1) * top_k])
            for layer in range(total_layers)
        ]
        for token in range(len(values) // stride)
    ]


def validate_route(
    route: list[int], *, num_experts: int, top_k: int, token: int, layer: int
) -> None:
    if len(route) != top_k:
        raise ValueError(
            f"routed-expert row token={token} layer={layer} has {len(route)} IDs, expected {top_k}"
        )
    invalid = [expert for expert in route if not 0 <= expert < num_experts]
    if invalid:
        raise ValueError(
            f"routed-expert row token={token} layer={layer} has out-of-range IDs {invalid}"
        )
    if len(set(route)) != top_k:
        raise ValueError(
            f"routed-expert row token={token} layer={layer} has duplicate IDs {route}; "
            "the reference capture hook may not have been populated"
        )


def capture_group(
    args: argparse.Namespace, prompts: list[str]
) -> tuple[list[dict[str, Any]], dict[str, list[int]]]:
    if len(prompts) != args.rows:
        raise ValueError(f"capture group has {len(prompts)} prompts, expected {args.rows}")
    payloads = [
        {
            "text": prompt,
            "sampling_params": {
                "temperature": 0,
                "max_new_tokens": args.decode_steps,
                "ignore_eos": True,
            },
            "return_routed_experts": True,
        }
        for prompt in prompts
    ]
    with ThreadPoolExecutor(max_workers=args.rows) as executor:
        responses = list(
            executor.map(
                lambda payload: post_json(args.url, payload, args.timeout), payloads
            )
        )
    routes = [decode_routes(response, args.total_layers, args.top_k) for response in responses]
    available_steps = min(map(len, routes))
    if available_steps < args.decode_steps:
        raise ValueError(
            f"short routed-expert response: {available_steps} < {args.decode_steps} steps"
        )
    routed_layers = args.total_layers - args.dense_layers
    events = []
    for step in range(args.decode_steps):
        source_step = available_steps - args.decode_steps + step
        layers = []
        for output_layer, source_layer in enumerate(
            range(args.dense_layers, args.total_layers)
        ):
            expert_ids = []
            for row in range(args.rows):
                route = routes[row][source_step][source_layer]
                validate_route(
                    route,
                    num_experts=args.num_experts,
                    top_k=args.top_k,
                    token=source_step,
                    layer=source_layer,
                )
                expert_ids.extend(route)
            layers.append(
                {
                    "layer": output_layer,
                    "routing_class": "learned",
                    "expert_ids": expert_ids,
                }
            )
        if len(layers) != routed_layers:
            raise AssertionError("routed layer count mismatch")
        events.append(
            {
                "schema": "rhino-moe-route-observation-v2",
                "step": step,
                "phase": "decode",
                "rows": args.rows,
                "top_k": args.top_k,
                "num_experts": args.num_experts,
                "layers": layers,
            }
        )
    usage = {
        "prompt_tokens": [
            int(response.get("meta_info", {}).get("prompt_tokens", -1))
            for response in responses
        ],
        "completion_tokens": [
            int(response.get("meta_info", {}).get("completion_tokens", -1))
            for response in responses
        ],
        "route_tokens": [len(route) for route in routes],
    }
    return events, usage


def self_test() -> None:
    total_layers = 4
    top_k = 2
    values = list(range(3 * total_layers * top_k))
    raw = struct.pack(f"={len(values)}i", *values)
    decoded = decode_routes(
        {"meta_info": {"routed_experts": base64.b64encode(raw).decode()}},
        total_layers,
        top_k,
    )
    assert decoded[2][3] == [22, 23]
    validate_route([1, 2], num_experts=4, top_k=2, token=0, layer=0)
    try:
        validate_route([0, 0], num_experts=4, top_k=2, token=0, layer=0)
    except ValueError:
        pass
    else:
        raise AssertionError("duplicate routed experts were accepted")


def main() -> None:
    args = parse_args()
    if args.self_test:
        self_test()
        return
    if args.total_layers <= args.dense_layers or args.rows <= 0 or args.decode_steps <= 0:
        raise ValueError("invalid layer, row, or decode-step configuration")
    prompts = DEFAULT_PROMPTS
    if args.prompts_json:
        prompts = json.loads(args.prompts_json.read_text())
    if len(prompts) < args.rows or len(prompts) % args.rows:
        raise ValueError("prompt count must be a positive multiple of --rows")

    started = time.time()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    event_count = 0
    usages: list[dict[str, list[int]]] = []
    with args.output.open("w") as output:
        for start in range(0, len(prompts), args.rows):
            events, usage = capture_group(args, prompts[start : start + args.rows])
            usages.append(usage)
            for event in events:
                event["step"] = event_count
                output.write(json.dumps(event, separators=(",", ":")) + "\n")
                event_count += 1
    metadata = {
        "schema": "rhino-moe-route-capture-metadata-v1",
        "checkpoint": args.checkpoint,
        "server_revision": args.server_revision,
        "server_args": args.server_args,
        "url": args.url,
        "prompt_count": len(prompts),
        "rows": args.rows,
        "decode_steps_per_group": args.decode_steps,
        "events": event_count,
        "routed_layers": args.total_layers - args.dense_layers,
        "num_experts": args.num_experts,
        "top_k": args.top_k,
        "elapsed_seconds": time.time() - started,
        "prompts": prompts,
        "groups": usages,
    }
    args.output.with_suffix(args.output.suffix + ".metadata.json").write_text(
        json.dumps(metadata, indent=2) + "\n"
    )


if __name__ == "__main__":
    main()
