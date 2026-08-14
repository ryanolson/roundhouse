<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Working on Roundhouse

Project-specific practice. The architecture itself is documented in `README.md`
and in module-level docs; this file is about *how we work*, not what the code
does.

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

Where validation lands before the fix does, mark the failing assertions
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

Two cautions if that becomes an import rather than a lookup. OpenRouter prices
a *route to* a model, so the same model appears at several prices depending on
the upstream provider — pick deliberately rather than taking the first. And a
price is not a capability claim: the capability gate in
`roundhouse-core/src/metrics/pricing.rs` exists precisely because cheap
lookups make it easy to price a small model against a flagship, which inflates
the one number the whole dashboard is judged by.

## Comment and doc style

Match the surrounding code. This codebase explains *why* a decision was made and
what the alternative would have cost, not what the line does. A comment that
restates the code is noise; a comment that records the failure mode a design
avoids is why the next person does not undo it.
