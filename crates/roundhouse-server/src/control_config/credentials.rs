// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The `"credentials"` block: what an operator writes, and what it resolves to.
//!
//! Three tiers write the same block — the file itself (the deployment's own
//! keys), a project, and a key — and one struct serves all three, the same
//! choice [`PolicyConfig`](super::config::PolicyConfig) makes for `"policy"`
//! and `"overrides"`. What differs is which fields are *meaningful*, and that
//! is decided by which conversion below is called rather than by three structs
//! that would drift apart the first time a field was added to one of them.
//!
//! Two rules are enforced here and nowhere else.
//!
//! **A secret is never in the file.** A provider entry names an environment
//! variable; [`CredentialRef::env_var`] refuses anything that is not a variable
//! name, which is structural rather than a convention — every credential format
//! in circulation carries a character that alphabet lacks. The value is read
//! **at boot**, so a variable that is unset stops the process instead of
//! failing one tenant's turns on a deployment that looked healthy.
//!
//! **An OAuth-shaped credential is refused with a reason.** Both axes: a
//! `"kind"` naming one goes through [`CredentialKind::parse`], and a *value*
//! shaped like one goes through [`Secret::api_key`]. Refusing only the first
//! would let an operator paste a device-login token into the variable a
//! `"kind": "api_key"` entry names, which is the shape §3 actually forbids.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use roundhouse_core::control::credential::access::ProviderKeys;
use roundhouse_core::control::{
    BudgetCounts, CredentialKind, CredentialMode, CredentialRef, Secret,
};

use super::config::ControlPlaneError;

/// A `"credentials"` object, as any of the three tiers may write it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CredentialsConfig {
    /// Whose credential a turn of this project's is paid with. Project-only —
    /// see [`Self::to_tier`] for why a key naming it is refused rather than
    /// ignored.
    pub mode: Option<CredentialMode>,
    /// Whether a member's own credential draws the project's budget.
    /// Project-only, for the same reason `mode` is.
    pub budget_counts: Option<BudgetCounts>,
    /// Provider name to the variable its key lives in. The provider name is
    /// the one the catalog spells, because that is what a `Target::Frontier`
    /// carries and what the candidate filter matches on.
    pub providers: BTreeMap<String, ProviderCredentialConfig>,
}

/// Where one provider's key lives.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCredentialConfig {
    /// The environment variable holding the secret. **Not the secret.**
    pub env_var: String,
    /// What kind of credential this is, if the operator said.
    ///
    /// A `String` rather than [`CredentialKind`] so the refusal is
    /// [`CredentialKind::parse`]'s and not serde's. The two answer the same
    /// mistake differently: serde says `unknown variant "oauth"`, which tells an
    /// operator their JSON is wrong, and `parse` says *why* roundhouse will not
    /// take it — which is the one a person can act on, and the one §3 promises.
    #[serde(default)]
    pub kind: Option<String>,
}

impl CredentialsConfig {
    /// Read as a project's block: the mode, the budget axis, and its own keys.
    pub fn to_project(
        &self,
        path: &str,
        entry: &str,
    ) -> Result<(CredentialMode, BudgetCounts, ProviderKeys), ControlPlaneError> {
        Ok((
            self.mode.unwrap_or_default(),
            self.budget_counts.unwrap_or_default(),
            self.resolve(path, entry)?,
        ))
    }

    /// Read as the deployment's or a member's block: keys only.
    ///
    /// A `mode` or a `budget_counts` here is **refused rather than ignored**.
    /// Both decide who pays, which is a project-level question by construction:
    /// a member who could set `mode` could set it to `ProjectOnly` and spend the
    /// project's key, and a member who could set `budget_counts` could exempt
    /// their own turns from the ceiling they are supposed to draw. Ignoring the
    /// field would leave an operator with a file that says one thing and a
    /// deployment that does another — the silent shape this milestone's auth
    /// ruling spent itself on.
    pub fn to_tier(&self, path: &str, entry: &str) -> Result<ProviderKeys, ControlPlaneError> {
        for (field, present) in [
            ("mode", self.mode.is_some()),
            ("budget_counts", self.budget_counts.is_some()),
        ] {
            if present {
                return Err(ControlPlaneError::CredentialFieldNotAllowedHere {
                    path: path.to_string(),
                    entry: entry.to_string(),
                    field,
                });
            }
        }
        self.resolve(path, entry)
    }

    /// Read every named variable and judge what it holds.
    ///
    /// **At boot, not at first use**, which is the whole reason this is not
    /// lazy: an unset variable discovered on a turn is a tenant's problem
    /// reported to a tenant, and an unset variable discovered here is an
    /// operator's problem reported to an operator, before anything is served.
    fn resolve(&self, path: &str, entry: &str) -> Result<ProviderKeys, ControlPlaneError> {
        let mut keys = ProviderKeys::new();
        for (provider, source) in &self.providers {
            let refuse =
                |source: roundhouse_core::control::CredentialError| ControlPlaneError::Credential {
                    path: path.to_string(),
                    entry: entry.to_string(),
                    provider: provider.clone(),
                    source,
                };
            if let Some(kind) = &source.kind {
                // The answer is discarded because there is only one kind; what
                // is kept is the refusal. A second variant would make this a
                // value the resolution below reads.
                CredentialKind::parse(kind).map_err(refuse)?;
            }
            // Validated before it is read, so a file that inlined a key is
            // refused for inlining a key rather than for naming a variable
            // nobody set.
            let named = CredentialRef::env_var(source.env_var.clone()).map_err(refuse)?;
            let value = std::env::var(named.env_var_name()).map_err(|_| {
                ControlPlaneError::CredentialEnvVarUnset {
                    path: path.to_string(),
                    entry: entry.to_string(),
                    provider: provider.clone(),
                    var: named.env_var_name().to_string(),
                }
            })?;
            keys.insert(provider.clone(), Secret::api_key(value).map_err(refuse)?);
        }
        Ok(keys)
    }
}
