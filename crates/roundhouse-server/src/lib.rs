// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The turn engine: the composition root where state, context, routing, and
//! execution meet.
//!
//! One turn is: claim the session, replay it into a context assembler, admit
//! the turn, price every target, reserve what may be spent, choose, record the
//! choice, execute, and settle — the log first, then the money. Each of those
//! steps lives in a lower layer; this module only sequences them.
//!
//! The sequencing itself carries three guarantees worth stating. The routing
//! decision is written to the log *before* execution starts, so the audit trail
//! records what was chosen even when execution then fails. The spend ledger is
//! written *after* the terminal event, so what a project is charged is priced
//! from what the log actually holds rather than from what a turn intended. And
//! a booked local reservation is always settled, including on the error path,
//! because a leaked reservation silently inflates the router's view of a worker
//! forever.
//!
//! [`http`] puts a transport in front of that sequence without joining it: it
//! streams turns by tailing the same log the engine writes to, so the wire
//! protocol has no state of its own to keep in agreement.
//!
//! [`responses_api`] is a second transport over that same log, speaking the
//! OpenAI Responses API so existing agents can drive Roundhouse unmodified. It
//! adds no state either: a client's resent conversation is checked against the
//! log as a prefix rather than remembered alongside it.
//!
//! [`metrics_api`] reports on all of it. Token counts, dollars, and the savings
//! figure are folded out of the same log as everything else — see
//! [`roundhouse_core::metrics`] — so the dashboard cannot disagree with the
//! audit trail it summarizes.

pub mod catalog_config;
pub mod control_config;
pub mod engine;
pub mod http;
pub mod metrics_api;
pub mod responses_api;
pub mod tokenizer;

pub use catalog_config::{CatalogConfig, CatalogError};
pub use control_config::{
    Admission, AuthError, ControlPlane, ControlPlaneConfig, ControlPlaneError, KeyScope,
};
pub use engine::{
    EchoLocalExecutor, Engine, EngineConfig, EngineError, LocalExecution, LocalExecutor, TurnResult,
};
pub use http::router;
pub use metrics_api::metrics_router;
pub use responses_api::responses_router;
pub use tokenizer::HfTokenizer;
