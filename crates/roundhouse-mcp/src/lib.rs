// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The roundhouse control surface, as an MCP server.
//!
//! An agent talking to roundhouse over the Responses API can see what it is
//! being routed to only by inference. This crate gives it eight tools that say
//! so directly — and, for two of them, let it ask for *less*.
//!
//! # Three properties hold the whole design up
//!
//! **No tool appends to a session log.** An MCP request arrives on its own HTTP
//! request, and a session log has exactly one writer at a time — the turn gate
//! within a process, the store's lease across them. A second writer would
//! contend with both. So every tool here is either a pure read of committed
//! state or a write to the node-local [`ControlStore`], and steer fulfilment is
//! a *projection* of the ordinary write path rather than something this crate
//! performs. That is what lets a handler stay a pure reader of a stateful loop.
//!
//! **Overlays narrow and never widen.** [`prefer`](ControlSurface::prefer) and
//! [`set_quality_floor`](ControlSurface::set_quality_floor) are exposed to a
//! model, and a model reading its own context is one prompt injection away from
//! being someone else's. They are safe to expose because
//! [`TurnPolicy::narrow`](roundhouse_core::control::TurnPolicy::narrow) is
//! total and can only shrink the admissible set: an overlay that asks for more
//! than the deployment's ceiling allows is *clamped and reported*, never
//! honored and never refused. See [`overlay`] for the second half of that rule
//! — the one the narrow machinery cannot state, which is that an overlay
//! leaving nothing admissible is also an over-ask.
//!
//! **The transport is one file.** The tool semantics live in
//! [`ControlSurface`], a trait over plain serde request/response types, and are
//! tested against it with no socket in sight. [`transport`] binds that trait to
//! the official `rmcp` SDK and holds every line of code that knows what
//! JSON-RPC is. Swapping it for a hand-rolled handler moves no test.
//!
//! # Dependency direction
//!
//! `roundhouse-mcp` depends on `roundhouse-core` and on nothing else of ours.
//! It must never depend on `roundhouse-server`, which is the crate that
//! *supplies* the two seams below and mounts the router. Everything the surface
//! needs to read about a deployment arrives through [`ControlReads`]; everything
//! it writes goes to [`ControlStore`].
//!
//! # Verified against a real binary
//!
//! Two facts about the client hold this surface up: that a `[mcp_servers.*]`
//! entry with a `url` speaks streamable HTTP and sends its bearer from
//! `bearer_token_env_var`, and that a tool the client resolves is dispatched
//! and its output appended to the conversation as an ordinary item. Both were
//! read out of Codex's source until M9. Both are now observed against the
//! binary an operator actually runs — `codex-cli 0.146.0`, tree `e363b08` —
//! by `crates/roundhouse-server/tests/codex_e2e.rs`, which drives the real
//! process against a real roundhouse over a real socket.
//!
//! - **The endpoint is reachable, keyed, and speaks our protocol.**
//!   `McpServerTransportConfig::StreamableHttp { url, bearer_token_env_var, … }`
//!   (`config/src/mcp_types.rs:449-463` @ `e363b08`) is selected by config
//!   shape, and the token is read from the environment rather than from the
//!   file. Proved by `a_real_codex_binary_completes_the_mcp_handshake_against_our_server`:
//!   codex's `initialize` and `tools/list` arrive at our mount carrying the
//!   minted turn key as `Authorization: Bearer …`, negotiate protocol version
//!   `2025-06-18` — which is exactly what [`transport`] declares — and
//!   `fetch_steer` is in the tool list that comes back. That test settles
//!   something no source reading could: rmcp 3.1.3 serving an rmcp 1.8.0
//!   client, a pairing nothing had ever exercised.
//!
//! - **A dispatched tool's output rides back into the conversation.** Codex
//!   builds the namespace it dispatches on as `mcp__{server table key}`
//!   (`codex-mcp/src/tools.rs:22,228-234`) and resolves a call on the exact
//!   `(namespace, name)` pair (`core/src/tools/handlers/mcp.rs:29-66,121`).
//!   Proved in M9 by `a_real_codex_binary_executes_our_synthetic_tool_call_and_returns_its_output`
//!   and `a_real_codex_binary_resends_the_call_and_output_and_the_session_does_not_fork`:
//!   the client dispatched a synthetic `fetch_steer` it had never been told
//!   about, appended the result, and resent the call with its `arguments`
//!   byte-identical and its `function_call_output` immediately after it —
//!   *extending* the history rather than rebuilding it, so the session never
//!   forked.
//!
//!   **Both tests were deleted by M10.0 T7, and this paragraph is now history
//!   rather than a live citation.** R1 retires the tool-call steer channel and
//!   T4 deletes the wire projection that emitted the synthetic call, so no turn
//!   this deployment serves can ask a client to dispatch anything: the observed
//!   fact stands (it was observed, at `e363b08`, and the runs are in M9's
//!   evidence), but nothing in the tree re-observes it, and a reader should not
//!   take the citation for a guard that would go red.
//!
//!   What survived the deletion, and is asserted today by
//!   `the_next_turn_reflects_the_correction`, is the half that was never really
//!   about tool calls: a real client *extends* its history rather than
//!   rebuilding it, item for item, so an assistant message roundhouse committed
//!   on one turn comes back as our prefix on the next and the session does not
//!   fork. That is the property prefix admission depends on, and the correction
//!   is an ordinary assistant message now, so it is tested where every other
//!   item is.
//!
//!   The consequence worth stating plainly, because it is easy to read this
//!   surface as more reachable than it is: **the MCP tools below are reachable
//!   only if the agent's own model decides to call one.** Roundhouse's wire
//!   emits assistant text and nothing else, so it cannot put a call in front of
//!   a client the way it used to. The handshake and the tool listing are still
//!   proved against the real binary; a dispatch through them is not, and cannot
//!   be until a provider-emitted tool call is relayed through this wire.
//!
//! Two properties of the real client that the source reading did not predict,
//! and that anything asserting on this path has to know. Codex renders an MCP
//! result as `"Wall time: … seconds\nOutput:\n[…]"`, so a tool's output is
//! matched by containment and never by equality. And under
//! `approval_policy = "never"` a tool carrying no MCP annotations is treated
//! as destructive and open-world, and its call is **cancelled** — the agent
//! receiving a cancellation notice where the output should have been, with
//! nothing in the turn saying so.
//!
//! Both halves of the answer to that second one are now in the tree, and they
//! are not redundant. Every descriptor in [`tools`] states all three hints
//! (`readOnlyHint`, `destructiveHint`, `openWorldHint`), which is the narrow
//! answer and the only one that reaches a client we never handed a config to.
//! **That last half is read and not yet observed** — `requires_mcp_tool_approval`
//! (`core/src/mcp_tool_call.rs:2156-2173` @ `e363b08`) consults the hints
//! under codex's default `Auto` mode ahead of any config at all, but every
//! e2e run in this tree drives a client holding the generated config, so what
//! the binary does with an *un*configured stanza is source reading of the same
//! kind the block above replaced. The generated launch config
//! (`crates/roundhouse-server/src/codex_launch.rs`) keeps
//! `default_tools_approval_mode = "approve"` beside them, as the Direct
//! topology's defense in depth rather than as the fix: `codex exec` forces
//! `approval_policy = "never"`, so a client that disagreed with our hints for
//! any reason would cancel a writer rather than prompt about it, and there is
//! no interactive operator in that topology to prompt. Scoping the grant to
//! the reads instead — the narrower-looking option — was considered and
//! refused for exactly that reason; the ruling and its citations live beside
//! the generated stanza.
//!
//! *[History — recorded so nobody re-litigates it. Until M9 this block was a
//! documented assumption citing the then-current Cargo pin `6344a65`:
//! `codex-rs/mcp-client/src/router.rs:164` for transport selection by config
//! shape, and `codex-rs/core/src/mcp/registry.rs:440-444` for dispatch and
//! append. The facts are restated above against `e363b08` rather than
//! re-pinned to those paths, because `e363b08` is the tree the binary under
//! test was built from and the two revs are not on one line of descent —
//! neither is an ancestor of the other, and the MCP client was reorganized
//! between them. The Cargo pin itself is unchanged; M9 bumped nothing.]*
//!
//! # A third property, and since M12.1 it is read: codex names the
//! conversation on every call
//!
//! Codex stamps `params._meta.threadId` on **every** `tools/call` it
//! dispatches — `with_mcp_tool_call_thread_id_meta`
//! (`core/src/mcp_tool_call.rs:1198-1220` @ `e363b08`, called at line 442
//! with no conditional guard) inserts `sess.thread_id`. It rides a `_meta`
//! object that also carries `x-codex-turn-metadata.session_id`.
//!
//! **The transport reads it, beside `claudecode/toolUseId`, and hands it to
//! [`ControlReads::resolve_session`]** (M12.1, R-M7). Why the value outranks
//! the `latest` guess, what happens when it disagrees with the model's own
//! `conversation` argument, and how it is resolved are one ruling with one
//! home: the doc on [`ControlReads::resolve_session`], which is the code that
//! decides. It used to be restated here in full, which is how two copies of a
//! ruling come to disagree (M12.1 review, F4).
//!
//! **What M12.1 review corrected, and it is a fact about the oracle rather
//! than about us** (F2, R-M9). R-M7 read the value purely as a
//! `prompt_cache_key`, on a capture where the two were byte-identical. That
//! identity holds for a codex *root* thread and for nothing else: at the
//! pinned checkout (`6344a65`) `AgentControl`'s session id "is shared by the
//! whole agent control session ... every sub-agents from a common root share
//! the same session ID" (`core/src/agent/control.rs:104-110`, taken by any
//! non-root source at `core/src/session/session.rs:671-676`), and that shared
//! id is what becomes `prompt_cache_key` (`core/src/client.rs`), while
//! `_meta.threadId` stays each member's own. So the whole family names one
//! cache key, and a subagent's thread id — the case the rung exists for — is
//! nobody's. The per-thread marker is on the wire anyway: every turn carries
//! `x-codex-turn-metadata`, whose `thread_id`
//! (`core/src/responses_metadata.rs:281`, from
//! `core/src/session/turn_context.rs:618-622`) is that turn's own. The
//! Responses ingest binds it to the session it decided, per principal, and
//! `resolve_session` reads that binding before falling back to the name.
//!
//! **A third correlator, which the same `_meta` was already carrying**
//! (M14.1, R-C5). Beside `threadId`, codex's `tools/call` carries its whole
//! turn-metadata object under `x-codex-turn-metadata`
//! (`build_mcp_tool_call_request_meta`, `core/src/mcp_tool_call.rs:1175-1221`,
//! built by `current_meta_value_for_mcp_request`,
//! `core/src/turn_metadata.rs:183-222`), and that object keeps `session_id` —
//! the id an agent family shares, and therefore that family's
//! `prompt_cache_key`. Until M14.1 nothing read it. The transport reads it now
//! and hands it to [`ControlReads::resolve_session`] as a **name**, weighed
//! *after* the thread arm and before the tool-use id. A never-forked
//! conversation's session id is a pure function of the caller and that string,
//! so this arm consults no table at all — but for a **root** thread it is the
//! same string `threadId` already carried, so what it actually rescues is the
//! member whose thread id is nobody's cache key and whose binding this
//! deployment does not hold: that call reached `latest` before and reaches its
//! own family's conversation now. It is ordered behind the thread binding for
//! F2's reason: reading the family's shared cache key first would answer every
//! subagent about its parent.
//!
//! **What is exact and what still falls through**, since an agent reading this
//! should know which: a thread this deployment served a turn of resolves
//! exactly — through every fork of the cache key underneath it, and from any
//! node where `ROUNDHOUSE_REDIS_URL` puts the bindings in one shared store
//! (M14.1, R-C4). A thread whose binding has aged out of that store, or whose
//! turns a different node served on a deployment that shares nothing, falls to
//! the cache-key arm, which answers the whole agent family exactly — root
//! thread and subagent alike, per R-C5 above — for as long as the client
//! sends `x-codex-turn-metadata` at all. `latest` is reached only once that
//! header is itself absent, which is a guess and stays node-local on purpose.
//! A client that sends neither correlator is in that last case on every call.
//!
//! `init_session` remains the client-agnostic path and this does not replace
//! it; see the section below for what is still write-only about it. What
//! changed is that the two clients this deployment actually serves each hand
//! us an exact answer on every call, and refusing to read either of them left
//! the surface guessing where it did not have to.
//!
//! `codexs_meta_thread_id_rides_every_tools_call_and_is_never_read`
//! (`crates/roundhouse-server/tests/codex_e2e.rs`) used to assert the negative
//! half of this, and **M10.0 T7 retired it**: with the synthetic call gone
//! there is no `tools/call` in a hermetic run to stamp, so the assertion had
//! nothing to read. Its positive successor is
//! `a_real_codex_binary_is_correlated_by_the_thread_id_it_stamps` in the same
//! file, ignored on a box with no codex binary, and the claim it cannot reach
//! is pinned hermetically at three seams instead: the shared resolver's own
//! unit tests ([`reads`]), the dispatched-tool tests in
//! `roundhouse-mcp/tests/tool_surface.rs`, and the real-adapter tests in
//! `roundhouse-server/tests/mcp_surface.rs`.
//!
//! # Note the tense: `init_session` is still write-only
//!
//! [`init_session`](ControlSurface::init_session) mints an id, records it and
//! returns it in a form a client keeps. The id reaches a session log only by
//! riding the client's own resent history, and the session whose log holds it
//! is the session that made the call. M9 proves the *carriage* — a real client
//! does resend a tool's output verbatim into the next turn — but nothing in
//! this deployment resolves a session from a binding yet: [`binding_in_items`]
//! has no caller outside tests. The read side was noted here as M7's; M7 has
//! since landed (real frontier credentials) without it, so it belongs to no
//! rung at present rather than to that one. Both agent-facing sentences about
//! it ([`tools::descriptors`] and [`surface::InitSessionResponse::note`]) are
//! still written to say what is recorded rather than what is correlated, which
//! is what keeps the gap honest in the one place a model can read.

pub mod overlay;
pub mod reads;
pub mod store;
pub mod surface;
pub mod tools;
pub mod transport;

mod plane;

pub use overlay::{ModeNarrowing, OverlayScope, PreferMode, SessionOverlay, TimedOverlay};
pub use plane::ControlPlaneSurface;
pub use reads::{ControlReads, SessionFacts};
pub use store::{
    BindingId, ControlStore, IntentRecord, OutcomeRecord, SessionBinding, binding_ids_in_items,
    binding_in_items,
};
pub use surface::{Caller, ControlSurface, Correlators, SurfaceError, ToolOutcome};
pub use tools::{TOOL_NAMES, ToolCall, ToolDescriptor, descriptor, descriptors, dispatch};

#[cfg(test)]
mod tests {
    /// This module's own source, so the guard reads what a reader would read
    /// rather than a constant that could be deleted along with the paragraph
    /// it is meant to protect.
    const SOURCE: &str = include_str!("lib.rs");

    /// The *deciding* site's source (M12.1 review, F4). The R-M7 rationale
    /// lives on `ControlReads::resolve_session`, in `reads.rs`; a guard
    /// spelled `include_str!("lib.rs")` alone watches the narrative copy and
    /// leaves the copy beside the code that decides free to drift.
    const DECISION_SITE: &str = include_str!("reads.rs");

    /// A file up to (not including) its own `#[cfg(test)]` module.
    ///
    /// The whole-file version is a tautology — a guard module's assertions
    /// retype the markers they check for, so `SOURCE.contains(...)` would find
    /// its own literal however the doc comment above was mutated.
    /// `routing::stage` learned that the expensive way; the slice is the fix,
    /// copied from there rather than rediscovered.
    fn before_tests(source: &'static str) -> &'static str {
        source.split("\n#[cfg(test)]").next().unwrap()
    }

    /// Everything the R-M7 contract is written across, guarded together.
    fn doc_and_code() -> String {
        format!("{}\n{}", before_tests(SOURCE), before_tests(DECISION_SITE))
    }

    /// M12.1 review, F3: the R-M7/R-M8 contract paragraph above had nothing
    /// holding it in the tree — deleting the whole "A third property, and
    /// since M12.1 it is read" section left every suite in this crate and in
    /// `roundhouse-server` green, because nothing read it back. This does not
    /// make the paragraph *true* (nothing short of the resolver's own tests
    /// does that); it only makes it a defect to delete silently, the way
    /// `routing::stage`'s attribution guard does for its own module doc.
    ///
    /// M12.1 review, F4: and it now reads `reads.rs` too. The rationale has
    /// one home — the doc on `ControlReads::resolve_session`, which is the
    /// code that decides — so a guard that watched only this file's pointer
    /// to it was watching the copy that matters least.
    #[test]
    fn the_r_m7_contract_paragraph_survives() {
        let doc_and_code = doc_and_code();
        assert!(
            before_tests(SOURCE)
                .contains("A third property, and since M12.1 it is read: codex names the"),
            "the section heading introducing the current _meta.threadId \
             contract is gone"
        );
        for marker in [
            // The upstream fact the ruling is built on.
            "with_mcp_tool_call_thread_id_meta",
            "core/src/mcp_tool_call.rs:1198-1220",
            "prompt_cache_key",
            // The upstream fact that *corrected* it (M12.1 review, F2): the
            // thread id is not the cache key for anyone but a root thread,
            // and the per-thread marker rides the turn header instead.
            "core/src/agent/control.rs:104-110",
            "core/src/responses_metadata.rs:281",
            "x-codex-turn-metadata",
            // The third correlator the same `_meta` was already carrying, and
            // the upstream fact that it is the family's cache key (M14.1,
            // R-C5). Without these the section can lose the arm while still
            // reading as a complete account of what codex sends.
            "core/src/mcp_tool_call.rs:1175-1221",
            "core/src/turn_metadata.rs:183-222",
            "session_id",
            // The ruling itself, R-M7.
            "ControlReads::resolve_session",
            "ContradictoryConversation",
            // What proves it, R-M8, and where.
            "codexs_meta_thread_id_rides_every_tools_call_and_is_never_read",
            "a_real_codex_binary_is_correlated_by_the_thread_id_it_stamps",
            "roundhouse-mcp/tests/tool_surface.rs",
            "roundhouse-server/tests/mcp_surface.rs",
        ] {
            assert!(
                doc_and_code.contains(marker),
                "the R-M7/R-M8 contract paragraph is missing `{marker}` — \
                 either it was deleted, or it drifted from what the code and \
                 tests actually do"
            );
        }
        for marker in [
            // The refusal, stated where it is raised.
            "ContradictoryConversation",
            // The swallow that is *not* the refusal, and its one exception.
            "Only `ForeignConversation` is swallowed",
            // What R-M7 costs, and the one call that pays nothing (F8).
            "detecting a contradiction means resolving the client's",
            "*same string*",
            // R-M9's order within the thread step (F2).
            "session_of_thread",
            // R-C5's arm and its position: behind the thread binding, ahead of
            // the tool-use id, and swallowing the same one error.
            "correlators.cache_key",
            "Behind (2) rather than in front of it",
        ] {
            assert!(
                before_tests(DECISION_SITE).contains(marker),
                "the deciding site's own copy of the R-M7 rationale is \
                 missing `{marker}` — an implementor reads that doc and not \
                 this file's pointer to it"
            );
        }
    }

    /// M12.1 review, F4: the guard directly above used to pin only this
    /// module's prose copy of the R-M7 rationale, while the decision it
    /// narrates is made in `reads.rs`. `SOURCE` was `include_str!("lib.rs")`
    /// — one file, this one — so editing, garbling or deleting the decision
    /// site's own copy could not fail any test in this crate.
    ///
    /// This is the assertion that says the guard's *reach* is right rather
    /// than its contents: a marker unique to `reads.rs` must be covered by
    /// whatever `doc_and_code()` watches.
    #[test]
    fn the_decision_sites_own_copy_of_the_rationale_is_guarded_too() {
        // Unique to `reads.rs`'s doc on `resolve_session` (R-M7's paragraph on
        // why the correlators can't stay lazy) — present in the decision-site
        // file, absent from `lib.rs`. Asserted first so a failure below is
        // never mistaken for this marker having drifted out of `reads.rs`
        // instead.
        let decision_site_marker = "detecting a contradiction means resolving the client's";
        assert!(
            DECISION_SITE.contains(decision_site_marker),
            "fixture marker missing from reads.rs — this test does not reach \
             F4's claim; re-anchor the marker to whatever now carries the \
             R-M7 rationale beside the deciding function"
        );

        assert!(
            doc_and_code().contains(decision_site_marker),
            "F4: `the_r_m7_contract_paragraph_survives` guards only this \
             file's module doc — reads.rs's own copy of the R-M7 rationale, \
             sitting beside the function that actually makes the decision, is \
             unguarded and can drift or be deleted without failing any test \
             in this crate"
        );
    }
}
