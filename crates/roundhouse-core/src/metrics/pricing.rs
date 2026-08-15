// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Pricing a model that has no price.
//!
//! "How much did serving this locally save us?" has no measured answer. A local
//! worker bills nothing, so the saving is the difference against a call that
//! never happened, and the only way to put a number on it is to name a hosted
//! model we would otherwise have used and charge its rate card against the
//! tokens we actually served. That named stand-in is a model's **correlary**,
//! and everything in this module exists to choose one defensibly and to keep
//! the resulting figure legible as the estimate it is.
//!
//! Two ways a correlary gets chosen, in this order:
//!
//! 1. **Declared.** Someone stated that our Llama deployment stands in for a
//!    particular hosted model. This is the only kind of answer that can account
//!    for benchmark results, evaluation runs, or a procurement decision, so a
//!    declaration always wins.
//! 2. **Inferred**, from the shape of the traffic each model actually sees —
//!    the correlary the deployment's own history suggests. See [`TokenShape`].
//!
//! ## The trap in inferring one
//!
//! Token shape says how a model is *used*, not how good it is. A 7B model
//! summarizing chat logs and a frontier reasoning model doing the same job have
//! nearly identical shapes: similar prompt lengths, similar output lengths, no
//! thinking. Pricing the 7B against the frontier model on that basis would
//! multiply the reported saving by an order of magnitude, and the number would
//! look better the more absurd the comparison got.
//!
//! So shape alone never selects a correlary. A candidate must first pass a
//! **capability gate**: its declared `quality_prior` must sit within
//! [`ShadowPricing::capability_band`] of the local model's. Only among models
//! we have already said are of comparable capability does shape decide which
//! one. Where nothing passes the gate, no correlary is produced and no shadow
//! price is charged — the local traffic is reported as unpriced rather than
//! priced against a model it has no business being compared to.
//!
//! Quality priors are configuration, not measurement (`FrontierModelSpec`
//! documents them that way). The gate is therefore only as honest as the
//! numbers fed to it, which is the argument for declaring correlaries outright
//! wherever a real evaluation exists.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::event::Usage;
use crate::routing::ProviderPricing;

/// A hosted model that a local one can be priced against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceModel {
    pub provider: String,
    pub model: String,
    pub pricing: ProviderPricing,
    /// Relative capability, 0.0..=1.0. Configuration, not measurement — the
    /// capability gate is only as good as this number.
    pub quality_prior: f64,
}

/// The observable shape of one model's traffic.
///
/// Ratios rather than totals, so a model that served ten turns is comparable
/// with one that served ten thousand: the question is what a *typical* call
/// looks like, not how many there were.
///
/// The two magnitude terms are log-scaled because prompt lengths in agentic
/// work span orders of magnitude. On a linear scale the difference between a
/// 90k and a 100k context would swamp the difference between 400 and 4,000
/// output tokens, and the nearest model would be decided almost entirely by
/// context length.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TokenShape {
    /// Output as a fraction of all billed tokens. Separates terse extraction
    /// work from generative work.
    pub output_ratio: f64,
    /// Cached fraction of the prompt. A model serving long-running sessions
    /// looks nothing like one serving one-shot requests.
    ///
    /// Biased low for a model whose provider reports usage unreliably: an
    /// unreported call records no cache reads by policy, so the model looks
    /// colder than it is and drifts away from genuinely cold candidates it
    /// might otherwise match. Second-order — inference is gated on capability
    /// and overridden by any declaration — but it is a reason to declare a
    /// correlary rather than infer one when coverage is poor.
    pub cache_ratio: f64,
    /// Thinking as a fraction of output. Near 1.0 for a reasoning model working
    /// hard, exactly 0.0 for a model with no thinking mode.
    pub reasoning_ratio: f64,
    /// `ln(1 + mean input tokens per call)`.
    pub log_mean_input: f64,
    /// `ln(1 + mean output tokens per call)`.
    pub log_mean_output: f64,
}

impl TokenShape {
    /// Derive a shape from a rollup, or `None` if there is nothing to describe.
    ///
    /// A model with no calls has no shape — not a shape of zeroes, which would
    /// sit at the origin and read as "maximally similar to whatever else is
    /// near the origin".
    pub fn from_rollup(usage: &Usage, calls: u64) -> Option<Self> {
        if calls == 0 {
            return None;
        }
        let calls = calls as f64;
        let input = usage.input_tokens as f64;
        let output = usage.output_tokens as f64;
        let billed = input + output;
        Some(Self {
            output_ratio: ratio(output, billed),
            cache_ratio: ratio(usage.cached_input_tokens as f64, input),
            reasoning_ratio: ratio(usage.reasoning_tokens as f64, output),
            log_mean_input: (1.0 + input / calls).ln(),
            log_mean_output: (1.0 + output / calls).ln(),
        })
    }

    /// Weighted distance to another shape. Zero means identical.
    ///
    /// The weights encode what makes two workloads genuinely alike. The three
    /// ratio terms are already 0..=1 and carry most of the signal, so they lead.
    /// The magnitude terms are divided down because a natural-log difference of
    /// 1.0 is a factor of `e` in tokens — a large gap that would otherwise
    /// dominate every comparison.
    pub fn distance(&self, other: &Self) -> f64 {
        const OUTPUT_RATIO: f64 = 1.0;
        const CACHE_RATIO: f64 = 0.75;
        const REASONING_RATIO: f64 = 1.25;
        const LOG_MAGNITUDE: f64 = 0.25;

        let terms = [
            OUTPUT_RATIO * (self.output_ratio - other.output_ratio).powi(2),
            CACHE_RATIO * (self.cache_ratio - other.cache_ratio).powi(2),
            REASONING_RATIO * (self.reasoning_ratio - other.reasoning_ratio).powi(2),
            LOG_MAGNITUDE * (self.log_mean_input - other.log_mean_input).powi(2),
            LOG_MAGNITUDE * (self.log_mean_output - other.log_mean_output).powi(2),
        ];
        terms.iter().sum::<f64>().sqrt()
    }
}

fn ratio(part: f64, whole: f64) -> f64 {
    if whole <= 0.0 { 0.0 } else { part / whole }
}

/// How a correlary was arrived at, as it appears on the wire.
///
/// Serialized alongside every shadow-priced figure so a reader can tell a
/// number resting on a procurement decision from one resting on a similarity
/// metric — they are not the same claim and should not be quoted the same way.
///
/// Private, and deliberately so: it is the JSON tag, not a type callers reason
/// with. [`Correlary`] is what Rust code matches on, and exporting a second,
/// looser spelling of the same three cases invites exactly the priced/unpriced
/// disagreement that type is shaped to prevent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CorrelaryBasis {
    /// Someone stated this equivalence. The note is theirs.
    Declared { note: String },
    /// Chosen as the nearest capability-comparable model by traffic shape.
    Inferred {
        shape_distance: f64,
        /// How many models passed the capability gate and were compared. One
        /// means the gate left no actual choice to make.
        considered: usize,
    },
    /// No stand-in could be justified, so no shadow price was charged.
    Unpriced { reason: String },
}

/// How a *priced* correlary was arrived at.
///
/// The same two cases as [`CorrelaryBasis`] minus `Unpriced`, which is not a
/// basis for a price but the absence of one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PricedBasis {
    Declared {
        note: String,
    },
    Inferred {
        shape_distance: f64,
        considered: usize,
    },
}

/// A local model and the hosted model it is priced against.
///
/// One tagged value rather than `Option<ReferenceModel>` beside a basis that
/// separately says whether a price exists. Those two encoded the same fact
/// twice and could disagree — a correlary carrying a reference model *and* an
/// `Unpriced` basis charged a full shadow price while the dashboard printed
/// "contributes nothing to the savings figure" from the same record. Nothing
/// in the tree built one, but every field was `pub` and the type derived
/// `Deserialize`, so the wire was an open door. Here the invalid state cannot
/// be named.
///
/// The serialized shape is unchanged — `{ local_model, reference, basis }` —
/// so consumers keep working; the difference is that deserialization now
/// validates instead of accepting anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(into = "CorrelaryWire", try_from = "CorrelaryWire")]
pub enum Correlary {
    /// A stand-in was found, so this model's traffic carries a shadow price.
    Priced {
        local_model: String,
        reference: ReferenceModel,
        basis: PricedBasis,
    },
    /// No stand-in could be justified. Contributes nothing to the saving.
    Unpriced { local_model: String, reason: String },
}

/// The wire form, kept as its own type so the JSON contract is stated in one
/// place rather than implied by whatever the enum happens to derive.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CorrelaryWire {
    local_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reference: Option<ReferenceModel>,
    basis: CorrelaryBasis,
}

impl From<Correlary> for CorrelaryWire {
    fn from(correlary: Correlary) -> Self {
        match correlary {
            Correlary::Priced {
                local_model,
                reference,
                basis,
            } => Self {
                local_model,
                reference: Some(reference),
                basis: match basis {
                    PricedBasis::Declared { note } => CorrelaryBasis::Declared { note },
                    PricedBasis::Inferred {
                        shape_distance,
                        considered,
                    } => CorrelaryBasis::Inferred {
                        shape_distance,
                        considered,
                    },
                },
            },
            Correlary::Unpriced {
                local_model,
                reason,
            } => Self {
                local_model,
                reference: None,
                basis: CorrelaryBasis::Unpriced { reason },
            },
        }
    }
}

/// Why a serialized correlary was refused.
#[derive(Debug, thiserror::Error)]
#[error("a correlary for `{local_model}` is {described}, which cannot be true of the same record")]
pub struct IncoherentCorrelary {
    local_model: String,
    described: &'static str,
}

impl TryFrom<CorrelaryWire> for Correlary {
    type Error = IncoherentCorrelary;

    fn try_from(wire: CorrelaryWire) -> Result<Self, Self::Error> {
        let incoherent = |described| IncoherentCorrelary {
            local_model: wire.local_model.clone(),
            described,
        };
        match (wire.reference, wire.basis) {
            (Some(reference), CorrelaryBasis::Declared { note }) => Ok(Correlary::Priced {
                local_model: wire.local_model,
                reference,
                basis: PricedBasis::Declared { note },
            }),
            (
                Some(reference),
                CorrelaryBasis::Inferred {
                    shape_distance,
                    considered,
                },
            ) => Ok(Correlary::Priced {
                local_model: wire.local_model,
                reference,
                basis: PricedBasis::Inferred {
                    shape_distance,
                    considered,
                },
            }),
            (None, CorrelaryBasis::Unpriced { reason }) => Ok(Correlary::Unpriced {
                local_model: wire.local_model,
                reason,
            }),
            (Some(_), CorrelaryBasis::Unpriced { .. }) => {
                Err(incoherent("unpriced yet carries a reference model"))
            }
            (None, _) => Err(incoherent("priced yet names no reference model")),
        }
    }
}

impl Correlary {
    pub fn local_model(&self) -> &str {
        match self {
            Correlary::Priced { local_model, .. } | Correlary::Unpriced { local_model, .. } => {
                local_model
            }
        }
    }

    pub fn reference(&self) -> Option<&ReferenceModel> {
        match self {
            Correlary::Priced { reference, .. } => Some(reference),
            Correlary::Unpriced { .. } => None,
        }
    }
}

impl Correlary {
    /// What this local traffic would have cost on its stand-in.
    ///
    /// The counterfactual is deliberately like-for-like: the *same* token
    /// counts including the same cached fraction, billed at the reference
    /// model's rates. It is not "what if we had sent this cold", which would
    /// assume the hosted provider's cache never warmed up and would roughly
    /// double the headline figure on long sessions. Had we been routing to
    /// that provider all along, its prefix cache would have been warm about as
    /// often as our own is — both are prefix caches over the same append-only
    /// conversation — so carrying our measured cache ratio across is the
    /// defensible reading, and it is the conservative one.
    pub fn shadow_cost_usd(&self, usage: &Usage) -> f64 {
        match self {
            Correlary::Priced { reference, .. } => reference.pricing.price(usage),
            // Not a zero that some branch remembered to return: there is no
            // rate card in this arm to price against.
            Correlary::Unpriced { .. } => 0.0,
        }
    }
}

/// The rate cards and declarations a deployment prices its local fleet with.
#[derive(Debug, Clone)]
pub struct ShadowPricing {
    references: Vec<ReferenceModel>,
    declared: HashMap<String, DeclaredCorrelary>,
    capability_band: f64,
}

#[derive(Debug, Clone)]
struct DeclaredCorrelary {
    provider: String,
    model: String,
    note: String,
}

/// Default width of the capability gate.
///
/// A tenth of the 0..=1 quality scale in each direction. Wide enough that a
/// deployment does not have to tune its priors to three decimal places before
/// anything matches, narrow enough that a mid-tier model cannot be priced
/// against a flagship.
pub const DEFAULT_CAPABILITY_BAND: f64 = 0.10;

impl ShadowPricing {
    pub fn new(references: Vec<ReferenceModel>) -> Self {
        Self {
            references,
            declared: HashMap::new(),
            capability_band: DEFAULT_CAPABILITY_BAND,
        }
    }

    /// State outright that `local_model` stands in for a hosted model.
    ///
    /// Beats inference unconditionally, including when the two models' traffic
    /// looks nothing alike: a declaration is a claim about capability, and
    /// usage patterns are not evidence against it.
    pub fn declare(
        mut self,
        local_model: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        note: impl Into<String>,
    ) -> Self {
        self.declared.insert(
            local_model.into(),
            DeclaredCorrelary {
                provider: provider.into(),
                model: model.into(),
                note: note.into(),
            },
        );
        self
    }

    /// How far apart two models' quality priors may be and still be compared.
    pub fn with_capability_band(mut self, band: f64) -> Self {
        self.capability_band = band.max(0.0);
        self
    }

    pub fn capability_band(&self) -> f64 {
        self.capability_band
    }

    pub fn references(&self) -> &[ReferenceModel] {
        &self.references
    }

    /// Choose the correlary for one local model.
    ///
    /// `observed` carries the traffic shape measured for each hosted model in
    /// the same window. A hosted model this deployment has never actually
    /// called has no observed shape, so it cannot be inferred against — only
    /// declared. That is the correct asymmetry: inference is an argument from
    /// this deployment's own history, and there is no history for a model it
    /// has never used.
    pub fn resolve(
        &self,
        local_model: &str,
        local_quality_prior: f64,
        local_shape: Option<TokenShape>,
        observed: &HashMap<(String, String), TokenShape>,
    ) -> Correlary {
        if let Some(declared) = self.declared.get(local_model) {
            let reference = self
                .references
                .iter()
                .find(|r| r.provider == declared.provider && r.model == declared.model);
            return match reference {
                Some(reference) => Correlary::Priced {
                    local_model: local_model.to_string(),
                    reference: reference.clone(),
                    basis: PricedBasis::Declared {
                        note: declared.note.clone(),
                    },
                },
                // Declared against a model with no rate card. Refusing to price
                // is the only safe answer: silently falling back to inference
                // would quietly overrule an explicit human decision, and the
                // configuration error would never surface.
                None => Correlary::Unpriced {
                    local_model: local_model.to_string(),
                    reason: format!(
                        "declared correlary `{}/{}` has no rate card",
                        declared.provider, declared.model
                    ),
                },
            };
        }

        let Some(local_shape) = local_shape else {
            return unpriced(local_model, "no local traffic to compare".to_string());
        };

        // The capability gate, applied before shape is looked at.
        let comparable: Vec<&ReferenceModel> = self
            .references
            .iter()
            .filter(|r| (r.quality_prior - local_quality_prior).abs() <= self.capability_band)
            .collect();
        if comparable.is_empty() {
            return unpriced(
                local_model,
                format!(
                    "no hosted model within {:.2} of quality prior {:.2}",
                    self.capability_band, local_quality_prior
                ),
            );
        }

        let mut scored: Vec<(&ReferenceModel, f64)> = comparable
            .iter()
            .filter_map(|reference| {
                let key = (reference.provider.clone(), reference.model.clone());
                observed
                    .get(&key)
                    .map(|shape| (*reference, local_shape.distance(shape)))
            })
            .collect();
        if scored.is_empty() {
            return unpriced(
                local_model,
                "no capability-comparable hosted model has been called, so there is \
                 no traffic to compare against"
                    .to_string(),
            );
        }

        // Ties broken by name so the answer is stable across restarts: a
        // correlary that silently changed between two runs would move the
        // headline saving with nothing in the log to explain it.
        scored.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| (&a.0.provider, &a.0.model).cmp(&(&b.0.provider, &b.0.model)))
        });
        let (reference, shape_distance) = scored[0];
        Correlary::Priced {
            local_model: local_model.to_string(),
            reference: reference.clone(),
            basis: PricedBasis::Inferred {
                shape_distance,
                considered: scored.len(),
            },
        }
    }
}

fn unpriced(local_model: &str, reason: String) -> Correlary {
    Correlary::Unpriced {
        local_model: local_model.to_string(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricing(input: f64, cached: f64, output: f64) -> ProviderPricing {
        ProviderPricing {
            input_per_mtok_usd: input,
            cached_input_per_mtok_usd: cached,
            cache_write_per_mtok_usd: 0.0,
            output_per_mtok_usd: output,
        }
    }

    fn reference(provider: &str, model: &str, quality: f64) -> ReferenceModel {
        ReferenceModel {
            provider: provider.into(),
            model: model.into(),
            pricing: pricing(3.0, 0.3, 15.0),
            quality_prior: quality,
        }
    }

    fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Usage {
        Usage {
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            reasoning_tokens: reasoning,
            ..Default::default()
        }
    }

    fn shapes(entries: &[(&str, &str, TokenShape)]) -> HashMap<(String, String), TokenShape> {
        entries
            .iter()
            .map(|(p, m, shape)| ((p.to_string(), m.to_string()), *shape))
            .collect()
    }

    #[test]
    fn a_model_with_no_calls_has_no_shape() {
        assert!(TokenShape::from_rollup(&usage(100, 0, 10, 0), 0).is_none());
    }

    #[test]
    fn shape_is_scale_free_across_call_counts() {
        let ten = TokenShape::from_rollup(&usage(10_000, 5_000, 1_000, 0), 10).unwrap();
        let hundred = TokenShape::from_rollup(&usage(100_000, 50_000, 10_000, 0), 100).unwrap();
        assert!(
            ten.distance(&hundred) < 1e-9,
            "the same traffic served more often is the same shape"
        );
    }

    #[test]
    fn a_reasoning_model_is_far_from_a_non_reasoning_one() {
        let thinker = TokenShape::from_rollup(&usage(10_000, 0, 4_000, 3_600), 10).unwrap();
        let plain = TokenShape::from_rollup(&usage(10_000, 0, 4_000, 0), 10).unwrap();
        assert!(
            thinker.distance(&plain) > 0.5,
            "spending 90% of output on thinking is a different workload"
        );
    }

    #[test]
    fn a_declaration_beats_a_closer_shape_match() {
        let observed = shapes(&[
            (
                "anthropic",
                "big",
                TokenShape::from_rollup(&usage(1_000, 0, 900, 0), 1).unwrap(),
            ),
            (
                "openai",
                "small",
                TokenShape::from_rollup(&usage(10_000, 5_000, 500, 0), 10).unwrap(),
            ),
        ]);
        let pricer = ShadowPricing::new(vec![
            reference("anthropic", "big", 0.6),
            reference("openai", "small", 0.6),
        ])
        .declare("llama", "anthropic", "big", "matched on our eval suite");

        // Shape points squarely at openai/small; the declaration overrules it.
        let local = TokenShape::from_rollup(&usage(10_000, 5_000, 500, 0), 10);
        let correlary = pricer.resolve("llama", 0.6, local, &observed);

        assert_eq!(correlary.reference().unwrap().model, "big");
        assert!(matches!(
            correlary,
            Correlary::Priced {
                basis: PricedBasis::Declared { .. },
                ..
            }
        ));
    }

    #[test]
    fn the_capability_gate_blocks_a_flagship_stand_in() {
        let observed = shapes(&[(
            "anthropic",
            "flagship",
            TokenShape::from_rollup(&usage(10_000, 5_000, 500, 0), 10).unwrap(),
        )]);
        let pricer = ShadowPricing::new(vec![reference("anthropic", "flagship", 0.95)]);

        // Identical traffic shape, wildly different capability.
        let local = TokenShape::from_rollup(&usage(10_000, 5_000, 500, 0), 10);
        let correlary = pricer.resolve("tiny-7b", 0.35, local, &observed);

        assert!(correlary.reference().is_none());
        assert!(matches!(correlary, Correlary::Unpriced { .. }));
        assert_eq!(
            correlary.shadow_cost_usd(&usage(10_000, 5_000, 500, 0)),
            0.0,
            "an unpriced correlary must contribute nothing to the saving"
        );
    }

    #[test]
    fn among_comparable_models_the_nearest_shape_wins() {
        let observed = shapes(&[
            (
                "anthropic",
                "chatty",
                TokenShape::from_rollup(&usage(1_000, 0, 2_000, 0), 1).unwrap(),
            ),
            (
                "openai",
                "terse",
                TokenShape::from_rollup(&usage(20_000, 10_000, 300, 0), 10).unwrap(),
            ),
        ]);
        let pricer = ShadowPricing::new(vec![
            reference("anthropic", "chatty", 0.62),
            reference("openai", "terse", 0.58),
        ]);

        let local = TokenShape::from_rollup(&usage(20_000, 10_000, 300, 0), 10);
        let correlary = pricer.resolve("llama", 0.6, local, &observed);

        assert_eq!(correlary.reference().unwrap().model, "terse");
        match &correlary {
            Correlary::Priced {
                basis: PricedBasis::Inferred { considered, .. },
                ..
            } => assert_eq!(*considered, 2),
            other => panic!("expected an inferred basis, got {other:?}"),
        }
    }

    #[test]
    fn a_hosted_model_never_called_cannot_be_inferred_against() {
        let pricer = ShadowPricing::new(vec![reference("anthropic", "unused", 0.6)]);
        let local = TokenShape::from_rollup(&usage(1_000, 0, 100, 0), 1);

        let correlary = pricer.resolve("llama", 0.6, local, &HashMap::new());
        assert!(correlary.reference().is_none());
    }

    #[test]
    fn a_declaration_naming_a_model_with_no_rate_card_refuses_to_price() {
        let pricer = ShadowPricing::new(vec![reference("anthropic", "known", 0.6)]).declare(
            "llama",
            "anthropic",
            "typo",
            "",
        );
        let observed = shapes(&[(
            "anthropic",
            "known",
            TokenShape::from_rollup(&usage(1_000, 0, 100, 0), 1).unwrap(),
        )]);

        let correlary = pricer.resolve(
            "llama",
            0.6,
            TokenShape::from_rollup(&usage(1_000, 0, 100, 0), 1),
            &observed,
        );
        assert!(
            correlary.reference().is_none(),
            "a misconfigured declaration must not silently fall back to inference"
        );
    }

    #[test]
    fn the_shadow_price_carries_the_measured_cache_ratio_across() {
        let pricer = ShadowPricing::new(vec![reference("anthropic", "big", 0.6)]).declare(
            "llama",
            "anthropic",
            "big",
            "",
        );
        let correlary = pricer.resolve("llama", 0.6, None, &HashMap::new());

        // 100k prompt, 90% of it cached, 1k output, at 3.00 / 0.30 / 15.00.
        let served = usage(100_000, 90_000, 1_000, 0);
        let shadow = correlary.shadow_cost_usd(&served);
        let expected = 10_000.0 * 3.0e-6 + 90_000.0 * 0.3e-6 + 1_000.0 * 15.0e-6;
        assert!((shadow - expected).abs() < 1e-12);

        // The cold reading would be far larger; we deliberately do not use it.
        let cold = 100_000.0 * 3.0e-6 + 1_000.0 * 15.0e-6;
        assert!(shadow < cold);
    }
}
