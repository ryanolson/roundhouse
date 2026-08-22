// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Relay's published formats, produced from roundhouse's log.
//!
//! NeMo Relay publishes three interchange shapes: **ATOF** (a runtime event
//! stream), **ATIF v1.7** (a finished agent trajectory), and
//! **`LlmOptimizationSummary`** (close-time savings accounting). This crate
//! produces all three out of the one thing roundhouse already has — the
//! append-only session log — so that a deployment interoperating with Relay's
//! ecosystem does not have to be instrumented twice.
//!
//! # Why emit theirs rather than invent ours
//!
//! Roundhouse's log is a strictly *better producer* than the exporter Relay
//! ships: totally ordered by `seq`, durable, and replayable from cold storage,
//! where Relay's ATIF exporter accumulates in memory and is lost with the
//! process. That is an argument for emitting their format from our log, not for
//! inventing a parallel one — a shared type is a conversation and a copy is a
//! fork.
//!
//! # Three properties hold the whole design up
//!
//! **Every producer is a pure function of the events it is given.** No clock, no
//! `Uuid::new_v4`, no ambient configuration: [`atof::events`],
//! [`atif::trajectory`] and [`summary::for_session`] take a slice of
//! [`SessionEvent`](roundhouse_core::event::SessionEvent) and return a value,
//! and calling one twice on one replayed session serializes to the same bytes
//! both times. That is not an aesthetic preference. These documents are produced
//! by *cold replay* of a finished session, so two exports that disagreed would
//! mean the log was not the source of truth after all, and nothing downstream
//! could diff two trajectories to see what changed. Every identifier is
//! therefore a UUIDv5 digest of facts already in the log — see [`ids`].
//!
//! **No money is computed here.** Every dollar in a summary comes out of
//! `roundhouse_core::metrics`: the correlary is the one the dashboard shows,
//! resolved through [`MetricsSnapshot`](roundhouse_core::metrics::MetricsSnapshot),
//! and the arithmetic is `Correlary::shadow_cost_usd` and `ProviderPricing`'s
//! own methods. A second pricing walk here would be a second answer to what a
//! turn cost, and the two would disagree on the day a rate card was corrected.
//! In particular the **capability gate's outcome is carried, never recomputed**:
//! this crate publishes which band the gate used and what it decided, and has no
//! opinion of its own about whether two models are comparable.
//!
//! **Seat tokens are counted and never priced.** A pass-through project's turn
//! is measured under somebody's subscription, and roundhouse holds no rate card
//! for a seat. Such a turn publishes no `baseline_cost`, no `actual_cost` and no
//! `estimated_cost_saved` at all — its tokens ride the typed payload as a bare
//! count. This is the same rule the spend ledger has kept since budgets existed,
//! restated at the one surface that would otherwise invent a bill for it.
//!
//! # Dependency direction
//!
//! `roundhouse-relay` depends on `roundhouse-core` and on nothing else of ours,
//! for the reason `roundhouse-mcp` does: one crate per external surface, and the
//! composition root is the only place that knows about more than one of them. It
//! must never depend on `roundhouse-server`, which is what mounts the three
//! routes over these producers — an emitter that could reach the router would
//! start reading engine state, and the whole claim above is that these documents
//! are a function of the log and nothing else.
//!
//! # What the ATIF types are, and what is owed for them
//!
//! ATIF is not in `nemo-relay-types`; it lives in Relay's heavy `crates/core`,
//! which pulls a runtime we have no use for. So [`atif`] carries a
//! field-for-field port of the twelve wire structs, under the Apache-2.0
//! attribution the source requires, with a test that pins every field name
//! against the upstream list so drift arrives as a diff rather than as a
//! surprise. Everything in [`atof`] and [`summary`], by contrast, is Relay's
//! *own* types from the pinned crate — see the manifest for why the pin is
//! `=0.7.3` and what it costs the rest of the graph.
//!
//! # One divergence, stated rather than reconciled
//!
//! A turn that roundhouse **steered** — the validate loop interjecting a
//! synthetic tool call — appears in these documents as an ordinary tool call,
//! because that is what it is on the wire and what the client acted on. The fact
//! that this deployment authored it rather than the model lives in the session
//! log (a `ToolCall` item bearing a `response_id`) and in the validate tally,
//! and it is deliberately not reconciled into the trajectory: ATIF describes
//! what the agent did, and inventing a step kind for it would make our export
//! unreadable by every consumer of the format. One event log is authoritative
//! for accounting, and it is ours; this is observability.

pub mod atif;
pub mod atof;
pub mod ids;
pub mod replay;
pub mod summary;
pub mod wire;

/// Session logs the whole crate's tests produce documents from. One builder, so
/// a test comparing an ATOF timestamp against a trajectory's is asserting about
/// the producers rather than about two fixtures that happened to agree.
#[cfg(test)]
mod fixtures;

pub use atif::{
    ATIF_SCHEMA_VERSION, AtifAgentInfo, AtifAncestry, AtifFinalMetrics, AtifInvocationInfo,
    AtifMetrics, AtifObservation, AtifObservationResult, AtifStep, AtifStepExtra,
    AtifSubagentTrajectoryRef, AtifToolCall, AtifTrajectory, trajectory,
};
pub use atof::events;
pub use replay::{SessionReplay, TurnOutcome, TurnRecord};
pub use summary::{Baseline, Baselines, RoutingEvidence, for_decision, for_session};
pub use wire::{route_facts, route_schema};

/// The `producer` every contribution this crate emits is stamped with.
///
/// Relay keys aggregation on it, so it is a stable identity rather than a
/// display name: a deployment renaming itself must not look like a second
/// optimizer arriving.
pub const PRODUCER: &str = "roundhouse";

/// The schema name our routing evidence and routing scope events are tagged
/// with, in Relay's `{name, version}` `DataSchema` vocabulary.
///
/// One name across both event formats on purpose: a consumer that learns to
/// read the ATIF step's `extra` has learned to read the ATOF scope's `data`, and
/// they carry the same facts about the same decision. The optimization
/// contribution's typed payload is a *different* schema
/// (`RoutingEvidence::SCHEMA_NAME`, `"roundhouse/routing"`), because it carries
/// money rather than a decision.
pub const ROUTE_SCHEMA_NAME: &str = "roundhouse/route";

/// The version of [`ROUTE_SCHEMA_NAME`]'s payload. Bump when a field's meaning
/// changes, not when one is added — Relay's consumers preserve unknown keys.
pub const ROUTE_SCHEMA_VERSION: &str = "1";
