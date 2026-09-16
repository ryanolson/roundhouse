---
name: analyze-profile
description: Use when analyzing Rhino `rhino profile` outputs or `rhino search-config` profile timing, including nodes.csv, blocks.csv, kernel_invocations.csv, CUPTI kernel traces, multiplicity, composite versus isolated measurements, end-to-end latency, hotspot rankings, or profile CSV questions.
---

# Analyze Rhino Profile Outputs

Use this skill to interpret the CSVs produced by `rhino profile`. Be precise about
which question is being answered before aggregating numbers.

## First Decide The Metric

- **End-to-end latency**: start from `blocks.csv`. Block timings are spans from
  earliest CUPTI kernel start to latest CUPTI kernel end for a profiling unit, so
  they preserve overlap across streams better than summing kernel durations.
- **Kernel optimization opportunity**: use `kernel_invocations.csv`, usually
  filtered to `measurement == "composite"`, then aggregate inside each replay
  unit before averaging. This is GPU work attributed to kernel groups, not
  wall-clock latency.
- **Per-node timing**: use `nodes.csv` for the Rhino shadow-graph node view.
- **Composite versus isolated effects**: compare `measurement == "composite"`
  rows with `measurement == "isolated"` rows, but never add them together.

## CSV Meanings

`nodes.csv` has one row per Rhino graph node. It includes node labels, parsed
`phase`/`layer`/`op`/`component`, Rhino kernel name, `profile_class`,
`multiplicity`, timing distribution fields, tensor metadata, PDL metadata, and
optional parent composite timing.

`blocks.csv` has one row per executable profiling unit. A block may be a single
node, PDL composite, or multi-node launch block. For latency analysis, prefer
its span timings: `mean_us`, `stddev_us`, `min_us`, `p25_us`, `p50_us`,
`p75_us`, `max_us`, and `samples`.

`kernel_invocations.csv` has one row per CUPTI kernel activity record per replay.
It includes `profile_unit_index`, mapping fields such as `node_id`,
`rhino_label`, `rhino_kernel_name`, parsed label fields, `multiplicity`, raw
`cupti_kernel_name`, `duration_us`, CUPTI timestamps, device/context/stream IDs,
graph IDs, launch dimensions, and memory/occupancy metadata.

## Multiplicity And Replay Rules

`multiplicity` is a model/search scaling factor for repeated equivalent work. It
is not the number of observed CUPTI launches.

Raw rows from `kernel_invocations.csv` are profiling sample rows. If there are
10 retained replays, summing `duration_us * multiplicity` across all rows
produces work across all 10 samples. Do not compare that total directly to a
single forward latency.

`replay_index` is reused for every profiled unit, so it is not a replay-unit
identity by itself. For composite timings, group rows by
`(profile_unit_index, replay_index)` and compute `max(end_ns) - min(start_ns)`;
that is one sample for that captured unit. Average those span samples, then
apply `multiplicity` and sum across units for a forward latency estimate.

For individual kernel work, aggregate rows within each replay unit first, then
compute means/stddev/percentiles across replay samples. For isolated rows from a
multi-node composite, include `source_node_index` in the replay-unit key because
each child node is captured separately.

```python
import polars as pl

trace = (
    pl.read_csv("target/profile/.../kernel_invocations.csv")
    .filter(pl.col("measurement") == "composite")
    .with_columns(
        (pl.col("duration_us") * pl.col("multiplicity")).alias("scaled_work_us")
    )
)

per_unit_replay = trace.group_by(["profile_unit_index", "replay_index", "phase", "op"]).agg(
    pl.col("scaled_work_us").sum().alias("scaled_work_us")
)

per_replay = per_unit_replay.group_by(["replay_index", "phase", "op"]).agg(
    pl.col("scaled_work_us").sum().alias("scaled_work_us")
)

summary = per_replay.group_by(["phase", "op"]).agg(
    [
        pl.col("scaled_work_us").mean().alias("mean_scaled_work_us"),
        pl.col("scaled_work_us").std(ddof=1).fill_null(0).alias("stddev_scaled_work_us"),
        pl.col("scaled_work_us").min().alias("min_scaled_work_us"),
        pl.col("scaled_work_us").quantile(0.25).alias("p25_scaled_work_us"),
        pl.col("scaled_work_us").quantile(0.50).alias("p50_scaled_work_us"),
        pl.col("scaled_work_us").quantile(0.75).alias("p75_scaled_work_us"),
        pl.col("scaled_work_us").max().alias("max_scaled_work_us"),
        pl.len().alias("samples"),
    ]
).sort("mean_scaled_work_us", descending=True)
```

Use the same pattern for `component`, `rhino_label`, `rhino_kernel_name`,
`cupti_kernel_name`, or `node_id` groupings.

## Profile And Search-Config Consistency

`rhino profile` and `rhino search-config` use the same profiling engine and
`ProfileReport` timing model.

- `ProfileBlock.elapsed_us` is the p50 composite replay-unit span for one
  captured profiling unit. It comes from CUPTI rows chunked per replay and timed
  as `max(end_ns) - min(start_ns)`. Rhino uses p50 as the representative
  search/profile cost because profiles with many small repeated graph launches
  and auxiliary streams can see rare host/driver or stream-scheduling artifacts
  that pull the mean far above steady-state replay cost.
- `ProfileBlock.scaled_elapsed_us` is `elapsed_us * multiplicity`.
- `rhino profile` writes those values to `blocks.csv`, and writes
  `profile_unit_index` into both `blocks.csv` and `kernel_invocations.csv`.
- `rhino search-config` consumes `ProfileReport.blocks` directly. Search costs
  should use `block.scaled_elapsed_us`, not sums of CUPTI kernel durations or
  isolated child-node timings.
- Search components must own whole profiled units/blocks. If a requested site
  set only partially overlaps a profiled composite, treat that as a search
  planning error rather than attributing a fraction of the span.

## Recommended Workflow

1. Check which CSVs exist and inspect headers before analysis.
2. Count rows by `measurement`, `device_id`, `context_id`, and `replay_index`.
   Also check that `profile_unit_index` exists before reconstructing composite
   spans from CUPTI rows.
3. For latency, summarize `blocks.csv`; for kernel breakdowns, summarize
   `kernel_invocations.csv`.
4. Filter to `measurement == "composite"` for the normal model execution path.
5. Aggregate CUPTI rows per replay unit, then compute cross-replay statistics.
6. Report units and label whether a number is wall-clock latency, raw kernel
   work, or multiplicity-scaled kernel work.

## Useful Commands

Quick grouped scan:

```bash
uv run scripts/analyze_cupti_trace.py target/profile/dsv4-decode \
  --group-by operation \
  --measurement composite \
  --top 20
```

Inspect one kernel family:

```bash
uv run scripts/analyze_cupti_trace.py target/profile/dsv4-decode \
  --group-by node-cupti \
  --measurement composite \
  --contains routed_moe \
  --top 20
```

Use the helper script for exploration. Its default CUPTI summaries are
replay-normalized, and when it is given a profile directory it also prints the
forward latency estimate from `blocks.csv`. For custom composite analysis, keep
the same rule: compute each replay-unit span as `max(end_ns) - min(start_ns)`
before averaging.

## Common Pitfalls

- Do not sum raw CUPTI kernel durations and call it end-to-end latency; kernels
  can overlap across streams and communication can overlap with compute.
- Do not add `composite` and `isolated` rows. `isolated` rows are extra
  measurements for comparison.
- Do not compare a sum over all replay samples to one forward latency.
- Do not interpret `scaled_calls` or `mean_scaled_calls` as observed launches
  without checking how replay normalization was done.
- Do not rank by raw launch count when the question is optimization benefit;
  rank by per-replay mean work or by block latency impact.
- When mappings are blank, keep the CUPTI symbol in the report and state that
  Rhino attribution is missing for those rows.
