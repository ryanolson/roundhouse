// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The `finding1` question, asked of the dialect that answers it the other way
//! round.
//!
//! `finding1_usage_enforcement.rs` asks whether a `FrontierClient` can discharge
//! `enforce_usage_reporting` from the one argument it receives. On the
//! OpenAI-compatible dialects the obligation is *additive*: a streaming request
//! that never asked for usage comes back with none, and the client's job is to
//! rewrite the body so it does.
//!
//! On `anthropic_messages` the obligation is the opposite shape and strictly
//! harder to notice. `enforce_usage_reporting` adds nothing here — the provider
//! reports unconditionally — and a client that read that as "nothing to do"
//! would be wrong in the one direction this codebase refuses to be wrong in:
//! the accounting arrives in **two** frames, `message_start` and the final
//! `message_delta`, and either one alone produces a record that looks complete.
//! Reading only the delta reports a 9 512-token prompt as zero input and no
//! cache reads, which is the quantity the whole product is judged on. Reading
//! only the prelude reports a finished answer as zero output.
//!
//! So this file is the analogue of finding1's claim, not a copy of it: the
//! dialect's promise (`reports_usage_before_completion`) is asserted, the
//! empty-enforcement fact is pinned so nobody "fixes" it by adding a field the
//! request schema forbids, and the fold is driven over a real socket with each
//! half of the accounting withheld in turn.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use futures::StreamExt;
use serde_json::json;

use roundhouse_core::control::{Secret, TurnCredential};
use roundhouse_core::routing::Target;
use roundhouse_fleet::anthropic_messages::AnthropicMessagesClient;
use roundhouse_fleet::{FrontierChunk, FrontierClient, FrontierQuote, WireProtocol};

/// `message_start` on a warm prefix: 12 fresh, 9 000 read, 500 written.
const START: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"type":"message","id":"msg_1","#,
    r#""role":"assistant","model":"claude-x","content":[],"stop_reason":null,"#,
    r#""stop_sequence":null,"usage":{"input_tokens":12,"cache_read_input_tokens":9000,"#,
    r#""cache_creation_input_tokens":500,"output_tokens":1}}}"#,
    "\n\n"
);

const TEXT: &str = concat!(
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
    "\n\n"
);

/// The final `message_delta`, which is the only frame carrying the output count.
const DELTA: &str = concat!(
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"#,
    r#""usage":{"output_tokens":64}}"#,
    "\n\n"
);

const STOP: &str = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

/// A mock upstream that answers every request with one canned stream.
///
/// Mounted at `/v1/messages` — the client's own `DEFAULT_MESSAGES_PATH` — so a
/// client that had lost the path would 404 here rather than pass.
async fn upstream(body: String) -> String {
    let app = Router::new()
        .route(
            "/v1/messages",
            post(|State(body): State<Arc<String>>| async move {
                ([("content-type", "text/event-stream")], body.to_string()).into_response()
            }),
        )
        .with_state(Arc::new(body));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn quote() -> FrontierQuote {
    FrontierQuote {
        target: Target::Frontier {
            provider: "anthropic".into(),
            model: "claude-x".into(),
        },
        wire_protocol: WireProtocol::AnthropicMessages,
        prompt: "<|user|>how many tokens did that turn bill?".into(),
        segment_boundaries: Vec::new(),
        prompt_cache_key: "sess_usage_contract".into(),
        expected_output_tokens: Some(512),
        credential: TurnCredential::Stored(
            Secret::api_key("sk-ant-api03-ZZZQQQ-usage-contract").expect("an ordinary API key"),
        ),
    }
}

/// Drive the real client over a canned stream and return what it yielded.
async fn dispatch(stream: &[&str]) -> Vec<FrontierChunk> {
    let base = upstream(stream.concat()).await;
    let client = AnthropicMessagesClient::with_bases(&base, &base).unwrap();
    client
        .execute(&quote())
        .await
        .expect("the mock answers 200")
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("none of these fixtures is a failure")
}

/// **The enforcement arm adds nothing, and the body is unchanged byte for
/// byte.**
///
/// Two assertions rather than one, because "returned an empty list" and "did not
/// touch the body" are different claims and the second is the one that matters
/// here: `CreateMessageParams` is `additionalProperties: false`, so a field
/// added by a well-meaning future edit to this arm would not be ignored by the
/// upstream — it would 400 every Anthropic turn.
#[test]
fn the_anthropic_arm_adds_nothing_and_leaves_the_request_untouched() {
    let original = json!({
        "model": "claude-x",
        "max_tokens": 8192,
        "stream": true,
        "messages": [{ "role": "user", "content": [{ "type": "text", "text": "hi" }] }],
    });

    let mut body = original.clone();
    let added = WireProtocol::AnthropicMessages.enforce_usage_reporting(&mut body);
    assert_eq!(added, Vec::<&str>::new());
    assert_eq!(
        body, original,
        "the request schema admits no extra property"
    );

    // CONTROL: the same body under the dialect where the obligation is real.
    // Without it, this test also passes on an `enforce_usage_reporting` that had
    // become a no-op for every dialect — which is finding1's defect restored.
    let mut chat = original.clone();
    assert_eq!(
        WireProtocol::OpenAiChatCompletions.enforce_usage_reporting(&mut chat),
        vec!["stream_options"]
    );
    assert_eq!(chat["stream_options"]["include_usage"], json!(true));
}

/// **This dialect is the only one that promises usage before completion, and
/// that promise is what obliges a client to fold two frames.**
///
/// `reports_usage_before_completion` is a `matches!` rather than a `match`, so
/// nothing makes a new dialect answer it deliberately. Pinning all three arms
/// here is what turns a wrong answer into a red test rather than into a client
/// that reads one frame.
#[test]
fn only_this_dialect_reports_usage_before_the_stream_completes() {
    assert!(WireProtocol::AnthropicMessages.reports_usage_before_completion());
    assert!(!WireProtocol::OpenAiResponses.reports_usage_before_completion());
    assert!(!WireProtocol::OpenAiChatCompletions.reports_usage_before_completion());
}

/// **Both usage events are needed, and each half alone produces a record that
/// looks fine.**
///
/// The whole point of the file. Three streams differing only in which
/// accounting frame is present, driven through the shipped client over a real
/// socket, so the claim is about what this build actually records rather than
/// about what its decoder was written to do.
#[tokio::test]
async fn the_accounting_is_whole_only_when_both_usage_events_are_folded() {
    // PROBE: both frames present. Anthropic's three input counters are disjoint
    // and roundhouse's `input_tokens` is their total.
    assert_eq!(
        dispatch(&[START, TEXT, DELTA, STOP]).await.last(),
        Some(&FrontierChunk::Done {
            input_tokens: 9_512,
            cached_input_tokens: 9_000,
            cache_write_tokens: 500,
            output_tokens: 64,
            reasoning_tokens: 0,
            provider_reported_cost: None,
        })
    );

    // HALF ONE: the prelude withheld. A client that read only `message_delta` —
    // the natural choice, since it is the frame that says the turn finished —
    // records a 9 512-token prompt as nothing at all. Here the fold refuses
    // instead: no prelude means no `Done`, so the engine substitutes its own
    // estimate and *marks* it, and the gap stays visible as a gap. A `Done`
    // with `input_tokens: 0` would fold into the dashboard as zero tokens for
    // zero dollars, which is indistinguishable from a saving.
    let delta_only = dispatch(&[TEXT, DELTA, STOP]).await;
    assert_eq!(delta_only, vec![FrontierChunk::OutputText("hi".into())]);

    // HALF TWO: the final delta withheld — an upstream that ended the turn
    // without ever reporting the output count. **Re-aimed by the F6 fix, and
    // this is the finding's whole substance.** This assertion used to pin a
    // `Done` carrying the real input side and `output_tokens: 0`, on the
    // reasoning that the half that arrived should be recorded and the half that
    // did not should be zero rather than guessed. That reasoning has a hole the
    // engine falls straight through: a `Done` is booked as
    // `Accounting::Reported` unconditionally, so the record did not say "half of
    // this was never measured" — it said "the provider reported this turn's
    // output as zero", which prices a real streamed answer on a hosted model at
    // zero dollars and is indistinguishable on the dashboard from a saving.
    //
    // So the rule is now symmetric with HALF ONE: nothing reported is not
    // written down as zero, on *either* axis, and the engine's
    // estimated-and-marked path runs for the whole turn. The measured input
    // counts are lost with it — the deliberate price, paid because an estimate
    // is marked everywhere it is read and a fabricated zero is not.
    let start_only = dispatch(&[START, TEXT, STOP]).await;
    assert_eq!(
        start_only,
        vec![FrontierChunk::OutputText("hi".into())],
        "a turn whose output count no frame ever reported must reach the engine \
         unaccounted, not as a provider-reported zero"
    );

    // And the two halves really are different frames rather than one frame read
    // twice: the complete stream's `Done` differs from each partial one.
    assert_ne!(dispatch(&[START, TEXT, DELTA, STOP]).await, start_only);
    assert_ne!(dispatch(&[START, TEXT, DELTA, STOP]).await, delta_only);
    // The symmetry itself, pinned: which half went missing does not change the
    // answer. A rule that refused one direction and fabricated a zero in the
    // other is exactly the shape F6 found, and it would read as reasonable in
    // every place but the dashboard.
    assert_eq!(
        start_only, delta_only,
        "a missing input count and a missing output count are the same fact -- \
         nobody reported this turn -- and must produce the same record"
    );
}
