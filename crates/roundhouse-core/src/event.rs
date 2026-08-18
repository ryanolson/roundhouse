// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The session event log.
//!
//! Every observable thing a session does becomes an event with a monotonic
//! `seq`. A client that reconnects presents the last `seq` it saw and the
//! server replays forward from there, which is the same mechanism the Responses
//! API exposes as `starting_after`. Because the log is the source of truth
//! rather than a side-channel, a partially generated response survives the
//! death of the process that was generating it.

use serde::{Deserialize, Serialize};

use crate::control::Principal;
use crate::ids::{ResponseId, SessionId, TurnId};
use crate::item::Item;
use crate::routing::DecisionRecord;

/// Token accounting for one completed model call.
///
/// Two of these four fields are *components* of the other two rather than
/// additions to them: `cached_input_tokens` is part of `input_tokens`, and
/// `reasoning_tokens` is part of `output_tokens`. Both providers Roundhouse
/// targets report them that way — OpenAI nests them under
/// `input_tokens_details` / `output_tokens_details`, and Anthropic bills
/// thinking as ordinary output — so storing them as separate addends would
/// double-count every total downstream, including the one billed to a client.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    /// Portion of `input_tokens` served from a prefix cache.
    ///
    /// Locally this is derived from the scheduler's cache credit; for frontier
    /// providers it is whatever the provider reports. It is the number the
    /// whole design exists to maximize.
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    /// Portion of `output_tokens` spent on reasoning the client never sees.
    ///
    /// Zero for models without a thinking mode, which is why it carries a
    /// serde default: logs written before this field existed deserialize as
    /// "no reasoning" rather than failing, and that reading is correct for
    /// every model that was routable at the time.
    #[serde(default)]
    pub reasoning_tokens: u64,
    /// Whether these counts came from the provider or from our own tokenizer.
    ///
    /// Load-bearing for the metrics layer rather than diagnostic. A streaming
    /// OpenAI-compatible endpoint reports no usage at all unless the request
    /// asked for it, and an unreported call folded into a rollup as zero
    /// tokens and zero dollars is indistinguishable from a saving — the
    /// dashboard would look its best exactly when its instrumentation was
    /// broken. Marking the call keeps that gap visible as a gap.
    #[serde(default)]
    pub accounting: Accounting,
}

/// Where a [`Usage`]'s counts came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Accounting {
    /// The provider (or, locally, the scheduler) reported them.
    ///
    /// The default, and the right reading for every log written before this
    /// field existed: usage was only ever recorded from a provider's own
    /// accounting chunk, so a record that predates the field was reported.
    #[default]
    Reported,
    /// The provider returned no usage and these are Roundhouse's own counts.
    ///
    /// Input is trustworthy — it is the prompt we tokenized and routed on —
    /// and output is a tokenization of what we received. Cached input is not
    /// estimated at all but left at zero, because no local evidence bears on
    /// what a remote cache did, and guessing high would inflate the one number
    /// this whole system is judged by.
    Estimated,
}

impl Usage {
    /// Billable tokens for this call.
    ///
    /// Cached input and reasoning output are deliberately absent: they are
    /// already inside the two terms. See the type's own note.
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Prompt tokens that were not served from cache, and so had to be
    /// prefilled. The complement of the quantity the routing optimizes for.
    pub fn uncached_input_tokens(&self) -> u64 {
        self.input_tokens.saturating_sub(self.cached_input_tokens)
    }

    /// Output tokens the client actually received, i.e. excluding reasoning.
    pub fn visible_output_tokens(&self) -> u64 {
        self.output_tokens.saturating_sub(self.reasoning_tokens)
    }

    /// Accumulate another call into this one.
    ///
    /// Saturating rather than wrapping: a metrics rollup that wrapped at
    /// `u64::MAX` would report a near-zero total for the busiest deployment on
    /// the fleet, which is the one case where the number matters most.
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        // Provenance degrades on contact: a total that mixes reported and
        // estimated calls is an estimate, and rounding that up to "reported"
        // would launder exactly the uncertainty this field exists to carry.
        if other.accounting == Accounting::Estimated {
            self.accounting = Accounting::Estimated;
        }
    }
}

/// Why a response stopped short.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompleteReason {
    /// The owning process lost its lease or died mid-generation.
    ///
    /// The partial output is already durable in the log, so a successor can
    /// resume from it rather than restarting the turn.
    OwnerLost,
    MaxOutputTokens,
    ClientCancelled,
    UpstreamError,
    /// No target the turn's principal may use was admissible.
    ///
    /// Distinct from [`Self::UpstreamError`] because no upstream was contacted:
    /// nothing was dispatched, there is no partial to resume from, and the
    /// cache ledger learns nothing about any target. Calling it an upstream
    /// error would blame a provider for a decision this deployment made, and
    /// an operator reading the log would go looking at the wrong system.
    ///
    /// It is also the one terminal reason a retry cannot fix on its own: the
    /// same turn under the same policy refuses again, and only an operator
    /// widening the policy changes the answer. Surfaces that speak a dialect
    /// with a separate "could not be served" terminal render it as that rather
    /// than as a truncated answer — see `responses_api`.
    PolicyRefused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEventKind {
    /// The first fact in a session's log: which policy serves it and who pays.
    ///
    /// Emitted once, when the log is empty, by the one caller that already
    /// holds the lease — so it is race-free and idempotent by construction, a
    /// log being empty exactly once. Everything downstream that needs to know
    /// whose turn this was reads it from here rather than from a side table: a
    /// replay starts at seq 0, so every fold sees this before the first event
    /// that costs money.
    SessionCreated {
        model_policy: String,
        /// The membership this session was opened for.
        ///
        /// `None` in exactly one case, and it is not "unknown": a log written
        /// before the control plane existed, when there was nobody to record.
        /// Everything that writes this field writes a principal, so the absent
        /// case can only ever mean "older than tenancy" — which is why the
        /// fold gives it a marked row of its own instead of guessing a project
        /// (see [`PrincipalKey`](crate::control::PrincipalKey)).
        ///
        /// Carries a serde default for the same reason
        /// [`Usage::reasoning_tokens`] does: history has to keep deserializing
        /// after the type grows, or an upgrade silently costs a deployment its
        /// past.
        #[serde(default)]
        principal: Option<Principal>,
    },
    /// A turn was admitted. Carries the client's idempotency key.
    TurnStarted {
        turn_id: TurnId,
        response_id: ResponseId,
    },
    /// An item was committed to the canonical conversation.
    ItemAppended {
        item: Item,
    },
    /// The routing layer chose a target. Emitted before any bytes are produced
    /// so the audit trail records the decision even if execution then fails.
    Routed {
        response_id: ResponseId,
        decision: DecisionRecord,
    },
    OutputTextDelta {
        response_id: ResponseId,
        text: String,
    },
    ResponseCompleted {
        response_id: ResponseId,
        usage: Usage,
    },
    ResponseIncomplete {
        response_id: ResponseId,
        reason: IncompleteReason,
        usage: Usage,
    },
    /// A turn was re-sent after reconnect and served from the existing result.
    TurnDeduplicated {
        turn_id: TurnId,
        response_id: ResponseId,
    },
    Error {
        message: String,
    },
}

/// Notified of every event a session commits.
///
/// Lives beside the events it observes rather than beside its first
/// implementer. The session state machine is the lower layer, and having it
/// import its own observation seam from the metrics module — a reporting
/// concern built on top of it — pointed the dependency backwards. Anything
/// else wanting to watch the log, an exporter or a tracer, hangs off this same
/// seam instead of growing a second one.
///
/// Called while the session holds its lease and before the commit returns, so
/// an implementation must not block or await. A few integer additions is the
/// budget.
///
/// Implementations must be idempotent by `(session, seq)`. A session feeds its
/// replay through here as well as its subsequent commits, so an observer
/// without that property double-counts every session opened more than once,
/// which is every session that takes more than one turn.
pub trait SessionObserver: Send + Sync + 'static {
    fn observe(&self, events: &[SessionEvent]);
}

/// A sealed log entry. `seq` is assigned by the store on append and is
/// contiguous and strictly increasing within a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub seq: u64,
    pub session_id: SessionId,
    pub at_ms: u64,
    #[serde(flatten)]
    pub kind: SessionEventKind,
}

impl SessionEvent {
    /// The response this event belongs to, if any.
    ///
    /// Used to project the session-wide log down to the per-response view the
    /// Responses API exposes.
    pub fn response_id(&self) -> Option<&ResponseId> {
        match &self.kind {
            SessionEventKind::TurnStarted { response_id, .. }
            | SessionEventKind::Routed { response_id, .. }
            | SessionEventKind::OutputTextDelta { response_id, .. }
            | SessionEventKind::ResponseCompleted { response_id, .. }
            | SessionEventKind::ResponseIncomplete { response_id, .. }
            | SessionEventKind::TurnDeduplicated { response_id, .. } => Some(response_id),
            SessionEventKind::SessionCreated { .. }
            | SessionEventKind::ItemAppended { .. }
            | SessionEventKind::Error { .. } => None,
        }
    }

    /// Whether this event ends its response.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.kind,
            SessionEventKind::ResponseCompleted { .. }
                | SessionEventKind::ResponseIncomplete { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_events_are_recognized() {
        let done = SessionEvent {
            seq: 4,
            session_id: SessionId::new("s"),
            at_ms: 0,
            kind: SessionEventKind::ResponseCompleted {
                response_id: ResponseId::new("r"),
                usage: Usage::default(),
            },
        };
        assert!(done.is_terminal());

        let delta = SessionEvent {
            seq: 3,
            session_id: SessionId::new("s"),
            at_ms: 0,
            kind: SessionEventKind::OutputTextDelta {
                response_id: ResponseId::new("r"),
                text: "hi".into(),
            },
        };
        assert!(!delta.is_terminal());
        assert_eq!(delta.response_id(), Some(&ResponseId::new("r")));
    }

    #[test]
    fn a_log_written_before_the_control_plane_deserializes_with_no_principal() {
        // Byte-for-byte what `SessionCreated` serialized to before tenancy
        // existed. Such logs are still being replayed after an upgrade, and a
        // fold that refused to parse them would take the deployment's whole
        // history with it.
        let json = r#"{"type":"session_created","model_policy":"affinity"}"#;
        let kind: SessionEventKind = serde_json::from_str(json).unwrap();
        assert_eq!(
            kind,
            SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: None,
            },
            "an absent principal is `None`, which the fold marks rather than guesses at"
        );
    }

    #[test]
    fn session_created_round_trips_its_principal() {
        let kind = SessionEventKind::SessionCreated {
            model_policy: "affinity".into(),
            principal: Some(Principal::new("acme", "ada")),
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(
            json,
            r#"{"type":"session_created","model_policy":"affinity","principal":{"project":"acme","user":"ada"}}"#
        );
        assert_eq!(
            serde_json::from_str::<SessionEventKind>(&json).unwrap(),
            kind,
            "attribution has to survive the round trip, or a replay reattributes the spend"
        );
    }
}
