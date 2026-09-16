// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! [`wire`](super)'s unit tests, in their own file for the reason
//! `prefix_admission`'s already are (M14.0 second fix pass, P2): the M17
//! rung (`namespace` threading through canonicalization, the render
//! decision, and the outbound projection) grew this module's own inline
//! suite past the production code it exercises, and a module that keeps
//! growing to hold its tests is not where the next reader looks first for
//! the wire vocabulary itself (M17 review, F2).

use super::*;

fn user(text: &str) -> Item {
    Item::user_text(text)
}

fn assistant(text: &str) -> Item {
    Item::assistant_text(text, ResponseId::new("resp_1"))
}

/// **A measured cache write reaches the wire; nothing else invents one.**
///
/// This field was the literal `0` for the whole of M10, with a doc saying no
/// provider reported one — and it went out as zero on every turn including
/// the ones that would have reported it, had a client existed. M11.0 added
/// the client, so the literal is now a read. Two halves, and neither is the
/// claim alone: an Anthropic turn's write count must arrive, and a Responses
/// turn's must stay zero rather than being back-filled from the uncached
/// count that roundhouse *prices* at the write rate.
#[test]
fn a_measured_cache_write_reaches_the_wire_and_an_unmeasured_one_stays_zero() {
    // PROBE: the shape an `anthropic_messages` turn folds to — the three
    // input counters already summed into `input_tokens` by the client, with
    // the write kept as its own component.
    let anthropic = Usage {
        input_tokens: 9_512,
        cached_input_tokens: 9_000,
        cache_write_tokens: 500,
        output_tokens: 64,
        reasoning_tokens: 0,
        accounting: Default::default(),
    };
    let usage = completed_usage(&anthropic);
    assert_eq!(
        usage["input_tokens_details"]["cache_write_tokens"],
        json!(500)
    );
    assert_eq!(usage["input_tokens_details"]["cached_tokens"], json!(9_000));
    assert_eq!(
        usage["input_tokens"],
        json!(9_512),
        "the two details are components of the input total, not addends — a client that \
         checks the parts against the whole still balances"
    );
    assert_eq!(usage["total_tokens"], json!(9_512 + 64));

    // CONTROL: the same prompt over the Responses wire, where 512 tokens
    // were uncached and nothing reported a write. Zero is the honest answer
    // and `uncached_input_tokens()` — 512 — is the number a well-meaning
    // back-derivation would put here, which is why the assertion names it.
    let responses = Usage {
        cache_write_tokens: 0,
        ..anthropic.clone()
    };
    assert_eq!(responses.uncached_input_tokens(), 512);
    assert_eq!(
        completed_usage(&responses)["input_tokens_details"]["cache_write_tokens"],
        json!(0),
        "a pricing convention must never be published in a field named for a measurement"
    );
}

#[test]
fn the_turn_id_is_the_conversation_and_nothing_else() {
    let conversation = vec![user("hello"), assistant("hi")];
    assert_eq!(turn_id_for(&conversation), turn_id_for(&conversation));
    assert_ne!(turn_id_for(&conversation), turn_id_for(&[user("hello")]));
    // Two conversations that concatenate to the same text must not collide;
    // the role prefix is what keeps them apart.
    assert_ne!(
        turn_id_for(&[user("ab")]),
        turn_id_for(&[user("a"), user("b")])
    );
}

#[test]
fn reasoning_is_dropped_and_unknown_items_are_refused() {
    let items = canonicalize(
        "be brief",
        &[
            json!({ "type": "reasoning", "summary": [] }),
            json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }),
        ],
    )
    .expect("reasoning is skipped rather than refused");
    assert_eq!(items, vec![Item::system_text("be brief"), user("hi")]);

    assert!(canonicalize("", &[json!({ "type": "web_search_call" })]).is_err());
}

/// A client's MCP tool call and the item the log keeps are the same call —
/// **name and namespace both, since M17 (R-N6), and still never the item
/// `id`**.
///
/// The two wire fields look alike and are not. `id` names the item within
/// one response and means nothing on a resend, so reading it would make the
/// claimed prefix disagree with the stored one on the very next turn and
/// rebind the session to a cold generation — silently, since every turn
/// would still answer. `namespace` names the MCP server the call was
/// dispatched to: it is stable across resends by construction (the client
/// sends the same one every time it re-sends the call), it is half of what
/// codex resolves a call against, and dropping it was what made a third
/// party's tool named `status` indistinguishable from ours in the log.
///
/// Carrying it moves no turn id — `Item::render` leaves the field out, and
/// `the_turn_id_of_a_control_call_conversation_is_pinned_bare_and_namespaced`
/// is the literal that says so.
///
/// The wire item is written out here rather than built by a projection,
/// which is what the pre-M10.0 version did. That is a real loss and it is
/// named: the old test could not drift from what we emitted, because it
/// asked the emitter. This one is a fixture of what *codex* emits, so it is
/// pinned against a client instead — `codex_wire_shapes.rs` builds the same
/// shape from codex's own types, and that is where the fixture is kept
/// honest.
#[test]
fn a_clients_namespaced_call_canonicalizes_with_its_namespace_and_without_its_item_id() {
    let wire = json!({
        "type": "function_call",
        "id": "fc_resp_03L",
        "namespace": "mcp__roundhouse",
        "name": "status",
        "call_id": "call_03L",
        "arguments": r#"{"conversation":"main"}"#,
    });

    let stored = canonicalize("", &[wire]).expect("a client's own call is resendable");
    assert_eq!(
        stored,
        vec![Item::namespaced_tool_call(
            "call_03L",
            "status",
            Some("mcp__roundhouse".into()),
            r#"{"conversation":"main"}"#,
        )],
        "the wire's namespace is kept beside the bare name and its item id \
         leaves no trace: {stored:#?}"
    );

    // A plain function tool sends no `namespace` at all, and that absence
    // is stored as an absence rather than refused — the shape every
    // non-MCP tool call on this wire has.
    let plain = canonicalize(
        "",
        &[json!({
            "type": "function_call",
            "name": "search",
            "call_id": "call_04",
            "arguments": "{}",
        })],
    )
    .expect("a plain function tool is resendable");
    assert_eq!(plain, vec![Item::tool_call("call_04", "search", "{}")]);
}

/// A namespace folded into `name` is part of the name, and canonicalization
/// does not split it back apart (F10).
///
/// The corrected half of `dialect.rs`'s "why that direction" argument. That
/// module's earlier draft justified keeping the namespace out of the log by
/// claiming a namespaced resend and a flat resend already arrive as one
/// canonical item, because `canonical_item` ignores `namespace` and `id` on
/// the way in. It ignores a *separate* `namespace` field — which is what
/// makes `CodexResponses`'s own resend round-trip, asserted directly above
/// — and nothing more. No dialect emits the flat spelling today, so nothing
/// is broken; what was wrong was the reason, and a reason that does not hold
/// is what gets a future change waved through.
///
/// Pinned as the divergence rather than deleted with the prose, because the
/// day a flat variant landed this was the assertion to revisit
/// deliberately.
///
/// **It landed, and the answer was to leave this exactly as it is** (M12,
/// R-M1). `ClientDialect::ClaudeMessages` spells MCP tools flat, and the
/// Messages surface stores them flat — it does *not* split them here or
/// anywhere. The two spellings never meet: each is written by one client on
/// one surface, a session is written by one client, and what has to
/// recognise either is `is_control_call_on`, which takes the surface and
/// so needs no reconciliation here. Teaching this function to split would
/// buy nothing and would move the `turn_id` of every already-stored
/// tool-using session. So the divergence below is the
/// shipped behaviour, not a debt.
///
/// **M17 widened the gap rather than closing it, and deliberately.** The
/// namespaced form now canonicalizes with a `namespace` field the flat form
/// cannot have, so the two items differ in two places instead of one. That
/// is the same ruling read forward: each spelling stays the word of the
/// client that sent it. It is also why `Item::tool_call`'s doc had to be
/// corrected in the same rung (R-N10) — it still claimed the two arrive as
/// one item, which the `assert_ne!` below has pinned as false since M12.
#[test]
fn a_flat_spelling_is_a_different_canonical_call_until_the_wire_learns_to_split_it() {
    let namespaced = json!({
        "type": "function_call",
        "call_id": "call_1",
        "namespace": "mcp__roundhouse",
        "name": "fetch_steer",
        "arguments": "{}",
    });
    let flat = json!({
        "type": "function_call",
        "call_id": "call_1",
        "name": "mcp__roundhouse__fetch_steer",
        "arguments": "{}",
    });

    let namespaced_item = canonicalize("", &[namespaced]).expect("namespaced form parses");
    let flat_item = canonicalize("", &[flat]).expect("flat form parses");

    assert_eq!(
        namespaced_item,
        vec![Item::namespaced_tool_call(
            "call_1",
            "fetch_steer",
            Some("mcp__roundhouse".into()),
            "{}"
        )],
        "a separate `namespace` field is carried beside the bare name \
         (M17, R-N6) and is never folded into it"
    );
    assert_eq!(
        flat_item,
        vec![Item::tool_call(
            "call_1",
            "mcp__roundhouse__fetch_steer",
            "{}"
        )],
        "a namespace folded into `name` is kept verbatim, so the two \
         spellings of one call are two canonical items"
    );
    assert_ne!(
        namespaced_item, flat_item,
        "if these ever agree, `canonical_item` has learned to split a flat \
         name — which R-M1 ruled it must not do, because splitting moves \
         the `turn_id` of every stored tool-using session and buys nothing \
         now that the recognizer is told which surface it is reading"
    );
}

/// R-M0 (M12): what a Codex MCP call *is* on the Responses wire, asked of
/// codex's own type rather than of a fixture this repo wrote.
///
/// The two tests above are hand-written JSON. They assert the right thing,
/// but they cannot answer the question M12 had to settle — which of the two
/// spellings a real Codex actually sends — because a fixture only ever
/// agrees with whoever typed it. This one builds the item with
/// [`codex_protocol::models::ResponseItem::FunctionCall`], serializes it
/// with codex's own `Serialize`, and feeds the bytes to [`canonicalize`].
/// If codex ever folds the namespace into `name`, the variant stops having a
/// `namespace` field and this stops compiling.
///
/// **The determination, with its citations against the Cargo pin
/// `6344a65`.** Codex presents an MCP server to the model as one
/// *namespace* object, not as N flat functions:
/// `core/src/tools/handlers/mcp.rs:388-393` builds
/// `ToolSpec::Namespace { name: callable_namespace, tools: [...] }` and
/// `tools/src/responses_api.rs:117-123` puts each tool in it under
/// `tool_name.name` — the **bare** name, via `.renamed(tool_name.name)`.
/// The namespace string is `mcp__<server>` (`codex-mcp/src/tools.rs:139-146`
/// prefixing the sanitized server name, `:228-234`), which is exactly
/// [`crate::dialect::DEFAULT_MCP_NAMESPACE`]. The model's call therefore
/// comes back as bare `name` plus a separate `namespace`
/// (`protocol/src/models.rs:910-928`, and codex's own suite emits precisely
/// that JSON in `core/tests/common/responses.rs:929-945`), and dispatch is
/// an exact `ToolName { name, namespace }` registry lookup
/// (`core/src/tools/router.rs:154-170`), which is why a flat spelling
/// resolves against nothing (`core/src/tools/registry.rs:828`). The M9
/// suite already read the first half of this off a *real binary*:
/// `codex_e2e.rs`'s `the_delimiter_a_skill_spells_is_the_one_the_real_binary_namespaces_with`
/// finds `tool["type"] == "namespace"`, `tool["name"] ==
/// DEFAULT_MCP_NAMESPACE`, and `prefer` listed under it bare.
///
/// **So the log stores the bare name**, and that is the fact this test
/// exists to make executable. It is not a defect of this module — dropping
/// the namespace is what makes a resend round-trip — but it is the premise
/// of a claim made a crate below, and `roundhouse-core`'s
/// `a_control_call_as_the_responses_wire_stores_it_is_recognised` is what
/// M12 turned that claim into: the classifier now knows the bare spelling
/// as well as the flat one.
#[test]
fn r_m0_a_codex_mcp_call_arrives_bare_with_a_separate_namespace() {
    use codex_protocol::models::ResponseItem;

    let call = ResponseItem::FunctionCall {
        id: None,
        name: "status".to_string(),
        namespace: Some(crate::dialect::DEFAULT_MCP_NAMESPACE.to_string()),
        arguments: "{}".to_string(),
        encrypted_function_args: None,
        call_id: "call_r_m0".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let wire = serde_json::to_value(&call).expect("codex serializes its own item");

    // The wire shape itself, before anything of ours reads it. Spelled as
    // three separate assertions rather than one whole-value comparison so a
    // new optional field upstream does not read as a change of spelling.
    assert_eq!(wire["type"], json!("function_call"));
    assert_eq!(
        wire["name"],
        json!("status"),
        "codex names an MCP tool by its bare name on the Responses wire: {wire:#?}"
    );
    assert_eq!(
        wire["namespace"],
        json!(crate::dialect::DEFAULT_MCP_NAMESPACE),
        "the namespace is a separate wire field, not a prefix on `name`: {wire:#?}"
    );

    let stored = canonicalize("", &[wire]).expect("a client's own call is resendable");
    assert_eq!(
        stored,
        vec![Item::namespaced_tool_call(
            "call_r_m0",
            "status",
            Some(crate::dialect::DEFAULT_MCP_NAMESPACE.to_string()),
            "{}"
        )],
        "the log keeps codex's two fields as two fields — the bare name and \
         the namespace beside it — so nothing downstream may match on a \
         flat `mcp__roundhouse__` prefix without splitting it first"
    );

    // The consequence, asserted here because this is where the evidence
    // is. R-M0 found this false — a bare stored name satisfied no
    // classifier — which is the finding M12 closed by teaching
    // the recognizer the bare spelling. Asserted through the *stored*
    // item rather than against a literal, so a change to canonicalization
    // that reintroduced the namespace would be caught here rather than
    // agreeing with a string this test typed.
    let Some(roundhouse_core::item::ItemContent::ToolCall {
        name, namespace, ..
    }) = stored.first().map(|item| &item.content)
    else {
        panic!("the canonical item is a tool call: {stored:#?}");
    };
    assert!(
        roundhouse_core::validate::is_control_call_on(
            name,
            namespace.as_deref(),
            roundhouse_core::validate::ControlCallDialect::CodexResponses
        ),
        "an MCP control call made over the Responses wire has to be \
         recognisable from what the log actually holds (`{name}` under \
         `{namespace:?}`), or the fold counts roundhouse's own chatter as \
         the agent's work"
    );
}

/// M12 fix-stage finding F1: the assertion above exercises the recognizer
/// only on the *bare* name this surface actually stores (`"status"`), which
/// `CONTROL_TOOL_NAMES.contains` alone already recognises — so a refute-stage
/// mutation that deleted the flat-name half of the classifier
/// (`is_flat_control_call`) left the test above green. Nothing on this
/// surface exercised the flat half at all; only the Messages-side tests
/// (`a_flat_control_call_is_recognised_and_dropped_from_the_task_view`,
/// `both_spellings_of_every_control_tool_are_ours_and_the_near_misses_are_not`
/// in `roundhouse-core`) did. This test closes that gap from the Responses
/// side: a flat-spelled name is not a shape this wire ever produces, but
/// the recognizer is a shared classifier and this surface's own suite
/// should not depend on a sibling crate's tests to prove the half it does
/// not exercise is still there.
///
/// **Rewritten by M12 review finding F8, and the rewrite is the finding.**
/// The classifier is no longer one function that says yes to both
/// spellings: it takes the surface, because a bare name means opposite
/// things on the two wires. So the assertion this test was reaching for —
/// "the flat half is still there" — is now a statement about the *Messages*
/// dialect, and the honest thing for this surface's suite to pin beside it
/// is that its own dialect says **no** to a flat name. That negative is the
/// half that would go silently wrong here: a Responses recognizer that
/// accepted the flat spelling would swallow a codex client's genuinely
/// flat-named tool from another server.
#[test]
fn a_flat_spelled_name_belongs_to_the_other_surfaces_dialect() {
    use roundhouse_core::validate::ControlCallDialect;

    let flat = roundhouse_core::validate::flat_control_call_name("status");
    assert!(
        roundhouse_core::validate::is_control_call_on(
            &flat,
            None,
            ControlCallDialect::ClaudeMessages
        ),
        "the flat spelling `{flat}` is what the Messages surface stores"
    );
    assert!(
        !roundhouse_core::validate::is_control_call_on(
            &flat,
            None,
            ControlCallDialect::CodexResponses
        ),
        "this wire cannot produce `{flat}` — codex dispatches on an exact \
         `ToolName {{ name, namespace }}` lookup — so a Responses fold that \
         recognised it would be exempting somebody else's tool"
    );
}

/// The turn id of a fixed pre-M4-shaped conversation, pinned as a literal.
///
/// The idempotency story rests on this hash being a pure function of the
/// conversation, stable across processes, machines, and releases: a client
/// retry hashes to the same turn and replays instead of paying twice. An
/// unchanged-code argument held that property through M4; a literal holds
/// it through every future change, because any edit to `Item::render`, the
/// FNV constants, or canonicalization that moves historical hashes fails
/// here first — and such an edit orphans every in-flight retry, so it must
/// be a decision, not a side effect.
#[test]
fn the_turn_id_of_a_fixed_conversation_is_pinned() {
    let claimed = canonicalize(
        "be brief",
        &[
            serde_json::json!({"type": "message", "role": "user", "content": "hello"}),
            serde_json::json!({"type": "function_call", "call_id": "call_1",
                                "name": "search", "arguments": "{\"q\":\"rust\"}"}),
            serde_json::json!({"type": "function_call_output", "call_id": "call_1",
                                "output": "3 hits"}),
        ],
    )
    .expect("a fixed, well-formed conversation canonicalizes");
    assert_eq!(turn_id_for(&claimed).to_string(), "turn_6a7aaa94e5b59fd2");
}

/// **R-N7: the turn id of a conversation that *contains a control call*,
/// bare and namespaced, pinned as one literal.**
///
/// The existing pin above cannot see this rung's edit: its fixture tool is
/// `search`, an ordinary client tool with no `namespace` on the wire, so a
/// change confined to how a namespaced call canonicalizes leaves it green.
/// That is the hazard §3.5 of the stored-namespace evidence names — the
/// guard the tree wrote to catch "an edit that moves historical hashes" is
/// blind to exactly the edit that carries a namespace into the log.
///
/// So these two fixtures differ in **one wire field and nothing else**, and
/// they pin the *same* literal. An implementation that folded the carried
/// namespace into [`Item::render`] would move both away from the literal at
/// once, and a client's in-flight retry of a control-call turn would miss
/// its own completed response and buy a second billed answer. Pinned as a
/// literal rather than as `assert_eq!(bare, namespaced)` alone, because the
/// equality on its own would stay true if the render started hashing the
/// namespace *and* canonicalization stopped reading it.
#[test]
fn the_turn_id_of_a_control_call_conversation_is_pinned_bare_and_namespaced() {
    let conversation = |call: Value| {
        canonicalize(
            "be brief",
            &[
                json!({"type": "message", "role": "user", "content": "how am I doing?"}),
                call,
                json!({"type": "function_call_output", "call_id": "call_1",
                        "output": "on budget"}),
            ],
        )
        .expect("a fixed, well-formed control-call conversation canonicalizes")
    };

    let bare = conversation(json!({
        "type": "function_call", "call_id": "call_1",
        "name": "status", "arguments": "{}",
    }));
    let namespaced = conversation(json!({
        "type": "function_call", "call_id": "call_1",
        "name": "status", "namespace": "mcp__roundhouse", "arguments": "{}",
    }));

    assert_eq!(
        turn_id_for(&bare).to_string(),
        "turn_a579e6c0755cc987",
        "a control-call conversation stored before this rung must keep the \
         turn id it already has"
    );
    assert_eq!(
        turn_id_for(&namespaced).to_string(),
        "turn_a579e6c0755cc987",
        "the namespace is carried beside the name and left out of the \
         render, so the same conversation hashes the same way whether the \
         client sent the field or not"
    );
}

/// F18 (review): codex's `ResponseItem` enum has resendable variants far
/// beyond `message`/`function_call`/`function_call_output`/`reasoning` —
/// `tool_search_call`, `local_shell_call`, the three compaction shapes,
/// and more. None of them can appear in a v1 turn (no tool loop means no
/// tool_search/shell/compaction), so today's suite only ever resends the
/// four shapes above and never proves what `canonical_item` does with the
/// rest. This pins that boundary as documented, enumerated behavior: each
/// of these types must 422 with an error that *names the type*, so a
/// future tool-loop milestone that starts emitting one finds a named
/// failure instead of a silent behavior change.
#[test]
fn the_item_types_a_real_client_can_resend_are_named() {
    let refused_types = [
        "agent_message",
        "local_shell_call",
        "tool_search_call",
        "tool_search_output",
        "custom_tool_call",
        "custom_tool_call_output",
        "web_search_call",
        "image_generation_call",
        "compaction",
        "compaction_trigger",
        "context_compaction",
    ];
    for kind in refused_types {
        let err = canonicalize("", &[json!({ "type": kind })])
            .expect_err(&format!("`{kind}` must be refused, not silently dropped"));
        let message = format!("{err:?}");
        assert!(
            message.contains(kind),
            "error for `{kind}` does not name the type it refused: {message}"
        );
    }
}

#[test]
fn tool_items_canonicalize_to_the_call_and_its_result() {
    let items = canonicalize(
        "",
        &[
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "grep",
                "arguments": "{\"q\":\"x\"}",
            }),
            json!({ "type": "function_call_output", "call_id": "call_1", "output": "3 hits" }),
        ],
    )
    .expect("both tool shapes are representable");
    assert_eq!(
        items,
        vec![
            Item {
                role: Role::Assistant,
                content: ItemContent::ToolCall {
                    call_id: "call_1".into(),
                    name: "grep".into(),
                    namespace: None,
                    arguments: "{\"q\":\"x\"}".into(),
                },
                response_id: None,
            },
            Item {
                role: Role::Tool,
                content: ItemContent::ToolResult {
                    call_id: "call_1".into(),
                    output: "3 hits".into(),
                },
                response_id: None,
            },
        ]
    );
}
