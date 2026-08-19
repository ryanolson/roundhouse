<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Working on Roundhouse

Project-specific practice. The architecture itself is documented in `README.md`
and in module-level docs; this file is about *how we work*, not what the code
does.

## Every test run is bounded

Run `cargo test` / `cargo nextest` under a coreutils timeout, always:
`timeout 900 cargo test --workspace` for the full suite, ~`timeout 300` for a
targeted one. A PreToolUse hook (`.claude/hooks/cargo-test-timeout.sh`)
enforces this mechanically.

The reason is not slow tests — it is that a hung test hangs the entire cargo
run, silently. The sessions most likely to hang one are exactly the sessions
this repo runs on purpose: adversarial reviews that *mutate timeout and
deadline code* to prove a guard test is load-bearing. Break a timeout path and
its guard does not go red — it waits forever. A bounded run converts "stalled
for hours" into "exit 124 in minutes"; on 124, suspect the newest test or the
mutation you just applied, and re-run the suspect binary with
`--test-threads=1 --nocapture` under a short timeout to name the hang.

## Validating a claim

**Write the test first, then rule, then fix.** This applies to review findings,
bug reports, hypotheses about behavior, and "I think X is broken" of any kind. A
claim confirmed by reading code is an opinion; a claim confirmed by a failing
test is a fact, and the same test proves the fix.

The order is not negotiable:

1. **Write a test that fails** for the reason the claim says it should. If it
   passes, the claim is wrong — or the test does not exercise what the claim is
   about, which is worth knowing before anyone writes a fix.
2. **If it cannot be tested, make it testable.** A defect that no test can reach
   is usually telling you the seam is in the wrong place. Add the accessor,
   extract the pure function, split the type — the change that makes the
   behavior observable is part of the fix, not a detour around it. Prefer
   additive, behavior-preserving changes so the failing test is unambiguously
   about the defect and not about the refactor.
3. **Rule on the claim** with the failure output as evidence. "Valid",
   "partially valid" (the defect is real but the described mechanism is wrong),
   or "invalid" are all useful answers; an external reviewer being mistaken is
   an ordinary outcome, not an awkward one.
4. **Only then fix it**, and keep the test.

The order is about evidence, not about stopping. Validating and fixing in one
pass is the normal case — the test comes first so the fix is aimed at the real
defect, not so anyone waits for permission between the two. Ruling and fix land
together unless something genuinely needs a decision: the finding turns out to
be a design question, the fix is far larger than the defect, or two valid
remedies point different ways. Then say so and ask.

Where validation really does land before the fix, mark the failing assertions
`#[ignore = "<finding>: <why it fails>"]` rather than leaving the suite red or
deleting the evidence, and keep any passing control tests live — the controls
are what prove the failing ones are not tautological. Be honest about what that
buys: **an ignored test enforces nothing.** It is documentation with a `cargo
test -- --ignored` entry point, not a safety net, so removing the ignore is the
first step of the fix and not a cleanup afterwards. A defect fixed while its
test stays ignored has bought nothing at all.

A finding that is real but whose stated mechanism is wrong must be reported as
partially valid with the correction spelled out. Fixing the described mechanism
rather than the actual one leaves the defect in place behind a passing test,
which is worse than not having looked.

## Choosing a model for subagents and workflows

Match the model to the *kind of thinking* the step needs, not to how important
the overall task feels. A workflow that runs every stage on the largest model
is not more correct, only slower.

| Use | For |
|---|---|
| **Opus** (`claude-opus-5`) | Load-bearing reasoning: judging whether an invariant actually holds, designing a type that makes an invalid state unrepresentable, tracing a lifecycle across modules, deciding whether a finding is real. Anything where being wrong is expensive and the answer is not lookup-shaped. |
| **Sonnet** (`claude-sonnet-5`) | Bounded work with a checkable answer: "does anything call this function", "does this parse", mechanical refactors, running a suite and reporting what failed, writing a test for a behavior someone has already characterized. |
| **Fable** (`claude-fable-5`) | High-volume shallow passes where a wrong answer is cheap and immediately visible — bulk extraction, formatting sweeps, first-pass triage that something else verifies. |

Two rules that matter more than the table:

- **Adversarial stages should not run the model that produced the claim.** A
  verifier sharing the author's blind spots agrees for the same wrong reason.
  Opus finds, Sonnet refutes — the disagreement is the signal.
- **Escalate on ambiguity, not on stakes.** If the step has one checkable
  answer, a smaller model gets it right and gets it right faster. Reach for
  Opus when the step requires judgment that could reasonably go either way.

## Cost and pricing data

Rate cards never go in source — `roundhouse-fleet/src/frontier.rs` states the
rule and `ROUNDHOUSE_CATALOG` is the mechanism. When sourcing prices or looking
for a hosted counterpart to a locally served model, **openrouter.ai** carries
comparable per-model pricing across providers and is the intended input to the
correlary table: it is one place where the hosted equivalents of an open-weights
model we serve ourselves can be priced against each other.

OpenRouter also publishes per-model **intelligence indexes and benchmark
scores**, and those are the natural source for the other half of a catalog
entry: `quality_prior`, which is what the capability gate in
`roundhouse-core/src/metrics/pricing.rs` compares when it decides whether two
models may be priced against each other. Today that number is hand-written
configuration, and `FrontierModelSpec` says so — "configuration, not
measurement". Sourcing it from a published index makes the gate defensible
rather than asserted, which matters because the gate is the only thing stopping
a small local model being priced against a flagship.

Three cautions if any of that becomes an import rather than a lookup:

- OpenRouter prices a *route to* a model, so the same model appears at several
  prices depending on the upstream provider. Pick deliberately rather than
  taking the first — and note that the catalog boundary now **rejects** two
  entries for one `(provider, model)`, precisely because the router and the
  dashboard would otherwise resolve that ambiguity differently.
- Normalize an index to the 0.0..=1.0 scale `quality_prior` is defined on, and
  record which index and which snapshot date it came from. An unversioned score
  silently re-ranks models when the upstream leaderboard moves.
- A price is not a capability claim, and neither is a benchmark score a price.
  Keeping them separate fields sourced from separate columns is what stops a
  cheap lookup from inflating the one number the whole dashboard is judged by.

## Comment and doc style

Match the surrounding code. This codebase explains *why* a decision was made and
what the alternative would have cost, not what the line does. A comment that
restates the code is noise; a comment that records the failure mode a design
avoids is why the next person does not undo it.
