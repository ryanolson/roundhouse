# Ledgerline Payments — Service Reference (v3.2)

> This document is the shared, stable context for the roundhouse cache-aware-routing demo. It is sent
> verbatim as the system prompt (`instructions`) on **every** turn of **every** session, by
> **every** user. That is the point: it is the long, identical prefix whose reprocessing cost the
> demo measures. It is fictional; any resemblance to a real payments system is incidental.

## 1. Overview

Ledgerline is an internal payments service that authorizes, captures, and settles
card transactions for the company's storefronts. It is a Rust service (the
`ledgerline` crate) fronted by an HTTP API and backed by PostgreSQL and Redis.

The service is organized into four layers:

1. **Ingress** (`src/ingress/`) — HTTP handlers, request validation, idempotency.
2. **Domain** (`src/domain/`) — the payment state machine and money math.
3. **Adapters** (`src/adapters/`) — outbound calls to card networks and the bank.
4. **Store** (`src/store/`) — Postgres and Redis persistence.

## 2. The payment lifecycle

A payment moves through a fixed set of states, defined in
`src/domain/state.rs` as the `PaymentState` enum:

- `Created` — the payment intent exists; no money has moved.
- `Authorized` — the card network approved a hold for the amount.
- `Captured` — the held funds were claimed; this is the billable moment.
- `Settled` — the acquiring bank confirmed funds transfer (T+1 or T+2).
- `Refunded` — a captured payment was reversed, fully or partially.
- `Voided` — an authorization was released before capture (no funds move).
- `Failed` — a terminal error state; carries a `FailureReason`.

Legal transitions are enforced by `PaymentState::can_transition_to`. The only
transitions permitted are:

- `Created -> Authorized | Failed`
- `Authorized -> Captured | Voided | Failed`
- `Captured -> Settled | Refunded`
- `Settled -> Refunded`

Any other transition returns `DomainError::IllegalTransition` and is never
persisted. `Refunded`, `Voided`, and `Failed` are terminal.

## 3. Money

All amounts are integer **minor units** (cents), never floats. The `Money` type in
`src/domain/money.rs` wraps an `i64` and a `Currency`. Adding two `Money` values of
different currencies is a compile-time impossibility — `Currency` is a type
parameter — so a mixed-currency sum cannot be written, let alone executed.

Rounding happens only at one place: `Money::allocate`, which splits an amount into N
parts without losing or inventing a cent (the remainder is distributed one cent at a
time to the first parts). Partial refunds use `allocate`.

## 4. Idempotency

Every mutating request carries an `Idempotency-Key` header. The ingress layer
(`src/ingress/idempotency.rs`) stores the key in Redis with the hash of the
canonicalized request body and the eventual response. Rules:

- Same key + same body hash → the stored response is replayed; the handler does not
  run twice.
- Same key + **different** body hash → `409 Conflict` with code
  `idempotency_key_reuse`. This is how a client retrying with a mutated payload is
  stopped from double-charging.
- Keys expire from Redis after 24 hours.

The idempotency record is written **before** the domain mutation and marked
complete **after**, so a crash mid-turn leaves a "pending" record that a retry can
detect and wait on rather than racing.

## 5. Authorization holds and expiry

An `Authorized` payment holds funds for a network-defined window (default 7 days,
carried as `auth_expires_at`). A background job, `expire_stale_auths`
(`src/domain/jobs.rs`), runs every 5 minutes and transitions any `Authorized`
payment past its expiry to `Voided` with `FailureReason::AuthExpired`. Capture on
an expired authorization returns `DomainError::AuthorizationExpired`.

## 6. Adapters and retries

Outbound calls to the card network go through the `CardNetwork` trait
(`src/adapters/network.rs`). The production implementation retries on transient
errors (network timeouts, HTTP 5xx) with exponential backoff, capped at 3 attempts.
It never retries a **decline** — a decline is a business answer, not a failure, and
retrying it would be both useless and, for some networks, a fraud signal.

Every adapter call is wrapped in a `Timeout` (default 4s). A timeout is a transient
error and is retried; the 3-attempt cap still applies, so the worst-case latency of
a single capture is bounded at roughly 12 seconds plus backoff.

## 7. Settlement reconciliation

The bank sends a settlement file nightly. `src/domain/reconcile.rs` matches each
line against a `Captured` payment by `(acquirer_ref, amount)` and transitions
matches to `Settled`. Unmatched lines are written to a `reconciliation_exceptions`
table for a human to review; they are never auto-resolved, because a mismatch
between what we captured and what the bank moved is exactly the thing a human must
see.

## 8. Errors

`DomainError` (`src/domain/error.rs`) is the single error type the domain layer
returns. It is `#[non_exhaustive]`. Ingress maps each variant to an HTTP status:

- `IllegalTransition`, `AuthorizationExpired` → `409 Conflict`
- `InsufficientFunds`, `Declined` → `402 Payment Required`
- `NotFound` → `404`
- `Validation` → `422`
- anything else → `500`, and the internal detail is logged but never returned.

## 9. Configuration

Configuration is environment-driven, loaded once at boot into a `Config` struct.
There are no runtime config reloads: a value that changes mid-process would let two
requests in the same second be judged by different rules. Notable knobs:

- `LEDGERLINE_AUTH_WINDOW_DAYS` (default 7)
- `LEDGERLINE_CAPTURE_TIMEOUT_MS` (default 4000)
- `LEDGERLINE_MAX_RETRIES` (default 3)
- `LEDGERLINE_DATABASE_URL`, `LEDGERLINE_REDIS_URL` (no defaults; boot fails if unset)
