// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Identifier newtypes.
//!
//! These are distinct types rather than `String` aliases because the routing
//! and store layers pass several of them side by side, and transposing a
//! session id with a response id is the kind of bug that surfaces only under
//! failover.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! string_id {
    ($name:ident, $prefix:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Mint a fresh, prefixed, globally unique id.
            pub fn generate() -> Self {
                Self(format!("{}_{}", $prefix, uuid::Uuid::new_v4().simple()))
            }

            /// Adopt a client- or store-supplied id verbatim.
            pub fn new(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(raw: String) -> Self {
                Self(raw)
            }
        }

        impl From<&str> for $name {
            fn from(raw: &str) -> Self {
                Self(raw.to_string())
            }
        }
    };
}

// Shared with `control`, which mints the same shape of newtype for the tenancy
// ids. Exported rather than copied so "an id is a transparent string newtype
// with a prefixed generator" stays one definition.
pub(crate) use string_id;

string_id!(SessionId, "sess", "Identifies one long-lived conversation.");
string_id!(
    ResponseId,
    "resp",
    "Identifies one model response. This is what a client passes back as `previous_response_id`."
);
string_id!(
    TurnId,
    "turn",
    "Client-supplied idempotency key for a single turn.\n\nRe-sending a turn after a reconnect must not produce a second response, so\nthe session replays the existing outcome when it sees a `TurnId` it has\nalready completed."
);
string_id!(
    SideCallId,
    "sc",
    "Identifies one model call this deployment made for its own purposes.\n\nA side call is not a turn: no client asked for it, it emits no conversation\nitem, and it never appears on the wire. It has an id of its own precisely so\nits money can be booked and joined to the decision that caused it without\nborrowing a [`ResponseId`], which would make the side call look like part of\nthe response a client is streaming."
);
string_id!(
    ValidationId,
    "val",
    "Identifies one consultation of the validate/steer loop.\n\nSeparate from [`SideCallId`] because a validation may run *without* a side\ncall — a spent budget, an arm that consults nobody — and a side call may\nexist whose verdict was unusable. One id per question asked, one per model\ncall made, and the join between them is a field rather than an assumption."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_prefixed_and_unique() {
        let a = SessionId::generate();
        let b = SessionId::generate();
        assert!(a.as_str().starts_with("sess_"));
        assert_ne!(a, b);
    }

    #[test]
    fn ids_roundtrip_through_json_as_bare_strings() {
        let id = ResponseId::new("resp_abc");
        let encoded = serde_json::to_string(&id).unwrap();
        assert_eq!(encoded, "\"resp_abc\"");
        assert_eq!(serde_json::from_str::<ResponseId>(&encoded).unwrap(), id);
    }
}
