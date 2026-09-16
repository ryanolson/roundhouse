<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

> **Status: evidence base.** Produced 2026-08-19 against kubernetes-sigs/gateway-api-inference-extension @ 84436a9 (fetched 2026-08-19; v1.6.0 two days prior), with the
> roundhouse tree read for comparison. The ruling that synthesizes this into
> direction is `../synergies/ecosystem-round-2.md`, which this document exists to justify.
> An independent fact-checker re-derived the highest-stakes claims from the
> pinned trees; its verdicts and any corrections are appended. Per
> `agent-docs/README.md`, this snapshot gains dated bracketed notes when the
> world moves - never silent rewrites.
>
> *[2026-08-21: re-read for the M9 watching brief. **GAIE `main` has not moved at all** — `git diff --stat 84436a9 HEAD` is empty and `gh api .../commits` agrees; 84436a9 is still the tip three days on. Every `gaie:<path>:<line>` citation below therefore still resolves exactly. All movement is in `llm-d/llm-d-router`, pinned for this pass at `e051872097bae66089936ae6c24af2bec7f92be6` (2026-08-21 14:36 -0700), cited below as `lldr@e051872:<path>:<line>`. See the dated section at the end.]*

़# Kubernetes Gateway API Inference Extension — evidence document

**Scope of this dive.** `/workspace/nvidia/gateway-api-inference-extension` @ `84436a9` (`chore(deps): bump github.com/envoyproxy/go-control-plane/envoy (#3019)`, Tue Aug 18 2026), read-only; its own `docs/proposals/` and `site-src/`; git history of the same tree for material removed two months before the pin; and published GitHub/site docs where the tree points at them. Compared against `/home/user/roundhouse` (M0–M6 shipped, review-fix diff uncommitted) and the pinned Dynamo checkout at `/root/.cargo/git/checkouts/dynamo-66ea943fd73cd568/ac7b751` (= `ai-dynamo/dynamo` rev `ac7b7513790ef1d619b46f805aea03c9f21200ba`, the rev `/home/user/roundhouse/Cargo.toml:37-45` pins).

**Citation convention.** `gaie:<path>:<line>` = the pinned tree. `gaie@a70292c^:<path>:<line>` = the same repo one commit before the EPP was deleted (June 2026) — material that is *no longer in this repo* and now lives at `llm-d/llm-d-router`. `rh:<path>:<line>` = `/home/user/roundhouse/<path>`. `dyn:<path>:<line>` = the Dynamo checkout above.

---

## 0. What it is — one page

The Gateway API Inference Extension (GAIE / "IGW") turns any Gateway API gateway that speaks Envoy **ext-proc** into an "inference gateway": a proxy that, per HTTP request, asks an out-of-process **Endpoint Picker (EPP)** which model-server *pod* should serve this request, then routes there. The unit of decision is one HTTP request; the unit of configuration is an **InferencePool** (a label selector over Pods + a reference to an EPP service). `gaie:README.md:26-46`, `gaie:site-src/concepts/api-overview.md:1-24`.

**The single most important fact about this pin, and it changes the whole question:**

> As of `v1.6.0` (released 17 Aug, two days before this pin), **the scheduler is not in this repository any more.** The full EPP, the scheduler plugin framework, all shipped scorers/filters/pickers, the flow-control module, the latency predictor, Body-Based Routing, and the `InferenceObjective` / `InferenceModelRewrite` / `EndpointPickerConfig` APIs were removed and moved to `llm-d/llm-d-router` and `llm-d/llm-d-inference-payload-processor`. `gaie:README.md:16-23`.
>
> The removal commits are in the tree: `a70292c` "Cleanup EPP and Latency Predictor (#2967)", Tue Jun 16 2026 — **481 files changed, 221 insertions, 90,903 deletions**; and `88fd479` "Remove inference objective, model rewrite and endpoint picker config APIs (#2973)", Wed Jun 17 2026 — 68 files, 49 insertions, 5,001 deletions.
>
> What remains: the **InferencePool** API (`api/v1`, GA), the **InferencePoolImport** API (`apix/v1alpha1`, multi-cluster, Draft), the **Endpoint Picker Protocol spec**, the **conformance suite**, and **LWEPP** — a deliberately dumb round-robin reference picker whose stated purpose is passing conformance tests. `gaie:pkg/lwepp/README.md:1-11`, `gaie:site-src/faq.md:12-22`.

So this repo is now **an API + a wire protocol + a conformance suite**, roughly 900 lines of non-test Go in `pkg/lwepp/{handlers,datastore,metadata}` plus 550 lines of CRD types. The interesting scheduling content is *history* here and *elsewhere* now. That reframes the brief's question: "would roundhouse implement or sit behind the EPP" is a question about a **protocol**, not about a competing scheduler in this tree.

**NVIDIA involvement in this tree: none visible.** The strings `NVIDIA`, `Dynamo`, `NIM`, `NeMo` appear **zero times** across the entire repository (verified `grep -rniE '\bnvidia\b|\bnim\b|\bnemo\b|\bdynamo\b'`, excluding `.git`). The only NVIDIA-adjacent presence anywhere is a metrics-name column for **Triton TensorRT-LLM** and **trtllm-serve** in the model-server protocol table (`gaie:docs/proposals/003-model-server-protocol/README.md:26-32`). The project's stated model-server partnership is with **vLLM via llm-d** (`gaie:README.md:79-85`). Community meetings have moved to the llm-d Router community meeting (`gaie:README.md:113-117`).

---

## 1. WHAT IT IS — resource model, EPP architecture, plugin framework, flow control, conformance

### 1.1 The resource model, as it stands in *this* tree

Two CRDs. That is all.

**`InferencePool`** — `inference.networking.k8s.io/v1`, storage version, GA. `gaie:api/v1/inferencepool_types.go:24-101`:

| Field | Meaning | Line |
|---|---|---|
| `spec.selector` | `matchLabels` only; **Pods in the same namespace only; cross-namespace explicitly unsupported** | `:64-70` |
| `spec.targetPorts[]` | 1..8 ports; **every port is a distinct endpoint, addressed `podIP:portNumber`** | `:72-81` |
| `spec.appProtocol` | `http` (default) or `kubernetes.io/h2c` — added for gRPC model servers | `:83-94` |
| `spec.endpointPickerRef` | **optional since v1.5.0**; Group/Kind (default `Service`)/Name/Port | `:96-100`, `:126-177` |
| `spec.endpointPickerRef.failureMode` | `FailOpen` \| `FailClose`, **defaults to `FailClose`** | `:165-176` |
| `status.parents[]` | Up to 32 parents (Gateways), each with its own conditions and `controllerName` | `:181-233` |

Conditions: `Accepted` (reasons `Accepted`, `NotSupportedByParent`, `HTTPRouteNotAccepted`, `EndpointPickerRefMissing`), `ResolvedRefs`, `Exported`. `gaie:api/v1/inferencepool_types.go:246-355`. The `EndpointPickerRefMissing` reason exists precisely because the field went from required to optional and most implementations still require it (`:311-320`).

Ownership is explicitly **shared** — multiple controllers may reconcile the same InferencePool; implementations MUST NOT claim it with `ownerReferences` or labels. `gaie:site-src/guides/implementers.md:175-190`.

**`InferencePoolImport`** — `inference.networking.x-k8s.io/v1alpha1`. Status-only, controller-managed, no spec at all. Represents an InferencePool exported from another cluster. `gaie:apix/v1alpha1/inferencepoolimport_types.go:26-52`, `:54-107`. The design (Draft) follows Multi-Cluster Services: export by annotating the InferencePool, hub/spoke or push/pull distribution, and two routing modes — **Endpoint Mode** (route directly to remote pods, requires pod-network connectivity across clusters) or **Parent Mode** (route to the remote cluster's Gateway). `gaie:docs/proposals/1374-multi-cluster-inference/README.md:1-80`.

**What is gone:** `InferenceModel` → renamed `InferenceObjective` (per-request `criticality`/`priority`) → **removed from this repo** at `88fd479`. `InferenceModelRewrite` (model-name rewrite) → removed. `EndpointPickerConfig` (the YAML that configured plugins, scheduling profiles, saturation detector, flow control, data layer, parser) → removed. The design record survives at `gaie:docs/proposals/1199-inferencemodel-api-evolution/README.md` and `gaie:docs/proposals/1816-inferenceomodelrewrite/README.md`, but the types are not in the tree.

### 1.2 EPP architecture (as documented; implementation now external)

The layered design: **ext-proc server (non-extensible)** → **Routing layer** → **Flow Controller** → **Scheduling layer**, with a **Data layer** as a vertical accessed by all. `gaie:docs/proposals/0683-epp-architecture-proposal/README.md:22-30`. The ext-proc server is explicitly declared non-extensible: "deviation could cause the EPP to become unusable or unstable. Extension is ill-advised." (`:70-74`).

The Data layer (`gaie:docs/proposals/1023-data-layer-architecture/README.md`, status *Accepted*) is a plugin registry of **DataSource** (a thing that polls/watches) and **DataCollection** (a thing that extracts attributes onto an `Endpoint`), with per-endpoint `map[string]any` storage. Two built-in sources: a Pod reconciler and a `/metrics` scraper, one goroutine per endpoint (`:74-88`).

### 1.3 The scheduler plugin framework

Documented at `gaie:docs/proposals/0845-scheduler-architecture-proposal/README.md` (*Implemented*), with a compilable interface sketch at `gaie:docs/proposals/0845-scheduler-architecture-proposal/interfaces/interface.go`. Shape:

- A **Scheduler** holds one **ProfileHandler** and N named **SchedulerProfiles**.
- `ProfileHandler.Pick` selects which profiles run this cycle (iteratively, may depend on prior results); `ProcessResults` aggregates and names the *primary* profile whose endpoints are used. `interface.go:96-118`.
- Each profile runs `Filter*` → `Score*` → exactly one `Picker`. Scorers SHOULD return `[0,1]`; weighting is profile-level config. `README.md:64-79`, `interface.go:120-155`.
- State lives in three places: per-request `CycleState`, per-plugin struct state (e.g. the prefix index), and data-layer endpoint attributes. `README.md:25-32`.

Design principles worth noting: the framework "should act as an independent library… agnostic to endpoint types… and K8s concepts. Opinions should be held by the plugins, not the framework" (`README.md:14-18`). Multi-profile exists to support P/D disaggregation (a `prefill` profile and a decode profile) and shadow/production A-B. `gaie@a70292c^:pkg/epp/framework/plugins/requestcontrol/dataproducer/approximateprefix/types.go:78-86`.

### 1.4 Flow control and criticality

The Flow Controller sits **between routing and scheduling**: it decides *if and when* a request proceeds, the scheduler decides *where*. `gaie@a70292c^:pkg/epp/flowcontrol/README.md:24-36`.

Vocabulary (all from `gaie@a70292c^`):

- **`FlowKey{ID string, Priority int}`** is the primary key — a composite of a logical group (tenant/model) and a numeric priority band. Different priority ⇒ *a different flow instance with its own queue*. Higher int = higher priority. `pkg/epp/framework/interface/flowcontrol/flow.go:21-63`.
- **Sheddable = `priority < 0`**, project-wide. `pkg/epp/framework/plugins/flowcontrol/eviction/filtering/sheddable.go:27-29`, `:56-58`.
- Two-tier policy: **Fairness** (between flows in a band) — `roundrobin`, `globalstrict`; and **Ordering** (within a flow) — `fcfs`, `edf`, `slodeadline` (`Deadline = ReceivedTimestamp + x-slo-ttft-ms`). `pkg/epp/framework/plugins/flowcontrol/ordering/slodeadline/README.md:17-31`.
- **SaturationDetector** returns a *gradient*, not a boolean. The `utilization` detector uses a roofline model: `PodScore = max(WaitingQueue/QueueThreshold, KVCacheUsage/KVCacheThreshold)`, `PoolSaturation = mean(PodScore)`; stale metrics (>200 ms default) score 1.0. Defaults: queue 5, kv 0.8, staleness 200 ms, headroom 0. `pkg/epp/framework/plugins/flowcontrol/saturationdetector/utilization/README.md:8-58`, `.../detector.go:104-133`.
- The same detector doubles as a scheduling **Filter**, removing pods above `threshold*(1+headroom)` — and **fails open**: if every candidate is filtered, the original list is returned. `.../README.md:26-34`.

Behaviour with flow control **off** (which was the default): sheddable requests get HTTP 503 under saturation; everything with priority ≥ 0 passes straight through FCFS, with queuing happening on the model servers. `gaie@a70292c^:site-src/concepts/priority-and-capacity.md:28-33`.

### 1.5 Conformance status, and who implements it

Conformance is versioned by GAIE release + profile, with a documented deprecation window of current + two previous minor releases. `gaie:site-src/concepts/conformance.md:5-27`.

Currently listed conformant gateways: **Istio**, **Agentgateway**, **NGINX Gateway Fabric**. `gaie:site-src/implementations/gateways.md:3-9`. (Istio's own support is still tracked by an open issue, `:23-25`.)

Submitted reports in-tree, by release (`gaie:conformance/reports/`): v0.4.0 istio; v0.5.0 gke-gateway, istio; v0.5.1 ack-gateway, agentgateway, envoy-ai-gateway, kgateway, kubvernor; v1.0.1/v1.0.2 istio, ack-gateway, kgateway, nginx-gateway-fabric; v1.1.0 nginx; **v1.4.0 agentgateway `inference-v1.0.0`, nginx `inference-v2.5.0`**; **v1.5.0 istio `1.30.1`, nginx `inference-v2.6.0`**. Note the attrition — GKE Gateway, kgateway, envoy-ai-gateway, ack-gateway and kubvernor have no report at v1.4.0+ and a PR pruned stale listings (`495ccc3 docs: prune stale gateway implementations (#2976)`).

The 14 conformance tests (`gaie:conformance/tests/`) assert: pool `Accepted`/`ResolvedRefs` conditions, invalid/missing EPP refs, appProtocol, HTTPRoute port validation, multi-rule/multi-gateway/weighted-two-pool routing, **that the gateway honors the EPP's chosen endpoint** (`gateway_following_epp_routing.go`, plus a data-parallel variant), **that the gateway reports back which endpoint served** (`gateway_destination_endpoint_served.go`), and **fail-open when the EPP is down** (`epp_unavailable_fail_open.go:40-42`).

---

## 2. THE EPP PROTOCOL, precisely

Spec: `gaie:docs/proposals/004-endpoint-picker-protocol/README.md`, status ***Implemented***, version **v1.0.0** (2025-07-29). Reference implementation in this tree: `gaie:pkg/lwepp/handlers/`.

### 2.1 Transport

- The EPP **MUST** implement the Envoy [external processing service](https://www.envoyproxy.io/docs/envoy/latest/api-v3/extensions/filters/http/ext_proc/v3/ext_proc.proto) — a **gRPC bidirectional stream per HTTP request**. `004:14-18`.
- The EPP **MUST support streaming mode** (full duplex). `004:20`.
- The EPP MUST expose gRPC health checks (`liveness`, `readiness`, `envoy.service.ext_proc.v3.ExternalProcessor`). **`readiness` returns `SERVING` only if the datastore has synced AND this replica is the elected leader; followers return `NOT_SERVING`.** `004:100-113`. *(This is a hard architectural fact: an InferencePool is served by exactly one active EPP.)*

### 2.2 Data plane → picker (what the picker sees)

1. **Endpoint subset** (optional), as ext-proc *filter metadata*: namespace `envoy.lb.subset_hint`, key `x-gateway-destination-endpoint-subset`, value a list (or comma string) of `ip:port`. If set, the EPP **MUST** pick only from it; if none is eligible, it MUST return `ImmediateResponse` **503**. If unset, the EPP picks from the InferencePool selector's set. `004:26-38`; implemented `gaie:pkg/lwepp/handlers/request.go:44-79`, `:105-133` (note the comment at `:53-60` explaining it accepts both `[]any` and a flat comma-joined string, because `HeaderToMetadata` filters produce the latter).
2. **All request headers**, as the ext-proc `RequestHeaders` message. Stored verbatim. `gaie:pkg/lwepp/handlers/request.go:38-42`.
3. **The full request body**, accumulated across `RequestBody` chunks until `EndOfStream`. LWEPP caps this at **10 MB** and returns `ResourceExhausted` above it. `gaie:pkg/lwepp/handlers/server.go:103`, `:192-198`.
4. **Response headers**, including `envoy.lb` → `x-gateway-destination-endpoint-served` — which endpoint actually served, after data-plane retries. `004:88-98`; `gaie:pkg/lwepp/handlers/response.go:29-46`.
5. **Model-server metrics**, *not* through this protocol — separately scraped by the EPP from each pod's Prometheus endpoint (see §3.1).

**Timing.** The decision point is: if request headers arrive with `EndOfStream`, pick immediately; otherwise **defer the headers response and pick only when the body's `EndOfStream` arrives**. `gaie:pkg/lwepp/handlers/server.go:139-190`, `:200-206`, `headersDeferred` at `:113`/`:186`. This is the load-bearing latency fact of §6.

*[2026-08-21: unchanged, and **the same is true of the real EPP**, which is what the brief asked. llm-d-router's streaming server accumulates every `RequestBody` chunk into a `bytes.Buffer` and does all of parsing, scheduling and header generation inside `if v.RequestBody.EndOfStream` (`lldr@e051872:pkg/epp/handlers/server.go:463-513`). Envoy's `FULL_DUPLEX_STREAMED` mode is mandatory there (`lldr@e051872:README.md:47-50`) but it is a *transport* mode: the decision point is still body end-of-stream. No partial-body or headers-only routing path exists. One difference from LWEPP: **the ext-proc body path has no size cap.** The 10 MB `ResourceExhausted` guard is LWEPP-only; a repo-wide search of llm-d-router finds exactly one `max_request_body_size` and it belongs to the *disaggregation coordinator's* plain HTTP server (default 64 MB, `lldr@e051872:pkg/coordinator/config/config.go:40`, enforced by `io.LimitReader` at `pkg/coordinator/server/handlers.go:48-53`) — a different process on a different path. `errcommon.ResourceExhausted` exists in llm-d-router but is the no-endpoint/capacity code (`lldr@e051872:pkg/epp/scheduling/scheduler_profile.go:134`), not a body-size guard. The EPP's buffer is bounded only by Envoy's own limits and, once flow control queues the request, by the per-band `maxBytes` (default 1G, `lldr@e051872:docs/operations.md:22-32`).]*

### 2.3 Picker → data plane (what it returns)

The EPP **MUST** communicate the chosen endpoint **both** ways, with **identical values**:

1. HTTP header `x-gateway-destination-endpoint: <ip:port>` (or a comma-separated ordered list), and
2. ext-proc `dynamic_metadata` under namespace `envoy.lb`, key `x-gateway-destination-endpoint`.

"The EPP MUST not set two different values in the header and the inner response metadata value… this protocol does not define what takes precedence." `004:42-79`. Implemented at `gaie:pkg/lwepp/handlers/server.go:145-183` (with `ClearRouteCache: true` at `:152`).

Multiple endpoints = ordered fallback list for retries. Retry stays within one InferencePool backend; a route-level retry that picks a *different* pool MUST re-invoke that pool's EPP and MUST NOT reuse the prior list. `004:64-70`.

Error returns, both as ext-proc `ImmediateResponse`: **503** (no ready endpoints), **429** (drop the request — "a Sheddable request, and the servers under heavy load"). `004:72-76`.

### 2.4 What the picker returns *beyond* an endpoint

`PickResult` in the reference implementation declares `Fallbacks []string`, `MutatedBody []byte` ("If non-nil, replaces the request body forwarded to Envoy"), and `ExtraHeaders map[string]string` — `gaie:pkg/lwepp/handlers/server.go:71-77`. **None of the three is consumed anywhere in `pkg/`** (verified by grep: the only hits are the declarations). Body mutation and header injection are structurally available in ext-proc but not wired in the reference picker. Body rewriting is what the separated **BBR** component does — and BBR now lives in `llm-d/llm-d-inference-payload-processor` (`gaie:README.md:19`).

Header vocabulary the protocol reserves (`gaie:pkg/lwepp/metadata/consts.go:20-38`):

| Header/key | Purpose |
|---|---|
| `x-gateway-destination-endpoint` | chosen endpoint (EPP→DP) |
| `x-gateway-destination-endpoint-subset` | candidate constraint (DP→EPP) |
| `x-gateway-destination-endpoint-served` | who actually served (DP→EPP) |
| `x-gateway-inference-fairness-id` | flow-control fairness identity |
| `x-gateway-inference-objective` | which InferenceObjective applies |
| `x-gateway-model-name-rewrite` | rewrite the model name sent upstream |
| `x-slo-ttft-ms`, `x-slo-tpot-ms` | per-request latency SLOs (`gaie@a70292c^:site-src/guides/latency-based-predictor.md:43-44`) |

Note that three of these name features **removed from this repo**; they survive as reserved constants in the LWEPP metadata package.

*[2026-08-21: **five of these seven names were renamed in llm-d-router and the two repos now disagree.** `lldr@e051872:pkg/epp/metadata/consts.go:37-56` carries `x-llm-d-inference-fairness-id`, `x-llm-d-inference-objective`, `x-llm-d-model-name-rewrite`, `x-llm-d-slo-ttft-ms`, `x-llm-d-slo-tpot-ms` as the current spellings, with each old `x-gateway-*`/`x-slo-*` name demoted to a deprecated alias in a `headerAliases` map (`:71-77`) read through `GetLowerCaseHeaderValue` (`:88-101`). The rename landed at `81d7f46054dc83dbd202a0b53e8f264e522dd840` (2026-05-20, llm-d-router PR #1161), whose message states the rule exactly: "Use x-llm-d-prefixed names for llm-d-managed EPP headers while preserving old names as aliases. Keep GAIE endpoint picker protocol headers on their x-gateway-destination-* names." So the split is principled — the three `x-gateway-destination-*` protocol headers keep their names in both repos; everything the EPP itself owns moved to `x-llm-d-`. GAIE's own LWEPP constants were never updated (they still read `x-gateway-inference-fairness-id` at `gaie:pkg/lwepp/metadata/consts.go:33-38`), and the historical tree this table was built from also predates the rename (`gaie@a70292c^:pkg/epp/metadata/consts.go` has the old names only) — the table was right about the tree it read and wrong about the live vocabulary. llm-d-router also adds `x-gateway-destination-endpoint-scores` (`:31-34`, emitted only under `--emit-endpoint-scores`), `x-llm-d-request-dropped-reason` (`lldr@e051872:pkg/common/error/error.go:30`), and three video-shape headers (`:57-62`). Consequence for S-A and open question 3 below.]*

### 2.5 Latency envelope

- **Design goal:** "The scheduler should be reasonably fast — decide request mapping to endpoints within **O(10 ms)** on average." `gaie:docs/proposals/006-scheduler/README.md:33`. Explicitly scoped to the scheduler subsystem, "not the EPP as a whole" (`:75-77`).
- **Measured:** with `inference-perf` against Qwen3-32B on vLLM, a staged ramp **1→5000 QPS**, shared-prefix workload, streaming completions: "**p90 scheduler latency remained within 100 ms** across all stages." `gaie@a70292c^:site-src/guides/epp-configuration/resource-tuning.md:22-34`.
- **Resource envelope for that:** EPP container requests **4 CPU cores / 8 GiB**, memory limit 16 GiB, no CPU limit. `ibid.:7-16`.
- **Not measured anywhere in-tree:** the ext-proc round-trip itself, or the body-buffering delay. LWEPP does no scheduling work at all (round-robin, `gaie:pkg/lwepp/handlers/server.go:79-100`), so it cannot serve as a latency reference.

*[2026-08-21: **this gap is now filled upstream, at exactly roundhouse's workload shape, and the numbers are worse than this section guessed.** `lldr@e051872:docs/operations.md` is an EPP container-sizing guide built from benchmarks described as "agentic" and run at 100k input / 1k output tokens. Its rule of thumb: allocate **0.5 to 1.0 CPU cores per request/second** for large agentic workloads (`:14`); measured peaks at 50 req/s of 100k/1k are **17.5-20.3 cores and 3.7-5.2 GiB** (`:82-83`); the co-located Envoy alone wants 8 cores at 100 req/s of the same shape (`:102`); idle EPP CPU reaches ~7.5 cores at 100 model-server pods purely from metric scraping (`:17`). Meanwhile the *scheduler* P50 stays at 0.1-0.2 ms (`:67-70`) — i.e. upstream's own data confirms the decision is nearly free and the cost is buffering, parsing and prefix-hashing the body. That is R2's thesis, now measured by the people who ship the thing.]*

### 2.6 One tell about how thin LWEPP is

`PickRequest` carries a `Model string` field (`gaie:pkg/lwepp/handlers/server.go:65-70`) — and `pickEndpoint` never sets it (`gaie:pkg/lwepp/handlers/request.go:144-147`; grep for `Model:` in `pkg/lwepp/` returns nothing outside tests). The reference picker in the repo that owns the protocol never sees the model name. This is consistent with its stated purpose ("intended only as a reference for testing and conformance, and is not recommended for production", `gaie:site-src/guides/implementers.md:110-112`) but it means **this tree contains no example of a picker that actually schedules**.

---

## 3. SCHEDULING CONTENT — three schedulers, mechanism by mechanism

### 3.1 What the shipped GAIE plugins optimized (all `gaie@a70292c^`)

Inputs come from the **Model Server Protocol** (`gaie:docs/proposals/003-model-server-protocol/README.md`, status *Partially implemented*): scraped Prometheus gauges, with a per-server mapping table for vLLM / Triton TRT-LLM / trtllm-serve / SGLang.

| Metric | vLLM name |
|---|---|
| `TotalQueuedRequests` | `vllm:num_requests_waiting` |
| `TotalRunningRequests` | `vllm:num_requests_running` |
| `KVCacheUtilization` | `vllm:kv_cache_usage_perc` |
| `BlockSize`, `NumGPUBlocks` (optional) | `vllm:cache_config_info{block_size,num_gpu_blocks}` |
| LoRA state | `vllm:lora_requests_info{max_lora, running_lora_adapters, waiting_lora_adapters}` |

`003:26-32`, `:36-52`. The doc concedes "the current algorithm in the reference EPP is highly biased towards vLLM's current dynamic LoRA implementation" (`:36-38`).

Shipped scorers (default profile = all four, unweighted: `gaie@a70292c^:test/integration/epp/testdata/default-config.yaml:1-16`):

| Plugin | Formula | Cite |
|---|---|---|
| `queue-scorer` | min-max normalize `WaitingQueueSize` across candidates; `1.0` if all equal | `.../scorer/queuedepth/queue.go:78-104` |
| `kv-cache-utilization-scorer` | `1 - KVCacheUsagePercent` | `.../scorer/kvcacheutilization/kvcache_utilization.go:77-83` |
| `lora-affinity-scorer` | adapter active → 1.0; capacity to load → 0.8; adapter waiting → 0.6; at `MaxActiveModels` → 0.0 | `.../scorer/loraaffinity/lora_affinity.go:73-98` |
| `prefix-cache-scorer` | `MatchBlocks / TotalBlocks` | `.../scorer/prefix/plugin.go:97-119` |
| `running-requests`, `token-load`, `latency` | present, not default | `.../scorer/{runningrequests,tokenload,latency}/` |

Pickers: `max-score` (shuffle for tie-break, sort desc, take top `maxNumOfEndpoints`, default 1 — "susceptible to **hot-spotting** if many concurrent requests produce identical scores", `.../picker/maxscore/README.md:9-20`), `random`, `weightedrandom`.

Filters: `prefixcacheaffinity`, `sloheadroomtier` (probabilistic tier split with 1% ε-exploration of the SLO-violating tier, `.../filter/sloheadroomtier/README.md:11-22`), plus the utilization detector acting as a filter.

**The prefix mechanism, precisely.** Design (`gaie:docs/proposals/0602-prefix-cache-aware-routing-proposal/README.md`, *Implemented*): an **approximate** index on the EPP, built by mimicking the model server's LRU. Chunk the prompt, `hash(chunk_i) = hash(content_i + hash(chunk_{i-1}))`, record every chunk hash → server on dispatch, look up longest match on the next request (`0602:74-93`). Explicitly *rejects* both session affinity and consistent hashing as alternatives (`0602:39-63`).

Implementation details that matter for §3.4:

- The hash is over **bytes of the JSON-marshalled request, not tokens**. `getUserInputBytes` marshals `Messages`/`Items`/`{instructions,tools,input}` to JSON; block size in characters is `blockSizeTokens × 4` where `averageCharactersPerToken = 4`. `.../approximateprefix/hashing.go:35-51`, `:105-131`; `.../types.go:110-114`.
- The first block's hash is salted with `TargetModel` and the request's `cache_salt` — so prefix matching is per-model and per-tenant-salt. `hashing.go:70-76`.
- Defaults: `blockSizeTokens = 16` (vLLM's), `maxPrefixBlocks = 256`, **`lruCapacityPerServer = 31250`** — with the sizing arithmetic spelled out for Llama-3-8B on an H100-80GB. `.../types.go:88-108`.
- A token-ID path exists but only via the vLLM gRPC parser (`TokenizedPrompt`, `.../parsers/vllmgrpc/vllmgrpc.go:201-212`), which is what the gRPC proposal exists to enable (`gaie:docs/proposals/2162-grpc-support/README.md:9-13`).
- Known limitation, stated by the authors: "cache hit performance decreases with multiple active EPP replicas" and the index must be rebuilt on restart. `0602:79-84`, `:120-126`.

**Request cost.** `SimpleTokenEstimator`: `inputTokens = bytes/4` (or `chars/4`), `outputTokens = inputTokens × 1.5`, cost = sum. `.../dataproducer/inflightload/token_estimator.go:38-71`.

### 3.2 Dynamo's KV-router, mechanism (authoritative, from the pinned rev)

Selection is **argmin of a cost logit** over eligible workers (`dyn:lib/kv-router/src/scheduling/selector/default.rs:216-322`):

```
overlap_credit_blocks =
      overlap_score_credit · decay · device_overlap_blocks
    + host_cache_hit_weight · host_overlap_blocks
    + disk_cache_hit_weight · disk_overlap_blocks
    + shared_cache_multiplier · shared_beyond_device_blocks

decay = 1 / (1 + overlap_score_credit_decay · normalized_excess_prefill_load)

logit = prefill_load_scale · max(0, raw_prefill_blocks − overlap_credit_blocks)
      + decode_cost_blocks
      + decode_active_request_weight · active_requests
```

Defaults (`dyn:lib/kv-router/src/scheduling/config.rs:42-66`, `:800-810`): `overlap_score_credit = 1.0`, `overlap_score_credit_decay = 0.0`, `prefill_load_scale = 1.0`, `host_cache_hit_weight = 0.75`, `disk_cache_hit_weight = 0.25`, `decode_active_request_weight = 0.0`, **`router_temperature = 0.0`** (deterministic argmin; `T>0` switches to softmax sampling over the logits, `default.rs:433-460`).

Salient differences from GAIE's approximate index:
- Overlap is **measured**, not approximated: it comes from a radix index fed by **KV events published by the workers themselves** (`router_assume_kv_reuse`, `use_kv_events = true`, ZMQ ingest per DP rank — `rh:crates/roundhouse-fleet/src/local.rs:250-252`, `rh:README.md:107-113`).
- It is **tiered**: device / host / disk / shared-beyond-device, each with its own credit weight. GAIE's index has one tier (HBM), by explicit non-goal: "Coordinate cache beyond accelerator HBM cache, such as remote caches" (`0602:16-18`).
- Load is **booked**, not scraped: `potential_prefill_tokens` accrues at reservation time and is released explicitly.
- The router exposes a **query-only `select`** that prices without booking, and a separate `reserve` that books — the split roundhouse depends on (`rh:crates/roundhouse-fleet/src/local.rs:6-14`, `:318-370`, `:372-405`).

### 3.3 Roundhouse's cache-adjusted expected-prefill routing

One axis, filled in two ways (`rh:crates/roundhouse-core/src/routing/mod.rs:4-22`, `rh:README.md:68-79`):

- **Local:** `Candidate.expected_prefill_tokens = SelectRequest → effective_prefill_tokens` — the Dynamo scheduler's own cache-credit-weighted number, taken verbatim. `rh:crates/roundhouse-fleet/src/local.rs:130-135`, `:161-172`, `:355-362`.
- **Frontier:** modelled, `isl − p_hit(elapsed) · last_prefix_tokens`, with `CacheModel::Deterministic{ttl_ms}` (Anthropic-shaped) or `InactivityDecay{half_life, max_ttl, min_prefix_tokens}` (OpenAI-shaped). `rh:crates/roundhouse-core/src/routing/ledger.rs:30-80`.
- **Load:** `Candidate.load = potential_prefill_tokens` booked on the worker — absolute tokens, same unit as prefill, `None` for frontier. `rh:crates/roundhouse-fleet/src/local.rs:301-315`, `rh:crates/roundhouse-core/src/routing/mod.rs:136-147`.
- **Choice:** `AffinityPolicy` min-max normalizes prefill/cost/ttft across the admitted pool and minimizes `1.0·prefill + 0.5·cost + 0.25·ttft`. `rh:crates/roundhouse-core/src/routing/policy.rs:64-72`, `:83-92`, `:160-186`.
- **Admission** is four axes in one function: allow-filter + quality floor (`TurnPolicy::permits`), frontier cadence, spend grant, then `max_load`; blame is ordered so a policy-emptied set is `PolicyRefused` and a budget/load-emptied set is `NoViableCandidate`; and the **overflow valve** re-admits the policy-admitted pool budget-aside when nothing local can take the turn. `rh:crates/roundhouse-core/src/routing/mod.rs:348-404`, `:196-198`.
- **Query cost:** block+sequence hashes, **never token ids** — "for a 100k-token context, the difference between shipping a 400 KB array and a few kilobytes". `rh:crates/roundhouse-fleet/src/local.rs:37-42`, `:84-119`; `rh:README.md:86-90`.
- **Not measured:** `expected_ttft_ms` for local candidates is a static config constant `local_base_ttft_ms` (`rh:crates/roundhouse-server/src/engine.rs:1058-1064`), and `quality_prior` is "configuration, not measurement" (`rh:crates/roundhouse-core/src/metrics/pricing.rs:54-60`).

### 3.4 Side-by-side

| Signal | GAIE EPP (historical) | Dynamo KV-router | Roundhouse |
|---|---|---|---|
| Cache locality | approximate LRU index on the picker, **char-hashed JSON**, HBM only, per-EPP-replica | measured radix index fed by worker KV events; device/host/disk/shared tiers | consumes Dynamo's `effective_prefill_tokens` locally; **models** provider TTL for frontier |
| Cache unit | 16 "tokens" ≈ 64 chars | real block hashes on real tokens | Dynamo's block/sequence hashes, incrementally computed per turn |
| Load | scraped gauges (queue depth, kv %), 200 ms staleness ⇒ score 1.0 | booked `potential_prefill_tokens` + decode blocks + active requests | Dynamo's booked number, passed through as `Candidate.load` |
| Request cost | `bytes/4 × 2.5` heuristic | prefill blocks from actual ISL and measured overlap | `effective_prefill_tokens` (local) / ledger model (frontier) |
| LoRA | first-class scorer (0/0.6/0.8/1.0) | `lora_name` is a routing input on `PromptRequest` | **not modelled** — `lora_name: None` at `rh:crates/roundhouse-fleet/src/local.rs:110` |
| Combination | weighted sum of normalized `[0,1]` scorers, `max-score` picker with shuffle | single additive cost logit, argmin (or softmax at `T>0`) | min-max normalized weighted sum, argmin |
| SLO | `x-slo-ttft-ms`/`x-slo-tpot-ms` → headroom tiers + latency scorer + EDF deadline ordering | not in the router | **absent** — see §5 |
| Dollars | absent entirely | absent | first-class: `expected_cost_usd`, rate cards, spend ledger |
| Admission | flow control: priority bands, fairness, TTL, displacement | `queue_admission` | budget grant/settle, degrade-to-local, overflow valve |
| Decision unit | one HTTP request | one request | **one turn of one session** |

*[2026-08-21: the **Load** row understates the GAIE/llm-d side and should be read as describing the `utilization` saturation detector only. Both trees also carry an `inflight-load-producer` that **books** load at dispatch and releases it on the response — `PreRequest` increments, `StartOfStream` releases the prefill portion, `EndOfStream` releases the rest (`lldr@e051872:pkg/epp/framework/plugins/requestcontrol/dataproducer/inflightload/README.md:13-17`; the same three hooks are already present at `gaie@a70292c^:.../inflightload/producer.go:50,101,122-152`, so this was true at the pin and the dive cited only `token_estimator.go` from that directory) — and a `concurrency-detector` saturation detector that computes `PoolSaturation = Aggregate Inflight Load / Aggregate Pool Capacity` from those booked counters with no scrape lag at all (`lldr@e051872:pkg/epp/framework/plugins/flowcontrol/saturationdetector/concurrency/README.md:11-27`; present at `gaie@a70292c^` too). Two qualifications keep the row's conclusion intact: the default profile still runs only the four scraped scorers (`lldr@e051872:test/integration/epp/testdata/default-config.yaml:4-15`, byte-identical in substance to the historical one), so booking is opt-in; and what is booked is an *estimate* (`inputTokens = bytes/4`, output `×1.5`), not Dynamo's measured reservation. New since the pin: the producer now discounts the cached prefix, adding only the **uncached** portion of the prompt to the in-flight counter via `PrefixCacheMatchInfo` (`.../inflightload/README.md:13`) — which is `effective_prefill_tokens` arrived at independently, from an approximate index instead of KV events.]*

### 3.5 Three schedulers stacked: which decision belongs where, and where they fight

The honest layering, by what each layer *uniquely* knows:

- **Gateway (Envoy/Istio/agentgateway/NGF):** TLS, HTTPRoute matching, host/path/weight splitting, retries, cross-pool failover. Knows nothing about tokens. **Belongs here: ingress.**
- **EPP:** which *pod* of a homogeneous pool. Knows pool membership, scraped pod metrics, and (approximately) prefix residency. **Belongs here: pod choice — but only when nothing better is available.**
- **Roundhouse:** which *provider* (local fleet vs. which frontier model), under whose budget, with what history. Uniquely knows: the session's committed log, the tenant's policy and spend, the frontier cache ledger, dollars. **Belongs here: the local-vs-frontier decision, tenancy, budget, steering.**
- **Dynamo:** which *worker and DP rank*. Uniquely knows: real KV residency across tiers, booked load, reservation lifecycle. **Belongs here: worker choice.**

Five places two owners hold the same signal:

1. **Prefix/cache locality — EPP vs Dynamo.** In a Dynamo deployment, the "pod" an EPP picks *is a Dynamo frontend that will route again*. The EPP's approximate index would be modelling residency on a fleet whose placement is decided by a scheduler it cannot observe, from character hashes of a body it re-parses, against a cache whose eviction it guesses. It is strictly worse information than Dynamo already has and it would be **fighting**: EPP steers request A to frontend 1 for prefix affinity; frontend 1's router places it on whatever worker is actually warm, invalidating the EPP's model of frontend-1's cache. **Verdict: exactly one prefix-aware layer, and it should be the one reading KV events.**
2. **Load — EPP scrape vs Dynamo booking.** The utilization detector's own README documents the failure: "a sudden burst of traffic can create a severe *thundering herd*… before the next metric interval reveals it is completely saturated" (`.../utilization/README.md:44-50`). Dynamo's booking model does not have that failure because load accrues at reservation, not at observation. Two load models over the same GPUs, one of which is knowingly lagged, is a fight with a predictable loser.
3. **Admission/shedding — EPP flow control vs roundhouse budget vs Dynamo queue admission.** These are *different* questions (fairness among tenants within a pool; dollars across providers; queue capacity on a worker) and can coexist — but only if each is the sole authority for its axis. The trap: EPP flow-control returns 429 for a sheddable request that roundhouse has already granted budget for and opened a reservation against. Roundhouse's `Reservation::drop` logs loudly precisely because a leaked reservation "permanently inflates the router's view of this worker's load" (`rh:crates/roundhouse-fleet/src/local.rs:198-210`).
4. **Retry/fallback — the sharpest one.** The protocol lets the EPP return an ordered endpoint list and the data plane walks it on retry (`004:64-70`). If roundhouse sat *behind* a gateway that did this, a data-plane retry to a fallback endpoint would silently move the request off the worker roundhouse reserved — leaving a booked reservation on a worker that never ran the turn and an unaccounted turn on one that did. Nothing in the protocol tells the upstream that a retry happened; `x-gateway-destination-endpoint-served` flows to the *EPP*, not to the backend.
5. **Model identity.** `x-gateway-model-name-rewrite` / BBR rewriting the model name in the body vs roundhouse choosing the target and stamping `DecisionRecord`. Two rewriters, and roundhouse's spend attribution is keyed on its own choice.

---

## 4. SESSION/STATE STORY — strictly per-request

**Finding: nothing in the extension, at this pin or before it, understands a conversation, a session, or a stateful API.** The evidence is uniformly negative and worth stating precisely, because it is the fact that decides the topology.

*[2026-08-21: **still true of GAIE, no longer true of llm-d-router, and the counter-example names our exact clients.** `lldr@e051872:pkg/epp/framework/plugins/requestcontrol/requestheader/agentidentity/agent_identity.go:42-62` defines an `agent-identity` plugin whose built-in priority list is `x-claude-code-session-id` (Claude Code), `x-session-affinity` (OpenCode), `session-id` (Codex ≥ 0.131.0) and `session_id` (Codex 0.130.x legacy); the Director copies the resolved value into `FairnessID` when no explicit fairness header is set (`lldr@e051872:pkg/epp/requestcontrol/director.go:290-292`), "so every turn of an agent session lands in the same flow-control fairness queue" (`.../agentidentity/README.md:6`). It landed at `f3ae7503` (2026-07-16, PR #1754) — a month after the June split, which is why reading GAIE at either revision could not have found it. Its README even documents the Claude-Code-via-LiteLLM and Codex client setups, and records the same `previous_response_id` trap roundhouse's prefix admission solves: keying on it "references the prior turn's response, not the chain root, so keying on it would shard one conversation across many queues" (`.../agentidentity/README.md`, Limitations). **The topology conclusion below is unaffected** — this is a fairness-queue label, not a session log, a lease, or a prefix-admission mechanism, and items 5 and 6 of this section (per-request ext-proc stream; an InferencePool cannot name a frontier model) are what actually decide the seam. But the premise "nothing understands a session" has moved, and the sentence should no longer be leaned on as the deciding fact. Note also the fact-checker's correction below: the "exactly four times" count in item 1 was already wrong (eight), and is left as written per the no-silent-rewrite rule.]*

1. **In the pinned tree**, the string "session" occurs exactly four times outside a Slack-channel sentence: three of them inside the prefix-cache proposal *arguing against* session affinity, and one in a unit-test `Cookie: session=abc` header fixture. `gaie:docs/proposals/0602-.../README.md:39,41,51`; `gaie:pkg/lwepp/handlers/request_test.go:261`.
2. **The proposal explicitly considered and rejected session affinity** as a design option — cons: "Limited use case; Does not exploit prefix cache between different clients; Using client IP isn't always reliable". `0602:39-52`. The chosen approach is prefix affinity precisely *because* it "doesn't require any client integration" (`0602:86-88`).
3. **The historical EPP does parse `/v1/responses` and `/v1/conversations`** — `ResponsesRequest{Input, Instructions, Tools, CacheSalt}` and `ConversationsRequest{Items, Metadata, CacheSalt}` (`gaie@a70292c^:pkg/epp/framework/interface/requesthandling/types.go:326-353`) — **but there is no `previous_response_id`, no `store`, no conversation id, and no state anywhere.** Verified by grep across the whole historical package. Those types exist for exactly one purpose: `getUserInputBytes` marshals them so the prefix hasher has bytes to chunk (`.../approximateprefix/hashing.go:105-131`).
4. **The one acknowledgement of the problem is an open question, in a Draft proposal:** "OpenAI API continues to evolve and most recently they added the 'responses api' which has some stateful logic… The design will be extended also to cover the OpenAI Responses API. For example the `PluginsChain` might be extended to provide common utilities to either help with state caching or letting plugins handle that completely." `gaie:docs/proposals/1964-pluggable-bbr-framework/README.md:138`. That is BBR (now in a third repo), status *Draft*, phrased as a future maybe.
5. **The protocol is structurally per-request**: one ext-proc stream per HTTP request, `RequestContext` created fresh at the top of `Process` and discarded at stream end (`gaie:pkg/lwepp/handlers/server.go:57-63`, `:110`). Anything session-shaped would have to live in plugin-struct state keyed by something the picker invents.
6. **An InferencePool cannot name a frontier model.** `spec.selector` selects **Pods, in the same namespace, by labels only** (`gaie:api/v1/inferencepool_types.go:64-70`), and endpoints are `podIP:portNumber` (`:72-78`). There is no ExternalName escape hatch — the EPP ref explicitly forbids `ExternalName` Services (`:140-146`). So the "co-optimize local and frontier" half of the product is **unrepresentable** in this resource model.

*[2026-08-21: still exactly true of `InferencePool`, but Gateway API proper is now growing a resource that *can* name an external destination: **GEP-4894 "Backend Resource"** (status Experimental, sponsored by Agentgateway, Istio and Airlock, incubated by the new `kubernetes-sigs/wg-ai-gateway` working group) proposes a namespace-scoped `Backend` that either decorates a Service via `EndpointSelector` or "represents external destinations via `ExternalHostname`, replacing the need for insecure synthetic `ExternalName` Services" (`kubernetes-sigs/gateway-api:geps/gep-4894/index.md`, TLDR). That is the first Gateway-API-native way to write down "api.anthropic.com" as a routable backend, and the GEP explicitly lists `InferencePool` alongside `Service` and `Backend` as resource types wanting consistent configuration (`:443`). It changes nothing today — Experimental, no CRD in `kubernetes-sigs/gateway-api:apis/` (verified: only `v1`, `v1alpha2`, `v1alpha3`, `v1beta1`), and naming a frontier endpoint is not the same as pricing, budgeting or authenticating a turn against it — but it removes "the resource model cannot express it" as a permanent structural argument, which is how R8 uses it.]*

**Consequence for the seam.** Roundhouse sits **in front**, not behind:

- Behind (roundhouse as a picked endpoint) is incoherent: the gateway would pick a roundhouse pod per request, but roundhouse pins a session to a **fenced single-writer lease** (`rh:README.md:60-64`) and admits resent history by prefix comparison against its own log (`rh:crates/roundhouse-server/src/responses_api.rs:344-370`). A per-request endpoint picker would route turn *n+1* of a session to a pod that does not hold the lease. The extension has no vocabulary to express "keep this conversation on that writer".
- Behind is also *pointless*: the EPP's value is prefix-aware pod choice, and roundhouse's entire premise is that the client stops re-uploading the prefix (`rh:README.md:9-18`). A picker downstream of roundhouse sees only the delta.
- In front is coherent: roundhouse owns the turn; **an InferencePool becomes at most one local target**, and even then only as a discovery/health mechanism, since the actual worker choice is Dynamo's.

---

## 5. SYNERGY SEAMS — both directions

### 5.1 Them → us (what GAIE has that roundhouse could take)

**S-A. The SLO header vocabulary — the cheapest real win, and it is k8s-independent.**
`x-slo-ttft-ms` / `x-slo-tpot-ms` are per-request client-supplied latency objectives (`gaie@a70292c^:site-src/guides/latency-based-predictor.md:43-44`), consumed by a headroom-tier filter, a latency scorer, and an EDF-deadline ordering policy. Roundhouse's own routing module names this exact gap in its own words: "`RoutingContext` carries no demand-side signal — no stakes, no verifiability… The cheapest honest next step is a **client-supplied per-turn quality floor**" (`rh:crates/roundhouse-core/src/routing/policy.rs:47-54`). Adopting an established header spelling instead of inventing one costs nothing and buys interop with any gateway already setting them. Same for `x-gateway-inference-fairness-id` as a tenancy identity on the wire.

*[2026-08-21: **the spellings recommended here are the deprecated ones.** Per the §2.4 note, llm-d-router's current names are `x-llm-d-slo-ttft-ms`, `x-llm-d-slo-tpot-ms`, `x-llm-d-inference-fairness-id`; the four names this item lists survive only as aliases. Nothing has shipped in roundhouse yet (a grep for `x-slo`, `fairness-id` and `x-gateway-inference` over every `.rs` in the tree returns nothing), so the correction is free — but the recommendation must become *read the alias set, emit the current name*, mirroring `HeaderNames` at `lldr@e051872:pkg/epp/metadata/consts.go:79-86`, not "adopt `x-slo-ttft-ms`". Second, cheaper adoption target discovered in the same pass: the agent session headers at `lldr@e051872:.../agentidentity/agent_identity.go:44-50`. Upstream's claim that Codex emits one was **re-derived against our own codex revs rather than taken from their README**: `codex-rs/codex-api/src/requests/headers.rs:5-14` builds `session-id` and `thread-id`, and `codex-rs/codex-api/src/endpoint/responses.rs:87-91` attaches them (plus `x-client-request-id` = the thread id) to every `/v1/responses` stream request — **byte-identical at `e363b08` (the codex 0.146.0 binary on this box) and at `6344a65` (our Cargo pin)**, so the hyphenated form is what M9's real binary sends. Note upstream's plugin knows only `session-id`; `thread-id` is a second, coarser identity nobody downstream is reading yet. This is a session identity roundhouse can read at its own edge with no client change at all — the same transparent-hook-up property M9 exists to prove.]*

**S-B. The Model Server Protocol (003) as roundhouse's metrics contract for *non-Dynamo* local fleets.**
Roundhouse today has exactly one local backend shape: an embedded Dynamo `SelectionService` with a hand-fed worker catalog (`WorkerRegistration`, `rh:crates/roundhouse-fleet/src/local.rs:232-297`). If a deployment has a plain vLLM/SGLang/TRT-LLM pool with no Dynamo, roundhouse has nothing. `003`'s four gauges plus the per-server name mapping is a ready-made, multi-vendor contract for producing `Candidate{expected_prefill_tokens?, load, expected_ttft_ms}` from a bare pool. Note this would be a *second, worse* candidate source, not a replacement — it gives queue depth and kv%, not block-level overlap.

**S-C. `InferencePool` as worker *discovery*, not as a scheduler.**
This is the most concrete k8s item. Roundhouse's worker catalog is hand-fed: model name, routing group, endpoint, block size, and a per-DP-rank ZMQ map (`rh:crates/roundhouse-fleet/src/local.rs:232-249`). An InferencePool is exactly a label selector over pods + a port list, and the extension's own `activePortsAnnotation` (`inference.networking.k8s.io/active-ports`) plus the `pod-rank-idx` endpoint naming (`gaie:pkg/lwepp/datastore/datastore.go:56-66`, `:327-335`) is a solved answer to "which DP ranks on this pod are live". Adopting the *selector semantics* (or watching the CRD) gives roundhouse pod discovery with no scheduler entanglement.

*[2026-08-21: upstream has since made this seam explicitly pluggable and shipped a non-Kubernetes implementation of it: an `EndpointDiscovery`/`DiscoveryNotifier` plugin interface where, when a discovery plugin is configured, "the EPP **does not** require an `InferencePool` CRD or any K8s RBAC", plus a `file-discovery` plugin reading a live-reloaded `endpoints.yaml` for "bare metal inference clusters, Slurm jobs, or local development" (`lldr@e051872:docs/discovery.md:29-45`). Two consequences: the InferencePool CRD is no longer even the EPP's own only discovery source, which weakens the case for roundhouse depending on it; and the interface shape (upsert/delete notifications behind a plugin, file-backed first, CRD-backed second) is a better model for roundhouse's hand-fed `WorkerRegistration` catalog than the CRD watch itself.]*

**S-D. Flow-control vocabulary vs M3.** These are complementary, not duplicated, and the synthesis should say so explicitly:

| GAIE | Roundhouse |
|---|---|
| `FlowKey{ID, Priority int}`, higher = more important, `<0` = sheddable | `Principal` + `TurnPolicy{min_quality, allow, frontier_cadence}` (`rh:crates/roundhouse-core/src/control/policy.rs:497-507`) |
| Saturation as a `[0,1+]` gradient over scraped pod metrics | `Candidate.load` in booked prefill tokens; `AffinityPolicy::with_max_load` ceiling (`rh:.../routing/policy.rs:133-137`) |
| Backpressure: hold the highest-priority request, shed negatives (429/503) | `Exhaustion::DegradeToLocal{overflow_when_local_saturated}` \| `Refuse` (`rh:crates/roundhouse-core/src/control/budget.rs:116-140`) |
| Fairness *within* a pool of identical GPUs | Fairness *across* providers, denominated in dollars |
| Ordering: FCFS / EDF / SLO-deadline | none — turns are dispatched as they arrive |

GAIE has queueing/ordering roundhouse lacks entirely. Roundhouse has money and cross-provider degradation GAIE lacks entirely. **The overflow valve and `DegradeToLocal` have no GAIE counterpart at all**, because GAIE has no "elsewhere" to degrade to.

**S-E. Per-objective priorities vs `TurnPolicy`.** `InferenceObjective.spec.priority *int` was deliberately an int rather than an enum "so an int/enum is more flexible & carries inherent stack rank value" (`gaie:docs/proposals/1199-.../README.md:100-115`); Phase 2 planned `PerformanceObjectives` (SLO CRD) and an `InferenceUsageMeter` for fairness/usage tracking (`:120-160`). **Phase 2 never shipped and the Phase 1 CRD has been deleted from this repo.** Roundhouse's `TurnPolicy` (quality floor + target allow-filter + frontier cadence, fingerprinted onto every `DecisionRecord`) is a strictly richer per-principal policy than what GAIE ever shipped — but GAIE's *priority band* concept (a stack-ranked int that orders queued work) is a real thing roundhouse has no word for.

**S-F. The conformance discipline.** Not code, but the model — versioned reports per release + a deprecation window (`gaie:site-src/concepts/conformance.md:5-45`) — is the shape roundhouse's own wire-shape oracle suite already takes.

### 5.2 Us → them

**S-G. Roundhouse as an EPP implementation: technically possible, strategically wrong.**
Buildable: implement `envoy.service.ext_proc.v3.ExternalProcessor`, read the subset hint, return `x-gateway-destination-endpoint` + `envoy.lb` metadata, expose the three gRPC health services. Roundhouse would be a *very* good picker because it can ask Dynamo for real overlap instead of guessing. But:
- The return value is an `ip:port`. **There is no way to say "I sent this to Anthropic."** Half the product is inexpressible at the seam.
- The contract is per-request and leader-elected (`004:100-113`); roundhouse's session lease, log, and prefix admission have no home in it.
- It would put a **full-body buffer and re-parse of a 100k-token context on the gateway hot path of every turn** — the exact cost roundhouse exists to remove (`rh:README.md:9-18`).
- The *narrow* version — an EPP that consults Dynamo's selection service to pick a pod — duplicates the router that pod already runs.

**S-H. What roundhouse actually has that the ecosystem lacks — and where to send it.**
The select/reserve split. GAIE's scheduler design says the chosen endpoint "is **assumed** to be running that request until the EPP observes the termination… The scheduler must integrate the impact of assumed load with informer state, especially when traffic spikes" (`gaie:docs/proposals/006-scheduler/README.md:145-149`) — an explicit acknowledgement of the thundering-herd hole its own saturation detector then documents as unfixable in a reactive design. Roundhouse's mandatory `select → reserve → prefill_complete → release` lifecycle, with a loud drop-guard (`rh:crates/roundhouse-fleet/src/local.rs:174-210`), is the closed-loop version of that. **But the contribution target for this is `llm-d/llm-d-router`, not `kubernetes-sigs/gateway-api-inference-extension`** — this repo has no scheduler to contribute to.

*[2026-08-21: **the target has largely closed this itself, and the contribution as framed is no longer available.** The `inflight-load-producer` books at `PreRequest` and releases at `StartOfStream`/`EndOfStream` — a select/reserve/prefill-complete/release lifecycle in different words — the `concurrency-detector` derives saturation from those booked counters rather than a scrape, and the producer now discounts the cached prefix so only uncached tokens are booked (all cited in the §3.4 note above). Upstream has also gone past this dive in adjacent places: a `CrossReplicaSyncer` for EPP state (`81aa2528`, 2026-08-03), an underflow guard for counters when an endpoint flaps (`caa32b77`), and a typed capacity-rejection path surfaced to the client as `x-llm-d-request-dropped-reason` (`645ba71e`, PR #2462, landed the day of this re-read; header at `lldr@e051872:pkg/common/error/error.go:30`). What remains genuinely ours is narrower and should be stated as such if it is ever offered: booking a *measured* overlap-adjusted number obtained from the serving engine's own KV events, rather than a `bytes/4 × 1.5` estimate discounted by an approximate character-hash index. That is a smaller, sharper claim than "the closed-loop version of assumed load", and it is a Dynamo-shaped contribution more than a roundhouse-shaped one.]*

**S-I. Nothing else is contributable here.** The remaining surface of this repo is a CRD, a protocol doc, and conformance tests. Roundhouse implements none of them and does not need them.

### 5.3 What deploying roundhouse on k8s *with* this extension concretely looks like

Sketching it honestly produces a diagram in which the extension is doing very little:

```
Codex/Claude Code
   │  (Responses API / Chat Completions)
   ▼
Gateway (Istio | agentgateway | NGF)  ── HTTPRoute, TLS, host match
   │  backendRef: Service/roundhouse        ◀── a PLAIN SERVICE, not an InferencePool
   ▼
roundhouse pods  (N ≈ 3–10; each embeds a Dynamo SelectionService replica)
   │   • fenced single-writer lease per session (Redis)
   │   • prefix admission of resent history
   │   • TurnPolicy / budget / spend ledger
   │   • cache-adjusted routing: local vs frontier
   ├────────────────────────────────► frontier providers (public endpoints)
   ▼
Dynamo workers  (Deployment; KV events over ZMQ → the embedded selectors)
   ▲
   └── InferencePool CR used ONLY as the pod-discovery source for the worker catalog
```

Load-bearing constraints on that picture:

- **Roundhouse must not be an InferencePool backend.** InferencePool endpoints are interchangeable per-request; roundhouse endpoints are not (session ↔ lease). Roundhouse would be a normal `Service`, and if multi-replica session affinity is needed it must come from Gateway API session persistence (a separate feature, **not part of this extension**) — the extension's own proposal rejected session affinity outright (`0602:39-52`).
- **The Dynamo workers could be an InferencePool with `endpointPickerRef` unset** — legal since v1.5.0 (`gaie:api/v1/inferencepool_types.go:96-100`, reason `EndpointPickerRefMissing` at `:311-320`) — used as a discovery object that roundhouse watches, with no gateway routing to it at all. This is the one clean use.
- **Roundhouse currently has no k8s footprint whatsoever**: no Dockerfile, no chart, no manifests anywhere in `/home/user/roundhouse` (verified), and zero occurrences of `kubernetes|k8s|helm|inferencepool|ext-proc|envoy|gateway api` in any `.rs`/`.md`/`.toml` outside `target/`. The milestone ladder M7 (frontier credentials), M8 (admin plane), M9 (real Codex E2E) adds none (`rh:PLAN-agentic-control-plane.md:1016-1035`).

---

## 6. RISKS

**R1 — This repo's centre of gravity moved, and could move again.** In eight weeks the project deleted 90k lines, removed three alpha APIs, made a required field optional, and handed its scheduler and its community meeting to another org (`a70292c`, `88fd479`, `80e21c3 Make endpointPickerRef optional (#2898)`, `gaie:README.md:113-117`). The FAQ says the *remaining* pieces are also migrating — APIs and conformance into `kubernetes-sigs/gateway-api` proper (`gaie:site-src/faq.md:7-12`). **Betting on this repo's shape is betting on a target that has moved twice this year and has announced a third move.** The `v1` InferencePool CRD is the stable part; everything around it is not.

*[2026-08-21: **the third move is announced and not executed, and the repo is now visibly quiet.** GAIE `main` took zero commits in the three days after the pin, and the three before it were dependabot bumps. The FAQ still describes the plan in the future tense — APIs and conformance to `kubernetes-sigs/gateway-api`, EPP to llm-d-router, BBR and the latency predictor to standalone llm-d repos (`gaie:site-src/faq.md:8-12`) — while the README says this repo "will remain the primary location for the development and maintenance of **conformance tests**" (`gaie:README.md:23`). Those two sentences contradict each other and neither has an execution date; treat the destination of the conformance suite as **unresolved**, not as settled either way. On the receiving side, `kubernetes-sigs/gateway-api:apis/` still has no inference group and no InferencePool types; `InferencePool` appears there only as prose inside GEP-4894 and GEP-1897. The energy has instead gone to a new venue, the `kubernetes-sigs/wg-ai-gateway` working group (created 2025-09-26), which is where GEP-4894 was incubated. Net: R1 is corroborated, and the watching brief should now watch three addresses, not one.]*

**R2 — ext-proc in the hot path is architecturally hostile to the product.** The picker decides at body `EndOfStream` (`gaie:pkg/lwepp/handlers/server.go:200-206`), which means: (a) the *entire* prompt is buffered in the gateway before routing, (b) LWEPP caps that at 10 MB and hard-fails above it (`:103`, `:192-198`), (c) the body is re-parsed and re-hashed per request. For agentic coding — 100k+ token contexts, hundreds of turns, the *whole* conversation on every turn — this reintroduces exactly the cost roundhouse exists to remove, and at a layer roundhouse does not control. The measured envelope (p90 scheduler latency ≤ 100 ms at 4 CPU / 8 GiB, `gaie@a70292c^:.../resource-tuning.md:22-34`) is for the *scheduler only* and was measured on a 60+12-token shared-prefix workload — nothing like agentic context sizes. No in-tree number covers the ext-proc round trip or the buffering delay.

*[2026-08-21: **R2 stands, and is now the best-evidenced risk in this document.** (a) The decision point is unchanged in the *real* EPP: llm-d-router buffers to `EndOfStream` before parsing or scheduling (§2.2 note). (b) `FULL_DUPLEX_STREAMED`, which llm-d-router requires, is a transport mode and does not move that point. (c) The 10 MB cap turns out to be LWEPP-only; llm-d-router's handler caps nothing, so the failure mode for a huge turn is memory, not a clean 413. (d) The missing agentic number now exists and is upstream's own: 0.5-1.0 CPU cores per request/second at 100k input tokens, ~20 cores and ~5 GiB at 50 req/s, with scheduler P50 at 0.1-0.2 ms (§2.5 note). The last figure is the important one — it says the ext-proc layer's cost for agentic contexts is *entirely* buffer-and-parse, which is precisely the cost roundhouse exists to remove. Nothing here softens the risk; the only correction is that the sentence "no in-tree number covers the buffering delay" is now out of date in the tree that matters.]*

**R3 — Single active EPP per pool.** Readiness returns `NOT_SERVING` for non-leaders (`004:106-108`). Any throughput or availability argument must account for one live picker per InferencePool. (Roundhouse's own lease has the same shape at session granularity, which is a *narrower* unit and therefore a better one.)

*[2026-08-21: **partially superseded.** The protocol text is unchanged (`gaie:docs/proposals/004-endpoint-picker-protocol/README.md:100-113` still says leader-only `SERVING`), but llm-d-router now documents an **Active-Active** mode with near-linear throughput scaling (1.0x/2.0x/2.7x/3.5x for 1-4 replicas, `lldr@e051872:docs/operations.md:37-49`) alongside Active-Passive, and since PR #1884 (2026-07-08) non-leader replicas keep their datastore populated so a request reaching a standby is routed instead of 503'd (`haPopulateNonLeaderDatastore`, on by default, `lldr@e051872:RELEASE-NOTES.md`). The catch is the one this document would predict: flow-control state and prefix state are **per replica and not shared**, so fairness and per-band capacity are enforced only within each replica's share, and "Active-Active mode should be avoided when using approximate prefix routing" because state partitioning "significantly degrades prefix cache hit rates" (`lldr@e051872:docs/operations.md:51-56`). So the single-active constraint has become a choice between one picker and a sharded cache model — R5's problem restated as a scaling knob, which strengthens rather than weakens the comparison drawn there.]*

**R4 — Double-scheduling pathologies.** §3.5 lists five. The two that would bite first: the retry-invalidates-reservation hole (§3.5-4), and two disagreeing load models over one set of GPUs where one is knowingly 200 ms stale and treats staleness as full saturation (`.../utilization/README.md:44-50`, `.../detector.go:120-124`).

**R5 — Prefix-index fidelity.** GAIE's index hashes **characters of marshalled JSON** at 4 chars/token (`hashing.go:35-51`, `types.go:110-114`), keeps an LRU of 31,250 entries per server, and its authors state accuracy degrades with EPP sharding and is lost on restart (`0602:79-84`). Roundhouse's hashes are Dynamo's own, on real tokens, incrementally computed, and verified against a full recompute (`rh:README.md:64-70`). Mixing the two would mean a worse model of the cache overriding a better one.

**R6 — `FailClose` is the default.** An EPP outage drops all traffic to the pool unless the operator sets `FailOpen` (`gaie:api/v1/inferencepool_types.go:165-176`). Conformance has a fail-open test, but the field default is fail-closed.

**R7 — Alpha/Draft surfaces.** `InferencePoolImport` is `v1alpha1` and its proposal is *Draft* (`gaie:docs/proposals/1374-.../README.md:5`). The BBR pluggable framework is *Draft* and its proposed interfaces are flagged as likely to change (`gaie:docs/proposals/1964-.../README.md:5`, `:141-143`). The InferenceObjective "Phase 2" (usage meters, fairness CRD, SLO CRD) is *WIP* and now homeless.

**R8 — Premature for the product as stated.** The product is "agentic coding agents transparently hook up to roundhouse and route across Dynamo-local and frontier targets." This extension optimizes **self-hosted pools on Kubernetes** and its resource model cannot name a frontier endpoint at all (§4.6). Nothing in M7–M9 needs it. Adopting it now would mean building a k8s controller, a chart, and an ext-proc surface for a product that has never been deployed on k8s — before M9 has proven the Codex path at all.

**R9 — A dedup question with no target in this repo.** The directive is to dedupe effort. There is **no scheduling code in this tree to dedupe against.** The genuine overlap — prefix-aware, load-aware endpoint selection — is with `llm-d/llm-d-router` (and, more sharply, with Dynamo's own router, which roundhouse already embeds rather than reimplements). Answering "dedupe against GAIE" by reading GAIE at this pin answers the wrong question.

---

## 7. OPEN QUESTIONS THE SYNTHESIS MUST DECIDE

1. **Is Kubernetes in scope before M9 at all?** Evidence: roundhouse has zero k8s footprint; the milestone ladder adds none; the product's stated topology (agent → roundhouse → {Dynamo, frontier}) has no k8s dependency. If the answer is "not yet", most of this document becomes a watching brief and items S-A/S-B (header vocabulary, model-server metrics contract) are the only live work — and both are k8s-independent.

2. **Given the repo moved, is the dedup question mis-targeted?** The scheduler roundhouse might duplicate lives at `llm-d/llm-d-router` and, more directly, inside Dynamo — which roundhouse *already consumes rather than reimplements* (`rh:crates/roundhouse-fleet/src/local.rs`, the entire crate). If "dedupe" means "stop building a second scheduler", roundhouse already complies. The synthesis should decide whether to re-scope this dive's question to `llm-d-router` (a different org, unpinned here) or to declare it answered.

3. **Do we adopt the SLO/fairness header vocabulary now?** `x-slo-ttft-ms`, `x-slo-tpot-ms`, `x-gateway-inference-fairness-id`, `x-gateway-inference-objective`. This closes the demand-side gap roundhouse's own source names as its next honest step (`rh:crates/roundhouse-core/src/routing/policy.rs:47-54`), costs one header parser plus one `RoutingContext` field, and buys wire-compat with every conformant inference gateway. **Cheapest high-value item in the whole dive.** Decide independently of everything else here.

*[2026-08-21: the question survives; the four header names in it do not. Read the §2.4 and S-A notes before implementing: emit `x-llm-d-slo-ttft-ms` / `x-llm-d-slo-tpot-ms` / `x-llm-d-inference-fairness-id`, accept the `x-slo-*` / `x-gateway-inference-*` forms as aliases, and leave the three `x-gateway-destination-*` protocol headers alone since both repos agree on those. Because nothing has been written in roundhouse yet, this costs a different string constant and not a migration — which is the whole return on re-reading before the code lands.]*

4. **If k8s: does roundhouse watch `InferencePool` for worker discovery?** This is the one place the CRD earns its keep — replacing the hand-fed `WorkerRegistration` catalog with a label selector plus the `active-ports` annotation. It requires a controller-runtime-shaped watcher (Rust: `kube-rs`) and a decision about whether `endpointPickerRef` is left unset (legal since v1.5.0) or whether roundhouse writes status as a parent. Cost: a new dependency and a new failure mode. Benefit: real pod discovery, which roundhouse currently does not have.

5. **Do we ever want a second local candidate source (bare vLLM/SGLang pools via the 003 metrics contract)?** It would make roundhouse useful in deployments with no Dynamo — but it introduces a second, weaker cache model and a second load model, i.e. voluntarily importing risk R5 and half of R4. Say yes or no deliberately.

6. **If roundhouse is ever fronted by a Gateway API gateway, what represents it?** Answer from evidence: a plain `Service`, never an `InferencePool` (§4, §5.3). But that leaves session affinity across roundhouse replicas unsolved by this extension — it must come from Gateway API session persistence or from the client honoring a returned session id. Worth a ruling before anyone writes a chart.

7. **Is the `select/reserve` contribution worth making, and to whom?** It closes a hole GAIE's own scheduler proposal admits (`006:145-149`). But the target repo is `llm-d/llm-d-router` under a different org, and `SYNERGY-nemo-relay.md`'s S4 already sequences contributions after M7 proves the seams (`rh:SYNERGY-nemo-relay.md:229-232`). Decide whether a second contribution track exists at all.

   *[2026-08-21: **answerable now, and the answer is mostly no.** Per the §3.4 and S-H notes, llm-d-router books in-flight load at dispatch, releases it at `StartOfStream`, derives saturation from those booked counters rather than a scrape, and discounts the cached prefix — the assumed-load hole this question was built on is closed upstream. What remains ours is the narrower claim that the booked number should be *measured* from the serving engine's KV events rather than estimated at `bytes/4 × 1.5` and discounted by an approximate character-hash index, and that claim is closer to a Dynamo contribution than a roundhouse one. The ruling in `../synergies/ecosystem-round-2.md` deferred this contribution anyway, so nothing operative changes; only its stated premise does.]*

8. **Does the "Relay-CLI-fronts-roundhouse" topology ruling need a k8s sibling?** `SYNERGY-nemo-relay.md` §S3 rules two supported topologies (Direct; Chained via Relay) with explicit chain guards (`rh:SYNERGY-nemo-relay.md:200-228`). A gateway in front of roundhouse is structurally the *same* class of risk — a component that can route around us, change who pays, or re-encode history — and if k8s is ever in scope it needs the same four guards written down before it is called supported.

---

### Appendix — pinned facts, for the synthesis to quote without re-deriving

| Fact | Cite |
|---|---|
| EPP/BBR/latency-predictor removed to llm-d; repo keeps InferencePool + protocol + conformance + LWEPP | `gaie:README.md:16-23`; commits `a70292c` (Jun 16 2026, −90,903 lines), `88fd479` (Jun 17 2026) |
| v1.6.0 released 17 Aug (2 days before pin); removed `InferenceObjective`, `InferenceModelRewrite`, `EndpointPickerConfig`; `endpointPickerRef` now optional | https://github.com/kubernetes-sigs/gateway-api-inference-extension/releases/tag/v1.6.0 |
| Rationale for the move: EPP needs "deep AI/ML domain expertise and tight integration with model server implementations"; llm-d better represents that community | https://github.com/kubernetes-sigs/gateway-api-inference-extension/issues/2430 |
| llm-d-router now hosts EPP + InferenceObjective + InferenceModelRewrite + a disaggregation sidecar | https://github.com/llm-d/llm-d-router |
| Zero occurrences of NVIDIA/Dynamo/NIM/NeMo in the tree; only NVIDIA presence is the Triton/trtllm metric column | `gaie:docs/proposals/003-model-server-protocol/README.md:26-32` |
| Conformant gateways listed: Istio, Agentgateway, NGINX Gateway Fabric | `gaie:site-src/implementations/gateways.md:3-9` |
| Protocol: ext-proc, streaming required, subset-in / endpoint-out, both header and `envoy.lb` metadata, 503/429 immediate responses, leader-only readiness | `gaie:docs/proposals/004-endpoint-picker-protocol/README.md` |
| Pick happens at body `EndOfStream`; 10 MB body cap | `gaie:pkg/lwepp/handlers/server.go:103`, `:192-206` |
| Scheduler goal O(10 ms) avg; measured p90 ≤ 100 ms at 4 CPU / 8 GiB, 1→5000 QPS shared-prefix | `gaie:docs/proposals/006-scheduler/README.md:33`; `gaie@a70292c^:site-src/guides/epp-configuration/resource-tuning.md:7-34` |
| Default scorers: queue, kv-cache-utilization, prefix-cache, lora-affinity; `max-score` picker | `gaie@a70292c^:test/integration/epp/testdata/default-config.yaml` |
| Prefix index: char-hashed JSON at 4 chars/token, block 16, max 256 blocks, LRU 31,250/server, HBM only, degrades when sharded | `gaie@a70292c^:.../approximateprefix/{hashing.go:35-51,105-131, types.go:88-114}`; `0602:16-18,79-84` |
| Flow control: `FlowKey{ID, Priority}`, sheddable = priority<0, fairness×ordering plugins, saturation as roofline gradient with 200 ms staleness ⇒ 1.0 | `gaie@a70292c^:pkg/epp/framework/interface/flowcontrol/flow.go:21-63`; `.../saturationdetector/utilization/README.md:8-58` |
| Dynamo cost logit + default weights (`overlap_score_credit=1.0`, `prefill_load_scale=1.0`, `host=0.75`, `disk=0.25`, `T=0`) | `dyn:lib/kv-router/src/scheduling/selector/default.rs:216-322`; `dyn:lib/kv-router/src/scheduling/config.rs:42-66,800-810` |
| Roundhouse axis: local `effective_prefill_tokens`; frontier `isl − p_hit·prefix`; weights 1.0/0.5/0.25; load in booked prefill tokens | `rh:crates/roundhouse-fleet/src/local.rs:130-172`; `rh:crates/roundhouse-core/src/routing/{ledger.rs:30-80, policy.rs:83-92,160-186}` |
| Roundhouse `expected_ttft_ms` is a static config constant, `quality_prior` is configuration not measurement | `rh:crates/roundhouse-server/src/engine.rs:1058-1064`; `rh:crates/roundhouse-core/src/metrics/pricing.rs:54-60` |
| Roundhouse MCP surface is 8 tools: `status`, `init_session`, `declare_intent`, `prefer`, `set_quality_floor`, `fetch_steer`, `report_outcome`, `explain_last_route` | `rh:crates/roundhouse-mcp/src/tools.rs:47`, `:102-232` |
| Roundhouse has no k8s artifacts of any kind | verified: no Dockerfile/chart/manifest in `/home/user/roundhouse`; zero `kubernetes\|k8s\|helm\|inferencepool\|ext-proc\|envoy\|gateway api` hits in `.rs`/`.md`/`.toml` outside `target/` |

---

## Appendix: independent verification (2026-08-19)

**[CONFIRMED]** As of v1.6.0 the scheduler is no longer in this repo — EPP, plugin framework, flow control, latency predictor, BBR, and InferenceObjective/InferenceModelRewrite/EndpointPickerConfig APIs were removed via commit a70292c ('Cleanup EPP and Latency Predictor #2967', Jun 16 2026), 481 files changed, 221 insertions, 90,903 deletions.

git show a70292c --stat in /workspace/nvidia/gateway-api-inference-extension reports exactly '481 files changed, 221 insertions(+), 90903 deletions(-)'. README.md:16-23 (import banner) states EPP, InferenceObjective/InferenceModelRewrite APIs and BBR moved to llm-d/llm-d-router and llm-d/llm-d-inference-payload-processor, with 'No new code will be accepted to these packages in this repository, and they will be archived soon.'

**[CONFIRMED]** InferencePool.spec.selector matches Pods by labels only within the same namespace, cross-namespace selection unsupported; targetPorts 1..8, addressed podIP:portNumber; appProtocol defaults to http with kubernetes.io/h2c option; endpointPickerRef is optional (since v1.5.0) with failureMode defaulting to FailClose; status.parents max 32.

api/v1/inferencepool_types.go: Selector field doc 'matches Pods by their labels only within the same namespace; cross-namespace selection is not supported'; TargetPorts has +kubebuilder:validation:MinItems=1/MaxItems=8 and doc 'addressable as a podIP:portNumber combination'; AppProtocol +kubebuilder:default="http", enum http/kubernetes.io/h2c; EndpointPickerRef is *EndpointPickerRef (pointer, optional); FailureMode +kubebuilder:default="FailClose"; Parents +kubebuilder:validation:MaxItems=32. InferencePoolReasonEndpointPickerRefMissing comment confirms the field became optional 'Until Gateway API Inference Extension release v1.5.0'.

**[CONFIRMED]** EPP readiness gRPC health check returns SERVING only if the datastore has synced AND this replica is the elected leader; followers/unsynced return NOT_SERVING (i.e. exactly one active EPP per pool).

docs/proposals/004-endpoint-picker-protocol/README.md:113 — 'Readiness Check (readiness): ... Returns SERVING if the EPP datastore has synced and the EPP is the elected leader (in multi-replica deployments). Returns NOT_SERVING if the datastore has not synced or the EPP is a follower.'

**[CONFIRMED]** LWEPP defers the pick until body EndOfStream when headers arrive without EndOfStream, caps request body at 10 MB, and returns gRPC ResourceExhausted above that cap.

pkg/lwepp/handlers/server.go:103 'const maxRequestBodySize = 10 * 1024 * 1024 // 10MB'; line 196 'return status.Errorf(codes.ResourceExhausted, "request body size limit of %d bytes exceeded", maxRequestBodySize)'; headersDeferred set true at header-processing branch (line 183) and consumed at RequestBody EndOfStream branch (lines 200-208), matching the described defer-to-body-EndOfStream timing.

**[CONFIRMED]** PickResult's Fallbacks, MutatedBody, and ExtraHeaders fields are declared in the reference picker but never consumed/read anywhere in pkg/ — body mutation and extra-header injection are structurally available but not wired up.

grep -rn '\.Fallbacks\b|\.MutatedBody\b|\.ExtraHeaders\b' pkg/ (excluding tests) returns zero hits; the only occurrences of the identifiers in pkg/ are the three field declarations at server.go:74-76.

**[REFUTED]** The string 'session' occurs exactly four times in the pinned tree outside a Slack-channel sentence — three in the 0602 prefix-cache proposal (arguing against session affinity) and one in a request_test.go Cookie fixture.

grep -rni 'session' across the whole tree (excluding .git) returns 8 hits, not 4: six in docs/proposals/0602-prefix-cache-aware-routing-proposal/README.md (lines 39, 41, 51, 55, 59, 86 — the report cites only 39/41/51) and two in pkg/lwepp/handlers/request_test.go (lines 261 AND 274 — the report cites only 261). No Slack-channel sentence containing 'session' exists anywhere in the tree, so the stated exclusion is moot. The undercount does not overturn the substantive point (all 8 occurrences are either the proposal arguing against session affinity or a Cookie-header parsing test, none is stateful session handling), but the specific count and citation set are wrong.

**[CONFIRMED]** Dynamo KV-router selection is argmin over a cost logit: logit = prefill_load_scale·max(0, raw_prefill_blocks − overlap_credit_blocks) + decode_cost_blocks + decode_active_request_weight·active_requests, with overlap_credit_blocks combining device/host/disk/shared tiers, and default weights overlap_score_credit=1.0, overlap_score_credit_decay=0.0, prefill_load_scale=1.0, host_cache_hit_weight=0.75, disk_cache_hit_weight=0.25, decode_active_request_weight=0.0, router_temperature=0.0.

lib/kv-router/src/scheduling/selector/default.rs:262-278 constructs exactly this logit (adjusted_prefill_blocks = (raw_prefill_blocks - overlap_credit_blocks).max(0.0); prefill_cost_blocks = weights.prefill_load_scale * adjusted_prefill_blocks; logit = prefill_cost_blocks + decode_cost_blocks + active_request_cost_blocks) with overlap_credit_blocks built from effective_overlap_score_credit*device + host_cache_hit_weight*host + disk_cache_hit_weight*disk + shared_cache_multiplier*shared (lines 246-250). lib/kv-router/src/scheduling/config.rs:41-66 and Default impl at 799-812 give exactly the cited defaults: overlap_score_credit: 1.0, overlap_score_credit_decay: 0.0 (default_overlap_score_credit_decay), prefill_load_scale: 1.0, decode_active_request_weight: 0.0, host_cache_hit_weight: 0.75, disk_cache_hit_weight: 0.25, router_temperature: 0.0.

**[CONFIRMED]** The historical EPP's ResponsesRequest{Input, Instructions, Tools, CacheSalt} and ConversationsRequest{Items, Metadata, CacheSalt} types exist, but no previous_response_id, store, or conversation-id field is present anywhere in that package — confirming no OpenAI Responses-API state is modeled.

git show a70292c^:pkg/epp/framework/interface/requesthandling/types.go shows ResponsesRequest with exactly {Input any, Instructions any, Tools any, CacheSalt string} and ConversationsRequest with exactly {Items []ConversationItem, Metadata map[string]any, CacheSalt string}. grep -in 'previous_response_id|PreviousResponseId|store|conversation_id|ConversationID' on that file returns nothing, and git grep for previous_response_id across the whole a70292c^ tree in *.go returns zero matches.

**[CONFIRMED]** Roundhouse has no k8s footprint whatsoever — no Dockerfile, chart, or manifest, and zero kubernetes/k8s/helm/inferencepool/ext-proc/envoy/gateway-api mentions in .rs/.md/.toml outside target/.

find for Dockerfile*/*.yaml/*.yml under /home/user/roundhouse (excluding target) returns nothing. A naive substring grep -rliE for 'helm' turns up two files (roundhouse-store-redis/tests/spend_contract.rs:214, roundhouse-mcp/src/surface.rs:119) but both are false positives on the word 'overwhelmingly' — a word-boundary grep (\bhelm\b, \bkubernetes\b, \bk8s\b, \binferencepool\b, \benvoy\b, \bgateway api\b) over the same file set returns zero hits, matching the report's claim.

### Corrections

- Section 4, item 1 ('Finding: nothing in the extension... understands a conversation'): the report states the string 'session' occurs exactly four times in the pinned tree outside a Slack-channel sentence, citing gaie:docs/proposals/0602-.../README.md:39,41,51 and gaie:pkg/lwepp/handlers/request_test.go:261. Actual count is eight: six occurrences in the 0602 proposal (lines 39, 41, 51, 55, 59, 86 — the report omits 55, 59, 86) and two in request_test.go (lines 261 and 274 — the report omits 274, which is the assertion that echoes the same Cookie value back). There is no Slack-channel sentence containing 'session' anywhere in the tree, so the report's own stated exclusion criterion never applies and is presumably a leftover from a different search. This does not change the ruling itself — all eight occurrences are consistent with 'arguing against session affinity' or a Cookie-header test fixture, not stateful session handling — but the specific number and citation set given as evidence are wrong by a factor of two, which matters given this exact sentence is flagged in the document as 'the fact that decides the topology.'

### Checker's confidence statement

I re-derived 8 of the highest-leverage claims directly against the pinned trees (gateway-api-inference-extension @ 84436a9, its pre-deletion parent a70292c^, the pinned Dynamo checkout @ ac7b751, and the roundhouse working tree, including its uncommitted M6 review-fix diff). Seven of eight held up exactly, several down to precise line numbers and numeric defaults (the a70292c diffstat, the Dynamo cost-logit formula and its six default weights, the InferencePool CRD field semantics, the EPP readiness/leader-election health-check text, the LWEPP 10MB/EndOfStream mechanics, the unused PickResult fields, and the historical Responses/Conversations request shapes). One claim — the 'session' occurs exactly four times' count — is wrong by 2x on inspection, though it does not overturn the surrounding argument. I did not re-check the bulk of the document: the scheduler-plugin-framework interface sketch (§1.3), the flow-control/saturation-detector formulas and defaults (§1.4), the full prefix-cache hashing implementation (§3.1, hashing.go internals, LRU capacity arithmetic), the conformance report/gateway attrition table (§1.5), the roundhouse routing/policy/budget internals cited in §3.3 and §5.1/5.2 (ledger.rs, policy.rs, control/budget.rs, control/policy.rs — several of these files are among the ones with uncommitted changes, so their cited line numbers carry real risk of drift even if the substantive claims are right), the MCP tool-surface list, and every external GitHub URL/issue citation. Given the one miss I found was a simple undercounted grep rather than a fabrication or a reversed API fact, and every structural/mechanical claim I checked (CRD shapes, protocol wire behavior, dependency pins and formulas) checked out exactly, I'd treat the unchecked remainder as probably accurate in its mechanical particulars but not guaranteed — especially any specific line-number citation into a roundhouse file that shows as modified in git status, and any claim whose precision depends on an exhaustive negative search the way the session-count claim did.

---

## Re-read 2026-08-21 — the M9 watching brief

Per the CLAUDE.md vigilance rule ("read what changed upstream before it
lands"), this document's pin was re-read on 2026-08-21, three days before the
SLO-header adoption item in `../synergies/ecosystem-round-2.md` would have been
written into code. Fresh full clones under the M9 scratchpad; no roundhouse
code was built or run.

**Revisions.**

| Tree | Pin this document was written against | State on 2026-08-21 |
|---|---|---|
| `kubernetes-sigs/gateway-api-inference-extension` | `84436a9` (2026-08-18) | `84436a9` — **identical**; `git diff --stat 84436a9 HEAD` empty, `gh api .../commits` agrees. Latest tag still `v1.6.0`. |
| `llm-d/llm-d-router` | not pinned (referred to by name only) | `e051872097bae66089936ae6c24af2bec7f92be6` (2026-08-21 14:36 -0700); latest release `v0.10.0` (2026-08-17). Cited above as `lldr@e051872:`. |
| `kubernetes-sigs/gateway-api` | not read | read via API only, for the migration question. |

**Re-checked and unchanged** (all still resolving at `84436a9`, so §§1-2 and the
Appendix table stand as written): the `InferencePool` v1 CRD fields, conditions
and `EndpointPickerRefMissing` reason; the `InferencePoolImport` alpha API; the
Endpoint Picker Protocol v1.0.0 spec including leader-only readiness; the LWEPP
handlers, their 10 MB body cap and the defer-to-`EndOfStream` timing; the unused
`PickResult` fields; the conformance suite and its reports; the zero
NVIDIA/Dynamo/NeMo hits; the listed conformant gateways. `llm-d-router` still
depends on `sigs.k8s.io/gateway-api-inference-extension v1.5.0`, one minor
behind the released CRD (`lldr@e051872:go.mod:61`).

**Claims that moved, each carrying a bracketed note above.**

1. **§2.4 header vocabulary — the one with code consequences.** Five of the
   seven reserved names were renamed to an `x-llm-d-` prefix in llm-d-router at
   `81d7f460` (2026-05-20, PR #1161), old names kept as deprecated aliases; the
   three `x-gateway-destination-*` protocol headers deliberately kept their
   names. GAIE's LWEPP constants were never updated, so the two repos now
   disagree.
2. **§2.2 / §2.5 / R2 — ext-proc body buffering.** Unchanged in LWEPP and
   unchanged in the real EPP: llm-d-router still decides at body `EndOfStream`,
   `FULL_DUPLEX_STREAMED` notwithstanding, and has no body cap of its own. New:
   an upstream container-sizing guide with measured numbers at 100k-token
   agentic workloads.
3. **§3.4 Load row / §5.2 S-H — booked vs scraped load.** Both trees carry a
   booking `inflight-load-producer` and a booked-counter `concurrency-detector`;
   the producer now discounts the cached prefix. The dive described only the
   scraped `utilization` detector.
4. **§4 session finding.** True of GAIE, false of llm-d-router since
   `f3ae7503` (2026-07-16): an `agent-identity` plugin keying flow-control
   fairness off `x-claude-code-session-id`, `x-session-affinity`, `session-id`
   (Codex) and `session_id`. The topology conclusion is unaffected; the premise
   is not.
5. **§4.6 / R8 — "cannot name a frontier model".** Still true of
   `InferencePool`; Gateway API's GEP-4894 `Backend` (Experimental) proposes
   `ExternalHostname` for exactly that purpose.
6. **§5.1 S-C — InferencePool as discovery.** Upstream made discovery a plugin
   interface with a file-backed, Kubernetes-free implementation.
7. **R1 — the third move.** Announced, not executed; no InferencePool types in
   `kubernetes-sigs/gateway-api:apis/`. README and FAQ contradict each other on
   where conformance ends up. A new venue, `kubernetes-sigs/wg-ai-gateway`,
   now incubates the AI-gateway GEPs.
8. **R3 — single active EPP.** Protocol text unchanged, but llm-d-router
   documents Active-Active with near-linear scaling, at the cost of unshared
   flow-control and prefix state.

**Not re-derived on this pass**, and therefore inherited at their original
confidence: the flow-control formula details in §1.4, the prefix-hashing
arithmetic in §3.1, the Dynamo cost-logit numbers in §3.2, every `rh:` citation
into roundhouse (line numbers there have moved through M7-M9), and the external
GitHub issue URLs. The fact-checker's `### Corrections` entry above — the
undercounted "session" grep — is left standing as written; it is not silently
repaired here.

**Verdict.** No decision in the round-2 ruling reverses. Watching brief: still
a watching brief, now over three addresses instead of one. InferencePool as
discovery: still deferred, and upstream's own move to pluggable discovery makes
deferring cheaper to reverse. SLO headers adopted: still yes, but **at the
current spellings** — the one operative correction, caught with zero lines of
roundhouse code written against the stale names, because the ruling item that
was to land with M7 did not. One further claim in the ruling is corrected at the
premise level and not at the decision: its select/reserve sentence rests on an
assumed-load hole upstream has since closed, which leaves the deferral intact
and open question 7 above answered.
