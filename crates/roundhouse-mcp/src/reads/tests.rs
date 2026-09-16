// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! [`reads`](super)'s unit tests, in their own file for the reason
//! `roundhouse-server`'s large modules (`mcp_api`, `conversations`,
//! `prefix_admission`, `claude_launch`, `relay_handoff`,
//! `control_config::directory`, `control_config::config`) already are: this
//! crate had no sibling-test-file convention of its own before M15's H7,
//! even though `reads.rs` crossed the same 1000-line threshold that moved
//! every one of those (M12.1 review, F3) — the resolver's own fixtures and
//! its `#[tokio::test]`s belong beside the surface they exercise, not
//! inline in the file that keeps growing to hold them.

use std::collections::HashMap;

use super::*;

fn subagent() -> SessionId {
    SessionId::new("acme/ada/sub")
}

fn most_recent() -> SessionId {
    SessionId::new("acme/ada/main")
}

fn thread() -> SessionId {
    SessionId::new("acme/ada/thread")
}

fn ada() -> Principal {
    Principal::new("acme", "ada")
}

/// The three tables an implementor supplies, and nothing else.
///
/// The order, the swallow and both refusals are the *provided*
/// [`ControlReads::resolve_session`]'s, so a double that fills these in is
/// exercising the shipped decision rather than a re-typed copy of it —
/// which is the whole of what M12.1 review F1 moved here.
#[derive(Default)]
struct Tables {
    names: HashMap<&'static str, SessionId>,
    /// The ingest's record of which session each thread's latest turn went
    /// to — a fourth table since R-M9, and the only one that can answer a
    /// subagent whose thread id was never anyone's cache key.
    threads: HashMap<&'static str, SessionId>,
    calls: HashMap<&'static str, SessionId>,
    latest: Option<SessionId>,
    /// A store that cannot answer, so a test can drive the one error the
    /// thread arm must *not* swallow.
    outage: bool,
    /// A per-table outage for the call table alone (review F11), so a
    /// test can drive an outage on the *last* arm while an earlier arm
    /// still answers — `outage` above fails every table together and
    /// cannot spell that case.
    calls_outage: bool,
}

#[async_trait]
impl ControlReads for Tables {
    async fn named_session(
        &self,
        _principal: &Principal,
        named: &str,
    ) -> Result<SessionId, SurfaceError> {
        if self.outage {
            return Err(SurfaceError::Internal("redis connection reset".into()));
        }
        self.names
            .get(named)
            .cloned()
            .ok_or_else(|| SurfaceError::ForeignConversation(named.to_string()))
    }

    async fn session_of_call(
        &self,
        _principal: &Principal,
        tool_use_id: &str,
    ) -> Result<Option<SessionId>, SurfaceError> {
        if self.outage || self.calls_outage {
            return Err(SurfaceError::Internal("redis connection reset".into()));
        }
        Ok(self.calls.get(tool_use_id).cloned())
    }

    async fn session_of_thread(
        &self,
        _principal: &Principal,
        thread_id: &str,
    ) -> Result<Option<SessionId>, SurfaceError> {
        if self.outage {
            return Err(SurfaceError::Internal("redis connection reset".into()));
        }
        Ok(self.threads.get(thread_id).cloned())
    }

    async fn latest_session(&self, _principal: &Principal) -> Option<SessionId> {
        self.latest.clone()
    }

    async fn ceiling_policy(&self, _principal: &Principal) -> Result<TurnPolicy, SurfaceError> {
        unimplemented!("these tests ask this trait exactly one question")
    }

    async fn admissible_targets(
        &self,
        _principal: &Principal,
        _policy: &TurnPolicy,
    ) -> Result<Vec<Target>, SurfaceError> {
        unimplemented!("these tests ask this trait exactly one question")
    }

    async fn balance(&self, _principal: &Principal) -> Result<Option<Balance>, SurfaceError> {
        unimplemented!("these tests ask this trait exactly one question")
    }

    async fn session_facts(&self, _session: &SessionId) -> Result<SessionFacts, SurfaceError> {
        unimplemented!("these tests ask this trait exactly one question")
    }

    fn now_ms(&self) -> u64 {
        0
    }
}

fn correlators(thread_id: Option<&str>, tool_use_id: Option<&str>) -> Correlators {
    Correlators {
        thread_id: thread_id.map(str::to_string),
        tool_use_id: tool_use_id.map(str::to_string),
        cache_key: None,
    }
}

/// The `_meta` a codex client actually sends: its thread id and, beside it,
/// the turn metadata's `session_id` — which is that turn's
/// `prompt_cache_key` (R-C5).
///
/// Both together rather than one helper each, because the ordering between
/// them is the thing under test and a fixture that could send only one of
/// them would make the interesting case unspellable.
fn codex_meta(thread_id: Option<&str>, cache_key: Option<&str>) -> Correlators {
    Correlators {
        thread_id: thread_id.map(str::to_string),
        tool_use_id: None,
        cache_key: cache_key.map(str::to_string),
    }
}

/// Just the call correlator, as R-M2 left it: exact where `latest` is a
/// guess, and both of them absent is a refusal rather than an invention.
#[tokio::test]
async fn the_tool_use_id_decides_and_the_most_recent_conversation_only_catches() {
    let tables = Tables {
        calls: HashMap::from([("toolu_sub", subagent())]),
        latest: Some(most_recent()),
        ..Tables::default()
    };
    assert_eq!(
        tables
            .resolve_session(&ada(), None, &correlators(None, Some("toolu_sub")))
            .await
            .ok(),
        Some(subagent()),
        "an id the node emitted is exact, so it outranks a guess"
    );
    assert_eq!(
        tables
            .resolve_session(&ada(), None, &correlators(None, Some("toolu_nobody")))
            .await
            .ok(),
        Some(most_recent()),
        "an id that names none of this caller's sessions resolves to \
         nothing and falls through rather than refusing — unknown, \
         evicted, ambiguous and foreign all answer alike, and so does an \
         absent id"
    );
    assert!(
        matches!(
            Tables::default()
                .resolve_session(&ada(), None, &Correlators::default())
                .await,
            Err(SurfaceError::NoSession)
        ),
        "a node that has served this principal no turn refuses rather \
         than inventing a conversation"
    );
}

/// R-M7: the thread id is a correlator too, and it is the first one.
#[tokio::test]
async fn the_thread_id_is_weighed_ahead_of_the_tool_use_id_and_both_ahead_of_latest() {
    let tables = Tables {
        names: HashMap::from([("thread", thread())]),
        calls: HashMap::from([("toolu_sub", subagent())]),
        latest: Some(most_recent()),
        ..Tables::default()
    };
    assert_eq!(
        tables
            .resolve_session(
                &ada(),
                None,
                &correlators(Some("thread"), Some("toolu_sub"))
            )
            .await
            .ok(),
        Some(thread()),
        "threadId first (R-M7): it is a *name* the client resolved through \
         the caller's own namespace, where the tool-use id is a lookup in \
         a node-local table"
    );
    assert_eq!(
        tables
            .resolve_session(&ada(), None, &correlators(Some("thread"), None))
            .await
            .ok(),
        Some(thread()),
        "and on its own it still outranks the guess — the control that \
         proves the assertion above is about the order and not about the \
         tool-use id being present"
    );
    assert_eq!(
        tables
            .resolve_session(&ada(), None, &correlators(None, Some("toolu_sub")))
            .await
            .ok(),
        Some(subagent()),
        "a client sending only the other correlator is unaffected by R-M7"
    );
}

/// M12.1 review, F2 (R-M9): within the thread step, the ingest's own
/// record outranks reading the thread id as a cache key.
///
/// The two assertions are the two halves of why the order matters. A
/// subagent's thread id is *nobody's* cache key, so the name lookup can
/// only miss and the table is the sole thing that can answer it. And when
/// both could answer, the table is the one that watched this thread's own
/// turn go past, where the name is the whole agent family's cache key at
/// whatever generation it has since reached.
#[tokio::test]
async fn a_threads_own_binding_outranks_reading_its_id_as_a_cache_key() {
    let tables = Tables {
        names: HashMap::from([("thread", most_recent())]),
        threads: HashMap::from([("thread", thread()), ("subagent-thread", subagent())]),
        latest: Some(most_recent()),
        ..Tables::default()
    };

    assert_eq!(
        tables
            .resolve_session(&ada(), None, &correlators(Some("subagent-thread"), None))
            .await
            .ok(),
        Some(subagent()),
        "a thread id that was never a cache key is answerable only from \
         the ingest's own record; without it the call falls through to \
         the parent's conversation, which is F2"
    );
    assert_eq!(
        tables
            .resolve_session(&ada(), None, &correlators(Some("thread"), None))
            .await
            .ok(),
        Some(thread()),
        "and where both could answer, the binding this deployment wrote \
         for *this thread* outranks the name it shares with its family"
    );

    // The control: strip the table and the same call falls back to the
    // name, which is the path a root thread takes on a deployment that
    // recorded no binding — so the assertions above are about the order
    // and not about the name lookup having stopped working.
    let unrecorded = Tables {
        names: HashMap::from([("thread", most_recent())]),
        latest: None,
        ..Tables::default()
    };
    assert_eq!(
        unrecorded
            .resolve_session(&ada(), None, &correlators(Some("thread"), None))
            .await
            .ok(),
        Some(most_recent()),
    );
}

/// M12.1 review, F1: the thread arm swallows `ForeignConversation` and
/// nothing else, and it does so *here* rather than once per implementor.
///
/// The asymmetry used to be re-typed at every call site: the server spelled
/// it as a match on the variant, both test doubles spelled it `.ok()`, and
/// nothing was red because neither double had a store that could fail.
/// Whatever an implementor's `named_session` reads, only one arm of it is
/// the caller's business.
#[tokio::test]
async fn a_thread_id_swallows_a_foreign_conversation_and_nothing_else() {
    let unknown = Tables {
        latest: Some(most_recent()),
        ..Tables::default()
    };
    assert_eq!(
        unknown
            .resolve_session(&ada(), None, &correlators(Some("nobodys"), None))
            .await
            .ok(),
        Some(most_recent()),
        "a thread id naming no conversation of this caller's falls through \
         as an unknown tool-use id does"
    );

    let outage = Tables {
        latest: Some(most_recent()),
        outage: true,
        ..Tables::default()
    };
    let error = outage
        .resolve_session(&ada(), None, &correlators(Some("main"), None))
        .await
        .expect_err("a store that cannot answer has not answered");
    assert!(
        matches!(error, SurfaceError::Internal(_)),
        "a deployment that cannot answer must say so rather than hand the \
         caller its `latest` — a plausible answer about the wrong \
         conversation is the failure R-M7 exists to remove: {error}"
    );
}

/// R-M7's refusal: the model's argument and the client's correlator
/// disagreeing is not a precedence question.
#[tokio::test]
async fn an_argument_that_contradicts_the_clients_correlator_is_refused_naming_both() {
    let tables = Tables {
        names: HashMap::from([("main", most_recent()), ("thread", thread())]),
        calls: HashMap::from([("toolu_sub", subagent())]),
        // Never consulted below: every call names something that resolves.
        latest: None,
        ..Tables::default()
    };

    let refused = tables
        .resolve_session(&ada(), Some("main"), &correlators(Some("thread"), None))
        .await
        .expect_err("a caller contradicting itself is refused");
    let message = refused.to_string();
    assert!(
        matches!(refused, SurfaceError::ContradictoryConversation { .. }),
        "and refused as its own variant, not as a tenancy verdict about \
         either conversation: {message}"
    );
    // **Both**, because either one alone leaves the agent guessing which of
    // its own two inputs the deployment disliked — and the argument is the
    // half a model can actually change.
    assert!(
        message.contains(most_recent().as_str()) && message.contains(thread().as_str()),
        "the refusal must name both conversations: {message}"
    );

    // The control, and the ordinary case: an argument that *agrees* with
    // the correlator is served, so the refusal above is about the
    // disagreement and not about sending both at once.
    assert_eq!(
        tables
            .resolve_session(&ada(), Some("main"), &correlators(Some("main"), None))
            .await
            .ok(),
        Some(most_recent()),
    );

    // And the correlator that is compared is the *effective* one — the one
    // the order would have used — so a tool-use id behind an agreeing
    // thread id does not manufacture a contradiction the client never had.
    assert_eq!(
        tables
            .resolve_session(
                &ada(),
                Some("main"),
                &correlators(Some("main"), Some("toolu_sub"))
            )
            .await
            .ok(),
        Some(most_recent()),
    );

    // A named argument with no correlator at all is the pre-R-M7 path and
    // still answers: the refusal needs two answers to be a contradiction.
    assert_eq!(
        tables
            .resolve_session(&ada(), Some("main"), &Correlators::default())
            .await
            .ok(),
        Some(most_recent()),
    );
}

/// M14.1, R-C5: a codex root thread resolves from the cache key it was
/// already sending, on a deployment holding no thread binding at all.
///
/// This is the whole of what the third correlator buys. The thread table is
/// *empty* here — the state of any node that served none of this
/// conversation's turns — and before this arm the call fell through to
/// `latest`, which for a principal running several agents is a coin toss
/// and for a fresh node is `NoSession`. The name is in `names` because
/// that is what a durable generation map makes true: at generation zero the
/// session id is a pure function of the caller and this string.
#[tokio::test]
async fn a_codex_root_thread_resolves_from_its_cache_key_with_no_thread_binding() {
    let tables = Tables {
        names: HashMap::from([("cache-key", thread())]),
        latest: Some(most_recent()),
        ..Tables::default()
    };

    assert_eq!(
        tables
            .resolve_session(
                &ada(),
                None,
                &codex_meta(Some("cache-key"), Some("cache-key"))
            )
            .await
            .ok(),
        Some(thread()),
        "a root thread stamps one string as both its thread id and its              session id, and either route reaches the same conversation"
    );

    // The case the arm exists for: a thread id that is *not* the cache key
    // and that nothing has bound. The thread arm misses both ways — no
    // binding, and no conversation under that name — and the cache key is
    // what is left before the guess.
    assert_eq!(
        tables
            .resolve_session(
                &ada(),
                None,
                &codex_meta(Some("unbound-thread"), Some("cache-key"))
            )
            .await
            .ok(),
        Some(thread()),
        "the cache key answers where the thread arm found nothing, rather              than the call falling to `latest`"
    );

    // CONTROL: the arm is a *fallback* and not a promotion. A subagent
    // whose own thread is bound stays in its own conversation, where its
    // family's shared cache key would have answered about the parent —
    // which is F2 exactly.
    let with_binding = Tables {
        names: HashMap::from([("cache-key", most_recent())]),
        threads: HashMap::from([("subagent-thread", subagent())]),
        latest: None,
        ..Tables::default()
    };
    assert_eq!(
        with_binding
            .resolve_session(
                &ada(),
                None,
                &codex_meta(Some("subagent-thread"), Some("cache-key"))
            )
            .await
            .ok(),
        Some(subagent()),
        "the thread binding is exact and the family's cache key is not, so              reading the cache key first would answer every subagent about its              parent"
    );

    // CONTROL: a cache key naming no conversation of this caller's falls
    // through like any other correlator, rather than refusing.
    assert_eq!(
        tables
            .resolve_session(&ada(), None, &codex_meta(None, Some("nobodys")))
            .await
            .ok(),
        Some(most_recent()),
    );

    // CONTROL: a Claude-shaped call carries none of this and is unaffected.
    let claude = Tables {
        calls: HashMap::from([("toolu_sub", subagent())]),
        latest: Some(most_recent()),
        ..Tables::default()
    };
    assert_eq!(
        claude
            .resolve_session(&ada(), None, &correlators(None, Some("toolu_sub")))
            .await
            .ok(),
        Some(subagent()),
    );
}

/// M14.1: a table lookup that could not reach its store refuses, where an
/// id the store answered "no" about falls through.
///
/// The two are one line apart in the resolver and were one answer before
/// the tables became shared: `Option` could only say "nothing of yours",
/// so an unreachable store handed the caller `latest` — a plausible answer
/// about the wrong conversation. The control below is what proves this
/// test is about the outage and not about the correlator being unknown.
#[tokio::test]
async fn a_correlator_whose_store_is_unreachable_refuses_rather_than_guessing() {
    let outage = Tables {
        calls: HashMap::from([("toolu_sub", subagent())]),
        latest: Some(most_recent()),
        outage: true,
        ..Tables::default()
    };
    let error = outage
        .resolve_session(&ada(), None, &correlators(None, Some("toolu_sub")))
        .await
        .expect_err("a store that cannot answer has not answered");
    assert!(
        matches!(error, SurfaceError::Internal(_)),
        "an unreachable call table must not read as an unknown id: {error}"
    );

    let thread_outage = Tables {
        latest: Some(most_recent()),
        outage: true,
        ..Tables::default()
    };
    assert!(
        matches!(
            thread_outage
                .resolve_session(&ada(), None, &correlators(Some("thread"), None))
                .await,
            Err(SurfaceError::Internal(_))
        ),
        "and neither must an unreachable thread table"
    );

    // CONTROL: the same tables, reachable. An id nothing bound falls
    // through to the guess, which is the answer the outage must not be
    // confused with.
    let reachable = Tables {
        latest: Some(most_recent()),
        ..Tables::default()
    };
    assert_eq!(
        reachable
            .resolve_session(&ada(), None, &correlators(None, Some("toolu_nobody")))
            .await
            .ok(),
        Some(most_recent()),
    );
}

/// Review M14.1 F11, closed: the tool-use-id arm (4) used to run
/// unconditionally while the cache-key arm (3) short-circuited on the
/// thread arm's answer, and since M14.1 the call-table lookup propagates
/// its error with `?` — so a call-table outage refused a call arm (2) had
/// already answered. Under one lazy chain arm (4) is never consulted here,
/// and an arm that is not consulted cannot refuse.
#[tokio::test]
async fn f11_lazy_chain_skips_tool_use_id_arm_once_thread_arm_answers() {
    let tables = Tables {
        threads: HashMap::from([("thread", subagent())]),
        calls_outage: true,
        ..Tables::default()
    };
    assert_eq!(
        tables
            .resolve_session(&ada(), None, &correlators(Some("thread"), Some("toolu")))
            .await
            .ok(),
        Some(subagent()),
        "the thread arm already answered; a later arm's outage must not \
         refuse a call the resolver could already serve",
    );
}

/// CONTROL for the test above: the same outage, but with no earlier arm
/// to answer first. Here arm (4) *must* be consulted — the whole
/// resolution rests on it — so its outage refusing is correct, and stays
/// correct under the lazy chain. This is what proves the test above is
/// about ordering, not about whether a call-table outage should ever
/// refuse.
#[tokio::test]
async fn f11_control_tool_use_id_arm_outage_refuses_when_nothing_earlier_answered() {
    let tables = Tables {
        calls_outage: true,
        ..Tables::default()
    };
    let error = tables
        .resolve_session(&ada(), None, &correlators(None, Some("toolu")))
        .await
        .expect_err("no earlier arm answered, so the call table's outage must surface");
    assert!(
        matches!(error, SurfaceError::Internal(_)),
        "must refuse as an outage, not read as an unknown id: {error}"
    );
}
