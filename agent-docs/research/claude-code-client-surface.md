<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# DIVE C — Claude Code as the client: hook-up surface, subscription OAuth pass-through, and the `/v1/messages` contract a server must satisfy

> **Independently fact-checked 2026-08-27**: 15 load-bearing claims re-derived
> against the same bundle and freshly re-fetched docs by a separate agent; all
> 15 confirmed, with four additions folded into the body below (§1.5's two
> extra conditional betas, §3.2's `x-should-retry` exception, §3.3's fifth
> throw, §4.3's third session-id path). **One systematic citation caveat**: the
> `@<offset>` byte offsets paired with `cli.js:<line>` citations are unreliable
> — every checked *line number* was exact, but the offsets drift (consistent
> with counting UTF-16 code units rather than UTF-8 bytes over a bundle full of
> multi-byte literals). To re-derive a citation, search for the quoted snippet;
> do not seek to the offset.
>
> **Two `claude` installs exist on this box, and this document reads the older
> one.** `/opt/node22/bin/claude` is a symlink to `/opt/claude-code/bin/claude`,
> a native ELF binary reporting **2.1.247** — that is what an e2e suite here
> would spawn, and it is not readable. The npm install at
> `/opt/node22/lib/node_modules/@anthropic-ai/claude-code/` (v2.1.42, readable
> JS bundle) is a second, stale install and is Primary source A below. The
> version-gap caveat that follows therefore applies on this very machine, not
> only in production.
>
> **Status:** evidence only. Every ruling-shaped question is left open at the bottom.
> **Date of access for all URLs:** 2026-08-27.
> **Primary source A (the client itself):** `@anthropic-ai/claude-code` **v2.1.42**,
> installed on this box at `/opt/node22/lib/node_modules/@anthropic-ai/claude-code/cli.js`
> — `sha256 64a259b0443010aca4574f8ccf70cef36163377a71b78ba1cd4f2ef52328fed8`,
> 11,495,956 bytes, `package.json` `"version": "2.1.42"`, embedded
> `BUILD_TIME:"2026-02-13T18:55:32Z"`. The bundle is minified; citations are
> `cli.js:<line>` plus a byte offset, because a "line" here can be a megabyte.
> Identifier names are the minifier's; each is defined at the cited offset.
> **Primary source B (docs):** `code.claude.com/docs/en/{llm-gateway,llm-gateway-protocol,llm-gateway-connect,env-vars,settings,errors}`
> and `platform.claude.com/docs/en/{build-with-claude/streaming,api/errors}`.
> `docs.claude.com` and `docs.anthropic.com` 302 to `platform.claude.com`.
> **Primary source C (proxies):** `musistudio/claude-code-router` @
> `aec22a00cc9f934b8ab793522731cf1c71864d39` (2026-08-27 22:41 +0800, "Merge PR
> #1682"); `1rgs/claude-code-proxy` @ `5e45ba683ded931c1832cfca6468a791c6855e45`
> (2026-06-22). Both cloned under the dive scratchpad.
> **Not read:** `~/.claude/.credentials.json` — the sandbox classifier denied the
> read and I did not work around it. Every token-shape claim below is therefore
> derived from code or docs, never from a live credential.

> ## The version gap, stated once, because it colours everything below
>
> The installed CLI is **v2.1.42 (built 2026-02-13)**. The live documentation
> references behaviour introduced in **v2.1.181, v2.1.186, v2.1.196, v2.1.197,
> v2.1.199, v2.1.203, v2.1.218, v2.1.223, v2.1.227, v2.1.229**. The current
> release line is therefore ≥ v2.1.229 and the bundle on disk is roughly six
> months stale. Where the two disagree I report both and say which is which.
> **Every claim tagged "v2.1.42" is a claim about a binary I read, not about the
> client roundhouse will meet in production.** This gap is itself a finding:
> three of the most load-bearing facts for roundhouse (session-id headers, the
> stream watchdog, the warming probe) changed across it.

---

## 1. The redirect surface

### 1.1 What v2.1.42 reads, exhaustively

`rg -o 'ANTHROPIC_[A-Z0-9_]+' cli.js | sort -u` yields exactly eighteen names:
`ANTHROPIC_API_KEY` (44 sites), `ANTHROPIC_BASE_URL` (8), `ANTHROPIC_AUTH_TOKEN`
(6), `ANTHROPIC_VERTEX_PROJECT_ID`, `ANTHROPIC_MODEL`, `ANTHROPIC_BEDROCK_BASE_URL`,
`ANTHROPIC_SMALL_FAST_MODEL`, `ANTHROPIC_FOUNDRY_API_KEY`, `ANTHROPIC_CUSTOM_HEADERS`,
`ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION`, `ANTHROPIC_LOG`, `ANTHROPIC_FOUNDRY_RESOURCE`,
`ANTHROPIC_DEFAULT_{SONNET,OPUS,HAIKU}_MODEL`, `ANTHROPIC_FOUNDRY_BASE_URL`,
`ANTHROPIC_BETAS`, `ANTHROPIC_VERTEX_BASE_URL`.

### 1.2 `ANTHROPIC_BASE_URL` is read by the SDK, not by Claude Code

`cli.js:394` @2531708 — the vendored Anthropic SDK client:

```js
class jz{constructor({baseURL:A=gC1("ANTHROPIC_BASE_URL"),
                     apiKey:q=gC1("ANTHROPIC_API_KEY")??null,
                     authToken:K=gC1("ANTHROPIC_AUTH_TOKEN")??null,...Y}={}){
  let z={apiKey:q,authToken:K,...Y,baseURL:A||"https://api.anthropic.com"};
```

Claude Code's client factory `zh()` (`cli.js:908` @4161400–4166000) **never passes
`baseURL`**, so the SDK's env default is what applies. There is no validation,
no allowlist, and no scheme check on that path.

### 1.3 Provider selection and the auth decision

```js
// cli.js:99 @932981
function I7(){return $6(process.env.CLAUDE_CODE_USE_BEDROCK)?"bedrock"
  :$6(process.env.CLAUDE_CODE_USE_VERTEX)?"vertex"
  :$6(process.env.CLAUDE_CODE_USE_FOUNDRY)?"foundry":"firstParty"}

// cli.js:220 @2315350 — "may OAuth be used at all?"
function VV(){let A=$6(BEDROCK)||$6(VERTEX)||$6(FOUNDRY),
  K=(y8()||{}).apiKeyHelper,
  Y=process.env.ANTHROPIC_AUTH_TOKEN||K||process.env.CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR,
  {source:z}=oH({skipRetrievingKeyFromApiKeyHelper:!0});
  return !(A||Y||(z==="ANTHROPIC_API_KEY"||z==="apiKeyHelper")&&!$6(process.env.CLAUDE_CODE_REMOTE))}

// cli.js:221 @2324778 — "is OAuth actually in force?"
function O7(){if(!VV())return!1;return FC(qq()?.scopes)}   // FC = scopes includes "user:inference"
```

So a subscription login is suppressed by **any** of: a cloud-provider variable,
`ANTHROPIC_AUTH_TOKEN`, an `apiKeyHelper`, `CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR`,
or a resolved `ANTHROPIC_API_KEY`. `ANTHROPIC_BASE_URL` is **not** in that list.

The docs agree and are more precise about the API-key case
(`code.claude.com/docs/en/env-vars`): *"`ANTHROPIC_API_KEY` … When set, this key
is used instead of your Claude Pro, Max, Team, or Enterprise subscription even if
you are logged in. In non-interactive mode (`-p`), the key is always used when
present. In interactive mode, you are prompted to approve the key once before it
overrides your subscription."* And
`code.claude.com/docs/en/llm-gateway-connect`: *"Each variable sends the
credential in a different HTTP header: `ANTHROPIC_AUTH_TOKEN` in `Authorization:
Bearer`, `ANTHROPIC_API_KEY` in `x-api-key`, and `apiKeyHelper` in both."*

### 1.4 The finding: OAuth Bearer follows `ANTHROPIC_BASE_URL` anywhere

`cli.js:908` @4162079, the first-party branch of `zh()`:

```js
let X={apiKey:O7()?null:A||Qk(), authToken:O7()?qq()?.accessToken:void 0, ...J, ...};
return new ZS(X)
```

Under OAuth the API key is nulled and the OAuth access token becomes the SDK's
`authToken`, which the SDK emits as `Authorization: Bearer <token>`. **No host
check gates this.** The one host check in the bundle,

```js
// cli.js:99 @933222
function tH1(){let A=process.env.ANTHROPIC_BASE_URL;if(!A)return!0;
  try{return ["api.anthropic.com"].includes(new URL(A).host)}catch{return!1}}
```

is referenced exactly **four** times (`rg -c 'tH1\(\)' → 4`): its own definition,
`Zu()` (`cli.js:221` @2329671), `TB()` (`cli.js:909` @4314120), and `lDz()`
(`cli.js:7220` @10791364) — remote-settings sync, policy-limits fetch, and
user-settings sync respectively. **None of them is on the inference path.**

The documentation states the same conclusion in prose
(`code.claude.com/docs/en/llm-gateway`): *"Setting only that variable, without a
gateway credential, doesn't replace the subscription. Requests still route
through the gateway, but a saved claude.ai login remains the active credential,
so its usage limits and billing apply. Gateways that pass this traffic on to
Anthropic must forward the OAuth capability in `anthropic-beta`."*

### 1.5 The OAuth beta rides with the token

`gf="oauth-2025-04-20"` (`cli.js:20` @299249, alongside the scope constants
`user:inference`, `user:profile`, `user:sessions:claude_code`,
`user:mcp_servers`, `org:create_api_key`). The beta-list builder:

```js
// cli.js:220 @2205520
y1A=zA((A)=>{let q=[],K=A.includes("haiku"),Y=I7(),z=G_1();
  if(!K)q.push(jiA);                       // jiA = "claude-code-20250219"
  if(O7())q.push(gf);                      // gf  = "oauth-2025-04-20"
  if(A.includes("[1m]"))q.push(eN1);       // eN1 = "context-1m-2025-08-07"
  if(!$6(CLAUDE_CODE_DISABLE_ADAPTIVE_THINKING)&&g76(A))q.push(WiA);  // adaptive-thinking-2026-01-28
  else if(!$6(DISABLE_INTERLEAVED_THINKING)&&z65(A))q.push(DiA);      // interleaved-thinking-2025-05-14
  ... if(G_1()&&(w||H))q.push(Hr1);        // context-management-2025-06-27
  ... if($71(A)&&$)q.push(Hi);             // structured-outputs-2025-12-15
  ... if(z)q.push(AT1);                    // prompt-caching-scope-2026-01-05
  if(process.env.ANTHROPIC_BETAS&&!K)q.push(...ANTHROPIC_BETAS.split(",")...);
  return q})
```

Constants at `cli.js:8` @24142. Two things matter for roundhouse. First,
**`claude-code-20250219` is pushed on every non-haiku request regardless of auth
mode** — it is the marker that identifies Claude Code traffic on the wire.
Second, `oauth-2025-04-20` is pushed **only** under `O7()`, and the docs make
stripping it fatal (`llm-gateway-protocol`, request-headers table): *"this header
also carries an OAuth capability that the upstream requires, and stripping it
fails those requests with `401`"*, and (`llm-gateway-protocol`,
disable-pre-release-capabilities): *"It never suppresses the OAuth capability
that subscription authentication requires."*

Two further conditional values the fact-check surfaced in the same function:
`tool-examples-2025-10-29` (`$r1`, `cli.js:8`), gated on the same
firstParty/foundry-and-not-`CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS` condition
as `prompt-caching-scope-2026-01-05` plus an Anthropic-side statsig flag — so
it is live for exactly the subscription-OAuth population; and
`web-search-2025-03-05` (`ER6`, `cli.js:8`), pushed under vertex (with a model
check) or foundry. A server enumerating the full `anthropic-beta` envelope
v2.1.42 can send must include both.

`ANTHROPIC_BETAS` is appended unconditionally; a separate settings-driven custom
beta path warns *"Custom betas are only available for API key users"* and allows
only `context-1m-2025-08-07` (`fd8=[eN1]`, `cli.js:220` @2205520). The docs
confirm the asymmetry: *"Unlike the `--betas` flag, which requires API key
authentication, this variable works with all auth methods including Claude.ai
subscription."*

### 1.6 `ANTHROPIC_CUSTOM_HEADERS` parsing

```js
// cli.js:908 @4165300
function I79(){let A={},q=process.env.ANTHROPIC_CUSTOM_HEADERS;if(!q)return A;
  for(let Y of q.split(/\n|\r\n/)){if(!Y.trim())continue;
    let z=Y.match(/^\s*(.*?)\s*:\s*(.*?)\s*$/);if(z){let[,w,H]=z;if(w&&H!==void 0)A[w]=H}}
  return A}
```

Newline-separated `Name: Value`, first colon wins, whitespace trimmed. These land
in `defaultHeaders` and therefore **override** the SDK's own defaults for the
same name (header merge order at `cli.js:401` @2540396 puts `defaultHeaders`
after `authHeaders`), which is how a proxy author can force an `Authorization`
header. Docs note a v2.1.227+ validation pass rejecting non-HTTP-safe characters.

### 1.7 What `ANTHROPIC_BASE_URL` does *not* redirect (v2.1.42)

`D4().BASE_API_URL` is the hardcoded `https://api.anthropic.com`
(`gq8={BASE_API_URL:"https://api.anthropic.com",…}`, `cli.js:20` @299249),
overridable only by `CLAUDE_CODE_CUSTOM_OAUTH_URL` against an internal allowlist
(`if(!N3K.includes(Y))throw Error("CLAUDE_CODE_CUSTOM_OAUTH_URL is not an
approved endpoint.")`). Everything under `/api/…` therefore bypasses a proxy:
OAuth token refresh, `/api/claude_cli_profile`, `/api/oauth/profile`,
`/api/claude_code/{policy_limits,user_settings,settings,metrics}`,
`/api/claude_code_penguin_mode`, `/v1/mcp_servers`. Telemetry is explicitly
pinned: `let q=process.env.ANTHROPIC_BASE_URL==="https://api-staging.anthropic.com"
?"https://api-staging.anthropic.com":"https://api.anthropic.com";
this.endpoint=`${q}/api/event_logging/batch`` (`cli.js:2181` @6891193). The
connection probe is a direct `GET https://api.anthropic.com/api/hello`
(`cli.js:2239` @7726944). The Files API *does* follow it:
`function PXz(){return process.env.ANTHROPIC_BASE_URL||process.env.CLAUDE_CODE_API_BASE_URL||"https://api.anthropic.com"}`
(`cli.js:6272` @10630667).

The docs corroborate for the current line
(`llm-gateway-protocol`): *"The fast mode availability check never appears in
gateway logs: it calls `api.anthropic.com` directly rather than following
`ANTHROPIC_BASE_URL` … The WebFetch domain safety check also calls
`api.anthropic.com` directly."* But they also say a gateway now receives *"a
`HEAD /api/hello` connection-warming probe"* — i.e. the probe was retargeted at
the base URL sometime after v2.1.42.

**There is no statsig/feature-flag host in the bundle.** `rg -oi 'statsig'`
returns only the literals `Statsig` and `StatsigGates`; there is no
`*.statsig.com` URL. Gates arrive over Anthropic's own `/api/…` endpoints.

---

## 2. The wire surface Claude Code exercises

### 2.1 Endpoints (v2.1.42)

| Path | Method | When |
|---|---|---|
| `/v1/messages?beta=true` | POST, `stream:true` | every turn (`cli.js:5646` @10269576) |
| `/v1/messages?beta=true` | POST, no `stream` | non-streaming fallback (§3.6), quota probe, auth probe, haiku helper |
| `/v1/messages/count_tokens?beta=true` | POST | context accounting (`cli.js:393` @2515519, `cli.js:964` @4459356) |
| `/v1/files/*` | GET/POST | only with `--file` and `CLAUDE_CODE_SESSION_ACCESS_TOKEN` |

The `?beta=true` suffix is not cosmetic: the beta message resource posts to the
literal string `"/v1/messages?beta=true"` (`cli.js:393` @2515097) and the beta
token-counter to `"/v1/messages/count_tokens?beta=true"`. The docs say the same
and give the remedy: *"Inference requests post to `/v1/messages?beta=true`, so
match on the path, not the full URL."* (`llm-gateway-protocol`).

**Negative (v2.1.42): Claude Code never calls `GET /v1/models`.**
`rg -c 'models\.list\(|models\.retrieve\(|ENABLE_GATEWAY_MODEL_DISCOVERY' cli.js`
→ 0. The string `"/v1/models"` occurs once, inside the SDK's
`getAPIList("/v1/models",GS,…)` definition (`cli.js:394` @2531708) — a method
never invoked. Current docs describe gateway model discovery as
`GET /v1/models?limit=1000`, 3-second timeout, redirects treated as failure,
**off by default** behind `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`
(`llm-gateway-protocol`, model-discovery). So the negative is true of v2.1.42 and
conditionally false of the current line.

### 2.2 Request headers on the inference client

```js
// cli.js:908 @4159585
let O={"x-app":"cli","User-Agent":$r(),...I79(),
   ...CLAUDE_CODE_CONTAINER_ID?{"x-claude-remote-container-id":…}:{},
   ...CLAUDE_CODE_REMOTE_SESSION_ID?{"x-claude-remote-session-id":…}:{},
   ...CLAUDE_AGENT_SDK_CLIENT_APP?{"x-client-app":…}:{}};
if($6(process.env.CLAUDE_CODE_ADDITIONAL_PROTECTION))O["x-anthropic-additional-protection"]="true";
if(!O7())h79(O,i7());   // h79: A.Authorization = `Bearer ${ANTHROPIC_AUTH_TOKEN||helperValue}`
let J={defaultHeaders:O,maxRetries:q,
   timeout:parseInt(process.env.API_TIMEOUT_MS||String(600000),10),
   dangerouslyAllowBrowser:!0,fetchOptions:u81(),...};
```

`$r()` = `claude-cli/2.1.42` plus optional `, agent-sdk/<v>` / `, client-app/<v>`
(`cli.js:220` @2189991). The SDK layer adds
(`cli.js:401` @2540396): `Accept: application/json`, `X-Stainless-Retry-Count`,
`X-Stainless-Timeout` (seconds), the `X-Stainless-{Lang,Package-Version,OS,Arch,Runtime,Runtime-Version}`
platform set (`cli.js:362` @2470945),
`anthropic-dangerous-direct-browser-access: true` (because
`dangerouslyAllowBrowser:!0` is passed), and `anthropic-version: 2023-06-01`.
`x-stainless-helper` is added per-request from tool/message shape
(`Eq6`, `cli.js:366` @2488711). `X-Stainless-Helper-Method: stream` appears only
on the SDK's `.stream()` helper, which Claude Code does not use.

**Negative: no idempotency header is ever sent.** `idempotencyHeader` occurs
twice, both inside `buildHeaders` (`if(this.idempotencyHeader&&q!=="get")`), and
is never assigned on the Anthropic client class, so the branch is dead.

### 2.3 Request body

```js
// cli.js:5646 @10269085 — N1(), the body builder
return {model:ig(w.model),
  messages:swz(Z,Y6,w.querySource,K6,V1,Z1),
  system:y, tools:[...V,...w.extraToolSchemas??[]], tool_choice:w.toolChoice,
  ...B?{betas:X1}:{}, metadata:wo(), max_tokens:p1, thinking:_1,
  ...temperature, ...context_management, ...output_config, ...speed}
```

`betas` is a body field on the SDK's beta resource, which strips it and emits
`anthropic-beta: <comma-joined>` (`cli.js:393` @2515097).

**System blocks and cache breakpoints.**

```js
// cli.js:5646 @10282084
function twz(A,q,K){return TIA(A,{...}).map((Y)=>({type:"text",text:Y.text,
  ...q&&Y.cacheScope!==null?{cache_control:Ic1({scope:Y.cacheScope,querySource:K?.querySource})}:{}}))}
// cli.js:5641 @10261714
function Ic1({scope:A,querySource:q}={}){return{type:"ephemeral",
  ...lwz(q)?{ttl:"1h"}:{}, ...A==="global"?{scope:A}:{}}}
```

The 1-hour TTL is gated on `O7()&&!UV.isUsingOverage` plus a statsig allowlist of
`querySource` values (`lwz`, same offset) — i.e. **the 1h prompt-caching TTL is a
subscription-only behaviour** in v2.1.42. `scope:"global"` pairs with
`prompt-caching-scope-2026-01-05`.

Message breakpoints (`swz`, `cli.js:5646` @10280670):

```js
let H=A.map((_,J)=>{let X=J>A.length-3; ...})
```

`X` is true only for the **final two messages**; `nwz`/`rwz` (`cli.js:5646`
@10262104/@10262600) then attach `cache_control` to the *last content block* of
those messages (skipping `thinking`/`redacted_thinking` for assistant turns).
Total breakpoints per request = system blocks + at most two.

**`metadata.user_id` — the only session name in the body.**

```js
// cli.js:5641 @10262104
function wo(){let A=oh(),q=X3()?.accountUuid??"",K=g6();
  return{user_id:`user_${A}_account_${q}_session_${K}`}}
```

`oh()` (`cli.js:6247` @10548111) is a persisted 32-byte hex install id;
`X3()?.accountUuid` is the OAuth account UUID (empty string under API-key auth);
`g6()` (`cli.js:8` @32023) returns `n6.sessionId`. `metadata:wo()` appears on
five call sites including the main builder.

### 2.4 Token counting, and its fallback

`Bb1` calls `beta.messages.countTokens({model,messages,tools,betas,thinking})`
(`cli.js:964` @4459356). On null or throw, `yp1` (`cli.js:3386` @8950600) falls
back to `IR7` (`cli.js:964` @4460289), which issues a **real
`beta.messages.create` with `max_tokens:1`** against the haiku model and reads
`usage.input_tokens + cache_creation_input_tokens + cache_read_input_tokens`.
The docs describe the same design from the gateway side: *"Token-counting
endpoints are the only optional ones: when they're absent, Claude Code falls back
to counting context usage through the inference endpoint instead."*

### 2.5 Timeouts and retries

- Client timeout: `API_TIMEOUT_MS` or **600000 ms** (`cli.js:908` @4159585); the
  SDK repeats `timeout:…??600000` at both message resources. Docs: *"default:
  600000, or 10 minutes; maximum: 2147483647."*
- App-level retry budget: `D59=10` (`cli.js:909` @4312737), overridable by
  `CLAUDE_CODE_MAX_RETRIES`. Docs put the default at 10 with a cap of 15 as of
  v2.1.186.
- Backoff: `Dp(A,q)` — `retry-after` seconds if present, else
  `min(500·2^(n-1),32000)` plus up to 25 % jitter (`cli.js:909` @4310819).
- The retry predicate is where subscription auth changes behaviour:

```js
// cli.js:909 @4311822
function Z59(A){if(oN7(A))return!1;
  if(A.message?.includes('"type":"overloaded_error"'))return!0;
  if(cE7(A))return!0;                                   // context-limit 400, parsed by regex
  let q=A.headers?.get("x-should-retry");
  if(q==="true"&&!O7())return!0;                        // ignored under OAuth
  if(q==="false"){...return!1}
  if(A instanceof ZW)return!0;                          // connection error
  if(!A.status)return!1;
  if(A.status===408)return!0; if(A.status===409)return!0;
  if(A.status===429)return!O7();                        // NOT retried under OAuth
  if(A.status===401){P46();return!0}
  if(A.status>=500)return!0; return!1}
```

Under subscription OAuth a `429` is routed to the rate-limit UI instead
(`cli.js:909` @4310107 reads `anthropic-ratelimit-unified-*` headers and sleeps
`v59(J)` from `retry-after`, else `V59=1800000` ms floored at `T59=600000`).
`pE7` treats 529 **or** a body containing `"type":"overloaded_error"` as overload;
`M59=3` consecutive overloads trigger the fallback model.

The current docs are consistent and add that as of v2.1.199 subscription 429s
*without* quota headers are retried too, so this is another version-sensitive
edge.

---

## 3. Server obligations

### 3.1 The documented contract

`platform.claude.com/docs/en/build-with-claude/streaming`, "Event types":

> *"Each server-sent event includes a named event type and associated JSON data.
> Each event uses an SSE event name (for example, `event: message_stop`), and
> includes the matching event `type` in its data."*
>
> 1. `message_start`: contains a `Message` object with empty `content`.
> 2. A series of content blocks, each of which has a `content_block_start`, one or
>    more `content_block_delta` events, and a `content_block_stop` event. Each
>    content block has an `index` that corresponds to its index in the final
>    Message `content` array.
> 3. One or more `message_delta` events…
> 4. A final `message_stop` event.
>
> *"The token counts shown in the `usage` field of the `message_delta` event are
> cumulative."* · *"Event streams may also include any number of `ping` events."*
> · *"new event types may be added, and your code should handle unknown event
> types gracefully."*

Mid-stream errors are `event: error` with
`data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}`.

### 3.2 What the client actually enforces — the SSE layer

```js
// cli.js:365 @2478416
for await(let $ of kM5(A,q)){
  if($.event==="completion") yield JSON.parse($.data);
  if($.event==="message_start"||$.event==="message_delta"||$.event==="message_stop"
   ||$.event==="content_block_start"||$.event==="content_block_delta"
   ||$.event==="content_block_stop") yield JSON.parse($.data);
  if($.event==="ping") continue;
  if($.event==="error") throw new Z4(void 0,Dq6($.data)??$.data,void 0,A.headers)}
```

Three consequences a proxy author must not get wrong:

1. **Dispatch is on the SSE `event:` name, not on `data.type`.** A stream that
   emits `data:` frames without an `event:` line matches no branch — the events
   are *silently dropped*, not rejected. The stream then ends with nothing
   consumed (§3.4).
2. Unknown event names are silently ignored, as the versioning policy requires.
3. The `error` event constructs the APIError with `status = undefined`. Feed that
   back into `Z59` (§2.5): `if(!A.status)return!1`. **A mid-stream error is
   therefore terminal unless its serialised message contains the literal
   `"type":"overloaded_error"`** — with one exception scoped to non-OAuth
   traffic, which the fact-check pinned: the Z4 built at `cli.js:365` carries
   the *response's* headers as its fourth argument, and `Z59` checks
   `x-should-retry: true` before the `!A.status` short-circuit but only when
   `!O7()`. So an API-key client retries a mid-stream error if the initial
   response headers set `x-should-retry: true`; a subscription-OAuth client
   never does. (The `cE7` context-limit check is dead for this error class —
   it requires `A.status===400`, which a mid-stream error never has.) Anything
   else — a `rate_limit_error` or an `api_error` delivered mid-stream — ends
   the turn.

### 3.3 What the client enforces — the accumulator

`cli.js:5646` @10270059 onward, Claude Code's own `for await` over the events:

| Condition | Result |
|---|---|
| `content_block_delta` at an index with no prior `content_block_start` | `RangeError("Content block not found")`, analytics `content_block_not_found_delta` |
| `input_json_delta` on a block whose type is not `tool_use`/`server_tool_use` | `Error("Content block is not a input_json block")` |
| `text_delta` on a non-`text` block | `Error("Content block is not a text block")` |
| `thinking_delta`/`signature_delta` on a non-`thinking` block | `Error("Content block is not a thinking block")` |
| `content_block_stop` at an unknown index | `RangeError("Content block not found")` |
| `content_block_stop` before any `message_start` | `Error("Message not found")` |
| `input_json_delta` on a `server_tool_use` block | throws `Error("Content block input is not a string")` — `content_block_start` seeds a `server_tool_use` block's `input` as `{}`, and the delta handler requires a string accumulator. A fifth throw condition, found by the fact-check |
| `citations_delta` | ignored (`break`) |
| unknown `content_block.type` on start | stored verbatim (`default:` branch) |
| `message_stop` | no-op |
| unknown top-level event type | falls through the switch, still re-yielded as `{type:"stream_event"}` |

`stop_reason` handling is in `message_delta`: `max_tokens` and
`model_context_window_exceeded` each produce a user-visible error message rather
than an exception.

**Index discipline is therefore strict on start/stop pairing and on block-type
agreement, and lenient on gaps and ordering *between* blocks** — the accumulator
is a sparse array `P1[index]`, so non-contiguous indices work.

### 3.4 Split usage accounting

```js
// cli.js:5646 @10278693
function p91(A,q){if(!q)return{...A};return{
  input_tokens: q.input_tokens!==null&&q.input_tokens>0 ? q.input_tokens : A.input_tokens,
  cache_creation_input_tokens: q.cache_creation_input_tokens>0 ? q.… : A.…,
  cache_read_input_tokens:     q.cache_read_input_tokens>0     ? q.… : A.…,
  output_tokens: q.output_tokens ?? A.output_tokens,
  server_tool_use:{web_search_requests:q…??A…, web_fetch_requests:q…??A…},
  service_tier: A.service_tier,
  cache_creation:{ephemeral_1h_input_tokens:q…??A…, ephemeral_5m_input_tokens:q…??A…}, …}}
```

Called twice: `p91(v1, message_start.message.usage)` and
`p91(v1, message_delta.usage)`. Three consequences:

- Input and cache counts use a **greater-than-zero guard**, so a `message_delta`
  that omits them or sends `0` does not clobber the `message_start` values. This
  is what makes the documented "split" reporting safe.
- `output_tokens` uses `??`, so an explicit `output_tokens: 0` in `message_delta`
  **does** overwrite a non-zero accumulated value.
- `service_tier` is taken from the accumulator, never from the wire; the seed
  `uN` hardcodes `service_tier:"standard"` (`cli.js:3413` @9018406). A proxy
  reporting a service tier is reporting it to nobody.

### 3.5 Liveness expectations

v2.1.42 has an **opt-in** watchdog (`cli.js:5646` @10270059):

```js
let S1=$6(process.env.CLAUDE_ENABLE_STREAM_WATCHDOG),x1=30000,M1=60000,…
Y6=function(){if(p1(),!S1)return;
  _1=setTimeout(()=>{h(`Streaming idle warning: no chunks received for 30s`)},x1);
  R1=setTimeout(()=>{y1=!0;h(`Streaming idle timeout: no chunks received for 60s, aborting stream`);ef1(r)},M1)}
```

Unconditionally, a gap > 30 s between events is logged as
`tengu_streaming_stall`, and stall counts are reported.

The **current** contract is stricter and always on
(`llm-gateway-protocol`, "Streaming"):

> *"Inference responses must stream. Claude Code consumes server-sent events as
> they arrive, so a gateway that buffers complete responses before relaying them
> stalls the client. Forward keep-alive pings as well. On connections through
> `ANTHROPIC_BASE_URL` … Claude Code counts every byte your gateway relays,
> including SSE `ping` events and comment lines, and aborts a stream that goes
> silent for 300 seconds by default. … An upstream that sends no pings at all …
> leaves those pauses with nothing to forward. When translating from such an
> upstream, emit your own `ping` events during silent gaps."*

Note the byte-level framing: **comment lines (`: keep-alive`) count**, so a
roundhouse-emitted heartbeat need not be a well-formed `ping` event to satisfy
the watchdog — though it must be if it is to survive §3.2's dispatch without
being dropped as an unknown event (it would be ignored either way).

### 3.6 Non-streaming — yes, Claude Code POSTs `stream:false`

Four unconditional non-streaming call sites:

- auth probe `rJq`: `max_tokens:1`, `messages:[{role:"user",content:"test"}]` (`cli.js:5641` @10262104)
- quota probe `g79`: `max_tokens:1`, `content:"quota"`, `.asResponse()` to read rate-limit headers (`cli.js:908` @4166493)
- token-count fallback `IR7` (`cli.js:964` @4460289)
- a helper create with `system`/`tools`/`output_config` (`cli.js:2735` @8713009)

Plus two fallback paths from the streaming turn (`cli.js:5646` @10275736 and
@10276453):

```js
if(!D8("…streaming_fallback",!1)) throw …   // gated ON by a statsig flag, default false
… let W6=yield*nJq(…)                        // non-streaming re-issue
// and, ungated:
if(!t&&D1 instanceof NB&&D1.originalError instanceof Z4&&D1.originalError.status===404){
  h("Streaming endpoint returned 404, falling back to non-streaming mode",{level:"warn"}); … }
```

`nJq` (`cli.js:5641` @10263809) calls `beta.messages.create` with **no `stream`
field** and clamps `max_tokens` to `min(requested, ewz=21333)` via `AHz`. The
triggers include the two "empty stream" errors:
`"Stream completed without receiving message_start event - triggering
non-streaming fallback"` and `"Stream completed with message_start but no content
blocks completed - triggering non-streaming fallback"`. **A server that emits an
SSE body Claude Code cannot parse gets a second, non-streaming request for the
same turn.**

Request size ceiling (`platform.claude.com/docs/en/api/errors`): Messages API and
Token Counting API **32 MB**, `413 request_too_large`; *"On the direct Claude
API, Cloudflare returns this error before the request reaches the API servers."*

### 3.7 Error-body fidelity

`llm-gateway-protocol`: *"The retry logic matches on the upstream's error
wording, so forward error response bodies unmodified. A gateway that wraps
upstream errors in its own envelope breaks the recovery path, even when it
preserves the status code, unless the envelope's message carries a stable
`capability_rejected:` token."* The bundle shows exactly which strings are load-
bearing: `'"type":"overloaded_error"'`, `"Fast mode is not enabled"` (`W59`), and
the regex ``/input length and `max_tokens` exceed context limit: (\d+) \+ (\d+) > (\d+)/``
(`cE7`, `cli.js:909` @4311822) whose three captures drive an automatic
`max_tokens` reduction and retry.

---

## 4. Session correlation

### 4.1 The headers: who really emits them

The current documentation (`llm-gateway-protocol`, request-headers table) lists
three:

| Header | Documented description |
|---|---|
| `x-claude-code-session-id` | *"A unique identifier for the current Claude Code session. Use it to aggregate all requests from one session without parsing request bodies"* |
| `x-claude-code-agent-id` | *"Identifier of the subagent that issued the request, present only on requests from an agent Claude Code spawned inside the session"* |
| `x-claude-code-parent-agent-id` | *"Identifier of the agent that spawned the requesting agent, present only for nested agents"* |

**Negative, established by exhaustive search of the installed binary:** none of
these exists in v2.1.42. `rg -oi 'x-claude-code-session-id|x-claude-code-agent-id|x-claude-code-parent-agent-id' cli.js`
→ 0 matches. `rg -oi '"x-claude[a-z0-9-]*"' cli.js` returns exactly three:
`"X-Claude-Code-Ide-Authorization"` (IDE websocket, not the API),
`"x-claude-remote-container-id"` and `"x-claude-remote-session-id"` — the latter
two conditional on `CLAUDE_CODE_CONTAINER_ID` / `CLAUDE_CODE_REMOTE_SESSION_ID`,
which are cloud-session variables, not the local session id. The full inference
header set is enumerated in §2.2.

So the headers were **added by Anthropic between v2.1.42 and the current line** —
they are Claude Code's own, not something NeMo Relay injects. The
`agent-docs/research/k8s-gateway-inference-deep-dive.md:300` note about
llm-d-router's `agent-identity` plugin priority-listing `x-claude-code-session-id`
is consistent with that: the ecosystem was building against a header the client
now sends, but a v2.1.42-era client would have produced nothing for it to key on.

### 4.2 The version-robust carrier: `metadata.user_id`

`user_id = "user_<installHex32>_account_<accountUuid>_session_<sessionUuid>"`
(§2.3). It is on **every** inference request in v2.1.42, and the docs give no
indication it was removed.

Independent corroboration from a third-party proxy — `claude-code-router` @
`aec22a0`, `packages/core/src/gateway/claude-code-router-plugin.ts:1686-1700`:

```ts
function resolveSessionId(body, headers): string | undefined {
  const fromHeader = readHeader(headers["x-claude-code-session-id"]) || readHeader(headers["x-claude-session-id"]);
  if (fromHeader) return fromHeader;
  const metadata = body.metadata;
  if (isRecord(metadata) && typeof metadata.user_id === "string") {
    const parts = metadata.user_id.split("_session_");
    if (parts.length > 1) return parts.at(-1);
  }
  return undefined;
}
```

A shipping proxy tries the header **and keeps a body fallback that splits on
`"_session_"`**. `packages/core/src/gateway/core-runtime/responses-session-affinity.ts:26`
carries the same two header names for its affinity path.

### 4.3 Lifecycle of the session component

`g6()` returns `n6.sessionId` (`cli.js:8` @32023). Only two mutators exist:

- `SR6({setCurrentAsParent})` — assigns a fresh UUID and records the old one as
  `parentSessionId`. `rg -c 'SR6\(' cli.js` → **2**, one being the definition;
  the single call site is the `/clear` handler (`cli.js:3529` @9182595).
- `dP(A)` — sets the id and mirrors it into `CLAUDE_CODE_SESSION_ID` if that
  variable is already defined. Eight non-definition call sites (the fact-check
  enumerated cli.js lines 6225, 6310, 7136, 7269×2, 7386, 7393, 7551): the
  `--continue` / `--resume` / `--session-id` / transcript-load paths — **plus
  one that adopts rather than restores**: the `--remote "<task>"` flow
  (`cli.js:7551`) calls `dP` with a session id freshly minted by the remote
  backend, so a server-issued id enters through the restore function.

Therefore, for v2.1.42:

- **Compaction does not change the session id** — no `SR6` call on that path.
- **`--resume` / `--continue` restore the prior id**, so `metadata.user_id` is
  stable across process restarts.
- **`/clear` mints a new one, and `--remote` adopts a server-minted one** —
  the two ways the `_session_` component changes besides a fresh unrestored
  process.
- **In-process subagents share the parent's id.** `w.agentId` exists in the query
  options and is threaded into analytics (`UAq`) and content post-processing
  (`Tv6`), but is absent from `N1()`'s body. Agent-teams *teammates*, by
  contrast, are spawned as separate `claude` processes with
  `--parent-session-id <uuid> --agent-id <id>` (`cli.js:2333` @8182204,
  `cli.js:2350` @8211034), so each teammate carries a distinct `_session_` value
  and the parent link exists only on its command line.

### 4.4 The other stable-prefix question: the attribution block

`llm-gateway-protocol` documents a *"system prompt attribution block"* prepended
as the **first system block**, which `api.anthropic.com` strips positionally, and
warns that *"prepending another system block, reordering the array, or converting
it to a single string defeats the strip."* It also claims: *"From Claude Code
v2.1.181, the block is stable for the lifetime of a conversation when requests
route through a custom base URL … Before v2.1.181 the block included a
per-request token that changed the start of the system prompt on every request."*

In v2.1.42 the block is built by:

```js
// cli.js:528 @2697199
function YT5(){if(FY(process.env.CLAUDE_CODE_ATTRIBUTION_HEADER))return!1;
  return D8("tengu_attribution_header",!0)}
function oK6(A){if(!YT5())return"";
  let q=`2.1.42.${A}`, K=process.env.CLAUDE_CODE_ENTRYPOINT??"unknown";
  return `x-anthropic-billing-header: cc_version=${q}; cc_entrypoint=${K}; cch=00000;`}
// cli.js:528 @2698203
function HT5(A){let q=A.find((Y)=>Y.type==="user");            // FIRST user message
  …return that message's first text…}
function AqA(A,q){let Y=[4,7,20].map((H)=>A[H]||"0").join("");
  return sha256(`59cf53e54c78${Y}${q}`).digest("hex").slice(0,3)}
function dA7(A){return AqA(HT5(A),"2.1.42")}
```

The fingerprint is three hex characters derived from **characters 4, 7 and 20 of
the first user message plus the client version**, and `cch=00000` is a hardcoded
literal. So as read, **v2.1.42's block is already stable for the lifetime of a
conversation** and changes only when the first user message changes — i.e. at a
compaction boundary or on `/clear`. That contradicts the docs' "before v2.1.181 …
changed on every request" characterisation for this particular build. I did not
read any intermediate version and cannot say where the per-request token lived;
see open question 3.

`CLAUDE_CODE_ATTRIBUTION_HEADER=0` suppresses the block (v2.1.42 and current).

---

## 5. Known proxy pitfalls, each with its source

1. **Buffering the SSE body.** *"a gateway that buffers complete responses before
   relaying them stalls the client"* — `llm-gateway-protocol`, Streaming.
2. **Dropping pings during silent gaps.** Same source: the 300-second byte
   watchdog counts relayed bytes including ping events and comment lines, and
   *"if your gateway strips or buffers them, Claude Code aborts the stream during
   those pauses."*
3. **Omitting the `event:` line.** Establishied here from `cli.js:365` @2478416:
   the dispatch is on the SSE event name, so `data:`-only frames are silently
   dropped and the turn ends in `"Stream ended without receiving any events"` →
   non-streaming re-issue (§3.6).
4. **Stripping `anthropic-beta` under subscription auth.** *"stripping it fails
   those requests with `401`"* — `llm-gateway-protocol`, request headers.
5. **Allowlisting beta values.** *"Forward the header verbatim; don't allowlist
   individual values, because the set changes with Claude Code releases … A
   gateway pinned to an observed list strips the next capability's header or
   field and breaks it on the release that introduces it."* — same.
6. **Half-forwarding a header/body pair.** *"A gateway that strips the header
   while passing the body … produces hard `400` errors; only when both halves are
   absent together does the feature turn off quietly. A gateway that rewrites or
   redacts request bodies for content inspection breaks the pairing the same
   way."* — `llm-gateway-protocol`, feature pass-through. The named pairs:
   `context_management`, `output_config`, tool-schema `strict`/`defer_loading`.
7. **Re-enveloping upstream errors.** §3.7.
8. **Reordering or merging the `system` array.** §4.4.
9. **Rewriting the response body without dropping `content-length`.**
   `claude-code-router` @ `aec22a0`,
   `packages/core/src/gateway/request/pipeline.ts:837` does
   `responseHeaders.delete("content-length")` before its rewrite path at :839.
10. **Echoing the upstream's model name in `message_start`.** ccr ships a whole
    transform for this — `packages/core/src/gateway/features/anthropic-response-model.ts:16-38`,
    `rewriteAnthropicMessageStartModelStream`, which re-frames the SSE stream on
    `/\r?\n\r?\n/` boundaries and rewrites `message_start.message.model` to the
    requested id.
11. **Zeroing cache fields.** `1rgs/claude-code-proxy` @ `5e45ba6`,
    `server.py:381-398` models `Usage` with `cache_creation_input_tokens: int = 0`
    and `cache_read_input_tokens: int = 0`, and its synthesised
    `message_start`/`message_delta` (`server.py:925-952`, `:1163-1208`) carry
    those zeroes. Harmless for input/cache under §3.4's `>0` guard; **not**
    harmless for `output_tokens`, which uses `??`.
12. **Not emitting `message_start` at all.** `claude-code-proxy` emits it first
    then `content_block_start` then a `ping` (`server.py:925-952`) — the minimum
    prelude Claude Code's accumulator needs before any delta.
13. **Client-disconnect accounting.** ccr `packages/core/src/gateway/internal/shared.ts:243-259`
    distinguishes "client closed before a terminal event" (status `499`) from
    "client closed after" — worth mirroring if roundhouse bills partial turns.
14. **Getting identified as someone else's gateway.** Claude Code fingerprints
    gateways from **response** header prefixes:
    `HnY={litellm:{prefixes:["x-litellm-"]},helicone:{prefixes:["helicone-"]},portkey:{prefixes:["x-portkey-"]},"cloudflare-ai-gateway":{prefixes:["cf-aig-"]}}`
    (`cli.js:3413` @9018406), reported as a `gateway` dimension on
    `tengu_api_success`/`tengu_api_error`. There is no roundhouse prefix and no
    generic opt-in.
15. **Redirecting `/v1/models`.** *"any redirect is treated as failure so the
    credential can't leak to a redirect target. A gateway that responds slowly or
    redirects `/v1/models`, even `http` to `https`, fails discovery silently."* —
    `llm-gateway-protocol`, model discovery.
16. **Assuming the OAuth token is yours to keep.** ccr
    `packages/core/src/agents/local-providers/claude-code.ts:144-186` reads
    Claude Code's own credential store (macOS keychain first, then
    `~/.claude/.credentials.json`) and replays the token with
    `{"anthropic-beta":"oauth-2025-04-20"}` against `https://api.anthropic.com`.
    Constants at `packages/core/src/gateway/internal/shared.ts:228,230`. That a
    third party can do this is exactly why a pass-through proxy sees a live
    subscription credential and must decide what it stores.

---

## 5.5 Addendum (2026-08-27, same day): live capture of the 2.1.247 binary

A loopback capture rig (mock `/v1/messages` server logging full headers and
bodies, answering a well-formed minimal SSE stream) was run against the
**native 2.1.247 binary** on this box — the client an e2e suite would
actually spawn — in three variants: an ambient run inheriting this CCR
container's host-managed OAuth, a fully cleared-env run under a fake
`ANTHROPIC_API_KEY`, and a cleared-env run under a fake
`ANTHROPIC_AUTH_TOKEN`. All three completed cleanly consuming the mock
stream (exit 0, no parser complaints). Raw captures live in the session
scratchpad (`capture/*.jsonl`, credential values redacted); this addendum
records what moved against the v2.1.42 read above. Where they disagree,
**this addendum wins for the current client line**.

1. **`metadata.user_id` changed shape.** At 2.1.247 it is a JSON-encoded
   object *string* — `{"device_id":"<64 hex>","account_uuid":"<uuid or
   empty>","session_id":"<uuid>"}` — not §2.3/§4.2's underscore-delimited
   `user_<hex>_account_<uuid>_session_<uuid>`. The `_session_`-split
   fallback that claude-code-router ships (and §4.2 cites) does **not**
   parse this shape; a robust reader handles both: parse as JSON and take
   `.session_id`, else split on `_session_`. `session_id` matched the
   header (below) in every run and persisted across `--continue`.
2. **`X-Claude-Code-Session-Id` is real at 2.1.247** — present on every
   inference request (§4.1's "added after v2.1.42" inference is confirmed
   live), a fresh UUID per invocation unless `CLAUDE_CODE_SESSION_ID` is in
   the env, stable across `--continue`. `x-claude-code-agent-id` /
   `-parent-agent-id` appeared in no run (single-agent turns — consistent
   with the docs' "only on subagent requests").
3. **Betas ride the header only.** No `betas` body array in any run; the
   path is `POST /v1/messages?beta=true` (confirmed at 2.1.247). Header
   under cleared-env API-key auth, verbatim: `claude-code-20250219,
   interleaved-thinking-2025-05-14, thinking-token-count-2026-05-13,
   context-management-2025-06-27, prompt-caching-scope-2026-01-05` (note
   `thinking-token-count-2026-05-13`, post-2.1.42). The ambient managed-
   OAuth run added `oauth-2025-04-20` **and** `extended-cache-ttl-2025-04-11`
   — and only that run's `cache_control` carried `"ttl": "1h"`; the
   auth-token run also used `Authorization: Bearer` but got neither flag,
   so the 1h TTL rides the managed-OAuth population specifically,
   confirming §2.3's subscription gating from the outside.
4. **The body is the beta shape.** `context_management` was present in
   every run (`{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}`
   — `keep` is undocumented), `thinking` used `budget_tokens` with a
   `display` field; `max_tokens: 32000`. A serve surface must accept the
   `BetaCreateMessageParams` property set.
5. **`system` is three blocks**, and block 0 is the attribution
   pseudo-header (`x-anthropic-billing-header: cc_version=2.1.247.3b2;
   cc_entrypoint=<entrypoint>;`) with **no** `cache_control` — deliberately
   ahead of the first breakpoint so it never invalidates the cached prefix;
   blocks 1-2 (agent-SDK line, full system prompt) each carry a breakpoint.
   §4.4's per-conversation-stability reading holds at 2.1.247.
6. **`count_tokens` was never called** in single non-interactive turns —
   whether longer interactive sessions call it remains UNVERIFIED; serving
   it stays worthwhile (the fallback probe costs a real create).
7. **A `--continue` second turn resends full history** ([user, assistant,
   user] with the mock's own reply replayed verbatim as the assistant item)
   — full-resend-with-prefix-admission is exactly the right serve model.
8. **Two cautions for anyone rebuilding the rig.** In this CCR container
   the ambient env silently overrides `ANTHROPIC_API_KEY` with the
   host-managed OAuth token (clear the env to test API-key auth), and one
   out-of-protocol probe (`claude config list` under `env -i` with no base
   URL override) still reached a real model through credentials living
   outside both the env and `CLAUDE_CONFIG_DIR` — clearing those two is
   NOT sufficient isolation; keep rigs pointed at loopback via
   `ANTHROPIC_BASE_URL` and treat any un-logged run as having gone to
   production. The ambient run also fired one `GET
   /v1/code/agent-proxy/ca-cert` probe (Bun user-agent, no auth) — CCR
   plumbing, not the Messages protocol.

## 5.6 Addendum (2026-08-28): the binary self-updated to 2.1.251 overnight, and the re-capture caught a blocking change

The M11.1 wiring stage re-ran §5.5's rig against the binary as it now stands
(2.1.251 — it self-updates; §5.5 read 2.1.247 the day before). Fresh fixtures
live in `crates/roundhouse-server/tests/fixtures/`. Where this addendum and
§5.5 disagree, this one wins for the current line. The one-day drift:

1. **NEW AND BLOCKING: `role: "system"` messages inside `messages`**, under
   the new beta `mid-conversation-system-2026-04-07`, on **every** request
   including the first. No prior read (v2.1.42 bundle, 2.1.247 capture, docs)
   had this shape. A serve surface that refuses a system role inside
   `messages` — the natural reading of every earlier source — refuses the
   entire current client line. Sharper still: that message's content arrives
   as a **one-block list on turn 1 and a bare string on the `--continue`
   resend** (text byte-identical), so canonicalization must be
   container-insensitive or every session forks at that item on turn two,
   silently. Both facts are now pinned by serve-surface tests.
2. **The beta set grew and reordered**: turn 1 sends `claude-code-20250219,
   context-1m-2025-08-07, interleaved-thinking-2025-05-14,
   thinking-token-count-2026-05-13, context-management-2025-06-27,
   prompt-caching-scope-2026-01-05, mid-conversation-system-2026-04-07,
   advisor-tool-2026-03-01, effort-2025-11-24, fallback-credit-2026-06-01`;
   turn 2 is identical minus `context-1m-2025-08-07`. New since 2.1.247: the
   last four plus `context-1m`; none of 2.1.247's five dropped.
3. **`thinking` changed shape**: `{budget_tokens, display}` →
   `{"type":"adaptive","display":"omitted"}` with no `budget_tokens` at all;
   `output_config` is `{"effort":"high"}`; `max_tokens` 32000 → 64000; the
   default model string is `claude-opus-5`.
4. **Confirmed stable across the update** (§5.5's claims re-verified):
   `metadata.user_id` still the JSON-object string with `session_id` equal to
   the `X-Claude-Code-Session-Id` header and stable across `--continue`
   (`account_uuid` empty under cleared-env API-key auth); the path still
   `POST /v1/messages?beta=true` with betas header-only; `system` still three
   blocks with the uncached attribution pseudo-header first (now
   `cc_version=2.1.251.6bb`); `context_management` unchanged (`keep` still
   undocumented); `count_tokens` still never called on single
   non-interactive turns.

The lesson is the cadence, not the details: **two captures one day apart
disagreed on a shape that would have refused every request.** The
`anthropic-spec-sync` skill's client-drift half (§7 of the skill) and the
e2e suite's version print are not hygiene — they are what stands between
this surface and a silent client-line refusal.

## 5.7 Addendum (2026-09-01): 2.1.257 re-capture, and a second blocking shape change

M11.2b re-ran §5.5/§5.6's rig against the binary as it stands four days later
— `/opt/node22/bin/claude --version` prints **2.1.257** (§5.6 read 2.1.251).
Rig: `mock_server.py` (stdlib `http.server`, records full headers + raw body
per request, answers the same minimal SSE stream as §5.5/§5.6) on
`127.0.0.1:8931`; two turns — `-p "say hi"`, then `--continue -p "and
again"` — the same two prompts the 2.1.251 capture used, so this is an
apples-to-apples replay, not a new script. Raw captures and the rig live in
the session scratchpad (`capture/req-005.json`, `req-006.json`,
`mock_server.py`); fixtures are committed at
`crates/roundhouse-server/tests/fixtures/claude-2.1.257-{headers,turn-1,turn-2-continue}.json`,
same shape and redaction convention as the 2.1.251 files (only
`metadata.user_id`'s `device_id` edited to an all-zero placeholder).

**Isolation note, itself evidence for §5.5 ¶8.** This box's own Claude Code
session is a Claude Code Remote (CCR) container, and `CLAUDE_CODE_REMOTE=true`
sits in the ambient environment. §1.3's `VV()` only lets an
`ANTHROPIC_API_KEY` source suppress OAuth when `!CLAUDE_CODE_REMOTE` — so on
this box, unsetting a handful of named variables while leaving the rest of
the ambient environment intact is **not** sufficient to reach cleared-env
API-key auth: the first attempt (env vars individually `unset`, not fully
cleared) sent `Authorization: Bearer sk-ant-oat01-…` (the container's real
managed-OAuth token, redacted here as it was locally), the full
`oauth-2025-04-20` + `extended-cache-ttl-2025-04-11` beta pair, and two
headers not in any prior read of this dialect —
`x-claude-remote-container-id`, `x-claude-remote-session-id` — plus a `GET
/v1/code/agent-proxy/ca-cert` probe before *every* turn (CCR plumbing, not
Messages, as §5.5 ¶8 already read once). This is a live confirmation of
§1.3's code reading from the outside, on a fifth variable §5.5 ¶8 did not
yet have a name for. It reproduces on this box with `env -i` retained: only
`HOME`, `CLAUDE_CONFIG_DIR`, `PATH`, `DISABLE_AUTOUPDATER=1`,
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, `ANTHROPIC_BASE_URL`,
`ANTHROPIC_API_KEY`. That run is the one this addendum's fixtures and
findings are drawn from: it sent `x-api-key` (not `Authorization`), no
`oauth-2025-04-20` / `extended-cache-ttl-2025-04-11`, no remote-container
headers, and no `ca-cert` probe on either turn — the cleared-env shape §5.6
fixtures were captured under. **Two auth surfaces are now on record for this
dialect**: cleared-env API-key (`x-api-key`, this addendum's fixtures) and
CCR-ambient managed-OAuth (`Authorization: Bearer sk-ant-oat…`, the two
extra betas, the two remote headers) — a third, a bare non-CCR subscription
login, is predicted by §1.3's read to land on the `Authorization` + OAuth-
beta shape *without* the CCR-specific headers or probe, but was not
attempted (no real login was touched, per the task's constraint).

### Field-by-field drift vs. the 2.1.251 fixtures

| field | 2.1.251 | 2.1.257 | blocking? |
|---|---|---|---|
| betas (turn 1) | `claude-code-20250219, context-1m-2025-08-07, interleaved-thinking-2025-05-14, thinking-token-count-2026-05-13, context-management-2025-06-27, prompt-caching-scope-2026-01-05, mid-conversation-system-2026-04-07, advisor-tool-2026-03-01, effort-2025-11-24, fallback-credit-2026-06-01` | identical minus `advisor-tool-2026-03-01`; order of the rest unchanged | No |
| betas (turn 2) | same list minus `context-1m-2025-08-07` | same, also minus `advisor-tool-2026-03-01` | No |
| `thinking` | `{"type":"adaptive","display":"omitted"}` | unchanged | No |
| `output_config` | `{"effort":"high"}` | unchanged | No |
| `max_tokens` | `64000` | unchanged | No |
| model string | `claude-opus-5` | unchanged | No |
| path + query | `POST /v1/messages?beta=true` | unchanged | No |
| header set (cleared-env API-key) | 21 headers, `x-api-key` present, no `Authorization` | identical set and order | No |
| header set (CCR-ambient) | not captured at 2.1.251 | `+x-claude-remote-container-id`, `+x-claude-remote-session-id`, `Authorization` replaces `x-api-key`, `+oauth-2025-04-20`, `+extended-cache-ttl-2025-04-11` | No — topology-conditional, not a version change; see isolation note |
| `system` block structure | 3 blocks; block 0 uncached attribution (`cc_version=2.1.251.6bb`), blocks 1–2 cached | unchanged shape; `cc_version=2.1.257.1f2` (value only) | No |
| model-identity line, turn 1→2 | `"...Opus 5 (1M context)."` → `"...Opus 5."` as `context-1m` drops between turns | same pattern, same trigger | No |
| `messages` roles, turn 1 | `[user(list, 2 blocks), system(list, 1 block)]` | unchanged | No |
| `messages` roles, turn 2 | `[user(list), system(str), assistant(list), user(list, 1 block w/ cache_control)]` — **4** items | `[user(list), system(str), assistant(list), user(str, no cache_control), system(list, 1 block, cache_control)]` — **5** items | **Yes** |
| `metadata.user_id` | JSON-object string, `device_id`/`account_uuid`/`session_id`, `session_id` = header | unchanged shape and correspondence | No |
| `context_management` | `{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}` | unchanged | No |
| `count_tokens` calls | never, single non-interactive turns | still never (checked against `mock.log`: only `POST /v1/messages?beta=true` and, in the contaminated run only, `GET .../ca-cert`) | No |

### The one blocking finding: a new trailing `system` message on `--continue`, and the cache breakpoint moved with it

At 2.1.251, turn 2's new user turn (`"and again"`) was itself the last
`messages` item: a one-block list carrying the `cache_control` breakpoint.
At 2.1.257, replaying the **identical two prompts**, that user turn is a
**bare string** (`"and again"`, no `cache_control` possible on a string
container) and is followed by a **new, fifth message**:

```json
{
  "role": "system",
  "content": [
    {
      "type": "text",
      "text": "<total_tokens>15000000 tokens left</total_tokens>",
      "cache_control": { "type": "ephemeral" }
    }
  ]
}
```

— i.e. the `cache_control` breakpoint moved off the user turn and onto this
new trailing system-role message, under the same
`mid-conversation-system-2026-04-07` beta the existing skills/agent-types
reminder already used. `15000000` matches the default context-tracking
budget this same CLI reports at the very start of a fresh session (confirmed
by inspection, not asserted from a single coincidence: it is a per-CLI
constant, not per-conversation state, since this subprocess's isolated
`HOME`/`CLAUDE_CONFIG_DIR` had no prior history to derive a budget from).

Driven through the real `wire::canonicalize` (never modified — the fixture
files were swapped in place, tests run, then reverted; see below), this
changes the canonicalized item count `the_shipping_clients_two_turns_are_
one_conversation_but_for_the_prompt_it_changed` asserts on: the test expects
`second.len() == first.len() + 2` ("the answer and the new question"); the
captured 2.1.257 turn 2 yields `first.len() + 3` (`left: 9, right: 8`) —
**the new trailing system message is a third new item**, not folded into
either of the two the pinned test already accounts for. A serve surface
built strictly to the 2.1.251 shape does not *refuse* this request (nothing
here closes an enum or rejects an unknown field — the new item is still an
ordinary `role: "system"` message, which M11.1's review already forced this
surface to accept as `Role::System`), but it **silently miscounts** what a
`--continue` turn appends: any accounting, budget, or prefix-verification
logic written against "`--continue` always adds exactly two items" undercounts
by one on every second-and-later turn of the current client line. That is
the practical blocking-ness: not a 422, a silent drift in what "the new
turn" means.

Two readings of *why* are both consistent with this evidence and neither is
settled by it: (a) genuinely new in 2.1.257 — the token-budget reminder was
added as its own system-role message between .251 and .257; or (b) present
already at 2.1.251 but conditioned on total context size or turn count in a
way the .251 capture's shorter system prompt (24 tools vs. this rig's 21,
see below) never crossed. The apples-to-apples prompts argue for (a), but
this dive did not get a byte-identical-environment 2.1.251 binary to test
against, so it cannot rule out (b).

### Two confounds worth naming so they are never read as drift

1. **Tool count, 21 vs. 24.** `count_tokens_counts_the_declared_toolbox`,
   `the_clients_tool_definitions_reach_the_dispatch_verbatim`, and two others
   fail on `left: Some(21), right: Some(24)`. Diffing the two fixtures' tool
   name lists directly: the 2.1.257 capture is missing exactly `DesignSync`,
   `Monitor`, `PushNotification` and has nothing the 2.1.251 list lacks —
   these are Cowork/interactive-surface tools this rig's plain `-p`
   invocation never declared, not tools the 2.1.257 binary dropped. Not
   evidence of client drift.
2. **Hardcoded session UUID in three F7 tests.** Substituting the fixture
   files under the *same* 2.1.251 filenames necessarily changes the
   `session_id` UUID inside them (this capture's is
   `c0cb70b6-938b-4cbb-a8e8-1b8a60b7c4d8`, not `e13acbde-…`), but
   `f7_the_live_continue_pair_continues_across_ordinary_system_volatility`,
   its control, and `the_captured_client_body_is_served_as_a_conformant_
   stream` all look up the store by the literal 2.1.251 session id compiled
   into the test. Every one of those three fails `stored_items`'s
   `.expect("the session exists")` with `SessionNotFound` — an artifact of
   substituting fixture *content* under a test that also hardcodes a value
   *from* that content, not a serve-surface refusal. A real fixture refresh
   (editing the `.rs`, out of scope here) would update the literal.

### Deliverable 3: running the pinning suite against the current line

Per the task's method: backed up the three `claude-2.1.251-*.json` files,
overwrote them in place with this addendum's 2.1.257 captures (same
filenames — `messages_api_surface.rs`'s `include_str!` at
`tests/messages_api_surface.rs:85-86` embeds by path, unedited), ran
`timeout 300 cargo test -p roundhouse-server --test messages_api_surface`,
then restored the originals from the scratchpad backup (`git diff --stat`
on the three fixture paths and on the `.rs` file is empty afterward — the
substitution left no trace).

Result: **36 passed, 9 failed.** Of the 9: 1 is the real finding above
(`the_shipping_clients_two_turns_are_one_conversation_but_for_the_prompt_it_
changed`, `left: 9, right: 8`); 4 are the tool-count confound; 3 are the
session-UUID confound (which two of them reach only via the cascading
`SessionNotFound` inside `f7_the_live_continue_pair_…`'s own three-turn
sequence, `f7_control_…`, and the F4-live stream test); 1
(`the_shipping_clients_body_becomes_the_prefix_it_will_be_checked_against`)
fails only on the test's literal `cc_version=2.1.251` prefix string — the
attribution block's *shape* (block 0, uncached, first) is unchanged, only
the version number in its text moved, exactly as expected. No test failed
on a rejected request, a parse error, or a 422 — the serve surface's
*acceptance* of the current client line is intact; only the one item-count
assertion is stale, and only because the client's own shape moved.

**No other addendum is warranted**: every field not named above (`thinking`,
`output_config`, `max_tokens`, model string, path, `context_management`,
`metadata.user_id`'s shape, the attribution block's position and cache
policy, turn 1's message shape) re-verified identical to §5.6, four days and
six patch versions later.

### 5.7.1 Three-turn follow-up (2026-09-01)

Extended the §5.7 rig to a third turn — `-p "say hi"`, `--continue -p "and
again"`, `--continue -p "once more"` — under the same isolation (`env -i`
with only `HOME`, `CLAUDE_CONFIG_DIR`, `PATH`, `DISABLE_AUTOUPDATER=1`,
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, `ANTHROPIC_BASE_URL`,
`ANTHROPIC_API_KEY`), same binary (2.1.257). Raw captures in the session
scratchpad (`capture3/req-00{1,2,3}.json`); not committed as fixtures — this
is a follow-up read, not a new pinning-suite input.

**Turn 1:** two `messages` items (`user` list, `system` list — the
skills/agent-types reminder), no trailing `<total_tokens>` notice at all.

**Turn 2:** five items, as §5.7 already read — the notice is a trailing
`role: "system"` message, `content` a one-block list, text
`<total_tokens>15000000 tokens left</total_tokens>`, carrying the
`cache_control: {"type":"ephemeral"}` breakpoint.

**Turn 3:** eight items. Turn 2's notice **is** present in the resent
history, at the same index (position 4, immediately after the resent
`"and again"` user turn) and with byte-identical text — `15000000`
unchanged. But its container is not byte-identical: it is re-serialized
from a one-block list with `cache_control` down to a **bare string**
(`content: "<total_tokens>15000000 tokens left</total_tokens>"`, no
`cache_control` field at all — a string container cannot carry one). This
is the same flattening §5.7 already documented for the *user* turn
(turn 2's `"and again"` arrives at turn 3 as a bare string too, having been
a one-block list with the breakpoint when it was the newest turn). A
**fresh** trailing system message, identical in text and shape to turn 2's,
is appended at the new end of the list (index 7) — list container, one text
block, same text, the `cache_control` breakpoint moved onto it. So the
pattern is not "notice dropped from history, new one appended" but "the
previous turn's cache-breakpoint-bearing item (user turn or notice alike)
is flattened to a plain string in place, and a new notice is appended
carrying the breakpoint forward" — one new `messages` item per turn, not a
history rewrite.

**Value stability probe:** re-ran the same three turns against a modified
rig that reports `usage.output_tokens: 5000` (vs. the default `1`) in turn
2's `message_delta` SSE event, everything else unchanged
(`capture3-variant/req-00{1,2,3}.json`). Turn 3's notice — both the resent
turn-2 copy and the fresh trailing one — still reads `15000000`, byte-for-byte
identical to the unmodified run. **N did not move.** This is consistent
with §5.7's reading that `15000000` is a per-CLI constant (the context-
tracking budget reported at the start of a fresh session) rather than a
figure derived from the `usage` numbers a server reports back — at minimum,
it does not track `message_delta.usage.output_tokens` the way a running
context-consumption counter would have to. Not tested: whether it tracks
`message_start.usage.input_tokens`, a locally-computed token count of the
request body itself, or nothing at all within a single CLI process
lifetime — only the one field the rig controls was varied.

### 5.7.2 Ruling pointer (2026-09-01)

The "undercounts by one" paragraph in §5.7 describes the surface *before*
M11.2b. `../PLAN-anthropic-messages.md`'s 2026-09-01 addendum (R-A) rules
the trailing notice an ephemeral client artifact that `wire::canonicalize`
drops in either container, so a `--continue` contributes exactly two items
on both client lines and nothing downstream ever counts the notice. §5.7.1's
three-turn capture is the evidence that ruling rests on; the cleared-env
isolation it insists on became the e2e suite's `env_clear` guard.

## 5.8 Addendum (2026-09-02): MCP over HTTP as the current client speaks it

M12 preflight: extended the §5.5-§5.7 rig with a second loopback stub — a
minimal Streamable-HTTP MCP server (`mcp_stub_server.py`, stdlib
`http.server`, JSON-RPC over POST at `/mcp`, two tools `status` and
`declare_intent`) — and taught the Messages mock to answer a `tool_use`
block naming an MCP tool so the real 2.1.257 binary calls the stub and
resends the `tool_result`. Rig and raw captures live in the session
scratchpad (`mcp_capture/{mock_server_mcp.py,mcp_stub_server.py,
build_mcp_fixtures.py}`, `mcp_capture/run1b/`, `.../run2*/`,
`.../run3*/`); redacted fixtures are committed at
`crates/roundhouse-server/tests/fixtures/claude-2.1.257-mcp-{headers,
turn-1,turn-2-toolresult,wire}.json`, same convention as §5.7's fixtures
(only the `device_id` hex substituted; the task's own dummy header value
is not a credential and is left verbatim). Isolation per the task's
`env -i` allowlist throughout; every run is one `-p` turn under `timeout
40`. **First-attempt caution for whoever re-runs this rig**: background
the two stub servers with `disown`, and kill strays only by exact PID
matched from `/proc/*/cmdline` — a `pkill -f` pattern loose enough to
match this box's own supervising process ended one shell mid-command
with no output at all (exit 144) during this dive; narrowing to exact
PIDs recovered cleanly.

### The config forms, and which ones the client honours

`claude mcp add-json <name> '{"type":"http","url":...,"headers":{...}}'
-s <scope>` writes the *same* JSON shape for every scope, letting each
scope's own file be read directly rather than guessed:

| Scope | Where it lands | Honoured for a real turn? |
|---|---|---|
| `--mcp-config '<json>'` (CLI flag, a `{"mcpServers":{...}}` object or file path) | nowhere on disk — process args only | **Yes** |
| project (`.mcp.json` in cwd, `-s project`) | `<cwd>/.mcp.json`, top-level `mcpServers` | **Yes**, and in `-p` mode with **no approval step** — see below |
| local (`-s local`, the default) | `~/.claude.json` → `projects["<cwd>"].mcpServers` (per-directory, not shared) | Yes (not separately re-verified this dive; same reader as project scope) |
| user (`-s user`) | `~/.claude.json` top-level `mcpServers` (`CLAUDE_CONFIG_DIR` relocates the file) | Yes (not separately re-verified this dive; same reader as project scope) |
| `settings.json`'s own `mcpServers` key (either `--settings <file>` or `.claude/settings.json` in cwd) | the file as given | **No — confirmed inert.** `claude mcp list` reports "No MCP servers configured", `claude doctor` raises no schema warning about the key (it is silently ignored, not rejected), and a live `-p` turn against a mock declaring 21 built-in tools shows **zero** `mcp__*` entries in `tools[]` when the server is defined only here. The task brief's premise that settings.json's `mcpServers` is a working config form does **not** hold at 2.1.257. |

The custom header (`x-roundhouse-key: capture-dummy-turn-key`, this
task's own value) was verified present on **every** MCP request —
`initialize` through `tools/call` — under both forms that actually
worked: `--mcp-config` and project `.mcp.json`. There is no form where
the header reaches the config but not the wire; the two live where the
JSON reaches the wire at all.

**Precedence, same server name, `--mcp-config` vs. project `.mcp.json`,
no `--strict-mcp-config`:** `--mcp-config`'s definition wins outright.
Two stub servers were configured under the identical name `"roundhouse"`
— one via `.mcp.json` (`x-roundhouse-key: from-dot-mcp-json`), one via
`--mcp-config` (`x-roundhouse-key: from-cli-flag`) — and the `.mcp.json`
stub received **zero** requests (`mcplogA` count 0) while the
`--mcp-config` stub received the full five-request sequence (`mcplogB`
count 5). This is CLI-flag-shadows-project-file, not merge-by-name with
project winning.

**`--strict-mcp-config` excludes `.mcp.json` even under a distinct
name**, not just a colliding one: with `--mcp-config` naming `cliserver`
and `.mcp.json` (same cwd) separately naming `projserver`, `tools[]`
carried only `mcp__cliserver__*` and `projserver`'s stub received zero
requests — its MCP server was never even initialized, matching
`--help`'s "ignoring all other MCP configurations" literally, not just
at the tool-selection step.

**Project-scope servers need no approval in `-p` mode.** `--help`
already says the workspace trust dialog is skipped non-interactively;
this extends to MCP server approval specifically. A fresh, empty
`CLAUDE_CONFIG_DIR` with a project-only `.mcp.json` (no `--mcp-config`,
no `--strict-mcp-config`) ran the tool to completion in one shot — no
prompt, no `⏸ Pending approval` state, exit 0 — and the post-run
`~/.claude.json` carried **no `projects` key at all**, i.e. `-p` mode
didn't even persist a trust decision to skip on a second run; it simply
never gated the first one. (`get`/`list`'s "⏸ Pending approval" state,
per `--help`, is an interactive-mode concept this rig never exercised.)

### `--allowedTools` naming the MCP tool vs. not, and what else does or doesn't unblock it

Without `--allowedTools` naming `mcp__roundhouse__status`, in `-p` mode
the client does **not** call the tool. It still completes `initialize`,
`notifications/initialized`, the optional `GET` SSE open, and
`tools/list` against the stub (so schema discovery for `tools[]` on the
Messages request is unconditional and permission-independent), but
`tools/call` is never sent. Instead the client synthesizes its own
`tool_result` and resends it to the model without asking the server
anything:

```json
{"type":"tool_result","is_error":true,"tool_use_id":"toolu_mock_001",
 "content":"Claude requested permissions to use mcp__roundhouse__status, but you haven't granted it yet."}
```

With `--allowedTools "mcp__roundhouse__status"` (naming the flat
`mcp__<server>__<tool>` form), the same run instead sends the real
`tools/call` and the round trip completes — reproduced identically
across three independent runs (`run1b`, `run1b-rep2`, `run1b-rep3`: same
exit 0, same 2 Messages requests, same 5 MCP requests each time).

Two other unblock paths were tried and both failed to help, for
different reasons, worth recording so nobody re-derives them by surprise:

- `--permission-mode bypassPermissions` (`--dangerously-skip-permissions`'s
  equivalent) refused outright before any network I/O — zero Messages
  requests, zero MCP requests, exit 1, stderr:
  `--dangerously-skip-permissions cannot be used with root/sudo
  privileges for security reasons`. This is an environment fact (this
  rig runs as root), not an MCP-specific finding, but it means this dive
  could not observe the fully-auto-approved shape and had to rely on
  `--allowedTools` for the clean capture.
- `--permission-mode dontAsk` **denies** rather than auto-approves an
  MCP tool call absent `--allowedTools` — its name is directional
  ("don't ask [before denying]"), not permissive. The synthesized
  `tool_result` differs from the no-flag case, confirming a distinct
  code path rather than a no-op:
  `"Permission to use mcp__roundhouse__status has been denied because
  Claude Code is running in don't ask mode. IMPORTANT: You *may*
  attempt to accomplish this action using other tools..."` — again zero
  `tools/call` requests reached the stub.

So for a headless `roundhouse`-fronted client, `--allowedTools` naming
the flat tool name (or a server-level wildcard, not itself tested this
dive) is the confirmed unblock; the task's other candidate,
`--permission-mode`, does not substitute for it at any setting tried.

### The MCP wire itself: initialize through tools/call, in order

Captured verbatim in `claude-2.1.257-mcp-wire.json` (five requests,
`initialize` → `notifications/initialized` → `GET` → `tools/list` →
`tools/call`). Load-bearing shapes:

1. **Transport is Streamable HTTP**, JSON-RPC 2.0 over `POST /mcp`
   (the path from `--mcp-config`'s `url`), `Accept: application/json,
   text/event-stream`, `Content-Type: application/json`,
   `Accept-Encoding: identity` (no compression negotiated for MCP,
   unlike the Messages side's `gzip, deflate, br, zstd`).
   `User-Agent: claude-code/2.1.257 (sdk-cli)` — note this differs in
   form from the Messages client's `claude-cli/2.1.257 (external,
   sdk-cli)` (§2.2): two different HTTP clients inside the one process.
2. **`initialize`** (`id: 0`) requests `protocolVersion: "2025-11-25"`,
   `capabilities: {roots:{listChanged:true}, elicitation:{}}`, and a full
   `clientInfo` block (`name`, `title`, `version`, `description`,
   `websiteUrl`) — richer than the bare name/version the 2025-03-26 spec
   requires. The stub's response carries a fresh `Mcp-Session-Id`
   header; from the *next* request on, the client echoes it back
   verbatim on `Mcp-Session-Id` and additionally sends
   `Mcp-Protocol-Version: 2025-11-25` on every subsequent request
   (`notifications/initialized`, `GET`, `tools/list`, `tools/call`) —
   session binding plus protocol-version pinning are both header-level,
   not body-level, after the handshake.
3. **`notifications/initialized`** is a bodyless-result JSON-RPC
   notification (no `id` key at all) — the stub answered bare `202` with
   no body, which the client accepted without complaint.
4. **The optional server→client `GET /mcp` SSE stream is opened**
   (`Accept: text/event-stream` only, same session/protocol headers, no
   body) even though this single-turn, single-tool-call flow never
   needed a server-initiated message. Whether a longer session ever
   closes and reopens it, or relies on one long-lived GET for the whole
   process lifetime, is unobserved — this rig's turns each spawn a fresh
   process.
5. **`tools/list`** (`id: 1`) takes no params; the response's two tools
   flow into the Messages `tools[]` array with the flat
   `mcp__<server>__<tool>` name prepended and the `description` /
   `inputSchema` passed through as `input_schema` **verbatim** — same
   key name change (`inputSchema` → `input_schema`) as Anthropic's own
   tool-schema convention, everything else (`type`, `properties`,
   `required`, `additionalProperties: false`) byte-identical. No
   `strict` or `defer_loading` field is added for an MCP-sourced tool
   (those are the built-in-tool-schema features §5 pitfall 6 names).
6. **`tools/call`** (`id: 2`) is where the flat name gets **split back
   apart**: `params.name` is the bare `"status"` (server prefix
   stripped), `params.arguments` is the parsed JSON object the model's
   `input_json_delta`s accumulated, and `params._meta` carries
   `{"claudecode/toolUseId": "toolu_mock_001", "progressToken": 2}` — the
   Anthropic `tool_use.id` riding through as a **namespaced `_meta` key**
   (`claudecode/` prefix) rather than a bare field, plus a numeric
   `progressToken` for the (unused, in this stub) progress-notification
   channel. This directly resolves `PLAN-anthropic-messages.md` §5 open
   question 3's "owed" item: the reverse split for
   `ClientDialect`'s flat-tool-name arm is `mcp__<server>__<tool>` →
   `(server, tool)` by splitting on the first two `__` occurrences (the
   tool name itself may contain further underscores), and the
   `_meta["claudecode/toolUseId"]` field — not any envelope-level
   field — is what a serve surface must thread back to the Anthropic
   `tool_use.id` when relaying the result.
7. **The tool's `content` array flows straight into `tool_result.content`
   unmodified** — the stub returned
   `{"content":[{"type":"text","text":"stub-result: tool=status
   args={}"}],"isError":false}` and the resent Messages body's
   `tool_result.content` is that same one-block array, byte for byte.
   `isError` was not exercised as `true` this dive (the denial paths in
   the previous section are client-synthesized, not server-signalled,
   so they don't test MCP's own `isError` field).

### What this leaves for the M12 design, unresolved by this evidence

- The `_meta["claudecode/toolUseId"]` key is confirmed as the join
  point for *this* binary; whether it is a documented, versioned contract
  or an implementation detail that could rename between releases (the
  way `metadata.user_id`'s shape moved between §5.5 and later reads) is
  not knowable from one client line's capture — the spec-sync discipline
  in `CLAUDE.md` applies here as much as to the Messages surface itself.
- User- and local-scope config files were read (via `claude mcp add-json
  -s user`/`-s local`) but not independently re-run through a live turn
  this dive — the assumption that they share project-scope's reader (and
  therefore project-scope's "no approval needed in `-p` mode" finding)
  rests on the identical on-disk shape, not a second live capture.
- Server-level tool wildcards for `--allowedTools` (e.g. `mcp__roundhouse`
  admitting every tool the server declares) were not tried; only the
  fully-qualified single-tool name was.
- `isError: true` from the MCP server's own `tools/call` response — as
  opposed to a client-synthesized permission denial — was not captured
  and may shape `tool_result.is_error` differently.

## 6. Open questions — decisions this evidence does not make

1. **Key prefix admission on the header or on `metadata.user_id`?** The header is
   documented today, absent in v2.1.42, and free of body parsing; `user_id` is
   present in v2.1.42, survives compaction and `--resume`, but requires reading
   the body and splitting on a literal. `claude-code-router` reads both, header
   first. Both ways are defensible; the trade is "clean seam, version-fragile"
   against "robust, but couples admission to body parsing." *(Also unresolved:
   whether roundhouse should treat the two as one key or two, given that a
   teammate subprocess changes the `_session_` component while
   `x-claude-code-parent-agent-id` would name the parent.)*
2. **Whether roundhouse should hold a subscription OAuth Bearer at all.** The
   client will hand it over unconditionally (§1.4) and the docs endorse the
   topology (§1.4 quote). Nothing in this dive establishes what Anthropic's terms
   permit a third party to do with a forwarded `sk-ant-oat`-class token, and I
   could not read a live credential to confirm even its prefix. Legal/ToS input,
   not evidence, decides this.
3. **The attribution-block discrepancy.** Docs say the block was per-request
   before v2.1.181; v2.1.42 as read is per-conversation (§4.4). Either the
   per-request token was introduced *after* 2.1.42 and removed by 2.1.181, or the
   docs' version boundary is approximate. Resolving it needs a build between the
   two, which is not on this box.
4. **Whether to emit `ping` events, comment keep-alives, or both.** The
   300-second watchdog counts bytes, so comments suffice for liveness; §3.2 says
   a `ping` event is explicitly skipped by the parser, so it is also free. No
   source establishes a required cadence, only the 300 s ceiling and v2.1.42's
   30 s stall-logging threshold.
5. **Whether roundhouse should serve `/v1/models`.** Discovery is opt-in
   (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`) and filters entries whose
   `id` contains `claude` or `anthropic`. Exposing roundhouse's frontier catalog
   through it would surface routes in the `/model` picker; declining keeps the
   router's choices invisible to the user. Both are coherent products.
6. **What to do with the non-streaming fallback.** A malformed SSE stream costs
   the upstream a second full turn (§3.6). Whether roundhouse should *ever* serve
   `stream:false` for a main turn, or hard-fail instead, is a budget question this
   dive does not answer.
