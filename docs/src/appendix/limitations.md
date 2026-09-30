# Limitations

This appendix lists what Roundhouse does not do, one current limit per line, with a link where another chapter explains it.

## Wire and transports

- There is no WebSocket or gRPC transport.
- An interrupted generation cannot resume from its partial output, although that output is durable in the log.
- No client speaks `openai_chat_completions`. See [Configure providers and the catalog](../guides/catalog.md#dialects).
- `/v1/models` is not served, so a client with gateway model discovery sees no catalog.
- The canonical item has three content kinds: text, tool call, and tool result. Responses `reasoning` items are dropped. Other input types, such as `custom_tool_call` and compaction items, get a 422. Image and file parts have no variant, and `/v1/responses/compact` is not served.
- On the Responses dialect, encrypted reasoning items are never requested or resent. This costs function on reasoning models, not cache reuse.
- The 200 SSE response starts before dispatch. An upstream 429 or 5xx fails over to the next admitted candidate, but a local target never fails over. When none is left, the failure arrives inside the stream, never as an HTTP 429 with `Retry-After`.

## Tokens, prices, and providers

- Each process has one tokenizer, Llama-family or byte. For an Anthropic model the quote is an estimate with an unmeasured error; the bill uses the provider's reported usage.
- `count_tokens` answers with the process tokenizer, labels the count as an estimate, and is exempt from fair use.
- `quality_prior` is configuration. See [Configure providers and the catalog](../guides/catalog.md#source-quality-priors).
- One `(provider, model)` carries one price. Several upstream routes for one model are not modeled.
- No OpenRouter routing preference is sent, and the served model is not read back, so a fallback is attributed to the requested model.
- The OpenRouter facts come from unauthenticated reads on 2026-08-24. `/messages` stability, the unit of `usage.cost`, unknown-field handling, and the live `/benchmarks` payload were not exercised with a key.
- No SLO or fairness header is read, and there is no dispatch queue, priority band, or LoRA-adapter scoring. `lora_name` is sent as `None`.
- No `x-dynamo-*` or `x-llm-d-*` header is sent.

## Local tier

- The shipped `roundhouse` binary attaches no local fleet; only an echo executor and test doubles implement the local executor. A key whose policy admits only local targets, or that promises local service when an allowance is spent, stops the boot.
- No measured prefill slope exists, so `local_ttft_ms_per_prefill_token` defaults to zero. See [Configure providers and the catalog](../guides/catalog.md#local-latency).
- The fail-open decision around the residency call cannot see a spent `degrade_to_local` budget, because the grant opens after quoting. A slow fleet can then drop the local candidate that the budget routes to.
- Dynamo wires no eviction signal, so the residency call cannot be skipped after a recent full local match.
- The ZMQ KV-event streams of the Dynamo workers cannot be tunneled over SSH, so Roundhouse must share a network with its workers.

## Clients and session identity

- No capture in this tree contains `x-claude-code-agent-id`. Without it, a Claude Code sub-agent shares its root's session label and log. `x-claude-code-parent-agent-id` is not read.
- No opencode header is read, and whether opencode sends a conversation identity is not established.
- `SessionCreated` records no client kind, so the log cannot be split by client.
- No close signal was found from Codex at `6344a65` or Claude Code 2.1.284, in a search that was not exhaustive. The stateless MCP surface has no session to end. `stop_reason` is often `None`, so read the last assistant item instead.
- `metadata.user_id` names the Claude session and is not forwarded to Anthropic.
- Whether Anthropic's terms allow a third party to handle a forwarded subscription token (`sk-ant-oat` class) is not settled. The Claude Code gateway fingerprint table has no Roundhouse row and no generic opt-in.
- No real ChatGPT login or Claude subscription seat has been forwarded. The first is tested with a crafted `auth.json`, the second against a mock upstream that checks the three admitted headers.

## MCP control surface and launch topologies

- A control call with no correlator, or with an aged-out binding, gets the caller's most recent conversation, a guess that is not shared across nodes. See [MCP control surface](../concepts/mcp.md).
- No real `codex` has dispatched a `tools/call` here, so the `threadId` correlator is tested only against a captured `_meta` shape.
- A control call does not pass through NeMo Relay. `topham relay` registers `/mcp` directly against the deployment, so a Relay chain carries only the turns. See [Hook up Claude Code](../guides/claude-code.md).
- Chained Codex is unproven, because a Codex `--config` override outranks the generated `config.toml`. See [Launch with topham](../guides/topham.md).
- Under an existing subscription login, an interactive Claude Code session asks once before it uses a `roundhouse-key` profile's key. It also asks before it calls an `mcp__roundhouse__*` tool, which a headless run allows with `--allowedTools`.

## Validate and steer

- The judge sends no credential. On a real provider client every review is refused before a socket opens, the check is abandoned as unreachable, and the turn goes on unchanged. See [Validate and steer](../concepts/validate-steer.md).

## Control plane and admin plane

- A revoked key works on another node for up to one `admission_cache_ttl_ms` (default 30 seconds), or two after a failed refresh. A streaming turn is not interrupted. See [Control plane](../concepts/control-plane.md#revocation-is-bounded-by-a-snapshot).
- The admin plane has no audit trail, key rotation without a service gap, pagination, or credential CRUD. A revoke leaves only `revoked_at_ms`.
- Archiving a project is terminal, and users cannot be deleted.
- There is no request-rate limit; a fair-use window with `max_tokens` limits volume.
- A request is authorized on one plane snapshot and can run on a version that moved microseconds later.
- A process composes a learner only if some project enables one at boot. Otherwise, a project that enables it later through the admin plane learns nothing until a restart, with one warning per project.
- Without `ROUNDHOUSE_REDIS_URL`, fair-use windows count per process, with a warning at the first enforcement of a non-empty ceiling.

## Operations and metrics

- There is no health, readiness, or status route for a load balancer.
- The node id is minted per process, so two restarts of one machine look like two nodes.
- The directory status carries no lineage, so two nodes at the same version number are not comparable.
- Metrics are per process; a fleet view needs a scrape of each node.
- The dashboard and the `measured_usd` of the reconciliation view cover all history, with no time window, because the fold keeps no buckets.
- In Configured mode, the dashboard page sends no key, so its `/v1/metrics` fetch is refused. Read the JSON with a key.
- The HTML dashboard omits `first_output`, `completed_turn_elapsed`, `incomplete_turn_elapsed`, and `cache_reuse_evidence`.
- `cache_reuse_evidence` does not update routing.

## Cache

- `CacheLedger::invalidate` has no production caller. Prefix admission forks a rewritten history into a new session with a cold ledger instead.
- No live Anthropic request has confirmed a read through the second cache marker. See [Measure cache reuse](../guides/cache-reuse.md).

## Redis

See [Deploy with Redis](../operations/redis.md#learner-store).

- A learner read of more than about 1,333 distinct targets fails as `Unavailable`, because Lua's `unpack` fails past about 8,000 values.
- A stored mark that the Lua parser rejects (`BADMARK`) blocks the next marked append as `CorruptLog` until an operator rewrites or removes it.
- If the session store loses the learning index with the events, nothing remains to deliver. A lost learner store is found only after a full audit cycle.

## Routing learner and classifier

See [The routing learner](../concepts/routing-learner.md).

- Intervals in one session are correlated. `min_sessions` and the session-clustered bootstrap reduce this but do not remove it.
- Only `Live` and `Shadow` validation arms produce labels, so a project without validation learns no quality corrections.
- The fold trusts the content gaps that the validator records.
- A steer changes later intervals, not the label of the reviewed interval.
- Consistent-trajectory credit loses failover intervals and gives no evidence to strategies that disagree anywhere in an interval.
- There is no live provider, Redis outage, crash, or deployment evidence for the learner, and no claim from it about routing quality, savings, or latency.
- The promotion gate reports cost as `unpriced` for a recipe with local targets until the log records a local capacity price. Judge dollars are unpriced, because side calls record no rate card.
- The selector does not learn from background classifier labels.
