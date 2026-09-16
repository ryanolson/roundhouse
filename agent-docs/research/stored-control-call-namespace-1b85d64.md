# The stored control-call namespace, and what a migration touches

*A read of roundhouse at `1b85d64`, codex at `6344a65`, and the byte-exact
Claude Code 2.1.257 fixtures under
`crates/roundhouse-server/tests/fixtures/`. Dated 2026-09-03.*

Evidence for D2's second question: **does the stored namespace change, and how
do existing logs migrate?** Every claim below carries a `file:line` at those
pins. Negatives name what was searched.

---

## 1. What the log stores for a control call, on each surface

### 1.1 The durable shape

`ItemContent::ToolCall` has exactly three fields, and none of them is a
namespace:

```
crates/roundhouse-core/src/item.rs:61-65
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
    },
```

It is serialized internally tagged (`#[serde(tag = "type", rename_all =
"snake_case")]`, `item.rs:55-57`) and reaches the store inside
`SessionEventKind::ItemAppended { item }`
(`crates/roundhouse-core/src/event.rs:275-277`).

### 1.2 The Responses path stores the bare name; the namespace is dropped at
canonicalization

`canonical_item`'s `function_call` arm reads three fields and never looks at
`namespace`:

```
crates/roundhouse-server/src/responses_api/wire.rs:94-102
    "function_call" => Ok(Some(Item {
        role: Role::Assistant,
        content: ItemContent::ToolCall {
            call_id: required_str(value, "call_id")?,
            name: required_str(value, "name")?,
            arguments: required_str(value, "arguments")?,
        },
        response_id: None,
    })),
```

**This is the substitution site.** There is no rewriting step anywhere else:
the name that lands in the log is `required_str(value, "name")` verbatim, and
the `namespace` sibling field is simply not read. The property is asserted at
`responses_api/wire.rs:577-598`
(`a_clients_namespaced_call_canonicalizes_to_the_bare_stored_item`) and again
at `:629-666`, which additionally proves a *flat* `name` is kept verbatim — so
the two spellings do **not** converge on one canonical item.

The wire fact it is built on is pinned against codex's own type:

```
codex-rs/protocol/src/models.rs:910-921 @ 6344a65
    FunctionCall {
        id: Option<ResponseItemId>,          // skip_serializing_if = "Option::is_none"
        name: String,
        namespace: Option<String>,           // skip_serializing_if = "Option::is_none"
        arguments: String,
        ...
```

and re-derived from the encoder in this tree's own conformance suite —
`crates/roundhouse-server/tests/codex_wire_shapes.rs:176-215` asserts the whole
`Value` equals `{"type":"function_call","name":"grep","namespace":
"mcp__roundhouse","arguments":…,"call_id":"call_theirs"}`, i.e. two wire
fields, not one flat string.

So the Responses log holds `status` (`dialect.rs:140`,
`ClientDialect::CodexResponses => tool.to_string()`).

### 1.3 The Messages path stores the flat name the client spells

`block_item`'s `ToolUse` arm carries the client's `name` through untouched:

```
crates/roundhouse-server/src/messages_api/wire.rs:632-656
    ContentBlock::ToolUse { id, name, input, .. } => (
        Role::Assistant,
        ItemContent::ToolCall { call_id: id, name, arguments: input.to_string() },
    ),
```

Claude Code folds the registration into every tool name it declares, calls and
permits, so what arrives (and is stored) is `mcp__roundhouse__status`. The
byte-exact fixtures hold it:
`tests/fixtures/claude-2.1.257-mcp-turn-1.json:722,739` and
`claude-2.1.257-mcp-turn-2-toolresult.json:27,752,769`. The end-to-end
assertion that the log holds exactly that is
`tests/messages_api_surface.rs:4703-4719`.

`ClientDialect::stored_call_name` is the *statement* of the two, and it is the
one renderer of the flat spelling:

```
crates/roundhouse-server/src/dialect.rs:136-147
    Self::CodexResponses  => tool.to_string(),
    Self::ClaudeMessages  => roundhouse_core::validate::flat_control_call_name(tool),
```

`flat_control_call_name` is `crates/roundhouse-core/src/validate/control_call.rs:93-95`.

### 1.4 The surface is recovered from the session key, not from the record

`ControlCallDialect::of_session_key` (`control_call.rs:123-132`) splits the key
on `/` and looks for the segment `anthropic_messages`
(`MESSAGES_SESSION_SEGMENT`, `control_call.rs:157`). The engine reads it at two
places and hands it to the folds:
`crates/roundhouse-server/src/engine.rs:1234` (validate evidence) and
`engine.rs:1991` (tier-selection signals).

**This is the only surviving trace of the client's identity below the wire
modules.** The stored record itself carries nothing that says which surface
wrote it.

---

## 2. Every consumer that reads the stored name

Enumerated by `rg` over `-g '*.rs' -g '!target'` for `ItemContent::ToolCall`,
`Exchange.name`, `is_control_call_on`, `task_exchanges_on`, and
`flat_control_call_name`. Non-test sites only:

| Consumer | Site | What it does with the name |
|---|---|---|
| the recognizer | `validate/control_call.rs:135-150` | `ClaudeMessages` → `is_flat_control_call` (exact prefix+delimiter, `:178-182`); `CodexResponses` → `CONTROL_TOOL_NAMES.contains(&name)` (`:148`) |
| the task view | `validate/control_call.rs:200-205` | filters control calls out of the exchange list |
| the exchange projection | `validate/exchange.rs:96-107` | copies `name` onto `Exchange` |
| trigger evidence | `validate/trigger.rs:178` | `task_exchanges_on(&self.exchanges, self.dialect)` — every signal's input |
| `NoProgressRepeat` | `validate/trigger.rs:228,237` | compares `call.name == latest.name`, and interpolates the name into the finding text |
| `PingPong` | `validate/trigger.rs:264` | `.map(\|call\| call.name.as_str())` for the alternation check |
| `ToolSignals` | `validate/tool_signals.rs:345,367` | filters via `task_exchanges_on`, then `classify_tool_call(&exchange.name, …)` |
| `recent_severities` [fact-check 2026-09-03: the draft labelled this row `severities_of`; the span at `:413-416` is `recent_severities`, which filters via `task_exchanges_on` and then delegates to `severities_of` (defined at `:423`, over an already-filtered slice); the citation and the point stand, the caller's name is corrected] | `validate/tool_signals.rs:413-416` | filters via `task_exchanges_on` |
| the judge brief | `validate/brief.rs:329` and `:253` | `name: call.name.clone()`, rendered into the prompt as `"{index}. {name} args#{hash}"` |
| tier selection | `routing/stage.rs:223-228` | `ToolSignals::from_exchanges(exchanges, dialect)` |
| Responses outbound projection | `responses_api.rs:998-1009` → `responses_api/wire.rs:307-315` | emits the stored `name` back on the wire in `function_call_item` |
| Messages outbound projection | `messages_api/emit.rs:623-631,761-766` | emits the stored `name` as `tool_use.name` |
| ATOF export | `roundhouse-relay/src/atof.rs:311-318` | `"function": { "name": name, … }` |
| ATIF export | `roundhouse-relay/src/atif.rs:506-528` | `function_name: name.clone()` |
| prefix admission | `prefix_admission.rs:805-807` | `stored.content == claimed.content` — structural equality, so the name is compared |
| the turn id | `item.rs:186-190` → `responses_api/wire.rs:199-211` | the name is inside `Item::render`, which is the FNV-1a input |

### 2.1 What nothing does — the negatives

- **The metrics fold never reads a tool name.** `SessionEventKind::ItemAppended`
  sits in the explicit no-op arm at
  `crates/roundhouse-core/src/metrics/fold.rs:816-820`; the fold's other arms
  read `SessionCreated`, `TurnStarted`, `Routed`, `ResponseCompleted`,
  `ResponseIncomplete`, `SideCall*` and `ValidationDecided` only
  (`fold.rs:560,588,605,661,664,753,772,780`). Searched: `rg -n
  "SessionEventKind::" crates/roundhouse-core/src/metrics/fold.rs` and `rg -n
  "ItemContent::ToolCall" crates/roundhouse-core/src/metrics/`.
- **Nothing stores the MCP namespace anywhere.** `rg -n "namespace"` over
  `crates/roundhouse-server/src/` and `crates/roundhouse-core/src/` returns:
  `responses_api/wire.rs` (tests and docs only), `dialect.rs` (the constant),
  `claude_launch/control_surface.rs` (the registration generator),
  `messages_api.rs:480` (a comment), and `shared_backend.rs` /
  `store-redis/src/keys.rs` — which is `KeyNamespace`, the Redis *deployment*
  namespace, an unrelated concept. No event, item or store field holds an MCP
  namespace.
- **Nothing splits a flat stored name back into (namespace, tool).**
  `is_flat_control_call` (`control_call.rs:178-182`) strips the prefix and
  discards the remainder; `mcp_server_name()` (`dialect.rs:75-79`) strips
  `mcp__` off the *constant*, not off a stored name. Searched: `rg -n
  "strip_prefix\|split" ` over `control_call.rs` and `dialect.rs`.
- **No admin, metrics, conversations or relay-summary surface reads a tool
  name.** `rg -n "ToolCall|tool_call"` over
  `crates/roundhouse-server/src/{conversations.rs,admin_api.rs,metrics_api.rs,relay_api.rs}`
  returns nothing; `crates/roundhouse-relay/src/summary.rs` likewise.
- **`classify_tool_call` cannot be reached by a control call on either
  surface today**, because `task_exchanges_on` filters them first
  (`tool_signals.rs:345` before `:367`). Its name lists
  (`tool_signals.rs:181-248`, `EDIT_TOOL_NAMES` at `:181` through `BASH_TOOL_NAMES` at `:242-248`) are exact lowercased matches on *client* tools
  (`bash`, `edit`, `read`, `update_plan`, …), none of which is ever
  namespaced on either wire, so a namespace change cannot move a category.
- **No codex builtin tool is named any of the eight.** Searched
  `rg -no 'name: "[a-z_]+"' codex-rs/core/src/` @ `6344a65` and matched the
  full result set against `CONTROL_TOOL_NAMES`: no hit. The only `"status"`
  literals in `codex-rs/core/src/tools/` are JSON *property* names
  (`handlers/plan_spec.rs:14`, `handlers/multi_agents_spec.rs:488-509`), not
  tool names.

---

## 3. Turn ids and dedup keys: what moves, and what does not

### 3.1 The turn id is a hash of the render, and the render contains the name

```
crates/roundhouse-core/src/item.rs:186-190
    ItemContent::ToolCall { call_id, name, arguments } =>
        format!("<tool_call id=\"{call_id}\" name=\"{name}\">{arguments}</tool_call>"),
crates/roundhouse-core/src/item.rs:341-343
    pub fn render(&self) -> String { format!("<|{}|>{}", self.role.as_str(), self.content.render()) }
```

```
crates/roundhouse-server/src/responses_api/wire.rs:199-211
    pub(crate) fn turn_id_for(items: &[Item]) -> TurnId {
        let mut hash = FNV_OFFSET;
        for item in items { for byte in item.render().bytes() { … } }
        TurnId::new(format!("turn_{hash:016x}"))
    }
```

The Messages surface delegates to the same function
(`messages_api/wire.rs:492-494`), deliberately, so the two dialects cannot
disagree about a conversation's identity.

**So renaming the stored name moves the turn id of any conversation containing
a control call, and only those.** A conversation with no control call is
untouched.

### 3.2 The turn id is computed from the *claim*, not from the log

```
crates/roundhouse-server/src/responses_api.rs:337-338
    let claimed = canonicalize(&request.instructions, &request.input)?;
    let turn_id = turn_id_for(&claimed);
```

### 3.3 The dedup key is the turn id

```
crates/roundhouse-core/src/session.rs:1304-1316
    pub async fn begin_turn(&mut self, turn_id: TurnId, input: Vec<Item>) -> … {
        if let Some(existing) = self.state.completed_response_for(&turn_id).cloned() {
            self.commit(vec![SessionEventKind::TurnDeduplicated { turn_id, response_id: existing.clone() }]).await?;
            return Ok(TurnAdmission::Deduplicated(existing));
        }
```

`completed_response_for` matches against `TurnStarted`/`ResponseCompleted`
pairs already in the log. So a canonicalization change that moves a turn id
makes an in-flight retry miss its own completed response: **a second answer,
generated and billed, for a question already answered** — the exact failure
`turn_id_for`'s own doc (`responses_api/wire.rs:191-198`) exists to prevent.

### 3.4 Prefix admission compares the name structurally

```
crates/roundhouse-server/src/prefix_admission.rs:805-807
    fn same_item(stored: &Item, claimed: &Item) -> bool {
        stored.role == claimed.role && stored.content == claimed.content
    }
```

`same_item` is already stamp-blind (it ignores `response_id`,
`prefix_admission.rs:798-804`) but it is **not** field-blind: `content` uses
the derived `PartialEq` on `ItemContent`. Any change to what canonicalization
puts in `name` — or any new field on `ToolCall` — makes a pre-change stored
item disagree with a post-change claim, and `suffix_after`
(`prefix_admission.rs:789-796`) returns `None`, which forks the conversation
into a new generation rather than continuing it.

### 3.5 The pinned turn-id literal does *not* move for a namespace change

```
crates/roundhouse-server/src/responses_api/wire.rs:830-843
    fn the_turn_id_of_a_fixed_conversation_is_pinned() { … assert_eq!(turn_id_for(&claimed).to_string(), "turn_6a7aaa94e5b59fd2"); }
```

The fixture's tool is `"search"`, an ordinary client tool, so a change confined
to roundhouse's own control names leaves this literal green. **That is a hazard
rather than a comfort**: the guard the tree wrote to catch "an edit that moves
historical hashes" is blind to exactly this edit, and a new pinned literal over
a control-call conversation would have to be added for the migration to be
guarded at all.

---

## 4. The exemption R-M1 pinned, and how often its shape can occur

The bare arm's price is asserted as a fact about the trade:

```
crates/roundhouse-core/src/validate/control_call.rs:419-442
    fn a_third_partys_bare_status_tool_is_exempted_with_ours_on_the_responses_wire_only()
```

with the comment naming it as the test to delete the day the log keeps a
namespace (`control_call.rs:414-417`). The reasoning is at
`control_call.rs:139-149`: the under-count of a call or two replaces G04's
over-count of all roundhouse's chatter, which fired steers at an agent that had
done nothing wrong.

**Three distinct shapes can produce a bare name colliding with one of the
eight on the Responses wire**, and they are not equally likely:

1. **Another MCP server's tool named e.g. `status`.** Arrives as
   `{"name":"status","namespace":"mcp__other"}` and canonicalizes to `status`
   (`responses_api/wire.rs:94-102`). Indistinguishable from ours today.
   *Keeping the namespace resolves this.*
2. **A plain (non-MCP) function tool named `status`.** `namespace` is
   `Option<String>` with `skip_serializing_if = "Option::is_none"`
   (`codex-rs/protocol/src/models.rs:915-917` @ `6344a65`), so a non-namespaced
   function tool sends **no** `namespace` field at all and stores as `status`.
   *Keeping the namespace also resolves this*, because ours would then store
   `mcp__roundhouse__status` and theirs would stay `status`.
3. **A codex builtin.** Does not occur: see §2.1's negative — none of codex's
   own tool names at `6344a65` is one of the eight.

The exposure is therefore bounded by what an operator's *own* extra MCP servers
and tool declarations happen to be named. The eight names —
`status`, `init_session`, `declare_intent`, `prefer`, `set_quality_floor`,
`fetch_steer`, `report_outcome`, `explain_last_route`
(`control_call.rs:73-82`) — are generic English, and `status` in particular is
a common MCP tool name; the other seven are distinctive enough that a collision
would be a coincidence. **One of eight is the realistic exposure, not eight of
eight.**

`roundhouse_mcp::tools::TOOL_NAMES` re-exports the same list
(`crates/roundhouse-mcp/src/tools.rs:97`), so the surface and the fold cannot
disagree about what "one of ours" means.

---

## 5. What a stored-namespace change would touch

### 5.1 The memory store — nothing durable

```
crates/roundhouse-core/src/store.rs:191-193
    pub struct MemoryStore { sessions: Arc<RwLock<HashMap<SessionId, SessionRecord>>> }
```

An in-process `HashMap`. It holds `SessionEventKind` values, not serialized
bytes, and does not survive a restart. **A stored-namespace change touches
nothing here that a process restart would not have destroyed anyway** — but
because the contract suite (`store/contract.rs`) judges both backends by one
identical suite (`store.rs:12-16`), any type-level change lands on both.

### 5.2 The Redis store's stream entries — append-only, and not rewritable in
place

Events are JSON-serialized whole and `XADD`ed under explicit ids:

```
crates/roundhouse-store-redis/src/lib.rs:438-444
    let payloads: Vec<String> = kinds.iter()
        .map(|kind| serde_json::to_string(kind).expect("event kinds are plain data and serialize"))
        .collect();
crates/roundhouse-store-redis/src/scripts.rs:92
    redis.call('XADD', KEYS[3], last .. '-0', 'at_ms', at_ms, 'kind', ARGV[i])
```

The append script derives the next seq from `XREVRANGE` and **aborts on any
entry id that is not `<seq>-0` shaped** (`scripts.rs:83-88`, returning
`CORRUPT`), which is documented as refusing to "launder" a foreign entry into a
log that otherwise proves its own integrity (`scripts.rs:72-75`).

**Consequence for migration: a Redis stream entry cannot be rewritten in
place.** `XADD` requires strictly increasing ids, so an edited entry cannot be
re-added under its own `<seq>-0`. A one-shot rewrite means reading the whole
log, writing a *new* stream key, and swapping it under the lease — with every
follower mid-projection and every node's `last_seq` cursor quiesced first.

The keys themselves carry a per-family version:

```
crates/roundhouse-store-redis/src/keys.rs:93-101
    pub(crate) fn version(self) -> &'static str { KeyFamily::Session => "v1", … }
crates/roundhouse-store-redis/src/keys.rs:207-231  build_key → "<ns>:<version>:<family>[:<part>]…"
```

So a *key-space* migration lever exists and is per family — `KeyFamily::Session
=> "v2"` would orphan nothing else. But the module doc records the state that
matters most for D2:

> **No migration.** … No deployment holds a pre-rule key of any of the three —
> **none has shipped yet** — so there is nothing to convert and no test that
> could prove a converter right.
> — `crates/roundhouse-store-redis/src/keys.rs:33-38`

### 5.3 Fixtures that pin bytes

- `tests/fixtures/claude-2.1.257-mcp-turn-1.json:722,739` and
  `claude-2.1.257-mcp-turn-2-toolresult.json:27,752,769` hold
  `mcp__roundhouse__declare_intent` / `mcp__roundhouse__status`. These are
  **captures of what the client sends**, so a change to the *Responses*
  canonicalization does not touch them; a change to the *Messages* stored
  spelling would invalidate the capture rather than the code.
- `tests/messages_api_surface.rs:4703,4717,4755` assert the stored flat name
  against those fixtures.
- `tests/codex_wire_shapes.rs:178-215,236,284,325` pin the codex-side
  namespaced `function_call` object field for field.
- `src/responses_api/wire.rs:842` pins `turn_6a7aaa94e5b59fd2` — see §3.5 for
  why it does *not* move here.
- `src/item.rs:427-443` (`a_pre_m11_log_record_still_deserializes`) pins the
  stored JSON of a `tool_call` record **byte for byte in both directions**:
  `{"role":"assistant","content":{"type":"tool_call","call_id":"c1","name":"grep","arguments":"{}"}}`.
  This is the test any record-shape change has to answer to.
- `crates/topham/src/plan/tests.rs:103,156,760` and
  `crates/topham/src/launch/tests.rs:331` carry the flat name in launcher
  prose/argv — these describe the *client's* spelling and are unaffected by a
  stored-record change.

---

## 6. The three migration shapes, and what each costs

### 6.1 A one-shot rewrite of stored logs

**Cost: high, and it buys nothing on the surface that needs it.** Redis stream
entries cannot be edited in place (§5.2); a rewrite is a read-all / write-new /
swap under lease. It would move the turn id of every rewritten conversation
containing a control call (§3.1), orphaning any in-flight retry (§3.3) and
making every already-issued `TurnId` in the log inconsistent with the items
that produced it.

**And it cannot recover the namespace.** The namespace was never written
(§2.1's second negative), so a rewrite could only *guess* which bare `status`
was ours — which is precisely the ambiguity the change exists to remove. A
rewrite is strictly worse than doing nothing.

### 6.2 A read-time canonicalisation

**Cost: near zero. Value: zero.** Mapping a stored `status` to
`mcp__roundhouse__status` at fold time adds no information the record did not
carry; it is today's `CONTROL_TOOL_NAMES.contains` recognizer
(`control_call.rs:148`) with an extra allocation. It moves no bytes, no turn id
and no key, and it resolves none of §4's three collision shapes.

### 6.3 A versioned record

**Cost: it breaks the additive discipline the log is built on.** A
`"type":"tool_call_v2"` tag is a new `ItemContent` variant; an older build
reading it hits an unknown variant on an internally-tagged enum and the
deserialization *fails*, so the log stops reading — the failure `item.rs:44-54`
names explicitly ("a log that no longer reads is not recoverable by a
rollback") and the reason the M11.1 widening was done as three new variants
rather than as changes to three existing ones. A version *field* rather than a
version tag is a different proposal, and is §7.

---

## 7. Carrying the namespace *beside* the name — a field, not a rename

Adding `namespace: Option<String>` to `ItemContent::ToolCall`, forward-only,
with `#[serde(default, skip_serializing_if = "Option::is_none")]`:

**What does not move.**

- **Turn ids do not move, if `render` leaves the field out.** The hash input is
  `Item::render` (`item.rs:341-343`) and the tool-call arm is one `format!`
  (`item.rs:186-190`). A field the render omits is a field the hash cannot see.
  The tree has already made this call deliberately in the other direction for
  `Thinking::signature` (`item.rs:200-215`: included *on purpose*, because
  excluding it would make two conversations hash to one turn id and buy a
  second billed answer). Here the collision risk is nil in practice — the
  `call_id` already distinguishes any two calls in one conversation — so
  excluding it is defensible, but it must be a stated decision with a comment,
  not a default.
- **No Redis key moves.** `KeyFamily::Session => "v1"` (`keys.rs:95`) stays;
  the change is inside the JSON payload, which the key format has no opinion
  about.
- **No existing stored bytes change.** With `skip_serializing_if`, a record
  with no namespace serializes exactly as it does today, so
  `a_pre_m11_log_record_still_deserializes` (`item.rs:427-443`) stays green in
  both directions for every pre-change record.
- **An older build reading a newer record degrades benignly.** serde ignores
  unknown fields by default, so an old binary reads a namespaced control call
  as a plain bare call — exactly the one-way-door argument already made for
  `SessionCreated::arm` (`event.rs:265-278`) and
  `SessionCreated::principal` (`event.rs:230-241`).

**What does move, and must be decided.**

- **Prefix admission.** `same_item` compares `content` structurally
  (`prefix_admission.rs:805-807`). A conversation whose first turn was stored
  before the change (`namespace: None`) and whose next claim canonicalizes with
  `namespace: Some("mcp__roundhouse")` **disagrees**, and forks. The remedy is
  the one the function already applies to `response_id`: make the comparison
  blind to the new field, or treat a stored `None` as agreeing with any claimed
  value. Either way it is a change to `same_item`, and it is the single
  load-bearing edit of this whole option.
- **The Responses outbound projection.** `function_call_item`
  (`responses_api/wire.rs:307-315`) emits `{"type","id","call_id","name",
  "arguments"}` and **no `namespace`**, and it is fed the stored name from the
  log at `responses_api.rs:998-1009`. Codex dispatches on an exact
  `ToolName { name, namespace }` lookup (`codex_wire_shapes.rs:203-212`
  citing `router.rs:164`, `registry.rs:440-444`), so a call re-emitted without
  a namespace resolves against nothing there. A carried field is the natural
  place to put it back. See §8.
- **The recognizer.** `ControlCallDialect::CodexResponses` could then match on
  the field instead of on `CONTROL_TOOL_NAMES.contains`
  (`control_call.rs:148`), which is what makes
  `a_third_partys_bare_status_tool_is_exempted_with_ours_on_the_responses_wire_only`
  (`control_call.rs:419`) the test to delete — but **only for records written
  after the change**. Records written before it stay ambiguous forever, so the
  bare-name arm cannot be removed; it becomes the fallback for a `None`
  namespace, and the exemption test narrows rather than disappears.
- **Construction sites.** `Item::tool_call` (`item.rs:325-338`) and the literal
  `ItemContent::ToolCall { … }` constructions: 22 `Item::tool_call` call sites
  across the workspace (`rg -c "Item::tool_call"`), four of them non-test
  (`responses_api/wire.rs` ×4, `messages_api/emit.rs` ×4,
  `messages_api/follower.rs`, `engine.rs`, `frontier.rs`, `relay/fixtures.rs`).
  A `..Default::default()`-free struct literal makes each a compile error,
  which is the wanted behaviour.

---

## 8. An open gap this read surfaced: the Responses outbound projection emits
no namespace

The Responses surface **does** render a stored tool call outbound, from the log:

```
crates/roundhouse-server/src/responses_api.rs:998-1009
    Some(Emitted::ToolCall { call_id, name, arguments }) => {
        let call = [ call_added_frame(call_id, name),
                     call_arguments_delta_frame(call_id, arguments),
                     call_done_frame(call_id, name, arguments) ];
crates/roundhouse-server/src/responses_api/wire.rs:307-315
    fn function_call_item(call_id: &str, name: &str, arguments: &str) -> Value {
        json!({ "type": "function_call", "id": call_id, "call_id": call_id,
                "name": name, "arguments": arguments })
    }
```

This contradicts `dialect.rs:25-27` ("**Nothing renders a tool call
outbound.**"), which is true of the `ClientDialect` *type* — nothing reads it
at run time — but is not true of the stored *name*, which is rendered outbound
on both surfaces.

The consequence, stated as a hypothesis because nothing in the tree tests it:
an upstream that asks for one of roundhouse's own MCP tools would have its
namespace dropped by the fleet decoder (`openai_responses/stream.rs:295-300`:
"`namespace` — the oracle's optional MCP qualifier — is deliberately not
read"), stored bare, and re-emitted to codex with no `namespace` field, which
codex's exact `ToolName { name, namespace }` lookup cannot resolve. The tree
says as much in the negative:

> what nothing in this tree has yet established is the exact wire shape of a
> namespaced `function_call` that codex 0.146.0 will route to MCP, and guessing
> it here would produce a test that is confidently wrong
> — `crates/roundhouse-server/tests/codex_e2e.rs:1552-1555`

So the round trip has never been closed on the Responses surface. **If the
namespace is carried as a field, it is available to `function_call_item` and
this gap closes as a side effect; if it is not, the gap stays open and is
unrelated to the exemption.** This is worth the orchestrator's attention
because it changes the value of the field option from "removes an under-count
of a call or two" to "also unblocks the one wire shape codex's MCP dispatch
needs".

---

## 9. A second gap the namespace question does *not* fix

`TurnSignals::turn_depth` is `exchanges.len()` — counted **before** control
calls are dropped:

```
crates/roundhouse-core/src/routing/stage.rs:226-228
    tools: ToolSignals::from_exchanges(exchanges, dialect),
    turn_depth: exchanges.len() as u32,
```

The cost is pinned as a live assertion at `routing/stage.rs:938-960`: five
uncategorised `cargo` calls are too shallow to be a stall, and three reads of
roundhouse's own `status` tool make the same session deep enough
(`turn_depth` 8, `spinning` 1.0). **No spelling of the stored name changes
this** — the depth counts every exchange whatever it is named — so the D2
ruling should not be read as closing it. The one-line remedy is named in the
same comment (`stage.rs:930-934`).

---

## 10. Summary of the load-bearing facts

1. The Responses log stores the bare name because `canonical_item`
   (`responses_api/wire.rs:94-102`) reads three fields and not `namespace`;
   the Messages log stores the flat name because `block_item`
   (`messages_api/wire.rs:632-656`) carries the client's `name` verbatim.
2. The turn id is FNV-1a over `Item::render`, and `name` is inside it
   (`item.rs:186-190`, `responses_api/wire.rs:199-211`); the dedup key *is*
   the turn id (`session.rs:1309`). A **rename** moves both. A **new field the
   render omits** moves neither.
3. Prefix admission's `same_item` (`prefix_admission.rs:805-807`) compares
   content structurally, so both a rename and a new field fork a conversation
   that straddles the change unless the comparison is made blind to it —
   which the same function already does for `response_id`.
4. A Redis stream entry cannot be rewritten in place (`scripts.rs:83-92`), and
   a rewrite could not recover a namespace that was never stored anyway.
5. Nothing has shipped: `store-redis/src/keys.rs:33-38` records "none has
   shipped yet — so there is nothing to convert and no test that could prove a
   converter right."
6. The exemption's realistic exposure is one name of eight (`status`), against
   a third party's MCP tool or a plain function tool, never a codex builtin.

---

## 11. A stale doc that would mislead the migration

`Item::tool_call`'s own doc still carries the claim the M12 review's F10
falsified:

```
crates/roundhouse-core/src/item.rs:320-323
    /// The name is the bare one. A namespace belongs to a client dialect and
    /// lives in the wire projection: canonicalization ignores it on the way
    /// in, so a namespaced resend and a flat one arrive as this same item, and
    /// the log keeps one spelling per tool.
```

Two of its four sentences are false at `1b85d64`:

- **"The name is the bare one"** — not on the Messages surface, where the log
  holds `mcp__roundhouse__status` (`messages_api/wire.rs:632-656`,
  `tests/messages_api_surface.rs:4717`).
- **"a namespaced resend and a flat one arrive as this same item"** — directly
  contradicted by
  `a_flat_spelling_is_a_different_canonical_call_until_the_wire_learns_to_split_it`
  (`responses_api/wire.rs:627-669`), whose `assert_ne!` at `:662-668` is the
  pinned divergence, and whose doc at `:600-612` names this exact sentence as
  "the corrected half of `dialect.rs`'s 'why that direction' argument … what
  was wrong was the reason, and a reason that does not hold is what gets a
  future change waved through."

The correction landed in `responses_api/wire.rs` and in `dialect.rs`
(`dialect.rs:11-14`, "Each wire module stores the name its own client sent,
verbatim") but not in `item.rs`, which is the doc a migration author reads
first because it sits on the constructor. Worth naming in the ruling: the
sentence being left in place is precisely the reasoning-by-stale-doc the F10
finding warned about.

Also stale for the same reason: `dialect.rs:25-27`'s "**Nothing renders a tool
call outbound.**" — true of the `ClientDialect` type, false of the stored name
(§8).

---

## Fact-check (2026-09-03)

An independent re-derivation of every negative and every high-stakes claim above, from the primary sources at the pinned revisions (roundhouse `1b85d64`, codex `6344a65` re-opened from the actual checkout, the 2.1.257 fixtures), by a second reader who did not write this document. Verdicts: 25 verified, 1 corrected, 0 unestablished.

Fact-checked D2 dive stored-namespace independently at roundhouse 1b85d64, codex 6344a65 (real checkout re-opened, not trusted from draft), and the 2.1.257 fixtures. All 8 negatives and all high-stakes claims verified against source, with file:line citations accurate (occasionally +/-1-2 lines from comment drift but always inside cited ranges). One minor labeling issue in the medium-stakes 16-site consumer table: tool_signals.rs:413-416 is attributed to "severities_of" but is actually recent_severities (which calls task_exchanges_on then delegates to severities_of); the line citation itself is correct, only the function name in the summary prose is one level removed. No claim was found to be wrong or unsupported. Full evidence at /tmp/claude-0/-home-user-roundhouse/d6addde3-2039-5f5e-8af5-d560d8c0b623/scratchpad/d2/stored-namespace-factcheck.md.

Corrections, each also applied above as a dated bracketed note:

- **Full consumer set of sixteen non-test sites reading the stored name** — Every cited site independently confirmed at its line except one labeling slip: tool_signals.rs:413-416 is attributed in the draft's table to a function named 'severities_of', but that exact span is recent_severities (lines 410-415), which calls task_exchanges_on and then delegates to severities_of (defined separately at line 423, which takes an already-filtered slice and does not itself call task_exchanges_on). The file:line citation is accurate and the substantive point (a site at that location filters via task_exchanges_on) is correct -- only the function name in the summary prose is one level removed from the actual caller.

Evidence file for the check: `scratchpad/d2/stored-namespace-factcheck.md` (session-local; the verdict table above is the durable record).
