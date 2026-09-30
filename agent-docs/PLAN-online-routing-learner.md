<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Plan: online routing learner

> **Status: accepted plan, 2026-09-28.** This plan supersedes the open questions of `DRAFT-online-routing-learner.md` (revision 3), sections 17 and 18, as of 2026-09-28. The owner ruled on them in the two 2026-09-28 addenda of `synergies/typesafe-selector-and-cache-affinity.md`. The draft stays in place and holds the full design rationale. This plan does not copy it. Where this plan and the draft disagree, this plan wins. The tree was read at `520eda5` on branch `ai/cache-ledger-marker-fact`. No learner code exists at that revision.

## 1. What this plan is

The draft describes a live learner that selects a serving strategy on each turn, for each project. Sections 3 to 15 of the draft are the design. Section 16 is its test-first sequence. Sections 17 and 18 list what waited for the owner. The owner has now ruled. This plan records each ruling as it applies to the learner, resolves the places where the draft and the rulings disagree, and breaks the work into milestone PRs.

Read in this order: `CLAUDE.md`, the two 2026-09-28 addenda in the synergy ruling, this plan, then the draft section that a milestone names.

The product sentence still decides every call: a router that optimizes cost alone ships worse answers. The learner therefore satisfies quality first, then minimizes cost within a latency limit. The rulings below give the quality signal, the latency basis, the cost basis, and the starting numbers.

## 2. The rulings, as they apply to the learner

Each row names the ruling, what the learner does, and the milestone that builds it. The rulings are quoted from the synergy addenda of 2026-09-28. The numbers in parentheses are the ruling numbers there.

| Ruling | What the learner does | Milestone |
|---|---|---|
| No quality evidence (7): `serve_rules` is the default | `on_infeasible` defaults to `serve_rules`. `refuse` is an accepted value. On an infeasible turn the learner serves the `rules` route, records the unmet constraints in `LearnedEvidence`, and marks the decision `ConstraintUnmet`. Hard budget and policy constraints still apply through admission. Credit still flows for the served trajectory, as in `shadow` mode. | M4, M8 |
| Exploration (8): yes, bounded | A `live` project can explore. The exploration set holds `Unproven` strategies whose first target costs strictly less than the reference target and meets every hard constraint. The turn explores only when the session's validation arm consults the judge, the admitted pool holds a frontier target, and the draw is below the configured rate. The rate defaults to 5%. The member is chosen uniformly. `BelowFloor` strategies are never explored. (Amended 2026-09-30: `rules` also joins the set whenever its plan meets every hard constraint, exempt from the unproven and cheaper filters. See "`rules` in the live exploration set" in section 9.) | M4, M8 |
| Credit (9): consistent-trajectory credit | Draft section 8.2 is accepted as written. A strategy receives an interval only when its plan matched the served first target on every covered turn. A failover in the interval credits nothing. | M5 |
| Latency limit (10): first output from turn start | The learner models first output from turn start as three terms: the quoted TTFT of the target, the mean residual of that target from the served `Routed` to the first non-empty `OutputTextDelta`, and one project-level mean of the overhead from `TurnStarted` to the served `Routed`. The sum is compared with `latency_limit_ms`. | M3, M5 |
| Local cost (6, 11): a configured price | The learner reads the candidate's `expected_cost_usd` and holds no local price of its own. A separate follow-up adds the optional local price per million tokens to the catalog. Until a project's catalog sets it, a local plan quotes $0 and wins on cost. M11 makes a configured local price a precondition for `live` on a recipe with local targets. | M11 |
| Estimand (12): logged-boundary conditional interval value | The calibrator publishes this estimate from the existing interval data, labeled as the conditional interval value and never as session value. Session deployment value waits for its own protocol. | M10 |
| Starting numbers (13) | `quality.floor = 0.8`, `quality.min_sessions = 20`, `latency_limit_ms = 10000`. Promotion from `shadow` to `live` needs an M10 report that shows three results: the lower bound of the candidate's positive rate is no more than 2 points below the `rules` rate, the estimated cost is at least 10% lower, and the latency limit is met at p50. The owner approves each promotion. | M8, M10, M11 |
| Arms and background sampling (14) | Strategy arms: `rules`, `efficient`, `capable`. No target arms. Background evaluation is the classification runtime at its configured rate and budget. The learner adds no value-of-information scheduler and buys no other background evaluation. | M2 |
| Jev scout (J1): one more question | The background classification call asks one more choice question: which tier fits this turn, `capable` or `efficient`. The same request carries it. The answer and its confidence are recorded on the classification. | M1 |
| Jev scout (J2): agreement comparison | `/v1/metrics` reports, for each project, how often the served tier matches Jev's pick. For each disagreement it reports the label of the covering frontier review. | M1 |
| Jev scout (J3): cold-start prior | Until a learner key has enough review evidence, Jev's tier answers on that key supply a prior for each strategy. The prior is a fixed small number of pseudo-observations. Real intervals replace it. The prior never counts toward `min_evidence` or `min_sessions`, never passes the gate alone, never relaxes a hard constraint, and never becomes a reward. | M4, M5, M6 |
| Jev scout (J4): coverage limit | Local-only sessions never reach Jev. They get no prior and no comparison. On a turn whose admitted pool has no frontier target, the learner serves `rules` and never explores. | M4 |

Three rulings from the same addendum are not learner work. Ruling 3 (book the estimate for a classifier call that fails after the send), ruling 4 (fallback order after a cost-guarded turn), and ruling 5 (C3 fail open under a 500 ms deadline) belong to the classification and cache follow-ups. The learner inherits ruling 4 through `route_pick`, because every strategy plan is built by the same stage routing code.

**Addendum, 2026-09-29: owner rulings on how ruling 13 is shown.** The M10 report found that a `shadow` report cannot show a cost saving honestly (M10 status, section 9). The owner ruled on 2026-09-29. These rulings are binding, and they amend the "Starting numbers (13)" row above and M11.

1. **Cost from corrected quotes.** A `shadow` project never explores. So on every eligible interval, the report prices the learned choice from its recorded plan quote, corrected by the M3 residuals for each target. The estimate takes the conservative bound of the correction, so it never reads cheaper than the evidence supports. A quote that cannot be corrected (no residual for the target, or no price) is `unpriced`, never $0. Every such number is labeled `corrected quote estimate`. The `rules` side of the cost test uses the same method on the same intervals.
2. **Promotion is staged.** The `shadow` report gates cost (corrected quotes) and latency. It gates quality only on the intervals where the learned choice agreed with `rules`, and it prints the share that differed. A test that lacks support reads `not evaluable`, never `pass`. Promotion from `shadow` to `live`, with the ruled 5% exploration, needs three things: the cost test passes, the latency test passes, and quality shows no loss on the agreeing intervals. The M11 rerun after the first 20 live sessions is the binding quality test, with full support. Any failure there means a revert to `shadow`.

How M10 applies them (the M10 review fixes, section 9):

- The corrected cost of a plan is `max(adjusted_usd, quoted_usd)` of its recorded `CostEvidence`, the M3 result. M3 only raises a quote, so this is already the conservative bound, and no second correction is written. `Applied` and `NoPredictedReuse` are corrected: the second had its samples, and M3 ruled that no correction applies. `TooFewSamples` and `NotFrontier` (a local target) are unpriced. If any eligible interval is unpriced on either side, the cost test is `not evaluable`, and the report prints how many intervals. Dropping those intervals from both sides would be selection again. (Amended 2026-09-30: under `NoPredictedReuse`, M3 now prices the quote with no cached tokens, so the max rule reads that bound. See "M10 round-2 fixes" in section 9.)
- **Implementer decision that needs the owner's confirmation: latency also uses the corrected quote.** The gate reads each turn's modeled first output from the learned plan's `TtftEvidence`, and only when both the residual and the overhead terms were applied. A measured p50 needs the learned target to have served, which `shadow` never does where the learner differs, so a measured gate could never pass on a report with any diverging interval. The measured p50 stays in the report as a display line, with "N of M turns sampled". This departs from section 4, which says that the p50 test is measured.
- "No loss on the agreeing intervals" is ruling 13's quality comparison, restricted to those intervals: the learned bootstrap lower bound against the `rules` estimate on the same intervals, less 0.02. `quality.min_sessions` is met on the sessions that hold an agreeing interval, because that is the set the quality test reads. (Superseded 2026-09-30: test 1 is now paired. See the rulings on the M10 review-fix questions at the end of this plan.) (Amended 2026-09-30, round 2: `quality.min_sessions` counts the sessions that hold an agreeing interval where the learned candidate has weight above zero. In `shadow` this is the same count. In `live` an explored interval has no weight, and its session no longer counts.)
- The report also prints quality over every eligible interval. It is `not evaluable` unless both sides have logging probability above zero on every interval.
- **Open, a design question for the owner: how "no loss" is measured.** On the agreeing intervals both sides served the same route, so in `shadow` the `rules` estimate equals the learned one exactly. The ruled form then asks whether the bootstrap lower bound is within 0.02 of its own point estimate, which is a sample-size test, not a loss test. With 20 agreeing `shadow` sessions, 17 positive, the report reads a lower bound of 0.70 against 0.85, so test 1 reads `fail` for a loss that cannot exist. It fails closed. A paired statistic (the lower bound of learned minus `rules` over the same resampled clusters, which is exactly 0 in `shadow`) would measure loss. The old summary had the same property wherever the learned choice equaled `rules`. Until the owner rules, a `staged: no` caused by test 1 alone is not evidence of a quality problem. (Ruled 2026-09-30: the paired statistic. See the rulings on the M10 review-fix questions at the end of this plan.)
- **Open for M11:** in a `live` report the learned side has full support, but the `rules` side does not. `live` never serves `rules` where the learned choice differs, unless it serves `rules` because no strategy passed. So the full comparison is still `not evaluable` on a `live` report whose learner diverged. The owner's "a live report stays evaluable" holds for the learned side only. M11 needs a ruling on the `rules` baseline for the binding rerun before the first promotion. The calibrator does not invent a baseline from another report. (Ruled 2026-09-30: `rules` joins the live exploration set, so the `rules` side has support wherever its plan meets every hard constraint. See "`rules` in the live exploration set" in section 9.)

## 3. Where the draft and the rulings disagree, and how this plan resolves it

1. **`on_infeasible` had no default.** Draft section 7.5 made it a required field with no default. Ruling 7 sets `serve_rules` as the default. The loader now defaults it and still accepts `refuse`. The draft test `a_live_learner_without_on_infeasible_is_refused` is replaced by `a_live_project_without_on_infeasible_serves_rules`.
2. **Uniform exploration was refused as a default.** Draft section 7.5 proposed no member distribution and the configuration refused any `exploration` block. Ruling 8 authorizes exploration and uniform choice. The `exploration` block is accepted on `live` projects and refused on `shadow` and `off` projects, so a block never sits inert. With uniform choice, the logged propensity of section 14.2 is computable: the exploit target has probability `1 - rate` plus `rate` times its share of the set, and each other target has `rate` times its share. The draft's refusals of explored turns in the calibrator are replaced by weights from the recorded propensity.
3. **Latency basis.** Draft section 8.3 measured the residual from the served `Routed` and argued against a turn-start basis, because the pre-dispatch overhead does not depend on the target. Ruling 10 keeps that residual and adds the overhead as one project-level term. Both samples come from the same turns: completed turns with a first output. The modeled sum equals the ruled quantity, first output from turn start.
4. **Local cost.** Draft section 9 left local plans at $0. Ruling 6 puts the price in the catalog, not in the learner. The learner does not change. The precondition moves to M11.
5. **Estimand.** Draft section 14.3 published no policy value. Ruling 12 selects the conditional interval value. This plan specifies its eligible-outcome rule in section 4 so that M10 does not have to guess.
6. **Jev as a prior.** The draft and the 2026-09-21 ruling made classifications features only. The Jev addendum amends that: Jev's tier answer is also a cold-start prior, and still never a reward. Draft section 6 already had a prior term from the calibration artifact. This plan adds a second prior source and rules how the two combine (section 4).
7. **The `typesafe_shadow` question rule.** The doc comment on `TypeSafeShadow::questions` says that no option is "a model id, a tier or a price". The Jev addendum adds a tier question. The 2026-09-17 ruling, section 2, already allowed exactly this question when its criteria describe the work and not a model. M1 changes the comment to say so. The rubric of the new options names no model and no price, and a test checks that.
8. **Thresholds.** The draft authorized no number. Ruling 13 supplies the starting values. The loader still requires every numeric field in the `learner` block except the two that have ruled defaults, `on_infeasible` and `exploration.rate`. A deployment writes the starting values into its configuration. Nothing in code silently supplies them.

## 4. Planner decisions the rulings leave open

The rulings settle policy. An implementer still needs these mechanism choices. This plan makes them so that no milestone re-litigates them. The owner can override any of them. Each one is a constant or a configuration value, and a change to one of the revision-bearing constants starts a new epoch.

**Gate and evidence**

- `CREDIT_SCALE = 1000`. One interval adds at most 1000 units to a strategy at each level.
- `quality.z = 1.96`. The Wilson bounds use a 95% interval.
- `quality.min_evidence = 5000` units, that is 5 intervals of live evidence at a level, before the gate reads that level.
- `latency_min_samples = 20` and `cache_min_samples = 20`. Below these counts the quote is used unadjusted, and the record says so.
- The overhead term uses the same minimum as the residual, `latency_min_samples`, over the project's completed turns.

**Jev prior**

- `JEV_PRIOR_PSEUDO_INTERVALS = 3`. The prior on a key is 3000 units of `n` for each strategy.
- `JEV_PRIOR_MIN_ANSWERS = 3`. A key with fewer Jev tier answers has no prior.
- The prior's positive units for strategy `s` on key `k` are `3000 * agree(k, tier(s)) / answers(k)`, in integer arithmetic. `agree` counts Jev answers on `k` equal to the tier that `s` picks on `k`. Every strategy has one tier on one key, because `rules_pick` is part of every key.
- The Jev prior applies only where the artifact prior for that key and strategy is zero. An artifact prior is review evidence carried across epochs, and it wins.
- The prior enters the Wilson bounds only when the level has `live_n >= min_evidence`. It never counts toward `min_evidence` or `min_sessions`. So a prior alone gives `Unproven`, never `Pass` and never `BelowFloor`.
- Jev tier answers reach the learner store as integer counts, `jev_capable` and `jev_efficient`, on each of the three keys of the source turn. They arrive through the same entry chain as credit, so the same once-only rule covers them.

**Exploration**

- The engine computes one SHA-256 digest over `"{LEARNER_DRAW_VERSION}\nrouting-explore\nsalt={arm_salt}\nsession={session_id}\nresponse={response_id}\n"`. The first 8 bytes give the rate draw as a fraction in `[0, 1)`. The next 8 bytes pick the member: the value modulo the set size, over the set in configured strategy order. The `routing-explore` domain separates this stream from `Arm::for_session`.
- The reference target for "cheaper" is the exploit target, or the `rules` target when no strategy passes.
- `exploration.rate` is a fraction in `(0, 1]`. The ruled default is `0.05`.

**Latency and cost**

- The residual sample is `first OutputTextDelta.at_ms - served Routed.at_ms - quoted_ttft_ms`, rounded to whole milliseconds. The overhead sample is `served Routed.at_ms - TurnStarted.at_ms`. Both come from the same completed turns. A turn without first output supplies neither.
- The online latency constraint compares the modeled point estimate with `latency_limit_ms`. ~~The p50 test of ruling 13 is measured in the M10 report from the log, turn start to first output, and is a promotion gate.~~ The two use different statistics on purpose: the online check needs a per-turn number, and the report needs a distribution. (Superseded on 2026-09-29, pending the owner's confirmation: the promotion gate is the p50 of the modeled first output of the learned plan, a corrected quote estimate, and the measured p50 is a display line. See the section 2 addendum.)
- Follow-up, 2026-09-28: the catalog can refuse a rate card whose cached read rate is more than its effective write rate. M3 clamps the re-priced cost term at zero, so the owner's cost rule does not depend on this refusal.
- `serve_rules` turns still produce credit. The served trajectory is real and reviewed. The record's `ConstraintUnmet` says that the learner did not validate the route. It does not say that the review is void.

**Store and delivery**

- `LEARNING_PAGE = 64`, `read_timeout_ms = 25`, `apply_timeout_ms = 250` as the starting configuration values.
- Recovery block starting values: `sweep_interval_ms = 30000`, `idle_after_ms = 60000`, `max_sessions_per_sweep = 64`, `pages_per_session_per_sweep = 4`, `audit_sessions_per_sweep = 32`.
- The learner store holds the overhead sums under the `ops` hash as `turn:pre_sum` and `turn:pre_n`, beside the per-target fields.

**Estimand mechanics for M10**

- The evaluation unit is an accepted interval with a `Positive` or `Negative` label, no failover, and learned evidence of one epoch on every covered decision.
- The estimate is the self-normalized importance-weighted positive rate over those intervals, with the trajectory weight of draft section 14.3 and the logged propensity of each turn.
- The report shows beside it: the support census, the effective sample size, the session-clustered bootstrap interval with its seed, and the count of intervals excluded for each cause (`Unknown`, failover, mixed epoch, no learned row).
- The report labels every such number `conditional interval value`. It never labels one `session value` or `deployment value`.
- ~~The promotion comparison uses the candidate's bootstrap lower bound against the `rules` factual positive rate on the same session set.~~ Superseded on 2026-09-29 by the owner rulings in the section 2 addendum: every ruled test compares both sides on one interval set, cost and latency come from corrected quotes, and quality is gated on the intervals where the learned choice agreed with `rules`.

## 5. Seam map at `520eda5` (verified again for M1 at `a266eb7`)

The draft's section 2 seam map was read at `1658633`. This table records what the current tree has. Line numbers are omitted on purpose, because another PR is changing `crates/` in this worktree.

| Seam | File | State at `520eda5` |
|---|---|---|
| `pick_tier`, `tier_pool`, `StagePolicy::choose`, `DecisionSource` with `CostGuard`, `is_signal_driven` | `crates/roundhouse-core/src/routing/stage.rs` | Present. No `route_pick`. No `Strategy` source. |
| `RoutingContext`, `admissible`, `LocalQuoteSkip` | `crates/roundhouse-core/src/routing/mod.rs` | Present. No `learning` field, and M8 adds none: the learner's inputs travel as a `LearningTurn` beside the context. |
| `SelectionSnapshot`, `SelectorBranch` (`Affinity`, `EscalationAudit`, `Stage`), `STAGE_SELECTOR_REVISION` | `crates/roundhouse-core/src/routing/selection.rs` | Present. No `Learned` branch. |
| `ProviderPricing::price_tokens`, `effective_write_per_mtok_usd`, `CacheLedger::model_for`, `BlockMarker`, `TargetState::last_block_marker` | `crates/roundhouse-core/src/routing/ledger.rs` | Present. The marker fact landed on this branch on 2026-09-28. |
| `ClassificationAxis`, `TurnIntent`, `TurnComplexity`, `ContextDependence`, `Graded`, `TurnClassification`, `ClassificationRecord`, `ClassificationWindow`, `TAXONOMY_VERSION = 1` | `crates/roundhouse-core/src/classify/mod.rs` | Present. No tier axis. |
| `SessionState::classifications`, `classifications_through`, `Session::commit` | `crates/roundhouse-core/src/session.rs` | Present. At `520eda5` `commit` passed `None` as the learning mark; M5 passes `learning_mark(&self.state, &kinds)`. |
| `ReviewTracker`, `ReviewOutcome`, `TrackedDecision`, `MAX_REVIEW_TURNS = 64`, `MAX_REVIEW_DECISIONS = 256` | `crates/roundhouse-core/src/session/review.rs` | Present. No learning row. |
| `IntervalLabel` (`Positive`, `Negative`, `Unknown`) | `crates/roundhouse-core/src/validate/interval.rs` | Present. |
| `Arm::consults_judge` | `crates/roundhouse-core/src/validate/arm.rs` | Present. |
| `SessionEventKind`: `TurnStarted`, `Routed`, `OutputTextDelta`, `ResponseCompleted`, `ResponseIncomplete`, `ValidationDecided`, `ClassificationRequested`, `ClassificationRecorded`, `ClassificationSettlementRepaired` | `crates/roundhouse-core/src/event.rs` | Present. No `LearningApplied` at `520eda5`; M5 adds it. |
| `TurnBudget::admits`, `BudgetState::ExhaustedOverflow` | `crates/roundhouse-core/src/control/budget.rs` | Present. |
| `SettlementKey::SessionWatermark`, `OncePerCall` | `crates/roundhouse-core/src/control/spend.rs` | Present. |
| `SessionStore::append_events(lease, kinds, mark)`, `clear_learning_mark`, `requeue_learning`, `pending_learning`, `learning_sessions`, `LearningMark`, `MarkedSession`, `LearningCursor`, `ClearOutcome` | `crates/roundhouse-core/src/store.rs`, `store/learning.rs`, `store/contract/learning.rs` | Present. L3b is built and mutation-checked. |
| `KeyFamily`, `build_key`, the key-convention test | `crates/roundhouse-store-redis/src/keys.rs` | Present. No `Learn` family at `520eda5`; M7 adds it. |
| `Engine::plan`, `Engine::run_turn`, `opened_a_tier_escalation` | `crates/roundhouse-server/src/engine.rs` | Present. Selection capture moved to `engine/selection.rs` (`SelectionInputs`, `selection_inputs`, `local_quote_skip`). Classifier lifecycle is in `engine/classification.rs`. M8 added `engine/learning.rs`: the learned choice and the tail's delivery. |
| `ProjectEntry` (`policy`, `budget`, `fair_use`, `validate`, `credentials`, `tiers`) | `crates/roundhouse-server/src/control_config/config.rs` | Present. M8 added the boxed `learner` block (`control_config/learner.rs`). |
| `ValidateConfig` | `crates/roundhouse-server/src/control_config/validate.rs` | Present. |
| `TypeSafeShadow::questions`, `prepare`, `NotRun::NoAdmittedFrontier` | `crates/roundhouse-server/src/typesafe_shadow.rs` | Present. Three questions: intent, complexity, context dependence. |
| `ChoiceQuestion`, `SystemOneRequest`, `SystemOneClient` | `crates/roundhouse-fleet/src/typesafe.rs` | Present. A map of choice questions is supported. |
| `MetricsFold::apply`, `MetricsSnapshot`, the evaluation section | `crates/roundhouse-core/src/metrics/fold.rs`, `snapshot.rs`, `evaluation.rs` | Present. M1 added the agreement section and M8 the `learning` section (`metrics/learning.rs`). |
| `/v1/metrics`, dashboard `renderEvaluation` | `crates/roundhouse-server/src/metrics_api.rs`, `dashboard.html` | Present. |
| `shared_backend::open`, `serve` | `crates/roundhouse-server/src/shared_backend.rs`, `main.rs` | Present. M9 added `Backends::open_learner_store`, `routing_composition` (the policy and learner choice), and the recovery task in `learner_recovery.rs`. |
| Local quotes at `expected_cost_usd: 0.0` | `crates/roundhouse-fleet/src/local.rs` | Present. The catalog price is a separate follow-up. |
| Existing binaries | `crates/roundhouse-server/src/bin/import-benchmarks` | The calibrator follows this layout. |

Files this plan creates, now built: `crates/roundhouse-core/src/routing/learn/` (M2 to M4, and `learn/artifact.rs` in M8), `crates/roundhouse-core/src/learn_store.rs` with `learn_store/` (M6), `crates/roundhouse-store-redis/src/learn.rs` with `learn/scripts.rs` (M7), and `crates/roundhouse-server/src/engine/learning.rs`, `crates/roundhouse-server/src/control_config/learner.rs` and `crates/roundhouse-core/src/metrics/learning.rs` (M8).

M9 built `crates/roundhouse-server/src/learner_recovery.rs`, with `routing_composition.rs`, `engine/learning/delivery.rs` and `control_config/learner_recovery.rs` beside it.

M10 built `crates/roundhouse-core/src/routing/learn/offline.rs` with `learn/offline/`, `crates/roundhouse-server/src/learner_calibrate.rs`, and `crates/roundhouse-server/src/bin/learner-calibrate/`.

## 6. Milestones

Each milestone is one PR, cut from `main` after PR #18 merges, in the order below. Each one is small enough for one Opus implementation stage and follows the ultracode cadence in `CLAUDE.md`: Opus core, Opus wiring, Sonnet churn, Sonnet refute by mutation, Sonnet fix. Tests come first. Every cargo command runs under `timeout`. A milestone does not start a live provider call. The `done means` list is the merge gate, together with `wills-mega-review`.

Dependency order: M1 is independent and ships first. M2 to M4 build the pure policy in core. M5 builds the fold. M6 and M7 build the store. M8 and M9 wire the engine and the process. M10 builds the calibrator. M11 is a procedure, not code.

### M1 — Jev tier question and agreement report

**Goal.** Ask Jev which tier fits the turn, in the same call, and report how often the served tier agrees. This is the data that tells the owner whether Jev is worth its cost.

**Settled points.**

- One request. The tier question joins the three taxonomy questions in `TypeSafeShadow::questions`. No new call, no new deadline, no new budget line.
- The question is about the work. Options are `capable` and `efficient`. The rubric describes the kind of work, for example careful multi-step reasoning against a routine well-specified change. No option names a model, a target, or a price. The doc comment on `questions` changes to state this rule.
- `TAXONOMY_VERSION` becomes 2. `TurnClassification` gains `tier: Option<Graded<TierChoice>>` with `#[serde(default)]`. A taxonomy-1 record decodes with `None`. Under taxonomy 2 the answer set is unusable without the tier answer, which is the existing complete-set rule.
- The served tier is the recipe tier of the served target, read from the stage evidence on the `Routed` of the source turn. On a cost-guarded turn the served tier is `Capable` even though the pick was `Efficient`.
- The agreement block lives in the metrics snapshot's evaluation section, for each existing scope. It reports `answered`, `agree`, `disagree`, disagreements by direction, and for disagreements the covering interval label: `positive`, `negative`, `unknown`, or `unlabeled` when no review has covered the turn yet.
- The metrics fold keeps, for each session, a bounded list of turns that have a tier answer and no covering label. The bound is `MAX_REVIEW_DECISIONS`. An eviction increments a counter. A `ValidationDecided` whose interval covers a retained turn resolves it.
- Local-only sessions produce no call and therefore no row.
- Never a reward. Nothing in M1 reads or writes learned state.

**Files.** `crates/roundhouse-core/src/classify/mod.rs`, `crates/roundhouse-server/src/typesafe_shadow.rs` and its tests, `crates/roundhouse-core/src/metrics/fold.rs`, `crates/roundhouse-core/src/metrics/evaluation.rs`, `crates/roundhouse-core/src/metrics/snapshot.rs`, `crates/roundhouse-server/src/metrics_api.rs`, `crates/roundhouse-server/src/dashboard.html`, `crates/roundhouse-core/tests/evaluation_metrics_snapshot.rs`, `crates/roundhouse-server/tests/evaluation_metrics.rs`.

**Tests first.**

- `the_tier_question_rides_the_same_request_as_the_taxonomy`
- `a_tier_option_rubric_names_no_model_and_no_price`
- `a_reply_without_the_tier_answer_is_unusable_under_taxonomy_2`
- `a_taxonomy_1_record_decodes_with_no_tier`
- `agreement_counts_the_served_tier_against_the_jev_tier_for_each_project`
- `a_cost_guarded_turn_compares_the_served_tier_not_the_pick`
- `a_disagreement_takes_the_label_of_the_covering_interval`
- `a_disagreement_without_a_covering_review_stays_unlabeled`
- `a_local_only_session_reports_no_agreement_row`
- `replay_rebuilds_the_same_agreement_counts`
- `retained_disagreements_are_bounded_and_an_eviction_is_counted`
- `the_dashboard_reads_the_fields_the_document_publishes` in `crates/roundhouse-server/src/metrics_api.rs` reads the body of `renderAgreement` with `function_body` and checks that the tile prints every agreement field the document publishes.

**Done means.** The loopback classifier receives one request with four questions. Old classification records replay. The agreement block appears in `/v1/metrics` for each scope and on the dashboard. Independent mutations of the question map, the served-tier source, the label join, and the bound fail their tests. No live TypeSafe call.

### M2 — Strategies, learned input, keys, and record types (draft L1, part 1)

**Goal.** Extract stage routing into `route_pick`, define the three strategies, the learned input and its keys, and the durable record types. Everything is pure data and pure functions. No gate and no policy yet.

**Settled points.**

- `StagePolicy::route_pick(recipe, pick, admitted) -> Result<Decision, RoutingError>` is a behavior-preserving extraction. (M4 changed the return type to `RoutedPick`; see the M4 review fixes note in section 9.) `rules` through `route_pick` equals `StagePolicy::choose`. The dominance cost guard, degrade-to-local, and the fallback order of ruling 4 stay inside it.
- `DecisionSource::Strategy` is a new variant. `is_signal_driven` returns `false` for it. `rules` keeps the source that `pick_tier` returned.
- Strategies are `rules`, `efficient`, `capable`, as draft section 4.1. No calibrated `rules` strategy in this revision.
- `LEARNING_INPUT_REVISION = 1`. The input is `rules_pick`, `newest`, `prior`, `tool_turn`, with the bands and the three key levels of draft section 5. `K = 3`.
- `LearnedEvidence` and `SelectorBranch::Learned(Box<LearnedEvidence>)` as draft section 7.7, with two additions: the exploration draw and member index, and the logged propensity of the served first target. `SessionEventKind` keeps its current size.
- `ReadView`, `PriorUnits`, and `LearnerTerms` are defined here as data. The Jev counts and the overhead sums are fields of `ReadView`.
- The rationale names the strategy, the tier, the epoch prefix, and the key. It never names a price.

**Files.** `crates/roundhouse-core/src/routing/stage.rs`, new `crates/roundhouse-core/src/routing/learn/mod.rs`, `learn/input.rs`, `learn/evidence.rs`, `crates/roundhouse-core/src/routing/selection.rs`, `crates/roundhouse-core/src/event.rs`.

**Tests first.**

- `route_pick_with_the_rules_pick_equals_stage_policy_choose` (a control that passes before and after the extraction)
- `a_forced_capable_pick_does_not_open_a_handoff_note`
- `every_strategy_keeps_the_dominance_cost_guard_and_degrade_to_local`
- `the_sequence_orders_by_source_turn_not_arrival`
- `identical_newest_labels_with_different_prior_sequences_give_different_l2_keys`
- `no_available_classification_is_band_none_not_low`
- `a_classification_after_the_cutoff_is_not_an_input`
- `rules_pick_is_part_of_every_key_level`
- `learned_evidence_round_trips_and_session_event_size_is_unchanged`
- `a_record_without_learned_evidence_still_decodes`
- `the_rationale_carries_no_price`

**Done means.** `StagePolicy` behavior is unchanged under the existing suites, including `handoff_escalation`. The new types serialize and replay. Mutations of the key derivation and the extraction fail their tests.

### M3 — Cost, latency, and cache corrections (draft L6, moved forward)

**Goal.** The pure corrections that the policy needs: the cache reuse correction on cost, the latency model of ruling 10, and the grant re-check on the adjusted cost.

**Settled points.**

- The cache correction is draft section 9, through `ProviderPricing::price_tokens`, so the effective write premium is kept. Zero predicted reuse applies no correction and the record says `NoPredictedReuse`. (Superseded 2026-09-30: under `NoPredictedReuse` the quote is priced with no cached tokens, through the same `price_tokens` and the same clamp at zero. See "M10 round-2 fixes" in section 9.) No separate cache-miss penalty. No record names eviction. No return-trip pricing.
- The latency model is `quoted_ttft + residual_mean(target) + overhead_mean(project)`. Each mean applies only at or above its minimum sample count, and the record says which terms applied. The overhead term is added once per turn, not once per target.
- The adjusted cost must pass `TurnBudget::admits` on a copy of the candidate. An `ExhaustedOverflow` admission keeps its status.

**Files.** New `crates/roundhouse-core/src/routing/learn/corrections.rs`. Reads `crates/roundhouse-core/src/routing/ledger.rs` and `crates/roundhouse-core/src/control/budget.rs`.

**Tests first.**

- `a_reuse_shortfall_keeps_the_effective_write_premium` (100 becomes 200)
- `zero_predicted_reuse_applies_no_correction`
- `the_adjusted_cached_count_stays_within_the_prefix_and_input`
- `a_cache_miss_adds_no_separate_penalty`
- `the_cache_correction_does_not_change_ttft`
- `a_latency_residual_applies_only_after_its_minimum_samples`
- `the_overhead_term_is_added_once_per_turn_not_per_target`
- `latency_without_overhead_samples_uses_the_quote_plus_residual_and_is_recorded_unadjusted`
- `an_adjusted_cost_above_the_grant_fails_the_grant_constraint`
- `an_overflow_admitted_candidate_keeps_its_status`
- `no_record_names_eviction`

**Done means.** Every function is total over finite inputs. Mutations of the write premium, the once-per-turn overhead, and the grant re-check fail their tests.

### M4 — Gate, constraints, exploration, and `LearnedPolicy::choose` (draft L1, part 2)

**Goal.** The pure policy. Given a `RoutingContext` with a learning input, it returns a decision and its evidence.

**Settled points.**

- The gate is `wilson-v1` of draft section 7.4, with the prior term equal to the artifact prior plus the Jev prior under the rules of section 4 of this plan. The gate reads the most specific level with `live_n >= min_evidence`. `Pass` needs `sessions >= min_sessions` and `L >= floor`.
- The hard constraints are draft section 7.2: admission, grant on the adjusted cost, latency on the modeled first output, and the gate.
- The exploit strategy is the passing strategy with the lowest adjusted cost. Ties go to the lower modeled latency, then to configured order.
- Exploration as ruled and as section 4 specifies: `live` only, reviewed session only, a frontier target in the admitted pool, `Unproven` members strictly cheaper than the reference, every hard constraint, draw below the rate, uniform member. (Amended 2026-09-30: `rules` also joins the set when it meets every hard constraint. See "`rules` in the live exploration set" in section 9.)
- Infeasible: `serve_rules` serves the `rules` decision and records the unmet constraints. `refuse` fails the turn with a typed error. Store failures take the same path with `StoreUnavailable` or `ReadTimedOut`.
- Fallbacks are the first targets of the other passing strategies, in exploit order, without duplicates. No unchecked target enters the plan.
- `shadow` returns the `rules` decision and records the learned result as not applied.
- The policy is pure. It never reads the store and never draws. The engine supplies the view and the draw.

**Files.** New `crates/roundhouse-core/src/routing/learn/gate.rs`, `learn/policy.rs`, `learn/explore.rs`.

**Tests first.**

- `wilson_v1_bounds_match_fixed_vectors`
- `the_gate_uses_the_most_specific_level_with_evidence`
- `an_artifact_prior_alone_cannot_pass_the_gate`
- `a_jev_prior_alone_cannot_pass_the_gate`
- `a_jev_prior_applies_only_where_the_artifact_prior_is_zero`
- `a_jev_prior_needs_the_minimum_answer_count`
- `a_jev_prior_moves_the_bounds_only_after_live_evidence_meets_the_minimum`
- `the_exploit_strategy_is_the_cheapest_passing_plan`
- `a_plan_over_the_latency_limit_fails_the_latency_constraint`
- `no_passing_strategy_is_infeasible_and_never_serves_an_unchecked_route`
- `serve_rules_serves_the_rules_decision_and_records_the_unmet_constraints`
- `refuse_fails_the_turn_with_the_unmet_constraints`
- `without_exploration_the_learned_route_equals_rules_or_is_infeasible` (a property test over every store state that consistent credit can produce)
- `exploration_is_uniform_over_the_eligible_set` (fixed draw vectors)
- `an_unreviewed_session_never_explores`
- `a_shadow_project_never_explores`
- `a_local_only_pool_serves_rules_and_never_explores`
- `an_exploring_turn_serves_only_a_cheaper_unproven_member_that_meets_every_hard_constraint`
- `a_below_floor_strategy_is_never_explored`
- `the_recorded_propensity_sums_selection_probabilities_by_first_target`
- `the_draw_uses_the_routing_explore_domain_and_the_arm_salt`
- `fallbacks_hold_only_targets_that_satisfy_every_constraint`
- `a_different_prior_sequence_changes_the_route_when_l2_evidence_differs`

**Done means.** Every row of draft section 7.8 passes as a pure-policy test, with the cold-start row now reading `serve_rules` by default and an exploration row added. Mutations of the gate level choice, the prior minimums, the exploration guards, and the propensity fail their tests.

### M5 — Entries, credit, operational rows, Jev counts, cursor, and automatic marks (draft L2)

**Goal.** The session fold turns the log into an ordered chain of learning entries, and `Session::commit` marks the source store on every entry-producing append.

**Settled points.**

- Entry-producing events, after the first `Routed` with learned evidence in the session: `ValidationDecided`, `ResponseCompleted`, `ResponseIncomplete`, and `ClassificationRecorded`. The event kind alone decides existence. Deltas can be empty. This list is a one-way door and does not change without a new `REVIEW_RULE_REVISION`.
- Credit is draft section 8.2, unchanged.
- Operational rows are draft section 8.3 plus the overhead sample of section 4 of this plan. Residual and overhead samples come from the same completed turns.
- Jev rows: a `ClassificationRecorded` whose classification has a tier answer, and whose source turn has a learned row, adds one `jev_<tier>` count to each of the three keys of that turn. Without a tier answer, or without a learned row, the entry has no deltas.
- The fold keeps the learning row of a turn from its `ClassificationRequested` until the matching `ClassificationRecorded` or the intent's expiry. The classification runtime already bounds outstanding intents, so this retention is bounded by the same number.
- `TrackedDecision` gains the fixed-size `LearningRow` of draft section 8.1.
- `LearningApplied { through_seq }` is a new event. It has no response id and is not terminal. The cursor is draft section 11.5 with `LEARNING_PAGE = 64`.
- `Session::commit` computes `learning_mark(&self.state, &kinds)` and passes it to `append_events`. Every public write that can carry an entry-producing event marks through this one path.

**Files.** `crates/roundhouse-core/src/session/review.rs`, `crates/roundhouse-core/src/session.rs`, `crates/roundhouse-core/src/session/classification.rs`, `crates/roundhouse-core/src/event.rs`, new `crates/roundhouse-core/tests/learning_entries.rs`.

**Tests first.**

- `every_entry_producing_event_yields_one_entry_even_with_empty_deltas`
- `entry_existence_does_not_depend_on_credit_revision`
- `prev_seq_links_each_entry_to_the_previous_one`
- `a_consistent_strategy_receives_one_interval_unit_split_over_keys`
- `an_inconsistent_strategy_receives_nothing`
- `a_failover_in_the_interval_credits_nothing`
- `another_epoch_or_credit_revision_credits_nothing`
- `an_unknown_label_yields_an_entry_without_quality_deltas`
- `a_serve_rules_turn_still_produces_credit_for_consistent_strategies`
- `a_late_review_does_not_change_recorded_inputs`
- `the_latency_interval_starts_at_the_served_routed_event`
- `the_overhead_sample_spans_turn_start_to_the_served_routed`
- `overhead_and_residual_samples_come_from_the_same_turns`
- `a_review_turn_judge_call_enters_the_overhead_and_not_the_residual`
- `a_turn_without_first_output_supplies_no_latency_sample`
- `cache_rows_require_provider_measurement`
- `a_classification_with_a_tier_answer_adds_one_jev_count_on_each_key_of_its_source_turn`
- `a_classification_without_a_tier_answer_yields_an_entry_with_no_deltas`
- `a_classification_for_a_turn_without_a_learned_row_yields_an_entry_with_no_deltas`
- `the_learning_row_is_kept_from_the_intent_until_the_result_or_expiry`
- `a_full_page_counts_beyond_and_holds_no_later_entry`
- `backfill_from_a_seq_refills_the_page_in_order`
- `learning_applied_moves_the_hint`
- `learning_applied_has_no_response_id_and_is_not_terminal`
- `replay_rebuilds_the_same_entries_and_cursor`
- `learning_mark_marks_every_entry_producing_append_after_the_first_learned_routed`
- `learning_mark_marks_nothing_before_learned_evidence`
- `commit_marks_through_every_public_write_that_can_emit_an_entry_producing_event`

**Done means.** A serialized log replays to the same entry chain on a fresh fold. Marked appends reach the existing source index through `commit`. Mutations of the entry list, the credit rule, the Jev key fan-out, and the mark computation fail their tests.

### M6 — Learner store contract and memory store (draft L3)

**Goal.** The `LearnerStore` trait, its shared contract suite, and the memory backend.

**Settled points.**

- The trait and the two-phase `apply` are draft section 11.2. The check phase writes nothing. `ChainGap` returns the store watermark.
- Counters and ranges are draft section 11.3, plus `jev_capable`, `jev_efficient` on each quality key and `pre_sum`, `pre_n` on the operations key. All are integers within `0..=2^53 - 1`, except the signed sums.
- `read` visits only the three quality keys of the turn and the operations key. It makes no write.
- The contract suite is a macro, in the style of `store_contract_suite!`, so M7 runs the same list against Redis.

**Files.** New `crates/roundhouse-core/src/learn_store.rs`, `learn_store/contract.rs`, `learn_store/memory.rs`. Uses `crates/roundhouse-core/src/contract_macro.rs`.

**Tests first.** The draft L3 list, unchanged, plus:

- `jev_counts_and_overhead_sums_apply_under_the_same_identity_rule`
- `read_returns_the_jev_counts_and_overhead_sums_of_the_turn`

**Done means.** The memory backend passes the whole suite. The fault hook between the phases proves no partial write. Mutations of the `prev_seq` check, the batch-level watermark, and the staging fail their tests.

### M7 — Redis learner store (draft L4)

**Goal.** The Redis backend behind the same contract, with one Lua script for `read` and one for `apply`.

**Settled points.**

- `KeyFamily::Learn`, name `learn`, version `v1`, through `build_key`. The key layout is draft section 11.4, with the two new field groups.
- The script writes only after every check passes, and only with `HSET` and `SADD` on keys whose types it checked. Numbers pass as RESP integers, never through `tostring`.
- Single instance. No Redis Cluster.

**Files.** New `crates/roundhouse-store-redis/src/learn.rs`, `learn/scripts.rs`. `crates/roundhouse-store-redis/src/keys.rs`.

**Tests first.** The M6 suite gated on `ROUNDHOUSE_TEST_REDIS_URL`, plus `a_wrong_type_key_is_refused_before_any_write`, `the_scripts_return_only_integers`, and the extended `every_learn_key_is_built_by_the_shared_builder`.

**Done means.** The gated suite passes against a task-owned Redis, with no skips. The key-convention test covers every `learn` key.

### M8 — Configuration, engine selection path, delivery, and metrics (draft L5, part 1)

**Goal.** A project can run the learner in `shadow` or `live`, the engine reads the store before `choose` and applies entries after the terminal event, and `/v1/metrics` shows the learning section.

**Settled points.**

Section 9's M8 notes record where the stage brief overrode these points before the implementation landed — `Engine::apply_learning`, `RoutingContext.learning`, and `engine/selection.rs` all changed shape there.

- `ProjectEntry.learner: Option<LearnerConfig>` with `deny_unknown_fields`. Fields are draft section 12.1 plus `exploration: Option<ExplorationConfig>` with `rate`. `mode` defaults to `off`. `on_infeasible` defaults to `serve_rules`. `exploration.rate` defaults to `0.05` when the block is present. Every other field is required in `shadow` and `live`.
- The loader refuses: a `learner` block without `tiers`, a floor outside `0.0..=1.0`, a non-positive `z`, a zero timeout, fewer than 2 strategies, a list without `rules`, a repeated or unknown strategy, an artifact whose strategy list differs, an `exploration` block on a project whose mode is not `live`, and a rate outside `(0, 1]`.
- `Admission.learner: Option<Arc<LearnerTerms>>`, resolved with `tiers`. Admin `ProjectRecord` carries the block.
- `Engine::plan` computes the learned input and keys after it captures features and classifications, reads the store under `read_timeout_ms`, computes the draw, and sets `reviewed` from the session arm. `RoutingContext.learning: Option<&LearningInput>`. Every other `RoutingContext` literal gets `None`.
- `Engine::apply_learning` runs in the `run_turn` tail after `settle` and the fair-use draw, for steered, failed, and dispatched turns. Steps are draft section 11.5. At most one backfill per tail. (Superseded 2026-09-29 by the M8 review fixes in section 9: at most one refill and one gap backfill per tail.)
- The metrics fold reads learned evidence on `Routed` and `LearningApplied`. The `learning` section is draft section 12.4. `explain_last_route` shows the learned rationale.

**Files.** `crates/roundhouse-server/src/control_config/config.rs`, new `control_config/learner.rs`, `crates/roundhouse-server/src/engine.rs`, new `engine/learning.rs`, `engine/selection.rs`, `crates/roundhouse-core/src/routing/mod.rs`, `crates/roundhouse-core/src/metrics/fold.rs`, `crates/roundhouse-server/src/metrics_api.rs`, new `crates/roundhouse-server/tests/learned_routing_engine.rs`.

**Tests first.**

- Each row of draft section 7.8 through `Engine::run_turn` with loopback providers, with the cold-start row reading `serve_rules` and one exploration row.
- `an_off_project_records_no_learned_evidence`
- `a_shadow_project_serves_rules_and_records_the_learned_choice`
- `a_positive_review_updates_the_project_once_across_reopen_and_replay`
- `a_refused_ack_then_a_new_entry_applies_only_the_new_entry`
- `an_apply_timeout_then_retry_applies_each_entry_once`
- `a_successor_after_apply_without_ack_applies_only_its_new_entries`
- `two_projects_learn_independently_through_the_engine`
- `a_steered_turn_still_applies_pending_entries`
- `a_learner_turn_makes_one_read_one_apply_one_clear_and_one_append`
- `a_refuse_project_fails_the_turn_on_a_store_outage_and_a_serve_rules_project_does_not`
- `a_jev_tier_answer_reaches_the_store_as_a_count_on_the_source_turn_keys`
- `an_explored_turn_records_its_draw_member_and_propensity`
- Configuration: `a_learner_without_tiers_is_refused`, `a_strategy_list_without_rules_is_refused`, `a_live_project_without_on_infeasible_serves_rules`, `an_exploration_block_on_a_shadow_project_is_refused`, `an_exploration_rate_defaults_to_five_percent`, `an_unknown_learner_field_is_refused`.
- API: `metrics_expose_the_learning_section_for_each_scope`.

**Done means.** A `shadow` project on the memory backend produces entries, applies them, and shows them in metrics, through the engine. A `live` project changes a route only from written evidence or an authorized exploration. Mutations of the read timeout path, the tail order, and the config refusals fail their tests.

### M9 — Startup composition, recovery task, and audit (draft L5, part 2)

**Goal.** The process composes the learner when any project enables it, and idle undelivered sessions are found and delivered without the engine.

**Settled points.**

- `shared_backend::open` adds the learner store: Redis when `ROUNDHOUSE_REDIS_URL` is set, else memory with a logged warning that state ends with the process.
- `serve` loads and validates each artifact. An invalid artifact stops the boot. When any admission has a mode other than `off`, `serve` wraps `StagePolicy` in `LearnedPolicy`, attaches the learner, and starts the recovery task. Otherwise the composition and the policy name do not change.
- The recovery task is draft section 11.7 with the M6 store and the existing source index methods `pending_learning`, `learning_sessions`, `clear_learning_mark`, `requeue_learning`. It never appends and never takes a lease. It does not read `is_leased`.
- The control-plane `learner_recovery` block is required when any project enables the learner.
- An admin-added learner block warns once until restart.

**Files.** `crates/roundhouse-server/src/shared_backend.rs`, `crates/roundhouse-server/src/main.rs`, new `crates/roundhouse-server/src/learner_recovery.rs`, `crates/roundhouse-server/src/control_config/`.

**Tests first.**

- `a_crash_before_the_first_apply_is_recovered_by_the_recovery_task`
- `a_session_whose_every_learner_store_call_failed_is_delivered_after_recovery`
- `a_delayed_engine_clear_after_lease_turnover_keeps_the_new_turn_mark`
- `a_lost_ack_leaves_the_mark_and_the_recovery_task_clears_it_once_delivered`
- `the_recovery_task_never_appends_and_is_safe_while_the_owner_runs`
- `the_recovery_task_delivers_on_a_store_that_inherits_the_is_leased_default`
- `two_recovery_tasks_on_one_session_apply_each_entry_once`
- `the_audit_requeues_a_session_whose_learner_watermark_fell_below_its_mark`
- Startup: `a_learner_project_composes_the_learned_policy_and_recovery_task_at_boot`, `no_learner_project_leaves_the_policy_name_unchanged`, `an_admin_added_learner_warns_until_restart`, `an_invalid_artifact_stops_the_boot`.
- Redis-gated: the reopen, successor, and recovery cases against `RedisSessionStore` and `RedisLearnerStore`.

**Done means.** The binary boots with a `shadow` project on both backends. A session delivered only by the recovery task reaches the same counters as one delivered by the engine. Mutations of the clear predicate use and the audit requeue fail their tests.

### M10 — Offline calibrator and promotion report (draft L7)

**Goal.** A binary that lists learner sessions from the source marks, replays their logs, writes a versioned artifact, and prints the report that ruling 13 needs for a promotion decision.

**Settled points.**

- Input, manifest, and cutoff are draft section 14.1. The drift check needs a point-in-time copy of the store, or the report says `drift check not run`.
- The estimand is the conditional interval value with the mechanics of section 4 of this plan. Weights use the recorded propensity of each turn. A trajectory with one mismatched turn has weight zero.
- The report has a promotion summary with the three ruled tests and their results: ~~the candidate's positive-rate lower bound against the `rules` rate with the 2-point allowance, the estimated cost against the `rules` cost with the 10% requirement, and the measured p50 of first output from turn start against the limit~~ (superseded on 2026-09-29 by the staged tests of the section 2 addendum). It states the session count against `min_sessions`. It also shows the support census, the effective sample size, the exclusion counts, the judge and classifier spend by strategy stratum, and the Jev agreement block of M1.
- The artifact is draft section 14.5. It contains no wall-clock time. The sidecar holds metadata. The artifact can also carry zero prior units for a new project.
- Rollback is a configuration change to the previous artifact or mode, as draft section 14.6.

**Files.** New `crates/roundhouse-core/src/routing/learn/offline.rs` and `crates/roundhouse-server/src/bin/learner-calibrate/`.

**Tests first.**

- `the_same_manifest_gives_byte_identical_artifacts`
- `the_sidecar_time_does_not_change_the_epoch_id`
- `replay_equivalence_fails_on_a_changed_draw`
- `one_mismatched_turn_zeroes_the_trajectory_weight` (IPS and SNIPS are 0, not 0.5 and 1/3)
- `an_explored_turn_uses_the_recorded_propensity`
- `trajectory_probability_multiplies_turn_probabilities`
- `the_conditional_interval_value_is_labeled_as_such_and_never_as_session_value` (the reviewer fixture: 0.75 is reported as the conditional value and 1.0 is not claimed)
- `the_support_census_counts_trajectories_with_zero_logging_probability`
- `intervals_are_excluded_by_cause_and_the_counts_are_reported`
- `learner_sessions_are_enumerated_from_the_source_marks`
- `the_bootstrap_resamples_sessions_with_the_recorded_seed`
- `measured_cost_uses_the_recorded_rate_card`
- `the_p50_first_output_is_measured_from_turn_start_in_the_log`
- `the_promotion_summary_states_each_of_the_three_ruled_tests_and_its_result`
- `the_drift_check_does_not_run_without_a_point_in_time_copy`
- `rolling_back_the_artifact_reads_the_previous_epoch_state`

**Done means.** The binary runs against a memory-store fixture log and against a task-owned Redis with the same result. The report is deterministic for one manifest. No number in it is labeled as session or deployment value.

### M11 — Live enablement for one project (draft L8)

This milestone changes configuration only. It has no code and no PR. It is an owner procedure.

Preconditions, all required (precondition 2 and step 3 are superseded by the addendum below):

1. Every node runs a build at or after M9. The new event variants are a one-way door.
2. The project ran in `shadow` long enough for an M10 report to satisfy ruling 13: at least 20 sessions, the positive-rate lower bound no more than 2 points below `rules`, the estimated cost at least 10% lower, and the p50 first output from turn start at or below 10 s.
3. If the recipe admits a local target, the catalog sets the local price from ruling 6. Without it a local plan quotes $0 and wins every cost comparison, and the report cannot show a cost signal.
4. The owner approves the promotion in writing, and the approval names the report's manifest digest.

Steps:

1. Set `mode` to `live` for the project, with `on_infeasible = serve_rules` and the `exploration` block at the ruled rate.
2. Watch the `learning` section of `/v1/metrics` for the first sessions: infeasible counts by cause, explored turns, and store read failures.
3. Run M10 again after the first 20 live sessions. If any of the three tests fails, set `mode` back to `shadow`.

Rollback is `mode = shadow` or `off`, or the previous artifact. The store keeps every epoch.

**Addendum, 2026-09-29: staged promotion (owner rulings in the section 2 addendum).** This replaces precondition 2 and step 3 above. The old text stays as the record.

- New precondition 2: the project ran in `shadow` long enough for an M10 report to show `promotion to live, staged: yes`. That means: the cost test passes on corrected quotes over every eligible interval, the latency test passes, and quality shows no loss on the intervals where the learned choice agreed with `rules`. The report also states the session count against `quality.min_sessions`, on the sessions with an agreeing interval, and the share of intervals that differed. The owner reads the share that differed: that share has no quality evidence until the live rerun.
- A recipe with local targets reports cost as `unpriced` in the corrected quote estimate, because M3 does not correct a local quote. Precondition 3 does not change this, and the cost test is `not evaluable` for such a recipe until the owner rules how a local quote is corrected.
- New step 3: run M10 again after the first 20 live sessions. That report is the binding quality test. If any of its tests fails, set `mode` back to `shadow`. Before the first promotion, the owner must also rule on the `rules` baseline for this rerun: in `live`, `rules` has no logging probability where the learned choice differs, so the report's full quality comparison reads `not evaluable` there (section 2 addendum, "Open for M11"). (Ruled 2026-09-30: `rules` joins the live exploration set. See section 9.)

Turning every project `off` after `shadow` composes no learner and no recovery task at the next boot, so marks left by the `shadow` sessions stay pending until a learner is enabled again (M9 review, 2026-09-29). A rollback to `off` that should still deliver those marks needs a design here first: for example, a recovery task composed for any `learner` block in any mode.

## 7. What needs live evidence or owner approval

No milestone M1 to M10 depends on a live run. Every test uses loopback providers, a memory store, or a task-owned Redis.

Live evidence is still necessary for these things, and each one is separate from this plan's merge gates:

- **Live C2 cache evidence** decides remedy (a) for P2 and belongs to the cache follow-up. The learner's cache correction works on whatever the ledger predicts.
- **Classifier latency at real digest sizes** with the fourth question. M1 adds a question to the request. A live call measures whether the answer time changes. The ruling says fan-out adds no latency, and one measurement confirms it.
- **The local prefill slope** for C5 and a **production local executor**. Without them a local target quotes the flat TTFT, and the learner's latency model for local plans is only as good as that quote.
- **Provider first-output measurements** under the learner in `shadow`. These arrive on their own once a project runs `shadow`, and the M10 report reads them from the log.

Owner rules that bound every live run, from the 2026-09-28 addendum: a live-provider test costs less than $20 USD per session, runs only on machines that the owner chooses, and starts only after the owner approves that run.

Owner approval is also required for: the merge of PR #18, which every milestone here is cut after, and each promotion of a project from `shadow` to `live` (M11).

## 8. How to continue

1. Read `CLAUDE.md`, then the two 2026-09-28 addenda in `synergies/typesafe-selector-and-cache-affinity.md`, then this plan.
2. Run `git fetch origin` and confirm that PR #18 has merged. Cut the milestone branch from `origin/main`. Trust only pushed state.
3. Take the next milestone in section 6 whose predecessors have merged. Write its stage brief from the milestone's settled points and section 4 of this plan. An implementation stage does not reopen a settled point. If a settled point turns out to be wrong in the code, stop and record the finding here as a dated addendum before the stage continues.
4. Tests first. Run targeted suites under `timeout 300` and the workspace suite under `timeout 900`. Commit before any mutation stage.
5. Before human review, run `wills-mega-review`. Write PR text with the `simple-english` skill. No PR text and no commit message names the assistant.
6. When a milestone merges, update the status table below and the corresponding row of `PLAN-routing-strategy-bandit.md`.

## 9. Status

| Milestone | Draft slice | Status | PR |
|---|---|---|---|
| M1 Jev tier question and agreement report | new | implemented on `ai/learner-m1-jev-tier`, awaiting review | |
| M2 strategies, learned input, keys, record types | L1 part 1 | implemented on `ai/learner-m2-strategies`, awaiting review | |
| M3 cost, latency, and cache corrections | L6 | implemented on `ai/learner-m3-corrections`, awaiting review | |
| M4 gate, constraints, exploration, policy | L1 part 2 | implemented on `ai/learner-m4-gate`, awaiting review | |
| M5 entries, credit, ops rows, Jev counts, cursor, marks | L2 | implemented on `ai/learner-m5-credit`, awaiting review | |
| M6 learner store contract and memory store | L3 | implemented on `ai/learner-m6-store`, awaiting review | |
| M7 Redis learner store | L4 | implemented on `ai/learner-m7-redis`, awaiting review | |
| M8 configuration, engine path, delivery, metrics | L5 part 1 | implemented on `ai/learner-m8-engine`, awaiting review | |
| M9 startup, recovery task, audit | L5 part 2 | implemented on `ai/learner-m9-startup`, awaiting review | |
| M10 calibrator and promotion report | L7 | implemented on `ai/learner-m10-calibrator`, awaiting review | |
| M11 live enablement | L8 | owner procedure | |

L3b, the source-side index, is done at `6a9eb07` and ships in PR #18. M5 connects `Session::commit` to it.

**M1 status, 2026-09-28.** M1 is implemented test-first on `ai/learner-m1-jev-tier`, from `485a093`. The seams agree with section 5. `TAXONOMY_VERSION` is 2. `TierChoice` is the fourth question in the same request. `evaluation.agreement` is on `/v1/metrics` for every scope and on the dashboard. The milestone did not settle these points, so the implementation made these decisions:

- The fold opens a slot at the classification intent. Thus a review that arrives before the answer still labels the disagreement.
- Agreement counts only the results that the evaluation join accepted.
- An answer with no served tier counts as `not_comparable`.
- The metrics fold uses the label of a review as written. Only the review tracker of the session can verify membership.
- The projection does not show the tier answer, so `PROJECTION_REVISION` does not change.

Independent mutations of these parts each failed their tests: the question map, the rubric, the served-tier source (pick and first dispatch), the label join, the bound, the acceptance join, the early label, the scope, and the dashboard tile. No live TypeSafe call occurred.

**M2 status, 2026-09-28.** M2 is implemented test-first on `ai/learner-m2-strategies`, from `75ccf2c`. The learner stays off: no engine path composes it, and no route changes. The seam map in section 5 was checked again. These rows are out of date:

- The catalog local price (ruling 6) landed in `82446c9` as `LocalCapacityPrice`. Local quotes are $0 only when the catalog sets no price. The M11 precondition therefore depends on code that exists now.
- The fallback order of ruling 4 (`f7ddc32`) is inside `StagePolicy::resolve`, and C3 fail-open (`ad5888c`) is in the engine. `route_pick` inherits ruling 4 as planned.
- M1 added a second reader of `SelectorBranch`, `metrics/agreement.rs`. The `Learned` arm reads the served tier from the recipe lists, which is the same rule as the `Stage` arm.
- `stage.rs` and `routing/mod.rs` are already large files. The extraction stays in `stage.rs`. The new code is in `routing/learn/`, and the new tests are in `crates/roundhouse-core/tests/learned_*.rs`.

Two points differ from draft section 7.7, which the M2 settled points name. Each record holds the same information. The orchestrator accepted both on 2026-09-28:

- `LearnedEvidence` holds the recipe lists one time (`RecipeEvidence`) and one `PlanEvidence` for each strategy, with its own pick and outcome. Section 7.7 lists "the `rules` selector evidence, boxed". That would put the `rules` pick and outcome in two places. `rules_stage()` rebuilds the stage evidence when a reader needs it.
- The three level keys that section 7.7 lists are not stored. The L2 key is the whole input, and L1 and L0 are projections of it (`LearnedInput::keys`). This is safe only while the projection is fixed for a given `LEARNED_SELECTOR_REVISION`. A change to how L1 or L0 are cut from L2 must bump that revision, so no reader re-derives an old record's keys under new rules.

The milestone did not settle these points, so the implementation made these decisions:

- The source rule moved to `StageOutcome::source(pick)`, and the recipe-tier lookup moved to `tier_named_by`. `StageEvidence` and `LearnedEvidence` use the same rule. `SelectionSnapshot::source` reads the source of the served plan. (The review fixes below replaced `tier_named_by`.)
- The learned input comes from the recorded `ClassificationWindow`. It resolves the named references against the accepted classifications of the session, and it checks the window cutoff again.
- A key part is spelled with dots and has no colon, for example `capable.low.no_high.tools`, because the store joins key parts with `:`. The tier words are `capable` and `efficient`, not the rationale labels `strong` and `weak`.
- `StrategySet` refuses a list that does not contain `rules`, a list with a repeated strategy, and a list with fewer than 2 or more than 3 entries, on the wire as well. `LearnerMode` (`off`, `shadow`, `live`) is separate from `ActiveMode` (`shadow`, `live`), so a record cannot say `off`.
- The rationale names the key of the gate level of the chosen strategy. When no strategy was chosen, it names the L2 key.
- `Tier` now derives `Ord`, because `LevelKey` is a map key in `PriorUnits`.

Each new test failed first, except three controls that passed before and after the change: `route_pick_with_the_rules_pick_equals_stage_policy_choose`, `a_record_without_learned_evidence_still_decodes`, and the `Override` control in the handoff test. These mutations each failed their tests: `prior` removed from L2, `rules_pick` removed from L1, sort by arrival, no cutoff check, no `K` truncation, a forced pick that copies the `rules` source, a forced pick that is stamped `Override`, `route_pick` refusing instead of degrading, the guard skipping a forced pick, a price in the rationale, the `Learned` arm without its box, `StrategySet` without the `rules` check, and the agreement arm returning no tier for a learned turn.

**M2 review fixes, 2026-09-28.** The PR 25 review and mutation pass found two design defects and three untested lines. All are fixed on the same branch. The wire shape of every existing record is unchanged.

- `LearnedEvidence` is now checked. `LearnedEvidence::new(LearnedEvidenceParts)` is the only constructor, and deserialization goes through it. It refuses a record whose plans are not a valid strategy list (so `rules` must be present), and a record with no plan for the served strategy or the chosen strategy. `served_plan()` and `rules_stage()` cannot fail now. Before this fix, `source()` returned `None` and the rationale dropped its served clause when the plan was missing. The configured order cannot be checked at decode, because a record is read without the configuration that wrote it.
- There is one recipe struct. `RecipeEvidence` moved to `selection.rs`, and `StageEvidence` holds it as a flattened `recipe` field. `RecipeEvidence::tier_of` replaces `tier_named_by`, `StageEvidence::tier_of`, and `LearnedEvidence::tier_of`. `SelectorBranch::source` and `SelectorBranch::tier_of` are the one place that decides which branches have a source or a recipe. `SelectionSnapshot::source` and the agreement report call them. An exact-JSON test pins the stage record's wire bytes.
- New tests for three mutations that survived: `StrategySet` accepting 4 entries, `LearnerMode::Off` mapping to a mode, and `rules_stage()` reading the served plan. Each test failed under its mutation.
- `LearnedChoice::Explore.member` is `u64`, the same width as `Draw.member`. The L2 doc says 40 keys are reachable (not 48), because `newest == None` forces `prior == Absent`. The size check on `SessionEventKind` is now a ceiling. The `handoff_escalation` rig moved to `tests/handoff_escalation/rig.rs`.

**M3 status, 2026-09-28.** M3 is implemented test-first on `ai/learner-m3-corrections`, from `bc0b5c8`. The corrections are pure functions in `crates/roundhouse-core/src/routing/learn/corrections.rs`. No engine path calls them, and no route changes. The tests are in `crates/roundhouse-core/tests/learned_corrections.rs`. The API is `Corrections::new(view, ledger, isl_tokens, latency_min_samples, cache_min_samples)` with `cost` and `first_output`, and the free functions `grant`, `latency_term`, and `adjusted_cached_tokens`. `ReadView::target` finds the counters of a target by its policy identity, so all workers of one local model share one residual.

Three pieces of context changed after the plan was written. This is how M3 handles each one:

- PR 23, `LocalCapacityPrice`: a local quote can be above $0. The cost correction still does not touch a local target. It records `NotFrontier` and keeps the quote as it is. The grant check calls `TurnBudget::admits`, which exempts local targets at any price. That is the budget's rule, so M3 does not add a second one.
- PR 21, the marker fact: the ledger predicts warmth only up to the last marker that a request sent. A request that placed no marker gets a cold quote, so the correction has no cached tokens to move on that turn.
- PR 22, a failed classifier call settles at its estimate: this has no effect on M3. Judge and classifier charges are not part of the per-turn cost (draft section 9).

The milestone did not settle these points, so the implementation made these decisions:

- **The reused count is also capped at the quote's own cached count.** Draft section 9 caps it at the matched prefix and the input. Today `FrontierCatalog::quote` sets the matched prefix to the floor of the weighted cached count, so the result is already one-sided. But the doc on `Candidate::matched_prefix_tokens` calls the field the raw prefix before weighting. A producer that followed that doc would let a reuse surplus price a route below its quote, and the owner's rule of 2026-09-28 forbids that. With the extra cap the correction can only raise a quote, whatever the producer does. The added test is `a_reuse_surplus_never_prices_a_route_below_its_quote`. Behavior under today's producers is unchanged.
- The checks run in this order: `NotFrontier`, then `TooFewSamples`, then `NoPredictedReuse` (which, since 2026-09-30, prices the quote with no cached tokens). Zero samples never meet a minimum, even a configured minimum of 0, so nothing divides by zero. When the predicted reuse is above zero and the observed reuse is zero, that is a full shortfall, and the correction applies.
- A latency mean rounds up, toward positive infinity, in integer arithmetic. The modeled first output is clamped at 0 ms. Both rules can only raise the estimate.
- `grant` takes the `BudgetState` from the plan's `Decision`. That field is public, so `Admitted` does not change. An overflow state returns `Overflow` before `admits` runs.
- `Corrections` needs a `ReadView`. On a turn whose store read failed, M4 can pass an empty view. Then every term records `TooFewSamples`, and the turn takes the section 7.5 path in any case.

The first run used a skeleton that returned the quote unchanged, `TooFewSamples`, and `Admits`. Five tests failed there on their own claim: `zero_predicted_reuse_applies_no_correction`, `a_latency_residual_applies_only_after_its_minimum_samples`, `latency_without_overhead_samples_uses_the_quote_plus_residual_and_is_recorded_unadjusted`, `an_adjusted_cost_above_the_grant_fails_the_grant_constraint`, and `an_overflow_admitted_candidate_keeps_its_status`. Five tests failed there only because the correction or the term did not apply yet. Each of them proved its claim under a named mutation below: the write premium, the separate miss penalty, the prefix bound, the once-per-turn overhead, and the quote cap. `the_adjusted_cached_count_stays_within_the_prefix_and_input` also failed on its scaling. `the_cache_correction_does_not_change_ttft` and `no_record_names_eviction` are controls, because their main assertion passed against the skeleton. Their mutations are the TTFT scaling and the `evicted` rename. These mutations each failed their tests: the plain input rate instead of the effective write rate, the overhead added once per target, the grant checked on the quote, no quote cap, no prefix cap, `>` instead of `>=` for the minimum, no overflow status, no `NoPredictedReuse` check, a corrected local target, a separate miss penalty, the cache shortfall scaling the TTFT, a floored mean, a variant renamed to `evicted`, and no clamp at zero. The clamp mutation survived at first, so a test assertion was added for it.

**M3 review fixes, 2026-09-28.** The PR 26 review found two defects and two documentation errors. The mutation pass found four lines that no test held. All are fixed on the same branch.

- The cost correction now holds the owner's rule in code, not in the rate card. The re-priced term is `(price(adjusted) - price(quoted)).max(0.0)`. Before this fix, a card with a cached read rate more than its effective write rate let a shortfall lower the quote. The new test `a_shortfall_never_lowers_the_quote_under_any_rate_card` failed before the fix: a quote of $0.003 became $0.0014.
- `grant` takes `&CostEvidence` and reads `adjusted_usd`. A call that passes the quote as a bare `f64` does not compile, and a `compile_fail` doc test on `grant` holds this. The "grant checked on the quote" mutation is now a type error. A mutation that reads `cost.quoted_usd` fails `an_adjusted_cost_above_the_grant_fails_the_grant_constraint`, because the evidence in that test has a quote that is different from its corrected cost.
- The docs of `CostCorrection::TooFewSamples` and `LatencyTerm::TooFewSamples` now say that the variant is also recorded for a target that is not in the view, and for zero samples under a configured minimum of zero. The comment on `a_cache_miss_adds_no_separate_penalty` now says that the floor can make a fractional cached count slightly more expensive than the quote. That is the allowed direction.
- New tests for four mutations that survived. Each test failed under its mutation. `the_cost_correction_uses_the_cache_sample_minimum` holds the cost check on `cache_min_samples`, not `latency_min_samples`. `too_few_samples_is_recorded_before_no_predicted_reuse` holds the check order. `every_worker_of_one_local_model_reads_the_same_residual` holds the lookup by policy identity. `a_prefill_above_the_input_quotes_no_cached_tokens` holds the lower clamp of the quoted cached count.
- The last test asserts that the corrected cost is equal to the quote, not only at least the quote. After the clamp, a negative quoted count shows only as a cost increase under an inverted card, so an "at least" assertion passes under the mutation.
- After the clamp, the "no quote cap" and "no prefix cap" mutations fail only on the direct `adjusted_cached_tokens` assertions, because the clamp hides their effect on the cost. Both assertions stay.

**M4 status, 2026-09-28.** M4 is implemented test-first on `ai/learner-m4-gate`, from `d7d499f`. The policy is pure and lives in `crates/roundhouse-core/src/routing/learn/gate.rs`, `explore.rs`, and `policy.rs`. No engine path calls it, and no route changes. The tests are in `crates/roundhouse-core/tests/learned_gate.rs`, `learned_policy.rs`, `learned_policy_examples.rs` (every row of draft section 7.8, plus an exploration row), `learned_explore.rs`, and `learned_policy_property.rs`, with shared fixtures in `learned_support/`. The API is `LearnedPolicy::choose(ctx, mode: ActiveMode, &LearningTurn) -> Result<Decision, LearnedError>` (the mode argument came with the review fixes below). The learned record rides on the decision's selector.

The milestone did not settle these points, so the implementation made these decisions:

- **`choose` takes a second argument and is not a `RoutingPolicy` yet.** (The review fixes below added the mode as a third argument.) The learner needs inputs that `RoutingContext` does not carry: the terms, the store read, the draw, the arm, and the classification window. `control/credential/access.rs` builds a `RoutingContext`, so a new field there reaches that module. M8 decides how the engine passes these inputs.
- **`LearnedError` is local to the learner.** It has three variants: `Routing` (admission or planning failed), `NoRecipe`, and `Refused { unmet }`. A new `RoutingError` variant would force an engine edit, because `incomplete_reason` in `engine.rs` names every variant. M8 maps `Refused` to an incomplete reason.
- **The policy computes the `rules` pick and encodes the learned input itself.** The turn passes the window, the accepted classifications, and the tool flag. A caller-built input could carry a pick that differs from the plan it is counted under.
- **`shadow` and `serve_rules` serve the `rules` decision whole**: its target, its own fallbacks, its budget state, and its admitted list. Only the rationale and the selector change. The draft 7.6 fallback rule applies to a learned route (exploit or explore). For an explored route, the fallbacks are the passing plans' first targets in exploit order.
- **The unmet list is every constraint that some plan failed, in a fixed order**: quality, latency, grant, then the read failure. A failed read replaces quality, because quality was not evaluated. The gate records `Unproven` with no level on such a turn.
- **An overflow admission meets the grant constraint** (draft 7.2). Only `GrantCheck::Exceeds` fails it.
- **The rate draw is the top 53 bits of the first 8 digest bytes, divided by 2^53.** Division of the whole `u64` by 2^64 rounds the top 1024 values to exactly 1.0, which is outside `[0, 1)`. `LEARNER_DRAW_VERSION` is `v1`. `Draw::for_turn` is the pure function that the engine calls. The policy never calls it.
- **Propensity under `refuse`.** When nothing passes, the non-exploring path serves nothing, so the served target has no `1 - rate` term. Under `serve_rules` the default target is the `rules` target. Because a member is strictly cheaper than the reference, no member shares the default target, and the two rules give the same number for every explored turn.
- **Local-only turns (J4)** are turns whose admitted pool holds no frontier target. They get no Jev prior and cannot explore. The gate otherwise runs as usual. The addendum says the rules route applies "until review evidence exists". A literal reading of section 2 ("serves `rules`") would also ignore live evidence on such a turn. The owner can choose that reading. The recorded choice would then need a new form, because a record that serves `rules` while it names another strategy as the choice breaks the `LearnedEvidence` invariant.
- **The gate keeps its bounds and prior in `GateReading`**, not in the record. The record keeps the level and the result, as draft 7.7 says, so the M2 wire shape does not change.
- **No property-testing crate is in the workspace**, so the property test is a deterministic exhaustive enumeration: 70,560 cases over six pool shapes, two pickers, four operational states, three prior states, and both `on_infeasible` values. No dependency was added.

**Finding: draft 7.5's "without exploration a live learner cannot change a route" is exact only per pool.** The property test gives live units only to the strategies whose first target equals the `rules` first target on the same pool, and there the claim holds. Credit earned on one pool can pass a strategy that routes differently on another pool of the same key. `credit_earned_on_another_pool_can_change_the_route_through_a_constraint` shows this. `capable` is credited on a pool that holds only `frontier/large`, where `rules` also serves it. On a later turn, `local/small` is admitted and too slow, so `rules` fails the latency limit and `capable` serves. This is the constraints working, not a defect, but the draft's sentence does not allow for it. The M10 report should not assume that a `live` project without exploration serves only `rules` routes.

**M4 review fixes, 2026-09-28.** The PR 27 review found one defect and four design points. The mutation pass found two lines that no test held. All are fixed on the same branch. The wire shape of every record is unchanged.

- **A prior alone could pass the gate under zero minimums.** With `min_evidence = 0` the gate opened a level that held no live units, and with `min_sessions = 0` it passed a level that no session reviewed, so a large artifact prior passed on its own. The gate now uses the corrections' rule (`corrections::enough`, now `pub(super)`): zero never meets a minimum, even a minimum of zero. It applies to the live units that open a level and to the sessions that `Pass` needs. `a_prior_alone_cannot_pass_under_zero_minimums` failed before the fix (`Pass`), and each half failed on its own: the level fix alone left the sessionless case at `Pass`.
- **One hard-constraint predicate.** `PlanEvidence::meets_hard` (grant not `Exceeds`, latency met) replaces the two spellings in the policy and in `explore::eligible`. With `latency_met` removed from it, seven tests fail: the exploration test, four in `learned_policy.rs` (latency limit, fallbacks, `serve_rules`, `refuse`), row 3 of the examples, and the other-pool property test.
- **`choose` takes an `ActiveMode`.** An `off` project never reaches `choose`: it is the stage decision and needs no store read and no draw. **M8 branches on `LearnerMode::active` before it reads the store or computes the draw, and calls `choose` only for `shadow` and `live`.** The `off` arm, and the `NoRecipe` error it could raise for an `off` project, are gone. `choose` does not read `terms.mode`.
- **`choose` is shorter.** The exploration arena, the rate and the non-empty set, is computed once and gates both the draw and the propensity. Planning is a `Planner` with `plan_all`, and the route is `route()`. Only the `rules` decision is kept after planning, so no `PlanEvidence` is cloned: the plans move into the record.
- **`StagePolicy::route_pick` returns a `RoutedPick`**: the decision, with the pick and outcome that its stage evidence records. This supersedes the M2 settled signature `-> Result<Decision, RoutingError>`. The learner no longer reads them back from the selector snapshot, so `stage_parts` and its impossible-branch error are gone. The struct holds the pick and the outcome, not the whole `StageEvidence`, because the recipe lists are the caller's own recipe, and cloning them on every stage turn would cost `StagePolicy::choose`, which drops them. `route_pick_with_the_rules_pick_equals_stage_policy_choose` still passes, and the strategies fixture now asserts that the returned pick and outcome equal the recorded ones on every branch.
- New tests for two mutations that survived. `the_draw_matches_a_golden_digest` holds a digest computed outside the crate, so a change to `LEARNER_DRAW_VERSION`, the domain string, or the encoding fails. `an_upper_bound_exactly_at_the_floor_is_unproven` sets the floor to the bound itself, so `<=` for `<` gives `BelowFloor` and fails.

**M5 status, 2026-09-28.** M5 is implemented test-first on `ai/learner-m5-credit`, from `4079d5b`. The fold is in `crates/roundhouse-core/src/session/learning.rs`, with credit in `learning/credit.rs` and the public entry types in `learning/entry.rs`. `SessionState` exposes `learning_page`, `learning_beyond`, `learning_hint`, `learning_causes`, and `project_learning`. `learning_mark` is public, and `Session::commit` calls it. `Session::record_learning_applied` writes the new event; no engine path calls it until M8. The tests are in `crates/roundhouse-core/tests/learning_entries.rs` and `learning_cursor.rs`, with fixtures in `learning_support/`. Draft section 23 records the automatic marks as a dated addendum to sections 21 and 22.

Two rows of the section 5 seam map were out of date: `commit` now passes the computed mark, and `SessionEventKind` has `LearningApplied`. The M5 review fixes updated both rows. The Redis fixture `every_event_kind` includes the new variant, but its Redis-gated round trip, and marked appends through `commit` on the Redis store, were not run in this milestone; only `MemoryStore` exercised the automatic marks.

What changes for an existing deployment: nothing while every project is `off`. No `Routed` carries learned evidence, so no entry exists, no append is marked, and the append bytes are unchanged. `LearningApplied` joins the one-way door of draft section 13.

The milestone did not settle these points, so the implementation made these decisions:

- **The row holds agreement, not indices.** Draft 8.1 lists the served target and each plan's first target as recipe indices. `LearningRow` holds the one fact credit reads from them: a bit for each strategy whose plan's first target is the target this dispatch went to. The comparison is structural (provider and model, or local model), so a recipe degrade to a local worker that the recipe does not name is still comparable.
- **A foreign credit revision suppresses every delta of that decision**, not only credit (draft 11.1: "contributes no deltas"). Its turn adds no operational rows, and a classification of it adds no Jev counts. The entry still exists.
- **Cache reuse per-mille rounds to nearest.** The rule for a measured pair is shared with the metrics fold (`metrics::cache_evidence::measured_pair`).
- **Jev retention.** The row of a learned turn is kept from its intent until the accepted result. It is dropped when a later intent is requested at or after its `expires_at_ms`. A result is delivered at the start of the next turn, possibly long after its call's deadline, and the next intent is written after that delivery. Pruning on the log clock of any event would drop late-delivered answers.
- **The residual quote** is the `expected_ttft_ms` of the candidate in `considered` that equals the served target. A served dispatch with no such candidate supplies neither the residual nor the overhead, so the two samples still come from the same turns.
- **`beyond` is never an undercount.** When a `LearningApplied` reaches past the page but not the newest entry, the count is kept as an upper bound, and the backfill finds the exact set.
- **Causes.** `LearningCauses` counts `unknown_label`, `failover_in_interval`, `missing_row`, `mixed_epoch`, and `other_credit_revision`, in the draft's check order.

**Finding: with per-decision agreement, the failover rule decides the cause and not the deltas.** A failover turn writes two `Routed` to two targets. A strategy's plan has one first target, so no strategy agrees with both dispatches, and consistency alone already credits nothing. Rule 1 of draft 8.2 still runs first. It is observable through `failover_in_interval`, which is how its test holds it.

The tests were run first against a skeleton that produced no entries, and then against one that produced entries with no deltas. At the first stage every test failed except `learning_applied_has_no_response_id_and_is_not_terminal` and `learning_mark_marks_nothing_before_learned_evidence`. At the second stage every credit, operational-row and Jev test failed on its own claim. Two tests passed there, `a_classification_without_a_tier_answer_yields_an_entry_with_no_deltas` and `a_classification_for_a_turn_without_a_learned_row_yields_an_entry_with_no_deltas`, so they count as controls. They and the other two controls are held by named mutations. These mutations each failed their tests:

- `ClassificationRecorded` dropped from the entry list;
- a strategy credited when any turn agreed;
- the failover rule removed;
- the epoch check removed;
- a negative interval counted as positive;
- no remainder;
- the Jev count on only one key;
- a mark that ignores learned evidence;
- a mark that names the first event instead of the newest;
- a mark that ignores the session state;
- `commit` passing no mark;
- `LearningApplied` made terminal;
- a missing tier answer counted;
- a row kept for any turn;
- the residual measured from turn start;
- latency sampled on an incomplete turn;
- a cache sample for a local target;
- `beyond` reset on every acknowledgement;
- no expiry prune;
- operational rows under a foreign credit revision;
- a backfill that ignores its floor.

**M5 review fixes, 2026-09-29.** The PR 28 review found one defect and three smaller points. The mutation pass found two lines that no test held. All are fixed on the same branch. The wire shape is unchanged.

- **A backfill did not reach below a stale hint.** `project_learning(store, id, hold_after)` held only entries above `hint.max(floor)`, and each `LearningApplied` in the log pruned its page. Suppose the log holds `LearningApplied { H }` and the learner store's watermark is `W < H`. This happens when the store lost recent writes (draft 11.6), or when the audit finds a watermark below the mark (11.7). Then the backfill from `W` skipped the entries in `(W, H]`. The store answered `ChainGap { W }` every time, and delivery for that session stalled for good. The hold threshold is now a two-variant `Hold`. A `Hint` fold is the live fold or a plain replay, and it behaves as before. A `Floor(W)` fold is a backfill. It holds every entry above `W`, and `LearningApplied` moves only its hint. `a_backfill_below_a_stale_hint_refills_from_the_floor` failed before the fix: the page was `[11, 12, 13]` where `[7, ..., 13]` was expected. The same test holds the control that the live fold still prunes at the hint. Only the pruning half of the old rule can be reached by a test. In a well-formed log a `LearningApplied { H }` always follows the entries through `H`, so the entry-time check against the hint never fires on its own.
- **The test helper hid the same defect.** `Script::entries` backfills from zero, and it silently lost the entries that an earlier `LearningApplied` acknowledged. It now asserts a whole chain, from `prev_seq == 0` through every link. `every_entry_producing_event_yields_one_entry_even_with_empty_deltas` now acknowledges an entry before it reads the chain. It failed before the fix, with a first `prev_seq` of 14.
- **`LearningApplied` is per session.** Its doc said that only a project whose learner is not `off` writes it. A session with a learned `Routed` keeps producing entries after its project returns to `off` (draft section 23), so it also keeps receiving this event.
- **The learner-off allocation comment.** `turn_started` clones the `ResponseId` on every `TurnStarted`, and the comment in `routed` implied the off path allocates nothing. The comment is corrected rather than the clone removed. If the clone were gated on `started`, the first learned turn would lose its rows, because its `TurnStarted` comes before its learned `Routed`. If the id were bound at the first learned `Routed`, the fold would accept a dispatch of a response that is not the open turn. The id match rejects that today, and `record_routing` does not.
- New tests for two mutations that survived. `a_mixed_epoch_review_counts_only_the_epoch_cause` and `a_foreign_credit_revision_review_counts_only_the_revision_cause` each isolate one cause, so swapping the two counters fails both. `cache_rows_require_provider_measurement` adds a 4506-of-10000 read: to nearest that is 451 per mille, and truncated it is 450, so `.floor()` for `.round()` fails.

These mutations failed their tests: the two cause counters swapped; per-mille truncated; `applied` pruning a backfill's page (the original defect); `applied` never pruning, which holds the live-fold control and fails three cursor tests. One mutation survives, as expected: a backfill's entry-time threshold set back to `hint.max(floor)` while `applied` still keeps its page. It is equivalent on a well-formed log, for the reason in the first bullet.

**M6 status, 2026-09-29.** M6 is implemented test-first on `ai/learner-m6-store`, from `f36f70b`. The trait, the request and batch types, and the errors are in `crates/roundhouse-core/src/learn_store.rs`. The memory backend is `learn_store/memory.rs`. The contract suite is `learn_store/contract.rs` and `learn_store/contract/isolation.rs`, listed by `learner_store_contract_suite!`. The memory store runs it in `learn_store/memory/tests.rs`, beside the memory-only cases: the fault between the phases, and the keys that a read visits (the review fixes below add when each of these is armed). No engine path calls the store, and no route changes.

The milestone did not settle these points, so the implementation made these decisions:

- **The macro is `learner_store_contract_suite!`**, named after the trait like the other families. Draft L3 called it `learner_contract_suite!`.
- **`ReadRequest::new` takes a `LearnedInput`**, so a read always names the three levels of one turn. The targets are policy identities, and a repeated target is read once. The engine can build the input before `choose` with the public `pick_tier` and `LearnedInput::encode`. M8 must pass the same pick and window that `choose` uses, or the read and the decision name different keys.
- **The read has one fixed shape.** It returns every requested key, strategy, and target, in request order, with zeros where the store holds nothing. The view is recorded on every learned `Routed`, so a backend that dropped empty rows would write a different log for the same state. A contract case holds the exact shape.
- **`Malformed` is about the input alone, and `CounterRange` is about the input against the stored counters.** `LearningBatch::check` is the shared input check that every backend runs first: sequences within `2^53 - 1`, strictly ascending from 1, `prev_seq` below `seq`, and every delta within the exact range. The counter types are `u64`, so a negative delta reaches the store only by a wrapping cast. `a_negative_delta_for_a_nonnegative_counter_is_refused` sends `-1i64 as u64` and expects `Malformed`.
- **Counters are range-checked after every entry, not only at the end of the batch.** A Lua number above `2^53` is not exact, so M7's script must refuse the step that crosses the limit. `a_signed_sum_is_range_checked_after_every_entry` holds this rule for the signed sums, which can cross the limit and come back.
- **No `WrongType` variant yet.** Only the Redis backend can hold a key of the wrong type, so M7 adds the variant together with its producer.
- **The Jev counts are the fields `jev_capable` and `jev_efficient` of each quality key. The overhead sums are `turn:pre_sum` and `turn:pre_n` of the operations key.** `sessions` counts a `(level, key, strategy)` once for each session and epoch, from the `seen` set. The staged `seen` members of a batch count as well, so two entries in one batch add one session.
- **`LearnerStoreControl`** is a test-support trait, like `LeaseControl`. Its `snapshot` and `restore` of one project are how the suite checks "changed nothing" and "made no write", and how it stages store loss. **M7 must build them for Redis:** a `SCAN` over the project's `learn` key prefix, then `DUMP` and `RESTORE`. The snapshot must include the watermark hash and the `seen` sets.

Every test failed first on its own claim, except the two controls named below. The first run used a store that applied nothing and read zeros. Every test failed there except `read_makes_no_write`, which is a control. The second run used a naive store that added every entry, with no skip, no `prev_seq` check, no staging, no range check, and one session for each delta. Five tests passed there. Three of them had failed in the first run on their own claim: `apply_then_read_returns_the_integer_sums`, `read_returns_the_jev_counts_and_overhead_sums_of_the_turn`, and the exact-shape case. The other two are controls. `read_makes_no_write` passed both runs, and `read_visits_only_the_keys_of_the_turn` failed the first run only on its setup. A read that creates empty keys fails the first control, and a read that visits extra keys fails the second.

These mutations each failed their tests: a batch-level watermark (7 tests, among them the lost acknowledgement and the gap; the fault-hook test fails only on the double count of its retry, not on a partial write); no `prev_seq` check (the gap, store loss, and the windows); no staging, which writes each entry as it is checked (the gap, the overflow, and the fault hook); a gap that reports the staged watermark; a skipped batch that returns its own last `seq`; no input check; no range check on the signed sums; sessions that ignore the stored `seen` set; sessions that ignore the batch's own `seen` set; `seen` shared across epochs (this mutation survived at first, so the epochs test now applies its two epochs in two batches); a read that ignores the epoch; a watermark read across projects; a read that creates empty keys; a read that drops empty strategy rows; the overhead not stored; a read that visits the operations key once for each quality key; and targets that are not deduplicated.

**Finding: the draft section 15 table does not hold for the batch-level watermark mutation on the overflow case.** In the draft's model, that mutation also left a change after an overflow. Here the staging is independent of the watermark rule, so the overflow test still passes under the mutation. The gap and lost-acknowledgement tests fail, as the table says.

**M6 review fixes, 2026-09-29.** The PR 29 review found one defect and two gaps in the contract. The mutation pass found three lines that no test held. All are fixed on the same branch.

- **A diverged chain is not a gap.** `stage` returned `ChainGap` in two cases. In the first, `prev_seq` is above the watermark: an entry is missing, and a backfill supplies it. In the second, `prev_seq` is below the watermark and `seq` is above it: the sender's chain has no entry at the watermark, so a backfill from the watermark sends the same entry again, forever. The second case is now `LearnerError::ChainDiverged { store_watermark }`. The trait doc says what each error means. `ChainGap` means "backfill from this watermark". `ChainDiverged` means "stop this session's delivery and report it; do not backfill". **M8 maps `ChainDiverged` to a reported stop for that session, never to a backfill.** It is not a project-epoch stop like `CounterRange`, because other sessions of the project are not affected. The rule compares `prev_seq` with the watermark as the earlier entries of the batch moved it, the same value the equality check uses. It still reports the watermark that the store holds. `a_diverged_chain_is_refused_and_is_not_a_gap` failed first with `ChainGap { 20 }` for the entry `(30, 10)`. It holds the control `(30, 25)`, which is still `ChainGap { 20 }`. It also holds a case inside one batch, `[(30, 20), (40, 25)]`. A rule that compares with the store watermark instead applies entry 40 on top of 30, and this case fails under that mutation. Draft section 11.2 step 2 still names only `ChainGap`. This note is the addendum.
- **Every input rule has a contract case.** `every_input_rule_refuses_its_batch_as_malformed` sends four batches: sequences that do not ascend, a `prev_seq` equal to its `seq`, a `prev_seq` above `2^53 - 1`, and a signed delta of `i64::MIN`. Each case is one that the chain rule alone answers differently, so removing its rule fails the test. Without the ascending rule, the batch applies 2 entries. Without the predecessor rule, both `prev_seq` cases return `ChainGap`. Without the signed bound, the delta returns `CounterRange`. The separate `prev_seq > 2^53 - 1` clause is deleted, because it could never be the rule that refuses: `prev_seq` must be below `seq`, and `seq` is within the bound.
- **`LearnerStoreControl` snapshots tell an absent project from an empty one.** The memory snapshot is now `Option<ProjectState>`, and a restore of `None` removes the project. `a_call_with_nothing_to_write_leaves_no_trace` checks a read, a watermark query, and an empty batch on a project the store has no data for. It also checks an empty batch for a new session on a project that exists. Two mutations now fail: a read that creates an empty project entry, and an apply that commits when nothing applied. The second is also held by a variant that commits only for a project that exists. A batch that skips every entry of a known session is a control. Its commit would rewrite the same values, so that mutation cannot fail there. **M7's Redis snapshot of an absent project is the empty key set, and a restore of that set deletes every key under the prefix.** The snapshot type is now also `Clone`.
- **The store-loss case credits different strategies before and after the loss.** Entries 10 and 20 credit `rules`. The lost entry 30 and the later entry 40 credit `capable`. A `restore` that kept the live `seen` sets passed the old test. It now fails, with `capable` sessions at 0 instead of 1.
- **The visit recorder is off until a test calls `record_visits()`.** A dependent that turns on test support for another reason no longer grows a list on every read. `an_unarmed_store_records_no_visits` failed first, with 8 recorded keys.
- **The fault hook fires on the next apply that has something to write.** An apply that the check refuses, or one that skips every entry, leaves it armed. `the_fault_hook_fires_on_the_next_apply_that_would_write` failed first: the skipped batch used up the fault.
- Smaller points. `LearningBatch<'a>` borrows its project, its session, and its entries, so M8 does not clone the fold's page on every turn tail. `MAX_EXACT_SIGNED` is deleted. One `key_name` function spells the quality and operations keys as the module doc's table does, `{epoch}:q:{level}:{key}` and `{epoch}:ops`. The visit recorder and the counter that `CounterRange` names both use it. `Staged::quality_hash` replaces the repeated seeding of a quality key. The trait doc says that `read` fails only with `Unavailable`; the M7 wrong-type refusal will join it.

**M7 status, 2026-09-29.** M7 is implemented test-first on `ai/learner-m7-redis`, from `95f19f7`. The store is `RedisLearnerStore` in `crates/roundhouse-store-redis/src/learn.rs`, with the two scripts in `learn/scripts.rs`. `KeyFamily::Learn` (`learn`, `v1`) is in `keys.rs`, and the crate's family table has its row. `tests/learn_contract.rs` runs `learner_store_contract_suite!` unchanged, under the Redis ignore reason, with the Redis-only cases beside it. `LearnerError::WrongType { key }` is added in core, and the trait doc now says that `read` fails only with `Unavailable` or `WrongType`. No engine path calls the store, and nothing selects it until M9.

The milestone did not settle these points, so the implementation made these decisions:

- **Rust names every key and every field; the scripts name none.** Each key reaches a script through `KEYS`, and each hash field and `seen` member through `ARGV`. The apply script is a small counter machine over three ops: add to an unsigned counter, add to a signed sum, and count a session once per `seen` member. So `build_key` is the only spelling of a key, and the fields that `read` asks for are the ones `apply` writes. `every_learn_key_is_built_by_the_shared_builder` checks both halves from the source: the four key functions call `build_key` under `KeyFamily::Learn`, and every `redis.call` in the scripts names its key as exactly `KEYS[...]`. `the_store_writes_only_the_keys_its_key_functions_name` checks the live half: after an apply that touches every key kind, the namespace holds exactly those keys.
- **Keys carry a `{project}` hash tag.** The layout is `…:learn:{<project>}:wm`, `…:{<project>}:<epoch>:q:<level>:<key>`, `…:{<project>}:<epoch>:ops`, and `…:{<project>}:<epoch>:seen:<session>`. The tag follows the `spend` and `fairuse` families, and it also ends the project segment for the test-support `SCAN`. Draft section 11.4 says the crate has no hash-tag convention. That was out of date; the crate has had one since R-S3. The store still claims single-instance only, as the settled point says.
- **Types are checked first, for every key the batch names.** This includes keys that only a skipped entry would touch. A key of the wrong type is foreign data, and its recovery does not depend on which entry meets it first.
- **A stored value that is not an exact integer in range** is refused as `CounterRange` by `apply`, naming the key and field, and as `Unavailable` by `read`, because the trait allows `read` only those two errors. This store never writes such a value.
- **`watermark` is a plain `HGET`**, with Redis's `WRONGTYPE` mapped to `WrongType`. It is not a third script.
- **Every reply is an array of integers.** The first element is a reply code. Refusals return 1-based positions in `KEYS` and `ARGV`, and Rust turns them back into names. Counters are passed to `HSET` as Lua numbers. Redis 7.4 writes those as exact decimal integers up to `2^53 - 1`, and a probe against the session's Redis confirmed this.
- **The test-support snapshot** is a `BTreeMap` from key to `DUMP` bytes, over a `SCAN` of the project prefix with glob characters escaped. An absent project is the empty map. A restore deletes every key under the prefix, then restores the snapshot's keys. A project id that contains `}:` could match another project's keys. Every test mints `proj_<hex>` ids.

Red lines, in the order the tests first ran. With the real snapshot and restore, a stub store that applied nothing and read zeros, and stub scripts that returned `{'TODO'}`: 22 of the 25 `learn_contract` tests failed on their own claim, for example `a_lost_ack_then_a_new_entry_credits_each_entry_once` with `Applied { applied: 0, watermark: 0 }` where `{ 1, 20 }` was expected, and `the_store_writes_only_the_keys_its_key_functions_name` with an empty key set. Three passed as controls: `read_makes_no_write`, and the two `Malformed` cases, which core's `LearningBatch::check` answers before the store is called. `the_scripts_return_only_integers` failed with `apply: [bulk-string("TODO")]`. `every_learn_key_is_built_by_the_shared_builder` failed with `no redis.call found in src/learn/scripts.rs`. With the real scripts but no type checks, every contract case passed, and `a_wrong_type_key_is_refused_before_any_write` failed in all four key kinds with `Unavailable("WRONGTYPE …")` where `WrongType { key }` was expected. The check phase already stopped before any write, because `redis.call` raises on the first command against the sabotaged key. So the "before any write" half is held by a mutation, not by this first run.

These mutations failed their tests: an apply that writes each counter as it stages it and does not type-check the sets (the wrong-type case, "apply wrote before its refusal"); a read that writes a field (`read_makes_no_write`, `a_call_with_nothing_to_write_leaves_no_trace`); a write of the watermark when nothing applied (the no-trace case); a gap that reports the staged watermark (the gap case); no range check (the overflow and signed-sum cases); session counting that ignores the batch's staged `seen` members (four cases); a store that skips `LearningBatch::check` (the three `Malformed` cases); a `tostring` watermark in a reply (the integer case); a `SCAN` pattern that matches nothing (the key-set case and store loss); a hand-formatted `ops_key` (both convention tests). One mutation survived at first. A script key built as `KEYS[1] .. ':x'` passed the convention check, because the check only looked for a `KEYS[` prefix. The check now requires the key argument to be exactly `KEYS[...]`, and that mutation fails it.

**M7 review fixes, 2026-09-29.** The PR 30 review found three defects. The mutation pass found two lines that no test held. All are fixed on the same branch. The key layout and the reply codes are unchanged.

- **A large batch failed after a partial write.** The write phase sent each hash in one `HSET` with `unpack`. Lua's `unpack` fails past about 8000 values, and the script writes the quality hashes before the operations hash. So a batch with a credit entry and then 700 targets stored the quality hash and then failed, with no watermark. The script now sends every `HSET` and `SADD` in chunks of at most 1000 values. `a_batch_past_the_lua_unpack_limit_applies_whole` failed first with `too many results to unpack`. A chunk size of 100000 fails it again. No batch built from real deltas names more than 8000 new `seen` members, so `a_seen_set_past_the_lua_unpack_limit_applies_whole` in `learn/tests.rs` builds the plan by hand, with 9000 members. An unchunked `SADD` fails it with the same error.
- **One key list and a type string.** `KEYS` is now every key in first-touch order, the watermark hash first. `ARGV[2]` is a type string with one letter per key, `h` or `s`. The apply script no longer computes where the sets start, and the Rust side lost `Slot`, `Op`, and its second pass. The behavior is unchanged: the contract suite, `the_scripts_return_only_integers`, and both convention tests pass as before.
- **A foreign stored value is refused with the right name, by a strict parse.** This replaces the M7 decision about such values. `counter` in the scripts accepts only decimal digits, with a leading `-` only for a signed field. Lua's `tonumber` alone also reads `0x10`, `1e3`, and ` 5`. The read script now gets one sign letter per field, so it refuses a negative count itself. Only `lat_sum` and `turn:pre_sum` are signed. `read` returns `Unavailable` that names the key and the field. Before, it named only the key, or it reported "an unexpected reply" for a negative count. `watermark()` uses the same rule as the script: digits only, within `2^53 - 1`. Rust's `parse` alone accepts `+5`. `a_foreign_stored_value_is_refused_by_read_naming_its_key_and_field` failed first in all seven cases. For example, `1e3` in `capable:n` read as 1000. `a_foreign_watermark_is_refused_by_apply_and_by_watermark` failed first in six cases. For example, `0x5` gave `ChainDiverged { 5 }` from `apply` and `Unavailable` from `watermark()`, and `+5` gave `Ok(5)` from `watermark()`. `apply` still refuses such a watermark as `CounterRange`, naming the key and the session.
- **Mutation: a read that takes a non-integer stored value as 0.** The case above fails under it, in all seven cases.
- **Mutation: a `SCAN` pattern without the `}:` after the tag. Partly valid.** The review named `project_prefix` in `learn.rs`. The byte pin `every_learn_key_is_pinned_by_byte` already fails when that function drops the `}:`. The line that no test held is the pattern in test support, now `learn_project_pattern`, which snapshot and restore share. Project ids never shared a prefix, so a pattern there without the `}:` survived. `a_restore_leaves_a_project_whose_id_extends_it_alone` uses `proj_ab` and `proj_ab12`. Under that mutation the byte pin passes, and the snapshot of `proj_ab` holds the three keys of `proj_ab12`.
- Smaller points. The convention test now also requires every script command to be `TYPE`, `HGET`, `HMGET`, `HSET`, `SISMEMBER`, or `SADD`; a `COPY` whose key is `KEYS[1]` fails it. A reply position outside `KEYS` or `ARGV` is now always refused as an unexpected reply; before, one path printed `<unknown key>`. The target counters and their signs are one table, which both the read and the apply use. Not fixed: the read script still sends all operations fields to `HMGET` in one `unpack`. A read of more than about 1333 distinct targets fails as `Unavailable`. It writes nothing, so this is not an atomicity defect. The test-support snapshot says that `SCAN MATCH` walks the whole keyspace and that a restore deletes only under the project prefix. Draft section 11.4 has a dated addendum that points here.

**M8 status, 2026-09-29.** M8 is implemented test-first on `ai/learner-m8-engine`, from `5cd0c7b`. The configuration is `LearnerConfig` in `crates/roundhouse-server/src/control_config/learner.rs`, resolved into `Admission.learner: Option<Arc<LearnerTerms>>` beside `tiers`. The engine seams are in `crates/roundhouse-server/src/engine/learning.rs`: `Engine::with_learner`, the learned choice in `plan`, and `Engine::deliver_learning` in the `run_turn` tail. The artifact format and the epoch id are in `crates/roundhouse-core/src/routing/learn/artifact.rs`. The metrics are the `learning` section, folded in `crates/roundhouse-core/src/metrics/learning.rs`. The engine tests are `crates/roundhouse-server/tests/learned_routing_engine.rs` with `learned_routing_engine/{rig,delivery,classified}.rs`. The binary attaches no learner store yet: M9 composes one at startup.

What changes for an existing deployment: nothing. With no `learner` block, or `mode: off`, a turn calls the composed policy exactly as before, and the tail returns before any store call because the session has no learned history. With `shadow`, each learned turn makes one store read bounded by `read_timeout_ms`, writes `SelectorBranch::Learned` and a learned rationale on every `Routed`, and serves the `rules` decision whole (target, fallbacks, budget state, admitted list). Each tail with entries pending makes one apply, one mark clear and one `LearningApplied` append. The policy name on the record does not change until M9 composes `learned`.

These settled points overrode the M8 text, as the stage brief ruled:

- **No `RoutingContext.learning` field.** The context is built in part in `control/credential/`. The engine passes a `LearningTurn` beside the context and calls `LearnedPolicy::choose` itself. The read and the decision use the same signals, window, accepted classifications and tool flag, so they name one key. Two tests check the visited keys against the recorded input, one of them with a classification window.
- **The mode branch comes first.** `Engine::learning_for` returns `None` for no learner, no block, or `off`, before anything reads the store or draws.
- **Delivery follows the log, not the mode** (draft section 23). The tail runs for any session with a non-empty page, whatever the project's current mode.
- **Store errors.** A read error maps to `StoreUnavailable`, and a late read maps to `ReadTimedOut`. The `apply` answers map as follows:
  - `ChainGap`: one backfill from the store watermark.
  - `ChainDiverged`: stop the delivery of that session and report it. Never backfill.
  - `CounterRange` and `Malformed`: stop the project epoch. (Superseded 2026-09-29 by the M8 review fixes below: a stop of that session.)
  - `Unavailable`, `WrongType` and a timeout: leave the entries pending. The draft stopped the epoch for `WrongType`. The `LearnerError` doc now gives the new rule.

The milestone did not settle these points, so the implementation made these decisions:

- **Addendum to draft 11.5 step 6: a gap's backfill is applied in the same tail.** The draft sends the backfilled page "on the next turn". That cannot work: the live fold's page still starts above the store watermark on the next turn, so the store answers with the same gap, forever. The tail applies the backfilled page at once: one backfill and a second apply. `a_gap_is_backfilled_once_and_applied_in_the_same_tail` holds it.
- **The artifact format is draft 14.5**, parsed in core so that M10 writes what the server reads. Every field is required and unknown fields are refused. It must match this build's input, selector and credit revisions, name the gate `wilson-v1`, and give a valid strategy list. A prior entry must name a listed strategy, once, with `pos <= n`. The epoch is SHA-256 over the SHA-256 of the artifact bytes, the ordered strategy list, `LEARNING_INPUT_REVISION`, `LEARNED_SELECTOR_REVISION`, `STAGE_SELECTOR_REVISION` and `LEARNING_CREDIT_REVISION`, truncated to 16 bytes. The stage revision is included because it versions the `rules` pick. The loader reads the file at validation, so an unreadable artifact stops the boot and an admin mutation that references one is refused.
- **`LearnerTerms` gains `read_timeout_ms` and `apply_timeout_ms`**, which keeps the settled type `Arc<LearnerTerms>`. The policy never reads them. (Superseded 2026-09-29 for `apply_timeout_ms` by the M8 review fixes below: it moved to `Admission`.)
- **An `off` block resolves to `None`**, not to terms with mode `off`. Present fields are still checked in every mode. A block on a project without `tiers` is refused even when `off`.
- **`ProjectEntry.learner` is boxed and skipped on the wire when absent.** A document with no learner block is byte-identical to one written before M8, so an older node reads it. A document with a block is the one-way door of draft section 13, and the directory schema stays at 1. The pinned directory document now carries a full block.
- **A refusal is `EngineError::LearnerRefused { unmet }`**, terminated as `IncompleteReason::PolicyRefused`. The change adds no `IncompleteReason` variant, because a new variant is a wire change that an older node cannot decode.
- **The read names every candidate target**, not only the recipe's, because a plan can degrade to a local worker the recipe does not name.
- **A session whose project no longer configures a learner** is delivered under `UNCONFIGURED_APPLY_TIMEOUT_MS` (250 ms, the section 4 starting value), because its project has no `apply_timeout_ms` any more.
- **Stops are process memory.** A diverged session, and a stopped `(project, epoch)`, are held in the engine until it restarts. A restart meets the same refusal and stops again, and a new artifact resumes a stopped project. (Superseded 2026-09-29 by the M8 review fixes below: every stop is a session's, and a new artifact resumes nothing.)
- **The acknowledgement is appended even when the mark clear fails.** The append only moves the hint, and the kept mark lets the recovery task confirm the delivery. (Superseded 2026-09-29 by the M8 round-2 fixes below: the append runs first, and a failed append skips the clear rather than the other way around.)
- **Delivery outcomes are not in the log.** Applied and duplicate entries, backfills, gaps, stops, outages, timeouts and failed acknowledgements are counted in `LearningDelivery` beside the metrics fold, in process memory, and reported as `learning.delivery` in the deployment and project scopes and as `null` in a member's scope. The fold still reproduces from the log. Review causes are not in the section: the credit rule decides them in the session fold, and a second spelling would drift.

- **The epoch includes `STAGE_SELECTOR_REVISION`. This needs a ruling.** Draft section 6 names three revisions. The fourth is included because it versions the `rules` pick. An owner who disagrees removes it before any project runs `shadow`, because the epoch is a key part of every stored counter.
- **`TurnResult.last_seq` now includes the `LearningApplied` event** on a turn that delivers, because delivery runs before the tail reads `last_seq`.

Two points are left for M9:

- **The artifact is a node-local input to directory compilation.** The directory compiles only when the stored document changes, so the file read is not on every admission. But `CompiledUnder` does not record the artifact bytes. Two nodes with different bytes at one path run two epochs, and no divergence is reported. A node that cannot read the path fails that compile and serves its last compiled plane, as for any configuration that its environment cannot satisfy.
- **The binary attaches no learner store**, so a `learner` block in a deployment's file has no effect and gives no warning until M9 composes one.

Red lines. The first run used a skeleton:

- `learning_for` returned `None`, and `deliver_learning` returned at once.
- The loader resolved every block to `None`, without `deny_unknown_fields`.
- The metrics fold was not wired.

21 of the 22 engine tests written first failed on their own claim:

- Row 1 and the exploration row served `large` where `small` was expected.
- Rows 3 to 8, the store outage, two projects, row 2 and the Jev test found no learned record.
- The count test read `(0, 0, 0, 0)` where it expected `(1, 1, 1, 1)`. The review test read `(0, 0)` units.
- The refused-ack, timeout, successor, gap and diverged tests read 0 residuals or 0 applies. The steered test read 0 applies where it expected 2.
- The metrics test read 0 decisions where it expected 2.

`an_off_project_records_no_learned_evidence` passed there and is a control. All 10 configuration tests failed: 7 with `expected LearnerRejected, got Ok(..)`, two on a `None` learner, and the unknown-field test on an `Ok`. With the implementation, the review test first failed with `(3000, 3000)` where it expected `(1000, 1000)`. One interval credits `CREDIT_SCALE` at each of the three levels, so the test was corrected.

These tests were written after the implementation, and mutations hold them:

- `a_project_turned_off_still_delivers_its_sessions_pending_entries`.
- `a_learned_route_fails_over_to_a_passing_plan_and_counts_one_decision`. It checks the draft 7.6 fallbacks, one selection on both dispatches, one metrics decision, and one failover row on `small`.
- `the_epoch_matches_a_golden_digest`, with a value computed outside the crate, and `every_content_rule_refuses_its_artifact` (gate, prior range, repeated prior, input and selector revisions).

These mutations each failed their tests:

- A timed-out read reported as `StoreUnavailable`, and a read with no timeout: row 6.
- Delivery moved to the start of the turn: 11 delivery tests.
- Delivery only on dispatched turns: the steered test.
- `off` treated as `shadow`: the off test and the turned-off test.
- Delivery gated on the current mode: the turned-off test.
- `ChainDiverged` backfilled like a gap, and stopped sessions not checked: the diverged test.
- A gap's backfill sent on the next turn: the gap test.
- The draw salted with the empty string: the exploration row.
- The read keyed without the window: row 2.
- Each configuration refusal removed (no tiers, exploration outside `live`, the artifact list, `deny_unknown_fields`, the zero timeout, the strategy-list check), `on_infeasible` resolved to `refuse`, and a default rate of 0.1: the configuration tests.
- Acknowledgements not folded, decisions not folded, and every dispatch counted as a decision: the metrics and failover tests.
- The epoch without the credit revision, with another prefix, or over the raw bytes: the golden digest. The gate, prior-range, repeated-prior and selector checks removed: the content-rule test.

Delivery after the lease release does not compile, because `release` consumes the session.

**Orchestrator ruling, 2026-09-29 (M8 decision 3).** The learner epoch id includes `STAGE_SELECTOR_REVISION`. That revision versions the `rules` pick, which every learned key embeds (`rules_pick` is part of every level). A change to the rules picker therefore starts a new epoch rather than reusing credit earned under a different meaning of the key.

**M8 review fixes, 2026-09-29.** The PR 31 review found five defects and six smaller points, and the mutation pass found four lines that no test held. The orchestrator ruled each one. All are fixed test-first on the same branch.

- **A refused entry stopped the project under every later epoch.** The stop set was keyed by the project and the admission's epoch. But the refused entry stays in its session's page under its own `Deltas.epoch`. So under a new epoch the same page was sent again, refused again, and the whole project stopped again. `Malformed` depends only on the batch, so it never clears. Ruling: `CounterRange` and `Malformed` now stop that one session, like `ChainDiverged`. There is one set, `RoutingLearner::stopped_sessions`, and the project-epoch set is deleted. The `Stopped` outcome and its log line stay; the log line names the project, the session and the epochs of the refused page. A new artifact no longer resumes anything. This supersedes the M8 note above and draft sections 11.3 and 11.6. `a_malformed_session_under_a_new_epoch_stops_only_itself` failed first: the other session had 0 acknowledgements where 1 was expected. `ProbeStore::refuse_session` refuses every apply for one session. A FIFO script cannot reproduce the defect, because the second send of the page would reach the real store.
- **A gap after a dry-page refill was never backfilled.** One flag served both the refill and the gap backfill, so a refill used up the tail's only backfill. Every later tail refilled, met the same gap, and returned. Ruling: at most one refill and one gap backfill per tail. `a_gap_after_a_dry_page_refill_is_backfilled_in_the_same_tail` builds `LEARNING_PAGE + 1` entries that failed to deliver, then delivers one page, then makes the store lose the project. It failed first with 0 residuals where 68 were expected.
- **The artifact names the stage revision.** `ArtifactFile` has `stage_revision`, and the revision table has a `stage` row. So `Artifact::parse` refuses an artifact that was calibrated under another `rules` picker. `every_content_rule_refuses_its_artifact` failed first: its new `stage` row parsed as `Ok`. The golden epoch digest does not change, because `epoch_of` already hashed `stage=`. Only the fixture JSON changed. Draft section 14.5 has a dated addendum.
- **`LearningFold` keeps no map.** `MetricsFold::apply` passes `self.pending.contains_key(response_id)` as `failover`. A response that is already pending is a failover's later `Routed`. The per-session map and its imports are deleted. Mutation: when the bool is ignored (`false`), `a_learned_route_fails_over_to_a_passing_plan_and_counts_one_decision` fails with 2 decisions where 1 was expected.
- **One derivation of the learned input.** `LearnedPolicy::turn_input(ctx, tool_turn, window, available)` returns the `rules` pick and the `LearnedInput`. `choose` plans and records under it, and the engine keys its read with it. The two tests that compare visited keys with recorded input still pass.
- **The `off` block keeps its apply timeout.** `LearnerTerms.apply_timeout_ms` is removed. `Admission.learner_apply_timeout_ms` holds the value the block wrote, in any mode. It is required in `shadow` and `live`, it is optional in `off`, and it is `None` when there is no block. An `off` block still resolves `learner` to `None`, so `learning_for` still returns before any read or draw. Delivery waits `learner_apply_timeout_ms`, or `UNCONFIGURED_APPLY_TIMEOUT_MS` when it is `None`. This supersedes the `LearnerTerms` point above. `an_off_block_keeps_its_written_apply_timeout` failed first with `None` where `Some(1500)` was expected. Orchestrator ruling, 2026-09-29: accepted, because the timeout governs delivery of entries that are already pending, and that delivery does not depend on the mode.
- Smaller points. `latency_limit_ms: 0` is refused as `ZeroLatencyLimit`, in every mode, with its own message. `duplicates` uses `saturating_sub`. The learner-or-policy `match` moved from `engine.rs` into `Engine::choose_route` in `engine/learning.rs`. The `artifact` field doc and the README say that a relative path resolves against the process working directory, and that an absolute path is recommended. `ArtifactStrategies` prints labels, for example `[rules, capable]`.

These mutations survived before and now fail their tests:

- **The gap-backfill guard is removed** (a second gap backfill in one tail). `a_second_gap_in_one_tail_is_not_backfilled_again` fails with 3 applies where 2 were expected.
- **`.contains` is changed to `.remove` in the stop check** (a stopped session resumes). The diverged test and `a_malformed_stop_holds_across_later_turns` now run a third turn. Both fail with 2 applies where 1 was expected.
- **`UNCONFIGURED_APPLY_TIMEOUT_MS` is changed from 250 to 5.** `a_session_without_a_learner_block_delivers_under_the_unconfigured_timeout` fails: an apply that answers in 40 ms got 0 acknowledgements. With the value changed to 60000, the same test fails in its 1.5 s case. When delivery ignores `learner_apply_timeout_ms`, `an_off_block_delivers_under_its_written_apply_timeout` fails with 0 acknowledgements.
- **`last_seq` is taken before the `LearningApplied` append.** `a_learner_turns_last_seq_includes_its_acknowledgement` fails with 7 where 8 was expected.

**M8 round-2 fixes, 2026-09-29.** A second review and mutation pass found one more ordering defect, two warning-volume points, one untested arithmetic edge, one untested pure function, one allocation, and two documentation gaps. All are fixed test-first on the same branch.

- **The mark clear ran before the acknowledgement append.** A successful clear followed by a failed append, or a crash between the two, dropped the mark while the log still showed nothing applied — the recovery task would then never revisit a session it had no record of ever owing. Ruling: acknowledge, then clear, and skip the clear entirely when the append fails, so every failure branch leaves the mark. `a_refused_ack_append_leaves_the_pending_mark` failed first: with `refuse_acks(true)`, `pending_learning` no longer listed the session because the clear ran and succeeded regardless of the append's outcome.
  - **2026-09-29 correction (M8 round-3):** the bullet above is right that the append must come first, but wrong about why. Clearing the mark before the append could not have lost a needed recovery: once the store answers `Applied` it durably holds the entries, the mark only tracks whether *this session's log* has recorded the delivery, and the recovery task (M9) reads that mark and never appends to the log itself (see M9's "it never appends"). So a mark cleared early just means recovery skips a session the store already has the entries for -- no data is lost. The append-first order still stands, as a choice rather than a fix: keeping the mark on every failure branch costs at most one redundant recovery apply (the store answers the resent entries as duplicates by watermark) plus a redundant clear, which is cheaper than the other order's failure mode -- a session that never turns again carrying a permanently unrecorded delivery in its own log. Round-2 finding 1 was invalid as stated.
- **Store read and apply failures warned on every turn of an outage.** Ruling: the same once-per-outage pattern `fair_use_unreachable_warned` already uses, with two `AtomicBool`s on `RoutingLearner` (`read_unreachable_warned`, `apply_unreachable_warned`), each reset on the next success and warning only on the transition into failure; a `debug` line still fires every time. `store_read_failures_during_an_outage_warn_once` and `apply_failures_during_an_outage_warn_once` each failed first with two warn lines where one was expected.
- **`duplicates` had no test of a store that over-reports.** `an_over_reported_apply_count_does_not_underflow_duplicates` scripts an `Applied { applied: 5, .. }` against a one-entry batch; it failed first with a subtract-with-overflow panic when `saturating_sub` was reverted to `-`.
- **`page_epochs` had no test.** `repeated_epochs_collapse_to_one_entry` and a distinct-epochs control were added in `engine/learning.rs`; the first fails when the `if !epochs.contains` de-duplication is removed.
- **`RoutingLearner::is_stopped` cloned a `(ProjectId, SessionId)` tuple on every tail**, stopped or not, to build a lookup key. `stopped_sessions` is now `HashMap<ProjectId, HashSet<SessionId>>`, so the read every tail makes looks up by borrowed keys, and only `stop` — which runs once per stopped session, not once per tail — pays for owned ones.
- **Two documentation gaps.** The README and `metrics/snapshot/learning.rs` now say that a `refuse` project's refused turn writes no `Routed`, so it counts under none of `decisions`, `unmet`, or `read_failures`, and terminates as `PolicyRefused` instead — not a `read_failures` reason of its own. The README's "one apply" is now "one apply per page, and one more after a gap backfill (a dry-page refill replays the log first but adds no apply of its own)," matching the at-most-one-refill-and-one-gap-backfill rule above.
- **The learner delivery timeout tests could flake under load.** `an_apply_timeout_then_retry_applies_each_entry_once`, `a_session_without_a_learner_block_delivers_under_the_unconfigured_timeout`, and `an_off_block_delivers_under_its_written_apply_timeout` now run under `#[tokio::test(start_paused = true)]`, so the `sleep` and `timeout` calls they race are ordered by the durations compared on one virtual clock rather than by real wall time. Each still fails under the mutations recorded above (`UNCONFIGURED_APPLY_TIMEOUT_MS` moved to 5 or to 60000, `learner_apply_timeout_ms` ignored). `roundhouse-server`'s dev-dependencies add `tokio`'s `test-util` feature, which `full` does not carry.
- **2026-09-29: `captured_warnings` lost lines under `cargo test`.** `a_read_success_between_two_outages_resets_the_warn_flag` failed 12 of 40 runs (20 of 80 with another suite running beside it), with an empty capture. The cause is in tracing-core 0.1.36, not in the warn-once flags. The old helper put a dispatcher in scope for each capture, and during a capture it was the only live dispatcher. In that state, the first registration of a callsite asks only the registering thread's default (`callsite.rs:544-566`). So a test with no capture that reached the warn callsite first, mid-capture, cached `never` for the whole process, after the capture's own rebuild. The global max level was not involved. Ruling: the claim is partially valid (the defect is real and it is the callsite interest; the max level cannot drop, because only the serialized helper rebuilt it). Fix: the helper now installs one global subscriber once, whose filter asks per event whether the calling thread is capturing, so a callsite's interest no longer depends on which thread registers it. `a_callsite_another_thread_registers_mid_capture_is_still_captured` in `test_support.rs` forces the order and failed 8 of 8 runs on the old helper. After the fix: 0 of 40 and 0 of 80. The "occasionally steal a warn" notes in `delivery.rs` are removed. The order-dependent warn-once test that M13.1 recorded as not reproducible in `PLAN-anthropic-messages.md` is probably the same race.

**M9 status, 2026-09-29.** M9 is implemented test-first on `ai/learner-m9-startup`, from `b32b51e`. The composition is `crates/roundhouse-server/src/routing_composition.rs`: `compose(plane, backends)` returns the policy (`affinity`, `stage` or `learned`) and, when a project is `shadow` or `live`, the learner store and the recovery cadence; `attach_learner` attaches the store and builds the recovery task over the engine's own learner. `main.rs` only wires the result, and `tests/learner_startup.rs` calls the same two functions. The recovery task is `LearnerRecovery` in `crates/roundhouse-server/src/learner_recovery.rs`. The delivery the engine tail and the task share is `crates/roundhouse-server/src/engine/learning/delivery.rs`. The `learner_recovery` block is `control_config/learner_recovery.rs`. The tests are `tests/learned_routing_engine/recovery.rs`, `tests/learner_startup.rs`, `tests/learner_binary_boot.rs`, `tests/learner_recovery_redis.rs`, `control_config/learner_recovery/tests.rs` and `control_config/directory/learner_tests.rs`.

What changes for a deployment:

- **No project in `shadow` or `live`: nothing.** The policy name is `affinity` or `stage` as before, no learner store is opened, no recovery task runs, and no learner line is logged. A directory document with no learner is byte-identical, because the new fingerprint axis is skipped when empty. A file that writes a `learner_recovery` block and no learner composes nothing either.
- **A `shadow` project:** every decision in the process records `policy: "learned"`. The learner store is Redis under the deployment's namespace when `ROUNDHOUSE_REDIS_URL` is set, else memory with a warning that learner state ends with the process. The recovery task starts and logs its cadence. The file must write `learner_recovery`; without it the boot is refused, and so is an admin write that enables a learner.
- **An admin-added learner** on a process that booted with none routes as before and warns once per project until a restart.

Two points from the M8 notes are closed. The artifact bytes enter `CompiledUnder` as `artifacts`, `{project}={sha256}` sorted, where the SHA-256 is the digest the epoch hashes (`Artifact::sha256`; the bytes are hashed once per parse). The binary composes the learner store.

The milestone did not settle these points, so the implementation made these decisions:

- **The learner store opens lazily.** The plan says `shared_backend::open` adds it. It is `Backends::open_learner_store`, in the same arm match, and `compose` calls it only when a project enables the learner, so a deployment with no learner gets no new connection and no new log line. The Redis arm now carries its namespace, so the store cannot open under another one.
- **The artifact axis is per version, not per handle.** A path can arrive with an admin write, so `DirectoryStore::commit` takes the axis of the plane the writer compiled, and a reader compares the axis of its own compile of the same records. When the reader cannot compile the version, the axis is not compared: the refused version already reports that.
- **`learner_recovery` fields.** The draft's five fields, plus `read_timeout_ms` (watermark read), `apply_timeout_ms` and `source_timeout_ms` (each session-store call). No field may be 0, `idle_after_ms` included, because a zero idle window makes the task contend with every live tail. The block is checked in `validate`, so the refusal is the same at boot and on an admin write.
- **`LearnedPolicy` is a `RoutingPolicy` value** wrapping the stage router: name `learned`, `reads_tier_recipes` true, and `choose` is the stage decision. The associated functions `turn_input` and `choose` are unchanged.
- **Outage and backoff.** An outage is only what says a store itself is down. On the learner store: a watermark read that answers `Unavailable` or times out, and an apply that answers `Unavailable`. On the session store: an index page or a requeue that fails or times out, and a replay, a gap backfill or a mark clear that fails with a backend error. Nothing that belongs to one session is an outage: not an apply, replay, backfill or clear that times out (the next session's watermark read probes the store), and not a `CorruptLog` from any call. An outage ends the sweep with the pass cursor unmoved, the next wait doubles up to 8 intervals (`MAX_BACKOFF_FACTOR`), and one warning covers the outage. A clean sweep resets both. (Rewritten by the M9 round-2 fixes below; the first version counted an apply timeout and every replay failure as the session's, and every session-store error as an outage.)
- **One session's trouble holds that session and the sweep goes on** (changed by the M9 review fixes below: a replay that fails with a backend error is now an outage). A `WrongType` from the watermark read or the apply, a replay of the session's log that fails or times out, a log that no longer exists, and a stopped session are each logged with the project and skipped. Treating any of them as an outage left the cursor on the same session, so one session nobody could finish stopped delivery and the audit for every project: the starvation the index's byte-order cursor exists to prevent. `a_wrong_typed_session_does_not_stall_the_sessions_behind_it` failed first with `outage: true` and the session behind it undelivered. A `WrongType` watermark counts as `unavailable` in the delivery counters, as a `WrongType` apply already did.
- **The audit does not ask whether a session is pending.** The index has no such query. A requeue of a session that is still pending changes nothing, so the audit requeues every session whose watermark is below its mark, and `SweepReport::requeued` counts both kinds.
- **The core clears after every page.** With several pages owed in one sweep, the earlier clears answer `Newer`. One code path is worth the extra call.
- **The admin-added warning is per project**, not once per process, because the remedy names the project.
- `SessionState` is boxed in `Source::Replayed`, and the plane's learner facts are one boxed `PlaneLearner`, for clippy's variant-size lint.

**Finding: the audit hides a premature clear within one sweep.** A clear that dropped a mark before its last page is exactly the loss the audit repairs, and the audit runs in the same sweep. The clear-predicate test therefore keeps the audit away from its session (an audit page of one, taken by an earlier session id). Without that, a mutation that clears with `u64::MAX` survived.

Red lines. The first run used a skeleton: `sweep` did nothing, `backoff` returned the interval, `compose` returned today's composition, the recovery block was not checked, the unread-learner warning returned at once, and the artifact axis was neither stamped nor compared. All 13 recovery tests failed on their own claim, for example the crash case with 0 residuals where 1 was expected, the audit with 0 requeued where 1 was expected, the page case with 0 where 64 was expected, the outage case with no outage reported, and the backoff with 1 s where 2 s was expected. Four startup tests failed: the M8-era boot composed no learner, the policy was `stage` where `learned` was expected, the memory warning was missing, and the admin-added case logged 0 lines where 2 were expected. Three configuration tests loaded where a refusal was expected (missing block, each zero field, the admin write). Both artifact tests failed: `differs_from` gave `[]`, and the directory reported no divergence. Controls that passed there: `an_invalid_artifact_stops_the_boot` (the M8 loader), `no_learner_project_leaves_the_policy_name_unchanged`, `an_unknown_learner_recovery_field_is_refused` and `a_resolved_plane_carries_the_cadence_the_file_wrote`. `one_sweep_delivers_as_many_pages_as_its_budget_allows`, the Redis-gated tests and the binary boot tests were written after the implementation.

These mutations each failed their tests: `WrongType` from the watermark read or from the apply treated as an outage (the wrong-type test); the core clearing with `u64::MAX` (the clear-predicate test); the audit never requeuing (the audit test); the task skipping sessions `is_leased` calls leased (13 recovery tests); a warning on every outage sweep, and a backoff never reset (the outage test); one page per sweep regardless of the budget (the budget test); a replay from 0 instead of the watermark (the lost-ack and budget tests); an empty replay that does not clear (the lost-ack test); the store's clear ignoring its predicate (the delayed-clear and clear-predicate tests); the learner composed for any recipe, and the store opened eagerly (the no-learner test); the unread-learner warning once per process (the admin-added test); a reader comparing the handle's empty axis, and a commit stamping none (the artifact test); `deny_unknown_fields` removed and the read and apply timeouts swapped (the configuration tests); `main.rs` not spawning the task (the binary boot test). One mutation survived (killed by the M9 review fixes below): `main.rs` passing a plain `AffinityPolicy` instead of the composed policy. Only a turn's record shows the policy, and the binary boot test does not drive a turn; the library test holds `compose` itself.

Points left for M10 (the M9 review fixes below close the fourth, close the fifth for the learner but not for `composes_the_stage_router`, make the sixth warn once rather than every sweep, and move the first to M11):

- A deployment that boots with every project `off` composes no recovery task, so marks left by earlier `shadow` sessions stay pending until a learner is enabled again. Delivery in the tail still runs for those sessions only if they turn again on a process with a learner attached.
- The audit costs one watermark read per marked session per cycle, and a redundant requeue write for each session that is still pending.
- The divergence check compares nothing at directory version 0, so a file-only deployment with no admin write never compares artifacts, as for every other axis.
- The binary boot test does not drive a turn; the composed policy's wiring in `main.rs` is held only by the library test.
- `composes_the_learner` asks the key table, and `validate` asks the projects. A file-declared `shadow` project with no turn key at boot (its keys minted later through the admin plane) passes the `learner_recovery` requirement but composes no learner, and its turns get the "added through the admin plane" warning, which names the wrong cause. `composes_the_stage_router` has the same property.
- A replay that times out on every sweep (a log too long for `source_timeout_ms`) is held forever, with a warning each sweep. Raising the timeout is the remedy today.

**M9 review fixes, 2026-09-29.** Each ruling was checked test-first. Its test failed before the fix, or fails under the mutation named with it.

- **M1, a garbage watermark field (material).** `RedisLearnerStore::watermark` answered `Unavailable` for a field that is not a count, so one such field ended every sweep at its session for every project. It now answers `WrongType { key }`, which holds that one session. `read` keeps `Unavailable` because its keys are project-wide. `apply` keeps `CounterRange`. The `WrongType` variant doc and the `watermark` trait doc now cover a garbage field. The M7 contract test `a_foreign_watermark_is_refused_by_apply_and_by_watermark` pinned `Unavailable` and now pins `WrongType`. Test: `a_garbage_watermark_field_holds_its_session_and_the_sweep_goes_on_on_redis`.
- **L1.** A session-store `Backend` failure in a replay or a gap backfill is an outage (`Hold::SourceDown` for the backfill), and the cursor stays where it was. The catch-all arm of the recovery replay (an error that is neither `Backend` nor `SessionNotFound`) stays held and has no test, because no store produces one from `read_events`. `SessionNotFound` and a per-session timeout stay held.
- **L2.** An apply timeout is `Hold::ApplyTimedOut`. It holds that session and is not an outage.
- **L3.** `CompiledUnder::artifacts` is `Option<Vec<String>>`. `None` means "not recorded" and is never compared. A writer stamps `None` when its plane enables no learner, so documents with no learner stay byte-identical.
- **L4.** `Delivery` carries an optional `source_timeout`. The recovery task uses it to bound the mark clear and the whole gap backfill. The engine passes `None`. A clear timeout is `Hold::ClearTimedOut`, which holds the session and is not an outage.
- **L5.** The `Malformed`, `GapPersisted`, `SourceUnavailable`, log-gone and replay-timeout holds each have a test. Each test fails when its hold is counted as an outage.
- **L6.** `validate`'s `enabling` is carried on the plane (`ControlPlane::enables_the_learner`), and `composes_the_learner` reads it. A `shadow` project declared in the file with no turn key now composes the learner. This is the right answer because the same check already required the `learner_recovery` block for that project.
- **L7 and S2.** `serve` logs `policy=<engine.policy().name()>`. The boot tests assert `learned` for a `shadow` file and `stage` for a file with tiers and no learner.
- **S1 and S3.** One test has an outage on a later page and checks that the page it stopped on is the next one delivered. Another checks that a session the engine's tail stopped costs the recovery task no replay and no apply.
- **Nits.** A held session warns once until it is delivered. This covers a replay that fails, a replay that times out, a log that is gone, and a `WrongType` watermark. The gap backfill's warning joined the set with the corrupt-log ruling below. The learner artifact fixtures use `tempfile`. The `CompiledUnder` docs now match the code.
- **Recorded, not built.** An all-`off` boot starts no recovery task. This is deferred to M11 and noted there.
- **Ruled not defects.** Finding #5: the audit hiding a premature clear in tests is a property of the test setup. The clear-predicate test already keeps the audit away from its session. Finding #6: `policy: "learned"` recorded for an `off` project's turns is by design. The record names the object in force for the process, as `stage` does for a project with no recipe.
- **Corrupt log, ruled 2026-09-29.** A log the store read but whose content it never writes is `StoreError::CorruptLog { session_id, detail }`, not `Backend`. It is one session's fault, so no caller may treat it as an outage. The Redis session store returns it for an entry id that is not `<seq>-0`, an entry with no integer `at_ms`, an entry with no `kind` or one it cannot decode, a `read_events` batch that is not contiguous, and a `last_seq` whose newest id is not the log's length. The recovery task holds the session, warns once, and goes on; `Backend` stays an outage. Every other caller keeps its old behavior, because each one handles it in the same catch-all arm as `Backend`. The gap backfill's warning now uses the recovery task's held set, so it also warns once per session. Tests: `a_corrupt_log_holds_its_session_and_the_sweep_goes_on_on_redis`, the updated `read_path.rs` `a_corrupted_log_fails_loudly_rather_than_dropping_events`, and `a_backfill_that_fails_on_every_sweep_warns_once`.

**M9 round-2 fixes, 2026-09-29.** The rule behind each: a fault in one session's stored data holds that session and never stalls the sweep; only a failure of the store as a whole is an outage, and where the store can tell the two apart it classifies the error where it happens. Each test failed first, or fails under the mutation named with it.

- **Item 1, a wrong-typed log key (material).** `RedisSessionStore::read_events` and `last_seq` answered `Backend` for `WRONGTYPE`, so a log key a foreign writer replaced with a string was an outage for every project. A call that touches only one session's keys now maps `WRONGTYPE` to `CorruptLog` (`one_session` in `lib.rs`). `RedisError::code` does not see a pipelined `WRONGTYPE` (redis 1.2.4 has no kind for it), so the check reads the server errors. The audit of one session's read paths: `read_events`, `last_seq`, and the `acquire`, `renew` and `release` lease scripts (meta and lease keys only) now classify; `create_session` (`SET NX`) and `is_leased` (`EXISTS`) cannot answer `WRONGTYPE`; the marked append also touches the namespace-wide index keys, so its `WRONGTYPE` cannot be attributed and stays `Backend`; the index scripts use namespace keys and stay `Backend`. `StoreError::CorruptLog`'s doc and message now say "stored data", since a key of another type and an unreadable mark are covered. Tests: `read_path.rs` `a_wrong_typed_session_key_is_corrupt_not_a_backend_failure`, `a_wrong_typed_log_key_holds_its_session_and_the_sweep_goes_on_on_redis`.
- **Item 2, an unreadable stored mark (material).** One `BADMARK` failed the index page as `Backend`, so the pending pass and the audit were an outage forever. The page now skips the member, names it in `LearningPage::unreadable`, and moves its cursor past it; the recovery task warns once for it (in the audit's held set, which both passes use for it). A clear or a requeue of an unreadable mark answers `CorruptLog`: the clear holds the session (`Hold::MarkUnreadable`, not an outage), and the audit skips it. `MemoryStore` matches through a test lever, and the contract case `an_unreadable_mark_is_named_and_the_page_goes_on` runs against both stores (`LearningMarkControl`). A pending member with no stored mark (`ORPHAN`) still fails the page; it was not ruled here. Tests: the contract case, `an_unreadable_mark_holds_its_session_and_the_sweep_goes_on_on_redis`, `an_unreadable_mark_is_skipped_warned_once_and_the_sweep_goes_on`, `a_clear_that_finds_the_mark_unreadable_holds_its_session`, `an_audit_requeue_that_finds_the_mark_unreadable_skips_that_session`.
- **Item 3, the held set.** It is keyed by the session and the mark the page named, so a new mark warns again even when the engine's tail delivered the old one. Each entry records the pass that last held it, and the end of each full pass keeps only what that pass held again, so the set is bounded by the pending set. A session therefore warns again after it is delivered, after it is marked anew, or after a full pass that did not hold it again through the set (a pass where its apply timed out, for example). The audit has its own set, pruned at the end of each audit pass: pruned with the pending pass, a foreign watermark on a delivered session would warn on every pass. Tests: `a_hold_after_the_tail_delivered_the_session_warns_again`, `the_held_set_is_pruned_to_the_sessions_still_pending`.
- **Item 4.** A clear that times out or fails, and an apply that meets a foreign key, warn through the held set in the recovery task. The engine's tail still warns every time for an acknowledgement failure, and keeps the store's once-per-outage flag for `Unavailable` and a foreign key. Tests: `a_mark_clear_that_hangs_on_every_sweep_warns_once`, `an_apply_that_meets_a_foreign_key_on_every_sweep_warns_once`. The module doc, the README and the outage bullet above say which holds warn once.
- **Item 5.** `an_apply_refused_unavailable_during_a_sweep_is_an_outage_that_keeps_the_cursor` and `a_mark_clear_that_fails_during_a_sweep_is_an_outage_that_keeps_the_cursor` each fail when their variant is taken out of `Hold::is_outage`.
- **S4.** `routing_composition::build_engine` builds the engine `serve` routes with, from the composition, and replaces `attach_learner`. `serve` calls it and nothing else builds an engine; `tests/learner_startup.rs` builds its engines with it. `the_built_engine_routes_under_the_composed_policy` asserts `engine.policy().name()` for a `shadow`, a `tiers` and a plain plane, and fails, with three other startup tests, when `build_engine` wires a plain `AffinityPolicy`.
- **S5.** `a_write_with_no_learner_stamps_no_artifact_axis` applies a plain project through `ControlDirectory::apply` and asserts the stored `artifacts` is `None`. It fails when `stamped_artifacts` loses its emptiness guard; the byte-identity tests stay green under that mutation.
- **Nits.** `first_hold` is the insert. `a_replay_that_times_out_on_every_sweep_warns_once` checks the real report. `Hold::ForeignKey`'s doc says the keys are the project's and why each session is still held on its own.

**M9 round-3 fixes, 2026-09-29.** A third review pass found one material gap the round-2 item 2 ruling explicitly left open, and three low findings. The governing rule is unchanged: a fault in one session's stored data holds that session and never stalls the sweep; only a failure of the store as a whole is an outage. Each new test failed first.

- **Item 1, `ORPHAN` (material, the round-2 gap).** Round-2's item 2 ruled the unparseable-mark case and said "a pending member with no stored mark (`ORPHAN`) still fails the page; it was not ruled here." It is now: `PAGE_BODY` names a member with no stored mark in `unreadable`, exactly like an unparseable one, instead of returning `{'ORPHAN', member}` and failing the whole page; `decode_page` drops the `ORPHAN` arm. `MemoryStore`'s `pending_page` matches: the hard `Err` it used to return for a pending member missing from `marks` is gone, and `PageBuilder::examine` treats `Ok(None)` the same as `Err(_)`. A clear or a requeue of an orphan answers `Unmarked` (the marks-hash `HGET` finds nothing to disagree with), not `CorruptLog`, and removes nothing from `pending` — an orphan cannot leave `pending` on its own, and is named on every later pass until an operator repairs the index. That is acceptable: the condition is already an operator-visible anomaly no normal write path produces (only a foreign `HDEL`/`DEL` of the marks field, or an `allkeys-lru` eviction of the whole hash), it never stalls the sweep or the sessions behind it, and the recovery task's existing held-set mechanism (round-2 item 2) warns about it once per pass the same as an unparseable mark. Tests: `crates/roundhouse-store-redis/tests/learning_index.rs::a_pending_member_without_a_mark_is_named_and_the_page_goes_on` (inverted from the round-2-era version that asserted `Backend`), the contract case `an_orphaned_pending_member_is_named_and_the_page_goes_on` (pending enumeration only — `MemoryStore`'s permanent enumeration is the marks map itself, so the test lever that empties one entry cannot stand in for Redis's independent marked/pending sets there), `crates/roundhouse-server/tests/learner_recovery_redis.rs::an_orphaned_pending_member_holds_its_session_and_the_sweep_goes_on_on_redis`, and `crates/roundhouse-server/tests/learned_routing_engine/recovery_warnings.rs::an_orphaned_pending_member_is_skipped_warned_once_and_the_sweep_goes_on`.
- **Item 2, low (L1), the lease-script `WRONGTYPE` claim is Redis-version scoped.** `is_wrong_type` reads `WRONGTYPE` from the server errors a plain command or a script's own raised error carries; only Redis 7 and later propagates a script's `WRONGTYPE` with its code intact; 6.x wraps it as `ERR Error running script ...`. `read_events` and `last_seq` (plain pipelined commands) classify on every version this crate supports; `acquire_lease`, `renew_lease` and `release_lease` (Lua scripts) classify on 7+ and fall back to `Backend` on 6.x. The floor stays Redis ≥ 6.2 (`lib.rs`'s module doc): harmless because no lease caller branches on `CorruptLog` versus `Backend`. Doc-only, scoped in `lib.rs`'s `one_session` doc and `read_path.rs`'s `a_wrong_typed_session_key_is_corrupt_not_a_backend_failure` doc.
- **Item 3, low (L2), correcting the round-2 item 1 rationale for `BADMARK` on the append.** Round-2 said "the marked append also touches the namespace-wide index keys, so its `WRONGTYPE` cannot be attributed and stays `Backend`" and left `BADMARK` grouped with it. That grouping was wrong: `APPEND_BODY` type-checks the three index keys with `is_type_or_absent` before anything else touches them, so a raw `WRONGTYPE` really is unattributable (any of six keys, several namespace-wide) and stays `Backend` — but `BADMARK` can only come from `HGET(KEYS[4], ARGV[3])`, keyed by this session's own id, so it is always this session's entry in the shared hash, never ambiguous. `BADMARK` on the append now maps to `StoreError::CorruptLog` (via the same `corrupt_log` construction `one_session` uses), consistent with `CLEAR_BODY` and `REQUEUE_BODY`'s own `BADMARK` handling. Every append caller is variant-blind: `Session::commit` (`session.rs`) and the `Delegating` test wrapper (`store/doubles.rs`) both propagate through `?`, and no production or test call site branches on `Backend` versus `CorruptLog` from `append_events`. Test: `crates/roundhouse-store-redis/tests/learning_index.rs::an_unreadable_stored_mark_refuses_the_append_before_any_write` now asserts `CorruptLog`, not `Backend`.
- **Item 4, low (L3), test state gated out of production.** `MemoryIndex::unreadable` and its check in `mark_of` were reachable in every build, not only where the test lever that populates it exists. Both are now `#[cfg(any(test, feature = "test-support"))]`, along with `unreadable_mark`; the workspace still builds with neither `cfg(test)` nor `test-support` (`cargo build --workspace`).
- **Nits.** The ragged "which holds warn" paragraph in `learner_recovery.rs`'s module doc is rewrapped and now also names the no-mark-at-all case. `pending_learning` and `learning_sessions`' trait docs in `store.rs` each point at `LearningPage::unreadable`. `Delivery::held: Option<(&HeldSessions, u64)>` is now `Option<HeldMark<'a>>`, a named struct with `sessions` and `mark` fields, in `engine/learning/delivery.rs`.

**M9 round-4 fixes, 2026-09-29.** A fourth review pass found one material gap and one low finding in the index page's decoding, one low finding in the watermark read, one low finding in stream decoding, and four documentation nits. The governing rule is unchanged: a fault in one session's stored data holds that session and never stalls the sweep; only a failure of the store as a whole is an outage. Each new test failed first. The outage table below is reconstructed from the actual `Err(Outage)` sites in `learner_recovery.rs` and `Hold::is_outage` in `delivery.rs` — the round-4 review's own table was not carried into this plan, and one row it should be honest about is not yet "none".

- **Item 1, an undecodable mark field inside a page (material).** `decode_page` parsed every returned chunk's seq, marked-at and project with `?`, so a value `parse_mark`'s Lua regex accepts (any digit run for seq and marked-at, any byte run for the project) but Rust's `u64` or UTF-8 cannot decode failed the whole page as `Backend`. Fixed: a chunk whose seq, marked-at or project does not decode is pushed onto `unreadable` instead, the same as the `ORPHAN` and unparseable-mark cases round-2 and round-3 already cover. **Residual, not fixed here:** the member itself (`ZRANGE`'s own output) still fails the whole page if it is not UTF-8 -- see the table's last row; this store's own writes are always valid UTF-8 (`SessionId::as_str`), so only a foreign `ZADD` reaches it, the same adversary the `ORPHAN` case already tolerates, but there is no `SessionId` a non-UTF-8 member could be named with in `unreadable`, so naming it the way `ORPHAN` is named is not available without a design decision (a raw-bytes id, or a lossy rendering) that this fix set does not make. The secondary path -- `seq_at` on a `NEWER`/`MISMATCH` reply's seq -- used the same `unexpected` (`Backend`) fallback; `clear` and `requeue` now map that failure to `CorruptLog` through a new `corrupt_seq` helper, since `ARGV[1]` keys the `HGET` (in both `CLEAR_BODY` and `REQUEUE_BODY`) by the calling session's own id. Tests: `crates/roundhouse-store-redis/tests/learning_index.rs::a_seq_that_overflows_u64_is_named_and_the_page_goes_on`, `::a_marked_at_that_overflows_u64_is_named_and_the_page_goes_on`, `::a_non_utf8_project_is_named_and_the_page_goes_on`, `::a_clear_whose_newer_reply_carries_an_overflowed_seq_is_corrupt_not_backend`, `::a_requeue_whose_mismatch_reply_carries_an_overflowed_seq_is_corrupt_not_backend`, and `crates/roundhouse-server/tests/learner_recovery_redis.rs::an_overflowed_seq_holds_its_session_and_the_sweep_goes_on_on_redis`.
- **Item 2, low (L1), a non-UTF-8 watermark field.** `RedisLearnerStore::watermark` decoded the `HGET` reply straight into a `String`; invalid UTF-8 bytes failed that decode with a code-less client error, which fell into the `_ => unavailable(error)` arm as `Unavailable` — an outage. Fixed: the field is read as `Vec<u8>`, and a UTF-8 failure now joins the digit-parse failure as `WrongType { key }`. Tests: `crates/roundhouse-store-redis/tests/learn_contract.rs::a_non_utf8_watermark_field_is_refused_as_wrong_type`, `crates/roundhouse-server/tests/learner_recovery_redis.rs::a_non_utf8_watermark_field_holds_its_session_and_the_sweep_goes_on_on_redis`.
- **Item 3, low (L2), non-UTF-8 stream field names -- shipped mechanism deviates from the ruling.** The ruling said to map a redis-rs response or parse error to `CorruptLog` "using the redis-rs error kind to tell them apart, and say which kinds you map." That was not done: `redis-rs-1.2.4/src/parser.rs:340` and `:373` show the low-level RESP parser itself raising `ParsingError` (`ErrorKind::Parse`) on a wire-decode failure, the same kind a client-side `FromRedisValue` conversion raises on an already-received reply (`types.rs`'s `String` decode, e.g. a non-UTF-8 stream field name) -- the kind alone cannot tell a genuine connection/protocol fault from one session's foreign bytes, so classifying `one_session` by `RedisError::kind() == ErrorKind::Parse` would have misclassified a real protocol desync as `CorruptLog` and let a whole-store fault through as one session's. **Kinds mapped in `one_session`: none** -- it is unchanged from round-3 (`WRONGTYPE` only) and still covers `acquire_lease`, `renew_lease` and `release_lease`. Shipped instead: `read_events` and `last_seq` decode their pipeline's reply as a generic `Value` through that unchanged `one_session` mapper (so a real protocol failure still surfaces as `Backend`, exactly as before), then convert the already-received `Value` into `StreamRangeReply` themselves and map only *that* local conversion's error to `CorruptLog` (`undecodable_log`) -- a conversion that runs entirely on bytes already off the wire, so it cannot itself be a connection fault. **This plan records the ruled mechanism (classify by kind in `one_session`) as rejected for the reason above; the orchestrator can overrule this and ask for the kind-based version instead.** Test: `crates/roundhouse-store-redis/tests/read_path.rs::a_non_utf8_stream_field_name_is_corrupt_not_a_backend_failure`.
- **Nits.** `MemoryIndex`'s `marks` ("never shrinks") and `pending` ("every member has an entry in `marks`") doc comments now say those invariants hold outside the test lever (`orphan`, gated to `cfg(test, feature = "test-support")`), which deliberately breaks both to stand in for a foreign `HDEL`. The orphan-warning cadence is now one wording in both `learner_recovery.rs`'s module doc and the `contract/learning.rs` doc: once per pass while it stays unreadable, or once until it is readable again if the audit meets it every pass. **Both orphan docs' healing claim is scoped to what the append script actually does, not to "an unparseable mark" generally:** a member with no stored mark at all heals unconditionally on the next marked append (`APPEND_BODY` finds nothing to check), and a mark whose seq or marked-at only Rust cannot parse heals the same way (Lua's own `parse_mark` still accepts it, so the append overwrites it) -- but a mark that fails Lua's own parse (`BADMARK`) blocks the next marked append instead of healing it (`an_unreadable_stored_mark_refuses_the_append_before_any_write` pins this: the append refuses, and the corrupt bytes are left unchanged), and needs an operator to rewrite or remove the field directly. The first pass at this doc fix conflated the two and is corrected here before landing. `scripts.rs`'s `BADMARK` arm no longer `expect`s `batch.mark` to be `Some`; a `None` (unreachable in practice, since the script only returns `BADMARK` from the marked branch) now answers `unexpected(&reply)` instead of panicking.

**M9 round-4 outage table**, rebuilt from every `Err(Outage)` site in `learner_recovery.rs` (`deliver_page`, `audit_page`, `watermark`, `replay`, `index`, `source`) and every `Hold::is_outage` variant in `delivery.rs` (`LearnerUnavailable`, `SourceDown`, `AcknowledgementFailed`), not from a supplied review table -- none was carried into this plan, so this is a reconstruction, not a restatement.

| Outage (sweep ends, cursor unmoved) | Per-session cause after these fixes |
|---|---|
| `index()` (a pending or marked page): any error the page call returns is treated as index-wide, by design (`decode_page` no longer returns a per-session error for a mark it cannot parse) | none for a mark's seq, marked-at or project (item 1 primary) |
| `index()` (same page call), a member (session id) that is not UTF-8 | **not none -- unfixed.** Three sites in `decode_page` decode a member as a `SessionId` and fail the whole page if it is not UTF-8: the cursor (`last`), the Lua-named `unreadable` prefix, and `str_at(entry, 0)` for a found chunk. Only a foreign `ZADD` can produce one (this store's own writes are always valid UTF-8, via `SessionId::as_str`), the same adversary class `ORPHAN` already tolerates, but none of the three can name it in `unreadable` or resume a cursor from it, because `SessionId` cannot carry non-UTF-8 bytes. Naming it needs a design decision (a raw-bytes id, or a lossy rendering) this fix set does not make |
| `audit_session`'s requeue, via `source()`: any error other than `CorruptLog` | none for a `MISMATCH` reply's seq (item 1 secondary) |
| `deliver_session`'s clear, via `Hold::AcknowledgementFailed` (`Hold::is_outage`): any clear error other than `CorruptLog` (`Hold::MarkUnreadable`) | none for a `NEWER` reply's seq (item 1 secondary) |
| `watermark()`: any error other than `LearnerError::WrongType` | none for a non-UTF-8 watermark field (item 2) |
| `replay()`: `StoreError::Backend` from the session store (`Backend` still ends the sweep; `CorruptLog` only holds the session) | none for a non-UTF-8 stream field name reached through `read_events` (item 3) |
| `Hold::SourceDown` (`delivery.rs`'s gap `backfill`, a `StoreError::Backend`): reachable from the recovery task's own delivery too (`ChainGap` handling is not gated to a live source) | none for a non-UTF-8 stream field name, same as `replay()` -- both `backfill` and `replay` reach it through `SessionState::project_learning` → `read_events` (item 3) |
| `Hold::LearnerUnavailable` (an apply `LearnerError::Unavailable`) | none found; checked rather than assumed. Every `KEYS`-indexed access in the apply and read Lua bodies (`learn/scripts.rs`) is preceded by its own `type_is` check, and every stored counter is parsed by `counter()`, which returns `nil` rather than raising on a non-digit field -- every foreign-data condition the scripts can meet returns a structured, decodable reply tag (`WRONG_TYPE`/`RANGE`/…), never a raised mid-script error. `Unavailable` here is left only for an actual connection failure invoking the script, or a reply shape no build of this store produces |

Every other outage source (a genuine `Unavailable`, a real timeout, an actual backend failure) is unchanged by round-4 and stays an outage on purpose -- those are the store itself being down, not one session's stored bytes.

**Orchestrator rulings on round 4, 2026-09-29.**

- **Item 3's mechanism is accepted.** The error kind cannot separate a RESP protocol failure from a client-side decode failure, because both raise `ErrorKind::Parse`. Decoding to `Value` first and mapping only the local conversion to `CorruptLog` keeps a real protocol failure an outage.
- **A pending or marked member that is not UTF-8 is an outage, by rule.** The governing rule holds a session when the fault can be attributed to a valid session. A member that is not a valid `SessionId` names no session. It is corruption of the shared index, in the same class as a wrong-typed index key, which is already an outage. Only a foreign `ZADD` of raw bytes into this namespace's keys produces it. The table row above that reads "not none" is therefore the accepted boundary of the rule, not an open defect.
- **Round 5, 2026-09-29.** The table covers every outage the recovery task can reach. `Hold::AcknowledgementFailed` from `record_learning_applied` is reachable only from the engine tail (`Source::Live`), so it is not a row. The merged `unreadable` list is now sorted, as its doc says, with a test that failed first (`[a, c, b]`).

**M10 status, 2026-09-29.** M10 is implemented test-first on `ai/learner-m10-calibrator`, from `fe92f63`. The calibrator is `crates/roundhouse-core/src/routing/learn/offline.rs` with `offline/{source,extract,estimate,report,write,drift,dump}.rs`. The binary is `crates/roundhouse-server/src/bin/learner-calibrate/main.rs`, and it only parses its two arguments. The adapter that opens the stores and writes the files is `crates/roundhouse-server/src/learner_calibrate.rs`. The tests are `crates/roundhouse-core/tests/learner_offline.rs` and `learner_offline_stores.rs` (with `tests/offline_support/`), and `crates/roundhouse-server/tests/learner_calibrate.rs` and `learner_calibrate_redis.rs` (Redis-gated). The README section "Calibration and the promotion report" documents the manifest, the four output files, and the report.

What exists:

- `calibrate(config, store, copy, source_commit)` enumerates the project's sessions through `learning_sessions`, reads each log to the cutoff, replays it, runs the drift check when a copy is given, writes the artifact, and builds the `Report`. It reads only. `the_calibrator_is_read_only_against_both_stores` counts every write, lease and pending call on a `Delegating` double, and every apply on the copy. The count is zero, and both marks are still pending after the run.
- Two additive seams in core. `SessionState::replay_learning(events)` folds a whole log through the same `apply` and returns every accepted review and the whole entry chain. The live fold keeps only 16 reviews and a page of 64 entries. `session::screen` is credit's screen, extracted from `credit()` without a change in behavior. Credit and the calibrator call it, so an interval is eligible exactly when credit credits it. `the_screen_exclusions_equal_the_session_folds_causes` compares the two counts. `exploit_order` is `pub(super)` so that the offline module re-derives an exploit choice with the policy's own function.
- The artifact is written for `Artifact::parse`, with `stage_revision`, the strategy list, the prior in `BTreeMap` order, the manifest digest and the source commit. It holds no clock time. The sidecar `artifact.json.meta.json` holds the time and the host.
- `InputManifest` is the cutoff (draft 14.1). It holds the sorted session ids, the last sequence read, and the SHA-256 of the event JSON lines. Its digest names the artifact and the report. `calibration.cutoff` pins a rerun. A pinned log whose digest changed is refused. A marked session that the pin does not name is counted as marked after the cutoff.
- `ClusterUnit::Session` has one variant today, labeled `sessions (sequence key)`. Every clustered number prints that label: the session counts, the bootstrap interval, the weighted clusters, and the line that compares the count with `quality.min_sessions`.

The milestone did not settle these points, so the implementation made these decisions:

- **The `learned` candidate is the logged learner, not a frozen state.** At each logged turn it is what `live` would have served without exploration: the recorded exploit strategy (`exploit_order` over the recorded plans), else `rules`. The report also shows each configured strategy as a fixed candidate. A re-gate of every turn over counters rebuilt from the manifest would evaluate a policy on the data that trained it, so M10 does not build one. The promotion summary is for `learned`.
- **The `rules` rate is factual.** It is unweighted, over the eligible intervals where the `rules` plan's first target was the served one on every turn. In `shadow` those are all the eligible intervals.
- **Cost and latency under the weights.** The candidate's cost is the self-normalized mean of per-interval measured cost. The `rules` cost is the factual mean. The candidate's p50 is the weighted lower median of first output from turn start over its weighted intervals' turns. A test whose inputs do not exist reports `not evaluable` with its reason, and it does not pass. This covers no support, an unpriced turn, and no latency sample.
- **Bootstrap.** The stream is SplitMix64, written out and pinned by a golden test, because a library generator can change its stream between versions. Each replicate draws as many clusters as have an eligible interval, which can be fewer than the manifest holds. The interval is `[floor(0.025 B), ceil(0.975 B) - 1]` of the sorted replicates. A replicate with no weight counts as 0.0 for the lower bound and 1.0 for the upper, and the report prints how many there were.
- **Replay equivalence checks what the record pins.** The checks are: a propensity in `(0, 1]`, and exactly 1 when the turn could not explore; the served dispatch on the served plan's first target; an exploit choice equal to the first entry of the recomputed exploit order; and an explored member equal to the recorded draw modulo the recorded set, naming the chosen strategy. A turn that fails excludes its interval as `record does not replay`. The exploration rate is not recorded, so a rate draw alone cannot be re-checked. The rate was not added to `ExplorationEvidence`, because that changes the M2 wire shape of every learned `Routed`.
- **The memory-store fixture is a dump.** Both session stores stamp `at_ms` with their own clock at append, so one fixture loaded into both would give two sets of latencies. `LogDump::capture` reads a store the way the calibrator does. `DumpStore` serves the dump through the `SessionStore` read methods and refuses every write. The Redis-gated test seeds Redis through real marked appends, captures it, and compares the two runs byte for byte. The enumeration and unreadable-member tests use the real `MemoryStore` index.
- **The artifact prior is summed review credit.** `prior: credit` sums the quality deltas of the manifest's entry chains, across epochs, for the listed strategies. The report shows credit for unlisted strategies as dropped. `prior: zero` writes none. The field is required. Jev counts never enter the prior (`jev_answers_never_enter_the_artifact_prior`).
- **The p50 is measured, not modeled.** It is `first output - TurnStarted` in the log, on the turns M5 samples: completed, with a first output after the served `Routed`, and a quote for the served target. On those turns it equals the M3 terms exactly: the overhead sample plus the quote plus the residual sample. So the brief's "residual-corrected TTFT plus the project overhead term" is the same number, measured per turn rather than modeled.
- **Pricing.** A dispatch is priced at its recorded rate card whatever its `billing`. A local dispatch has no rate card, and the catalog's capacity price (ruling 6) is not in the log, so the manifest takes no local price. A local turn is `unpriced`, never $0. Judge side calls record no rate card, so the report prints judge tokens and prints judge dollars as unpriced. Classifier spend is the measured `usd`. A call with unknown usage is shown at its submitted estimate. A result counts once, and only against an open intent for the same turn: the metrics evaluation block's join, so a repeated delivery is not counted twice (`classifier_spend_counts_each_call_once_against_its_intent`).
- **The drift check** reads the copy's watermark for each session, rebuilds the entries at or below it, and compares every counter that the manifest's recorded inputs read: quality pos, n and sessions for all three strategies, Jev counts, target operations, and the overhead. A copy watermark above the cutoff is listed. `SOURCE_COMMIT` is `ROUNDHOUSE_SOURCE_COMMIT` at build time, else the crate version, and never the clock.

**Finding: a `shadow` report cannot pass the ruled cost test.** A `shadow` project never explores, so every turn's propensity is 1 for the `rules` target and 0 for any other. The `learned` candidate then has weight 1 on the intervals where it agreed with `rules` on every turn, and 0 elsewhere. Its estimated cost is the `rules` cost on that subset, so a 10% saving can come only from which intervals agreed, not from routing. Where it never agrees, every test is `not evaluable` (`a_shadow_candidate_that_differs_from_rules_is_not_evaluable`, renamed in the M10 review fixes below). This is the conditional estimand working as specified. It does mean that M11 precondition 2 cannot be met honestly from `shadow` logs alone. It needs an owner decision before M11: for example, a bounded `live` run with exploration to buy support, or a separately labeled estimate from the recorded plan quotes. (Decided on 2026-09-29: corrected quotes and a staged promotion. See the section 2 addendum and the M10 review fixes below.)

Red lines. The tests were written against the implementation and then run against a skeleton, file by file. Nothing is committed, so the skeleton was the real files with these edits: the revision-2 weight (a product over matching turns only), the first turn's propensity as the trajectory probability, an effective sample size equal to N, per-interval resampling, one cause for every exclusion, no draw check, support only for the served target, propensity 1.0, the quote as cost and $0 for local, latency from `Routed`, the prior mode ignored, labels and unit names removed from the text, and every promotion test `not evaluable`. An earlier run with no `stage_revision` failed the round trip with `Artifact(Format("missing field `stage_revision`"))`. On the skeleton, 17 of the 24 pure tests written then failed on their own claim:

- the weight: `left: 1.0, right: 0.0`;
- the probability: `0.5` where `0.125` was expected;
- the explored propensity: `1.0` where `0.05` was expected;
- the reviewer fixture: `0.375` where `0.75` was expected;
- the census: `(0, 3)` where `(1, 3)` was expected;
- the exclusions: `Some(5)` where `Some(1)` was expected;
- the fold agreement: `1` where `3` was expected;
- the bootstrap: a replicate that split a session;
- cost: `Priced(0.01)` where `Priced(0.00183)` was expected;
- local: `Priced(0.0)` where `Unpriced` was expected;
- p50: `400` where `700` was expected;
- the effective sample size: `3.0` where `1.47` was expected;
- the zero prior: `1000` where `0` was expected;
- the promotion summary: `NotEvaluable("skeleton")` where `Pass` was expected;
- the unit label, the changed draw (2 intervals where 1 was expected), and the shadow candidate (weight 1 where 0 was expected).

With the enumeration and drift skeletons (`read_source` returning nothing, and `check` returning an empty result), 6 of 9 store tests failed on their own claim. Enumeration gave `[]` where the marked session was expected. The unreadable list was `[]`. Rollback gave two equal epochs. Drift compared 0 counters. The pinned refusal had no log to pin. The read-only test failed on its own guard that the copy was read.

Controls, each held by a named mutation that fails it:

- a clock in the artifact bytes: the byte-identity, SHA-256 and sidecar tests;
- Jev counts summed into the prior: the Jev test;
- a lease taken by `read_log`: the read-only test, with 2 writes where 0 were expected;
- the `drift check not run` line removed: its test;
- the rebuild ignoring the watermark: the watermark test;
- the quality test without its support guard, and `learned` always equal to `rules`: the shadow-candidate test;
- a changed SplitMix64 constant: the golden;
- `DumpStore::read_events` ignoring `after_seq`: the dump test;
- a fixed candidate acting on the served target: the action test;
- the screen without its failover check: the exclusions test;
- a clock in the report: the binary test;
- `deny_unknown_fields` removed from `CalibrationConfig`: the manifest test;
- a capture that drops each log's tail: both Redis-gated tests.
- no intent join for classifier spend: `classifier_spend_counts_each_call_once_against_its_intent`, 3 calls where 1 was expected. That test was written after the join, the 25th pure test;
- a Redis URL error message that drops the variable's name: `a_redis_source_names_its_url_variable_and_never_the_url`.

Points left for M11:

- ~~The finding above: an owner decision on how a promotion shows a cost saving.~~ Decided on 2026-09-29 (section 2 addendum). What is still open: confirmation of the corrected-quote latency gate, and the `rules` baseline for the binding live rerun.
- A local price. A recipe with local targets reports cost as unpriced until the log records the capacity price that each local dispatch was quoted at, or the owner rules that the manifest may name one.
- The frozen-state candidate (a re-gate over counters rebuilt at the cutoff, held out from the intervals it is evaluated on) is not built.
- `quality.min_sessions` counts the cluster unit. Renaming it `min_anchors` waits on the owner and on the anchor.
- Judge dollars need a rate card on the side-call record.

**M10 review fixes, 2026-09-29.** The PR 35 review, the mutation pass, and the owner rulings of 2026-09-29 (section 2 addendum) were fixed test-first on the same branch. New tests are in `crates/roundhouse-core/tests/learner_offline_promotion.rs`, except where noted.

- **M1, promotion by selection alone.** The learned candidate's estimates covered only the intervals where it agreed with `rules`, and the old summary compared them with `rules` over every interval. The refuter's fixture (10 `shadow` sessions: 5 agree at $0.01, all positive; 5 diverge at $0.10, 4 positive) printed `all three ruled tests pass: yes`. The gates moved to `offline/promotion.rs`: cost and latency from corrected quotes on every eligible interval, quality on the agreeing intervals, a full-set quality line that reads `not evaluable` without support on both sides, and a staged verdict line in place of "all three". `a_shadow_report_does_not_promote_by_selection_alone` failed first on that line. `an_uncorrectable_quote_makes_the_cost_test_not_evaluable` failed first because the cost line read `pass`.
- **L1, sparse support.** When more replicates held no weight than the 2.5% tail has places, the lower bound was the 0.0 filler, and the quality test read `fail`. `Bootstrap::sparse` now says so, and the test reads `not evaluable`. `sparse_support_reads_not_evaluable_never_fail` (3 weighted clusters of 50) failed first with `Fail`.
- **L2, estimated usage.** `Accounting::Estimated` leaves cached input at zero, so it overpriced. It is now unpriced. `estimated_usage_is_unpriced_never_priced_as_measured` failed first with `Priced(0.01)`.
- **L3, `min_sessions`.** The `met` line compared eligible sessions. It now prints both counts and decides on the sessions with an agreeing interval. `min_sessions_is_met_on_the_quality_tests_own_sessions` failed first with `10 against quality.min_sessions 6: met`.
- **L4, `propensity` compared targets with `==`.** Credit and the calibrator use `same_route`. `the_propensity_counts_members_by_route_not_by_worker` (in `learned_explore.rs`: two members on one local model, different workers) failed first with `0.025` where `0.05` was expected. Both comparisons in `propensity` now use `same_route`. This changes the propensity that a `live` turn records only when two plans name one local model on different workers. No version changed. `LEARNER_DRAW_VERSION` names the hash encoding, which did not change. `LEARNED_SELECTOR_REVISION` covers the exploration rule and is part of the epoch, but the fix changes no route, only the probability recorded for one, and no record written under revision 1 used the old rule: `shadow` never calls `propensity` (the arena needs `live`), and no deployed build composes a `live` learner, because M8 and M9 are still awaiting review (the status table above).
- **Mutation survivors.** S1 (an undefined replicate counted as the point estimate), in either bound: `an_undefined_replicate_widens_the_bound_asymmetrically`, which searches the seed for a fixture where the substitution moves both bounds. S2 (`>=` to `>` in the quality gate): `a_lower_bound_exactly_the_allowance_below_the_rate_passes`. S3 (`<=` to `<` in the cost gate): `a_cost_exactly_the_required_reduction_below_rules_passes`. Each failed under its mutation.
- **Nits.** `read_source_reads_sessions_in_byte_order_whatever_the_index_order` (in `learner_offline_stores.rs`) holds the sort in `read_source`. It fails when the sort is removed. The ESS and support-census lines are labeled `conditional interval value`. Each p50 line prints "N of M turns sampled". The per-candidate cost lines are now labeled `measured`, because the gate reads corrected quotes. `LearningCauses` implements `AddAssign`. `Money` and `CostEstimate` stay separate: `CostEstimate` has `NoSupport`, which a single turn never has.
- Rewritten on purpose, because their claims changed: `the_promotion_summary_states_each_of_the_three_ruled_tests_and_its_result` now states the staged tests, and `a_shadow_candidate_that_differs_from_rules_is_not_evaluable` became `a_shadow_candidate_that_differs_from_rules_is_priced_from_quotes_but_not_quality_gated`. The fixture plans record `TooFewSamples` by default, so without `Spec::corrected` both would have passed only because cost was unpriced.
- Mutations of the fix, each failing its test: the agreeing set taken as every interval, and the full-set support check removed (the M1 test); `TooFewSamples` priced (the uncorrectable test); `sparse` forced false, and the sparse check removed (the L1 test); the `met` line on eligible sessions (the L3 test); either `same_route` in `propensity` reverted (the L4 test).

**Rulings on the M10 review-fix questions, 2026-09-30.**

- **Latency is gated on the corrected quote (orchestrator ruling, accepted).** A shadow log has a measured first output only where the learned target served, and that happens only where it agreed with `rules`. The latency gate reads the modeled first output of the learned plan, and only where both the residual term and the overhead term applied. This is the same reasoning as the owner's cost ruling. The measured p50 stays a display line.
- **"No quality loss on the agreeing intervals" is a paired statistic (orchestrator ruling, to be implemented).** On those intervals both policies served the same route. The test must compare them pair by pair: the bootstrap lower bound of the per-interval difference, learned minus `rules`, clustered by the same unit, against the 2-point allowance. It must not compare the learned lower bound with the `rules` point estimate. That form fails on sample noise alone. It blocks promotion whenever the positive rate varies, and it says nothing about loss. In `shadow` the difference is identically zero, so the test passes once the agreeing set meets `quality.min_sessions`. That is the honest reading: shadow cannot show a quality loss on turns where the two policies are the same. The binding test is the M11 live rerun.
  - *Implemented 2026-09-30.* `paired_bootstrap` in `offline/estimate.rs` draws learned and `rules` over the same resampled clusters from the one SplitMix64 stream. The draw (`resample`) is shared with the single-candidate bootstrap, whose replicates do not change. Test 1 in `offline/promotion.rs` passes when the lower bound of learned minus `rules` is at least -0.02. It reads `not evaluable` below `quality.min_sessions`, when a side has no weight, or on sparse support. The earlier fixture (20 agreeing `shadow` sessions, 17 positive) now reads `pass`. **Correction to the ruling:** the difference is zero on the agreeing set in `live` too, not only in `shadow`. An agreeing interval is one where the learned action is the `rules` action on every turn. A candidate's weight reads only its action, the served target, and the recorded propensity. So both sides carry the same weight on every agreeing interval, explored or not, and no fixture can make learned worse than `rules` on the agreeing set in any mode (`on_agreeing_live_intervals_learned_and_rules_carry_the_same_weight`). The failing direction is shown on the pure function instead (`the_paired_bound_fails_a_loss_the_weights_show`). Test 1 carries no loss signal in either mode. Test 1b at the M11 rerun is still the only binding quality test.
- **`rules` joins the live exploration set (owner ruling).** In `live`, `rules` was not a member of the exploration set. So wherever the learned policy differed, `rules` had zero logging probability, and the binding M11 quality comparison was never evaluable. `rules` now joins the uniform exploration set in `live`, so every turn has a positive probability of serving `rules`, and both policies get exact importance-weighted estimates over every interval. This changes the M4 exploration set and needs a new draw version. It ships as its own small PR before M11. (Implemented 2026-09-30, with a correction: the draw's encoding did not change, so the version that moved is `LEARNED_SELECTOR_REVISION`, to 3, not `LEARNER_DRAW_VERSION`. See "`rules` in the live exploration set" below.)

**M10 round-2 fixes, 2026-09-30.** The round-2 mutation pass and review findings on PR 35 were fixed test-first on the same branch. New tests are in `crates/roundhouse-core/tests/learner_offline_gates.rs`, except where noted.

- **Mutation survivors.** Each new test fails under its mutation:
  - `corrected_cost`'s `max`, taken as corrected only or quoted only: `the_cost_test_reads_the_larger_of_the_corrected_and_the_quoted_cost`.
  - `NoPredictedReuse` classified as unpriced: `a_no_predicted_reuse_quote_is_priced_at_its_uncached_bound`.
  - Either latency term dropped from `corrected_first_output`: `a_first_output_with_one_latency_term_applied_is_uncorrectable`.
  - The arguments of `paired_bootstrap` swapped in `paired_quality`: `the_paired_difference_is_learned_minus_rules`. `paired_quality` is now public, so the test can feed it diverging `live` intervals. On the agreeing set the order cannot show.
  - Either paired fill value changed: `an_undefined_paired_replicate_widens_the_bound_to_minus_one_and_one`.
  - The guard order swapped, or either weight guard removed: `each_paired_not_evaluable_reason_has_its_own_message_in_guard_order`. Each reason now has its own message: no learned weight and no `rules` weight are two guards.
  - Latency dropped from `promotable`: `a_latency_failure_alone_blocks_promotion`.
  - The census guard removed or inverted: `the_quote_census_counts_each_plan_the_cost_test_reads_once`.
- **`quality.min_sessions` counted sessions with no weight in `live`.** It now counts the sessions that hold an agreeing interval where the learned candidate has weight above zero. `min_sessions_counts_only_agreeing_sessions_where_learned_has_weight` (16 explored sessions and 4 served ones, against 20) failed first with 20 where 4 was expected. From the report, the no-weight guard now answers only under a minimum of 0, so `the_paired_test_without_support_or_on_sparse_support_is_not_evaluable` runs at minimums of 0 and 3.
- **A non-finite trajectory weight.** A new exclusion cause, `trajectory weight not finite`, is counted before any estimate reads the interval. `intervals_are_excluded_by_cause_and_the_counts_are_reported` (in `learner_offline.rs`, now with 3 turns at propensity 1e-110) failed first with `NonFiniteWeight` at `None` where `Some(1)` was expected. With the exclusion removed, `a_trajectory_weight_that_is_not_finite_is_excluded_by_its_cause` fails because the learned estimate reads `snips: Some(NaN)`, with an upper bound of NaN and a lower bound of 1.0. **Correction to the fixture in the review:** one review covers at most `MAX_REVIEW_TURNS = 64` turns, so a review of 200 turns is never accepted. 0.025^64 is about 1e-103, which does not underflow, so the test uses 64 turns at propensity 1e-5 (a product of 1e-320). The defect is real but hard to reach: at the ruled 5% rate over at most three strategies, the smallest recorded propensity is about 0.017, far above the 1.5e-5 per turn that 64 turns need. Only an extreme configuration or a corrupt record reaches it.
- **`NoPredictedReuse` kept an unverified cache discount** (orchestrator ruling under the owner's delegated cost authority). M3 has the quote's cached count and the ledger's rate card when it corrects. So under `NoPredictedReuse`, `Corrections::cost` now prices the quote with no cached tokens. It uses the same helper as `Applied`: the same `price_tokens` and the same clamp at zero. `a_predicted_reuse_on_a_target_that_never_predicted_any_is_priced_uncached` (in `learned_corrections.rs`) failed first with `got 10` where 200 was expected. A quote that predicts no reuse is unchanged.
  - What changes in a learner project, `shadow` or `live`: for a frontier plan whose target has enough cache samples but never predicted reuse, `adjusted_usd` rises from the quote to the quote priced with no cached tokens. In `shadow` only the record changes, and so does the report's corrected quote estimate. In `live` three readers see the higher number, so routes can change: the exploit order (`policy::exploit_order`), exploration eligibility (`explore::eligible`), and the grant check (`corrections::grant`), which can now return `Exceeds`.
  - No version changed. `LEARNED_SELECTOR_REVISION` covers the constraints and the choice, so this change is in its scope. But no record written under revision 1 exists outside tests: no deployed build composes a learner, because M8 and M9 are still awaiting review. If a learner build is deployed before this lands, the revision must be bumped.
  - **Addendum, 2026-09-30 (round-3 fixes): bumped to 2.** The reprice above changes corrected cost, which feeds both the constraints (the grant check) and the choice among passing strategies (`exploit_order`, exploration eligibility) — a change of interpretation under `LEARNED_SELECTOR_REVISION`'s own doc. A record or artifact written under revision 1 is now refused rather than silently re-read under the new pricing (`MixedEpoch` on a stored record; `ArtifactError::Revision` on an artifact — `an_artifact_written_under_the_prior_selector_revision_is_refused` in `artifact.rs`). The epoch's golden-digest test was recomputed for `selector=2`; every fixture naming `selector_revision` was bumped to match.
- **The 1b line had no cluster unit.** It now says `clustered by sessions (sequence key)`. `the_cluster_unit_label_appears_on_every_clustered_number` (in `learner_offline.rs`) checks `bootstrap lower bound`.
- **Nits.** `BootstrapPlan` refuses fewer than 40 resamples when the manifest is read, and the error names `bootstrap.resamples` (`a_calibration_config_with_fewer_than_forty_resamples_is_refused`). `sparse_starts_one_past_the_lower_index` (in `offline/estimate.rs`) pins the boundary: 5 undefined replicates of 200 are not sparse, and 6 are. `PromotionSummary::build` reads the report's learned and `rules` estimates instead of running both bootstraps again. `latency()` has one `match`. The propensity doc in `evidence.rs` says it is the probability of the served route.

**`rules` in the live exploration set, 2026-09-30.** The owner ruling above (the third bullet of the rulings on the M10 review-fix questions) is implemented test-first on `ai/learner-explore-rules`, cut from `ai/learner-m10-calibrator` at `25a13a9`. It ships as its own PR before M11.

- **The set.** `explore::eligible(plans, on_infeasible)` returns the M4 members (unproven, meets every hard constraint, strictly cheaper than the reference) in configured order, then `rules` when the `rules` plan meets every hard constraint and is not already a member. `rules` is exempt from the unproven and cheaper filters, because it is the quality baseline, not a probe for a cheaper route. A member the M4 filters already admit keeps its place, so the M4 set is always a prefix of the new one, and `rules` is never listed twice (`propensity` counts members, so a repeat would inflate its share). The reference (the exploit head, else `rules`) is now derived inside `eligible`, so the policy and the calibrator call one function.
- **Tests** (`learned_explore.rs`, except where noted). Each failed first as shown:
  - `a_diverging_live_turn_that_draws_rules_serves_the_rules_route`: set `[]` where `[rules]` was expected. The turn serves `frontier/large` at propensity `rate`, and a stay draw serves the exploit at `1 - rate`.
  - `a_live_turn_whose_exploit_is_rules_records_the_shared_probability`: set `[efficient]` where `[efficient, rules]` was expected. The exploit and the explored `rules` member both record `0.95 + 0.05/2 = 0.975`; exploring `efficient` records `0.025`. On the pure function, two members sharing the exploit route give 1.0.
  - `a_rules_plan_that_fails_a_hard_constraint_never_joins_the_set`: passed before the change (nothing added `rules` then). It fails under the mutation that drops the `meets_hard` check.
  - `rules_in_the_set_does_not_open_shadow_or_unreviewed_turns`, with the existing shadow and local-only tests: still hold.
  - ~~`a_live_turn_nothing_passes_explores_rules_under_either_on_infeasible`~~: replaced by `a_refuse_turn_nothing_passes_never_explores_rules` after the owner's `refuse` ruling below; its `serve_rules` half is kept there.
  - `a_record_from_the_old_set_rule_does_not_replay` (`learner_offline_promotion.rs`): `ReplayMismatch` was `None` where 20 was expected.
  - `test_1b_is_evaluable_on_live_intervals_that_explored_rules` (`learner_offline_promotion.rs`): 20 `live` sessions, each with three served diverging intervals and one that explored `rules`. 1b reads `pass`, with both sides supported on 80 of 80 intervals and the `rules` estimate at 0.75. A control where `rules` is over the grant reads `not evaluable` (the support guard stays). This test passed before the change, because `replays` did not check the set. With the set check, it fails when `rules` is dropped from the rule.
  - Mutations: dropping `rules` from the set fails tests 1, 2 and 6 (and the replay test). Letting `rules` join without the hard-constraint check fails test 3. Removing the de-duplication fails `exploration_is_uniform_over_the_eligible_set`.
- **Replay checks the set (correction to the brief).** The brief said the M10 replay check "must still refuse" a record whose set rule does not match. It did not refuse one before: `replays` checked the explored member against the recorded set, but never re-derived the set, and the calibrator never compares selector revisions or epochs across records (only `MixedEpoch` inside one interval). `replays` now requires the recorded set to equal `eligible` over the recorded plans when the turn could explore, and to be empty when it could not. This is a check on what the record already pins. Six offline fixtures recorded sets that the M4 rule itself would not derive (for example `[efficient]` at the same price as `rules`). They now record rule-consistent sets, using a new `Spec::priced` helper.
- **Version: `LEARNED_SELECTOR_REVISION` 2 to 3 (correction to the ruling's "new draw version").** `LEARNER_DRAW_VERSION` names the hash encoding of the draw, which did not change, and its golden digest is unchanged. The same draw now indexes a different set. `LEARNED_SELECTOR_REVISION`'s own doc names "the exploration rule", so this change is in its scope. Revision 2 has not shipped, but PR 35 (revision 2, old set rule) merges separately from this PR; without a bump, one number would name two set rules. The epoch golden digest is recomputed (`10ec3614d0d7ce2f80574c77e8f1500e` under `selector=3`), an artifact under revision 1 or 2 is refused, and the server fixtures name 3. The bump changes the epoch, so it separates learner-store counters and refuses old artifacts; the `replays` set check is what refuses an old record's interval.
- **What changes in a `live` project.** A turn that could explore and whose `rules` plan meets every hard constraint now has a non-empty set, even when no other member qualifies. So:
  - When the learned choice differs from `rules`, a turn that had no exploration before now serves the `rules` route with probability `rate / |set|` (`rate` when `rules` is the only member). This is the ruled intent.
  - When the exploit is already the `rules` route, the served route is unchanged; only the record changes (the choice can be `explore`, and the propensity is below 1).
  - Every other member's share drops from `rate / k` to `rate / (k + 1)`.
  - ~~**Under `refuse`**, a turn that nothing passes used to refuse unless a cheaper unproven member existed. It now serves `rules` with probability `rate` instead of refusing.~~ Superseded by the owner's ruling below.
  - Under `serve_rules`, a turn that nothing passes and that explores `rules` serves the same target, at propensity 1. The route is built from the learned plans, so its fallbacks are the passing plans' first targets (none here), not the `rules` decision's own fallbacks. A turn with a failover is excluded from credit and calibration in any case.
- `shadow`, unreviewed sessions and local-only pools are unchanged: `possible` is false, the set is empty, and the propensity is 1.
- **Owner ruling, 2026-09-30: `refuse` keeps refusing.** Under `on_infeasible: refuse`, `rules` joins the set only on a turn where at least one strategy passes. A turn that nothing passes is refused exactly as before `rules` joined: on every draw when no cheaper unproven member exists, and otherwise it explores only those members. `serve_rules` is unchanged.
  - The rule is still one function: `eligible(plans, on_infeasible)`, called by the policy and by `replays`. The record did not carry `on_infeasible`, so replay could not apply the rule to a turn that nothing passed. `ExplorationEvidence` now records `on_infeasible` (a wire addition to learned `Routed` records; no deployed build writes them, and the selector revision is already 3 on this branch).
  - Tests: `a_refuse_turn_nothing_passes_never_explores_rules` (`learned_explore.rs`) failed first with `Draw { rate: 0.0, member: 0 } must refuse`. `a_refuse_record_nothing_passes_with_rules_in_its_set_does_not_replay` (`learner_offline_promotion.rs`) failed first on `Refuse [Efficient, Rules]`, which replayed (1 interval, expected 0). `a_refuse_turn_with_a_passing_strategy_keeps_rules_in_the_set` passed before and after; it holds the kept half. The mutation that lets `rules` join on a `refuse` turn that nothing passes fails the first two.
- **PR 37 round-1 fixes, 2026-09-30.** `ExplorationEvidence.on_infeasible` is now `Option` with a serde default, so a record written before the field decodes instead of making the whole log unreadable (the Redis reader and a dump refused the log). The policy always writes `Some`; `replays` refuses a `None` as `ReplayMismatch`, since its set rule is unknown. `a_learned_record_without_on_infeasible_still_decodes` (`learned_evidence.rs`) failed first with ``missing field `on_infeasible` ``, and `a_dump_with_a_record_from_before_on_infeasible_still_calibrates` (`learner_calibrate.rs`) failed first with `parsing the dump`; a default-rule mutation in `replays` fails the second. One predicate, `explore::has_default`, now says whether a turn has a default route, for both `eligible` and the policy's propensity; doc drift in the policy comment and the engine exploration test was corrected.
