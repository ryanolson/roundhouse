---
name: anthropic-spec-sync
description: Fetch the current Anthropic Messages API OpenAPI spec, diff it against roundhouse's pinned vocabulary, update the protocol pin and wire module, and fix the codebase test-first where the API moved. Use for syncing the Anthropic spec pin, checking Anthropic API drift, refreshing the Messages wire vocabulary, or before any milestone that depends on the anthropic_messages dialect.
---

# Anthropic spec sync

The `anthropic_messages` wire module is hand-written against a pinned snapshot
of Anthropic's OpenAPI spec (the ruling is `agent-docs/PLAN-anthropic-messages.md`
R1; the evidence is `agent-docs/research/anthropic-messages-wire-crates.md` §2–§3).
A pinned snapshot rots silently: Anthropic ships betas weekly, the spec URL is
content-addressed with **no `latest` alias**, and nothing breaks loudly when the
API grows a field we mis-handle. This skill is the rot-prevention loop the
CLAUDE.md synergy-vigilance rule demands for this dependency: fetch, diff,
update the pin, fix what broke — test-first — and record what moved as a dated
addendum. Run it **before any milestone that touches the dialect**, and on any
recurring cadence the operator sets; a run that finds nothing changed is cheap
and still worth the date-stamp.

## Where the pin lives

- **After M11.0 lands:** `crates/roundhouse-fleet/src/anthropic_messages/spec_pin.json`
  — the machine-readable pin: `spec_sha256`, `spec_url`, `source_sdk_rev`
  (the `anthropic-sdk-typescript` revision whose `.stats.yml` named the URL),
  `fetched` (date), and the `vocabulary` object the wire module's pinning
  tests read (see "The vocabulary contract" below). The wire module's tests
  `include_str!` this file; changing it and re-running the tests is what turns
  an upstream move into a red worklist.
- **Before M11.0 lands** (the fixture does not exist yet): the pin is the one
  recorded in `agent-docs/research/anthropic-messages-wire-crates.md` §0/§2
  (sha256 `942a1163…3d2ee87` from `anthropic-sdk-typescript@7ba6a3fc`). Limit
  the run to steps 1–3 and record the delta as a dated addendum in that
  document — there is no code to update yet, and the addendum is what keeps
  M11.0's implementation brief honest.

## The loop

Run `python3 .claude/skills/anthropic-spec-sync/scripts/spec_sync.py --help`
first — it automates steps 1–3 and prints the structured diff. Work in the
session scratchpad, never in `/tmp`.

### 1. Discover the current spec

Clone `anthropic-sdk-typescript` shallow at its default branch, record
`git rev-parse HEAD`, and read `.stats.yml` — `openapi_spec_url` names the
current content-addressed spec URL (the sha256 of the spec body is embedded in
the filename). If the URL equals the pinned one, the spec has not moved:
record a one-line dated re-verification note next to the pin (in the fixture's
`fetched` history or the evidence doc), and stop — do not manufacture work.

### 2. Fetch and verify

Download the spec and record the sha256 **you compute over the body**. Three
identifiers exist and none may be conflated (established empirically,
2026-08-27): the 64-hex hash in the URL filename and `.stats.yml`'s 32-hex
`openapi_spec_hash` are both opaque Stainless-internal content addresses —
neither hashes the raw body. So: a *changed URL* means the spec moved; an
*unchanged URL* with a changed body sha256 means a broken download (the
storage is immutable) — refuse and re-fetch, never proceed. The pin records
all three (URL, our body sha256, `openapi_spec_hash`) so any of them can be
compared later without re-deriving this paragraph.

### 3. Diff the vocabulary

Extract from old and new specs the exact vocabulary the pin carries, and diff
structurally (the script does this):

- `StopReason` enum values
- `Usage` property names, and `CacheCreation`'s field names
  (`ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens` today)
- `MessageStreamEvent` union members and the `*ContentBlockDelta` variants
- the response `ContentBlock` union members
- `CreateMessageParams` and `BetaCreateMessageParams` property names, and
  which carry `additionalProperties: false`
- `CacheControlEphemeral`'s `ttl` values
- `AnthropicBeta`'s named enum values (the open-enum shape itself —
  `anyOf [string, enum]` — is load-bearing; flag if it closes)
- `Message` top-level property names
- the non-beta vs `?beta=true` path split (flag if the beta-path convention
  changes shape)

Remember what the spec does NOT carry, so its absence is never read as a
removal: the SSE transport events `ping` and mid-stream `error` (evidence doc
§3.1). Those are pinned by the wire module's own tests, not by the spec.

### 4. Update the pin (the protocols)

Write the new `spec_pin.json`: new sha, new URL, new `source_sdk_rev`, new
date, new vocabulary. Then run the wire module's pinning tests, bounded, per
house rule:

```bash
timeout 300 cargo test -p roundhouse-fleet anthropic_messages
```

Every failure is a named upstream move. A green run after a vocabulary change
means the change was additive in a direction the module is deliberately open
to — still record it (step 6), because "open to it" and "handled well" are
different claims.

### 5. Fix the codebase where it broke — test-first, classified

House rule from CLAUDE.md applies unchanged: **write the failing test first,
then rule, then fix.** For each delta, classify before touching code:

- **New response field / new content block / new beta value** — the shipped
  types are open (flattened extras, opaque blocks, `Unknown` arms), so
  nothing *parses* wrong. The questions are semantic: does a new usage field
  belong in accounting (the `cache_creation` breakdown precedent)? Does a new
  block type need an item mapping for prefix admission to round-trip it, or
  does opaque passthrough suffice? Decide per field, and say why in the
  commit.
- **New `stop_reason` value** — flows to `Unknown(String)` today. Decide
  whether the engine must *act* on it (a new terminal-vs-continue semantic is
  load-bearing; a new flavor of "done" may not be) and add the typed arm with
  a test when it must.
- **New request field** — passthrough serves it upstream untouched; check
  only that the serve surface's validation does not refuse it and that
  dispatch does not strip it (the half-forwarded header/body pairing trap in
  `research/claude-code-client-surface.md` §5.6).
- **Removed or renamed field, changed type, closed enum** — breaking. Failing
  test first, then fix the wire module, then chase every consumer the
  compiler names — and then the ones it cannot: the silent-gap list in
  `research/anthropic-messages-seam-map.md` §5 is the checklist of places a
  wire change does not go red on its own.
- **Strict-oracle drift** — the dev-only strict parser (oracle tier 1) is
  *deliberately* closed, so upstream additions turn it red before they turn
  anything else red. That is its job; update it with the pin, never loosen it
  to make the diff quiet.

Then the full bounded suite: `timeout 900 cargo test --workspace`.

### 6. Record the move

- A **dated bracketed addendum** to
  `agent-docs/research/anthropic-messages-wire-crates.md` §2–§3: old→new
  sha256, source SDK rev, the vocabulary diff, and what changed in code as a
  result (or "nothing — additive in open directions", with the classification
  from step 5). Never silently rewrite the original claims.
- If the wire module's pin comment names counts or values that moved, update
  them in place with the same dated-correction style the
  `roundhouse-relay/Cargo.toml` pin comment uses.

### 7. Optional second half: client drift

The spec is one half of rot; the *client* is the other. When asked for a full
sync (or when a milestone depends on client behavior), also:

- compare `claude --version` on the box against the version the e2e suite's
  vigilance print last recorded, and against the version gates named in
  `research/claude-code-client-surface.md`;
- re-fetch `code.claude.com/docs/en/llm-gateway-protocol` and diff its
  request-headers and beta guidance against that document's §1.5/§4.1;
- if the serve surface exists, re-run the loopback capture rig against the
  current binary and diff the captured request shape against the last
  recorded capture.

Client drift lands as a dated addendum to `claude-code-client-surface.md`,
same rules.

## What this skill must never do

- Loosen an open/closed polarity to make a diff quiet (shipped-open,
  oracle-strict is a ruling — R1/R6 — not a default).
- Update the pin without running the pinning tests, or fix a break without a
  failing test first.
- Treat "the URL changed" as "everything changed" — diff the vocabulary; most
  spec churn is additive and lands as a recorded no-op.
- Rewrite evidence. Addenda are dated and bracketed; the original snapshot
  claims stand as claims about their revision.
