# Glossary

This glossary defines the terms that the book uses. The terms are in alphabetical order. A term in *italics* in a definition has its own entry.

**Admin key.** A key of the form `rh_admin_<43 base62 characters>`. It administers the *control plane* through the *admin plane*. Every turn route refuses it with `wrong_key_kind`.

**Admin plane.** The routes under `/v1/admin`. They are the only routes that write tenancy: projects, users, memberships, and keys. They refuse Open mode.

**Admission cache.** The cache that each surface uses to resolve a key against the *control directory*. Its default lifetime is 30 seconds, so a revoked key stops working within that time, not at once.

**Admission, policy.** The filter that removes the targets that a key's policy, quality floor, or credentials do not allow. A target that admission removes is never added back.

**Arm.** One of three modes of the *validate/steer* loop, fixed when a session is created. `Live` takes the action. `Shadow` runs the judge, logs everything, and discards the action. `Placebo` runs no judge and intervenes on a fixed timing, as a control.

**ATIF.** NeMo Relay's format for a finished agent trajectory. Roundhouse emits ATIF v1.7 from the *event log* at `GET /v1/sessions/{id}/trajectory`.

**ATOF.** NeMo Relay's format for a stream of agent events. Roundhouse emits it as NDJSON at `GET /v1/sessions/{id}/atof`.

**Budget.** A dollar ceiling on a project and, optionally, on a member. It works as a *grant* ledger, not as a counter. A spent budget *degrades to local* and does not fail the turn.

**Cache ledger.** The router's record of what it last sent to each target, when, and under which cache model. It predicts whether a hosted target's cache is warm.

**Cache model.** The rule that predicts how a provider's prompt cache expires. The catalog names one for each entry: `inactivity_decay`, `deterministic`, or `observed`.

**Cadence, frontier.** A limit on how many hosted dispatches a project can make in a trailing window of turns. When the window is spent, hosted targets become inadmissible and the turn serves locally. A cadence counts dispatches, not turns.

**Capability band.** The largest difference between two *quality priors* that still lets the *capability gate* compare the two models. The default is `0.10`.

**Capability gate.** The check that decides whether two models can be priced against each other. It compares their *quality priors* within the *capability band*. It stops a small local model from being priced against a flagship.

**Catalog.** The JSON file that `ROUNDHOUSE_CATALOG` names. It lists the hosted models the router can choose, their prices, and the *providers* where they live. See [Configure providers and the catalog](../guides/catalog.md).

**Chained topology.** A launch in which NeMo Relay runs between the agent and Roundhouse. The other topology is the *direct topology*.

**Classification.** An optional background call that labels a turn by intent, complexity, dependence on context, and the tier of work it needs. A label describes the turn. It does not measure answer quality, and the selector does not learn from it.

**Configured mode.** A deployment that has a *control plane* file. Every surface demands a key. The other mode is *Open mode*.

**Control call.** A call that an agent makes to one of the eight tools of the *MCP* control surface, such as `status` or `prefer`. The *validate/steer* triggers do not count control calls as work on the task.

**Control directory.** The object that every surface holds in place of a compiled *control plane*. It re-resolves the plane on each request, so an admin write takes effect on the next admission.

**Control plane.** The configuration of who is asking, what they can be routed to, and what it can cost. It has *projects*, *users*, *memberships*, and keys. The file is `ROUNDHOUSE_CONTROL_PLANE`.

**Conversation.** The thread of turns that a client names, for example by `thread-id`, `session-id`, or *prompt cache key* on the Responses surface. Roundhouse maps a conversation to a *session*.

**Correlary.** A declared equivalence between one of your local models and a hosted model. The dashboard uses it to price a saving. Its `note` is shown on the dashboard.

**Correlator.** A value that a client attaches to an MCP call so that Roundhouse can find the *conversation* that the call is about. Examples are a Codex `threadId` and a Claude Code `toolUseId`. With no correlator, Roundhouse guesses the most recent conversation.

**Decision record.** The event that Roundhouse writes to the log before a dispatch. It holds the winner, the losers, the policy, the rationale, and the budget state.

**Declared baseline.** The `model` that a client wrote in its request. Roundhouse records it and never routes on it. The dashboard prices a counterfactual against it, through the *capability gate*.

**Degrade to local.** What a turn does when its budget or its *cadence* is spent: it serves the local candidate and does not fail.

**Dialect.** The wire format that a provider endpoint speaks. The catalog calls it `wire_protocol`. The values are `openai_responses`, `anthropic_messages`, and `openai_chat_completions`. Only the first two have a client.

**Direct topology.** A launch in which the agent talks to Roundhouse without NeMo Relay in between. See *chained topology*.

**Dynamo.** NVIDIA's inference serving system. Roundhouse sits in front of it. Roundhouse pins two Dynamo crates, the *selection service* and token-block hashing, and builds on its own.

**Echo stub.** The offline answerer that serves every turn when no upstream is configured. Every price is zero. It exists so that the demo runs without a provider.

**Effective prefill tokens.** The number that the *selection service* returns for a worker. It is the prefill cost of the prompt after the scheduler credits the cache that the worker already holds.

**Event log.** The append-only list of events of one *session*, ordered by *seq*. The conversation items, the routing ledger, and the metrics are all *projections* of it.

**Fair-use window.** A limit on the tokens or dollars that a project or a member can use in a rolling `5h`, `24h`, or `7d` window. A turn over the limit gets HTTP 429 `fair_use_exceeded` with a `resets_at` time.

**Fencing token.** A unique value that the store mints each time a node acquires a *lease*. It makes the handles of an earlier tenure invalid, so an old owner cannot write behind its successor.

**Fold.** See *projection*.

**Forwarded login.** An authentication mode in which the caller's own subscription token, such as a ChatGPT or Claude login, arrives with each turn and is forwarded to the provider. Roundhouse never stores it.

**Frontier provider.** A hosted model endpoint that Roundhouse dispatches to, such as OpenAI, Anthropic, or OpenRouter. No provider exposes its cache, so the *cache ledger* models it.

**Generation.** A run of a *conversation* that shares one stored prefix. A history rewrite starts a new generation, which is a new session with a cold cache ledger.

**Grant.** A hold on a budget for one turn. It reserves the smaller of the requested amount, the project's remaining amount, and the member's remaining amount. It settles once when the turn ends. A hold whose turn died lapses after its TTL.

**Judge.** The model that reviews an agent's recent steps in the *validate/steer* loop. It runs on the catalog entry that `ROUNDHOUSE_JUDGE_MODEL` names. A judge that cannot be reached releases the turn.

**KV event.** A message that a Dynamo worker publishes when its KV cache changes. The embedded *selection service* uses these messages to know which prefix each worker holds.

**Learner.** The optional online component that chooses a serving strategy for each turn from logged reviews. See [The routing learner](../concepts/routing-learner.md).

**Lease.** The proof that one node is the single writer for a *session*. Every append needs one. A node whose lease expired fails its next append.

**Local fleet.** The set of Dynamo workers that Roundhouse quotes and serves locally. The shipped binary attaches none.

**MCP.** The Model Context Protocol. Roundhouse mounts a control surface at `/mcp` with eight tools that let an agent read its routing state and narrow it.

**Membership.** The link between a *user* and a *project*. It is the unit that a turn is attributed to, as a *principal*. Keys belong to memberships.

**Namespace, Redis.** The prefix that every shared Redis key is built under. `ROUNDHOUSE_REDIS_NAMESPACE` sets it. The default is `rh`.

**Namespace, session.** The prefix `{project}/{user}/` of every session id. The namespace check is what authorizes the session reads.

**NeMo Relay.** The harness that Roundhouse works with. Roundhouse emits Relay's formats from its log, and a launch can chain the agent through Relay.

**Open mode.** A deployment that has no *control plane* file. Every request is the `default/default` principal, and no key is required.

**Overlay.** A change that an agent can make to its own routing through an MCP tool, such as `prefer` or `set_quality_floor`. An overlay can only narrow, and a request for more is clamped and reported.

**Pass-through auth.** The design in which an agent keeps its own credentials and Roundhouse uses or forwards them. It lets an unmodified agent hook up without a change to its stack.

**Picker.** The part of a tier recipe that chooses between the capable and the efficient tier. `efficient_first` is the default.

**Policy narrowing.** The only way to combine policies. A key's overrides and an overlay can make a project's policy tighter, and never looser. An override that is wider than its project is refused at load.

**Prefix admission.** The step that compares the history that a client resends with the stored prefix and admits only the new suffix. A history rewrite starts a new *generation*.

**Principal.** The pair `project/user` that a turn is attributed to. A *membership* names one.

**Project.** The entity that has a budget and a policy. *Users* join it through *memberships*.

**Projection.** A value that is computed from the *event log* and not stored on its own. Conversation items, the routing ledger, and the metrics are projections. A fold of the stored events gives the same numbers as the running process.

**Prompt cache key.** A field that steers a request to a provider cache node. Roundhouse always sends one. On the Responses surface it is also one of the names that identify a *conversation*.

**Provider.** An entry in the catalog's `providers` section: a base URL, a route for each *dialect*, a key variable, and optional headers. Roundhouse builds one client for each provider.

**Quality prior.** A number from `0.0` to `1.0` that states the relative capability of a model. It is configuration, not measurement. You can source it with `import-benchmarks`.

**Rate card.** The prices of a model. Roundhouse keeps them in the *catalog*, never in source code.

**Re-discovery tax.** The cost of processing a prompt prefix again. Tax A occurs within a session as the resent prefix grows. Tax B occurs across sessions that send the same prefix. See [Measure cache reuse](../guides/cache-reuse.md).

**Reservation.** A booking of capacity on a local worker. The *select/reserve* split creates it only if the local option wins. A reservation must be released, or it inflates the router's view of the worker.

**Residency check.** The realtime call to Dynamo that asks how much of a prompt a worker already holds. The router makes it only when the answer can change the decision.

**Routing ledger.** The record of routing decisions in the *event log*. It is a *projection*.

**Select/reserve.** The split between pricing a local option without booking it (`select`) and booking it (`reserve`). It lets the router compare a local worker with a hosted model and book only if local wins. An abandoned quote costs nothing.

**Selection service.** Dynamo's `SelectionService`. It answers which worker holds a prefix. Roundhouse embeds it in the process, so a query needs no network round trip and sends block hashes, never token ids.

**seq.** The sequence number of an event in a *session*'s log. It increases by one for each event. On Redis, the stream entry id is the seq.

**Session.** The durable state of one *conversation*: an *event log* with one writer at a time. Its id is `{project}/{user}/{name}`.

**Steer.** A correction that Roundhouse gives to an agent after a negative review. It is delivered as text, in the answer of the steered turn. The next resend of the history admits it as prefix. That is also how Roundhouse knows the steer was fulfilled.

**Switchyard.** A routing project. Roundhouse adopts ideas and judge prompts from it, such as the coding-agent scorer and two of the validate signals.

**Tier.** One of two ordered lists of targets in a project recipe: `capable` and `efficient`. The first admitted entry of the chosen tier serves the turn, and the rest of that tier are its ordered fallbacks.

**Topham.** The operator entry point. It turns a saved *topham profile* into a running agent, and it mints keys. See [Launch with topham](../guides/topham.md).

**Topham profile.** A TOML file under `$XDG_CONFIG_HOME/topham/profiles/` that names an agent, a deployment root, an auth kind, the variable that holds the turn key, and a topology. It never holds a secret.

**Trigger.** The condition that starts a validation. It is a budget gate together with a signal. The gate is a projection of the log. The six signals are `NoProgressRepeat`, `PingPong`, `ToolFailureStreak`, `CostAnomaly`, `ErrorSeverity`, and `PureBashStreak`.

**Turn key.** A key of the form `rh_turn_<43 base62 characters>`. It authorizes turns for one *membership*. The control-plane file holds only its SHA-256 hash.

**User.** An identity in the *control plane*. A user joins a *project* through a *membership*.

**Validate/steer.** The loop that watches a session and interposes when an agent seems to go the wrong way. A *judge* reviews the recent steps, and a *steer* delivers the correction. It is off unless a project turns it on.

**Wire protocol.** See *dialect*.
