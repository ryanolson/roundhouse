// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `anthropic_messages::stream` under test.
//!
//! One module rather than the two this file used to carry (`tests` and a
//! `cache_read_tests` appended after it): both drove the same `decode`
//! helper, and splitting the cache-read cases into a second module bought
//! nothing but a second `use` of it — merged, `decode` needs no visibility
//! wider than this file.

use super::*;
use roundhouse_core::event::CacheReadSource;

/// Drive the decoder over `pieces` and collect what it yields.
fn decode(pieces: &[&str]) -> Result<Vec<FrontierChunk>, FrontierError> {
    let mut decoder = SseDecoder::default();
    let mut chunks = Vec::new();
    for piece in pieces {
        decoder.feed(piece.as_bytes())?;
        while let Some(chunk) = decoder.next_chunk() {
            chunks.push(chunk);
        }
    }
    decoder.eof()?;
    while let Some(chunk) = decoder.next_chunk() {
        chunks.push(chunk);
    }
    Ok(chunks)
}

/// A `message_start` whose usage object is the one an Anthropic turn on a
/// warm prefix actually reports: a handful of fresh tokens, a large cache
/// read, and a cache write for whatever the breakpoint newly covered.
const START: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"type":"message","id":"msg_1","#,
    r#""role":"assistant","model":"claude-x","content":[],"stop_reason":null,"#,
    r#""stop_sequence":null,"usage":{"input_tokens":12,"cache_read_input_tokens":9000,"#,
    r#""cache_creation_input_tokens":500,"output_tokens":1,"#,
    r#""cache_creation":{"ephemeral_5m_input_tokens":500,"ephemeral_1h_input_tokens":0}}}}"#,
    "\n\n"
);

const DELTA: &str = concat!(
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","#,
    r#""stop_sequence":null},"usage":{"output_tokens":64}}"#,
    "\n\n"
);

const STOP: &str = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

fn text(index: u64, body: &str) -> String {
    format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\
         \"index\":{index},\"delta\":{{\"type\":\"text_delta\",\"text\":\"{body}\"}}}}\n\n"
    )
}

/// A `content_block_start` frame carrying `block` verbatim as its
/// `content_block`.
fn block_start(index: u64, block: &str) -> String {
    format!(
        "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\
         \"index\":{index},\"content_block\":{block}}}\n\n"
    )
}

/// One `input_json_delta`. `fragment` is written already-escaped for JSON,
/// because that is what a fragment of tool arguments looks like inside the
/// frame that carries it.
fn json_delta(index: u64, fragment: &str) -> String {
    format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\
         \"index\":{index},\"delta\":{{\"type\":\"input_json_delta\",\
         \"partial_json\":\"{fragment}\"}}}}\n\n"
    )
}

/// A final `message_delta` reporting `reason` and the usual output count.
fn stop_because(reason: &str) -> String {
    format!(
        "event: message_delta\ndata: {{\"type\":\"message_delta\",\
         \"delta\":{{\"stop_reason\":\"{reason}\",\"stop_sequence\":null}},\
         \"usage\":{{\"output_tokens\":64}}}}\n\n"
    )
}

/// **The finding-1 analog for this dialect: both usage events fold into one
/// `Done`, and the input side is converted out of Anthropic's axes.**
///
/// A client that read only `message_delta` — the natural choice, since it is
/// the frame that carries the completion — reports zero input tokens and no
/// cache reads, which is exactly the quantity this system exists to
/// maximize. A client that read `message_start` and passed `input_tokens`
/// through unconverted reports 12 input tokens for a 9 512-token prompt.
#[test]
fn the_two_usage_events_fold_into_one_done_in_roundhouse_axes() {
    let chunks = decode(&[
        START,
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\
         \"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        &text(0, "Hel"),
        &text(0, "lo"),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        DELTA,
        STOP,
    ])
    .unwrap();

    assert_eq!(
        chunks,
        vec![
            FrontierChunk::OutputText("Hel".into()),
            FrontierChunk::OutputText("lo".into()),
            FrontierChunk::Done {
                // 12 fresh + 9 000 read + 500 written. Anthropic's three
                // counters are disjoint; roundhouse's input is the total.
                input_tokens: 9_512,
                cached_input_tokens: 9_000,
                cache_read_source: CacheReadSource::Provider,
                cache_write_tokens: 500,
                output_tokens: 64,
                reasoning_tokens: 0,
                provider_reported_cost: None,
                // `DELTA` says `end_turn`, and it reaches the log as the
                // word the wire used.
                stop_reason: Some("end_turn".into()),
            },
        ]
    );
}

#[test]
fn a_stream_that_dies_before_message_stop_yields_no_accounting_frame() {
    // PROBE: everything but the terminal frame, including a complete usage
    // picture. A `Done` here would be a *correct-looking* accounting record
    // for a turn that never finished — worse than none, because the engine's
    // estimated-and-marked path would then never run.
    let chunks = decode(&[START, &text(0, "half an answ"), DELTA]).unwrap();
    assert_eq!(
        chunks,
        vec![FrontierChunk::OutputText("half an answ".into())]
    );

    // CONTROL: the identical stream with the terminal frame appended does
    // account, so the assertion above is about `message_stop` and not about
    // the fold being broken.
    let complete = decode(&[START, &text(0, "half an answ"), DELTA, STOP]).unwrap();
    assert!(matches!(complete[1], FrontierChunk::Done { .. }));
}

#[test]
fn a_stream_with_no_reported_input_is_unaccounted_rather_than_free() {
    // PROBE: a prelude whose usage object reports nothing — a proxy that
    // stripped it, or an upstream that never sent one. Emitting a `Done`
    // with `input_tokens: 0` would bill the prompt at nothing, which folds
    // as a saving on the one dashboard this product is judged by.
    let chunks = decode(&[
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":\
         {\"type\":\"message\",\"content\":[],\"usage\":{\"output_tokens\":1}}}\n\n",
        &text(0, "hi"),
        DELTA,
        STOP,
    ])
    .unwrap();
    assert_eq!(chunks, vec![FrontierChunk::OutputText("hi".into())]);

    // CONTROL: one fresh input token is enough to make the same stream
    // accountable, so the rule is "nothing was reported", not "the counts
    // were small".
    let counted = decode(&[
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":\
         {\"type\":\"message\",\"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n",
        DELTA,
        STOP,
    ])
    .unwrap();
    assert_eq!(
        counted,
        vec![FrontierChunk::Done {
            input_tokens: 1,
            cached_input_tokens: 0,
            cache_read_source: CacheReadSource::Unreported,
            cache_write_tokens: 0,
            output_tokens: 64,
            reasoning_tokens: 0,
            provider_reported_cost: None,
            stop_reason: Some("end_turn".into()),
        }]
    );
}

#[test]
fn a_message_delta_that_never_reports_output_before_message_stop_is_unaccounted_rather_than_free() {
    // PROBE (F6, valid): the output-side mirror of the test just above.
    // `usage` on `MessageDelta` is `Option<Usage>` specifically so "the
    // frame reported no counts" and "the frame reported zero" are different
    // facts (`wire.rs`'s own doc on `MessageDelta::usage`) — and `emit_done`
    // used to gate on `saw_input` alone, with no output-side equivalent. A
    // `message_delta` that carries `stop_reason` but omits `usage` entirely
    // (a proxy that stripped it, or an upstream that never restates the
    // count) left `output_tokens` at Rust's default `0`, and `message_stop`
    // folded that into a `Done` the engine books as
    // `Accounting::Reported` — real streamed output priced at zero dollars
    // and labelled "the provider reported this" when no frame ever did.
    // Symmetric with the rule above: nothing reported must not be written
    // down as zero, on either axis.
    let chunks = decode(&[
        START,
        &text(0, "half an answer"),
        concat!(
            "event: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","#,
            r#""stop_sequence":null}}"#,
            "\n\n"
        ),
        STOP,
    ])
    .unwrap();
    assert_eq!(
        chunks,
        vec![FrontierChunk::OutputText("half an answer".into())],
        "a Done was emitted carrying a fabricated output_tokens: 0 even \
         though no frame ever reported the output count"
    );
}

/// **F1's reporting half, closed (M11.1 thermo-nuclear review → M11.2).**
///
/// The ceiling half was fixed in M11.1: a dispatch carries the client's own
/// `max_tokens` rather than the router's 256-token pricing estimate, which
/// makes a truncation here an *honest* one. This was the half that said
/// nobody downstream can tell it happened — the `"message_delta"` arm
/// destructured `let StreamEvent::MessageDelta { usage, .. }` and
/// `delta.stop_reason` went into the `..`, while
/// [`FrontierChunk::Done`] had no field to carry one at all. The loss was
/// therefore structural rather than a missed read, and this test stood
/// `#[ignore]`d as its evidence until `Done::stop_reason` existed.
///
/// PROBE: two streams differing in *only* `delta.stop_reason` — same
/// prelude, same text, same `output_tokens: 64`. They must decode
/// differently, and to the two words the wire actually used.
#[test]
fn f1_a_dispatch_ceiling_truncation_is_distinguishable_from_a_natural_stop() {
    let truncated = decode(&[
        START,
        &text(0, "cut off mid-sen"),
        concat!(
            "event: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens","#,
            r#""stop_sequence":null},"usage":{"output_tokens":64}}"#,
            "\n\n"
        ),
        STOP,
    ])
    .unwrap();

    // CONTROL: the identical stream with `stop_reason` swapped back to
    // `end_turn` and nothing else touched — same text, same
    // `output_tokens: 64`. Without it the assertions below would pass for a
    // decoder that stamped `max_tokens` on every turn.
    let natural = decode(&[START, &text(0, "cut off mid-sen"), DELTA, STOP]).unwrap();

    assert_ne!(
        truncated, natural,
        "the truncation signal must survive the decode or it can never \
         surface to a client as stop_reason: max_tokens either"
    );
    assert!(
        matches!(
            &truncated[1],
            FrontierChunk::Done { stop_reason: Some(reason), .. } if reason == "max_tokens"
        ),
        "{truncated:?}"
    );
    assert!(
        matches!(
            &natural[1],
            FrontierChunk::Done { stop_reason: Some(reason), .. } if reason == "end_turn"
        ),
        "{natural:?}"
    );
}

/// A stop reason newer than this build reaches the log as the wire spelled
/// it.
///
/// `StopReason` has an `Other(String)` arm for exactly this, and it would be
/// worth nothing if the decoder collapsed the arm into `None` or into a
/// nearest-neighbour guess on the way out. Two of the seven values Anthropic
/// ships today arrived after the crates that closed this enum shipped, so an
/// eighth is a scheduled event and not a hypothetical.
#[test]
fn a_stop_reason_this_build_has_never_seen_is_carried_verbatim() {
    let chunks = decode(&[
        START,
        &text(0, "hm"),
        concat!(
            "event: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"quantum_hesitation"},"#,
            r#""usage":{"output_tokens":3}}"#,
            "\n\n"
        ),
        STOP,
    ])
    .unwrap();
    assert!(
        matches!(
            &chunks[1],
            FrontierChunk::Done { stop_reason: Some(reason), .. }
                if reason == "quantum_hesitation"
        ),
        "{chunks:?}"
    );
}

/// A later frame that omits `stop_reason` must not retract the one that
/// ended the turn.
///
/// The wire sends an explicit `"stop_reason": null` on every non-final
/// delta, so a plain assignment reads the *last* frame rather than the one
/// that said something — and on a stream whose final `message_delta` is
/// followed by another restating only the counts, the reason vanishes.
/// Symmetric with the count-merge rule two tests below.
#[test]
fn a_message_delta_that_omits_a_stop_reason_cannot_retract_one() {
    let chunks = decode(&[
        START,
        &text(0, "x"),
        concat!(
            "event: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"#,
            r#""usage":{"output_tokens":9}}"#,
            "\n\n"
        ),
        // An explicit null, which is what the wire sends on a non-final
        // delta, and then a frame with no `stop_reason` key at all.
        concat!(
            "event: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":null},"#,
            r#""usage":{"output_tokens":11}}"#,
            "\n\n"
        ),
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{}}\n\n",
        STOP,
    ])
    .unwrap();
    assert!(
        matches!(
            &chunks[1],
            FrontierChunk::Done { stop_reason: Some(reason), output_tokens: 11, .. }
                if reason == "tool_use"
        ),
        "{chunks:?}"
    );
}

/// **The tool-call decode, on the shape a real agentic turn has.**
///
/// Text first, then a tool block whose arguments arrive as fragments that
/// are not JSON on their own, then a second tool block — because a turn that
/// reads two files calls the tool twice, and the two calls' fragments are
/// distinguished by *nothing but the index*. A decoder that kept one
/// "current" block would splice the second call's arguments onto the first.
#[test]
fn interleaved_text_and_tool_blocks_decode_to_one_call_per_block() {
    let chunks = decode(&[
        START,
        &block_start(0, r#"{"type":"text","text":""}"#),
        &text(0, "Let me look."),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        &block_start(
            1,
            r#"{"type":"tool_use","id":"toolu_01A","name":"Read","input":{}}"#,
        ),
        &json_delta(1, r#"{\"path\":"#),
        &json_delta(1, r#"\"/etc/hosts\"}"#),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        &block_start(
            2,
            r#"{"type":"tool_use","id":"toolu_01B","name":"Grep","input":{}}"#,
        ),
        &json_delta(2, r#"{\"pattern\":\"fn main\"}"#),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
        &stop_because("tool_use"),
        STOP,
    ])
    .unwrap();

    assert_eq!(
        chunks,
        vec![
            FrontierChunk::OutputText("Let me look.".into()),
            FrontierChunk::ToolCall {
                id: "toolu_01A".into(),
                name: "Read".into(),
                namespace: None,
                // Byte-exact reassembly of the two fragments, in order.
                arguments: r#"{"path":"/etc/hosts"}"#.into(),
            },
            FrontierChunk::ToolCall {
                id: "toolu_01B".into(),
                name: "Grep".into(),
                namespace: None,
                arguments: r#"{"pattern":"fn main"}"#.into(),
            },
            FrontierChunk::Done {
                input_tokens: 9_512,
                cached_input_tokens: 9_000,
                cache_read_source: CacheReadSource::Provider,
                cache_write_tokens: 500,
                output_tokens: 64,
                reasoning_tokens: 0,
                provider_reported_cost: None,
                // The whole point of the turn: the client is being told to
                // run something and come back, not that the answer is over.
                stop_reason: Some("tool_use".into()),
            },
        ]
    );
}

/// The reassembly must survive a socket, which is where it is actually done.
///
/// A fragment boundary and a read boundary have nothing to do with each
/// other: the provider chooses the first and the network the second, and a
/// decoder that happened to work when each frame arrived whole would fail on
/// a real connection at a rate that looks like intermittent tool corruption.
#[test]
fn tool_arguments_reassemble_across_arbitrary_read_boundaries() {
    let whole = format!(
        "{START}{}{}{}{}{}{}{STOP}",
        block_start(
            0,
            r#"{"type":"tool_use","id":"toolu_01C","name":"Bash","input":{}}"#
        ),
        json_delta(0, r#"{\"command\":\"cargo "#),
        json_delta(0, r#"test --workspace\","#),
        json_delta(0, r#"\"timeout\":900}"#),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        stop_because("tool_use"),
    );

    let mut decoder = SseDecoder::default();
    let mut chunks = Vec::new();
    // One byte at a time: every fragment, every frame and every line
    // terminator is split.
    for byte in whole.as_bytes() {
        decoder.feed(&[*byte]).unwrap();
        while let Some(chunk) = decoder.next_chunk() {
            chunks.push(chunk);
        }
    }
    decoder.eof().unwrap();
    while let Some(chunk) = decoder.next_chunk() {
        chunks.push(chunk);
    }

    assert_eq!(
        chunks[0],
        FrontierChunk::ToolCall {
            id: "toolu_01C".into(),
            name: "Bash".into(),
            namespace: None,
            arguments: r#"{"command":"cargo test --workspace","timeout":900}"#.into(),
        }
    );
    assert!(matches!(chunks[1], FrontierChunk::Done { .. }));
}

/// **A tool block that never closes emits nothing**, the same discipline as
/// the missing `Done`.
///
/// Its arguments are a *prefix* of a JSON document the provider stopped
/// sending. Emitting the prefix would hand a client a call it cannot parse,
/// and a client that ran it anyway would act on truncated input — a
/// `{"command":"rm -rf /tm` is not a smaller version of the command that was
/// being sent.
#[test]
fn a_tool_block_that_never_closes_emits_no_call() {
    // PROBE: fragments arrive, the terminal frame arrives, the block's stop
    // never does.
    let chunks = decode(&[
        START,
        &block_start(
            0,
            r#"{"type":"tool_use","id":"toolu_01D","name":"Bash","input":{}}"#,
        ),
        &json_delta(0, r#"{\"command\":\"rm -rf /tm"#),
        &stop_because("tool_use"),
        STOP,
    ])
    .unwrap();
    assert_eq!(chunks.len(), 1, "only the accounting frame: {chunks:?}");
    assert!(matches!(chunks[0], FrontierChunk::Done { .. }));

    // CONTROL: the identical stream with the block's own stop frame appended
    // before the terminal one does emit the call, so the rule is "the block
    // never closed" and not "tool blocks are dropped".
    let closed = decode(&[
        START,
        &block_start(
            0,
            r#"{"type":"tool_use","id":"toolu_01D","name":"Bash","input":{}}"#,
        ),
        &json_delta(0, r#"{\"command\":\"ls\"}"#),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        &stop_because("tool_use"),
        STOP,
    ])
    .unwrap();
    assert_eq!(
        closed[0],
        FrontierChunk::ToolCall {
            id: "toolu_01D".into(),
            name: "Bash".into(),
            namespace: None,
            arguments: r#"{"command":"ls"}"#.into(),
        }
    );
}

/// **F7 (M11.2a thermo-nuclear review).** A `content_block_stop` whose
/// reassembled fragments do not parse as JSON drops the call — the same
/// answer this decoder gives a block that never closes at all — so nothing
/// downstream (the log commit, both serve projections) ever holds a
/// `FrontierChunk::ToolCall` with unparseable arguments.
///
/// PROBE: a block that *does* receive its `content_block_stop`, but whose
/// one fragment is `{"command": "ls -la"` — a complete key/value pair with
/// no closing brace, exactly what a connection that died one fragment early
/// produces. Before the fix this reached the log, and the two serve
/// projections disagreed about the identical stored call — see
/// `messages_api.rs`'s own F7 evidence for the client-visible half of that.
#[test]
fn a_closed_block_whose_arguments_never_parse_emits_no_call() {
    let chunks = decode(&[
        START,
        &block_start(
            0,
            r#"{"type":"tool_use","id":"toolu_01E","name":"Bash","input":{}}"#,
        ),
        &json_delta(0, r#"{\"command\": \"ls -la\""#),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        &stop_because("tool_use"),
        STOP,
    ])
    .unwrap();
    assert_eq!(chunks.len(), 1, "only the accounting frame: {chunks:?}");
    assert!(matches!(chunks[0], FrontierChunk::Done { .. }));

    // CONTROL: a parseable-but-weird argument string — unusual whitespace,
    // nothing a model would normally produce — still reaches the client
    // verbatim. The gate above is "does this parse", never "does this look
    // ordinary", and this is what proves the PROBE above failed for being
    // invalid JSON and not merely for looking unfamiliar.
    let weird = decode(&[
        START,
        &block_start(
            0,
            r#"{"type":"tool_use","id":"toolu_01F","name":"Bash","input":{}}"#,
        ),
        &json_delta(0, r#"{ \"command\" : \"ls -la\" }"#),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        &stop_because("tool_use"),
        STOP,
    ])
    .unwrap();
    assert_eq!(
        weird[0],
        FrontierChunk::ToolCall {
            id: "toolu_01F".into(),
            name: "Bash".into(),
            namespace: None,
            arguments: r#"{ "command" : "ls -la" }"#.into(),
        },
        "a parseable-but-unusual argument string must pass through \
         untouched, not be refused or reformatted: {weird:?}"
    );
}

/// A tool that takes no arguments is a call with `{}`, not a call with an
/// empty string.
///
/// The wire's own answer: `content_block_start` carries `input: {}` and no
/// fragment ever follows. An empty `arguments` is not valid JSON, so every
/// consumer downstream would have to special-case it — and the one that
/// forgot would hand the client's tool runner a parse error for a call the
/// model made correctly.
#[test]
fn a_tool_block_with_no_argument_fragments_yields_the_empty_object() {
    for (seed, expected, why) in [
        (r#""input":{}"#, "{}", "the wire's own empty object"),
        (r#""input":null"#, "{}", "a null input is nobody saying"),
        // A whole `input` with no fragments: not what the streaming API
        // documents, and exactly what a proxy that collapsed a block into
        // its start frame would send. Read rather than discarded, because
        // discarding it would turn a complete call into an argument-less one.
        (
            r#""input":{"a":1}"#,
            r#"{"a":1}"#,
            "a block sent whole on its start frame",
        ),
    ] {
        let chunks = decode(&[
            START,
            &block_start(
                0,
                &format!(r#"{{"type":"tool_use","id":"t","name":"Now",{seed}}}"#),
            ),
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            &stop_because("tool_use"),
            STOP,
        ])
        .unwrap_or_else(|error| panic!("{why}: {error}"));
        assert_eq!(
            chunks[0],
            FrontierChunk::ToolCall {
                id: "t".into(),
                name: "Now".into(),
                namespace: None,
                arguments: expected.to_string(),
            },
            "{why}"
        );
    }
}

/// A fragment for a block nobody opened, and a lifecycle frame this build
/// cannot read, both cost the call and never the turn.
///
/// The asymmetry with `message_start` is deliberate and stated in the
/// dispatcher: an unreadable prelude loses the turn's accounting, which is
/// worth failing for; an unreadable block frame loses one tool call from a
/// turn that was still served and still billed, and failing it would turn a
/// frame we merely did not understand into an unanswered request.
#[test]
fn an_unreadable_block_frame_loses_its_call_and_not_the_turn() {
    let chunks = decode(&[
        START,
        // An index that is not a number: nothing opens.
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\
         \"index\":\"one\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\
         \"name\":\"Read\",\"input\":{}}}\n\n",
        // A fragment for a block that was never opened.
        &json_delta(7, r#"{\"orphan\":true}"#),
        // And a stop for one, which must not invent a call either.
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":7}\n\n",
        &text(1, "answered anyway"),
        DELTA,
        STOP,
    ])
    .unwrap();
    assert_eq!(
        chunks,
        vec![
            FrontierChunk::OutputText("answered anyway".into()),
            FrontierChunk::Done {
                input_tokens: 9_512,
                cached_input_tokens: 9_000,
                cache_read_source: CacheReadSource::Provider,
                cache_write_tokens: 500,
                output_tokens: 64,
                reasoning_tokens: 0,
                provider_reported_cost: None,
                stop_reason: Some("end_turn".into()),
            },
        ]
    );
}

#[test]
fn thinking_and_signature_deltas_are_consumed_and_never_spoken() {
    // PROBE: an extended-thinking turn. The thinking text must not reach the
    // client's transcript or the durable item the next turn resends, and the
    // frames must not fail the turn either — they arrive on every thinking
    // turn, which is the traffic this product routes most.
    let chunks = decode(&[
        START,
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\
         \"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\
         \"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"the user wants\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\
         \"delta\":{\"type\":\"signature_delta\",\"signature\":\"EqQBCgIYAh\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        &text(1, "Hello"),
        DELTA,
        STOP,
    ])
    .unwrap();

    assert_eq!(chunks[0], FrontierChunk::OutputText("Hello".into()));
    assert_eq!(chunks.len(), 2, "one delta and one accounting frame");
    for chunk in &chunks {
        if let FrontierChunk::OutputText(text) = chunk {
            assert!(!text.contains("the user wants"), "thinking was spoken");
            assert!(!text.contains("EqQBCgIYAh"), "a signature was spoken");
        }
    }
}

#[test]
fn tool_argument_fragments_and_unknown_deltas_do_not_fail_the_turn() {
    // `input_json_delta` fragments are not JSON on their own, and
    // `citations_delta` is a type this build does not name. Both must be
    // consumed silently: the client runs its own tools, and an upstream
    // adding a delta type must not break a deployment that already works.
    let chunks = decode(&[
        START,
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\
         \"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\
         \"delta\":{\"type\":\"citations_delta\",\"citation\":{\"x\":1}}}\n\n",
        &text(2, "done"),
        DELTA,
        STOP,
    ])
    .unwrap();
    assert_eq!(chunks[0], FrontierChunk::OutputText("done".into()));
    assert!(matches!(chunks[1], FrontierChunk::Done { .. }));
}

#[test]
fn ping_frames_and_unknown_events_are_skipped_and_keep_the_stream_alive() {
    // The gateway contract requires roundhouse to *forward* pings; this is
    // the dispatch side of the same fact — a keepalive from the upstream is
    // not output and not an error.
    let chunks = decode(&[
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        START,
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        ": a comment keepalive\n\n",
        "event: message_future\ndata: {\"type\":\"message_future\",\"x\":1}\n\n",
        &text(0, "ok"),
        DELTA,
        STOP,
    ])
    .unwrap();
    assert_eq!(chunks[0], FrontierChunk::OutputText("ok".into()));
    assert!(matches!(chunks[1], FrontierChunk::Done { .. }));
}

#[test]
fn a_mid_stream_error_is_a_failure_and_not_a_short_answer() {
    let error = decode(&[
        START,
        &text(0, "partial"),
        concat!(
            "event: error\n",
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            "\n\n"
        ),
    ])
    .expect_err("a mid-stream error must fail the turn");
    assert!(
        matches!(&error, FrontierError::Upstream(message)
            if message.contains("overloaded_error") && message.contains("Overloaded")),
        "{error}"
    );

    // An error frame whose body this build cannot read is still a failure.
    // The alternative — treating an unreadable error as an unknown frame —
    // would turn a refused turn into a silently truncated one.
    let opaque = decode(&["event: error\ndata: {\"type\":\"error\",\"error\":7}\n\n"])
        .expect_err("an unreadable error body is still an error");
    assert!(opaque.to_string().contains("error frame"), "{opaque}");
}

#[test]
fn a_frame_whose_name_and_payload_type_disagree_is_refused() {
    // PROBE: the one shape that must not be guessed at. Trusting the
    // `event:` line would fold a `message_start`'s input counts as an
    // output; trusting the payload would do the reverse. Either way an
    // accounting record is written from a frame nobody can identify.
    let error =
        decode(&["event: message_start\ndata: {\"type\":\"message_delta\",\"delta\":{}}\n\n"])
            .expect_err("a self-contradicting frame must be refused");
    assert!(
        error.to_string().contains("message_start") && error.to_string().contains("message_delta"),
        "the refusal has to name both claims: {error}"
    );

    // CONTROL: agreement is the ordinary case and decodes.
    assert!(decode(&[START, DELTA, STOP]).is_ok());
}

#[test]
fn a_frame_with_no_event_line_falls_back_to_the_payload_type() {
    // Anthropic's own client drops these; this decoder does not, because a
    // rewriting proxy in the path is a deployment shape and not a protocol
    // violation this client gains anything by punishing.
    let chunks = decode(&[
        &START.replace("event: message_start\n", ""),
        &text(0, "ok").replace("event: content_block_delta\n", ""),
        &DELTA.replace("event: message_delta\n", ""),
        &STOP.replace("event: message_stop\n", ""),
    ])
    .unwrap();
    assert_eq!(chunks[0], FrontierChunk::OutputText("ok".into()));
    assert!(matches!(
        chunks[1],
        FrontierChunk::Done {
            input_tokens: 9_512,
            ..
        }
    ));
}

#[test]
fn a_frame_with_no_payload_type_is_dispatched_on_the_event_line_alone() {
    // The mirror of the case above: the `event:` line survived and the
    // payload's own tag did not. Supplying it is not a guess, because the
    // cross-check already proved the two cannot disagree.
    let chunks = decode(&[
        "event: message_start\ndata: {\"message\":{\"content\":[],\
         \"usage\":{\"input_tokens\":40}}}\n\n",
        "event: content_block_delta\ndata: {\"index\":0,\
         \"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        "event: message_delta\ndata: {\"delta\":{},\"usage\":{\"output_tokens\":2}}\n\n",
        STOP,
    ])
    .unwrap();
    assert_eq!(chunks[0], FrontierChunk::OutputText("hi".into()));
    assert_eq!(
        chunks[1],
        FrontierChunk::Done {
            input_tokens: 40,
            cached_input_tokens: 0,
            cache_read_source: CacheReadSource::Unreported,
            cache_write_tokens: 0,
            output_tokens: 2,
            reasoning_tokens: 0,
            provider_reported_cost: None,
            // The `message_delta` here carries `"delta":{}` -- no stop
            // reason at all -- and "nobody said" is `None` rather than a
            // guessed `end_turn`.
            stop_reason: None,
        }
    );
}

#[test]
fn a_message_delta_that_omits_a_count_cannot_retract_one_already_reported() {
    // The merge rule, and the reason it is not a plain assignment: the
    // second `message_delta` here carries no counts at all, and the third
    // restates only the output. Neither may erase the prelude's input side.
    let chunks = decode(&[
        START,
        &text(0, "x"),
        DELTA,
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{}}\n\n",
        concat!(
            "event: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"#,
            r#""usage":{"input_tokens":0,"cache_read_input_tokens":0,"output_tokens":70}}"#,
            "\n\n"
        ),
        STOP,
    ])
    .unwrap();
    assert_eq!(
        chunks[1],
        FrontierChunk::Done {
            input_tokens: 9_512,
            cached_input_tokens: 9_000,
            cache_read_source: CacheReadSource::Provider,
            cache_write_tokens: 500,
            // Cumulative counts only ever grow, so the later frame wins.
            output_tokens: 70,
            reasoning_tokens: 0,
            provider_reported_cost: None,
            stop_reason: Some("end_turn".into()),
        }
    );
}

#[test]
fn a_frame_split_across_reads_is_reassembled() {
    let whole = format!("{START}{}{DELTA}{STOP}", text(0, "ok"));
    let bytes = whole.as_bytes();
    let mut decoder = SseDecoder::default();
    let mut chunks = Vec::new();
    // Deliberately unaligned to every boundary the parser cares about.
    for piece in bytes.chunks(37) {
        decoder.feed(piece).unwrap();
        while let Some(chunk) = decoder.next_chunk() {
            chunks.push(chunk);
        }
    }
    decoder.eof().unwrap();
    while let Some(chunk) = decoder.next_chunk() {
        chunks.push(chunk);
    }
    assert_eq!(chunks[0], FrontierChunk::OutputText("ok".into()));
    assert!(matches!(chunks[1], FrontierChunk::Done { .. }));

    // A multi-byte character straddling a read is text, not a failure.
    let mut decoder = SseDecoder::default();
    let frame = text(0, "日本語");
    let bytes = frame.as_bytes();
    decoder.feed(&bytes[..70]).unwrap();
    decoder.feed(&bytes[70..]).unwrap();
    assert_eq!(
        decoder.next_chunk(),
        Some(FrontierChunk::OutputText("日本語".into()))
    );
}

#[test]
fn an_unparseable_payload_names_what_happened() {
    let garbage =
        decode(&["event: message_start\ndata: not json at all\n\n"]).expect_err("must be an error");
    assert!(garbage.to_string().contains("unparseable"), "{garbage}");

    // A frame this client *reads* whose shape it cannot make sense of is
    // also an error, and it names the frame: an unreadable `message_start`
    // means the turn's input count is gone, which is not something to
    // continue past quietly.
    let unreadable =
        decode(&["event: message_start\ndata: {\"type\":\"message_start\",\"message\":3}\n\n"])
            .expect_err("must be an error");
    assert!(
        unreadable.to_string().contains("message_start"),
        "{unreadable}"
    );

    // CONTROL: the same strictness is *not* applied to a frame nothing
    // reads. A `content_block_stop` with a nonsense index is skipped without
    // parsing, so novelty in a frame this client ignores cannot fail a turn.
    assert!(
        decode(&[
            START,
            "event: content_block_stop\ndata: {\"index\":\"not a number\"}\n\n",
            DELTA,
            STOP,
        ])
        .is_ok()
    );
}

/// **The accumulation that spans frames is bounded too.**
///
/// `MAX_EVENT_BYTES` holds one frame; a tool block spans many, so an
/// upstream whose every individual frame is small and legal can still grow
/// this decoder without limit — one `content_block_start` per index and
/// never a stop, or one open block fed fragments forever. Both are the same
/// hazard `MAX_EVENT_BYTES` exists for, arriving through the door M11.2
/// opened, and both are answered the same way: abandon the stream rather
/// than buffer it.
#[test]
fn an_upstream_that_opens_tool_blocks_without_end_is_abandoned() {
    // PROBE 1: blocks opened and never closed, each frame tiny.
    let mut decoder = SseDecoder::default();
    decoder.feed(START.as_bytes()).unwrap();
    let error = (0..)
        .find_map(|index| {
            let block =
                format!(r#"{{"type":"tool_use","id":"t{index}","name":"Read","input":{{}}}}"#);
            decoder.feed(block_start(index, &block).as_bytes()).err()
        })
        .expect("an unbounded run of open blocks must be refused");
    assert!(error.to_string().contains("tool blocks"), "{error}");

    // PROBE 2: one block, fragments without end.
    let mut decoder = SseDecoder::default();
    decoder.feed(START.as_bytes()).unwrap();
    decoder
        .feed(
            block_start(
                0,
                r#"{"type":"tool_use","id":"t","name":"Read","input":{}}"#,
            )
            .as_bytes(),
        )
        .unwrap();
    let filler = "x".repeat(64 * 1024);
    let error = loop {
        match decoder.feed(json_delta(0, &filler).as_bytes()) {
            Ok(()) => continue,
            Err(error) => break error,
        }
    };
    assert!(error.to_string().contains("arguments"), "{error}");

    // CONTROL: a turn that calls many tools *in sequence* is unaffected,
    // because each block closes before the next opens — so the limit is
    // about blocks left open, not about how much a turn may do. Well past
    // `MAX_OPEN_TOOL_BLOCKS`, so a bound on the wrong quantity fails here.
    let mut decoder = SseDecoder::default();
    decoder.feed(START.as_bytes()).unwrap();
    for index in 0..(MAX_OPEN_TOOL_BLOCKS as u64 * 3) {
        let block = format!(r#"{{"type":"tool_use","id":"t{index}","name":"Read","input":{{}}}}"#);
        decoder.feed(block_start(index, &block).as_bytes()).unwrap();
        decoder
            .feed(
                format!(
                    "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\
                     \"index\":{index}}}\n\n"
                )
                .as_bytes(),
            )
            .unwrap();
    }
    let mut calls = 0;
    while let Some(chunk) = decoder.next_chunk() {
        assert!(matches!(chunk, FrontierChunk::ToolCall { .. }), "{chunk:?}");
        calls += 1;
    }
    assert_eq!(calls, MAX_OPEN_TOOL_BLOCKS * 3);
}

#[test]
fn an_upstream_that_never_ends_an_event_is_abandoned_rather_than_buffered() {
    let mut decoder = SseDecoder::default();
    let filler = "x".repeat(64 * 1024);
    let error = loop {
        match decoder.feed(filler.as_bytes()) {
            Ok(()) => continue,
            Err(error) => break error,
        }
    };
    assert!(error.to_string().contains("no event boundary"), "{error}");

    // CONTROL: a payload comfortably under the bound decodes, so the limit
    // is about an unterminated stream and not about size.
    let big = "y".repeat(4096);
    let chunks = decode(&[&text(0, &big)]).unwrap();
    assert_eq!(chunks, vec![FrontierChunk::OutputText(big)]);
}

#[test]
fn crlf_framed_events_are_split_by_drain_the_same_as_lf() {
    // PROBE (F5): SSE's line grammar accepts CR, LF, or CRLF as the line
    // terminator, so a purely CRLF-framed body is legal on the wire — as are
    // the mixed forms a lenient server produces by terminating a field line
    // one way and the blank line the other. Each framing below is the same
    // stream as `the_two_usage_events_fold_into_one_done_in_roundhouse_axes`
    // above and must decode to the same chunks; before the fix, `drain`
    // searched for `\n\n`, which does not occur in `\r\n\r\n` at all, so the
    // whole body reached `eof` as one event and the turn failed with
    // `trailing characters at line 2` — no output, no accounting, no
    // failover.
    for (framing, reframe) in [
        (
            "CRLF",
            (|s: &str| s.replace('\n', "\r\n")) as fn(&str) -> String,
        ),
        ("CR only", |s: &str| s.replace('\n', "\r")),
        ("CRLF line, LF blank", |s: &str| s.replace("\n\n", "\r\n\n")),
        ("LF line, CRLF blank", |s: &str| s.replace("\n\n", "\n\r\n")),
        // CONTROL: the LF framing every other test in this file uses, run
        // through the same loop. Without it a boundary scan that had stopped
        // finding `\n\n` would pass every assertion above.
        ("LF", |s: &str| s.to_string()),
    ] {
        let chunks = decode(&[
            &reframe(START),
            &reframe(&text(0, "Hel")),
            &reframe(&text(0, "lo")),
            &reframe(DELTA),
            &reframe(STOP),
        ])
        .unwrap_or_else(|error| {
            panic!("a legal {framing}-framed stream must decode, not fail the turn: {error}")
        });

        assert_eq!(
            chunks,
            vec![
                FrontierChunk::OutputText("Hel".into()),
                FrontierChunk::OutputText("lo".into()),
                FrontierChunk::Done {
                    input_tokens: 9_512,
                    cached_input_tokens: 9_000,
                    cache_read_source: CacheReadSource::Provider,
                    cache_write_tokens: 500,
                    output_tokens: 64,
                    reasoning_tokens: 0,
                    provider_reported_cost: None,
                    stop_reason: Some("end_turn".into()),
                },
            ],
            "{framing} framing"
        );
    }
}

/// A `\r` and the `\n` that completes it, arriving in different reads.
///
/// The one shape the boundary scan can get wrong in a way no whole-body test
/// sees: a chunk that ends on the `\r` of a `\r\n` line terminator. Treating
/// that `\r` as a complete terminator and the next chunk's `\n` as a second
/// one would split an ordinary line into a false event boundary, dropping
/// the `data:` line that followed it — silently, as an empty frame.
#[test]
fn a_line_terminator_split_across_two_reads_is_not_a_false_boundary() {
    let whole = format!("{START}{}{DELTA}{STOP}", text(0, "ok")).replace('\n', "\r\n");
    let mut decoder = SseDecoder::default();
    let mut chunks = Vec::new();
    // One byte at a time, so every `\r\n` in the body is split.
    for byte in whole.as_bytes() {
        decoder.feed(&[*byte]).unwrap();
        while let Some(chunk) = decoder.next_chunk() {
            chunks.push(chunk);
        }
    }
    decoder.eof().unwrap();
    while let Some(chunk) = decoder.next_chunk() {
        chunks.push(chunk);
    }
    assert_eq!(chunks[0], FrontierChunk::OutputText("ok".into()));
    assert!(
        matches!(
            chunks[1],
            FrontierChunk::Done {
                input_tokens: 9_512,
                output_tokens: 64,
                ..
            }
        ),
        "{chunks:?}"
    );
}

#[test]
fn nothing_after_message_stop_is_read() {
    // The provider has already said what the turn cost. A frame after the
    // terminal one cannot change it, and reading one would let an upstream
    // append output to a turn the log has already settled.
    let chunks = decode(&[START, DELTA, STOP, &text(0, "afterthought")]).unwrap();
    assert_eq!(chunks.len(), 1);
    assert!(matches!(chunks[0], FrontierChunk::Done { .. }));
}

const TEXT: &str = concat!(
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
    "\n\n"
);
/// The closing frames, which carry the output count and nothing about a
/// cache. Named apart from [`STOP`] above: that one is the bare
/// `message_stop` event, this one also carries the `message_delta` usage
/// frame the cache-read tests need ahead of it.
const CACHE_STOP: &str = concat!(
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":64}}"#,
    "\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
);

fn cache_read(chunks: &[FrontierChunk]) -> (u64, CacheReadSource) {
    chunks
        .iter()
        .find_map(|chunk| match chunk {
            FrontierChunk::Done {
                cached_input_tokens,
                cache_read_source,
                ..
            } => Some((*cached_input_tokens, *cache_read_source)),
            _ => None,
        })
        .expect("the stream carried an accounting frame")
}

fn stream(usage: &str) -> Vec<FrontierChunk> {
    let start = format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":\
         {{\"type\":\"message\",\"content\":[],\"usage\":{usage}}}}}\n\n"
    );
    decode(&[&start, TEXT, CACHE_STOP]).expect("the stream decodes")
}

/// `#[serde(default)]` plus an assembler that only overwrote on a positive
/// value collapsed an explicit zero into an absent field.
#[test]
fn an_explicit_cache_zero_is_distinguishable_from_an_omitted_count() {
    let explicit_zero = stream(r#"{"input_tokens":120,"cache_read_input_tokens":0}"#);
    let omitted = stream(r#"{"input_tokens":120}"#);

    assert_eq!(cache_read(&explicit_zero), (0, CacheReadSource::Provider));
    assert_eq!(cache_read(&omitted), (0, CacheReadSource::Unreported));
    assert_ne!(cache_read(&explicit_zero), cache_read(&omitted));
}

/// An explicit `null` is silence, like an omitted field.
#[test]
fn a_null_cache_count_is_unreported_rather_than_zero() {
    assert_eq!(
        cache_read(&stream(
            r#"{"input_tokens":120,"cache_read_input_tokens":null}"#
        )),
        (0, CacheReadSource::Unreported)
    );
}

/// A sparse `message_delta` must not retract what the prelude reported.
///
/// The accounting arrives in two frames and only the first says anything
/// about a cache. A merge that reset the provenance per frame would
/// downgrade every real read to silence at the last event of every stream.
#[test]
fn a_sparse_update_keeps_the_provenance_the_prelude_reported() {
    assert_eq!(
        cache_read(&stream(
            r#"{"input_tokens":12,"cache_read_input_tokens":9000}"#
        )),
        (9_000, CacheReadSource::Provider),
        "the prelude reported the read; a later silent frame does not \
         unreport it"
    );
}
