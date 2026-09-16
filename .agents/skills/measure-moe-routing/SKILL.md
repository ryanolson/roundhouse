---
name: measure-moe-routing
description: Onboard MoE routing for a model by capturing expert selections from real-weight traffic, testing whether token routes are IID, fitting and plotting Rhino's beta-rank expert-inclusion distribution, and configuring deterministic synthetic routing for profile, search, and mocker. Use only when the user explicitly invokes or requests this workflow, or when adding MoE profiling support for a model for the first time. Never use for routine profile analysis, benchmark discrepancies, MoE debugging, refactoring, or tuning an already-onboarded model unless the user explicitly asks for routing measurement or calibration.
---

# Measure MoE Routing

Capture native top-k decisions, fit the batch-independent ranked inclusion law,
and validate the timing-relevant load statistics before changing model constants.

## Scope boundary

Treat this as a model-onboarding workflow, not a general MoE troubleshooting
procedure. Invoke it implicitly only while establishing a model's initial MoE
profile distribution. For an existing model, use it only when the user
explicitly asks to measure, capture, fit, or recalibrate routing behavior.

Do not invoke this skill merely because profile and serving timings differ, an
MoE kernel is under investigation, or routing-related code is being reviewed.
Use the ordinary profiling, benchmarking, or code-review workflow instead.

## Provide route observations

Collect expert IDs with temporary diagnostic tooling outside the production
serving graph. Instrument immediately after the native router and before A2A,
record only one TP replica for each independent token batch, and remove the
instrumentation after calibration. Store one JSON event per decode step with
this schema:

```json
{
  "schema": "rhino-moe-route-observation-v2",
  "step": 42,
  "phase": "decode",
  "rows": 8,
  "top_k": 4,
  "num_experts": 128,
  "layers": [
    {
      "layer": 0,
      "routing_class": "learned",
      "expert_ids": [12, 75, 3, 99]
    }
  ]
}
```

Flatten fields in row-major `[rows, top_k]` order and emit all layers together.
Capture expert IDs only: combine-weight values do not determine MoE kernel load
and are synthesized by the profile backend.

Use varied prompts and enough decode steps to cover routing behavior. Record the
actual ISL, OSL, concurrency, model checkpoint, backend configuration, and git
revision alongside the trace. Do not benchmark with instrumentation enabled.

## Fit and plot

Run the bundled analyzer through the locked environment:

```bash
MPLCONFIGDIR=/tmp/matplotlib uv run python \
  .agents/skills/measure-moe-routing/scripts/fit_moe_distribution.py \
  target/routes/observations.jsonl \
  --output target/routes/fit.json \
  --plot target/routes/fit.png \
  --phase decode \
  --num-experts 128 \
  --top-k 4 \
  --rows 8 \
  --num-layers 36 \
  --ep-sizes 2,4,8
```

Set parameters from the model and the profile shape:

- `--num-experts` and `--top-k`: model routing configuration.
- `--rows`: steady-state full-batch rows; partial batches remain in the trace
  histogram but are excluded from fitting.
- `--num-layers`: expected routed layer count; fail on incomplete events.
- `--ep-sizes`: every expert-parallel topology to compare.
- `--layer-group`: optional graph-equivalent layer classes, using
  `name=start:stop:step` or comma-separated indices. Omit it when the combined
  fit is accurate enough for all routed layers.
- `--seed-candidates`: deterministic representative cycles tested per class;
  the default 1024 is appropriate for final calibration.

The script fits

`logit(q_r) = intercept - head_shape*ln(x_r) + tail_shape*ln(1-x_r)`,

where `q_r` is the probability that ranked expert `r` appears in a token's
top-k, `x_r=(r+0.5)/num_experts`, and `sum(q)=top_k`. It writes Rust-ready shape
parameters and a seed, plots observed versus fitted inclusion and batch-size
extrapolation, and compares observed batches against both an IID shuffled-token
control and generated representative routes.

Do not use the IID generator if the shuffled control materially changes active
experts, busiest-expert rows, or pair-route overlap. In that case the model
needs a correlated route law.

## Configure the profile

Store only `head_shape`, `tail_shape`, and the selected seed in the model crate.
Declare a static `rhino_model_spec::MoeLogitBetaRankDistribution` and return it
from `ProfileModelFamily::moe_profile_layer` inside a `MoeProfileLayerSpec`.
The generic `moe_profile_plan` path is shared by profile, search, mocker, and
dummy-weight materialization. Models must not construct `MoeProfileRequest`,
expand EP source ranks, or build route-plan keys themselves. Route-cycle length
follows the `MoeProfileWorkload`; dummy router weights use
`MoeProfileWorkload::materialization` with their synthetic one-hot capacity.

For expert/data parallel profiles, include the EP source rank in the
deterministic draw seed so different token batches are independent. Do not salt
by TP rank: tensor-parallel replicas must make identical routing decisions.
The A2A receive artifact must aggregate every source rank's plan rather than
replicating rank 0.

Compare generated and observed values for:

- mean partition fanout;
- mean rows per partition;
- active experts and maximum expert load;
- pair-route overlap;
- the active-expert curve over every relevant batch size.

Then run `rhino profile`, `rhino search-config`, and a real-weight benchmark with
the same batch shape and backend choices. Compare profile block spans with
benchmark step latency; do not sum overlapping CUPTI kernel durations.

## GPT-OSS-120B reference setup

The reference measurement used 16 varied prompts, actual ISL 137, OSL 512,
concurrency/full rows 8, all 36 layers, and 1,022 full decode steps. That yielded
294,336 layer-token rows. A single all-layer fit produced `head_shape =
0.4848297478852521`, `tail_shape = 2.47523696506534`, and seed
`0x779870d7c06d5817`; splitting sliding/full attention did not materially
improve the timing-relevant batch statistics.
