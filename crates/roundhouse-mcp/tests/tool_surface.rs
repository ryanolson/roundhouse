// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The tool surface, exercised through [`dispatch`] and against nothing but the
//! [`ControlReads`](roundhouse_mcp::reads::ControlReads) seam.
//!
//! Every test here calls the tools the way a client does — by name, with a JSON
//! arguments object — because the name and the object are the contract. A test
//! that called the trait methods directly would keep passing through a
//! dispatcher that routed `prefer` to `status`.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use roundhouse_core::control::{Principal, TurnPolicy};
use roundhouse_core::ids::SessionId;
use roundhouse_mcp::reads::SessionFacts;
use roundhouse_mcp::surface::ControlSurface;
use roundhouse_mcp::{
    ControlPlaneSurface, ControlStore, ToolCall, ToolOutcome, descriptors, dispatch,
};

use common::*;

/// Call a tool the way a client does, and parse the one text block back.
async fn call(
    surface: &dyn ControlSurface,
    principal: &Principal,
    name: &str,
    arguments: Value,
) -> ToolOutcome {
    dispatch(
        surface,
        principal,
        ToolCall {
            name: name.to_string(),
            arguments,
        },
    )
    .await
}

/// The JSON a served tool answered with.
fn served(outcome: &ToolOutcome) -> Value {
    assert!(
        !outcome.is_error(),
        "expected a served result, got a refusal: {}",
        outcome.text()
    );
    serde_json::from_str(outcome.text()).expect("a served tool answers with JSON")
}

// ---------------------------------------------------------------------------
// The list
// ---------------------------------------------------------------------------

#[test]
fn the_tool_list_is_stable_and_golden_pinned() {
    // Two pins, because they fail differently and a reader needs both. The
    // shape assertion below names the tool and the property that moved; the
    // digest catches everything the shape ignores -- above all a *description*
    // edit, which is the half of the contract a model actually reads and the
    // half no structural assertion can see.
    //
    // **When the digest fails, the published contract changed.** That is not a
    // literal to update on the way past: a client caches this list and Codex
    // folds it into the prompt, so every session in a deployment re-primes its
    // prompt cache the moment it moves. Update the literal only once the change
    // is the intended one.
    //
    // **What the digest covers is name, description and schema, and not the
    // MCP annotations**, which also go on the wire in `tools/list`. They are
    // pinned instead by
    // `every_tool_states_what_it_does_to_a_client_that_was_handed_no_config`,
    // which asserts the exact triple per tool rather than hashing it, because
    // the useful failure there names the tool and the hint rather than saying
    // a digest moved. The split is why adding the hints (F06) left this
    // literal untouched -- worth knowing before reading the digest as a pin on
    // everything a client sees.
    let list: Vec<Value> = descriptors()
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.input_schema,
            })
        })
        .collect();
    let canonical = serde_json::to_string(&list).expect("the tool list serializes");
    assert_eq!(
        hex_digest(&canonical),
        // Moved twice since M5 shipped, both times deliberately.
        //
        // The first: two descriptions said things the deployment does not do.
        // `status` was advertised as costing nothing when every call replayed
        // the session log, and `init_session` was advertised as performing a
        // correlation whose read side does not land until M7.
        //
        // The second is **M10.0 (T4)**, and it is the larger of the two: the
        // steer became a text instruction, so the two steer tools stopped being
        // keyed by a synthetic call's id. `fetch_steer` and `report_outcome`
        // both take a `conversation` now — optional, like every other
        // session-scoped tool — instead of a required `steer_id`, and their
        // descriptions say what they are for now that the correction arrives as
        // the turn's own answer: re-reading it, not receiving it. `status` lost
        // `open_steers` from its description for the same reason, and gained an
        // honest account of what it *does* cost. **The tool count is unchanged
        // at eight**, and that is a decision rather than an accident: a surface
        // that shrank would re-prime every prompt cache in the deployment to
        // delete a read that still answers a real question.
        "239288254d69f509bb7556197eebefeff7c3361b4142b62c46704569897a81a4",
        "the published tool list changed; see this test's comment before editing the literal"
    );

    // The readable half: names in order, and the argument shape of each.
    let shape: Vec<(String, Vec<String>, Vec<String>)> = list
        .iter()
        .map(|tool| {
            let mut properties: Vec<String> = tool["inputSchema"]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            properties.sort();
            let required: Vec<String> = tool["inputSchema"]["required"]
                .as_array()
                .map(|entries| {
                    entries
                        .iter()
                        .map(|entry| entry.as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default();
            (
                tool["name"].as_str().unwrap().to_string(),
                properties,
                required,
            )
        })
        .collect();
    let expected: Vec<(&str, Vec<&str>, Vec<&str>)> = vec![
        ("status", vec!["conversation"], vec![]),
        ("init_session", vec!["conversation"], vec![]),
        (
            "declare_intent",
            vec!["conversation", "done_when", "goal", "plan_steps"],
            vec!["goal", "done_when"],
        ),
        (
            "prefer",
            vec!["conversation", "mode", "reason", "scope", "turns"],
            vec!["mode", "scope", "reason"],
        ),
        (
            "set_quality_floor",
            vec!["conversation", "floor", "reason", "turns"],
            vec!["floor", "turns", "reason"],
        ),
        ("fetch_steer", vec!["conversation"], vec![]),
        (
            "report_outcome",
            vec!["conversation", "note", "outcome"],
            vec!["outcome"],
        ),
        ("explain_last_route", vec!["conversation"], vec![]),
    ];
    assert_eq!(
        shape,
        expected
            .into_iter()
            .map(|(name, properties, required)| (
                name.to_string(),
                properties.into_iter().map(str::to_string).collect(),
                required.into_iter().map(str::to_string).collect()
            ))
            .collect::<Vec<_>>()
    );

    // Every schema is closed. An open one lets a client send a field the
    // surface silently drops, which is how an agent comes to believe it set a
    // preference it did not set.
    for tool in descriptors() {
        assert_eq!(
            tool.input_schema["additionalProperties"],
            json!(false),
            "`{}` accepts arguments it never declared",
            tool.name
        );
    }
}

fn hex_digest(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[tokio::test]
async fn every_tool_result_is_a_single_text_block() {
    // The invariant the conversation prefix depends on: a tool result travels
    // back into a session as an item, and our canonicalizer round-trips it
    // through its string branch. Two blocks, or a structured object beside the
    // text, and the bytes the client resends next turn are not the bytes we
    // emitted.
    // The conversation is one roundhouse has corrected, so the served path of
    // `fetch_steer` is in the sweep rather than only its refusal.
    let (surface, _store) = FakeDeployment::default()
        .with_facts(
            &adas_session(),
            SessionFacts {
                latest_guidance: Some("re-read the task".into()),
                last_decision: None,
            },
        )
        .surface();

    let calls = vec![
        ("status", json!({})),
        ("init_session", json!({})),
        (
            "declare_intent",
            json!({"goal": "ship the parser", "done_when": "cargo test is green"}),
        ),
        (
            "prefer",
            json!({"mode": "local", "scope": "session", "reason": "cheap work"}),
        ),
        (
            "set_quality_floor",
            json!({"floor": 0.5, "turns": 3, "reason": "hard work"}),
        ),
        ("fetch_steer", json!({})),
        ("report_outcome", json!({"outcome": "applied"})),
        ("explain_last_route", json!({})),
        // The refusal paths travel the same way, which is the half a happy-path
        // sweep would miss.
        ("fetch_steer", json!({"conversation": "other/bob/sess_1"})),
        ("prefer", json!({"mode": "local"})),
        ("no_such_tool", json!({})),
    ];
    assert!(
        calls.len() > descriptors().len(),
        "the sweep covers every tool plus the refusal paths"
    );

    for (name, arguments) in calls {
        let outcome = call(&surface, &ada(), name, arguments.clone()).await;
        let wire = outcome.to_call_tool_json();
        let content = wire["content"].as_array().expect("a content array");
        assert_eq!(
            content.len(),
            1,
            "`{name}` answered with {} blocks",
            content.len()
        );
        assert_eq!(content[0]["type"], json!("text"), "`{name}`");
        assert!(
            content[0]["text"].is_string() && !content[0]["text"].as_str().unwrap().is_empty(),
            "`{name}` answered with an empty block"
        );
        assert!(
            wire.get("structuredContent").is_none(),
            "`{name}` published a structured result beside its text"
        );
    }
}

// ---------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_over_asking_overlay_is_narrowed_and_says_so() {
    // The plan's own example: `prefer frontier` on a local-only project. The
    // ask cannot be honored -- there is no hosted target this key may use --
    // and honoring it as written would narrow the admissible set to nothing,
    // which fails every remaining turn of the session at a seam the agent
    // cannot reach.
    let (surface, store) = FakeDeployment::local_only().surface();
    let before = TurnPolicy {
        allow: roundhouse_core::control::TargetFilter::parse(["local/*"]).unwrap(),
        ..TurnPolicy::unrestricted()
    }
    .digest();

    let answer = served(
        &call(
            &surface,
            &ada(),
            "prefer",
            json!({"mode": "frontier", "scope": "session", "reason": "this needs a big model"}),
        )
        .await,
    );

    assert_eq!(answer["narrowed"], json!(true));
    assert!(
        answer["narrowed_because"]
            .as_str()
            .unwrap()
            .contains("left as it was"),
        "the agent has to be told what happened instead: {answer}"
    );
    assert_eq!(
        answer["admissible_targets"],
        json!(["local/llama-3.1-8b"]),
        "and left routable"
    );
    assert_eq!(
        answer["policy_digest"], before,
        "an unhonorable ask moves no digest, which is what the audit trail will say too"
    );
    assert!(answer["overlay"].is_null(), "nothing was stored: {answer}");
    assert!(
        store.overlay(&adas_session()).is_none(),
        "and nothing reaches the turn that follows"
    );
}

#[tokio::test]
async fn a_within_ceiling_overlay_applies_verbatim() {
    // The control. The identical call shape, on a deployment where the ask is
    // satisfiable, is honored in full and moves the digest -- which is the
    // observable the next turn's `DecisionRecord` carries.
    let (surface, store) = FakeDeployment::default().surface();
    let before = TurnPolicy::unrestricted().digest();

    let answer = served(
        &call(
            &surface,
            &ada(),
            "prefer",
            json!({"mode": "local", "scope": "session", "turns": 4, "reason": "bulk edits"}),
        )
        .await,
    );

    assert_eq!(answer["narrowed"], json!(false));
    assert!(
        answer.get("narrowed_because").is_none(),
        "a served ask explains nothing, because nothing happened to it"
    );
    assert_eq!(answer["admissible_targets"], json!(["local/llama-3.1-8b"]));
    assert_ne!(
        answer["policy_digest"], before,
        "an applied overlay has to be visible in the fingerprint or the audit trail cannot see it"
    );
    assert_eq!(answer["overlay"]["mode"], json!("local"));
    assert_eq!(answer["overlay"]["mode_reason"], json!("bulk edits"));
    assert_eq!(answer["overlay"]["mode_turns_remaining"], json!(4));

    // And the engine reads the same narrowing out of the store.
    let overrides = store
        .consume_overlay(&adas_session())
        .expect("the turn that follows resolves it");
    assert_eq!(
        TurnPolicy::unrestricted().narrow(&overrides).digest(),
        answer["policy_digest"].as_str().unwrap(),
        "the digest the tool reported and the one the next turn will record are one number"
    );
}

#[tokio::test]
async fn a_quality_floor_below_the_ceiling_is_clamped_and_says_so() {
    // The second overlay axis, and the other reason an ask is narrowed: not
    // "there is nothing there" but "your policy is already tighter than that".
    let deployment = FakeDeployment {
        ceiling: TurnPolicy {
            min_quality: 0.8,
            ..TurnPolicy::unrestricted()
        },
        ..FakeDeployment::default()
    };
    let (surface, _store) = deployment.surface();

    let widening = served(
        &call(
            &surface,
            &ada(),
            "set_quality_floor",
            json!({"floor": 0.1, "turns": 2, "reason": "cheap is fine"}),
        )
        .await,
    );
    assert_eq!(widening["narrowed"], json!(true));
    assert!(
        widening["narrowed_because"]
            .as_str()
            .unwrap()
            .contains("already at least this narrow")
    );
    assert_eq!(
        widening["admissible_targets"],
        json!(["anthropic/claude-opus-4", "openai/gpt-5"]),
        "the ceiling's own floor still binds, which is the whole reason a lower ask is safe"
    );

    // The control: a floor above the ceiling's is a narrowing, and applies.
    let tightening = served(
        &call(
            &surface,
            &ada(),
            "set_quality_floor",
            json!({"floor": 0.93, "turns": 2, "reason": "this one is subtle"}),
        )
        .await,
    );
    assert_eq!(tightening["narrowed"], json!(false));
    assert_eq!(
        tightening["admissible_targets"],
        json!(["anthropic/claude-opus-4"])
    );

    // And the probe for the other clamp on this axis: a floor nothing clears.
    let impossible = served(
        &call(
            &surface,
            &ada(),
            "set_quality_floor",
            json!({"floor": 0.99, "turns": 2, "reason": "only the best"}),
        )
        .await,
    );
    assert_eq!(impossible["narrowed"], json!(true));
    assert_eq!(
        impossible["admissible_targets"],
        json!(["anthropic/claude-opus-4"]),
        "the session keeps the overlay it had rather than being pinned to an empty set"
    );
}

#[tokio::test]
async fn a_missing_reason_is_refused_naming_the_field() {
    let (surface, store) = FakeDeployment::default().surface();

    // Absent: serde names the field, and the surface passes the name through
    // rather than flattening it to "invalid arguments".
    let absent = call(
        &surface,
        &ada(),
        "prefer",
        json!({"mode": "local", "scope": "session"}),
    )
    .await;
    assert!(absent.is_error());
    assert!(
        absent.text().contains("reason"),
        "an agent that is not told which field it forgot retries the same call: {}",
        absent.text()
    );

    // Present and empty: serde cannot see this one, so the surface has to.
    let blank = call(
        &surface,
        &ada(),
        "prefer",
        json!({"mode": "local", "scope": "session", "reason": "   "}),
    )
    .await;
    assert!(blank.is_error());
    assert!(blank.text().contains("reason"), "{}", blank.text());
    assert!(
        store.overlay(&adas_session()).is_none(),
        "an unexplained routing change is unauditable, so it is also not stored"
    );

    // The same rule on the other overlay tool.
    let floor = call(
        &surface,
        &ada(),
        "set_quality_floor",
        json!({"floor": 0.9, "turns": 1, "reason": ""}),
    )
    .await;
    assert!(floor.is_error() && floor.text().contains("reason"));

    // The control: the identical call with a reason is served.
    let served_call = call(
        &surface,
        &ada(),
        "prefer",
        json!({"mode": "local", "scope": "session", "reason": "bulk edits"}),
    )
    .await;
    assert!(!served_call.is_error(), "{}", served_call.text());
    assert!(store.overlay(&adas_session()).is_some());
}

#[tokio::test]
async fn a_scope_and_a_turn_count_that_disagree_are_refused_rather_than_resolved() {
    // Either resolution drops something the agent wrote: the scope drops the
    // number, the number drops the word. Both leave it believing a preference
    // it does not have.
    let (surface, store) = FakeDeployment::default().surface();
    let refused = call(
        &surface,
        &ada(),
        "prefer",
        json!({"mode": "local", "scope": "turn", "turns": 5, "reason": "bulk edits"}),
    )
    .await;
    assert!(refused.is_error());
    assert!(refused.text().contains("turns"), "{}", refused.text());
    assert!(store.overlay(&adas_session()).is_none());

    // The controls: the scope alone, and the scope with the one count that
    // agrees with it.
    for arguments in [
        json!({"mode": "local", "scope": "turn", "reason": "one turn"}),
        json!({"mode": "local", "scope": "turn", "turns": 1, "reason": "one turn"}),
    ] {
        let answer = served(&call(&surface, &ada(), "prefer", arguments).await);
        assert_eq!(answer["overlay"]["mode_turns_remaining"], json!(1));
    }
}

#[tokio::test]
async fn asking_for_auto_releases_a_preference_the_agent_set_itself() {
    // Releasing an overlay is not widening: narrowing-only is a rule about the
    // deployment's ceiling, which an overlay never touches. Dropping one
    // returns the session to the ceiling and no further.
    let (surface, store) = FakeDeployment::default().surface();
    call(
        &surface,
        &ada(),
        "prefer",
        json!({"mode": "local", "scope": "session", "reason": "bulk edits"}),
    )
    .await;
    assert!(store.overlay(&adas_session()).is_some());

    let answer = served(
        &call(
            &surface,
            &ada(),
            "prefer",
            json!({"mode": "auto", "scope": "session", "reason": "back to normal"}),
        )
        .await,
    );
    assert_eq!(answer["narrowed"], json!(false));
    assert_eq!(
        answer["policy_digest"],
        json!(TurnPolicy::unrestricted().digest()),
        "the ceiling, and not one step past it"
    );
    assert!(store.overlay(&adas_session()).is_none());
}

/// A [`ControlReads`] whose admissibility read stands in for a turn starting on
/// another thread, in the gap between an overlay tool's read and its write.
struct RacingReads {
    inner: FakeDeployment,
    store: Arc<ControlStore>,
    armed: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl roundhouse_mcp::reads::ControlReads for RacingReads {
    async fn resolve_session(
        &self,
        principal: &Principal,
        conversation: Option<&str>,
    ) -> Result<SessionId, roundhouse_mcp::SurfaceError> {
        self.inner.resolve_session(principal, conversation).await
    }

    async fn ceiling_policy(
        &self,
        principal: &Principal,
    ) -> Result<TurnPolicy, roundhouse_mcp::SurfaceError> {
        self.inner.ceiling_policy(principal).await
    }

    async fn admissible_targets(
        &self,
        principal: &Principal,
        policy: &TurnPolicy,
    ) -> Result<Vec<roundhouse_core::routing::Target>, roundhouse_mcp::SurfaceError> {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            // The engine's turn start, landing inside the surface's await.
            self.store.consume_overlay(&adas_session());
        }
        self.inner.admissible_targets(principal, policy).await
    }

    async fn balance(
        &self,
        principal: &Principal,
    ) -> Result<Option<roundhouse_core::control::Balance>, roundhouse_mcp::SurfaceError> {
        self.inner.balance(principal).await
    }

    async fn session_facts(
        &self,
        session: &SessionId,
    ) -> Result<SessionFacts, roundhouse_mcp::SurfaceError> {
        self.inner.session_facts(session).await
    }

    fn now_ms(&self) -> u64 {
        self.inner.now_ms()
    }
}

#[tokio::test]
async fn a_turn_that_spends_an_axis_during_an_overlay_write_does_not_get_it_back() {
    // The overlay entry has two writers: this surface, and the engine at every
    // turn start. An overlay tool has to decide whether the ask leaves anything
    // routable, and that decision is an `await` — so a writer that read the
    // whole overlay before it and wrote the whole overlay after it publishes a
    // picture of a moment that has passed. Here the moment that passes is the
    // one in which a turn spends this session's last ration of `local`.
    let store = Arc::new(ControlStore::new());
    let reads = Arc::new(RacingReads {
        inner: FakeDeployment::default(),
        store: Arc::clone(&store),
        armed: std::sync::atomic::AtomicBool::new(false),
    });
    let surface = ControlPlaneSurface::new(Arc::clone(&reads), Arc::clone(&store));

    let first = served(
        &call(
            &surface,
            &ada(),
            "prefer",
            json!({"mode": "local", "scope": "turn", "reason": "one cheap turn"}),
        )
        .await,
    );
    assert_eq!(first["overlay"]["mode_turns_remaining"], json!(1));

    // The engine's turn start, scheduled inside the next tool call's await.
    reads.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let answer = served(
        &call(
            &surface,
            &ada(),
            "set_quality_floor",
            json!({"floor": 0.5, "turns": 3, "reason": "this one is subtle"}),
        )
        .await,
    );

    let after = store
        .overlay(&adas_session())
        .expect("the floor axis is installed");
    assert!(
        after.mode.is_none(),
        "the turn that spent the mode axis had it handed back: {:?}",
        after.mode
    );
    assert!(
        answer["overlay"]["mode"].is_null(),
        "and the agent is told it still holds a preference it already spent: {answer}"
    );

    // The control: the axis this call was actually about did land, so the
    // absence above is one axis untouched and not a write that failed.
    assert_eq!(answer["overlay"]["quality_floor"], json!(0.5));
    assert_eq!(answer["overlay"]["floor_turns_remaining"], json!(3));
    assert_eq!(
        after.floor.as_ref().map(|floor| floor.ask),
        Some(0.5),
        "and the engine reads the same floor out of the store"
    );
}

#[tokio::test]
async fn an_unhonorable_ask_writes_nothing_and_leaves_the_session_as_it_was() {
    // The second rung of `install`, pinned at the store rather than only in the
    // answer — it is the whole of the fallback now that the third rung is gone,
    // and what "the session keeps the overlay it had" means once a write is per
    // axis is that no write happens at all.
    let (surface, store) = FakeDeployment::local_only().surface();
    served(
        &call(
            &surface,
            &ada(),
            "prefer",
            json!({"mode": "local", "scope": "session", "turns": 4, "reason": "bulk edits"}),
        )
        .await,
    );
    let before = store.overlay(&adas_session()).expect("a standing overlay");

    let refused = served(
        &call(
            &surface,
            &ada(),
            "set_quality_floor",
            json!({"floor": 0.99, "turns": 2, "reason": "only the best"}),
        )
        .await,
    );
    assert_eq!(refused["narrowed"], json!(true));
    assert_eq!(
        store.overlay(&adas_session()),
        Some(before),
        "an ask that would empty the admissible set leaves the store untouched, \
         axis and turn count included"
    );
    assert_eq!(refused["overlay"]["mode"], json!("local"));
    assert!(
        refused["overlay"]["quality_floor"].is_null(),
        "and the agent is not told it holds a floor that was never stored: {refused}"
    );
}

/// A [`ControlReads`] with a movable log: a cursor the surface can check and a
/// projection that changes when it moves.
struct MemoProbeReads {
    inner: FakeDeployment,
    logs: std::sync::Mutex<std::collections::HashMap<SessionId, (u64, SessionFacts)>>,
    projections: std::sync::atomic::AtomicUsize,
}

impl MemoProbeReads {
    fn advance(&self, session: &SessionId, facts: SessionFacts) {
        let mut logs = self.logs.lock().unwrap();
        let entry = logs.entry(session.clone()).or_insert((0, facts.clone()));
        entry.0 += 1;
        entry.1 = facts;
    }

    fn projections(&self) -> usize {
        self.projections.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl roundhouse_mcp::reads::ControlReads for MemoProbeReads {
    async fn resolve_session(
        &self,
        principal: &Principal,
        conversation: Option<&str>,
    ) -> Result<SessionId, roundhouse_mcp::SurfaceError> {
        self.inner.resolve_session(principal, conversation).await
    }

    async fn ceiling_policy(
        &self,
        principal: &Principal,
    ) -> Result<TurnPolicy, roundhouse_mcp::SurfaceError> {
        self.inner.ceiling_policy(principal).await
    }

    async fn admissible_targets(
        &self,
        principal: &Principal,
        policy: &TurnPolicy,
    ) -> Result<Vec<roundhouse_core::routing::Target>, roundhouse_mcp::SurfaceError> {
        self.inner.admissible_targets(principal, policy).await
    }

    async fn balance(
        &self,
        principal: &Principal,
    ) -> Result<Option<roundhouse_core::control::Balance>, roundhouse_mcp::SurfaceError> {
        self.inner.balance(principal).await
    }

    async fn session_facts(
        &self,
        session: &SessionId,
    ) -> Result<SessionFacts, roundhouse_mcp::SurfaceError> {
        self.projections
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self
            .logs
            .lock()
            .unwrap()
            .get(session)
            .map(|(_, facts)| facts.clone())
            .unwrap_or_default())
    }

    async fn session_cursor(
        &self,
        session: &SessionId,
    ) -> Result<Option<u64>, roundhouse_mcp::SurfaceError> {
        Ok(Some(
            self.logs
                .lock()
                .unwrap()
                .get(session)
                .map(|(cursor, _)| *cursor)
                .unwrap_or(0),
        ))
    }

    fn now_ms(&self) -> u64 {
        self.inner.now_ms()
    }
}

async fn facts_with(guidance: &str) -> SessionFacts {
    SessionFacts {
        latest_guidance: Some(guidance.to_string()),
        last_decision: Some(decision().await),
    }
}

#[tokio::test]
async fn a_repeat_read_between_turns_reads_the_cursor_rather_than_the_whole_log() {
    // `explain_last_route` and `fetch_steer` are called from a model's context,
    // and on a real deployment each answer is a replay of the whole session log
    // — a store round trip per batch plus a clone of every item and every
    // routing decision. Nothing rate-limits either tool, so the cost is bounded
    // here or it is not bounded at all.
    //
    // **`status` used to be the third, and M10.0 took it out of the projection
    // entirely.** Its one log-derived field was `open_steers`, which listed
    // synthetic calls awaiting an answer; there are none, so the tool now
    // resolves the conversation and quotes the catalog and never replays. That
    // makes it the *control* below rather than a subject — a call that pays
    // nothing here must not move the counter the memo is measured on.
    let mut sessions = std::collections::HashMap::new();
    sessions.insert(ada(), adas_session());
    let bobs_session = SessionId::new("other/bob/sess_1");
    sessions.insert(bob(), bobs_session.clone());

    let reads = Arc::new(MemoProbeReads {
        inner: FakeDeployment {
            sessions,
            ..FakeDeployment::default()
        },
        logs: std::sync::Mutex::new(std::collections::HashMap::new()),
        projections: std::sync::atomic::AtomicUsize::new(0),
    });
    reads.advance(&adas_session(), facts_with("re-read the parser task").await);
    reads.advance(
        &bobs_session,
        facts_with("bob was told something else").await,
    );
    let surface = ControlPlaneSurface::new(Arc::clone(&reads), Arc::new(ControlStore::new()));

    let first = served(&call(&surface, &ada(), "fetch_steer", json!({})).await);
    assert_eq!(first["guidance"], json!("re-read the parser task"));
    assert_eq!(reads.projections(), 1);

    let repeat = served(&call(&surface, &ada(), "fetch_steer", json!({})).await);
    assert_eq!(
        reads.projections(),
        1,
        "a second read with no turn in between replayed the log again"
    );
    assert_eq!(repeat["guidance"], json!("re-read the parser task"));

    // The other tool that pays the same cost shares the same memo.
    let explained = served(&call(&surface, &ada(), "explain_last_route", json!({})).await);
    assert_eq!(explained["chosen"], json!("anthropic/claude-opus-4"));
    assert_eq!(reads.projections(), 1);

    // The control on the other side: `status` no longer reads the projection at
    // all, so it must not move the counter either.
    assert!(!call(&surface, &ada(), "status", json!({})).await.is_error());
    assert_eq!(
        reads.projections(),
        1,
        "`status` replayed a log it has no field left to read from"
    );

    // A different conversation is a different memo, not a hit: a cache keyed on
    // the cursor alone would answer bob with ada's correction.
    let bobs = served(&call(&surface, &bob(), "fetch_steer", json!({})).await);
    assert_eq!(bobs["guidance"], json!("bob was told something else"));
    assert_eq!(reads.projections(), 2);
    assert_eq!(
        served(&call(&surface, &ada(), "fetch_steer", json!({})).await)["guidance"],
        json!("re-read the parser task"),
        "and ada's memo survived bob's call rather than being overwritten by it"
    );
    assert_eq!(reads.projections(), 2);

    // The control that keeps every assertion above from being a study of a
    // frozen cache: a turn moves the log, and the next call sees it.
    reads.advance(&adas_session(), facts_with("and now something newer").await);
    let after_turn = served(&call(&surface, &ada(), "fetch_steer", json!({})).await);
    assert_eq!(
        after_turn["guidance"],
        json!("and now something newer"),
        "a memo that outlived the turn it was taken before would serve a \
         correction the deployment has already replaced"
    );
    assert_eq!(reads.projections(), 3);
}

// ---------------------------------------------------------------------------
// Steers
// ---------------------------------------------------------------------------

/// A conversation roundhouse has corrected, as the log's own fold reports it.
fn steered(guidance: &str) -> SessionFacts {
    SessionFacts {
        latest_guidance: Some(guidance.to_string()),
        last_decision: None,
    }
}

/// The guidance the fixtures below re-read, distinctive enough that a rendering
/// which leaked it somewhere else is visible as a literal.
const GUIDANCE: &str = "the task named src/parser.rs; you are editing src/main.rs";

#[tokio::test]
async fn fetch_steer_is_byte_identical_on_a_second_call() {
    // The reason the tool is a pure read: a handler that ran the judge on
    // invocation would let a model -- or a prompt injection reading the tool's
    // own description -- drain the validate budget by calling it in a loop.
    //
    // **What changed with M10.0 is where the bytes come from, not that they are
    // fixed.** They used to be a record deposited when the synthetic call was
    // emitted; they are now a fold of the conversation's own log, which is
    // strictly more stable -- a node restart used to lose the deposit and leave
    // `fetch_steer` refusing an id the log still named.
    let (surface, _store) = FakeDeployment::default()
        .with_facts(&adas_session(), steered(GUIDANCE))
        .surface();

    let first = call(&surface, &ada(), "fetch_steer", json!({})).await;
    let second = call(&surface, &ada(), "fetch_steer", json!({})).await;
    assert_eq!(
        first.text(),
        second.text(),
        "a retry has to see the same correction, byte for byte"
    );
    assert_eq!(first, second);

    let payload = served(&first);
    assert_eq!(payload["conversation"], json!("acme/ada/sess_1"));
    assert!(
        payload["guidance"]
            .as_str()
            .unwrap()
            .contains("src/parser.rs"),
        "the corrective text is what the tool exists to deliver"
    );

    // Naming the conversation explicitly is the same answer as letting it
    // default, which is what makes the defaulted form safe for an agent that
    // holds one conversation and never learned its id.
    assert_eq!(
        served(
            &call(
                &surface,
                &ada(),
                "fetch_steer",
                json!({"conversation": "sess_1"})
            )
            .await
        ),
        payload
    );
}

#[tokio::test]
async fn fetch_steer_quotes_nothing_but_the_fold_and_pays_for_no_side_read() {
    // The module doc's "no clock, no fleet, no judge" claim, made checkable. It
    // is a *narrower* claim than it was: before M10.0 the payload lived in a
    // node-local store, so the honest assertion was `total_calls() == 0`. The
    // correction is a conversation item now, so the tool must read the log --
    // and what it must still never do is quote the fleet, ask the ledger, or
    // touch the policy, any of which would put a price or a candidate list one
    // tool call away from a model.
    let reads = Arc::new(CountingReads::new(
        FakeDeployment::default().with_facts(&adas_session(), steered(GUIDANCE)),
    ));
    let surface = ControlPlaneSurface::new(Arc::clone(&reads), Arc::new(ControlStore::new()));

    let outcome = call(&surface, &ada(), "fetch_steer", json!({})).await;
    assert!(!outcome.is_error(), "{}", outcome.text());
    assert_eq!(
        reads.admissible_targets_calls.load(SeqCst),
        0,
        "fetch_steer must never quote the fleet"
    );
    assert_eq!(reads.ceiling_policy_calls.load(SeqCst), 0);
    assert_eq!(
        reads.balance_calls.load(SeqCst),
        0,
        "and never read the ledger: a correction is not a place to learn what is left to spend"
    );
    assert_eq!(
        reads.session_facts_calls.load(SeqCst),
        1,
        "exactly one projection, which is the read the correction lives in"
    );

    // The control: a tool that legitimately quotes the fleet moves the counters
    // the zeroes above are asserted on, so those zeroes are not vacuous.
    let _ = call(&surface, &ada(), "status", json!({})).await;
    assert!(reads.admissible_targets_calls.load(SeqCst) > 0);
    assert!(reads.balance_calls.load(SeqCst) > 0);
}

#[tokio::test]
async fn fetch_steer_on_an_uncorrected_conversation_is_an_error_not_an_empty_payload() {
    // No fail-open. An empty payload reads to an agent as "there was nothing to
    // correct", which is the one thing a steer must never be mistaken for.
    let (surface, _store) = FakeDeployment::default().surface();
    let refused = call(&surface, &ada(), "fetch_steer", json!({})).await;
    assert!(refused.is_error());
    assert!(
        serde_json::from_str::<Value>(refused.text()).is_err()
            || serde_json::from_str::<Value>(refused.text()).unwrap()["guidance"].is_null(),
        "a refusal must not be parseable as a payload with empty guidance"
    );

    // The control: the same call against a conversation that *was* corrected is
    // served, so the refusal is about the fold and not about the tool.
    let (steered_surface, _) = FakeDeployment::default()
        .with_facts(&adas_session(), steered(GUIDANCE))
        .surface();
    assert!(
        !call(&steered_surface, &ada(), "fetch_steer", json!({}))
            .await
            .is_error()
    );
}

#[tokio::test]
async fn fetch_steer_for_another_tenants_conversation_is_refused_without_naming_it() {
    // **T4 moved this boundary rather than removing it.** The tool used to take
    // a `steer_id`, compare principals itself, and refuse an unknown id and
    // another tenant's id in identical words -- or a caller could enumerate ids
    // and learn which ones exist in somebody else's session. There is no id any
    // more: both steer tools name a *conversation* and resolve it through
    // `resolve_session`, so the refusal is `ForeignConversation`, the same door
    // every other session-scoped tool already sits behind. What has to hold is
    // unchanged: the refusal must reveal nothing about the tenant that does own
    // the conversation.
    let (surface, _store) = FakeDeployment::default()
        .with_facts(&SessionId::new("other/bob/sess_1"), steered(GUIDANCE))
        .surface();

    let refused = call(
        &surface,
        &ada(),
        "fetch_steer",
        json!({"conversation": "other/bob/sess_1"}),
    )
    .await;
    assert!(refused.is_error());
    for leak in ["parser.rs", "main.rs"] {
        assert!(
            !refused.text().contains(leak),
            "the refusal named `{leak}`: {}",
            refused.text()
        );
    }

    // And it reads identically to a conversation nobody has ever started, which
    // is what stops the difference being measurable.
    let unknown = call(
        &surface,
        &ada(),
        "fetch_steer",
        json!({"conversation": "other/bob/never_started"}),
    )
    .await;
    assert_eq!(
        refused.text().replace("sess_1", "NAME"),
        unknown.text().replace("never_started", "NAME"),
        "a foreign conversation must read exactly like one nobody started"
    );

    // The control: within her own namespace ada is served, so the refusal is
    // about the namespace and not about the argument being spelled out.
    let (hers, _) = FakeDeployment::default()
        .with_facts(&adas_session(), steered(GUIDANCE))
        .surface();
    assert!(
        !call(
            &hers,
            &ada(),
            "fetch_steer",
            json!({"conversation": "sess_1"})
        )
        .await
        .is_error()
    );
}

#[tokio::test]
async fn report_outcome_is_filed_for_any_conversation_and_blocks_nothing() {
    // Advisory in the strongest sense, and M10.0 made it more so. The old tool
    // refused a report against an unknown `steer_id`; there is no id now, and a
    // refusal keyed on "has this conversation been steered" would make the
    // tool's answer depend on a fact the agent cannot see. So it files, and the
    // session carries on exactly as it would have.
    let (surface, store) = FakeDeployment::default().surface();

    let filed = served(
        &call(
            &surface,
            &ada(),
            "report_outcome",
            json!({"outcome": "not_applicable", "note": "already fixed"}),
        )
        .await,
    );
    assert_eq!(filed["conversation"], json!("acme/ada/sess_1"));
    assert_eq!(filed["outcome"], json!("not_applicable"));
    assert_eq!(filed["recorded"], json!(true));
    assert_eq!(
        store.outcome_for(&adas_session()).unwrap().note.as_deref(),
        Some("already fixed"),
        "the report reaches the store, not just the answer"
    );

    // Nothing about the session moved: no overlay was written and status still
    // answers, which is the "blocks nothing" half.
    assert!(store.overlay(&adas_session()).is_none());
    assert!(!call(&surface, &ada(), "status", json!({})).await.is_error());
}

#[tokio::test]
async fn report_outcome_cannot_write_against_another_tenants_conversation() {
    // The write-side mirror of the read guard above, and the more consequential
    // half: attaching an outcome to somebody else's conversation is a
    // cross-tenant *write*, so it has to be refused at the same door and leave
    // the other tenant's record untouched.
    let bobs = SessionId::new("other/bob/sess_1");
    let (surface, store) = FakeDeployment::default().surface();

    let refused = call(
        &surface,
        &ada(),
        "report_outcome",
        json!({"conversation": "other/bob/sess_1", "outcome": "applied"}),
    )
    .await;
    assert!(
        refused.is_error(),
        "ada must not be able to file against bob's conversation"
    );
    assert!(
        store.outcome_for(&bobs).is_none(),
        "a refused report must not have written anything"
    );
    // Textually identical to a conversation nobody started -- the same
    // no-oracle property the read side holds.
    let unknown = call(
        &surface,
        &ada(),
        "report_outcome",
        json!({"conversation": "other/bob/never_started", "outcome": "applied"}),
    )
    .await;
    assert_eq!(
        refused.text().replace("sess_1", "NAME"),
        unknown.text().replace("never_started", "NAME")
    );

    // The control: each tenant may file against their own, and the two records
    // are separate.
    let mut sessions = std::collections::HashMap::new();
    sessions.insert(ada(), adas_session());
    sessions.insert(bob(), bobs.clone());
    let (shared, shared_store) = FakeDeployment {
        sessions,
        ..FakeDeployment::default()
    }
    .surface();
    for (who, outcome) in [(ada(), "rejected"), (bob(), "applied")] {
        assert!(
            !call(&shared, &who, "report_outcome", json!({"outcome": outcome}),)
                .await
                .is_error()
        );
    }
    assert_eq!(
        shared_store.outcome_for(&adas_session()).unwrap().outcome,
        roundhouse_mcp::surface::SteerOutcome::Rejected
    );
    assert_eq!(
        shared_store.outcome_for(&bobs).unwrap().outcome,
        roundhouse_mcp::surface::SteerOutcome::Applied
    );
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_reports_names_not_prices() {
    // The family-bias guard, as a negative assertion over the rendered string.
    // An agent that can see what a model costs can argue about what it costs,
    // and the argument is with a component that cannot check whether the agent
    // is quoting its own context back at it.
    let facts = SessionFacts {
        latest_guidance: None,
        last_decision: Some(decision().await),
    };
    let (surface, _store) = FakeDeployment::default()
        .with_facts(&adas_session(), facts)
        .surface();

    let outcome = call(&surface, &ada(), "status", json!({})).await;
    let text = outcome.text();
    let answer = served(&outcome);

    // The names are there.
    assert_eq!(
        answer["admissible_targets"],
        json!([
            "local/llama-3.1-8b",
            "anthropic/claude-opus-4",
            "openai/gpt-5"
        ])
    );
    assert!(
        answer.get("open_steers").is_none(),
        "M10.0 retired the field: there are no synthetic calls to await, and a \
         permanently empty list in a model's context is a question it keeps \
         asking and always gets `[]` to"
    );
    assert_eq!(
        answer["policy_digest"],
        json!(TurnPolicy::unrestricted().digest())
    );

    // The prices are not, in any spelling.
    for price in [CLAUDE_PRICE_USD, GPT_PRICE_USD] {
        for spelling in [
            format!("{price}"),
            format!("{price:.2}"),
            format!("{price:.6}"),
        ] {
            assert!(
                !text.contains(&spelling),
                "status leaked a per-model price as `{spelling}`:\n{text}"
            );
        }
    }

    // Budget is the deliberate exception, and it is stamped with the basis it
    // was read on rather than left for a reader to assume.
    assert_eq!(answer["budget"]["basis"], json!("committed"));
    assert_eq!(answer["budget"]["project_remaining_usd"], json!(88.0));
    assert_eq!(answer["budget"]["member_remaining_usd"], json!(17.0));
    assert_eq!(answer["budget"]["state"], json!("unconstrained"));
    assert!(
        answer["budget"].get("tokens_since_validation").is_none(),
        "a field with no producer is a lie the first reader believes"
    );
}

#[tokio::test]
async fn a_deployment_that_meters_nothing_reports_no_budget_rather_than_a_zero() {
    // Open mode, and every configured project with no `"budget"`: the engine
    // never calls the ledger for these, so there is no position to report. The
    // failure this pins is a plausible one — a `0.0 remaining` would read to an
    // agent as "you are out of money" on a deployment that meters nothing, and
    // it would read that way for the whole of a session.
    let (surface, _store) = FakeDeployment {
        balance: None,
        ..FakeDeployment::default()
    }
    .surface();

    let answer = served(&call(&surface, &ada(), "status", json!({})).await);
    assert!(
        answer.get("budget").is_none_or(serde_json::Value::is_null),
        "an unmetered deployment must not put a dollar figure nobody wrote into \
         a model's context: {answer}"
    );
    assert!(
        !answer["admissible_targets"].as_array().unwrap().is_empty(),
        "and the rest of the answer is unaffected: this is a deployment with no \
         ceiling, not a deployment with nothing to route to"
    );
}

#[tokio::test]
async fn explain_last_route_reports_the_decision_without_its_prices() {
    // **The rationale under test is one a real `AffinityPolicy::choose` wrote.**
    // It used to be the literal `"cheapest warm option"`, and that made the
    // no-prices assertion below tautological twice over: a hand-written string
    // contains a price only if the author put one there, and no deployment ever
    // writes that string. What a deployment does write — the affinity policy's
    // own account of the turn, copied verbatim by `engine.rs` into
    // `DecisionRecord::rationale` and verbatim again by `plane.rs` into this
    // tool's answer — used to carry the winning candidate's `expected_cost_usd`
    // formatted in. See [`common::decision`].
    //
    // The fixture is arranged so the router picks a *priced* target: a fleet
    // where the free local worker always wins produces `$0.00000`, and an
    // assertion about `7.77` would then hold for a producer that prints prices.
    let facts = SessionFacts {
        latest_guidance: None,
        last_decision: Some(decision().await),
    };
    let (surface, _store) = FakeDeployment::default()
        .with_facts(&adas_session(), facts)
        .surface();

    let outcome = call(&surface, &ada(), "explain_last_route", json!({})).await;
    let answer = served(&outcome);
    assert_eq!(answer["chosen"], json!("anthropic/claude-opus-4"));
    assert_eq!(
        answer["rationale"],
        json!("score 0.5000 over 3 candidate(s); expected prefill 200 of 1200 tokens (83% cached)"),
        "the policy's own account of the turn, republished verbatim -- and \
         pinned as a literal so that a term added back to the producer's format \
         string fails here and not only in the negative assertion below"
    );
    assert_eq!(answer["routing_policy"], json!("affinity"));
    assert_eq!(answer["budget_state"], json!("warned"));
    assert_eq!(
        answer["turn_policy_digest"],
        json!(TurnPolicy::unrestricted().digest())
    );
    assert_eq!(
        answer["considered"],
        json!([
            "local/llama-3.1-8b",
            "anthropic/claude-opus-4",
            "openai/gpt-5"
        ]),
        "the counterfactual is the useful half, and it is a list of names"
    );
    for price in [CLAUDE_PRICE_USD, GPT_PRICE_USD] {
        assert!(
            !outcome.text().contains(&format!("{price}")),
            "the audit trail as a tool still carries no prices:\n{}",
            outcome.text()
        );
    }

    // The control: a conversation nobody has routed yet is an error naming that
    // fact, not an explanation of nothing.
    let (fresh, _) = FakeDeployment::default().surface();
    let unrouted = call(&fresh, &ada(), "explain_last_route", json!({})).await;
    assert!(unrouted.is_error());
    assert!(unrouted.text().contains("not been routed"));
}

#[tokio::test]
async fn a_conversation_outside_the_callers_namespace_is_refused_not_replaced() {
    // Falling back to the caller's own most recent session would make a probe
    // for somebody else's conversation read as an ordinary answer.
    let (surface, _store) = FakeDeployment::default().surface();
    let refused = call(
        &surface,
        &ada(),
        "status",
        json!({"conversation": "somebody-elses-key"}),
    )
    .await;
    assert!(refused.is_error());
    assert!(refused.text().contains("does not belong"));
}

// ---------------------------------------------------------------------------
// The correlation trick
// ---------------------------------------------------------------------------

#[tokio::test]
async fn init_session_returns_the_binding_id_in_its_output_text() {
    // The whole mechanism, minus the client. An MCP connection cannot carry a
    // conversation id -- Codex sources those headers from static config -- so
    // the id goes out in the tool *output*, rides the client's resent history
    // into the log, and is found there. This proves the first half: the id is
    // in the text, and the text tells the client why to keep it.
    let (surface, store) = FakeDeployment::default().surface();
    let outcome = call(&surface, &ada(), "init_session", json!({})).await;
    let answer = served(&outcome);

    let id = answer["session_binding_id"].as_str().expect("an id");
    assert!(id.starts_with("rhb_"), "opaque and recognizable: {id}");
    assert!(
        outcome.text().contains(id),
        "the id has to be in the text the client appends, not only in a field \
         some other framing would carry"
    );
    assert!(
        answer["note"]
            .as_str()
            .unwrap()
            .contains("Keep this tool output"),
        "a client that is not told to keep the output summarizes it away and the join never happens"
    );
    assert_eq!(answer["conversation"], json!(adas_session().to_string()));

    // The second half: the id resolves back to the session that minted it, and
    // the projection finds it in a conversation the client resent.
    let binding = store
        .binding(&ada(), &adas_session(), &roundhouse_mcp::BindingId::new(id))
        .expect("this node minted it, for this caller");
    assert_eq!(binding.session, adas_session());
    assert_eq!(binding.principal, ada());

    let resent = vec![roundhouse_core::item::Item {
        role: roundhouse_core::item::Role::User,
        content: roundhouse_core::item::ItemContent::ToolResult {
            call_id: "call_1".into(),
            output: outcome.text().to_string(),
        },
        response_id: None,
    }];
    assert_eq!(
        roundhouse_mcp::binding_in_items(&resent)
            .as_ref()
            .map(|id| id.as_str()),
        Some(id),
        "the session whose log holds the id is the session that made the call"
    );

    // The control: a conversation that never carried one joins to nothing.
    assert!(
        roundhouse_mcp::binding_in_items(&[roundhouse_core::item::Item::user_text(
            "an ordinary turn"
        )])
        .is_none()
    );
}

#[tokio::test]
async fn init_session_called_in_a_loop_answers_with_one_id_and_writes_once() {
    // One of the eight tools is a *write*, and nothing rate-limits a model that
    // calls a tool in a loop. Answering with the binding already recorded for
    // this conversation is what makes the loop free: the store cannot grow, and
    // a client that called the tool twice appends the same token twice rather
    // than two tokens a later reader has to adjudicate between.
    let (surface, store) = FakeDeployment::default().surface();

    let first = call(&surface, &ada(), "init_session", json!({})).await;
    for _ in 0..4 {
        let again = call(&surface, &ada(), "init_session", json!({})).await;
        assert_eq!(
            first.text(),
            again.text(),
            "a second init_session minted a second id for one conversation"
        );
    }

    let id = served(&first)["session_binding_id"]
        .as_str()
        .expect("an id")
        .to_string();
    let both = vec![
        roundhouse_core::item::Item::user_text(id.clone()),
        roundhouse_core::item::Item::user_text(id.clone()),
    ];
    assert_eq!(
        roundhouse_mcp::binding_ids_in_items(&both)
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1,
        "and a conversation that carries the answer twice carries one id"
    );
    assert!(
        store
            .binding(&ada(), &adas_session(), &roundhouse_mcp::BindingId::new(id))
            .is_some()
    );
}

#[tokio::test]
async fn a_principal_with_no_conversation_is_told_so_rather_than_shown_an_empty_one() {
    let (surface, _store) = FakeDeployment {
        sessions: Default::default(),
        ..FakeDeployment::default()
    }
    .surface();
    for tool in ["status", "init_session", "explain_last_route"] {
        let refused = call(&surface, &ada(), tool, json!({})).await;
        assert!(refused.is_error(), "`{tool}` invented a conversation");
        assert!(refused.text().contains("no conversation"), "`{tool}`");
    }
}

#[tokio::test]
async fn an_intent_is_stored_against_the_conversation_and_changes_no_routing() {
    let (surface, store) = FakeDeployment::default().surface();
    let before = served(&call(&surface, &ada(), "status", json!({})).await);

    let answer = served(
        &call(
            &surface,
            &ada(),
            "declare_intent",
            json!({
                "goal": "make the parser accept trailing commas",
                "plan_steps": ["read the grammar", "add a test", "fix the lexer"],
                "done_when": "cargo test -p parser is green"
            }),
        )
        .await,
    );
    assert_eq!(
        answer["goal"],
        json!("make the parser accept trailing commas")
    );
    assert_eq!(answer["plan_steps"].as_array().unwrap().len(), 3);
    assert!(
        answer["routing_effect"]
            .as_str()
            .unwrap()
            .starts_with("none")
    );

    let after = served(&call(&surface, &ada(), "status", json!({})).await);
    assert_eq!(
        before["policy_digest"], after["policy_digest"],
        "an intent that moved the routing fingerprint would be a routing tool wearing a label"
    );
    assert_eq!(before["admissible_targets"], after["admissible_targets"]);
    assert!(store.overlay(&adas_session()).is_none());

    // And a goal that says nothing is refused naming the field.
    let blank = call(
        &surface,
        &ada(),
        "declare_intent",
        json!({"goal": "  ", "done_when": "it works"}),
    )
    .await;
    assert!(blank.is_error() && blank.text().contains("goal"));
}

#[tokio::test]
async fn an_unknown_tool_is_refused_naming_it_and_nothing_else() {
    let (surface, _store) = FakeDeployment::default().surface();
    let refused = call(&surface, &ada(), "drop_the_budget", json!({})).await;
    assert!(refused.is_error());
    assert!(refused.text().contains("drop_the_budget"));
}

/// A second surface over one store, to pin what the store is for.
#[tokio::test]
async fn two_surfaces_over_one_store_see_one_control_plane() {
    // The store is shared with the engine, which is what makes an overlay reach
    // the next turn at all. A surface holding its own copy would agree with the
    // engine only by luck.
    let store = Arc::new(ControlStore::new());
    let reader = roundhouse_mcp::ControlPlaneSurface::new(
        Arc::new(FakeDeployment::default()),
        Arc::clone(&store),
    );
    let writer = roundhouse_mcp::ControlPlaneSurface::new(
        Arc::new(FakeDeployment::default()),
        Arc::clone(&store),
    );

    call(
        &writer,
        &ada(),
        "prefer",
        json!({"mode": "local", "scope": "session", "reason": "bulk edits"}),
    )
    .await;
    let seen = served(&call(&reader, &ada(), "status", json!({})).await);
    assert_eq!(seen["overlay"]["mode"], json!("local"));
}

/// F06: a client that was never handed the generated launch config -- an
/// operator's own `codex` install, a bare MCP inspector, any client that
/// speaks the protocol without `default_tools_approval_mode = "approve"` in
/// its `[mcp_servers.*]` stanza -- sees exactly what `RoundhouseMcp::tools()`
/// serializes over the wire. codex 0.146.0's `requires_mcp_tool_approval`
/// (`core/src/mcp_tool_call.rs:2156-2173` @ `e363b08`, identical at
/// `2182-2199` @ the Cargo pin `6344a65`) treats an absent `read_only_hint`
/// as `false` and an absent `destructive_hint`/`open_world_hint` as `true`,
/// so a tool with no `annotations` at all reads as destructive-and-open-world
/// regardless of what it actually does -- and under `approval_policy = never`
/// an approval nobody can be asked for resolves to *cancelled*.
///
/// This asserts the exact triple per tool rather than merely that annotations
/// exist, because `ToolAnnotations::default()` is `Some(..)` and serializes to
/// `{}`: it would satisfy an `is_some()` check while leaving codex's
/// arithmetic reading exactly what it reads today. The read set is spelled out
/// here as a literal so a ninth tool arrives unclassified and fails, which is
/// the review F16's tripwire demands.
#[test]
fn every_tool_states_what_it_does_to_a_client_that_was_handed_no_config() {
    use roundhouse_mcp::transport::RoundhouseMcp;

    // The crate's own read/write split (see the module doc's "No tool appends
    // to a session log"): these three answer out of committed state, the other
    // five write to the node-local `ControlStore`.
    let reads = ["status", "fetch_steer", "explain_last_route"];

    for tool in RoundhouseMcp::tools() {
        let annotations = tool.annotations.as_ref().unwrap_or_else(|| {
            panic!(
                "`{}` ships with no ToolAnnotations at all; a client that was \
                 not handed the generated launch config (which papers over \
                 this with `default_tools_approval_mode = \"approve\"`) reads \
                 it as destructive and open-world",
                tool.name
            )
        });
        let expected_read_only = reads.contains(&tool.name.as_ref());
        assert_eq!(
            annotations.read_only_hint,
            Some(expected_read_only),
            "`{}` must say whether it writes: codex defaults an absent \
             read_only_hint to false, and a read that stays silent buys an \
             approval it never needed",
            tool.name
        );
        // Both false for all eight: an overlay only ever narrows (nothing here
        // destroys committed state) and the whole surface reaches roundhouse's
        // own control plane and nothing outside it.
        assert_eq!(
            annotations.destructive_hint,
            Some(false),
            "`{}` must deny destructiveness explicitly; absent, codex reads it \
             as true and demands an approval `codex exec` can never give",
            tool.name
        );
        assert_eq!(
            annotations.open_world_hint,
            Some(false),
            "`{}` must deny an open world explicitly; absent, codex reads it \
             as true and demands an approval `codex exec` can never give",
            tool.name
        );
    }
}
