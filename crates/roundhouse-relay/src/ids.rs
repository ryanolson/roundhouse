// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Identifiers that survive a replay.
//!
//! ATOF gives every event a UUID and every scope a span id shared by its start
//! and its end. Relay's own producer mints those with `Uuid::now_v7`, which is
//! right for a live runtime and wrong for us: these documents are produced by
//! *cold replay* of a finished session, and a v7 (or a v4) would make two
//! exports of one unchanged log differ in every id. A consumer diffing two
//! trajectories to see what a re-run changed would find that everything changed.
//!
//! So every id here is a **UUIDv5 digest of facts already in the log** — the
//! session id plus a name that says which thing in that session this is. Two
//! consequences worth stating, because they are the reason to prefer this over
//! carrying the log's own string ids in a metadata field:
//!
//! - the same session exported twice, on two nodes, a month apart, produces
//!   byte-identical documents;
//! - two *different* sessions never collide, because the session id is inside
//!   the digest rather than beside it.
//!
//! The cost is that an id here is not resolvable back to a log entry by a
//! consumer — v5 is one-way. That is what the events' own `data` payloads are
//! for: the routing scope carries the response id in the clear.

use roundhouse_core::ids::SessionId;
use uuid::Uuid;

/// The namespace every id this crate mints hangs off.
///
/// Derived rather than typed out as sixteen literal bytes, so the value cannot
/// be transcribed wrongly and the string it comes from documents itself. It is a
/// v5 of the URL namespace, which is the standard way to name a namespace that
/// has a URL — and this one does, because the format it names is Relay's.
///
/// Computed per call: a SHA-1 over forty bytes, against a producer that already
/// walks a whole session log. A `LazyLock` would buy nothing measurable and add
/// a static.
pub fn namespace() -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        b"https://github.com/NVIDIA/NeMo-Relay/atof#roundhouse",
    )
}

/// A stable id for the thing `name` identifies within `session`.
///
/// `name` is a path — `"session"`, `"turn/resp_7"`, `"route/resp_7"` — and its
/// spelling is part of the wire contract, since changing it changes every id in
/// every document a deployment has already exported. The [`Name`] constructors
/// below exist so those spellings live in one place rather than at each call
/// site.
pub fn derive(session: &SessionId, name: &str) -> Uuid {
    Uuid::new_v5(
        &namespace(),
        format!("{}\u{1f}{}", session.as_str(), name).as_bytes(),
    )
}

/// The names [`derive`] is called with, spelled once.
///
/// A unit struct of associated functions rather than a set of free ones, so that
/// "what can be identified in a session" reads as a closed list. The separator
/// inside [`derive`] is an ASCII unit separator precisely so that a session id
/// containing a slash — which every namespaced one does,
/// `{project}/{user}/{key}` — cannot collide with a name containing one.
pub struct Name;

impl Name {
    /// The session-wide agent scope every turn hangs off.
    ///
    /// One per document, and its uuid is what every other event names as its
    /// `parent_uuid`. That is load-bearing rather than decorative: the
    /// ATOF→ATIF converter finds the trajectory root by looking for the
    /// `agent` scope-start with no parent, and it deduplicates repeated input
    /// messages per `(parent_uuid, role)` — so turns that did not share one
    /// parent would each re-emit the conversation's history as fresh user
    /// steps.
    pub fn session() -> String {
        "session".to_string()
    }

    /// The LLM scope for one dispatched turn. Start and end share it, because
    /// in ATOF a scope is a span and its two events are its edges.
    pub fn turn(response_id: &str) -> String {
        format!("turn/{response_id}")
    }

    /// The context scope carrying one routing decision.
    pub fn route(response_id: &str) -> String {
        format!("route/{response_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The namespace is a wire constant, so it is pinned as a literal.
    ///
    /// Not a tautology against the expression above: the point is that a future
    /// edit to the URL string — a typo, a repository rename — is caught here
    /// rather than by a consumer whose stored trajectory ids stopped matching
    /// the ones we now export.
    #[test]
    fn the_namespace_is_a_fixed_value() {
        assert_eq!(
            namespace().to_string(),
            "04b4e885-ea1e-57c3-9322-63446c1b50fc"
        );
    }

    #[test]
    fn ids_are_stable_across_calls_and_distinct_across_sessions() {
        let a = SessionId::new("acme/ada/main");
        let b = SessionId::new("globex/bob/main");
        assert_eq!(derive(&a, &Name::session()), derive(&a, &Name::session()));
        assert_ne!(derive(&a, &Name::session()), derive(&b, &Name::session()));
        assert_ne!(derive(&a, &Name::turn("r1")), derive(&a, &Name::turn("r2")));
        assert_ne!(
            derive(&a, &Name::turn("r1")),
            derive(&a, &Name::route("r1"))
        );
    }

    /// The separator has to do its job, or two sessions can name one event.
    ///
    /// `sess` + `"a/turn/x"` and `sess/a` + `"turn/x"` would be the same string
    /// under a `/` join, and the second is an ordinary namespaced session id.
    #[test]
    fn a_session_id_containing_a_slash_cannot_borrow_another_sessions_id() {
        let outer = SessionId::new("acme/ada");
        let inner = SessionId::new("acme");
        assert_ne!(derive(&outer, "turn/r1"), derive(&inner, "ada/turn/r1"));
    }
}
