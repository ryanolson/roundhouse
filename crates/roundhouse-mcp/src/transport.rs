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
            })
            .collect()
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
        };
        let outcome = crate::tools::dispatch(self.surface.as_ref(), &principal, call).await;
        Ok(CallToolResponse::Complete(into_result(outcome)))
    }
}

/// The `/mcp` route: a `tower` service the server mounts beside its other three
/// routers.
///
/// `allowed_hosts` is cleared. The default list is loopback-only, a DNS
/// rebinding guard aimed at MCP servers running on a developer's laptop; this
/// one is a deployment behind whatever hostname an operator gave it, and the
/// guard would refuse every real request. What replaces it is the bearer key,
/// which a rebinding attack cannot supply — the browser it hijacks does not
/// have one.
pub fn mcp_service(
    surface: Arc<dyn ControlSurface>,
) -> StreamableHttpService<RoundhouseMcp, NeverSessionManager> {
    let handler = RoundhouseMcp::new(surface);
    let mut config = StreamableHttpServerConfig::default().disable_allowed_hosts();
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
        }
    }

    #[test]
    fn a_refusal_travels_as_a_tool_error_and_not_as_a_protocol_error() {
        // MCP's own distinction, and the one that matters mid-turn: a protocol
        // error is rendered opaquely by a client -- "tool result missing" --
        // while a tool error's content reaches the model, which is the only
        // form in which "no steer by that id" is useful.
        let refused = into_result(ToolOutcome::refused(
            &crate::surface::SurfaceError::UnknownSteer {
                steer_id: "fc_nope".into(),
            },
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
