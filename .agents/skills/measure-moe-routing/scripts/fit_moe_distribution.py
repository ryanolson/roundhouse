#!/usr/bin/env python3
"""Fit and validate Rhino's IID beta-rank MoE routing distribution."""

from __future__ import annotations

import argparse
import json
import math
from collections import Counter
from pathlib import Path
from typing import Any

import matplotlib.pyplot as plt
import numpy as np
from scipy.optimize import brentq, minimize
from scipy.special import expit


MASK64 = (1 << 64) - 1
PHASES = {"prefill", "decode"}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("trace", type=Path, help="route JSONL or server log")
    parser.add_argument("--output", type=Path, required=True, help="fit JSON")
    parser.add_argument("--plot", type=Path, required=True, help="fit plot (.png or .svg)")
    parser.add_argument("--prefix", default="", help="text immediately before each JSON event")
    parser.add_argument("--phase", default="decode")
    parser.add_argument("--routing-class", default="learned")
    parser.add_argument("--rows", type=int, required=True)
    parser.add_argument("--num-experts", type=int, required=True)
    parser.add_argument("--top-k", type=int, required=True)
    parser.add_argument("--num-layers", type=int)
    parser.add_argument("--ep-sizes", default="1", help="comma-separated EP sizes")
    parser.add_argument(
        "--batch-sizes",
        default="1,2,4,8,16,32,64,128",
        help="IID batch sizes used to validate extrapolation",
    )
    parser.add_argument(
        "--layer-group",
        action="append",
        default=[],
        metavar="NAME=INDICES",
        help="group with comma indices or start:stop[:step]",
    )
    parser.add_argument(
        "--seed-candidates",
        type=int,
        default=1024,
        help="deterministic seeds evaluated against captured batch metrics",
    )
    return parser.parse_args()


def parse_indices(raw: str) -> list[int]:
    if ":" not in raw:
        return [int(value) for value in raw.split(",") if value]
    parts = [int(value) if value else None for value in raw.split(":")]
    if len(parts) not in (2, 3) or parts[1] is None:
        raise ValueError(f"invalid range {raw!r}")
    start = parts[0] or 0
    step = parts[2] if len(parts) == 3 and parts[2] is not None else 1
    return list(range(start, parts[1], step))


def parse_groups(raw_groups: list[str], num_layers: int) -> dict[str, list[int]]:
    if not raw_groups:
        return {"all": list(range(num_layers))}
    groups: dict[str, list[int]] = {}
    for raw in raw_groups:
        name, separator, spec = raw.partition("=")
        if not separator or not name or name in groups:
            raise ValueError(f"invalid or duplicate layer group {raw!r}")
        indices = parse_indices(spec)
        if not indices or any(index < 0 or index >= num_layers for index in indices):
            raise ValueError(f"layer group {name!r} lies outside [0, {num_layers})")
        groups[name] = indices
    return groups


def decode_line(line: str, prefix: str) -> dict[str, Any] | None:
    if prefix:
        position = line.find(prefix)
        if position < 0:
            return None
        line = line[position + len(prefix) :]
    else:
        line = line.strip()
        if not line.startswith("{"):
            return None
    return json.loads(line)


def load_routes(args: argparse.Namespace) -> tuple[np.ndarray, dict[int, int]]:
    events: list[list[np.ndarray]] = []
    row_histogram: Counter[int] = Counter()
    with args.trace.open() as source:
        for line in source:
            event = decode_line(line, args.prefix)
            if event is None or event.get("phase", args.phase) != args.phase:
                continue
            rows = int(event["rows"])
            row_histogram[rows] += 1
            if rows != args.rows:
                continue
            if int(event.get("top_k", args.top_k)) != args.top_k:
                raise ValueError("trace top_k does not match --top-k")
            if int(event.get("num_experts", args.num_experts)) != args.num_experts:
                raise ValueError("trace num_experts does not match --num-experts")
            layers = event["layers"]
            if args.num_layers is not None and len(layers) != args.num_layers:
                raise ValueError(f"expected {args.num_layers} layers, found {len(layers)}")
            parsed_layers = []
            for expected_layer, layer in enumerate(layers):
                if int(layer.get("layer", expected_layer)) != expected_layer:
                    raise ValueError("trace layers must be dense and ordered")
                ids = np.asarray(layer["expert_ids"], dtype=np.int64).reshape(
                    rows, args.top_k
                )
                if np.any(ids < 0) or np.any(ids >= args.num_experts):
                    raise ValueError("trace contains an out-of-range expert ID")
                if np.any(np.diff(np.sort(ids, axis=1), axis=1) == 0):
                    raise ValueError("trace repeats an expert within a token route")
                parsed_layers.append(ids)
            events.append(parsed_layers)
    if not events:
        raise ValueError("trace contains no matching full-row events")
    routes = np.asarray(events, dtype=np.int64)
    return routes, dict(sorted(row_histogram.items()))


def ranked_inclusion(routes: np.ndarray, num_experts: int) -> np.ndarray:
    probabilities = np.empty((routes.shape[1], num_experts), dtype=np.float64)
    token_rows = routes.shape[0] * routes.shape[2]
    for layer in range(routes.shape[1]):
        probabilities[layer] = np.bincount(
            routes[:, layer].ravel(), minlength=num_experts
        ) / token_rows
    return np.sort(probabilities, axis=1)[:, ::-1]


def shuffled_token_control(routes: np.ndarray) -> np.ndarray:
    """Preserve each token's route and each layer's marginals, but rebuild batches IID."""
    rng = np.random.default_rng(0x105)
    events, layers, rows, top_k = routes.shape
    shuffled = np.empty_like(routes)
    for layer in range(layers):
        token_routes = routes[:, layer].reshape(events * rows, top_k)
        shuffled[:, layer] = token_routes[rng.permutation(len(token_routes))].reshape(
            events, rows, top_k
        )
    return shuffled


def beta_rank_design(num_experts: int) -> np.ndarray:
    quantiles = (np.arange(num_experts, dtype=np.float64) + 0.5) / num_experts
    return np.column_stack(
        (
            np.ones(num_experts),
            -np.log(quantiles),
            np.log1p(-quantiles),
        )
    )


def fit_beta_rank(target: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    design = beta_rank_design(len(target))
    total_inclusion = float(target.sum())

    def objective(shape: np.ndarray) -> tuple[float, np.ndarray]:
        _, fitted, logits = normalized_beta_rank(design, total_inclusion, shape)
        loss = float(np.sum(np.logaddexp(0.0, logits) - target * logits))
        # The intercept is implicitly normalized so sum(fitted - target) == 0;
        # its derivative therefore cancels from the likelihood gradient.
        gradient = design[:, 1:].T @ (fitted - target)
        return loss, gradient

    result = minimize(
        objective,
        np.asarray([0.5, 2.5], dtype=np.float64),
        method="L-BFGS-B",
        jac=True,
        bounds=((0.0, None), (0.0, None)),
        options={"ftol": 1e-15, "gtol": 1e-12, "maxiter": 1000},
    )
    if not result.success:
        raise ValueError(f"beta-rank fit failed: {result.message}")
    intercept, fitted, _ = normalized_beta_rank(design, total_inclusion, result.x)
    coefficients = np.asarray([intercept, *result.x], dtype=np.float64)
    return coefficients, fitted


def solve_intercept(
    num_experts: int, top_k: int, head_shape: float, tail_shape: float
) -> tuple[float, np.ndarray]:
    design = beta_rank_design(num_experts)
    intercept, probabilities, _ = normalized_beta_rank(
        design,
        float(top_k),
        np.asarray([head_shape, tail_shape], dtype=np.float64),
    )
    return intercept, probabilities


def normalized_beta_rank(
    design: np.ndarray, total_inclusion: float, shape: np.ndarray
) -> tuple[float, np.ndarray, np.ndarray]:
    shape_logits = design[:, 1:] @ shape
    intercept = brentq(
        lambda candidate: float(expit(candidate + shape_logits).sum() - total_inclusion),
        -64.0,
        64.0,
    )
    logits = intercept + shape_logits
    return intercept, expit(logits), logits


def mix64(value: int) -> int:
    value &= MASK64
    value = ((value ^ (value >> 30)) * 0xBF58476D1CE4E5B9) & MASK64
    value = ((value ^ (value >> 27)) * 0x94D049BB133111EB) & MASK64
    return (value ^ (value >> 31)) & MASK64


def splitmix64(state: int) -> tuple[int, int]:
    state = (state + 0x9E3779B97F4A7C15) & MASK64
    return state, mix64(state)


def stable_hash(value: bytes) -> int:
    result = 0xCBF29CE484222325
    for byte in value:
        result = ((result ^ byte) * 0x00000100000001B3) & MASK64
    return result


def layer_seed(base: int, routing_class: str, layer: int, source_rank: int = 0) -> int:
    return mix64(
        base
        ^ stable_hash(routing_class.encode())
        ^ ((layer * 0x9E3779B97F4A7C15) & MASK64)
        ^ ((source_rank * 0x8CB92BAAF1337F5D) & MASK64)
    )


def low_discrepancy_permutation(num_experts: int, seed: int) -> np.ndarray:
    bits = (num_experts - 1).bit_length()
    domain = 1 << bits
    offset = seed % num_experts
    values = []
    for index in range(domain):
        reversed_index = int(f"{index:0{bits}b}"[::-1], 2) if bits else 0
        if reversed_index < num_experts:
            values.append((reversed_index + offset) % num_experts)
    return np.asarray(values, dtype=np.int64)


def dependent_round(probabilities: np.ndarray, top_k: int, state: int) -> tuple[np.ndarray, int]:
    values = probabilities.copy()
    order = list(range(len(values)))
    for index in range(len(order) - 1, 0, -1):
        state, random = splitmix64(state)
        swap_with = random % (index + 1)
        order[index], order[swap_with] = order[swap_with], order[index]
    while True:
        fractional = [
            index for index in order if 1e-12 < values[index] < 1.0 - 1e-12
        ]
        if len(fractional) < 2:
            break
        left, right = fractional[:2]
        alpha = min(1.0 - values[left], values[right])
        beta = min(values[left], 1.0 - values[right])
        state, random = splitmix64(state)
        uniform = ((random >> 11) + 0.5) / float(1 << 53)
        if uniform < beta / (alpha + beta):
            values[left] += alpha
            values[right] -= alpha
        else:
            values[left] -= beta
            values[right] += beta
        values[np.abs(values) <= 1e-12] = 0.0
        values[np.abs(values - 1.0) <= 1e-12] = 1.0
    selected = np.flatnonzero(values >= 0.5)
    if len(selected) != top_k:
        raise ValueError(f"dependent rounding selected {len(selected)} experts, expected {top_k}")
    return selected, state


def generate_routes(
    probabilities: np.ndarray,
    rows: int,
    top_k: int,
    base_seed: int,
    routing_class: str,
    layer: int,
    source_rank: int = 0,
) -> np.ndarray:
    seed = layer_seed(base_seed, routing_class, layer, source_rank)
    permutation = low_discrepancy_permutation(len(probabilities), seed)
    result = np.empty((rows, top_k), dtype=np.int64)
    for row in range(rows):
        state = mix64(
            seed
            ^ ((row * 0xD6E8FEB86659FD93) & MASK64)
            ^ 0xA0761D6478BD642F
        )
        ranks, _ = dependent_round(probabilities, top_k, state)
        result[row] = permutation[ranks]
    return result


def batch_metrics(routes: np.ndarray, num_experts: int, ep_sizes: list[int]) -> dict[str, float]:
    flattened = routes.reshape(-1, routes.shape[-2], routes.shape[-1])
    active, busiest, overlap = [], [], []
    ep_fanout: dict[int, list[float]] = {size: [] for size in ep_sizes}
    for batch in flattened:
        counts = np.bincount(batch.ravel(), minlength=num_experts)
        active.append(float(np.count_nonzero(counts)))
        busiest.append(float(counts.max()))
        for left in range(len(batch)):
            for right in range(left + 1, len(batch)):
                overlap.append(float(np.isin(batch[left], batch[right]).sum()))
        for ep_size in ep_sizes:
            owners = batch // (num_experts // ep_size)
            ep_fanout[ep_size].extend(
                float(len(set(row.tolist()))) for row in owners
            )
    metrics = {
        "active_experts": float(np.mean(active)),
        "busiest_expert_rows": float(np.mean(busiest)),
        "pair_route_overlap": float(np.mean(overlap)) if overlap else 0.0,
    }
    metrics.update(
        {f"ep{size}_fanout": float(np.mean(values)) for size, values in ep_fanout.items()}
    )
    return metrics


def metric_error(generated: dict[str, float], observed: dict[str, float]) -> float:
    return sum(
        ((generated[key] - value) / max(abs(value), 1.0)) ** 2
        for key, value in observed.items()
    )


def choose_seed(
    group_name: str,
    layer_indices: list[int],
    probabilities: np.ndarray,
    observed: dict[str, float],
    args: argparse.Namespace,
    ep_sizes: list[int],
) -> tuple[int, dict[str, float], np.ndarray]:
    best: tuple[float, int, dict[str, float], np.ndarray] | None = None
    representative_layer = layer_indices[0]
    for candidate in range(args.seed_candidates):
        seed = mix64(stable_hash(group_name.encode()) ^ candidate)
        generated_routes = generate_routes(
            probabilities,
            args.rows,
            args.top_k,
            seed,
            args.routing_class,
            representative_layer,
        )
        generated = batch_metrics(
            generated_routes[None, None], args.num_experts, ep_sizes
        )
        error = metric_error(generated, observed)
        if best is None or error < best[0]:
            best = (error, seed, generated, generated_routes)
    assert best is not None
    return best[1], best[2], best[3]


def active_curve(probabilities: np.ndarray, batch_sizes: np.ndarray) -> np.ndarray:
    return np.asarray(
        [
            np.mean(np.sum(1.0 - (1.0 - probabilities) ** batch, axis=1))
            for batch in batch_sizes
        ]
    )


def metric_ratio(generated: float, observed: float) -> float:
    if observed == 0.0:
        return 1.0 if generated == 0.0 else math.nan
    return generated / observed


def make_plot(
    path: Path,
    ranked: np.ndarray,
    layer_coefficients: np.ndarray,
    groups: dict[str, list[int]],
    group_results: dict[str, dict[str, Any]],
    batch_sizes: np.ndarray,
) -> None:
    colors = plt.get_cmap("tab10")
    ranks = np.arange(1, ranked.shape[1] + 1)
    fig, axes = plt.subplots(2, 2, figsize=(13.5, 9), constrained_layout=True)
    for group_index, (name, indices) in enumerate(groups.items()):
        color = colors(group_index)
        target = ranked[indices].mean(axis=0)
        fit = np.asarray(group_results[name]["fitted_inclusion"])
        axes[0, 0].plot(ranks, target, color=color, lw=2.4, label=f"{name} observed")
        axes[0, 0].plot(ranks, fit, color=color, ls="--", lw=2, label=f"{name} fit")
    axes[0, 0].set_yscale("log")
    axes[0, 0].set_title("Ranked expert inclusion probability")
    axes[0, 0].set_xlabel("Popularity rank")
    axes[0, 0].set_ylabel("P(expert is in token top-k)")
    axes[0, 0].legend(frameon=False)

    layers = np.arange(len(layer_coefficients))
    axes[0, 1].scatter(layers, layer_coefficients[:, 1], label="hot-head shape", s=28)
    axes[0, 1].scatter(layers, layer_coefficients[:, 2], label="tail-decay shape", s=28)
    axes[0, 1].set_title("Batch-invariant shape parameters")
    axes[0, 1].set_xlabel("Layer")
    axes[0, 1].set_ylabel("Coefficient")
    axes[0, 1].legend(frameon=False)

    for group_index, (name, indices) in enumerate(groups.items()):
        color = colors(group_index)
        observed_curve = active_curve(ranked[indices], batch_sizes)
        fit = np.asarray(group_results[name]["fitted_inclusion"])[None, :]
        fitted_curve = active_curve(fit, batch_sizes)
        axes[1, 0].plot(batch_sizes, observed_curve, color=color, lw=2.4, label=f"{name} observed")
        axes[1, 0].plot(batch_sizes, fitted_curve, color=color, ls="--", lw=2, label=f"{name} fit")
    axes[1, 0].set_title("Expected active experts for IID token rows")
    axes[1, 0].set_xlabel("Batch/token rows")
    axes[1, 0].set_ylabel("Expected active experts")
    axes[1, 0].legend(frameon=False)

    metric_names = ["active_experts", "busiest_expert_rows", "pair_route_overlap"]
    labels = ["Active", "Busiest", "Overlap"]
    x = np.arange(len(metric_names))
    width = 0.8 / max(len(groups), 1)
    for group_index, (name, result) in enumerate(group_results.items()):
        ratios = []
        for key in metric_names:
            generated = result["generated_batch_metrics"][key]
            observed = result["observed_batch_metrics"][key]
            ratios.append(metric_ratio(generated, observed))
        axes[1, 1].bar(x + (group_index - (len(groups) - 1) / 2) * width, ratios, width, color=colors(group_index), label=name)
    axes[1, 1].axhline(1.0, color="#0f172a", ls="--", lw=1.5)
    axes[1, 1].set_xticks(x, labels)
    axes[1, 1].set_title("Generated batch metrics / observed")
    axes[1, 1].legend(frameon=False)
    path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(path, dpi=180, bbox_inches="tight")
    plt.close(fig)


def main() -> None:
    args = parse_args()
    if args.phase not in PHASES:
        raise ValueError(f"phase must be one of {sorted(PHASES)}")
    if args.rows <= 0 or args.num_experts <= 0 or not 0 < args.top_k <= args.num_experts:
        raise ValueError("require positive rows and 1 <= top_k <= num_experts")
    if args.seed_candidates <= 0:
        raise ValueError("--seed-candidates must be positive")
    ep_sizes = [int(value) for value in args.ep_sizes.split(",") if value]
    if not ep_sizes or any(size <= 0 or args.num_experts % size for size in ep_sizes):
        raise ValueError("each EP size must be positive and divide num_experts")
    batch_sizes = np.asarray(
        [int(value) for value in args.batch_sizes.split(",") if value], dtype=np.int64
    )
    if np.any(batch_sizes <= 0):
        raise ValueError("batch sizes must be positive")

    routes, row_histogram = load_routes(args)
    iid_control_routes = shuffled_token_control(routes)
    groups = parse_groups(args.layer_group, routes.shape[1])
    ranked = ranked_inclusion(routes, args.num_experts)
    fitted_layers, coefficients = [], []
    for target in ranked:
        layer_coefficients, fitted = fit_beta_rank(target)
        coefficients.append(layer_coefficients)
        fitted_layers.append(fitted)
    coefficients_array = np.asarray(coefficients)
    fitted_layers_array = np.asarray(fitted_layers)

    group_results: dict[str, dict[str, Any]] = {}
    for name, indices in groups.items():
        head_shape = float(coefficients_array[indices, 1].mean())
        tail_shape = float(coefficients_array[indices, 2].mean())
        intercept, fitted = solve_intercept(
            args.num_experts, args.top_k, head_shape, tail_shape
        )
        observed_routes = routes[:, indices]
        observed_metrics = batch_metrics(observed_routes, args.num_experts, ep_sizes)
        iid_control_metrics = batch_metrics(
            iid_control_routes[:, indices], args.num_experts, ep_sizes
        )
        seed, generated_metrics, representative_routes = choose_seed(
            name,
            indices,
            fitted,
            observed_metrics,
            args,
            ep_sizes,
        )
        observed_active = active_curve(ranked[indices], batch_sizes)
        fitted_active = active_curve(fitted[None, :], batch_sizes)
        group_results[name] = {
            "layer_indices": indices,
            "head_shape": head_shape,
            "tail_shape": tail_shape,
            "intercept_for_captured_shape": intercept,
            "seed": seed,
            "seed_hex": f"0x{seed:016x}",
            "fitted_inclusion": fitted.tolist(),
            "inclusion_probability_sum": float(fitted.sum()),
            "ranked_inclusion_rmse": float(
                np.sqrt(np.mean((fitted - ranked[indices].mean(axis=0)) ** 2))
            ),
            "active_expert_curve_rmse": float(
                np.sqrt(np.mean((fitted_active - observed_active) ** 2))
            ),
            "observed_batch_metrics": observed_metrics,
            "iid_control_batch_metrics": iid_control_metrics,
            "generated_batch_metrics": generated_metrics,
            "representative_routes": representative_routes.tolist(),
            "layer_parameter_std": {
                "head_shape": float(coefficients_array[indices, 1].std()),
                "tail_shape": float(coefficients_array[indices, 2].std()),
            },
        }

    result = {
        "schema": "rhino-moe-iid-beta-rank-fit-v1",
        "source": str(args.trace),
        "parameters": {
            "phase": args.phase,
            "routing_class": args.routing_class,
            "rows": args.rows,
            "num_experts": args.num_experts,
            "top_k": args.top_k,
            "num_layers": int(routes.shape[1]),
            "ep_sizes": ep_sizes,
            "batch_sizes": batch_sizes.tolist(),
        },
        "trace": {
            "row_histogram": row_histogram,
            "full_events": int(routes.shape[0]),
            "layer_token_rows": int(routes.shape[0] * routes.shape[1] * routes.shape[2]),
        },
        "fit": {
            "layer_probability_rmse": float(
                np.sqrt(np.mean((fitted_layers_array - ranked) ** 2))
            ),
            "groups": group_results,
        },
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    make_plot(args.plot, ranked, coefficients_array, groups, group_results, batch_sizes)
    print(f"events={routes.shape[0]:,} layer-token-rows={result['trace']['layer_token_rows']:,}")
    for name, fit in group_results.items():
        print(
            f"{name}: head={fit['head_shape']:.6f} tail={fit['tail_shape']:.6f} "
            f"seed={fit['seed_hex']} active-curve-rmse={fit['active_expert_curve_rmse']:.4f}"
        )
    print(f"fit: {args.output}")
    print(f"plot: {args.plot}")


if __name__ == "__main__":
    main()
