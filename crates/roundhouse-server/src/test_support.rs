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

use std::sync::{Arc, Mutex};

use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

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
