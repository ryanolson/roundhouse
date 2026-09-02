// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which fair-use ledger a deployment gets.
//!
//! **In the library rather than in `main.rs`, and that placement is the
//! finding.** M13 shipped this rule as a private function of the binary, so
//! the only thing that could check it was a unit test over booleans; the
//! end-to-end question — does a ceiling an operator adds through the admin
//! plane at 3pm actually reach the Redis this deployment names? — had nowhere
//! to be asked from, and the answer was "no" for a whole class of deployment
//! without a single test going red. `tests/fair_use_backend_boot.rs` asks it
//! now, against a real Redis, by calling *this* function rather than
//! re-deriving its rule — which is the same lesson M13's own refute pass
//! learned about the boot warning one layer down.

/// Where a deployment's rolling fair-use counters live.
///
/// Two variants rather than a `bool` because the shared arm carries the URL it
/// was chosen by: a bare `bool` would leave the composition root holding
/// "shared buckets" and an `Option<String>` it had to unwrap again, and the
/// two disagreeing is exactly the boot-time state this rung exists to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FairUseBackend<'a> {
    /// The buckets every node of the deployment shares, in the Redis named by
    /// this URL.
    Shared { url: &'a str },
    /// This process's own memory.
    PerProcess,
}

/// The same rule that chooses a session store and a spend ledger, and *only*
/// that rule.
///
/// `ROUNDHOUSE_REDIS_URL` is the variable an operator sets when they mean
/// "this is more than one process", and it is exactly then that a per-process
/// rolling counter stops being the ceiling its configuration says it is. So
/// Redis selects the shared buckets, the same way it selects the durable log
/// and the durable ledger, and for the same reason.
///
/// **It used to take a second argument — whether any membership had a
/// `fair_use` block — and that was the M13 thermo-nuclear review's F1.** The
/// argument could only ever be a *boot-time snapshot* of an axis the admin
/// plane patches at runtime (`patch_project` writes a `fair_use` block onto a
/// live project, and the engine reads the recompiled plane on the very next
/// request), so a deployment that booted with a Redis and no ceiling anywhere
/// enforced every later-added ceiling in one node's memory for the rest of the
/// process's life, with nothing in Redis and no warning. The stated
/// justification for the argument — that connecting a third handle could make
/// an unreachable Redis fail the boot of a deployment with no ceiling to fail
/// about — was false besides: `RedisSessionStore::connect` on the same URL
/// already fails that boot, several lines earlier.
///
/// What is left is one connection per deployment that named a Redis, which is
/// what naming one already means for the two backends beside this.
pub fn fair_use_backend(redis_url: Option<&str>) -> FairUseBackend<'_> {
    match redis_url {
        Some(url) => FairUseBackend::Shared { url },
        None => FairUseBackend::PerProcess,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Naming a Redis is the whole rule, and nothing else is.**
    ///
    /// The cell that changed in the M13 review is the first one: a deployment
    /// with a Redis and no configured window used to get `PerProcess`, which
    /// is what let a window PATCHed in an hour later be counted in one node's
    /// memory. There is no "configured" axis left to tabulate against — that
    /// is the fix — so the interesting assertion is that the URL survives the
    /// choice, since a `Shared` that had to be paired with the URL again at
    /// the call site is the two-facts-that-can-disagree shape over again.
    #[test]
    fn naming_a_redis_selects_the_shared_buckets_and_carries_its_url() {
        assert_eq!(
            fair_use_backend(Some("redis://127.0.0.1:6379/")),
            FairUseBackend::Shared {
                url: "redis://127.0.0.1:6379/"
            }
        );
        assert_eq!(fair_use_backend(None), FairUseBackend::PerProcess);
    }
}
