// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Fixtures shared by the fair-use ledger's themed test binaries
//! (`fair_use_contract.rs`, `fair_use_decay.rs`, `fair_use_storage.rs`).
//!
//! M13.1's review (F1) found these copy-pasted into each binary rather than
//! shared here, alongside a fourth copy of the generic raw-connection helper
//! (`raw_from_env`, which lives in `common` itself rather than here, since
//! `spend_contract.rs` needs it too and nothing about it is fair-use
//! specific). Each of the three binaries compiles its own copy of this module
//! via `common`'s own `mod fair_use;`, and none uses every item, so
//! `#![allow(dead_code)]` at the crate root of `common` covers this file too
//! rather than an `allow` per unused item.

use roundhouse_core::control::{FairUseLimit, FairUseTerms, FairUseWindow};
use roundhouse_store_redis::RedisFairUseLedger;
use roundhouse_store_redis::test_support::{
    fair_use_bucket_fields, fair_use_window_sum_fields, url_from_env,
};

pub async fn connect_fair_use_from_env() -> RedisFairUseLedger {
    RedisFairUseLedger::connect(url_from_env())
        .await
        .expect("Redis named by the env var must be reachable")
}

pub fn tokens_cap(window: FairUseWindow, max: u64) -> FairUseLimit {
    FairUseLimit {
        window,
        max_tokens: Some(max),
        max_usd: None,
    }
}

pub fn usd_cap(window: FairUseWindow, max: f64) -> FairUseLimit {
    FairUseLimit {
        window,
        max_tokens: None,
        max_usd: Some(max),
    }
}

pub fn project_only(limits: Vec<FairUseLimit>) -> FairUseTerms {
    FairUseTerms {
        project: limits,
        member: Vec::new(),
    }
}

/// One window's persisted running sum, as `(tokens, micros, from, to)`.
///
/// `None` where the window carries no sum at all, which is a state the decay
/// really does write: a window every draw it covered has aged out of is
/// *deleted* rather than zeroed, so an untouched window and a fully-decayed
/// one are one state rather than two spellings a later read has to tell
/// apart.
pub async fn window_sum(
    raw: &mut redis::aio::MultiplexedConnection,
    key: &str,
    window: FairUseWindow,
) -> Option<(u64, u64, u64, u64)> {
    let (t, u, from, to) = fair_use_window_sum_fields(window);
    let fields: Vec<Option<String>> = redis::cmd("HMGET")
        .arg(key)
        .arg(&t)
        .arg(&u)
        .arg(&from)
        .arg(&to)
        .query_async(raw)
        .await
        .unwrap();
    let read = |at: usize| fields[at].as_ref().map(|text| text.parse::<u64>().unwrap());
    Some((read(0)?, read(1)?, read(2)?, read(3)?))
}

pub async fn bucket_exists(
    raw: &mut redis::aio::MultiplexedConnection,
    key: &str,
    at_ms: u64,
) -> bool {
    let (t, _) = fair_use_bucket_fields(at_ms);
    redis::cmd("HEXISTS")
        .arg(key)
        .arg(&t)
        .query_async(raw)
        .await
        .unwrap()
}
