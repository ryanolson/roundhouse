// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What the unkeyed chain costs next to the tokenization it rides beside.
//!
//! **Nothing in production extends the chain yet.** The server's cache-hint
//! fingerprint is its own digest (`request_context::prefix_fingerprint`), not a
//! chain. Extending the chain in the context assembler from the render it
//! already computes, once per item, on every turn — the same loop that encodes
//! each render into token ids — is the change this anticipates. The expectation
//! is that the chain will be noise against that encode; this measures the chain
//! against the encode it will sit beside, on real Claude Code bodies with a
//! real BPE vocabulary, per 100 KiB of rendered text, so the claim is a number
//! rather than an intuition about SHA-256 being fast — and is a number before
//! the wiring exists, when a surprising answer still costs a design change
//! rather than a revert.
//!
//! It lives under `tokenizer` as a lib unit test because that puts it beside the
//! `HfTokenizer` it measures against, and because an integration test would be
//! another binary to link for one ignored timing.
//!
//! An `#[ignore]` test rather than a criterion bench because the workspace has
//! no bench harness and one timing is not worth adding it for. Run it with
//!
//! ```text
//! timeout 900 cargo test -p roundhouse-server --lib chain_cost -- --ignored --nocapture
//! ```
//!
//! and read the three lines it prints. It asserts nothing about speed — a
//! timing assertion is a flaky test on a loaded box — only that the corpus is
//! the size it claims and that the chain agrees with itself, so the number
//! printed is the cost of the work and not of a no-op the optimizer found.

use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use roundhouse_core::context::Tokenizer;
use roundhouse_core::item::Item;
use roundhouse_core::item::chain::Chain;

use super::HfTokenizer;
use crate::messages_api::wire::{CreateMessageParams, canonicalize};

/// The seven Claude Code Messages bodies, as captured.
const BODIES: [&str; 7] = [
    include_str!("../../tests/fixtures/claude-2.1.251-turn-1.json"),
    include_str!("../../tests/fixtures/claude-2.1.251-turn-2-continue.json"),
    include_str!("../../tests/fixtures/claude-2.1.257-turn-1.json"),
    include_str!("../../tests/fixtures/claude-2.1.257-turn-2-continue.json"),
    include_str!("../../tests/fixtures/claude-2.1.257-turn-3-continue.json"),
    include_str!("../../tests/fixtures/claude-2.1.257-mcp-turn-1.json"),
    include_str!("../../tests/fixtures/claude-2.1.257-mcp-turn-2-toolresult.json"),
];

/// The unit the numbers are reported in.
const HUNDRED_KIB: usize = 100 * 1024;

/// Every body's canonical items, repeated until their renders reach 100 KiB.
///
/// Repeated whole rather than truncated, so the item-size mix — many short
/// turns, a few long system blocks and tool results — is the captures' own.
/// The tools array is not here: it enters only the keyed tip, never the chain,
/// and it is not rendered into the token stream item by item either.
fn corpus() -> Vec<Item> {
    let items: Vec<Item> = BODIES
        .iter()
        .flat_map(|body| {
            let params: CreateMessageParams =
                serde_json::from_str(body).expect("a captured Messages body");
            canonicalize(&params).expect("a captured body canonicalizes")
        })
        .collect();
    let mut corpus = Vec::new();
    let mut bytes = 0;
    while bytes < HUNDRED_KIB {
        for item in &items {
            bytes += item.render().len();
            corpus.push(item.clone());
        }
    }
    corpus
}

/// The fastest of `runs` timings: the one least disturbed by the rest of the
/// box, which is the cost of the work itself.
fn fastest(runs: usize, mut work: impl FnMut()) -> Duration {
    (0..runs)
        .map(|_| {
            let start = Instant::now();
            work();
            start.elapsed()
        })
        .min()
        .expect("at least one run")
}

fn per_hundred_kib(elapsed: Duration, bytes: usize) -> f64 {
    elapsed.as_secs_f64() * 1e6 * HUNDRED_KIB as f64 / bytes as f64
}

#[test]
#[ignore = "a timing report, not a check: run with --ignored --nocapture"]
fn chain_time_against_tokenization_time_per_100_kib() {
    let tokenizer = HfTokenizer::from_file(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/tinyllama-tokenizer.json"),
    )
    .expect("fixture tokenizer must load");
    let items = corpus();
    let renders: Vec<String> = items.iter().map(Item::render).collect();
    let bytes: usize = renders.iter().map(String::len).sum();
    assert!(bytes >= HUNDRED_KIB, "the corpus is {bytes} bytes");

    // What the context assembler adds: one link per render it already holds.
    let chain_rendered = fastest(50, || {
        let mut chain = Chain::default();
        for render in &renders {
            black_box(chain.push_rendered(black_box(render)));
        }
        black_box(&chain);
    });
    // A caller with only items pays the render too.
    let chain_items = fastest(50, || {
        black_box(Chain::over(black_box(&items)));
    });
    // The encode the same loop already does, render by render.
    let mut tokens = 0;
    let tokenize = fastest(5, || {
        tokens = 0;
        for render in &renders {
            tokens += black_box(tokenizer.encode(black_box(render))).len();
        }
    });

    // The chain is a function of the renders, whichever way it was built.
    let mut from_renders = Chain::default();
    for render in &renders {
        from_renders.push_rendered(render);
    }
    assert_eq!(
        Chain::over(&items).agreed_len(&from_renders),
        items.len(),
        "the two builds must agree on every link"
    );

    let chain_rendered = per_hundred_kib(chain_rendered, bytes);
    let chain_items = per_hundred_kib(chain_items, bytes);
    let tokenize = per_hundred_kib(tokenize, bytes);
    println!(
        "corpus: {} items, {bytes} rendered bytes, {tokens} tokens (TinyLlama)",
        items.len()
    );
    println!(
        "per 100 KiB: chain over renders {chain_rendered:.0} us, chain over items \
         {chain_items:.0} us, tokenization {tokenize:.0} us"
    );
    println!(
        "chain / tokenization: {:.2}% (over renders), {:.2}% (over items)",
        100.0 * chain_rendered / tokenize,
        100.0 * chain_items / tokenize
    );
}
