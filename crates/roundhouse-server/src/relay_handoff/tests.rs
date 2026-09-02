// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a chained handoff renders, and what it refuses.
//!
//! The two renderings are pinned as whole strings rather than by key-by-key
//! assertions: this file's bytes are read by a Relay that parses them with
//! `deny_unknown_fields`, so "the whole document, exactly" is the property that
//! matters and a per-key check would pass on a document with a stray table in
//! it. The values in the snapshots are the ones a 0.8.2 `--dry-run` was
//! observed echoing back (`agent = claude|codex`, `anthropic_base_url = …`,
//! `openai_base_url = …`), which is what makes them a claim about Relay rather
//! than about this formatter.

use std::path::PathBuf;

use super::*;

const ROOT: &str = "http://127.0.0.1:4321";

#[test]
fn a_claude_handoff_renders_the_anthropic_upstream_and_the_claude_agent_table() {
    let handoff = RelayHandoff::for_claude(ROOT, "claude").expect("a root and a command");
    assert_eq!(
        handoff.config_toml(),
        "[upstream]\n\
         anthropic_base_url = \"http://127.0.0.1:4321\"\n\
         \n\
         [agents.claude]\n\
         command = \"claude\"\n"
    );
}

/// The codex rendering, and the one asymmetry between the two: Relay's
/// `openai_base_url` is the **prefixed** base, so the same deployment root
/// produces a different upstream string here.
#[test]
fn a_codex_handoff_renders_the_prefixed_openai_upstream_from_the_same_root() {
    let handoff = RelayHandoff::for_codex(ROOT, "codex").expect("a root and a command");
    assert_eq!(
        handoff.config_toml(),
        "[upstream]\n\
         openai_base_url = \"http://127.0.0.1:4321/v1\"\n\
         \n\
         [agents.codex]\n\
         command = \"codex\"\n"
    );
    assert_eq!(handoff.upstream_base_url(), "http://127.0.0.1:4321/v1");
}

/// Neither rendering writes an upstream auth header — the reference chained
/// wiring carries the turn key on the client's own header instead, and a header
/// written here is one system config layer away from being silently cleared
/// (hazard 4). Asserted as an absence because that is what it is: no
/// constructor, no builder and no argument can produce one.
#[test]
fn neither_rendering_configures_an_upstream_auth_header() {
    for toml in [
        RelayHandoff::for_claude(ROOT, "claude")
            .unwrap()
            .config_toml(),
        RelayHandoff::for_codex(ROOT, "codex")
            .unwrap()
            .config_toml(),
    ] {
        assert!(!toml.contains("auth_header"), "{toml}");
    }
}

/// The refusal `ClaudeLaunch::new` makes, made again here — because the value
/// that reaches Relay is a *different* string built by a different call, and
/// the generator's refusal says nothing about it.
#[test]
fn a_root_carrying_the_api_prefix_is_refused_by_both_constructors() {
    let root = format!("{ROOT}{API_PREFIX}");
    for error in [
        RelayHandoff::for_claude(&root, "claude").unwrap_err(),
        RelayHandoff::for_codex(&root, "codex").unwrap_err(),
    ] {
        assert!(
            matches!(error, RelayHandoffError::RootCarriesApiPrefix { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("/v1/v1/messages"), "{error}");
    }
}

#[test]
fn an_empty_root_is_refused_naming_the_upstream_key_it_would_have_filled() {
    let error = RelayHandoff::for_codex("   ", "codex").unwrap_err();
    assert_eq!(
        error,
        RelayHandoffError::NoRoot {
            key: "openai_base_url"
        }
    );
}

/// A trailing slash is trimmed rather than refused: it is an ordinary way to
/// write a base URL and Relay itself trims one at concatenation, so refusing it
/// would be this launcher inventing a rule. What must not happen is the
/// *configured* and the *verified* strings disagreeing about it — hence the
/// second assertion.
#[test]
fn a_trailing_slash_is_trimmed_in_both_the_rendering_and_the_verification() {
    let handoff = RelayHandoff::for_claude("http://127.0.0.1:4321/", "claude").unwrap();
    assert_eq!(handoff.upstream_base_url(), ROOT);
    assert_eq!(
        handoff.resolved_upstream_line(),
        "anthropic_base_url = http://127.0.0.1:4321"
    );
}

/// A quote in either field would either break the file or, worse, parse as a
/// different value — and a chained launch aimed at a value nobody wrote is the
/// exact failure the preflight exists for.
#[test]
fn a_value_toml_would_mangle_is_refused_naming_the_field() {
    let error = RelayHandoff::for_claude("http://host\"/", "claude").unwrap_err();
    assert!(
        matches!(
            error,
            RelayHandoffError::Unquotable {
                field: "upstream base URL",
                ..
            }
        ),
        "{error:?}"
    );
    let error = RelayHandoff::for_claude(ROOT, "cla\nude").unwrap_err();
    assert!(
        matches!(
            error,
            RelayHandoffError::Unquotable {
                field: "agent command",
                ..
            }
        ),
        "{error:?}"
    );
}

/// The argv Relay is actually run with, both forms. `--` is asserted as the
/// last token of the launch form because its absence is not a parse error with
/// a message — Relay would consume the agent's own flags as its own.
#[test]
fn the_two_argv_forms_differ_only_in_how_they_end() {
    let handoff = RelayHandoff::for_claude(ROOT, "claude").unwrap();
    let config = PathBuf::from("/scratch/relay-config.toml");
    assert_eq!(
        handoff.run_argv(&config),
        [
            "run",
            "--agent",
            "claude",
            "--config",
            "/scratch/relay-config.toml",
            "--"
        ]
    );
    assert_eq!(
        handoff.preflight_argv(&config),
        [
            "run",
            "--agent",
            "claude",
            "--config",
            "/scratch/relay-config.toml",
            "--dry-run"
        ]
    );
}

/// The isolation set, as a value. Every variable Relay reads a config layer or
/// writes state through points at the caller's scratch, and `PATH` is the one
/// thing carried over.
#[test]
fn the_preflight_environment_is_path_plus_one_scratch_directory() {
    let env = RelayHandoff::preflight_env(Path::new("/scratch/home"), "/usr/bin:/bin");
    let names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "PATH",
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME"
        ]
    );
    assert!(
        env.iter()
            .skip(1)
            .all(|(_, value)| value == "/scratch/home"),
        "{env:?}"
    );
}

/// A real 0.8.2 `--dry-run` report, verbatim, with the upstream this handoff
/// asked for. Transcribed from a run against the pinned binary rather than
/// invented, so the parse below is a claim about Relay's report format.
const RESOLVED_REPORT: &str = "\
agent = claude
gateway_url = http://127.0.0.1:45169
openai_base_url = https://api.openai.com/v1
openai_auth = unset
anthropic_base_url = http://127.0.0.1:4321
anthropic_auth = unset
max_hook_payload_bytes = 20971520
";

#[test]
fn a_report_naming_this_upstream_verifies() {
    let handoff = RelayHandoff::for_claude(ROOT, "claude").unwrap();
    assert_eq!(handoff.verify_resolved(RESOLVED_REPORT), Ok(()));
}

/// The re-aim. Note which line is *not* enough to satisfy the check: the report
/// still carries an `openai_base_url` and a `gateway_url`, and a `contains`
/// against the wrong key would have passed.
#[test]
fn a_report_naming_a_different_upstream_is_refused_naming_the_system_config() {
    let handoff = RelayHandoff::for_claude(ROOT, "claude").unwrap();
    let re_aimed = RESOLVED_REPORT.replace("http://127.0.0.1:4321", "http://10.0.0.9:8080");
    let error = handoff.verify_resolved(&re_aimed).unwrap_err();
    assert_eq!(error.key, "anthropic_base_url");
    assert_eq!(error.resolved, "http://10.0.0.9:8080");
    assert!(error.to_string().contains(RELAY_SYSTEM_CONFIG), "{error}");
}

/// A codex handoff must not be satisfied by the Anthropic line the same report
/// always carries — this is the assertion that pins `verify_resolved` to the
/// agent's own key rather than to "some base URL appeared".
#[test]
fn a_codex_handoff_reads_the_openai_key_and_not_the_anthropic_one() {
    let handoff = RelayHandoff::for_codex(ROOT, "codex").unwrap();
    let error = handoff.verify_resolved(RESOLVED_REPORT).unwrap_err();
    assert_eq!(error.key, "openai_base_url");
    assert_eq!(error.resolved, "https://api.openai.com/v1");
}

/// A report with no such key at all reports it as absent rather than as a
/// re-aim: a Relay whose `--dry-run` vocabulary moved is a pin-bump question,
/// and an operator told "a system config file re-aimed your launch" would go
/// looking for a file that does not exist.
#[test]
fn a_report_missing_the_key_is_distinguishable_from_a_re_aim() {
    let handoff = RelayHandoff::for_claude(ROOT, "claude").unwrap();
    let error = handoff.verify_resolved("agent = claude\n").unwrap_err();
    assert_eq!(error.resolved, NO_SUCH_LINE);
}
