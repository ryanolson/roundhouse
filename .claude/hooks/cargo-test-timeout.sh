#!/usr/bin/env bash
# PreToolUse hook: refuse a `cargo test` / `cargo nextest` invocation that is
# not wrapped in a coreutils `timeout`.
#
# Why a hard gate and not advice: a hung test hangs the whole cargo run, and
# the sessions most likely to hang one are exactly the ones mutating timeout
# and deadline code — break a timeout path and its guard test becomes an
# infinite wait, not a red assertion. A bounded run turns "stalled for three
# hours" into "exit 124 in fifteen minutes, go read the newest test".
cmd=$(jq -r '.tool_input.command // ""')
if echo "$cmd" | grep -qE '(^|[;&|[:space:](])cargo[[:space:]]+(test|nextest)\b' \
   && ! echo "$cmd" | grep -qE '(^|[;&|[:space:](])timeout[[:space:]]'; then
  cat <<'JSON'
{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"cargo test / cargo nextest must run under a bounded coreutils timeout so a hung test cannot stall the session. Re-run wrapped, e.g. `timeout 900 cargo test --workspace` (use ~300s for a targeted suite). Exit code 124 means something hung: suspect the newest test or a mutated timeout path, and run the suspect binary with `--test-threads=1 --nocapture` under a short timeout to name it."}}
JSON
fi
exit 0
