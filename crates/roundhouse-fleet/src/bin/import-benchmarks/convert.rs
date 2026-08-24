// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! One OpenRouter `/benchmarks` response in, two files out.
//!
//! Everything here is a pure function of a response body. The fetch lives in
//! `main.rs` and never reaches this module — [`convert`] takes a `&str`, so a
//! test of it *cannot* reach the network, which is the only way "the live call
//! is never made in tests" (P6) is a property rather than a promise.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};

/// Where a `quality_prior` came from and what was done to it.
///
/// Recorded per entry rather than once per file because a single unfiltered
/// fetch legitimately mixes the two — the endpoint's own schema example returns
/// `openai/gpt-4o` twice, once from Artificial Analysis and once from
/// OpenRouter's evals — and the two are not on the same scale. A file that said
/// only "imported from OpenRouter" would leave a reader unable to tell a
/// documented 0–1 accuracy from a composite index divided by a bound nobody
/// published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// `UnifiedBenchmarksORItem.accuracy`, used verbatim.
    ///
    /// The one index that is natively on `quality_prior`'s scale: the schema
    /// documents it as "Aggregate accuracy score from 0 to 1. Higher is
    /// better." Nothing is normalized, so nothing can be normalized wrongly.
    OpenRouterAccuracy,
    /// `UnifiedBenchmarksAAItem.intelligence_index`, divided by 100.
    ///
    /// **The division has no upstream anchor and that is worth stating at the
    /// point of use.** Artificial Analysis's composite is documented only as
    /// "Higher is better" with no bound; across the 147 models carrying one in
    /// the 2026-08-24 `/models` snapshot the observed range was 5.5..=63.1, so
    /// dividing by 100 puts today's whole corpus under 0.64 and leaves the top
    /// of `quality_prior`'s scale empty. That is the conservative direction —
    /// the capability gate in `roundhouse-core/src/metrics/pricing.rs` compares
    /// priors to decide whether two models may be priced against each other, and
    /// a prior that is too low narrows what a model may be compared to rather
    /// than widening it. The alternative, dividing by the observed maximum,
    /// makes every number a function of which models happened to be listed on
    /// the fetch date: re-running the import next month would silently re-rank
    /// models nobody touched.
    ArtificialAnalysisIntelligenceIndex,
}

impl Basis {
    /// How the provenance file spells it.
    fn wire_name(self) -> &'static str {
        match self {
            Self::OpenRouterAccuracy => "openrouter.accuracy",
            Self::ArtificialAnalysisIntelligenceIndex => {
                "artificial_analysis.intelligence_index/100"
            }
        }
    }

    /// The sentence the provenance file carries beside the number.
    ///
    /// In the file and not only in this doc comment, for
    /// `codex_launch`'s reason: the person who needs to know what `0.631` means
    /// is reading the generated artefact, not this source.
    fn note(self) -> &'static str {
        match self {
            Self::OpenRouterAccuracy => {
                "Used verbatim. OpenRouter documents `accuracy` as an aggregate score from 0 \
                 to 1, which is the scale `quality_prior` is defined on."
            }
            Self::ArtificialAnalysisIntelligenceIndex => {
                "Divided by 100. Artificial Analysis publishes no upper bound for the \
                 Intelligence Index; 100 is a stated denominator rather than a measured \
                 maximum, chosen because normalizing by the observed maximum would make every \
                 prior a function of which models were listed on the fetch date."
            }
        }
    }
}

/// Why an import refused rather than writing a file.
///
/// Every variant refuses at *import* time, and that placement is the point.
/// `CatalogConfig::validate` would catch an off-scale prior later, but it would
/// report it as a catalog naming a bad model — sending an operator to edit the
/// file this tool just generated instead of to the item upstream that produced
/// it.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error(
        "the response is not the documented `{{data, meta}}` benchmarks envelope: {source}. \
         Check that the URL was `/benchmarks` and not `/models`, whose rows carry an \
         undated `benchmarks` block this tool deliberately refuses as an input"
    )]
    Envelope {
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "item {index} carries no `model_permaslug`, so there is no identity to key a \
         catalog entry on. The permaslug is the stable id; the request-time `id` an \
         alias resolves from is not"
    )]
    NoModelIdentity { index: usize },
    #[error(
        "`{model}` has no attribution: `meta.citation` is null (which is what a response \
         spanning several sources returns) and the item names no `source` discriminator to \
         attribute it by instead. OpenRouter states attribution is REQUIRED when \
         republishing this data, and the savings dashboard republishes it"
    )]
    NoAttribution { model: String },
    /// `publisher` and not `source`: `thiserror` reads a field literally named
    /// `source` as the error's `std::error::Error::source`, which a `String`
    /// cannot be. The name it forces is the clearer one anyway — the value is
    /// the `source` *discriminator*, which is a publisher, and the word
    /// "source" is doing three jobs across this file already.
    #[error(
        "`{model}` carries {count} `{publisher}` scores ({kinds}) and nothing here can choose \
         between them. Re-run with `--benchmark-type <one of them>`, or with `--source` to \
         narrow to one publisher: averaging them would invent a composite this tool has no \
         basis to define"
    )]
    AmbiguousScore {
        model: String,
        publisher: String,
        count: usize,
        kinds: String,
    },
    #[error(
        "`{model}` normalizes to {value} via {basis}, outside the 0.0..=1.0 capability \
         scale. Refused here rather than written out, because the catalog boundary would \
         later report it as a bad model in a file nobody hand-wrote"
    )]
    OutOfScale {
        model: String,
        basis: &'static str,
        value: f64,
    },
    #[error(
        "no item in the response carried a score this tool can normalize, so the fragment \
         would be empty. {skipped} item(s) were skipped: {reasons}"
    )]
    NothingToEmit { skipped: usize, reasons: String },
}

/// What a run of the tool is for: which catalog provider the entries belong to,
/// and where the body came from.
pub struct ImportRequest<'a> {
    /// The `provider` name each emitted entry is keyed under — the same name a
    /// `[providers]` section in the catalog defines. Not derived from the
    /// response: OpenRouter is the *benchmark* publisher here, and a deployment
    /// may serve those models through a provider it named something else.
    pub provider: &'a str,
    /// The URL the body was read from, recorded verbatim in the provenance.
    pub endpoint: &'a str,
    /// When this run fetched it, as Unix milliseconds.
    ///
    /// An argument, never a clock read inside [`convert`] — the same rule the
    /// fair-use ledger follows, and for the same reason: a function that reads
    /// a clock has an output no test can pin.
    pub fetched_at_ms: u64,
}

/// The two files, before they are written.
#[derive(Debug)]
pub struct Import {
    /// The catalog fragment: identity plus `quality_prior`, and nothing else.
    pub fragment: Value,
    /// The provenance record that makes the fragment republishable.
    pub provenance: Value,
}

/// The envelope, as much of it as this tool reads.
///
/// **No `deny_unknown_fields`, deliberately** — the inverse of the rule the
/// config shapes in `roundhouse-server` follow, because the direction of danger
/// is inverted. Those shapes read a file *an operator wrote*, where an unknown
/// key is a typo that silently widens something. This reads a document *a
/// vendor publishes*, where an unknown key is next quarter's feature: refusing
/// it would turn every upstream addition into a tool that stops working, with
/// nothing gained, since the fields below are the only ones consulted.
#[derive(Debug, Deserialize)]
struct BenchmarksResponse {
    data: Vec<Value>,
    meta: Meta,
}

#[derive(Debug, Deserialize)]
struct Meta {
    /// "ISO-8601 timestamp of when this data was last updated."
    as_of: String,
    /// "Dataset version."
    version: String,
    /// "Required attribution when republishing this data, or null when results
    /// span multiple sources (attribute each item individually by its `source`
    /// discriminator)."
    #[serde(default)]
    citation: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    task_type: Option<String>,
    #[serde(default)]
    model_count: Option<u64>,
}

/// One item's score, after the fields that decide it have been read.
struct Score {
    basis: Basis,
    /// The number as published, before normalization — kept so the provenance
    /// records what was read and not only what was computed.
    raw: f64,
    quality_prior: f64,
    source: String,
    /// `benchmark_type` for an OpenRouter eval; `None` for an index that has
    /// only one kind. This is what an ambiguity is named by.
    kind: Option<String>,
}

/// Why an item contributed nothing.
struct Skipped {
    model: String,
    reason: String,
}

/// Turn one response body into the fragment and its provenance.
pub fn convert(body: &str, request: &ImportRequest<'_>) -> Result<Import, ImportError> {
    let response: BenchmarksResponse =
        serde_json::from_str(body).map_err(|source| ImportError::Envelope { source })?;

    // Grouped by model, and `BTreeMap` rather than `HashMap` so two runs over
    // one body emit byte-identical files. A configuration generator whose
    // output reorders run to run cannot be diffed against what is already
    // deployed, which is the one review a generated file gets.
    let mut by_model: BTreeMap<String, Vec<Score>> = BTreeMap::new();
    let mut skipped: Vec<Skipped> = Vec::new();

    for (index, item) in response.data.iter().enumerate() {
        let model = item["model_permaslug"]
            .as_str()
            .ok_or(ImportError::NoModelIdentity { index })?
            .to_string();
        let source = item["source"].as_str().unwrap_or_default().to_string();
        match read_score(item, &source) {
            Some(score) => by_model.entry(model).or_default().push(score),
            None => skipped.push(Skipped {
                model,
                reason: format!(
                    "`{source}` item carries neither a numeric `accuracy` nor a non-null \
                     `intelligence_index`"
                ),
            }),
        }
    }

    let mut entries = Vec::new();
    let mut provenance_items = Vec::new();
    for (model, mut scores) in by_model {
        // P6's precedence, applied per model: an OpenRouter-native accuracy is
        // preferred over an Artificial Analysis index wherever both exist. This
        // is a resolution and not an ambiguity, which matters because the
        // unfiltered fetch *routinely* returns both for one model — the
        // endpoint's own schema example does. What is genuinely ambiguous is
        // two scores from one publisher, and that is refused below.
        scores.sort_by_key(|score| match score.basis {
            Basis::OpenRouterAccuracy => 0,
            Basis::ArtificialAnalysisIntelligenceIndex => 1,
        });
        let best = scores.first().expect("a model is only grouped by a score");
        let rivals: Vec<&Score> = scores
            .iter()
            .filter(|score| score.basis == best.basis)
            .collect();
        if rivals.len() > 1 {
            return Err(ImportError::AmbiguousScore {
                model,
                publisher: best.source.clone(),
                count: rivals.len(),
                kinds: rivals
                    .iter()
                    .map(|score| {
                        score
                            .kind
                            .clone()
                            .unwrap_or_else(|| "<unnamed>".to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        }

        if !(0.0..=1.0).contains(&best.quality_prior) {
            return Err(ImportError::OutOfScale {
                model,
                basis: best.basis.wire_name(),
                value: best.quality_prior,
            });
        }

        // Attribution, resolved per entry. `meta.citation` when the response
        // named one; otherwise the item's own `source` discriminator, which is
        // what the schema says to attribute by when a response spans sources.
        // An entry that can do neither is a refusal rather than an omission:
        // this file feeds a dashboard that republishes the number.
        let attribution = match (&response.meta.citation, best.source.as_str()) {
            (Some(citation), _) => json!({ "citation": citation, "source": best.source }),
            (None, "") => return Err(ImportError::NoAttribution { model }),
            (None, source) => json!({
                "citation": Value::Null,
                "source": source,
                "source_url": response.meta.source_url,
                "note": "`meta.citation` was null, which OpenRouter documents as what a \
                         response spanning several sources returns; the schema says to \
                         attribute each item individually by its `source` discriminator."
            }),
        };

        entries.push(json!({
            "provider": request.provider,
            "model": model,
            "quality_prior": best.quality_prior,
        }));
        provenance_items.push(json!({
            "provider": request.provider,
            "model": model,
            // Recorded under its own name as well as under `model`: the
            // permaslug is the id the *score* is keyed on upstream, while
            // `model` is what the catalog will dispatch with. They are the same
            // string today and a deployment that renamed one would need to know
            // which one it renamed.
            "model_permaslug": model,
            "quality_prior": best.quality_prior,
            "basis": best.basis.wire_name(),
            "basis_note": best.basis.note(),
            "raw_score": best.raw,
            "benchmark_type": best.kind,
            "attribution": attribution,
            // Deliberately absent: `pricing`. Both AA and Design Arena items
            // carry OpenRouter's per-token rate card and it would ride along for
            // free. A benchmark score is not a price and this tool has one job;
            // an importer that emitted both would be the shortest path to a rate
            // card entering the tree through the capability door, which is
            // exactly the separation `CLAUDE.md` asks these two fields to keep.
        }));
    }

    if entries.is_empty() {
        return Err(ImportError::NothingToEmit {
            skipped: skipped.len(),
            reasons: skipped
                .iter()
                .map(|item| format!("{}: {}", item.model, item.reason))
                .collect::<Vec<_>>()
                .join("; "),
        });
    }

    let fragment = json!({
        "$comment": [
            "GENERATED by roundhouse-fleet's `import-benchmarks`. Not a catalog: every entry \
             below carries a model identity and a quality_prior and nothing else.",
            "Merge each quality_prior onto the matching entry of your own catalog. Pasting \
             this file in whole will not load -- a catalog entry also needs pricing, a cache \
             model and latency figures, and this tool has no basis to invent any of them.",
            "The paired provenance file is what makes these numbers republishable: it carries \
             the dataset version, the upstream as-of date, the attribution, and what was done \
             to each raw score. Keep the two together."
        ],
        "models": entries,
    });

    let provenance = json!({
        "$comment": [
            "Provenance for the catalog fragment generated beside this file. OpenRouter \
             states attribution is REQUIRED when republishing benchmark data; roundhouse's \
             savings dashboard republishes it, so this file travels with the numbers."
        ],
        "endpoint": request.endpoint,
        "fetched_at_unix_ms": request.fetched_at_ms,
        "fetched_at": rfc3339_utc(request.fetched_at_ms),
        "meta": {
            // Verbatim from the response. `as_of` is the dataset's date and
            // `fetched_at` above is only when we pulled it; conflating the two
            // would let a re-run look like fresher data.
            "version": response.meta.version,
            "as_of": response.meta.as_of,
            "citation": response.meta.citation,
            "source": response.meta.source,
            "source_url": response.meta.source_url,
            "task_type": response.meta.task_type,
            "model_count": response.meta.model_count,
        },
        "entries": provenance_items,
        "skipped": skipped
            .iter()
            .map(|item| json!({ "model": item.model, "reason": item.reason }))
            .collect::<Vec<_>>(),
    });

    Ok(Import {
        fragment,
        provenance,
    })
}

/// The one score this item offers, if it offers one.
///
/// `accuracy` is checked before `intelligence_index` so that the precedence is
/// the same whether a model has one item or several. The two are read off the
/// same `Value` rather than through a `oneOf`-shaped enum because the
/// discriminator that would select the variant is `source`, and an item whose
/// `source` is a value this tool has never heard of should still contribute its
/// score if it carries one — an enum would refuse the whole document instead.
fn read_score(item: &Value, source: &str) -> Option<Score> {
    if let Some(accuracy) = item["accuracy"].as_f64() {
        return Some(Score {
            basis: Basis::OpenRouterAccuracy,
            raw: accuracy,
            quality_prior: accuracy,
            source: source.to_string(),
            kind: item["benchmark_type"].as_str().map(str::to_string),
        });
    }
    // Nullable in the schema (`type: ["number", "null"]`), and null is common:
    // of the 416 models in the 2026-08-24 `/models` snapshot only 147 carried
    // one. `as_f64` returns `None` for both null and absent, which is the same
    // answer for this tool's purposes.
    if let Some(index) = item["intelligence_index"].as_f64() {
        return Some(Score {
            basis: Basis::ArtificialAnalysisIntelligenceIndex,
            raw: index,
            quality_prior: index / 100.0,
            source: source.to_string(),
            kind: None,
        });
    }
    None
}

/// Unix milliseconds as an RFC 3339 UTC timestamp.
///
/// Hand-rolled rather than pulling a date crate into a graph that has none:
/// this is the only date arithmetic anywhere in the workspace, it is needed by
/// one generated file, and a dependency added for one line in one binary is a
/// dependency the whole workspace then resolves against. The civil-from-days
/// conversion is Howard Hinnant's `civil_from_days`, which is exact for every
/// day in the proleptic Gregorian calendar and has no leap-second concept —
/// correct here, since Unix time has none either.
///
/// The integer is written beside this in the provenance file, so a reader who
/// distrusts this function has the unambiguous number too.
fn rfc3339_utc(ms: u64) -> String {
    let seconds = (ms / 1000) as i64;
    let (days, seconds_of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Shift the epoch to 0000-03-01 so leap day lands at the end of the "year".
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A committed snapshot of the response shape, built field by field from
    /// `https://openrouter.ai/openapi.json` as fetched 2026-08-24.
    ///
    /// **Schema-derived, not captured** — and that distinction is recorded in
    /// the file itself rather than only here. `GET /api/v1/benchmarks` requires
    /// a key, the M10 research round deliberately made no keyed call, and a
    /// fixture that implied otherwise would be the kind of provenance claim
    /// this whole tool exists to make honest. Every field, and the two item
    /// shapes, come from the spec's own `example` blocks for
    /// `UnifiedBenchmarksResponse`, `UnifiedBenchmarksAAItem`,
    /// `UnifiedBenchmarksORItem` and `UnifiedBenchmarksDAItem`.
    const SNAPSHOT: &str = include_str!("../../../fixtures/openrouter-benchmarks.json");

    fn request() -> ImportRequest<'static> {
        ImportRequest {
            provider: "openrouter",
            endpoint: "https://openrouter.ai/api/v1/benchmarks",
            // 2026-08-24T00:00:00Z.
            fetched_at_ms: 1_787_529_600_000,
        }
    }

    fn imported() -> Import {
        convert(SNAPSHOT, &request()).expect("the committed snapshot imports")
    }

    fn entry<'a>(provenance: &'a Value, model: &str) -> &'a Value {
        provenance["entries"]
            .as_array()
            .expect("entries is an array")
            .iter()
            .find(|item| item["model"] == model)
            .unwrap_or_else(|| panic!("no provenance entry for `{model}`"))
    }

    /// The plan's named test for this rung.
    ///
    /// All three provenance facts in one assertion because they are one claim:
    /// a number without them is not republishable. `meta.citation` being null
    /// in this fixture is the *interesting* case rather than a gap — it is what
    /// a response spanning several sources returns, which is what an unfiltered
    /// fetch is, so an import that demanded a non-null `meta.citation` would
    /// refuse its own default request. The attribution therefore has to be
    /// resolvable per entry, and that is what is asserted.
    #[test]
    fn an_imported_quality_prior_carries_its_version_date_and_citation() {
        let import = imported();
        let meta = &import.provenance["meta"];
        assert_eq!(meta["version"], "v1");
        assert_eq!(meta["as_of"], "2026-06-03T12:00:00Z");
        assert!(
            meta["citation"].is_null(),
            "this fixture spans sources, which is the case that makes per-entry attribution \
             load-bearing"
        );
        // Our own fetch date is recorded separately from the dataset's, so a
        // re-run cannot make stale data look fresh.
        assert_eq!(import.provenance["fetched_at"], "2026-08-24T00:00:00Z");
        assert_ne!(meta["as_of"], import.provenance["fetched_at"]);

        for item in import.provenance["entries"]
            .as_array()
            .expect("entries is an array")
        {
            let attribution = &item["attribution"];
            let attributable = attribution["citation"].is_string()
                || attribution["source"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty());
            assert!(
                attributable,
                "every entry must be attributable, by `meta.citation` or by its own source \
                 discriminator: {item}"
            );
        }
    }

    /// A single-source fetch is the other half of the same claim: when the
    /// response *does* carry a citation, every entry carries that citation.
    ///
    /// Built by editing the snapshot rather than committing a second file: the
    /// only difference that matters is one `meta` field, and two fixtures
    /// differing in one line is how the two drift.
    #[test]
    fn a_single_source_fetch_puts_the_responses_own_citation_on_every_entry() {
        let citation = "Source: Artificial Analysis (artificialanalysis.ai) via OpenRouter.";
        let mut body: Value = serde_json::from_str(SNAPSHOT).unwrap();
        body["meta"]["citation"] = json!(citation);
        let import = convert(&body.to_string(), &request()).expect("imports");
        for item in import.provenance["entries"].as_array().unwrap() {
            assert_eq!(item["attribution"]["citation"], citation);
        }
    }

    /// The two normalizations, and the record of which was used.
    ///
    /// One assertion over both because the bug this guards is picking the wrong
    /// one, which is only visible by comparison: 0.72 and 0.631 are both
    /// plausible priors, and only the recorded basis says which arithmetic
    /// produced them.
    #[test]
    fn an_openrouter_accuracy_is_verbatim_and_an_intelligence_index_is_divided_by_a_hundred() {
        let import = imported();
        let native = entry(&import.provenance, "deepseek/deepseek-v4-flash-0731");
        assert_eq!(native["quality_prior"], 0.72);
        assert_eq!(native["raw_score"], 0.72);
        assert_eq!(native["basis"], "openrouter.accuracy");

        let indexed = entry(&import.provenance, "moonshotai/kimi-k3");
        assert_eq!(indexed["quality_prior"], 0.631);
        assert_eq!(indexed["raw_score"], 63.1);
        assert_eq!(
            indexed["basis"],
            "artificial_analysis.intelligence_index/100"
        );
        // The denominator is stated in the artefact, not only in this crate.
        // A reader deciding whether to trust 0.631 is deciding whether to trust
        // that sentence.
        assert!(
            indexed["basis_note"]
                .as_str()
                .expect("a note")
                .contains("no upper bound")
        );
    }

    /// Where both exist for one model, the accuracy wins.
    ///
    /// The unfiltered fetch's ordinary case — the endpoint's own schema example
    /// returns `openai/gpt-4o` twice, once per source — so this is not an edge
    /// case but the default path. The fixture carries the same collision.
    #[test]
    fn a_model_scored_by_both_publishers_takes_the_one_already_on_our_scale() {
        let import = imported();
        let both = entry(&import.provenance, "openai/gpt-5.6-sol");
        assert_eq!(both["basis"], "openrouter.accuracy");
        assert_eq!(both["quality_prior"], 0.81);
        // The control: the AA index for the same model is 74.0, so a
        // precedence that went the other way would read 0.74 here.
        assert_ne!(both["quality_prior"], 0.74);
    }

    /// Two scores from one publisher are a refusal, not an average.
    ///
    /// The refusal names the flag that resolves it. An average would be a
    /// composite index this tool has no basis to define, and it would be
    /// indistinguishable afterwards from a published number.
    #[test]
    fn two_scores_from_one_publisher_are_refused_and_the_remedy_is_named() {
        let mut body: Value = serde_json::from_str(SNAPSHOT).unwrap();
        let second = json!({
            "source": "openrouter",
            "model_permaslug": "deepseek/deepseek-v4-flash-0731",
            "display_name": "DeepSeek V4 Flash",
            "benchmark_type": "tau_bench_verified_airline",
            "accuracy": 0.41,
            "accuracy_stddev": 0.02,
            "avg_cost_per_task": 0.001,
            "total_tasks": 120,
            "last_run_timestamp": "2026-06-03T12:00:00Z"
        });
        body["data"].as_array_mut().unwrap().push(second);
        let error = convert(&body.to_string(), &request()).expect_err("ambiguous");
        let message = error.to_string();
        assert!(
            message.contains("deepseek/deepseek-v4-flash-0731"),
            "{message}"
        );
        assert!(message.contains("gpqa_diamond"), "{message}");
        assert!(message.contains("tau_bench_verified_airline"), "{message}");
        assert!(message.contains("--benchmark-type"), "{message}");
        // The control: the same body without the second score imports fine, so
        // the refusal is about the collision and not about the fixture.
        imported();
    }

    /// An off-scale score is refused here, where the item can be named.
    ///
    /// The failure this prevents is a silent scale change upstream — an index
    /// republished on 0–1000, say. Divided by 100 that is 8.4, which
    /// `CatalogConfig::validate` would eventually refuse with a message about a
    /// model in a generated file, one layer too far from the cause.
    #[test]
    fn a_score_off_the_capability_scale_is_refused_at_import_and_names_the_item() {
        let mut body: Value = serde_json::from_str(SNAPSHOT).unwrap();
        for item in body["data"].as_array_mut().unwrap() {
            if item["model_permaslug"] == "moonshotai/kimi-k3" {
                item["intelligence_index"] = json!(840.0);
            }
        }
        let error = convert(&body.to_string(), &request()).expect_err("off scale");
        let message = error.to_string();
        assert!(message.contains("moonshotai/kimi-k3"), "{message}");
        assert!(message.contains("8.4"), "{message}");
    }

    /// The fragment carries identity and a prior, and nothing else.
    ///
    /// Especially not a price. Both AA and Design Arena items carry
    /// OpenRouter's per-token rate card, so emitting it would cost nothing and
    /// would put a rate card into a generated config file — the one thing this
    /// codebase refuses to source from anywhere but a deployment's own catalog.
    #[test]
    fn a_benchmark_score_never_becomes_a_price() {
        let import = imported();
        // Scanned over the *data* rather than the whole document: both files
        // carry `$comment` prose that names pricing in order to say the
        // fragment is not a catalog, and a substring match over that would be a
        // test that fails on its own documentation.
        let data = format!(
            "{}{}",
            import.fragment["models"], import.provenance["entries"]
        );
        for banned in ["pricing", "completion", "prompt", "avg_cost_per_task"] {
            assert!(
                !data.contains(banned),
                "`{banned}` reached a generated file; every AA and Design Arena item in the \
                 fixture carries OpenRouter's rate card, and letting it ride along is how a \
                 price enters the tree through the capability door:\n{data}"
            );
        }
        for model in import.fragment["models"].as_array().expect("models") {
            // Sorted, because `serde_json`'s object is a `BTreeMap` and the
            // emitted order is alphabetical rather than declaration order. What
            // is being pinned is the *set* of keys, not their order.
            let keys: Vec<&str> = model
                .as_object()
                .expect("an object")
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                keys,
                vec!["model", "provider", "quality_prior"],
                "the fragment is identity plus one number; anything else is a claim this \
                 tool did not import"
            );
        }
    }

    /// An item with no usable score is skipped by name, never silently.
    ///
    /// Design Arena publishes an ELO and a win rate, neither of which is a
    /// 0..=1 capability score, and Artificial Analysis's index is nullable. A
    /// tool that dropped both without saying so would answer "why is this model
    /// missing?" with nothing.
    #[test]
    fn an_item_with_no_usable_score_is_named_in_the_provenance_rather_than_dropped() {
        let import = imported();
        let skipped: Vec<&str> = import.provenance["skipped"]
            .as_array()
            .expect("skipped is an array")
            .iter()
            .map(|item| item["model"].as_str().expect("a model"))
            .collect();
        assert!(
            skipped.contains(&"anthropic/claude-sonnet-4"),
            "{skipped:?}"
        );
        assert!(skipped.contains(&"mistral/unscored-preview"), "{skipped:?}");
        // And they are absent from the fragment, rather than emitted with a
        // guessed prior.
        let emitted: Vec<&str> = import.fragment["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["model"].as_str().unwrap())
            .collect();
        assert!(!emitted.contains(&"anthropic/claude-sonnet-4"));
    }

    /// A body with nothing importable refuses rather than writing an empty
    /// fragment, and says what it skipped.
    #[test]
    fn a_response_with_no_usable_score_refuses_rather_than_emitting_an_empty_file() {
        let body = json!({
            "data": [{
                "source": "design-arena",
                "model_permaslug": "anthropic/claude-sonnet-4",
                "display_name": "Claude Sonnet 4",
                "elo": 1423,
                "win_rate": 72
            }],
            "meta": {
                "as_of": "2026-06-03T12:00:00Z", "version": "v1", "citation": null,
                "source": null, "source_url": null, "task_type": null, "model_count": 1
            }
        });
        let error = convert(&body.to_string(), &request()).expect_err("nothing to emit");
        assert!(error.to_string().contains("anthropic/claude-sonnet-4"));
    }

    /// An item with no attribution at all is refused.
    ///
    /// Reachable only from a malformed response — `source` is required by the
    /// schema — and checked anyway, because the check is what makes "everything
    /// we republish is attributed" a property of the output rather than a
    /// property of the vendor's current spec.
    #[test]
    fn an_entry_that_cannot_be_attributed_is_refused_rather_than_republished() {
        let body = json!({
            "data": [{ "model_permaslug": "x/y", "accuracy": 0.5 }],
            "meta": {
                "as_of": "2026-06-03T12:00:00Z", "version": "v1", "citation": null,
                "source": null, "source_url": null, "task_type": null, "model_count": 1
            }
        });
        let error = convert(&body.to_string(), &request()).expect_err("unattributable");
        assert!(error.to_string().contains("x/y"));
        assert!(error.to_string().contains("REQUIRED"));
    }

    /// The undated `/models` rows are refused as an input by shape.
    ///
    /// R8 says the models-list scores are not an acceptable source: they carry
    /// no `version`, no `as_of` and no citation. Nothing has to check for that
    /// specially — a `/models` body has no `meta`, so the envelope refuses it —
    /// but the refusal has to *say* so, since "expected `meta`" would send an
    /// operator looking for a bug in this tool.
    #[test]
    fn a_models_list_body_is_refused_with_the_reason_it_is_not_an_input() {
        let body = json!({ "data": [{ "id": "openai/gpt-4o", "benchmarks": {} }] });
        let error = convert(&body.to_string(), &request()).expect_err("wrong endpoint");
        assert!(error.to_string().contains("/models"), "{error}");
    }

    /// Two runs over one body produce byte-identical files.
    ///
    /// The only review a generated config file gets is a diff against the one
    /// already deployed, and a map iteration order that varied run to run would
    /// make every diff unreadable.
    #[test]
    fn the_same_body_imports_to_the_same_bytes_twice() {
        let first = imported();
        let second = imported();
        assert_eq!(first.fragment.to_string(), second.fragment.to_string());
        assert_eq!(first.provenance.to_string(), second.provenance.to_string());
    }

    /// The timestamp rendering, against dates whose answers are known
    /// independently.
    #[test]
    fn the_fetch_timestamp_renders_as_utc() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_787_529_600_000), "2026-08-24T00:00:00Z");
        // A leap day, which is what the civil-from-days shift exists for.
        assert_eq!(rfc3339_utc(1_709_164_800_000), "2024-02-29T00:00:00Z");
        assert_eq!(
            rfc3339_utc(1_787_529_600_000 + 45_296_000),
            "2026-08-24T12:34:56Z"
        );
    }
}
