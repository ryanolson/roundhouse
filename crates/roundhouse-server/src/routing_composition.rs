// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which router a process composes, and whether it runs the online routing
//! learner: the composition root's routing decision, in the library.
//!
//! **In the library for `shared_backend`'s reason** (M14.1 review, F1): a
//! `[[bin]]` is not something a test can call, so a rule spelled only in
//! `main.rs` can be mutated with every suite green. `serve` wires what
//! [`compose`] and [`attach_learner`] return and decides nothing itself, and
//! `tests/learner_startup.rs` calls the same two functions.
//!
//! **Three routers, chosen by what the booted plane configures:**
//!
//! | Plane at boot | Policy on the record | Learner |
//! |---|---|---|
//! | no project writes `tiers` | `affinity` | none |
//! | a project writes `tiers`, none enables the learner | `stage` | none |
//! | a project's learner is `shadow` or `live` | `learned` | store and recovery task |
//!
//! **Conditional composition, and the condition is not a micro-optimization.**
//! Each wrapper serves the turns it does not own exactly as the router inside
//! it would, but the record names the object in force. Composing a wrapper
//! unconditionally would relabel every existing deployment's decisions on an
//! upgrade that changed no routing, so a deployment with no learner enabled
//! keeps exactly today's composition: no learner store is opened, no recovery
//! task runs, and no learner line is logged.
//!
//! **The hole this leaves is a block added through the admin plane after
//! boot.** Nothing here can see it. The engine names such a project once, when
//! its first turn arrives (`Engine::unread_recipe` for a recipe, the learning
//! module's `warn_unread_learner` for a learner), and a restart composes it.

use std::sync::Arc;

use roundhouse_core::context::Tokenizer;
use roundhouse_core::learn_store::LearnerStore;
use roundhouse_core::routing::learn::LearnedPolicy;
use roundhouse_core::routing::{AffinityPolicy, RoutingPolicy, StagePolicy};
use roundhouse_core::store::SessionStore;

use crate::Engine;
use crate::control_config::ControlPlane;
use crate::learner_recovery::{LearnerRecovery, RecoveryCadence};
use crate::shared_backend::Backends;

/// Does any project on this plane route between tiers? (M10.2, S3)
///
/// Read through `configured_admissions`, the one accessor for the key table's
/// layout outside its own module. `ControlPlane::Open` has no file to write a
/// recipe in and answers `false` by construction.
pub fn composes_the_stage_router(plane: &ControlPlane) -> bool {
    plane
        .configured_admissions()
        .any(|admission| admission.tiers.is_some())
}

/// Does any project on this plane run the learner in `shadow` or `live`?
///
/// An `off` block resolves to no terms at all, so this asks only whether some
/// admission carries terms whose mode is active.
pub fn composes_the_learner(plane: &ControlPlane) -> bool {
    plane.configured_admissions().any(|admission| {
        admission
            .learner
            .as_deref()
            .is_some_and(|terms| terms.mode.active().is_some())
    })
}

/// What `serve` builds the engine from: the router, and the learner when one
/// is composed.
pub struct RoutingComposition {
    /// `affinity`, `stage` or `learned`; see the module doc.
    pub policy: Arc<dyn RoutingPolicy>,
    /// `Some` exactly when [`composes_the_learner`] answered `true`.
    pub learner: Option<ComposedLearner>,
}

/// The learner half of a composition: the store every learned turn reads and
/// applies to, and the recovery task's cadence.
pub struct ComposedLearner {
    pub store: Arc<dyn LearnerStore>,
    pub cadence: RecoveryCadence,
}

/// Compose the router for the plane this process booted with, opening the
/// learner store in `backends`' arm only when a project enables the learner.
///
/// Fails only when the learner store cannot be opened (an unreachable Redis,
/// named in the error), or on a plane that enables a learner with no
/// recovery cadence, which `ControlPlaneConfig::validate` refuses before any
/// plane exists.
pub async fn compose(
    plane: &ControlPlane,
    backends: &Backends,
) -> anyhow::Result<RoutingComposition> {
    let stage = || StagePolicy::new(Box::new(AffinityPolicy::new()));
    if composes_the_learner(plane) {
        let cadence = plane.learner_recovery().ok_or_else(|| {
            anyhow::anyhow!(
                "a project enables the learner and the plane carries no `learner_recovery` \
                 cadence; the loader refuses this, so the plane was built another way"
            )
        })?;
        let store = backends.open_learner_store().await?;
        tracing::info!(
            "a project enables the online routing learner; the learned router is composed over \
             the stage router, and the recovery task delivers sessions that go idle with \
             entries owed"
        );
        return Ok(RoutingComposition {
            policy: Arc::new(LearnedPolicy::new(stage())),
            learner: Some(ComposedLearner { store, cadence }),
        });
    }
    let policy: Arc<dyn RoutingPolicy> = match composes_the_stage_router(plane) {
        true => {
            tracing::info!(
                "a project configures a tier recipe; the stage router is composed over the \
                 ordinary policy, and projects with no recipe route through it unchanged"
            );
            Arc::new(stage())
        }
        false => Arc::new(AffinityPolicy::new()),
    };
    Ok(RoutingComposition {
        policy,
        learner: None,
    })
}

/// Attach the composed learner, if any, and build its recovery task over the
/// engine's own learner, so the two share the store, the stopped sessions and
/// the delivery counters.
///
/// The task is returned rather than spawned, so a caller decides when it runs
/// and holds its handle: `serve` spawns it and keeps the handle for as long
/// as it serves; a test drives single sweeps.
pub fn attach_learner<S: SessionStore, T: Tokenizer + Clone + 'static>(
    engine: Engine<S, T>,
    learner: Option<ComposedLearner>,
) -> (Engine<S, T>, Option<LearnerRecovery<S>>) {
    let Some(ComposedLearner { store, cadence }) = learner else {
        return (engine, None);
    };
    let engine = engine.with_learner(store);
    let recovery = engine.learner_recovery(cadence);
    (engine, recovery)
}
