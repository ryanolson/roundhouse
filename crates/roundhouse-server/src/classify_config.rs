// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a deployment has to write down before any turn content leaves it.
//!
//! **Off unless a file says otherwise, and the file has no defaults.** The
//! variable being unset is the shipped state and costs nothing at boot; a file
//! that is named and unreadable stops the process, which is the posture the
//! catalog and the control plane already take and for the same reason — starting
//! anyway would serve under settings nobody chose.
//!
//! Three things have no default here and never will:
//!
//! - **the model**, because a floating alias re-ranks itself underneath a
//!   deployment that never changed a line;
//! - **the rate card**, because rate cards never go in source (`CLAUDE.md`), and
//!   a guessed one would put invented dollars in a ledger;
//! - **the credential's variable name**, because this deployment spends its own
//!   key and must be told which one. Nothing here reads a key off disk, and a
//!   caller's forwarded seat is refused by the transport rather than sent.
//!
//! `enabled` defaults to false even inside a file that specifies everything
//! else, so writing the configuration and turning it on are two acts.

use std::path::Path;

use roundhouse_core::classify::ProjectionCaps;
use roundhouse_core::control::{
    Allocation, Budget, BudgetTerms, BudgetWindow, Exhaustion, Secret, TurnCredential,
};
use roundhouse_core::routing::ledger::ProviderPricing;
use roundhouse_fleet::typesafe::{DEFAULT_SYSTEM_ONE_BASE, SystemOneLimits};
use serde::Deserialize;

use crate::classify_runtime::RuntimeLimits;
use crate::typesafe_shadow::ShadowConfig;

/// The file that turns background classification on.
pub const CLASSIFY_VAR: &str = "ROUNDHOUSE_CLASSIFY_CONFIG";

/// What is wrong with the configuration, in the operator's terms.
#[derive(Debug, thiserror::Error)]
pub enum ClassifyConfigError {
    #[error("reading the classification configuration at {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the classification configuration at {path} is not valid JSON: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path}: `{field}` must be {expectation}")]
    Invalid {
        path: String,
        field: &'static str,
        expectation: &'static str,
    },
    #[error(
        "{path} enables turn classification and names `{var}` for its credential, \
         and that variable is not set in this process's environment"
    )]
    MissingCredential { path: String, var: String },
    #[error("{path}: the value of `{var}` is not usable as an API key")]
    UnusableCredential { path: String, var: String },
}

/// Where the classifier's key is read from. A variable name, never a key.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub env: String,
}

/// The rate card, as the operator copied it from the service's price list.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingConfig {
    pub input_per_mtok_usd: f64,
    pub output_per_mtok_usd: f64,
}

/// What this deployment is willing to send.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsConfig {
    pub max_prior_classifications: usize,
    /// Prior-turn local metadata records to carry — see
    /// [`ProjectionCaps::max_prior_turns`]. No default: that type's own doc
    /// says a deployment that has not chosen this has not decided, and the
    /// two lists are unrelated context with unrelated sizes, so a file that
    /// omits it is refused rather than silently bounded by
    /// `max_prior_classifications` on its behalf.
    pub max_prior_turns: usize,
    pub max_prompt_chars: usize,
    pub max_total_bytes: usize,
}

/// The transport's own bounds.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportConfig {
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub deadline_ms: u64,
}

/// What the background executor may hold.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorConfig {
    pub max_in_flight: usize,
    pub max_http_concurrency: usize,
    pub call_ttl_ms: u64,
    /// How long a finished result is held for a turn to drain it. A separate
    /// clock from `call_ttl_ms`; see [`RuntimeLimits::result_retention_ms`].
    pub result_retention_ms: u64,
    pub sweep_interval_ms: u64,
}

/// The separate ceiling evaluation calls draw on.
///
/// Project and member semantics, exactly as the serving budget has them, against
/// a ledger of its own: a classification must not be able to spend a project's
/// serving budget, and an evaluation overspend must not refuse a turn.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationBudgetConfig {
    pub limit_usd: f64,
    pub window: BudgetWindow,
    pub warn_at: f64,
    /// A member's share of the ceiling above. Absent pools the whole of it.
    #[serde(default)]
    pub member_share: Option<f64>,
}

/// One deployment's whole answer about turn classification.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifyConfig {
    /// Off unless this says otherwise, whatever else the file contains.
    #[serde(default)]
    pub enabled: bool,
    /// The operator's own revision, stamped on every durable intent.
    pub revision: u32,
    /// The pinned model id.
    pub model: String,
    /// The API root. Defaulted to the published one because it is the service's
    /// own address rather than a choice about money or content.
    #[serde(default = "default_base_url")]
    pub base_url: String,
    pub auth: AuthConfig,
    pub pricing: PricingConfig,
    /// What the quote is taken with. The service answers structured choices, so
    /// it is small.
    pub expected_output_tokens: u64,
    pub caps: CapsConfig,
    pub transport: TransportConfig,
    pub executor: ExecutorConfig,
    pub budget: EvaluationBudgetConfig,
}

fn default_base_url() -> String {
    DEFAULT_SYSTEM_ONE_BASE.to_string()
}

impl ClassifyConfig {
    pub fn from_json(json: &str, path: &str) -> Result<Self, ClassifyConfigError> {
        let config: Self =
            serde_json::from_str(json).map_err(|source| ClassifyConfigError::Parse {
                path: path.to_string(),
                source,
            })?;
        config.validate(path)?;
        Ok(config)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ClassifyConfigError> {
        let path = path.as_ref();
        let display = path.display().to_string();
        let json = std::fs::read_to_string(path).map_err(|source| ClassifyConfigError::Read {
            path: display.clone(),
            source,
        })?;
        Self::from_json(&json, &display)
    }

    /// Everything that would otherwise become a runtime surprise.
    ///
    /// Zeroes are refused rather than accepted as "no limit": a zero in-flight
    /// bound would disable the feature silently on a deployment that had just
    /// enabled it, and a zero TTL would expire every call before it was sent.
    fn validate(&self, path: &str) -> Result<(), ClassifyConfigError> {
        let invalid = |field, expectation| ClassifyConfigError::Invalid {
            path: path.to_string(),
            field,
            expectation,
        };
        if self.model.trim().is_empty() {
            return Err(invalid("model", "a pinned model id"));
        }
        if self.auth.env.trim().is_empty() {
            return Err(invalid("auth.env", "the name of an environment variable"));
        }
        for (field, rate) in [
            (
                "pricing.input_per_mtok_usd",
                self.pricing.input_per_mtok_usd,
            ),
            (
                "pricing.output_per_mtok_usd",
                self.pricing.output_per_mtok_usd,
            ),
        ] {
            if !rate.is_finite() || rate < 0.0 {
                return Err(invalid(field, "a finite rate of zero or more"));
            }
        }
        // A cap no wider than the truncation marker itself saturates `keep`
        // to zero: the capture would be the bare marker with no user text at
        // all, which breaks the bound this cap exists to state. Named
        // against the marker's own length rather than a literal, so the two
        // can never drift apart the way a hand-counted twelve would.
        let min_prompt_chars = roundhouse_core::validate::brief::TRUNCATION_MARKER
            .chars()
            .count();
        if self.caps.max_prompt_chars <= min_prompt_chars || self.caps.max_total_bytes == 0 {
            return Err(invalid(
                "caps",
                "max_prompt_chars wider than the truncation marker, and max_total_bytes non-zero",
            ));
        }
        if self.transport.max_request_bytes == 0
            || self.transport.max_response_bytes == 0
            || self.transport.deadline_ms == 0
        {
            return Err(invalid("transport", "non-zero on every axis"));
        }
        if self.executor.max_in_flight == 0
            || self.executor.max_http_concurrency == 0
            || self.executor.call_ttl_ms == 0
            || self.executor.result_retention_ms == 0
            || self.executor.sweep_interval_ms == 0
        {
            return Err(invalid("executor", "non-zero on every axis"));
        }
        if !self.budget.limit_usd.is_finite() || self.budget.limit_usd <= 0.0 {
            return Err(invalid("budget.limit_usd", "a finite ceiling above zero"));
        }
        if !(0.0..=1.0).contains(&self.budget.warn_at) || self.budget.warn_at <= 0.0 {
            return Err(invalid("budget.warn_at", "a fraction in (0, 1]"));
        }
        if let Some(share) = self.budget.member_share
            && (!(0.0..=1.0).contains(&share) || share <= 0.0)
        {
            return Err(invalid("budget.member_share", "a fraction in (0, 1]"));
        }
        Ok(())
    }

    pub fn caps(&self) -> ProjectionCaps {
        ProjectionCaps {
            max_prior_classifications: self.caps.max_prior_classifications,
            max_prior_turns: self.caps.max_prior_turns,
            max_prompt_chars: self.caps.max_prompt_chars,
            max_total_bytes: self.caps.max_total_bytes,
        }
    }

    pub fn transport_limits(&self) -> SystemOneLimits {
        SystemOneLimits {
            max_request_bytes: self.transport.max_request_bytes,
            max_response_bytes: self.transport.max_response_bytes,
            deadline_ms: self.transport.deadline_ms,
        }
    }

    pub fn runtime_limits(&self) -> RuntimeLimits {
        RuntimeLimits {
            max_in_flight: self.executor.max_in_flight,
            max_http_concurrency: self.executor.max_http_concurrency,
            call_ttl_ms: self.executor.call_ttl_ms,
            result_retention_ms: self.executor.result_retention_ms,
            sweep_interval_ms: self.executor.sweep_interval_ms,
        }
    }

    /// The rate card, with both cache axes at zero.
    ///
    /// This service reports no cache term, so those two are absent measurements
    /// rather than rates an operator chose. Giving them a field in the file
    /// would invite somebody to fill one in.
    pub fn pricing(&self) -> ProviderPricing {
        ProviderPricing {
            input_per_mtok_usd: self.pricing.input_per_mtok_usd,
            cached_input_per_mtok_usd: 0.0,
            cache_write_per_mtok_usd: 0.0,
            output_per_mtok_usd: self.pricing.output_per_mtok_usd,
        }
    }

    /// The adapter's configuration.
    ///
    /// Called only from [`crate::classify_runtime::compose`], and only after it
    /// has already refused a file that said `enabled: false` — so every
    /// `ShadowConfig` this builds is for a deployment that opted in. `enabled`
    /// gates whether this method is reached at all; `ShadowConfig` itself has
    /// no second copy of that switch to fall out of step with this one.
    pub fn shadow_config(&self) -> ShadowConfig {
        ShadowConfig::new(
            self.model.trim(),
            self.pricing(),
            self.expected_output_tokens,
            self.caps(),
            self.revision,
        )
    }

    /// The ceiling every evaluation call is held against.
    ///
    /// `Exhaustion::Refuse` and not a degrade: there is no cheaper classifier to
    /// fall back to, and serving is unaffected either way — an exhausted
    /// evaluation budget stops classifying and stops nothing else.
    pub fn budget_terms(&self) -> BudgetTerms {
        BudgetTerms {
            budget: Budget {
                limit_usd: self.budget.limit_usd,
                window: self.budget.window,
                on_exhaustion: Exhaustion::Refuse,
                warn_at: self.budget.warn_at,
            },
            allocation: match self.budget.member_share {
                Some(fraction) => Allocation::Share { fraction },
                None => Allocation::Pooled,
            },
        }
    }

    /// The deployment's own key, from the variable this file names.
    ///
    /// `env` is passed in rather than read here for the reason `frontier_clients`
    /// takes one: a boot refusal is the load-bearing half of "an enabled
    /// classifier with no key stops the process", and a test that had to mutate
    /// the process environment to reach it would race every other test in the
    /// binary.
    pub fn credential(
        &self,
        path: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<TurnCredential, ClassifyConfigError> {
        let var = self.auth.env.trim();
        let raw = env(var).ok_or_else(|| ClassifyConfigError::MissingCredential {
            path: path.to_string(),
            var: var.to_string(),
        })?;
        let secret = Secret::api_key(raw).map_err(|_| ClassifyConfigError::UnusableCredential {
            path: path.to_string(),
            var: var.to_string(),
        })?;
        Ok(TurnCredential::Stored(secret))
    }
}

/// The configuration named by [`CLASSIFY_VAR`], or `None` if it is unset.
///
/// Returns the path beside the configuration: every later refusal names the file
/// an operator would go and edit, and re-deriving the path at those sites is how
/// two spellings of one filename get into one boot log.
pub fn from_env() -> Result<Option<(String, ClassifyConfig)>, ClassifyConfigError> {
    match std::env::var(CLASSIFY_VAR) {
        Ok(path) if !path.trim().is_empty() => {
            let path = path.trim().to_string();
            let config = ClassifyConfig::load(&path)?;
            Ok(Some((path, config)))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests;
