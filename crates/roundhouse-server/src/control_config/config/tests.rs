// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
// Named here rather than reached for through `super::*`: the validator resolves
// an allocation through `budget_terms` and no longer mentions the type itself,
// and a test that borrowed its parent's imports would go on compiling only
// until the production file stopped needing them.
use roundhouse_core::control::Allocation;
// The same argument, for M10.2's recipe assertions: the validator hands out an
// `Arc<TierRecipe>` and never names `Tier`, `PickerMode` or `Target` itself.
use roundhouse_core::routing::{PickerMode, Target, Tier};
use std::sync::Arc;

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

/// **The file fingerprint is a hash of the bytes on disk** (M16.1, R-D9), and
/// this is the assertion that keeps it from quietly becoming a hash of
/// something else.
///
/// Checked against a digest computed here, from the same bytes, rather than
/// against a hard-coded hex string: a pinned literal over the shipped example
/// would fail the day somebody fixed a comma in it, and would be pinning the
/// example rather than the function. What must hold is that the digest
/// `load_fingerprinted` answers is the digest of what it read — because the
/// whole divergence check is two nodes comparing this number, and a hash of
/// the *parsed* config would call two visibly different files the same.
#[test]
fn the_file_fingerprint_is_the_sha256_of_the_bytes_that_were_parsed() {
    use sha2::{Digest, Sha256};

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/control-plane.example.json");
    let (_, sha256) = ControlPlaneConfig::load_fingerprinted(&path)
        .unwrap_or_else(|error| panic!("the shipped example must validate: {error}"));
    let bytes = std::fs::read(&path).expect("the example is readable");
    assert_eq!(sha256, hex::encode(Sha256::digest(&bytes)));
    assert_eq!(sha256.len(), 64, "hex, lowercase, unprefixed: {sha256}");

    // And `load` is the same read with the digest dropped, rather than a
    // second path through the file: two loaders would be two chances to
    // disagree about what the file says.
    let plain = ControlPlaneConfig::load(&path).expect("the shipped example must validate");
    assert_eq!(plain.projects.len(), {
        let (config, _) = ControlPlaneConfig::load_fingerprinted(&path).unwrap();
        config.projects.len()
    });
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

    // M10.2's config addition, read off the **resolved** `turn_keys` table for
    // the reason the ceilings above are: the shape parsing is the cheap half,
    // and what an operator copying this file actually gets is what
    // `TierRecipe::new` accepted and what every key of the project resolved to.
    // A recipe that parsed and then failed to reach a membership would be an
    // example that demonstrates nothing.
    let recipes: Vec<_> = config
        .turn_keys
        .values()
        .map(|admission| admission.tiers.as_ref().map(Arc::clone))
        .collect();
    assert!(
        recipes.iter().all(Option::is_some),
        "every key of the only project in the example belongs to a project with \
         a recipe, so every membership must resolve to one: {recipes:?}"
    );
    let recipe = recipes[0].as_ref().expect("checked present above");
    assert_eq!(
        recipe.list(Tier::Capable).len(),
        2,
        "the example must demonstrate the within-tier fallback order, which \
         needs a tier with somewhere to fall to"
    );
    assert_eq!(
        recipe.picker(),
        PickerMode::EfficientFirst,
        "the shipped example must sit on the operating point that has been \
         calibrated, not on the one the process warns about"
    );

    // And every identity the recipe names is one the project's own `allow`
    // admits. A recipe can only narrow, so an entry the policy refuses is
    // skipped in silence at routing time — which in a *worked example* would be
    // a line an operator copies believing it does something.
    let allow = &config
        .turn_keys
        .values()
        .next()
        .expect("the example ships keys")
        .policy
        .allow;
    for tier in [Tier::Capable, Tier::Efficient] {
        for named in recipe.list(tier) {
            let target = match named.strip_prefix("local/") {
                Some(model) => Target::Local {
                    worker_id: 0,
                    dp_rank: 0,
                    model: model.to_string(),
                },
                None => {
                    let (provider, model) = named
                        .split_once('/')
                        .unwrap_or_else(|| panic!("`{named}` is not a `provider/model` identity"));
                    Target::Frontier {
                        provider: provider.to_string(),
                        model: model.to_string(),
                    }
                }
            };
            assert!(
                allow.matches(&target),
                "the example's recipe names `{named}`, which acme's own `allow` \
                 does not admit: the entry would be skipped at routing time and \
                 the example would be demonstrating nothing"
            );
        }
    }
}

/// Review finding G07, ruled on a corrected mechanism — see
/// `reports/m10-fix-C.md`, and the rename from
/// `..._is_one_this_binary_could_quote`, which named a property the shipped
/// binary cannot have.
///
/// **The finding's question.** The shipped `roundhouse` binary attaches no
/// [`LocalFleet`], so `local/<model>` names nothing `reachable_candidates`
/// (main.rs) ever quotes, and the check above only asks whether acme's `allow`
/// *admits* each recipe entry — which `local/REPLACE-with-your-local-model`
/// passes, the pattern being `local/*`. True, and its predicted consequence —
/// every turn falling silently to the expensive tier — is not: the same file's
/// `frontier_cadence` promises local service on a spent window, and
/// [`refuse_promises_of_a_local_fallback`] refuses the whole plane at boot
/// before a socket is opened. A fleetless operator meets a refusal naming the
/// missing local capacity, not a quiet bill.
///
/// **What was actually broken, and what this test guards.** On the deployment
/// these two files *describe* — one whose fleet serves the local model the
/// catalog example declares in `local_quality` — ada's key was still refused,
/// because her `min_quality: 0.8` override put that model's declared 0.62
/// below her floor and so took her own cadence's fallback away. The shipped
/// pair booted nothing at all, fleeted or fleetless, and nothing said so. Both
/// halves are asserted here: the pair boots the deployment it describes, and
/// the fleetless refusal is the documented one rather than silence.
///
/// The local candidate is built from the catalog example's own `local_quality`
/// rather than hard-coded, so the two files' spelling of the local model is
/// what is being compared. That is the shape the finding's silent-expensive-tier
/// scenario really has: a typo between the files, on a deployment with a fleet.
///
/// [`LocalFleet`]: roundhouse_fleet::LocalFleet
/// [`refuse_promises_of_a_local_fallback`]: crate::control_config::crosscheck::refuse_promises_of_a_local_fallback
#[test]
fn every_target_the_examples_recipe_names_is_one_the_deployment_it_describes_can_quote() {
    let control_plane_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/control-plane.example.json");
    let config = ControlPlaneConfig::load(&control_plane_path)
        .unwrap_or_else(|error| panic!("the shipped example must validate: {error}"));

    let catalog_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/catalog.example.json");
    let catalog_config = crate::CatalogConfig::load(&catalog_path)
        .unwrap_or_else(|error| panic!("the shipped catalog example must validate: {error}"));
    let catalog = catalog_config.catalog();

    // Same recipe for the same reason `the_example_file_validates` above
    // handled multiplicity -- this milestone's example ships one project.
    let recipe = config
        .turn_keys
        .values()
        .next()
        .and_then(|admission| admission.tiers.as_ref())
        .expect("the example ships a key with a recipe")
        .clone();
    let plane = crate::ControlPlane::configured(config);

    // `reachable_candidates` in main.rs, reproduced here rather than called:
    // that function lives in the `roundhouse` binary crate and is not
    // reachable from this lib crate's tests.
    let mut ledger = roundhouse_core::routing::CacheLedger::new();
    catalog.apply_to_ledger(&mut ledger);
    let fleetless = catalog.quote(&ledger, roundhouse_core::now_ms(), 1_024, 256);

    // The deployment the two files describe: the catalog's hosted entries plus
    // a worker for every local model the catalog itself declares a quality for.
    // Quoted at that declared prior, because the floor a key's override sets is
    // compared against exactly this number and the whole defect lived in that
    // comparison.
    let mut described = fleetless.clone();
    for (model, prior) in &catalog_config.local_quality {
        described.push(roundhouse_core::routing::Candidate {
            target: Target::Local {
                worker_id: 1,
                dp_rank: 0,
                model: model.clone(),
            },
            expected_prefill_tokens: 1_024.0,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 60.0,
            expected_cost_usd: 0.0,
            quality_prior: *prior,
            load: Some(0.0),
        });
    }
    let described_identities: HashSet<String> = described
        .iter()
        .map(|candidate| candidate.target.policy_identity())
        .collect();

    for tier in [Tier::Capable, Tier::Efficient] {
        for named in recipe.list(tier) {
            assert!(
                described_identities.contains(named),
                "the example's {tier:?} tier names `{named}`, which neither file's own \
                 contents can produce a candidate for ({described_identities:?}): a hosted \
                 entry the catalog does not price, or a local model it declares no quality \
                 for, is a tier that is empty on every turn -- and an empty tier is not a \
                 failure, it is the other tier serving the turn at another price"
            );
        }
    }

    // And the deployment described boots: every key, not just the project's,
    // since a turn arrives on a key and ada's own narrowing is where this broke.
    crate::control_config::crosscheck::CrossChecks::new(described, None)
        .refuse(&plane)
        .unwrap_or_else(|refusal| {
            panic!(
                "the two shipped examples must describe a deployment that starts: {} said {}",
                refusal.check, refusal.detail
            )
        });

    // CONTROL, and the half that makes the assertion above non-vacuous: the
    // same plane on a *fleetless* process -- the shipped binary's own wiring --
    // is refused, and refused by name. If this ever passed, the examples would
    // be describing a deployment nobody can tell apart from the one they get,
    // and the finding's silent-expensive-tier scenario would be back.
    let refusal = crate::control_config::crosscheck::CrossChecks::new(fleetless, None)
        .refuse(&plane)
        .expect_err(
            "the shipped control plane promises local service and the shipped binary \
             attaches no fleet; a process that started here would serve the promise's \
             opposite in silence",
        );
    assert!(
        refusal.detail.contains("no local capacity") && refusal.detail.contains("project `acme`"),
        "the fleetless refusal is what an operator copying both files actually meets, so \
         it has to name the capacity and the keys rather than the tier that went empty: {}",
        refusal.detail
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
fn a_misspelled_top_level_field_on_a_project_is_refused_rather_than_ignored() {
    let json = r#"{
      "projects": [
        { "id": "acme", "credential": { "mode": "pass_through" } }
      ],
      "users": []
    }"#;
    let error = ControlPlaneConfig::from_json(json, "test").unwrap_err();
    assert!(
        matches!(error, ControlPlaneError::Parse { .. }),
        "a field nobody reads must stop the load, the same way it does one level down inside \
         `frontier_cadence`: {error:?}"
    );
    assert!(
        error.to_string().contains("credential"),
        "and name the line to delete: {error}"
    );
}

/// Control for the test above: the same entry, correctly spelled, loads.
/// Proves the refusal is about the missing field name and not about some
/// unrelated malformed-JSON accident in the fixture.
#[test]
fn the_correctly_spelled_project_credentials_field_still_loads() {
    let json = r#"{
      "projects": [
        { "id": "acme", "credentials": { "mode": "pass_through" } }
      ],
      "users": []
    }"#;
    ControlPlaneConfig::from_json(json, "test").expect("the real field name is enough");
}

#[test]
fn a_misspelled_top_level_field_on_a_user_is_refused_rather_than_ignored() {
    let json = r#"{
      "projects": [],
      "users": [
        { "id": "ada", "nmae": "Ada" }
      ]
    }"#;
    let error = ControlPlaneConfig::from_json(json, "test").unwrap_err();
    assert!(
        matches!(error, ControlPlaneError::Parse { .. }),
        "a field nobody reads must stop the load: {error:?}"
    );
    assert!(
        error.to_string().contains("nmae"),
        "and name the line to delete: {error}"
    );
}

/// Control for the test above: the same entry with no stray field loads.
#[test]
fn a_plain_user_entry_still_loads() {
    let json = r#"{
      "projects": [],
      "users": [
        { "id": "ada" }
      ]
    }"#;
    ControlPlaneConfig::from_json(json, "test").expect("a bare id is a complete user entry");
}

/// The third entry type, found by the R6 audit rather than named by it, and
/// the one whose typo widens the most: `override` for `overrides` drops a
/// *narrowing* overlay, so the key resolves to its project's whole policy —
/// which is indistinguishable, everywhere downstream, from an operator who
/// wrote no overlay at all.
#[test]
fn a_misspelled_top_level_field_on_a_key_is_refused_rather_than_ignored() {
    let json = format!(
        r#"{{
          "projects": [{{ "id": "acme" }}],
          "users": [{{ "id": "ada" }}],
          "keys": [{{
            "project": "acme",
            "user": "ada",
            "key_sha256": "{TURN_HASH}",
            "override": {{ "min_quality": 0.9 }}
          }}]
        }}"#
    );
    let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
    assert!(
        matches!(error, ControlPlaneError::Parse { .. }),
        "a narrowing overlay lost to a typo is the widest reading of the entry: {error:?}"
    );
    assert!(
        error.to_string().contains("override"),
        "and name the line to correct: {error}"
    );

    // Control: the same entry with the real field name loads, so the refusal
    // above is about the spelling and not about overlays being unwelcome here.
    let spelled = format!(
        r#"{{
          "projects": [{{ "id": "acme" }}],
          "users": [{{ "id": "ada" }}],
          "keys": [{{
            "project": "acme",
            "user": "ada",
            "key_sha256": "{TURN_HASH}",
            "overrides": {{ "min_quality": 0.9 }}
          }}]
        }}"#
    );
    ControlPlaneConfig::from_json(&spelled, "test").expect("the real field name is enough");
}

/// The document itself, not an entry inside it — where the widening typos are
/// worst, because `admin_key` for `admin_keys` is a deployment whose admin
/// plane has no root of trust and which starts anyway.
#[test]
fn a_misspelled_top_level_field_on_the_document_is_refused_rather_than_ignored() {
    let json = format!(
        r#"{{
          "projects": [],
          "users": [],
          "admin_key": ["{TURN_HASH}"]
        }}"#
    );
    let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
    assert!(
        matches!(error, ControlPlaneError::Parse { .. }),
        "a deployment with no admin key must not start believing it has one: {error:?}"
    );
    assert!(
        error.to_string().contains("admin_key"),
        "and name the line to correct: {error}"
    );

    let spelled = format!(
        r#"{{
          "projects": [],
          "users": [],
          "admin_keys": ["{TURN_HASH}"]
        }}"#
    );
    ControlPlaneConfig::from_json(&spelled, "test").expect("the real field name is enough");
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

/// M12 review F2: the retired `mcp_namespace` knob is refused, whatever it says.
///
/// Every shape an operator could write, including the one that used to be
/// accepted (`mcp__acme`, well-formed and dispatchable), because the finding is
/// not that some namespaces are bad — it is that *no* configured namespace ever
/// reached a launcher, the signage or the fold, and a config that loads while
/// meaning nothing is how an operator loses an afternoon.
#[test]
fn the_retired_mcp_namespace_knob_is_refused_at_load() {
    for namespace in ["mcp__acme", "", "mcp roundhouse", "mcp__roundhouse"] {
        let json = format!(
            r#"{{
              "projects": [{{ "id": "acme" }}],
              "users": [{{ "id": "ada" }}],
              "mcp_namespace": {namespace:?}
            }}"#
        );
        let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
        match error {
            ControlPlaneError::RetiredMcpNamespace {
                namespace: rejected,
                ..
            } => assert_eq!(rejected, namespace),
            other => panic!("namespace {namespace:?} should have been refused, got {other:?}"),
        }
    }
}

/// The control for the rule above: a file that names no namespace still loads.
///
/// Without it, a boundary that refused every config would leave the test above
/// green while making the whole file unusable.
#[test]
fn a_config_that_names_no_namespace_still_loads() {
    let unnamed =
        ControlPlaneConfig::from_json(sample_config(), "test").expect("the sample config loads");
    assert_eq!(
        unnamed.mcp_namespace, None,
        "the field survives only so the refusal can name it"
    );
}

/// M10.2 S3: a project's `tiers` block reaches the admission its keys resolve
/// to, and a key inherits its project's recipe rather than carrying one.
#[test]
fn a_projects_tier_recipe_reaches_every_key_of_that_project() {
    let json = format!(
        r#"{{
          "projects": [
            {{
              "id": "acme",
              "tiers": {{
                "capable": ["openrouter/moonshotai/kimi-k3", "openai/gpt-5.6-sol"],
                "efficient": ["openai/gpt-5.6-luna"],
                "picker": "capable_first",
                "confidence_threshold": 0.4
              }}
            }},
            {{ "id": "other" }}
          ],
          "users": [{{ "id": "ada" }}],
          "keys": [
            {{ "project": "acme", "user": "ada", "key_sha256": "{TURN_HASH}" }}
          ]
        }}"#
    );
    let config = ControlPlaneConfig::from_json(&json, "test").expect("a well-formed recipe loads");
    let admission = config
        .turn_keys
        .get(TURN_HASH)
        .expect("the key resolved to an admission");
    let recipe = admission
        .tiers
        .as_ref()
        .expect("the project declared a recipe");

    assert_eq!(recipe.picker(), PickerMode::CapableFirst);
    assert!((recipe.confidence_threshold() - 0.4).abs() < 1e-12);
    assert_eq!(
        recipe.list(roundhouse_core::routing::Tier::Capable),
        [
            "openrouter/moonshotai/kimi-k3".to_string(),
            "openai/gpt-5.6-sol".to_string()
        ],
        "in the operator's order, which is what the failover walks"
    );
    assert_eq!(
        recipe.list(roundhouse_core::routing::Tier::Efficient),
        ["openai/gpt-5.6-luna".to_string()]
    );

    // The control, and it is the compatibility guarantee: a project that wrote
    // no block resolves to no recipe, so its turns route exactly as they did
    // before M10.
    let json = format!(
        r#"{{
          "projects": [{{ "id": "acme" }}],
          "users": [{{ "id": "ada" }}],
          "keys": [
            {{ "project": "acme", "user": "ada", "key_sha256": "{TURN_HASH}" }}
          ]
        }}"#
    );
    let config = ControlPlaneConfig::from_json(&json, "test").unwrap();
    assert!(config.turn_keys.get(TURN_HASH).unwrap().tiers.is_none());
}

/// A threshold no confidence can reach stops the boot, naming the project.
///
/// Upstream refuses it at construction and so does this: an operator watching a
/// start-up can fix a number, and a deployment that discovered it on turn four
/// hundred would have spent four hundred turns on the picker's default with
/// nothing saying why.
#[test]
fn a_tier_recipe_with_an_unreachable_threshold_stops_the_boot() {
    for threshold in [-0.5, 1.5] {
        let json = format!(
            r#"{{
              "projects": [
                {{
                  "id": "acme",
                  "tiers": {{
                    "capable": ["openai/sol"],
                    "confidence_threshold": {threshold}
                  }}
                }}
              ],
              "users": []
            }}"#
        );
        match ControlPlaneConfig::from_json(&json, "test").unwrap_err() {
            ControlPlaneError::TierRecipeRejected { entry, source, .. } => {
                assert_eq!(entry, "project `acme`");
                assert!(
                    matches!(
                        source,
                        roundhouse_core::routing::TierRecipeError::ThresholdOutOfRange { .. }
                    ),
                    "{source}"
                );
            }
            other => panic!("expected TierRecipeRejected, got {other:?}"),
        }
    }

    // A recipe that names nothing in either tier is refused too: there is
    // nothing for the scorer to pick between, and an empty recipe would route
    // every turn through the no-viable-candidate refusal.
    let json = r#"{
      "projects": [{ "id": "acme", "tiers": { "capable": [], "efficient": [] } }],
      "users": []
    }"#;
    assert!(matches!(
        ControlPlaneConfig::from_json(json, "test").unwrap_err(),
        ControlPlaneError::TierRecipeRejected { .. }
    ));

    // Control: the bounds themselves are inside the interval, and one-sided
    // recipes are legitimate.
    let json = r#"{
      "projects": [
        { "id": "a", "tiers": { "capable": ["openai/sol"], "confidence_threshold": 0.0 } },
        { "id": "b", "tiers": { "efficient": ["openai/luna"], "confidence_threshold": 1.0 } }
      ],
      "users": []
    }"#;
    ControlPlaneConfig::from_json(json, "test").expect("both bounds are valid thresholds");
}

/// A misspelt tier name is a boot refusal, not a tier that quietly routes
/// nothing.
#[test]
fn a_misspelt_tier_field_is_refused_rather_than_dropped() {
    let json = r#"{
      "projects": [{ "id": "acme", "tiers": { "capible": ["openai/sol"] } }],
      "users": []
    }"#;
    assert!(
        ControlPlaneConfig::from_json(json, "test").is_err(),
        "`capible` would have resolved to an empty capable tier, which is a \
         legitimate one-sided recipe and therefore invisible afterwards"
    );
}
