# Measurements

This chapter records measured numbers with their setup, source, and revision, and what each number does not prove. Roundhouse was in the request path for none of them.

## Local prefix-cache reuse on Dynamo and vLLM

This measurement shows how much of each prompt a local vLLM worker behind Dynamo serves from its prefix cache on a replayed agentic session. The client called the Dynamo frontend directly.

Source: the branch `origin/ai/charming-hypatia-vtpz0y` at commit `6756c85`, files `use-cases/cache-aware-routing/README.md` ("Local tier") and `use-cases/cache-aware-routing/DYNAMO_LOCAL_SERVING.md`. That branch is not merged, so its replay script `run_local.py` and its single-GPU `serve_model.sh` are not on `main`. Read them from that commit to repeat the run.

Setup:

| Item | Value |
|---|---|
| Hardware | 1x NVIDIA H100 80GB |
| Dynamo | `ai-dynamo/dynamo` built from source at `ac7b7513790ef1d619b46f805aea03c9f21200ba` (the revision that the Roundhouse `Cargo.toml` pins) |
| Backend | vLLM 0.26.0 and torch 2.11.0, installed by `uv pip install -e '.[vllm]'` |
| Model | `Qwen/Qwen2.5-Coder-32B-Instruct`, `TP=1`, `BLOCK_SIZE=64`, `--enable-prefix-caching` |
| Workload | One `corpus.md` of about 1,000 tokens plus 20 turns from `turns.jsonl`, replayed against `/v1/chat/completions` of Dynamo |
| Metric | `usage.prompt_tokens_details.cached_tokens` as vLLM reports it. This is the prefix-cache block match of vLLM, not an estimate. |
| Date | 2026-09-23 |

Results:

| Session | Turn | Input tokens | Cached tokens | Cached share |
|---|---|---|---|---|
| 1 | 1 | 1490 | 0 | 0.0% |
| 1 | 2 | 1539 | 1472 | 95.6% |
| 1 | 20 | 2833 | 2752 | 97.1% |
| 1 | all 20 | | | 93.6% of input |
| 2 | 1 | 1490 | 1472 | 98.8% |

Two effects show:

- **Reuse within a session.** Turn 1 is cold. From turn 2, each turn reads the prefix that the previous turn wrote.
- **Reuse across sessions.** Session 2 opens warm, because the corpus prefix is still resident from session 1.

The cached counts (1472, 2752) are multiples of 64, the block size. The source does not state that as the cause.

What this does not prove:

- It is not a Roundhouse routing test. The server binary attaches no local fleet, so Roundhouse cannot dispatch a turn to this worker.
- It is not a multi-GPU or tensor-parallel test. Only `TP=1` was run.
- It is not a KV-event-stream test. The numbers come from the HTTP usage of vLLM. Nothing subscribed to the ZMQ KV-event stream on port 20080, so the block-hash indexing of `EmbeddedFleet` was not exercised.

## Qwen2.5-Coder-32B capacity on one H100 80GB

Same source, setup, and date as the previous section, with the vLLM default `gpu_memory_utilization` and `max_model_len=32768`.

| Item | Value |
|---|---|
| Weight load | 61.04 GiB in 11 s, from local ext4 disk |
| KV cache budget | 9.97 GiB, which is 40,832 tokens |
| Maximum concurrency at `max_model_len=32768` | 1.25x |
| Steady-state GPU memory | about 76.9 of 81.6 GB used, about 4.2 GB free |
| Weights on disk | about 62 GB of bf16 safetensors |
| `maturin develop --uv` build of the Dynamo bindings | about 3.5 min |

The 32B model fits with almost no headroom. At 1.25x concurrency, the GPU can barely serve two full-length requests of 32K tokens at once. That is enough for one session at a time with a corpus of about 1,000 tokens. For real concurrency on one GPU, `Qwen/Qwen2.5-Coder-14B-Instruct` (about 28 GB of weights) leaves more KV headroom.

## TypeSafe Jev routing benchmark

This is the only published routing evidence for the TypeSafe Jev classifier. It is part of the reason that the [routing learner](../concepts/routing-learner.md#cold-start-prior-from-classifier-tier-answers) uses Jev answers only as features and a small cold-start prior.

Source: `pst2154/Typesafe_Testing` at commit `84facd7` (branch `benchmark/typesafe-sol-classification`), three commits dated 2026-09-16. This project did not run it, and no call from this project reached the TypeSafe API.

Setup:

- The inputs are single sentences, the longest about 20 words. No case carries a conversation, a tool result, a system prompt, or code.
- The labels are handler classes (`deterministic_tool`, `small_model`, `reasoning_model`, `human_review`) that one person wrote. Accuracy means agreement with that label.
- Jev gets `{"request": text}` as its state and one `choice` question named `route`.
- Latency is hosted end to end, with queueing and serving configuration included.
- The Qwen baseline generates JSON text. Nobody ran it as a single prefill with constrained decoding or option logprobs.

Results:

| Benchmark | Cases | Jev | Baseline |
|---|---|---|---|
| Classification against `SOL` (a hosted LLM baseline named so in the report) | 46: 28 ordinary, 12 adversarial, 6 ambiguous | 38/40 (95.0%), mean 237 ms | 40/40, mean 2,464 ms |
| Routing against Llama 3.2 1B | 36, 32 scored | 32/32, mean 236 ms | 5/32, mean 4,783 ms, valid JSON on 24 of 36 |
| Routing against Qwen3.8-27B, two runs | 36 per run | 64/64, mean 228 ms, p95 260 ms | 64/64, mean 1,887 ms, p95 5,642 ms |

Other observations from the same reports:

- The mean certainty of Jev fell from 0.951 on ordinary cases to 0.756 on adversarial cases and 0.673 on ambiguous cases. The self-reported confidence of `SOL` stayed at 0.99 on adversarial cases.
- The one adversarial miss of Jev returned 0.54 against a 0.50 threshold, with certainty 0.08. A confidence floor catches that case.
- Jev and Qwen disagreed only on the four ambiguous cases.

Reading: against a 27B general model, accuracy is a tie, and the measured advantage of Jev is latency only. No case measures whether the routed handler did the task. So the benchmark does not show that routing by this classification improves function, cost, or time to solution.

A separate TypeSafe cookbook on skill suggestions (docs.typesafe.ai, read 2026-09-17) reports that over 488 requests, suggestions fixed 37 turns and broke 7 that the agent had right. A confident wrong hint is worse than no hint.

## Endpoint picker cost at agentic request shapes

The Kubernetes Gateway API Inference Extension routes each request through an out-of-process Endpoint Picker (EPP) over Envoy ext-proc, and the EPP buffers the whole request body before it decides. Upstream projects measured where that cost goes. Roundhouse did not.

Scheduler latency, from `gateway-api-inference-extension` one commit before `a70292c` (`site-src/guides/epp-configuration/resource-tuning.md:22-34`):

| Item | Value |
|---|---|
| Tool | `inference-perf` |
| Model and server | Qwen3-32B on vLLM, streaming completions |
| Workload | Shared prefix, prompts of about 60 + 12 tokens, staged ramp from 1 to 5000 QPS |
| EPP container | Requests 4 CPU cores and 8 GiB, memory limit 16 GiB, no CPU limit |
| Result | p90 scheduler latency within 100 ms at every stage |

This covers the scheduler only. It does not cover the ext-proc round trip or body buffering.

EPP sizing, from `llm-d/llm-d-router` at `e051872` (`docs/operations.md`), for benchmarks described as agentic at 100k input and 1k output tokens:

| Item | Value |
|---|---|
| Rule of thumb | 0.5 to 1.0 CPU cores per request per second |
| Peak at 50 requests per second | 17.5 to 20.3 cores, 3.7 to 5.2 GiB |
| Co-located Envoy alone at 100 requests per second | about 8 cores |
| Idle EPP at 100 model-server pods | about 7.5 cores, from metric scraping alone |
| Scheduler P50 | 0.1 to 0.2 ms |
| Active-Active throughput, 1 to 4 replicas | 1.0x, 2.0x, 2.7x, 3.5x |

Reading: the routing decision is almost free. The cost is buffering, parsing, and prefix-hashing the body, which an embedded selection service avoids (see [Routing and the selection service](../concepts/routing.md)). Active-Active replicas keep flow-control and prefix state per replica. Upstream says to avoid Active-Active with approximate prefix routing, because the partitioned state lowers prefix-cache hit rates.
