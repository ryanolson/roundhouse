// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The wire. One file, on purpose.
//!
//! Everything in this crate that knows what JSON-RPC is lives here, and nothing
//! here decides anything. [`ControlSurface`] holds the semantics,
//! [`crate::tools::dispatch`] turns a named call into a typed one, and this file
//! translates between that and `rmcp`'s types. Replacing the SDK with a
//! hand-rolled POST-only handler is a rewrite of this file and of no test.
//!
//! # Why the official SDK, and what the gate was
//!
//! The plan sanctioned either `rmcp` or a hand-rolled handler, gated on the
//! dependency tree: no OpenSSL — the Dynamo `deny.toml` rule this workspace
//! matches — and no second `axum` against the `=0.8.4` pin. `rmcp = "3.1.3"`
//! with `default-features = false` and three features adds **five** crates to
//! the workspace (`rmcp`, `base64 0.23`, `sse-stream`, `schemars_derive`,
//! `serde_derive_internals`), pulls no TLS stack of its own, and reaches axum
//! not at all: its HTTP server is a `tower_service::Service`, so it mounts into
//! the existing router as a route rather than as a second framework. The gate
//! passed on the first resolution, so the SDK is what we speak — and Codex's
//! own client is `rmcp`-based, which is the direction wire-level disagreement
//! would have hurt most.
//!
//! # Stateless, and what that buys
//!
//! [`NeverSessionManager`] plus `legacy_session_mode: false` is the
//! configuration the plan describes: no `Mcp-Session-Id` is issued, no session
//! state is held, and `GET /mcp` answers 405 — which the specification permits
//! for a server offering no stream, and which is honest here because §1 of the
//! plan established that nothing we could push would reach the model anyway.
//! It is also where the 2026-07-28 revision lands, so the surface is
//! forward-compatible rather than merely minimal.
//!
//! # Authentication is the server's, not ours
//!
//! There is no key parsing in this crate. The server mounts its existing
//! `Authorization: Bearer rh_turn_…` resolution as a layer in front of this
//! service and inserts the resolved [`Principal`] into the request extensions;
//! [`RoundhouseMcp::caller`] reads it back out of the `http::request::Parts`
//! `rmcp` hands to a tool call. One extractor, one place that knows what a key
//! looks like, and a request that reaches a tool without a principal is refused
//! rather than served to a default one.

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{RoleServer, ServerHandler};

use roundhouse_core::control::Principal;

use crate::surface::{ControlSurface, ToolOutcome};
use crate::tools::{ToolCall, descriptors};

/// The MCP endpoint, bound to one [`ControlSurface`].
#[derive(Clone)]
pub struct RoundhouseMcp {
    surface: Arc<dyn ControlSurface>,
}

impl RoundhouseMcp {
    pub fn new(surface: Arc<dyn ControlSurface>) -> Self {
        Self { surface }
    }

    /// The tool list, in `rmcp`'s vocabulary.
    ///
    /// A projection of [`descriptors`] and never a second list: the golden pin
    /// is on the descriptors, and
    /// `the_adapter_lists_exactly_what_the_surface_declares` is what keeps this
    /// function from becoming a place a tool can be added or renamed.
    ///
    /// The three MCP hints ride along because the *client* reads them, not us:
    /// an absent annotation is not a neutral one, and codex resolves the
    /// absence to destructive-and-open-world (see [`crate::tools`]'s
    /// *Annotations are not decoration*). They are projected here rather than
    /// restated because [`get_tool`](ServerHandler::get_tool) and
    /// [`list_tools`](ServerHandler::list_tools) both answer out of this one
    /// function, so there is exactly one place the wire form can drift from
    /// the descriptor.
    pub fn tools() -> Vec<Tool> {
        descriptors()
            .into_iter()
            .map(|tool| {
                let schema = tool
                    .input_schema
                    .as_object()
                    .cloned()
                    .expect("every declared input schema is a JSON object");
                Tool::new(
                    Cow::Borrowed(tool.name),
                    Cow::Borrowed(tool.description),
                    Arc::new(schema),
                )
                .with_annotations(
                    // `idempotent_hint` is deliberately left unset. It is the
                    // one hint whose honest answer differs per tool -- the
                    // reads are idempotent, `declare_intent` and the overlay
                    // writers are not -- and codex's approval arithmetic never
                    // consults it, so a value here would be a claim made for
                    // no reader.
                    ToolAnnotations::new()
                        .read_only(tool.read_only_hint)
                        .destructive(tool.destructive_hint)
                        .open_world(tool.open_world_hint),
                )
            })
            .collect()
    }

    /// The `_meta` key Claude Code puts the id of the `tool_use` block it is
    /// answering under.
    ///
    /// Spelled by the client, not by us: read off the 2.1.257 capture in
    /// `roundhouse-server`'s `tests/fixtures/claude-2.1.257-mcp-wire.json`,
    /// where a `tools/call` carries
    /// `"_meta": {"claudecode/toolUseId": "toolu_…", "progressToken": 2}`.
    /// A namespaced key like this one is exactly what MCP's `_meta` is for, so
    /// nothing here is reaching into a field that was not offered.
    const TOOL_USE_ID_META: &'static str = "claudecode/toolUseId";

    /// The `_meta` key Codex puts the id of the thread it is running under.
    ///
    /// Spelled by the client, not by us, and unnamespaced because that is how
    /// the client spells it: `with_mcp_tool_call_thread_id_meta`
    /// (codex `core/src/mcp_tool_call.rs:1198-1220` @ `e363b08`, called at line
    /// 442 with no conditional guard) inserts `sess.thread_id` under this exact
    /// key on **every** `tools/call`, beside an `x-codex-turn-metadata` object
    /// carrying the client's own `session_id`. The M9 capture shows the value
    /// byte-identical to the `prompt_cache_key` on the same turn's
    /// `/v1/responses` bodies, which is why the resolver treats it as a *name*
    /// (M12.1, R-M7) and not as a second kind of opaque correlator.
    ///
    /// A bare key rather than a namespaced one is worth noting, not fixing:
    /// MCP's `_meta` reserves namespaced keys for the sender's own use, and an
    /// unnamespaced one is a name any client could collide with. Reading it is
    /// safe regardless because what it resolves *through* is the caller's own
    /// namespace — a client that meant something else by `threadId` names a
    /// conversation this caller does not hold, and falls through as an unknown
    /// correlator rather than reaching another tenant's session.
    const THREAD_ID_META: &'static str = "threadId";

    /// The `tool_use` block this call is answering, if the client named one.
    ///
    /// **Read from the request *context* and not from `request.meta`.** `rmcp`
    /// strips the wire's `params._meta` into the message envelope's extensions
    /// during deserialization and its service loop moves it onto
    /// [`RequestContext::meta`] before dispatch — the typed params' own `meta`
    /// field stays empty on the way in. A reader that took `request.meta` would
    /// compile, would be `None` on every real request, and would silently
    /// return every MCP call to the pre-R-M2 `latest` guess.
    ///
    /// A non-string value is `None` rather than a refusal. This is a
    /// correlation hint on a call the deployment can serve without it, so a
    /// client that spells it oddly gets the fallback and its answer, not a
    /// protocol error mid-turn.
    fn tool_use_id(context: &RequestContext<RoleServer>) -> Option<String> {
        Self::meta_string(context, Self::TOOL_USE_ID_META)
    }

    /// The thread the client says this call is in, if it named one.
    ///
    /// Read from the same place and on the same terms as [`Self::tool_use_id`]
    /// — see that function for why the request *context* and not
    /// `request.meta`, and why a non-string value is `None` rather than a
    /// refusal.
    fn thread_id(context: &RequestContext<RoleServer>) -> Option<String> {
        Self::meta_string(context, Self::THREAD_ID_META)
    }

    /// One `_meta` string, or `None`.
    ///
    /// Both correlators read through one function rather than two copies of
    /// four lines: the copies would be identical the day they were written and
    /// the interesting way for them to diverge is silent — one reading
    /// `context.meta` and the other `request.meta`, which is exactly the
    /// mistake the doc above exists to warn about and which no test on either
    /// key alone would catch.
    fn meta_string(context: &RequestContext<RoleServer>, key: &str) -> Option<String> {
        context
            .meta
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::to_string)
    }

    /// Who is calling, from the extensions the HTTP layer filled in.
    ///
    /// `rmcp` injects the request's `http::request::Parts` into the tool
    /// context; the server's auth layer put a [`Principal`] into that request's
    /// own extensions. A missing principal is a protocol error rather than a
    /// tool error — the request could not be routed to a tenant at all, so
    /// there is no tenant to render a tool result for.
    fn caller(context: &RequestContext<RoleServer>) -> Result<Principal, McpError> {
        context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Principal>())
            .cloned()
            .ok_or_else(|| {
                McpError::invalid_request(
                    "this request carried no resolved key; the /mcp route is served behind the same bearer-key resolution as the turn surfaces",
                    None,
                )
            })
    }
}

/// The one text block, in `rmcp`'s vocabulary.
///
/// [`ToolOutcome`] can hold nothing else — see [`crate::surface`] — so this is
/// a translation and not a decision. `structured_content` is left `None`, which
/// is the same statement made twice on purpose: once in the type, once here,
/// where a future contributor would otherwise reach for it.
fn into_result(outcome: ToolOutcome) -> CallToolResult {
    let content = vec![ContentBlock::text(outcome.text().to_string())];
    if outcome.is_error() {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

impl ServerHandler for RoundhouseMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            // The version the plan pins the semantics to. `rmcp` negotiates
            // upward for a client that asks for a later one, and every later
            // revision so far only removes session state we do not keep.
            .with_protocol_version(ProtocolVersion::V_2025_06_18)
            // Not `from_build_env`, which would publish this crate's name and
            // version: what a client is identifying is the deployment it is
            // talking to, and "roundhouse-mcp 0.1.0" names a library nobody
            // outside this repository has heard of.
            .with_server_info(Implementation::new("roundhouse", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Roundhouse routes this conversation between local and hosted models. These tools \
                 report what it is doing and let you ask for less than your key allows -- never \
                 more.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Unpaginated and identical on every call. A client caches this list
        // and Codex folds it into the prompt, so a list that varied by cursor
        // or by caller would invalidate prompt caches across every session in
        // the deployment at once.
        // Built by mutating a default rather than by a struct literal, and the
        // same below for the server config: both are `#[non_exhaustive]` in
        // `rmcp`, so a literal does not compile from outside that crate. The
        // clippy lint that objects is aimed at the case where a literal *is*
        // available.
        #[allow(clippy::field_reassign_with_default)]
        let result = {
            let mut result = ListToolsResult::default();
            result.tools = Self::tools();
            result
        };
        Ok(result)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        Self::tools().into_iter().find(|tool| tool.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let principal = Self::caller(&context)?;
        let call = ToolCall {
            name: request.name.to_string(),
            arguments: request
                .arguments
                .map(serde_json::Value::Object)
                .unwrap_or(serde_json::Value::Null),
            thread_id: Self::thread_id(&context),
            tool_use_id: Self::tool_use_id(&context),
        };
        let outcome = crate::tools::dispatch(self.surface.as_ref(), &principal, call).await;
        Ok(CallToolResponse::Complete(into_result(outcome)))
    }
}

/// Which `Host` headers the `/mcp` route answers to.
///
/// `rmcp` ships a loopback-only allowlist as its DNS-rebinding guard, aimed at
/// MCP servers running on a developer's laptop. Whether that guard is the right
/// one is not this crate's question — it depends on whether the deployment
/// requires a credential — so the answer arrives as an argument rather than as a
/// decision taken here. See [`mcp_service`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostGuard {
    /// `rmcp`'s default: `localhost`, `127.0.0.1`, `::1`, on any port.
    Loopback,
    /// Every `Host`, because something else is doing the work.
    AnyHost,
}

/// The `/mcp` route: a `tower` service the server mounts beside its other three
/// routers.
///
/// **`hosts` is a security decision and it follows the deployment's mode.**
/// [`HostGuard::AnyHost`] clears `allowed_hosts`, which is right for a
/// deployment that requires a bearer key: it is served behind whatever hostname
/// an operator gave it — the loopback list would refuse every real request — and
/// what replaces the guard is the key, which a rebinding attack cannot supply
/// because the browser it hijacks does not have one.
///
/// A deployment that requires *no* key has nothing to replace it with, so it
/// passes [`HostGuard::Loopback`] and keeps rmcp's default. That is not
/// belt-and-braces: an unconfigured deployment is a process on 127.0.0.1 with
/// eight tools that read its posture and write overlays against the developer's
/// live conversation, and it is exactly the deployment the guard was written
/// for. `allowed_origins` cannot stand in for it either — under rebinding the
/// browser believes it is same-origin, so the `Origin` check never fires and the
/// `Host` header is the only one still telling the truth.
pub fn mcp_service(
    surface: Arc<dyn ControlSurface>,
    hosts: HostGuard,
) -> StreamableHttpService<RoundhouseMcp, NeverSessionManager> {
    let handler = RoundhouseMcp::new(surface);
    let mut config = match hosts {
        HostGuard::Loopback => StreamableHttpServerConfig::default(),
        HostGuard::AnyHost => StreamableHttpServerConfig::default().disable_allowed_hosts(),
    };
    // No sessions, no `Mcp-Session-Id`, and therefore `GET /mcp` -> 405.
    config.legacy_session_mode = false;
    // One JSON response per POST rather than an SSE frame carrying one message:
    // every tool here answers in a single round trip, and a stream would be a
    // second framing for a client to get wrong.
    config.json_response = true;
    config.sse_keep_alive = None;
    config.sse_retry = None;
    StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::TOOL_NAMES;

    #[test]
    fn the_adapter_lists_exactly_what_the_surface_declares() {
        // The transport's whole contract with the rest of the crate. A tool the
        // adapter added, renamed or dropped on its way to the wire would be a
        // tool the golden pin never saw.
        let declared = descriptors();
        let published = RoundhouseMcp::tools();
        assert_eq!(
            published
                .iter()
                .map(|t| t.name.as_ref())
                .collect::<Vec<_>>(),
            TOOL_NAMES.to_vec()
        );
        for (declared, published) in declared.iter().zip(published.iter()) {
            assert_eq!(published.name.as_ref(), declared.name);
            assert_eq!(published.description.as_deref(), Some(declared.description));
            assert_eq!(
                serde_json::Value::Object((*published.input_schema).clone()),
                declared.input_schema,
                "`{}`'s schema is republished verbatim or not at all",
                declared.name
            );
            assert!(
                published.output_schema.is_none(),
                "`{}` must not advertise structured output: the single text \
                 block is what round-trips through the conversation",
                declared.name
            );
            // The hints are the half of the contract the *client* acts on
            // before it ever calls anything, so an adapter that dropped them
            // would restore F06 silently: the descriptor would still read
            // truthfully and the wire would still say destructive-and-open-world.
            let annotations = published
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("`{}` publishes no annotations", declared.name));
            assert_eq!(
                (
                    annotations.read_only_hint,
                    annotations.destructive_hint,
                    annotations.open_world_hint
                ),
                (
                    Some(declared.read_only_hint),
                    Some(declared.destructive_hint),
                    Some(declared.open_world_hint)
                ),
                "`{}`'s published hints are not the ones it declares",
                declared.name
            );
        }
    }

    #[test]
    fn a_refusal_travels_as_a_tool_error_and_not_as_a_protocol_error() {
        // MCP's own distinction, and the one that matters mid-turn: a protocol
        // error is rendered opaquely by a client -- "tool result missing" --
        // while a tool error's content reaches the model, which is the only
        // form in which "roundhouse has not corrected this conversation" is
        // useful.
        let refused = into_result(ToolOutcome::refused(
            &crate::surface::SurfaceError::NoGuidanceYet("acme/ada/main".into()),
        ));
        assert_eq!(refused.is_error, Some(true));
        assert_eq!(refused.content.len(), 1);
        assert!(refused.structured_content.is_none());

        // The control.
        let served = into_result(ToolOutcome::ok(&serde_json::json!({"ok": true})).unwrap());
        assert_eq!(served.is_error, Some(false));
        assert_eq!(served.content.len(), 1);
        assert!(served.structured_content.is_none());
    }
}
