# Glossary

This glossary defines the terms that the book uses across chapters, in alphabetical order. A term in *italics* has its own entry.

**Admin key.** A key of the form `rh_admin_<43 base62 characters>`. It works only on the *admin plane*. Every turn route refuses it with `wrong_key_kind`.

**Admin plane.** The routes under `/v1/admin`, the only routes that write tenancy: projects, users, memberships, and keys. They refuse every request in *Open mode*.

**Admission cache.** The compiled *control plane* that each node resolves keys against. A node refreshes it only after `admission_cache_ttl_ms` (default 30 seconds) has passed and the stored version has moved. A revocation therefore reaches another node within one TTL, or two after a failed refresh.

**Admission, policy.** The filter that removes the targets that a key's policy, quality floor, or credentials do not allow. A removed target is never added back.

**Arm.** One of three modes of the *validate/steer* loop, fixed when a session is created. `Live` takes the action. `Shadow` runs the *judge*, logs everything, and discards the action. `Placebo` runs no judge and intervenes on a fixed timing, as a control.

**Budget.** A dollar ceiling on a project and, optionally, on a member, kept as a ledger of *grants*. When it is spent, the turn *degrades to local* or is refused, as the budget's `on_exhaustion` says.

**Cache ledger.** The router's record of what it last sent to each hosted target, when, and under which cache model. No provider exposes its cache, so the ledger predicts whether the cache is warm.

**Cadence, frontier.** A limit of `max_frontier` hosted dispatches in the last `per_turns` turns of a session. When it is spent, hosted targets become inadmissible and the turn serves locally.

**Capability gate.** The check that lets two models be priced against each other only when their *quality priors* are within `capability_band` (default `0.10`). It stops a small local model from being priced against a flagship.

**Catalog.** The JSON file that `ROUNDHOUSE_CATALOG` names: the hosted models the router can choose, their prices, and their *providers*. See [Configure providers and the catalog](../guides/catalog.md).

**Chained topology.** A launch in which NeMo Relay runs between the agent and Roundhouse. In the direct topology, there is no Relay in between.

**Configured mode.** A deployment that has a *control plane* file. Every surface demands a key.

**Control call.** A call that an agent makes to one of the eight tools of the *MCP* control surface, such as `status` or `prefer`.

**Control plane.** Who is asking, what they can be routed to, and what it can cost: *projects*, *users*, *memberships*, and keys. `ROUNDHOUSE_CONTROL_PLANE` names the file, and the *admin plane* changes it at runtime.

**Conversation.** The thread of turns that a client names, for example by `thread-id`, `session-id`, or *prompt cache key*. Roundhouse maps a conversation to a *session*.

**Correlator.** A value on an MCP call that names the *conversation* the call is about, such as a Codex `threadId` or a Claude Code `toolUseId`. With none, Roundhouse guesses the caller's most recent conversation.

**Degrade to local.** Serve a local candidate instead of failing. A turn does this when its *cadence* is spent, or when its *budget* is spent under `on_exhaustion: "degrade_to_local"`.

**Dialect.** The wire format of a provider endpoint, named `wire_protocol` in the catalog: `openai_responses`, `anthropic_messages`, or `openai_chat_completions`. Only the first two have a client.

**Dynamo.** NVIDIA's inference serving system. Roundhouse pins Dynamo crates for the *selection service* and token-block hashing and runs them in its own process.

**Echo stub.** The offline answerer that serves every turn when no upstream is configured. Every price is zero, and no request leaves the process.

**Event log.** The append-only list of events of one *session*, ordered by *seq*. Conversation items, routing decisions, and metrics are *projections* of it.

**Fair-use window.** A limit on the tokens or dollars of a project or member in a rolling `5h`, `24h`, or `7d` window. A turn over it gets HTTP 429 `fair_use_exceeded` with `resets_at`.

**Generation.** A run of a *conversation* over one stored prefix. A history rewrite starts a new generation: a new session with a cold *cache ledger*.

**Grant.** A hold on a *budget* for one turn. It reserves the smallest of the requested amount and the project's and member's remaining amounts, and settles once when the turn ends. A hold whose turn died lapses after its TTL.

**Judge.** The model that reviews an agent's recent steps in the *validate/steer* loop, on the catalog entry that `ROUNDHOUSE_JUDGE_MODEL` names.

**Learner.** The optional component that chooses a serving strategy per turn from logged reviews. See [The routing learner](../concepts/routing-learner.md).

**Lease.** The proof that one node is the single writer for a *session*. Every append needs one.

**Local fleet.** The Dynamo workers that Roundhouse quotes and serves locally. The shipped `roundhouse` binary attaches none.

**MCP.** The Model Context Protocol. Roundhouse mounts a control surface at `/mcp` with eight tools that let an agent read its routing state and narrow it.

**Membership.** The link between a *user* and a *project*. Keys belong to memberships, and a turn is attributed to one as a *principal*.

**NeMo Relay.** The agent harness that Roundhouse works with. Roundhouse emits Relay's formats from its log, and a launch can chain the agent through Relay.

**Open mode.** A deployment with no *control plane* file. Every request is the `default/default` principal, and no key is required.

**Overlay.** A change that an agent makes to its own routing through an MCP tool, such as `prefer` or `set_quality_floor`. Like every policy change, it can only narrow. A request for more is clamped and reported.

**Pass-through auth.** An agent keeps its own credentials, and Roundhouse uses them. A forwarded login, such as a ChatGPT or Claude subscription token, arrives with each turn, goes to the provider, and is never stored.

**Prefix admission.** Comparing the history that a client resends with the stored prefix, and admitting only the new suffix. A rewrite starts a new *generation*.

**Principal.** The pair `project/user` that a turn is attributed to.

**Project.** The unit that has a *budget* and a policy. *Users* join it through *memberships*.

**Projection.** A value computed from the *event log* and not stored on its own, also called a fold. A fold of the stored events gives the same numbers as the running process.

**Prompt cache key.** The `prompt_cache_key` field, which steers a request to a provider cache node. Roundhouse always sends one. On the Responses surface it can also name a *conversation*.

**Provider.** An entry in the catalog's `providers` section: a base URL, a route per *dialect*, a key variable, and optional headers. Roundhouse builds one client per provider.

**Quality prior.** A number from `0.0` to `1.0` that states a model's relative capability. It is configuration, not measurement.

**Redis namespace.** The prefix of every shared Redis key, set by `ROUNDHOUSE_REDIS_NAMESPACE`. The default is `rh`.

**Residency call.** The realtime call that asks Dynamo how much of a prompt a worker already holds. The router makes it only when the answer can change the decision.

**Selection service.** Dynamo's `SelectionService`, which answers which worker holds a prefix. Roundhouse embeds it, so a query has no network round trip and sends block hashes, never token ids.

**seq.** The sequence number of an event in a *session*'s log, increasing by one per event. On Redis, the stream entry id is the seq.

**Session.** The durable state of one *conversation*: an *event log* with one writer at a time. Its id is `{project}/{user}/{name}`, and a session read checks that the prefix is the caller's.

**Steer.** A correction that Roundhouse gives an agent after a negative review, as text in the answer of the steered turn. The next resend admits it as prefix, which is how Roundhouse knows it arrived.

**Switchyard.** A routing project that Roundhouse adopts ideas and judge prompts from.

**Tier.** One of two ordered target lists in a project recipe, `capable` and `efficient`. The first admitted entry of the chosen tier serves the turn, and the rest are its fallbacks.

**Topham.** The operator entry point. It mints keys and turns a saved profile, a TOML file under `$XDG_CONFIG_HOME/topham/profiles/` that holds no secret, into a running agent. See [Launch with topham](../guides/topham.md).

**Turn key.** A key of the form `rh_turn_<43 base62 characters>` that authorizes turns for one *membership*. The control-plane file holds only its SHA-256 hash.

**User.** An identity in the *control plane*. A user joins a *project* through a *membership*.

**Validate/steer.** The loop that watches a session and interposes when the agent seems to go the wrong way. A *judge* reviews recent steps, and a *steer* delivers the correction. It is off unless a project enrolls.
