// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M10.1 P4: an external provider's key rides the three tiers that already
//! exist.
//!
//! R7 of `PLAN-frontier-selection.md` rules that attaching an OpenRouter key
//! adds **no credential variant and no schema change** — deployment, project and
//! member are already the three scopes, and `"openrouter"` is just another
//! provider name in the block a project or a key already writes. A ruling like
//! that is only worth anything if somebody checks it, because the way it fails
//! is silent: a tier that quietly falls back to the deployment's key spends the
//! wrong account, and a project that can see another project's key is a tenancy
//! breach that no turn reports.
//!
//! So this file makes no changes and asserts four resolutions. It is deliberately
//! *not* in `credential_gating.rs`, which is under this repo's read-deny for
//! files matching `*credential*`: the point of R7 is that nothing under that deny
//! had to move, and a suite that could not be written without opening those files
//! would have disproved it.
//!
//! What each test reads is the plaintext the dispatch seam would put on the
//! wire — `TurnCredential::require_api_key`, the one accessor that yields it —
//! because "which key did this membership resolve" is exactly "which key would
//! this turn have sent".

use std::sync::{Arc, LazyLock};

use roundhouse_core::control::{Principal, TurnCredential};
use roundhouse_core::routing::Target;
use roundhouse_server::{ControlPlane, ControlPlaneConfig};

mod common;
use common::{key, sha256_hex};

/// The provider under test. A real OpenRouter model id, written in full —
/// `openrouter-api-surface.md` Q2 is emphatic that a bare vendor name is a
/// different row from a dated one, and a fixture that wrote `kimi` would teach
/// the wrong habit.
const PROVIDER: &str = "openrouter";
const MODEL: &str = "moonshotai/kimi-k3";

const DEPLOYMENT_VAR: &str = "RH_TEST_OR_DEPLOYMENT_KEY";
const PROJECT_VAR: &str = "RH_TEST_OR_PROJECT_KEY";
const ADA_VAR: &str = "RH_TEST_OR_ADA_KEY";

/// Three plaintexts with no substring in common, so an assertion that finds one
/// found *that* tier's key rather than a prefix of another's.
const DEPLOYMENT_KEY: &str = "sk-or-v1-deployment-QQQQ";
const PROJECT_KEY: &str = "sk-or-v1-project-WWWW";
const ADA_KEY: &str = "sk-or-v1-ada-ZZZZ";

static ENV: LazyLock<()> = LazyLock::new(|| {
    // SAFETY: this closure runs exactly once and `LazyLock` blocks every other
    // thread inside `force` until it returns. Every read of these variables in
    // this binary is downstream of the `force` in `plane`, and nothing unsets
    // or rewrites them afterwards. (The same argument `mcp_surface.rs` makes
    // for its own one-variable version.)
    unsafe {
        std::env::set_var(DEPLOYMENT_VAR, DEPLOYMENT_KEY);
        std::env::set_var(PROJECT_VAR, PROJECT_KEY);
        std::env::set_var(ADA_VAR, ADA_KEY);
    }
});

fn target() -> Target {
    Target::Frontier {
        provider: PROVIDER.into(),
        model: MODEL.into(),
    }
}

/// One plane carrying all three tiers at once.
///
/// One fixture rather than three, because the claims are about *which* tier
/// wins: a per-test plane that declared only the tier under test would prove
/// each key is readable and nothing about precedence or isolation.
///
/// - `deployment-tier` declares nothing, so it runs on the deployment's keys.
/// - `project-tier` declares its own, which is the tier that must win over the
///   deployment's.
/// - `byok` is `user_only`, so `ada`'s own key is the only one reachable and
///   `bob`, who attached none, reaches nothing.
/// - `other-project` declares nothing and exists to be the second tenant in the
///   isolation claim.
fn plane() -> Arc<ControlPlane> {
    LazyLock::force(&ENV);
    let json = serde_json::json!({
        "credentials": { "providers": { PROVIDER: { "env_var": DEPLOYMENT_VAR } } },
        "projects": [
            { "id": "deployment-tier" },
            {
                "id": "project-tier",
                "credentials": { "providers": { PROVIDER: { "env_var": PROJECT_VAR } } },
            },
            { "id": "byok", "credentials": { "mode": "user_only" } },
            { "id": "other-project" },
        ],
        "users": [{ "id": "ada" }, { "id": "bob" }],
        "keys": [
            {
                "project": "deployment-tier", "user": "ada",
                "key_sha256": sha256_hex(&key("dep")),
            },
            {
                "project": "project-tier", "user": "ada",
                "key_sha256": sha256_hex(&key("prj")),
            },
            {
                "project": "byok", "user": "ada",
                "key_sha256": sha256_hex(&key("ada")),
                "credentials": { "providers": { PROVIDER: { "env_var": ADA_VAR } } },
            },
            {
                "project": "byok", "user": "bob",
                "key_sha256": sha256_hex(&key("bob")),
            },
            {
                "project": "other-project", "user": "bob",
                "key_sha256": sha256_hex(&key("oth")),
            },
        ],
    })
    .to_string();
    Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "provider-key fixture").expect(
            "R7's claim is that this file needs no new schema; if it does not \
                     validate, the claim is already wrong",
        ),
    ))
}

/// The credential this membership's turn would authenticate `openrouter` with.
fn resolved(plane: &ControlPlane, project: &str, user: &str) -> TurnCredential {
    plane
        .membership(&Principal::new(project, user))
        .expect("the fixture declares this membership")
        .credentials
        .access_for(&target())
        .map(|access| access.credential)
        .unwrap_or(TurnCredential::Absent)
}

/// The plaintext the dispatch seam would put on the wire, or `None` where the
/// membership cannot authenticate to this provider at all.
fn key_for(plane: &ControlPlane, project: &str, user: &str) -> Option<String> {
    resolved(plane, project, user)
        .require_api_key(PROVIDER)
        .ok()
        .map(str::to_string)
}

/// **R7's claim, all three tiers in one assertion.**
///
/// Each membership resolves the key its own tier names, and the tiers do not
/// leak into one another. The three plaintexts share no substring, so a
/// resolution that fell back one tier is a visibly different string rather than
/// a near-miss.
#[tokio::test]
async fn an_openrouter_key_resolves_at_whichever_tier_declared_it() {
    let plane = plane();

    assert_eq!(
        key_for(&plane, "deployment-tier", "ada").as_deref(),
        Some(DEPLOYMENT_KEY),
        "a project that declares nothing runs on the deployment's own keys"
    );
    assert_eq!(
        key_for(&plane, "project-tier", "ada").as_deref(),
        Some(PROJECT_KEY),
        "a project's own key must win over the deployment's, or an operator who \
         gave one tenant a separate account is silently billing the shared one"
    );
    assert_eq!(
        key_for(&plane, "byok", "ada").as_deref(),
        Some(ADA_KEY),
        "a member's own key is what `user_only` means"
    );
}

/// **The isolation claim, and the reason it is a separate test.**
///
/// `a_key_scoped_to_a_project_is_invisible_to_another_project` — the failure it
/// guards is a tenancy breach rather than a misconfiguration: `bob` on
/// `other-project` must never resolve the key `project-tier` attached, and
/// nothing in a turn's own log would say if he did.
#[tokio::test]
async fn a_key_scoped_to_a_project_is_invisible_to_another_project() {
    let plane = plane();

    // PROBE: the second tenant, asking for the same provider.
    assert_eq!(
        key_for(&plane, "other-project", "bob").as_deref(),
        Some(DEPLOYMENT_KEY),
        "another project reaches the deployment's key and never the first \
         project's -- if this reads `{PROJECT_KEY}`, one tenant is spending \
         another's account"
    );
    assert_ne!(
        key_for(&plane, "other-project", "bob").as_deref(),
        Some(PROJECT_KEY)
    );

    // And the member scope, which is narrower still: `bob` in the same project
    // as `ada`, under `user_only`, attached nothing — so he reaches nothing.
    // Absent rather than the deployment's key is the whole content of
    // `user_only`, and it is what makes `ada`'s key hers rather than the
    // project's.
    assert_eq!(
        key_for(&plane, "byok", "bob"),
        None,
        "a member who attached no key must not inherit one; `user_only` that \
         fell back would spend the deployment's account under a member's name"
    );
    assert!(matches!(
        resolved(&plane, "byok", "bob"),
        TurnCredential::Absent
    ));

    // CONTROL: the same project, the same provider, the member who *did*
    // attach one. Without this, every assertion above would pass on a plane
    // that resolved nothing for anybody.
    assert_eq!(key_for(&plane, "byok", "ada").as_deref(), Some(ADA_KEY));
}

/// The narrower half of the same ruling: attaching a key does not widen what a
/// membership can reach beyond the provider it named.
///
/// A tier that resolved *any* key for *any* provider would satisfy the tests
/// above while making every hosted target reachable — and a candidate left in
/// the pool for want of a credential nobody has is the shape
/// `engine.rs`'s credential filter exists to remove.
#[tokio::test]
async fn a_key_for_one_provider_authenticates_no_other() {
    let plane = plane();
    let elsewhere = Target::Frontier {
        provider: "anthropic".into(),
        model: "claude".into(),
    };

    let admission = plane
        .membership(&Principal::new("byok", "ada"))
        .expect("the fixture declares this membership");
    assert!(
        admission
            .credentials
            .access_for(&elsewhere)
            .map(|access| access.credential.require_api_key("anthropic").is_ok())
            != Some(true),
        "ada attached an OpenRouter key and nothing else; a provider she named \
         no key for must stay unreachable"
    );

    // CONTROL: the provider she did name, through the same accessor.
    assert!(
        admission
            .credentials
            .access_for(&target())
            .expect("openrouter is reachable for ada")
            .credential
            .require_api_key(PROVIDER)
            .is_ok()
    );
}
