// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Test-only fixtures shared across this crate's own unit tests and the
//! integration-test binaries under `tests/`.
//!
//! **One copy, not two.** Before M13.1's review (F5), `SeveredStore` was a
//! 270-line addition duplicated verbatim between `engine/fair_use.rs`'s
//! `#[cfg(test)]` module and `tests/fair_use.rs` — the two could not share a
//! copy because a `#[cfg(test)]` unit test module compiles as part of this
//! crate itself, with no path to `tests/common`, which is private to each
//! integration-test binary. Gated behind `test-support` — the same feature
//! [`control_config::directory`](crate::control_config::directory)'s
//! `PlaneSource for ControlPlane` uses for the identical reason — this module
//! is visible from both: this crate's own unit tests build under
//! `#[cfg(test)]`, which the self-referential dev-dependency in `Cargo.toml`
//! turns `test-support` on for, and every downstream integration-test binary
//! already depends on this crate with the same feature to reach
//! `roundhouse_store_redis::test_support` for its own Redis fixtures.
//!
//! [`bind_conversation`] and [`fork_conversation`] are the same story told
//! about [`crate::conversations::Conversations`] rather than a store (M15,
//! H1): `Conversations::bind` and `::fork` had no caller left on the serving
//! path once M14.0 moved every real turn's write onto `Conversations::commit`,
//! but the fixtures that stand a conversation up without driving a turn
//! through admission still need the shape those two spelled — so it is named
//! once, here, rather than reappearing as `conversations.generation(key).await`
//! followed by a hand-rolled `commit` at each of the call sites that used to
//! read `conversations.bind(...)`.
//!
//! [`frontier_spec`], [`single_model_catalog`] and [`engine_over_echo`] are
//! the same discipline applied to a shape M15's H2 named eleven copies of:
//! `fn catalog() -> StaticFrontierCatalog` and `fn engine(...)` /
//! `fn engine_over(...)` under `tests/` and (once, for the unit suite that
//! cannot reach `tests/common`) `src/prefix_admission/tests.rs`. Each copy
//! hand-rolled the same [`FrontierModelSpec`] literal — provider, model and
//! quality genuinely varying; `wire_protocol`, `cache_model` and
//! `ttft_ms_per_uncached_token` never doing anything but repeating — and the
//! same seven-argument `Engine::new`, varying only in the store, the catalog,
//! the frontier client and the config. `tests/common/mod.rs::frontier_catalog`
//! already named the single-entry, zero-variation case for the binaries that
//! could reach it (M14.0 review, F5's shared-fixture lesson applied to a
//! different pair of duplicates); these three go one level down, to the
//! *pieces* a catalog and an engine are built from, so a fixture that needs
//! two or three priced models, or a store `tests/common` cannot name, still
//! shares the boilerplate rather than retyping it.

use std::sync::{Arc, Mutex};

use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::Principal;
use roundhouse_core::routing::{AffinityPolicy, CacheModel, ProviderPricing};
use roundhouse_core::store::SessionStore;
use roundhouse_fleet::{FrontierClient, FrontierModelSpec, StaticFrontierCatalog, WireProtocol};

use crate::engine::{EchoLocalExecutor, Engine, EngineConfig};
use roundhouse_core::ids::SessionId;

use crate::conversations::Conversations;

/// Bind `key` to the generation [`Conversations::commit`] would land a fresh,
/// never-forked turn on — the shape `Conversations::bind` used to spell before
/// M14.0 moved every serving-path write onto `commit` once prefix admission's
/// search had an answer (M15, H1). A fixture that stands a conversation up
/// without driving a real turn through admission still needs exactly this:
/// "generation zero (or whatever this key is already at), now" — and one
/// helper here, rather than `self.generation(key).await` repeated at each of
/// the dozens of call sites `bind` used to serve, is what stops the shape from
/// silently drifting the day `commit`'s signature next changes.
pub async fn bind_conversation(
    conversations: &Conversations,
    principal: &Principal,
    key: &str,
) -> SessionId {
    let generation = conversations.generation(key).await;
    conversations.commit(principal, key, generation).await
}

/// Rebind `key` to a fresh session one generation past whatever it is at now
/// — the shape `Conversations::fork` used to spell, for the fixtures that
/// need to play a client whose resent history disagreed with the log without
/// running prefix admission's search to get there. See
/// [`bind_conversation`] for why this lives here rather than at each call
/// site.
pub async fn fork_conversation(
    conversations: &Conversations,
    principal: &Principal,
    key: &str,
) -> SessionId {
    let generation = conversations.generation(key).await.saturating_add(1);
    conversations.commit(principal, key, generation).await
}

/// One [`FrontierModelSpec`], named and dialected the way a caller asks and
/// priced the way every one of H2's eleven fixtures that never varied price
/// agreed on: `$3`/`$0.3`/`$3.75`/`$15` per Mtok, `quality_prior` 0.95,
/// `base_ttft_ms` 350.0, `ttft_ms_per_uncached_token` 0.002, and a
/// five-minute deterministic cache. A call site whose price, quality or TTFT
/// genuinely differs from that names the field it overrides with struct-update
/// syntax over this — `FrontierModelSpec { quality_prior: 0.6, ..frontier_spec(p,
/// m, w) }` — rather than a positional argument.
///
/// **Why not parameters, after M15's F1.** The four values that varied were
/// three bare `f64`s in a row (`quality_prior`, `base_ttft_ms`,
/// `ttft_ms_per_uncached_token`) with a `ProviderPricing` between the first
/// two — nothing in that signature stopped a call site from transposing
/// `quality_prior` and `base_ttft_ms`; the swap still type-checked and
/// silently moved both the capability gate (`quality_prior`) and the
/// router's TTFT term (`base_ttft_ms`) at once, with no diagnostic between
/// the call site and whatever assertion later noticed. A named
/// struct-update field is a compile error to misspell and a diff to
/// transpose — it cannot silently swap with its neighbor the way a bare
/// positional `f64` can.
pub fn frontier_spec(
    provider: &str,
    model: &str,
    wire_protocol: WireProtocol,
) -> FrontierModelSpec {
    FrontierModelSpec {
        provider: provider.to_string(),
        model: model.to_string(),
        wire_protocol,
        cache_model: CacheModel::Deterministic { ttl_ms: 5 * 60_000 },
        pricing: ProviderPricing {
            input_per_mtok_usd: 3.0,
            cached_input_per_mtok_usd: 0.3,
            cache_write_per_mtok_usd: 3.75,
            output_per_mtok_usd: 15.0,
        },
        quality_prior: 0.95,
        base_ttft_ms: 350.0,
        ttft_ms_per_uncached_token: 0.002,
    }
}

/// A catalog of one priced frontier model, so a turn always has somewhere to
/// go — `spec` wrapped in the one-entry [`StaticFrontierCatalog`] every
/// single-model fixture built by hand.
///
/// `tests/common/mod.rs::frontier_catalog` already names this exact shape
/// for the integration binaries that can reach it; this is the same
/// function for the two audiences that cannot — this crate's own unit
/// tests (`src/prefix_admission/tests.rs`), and a fixture whose price,
/// wire protocol, quality or TTFT genuinely needs to differ from
/// `frontier_catalog`'s own. Takes the built [`FrontierModelSpec`] rather
/// than [`frontier_spec`]'s own parameters, so a caller who needs to
/// override a field does it once, by name, at the call site — see
/// [`frontier_spec`]'s doc for why that replaced a positional tail.
pub fn single_model_catalog(spec: FrontierModelSpec) -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![spec])
}

/// `Engine::new` over this crate's local double, [`EchoLocalExecutor`]
/// answering `"local answer"`, and a plain [`AffinityPolicy`] — the two
/// things every `fn engine(...)` / `fn engine_over(...)` fixture H2
/// replaces built identically, whatever else it varied.
///
/// The frontier client stays a parameter rather than fixed to
/// [`roundhouse_fleet::EchoFrontierClient`], because one of the five
/// fixtures this replaces (`metrics_end_to_end.rs`) is parameterized over it
/// for exactly the reason a caller would still want to be: the same engine
/// shape driven by a client that answers differently per test. A fixture
/// that dispatches through more than one provider's own client
/// (`tests/provider_registry.rs`) needs `Engine::with_provider_clients`
/// instead and is not this function's shape at all — a different
/// constructor is a real variation, not boilerplate this could absorb.
pub fn engine_over_echo<S: SessionStore>(
    store: Arc<S>,
    catalog: StaticFrontierCatalog,
    frontier: Arc<dyn FrontierClient>,
    config: EngineConfig,
) -> Engine<S, ByteTokenizer> {
    Engine::new(
        store,
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        catalog,
        frontier,
        Arc::new(AffinityPolicy::new()),
        config,
    )
}

/// A TCP relay in front of a real Redis, so a store can be taken away
/// mid-test without touching a Redis the test does not own.
///
/// **Why a relay rather than a `SHUTDOWN`.** The Redis
/// `ROUNDHOUSE_TEST_REDIS_URL` names is shared with every other gated suite
/// in this workspace, and stopping it would fail them rather than the one
/// test exercising an outage; starting a second server would put a binary on
/// every such test's requirements. What an outage *is*, from this process's
/// side, is a connection that stops answering and a reconnect that is
/// refused — which is exactly what dropping the relay produces, with the
/// store itself untouched.
pub struct SeveredStore {
    /// The `redis://…` URL a ledger should connect through — the relay's own
    /// loopback address, not the upstream's.
    pub url: String,
    accept: JoinHandle<()>,
    pipes: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl SeveredStore {
    /// Start relaying to `upstream` and return the loopback address to
    /// connect through instead of it.
    pub async fn in_front_of(upstream: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback listener binds");
        let port = listener.local_addr().unwrap().port();
        // `redis://host:port/db`, which is the only shape
        // `ROUNDHOUSE_TEST_REDIS_URL` takes in this workspace. Parsed by hand
        // rather than through the `redis` crate's own URL type: that crate is
        // a dependency of `roundhouse-store-redis` only, and pulling it into
        // this crate just to parse a test fixture's own env var would be a
        // production dependency added for a test-only reason.
        let target = upstream
            .trim_start_matches("redis://")
            .split('/')
            .next()
            .expect("a redis URL names a host and a port")
            .to_string();
        let pipes: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let accept = tokio::spawn({
            let pipes = Arc::clone(&pipes);
            async move {
                loop {
                    let Ok((mut inbound, _)) = listener.accept().await else {
                        return;
                    };
                    let target = target.clone();
                    let pipe = tokio::spawn(async move {
                        let Ok(mut outbound) = TcpStream::connect(target).await else {
                            return;
                        };
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    });
                    pipes.lock().unwrap().push(pipe);
                }
            }
        });
        Self {
            url: format!("redis://127.0.0.1:{port}/"),
            accept,
            pipes,
        }
    }

    /// The outage: stop listening — so a reconnect is refused — and drop
    /// every connection already open.
    ///
    /// Dropping only the accept loop leaves any connection already piped
    /// alive and answering, which is a relay that never actually severs
    /// anything a `ConnectionManager` had already established — so both are
    /// torn down here.
    pub fn cut(&self) {
        self.accept.abort();
        for pipe in self.pipes.lock().unwrap().drain(..) {
            pipe.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Control for the F1 guard below: a named struct-update override round
    /// trips into the built spec exactly, and the fields that were not
    /// named keep `frontier_spec`'s defaults. Keeping this live is what
    /// proves the guard is checking a real shape property, not failing for
    /// an unrelated reason.
    #[test]
    fn correctly_ordered_quality_prior_and_base_ttft_ms_round_trip() {
        let correct = single_model_catalog(FrontierModelSpec {
            quality_prior: 0.6,
            base_ttft_ms: 900.0,
            ..frontier_spec("anthropic", "claude", WireProtocol::AnthropicMessages)
        });
        let spec = &correct.models()[0];
        assert_eq!(spec.quality_prior, 0.6);
        assert_eq!(spec.base_ttft_ms, 900.0);
        // Untouched fields keep frontier_spec's own defaults — the whole
        // point of struct-update over a positional tail: naming the two
        // fields that vary does not require restating the rest.
        assert_eq!(spec.ttft_ms_per_uncached_token, 0.002);
    }

    /// M15 review, F1: `frontier_spec`/`single_model_catalog` used to take
    /// `quality_prior`, `base_ttft_ms` and `ttft_ms_per_uncached_token` as
    /// three positional, unlabeled `f64` parameters, so a call site that
    /// transposed `quality_prior` and `base_ttft_ms` type-checked and
    /// silently moved both the capability gate and the router's TTFT term.
    ///
    /// The fix removed the positional tail rather than validating it: a
    /// transposed *value* can still be wrong (0.6 really was meant for one
    /// field or the other), but it can no longer be wrong from the shape of
    /// the call alone. That is a static property of the signature, not a
    /// runtime one — the old ignored test's demonstration (an out-of-range
    /// `quality_prior` slipping through unvalidated) has no equivalent to
    /// swap in any more, because there is no longer a positional pair to
    /// transpose. What replaces it is this: read the source and assert
    /// neither helper declares an `f64` parameter, so a positional tail
    /// cannot come back unnoticed in a later edit.
    #[test]
    fn frontier_spec_and_single_model_catalog_take_no_positional_f64_parameters() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let source =
            std::fs::read_to_string(std::path::Path::new(manifest_dir).join("src/test_support.rs"))
                .expect("this crate's own test_support.rs must be readable from its own test");

        for signature in ["fn frontier_spec(", "fn single_model_catalog("] {
            let start = source
                .find(signature)
                .unwrap_or_else(|| panic!("test_support.rs should declare {signature}"));
            let params_end = source[start..]
                .find(") ->")
                .unwrap_or_else(|| panic!("{signature} should have a return type"));
            let params = &source[start..start + params_end];
            assert!(
                !params.contains("f64"),
                "{signature} declares a bare f64 parameter again: {params:?} — \
                 F1 replaced the positional quality_prior/base_ttft_ms/\
                 ttft_ms_per_uncached_token tail with named struct-update \
                 overrides on FrontierModelSpec precisely because a \
                 positional f64 pair type-checks when transposed and moves \
                 the capability gate and the router's TTFT term silently"
            );
        }
    }
}
