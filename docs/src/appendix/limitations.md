# Limitations

This appendix lists what Roundhouse does not do today. Each entry is a current limit. Where another chapter explains a limit in depth, the entry gives one line and a link.

## Wire and transports

- Roundhouse has no WebSocket transport and no gRPC transport.
- Roundhouse cannot resume an interrupted generation from its partial output. The partial output is durable in the log, but nothing reads it back to continue the turn.
- No client speaks `openai_chat_completions`. A catalog entry with that dialect loads, but the boot stops when `ROUNDHOUSE_FRONTIER_UPSTREAM` names a real upstream.
- Roundhouse does not serve `/v1/models`. A client with gateway model discovery sees no catalog. Showing the Roundhouse routes in a model picker has not been decided.
- Roundhouse does not serve `/v1/responses/compact`. It refuses opaque compaction input items.
- The canonical item has three content kinds: text, tool call, and tool result. Responses `reasoning` items, `custom_tool_call` items, compaction items, and image or file parts have no variant.
- On the OpenAI Responses dialect, Roundhouse never requests encrypted reasoning items and never resends them. This costs function on reasoning models. It does not cost cache reuse.
- Roundhouse returns the 200 SSE response before routing and dispatch finish. An upstream 429 or 5xx first fails over to the next admitted candidate. A local target never fails over. If no candidate is left, the failure reaches the client inside the stream. It is never an HTTP 429 with `Retry-After`. Only a refusal before the turn starts, such as fair use, carries an HTTP status.

## Tokens, prices, and providers

- There is one tokenizer for each process: a Llama-family tokenizer or the byte tokenizer. Anthropic publishes no local tokenizer. For a turn on an Anthropic model, the quote is a systematic estimate. The bill is not affected when the provider reports usage, because the log then records the provider's numbers. Nothing in the tree measures the size of the error.
- `count_tokens` answers with the same tokenizer and labels the result as an estimate. Fair use does not apply to it.
- `quality_prior` is configuration. You write it or you import it from a published index. See [Configure providers and the catalog](../guides/catalog.md#source-quality-priors).
- One `(provider, model)` carries one price. The catalog does not model several upstream routes for one model.
- Roundhouse sends no OpenRouter routing preference and does not read the served model or provider back. If OpenRouter falls back to another model, cost and quality are attributed to the requested model.
- The OpenRouter facts in this book come from unauthenticated reads on 2026-08-24. They were not exercised with a key. This includes the stability of `/messages`, the unit of `usage.cost`, how unknown request fields are handled, and the live `/benchmarks` payload.
- Roundhouse reads no SLO header or fairness header. It has no dispatch queue, no priority bands, and no LoRA-adapter scoring. Turns dispatch as they arrive, and `lora_name` is sent as `None`.
- Roundhouse sends no `x-dynamo-*` header and no `x-llm-d-*` header.

## Local tier

- The shipped `roundhouse` binary attaches no local fleet. It quotes the catalog and nothing else. Only an echo executor and test doubles implement the local executor, so the binary cannot serve a local tier.
- A `local/` entry in a policy or a tier needs a fleet. A process that has none refuses to boot on a configuration that names one.
- No measured prefill slope has been supplied. The slope of `local_ttft_ms_per_prefill_token` defaults to zero. See [Configure providers and the catalog](../guides/catalog.md#local-latency).
- The residency call cannot see a spent `degrade_to_local` budget. The decision to fail open around a slow or failing residency call uses the policy and the credentials. It does not use the budget, because the grant opens after quoting. A turn whose budget is spent is still held to `fleet_quote_deadline_ms`. If the fleet is slower than the bound, the turn loses the local candidate to which the spent budget routes it.
- Dynamo wires no eviction signal. So Roundhouse cannot skip the residency call when its ledger shows a recent full local match.
- The `EmbeddedFleet` subscribes to the ZMQ KV-event streams of the Dynamo workers. Those streams cannot be tunneled over SSH. Roundhouse must run on the same network as its workers.

## Clients and session identity

- No capture in this tree contains the `x-claude-code-agent-id` header. Roundhouse reads it to split a sub-agent into its own session label. If Claude Code does not send it, an in-process sub-agent shares the label of its root, and their turns interleave on one log. Roundhouse does not read `x-claude-code-parent-agent-id`.
- Roundhouse reads no opencode header. A Responses request with no `thread-id`, no `session-id`, and no `prompt_cache_key` is refused with a 422. A Messages request with neither the session header nor `user_id` is anonymous on every turn. Whether opencode sends any of these is not established.
- The `SessionCreated` event records the policy, the principal, and the arm. It records no client kind. So a per-client analysis of the log cannot split by client.
- No close signal was found from Codex at commit `6344a65` or from Claude Code 2.1.284. The MCP surface is stateless, so no MCP session ends when a client exits. The search was not exhaustive. `stop_reason` is an open provider string and is often `None`. Read how a turn ended from its last assistant item.
- Roundhouse reads `metadata.user_id` to name the Claude session. It does not forward it to Anthropic.
- Whether Anthropic's terms allow a third party to handle a forwarded subscription token (`sk-ant-oat` class) is not settled. The forwarding design is for internal test deployments. Roundhouse has no row in the gateway fingerprint table of Claude Code, and that table has no generic opt-in.
- The forwarded ChatGPT login is tested with a crafted `auth.json`. No real login has passed through this code. The same holds for the Anthropic pass-through row: a mock upstream on a real socket asserts the three headers that it admits, and no real Claude subscription seat has passed through it.

## MCP control surface and launch topologies

- A control call with no correlator falls back to the caller's most recent conversation. That is a guess. See [MCP control surface](../concepts/mcp.md).
- Only the `claudecode/toolUseId` correlator has been seen arriving from a real binary. The Codex `threadId` path is tested against a captured `_meta` shape, and no real `codex` has dispatched a `tools/call` here. See [MCP control surface](../concepts/mcp.md).
- A control call chained through NeMo Relay is not tested. See [Hook up Claude Code](../guides/claude-code.md).
- Chained Codex is unproven, because a Codex `--config` override outranks the generated `config.toml`. `topham plan` says so. See [Hook up Codex](../guides/codex.md) and [Launch with topham](../guides/topham.md).
- A `roundhouse-key` profile under an existing subscription login still asks once in an interactive session before it uses the API key. An interactive run also asks before it calls an `mcp__roundhouse__*` tool. See [Launch with topham](../guides/topham.md).

## Control plane and admin plane

- Revocation is bounded by a snapshot. Each surface re-resolves a key against an admission cache. The default lifetime of that cache is 30 seconds. A key that is revoked while a turn streams does not interrupt that turn.
- The admin plane has no audit trail. Admin writes are not attributed to a key. A revoke leaves only `revoked_at_ms`.
- The admin plane has no key rotation without a service gap, no pagination, and no credential CRUD. `POST /v1/admin/credentials` answers 501.
- A project cannot be un-archived. Archive is terminal. Users cannot be deleted.
- Roundhouse has no request-rate limit. A fair-use window with `max_tokens` limits volume.
- The admin layer resolves the plane once to authorize a request, and each handler resolves it again to do its work. A request can be authorized on one snapshot and run on a version that moved microseconds later.
- A routing recipe that is added through the admin plane after boot selects nothing until a restart. The process warns once.
- Without `ROUNDHOUSE_REDIS_URL`, fair-use windows count in process memory. The first enforcement of a non-empty ceiling logs a warning.

## Operations and metrics

- There is no health, readiness, or status route. A load balancer has no cheap unauthenticated probe.
- The node id is minted for each process, so two restarts of one machine look like two nodes. There is no setting for a stable node name.
- The directory status does not carry its lineage, so two nodes that serve the same version number are not comparable.
- Metrics are per process. A fleet-wide view needs a scrape of each node.
- The dashboard reports totals over all history. It has no time-window selector. The `measured_usd` of the reconciliation view cannot be windowed.
- In Configured mode, a browser that opens `/v1/metrics/dashboard` sends no key. The page fetches `/v1/metrics`, the request is refused, and the page shows an error. Read the JSON with a key.
- The HTML dashboard does not show `first_output`, `completed_turn_elapsed`, `incomplete_turn_elapsed`, or `cache_reuse_evidence`. Read them in the JSON.
- The `cache_reuse_evidence` observations do not update routing.

## Cache

- `CacheLedger::invalidate` exists for compaction and has no production caller. Prefix admission forks a rewritten history into a new session with a cold ledger. That covers the known path.
- No live Anthropic request has confirmed reads through the second breakpoint. See [Measure cache reuse](../guides/cache-reuse.md).
- The probe for cache reuse does not establish provider cache behavior. A zero read is an observation, not a verdict.

## Redis

- A learner read of more than about 1,333 distinct targets fails as `Unavailable`. The read script sends all fields to one `HMGET` through one `unpack`, and Lua's `unpack` fails past about 8,000 values. The failed read writes nothing.
- A stored mark that fails the Lua parser (`BADMARK`) blocks the next marked append. The append is refused as `CorruptLog` and leaves the bytes unchanged. An operator must rewrite or remove the field. A missing mark, or a mark that only Rust can parse, heals on the next marked append.
- If the session store loses the learning index together with the events, nothing remains to deliver. The audit finds the loss of the learner store only after a full audit cycle.

## Routing learner and classifier

- Intervals in one session are correlated. The online gate treats each interval as one unit for each strategy. `min_sessions` and the session-clustered bootstrap reduce this but do not remove it.
- Only sessions in the `Live` and `Shadow` validation arms produce labels. A project without validation enrollment learns corrections for latency and cache, and none for quality.
- The fold trusts the content gaps that the validator records.
- A steer after a negative review changes later intervals. It does not change the label of the reviewed interval.
- Consistent-trajectory credit loses failover intervals. It gives no evidence to strategies that disagree anywhere in an interval.
- No live provider, Redis outage, crash, or deployment evidence exists for the learner. Roundhouse makes no claim about routing quality, savings, or latency from it.
- A recipe with local targets reports cost `unpriced` in the promotion gate until the log records a local capacity price. Judge dollars are unpriced, because side calls record no rate card.
- The current selector does not use the background classifier labels to learn a routing policy.

See [The routing learner](../concepts/routing-learner.md) for the design.
