#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Run with: bash agent-docs/research/agent-session-context-probe.sh
#
# This sends six short paid Codex and Claude Code requests. It requires existing logins. It writes raw results only in a temporary directory and prints selected metadata. Do not point it at production conversations.

set -euo pipefail

probe_dir="$(mktemp -d "${TMPDIR:-/tmp}/roundhouse-agent-session-probe.XXXXXX")"
session_id="$(uuidgen)"

run_claude() {
    local label="$1"
    shift
    local result="$probe_dir/$label.json"
    timeout 300 claude -p --safe-mode --tools '' --permission-mode dontAsk \
        --permission-prompts none --output-format json --max-budget-usd 0.10 \
        "$@" >"$result"
    jq -c '{session_id, local_command, stop_reason, num_turns, usage: {input_tokens: .usage.input_tokens, cache_read_input_tokens: .usage.cache_read_input_tokens, cache_creation_input_tokens: .usage.cache_creation_input_tokens}}' "$result"
}

run_codex() {
    local label="$1"
    shift
    local result="$probe_dir/$label.jsonl"
    timeout 300 codex "$@" >"$result"
    local thread_hash
    thread_hash="$(jq -r 'select(.type == "thread.started") | .thread_id' "$result" | sha256sum | awk '{print $1}')"
    jq -cs --arg thread_hash "$thread_hash" '{thread_sha256: $thread_hash, event_types: [.[] | .type] | unique, usage: ([.[] | select(.type == "turn.completed") | .usage] | last)}' "$result"
}

run_codex codex_create exec --skip-git-repo-check --sandbox read-only --json 'Return exactly PROBE_CODEX_OK.'
codex_thread_id="$(jq -r 'select(.type == "thread.started") | .thread_id' "$probe_dir/codex_create.jsonl" | head -n 1)"
run_codex codex_resume exec resume "$codex_thread_id" --skip-git-repo-check --json 'Return exactly PROBE_CODEX_RESUMED.'
run_claude create --session-id "$session_id" 'Return exactly PROBE_OK.'
run_claude resume --resume "$session_id" 'Return exactly PROBE_RESUMED.'
run_claude compact --resume "$session_id" '/compact'
run_claude after_compact --resume "$session_id" 'Return exactly PROBE_AFTER_COMPACT.'

printf 'Raw probe output is in %s\n' "$probe_dir"
