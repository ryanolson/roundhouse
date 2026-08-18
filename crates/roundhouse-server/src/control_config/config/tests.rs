// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::control_config::fixtures::{TURN_HASH, sample_config};

#[test]
fn a_named_but_unreadable_control_plane_file_stops_the_process() {
    let error =
        ControlPlaneConfig::load("/nonexistent/roundhouse-control-plane-test.json").unwrap_err();
    assert!(
        matches!(error, ControlPlaneError::Read { .. }),
        "an unreadable file must be a Read error, not silently treated as Open: {error}"
    );
    assert!(
        error
            .to_string()
            .contains("roundhouse-control-plane-test.json")
    );
}

#[test]
fn a_duplicate_key_hash_is_rejected_naming_the_key() {
    // The same hash appears once in `keys` and once more in `admin_keys`:
    // one secret must resolve to exactly one scope.
    let json = r#"{
      "projects": [{ "id": "acme" }],
      "users": [{ "id": "ada" }],
      "keys": [
        { "project": "acme", "user": "ada", "key_sha256": "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677ddfd" }
      ],
      "admin_keys": [
        "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677ddfd"
      ]
    }"#;
    let error = ControlPlaneConfig::from_json(json, "test").unwrap_err();
    match error {
        ControlPlaneError::DuplicateHash { key_sha256, .. } => {
            assert_eq!(key_sha256, TURN_HASH);
        }
        other => panic!("expected DuplicateHash, got {other:?}"),
    }
}

#[test]
fn a_key_referencing_an_unknown_project_or_user_is_rejected() {
    let unknown_project = r#"{
      "projects": [{ "id": "acme" }],
      "users": [{ "id": "ada" }],
      "keys": [
        { "project": "ghost", "user": "ada", "key_sha256": "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677ddfd" }
      ]
    }"#;
    let error = ControlPlaneConfig::from_json(unknown_project, "test").unwrap_err();
    match error {
        ControlPlaneError::UnknownProject { project, .. } => assert_eq!(project, "ghost"),
        other => panic!("expected UnknownProject, got {other:?}"),
    }

    let unknown_user = r#"{
      "projects": [{ "id": "acme" }],
      "users": [{ "id": "ada" }],
      "keys": [
        { "project": "acme", "user": "ghost", "key_sha256": "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677ddfd" }
      ]
    }"#;
    let error = ControlPlaneConfig::from_json(unknown_user, "test").unwrap_err();
    match error {
        ControlPlaneError::UnknownUser { user, .. } => assert_eq!(user, "ghost"),
        other => panic!("expected UnknownUser, got {other:?}"),
    }
}

#[test]
fn a_bad_slug_is_rejected() {
    let cases = ["Acme", "ac/me", "", &"a".repeat(65)];
    for id in cases {
        let json = format!(
            r#"{{ "projects": [{{ "id": {id:?} }}], "users": [] }}"#,
            id = id
        );
        let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
        assert!(
            matches!(error, ControlPlaneError::BadProjectSlug { .. }),
            "slug `{id}` should have been rejected, got {error:?}"
        );
    }

    // A slug that is valid at exactly the length bound is accepted: the
    // bound is `<= 64`, not `< 64`.
    let boundary = "a".repeat(64);
    let json = format!(r#"{{ "projects": [{{ "id": {boundary:?} }}], "users": [] }}"#);
    ControlPlaneConfig::from_json(&json, "test")
        .expect("a 64-character slug is exactly at the bound and must validate");
}

#[test]
fn a_malformed_sha256_is_rejected() {
    let cases = [
        "not-hex-at-all",
        "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677dd", // 63 chars
        "0BD5182863262C911D4479F1B25FEC5F3E6846653B9028E65F61B2B33677DDF", // uppercase
    ];
    for hash in cases {
        let json = format!(
            r#"{{
              "projects": [{{ "id": "acme" }}],
              "users": [{{ "id": "ada" }}],
              "keys": [{{ "project": "acme", "user": "ada", "key_sha256": {hash:?} }}]
            }}"#
        );
        let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
        assert!(
            matches!(error, ControlPlaneError::MalformedHash { .. }),
            "hash `{hash}` should have been rejected, got {error:?}"
        );
    }
}

#[test]
fn the_example_file_validates() {
    // From the crate root up to the workspace root, mirroring
    // `tests/example_catalog.rs::example_path`.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/control-plane.example.json");
    let config = ControlPlaneConfig::load(&path)
        .unwrap_or_else(|error| panic!("the shipped example must validate: {error}"));
    assert!(!config.projects.is_empty());
    assert!(!config.users.is_empty());

    // Decision 9's two config additions, both present in the shipped
    // example: a project policy, and a key override that narrows it. A
    // widening override would have failed `load` above, so reaching this
    // line already proves the override narrows -- these assertions pin
    // down *that it is exercised at all*, not just that nothing widened.
    let acme = config
        .projects
        .iter()
        .find(|project| project.id == "acme")
        .expect("the example's acme project");
    assert!(
        acme.policy.is_some(),
        "the example must demonstrate a project policy"
    );
    let key = config
        .keys
        .first()
        .expect("the example must ship at least one key");
    assert!(
        key.overrides.is_some(),
        "the example must demonstrate a key override"
    );

    // Decision 8's config additions: a budgeted project with overflow
    // spelled out explicitly, a capped key, and a share key. Reading
    // these off the *resolved* `turn_keys` table (not the raw
    // `AllocationConfig`) proves the whole seam -- parse, validate,
    // resolve -- rather than just that the JSON shape parses.
    assert!(
        acme.budget.is_some(),
        "the example must demonstrate a project budget"
    );
    assert_eq!(
        acme.budget.as_ref().unwrap().overflow_when_local_saturated,
        Some(true),
        "the example spells the overflow valve out explicitly rather than \
         relying on the default"
    );

    let ceilings: HashSet<_> = config
        .turn_keys
        .values()
        .filter_map(|admission| admission.budget.as_ref())
        .map(|terms| terms.member_ceiling_usd().map(|usd| usd.to_bits()))
        .collect();
    assert!(
        ceilings.contains(&Some(100.0f64.to_bits())),
        "the example must demonstrate a capped key resolving to its ceiling: {ceilings:?}"
    );
    assert!(
        ceilings.contains(&Some(125.0f64.to_bits())),
        "the example must demonstrate a share key resolving to its ceiling \
         (25% of the $500 project limit): {ceilings:?}"
    );
}

// -----------------------------------------------------------------------
// Decision 8: budget on projects, allocation on keys
// -----------------------------------------------------------------------

/// The smallest JSON that carries a valid budget, with `limit_usd`
/// substitutable so the boundary tests below can push it out of range.
fn budget_json(limit_usd: impl std::fmt::Display) -> String {
    format!(
        r#"{{
          "projects": [
            {{
              "id": "acme",
              "budget": {{
                "limit_usd": {limit_usd},
                "window": "total",
                "on_exhaustion": "degrade_to_local"
              }}
            }}
          ],
          "users": []
        }}"#
    )
}

#[test]
fn a_budget_with_nonpositive_limit_is_rejected() {
    for limit_usd in ["0.0", "-5.0"] {
        let error = ControlPlaneConfig::from_json(&budget_json(limit_usd), "test").unwrap_err();
        match error {
            ControlPlaneError::BudgetLimitNotPositive {
                entry,
                limit_usd: got,
                ..
            } => {
                assert_eq!(entry, "project `acme`");
                assert_eq!(got, limit_usd.parse::<f64>().unwrap());
            }
            other => panic!("expected BudgetLimitNotPositive, got {other:?}"),
        }
    }

    // Control: a real limit validates.
    ControlPlaneConfig::from_json(&budget_json("1.0"), "test")
        .expect("a positive limit is not refused");
}

#[test]
fn warn_at_outside_the_half_open_interval_is_rejected() {
    let json = format!(
        r#"{{
          "projects": [
            {{
              "id": "acme",
              "budget": {{
                "limit_usd": 10.0,
                "window": "total",
                "on_exhaustion": "degrade_to_local",
                "warn_at": {warn_at}
              }}
            }}
          ],
          "users": []
        }}"#,
        warn_at = 0.0
    );
    let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
    match error {
        ControlPlaneError::WarnAtOutOfRange { entry, warn_at, .. } => {
            assert_eq!(entry, "project `acme`");
            assert_eq!(warn_at, 0.0);
        }
        other => panic!("expected WarnAtOutOfRange, got {other:?}"),
    }

    let json = format!(
        r#"{{
          "projects": [
            {{
              "id": "acme",
              "budget": {{
                "limit_usd": 10.0,
                "window": "total",
                "on_exhaustion": "degrade_to_local",
                "warn_at": {warn_at}
              }}
            }}
          ],
          "users": []
        }}"#,
        warn_at = 1.1
    );
    let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
    assert!(matches!(error, ControlPlaneError::WarnAtOutOfRange { .. }));

    // Control: 1.0 is the closed end of the interval and validates; an
    // absent warn_at falls back to the shared default.
    let json = r#"{
      "projects": [
        {
          "id": "acme",
          "budget": {
            "limit_usd": 10.0,
            "window": "total",
            "on_exhaustion": "degrade_to_local",
            "warn_at": 1.0
          }
        }
      ],
      "users": []
    }"#;
    ControlPlaneConfig::from_json(json, "test").expect("1.0 is exactly at the bound");
    ControlPlaneConfig::from_json(&budget_json(10.0), "test")
        .expect("an absent warn_at is not refused");
}

#[test]
fn a_share_fraction_outside_the_interval_is_rejected() {
    let json = format!(
        r#"{{
          "projects": [
            {{
              "id": "acme",
              "budget": {{
                "limit_usd": 100.0,
                "window": "total",
                "on_exhaustion": "degrade_to_local"
              }}
            }}
          ],
          "users": [{{ "id": "ada" }}],
          "keys": [{{
            "project": "acme",
            "user": "ada",
            "key_sha256": "{TURN_HASH}",
            "allocation": {{ "share": {{ "fraction": 0.0 }} }}
          }}]
        }}"#
    );
    let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
    match error {
        ControlPlaneError::ShareFractionOutOfRange {
            entry, fraction, ..
        } => {
            assert_eq!(entry, "key for project `acme`, user `ada`");
            assert_eq!(fraction, 0.0);
        }
        other => panic!("expected ShareFractionOutOfRange, got {other:?}"),
    }

    // Control 1: 1.0 is the closed end of a single share's interval (a
    // member may be allowed the whole project) and validates.
    let json = format!(
        r#"{{
          "projects": [
            {{
              "id": "acme",
              "budget": {{
                "limit_usd": 100.0,
                "window": "total",
                "on_exhaustion": "degrade_to_local"
              }}
            }}
          ],
          "users": [{{ "id": "ada" }}],
          "keys": [{{
            "project": "acme",
            "user": "ada",
            "key_sha256": "{TURN_HASH}",
            "allocation": {{ "share": {{ "fraction": 1.0 }} }}
          }}]
        }}"#
    );
    ControlPlaneConfig::from_json(&json, "test").expect("1.0 is exactly at the bound");

    // Control 2: this rule is about one member's own fraction, not the
    // sum across members -- two keys each within (0.0, 1.0] whose shares
    // sum past 1.0 is the legitimate over-subscription decision 1 and
    // decision 8 both describe, and is accepted (the project limit still
    // binds both).
    let json = format!(
        r#"{{
          "projects": [
            {{
              "id": "acme",
              "budget": {{
                "limit_usd": 100.0,
                "window": "total",
                "on_exhaustion": "degrade_to_local"
              }}
            }}
          ],
          "users": [{{ "id": "ada" }}, {{ "id": "bob" }}],
          "keys": [
            {{
              "project": "acme",
              "user": "ada",
              "key_sha256": "{TURN_HASH}",
              "allocation": {{ "share": {{ "fraction": 0.6 }} }}
            }},
            {{
              "project": "acme",
              "user": "bob",
              "key_sha256": "{SHARE_HASH}",
              "allocation": {{ "share": {{ "fraction": 0.6 }} }}
            }}
          ]
        }}"#
    );
    ControlPlaneConfig::from_json(&json, "test")
        .expect("0.6 + 0.6 over-subscribes the project, which is accepted");
}

#[test]
fn overflow_with_refuse_is_rejected_naming_the_project() {
    let json = r#"{
      "projects": [
        {
          "id": "acme",
          "budget": {
            "limit_usd": 10.0,
            "window": "total",
            "on_exhaustion": "refuse",
            "overflow_when_local_saturated": true
          }
        }
      ],
      "users": []
    }"#;
    let error = ControlPlaneConfig::from_json(json, "test").unwrap_err();
    let message = error.to_string();
    match error {
        ControlPlaneError::OverflowWithRefuse { entry, .. } => {
            assert_eq!(entry, "project `acme`");
        }
        other => panic!("expected OverflowWithRefuse, got {other:?}"),
    }
    assert!(
        message.contains("degrade-mode valve"),
        "the refusal has to say why refuse and overflow do not mix: {message}"
    );

    // The same rule with the flag set to false: still meaningless under
    // refuse, still rejected.
    let json = r#"{
      "projects": [
        {
          "id": "acme",
          "budget": {
            "limit_usd": 10.0,
            "window": "total",
            "on_exhaustion": "refuse",
            "overflow_when_local_saturated": false
          }
        }
      ],
      "users": []
    }"#;
    assert!(matches!(
        ControlPlaneConfig::from_json(json, "test").unwrap_err(),
        ControlPlaneError::OverflowWithRefuse { .. }
    ));

    // Control: refuse with no overflow field at all validates.
    let json = r#"{
      "projects": [
        {
          "id": "acme",
          "budget": {
            "limit_usd": 10.0,
            "window": "total",
            "on_exhaustion": "refuse"
          }
        }
      ],
      "users": []
    }"#;
    ControlPlaneConfig::from_json(json, "test")
        .expect("refuse with no overflow field is the ordinary spelling");
}

#[test]
fn an_absent_budget_resolves_to_unconstrained() {
    // No `"budget"` anywhere in the fixture: every resolved admission
    // carries `budget: None`.
    let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
    let admission = config
        .turn_keys
        .get(TURN_HASH)
        .expect("the fixture's one turn key");
    assert!(
        admission.budget.is_none(),
        "an absent project budget must resolve to no budget terms, not a \
         very large one"
    );

    // M2-compat: adding `budget`/`allocation` to the config shapes must
    // not move a single byte of what a config with neither already
    // resolved to. Principal and policy are exactly what M2 produced.
    assert_eq!(admission.principal, Principal::new("acme", "ada"));
    assert_eq!(*admission.policy, TurnPolicy::unrestricted());
}

/// A second, unrelated well-formed hash, distinct from every fixture
/// hash: `a_capped_and_a_share_key_resolve_to_their_ceilings` needs two
/// keys under one project.
const SHARE_HASH: &str = "075a9ac6b0608cfb207cb2f2e21e41b8e9771815b4403e0de138d18a240e0624";

#[test]
fn a_capped_and_a_share_key_resolve_to_their_ceilings() {
    let json = format!(
        r#"{{
          "projects": [
            {{
              "id": "acme",
              "budget": {{
                "limit_usd": 100.0,
                "window": "total",
                "on_exhaustion": "degrade_to_local"
              }}
            }}
          ],
          "users": [{{ "id": "cappy" }}, {{ "id": "sharey" }}],
          "keys": [
            {{
              "project": "acme",
              "user": "cappy",
              "key_sha256": "{TURN_HASH}",
              "allocation": {{ "capped": {{ "limit_usd": 30.0 }} }}
            }},
            {{
              "project": "acme",
              "user": "sharey",
              "key_sha256": "{SHARE_HASH}",
              "allocation": {{ "share": {{ "fraction": 0.25 }} }}
            }}
          ]
        }}"#
    );
    let config = ControlPlaneConfig::from_json(&json, "test").unwrap();

    let capped = config
        .turn_keys
        .get(TURN_HASH)
        .expect("cappy's key")
        .budget
        .as_ref()
        .expect("acme has a budget");
    assert_eq!(capped.allocation, Allocation::Capped { limit_usd: 30.0 });
    assert_eq!(capped.member_ceiling_usd(), Some(30.0));

    let shared = config
        .turn_keys
        .get(SHARE_HASH)
        .expect("sharey's key")
        .budget
        .as_ref()
        .expect("acme has a budget");
    assert_eq!(shared.allocation, Allocation::Share { fraction: 0.25 });
    assert_eq!(
        shared.member_ceiling_usd(),
        Some(25.0),
        "a quarter of the $100 project limit"
    );

    // Control: a key with no `allocation` on the same budgeted project
    // resolves to `Pooled` -- a budget, but no second ceiling.
    let pooled_json = format!(
        r#"{{
          "projects": [
            {{
              "id": "acme",
              "budget": {{
                "limit_usd": 100.0,
                "window": "total",
                "on_exhaustion": "degrade_to_local"
              }}
            }}
          ],
          "users": [{{ "id": "ada" }}],
          "keys": [{{ "project": "acme", "user": "ada", "key_sha256": "{TURN_HASH}" }}]
        }}"#
    );
    let config = ControlPlaneConfig::from_json(&pooled_json, "test").unwrap();
    let pooled = config
        .turn_keys
        .get(TURN_HASH)
        .unwrap()
        .budget
        .as_ref()
        .unwrap();
    assert_eq!(pooled.allocation, Allocation::Pooled);
    assert_eq!(pooled.member_ceiling_usd(), None);
}

#[test]
fn every_declared_key_is_in_the_table_the_resolver_runs_on() {
    // The invariant that used to be a re-join in `ControlPlane::configured`
    // guarded by `unwrap_or_else(TurnPolicy::unrestricted)`. There is no
    // second lookup now, so the claim is stated where it is established:
    // one table entry per declared key, built as the key was judged.
    let config = ControlPlaneConfig::from_json(sample_config(), "test").unwrap();
    assert_eq!(config.turn_keys.len(), config.keys.len());
    for key in &config.keys {
        let admission = config
            .turn_keys
            .get(&key.key_sha256)
            .unwrap_or_else(|| panic!("key `{}` reached no table entry", key.key_sha256));
        assert_eq!(
            admission.principal,
            Principal::new(key.project.as_str(), key.user.as_str())
        );
    }
}

// -----------------------------------------------------------------------
// Decision 9: policy on project entries, overrides on key entries
// -----------------------------------------------------------------------

#[test]
fn a_min_quality_outside_the_unit_interval_is_rejected_naming_the_project() {
    for min_quality in [-0.1, 1.1] {
        let json = format!(
            r#"{{
              "projects": [
                {{ "id": "acme", "policy": {{ "min_quality": {min_quality} }} }}
              ],
              "users": []
            }}"#
        );
        let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
        match error {
            ControlPlaneError::MinQualityOutOfRange {
                entry,
                min_quality: got,
                ..
            } => {
                assert_eq!(entry, "project `acme`");
                assert_eq!(got, min_quality);
            }
            other => panic!("expected MinQualityOutOfRange, got {other:?}"),
        }
    }

    // Control: the bounds themselves are inside the interval and validate.
    let json = r#"{
      "projects": [{ "id": "acme", "policy": { "min_quality": 0.0 } }],
      "users": []
    }"#;
    ControlPlaneConfig::from_json(json, "test").expect("0.0 is inside 0.0..=1.0");
    let json = r#"{
      "projects": [{ "id": "acme", "policy": { "min_quality": 1.0 } }],
      "users": []
    }"#;
    ControlPlaneConfig::from_json(json, "test").expect("1.0 is inside 0.0..=1.0");
}

#[test]
fn a_cadence_with_zero_per_turns_or_excess_max_frontier_is_rejected() {
    let zero_window = r#"{
      "projects": [
        {
          "id": "acme",
          "policy": { "frontier_cadence": { "max_frontier": 0, "per_turns": 0 } }
        }
      ],
      "users": []
    }"#;
    let error = ControlPlaneConfig::from_json(zero_window, "test").unwrap_err();
    match error {
        ControlPlaneError::CadencePerTurnsZero { entry, .. } => {
            assert_eq!(entry, "project `acme`");
        }
        other => panic!("expected CadencePerTurnsZero, got {other:?}"),
    }

    let excess = r#"{
      "projects": [
        {
          "id": "acme",
          "policy": { "frontier_cadence": { "max_frontier": 5, "per_turns": 2 } }
        }
      ],
      "users": []
    }"#;
    let error = ControlPlaneConfig::from_json(excess, "test").unwrap_err();
    match error {
        ControlPlaneError::CadenceExceedsWindow {
            entry,
            max_frontier,
            per_turns,
            ..
        } => {
            assert_eq!(entry, "project `acme`");
            assert_eq!(max_frontier, 5);
            assert_eq!(per_turns, 2);
        }
        other => panic!("expected CadenceExceedsWindow, got {other:?}"),
    }

    // Control: `max_frontier == per_turns` is the bound, not past it.
    let boundary = r#"{
      "projects": [
        {
          "id": "acme",
          "policy": { "frontier_cadence": { "max_frontier": 3, "per_turns": 3 } }
        }
      ],
      "users": []
    }"#;
    ControlPlaneConfig::from_json(boundary, "test")
        .expect("max_frontier == per_turns is exactly at the bound");
}

#[test]
fn a_cadence_that_rations_nothing_is_refused_and_pointed_at_the_allow_list() {
    // `max_frontier: 0` is a filter spelled as a cadence: it forbids every
    // hosted target on every turn. Refusing it is what lets the two
    // history-independent axes (`permits`) be the whole of what a startup
    // check and a candidate-set filter need to ask -- with a zero ration
    // accepted, "is this candidate reachable at all" and "is it reachable
    // this turn" would have different answers for a policy that never
    // reaches it.
    let json = r#"{
      "projects": [
        {
          "id": "acme",
          "policy": { "frontier_cadence": { "max_frontier": 0, "per_turns": 10 } }
        }
      ],
      "users": []
    }"#;
    let error = ControlPlaneConfig::from_json(json, "test").unwrap_err();
    let message = error.to_string();
    match error {
        ControlPlaneError::CadenceRationsNothing { entry, .. } => {
            assert_eq!(entry, "project `acme`");
        }
        other => panic!("expected CadenceRationsNothing, got {other:?}"),
    }
    assert!(
        message.contains(r#""allow": ["local/*"]"#),
        "the refusal has to say what to write instead: {message}"
    );

    // The same rule on the overrides half of the format.
    let key_side = format!(
        r#"{{
          "projects": [{{ "id": "acme" }}],
          "users": [{{ "id": "ada" }}],
          "keys": [{{
            "project": "acme",
            "user": "ada",
            "key_sha256": "{TURN_HASH}",
            "overrides": {{ "frontier_cadence": {{ "max_frontier": 0, "per_turns": 4 }} }}
          }}]
        }}"#
    );
    match ControlPlaneConfig::from_json(&key_side, "test").unwrap_err() {
        ControlPlaneError::CadenceRationsNothing { entry, .. } => {
            assert_eq!(entry, "key for project `acme`, user `ada`");
        }
        other => panic!("expected CadenceRationsNothing, got {other:?}"),
    }

    // Control: one dispatch per window is the smallest real ration and
    // validates.
    let smallest = r#"{
      "projects": [
        {
          "id": "acme",
          "policy": { "frontier_cadence": { "max_frontier": 1, "per_turns": 10 } }
        }
      ],
      "users": []
    }"#;
    ControlPlaneConfig::from_json(smallest, "test")
        .expect("one per ten is a cadence, not an allow-list");
}

#[test]
fn a_misspelled_field_inside_frontier_cadence_is_refused_rather_than_ignored() {
    // `PolicyConfig` carries `deny_unknown_fields`, and serde does not
    // recurse: the attribute guards the three axes and nothing inside
    // them. So a stale or misspelled key left inside `frontier_cadence`
    // was accepted and dropped, and the operator got the cadence they did
    // not mean with no indication that a line of their file had been
    // ignored -- the exact failure `deny_unknown_fields` is on
    // `PolicyConfig` to prevent, one level down.
    let json = r#"{
      "projects": [
        {
          "id": "acme",
          "policy": {
            "frontier_cadence": { "max_frontier": 1, "per_turns": 10, "per_turn": 3 }
          }
        }
      ],
      "users": []
    }"#;
    let error = ControlPlaneConfig::from_json(json, "test").unwrap_err();
    assert!(
        matches!(error, ControlPlaneError::Parse { .. }),
        "a field nobody reads must stop the load: {error:?}"
    );
    assert!(
        error.to_string().contains("per_turn"),
        "and name the line to delete: {error}"
    );

    // Control: the same object without the stray key loads.
    let clean = r#"{
      "projects": [
        {
          "id": "acme",
          "policy": { "frontier_cadence": { "max_frontier": 1, "per_turns": 10 } }
        }
      ],
      "users": []
    }"#;
    ControlPlaneConfig::from_json(clean, "test").expect("the two real fields are enough");
}

#[test]
fn a_malformed_glob_is_rejected_naming_the_entry() {
    let project_glob = r#"{
      "projects": [{ "id": "acme", "policy": { "allow": ["anthropic/**"] } }],
      "users": []
    }"#;
    let error = ControlPlaneConfig::from_json(project_glob, "test").unwrap_err();
    match error {
        ControlPlaneError::MalformedGlob { entry, .. } => {
            assert_eq!(entry, "project `acme`");
        }
        other => panic!("expected MalformedGlob, got {other:?}"),
    }

    let key_glob = format!(
        r#"{{
          "projects": [{{ "id": "acme" }}],
          "users": [{{ "id": "ada" }}],
          "keys": [{{
            "project": "acme",
            "user": "ada",
            "key_sha256": "{TURN_HASH}",
            "overrides": {{ "allow": ["anthropic/{{a,b}}"] }}
          }}]
        }}"#
    );
    let error = ControlPlaneConfig::from_json(&key_glob, "test").unwrap_err();
    match error {
        ControlPlaneError::MalformedGlob { entry, .. } => {
            assert_eq!(entry, "key for project `acme`, user `ada`");
        }
        other => panic!("expected MalformedGlob, got {other:?}"),
    }
}

#[test]
fn an_override_wider_than_the_project_policy_is_rejected_naming_both() {
    let json = format!(
        r#"{{
          "projects": [
            {{ "id": "acme", "policy": {{ "min_quality": 0.7 }} }}
          ],
          "users": [{{ "id": "ada" }}],
          "keys": [{{
            "project": "acme",
            "user": "ada",
            "key_sha256": "{TURN_HASH}",
            "overrides": {{ "min_quality": 0.3 }}
          }}]
        }}"#
    );
    let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
    match error {
        ControlPlaneError::OverrideWiderThanProject {
            project_entry,
            key_entry,
            axes,
            ..
        } => {
            assert_eq!(project_entry, "project `acme`");
            assert_eq!(key_entry, "key for project `acme`, user `ada`");
            assert_eq!(axes, "min_quality");
        }
        other => panic!("expected OverrideWiderThanProject, got {other:?}"),
    }

    // Two axes at once read as prose rather than as a debug-printed Vec:
    // the operator is being told which field names to go and edit.
    let both = format!(
        r#"{{
          "projects": [
            {{
              "id": "acme",
              "policy": {{
                "min_quality": 0.7,
                "frontier_cadence": {{ "max_frontier": 1, "per_turns": 4 }}
              }}
            }}
          ],
          "users": [{{ "id": "ada" }}],
          "keys": [{{
            "project": "acme",
            "user": "ada",
            "key_sha256": "{TURN_HASH}",
            "overrides": {{
              "min_quality": 0.3,
              "frontier_cadence": {{ "max_frontier": 3, "per_turns": 4 }}
            }}
          }}]
        }}"#
    );
    let message = ControlPlaneConfig::from_json(&both, "test")
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("on min_quality, frontier_cadence --"),
        "{message}"
    );

    // Control: an override that only tightens validates.
    let json = format!(
        r#"{{
          "projects": [
            {{ "id": "acme", "policy": {{ "min_quality": 0.5 }} }}
          ],
          "users": [{{ "id": "ada" }}],
          "keys": [{{
            "project": "acme",
            "user": "ada",
            "key_sha256": "{TURN_HASH}",
            "overrides": {{ "min_quality": 0.8 }}
          }}]
        }}"#
    );
    ControlPlaneConfig::from_json(&json, "test")
        .expect("an override that only raises the floor narrows and must validate");
}
