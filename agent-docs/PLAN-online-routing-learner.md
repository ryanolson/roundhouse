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
| Exploration (8): yes, bounded | A `live` project can explore. The exploration set holds `Unproven` strategies whose first target costs strictly less than the reference target and meets every hard constraint. The turn explores only when the session's validation arm consults the judge, the admitted pool holds a frontier target, and the draw is below the configured rate. The rate defaults to 5%. The member is chosen uniformly. `BelowFloor` strategies are never explored. | M4, M8 |
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
- The online latency constraint compares the modeled point estimate with `latency_limit_ms`. The p50 test of ruling 13 is measured in the M10 report from the log, turn start to first output, and is a promotion gate. The two use different statistics on purpose: the online check needs a per-turn number, and the report needs a distribution.
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
- The promotion comparison uses the candidate's bootstrap lower bound against the `rules` factual positive rate on the same session set.

## 5. Seam map at `520eda5` (verified again for M1 at `a266eb7`)

The draft's section 2 seam map was read at `1658633`. This table records what the current tree has. Line numbers are omitted on purpose, because another PR is changing `crates/` in this worktree.

| Seam | File | State at `520eda5` |
|---|---|---|
| `pick_tier`, `tier_pool`, `StagePolicy::choose`, `DecisionSource` with `CostGuard`, `is_signal_driven` | `crates/roundhouse-core/src/routing/stage.rs` | Present. No `route_pick`. No `Strategy` source. |
| `RoutingContext`, `admissible`, `LocalQuoteSkip` | `crates/roundhouse-core/src/routing/mod.rs` | Present. No `learning` field. |
| `SelectionSnapshot`, `SelectorBranch` (`Affinity`, `EscalationAudit`, `Stage`), `STAGE_SELECTOR_REVISION` | `crates/roundhouse-core/src/routing/selection.rs` | Present. No `Learned` branch. |
| `ProviderPricing::price_tokens`, `effective_write_per_mtok_usd`, `CacheLedger::model_for`, `BlockMarker`, `TargetState::last_block_marker` | `crates/roundhouse-core/src/routing/ledger.rs` | Present. The marker fact landed on this branch on 2026-09-28. |
| `ClassificationAxis`, `TurnIntent`, `TurnComplexity`, `ContextDependence`, `Graded`, `TurnClassification`, `ClassificationRecord`, `ClassificationWindow`, `TAXONOMY_VERSION = 1` | `crates/roundhouse-core/src/classify/mod.rs` | Present. No tier axis. |
| `SessionState::classifications`, `classifications_through`, `Session::commit` | `crates/roundhouse-core/src/session.rs` | Present. `commit` passes `None` as the learning mark. |
| `ReviewTracker`, `ReviewOutcome`, `TrackedDecision`, `MAX_REVIEW_TURNS = 64`, `MAX_REVIEW_DECISIONS = 256` | `crates/roundhouse-core/src/session/review.rs` | Present. No learning row. |
| `IntervalLabel` (`Positive`, `Negative`, `Unknown`) | `crates/roundhouse-core/src/validate/interval.rs` | Present. |
| `Arm::consults_judge` | `crates/roundhouse-core/src/validate/arm.rs` | Present. |
| `SessionEventKind`: `TurnStarted`, `Routed`, `OutputTextDelta`, `ResponseCompleted`, `ResponseIncomplete`, `ValidationDecided`, `ClassificationRequested`, `ClassificationRecorded`, `ClassificationSettlementRepaired` | `crates/roundhouse-core/src/event.rs` | Present. No `LearningApplied`. |
| `TurnBudget::admits`, `BudgetState::ExhaustedOverflow` | `crates/roundhouse-core/src/control/budget.rs` | Present. |
| `SettlementKey::SessionWatermark`, `OncePerCall` | `crates/roundhouse-core/src/control/spend.rs` | Present. |
| `SessionStore::append_events(lease, kinds, mark)`, `clear_learning_mark`, `requeue_learning`, `pending_learning`, `learning_sessions`, `LearningMark`, `MarkedSession`, `LearningCursor`, `ClearOutcome` | `crates/roundhouse-core/src/store.rs`, `store/learning.rs`, `store/contract/learning.rs` | Present. L3b is built and mutation-checked. |
| `KeyFamily`, `build_key`, the key-convention test | `crates/roundhouse-store-redis/src/keys.rs` | Present. No `Learn` family. |
| `Engine::plan`, `Engine::run_turn`, `opened_a_tier_escalation` | `crates/roundhouse-server/src/engine.rs` | Present. Selection capture moved to `engine/selection.rs` (`SelectionInputs`, `selection_inputs`, `local_quote_skip`). Classifier lifecycle is in `engine/classification.rs`. |
| `ProjectEntry` (`policy`, `budget`, `fair_use`, `validate`, `credentials`, `tiers`) | `crates/roundhouse-server/src/control_config/config.rs` | Present. No `learner` block. |
| `ValidateConfig` | `crates/roundhouse-server/src/control_config/validate.rs` | Present. |
| `TypeSafeShadow::questions`, `prepare`, `NotRun::NoAdmittedFrontier` | `crates/roundhouse-server/src/typesafe_shadow.rs` | Present. Three questions: intent, complexity, context dependence. |
| `ChoiceQuestion`, `SystemOneRequest`, `SystemOneClient` | `crates/roundhouse-fleet/src/typesafe.rs` | Present. A map of choice questions is supported. |
| `MetricsFold::apply`, `MetricsSnapshot`, the evaluation section | `crates/roundhouse-core/src/metrics/fold.rs`, `snapshot.rs`, `evaluation.rs` | Present. No learning or agreement section. |
| `/v1/metrics`, dashboard `renderEvaluation` | `crates/roundhouse-server/src/metrics_api.rs`, `dashboard.html` | Present. |
| `shared_backend::open`, `serve` | `crates/roundhouse-server/src/shared_backend.rs`, `main.rs` | Present. No learner store, no recovery task. |
| Local quotes at `expected_cost_usd: 0.0` | `crates/roundhouse-fleet/src/local.rs` | Present. The catalog price is a separate follow-up. |
| Existing binaries | `crates/roundhouse-server/src/bin/import-benchmarks` | The calibrator follows this layout. |

Files that do not exist yet and that this plan creates: `crates/roundhouse-core/src/routing/learn/` (M2 to M4), `crates/roundhouse-core/src/learn_store.rs` with `learn_store/` (M6), `crates/roundhouse-store-redis/src/learn.rs` with `learn/scripts.rs` (M7), `crates/roundhouse-server/src/engine/learning.rs` (M8), `crates/roundhouse-server/src/control_config/learner.rs` (M8), `crates/roundhouse-server/src/learner_recovery.rs` (M9), `crates/roundhouse-core/src/routing/learn/offline.rs` and `crates/roundhouse-server/src/bin/learner-calibrate/` (M10).

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

- `StagePolicy::route_pick(recipe, pick, admitted) -> Result<Decision, RoutingError>` is a behavior-preserving extraction. `rules` through `route_pick` equals `StagePolicy::choose`. The dominance cost guard, degrade-to-local, and the fallback order of ruling 4 stay inside it.
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

- The cache correction is draft section 9, through `ProviderPricing::price_tokens`, so the effective write premium is kept. Zero predicted reuse applies no correction and the record says `NoPredictedReuse`. No separate cache-miss penalty. No record names eviction. No return-trip pricing.
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
- Exploration as ruled and as section 4 specifies: `live` only, reviewed session only, a frontier target in the admitted pool, `Unproven` members strictly cheaper than the reference, every hard constraint, draw below the rate, uniform member.
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

- `ProjectEntry.learner: Option<LearnerConfig>` with `deny_unknown_fields`. Fields are draft section 12.1 plus `exploration: Option<ExplorationConfig>` with `rate`. `mode` defaults to `off`. `on_infeasible` defaults to `serve_rules`. `exploration.rate` defaults to `0.05` when the block is present. Every other field is required in `shadow` and `live`.
- The loader refuses: a `learner` block without `tiers`, a floor outside `0.0..=1.0`, a non-positive `z`, a zero timeout, fewer than 2 strategies, a list without `rules`, a repeated or unknown strategy, an artifact whose strategy list differs, an `exploration` block on a project whose mode is not `live`, and a rate outside `(0, 1]`.
- `Admission.learner: Option<Arc<LearnerTerms>>`, resolved with `tiers`. Admin `ProjectRecord` carries the block.
- `Engine::plan` computes the learned input and keys after it captures features and classifications, reads the store under `read_timeout_ms`, computes the draw, and sets `reviewed` from the session arm. `RoutingContext.learning: Option<&LearningInput>`. Every other `RoutingContext` literal gets `None`.
- `Engine::apply_learning` runs in the `run_turn` tail after `settle` and the fair-use draw, for steered, failed, and dispatched turns. Steps are draft section 11.5. At most one backfill per tail.
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
- The report has a promotion summary with the three ruled tests and their results: the candidate's positive-rate lower bound against the `rules` rate with the 2-point allowance, the estimated cost against the `rules` cost with the 10% requirement, and the measured p50 of first output from turn start against the limit. It states the session count against `min_sessions`. It also shows the support census, the effective sample size, the exclusion counts, the judge and classifier spend by strategy stratum, and the Jev agreement block of M1.
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

Preconditions, all required:

1. Every node runs a build at or after M9. The new event variants are a one-way door.
2. The project ran in `shadow` long enough for an M10 report to satisfy ruling 13: at least 20 sessions, the positive-rate lower bound no more than 2 points below `rules`, the estimated cost at least 10% lower, and the p50 first output from turn start at or below 10 s.
3. If the recipe admits a local target, the catalog sets the local price from ruling 6. Without it a local plan quotes $0 and wins every cost comparison, and the report cannot show a cost signal.
4. The owner approves the promotion in writing, and the approval names the report's manifest digest.

Steps:

1. Set `mode` to `live` for the project, with `on_infeasible = serve_rules` and the `exploration` block at the ruled rate.
2. Watch the `learning` section of `/v1/metrics` for the first sessions: infeasible counts by cause, explored turns, and store read failures.
3. Run M10 again after the first 20 live sessions. If any of the three tests fails, set `mode` back to `shadow`.

Rollback is `mode = shadow` or `off`, or the previous artifact. The store keeps every epoch.

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
| M3 cost, latency, and cache corrections | L6 | not started | |
| M4 gate, constraints, exploration, policy | L1 part 2 | not started | |
| M5 entries, credit, ops rows, Jev counts, cursor, marks | L2 | not started | |
| M6 learner store contract and memory store | L3 | not started | |
| M7 Redis learner store | L4 | not started | |
| M8 configuration, engine path, delivery, metrics | L5 part 1 | not started | |
| M9 startup, recovery task, audit | L5 part 2 | not started | |
| M10 calibrator and promotion report | L7 | not started | |
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
