# Model Onboarding Field Notes

Keep only reusable surprises here. The workflow belongs in `../SKILL.md`;
checkpoint constants, benchmark results, and current implementation details
belong in tests, manifests, or the PR.

## Weight loading

### Source views and placement are separate

`TensorSource::view(...)` selects which checkpoint elements a rank reads.
`TensorPlacement` controls distribution or transformation after that read.
Consequently:

```text
ShardDim { dim, rank, size } + TensorPlacement::Local
    = rank-local checkpoint shard, not replication
```

This is useful for non-leading TP axes. A tensor shaped `[1, 1, heads, 1]`
may not work with a generic lowering that first partitions axis zero. Read the
rank's head slice directly on axis two, keep that slice local, and validate the
materialized shape as `[1, 1, local_heads, 1]`. Continue using coordinated TP
placement for tensors supported by its redistribution contract. Replicate only
genuinely head-independent weights.

### Checkpoint and runtime dtypes are independent

Read the tensor in its declared checkpoint dtype, then make any kernel-required
conversion an explicit weight transform. Do not falsify source metadata to
match the consumer.

### Layer taxonomy comes from keys and config

Names such as “dense” often describe only the FFN. Enumerate the cross-product
of attention, FFN, normalization, and state-sharing variants, then test every
semantic layer class.

## State, cache, and activation lifetime

### Cross-layer state must be explicit

Carry a state tensor's slot and producer dependency together. Producer layers
create it and consumer layers require it; do not recompute it or hide it in a
mutable builder. Apply the same rule to fused residual accumulation so an add
is not accidentally performed twice.

### Persistent state and transient scratch need separate audits

- Size persistent state from admitted requests plus any disjoint graph-capture
  bank, and prove slots are reused after request completion.
- Register transient scratch once per graph and prove slot identity is reused
  across representative layers.
- Exclude prefill-only buffers from decode graphs and vice versa.

### Hybrid attention has multiple state families

Fixed recurrent/convolution state and paged KV are distinct resources. Model
every backing pool in scheduler, capture, scratch, and runtime metadata.
Prefix reuse is atomic across the families: if one cannot restore a prefix,
replay it even if another family's pages remain cached.
The restored mutable snapshot and reusable pages must end at the same token;
test a real nonzero-hit continuation against a precision-equivalent cold replay.

When block-backed pages are immutable, keep the incomplete page in private live
storage. Promote it into its allocated block only when the page becomes full,
before that block is published; an exact unaligned prefix snapshot must retain
the live page alongside recurrent state while advertising only complete blocks.
Make promotion a forward-level cache epilogue: layer-major storage lets one
metadata-driven launch update every layer and overlap independent output work.

Bounds-check every active state slot and page ID against physical storage before
launch, and use checked conversion for kernel indices.

### Captured buffer shapes are capacity contracts

Replay inputs retain captured shapes while active contents vary. Fill the whole
bound metadata buffer, use the kernel's documented padding sentinel, and test
executor-generated capture rows—not only the model metadata planner.

## Parallel execution

### Use one canonical parallel layout

Derive attention weights, dense/shared FFNs, routed width, local experts, token
ownership, collectives, and cache ownership from `ModelParallelism`. Reject a
topology early when it yields a local shape unsupported by a compiled kernel.

Expert-local weights alone do not establish EP support. Validate A2A
dispatch/combine, valid-row compaction, shared-expert collectives, row
restoration, scratch capacity, and independent ADP cache/state ownership. Use
distinct inputs for different lanes and owned rows so permutations cannot hide.
Keep temporary coverage gates distinct from architectural limitations; lift
them only with topology tests and simultaneous distinct-lane serving traffic.

Treat MoE source rows, receive-expanded kernel rows, and fused-provider rows as
separate capacity contracts. Route only the contiguous rows owned by the
attention rank. A separate all-to-all kernel sees `local_rows * ep_size` after
dispatch, while a provider that fuses dispatch/combine sees only `local_rows`;
derive serving workspaces and portable hooks from the same distinction. Derive
the A2A payload width from the routed expert input space, which may be a latent
projection narrower than model hidden. Shared experts remain at model width:
reduce-scatter their full-row output to the routed row shard, combine locally,
then all-gather rows before the next layer.

## Kernel and layout contracts

### Name a kernel by its callable role

Inspect the exported function and call site: forward/backward,
training/inference, prefill/decode, and exact operator math. A language label or
neighboring related kernel is not evidence of serving support; for example, a
TileLang file in an attention tree may expose only a training-backward kernel.

### Keep provenance mechanical

Vendor canonical upstream math with an immutable revision, license, and hash
inventory. Keep unavoidable integration changes as manifest-listed patches.
Model-only specialization belongs in a model kernel crate; reusable provider
code belongs in the provider crate.

### Kernel sets must close selectable backend dependencies

A backend compiling in its owner crate does not make a model kernel set
deployable. Trace the selected runtime path through weight transforms, input
quantization, compute, and communication, then include every invoked module in
the model set. Use the owning crate's existing build-provider label; an ad hoc
label in one manifest is not a new provider and can invalidate the entire
registry. For architecture-specific AOT, keep one architecture-neutral logical
module while emitting and declaring a distinct artifact for each supported
target. Validate the registry and build every advertised target before the GPU
session. Shared-library initialization must not resolve or load an embedded
target cubin: cross-compilation may run on a different GPU architecture. Resolve
the kernel lazily on its first runtime launch.

For an auto-resolved communication backend, include the dependencies of each
resolution that a supported topology can select. Do not include only the
dependencies of the enum default.

A reduced runtime kernel set must cover checkpoint transforms and the first
real-weight forward pass. Model construction does not resolve utilities that
the runtime starts on demand.

### Prefill and decode are different contracts

Validate each phase numerically on the target architecture, including state
updates, scheduling metadata, symbol visibility, and launch ABI. Compilation
and graph construction do not validate execution, and a decode kernel that
accepts multiple rows is not automatically a correct prefill kernel.

When multiple providers share the logical cache/state contract but require
different activation layouts or scheduling inputs, make the provider a graph
construction choice. Allocate only the selected provider's scratch and
metadata. Put the shared math behind phase-specific operator traits that expose
their physical scratch and metadata requirements; keep provider selection in a
small factory. Compare the complete composite operation—including layout
conversion—while requiring independent output and final-state parity. Enter
parity tests through the production operator trait and graph wrapper; calling
generated module exports directly validates the AOT ABI, not Rhino's layout,
metadata, preprocessing, dependency, or kernel-selection contract.

Distinguish recurrent and chunk-parallel prefill even when both implement the
same recurrence. Benchmark the deployable composites over log-spaced sequence
lengths; a raw chunk kernel can be a useful ceiling but cannot establish the
production crossover when stability, packing, or metadata kernels are omitted.

### Compare the physical recurrent-state layout

An independent oracle can produce correct continuation outputs while recording
an incompatible recurrent-state layout if its model wrapper treats the state as
opaque. Honor flags such as `state_v_first` or `transpose_state_layout` when
serializing the oracle, or explicitly transpose before physical cache parity.
Equal key and value dimensions hide this error from shape checks, so use
non-symmetric state values and compare the cache storage itself after prefill
and decode.

### Encode specialized planning contracts in the backend type

Do not type-alias a specialized backend to a generic implementation when their
planning invariants differ: the alias exposes generic constructors that can
silently bypass the specialized plan. Use a narrow wrapper with one constructor
and enforce shape-dependent invariants during `plan`. Test a capacity that
requires padding; production maxima may satisfy the constraint accidentally.

A narrow wrapper must also forward every nontrivial provider-trait method.
Default implementations such as an empty graph-resource set or unsupported
portable authoring hook can compile while silently discarding the wrapped
backend's planned metadata. Attach graph-instance resources to the function
definition after its nodes are authored; the global serving-resource plan
cannot supply metadata keyed to one node's stable graph position. Validate
eager launch and CUDA-graph capture, since construction alone does not resolve
resource handles.

### Views require consumer stride support

A slice or `as_strided` changes graph metadata, not kernel behavior. Trace
physical pitches through every wrapper and generated ABI. Keep a projection
split as a view only while every consumer accepts its strides; otherwise
materialize once at the first compact-only boundary. Delete wrappers and view
primitives abandoned by the final kernel path. Exercise at least two rows:
singleton dimensions can make an invalid pitched reshape appear contiguous.

Packed graph storage also makes pointer alignment an explicit kernel ABI.
Declare the required alignment on the exact tensor view passed to the provider
kernel, and let lowering propagate the view's byte offset and alignment back
to its arena allocation. Validate this with real multi-rank CUDA-graph capture;
independent allocations may otherwise hide a missing alignment contract.

## Validation

### Test complete semantic paths

A layer test includes configured attention/indexer, cache updates, routed and
shared experts, collectives, residual handling, and carried state. Independent
oracles should read raw weights, preserve TP-local boundaries, and compare
quantized graphs at justified stable boundaries. Treat Top-K ties according to
the operator's documented set semantics.

The minimum useful causal test is two packed prefill tokens followed by a third
token through ordinary decode using the written cache/state.

### Distinguish compatibility fixtures from correctness oracles

An older checkpoint re-expressed through a new architecture with identity or
near-identity parameters is valuable for config parsing, load-plan coverage,
variant materialization, and feature bisection. It does not establish numerical
correctness for the new model when it retains the old geometry, disables bounded
operations, substitutes saturation limits, or avoids native quantized and
distributed paths. Name and document that artifact as a compatibility fixture.
Use a checkpoint-derived fixture that preserves real weights and shapes for
independent layer parity, and state any dequantization or topology gaps. Require
the native checkpoint and serving topology for the remaining end-to-end gates.

### Serving correctness includes text quality

A real request must show admission, prefill, decode, and completion. Generate
enough text to inspect structure, coherence, and repetition; endpoint health,
HTTP 200, or a short plausible prefix does not prove the whole model.
