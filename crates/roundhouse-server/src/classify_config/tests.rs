// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a deployment has to write down, and what it cannot leave out.

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::MemorySpendLedger;
use std::sync::Arc;

use super::*;
use crate::classify_runtime::compose;

const COMPLETE: &str = r#"{
  "revision": 3,
  "model": "jev-1.12",
  "base_url": "https://classifier.test/v1",
  "auth": { "env": "TYPESAFE_API_KEY" },
  "pricing": { "input_per_mtok_usd": 0.042, "output_per_mtok_usd": 0.084 },
  "expected_output_tokens": 24,
  "caps": { "max_prior_classifications": 6, "max_prompt_chars": 2000, "max_total_bytes": 8192 },
  "transport": { "max_request_bytes": 65536, "max_response_bytes": 16384, "deadline_ms": 4000 },
  "executor": {
    "max_in_flight": 16,
    "max_http_concurrency": 4,
    "call_ttl_ms": 60000,
    "result_retention_ms": 900000,
    "sweep_interval_ms": 5000
  },
  "budget": { "limit_usd": 25.0, "window": "monthly", "warn_at": 0.8 }
}"#;

fn with(field: &str, replacement: &str) -> String {
    COMPLETE.replace(field, replacement)
}

fn env(name: &str) -> Option<String> {
    match name {
        "TYPESAFE_API_KEY" => Some("sk-classifier-deployment-key".to_string()),
        _ => None,
    }
}

/// **A complete file is still off until it says otherwise.**
///
/// Writing the configuration and turning it on are two acts: an operator who
/// has decided the model, the rate card and the caps has still not agreed to
/// send anybody's prompt to a third party.
#[test]
fn a_complete_configuration_is_disabled_unless_it_says_enabled() {
    let config = ClassifyConfig::from_json(COMPLETE, "<test>").expect("a valid file");
    assert!(!config.enabled);

    let on = ClassifyConfig::from_json(
        &COMPLETE.replace("\"revision\": 3,", "\"revision\": 3, \"enabled\": true,"),
        "<test>",
    )
    .expect("a valid file");
    assert!(on.enabled);
}

/// **A disabled file composes no runtime**, so the code path of a deployment
/// that wrote the configuration and left it off is the path of one that never
/// wrote it at all.
#[test]
fn a_disabled_configuration_composes_no_runtime() {
    let config = ClassifyConfig::from_json(COMPLETE, "<test>").expect("a valid file");
    let runtime = compose(
        "<test>",
        &config,
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &env,
    )
    .expect("a disabled file is not an error");
    assert!(runtime.is_none());
}

/// The control: an enabled file with its key present does compose one.
#[test]
fn an_enabled_configuration_with_its_key_composes_a_runtime() {
    let config = ClassifyConfig::from_json(
        &COMPLETE.replace("\"revision\": 3,", "\"revision\": 3, \"enabled\": true,"),
        "<test>",
    )
    .expect("a valid file");
    let runtime = compose(
        "<test>",
        &config,
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and it is present");

    assert_eq!(runtime.limits().max_in_flight, 16);
    assert_eq!(runtime.limits().max_http_concurrency, 4);
    assert_eq!(runtime.projection_caps().max_prior_classifications, 6);
    // `COMPLETE` predates the `max_prior_turns` split and names no opinion of
    // its own, so an existing deployment's configuration must keep bounding
    // local metadata exactly as `max_prior_classifications` always did.
    assert_eq!(runtime.projection_caps().max_prior_turns, 6);
}

/// A file that has taken an opinion on `max_prior_turns` is read literally,
/// and distinctly from `max_prior_classifications` — the two caps bound
/// unrelated lists and an operator may size them differently.
#[test]
fn an_explicit_max_prior_turns_overrides_the_fallback() {
    let config = ClassifyConfig::from_json(
        &with(
            "\"max_prior_classifications\": 6",
            "\"max_prior_classifications\": 6, \"max_prior_turns\": 2",
        ),
        "<test>",
    )
    .expect("a valid file");
    assert_eq!(config.caps().max_prior_classifications, 6);
    assert_eq!(config.caps().max_prior_turns, 2);
}

/// **An enabled classifier with no key stops the process.**
///
/// A runtime surprise on the first turn that would have used it is the shape a
/// boot check exists to prevent — and the variable is named, because that is
/// the part an operator acts on.
#[test]
fn an_enabled_configuration_without_its_key_is_a_boot_refusal() {
    let config = ClassifyConfig::from_json(
        &COMPLETE.replace("\"revision\": 3,", "\"revision\": 3, \"enabled\": true,"),
        "<test>",
    )
    .expect("a valid file");
    let refusal = compose::<ByteTokenizer>(
        "/etc/roundhouse/classify.json",
        &config,
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &|_| None,
    )
    .err()
    .expect("an enabled classifier with no credential cannot start");

    let message = format!("{refusal:#}");
    assert!(message.contains("TYPESAFE_API_KEY"), "{message}");
    assert!(
        message.contains("/etc/roundhouse/classify.json"),
        "{message}"
    );
}

/// Nothing load-bearing has a default. Each of these is a number that would
/// otherwise be chosen by this source rather than by the deployment.
#[test]
fn every_load_bearing_field_is_required() {
    for (field, why) in [
        (
            "\"model\": \"jev-1.12\",",
            "a floating model id re-ranks itself",
        ),
        (
            "\"auth\": { \"env\": \"TYPESAFE_API_KEY\" },",
            "no key, no call",
        ),
        (
            "\"pricing\": { \"input_per_mtok_usd\": 0.042, \"output_per_mtok_usd\": 0.084 },",
            "a guessed rate card puts invented dollars in a ledger",
        ),
        ("\"revision\": 3,", "records need a configuration identity"),
        (
            "\"budget\": { \"limit_usd\": 25.0, \"window\": \"monthly\", \"warn_at\": 0.8 }",
            "an unbudgeted classifier has no ceiling",
        ),
    ] {
        let without = COMPLETE.replace(field, "").replace(",\n}", "\n}");
        assert!(
            ClassifyConfig::from_json(&without, "<test>").is_err(),
            "a file missing `{field}` must be refused: {why}"
        );
    }
}

/// Zeroes are refused rather than read as "no limit": a zero in-flight bound
/// disables the feature silently on a deployment that has just enabled it.
#[test]
fn a_zero_bound_is_refused_rather_than_read_as_unlimited() {
    for broken in [
        with("\"max_in_flight\": 16", "\"max_in_flight\": 0"),
        with("\"max_http_concurrency\": 4", "\"max_http_concurrency\": 0"),
        with("\"call_ttl_ms\": 60000", "\"call_ttl_ms\": 0"),
        with("\"sweep_interval_ms\": 5000", "\"sweep_interval_ms\": 0"),
        with("\"max_prompt_chars\": 2000", "\"max_prompt_chars\": 0"),
        with("\"max_total_bytes\": 8192", "\"max_total_bytes\": 0"),
        with("\"deadline_ms\": 4000", "\"deadline_ms\": 0"),
        with("\"limit_usd\": 25.0", "\"limit_usd\": 0.0"),
        with("\"warn_at\": 0.8", "\"warn_at\": 0.0"),
    ] {
        assert!(
            ClassifyConfig::from_json(&broken, "<test>").is_err(),
            "a zero bound must be refused:\n{broken}"
        );
    }
}

/// An unknown key is a typo, and a typo in a file that governs egress is worth
/// refusing rather than ignoring.
#[test]
fn an_unknown_field_is_refused() {
    let typo = with("\"revision\": 3,", "\"revision\": 3, \"enabed\": true,");
    assert!(ClassifyConfig::from_json(&typo, "<test>").is_err());
}

/// The rate card reaches the adapter with its two cache axes at zero, because
/// this service reports no cache term. A field for either would invite somebody
/// to fill one in.
#[test]
fn the_rate_card_has_no_cache_axes_to_guess_at() {
    let config = ClassifyConfig::from_json(COMPLETE, "<test>").expect("a valid file");
    let pricing = config.pricing();
    assert_eq!(pricing.input_per_mtok_usd, 0.042);
    assert_eq!(pricing.output_per_mtok_usd, 0.084);
    assert_eq!(pricing.cached_input_per_mtok_usd, 0.0);
    assert_eq!(pricing.cache_write_per_mtok_usd, 0.0);
}

/// The evaluation budget carries project and member semantics, and refuses
/// rather than degrading: there is no cheaper classifier to fall back to, and an
/// exhausted evaluation budget stops classifying and stops nothing else.
#[test]
fn the_evaluation_budget_has_project_and_member_ceilings() {
    let pooled = ClassifyConfig::from_json(COMPLETE, "<test>").expect("a valid file");
    let terms = pooled.budget_terms();
    assert_eq!(terms.budget.limit_usd, 25.0);
    assert_eq!(terms.budget.window, BudgetWindow::Monthly);
    assert_eq!(terms.budget.on_exhaustion, Exhaustion::Refuse);
    assert_eq!(terms.member_ceiling_usd(), None);

    let shared = ClassifyConfig::from_json(
        &with(
            "\"warn_at\": 0.8",
            "\"warn_at\": 0.8, \"member_share\": 0.25",
        ),
        "<test>",
    )
    .expect("a valid file");
    assert_eq!(shared.budget_terms().member_ceiling_usd(), Some(6.25));
}
