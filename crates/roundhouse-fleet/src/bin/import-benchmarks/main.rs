// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `import-benchmarks` — turn OpenRouter's published benchmark index into a
//! catalog fragment carrying sourced `quality_prior`s, and the provenance
//! record that makes those numbers republishable.
//!
//! # Why this exists
//!
//! `FrontierModelSpec::quality_prior` says of itself "Configuration, not
//! measurement", and it is the number the capability gate in
//! `roundhouse-core/src/metrics/pricing.rs` compares when it decides whether two
//! models may be priced against each other. That gate is the only thing
//! stopping a small local model being priced against a flagship, and today it
//! is defended by a hand-written figure. Sourcing the figure from a published,
//! versioned index makes the gate defensible rather than asserted — which is
//! `CLAUDE.md`'s "Cost and pricing data" ruling, applied to the half of a
//! catalog entry that is not a price.
//!
//! # Why it is a tool and not a runtime path
//!
//! Same rule as the rate card: prices — and now capability priors — are not in
//! source and are not fetched by a serving process. A router that resolved
//! `quality_prior` over the network would make every routing decision depend on
//! a third party's uptime and would re-rank models mid-deployment when an
//! upstream leaderboard moved. This binary writes files an operator reads,
//! reviews, and merges into their own catalog. It is not linked into the
//! library: `src/bin/import-benchmarks/` is a binary target with its own
//! modules, so nothing in `roundhouse-fleet`'s public surface can call it and
//! nothing in a shipped `roundhouse` binary contains it.
//!
//! # Usage
//!
//! ```text
//! OPENROUTER_API_KEY=... import-benchmarks \
//!     --provider openrouter \
//!     --out quality-prior.fragment.json \
//!     --provenance quality-prior.provenance.json \
//!     [--source artificial-analysis|design-arena|openrouter] \
//!     [--benchmark-type gpqa_diamond|tau_bench_verified_airline|...] \
//!     [--task-type coding|intelligence|agentic|search] \
//!     [--base-url https://openrouter.ai/api/v1] \
//!     [--from-file saved-response.json]
//! ```
//!
//! `--from-file` reads a body that was already fetched instead of calling the
//! route. It is not a convenience: `GET /api/v1/benchmarks` is rate-limited to
//! 30 requests a minute and 500 a day per account (its own OpenAPI
//! description), and re-running an import against a saved body while tuning
//! `--provider` or a filter is how an operator stays under that without
//! learning about it from a 429.
//!
//! Arguments are parsed by hand rather than with a CLI crate. Eight flags, all
//! `--name value`, in a binary nothing else depends on — a dependency added
//! here is one the whole workspace resolves against, which is the trade
//! `roundhouse-fleet`'s manifest already makes explicit for `bytes`.

mod convert;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use convert::{ImportRequest, convert};

/// Where the benchmarks route lives, unless `--base-url` says otherwise.
const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// The route itself, relative to the base.
const BENCHMARKS_ROUTE: &str = "/benchmarks";

/// The variable the key arrives in — the same name a catalog `[providers]`
/// entry for OpenRouter declares in its `auth.env`.
const KEY_VAR: &str = "OPENROUTER_API_KEY";

struct Options {
    provider: String,
    out: PathBuf,
    provenance: PathBuf,
    base_url: String,
    source: Option<String>,
    benchmark_type: Option<String>,
    task_type: Option<String>,
    from_file: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            provider: "openrouter".to_string(),
            out: PathBuf::from("quality-prior.fragment.json"),
            provenance: PathBuf::from("quality-prior.provenance.json"),
            base_url: DEFAULT_BASE_URL.to_string(),
            source: None,
            benchmark_type: None,
            task_type: None,
            from_file: None,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            // To stderr and with a non-zero code, because the ordinary way this
            // is run is inside somebody's shell pipeline that then copies the
            // fragment somewhere. A tool that printed a refusal to stdout and
            // exited zero would have that pipeline copy the refusal.
            eprintln!("import-benchmarks: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let options = parse_args(std::env::args().skip(1).collect())?;
    let endpoint = request_url(&options);

    let body = match &options.from_file {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|error| format!("could not read `{}`: {error}", path.display()))?,
        None => fetch(&endpoint).await?,
    };

    let import = convert(
        &body,
        &ImportRequest {
            provider: &options.provider,
            endpoint: &endpoint,
            fetched_at_ms: now_ms(),
        },
    )
    .map_err(|error| error.to_string())?;

    write_json(&options.out, &import.fragment)?;
    write_json(&options.provenance, &import.provenance)?;

    let count = import.fragment["models"]
        .as_array()
        .map(Vec::len)
        .unwrap_or_default();
    let skipped = import.provenance["skipped"]
        .as_array()
        .map(Vec::len)
        .unwrap_or_default();
    println!(
        "wrote {count} quality_prior entr{} to {} ({skipped} item(s) skipped; see {})",
        if count == 1 { "y" } else { "ies" },
        options.out.display(),
        options.provenance.display()
    );
    // Said on every successful run rather than in the README only. The file
    // this tool writes is the one that gets copied around; the obligation
    // travels with it, and an operator who never reads a README still sees this.
    println!(
        "The provenance file carries the attribution OpenRouter requires when this data is \
         republished. Keep the two files together."
    );
    Ok(())
}

/// The URL this run reads, with the filters the operator asked for.
///
/// Built as a plain string rather than through a URL type: the three optional
/// filters are the only query parameters, their accepted values are enum
/// members with no characters needing escaping, and the string is recorded
/// verbatim in the provenance so an operator can re-issue exactly this request.
fn request_url(options: &Options) -> String {
    let mut url = format!(
        "{}{BENCHMARKS_ROUTE}",
        options.base_url.trim_end_matches('/')
    );
    let filters = [
        ("source", &options.source),
        ("benchmark_type", &options.benchmark_type),
        ("task_type", &options.task_type),
    ];
    let mut first = true;
    for (name, value) in filters {
        if let Some(value) = value {
            url.push(if first { '?' } else { '&' });
            url.push_str(&format!("{name}={value}"));
            first = false;
        }
    }
    url
}

/// One authenticated GET.
///
/// The key is read here and nowhere else, and it is never written into either
/// output file — for the reason `codex_launch` states about its own generated
/// files, sharpened by what these two are for: a provenance record is meant to
/// be attached to a dashboard and shown to somebody.
async fn fetch(url: &str) -> Result<String, String> {
    let key = std::env::var(KEY_VAR).map_err(|_| {
        format!(
            "{KEY_VAR} is not set. `GET {BENCHMARKS_ROUTE}` requires a key (any valid \
             OpenRouter key); the unauthenticated `/models` route carries benchmark scores \
             too, but they are undated and unversioned, which is exactly what this tool \
             refuses as an input"
        )
    })?;
    let response = reqwest::Client::new()
        .get(url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|error| format!("GET {url} failed: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("reading the body of GET {url} failed: {error}"))?;
    if !status.is_success() {
        // The body is included because OpenRouter's error envelope carries the
        // reason (`{"error": {"code", "message", "metadata"}}`) and a bare
        // status here would send an operator to guess between an expired key,
        // a spent daily quota and a filter value the route does not accept.
        return Err(format!("GET {url} answered {status}: {body}"));
    }
    Ok(body)
}

/// Pretty, newline-terminated, and refusing to clobber silently is *not* done —
/// the file is an output an operator names, and a tool that refused to
/// overwrite its own previous output would make re-running it after fixing a
/// filter a two-step chore.
fn write_json(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|error| format!("could not encode `{}`: {error}", path.display()))?;
    text.push('\n');
    std::fs::write(path, text)
        .map_err(|error| format!("could not write `{}`: {error}", path.display()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

/// `--name value` pairs, refusing anything else.
///
/// An unknown flag is a refusal rather than a warning: the flags that matter
/// here narrow *what gets imported*, and a misspelled `--sorce` that was
/// ignored would produce a full unfiltered import that looks exactly like the
/// filtered one the operator asked for.
fn parse_args(args: Vec<String>) -> Result<Options, String> {
    let mut options = Options::default();
    let mut rest = args.into_iter();
    while let Some(flag) = rest.next() {
        if flag == "--help" || flag == "-h" {
            return Err(USAGE.to_string());
        }
        let value = rest
            .next()
            .ok_or_else(|| format!("`{flag}` needs a value\n\n{USAGE}"))?;
        match flag.as_str() {
            "--provider" => options.provider = value,
            "--out" => options.out = PathBuf::from(value),
            "--provenance" => options.provenance = PathBuf::from(value),
            "--base-url" => options.base_url = value,
            "--source" => options.source = Some(value),
            "--benchmark-type" => options.benchmark_type = Some(value),
            "--task-type" => options.task_type = Some(value),
            "--from-file" => options.from_file = Some(PathBuf::from(value)),
            other => return Err(format!("unknown flag `{other}`\n\n{USAGE}")),
        }
    }
    Ok(options)
}

const USAGE: &str = "\
usage: import-benchmarks [options]

  --provider <name>        catalog provider these entries belong to (default: openrouter)
  --out <path>             catalog fragment to write (default: quality-prior.fragment.json)
  --provenance <path>      provenance record to write (default: quality-prior.provenance.json)
  --source <s>             artificial-analysis | design-arena | openrouter
  --benchmark-type <t>     gpqa_diamond | tau_bench_verified_airline | search_*
  --task-type <t>          coding | intelligence | agentic | search
  --base-url <url>         default https://openrouter.ai/api/v1
  --from-file <path>       import a saved response instead of fetching

Reads OPENROUTER_API_KEY from the environment unless --from-file is given.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_name_the_route_the_ruling_points_at() {
        let options = Options::default();
        assert_eq!(
            request_url(&options),
            "https://openrouter.ai/api/v1/benchmarks",
            "R8 names the versioned surface; the undated `/models` scores are not an input"
        );
    }

    /// Filters reach the URL, and the URL is what the provenance records.
    #[test]
    fn each_filter_reaches_the_query_string_in_the_order_it_was_declared() {
        let mut options = Options {
            source: Some("openrouter".to_string()),
            benchmark_type: Some("gpqa_diamond".to_string()),
            ..Default::default()
        };
        assert_eq!(
            request_url(&options),
            "https://openrouter.ai/api/v1/benchmarks?source=openrouter&benchmark_type=gpqa_diamond"
        );
        // A trailing slash on a copy-pasted base is normalised rather than
        // doubling the separator, which some gateways route on exactly.
        options.base_url = "https://gateway.example.com/api/v1/".to_string();
        assert!(
            request_url(&options).starts_with("https://gateway.example.com/api/v1/benchmarks?")
        );
    }

    /// A misspelled flag is a refusal.
    ///
    /// The failure it prevents is the quiet one: an ignored `--sorce` imports
    /// every source, which is a *valid* import that is not the one asked for,
    /// and the provenance would faithfully record the request that was actually
    /// made rather than the one that was meant.
    #[test]
    fn a_misspelled_filter_is_refused_rather_than_widening_the_import() {
        let error = parse_args(vec!["--sorce".into(), "openrouter".into()])
            .err()
            .expect("unknown flags are refused");
        assert!(error.contains("--sorce"));
        // The control: spelled right, it parses.
        let options = parse_args(vec!["--source".into(), "openrouter".into()])
            .expect("the flag exists when spelled correctly");
        assert_eq!(options.source.as_deref(), Some("openrouter"));
    }

    #[test]
    fn a_flag_with_no_value_names_itself() {
        let error = parse_args(vec!["--provider".into()])
            .err()
            .expect("a dangling flag is refused");
        assert!(error.contains("--provider"));
    }
}
