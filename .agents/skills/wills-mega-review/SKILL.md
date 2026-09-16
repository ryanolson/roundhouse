---
name: wills-mega-review
description: Prepare a mostly working, tested PR for human review by iterating independent read-only thermonuclear code-quality reviews, fixes, and regression tests until the review is clean, then add the `human-review` PR tag. Use before asking a human to review, approve, or merge a PR.
---

# Will's Mega Review

Run this only once the feature mostly works, relevant tests pass, and the PR could be ready to merge.

1. Launch a fresh **read-only** subagent. Direct it to run the repo-local `$thermo-nuclear-code-quality-review` skill against the current branch/PR diff, report actionable findings, and make no edits, commits, labels, or other changes.
2. If it reports findings, incorporate the feedback yourself to the best of your ability. Run the relevant tests after the changes.
3. Launch another fresh read-only review subagent and repeat steps 1–2. Do not reuse a prior reviewer or stop after one pass.
4. Finish only when the latest review reports no actionable code-quality findings and the tests still pass. If a finding cannot be resolved autonomously, do not tag the PR; report the blocker instead.
5. Add the `human-review` tag to the PR, then ask for human review. Never request human review or add the tag earlier.
