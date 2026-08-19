// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Roundhouse core: the session state machine and the routing decision layer.
//!
//! The design rests on one structure: an append-only event log per session with
//! a monotonic sequence number. That single log serves three otherwise separate
//! needs — OpenAI Responses streaming resumption (`starting_after`), reconnect
//! replay for the bidirectional transports, and the routing audit trail.
//!
//! Nothing here knows how to execute a turn. Execution lives behind
//! [`routing::RoutingPolicy`] and the fleet traits in `roundhouse-fleet`, so
//! the state machine stays testable without a GPU, a network, or a provider
//! account.

pub mod context;
pub mod control;
pub mod event;
pub mod ids;
pub mod interject;
pub mod item;
pub mod metrics;
pub mod routing;
pub mod session;
pub mod store;
pub mod validate;

pub use control::{
    Allocation, Budget, BudgetState, BudgetTerms, BudgetWindow, Exhaustion, FrontierCadence,
    FrontierHistory, Grant, GrantRequest, MemorySpendLedger, PolicyOverrides, Principal,
    PrincipalKey, ProjectId, Settlement, SpendLedger, TargetFilter, TurnBudget, TurnPolicy, UserId,
};
pub use event::{
    Accounting, ControlRecord, NotRunReason, PlaceboTiming, SessionEvent, SessionEventKind,
    SessionObserver, SideCallAbandonReason, SideCallPurpose, Usage, ValidationOutcome,
};
pub use ids::{ResponseId, SessionId, SideCallId, TurnId, ValidationId};
pub use interject::{Interjection, InterjectionContext, Interjector};
pub use item::{Item, ItemContent, Role};
pub use metrics::{MetricsConfig, MetricsFold, MetricsSnapshot, ServingMode};
pub use session::{ActiveEscalation, Session, SessionError, TerminalSettlement};
pub use store::{Lease, SessionStore, StoreError};
pub use validate::{
    ActionPolicy, Arm, ArmShares, JudgeAnswer, JudgeClient, JudgeFailure, Objective, SideCall,
    SteerAction, SteerCapability, SteerChannel, TriggerConfig, TriggerRecord, ValidationTerms,
    Validator, ValidatorConfig, Verdict,
};

/// Milliseconds since the Unix epoch.
///
/// Wall-clock rather than monotonic: these timestamps are persisted, compared
/// across processes after a failover, and fed to the frontier cache model,
/// which reasons in provider TTL terms.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
