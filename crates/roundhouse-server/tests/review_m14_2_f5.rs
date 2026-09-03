// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.2 review, F5 — was: `main.rs`'s empty-`ROUNDHOUSE_REDIS_NAMESPACE`
//! boot error wrapped [`resolve_namespace`]'s `EmptyNamespace` in an
//! `anyhow::Context` string that repeated the underlying error's own
//! message, rather than adding information a reader doesn't already have.
//!
//! This exercises the exact two calls `main.rs` makes — no Redis connection
//! is reached at either (`resolve_namespace` is validated before
//! `shared_backend::open` is ever called), so this suite is deliberately
//! ungated: proving the message stays singular needs no server on the
//! loopback.
//!
//! **Closed.** `main.rs`'s context now names the variable rather than
//! restating the reason: `format!("reading {REDIS_NAMESPACE_VAR}")`. That
//! reads correctly for every `EmptyNamespace` reason — blank, or (M14.2
//! review, F6) a character the key format itself reserves — where a context
//! that assumed "must not be empty" would have been wrong for the second
//! kind of refusal even once the doubling was fixed.

use roundhouse_server::{REDIS_NAMESPACE_VAR, resolve_namespace};

use anyhow::Context;

/// **The defect cell, before the fix.** `main.rs` did:
///
/// ```ignore
/// resolve_namespace(std::env::var(REDIS_NAMESPACE_VAR).ok().as_deref())
///     .with_context(|| format!("{REDIS_NAMESPACE_VAR} must not be empty"))?;
/// ```
///
/// and `EmptyNamespace`'s own `Display` already read "the Redis key
/// namespace must not be empty — ...". Chained by `anyhow`, the printed
/// error said "must not be empty" twice for one fact. `main.rs` now builds
/// its context the way this test does, which is what this asserts against
/// as a standing regression guard.
#[test]
fn empty_namespace_context_does_not_repeat_the_underlying_message() {
    let err = resolve_namespace(Some(""))
        .with_context(|| format!("reading {REDIS_NAMESPACE_VAR}"))
        .unwrap_err();

    // anyhow's alternate Display prints the whole chain ("context: cause"),
    // which is what an operator actually sees at the boot failure site.
    let full_chain = format!("{err:#}");

    let occurrences = full_chain.matches("must not be empty").count();
    assert_eq!(
        occurrences, 1,
        "the context string repeats the underlying error's own message \
         verbatim instead of adding anything: {full_chain:?}"
    );
}

/// CONTROL: `EmptyNamespace`'s own `Display`, on its own, says the thing
/// exactly once — the duplication above is introduced by the `with_context`
/// wrapper in `main.rs`, not by the underlying error type.
#[test]
fn empty_namespace_display_alone_says_it_once() {
    let err = resolve_namespace(Some("")).unwrap_err();
    let message = err.to_string();
    assert_eq!(message.matches("must not be empty").count(), 1);
}
