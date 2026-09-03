// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M11.2a thermo-nuclear review, finding F4 — ruled valid, fixed, and kept
//! here as the guard.
//!
//! **The resolution.** A tool-declaring turn's token accounting now includes a
//! declaration overhead: the canonical-JSON render of `tools` + `tool_choice`,
//! tokenized by the process tokenizer and added to the turn's input-side counts
//! — `isl_tokens` for the quote, the grant and the recorded decision;
//! `admitted_input_tokens` for what the wire reports and what `count_tokens`
//! answers; `estimated_usage`'s input where a provider reported none. It stays
//! out of the conversation items, the prefix check and the block hashes, so the
//! session's identity is exactly what it was. What a provider's cache does with
//! the preamble is deliberately not modelled at this rung; see
//! `Engine::declaration_tokens`.
//!
//! **The claim, as it was found.** `isl_tokens` (`engine.rs:1714`,
//! `assembler.buffer().isl_tokens()`) is a projection of the canonicalized log
//! items only. `TurnInput::tools` never becomes an item — deliberately, per
//! `messages_api/wire.rs`'s own test
//! (`the_beta_property_surface_is_accepted_and_never_becomes_an_item`), to
//! keep a client's toolbox out of the prefix hash. But every candidate is
//! quoted against `isl_tokens` alone (`engine.rs:1759-1764`,
//! `FrontierCatalog::quote`), the budget grant is opened against those same
//! candidates (`spend.rs`'s `open_grant` calling
//! `dearest_admissible_frontier_usd`), and `Candidate::cache_hit_ratio` reads
//! the same number — so a toolbox that changes nothing about *which* model
//! answers (the documented ruling at `engine.rs:268-270`) still changes how
//! big the request actually is, and nothing downstream of `plan` ever finds
//! out. The tool JSON does reach the real wire, verbatim
//! (`anthropic_messages.rs:416`, `body["tools"] = tools.clone()`), so a real
//! upstream tokenizes and bills every byte of it; roundhouse's own quote,
//! grant and dashboard numbers do not.
//!
//! **Why a fresh file rather than an addition to an existing suite.** F4's
//! file cluster is `engine.rs`, `engine/spend.rs` and `fleet/frontier.rs` —
//! internal pricing and budget mechanics with no dialect-specific parsing
//! involved, so the claim is tested the way `tier_selection.rs` tests routing
//! claims: directly against [`Engine::run_turn`] and a hand-built
//! [`Admission`], with no HTTP surface, control-plane JSON or wire dialect in
//! the way of the arithmetic under test.
//!
//! **The double.** [`HonestBillingClient`] is what a real upstream is: it
//! bills on the whole request it actually received, `quote.prompt` (the
//! conversation) *and* `quote.tools` (the client's declared toolbox) alike —
//! because that really is what the Anthropic wire sends
//! (`anthropic_messages.rs:416`) and really is what a provider's own
//! tokenizer counts. Nothing about that is invented for this test; it is the
//! same measurement `isl_tokens` itself uses (one byte, one token, under
//! [`ByteTokenizer`]) applied to one more field of the same quote.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{
    Allocation, BalanceQuery, Budget, BudgetTerms, BudgetWindow, DEFAULT_WARN_AT, Exhaustion,
    MemorySpendLedger, Principal, SpendLedger,
};
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::now_ms;
use roundhouse_core::routing::{AffinityPolicy, DecisionRecord, ProviderPricing};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierClients, FrontierError, FrontierQuote, FrontierStream,
    StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::test_support::single_model_catalog;
use roundhouse_server::{
    Admission, EchoLocalExecutor, Engine, EngineConfig, LocalExecutor, TurnInput,
};

mod common;

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// One frontier model, priced on *input* alone.
///
/// Output is free (`output_per_mtok_usd: 0.0`) so every dollar in this file
/// is a dollar the input axis produced — the one axis `isl_tokens` feeds. A
/// turn's price is therefore a pure function of what the quote priced as
/// input, which is exactly the number under test.
const INPUT_PER_MTOK_USD: f64 = 25.0;

/// The byte size of the tool schema's one padding field.
///
/// Sixteen kilobytes — smaller than the 65,835-byte, 24-tool preamble a real
/// Claude Code turn carries (`claude-2.1.251-turn-1.json`, confirmed by
/// direct measurement: tools are 79.0% of tools+system+messages), but large
/// enough that "this contributes zero to isl_tokens" is either true or
/// dramatically false — there is no rounding error this size.
const TOOL_SCHEMA_BYTES: usize = 16_000;

/// A project's entire lifetime ceiling.
const LIMIT_USD: f64 = 0.5;

/// How many independent, single-turn sessions one project runs.
///
/// More than enough to blow through [`LIMIT_USD`] if even two of them are
/// admitted at the true, tools-inclusive cost the [`HonestBillingClient`]
/// reports — see the module doc's worked arithmetic.
const TURNS: usize = 4;

/// A tool declaration shaped like a real one, sized so its byte count is
/// exact and legible rather than incidental.
fn big_tools() -> serde_json::Value {
    json!([{
        "name": "Read",
        "description": "x".repeat(TOOL_SCHEMA_BYTES),
        "input_schema": { "type": "object" },
    }])
}

/// One frontier candidate, input-priced, on a provider named `acme`.
///
/// [`single_model_catalog`] (M15, H2): one of the eleven fixtures of this
/// exact shape the rung named.
fn catalog() -> StaticFrontierCatalog {
    single_model_catalog(
        "acme",
        "flagship",
        WireProtocol::AnthropicMessages,
        0.9,
        ProviderPricing {
            input_per_mtok_usd: INPUT_PER_MTOK_USD,
            cached_input_per_mtok_usd: 0.0,
            cache_write_per_mtok_usd: 0.0,
            output_per_mtok_usd: 0.0,
        },
        1.0,
        0.0,
    )
}

/// A provider stand-in that bills the way a real upstream does.
///
/// `quote.prompt` is the conversation (what `isl_tokens` measures);
/// `quote.tools` is the client's declared toolbox, carried on the quote
/// separately and forwarded to the real wire verbatim
/// (`anthropic_messages.rs`'s `body["tools"] = tools.clone()`, cited at
/// `anthropic_messages.rs:416`). A real tokenizer counts both, because both
/// are bytes in the POST body it receives. This double reports exactly that
/// sum as `input_tokens` on `Done` — nothing invented, nothing roundhouse's
/// own quote did not already have on hand in `quote.tools`, just not folded
/// into the number that gets priced before dispatch.
struct HonestBillingClient {
    reply: String,
}

#[async_trait]
impl FrontierClient for HonestBillingClient {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        let tools_bytes = quote
            .tools
            .as_ref()
            .map(|tools| serde_json::to_string(tools).expect("tools serialize").len())
            .unwrap_or(0);
        let true_input_tokens = (quote.prompt.len() + tools_bytes) as u64;
        Ok(FrontierChunk::whole_response(
            self.reply.clone(),
            true_input_tokens,
            0,
            self.reply.len() as u64,
            0,
        ))
    }
}

struct Rig {
    engine: Arc<Engine<MemoryStore, ByteTokenizer>>,
    store: Arc<MemoryStore>,
    ledger: Arc<MemorySpendLedger>,
}

fn rig(client: Arc<dyn FrontierClient>) -> Rig {
    let store = Arc::new(MemoryStore::new());
    let ledger = Arc::new(MemorySpendLedger::new());
    let mut by_provider: HashMap<String, Arc<dyn FrontierClient>> = HashMap::new();
    by_provider.insert("acme".to_string(), client);
    let engine = Engine::with_provider_clients(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local")) as Arc<dyn LocalExecutor>,
        catalog(),
        Arc::new(FrontierClients::keyed(by_provider)),
        Arc::new(AffinityPolicy::new()),
        EngineConfig {
            turn_deadline_ms: 5_000,
            ..EngineConfig::default()
        },
    )
    .with_spend_ledger(Arc::clone(&ledger) as Arc<dyn SpendLedger>);
    Rig {
        engine: Arc::new(engine),
        store,
        ledger,
    }
}

/// One project, `refuse`-on-exhaustion, at [`LIMIT_USD`].
fn admission() -> Admission {
    Admission {
        principal: Principal::new("proj", "user"),
        budget: Some(BudgetTerms {
            budget: Budget {
                limit_usd: LIMIT_USD,
                window: BudgetWindow::Total,
                on_exhaustion: Exhaustion::Refuse,
                warn_at: DEFAULT_WARN_AT,
            },
            allocation: Allocation::Pooled,
        }),
        ..Admission::open()
    }
}

fn turn_input(tools: Option<serde_json::Value>) -> TurnInput {
    TurnInput {
        items: vec![Item::user_text("hi")],
        declared_baseline: None,
        output_token_cap: None,
        tools,
        tool_choice: None,
        // The dialect the Messages surface stamps, since that is the surface
        // whose tool payload this fixture imitates (M11.2a, F1).
        tools_dialect: Some(roundhouse_fleet::WireProtocol::AnthropicMessages),
    }
}

async fn decisions(store: &MemoryStore, session_id: &SessionId) -> Vec<DecisionRecord> {
    store
        .read_events(session_id, 0, 1_000)
        .await
        .expect("an in-memory log reads")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision),
            _ => None,
        })
        .collect()
}

async fn decision(store: &MemoryStore, session_id: &SessionId) -> DecisionRecord {
    let mut all = decisions(store, session_id).await;
    assert_eq!(all.len(), 1, "session should have routed exactly one turn");
    all.remove(0)
}

// ---------------------------------------------------------------------------
// F4a: the arithmetic — isl_tokens and expected_cost_usd see no tools at all
// ---------------------------------------------------------------------------

/// **CONTROL.** A conversation that is actually longer changes `isl_tokens`
/// and `expected_cost_usd`.
///
/// Kept live (never ignored) so the two tests below cannot be dismissed as
/// measuring a number that never moves for anyone: this proves the harness's
/// metric is real and responsive to a real change in what was sent, which is
/// exactly the property that makes its blindness to `tools` — sixteen
/// kilobytes of exactly the same kind of bytes — a defect rather than a
/// no-op assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_longer_conversation_does_change_isl_tokens_and_cost() {
    let rig = rig(Arc::new(HonestBillingClient { reply: "ok".into() }));

    // Unbudgeted, deliberately: this control is about isl_tokens sensitivity
    // to real content, not about the budget machinery, and a long-enough
    // conversation priced at the same rate as F4's tool payload would
    // otherwise collide with a tight ceiling for reasons that have nothing to
    // do with the property under test (see the constants module doc).
    let open = Admission::open();

    let short = SessionId::generate();
    rig.engine.create_session(&short).await.unwrap();
    rig.engine
        .run_turn(
            &short,
            TurnId::new("t1"),
            TurnInput {
                items: vec![Item::user_text("hi")],
                ..turn_input(None)
            },
            &open,
        )
        .await
        .expect("a tiny untooled turn must be admitted");

    let long = SessionId::generate();
    rig.engine.create_session(&long).await.unwrap();
    rig.engine
        .run_turn(
            &long,
            TurnId::new("t1"),
            TurnInput {
                items: vec![Item::user_text(&"word ".repeat(4_000))],
                ..turn_input(None)
            },
            &open,
        )
        .await
        .expect("a longer untooled turn must be admitted under the same open admission");

    let short_decision = decision(&rig.store, &short).await;
    let long_decision = decision(&rig.store, &long).await;

    assert!(
        long_decision.isl_tokens > short_decision.isl_tokens + 15_000,
        "control failed: a conversation carrying ~20,000 real bytes more \
         than the baseline did not move isl_tokens (short={}, long={}) -- \
         the metric under test is dead, and the F4 assertions below would be \
         tautological",
        short_decision.isl_tokens,
        long_decision.isl_tokens
    );
    assert!(
        long_decision.expected_cost_usd > short_decision.expected_cost_usd,
        "control failed: a longer conversation did not raise expected_cost_usd"
    );
}

/// **F4, the arithmetic half.** A 16,000-byte tool declaration must move both
/// `isl_tokens` and `expected_cost_usd` on the routing decision, because the
/// identical bytes reach the real wire verbatim and are billed by any real
/// upstream (`anthropic_messages.rs`'s `body["tools"] = tools.clone()`).
///
/// Before the fix it moved neither: `FrontierCatalog::quote` took
/// `isl_tokens: u64` and nothing that could carry a toolbox, and `isl_tokens`
/// was a fold over log items alone — which tools deliberately never become.
/// The fix adds `Engine::declaration_tokens` to the conversation's own count
/// at the one seam `plan` prices from, leaving the assembler's buffer, its
/// blocks and their hashes untouched: what changed is the *size* of the
/// request, never the identity of the conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_definitions_are_counted_in_the_quoted_isl_tokens_and_cost() {
    let rig = rig(Arc::new(HonestBillingClient { reply: "ok".into() }));

    let bare = SessionId::generate();
    rig.engine.create_session(&bare).await.unwrap();
    rig.engine
        .run_turn(&bare, TurnId::new("t1"), turn_input(None), &admission())
        .await
        .expect("a tiny untooled turn must be admitted under a fresh ceiling");

    let tooled = SessionId::generate();
    rig.engine.create_session(&tooled).await.unwrap();
    rig.engine
        .run_turn(
            &tooled,
            TurnId::new("t1"),
            turn_input(Some(big_tools())),
            &admission(),
        )
        .await
        .expect(
            "one tool-declaring turn still fits under a fresh $0.50 ceiling: \
             ~16k tokens at $25/Mtok is ~$0.40, so this turn is admitted and \
             priced honestly rather than refused",
        );

    let bare_decision = decision(&rig.store, &bare).await;
    let tooled_decision = decision(&rig.store, &tooled).await;
    let tools_bytes = serde_json::to_string(&big_tools()).unwrap().len();

    // THE CLAIM: a request that is genuinely `tools_bytes` bigger on the wire
    // (`anthropic_messages.rs` forwards it verbatim, so a real upstream's
    // tokenizer counts every byte) is quoted as bigger, rather than
    // identically sized to a request with no toolbox at all.
    assert!(
        tooled_decision.isl_tokens >= bare_decision.isl_tokens + 10_000,
        "F4: a turn declaring a {tools_bytes}-byte tool schema must count \
         those bytes toward isl_tokens -- the one number every candidate is \
         priced against -- but bare isl_tokens={}, tooled isl_tokens={}",
        bare_decision.isl_tokens,
        tooled_decision.isl_tokens
    );
    assert!(
        tooled_decision.expected_cost_usd > bare_decision.expected_cost_usd,
        "F4: expected_cost_usd -- the number open_grant's \
         dearest_admissible_frontier_usd opens the budget grant against -- \
         must rise when a {tools_bytes}-byte tool preamble is declared, but \
         it stayed at ${}",
        bare_decision.expected_cost_usd
    );
}

/// **CONTROL for the fix, and the half that keeps it from being a blank
/// cheque:** a turn that declares *nothing* is counted exactly as it was
/// before F4.
///
/// The overhead is charged for a declaration that exists, never as a constant
/// on every turn. Without this, "tools now cost something" could be satisfied
/// by an implementation that inflated every request — including the untooled
/// MCP, admin and Responses turns that pass `None` — and every quote in the
/// system would drift up by the cost of an empty render.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_untooled_turn_carries_no_declaration_overhead() {
    let rig = rig(Arc::new(HonestBillingClient { reply: "ok".into() }));
    let items = vec![Item::user_text("hi")];

    assert_eq!(
        rig.engine.declaration_tokens(None, None),
        0,
        "a turn with nothing declared must add nothing"
    );
    // And the boundary the fix draws deliberately: `tool_choice` is counted
    // because it rides with a toolbox, never on its own. Every codex request
    // this repo builds sends `"tool_choice": "auto"` unconditionally, so
    // charging for a dangling choice would have re-priced every untooled turn
    // on the Responses surface — a ~22-token bump that is most of a short
    // prompt, enough to move a routing decision, and nothing to do with the
    // 65,835-byte preamble F4 is about. `budget_routing.rs`'s member-ceiling
    // and hold-expiry tests are the ones that caught it.
    assert_eq!(
        rig.engine
            .declaration_tokens(None, Some(&json!({ "type": "auto" }))),
        0,
        "a choice with no toolbox is envelope, not preamble"
    );
    assert!(
        rig.engine
            .declaration_tokens(Some(&big_tools()), Some(&json!({ "type": "auto" })))
            > rig.engine.declaration_tokens(Some(&big_tools()), None),
        "but a choice sent *with* a toolbox is part of the same declaration \
         and is counted with it"
    );
    assert_eq!(
        rig.engine.admitted_input_tokens(&items, None, None),
        rig.engine
            .admitted_input_tokens(&items, Some(&big_tools()), None)
            - rig.engine.declaration_tokens(Some(&big_tools()), None),
        "the admitted count is the conversation plus exactly the declaration \
         overhead -- one addition, not a second convention"
    );
}

/// **The count does not move when a proxy reorders the client's JSON.**
///
/// S3's first chain guard, instantiated for this number. A chained NeMo Relay
/// re-serializes every intercepted body through an alphabetizing map, so the
/// same toolbox arrives with its keys in a different order on the turn that
/// went through Relay than on the one that did not. A count that followed
/// insertion order would price and grant those two turns differently and,
/// worse, would report a different `count_tokens` answer for the same
/// toolbox. The property holds because `preserve_order` is off workspace-wide
/// (root manifest) — this test is what goes red if a dependency ever turns it
/// on, which is the manifest's own stated unlock condition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reordered_toolbox_is_counted_identically() {
    let rig = rig(Arc::new(HonestBillingClient { reply: "ok".into() }));

    let declared: serde_json::Value =
        serde_json::from_str(r#"[{"name":"Read","description":"d","input_schema":{"a":1,"b":2}}]"#)
            .expect("well-formed");
    let alphabetized: serde_json::Value =
        serde_json::from_str(r#"[{"description":"d","input_schema":{"b":2,"a":1},"name":"Read"}]"#)
            .expect("well-formed");

    assert_eq!(
        rig.engine.declaration_tokens(Some(&declared), None),
        rig.engine.declaration_tokens(Some(&alphabetized), None),
        "one toolbox, two encodings: a re-serializing proxy in the path must \
         not change what the turn is quoted at"
    );
}

// ---------------------------------------------------------------------------
// F4b: the concrete failure -- a Refuse budget crossed before it ever fires
// ---------------------------------------------------------------------------

/// **F4, the failure scenario, now the guard.** A project with
/// `on_exhaustion: refuse` and a $0.50 lifetime ceiling runs several
/// tool-declaring turns, each genuinely costing roughly $0.40 once the tools
/// that actually rode the wire are billed — which is what a real upstream
/// reports, modelled here by [`HonestBillingClient`].
///
/// Before the fix every one of them was granted for pennies, because the quote
/// never saw the tool bytes: all four dispatched and the ledger committed
/// $0.8038, 161% of the entire lifetime ceiling, with the `Refuse` arm only
/// ever stopping a turn *after* the settle that blew through it. Now the grant
/// is opened against the true size, so the first turn fits, the ceiling is
/// reached honestly, and the rest are refused before they dispatch — which is
/// what a refusing project is for.
///
/// Existing control, cited rather than duplicated (the F3 pattern): the
/// `Refuse` mechanism itself is proven correct on accurately-priced turns by
/// `budget_routing.rs`'s
/// `a_refuse_project_terminates_as_budget_exhausted_and_stays_retryable` —
/// three turns at a correctly-quoted, correctly-granted $0.10 each exhaust a
/// $0.55 ceiling exactly on schedule and the fourth is refused before it
/// dispatches. What is different here is not the `Refuse` arm; it is what
/// `open_grant` was asked to check against.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refuse_budget_stops_tool_declaring_turns_at_its_ceiling() {
    let rig = rig(Arc::new(HonestBillingClient { reply: "ok".into() }));

    let mut admitted = 0usize;
    for _ in 0..TURNS {
        let session_id = SessionId::generate();
        rig.engine.create_session(&session_id).await.unwrap();
        let result = rig
            .engine
            .run_turn(
                &session_id,
                TurnId::new("t1"),
                turn_input(Some(big_tools())),
                &admission(),
            )
            .await;
        if result.is_ok() {
            admitted += 1;
        }
    }

    let principal = admission().principal;
    let terms = admission()
        .budget
        .expect("this fixture always sets a budget");
    let balance = rig
        .ledger
        .balance(BalanceQuery {
            principal,
            terms,
            now_ms: now_ms(),
        })
        .await
        .expect("the memory ledger answers every well-formed read");

    // Fixture guard (not the finding), in both directions, because each
    // direction rules out a way this test could pass while saying nothing.
    // At least one: a ceiling that refused *everything* would satisfy the
    // spend assertion below trivially, and would mean these constants no
    // longer describe a project that can run a tooled turn at all. Fewer than
    // all: if every attempt were admitted the ceiling was never reached, and
    // the assertion below would be about a budget nobody approached.
    //
    // The exact figure this fixture produces is one admitted turn out of four
    // -- ~16k tokens at $25/Mtok is ~$0.40 of a $0.50 lifetime ceiling, so the
    // second turn's grant is refused before it dispatches. Before the fix all
    // four were admitted, which is what made `admitted >= 2` the right guard
    // to write at the time and what makes its inversion here evidence rather
    // than an adjustment.
    assert!(
        (1..TURNS).contains(&admitted),
        "fixture guard (not the finding): {admitted} of {TURNS} tool-declaring \
         turns were admitted -- the constants must leave room for at least one \
         turn and still reach the ceiling, or the assertion below is vacuous"
    );

    // THE CLAIM, stated as the behavior a `refuse`-on-exhaustion project is
    // for: it may not spend materially past the ceiling an admin wrote down.
    // Ten percent is the generous reading of the ledger's own documented
    // hold-vs-settle slop (`budget_routing.rs`'s module doc runs its whole
    // suite at a 2x output-hold ratio and still calls the *ceiling itself*
    // binding); it was never generous enough to absorb an entire tool preamble
    // priced as zero bytes on every one of `admitted` turns, which is what
    // made this the finding's assertion.
    assert!(
        balance.committed_usd <= 1.1 * LIMIT_USD,
        "F4: on_exhaustion: Refuse's ceiling is ${LIMIT_USD} -- a refusing \
         project must not spend materially past it -- but after {admitted} \
         admitted tool-declaring turns (out of {TURNS} attempted), each \
         carrying a {}-byte tool preamble, roundhouse's own ledger has \
         committed ${:.4}: {:.0}% of the entire lifetime ceiling. A grant \
         opened against a request smaller than the one actually sent is the \
         only way that happens",
        serde_json::to_string(&big_tools()).unwrap().len(),
        balance.committed_usd,
        100.0 * balance.committed_usd / LIMIT_USD,
    );
}
