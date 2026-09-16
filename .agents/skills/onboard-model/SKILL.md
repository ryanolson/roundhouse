---
name: onboard-model
description: WIP end-to-end workflow for integrating a new model checkpoint and reference implementation into Rhino. Use when adding a model family, porting model graphs or quantized weights, validating TP/ADP/EP behavior, building prefill/decode serving, or auditing whether an onboarding is genuinely complete rather than a partial operator or layer smoke test.
---

# Onboard a Rhino Model (WIP)

Treat this skill as a living checklist. Update [field-notes.md](references/field-notes.md) whenever the integration exposes a reusable failure mode, invariant, missing abstraction, or validation technique.

## Set the completion contract

Create a robust goal before editing. Define “done” as a runnable model family, not a compiling crate or partial graph. Unless the user narrows scope, require:

- checkpoint/config detection and complete weight loading;
- every real layer type and cross-layer state transition;
- independent real-weight oracle parity, including distributed reductions;
- `ModelParallelism`-derived ATP, ADP, MoE TP, and EP behavior, or explicit validated rejections;
- embeddings, all transformer layers, final norm, LM head, caches, prefill, decode, and sampling-facing outputs;
- executor/runtime, resource planning, synthetic/profile/search paths, CLI/distributed serving, and kernel manifests;
- CPU checks plus representative real-GPU end-to-end execution.

Do not report completion because one projection, attention half, or FFN half runs. A “single-layer test” must execute the checkpoint’s entire layer path.

## Inventory before implementation

1. Read repository instructions and run the required environment bootstrap.
2. Inspect the checkpoint config and tensor index programmatically. Enumerate layer classes from actual keys and config fields.
3. Read the authoritative reference implementation, including cache formats, shared state, quantization boundaries, routing, and residual semantics.
4. Inventory analogous Rhino models. Prefer their model-spec, graph, executor, cache, parallel, and serving abstractions.
5. Build a matrix of layer variants. Treat attention/indexer type and FFN type as independent axes.
6. Search FlashInfer, already vendored kernels, and imported model kernels before proposing new code. If a required kernel truly does not exist, stop and ask for a vendoring plan.
7. Classify reused kernels by implementation owner, not by the first model that imported them. When a second model consumes a model-owned DeepGEMM, SGLang, or TRT-LLM wrapper, move the shared operation into the corresponding provider crate and leave only genuinely model-specific composition behind.
8. Inventory normalization per submodule from checkpoint keys and reference code. Do not infer that every `*norm*` in an RMSNorm model is RMSNorm; affine weight-plus-bias sites may be true LayerNorm with different accumulation and parameter dtypes.
9. Inventory prefill, decode, and speculative/verify kernels independently for
   every stateful attention or SSM layer. Record the algorithm class used in
   each phase (for example chunked prefill versus recurrent decode), its state
   dtype/layout contract, and whether it addresses the persistent pool directly.
   Do not use an already-vendored decode kernel for prefill merely because it
   implements the same recurrence mathematically. When the reference runtime
   uses different algorithm classes by phase, preserve that phase boundary in
   the backend interfaces and configuration.

Treat the reference runtime as a versioned test artifact. Record its source
revision, the checkpoint-declared framework version, resolved dependency and
kernel versions, launch arguments, and any local runtime-library shim. Prefer
native config/model support; do not enable remote checkpoint code merely to
work around version skew. For long-lived reference environments, use a durable
cache rather than `/tmp`, build from a clean commit archive rather than a dirty
checkout, and point build-tool caches such as Cargo at writable durable paths.

## Establish independent truth

Write a small Python oracle that reads raw checkpoint tensors and does not import Rhino or reuse Rhino transforms. Make inputs and cache contents deterministic. Save intermediate tensors at meaningful boundaries so failures localize quickly.

For each layer type:

1. Generate the known-good baseline first.
2. Build the complete Rhino graph.
3. Execute it with the intended tensor-parallel topology.
4. Compare local projections, state/cache outputs, collective results, routing, FFN output, and final residual output.
5. Use exact comparisons for discrete semantics. If a top-k kernel permits arbitrary tie ordering, compare the exact selected set rather than byte order.

Use distinct logical inputs across ADP lanes and ATP-owned rows when validating
distributed permutations. Identical rows can hide A2A source swaps, incorrect
source compaction, and all-gather ordering bugs.

Generate Python CUDA references before initializing CUDA in the Rust test process. Keep GPU reservations coordinated and release them after success or failure.

## Model graph architecture

Factor the graph around semantic halves and explicit state:

- common attention weights and buffers;
- optional per-layer indexer/compressor weights;
- dense and routed-MoE FFN implementations;
- typed cross-layer state carrying both its tensor slot and producer dependency;
- a layer result describing output activation, residual state, and carried state.

Validate that shared-state layers cannot recompute missing state and full-state layers cannot accidentally consume stale state. Keep carried kernel inputs in graph-owned storage when kernels impose alignment or layout requirements.

Model fused residual/norm state deliberately. Specify which tensor is the FFN partial, which tensor owns the accumulated residual, and what the next layer consumes. Verify the final layer materializes the actual hidden state exactly once.

Audit every layout-changing view against the backend that consumes it. A
`reshape` or strided graph view does not make a kernel stride-aware: if a GEMM
wrapper hard-codes contiguous batch strides, pass the actual leading and batch
dimensions or materialize the required layout. Include multiple prompt rows
and distinct per-head weights in the oracle so one-row decode cannot hide this
class of error.

Keep symbolic graph identity pointer-free: record only semantic slot identity,
shape, dtype, view operations, offsets, strides, and alignments. Never hash or
compare resolved device addresses. Add a graph view primitive only when a
production kernel consumes that view contract; remove abandoned view APIs when
the final kernel instead accepts the physical layout and explicit strides.

Classify GEMM sites by their real request contract (output dtype, bias,
activation, and layout) and load them through the repository's backend
abstraction. Keep an exceptional strided BMM on an explicit specialized path
instead of pinning every ordinary GEMM to its concrete backend. Reject a
selected backend at load time when it cannot satisfy a site's contract.

## Consume `ModelParallelism`

Never pass one ambiguous `tp_size` through the implementation. Derive and validate:

- attention projections, dense FFNs, and shared experts from attention TP;
- routed-expert width from MoE TP;
- local expert range from EP;
- token ownership from `MoeTokenLayout` for ATP-to-EP transitions;
- shared-expert reduce-scatter, routed dispatch/combine, and output all-gather for EP;
- independent ADP lanes and cache ownership;
- runtime row-validity/source-row compaction for mixed ADP+EP when required.

Add topology unit tests before distributed GPU tests. Keep legacy TP test adapters only when they are clearly separate from serving APIs.

## Build serving in dependency order

1. Whole-model weights and weight slots.
2. Per-layer KV/index state and allocation/resource specifications.
3. Embedding and initial residual state.
4. Full layer loop with explicit shared-state lifetime.
5. Final residual merge, norm, and LM head.
6. Decode graph at serving batch capacities.
7. Prefill/chunked-prefill graph and cache writes.
8. Runtime kernel selection and resource planning.
9. Executor, model family, synthetic weights, profile/search support.
10. CLI config detection, backend config, distributed loading, and kernel manifests.

Wire `--skip-weight-load` when the model has a complete backend-formatted
synthetic-weight constructor. It should traverse the same runtime, topology,
resource-planning, graph, capture, and serving paths as real weights while
skipping checkpoint reads and load-time transforms; do not invent a second
shape or quantization contract just for the flag.

For model-defined block resources, test the executor's padding path as well as
the model metadata planner. Dummy decode/prefill rows must materialize every
physical backing pool named by the logical block, not a legacy default resource.
For a topology with row alignment greater than one, make the serving capture
capacity at least that alignment, then submit a smaller active batch to prove
padding, route ownership, and cache allocation together.
Do not require a static prefill bucket when the executor supports exact lazy
prefill construction. Very-long-context model maxima can make every static
bucket's conservative page capacity exceed a deliberately bounded KV pool;
validate the runtime token capacity against row alignment instead.
Likewise, size decode graph columns from the smaller of the architectural
context limit and the logical KV capacity that the process actually owns.
Prove the imported long-context kernel at that capacity with eager execution
and CUDA-graph replay; do not capture an unservable architectural maximum.
Inspect the complete producer-to-consumer chain at that shape: an asynchronous
launch error can surface at the next kernel's error check. Check CUDA grid-axis
limits and prefer a scheduled kernel whose launch work follows active sequence
lengths instead of allocating one block for every column in graph capacity.
When a kernel alignment rounds scratch width, use that physical width for the
bound page tables, block offsets, first input fill, and replay fills while
retaining the unrounded logical capacity for admission.

Automatic KV sizing and runtime workspace planning must share a memory budget.
Confirm whether topology-dependent collective/A2A resources are materialized
before KV allocation or explicitly reserved in its calculation; otherwise a
successful model load and KV allocation can still OOM during executor setup.

Before adding a model-specific execution-shape policy, audit the standard
executor for canonical-resource assumptions. Generic padding should operate on
an explicitly validated supported resource shape (for example exactly one
logical block resource with arbitrary declared backing pools), never fixed
resource or pool IDs and never an unchecked `first()` element. Add regressions
with noncanonical IDs and multiple backing pools. Use a custom policy for truly
hybrid fixed-state or multi-block-resource models rather than weakening the
standard policy's capability boundary.

Build checkpoint load plans for multiple layers at a time. One plan per layer
repeats metadata scans, file setup, and coordinator phases; one unbounded
model-wide plan can also be wrong when checkpoint chunk ordering leaves source
tensors for many incomplete artifact transforms resident simultaneously.
Measure both startup time and peak device memory, then choose a bounded
multi-layer plan size (or improve artifact-aware scheduling) that amortizes
setup without retaining the entire checkpoint's transform inputs.

Prefill may execute one causal graph row per query token while returning one
sample per request. Keep KV writes and every transformer layer token-complete,
but gather only the last query row of each request before LM head and sampling;
allocating full-vocabulary logits for every prompt token is not a viable serving
implementation.

Make the smallest meaningful prefill oracle at least three tokens long: execute
the first two tokens in one packed Rhino graph, compare every complete semantic
layer boundary to a raw-checkpoint causal Python oracle, then run token three
through the ordinary decode graph using the prefill-written caches. Verify final
norm from the graph's actual residual input and verify representative LM-head
rows independently. Do not use bitwise packed-versus-sequential equality as the
sole oracle: different correct GEMM/Top-K execution shapes can reorder tied
indices or round quantized intermediates differently.

Do not wire CLI detection before the underlying executor can serve; do not declare serving support while only one-token standalone layers exist.

## Validate progressively

Run formatting, strict linting, graph/unit tests, checkpoint-loader tests, each independent layer oracle, topology tests, whole-model decode, a two-token packed-prefill plus third-token decode-continuation oracle, multi-rank serving, a sampled-token smoke test, and a long semantic-generation gate. Re-run earlier layer oracles after refactors.

Prefix-cache hits are a completion gate, not an optional optimization. Submit a
second request sharing at least one complete cache block with a finished first
request, prove that admission reports nonzero cached tokens, and compare its
suffix logits or generated tokens with a precision-equivalent cold full-prefix
replay. If normal cold prefill uses higher precision than the cache, also run a
controlled cold replay through the cache representation and report the expected
drift from the higher-precision path. Hybrid models must restore every mutable
state family into private request storage at the same token frontier as the
reused block-backed cache; declining every hit or combining mismatched frontiers
leaves onboarding incomplete.

Benchmark selectable kernels only against implementations with the same
serving role and algorithm class: chunked prefill against chunked prefill,
recurrent decode against recurrent decode, and complete fused pipelines against
complete fused pipelines. Include preprocessing, state load/store, required
workspace traffic, and epilogues in timed regions. A faster result against a
serial recurrence is useful diagnostic evidence, but it is not the baseline
for choosing between chunked prefill providers. Keep prefill and decode
selection independent when no single provider is best for both.

For quantized reductions, compare score tensors with justified tolerances and
measure Top-K boundary overlap. Keep exact selection checks across replicated
ranks; if the independent scalar oracle differs only at a tightly bounded set
of nearly tied entries, continue through attention and the entire layer and
require downstream semantic outputs to match. Record the bound explicitly.

A serving smoke must pass request validation and exercise scheduling. Assert a
successful status, output shape/token count, and logs or counters showing
prefill admission followed by decode completion; endpoint health alone is not
an inference test.

Do not treat a short token-count smoke as evidence that the whole model is
correct. Use the checkpoint's actual prompt/chat template, ask for several
paragraphs of constrained factual prose, and inspect the complete decoded
text. Require coherent language, compliance with the requested structure, and
stable non-repetitive decoding for hundreds of tokens. HTTP 200, the requested
token count, finite logits, and a few plausible leading tokens can all coexist
with a full-model numerical or state-propagation bug. If this gate fails,
retain the raw response and trace the earliest full-model boundary that
diverges from an independent reference before declaring onboarding complete.
For reasoning-capable checkpoints, make the output budget long enough to pass
through answer planning and inspect whether the serving surface separates
reasoning from final text. Inline coherent reasoning with no final-answer
segment is a parser/budget gap, not by itself evidence of a bad model graph.

When onboarding the model's first MoE profile path, capture native real-weight
routes and fit the repository's routing distribution workflow. Do not make
search/profile appear complete with uniform, tied, or arbitrary dummy routing.
If the reference server is SGLang and supports routed-expert responses, use
`scripts/capture_sglang_routes.py` to convert concurrent native responses into
the repository observation schema; pass the checkpoint's total and dense layer
counts so non-routed layers are removed and routed layers are renumbered.

Do not infer reference-server readiness from allocated GPU memory. Poll its
health endpoint, and if startup is unusually long, use read-only process stacks
and I/O/compiler activity to distinguish weight transfer, post-load transforms,
and JIT compilation from a deadlock. Capture traffic only after every rank and
the serving endpoint are ready. Prebuild first-use host-side kernels outside the
server watchdog when the build manifest is available; otherwise set a watchdog
that covers compilation and send a small warmup before the long calibration.

Before handoff, list supported topology combinations and reject unsupported combinations with precise errors. Record every remaining gap in the PR rather than hiding it behind a generic “model supported” statement.

Treat the expert-compute backend and MoE A2A transport as independent choices.
For every advertised compute backend, prove the execution family, transformed
weight layout, input quantization format, routing-output contract, resource
planning, and supported TP/EP partition—not merely that the A2A backend loads.

For routing calibration, prefer a materialized-TopK mode from the same fused
runner and weight-preparation family as production. A backend advertising the
same quantization name may still expect a different checkpoint tensor layout;
prove compatibility with a full checkpoint load before launching the capture.

Run the actual profile CLI after graph unit tests. A device memcpy node may be
numerically correct yet invisible to a kernel-only CUPTI profiler. When the
model state contract permits it, make producer and next-consumer slots share a
backing allocation and write the result in place; never label a real copy as a
zero-cost no-op merely to make profiling pass. Read the CLI's runtime validator
as well as `--help` before choosing shortened smoke-test iteration counts.

## Maintain the WIP skill

After each material discovery, update the relevant topical entry in
[field-notes.md](references/field-notes.md). Add a new heading only when the
lesson does not fit an existing invariant. Keep enough of the failure mode to
make the rule concrete:

- symptom or tempting wrong turn;
- root cause or violated invariant;
- fix and validation that proved it;
- general rule for future model ports.

Do not maintain a chronological implementation diary. Merge duplicates, delete
superseded paths, and keep checkpoint-specific constants, benchmark numbers,
and current implementation details in tests, manifests, model code, or the PR.
Promote stable, broadly applicable rules from the notes into this file.
