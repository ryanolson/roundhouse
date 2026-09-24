// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `openai_responses::stream` under test.
//!
//! One module rather than the two this file used to carry (`tests` and a
//! `cache_read_tests` appended after it): both drove the same `decode`
//! helper, and splitting the cache-read cases into a second module bought
//! nothing but a second `use` of it — merged, `decode` needs no visibility
//! wider than this file.

use super::*;
use roundhouse_core::event::CacheReadSource;

/// M17 review F5: this file's `function_call` doc (stream.rs:302-304)
/// reads an absent `namespace` as "this tool has no server" — a
/// *definite* fact about the call. `roundhouse_core::item::ItemContent`'s
/// doc for the same stored field (item.rs:78-79) reads a stored `None`
/// as the opposite: "not \"no namespace\"; it is \"this client does not
/// spell one\"" — an *unknown*, not a fact. `prefix_admission.rs`'s
/// `same_namespace` (R-N8) justifies matching a stored `None` against
/// any claimed namespace by citing only the item.rs reading (a pre-M17
/// record, or the Messages surface's flat spelling) — never the fleet's
/// "no server" reading. Both doc comments are reachable as data via
/// `include_str!` (rustdoc text is not otherwise inspectable at
/// runtime); this test fails while both readings are asserted at once,
/// which is the on-its-face contradiction the finding names.
///
/// **M18, H6: the conjunction alone is not a guard once one side of it
/// is fixed for good.** The M17 review's fix reworded this file's
/// `function_call` doc to stop asserting "no server" (it now points at
/// item.rs's reading instead — see the doc above `function_call`), which
/// makes `fleet_reads_absence_as_a_fact` `false` forever: the `&&` below
/// can never be true again, so `!(false && anything)` holds whatever
/// item.rs's doc says from here on. A reword of item.rs's namespace-field
/// doc away from "this client does not spell one" would pass this test
/// in perfect silence. The control assertion after it is the fix: it
/// pins the item.rs literal on its own, unconditionally, so a reword
/// fails a named assertion instead of emptying a conjunction nobody is
/// watching.
#[test]
fn fleet_and_core_namespace_docs_do_not_read_absence_oppositely() {
    let fleet_doc = include_str!("../stream.rs");
    let core_doc = include_str!("../../../../roundhouse-core/src/item.rs");

    let fleet_reads_absence_as_a_fact =
        fleet_doc.contains("means \"this tool has no server\", not \"nobody said\"");
    let core_reads_absence_as_unknown =
        core_doc.contains("is not \"no namespace\"; it is \"this client does not spell");

    assert!(
        !(fleet_reads_absence_as_a_fact && core_reads_absence_as_unknown),
        "stream.rs's function_call doc and item.rs's ItemContent::ToolCall::namespace \
         doc give opposite readings of a stored/absent namespace: the fleet decoder \
         reads absence as the definite \"this tool has no server\", while the core item \
         doc reads it as the unknown \"this client does not spell one\" -- R-N8's \
         stored-None-agrees-with-any-claim admission rule (prefix_admission.rs) only \
         holds under the core reading"
    );

    // The control (M18, H6). Sliced up to (not including) `mod tests`,
    // the same pattern `item.rs`'s own
    // `canonical_arguments_doc_names_current_join_call_shape` uses and
    // for the identical reason: the whole-file version is a tautology,
    // since this very assertion's message quotes the phrase it searches
    // for, and `include_str!` reading the whole file would find that
    // quotation however the doc comment above `namespace` was mutated.
    let core_src_before_tests = core_doc.split("\n#[cfg(test)]").next().unwrap();
    assert!(
        core_src_before_tests
            .contains("is not \"no namespace\"; it is \"this client does not spell"),
        "item.rs's ItemContent::ToolCall::namespace doc no longer reads a stored `None` as \
         \"this client does not spell one\" -- R-N8's stored-None-agrees-with-any-claim \
         admission rule (prefix_admission.rs) is sound only under that reading, and the \
         assertion above this one cannot catch a reword here on its own: the fleet doc's \
         half of the contradiction it checks was fixed for good in the M17 review, so the \
         `&&` above never fires again regardless of what this file says"
    );
}

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

const COMPLETED: &str = concat!(
    "event: response.completed\n",
    r#"data: {"type":"response.completed","response":{"usage":{"#,
    r#""input_tokens":120,"input_tokens_details":{"cached_tokens":100},"#,
    r#""output_tokens":30,"output_tokens_details":{"reasoning_tokens":12}}}}"#,
    "\n\n"
);

#[test]
fn a_responses_stream_becomes_deltas_and_one_accounting_frame() {
    let chunks = decode(&[
        "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
        COMPLETED,
    ])
    .unwrap();

    assert_eq!(
        chunks,
        vec![
            FrontierChunk::OutputText("Hel".into()),
            FrontierChunk::OutputText("lo".into()),
            FrontierChunk::Done {
                input_tokens: 120,
                cached_input_tokens: 100,
                cache_read_source: CacheReadSource::Provider,
                cache_write_tokens: 0,
                output_tokens: 30,
                reasoning_tokens: 12,
                // OpenAI's own Responses usage object carries no `cost`,
                // and absent is `None` rather than zero — see
                // `usage_chunk`.
                provider_reported_cost: None,
                // Nor a stop reason: this wire names one only when a turn
                // ended early, and `COMPLETED` did not.
                stop_reason: None,
            },
        ],
        "the cached count is the quantity the whole system exists to \
         maximize, and it only arrives if this reads the field"
    );
}

#[test]
fn a_frame_split_across_reads_is_reassembled() {
    // The ordinary case on a real socket, and the one a naive
    // parse-per-read gets wrong: a chunk boundary anywhere, including
    // mid-JSON and mid-separator.
    let split = decode(&[
        "event: response.outp",
        "ut_text.delta\ndata: {\"type\":\"response.output_te",
        "xt.delta\",\"delta\":\"ok\"}\n",
        "\n",
        &COMPLETED[..40],
        &COMPLETED[40..],
    ])
    .unwrap();
    assert_eq!(split[0], FrontierChunk::OutputText("ok".into()));
    assert!(matches!(split[1], FrontierChunk::Done { .. }));

    // A multi-byte character straddling a read is text, not a failure.
    let text = "日本語";
    let frame =
        format!("data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{text}\"}}\n\n");
    let bytes = frame.as_bytes();
    let mut decoder = SseDecoder::default();
    decoder.feed(&bytes[..40]).unwrap();
    decoder.feed(&bytes[40..]).unwrap();
    assert_eq!(
        decoder.next_chunk(),
        Some(FrontierChunk::OutputText(text.into()))
    );
}

/// The same defect F5 found next door, in the file that decoder copied.
///
/// No review finding named this one — the Anthropic decoder is where a CRLF
/// stream was caught — but `drain` here carried the identical
/// `find("\n\n")` scan under the identical comment claiming it handled CRLF,
/// and the two files are deliberate mirrors. The consequence is the same and
/// arrives on a wire roundhouse has been dispatching since M8: a legal
/// CRLF-framed body has no `\n\n` byte pair anywhere, so no boundary is ever
/// found, the whole body reaches `eof` as one event, and several `data:`
/// lines joined by newlines fail `serde_json` — one error, no output, no
/// accounting, for a turn the provider served correctly.
#[test]
fn crlf_and_cr_framed_events_are_split_by_drain_the_same_as_lf() {
    let delta = "event: response.output_text.delta\n\
                 data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n";
    for (framing, reframe) in [
        (
            "CRLF",
            (|s: &str| s.replace('\n', "\r\n")) as fn(&str) -> String,
        ),
        ("CR only", |s: &str| s.replace('\n', "\r")),
        ("CRLF line, LF blank", |s: &str| s.replace("\n\n", "\r\n\n")),
        ("LF line, CRLF blank", |s: &str| s.replace("\n\n", "\n\r\n")),
        // CONTROL: the LF framing every other test here uses, through the
        // same loop, so a scan that had stopped finding `\n\n` cannot pass.
        ("LF", |s: &str| s.to_string()),
    ] {
        let chunks = decode(&[&reframe(delta), &reframe(COMPLETED)])
            .unwrap_or_else(|error| panic!("{framing} framing must decode: {error}"));
        assert_eq!(
            chunks,
            vec![
                FrontierChunk::OutputText("ok".into()),
                FrontierChunk::Done {
                    input_tokens: 120,
                    cached_input_tokens: 100,
                    cache_read_source: CacheReadSource::Provider,
                    cache_write_tokens: 0,
                    output_tokens: 30,
                    reasoning_tokens: 12,
                    provider_reported_cost: None,
                    stop_reason: None,
                },
            ],
            "{framing} framing"
        );
    }
}

#[test]
fn a_stream_that_never_completes_yields_no_accounting_frame() {
    // The engine estimates for a provider that reported nothing and marks
    // the estimate. A synthesized zero-token `Done` here would fold as zero
    // tokens for zero dollars, which reads as a saving.
    let chunks =
        decode(&["data: {\"type\":\"response.output_text.delta\",\"delta\":\"half an answ\"}\n\n"])
            .unwrap();
    assert_eq!(
        chunks,
        vec![FrontierChunk::OutputText("half an answ".into())]
    );

    // And a completion with no usage object is the same case, not a zeroed
    // one.
    let unaccounted =
        decode(&["data: {\"type\":\"response.completed\",\"response\":{}}\n\n"]).unwrap();
    assert!(unaccounted.is_empty());
}

#[test]
fn an_upstream_failure_is_an_error_and_not_a_short_stream() {
    // PROBE: both terminal failure shapes the Responses wire uses.
    for frame in [
        r#"data: {"type":"response.failed","response":{"error":{"message":"quota exceeded"}}}"#,
        r#"data: {"type":"error","message":"quota exceeded"}"#,
    ] {
        let error = decode(&[&format!("{frame}\n\n")]).expect_err("must be an error");
        assert!(
            matches!(&error, FrontierError::Upstream(message) if message.contains("quota exceeded")),
            "{error}"
        );
    }

    // CONTROL: an unparseable body is also an error rather than a silently
    // empty turn -- and it names what happened, because "the upstream sent
    // something we could not read" and "the model said nothing" are
    // different operational problems.
    let garbage = decode(&["data: not json at all\n\n"]).expect_err("must be an error");
    assert!(garbage.to_string().contains("unparseable"), "{garbage}");

    // CONTROL: a keep-alive comment and an unknown frame type are skipped,
    // so the strictness above is about failures and not about novelty.
    assert!(
        decode(&[": keep-alive\n\ndata: {\"type\":\"response.in_progress\"}\n\n"])
            .unwrap()
            .is_empty()
    );
}

/// **P3: an OpenRouter-shaped stream parses, keep-alives and all.**
///
/// OpenRouter injects `: OPENROUTER PROCESSING` comment lines into the SSE
/// body (`openrouter-api-surface.md` Q5.1, live 2026-08-24), and a
/// line-oriented parser that tried to JSON-decode one would fail an
/// otherwise perfect turn — at an interval that depends on how long the
/// upstream took, so it would fail *the slow turns* and look like a
/// timeout.
///
/// Two placements, because they exercise different code. A comment as its
/// own event block reaches `decode_event` with no `data:` line at all and
/// is discarded by the empty-payload check; a comment *inside* an event
/// block has to be skipped by the line loop while the block's real `data:`
/// line still decodes. Only the first was covered before, by the
/// `": keep-alive"` control in the test above.
#[test]
fn an_openrouter_shaped_stream_with_comment_keepalives_parses() {
    let chunks = decode(&[
        ": OPENROUTER PROCESSING\n\n",
        // Interleaved: the comment shares the block with the payload.
        concat!(
            ": OPENROUTER PROCESSING\n",
            r#"data: {"type":"response.output_text.delta","delta":"kimi"}"#,
            "\n\n",
        ),
        ": OPENROUTER PROCESSING\n\n",
        concat!(
            r#"data: {"type":"response.completed","response":{"usage":{"#,
            r#""input_tokens":1200,"input_tokens_details":{"cached_tokens":900},"#,
            r#""output_tokens":64,"output_tokens_details":{"reasoning_tokens":8},"#,
            // The OpenRouter extension: dollars beside the counts.
            r#""cost":0.00421,"cost_details":{"upstream_inference_cost":0.004}}}}"#,
            "\n\n",
        ),
    ])
    .expect("a keep-alive must never fail a turn");

    assert_eq!(
        chunks,
        vec![
            FrontierChunk::OutputText("kimi".into()),
            FrontierChunk::Done {
                input_tokens: 1200,
                cached_input_tokens: 900,
                cache_read_source: CacheReadSource::Provider,
                cache_write_tokens: 0,
                output_tokens: 64,
                reasoning_tokens: 8,
                // The number the reconciliation rung will compare our
                // token-priced figure against. Carried, never added to the
                // counts beside it.
                provider_reported_cost: Some(0.00421),
                stop_reason: None,
            },
        ]
    );
}

/// The control that stops the assertion above being about `cost` existing
/// rather than about it being *read*: the identical stream with the field
/// absent decodes the identical counts and reports no price.
///
/// Zero would be the tempting default and it is the one answer that is
/// wrong in a way nobody sees — a provider that reports no price is not a
/// provider that served the turn for free, and a reconciliation view fed
/// zeroes would report perfect agreement with a bill it never read.
#[test]
fn a_usage_object_without_cost_reports_no_price_rather_than_a_free_turn() {
    let chunks = decode(&[COMPLETED]).unwrap();
    assert_eq!(
        chunks,
        vec![FrontierChunk::Done {
            input_tokens: 120,
            cached_input_tokens: 100,
            cache_read_source: CacheReadSource::Provider,
            cache_write_tokens: 0,
            output_tokens: 30,
            reasoning_tokens: 12,
            provider_reported_cost: None,
            stop_reason: None,
        }]
    );
}

/// **The tool call, in the shape the conformance oracle builds it.**
///
/// Copied structurally from codex's own test helper
/// (`core/tests/common/responses.rs::ev_function_call` @ `6344a65`): a
/// `response.output_item.done` whose item is a `function_call` with
/// `call_id`, `name` and a complete `arguments` string. The `added` frame
/// that precedes it in a real stream carries `"arguments": ""`, and the
/// argument deltas between them are what the oracle's parser explicitly does
/// not read — so both appear here, and neither may produce a chunk.
#[test]
fn a_function_call_is_read_once_from_the_done_item_and_not_from_its_deltas() {
    let chunks = decode(&[
        concat!(
            r#"data: {"type":"response.output_item.added","item":{"#,
            r#""type":"function_call","id":"fc_1","call_id":"call_1","#,
            r#""name":"shell","arguments":"","status":"in_progress"}}"#,
            "\n\n",
        ),
        concat!(
            r#"data: {"type":"response.function_call_arguments.delta","#,
            r#""item_id":"fc_1","delta":"{\"command\":"}"#,
            "\n\n",
        ),
        concat!(
            r#"data: {"type":"response.output_item.done","item":{"#,
            r#""type":"function_call","call_id":"call_1","name":"shell","#,
            r#""arguments":"{\"command\": [\"ls\", \"-l\"]}"}}"#,
            "\n\n",
        ),
        COMPLETED,
    ])
    .expect("a function-call turn must decode");

    assert_eq!(
        chunks,
        vec![
            FrontierChunk::ToolCall {
                // `call_id`, not `id`: `id` names the *item*, `call_id`
                // names the call, and only the second is what a
                // `function_call_output` is paired on.
                id: "call_1".into(),
                name: "shell".into(),
                namespace: None,
                // The wire's own string, with its spacing, moved rather than
                // parsed — see `function_call`.
                arguments: r#"{"command": ["ls", "-l"]}"#.into(),
            },
            FrontierChunk::Done {
                input_tokens: 120,
                cached_input_tokens: 100,
                cache_read_source: CacheReadSource::Provider,
                cache_write_tokens: 0,
                output_tokens: 30,
                reasoning_tokens: 12,
                provider_reported_cost: None,
                // This wire names no reason for a turn that ended normally,
                // and a tool-use turn is one of those: the call above is the
                // signal, not a synthesized word.
                stop_reason: None,
            },
        ],
        "exactly one call: the `added` placeholder and the argument delta \
         must not each produce one"
    );
}

/// **R-N6: an upstream's `namespace` reaches the chunk, and its absence
/// reaches it as `None` rather than as a dropped call.**
///
/// The gap this closes had no test because it had no field: a model asking
/// for one of roundhouse's own MCP tools had its namespace dropped here,
/// was stored bare, and was re-emitted to a codex client with no
/// `namespace` for that client's exact `ToolName { name, namespace }`
/// lookup to resolve — a round trip that never worked and that nothing went
/// red about.
///
/// The negative half is the one that would go wrong quietly. `namespace` is
/// optional on this wire (a plain function tool sends none), so reading it
/// with the same `?` the three required fields use would turn every
/// non-MCP tool call into a silently dropped item — a turn that called a
/// tool arriving as a turn that called nothing.
#[test]
fn a_namespaced_function_call_carries_its_namespace_and_a_bare_one_carries_none() {
    for (item, name, expected, why) in [
        (
            concat!(
                r#"{"type":"function_call","call_id":"call_1","name":"status","#,
                r#""namespace":"mcp__roundhouse","arguments":"{}"}"#,
            ),
            "status",
            Some("mcp__roundhouse"),
            "the model asked for an MCP tool and named the server it is on",
        ),
        (
            r#"{"type":"function_call","call_id":"call_1","name":"shell","arguments":"{}"}"#,
            "shell",
            None,
            "a plain function tool has no server and sends no field",
        ),
    ] {
        let done = format!("data: {{\"type\":\"response.output_item.done\",\"item\":{item}}}\n\n");
        let chunks = decode(&[&done, COMPLETED]).unwrap_or_else(|error| panic!("{why}: {error:?}"));

        assert_eq!(
            chunks.first(),
            Some(&FrontierChunk::ToolCall {
                id: "call_1".into(),
                name: name.into(),
                namespace: expected.map(str::to_string),
                arguments: "{}".into(),
            }),
            "{why}"
        );
    }
}

/// Output items that are not function calls, and function calls missing a
/// field a client would need, both yield nothing — and neither fails the
/// turn.
///
/// A `message` item arrives on `response.output_item.done` on *every*
/// ordinary turn, so treating an unrecognised item as an error would fail
/// every turn this decoder has ever served. A call with no `call_id` cannot
/// be paired with its result and one with no `name` names no tool; handing
/// either to a client fails somewhere that says nothing about where the
/// value went missing.
#[test]
fn an_output_item_that_is_not_a_usable_function_call_yields_nothing() {
    for (item, why) in [
        (
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello"}]}"#,
            "the ordinary assistant message every turn ends with",
        ),
        (
            r#"{"type":"web_search_call","status":"completed"}"#,
            "a server-side tool this build does not model",
        ),
        (
            r#"{"type":"function_call","name":"shell","arguments":"{}"}"#,
            "no call_id: nothing could pair the result",
        ),
        (
            r#"{"type":"function_call","call_id":"c","arguments":"{}"}"#,
            "no name: it names no tool",
        ),
        (
            r#"{"type":"function_call","call_id":"c","name":"shell"}"#,
            "no arguments at all",
        ),
        (
            r#"{"type":"function_call","call_id":"c","name":"shell","arguments":{"a":1}}"#,
            "arguments as an object rather than the wire's JSON string",
        ),
    ] {
        let chunks = decode(&[
            &format!("data: {{\"type\":\"response.output_item.done\",\"item\":{item}}}\n\n"),
            COMPLETED,
        ])
        .unwrap_or_else(|error| panic!("{why} must not fail the turn: {error}"));
        assert_eq!(chunks.len(), 1, "{why}: {chunks:?}");
        assert!(matches!(chunks[0], FrontierChunk::Done { .. }), "{why}");
    }
}

/// The one place this wire does name a reason, read.
///
/// `incomplete_details.reason` is the Responses spelling of "the answer was
/// cut off", and it is the same fact the Messages wire spells
/// `stop_reason: max_tokens`. Without it a truncated turn is
/// indistinguishable from a complete one everywhere downstream — the defect
/// M11.1's F1 named on the other dialect.
#[test]
fn an_incomplete_reason_on_the_completion_frame_is_carried() {
    let chunks = decode(&[concat!(
        r#"data: {"type":"response.completed","response":{"#,
        r#""incomplete_details":{"reason":"max_output_tokens"},"#,
        r#""usage":{"input_tokens":10,"output_tokens":4}}}"#,
        "\n\n",
    )])
    .unwrap();
    assert!(
        matches!(
            &chunks[0],
            FrontierChunk::Done { stop_reason: Some(reason), .. }
                if reason == "max_output_tokens"
        ),
        "{chunks:?}"
    );

    // CONTROL: the ordinary completion names none, so the assertion above is
    // about the field being *read* rather than about a constant.
    assert!(matches!(
        decode(&[COMPLETED]).unwrap()[0],
        FrontierChunk::Done {
            stop_reason: None,
            ..
        }
    ));
}

#[test]
fn an_upstream_that_never_ends_an_event_is_abandoned_rather_than_buffered() {
    // The body is a remote party's. Without the bound this grows until the
    // process dies, which is a denial of service delivered by a provider.
    let mut decoder = SseDecoder::default();
    let filler = "x".repeat(64 * 1024);
    let error = loop {
        match decoder.feed(filler.as_bytes()) {
            Ok(()) => continue,
            Err(error) => break error,
        }
    };
    assert!(error.to_string().contains("no event boundary"), "{error}");

    // CONTROL: a payload comfortably under the bound is decoded, so the
    // limit is about an unterminated stream and not about size.
    let big = "y".repeat(4096);
    let chunks = decode(&[&format!(
        "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{big}\"}}\n\n"
    )])
    .unwrap();
    assert_eq!(chunks, vec![FrontierChunk::OutputText(big)]);
}

/// The cache-read half of the accounting frame.
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

fn completed(usage: &str) -> Vec<FrontierChunk> {
    decode(&[&format!(
        "event: response.completed\ndata: {{\"type\":\"response.completed\",\
         \"response\":{{\"usage\":{usage}}}}}\n\n"
    )])
    .expect("the frame decodes")
}

/// An explicit zero and an absent details object are different answers.
///
/// Before this the count came through an `unwrap_or(0)` that spent the
/// distinction, so a silent upstream divided as a measured miss.
#[test]
fn an_explicit_cache_zero_is_distinguishable_from_an_omitted_count() {
    let explicit_zero = completed(
        r#"{"input_tokens":120,"input_tokens_details":{"cached_tokens":0},"output_tokens":30}"#,
    );
    let omitted = completed(r#"{"input_tokens":120,"output_tokens":30}"#);

    assert_eq!(cache_read(&explicit_zero), (0, CacheReadSource::Provider));
    assert_eq!(cache_read(&omitted), (0, CacheReadSource::Unreported));
    assert_ne!(cache_read(&explicit_zero), cache_read(&omitted));
}

/// A null or unparseable count is silence rather than zero.
///
/// The shapes a gateway produces when it rewrites a body it does not fully
/// understand. `as_u64` refuses each, and the refusal has to reach the
/// chunk.
#[test]
fn a_null_or_malformed_cache_count_is_unreported_rather_than_zero() {
    for usage in [
        r#"{"input_tokens":120,"input_tokens_details":null,"output_tokens":30}"#,
        r#"{"input_tokens":120,"input_tokens_details":{"cached_tokens":null},"output_tokens":30}"#,
        r#"{"input_tokens":120,"input_tokens_details":{"cached_tokens":"100"},"output_tokens":30}"#,
        r#"{"input_tokens":120,"input_tokens_details":{"cached_tokens":-5},"output_tokens":30}"#,
    ] {
        assert_eq!(
            cache_read(&completed(usage)),
            (0, CacheReadSource::Unreported),
            "a count that will not parse is not a zero: {usage}"
        );
    }
}

/// The control: a real count is still read, and still a provider report.
#[test]
fn a_nonzero_cache_read_is_still_a_provider_report() {
    let chunks = completed(
        r#"{"input_tokens":120,"input_tokens_details":{"cached_tokens":100},"output_tokens":30}"#,
    );
    assert_eq!(cache_read(&chunks), (100, CacheReadSource::Provider));
}
