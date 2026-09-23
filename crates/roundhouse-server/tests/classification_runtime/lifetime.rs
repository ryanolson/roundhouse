// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
use super::*;

// ------------------------------------------------- production lifetime (H6)

/// The production lifetime guard (`Supervisor`, returned by
/// `runtime.supervise()` and held for the life of `serve` in `main.rs`) must
/// stop admission when it ends. `Supervisor::drop` today only aborts the
/// sweep task; it does not call `shutdown`, so this is exercised through the
/// real composition seam (`compose` + `.supervise()`) rather than a hand-built
/// `shutdown()` call, which `shutdown_cancels_in_flight_work_and_releases_its_capacity`
/// (`classify_runtime/tests/mailbox.rs`) already proves works on its own.
#[tokio::test]
async fn the_production_lifetime_guard_stops_admission_on_drop() {
    let (base_url, _upstream) = classifier_upstream().await;
    let runtime = compose(
        "<test>",
        &config(&base_url, true),
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");

    // The production seam, exactly as `main.rs` calls it, held for a moment
    // and then dropped -- standing in for `serve` returning.
    let supervisor = runtime.supervise();
    drop(supervisor);

    assert!(
        runtime.capacity().is_none(),
        "dropping the production lifetime guard must stop admission"
    );
}

/// The same guarantee for a worker already spawned and waiting on the wire:
/// ending the production lifetime must cancel it and release its permit.
///
/// Synchronizes on the classifier double's handler actually being invoked --
/// not merely on the runtime having admitted and spawned the call -- so
/// dropping the guard genuinely races a request already on the wire, which is
/// the claim this test's name makes. Admission alone would still leave a
/// window between the worker being spawned and its request reaching the
/// upstream at all.
#[tokio::test]
async fn the_production_lifetime_guard_cancels_a_worker_in_flight_on_drop() {
    use axum::Router;
    use axum::routing::post;
    use std::future::pending;

    // A classifier that never answers -- signals `arrived` the instant its
    // handler is invoked, then hangs forever, so the worker is genuinely on
    // the wire until something cancels it.
    let arrived = Arc::new(tokio::sync::Notify::new());
    let handler_arrived = Arc::clone(&arrived);
    let app = Router::new().route(
        "/systemone",
        post(move || {
            let arrived = Arc::clone(&handler_arrived);
            async move {
                arrived.notify_one();
                pending::<()>().await
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let base_url = format!("http://{addr}");

    let rig = rig(Some(&config(&base_url, true)));
    let runtime = rig.runtime.clone().expect("a runtime");
    let session = SessionId::new("sess_guard_inflight");
    rig.turn(&session, "t1", "fix the parser").await;

    // Synchronize on the request actually reaching the upstream handler, not
    // on a guessed sleep and not merely on the runtime's own admission count.
    tokio::time::timeout(Duration::from_secs(5), arrived.notified())
        .await
        .expect(
            "the worker's request must actually reach the upstream for this \
             test to be about on-wire cancellation",
        );
    assert!(
        runtime.available_capacity() < runtime.limits().max_in_flight,
        "the call must actually be admitted for this test to be about anything"
    );

    let supervisor = runtime.supervise();
    drop(supervisor);

    // Bounded wait, standing in for "long enough that a real cancellation would
    // have taken effect". The upstream never answers, so the only way capacity
    // comes back is a cancellation the drop above was supposed to cause.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        runtime.available_capacity(),
        runtime.limits().max_in_flight,
        "dropping the production lifetime guard must cancel a worker still \
         waiting to dispatch and release its permit"
    );
}
