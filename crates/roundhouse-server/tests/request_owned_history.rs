// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract: a request that carries its whole conversation owns it.
//!
//! Both serve surfaces resend the entire conversation on every turn. What the
//! client sent is therefore the conversation, and the prompt this deployment
//! builds is exactly that — nothing the client left out is put back.
//!
//! Two kinds of content used to come back. **Omitted configuration**: a turn
//! that sends no system block left the stored run at the head of the prompt, so
//! a client that dropped its instructions still got them. **Interrupted
//! output**: a dispatch that died mid-answer commits the bytes it produced, the
//! client's stream threw that answer away, and the next prompt carried a partial
//! the client has never seen. Prefix admission already leaves both out of what a
//! claim is *checked* against; this suite is about what the model is *sent*.
//!
//! Each case has its control, and the controls are what keep the rule narrow:
//! a client that resends its instructions still gets them, a client that
//! resends the interrupted text still gets it, and the native session API —
//! where a request names a session and sends only new input — still continues
//! from the log, because there is no claim to be authoritative.
//!
//! The evidence is the prompt the provider was handed, not the log: the log
//! still holds every item it held before, which is what makes "the prompt is
//! the request's conversation" a statement about the prompt alone.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Request, StatusCode};
use futures::StreamExt as _;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::CacheReadSource;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::interject::{Interjection, InterjectionContext, Interjector};
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::store::MemoryStore;
use roundhouse_core::validate::Objective;
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierError, FrontierQuote, FrontierStream,
    StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::test_support::{engine_over_echo, frontier_spec, single_model_catalog};
use roundhouse_server::{
    Admission, ControlPlane, Conversations, Engine, EngineConfig, messages_router, responses_router,
};

/// What the provider answers when it answers at all.
const ANSWER: &str = "the whole answer";
/// What the dying dispatch streams before it breaks.
const PARTIAL: &str = "half of an answer nobody saw";
/// The instruction block a second turn may or may not resend.
const SYSTEM: &str = "SYSTEM_SENTINEL_only_if_the_client_sent_it";
const FIRST_QUESTION: &str = "FIRST_QUESTION_sentinel";
const SECOND_QUESTION: &str = "SECOND_QUESTION_sentinel";

const PROVIDER: &str = "request-owned";

// ---------------------------------------------------------------------------
// The provider double
// ---------------------------------------------------------------------------

/// Records every prompt it is handed, and optionally dies mid-answer on its
/// first call.
///
/// Recording rather than asserting inside: every case below is about the
/// *second* dispatch, and a double that could only check one prompt would have
/// to know which call it was looking at.
struct PromptRecorder {
    prompts: Mutex<Vec<String>>,
    calls: AtomicUsize,
    /// Whether the first dispatch streams [`PARTIAL`] and then breaks.
    dies_first: bool,
}

impl PromptRecorder {
    fn answering() -> Self {
        Self {
            prompts: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            dies_first: false,
        }
    }

    fn dying_once() -> Self {
        Self {
            prompts: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            dies_first: true,
        }
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts
            .lock()
            .expect("the recorder is never held across a panic")
            .clone()
    }

    /// The prompt of dispatch `index`, counted from zero.
    fn prompt(&self, index: usize) -> String {
        let prompts = self.prompts();
        assert!(
            prompts.len() > index,
            "expected at least {} dispatches, saw {}",
            index + 1,
            prompts.len()
        );
        prompts[index].clone()
    }
}

#[async_trait]
impl FrontierClient for PromptRecorder {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.prompts
            .lock()
            .expect("the recorder is never held across a panic")
            .push(quote.prompt.clone());
        if self.dies_first && self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(futures::stream::iter([
                Ok(FrontierChunk::OutputText(PARTIAL.to_string())),
                Err(FrontierError::Upstream("the provider broke".into())),
            ])
            .boxed());
        }
        Ok(FrontierChunk::whole_response(
            ANSWER.to_string(),
            quote.prompt.len() as u64,
            0,
            CacheReadSource::Provider,
            ANSWER.len() as u64,
            0,
        ))
    }
}

/// The canonical shape of an instruction block, as both surfaces' wire layers
/// produce one: a leading developer item with no response stamp.
fn configuration(text: &str) -> Item {
    Item {
        role: Role::Developer,
        content: ItemContent::Text { text: text.into() },
        response_id: None,
    }
}

fn catalog() -> StaticFrontierCatalog {
    single_model_catalog(frontier_spec(
        PROVIDER,
        "m",
        WireProtocol::AnthropicMessages,
    ))
}

fn engine_over(
    store: &Arc<MemoryStore>,
    recorder: &Arc<PromptRecorder>,
) -> Engine<MemoryStore, ByteTokenizer> {
    engine_over_echo(
        Arc::clone(store),
        catalog(),
        Arc::clone(recorder) as Arc<dyn FrontierClient>,
        EngineConfig::default(),
    )
}

/// The Messages surface over a recording provider.
fn messages_surface(recorder: Arc<PromptRecorder>) -> Router {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(engine_over(&store, &recorder));
    messages_router(
        ControlPlane::open(),
        engine,
        store,
        Arc::new(Conversations::new()),
    )
}

/// The Responses surface over a recording provider.
fn responses_surface(recorder: Arc<PromptRecorder>) -> Router {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(engine_over(&store, &recorder));
    responses_router(
        ControlPlane::open(),
        engine,
        store,
        Arc::new(Conversations::new()),
    )
}

async fn post(app: &Router, uri: &str, body: &Value) -> StatusCode {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(body).expect("a JSON body")))
                .expect("a well-formed request"),
        )
        .await
        .expect("the router answers");
    let status = response.status();
    // Drained to completion: the turn runs inside the stream, so a test that
    // dropped the body would assert against a dispatch that had not happened.
    let _ = response
        .into_body()
        .collect()
        .await
        .expect("the stream completes")
        .to_bytes();
    status
}

// ---------------------------------------------------------------------------
// Omitted configuration
// ---------------------------------------------------------------------------

/// One Messages body, with or without its system block.
fn messages_body(system: Option<&str>, messages: Value) -> Value {
    let mut body = json!({
        "model": "claude-opus-5",
        "max_tokens": 1024,
        "stream": true,
        "metadata": { "user_id": "request-owned-history" },
        "messages": messages,
    });
    if let Some(system) = system {
        body["system"] = json!(system);
    }
    body
}

/// A turn that sends no system block gets a prompt with no system block.
///
/// The client re-derives its instructions from its own environment on every
/// invocation. One that sends none is not asking for the ones it sent an hour
/// ago; the request is the conversation.
#[tokio::test]
async fn an_omitted_instruction_block_stays_out_of_the_prompt() {
    let recorder = Arc::new(PromptRecorder::answering());
    let app = messages_surface(Arc::clone(&recorder));

    assert_eq!(
        post(
            &app,
            "/v1/messages",
            &messages_body(
                Some(SYSTEM),
                json!([{ "role": "user", "content": FIRST_QUESTION }]),
            ),
        )
        .await,
        StatusCode::OK
    );
    assert!(
        recorder.prompt(0).contains(SYSTEM),
        "the control: the turn that sent the instructions was given them"
    );

    assert_eq!(
        post(
            &app,
            "/v1/messages",
            &messages_body(
                None,
                json!([
                    { "role": "user", "content": FIRST_QUESTION },
                    { "role": "assistant", "content": ANSWER },
                    { "role": "user", "content": SECOND_QUESTION },
                ]),
            ),
        )
        .await,
        StatusCode::OK
    );
    let second = recorder.prompt(1);
    assert!(
        !second.contains(SYSTEM),
        "a turn that sent no instructions must not be given the stored run: {second}"
    );
    assert!(
        second.contains(FIRST_QUESTION) && second.contains(SECOND_QUESTION),
        "the rest of the client's conversation is still the prompt: {second}"
    );
}

/// The control: instructions the client resends are in the prompt.
#[tokio::test]
async fn resent_instructions_stay_in_the_prompt() {
    let recorder = Arc::new(PromptRecorder::answering());
    let app = messages_surface(Arc::clone(&recorder));

    post(
        &app,
        "/v1/messages",
        &messages_body(
            Some(SYSTEM),
            json!([{ "role": "user", "content": FIRST_QUESTION }]),
        ),
    )
    .await;
    post(
        &app,
        "/v1/messages",
        &messages_body(
            Some(SYSTEM),
            json!([
                { "role": "user", "content": FIRST_QUESTION },
                { "role": "assistant", "content": ANSWER },
                { "role": "user", "content": SECOND_QUESTION },
            ]),
        ),
    )
    .await;

    let second = recorder.prompt(1);
    assert!(
        second.contains(SYSTEM),
        "a client that resent its instructions is asking for them: {second}"
    );
}

/// One Responses body over `input` items alone.
///
/// The instruction run is a leading `developer` message rather than the
/// `instructions` field, because that field canonicalizes to a `system` item
/// and only a leading `developer` run is turn configuration — see
/// [`roundhouse_core::session::is_turn_configuration`]. Omitting a `system`
/// item is a *changed history*, which forks; omitting a configuration run is
/// what this suite is about.
fn responses_body(configuration: Option<&str>, messages: Vec<Value>) -> Value {
    let mut input: Vec<Value> = Vec::new();
    if let Some(text) = configuration {
        input.push(json!({ "type": "message", "role": "developer", "content": text }));
    }
    input.extend(messages);
    json!({
        "model": "gpt-5",
        "stream": true,
        "prompt_cache_key": "request-owned-history",
        "input": input,
    })
}

fn responses_message(role: &str, text: &str) -> Value {
    json!({ "type": "message", "role": role, "content": text })
}

/// The same rule on the other dialect.
#[tokio::test]
async fn an_omitted_configuration_run_stays_out_of_a_responses_prompt() {
    let recorder = Arc::new(PromptRecorder::answering());
    let app = responses_surface(Arc::clone(&recorder));

    assert_eq!(
        post(
            &app,
            "/v1/responses",
            &responses_body(
                Some(SYSTEM),
                vec![responses_message("user", FIRST_QUESTION)],
            ),
        )
        .await,
        StatusCode::OK,
        "the first turn is served"
    );
    assert!(recorder.prompt(0).contains(SYSTEM), "the control");

    assert_eq!(
        post(
            &app,
            "/v1/responses",
            &responses_body(
                None,
                vec![
                    responses_message("user", FIRST_QUESTION),
                    responses_message("assistant", ANSWER),
                    responses_message("user", SECOND_QUESTION),
                ],
            ),
        )
        .await,
        StatusCode::OK
    );
    let prompt = recorder.prompt(1);
    assert!(
        !prompt.contains(SYSTEM),
        "a Responses turn that sent no configuration must not be given the \
         stored run: {prompt}"
    );
    assert!(
        prompt.contains(SECOND_QUESTION),
        "and the client's own conversation is still the prompt: {prompt}"
    );
}

// ---------------------------------------------------------------------------
// Interrupted output
// ---------------------------------------------------------------------------

/// A partial the client's stream threw away stays out of the next prompt.
///
/// The dying dispatch commits what it produced so a successor can resume from
/// it. The client read the error frame and dropped the text, so its honest
/// retry does not contain it — and a prompt that put it back would be asking
/// the model to continue an answer the client has never seen.
#[tokio::test]
async fn an_interrupted_answer_the_client_never_saw_stays_out_of_the_prompt() {
    let recorder = Arc::new(PromptRecorder::dying_once());
    let app = messages_surface(Arc::clone(&recorder));

    let body = messages_body(None, json!([{ "role": "user", "content": FIRST_QUESTION }]));
    post(&app, "/v1/messages", &body).await;
    assert!(
        !recorder.prompt(0).contains(PARTIAL),
        "nothing had been produced when the first dispatch was made"
    );

    // The same request again: the client never saw the partial, so its retry is
    // byte-identical to what it sent before.
    post(&app, "/v1/messages", &body).await;
    let retry = recorder.prompt(1);
    assert!(
        !retry.contains(PARTIAL),
        "the retry must not be asked to continue an answer the client never \
         received: {retry}"
    );
    assert!(
        retry.contains(FIRST_QUESTION),
        "and it is still the client's own question: {retry}"
    );
}

/// The control: a client that does resend the interrupted text gets it.
///
/// This is what keeps the rule about the *request* rather than about partials:
/// the same stored item, claimed by the client, is ordinary history.
#[tokio::test]
async fn an_interrupted_answer_the_client_resends_stays_in_the_prompt() {
    let recorder = Arc::new(PromptRecorder::dying_once());
    let app = messages_surface(Arc::clone(&recorder));

    post(
        &app,
        "/v1/messages",
        &messages_body(None, json!([{ "role": "user", "content": FIRST_QUESTION }])),
    )
    .await;

    post(
        &app,
        "/v1/messages",
        &messages_body(
            None,
            json!([
                { "role": "user", "content": FIRST_QUESTION },
                { "role": "assistant", "content": PARTIAL },
                { "role": "user", "content": SECOND_QUESTION },
            ]),
        ),
    )
    .await;

    let second = recorder.prompt(1);
    assert!(
        second.contains(PARTIAL),
        "a client that kept the partial is continuing from it: {second}"
    );
}

// ---------------------------------------------------------------------------
// The policy, learning and validation readers
// ---------------------------------------------------------------------------

/// Records the conversation the interjection seam was handed, and the objective
/// derived from it.
///
/// **The seam is the one place every reader of conversation content can be
/// observed from.** The validator's trigger, its brief and its restated request
/// all read [`InterjectionContext::conversation`]; so does the objective
/// fallback the engine computes beside it. An occupant that records the field is
/// therefore the cheapest evidence that the readers see the request's
/// conversation and not the log's projection — and it costs no judge, no
/// enrolment and no model call.
struct SeamWitness {
    seen: Mutex<Vec<Vec<Item>>>,
    objectives: Mutex<Vec<Objective>>,
}

impl SeamWitness {
    fn new() -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            objectives: Mutex::new(Vec::new()),
        }
    }

    fn conversation(&self, index: usize) -> Vec<Item> {
        let seen = self.seen.lock().expect("the witness is never poisoned");
        assert!(
            seen.len() > index,
            "expected at least {} turns through the seam, saw {}",
            index + 1,
            seen.len()
        );
        seen[index].clone()
    }

    fn objective(&self, index: usize) -> Objective {
        self.objectives
            .lock()
            .expect("the witness is never poisoned")[index]
            .clone()
    }
}

#[async_trait]
impl Interjector for SeamWitness {
    async fn consider(&self, context: &InterjectionContext<'_>) -> Interjection {
        self.seen
            .lock()
            .expect("the witness is never poisoned")
            .push(context.conversation.to_vec());
        self.objectives
            .lock()
            .expect("the witness is never poisoned")
            .push(context.objective.clone());
        Interjection::proceed()
    }
}

/// Every reader behind the interjection seam sees the request's conversation.
///
/// The second turn's claim omits the instruction run the log holds. What the
/// seam is handed must be the claim — the trigger's exchanges, the brief's
/// trajectory and the restated user request are all built from it — and the
/// objective derived beside it must name the client's own trailing question.
#[tokio::test]
async fn the_policy_and_validation_readers_see_the_requests_conversation() {
    let store = Arc::new(MemoryStore::new());
    let recorder = Arc::new(PromptRecorder::answering());
    let witness = Arc::new(SeamWitness::new());
    let engine = Arc::new(
        engine_over(&store, &recorder)
            .with_interjector(Arc::clone(&witness) as Arc<dyn Interjector>),
    );
    let app = messages_router(
        ControlPlane::open(),
        engine,
        store,
        Arc::new(Conversations::new()),
    );

    post(
        &app,
        "/v1/messages",
        &messages_body(
            Some(SYSTEM),
            json!([{ "role": "user", "content": FIRST_QUESTION }]),
        ),
    )
    .await;
    post(
        &app,
        "/v1/messages",
        &messages_body(
            None,
            json!([
                { "role": "user", "content": FIRST_QUESTION },
                { "role": "assistant", "content": ANSWER },
                { "role": "user", "content": SECOND_QUESTION },
            ]),
        ),
    )
    .await;

    let seen = witness.conversation(1);
    assert!(
        !seen
            .iter()
            .any(|item| item.spoken_text().contains(SYSTEM) || item.render().contains(SYSTEM)),
        "the seam must judge the conversation the client sent: {seen:?}"
    );
    assert_eq!(
        seen.len(),
        3,
        "three items, which is what the client sent: {seen:?}"
    );
    assert_eq!(
        witness.objective(1),
        Objective::from_items(&seen),
        "and the objective derived beside it is derived from the same items"
    );
}

/// The control on the other side of the same seam: a reference turn is handed
/// the log's projection, instruction run included.
#[tokio::test]
async fn the_readers_of_a_reference_turn_still_see_the_log() {
    let store = Arc::new(MemoryStore::new());
    let recorder = Arc::new(PromptRecorder::answering());
    let witness = Arc::new(SeamWitness::new());
    let engine = engine_over(&store, &recorder)
        .with_interjector(Arc::clone(&witness) as Arc<dyn Interjector>);
    let session_id = SessionId::new("native/readers");
    engine.create_session(&session_id).await.unwrap();

    engine
        .run_turn(
            &session_id,
            TurnId::new("t0"),
            vec![configuration(SYSTEM), Item::user_text(FIRST_QUESTION)],
            &Admission::open(),
        )
        .await
        .expect("the first reference turn is served");
    engine
        .run_turn(
            &session_id,
            TurnId::new("t1"),
            vec![Item::user_text(SECOND_QUESTION)],
            &Admission::open(),
        )
        .await
        .expect("the second reference turn is served");

    let seen = witness.conversation(1);
    assert!(
        seen.iter().any(|item| item.render().contains(SYSTEM)),
        "a reference turn's readers see what the log holds: {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// The native session API, where the log is the conversation
// ---------------------------------------------------------------------------

/// A request that names a session and sends only new input still continues from
/// the log.
///
/// There is no claim here to be authoritative: the native API's whole contract
/// is that the server holds the conversation. A reference turn therefore keeps
/// every item the log holds, instruction run included, which is the behaviour
/// the full-history rule above must not reach into.
#[tokio::test]
async fn a_native_reference_turn_still_continues_from_the_log() {
    let store = Arc::new(MemoryStore::new());
    let recorder = Arc::new(PromptRecorder::answering());
    let engine = engine_over(&store, &recorder);
    let session_id = SessionId::new("native/reference");
    engine.create_session(&session_id).await.unwrap();

    engine
        .run_turn(
            &session_id,
            TurnId::new("t0"),
            vec![configuration(SYSTEM), Item::user_text(FIRST_QUESTION)],
            &Admission::open(),
        )
        .await
        .expect("the first reference turn is served");

    // Only the new input, which is the whole point of the native surface.
    engine
        .run_turn(
            &session_id,
            TurnId::new("t1"),
            vec![Item::user_text(SECOND_QUESTION)],
            &Admission::open(),
        )
        .await
        .expect("the second reference turn is served");

    let second = recorder.prompt(1);
    assert!(
        second.contains(SYSTEM),
        "a reference turn sent no instructions and must still get the \
         session's: {second}"
    );
    assert!(
        second.contains(FIRST_QUESTION) && second.contains(ANSWER),
        "and the history the log holds, which the request did not carry: {second}"
    );
}

#[tokio::test]
async fn responses_preserve_interior_developer_instructions_in_order() {
    let recorder = Arc::new(PromptRecorder::answering());
    let app = responses_surface(Arc::clone(&recorder));
    assert_eq!(
        post(
            &app,
            "/v1/responses",
            &responses_body(
                Some(SYSTEM),
                vec![
                    responses_message("user", FIRST_QUESTION),
                    responses_message("developer", "INTERIOR_INSTRUCTION_SENTINEL"),
                    responses_message("user", SECOND_QUESTION),
                ]
            )
        )
        .await,
        StatusCode::OK
    );
    let prompt = recorder.prompt(0);
    let positions: Vec<_> = [
        SYSTEM,
        FIRST_QUESTION,
        "INTERIOR_INSTRUCTION_SENTINEL",
        SECOND_QUESTION,
    ]
    .iter()
    .map(|text| prompt.find(text).expect("supplied item remains in prompt"))
    .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{prompt}"
    );
}

#[tokio::test]
#[ignore = "cache recovery after omitted partial output requires dispatched-context provenance"]
async fn cache_provenance_recovers_after_omitted_interrupted_output() {
    use roundhouse_core::event::SessionEventKind;
    use roundhouse_core::ids::ResponseId;
    use roundhouse_core::store::SessionStore;
    use roundhouse_server::{TurnHistory, TurnInput};

    let store = Arc::new(MemoryStore::new());
    let recorder = Arc::new(PromptRecorder::dying_once());
    let engine = engine_over(&store, &recorder);
    let session = SessionId::new("cache-after-partial");
    engine.create_session(&session).await.unwrap();
    let question = Item::user_text(FIRST_QUESTION);
    let complete = |items, history| TurnInput {
        items,
        history: TurnHistory::Complete(history),
        ..TurnInput::from(Vec::new())
    };
    assert!(
        engine
            .run_turn(
                &session,
                TurnId::new("failed"),
                complete(vec![question.clone()], vec![question.clone()]),
                &Admission::open()
            )
            .await
            .is_err()
    );
    engine
        .run_turn(
            &session,
            TurnId::new("retry"),
            complete(Vec::new(), vec![question.clone()]),
            &Admission::open(),
        )
        .await
        .unwrap();
    let next = Item::user_text(SECOND_QUESTION);
    engine
        .run_turn(
            &session,
            TurnId::new("next"),
            complete(
                vec![next.clone()],
                vec![
                    question,
                    Item::assistant_text(ANSWER, ResponseId::new("client")),
                    next,
                ],
            ),
            &Admission::open(),
        )
        .await
        .unwrap();
    let events = store.read_events(&session, 0, 1024).await.unwrap();
    let unverified = events
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision.cache_context_unverified),
            _ => None,
        })
        .unwrap();
    assert!(
        !unverified,
        "completed retry should establish usable cache provenance"
    );
}
