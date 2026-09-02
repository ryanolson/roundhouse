// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The control surface: the arguments that reach roundhouse's own MCP tools,
//! and the structured form a launcher passes them in.
//!
//! Its own module for the reason [`suppressors`](super::suppressors) and
//! [`signage`](super::signage) are theirs: [`super`] renders *environment*, and
//! this is the one other output a launch has. The two move on different clocks
//! — the redirect changes when the client's variables change, the registration
//! when its config forms or its tool list do — so a single file holding both
//! crosses the maintainability boundary M11.2b's F11 drew every time either
//! side moves, and the review that matters gets read as one diff instead of
//! two.
//!
//! Two arguments, generated and passed *before* the operator's own, which is
//! everything a client needs to reach roundhouse's own MCP tools and to know
//! when to call one.
//!
//! **`--mcp-config <json>`, and the key rides `${VAR}`.** Of the config forms
//! the 2.1.257 capture found (§5.8), three work and one does not: the CLI flag,
//! a project `.mcp.json` and the per-directory entries in `~/.claude.json` are
//! honoured, and a `settings.json` `mcpServers` key is silently inert. The flag
//! is the only one of the three that writes nothing — a `.mcp.json` lands in
//! the operator's repository the same way a `CLAUDE.md` would — and it
//! *shadows* a project file of the same server name outright, so a generated
//! registration cannot be half-overridden by one an operator forgot about.
//!
//! The header value is the literal `${<key variable>}`, which the client
//! expands from the environment [`super`]'s own map already carries the key
//! in. So the secret is in neither the argv nor any file, and this is the one
//! place Claude Code offers the indirection `codex_launch` gets from
//! `env_http_headers`. **The hazard is the unexpanded literal** — an unset
//! variable would put `${…}` on the wire as a turn key and every control call
//! would come back `401` — and it is closed upstream rather than here:
//! `topham` refuses a launch whose key variable is not exported, before
//! anything is spawned, which is the same refusal that stops a keyless client
//! reaching the deployment at all.
//!
//! **`--strict-mcp-config` is off by default.** It excludes *every* other MCP
//! configuration, not merely a colliding one (§5.8: a project server under a
//! distinct name was never even initialised), so switching it on means an
//! operator's own servers stop existing for that launch. A deployment that
//! wants exactly roundhouse's tools and nothing else says so in the profile.
//!
//! **`--append-system-prompt <text>`** carries [`signage`], and the two other
//! places that text could have gone are refused in [`signage`]'s own doc.

use crate::control_config::TURN_KEY_HEADER;
use crate::dialect::mcp_server_name;
use crate::mcp_api::MCP_MOUNT_PATH;

use super::{ClaudeLaunch, signage};

impl ClaudeLaunch {
    /// Where this deployment serves the MCP control surface, as the generated
    /// registration names it.
    ///
    /// A join onto the deployment root and *not* onto [`Self::messages_url`]:
    /// the two routes are siblings, mounted at the root beside each other, and
    /// a registration carrying [`API_PREFIX`](crate::responses_api::API_PREFIX)
    /// would point at a path nothing serves. The failure is the one
    /// `codex_launch::mcp_endpoint`'s doc names
    /// — a client that starts, runs turns perfectly, and cannot resolve a
    /// single control call — and it is why [`MCP_MOUNT_PATH`] is read from the
    /// module that mounts the route rather than spelled here.
    ///
    /// Simpler than the codex sibling because this launch's `base_url` already
    /// *is* the root: that client's SDK appends the version segment itself, so
    /// there is nothing to strip.
    pub fn mcp_url(&self) -> String {
        format!("{}{MCP_MOUNT_PATH}", self.base_url)
    }

    /// The `--mcp-config` value: one inline JSON object registering this
    /// deployment's control surface.
    ///
    /// **No secret is in it and none can get in**: the header value is the
    /// literal `${<key variable>}`, expanded by the client out of the
    /// environment [`Self::env`] put the key in. See the module doc's *The
    /// control surface* for why the flag rather than a file, and for what
    /// closes the unexpanded-literal hazard.
    ///
    /// The server *name* is [`mcp_server_name`] — derived from the namespace
    /// this deployment's tools come back under, never typed here. A
    /// registration under any other name produces a client whose every control
    /// tool is called `mcp__<that name>__…`, which the validate fold does not
    /// recognise as roundhouse's own traffic and which no steer resolves
    /// against.
    ///
    /// Rendered through `serde_json` rather than `format!` for the reason
    /// `codex_launch::quote` gives about TOML: a base URL containing a quote or
    /// a backslash would otherwise produce a document that fails inside the
    /// *client*, where the error names a column in a string nobody wrote by
    /// hand.
    pub fn mcp_registration(&self) -> String {
        serde_json::json!({
            "mcpServers": {
                mcp_server_name(): {
                    // `http` and not `sse`: the surface is Streamable HTTP, and
                    // the 2.1.257 capture (§5.8) shows the client speaking
                    // JSON-RPC over `POST {url}` under exactly this type.
                    "type": "http",
                    "url": self.mcp_url(),
                    "headers": { TURN_KEY_HEADER: format!("${{{}}}", self.key_env) }
                }
            }
        })
        .to_string()
    }

    /// The arguments a launcher puts *before* the operator's own.
    ///
    /// **Generated, and separate from the operator's verbatim tail**, which is
    /// the seam a launcher needs and the reason this returns [`GeneratedArg`]s
    /// rather than a string a caller splits: an argument containing a space —
    /// the signage contains thousands — is one argv element, and a launcher
    /// that rebuilt this by splitting text would hand the client a hundred
    /// flags it does not know. Pairs rather than a flat `Vec<String>` for the
    /// same reason one step on: a launcher that has to tell a flag from a value
    /// guesses, and guessed wrong (F5).
    ///
    /// Leading rather than trailing so that the operator's own argv is the last
    /// word on anything both name. A launcher that lets both spell
    /// `--mcp-config` has two answers to one question and should refuse; this
    /// module cannot see the operator's tail and so cannot be the one to.
    pub fn leading_argv(&self) -> Vec<GeneratedArg> {
        let mut argv = vec![GeneratedArg::pair("--mcp-config", self.mcp_registration())];
        if self.strict_mcp {
            argv.push(GeneratedArg::bare("--strict-mcp-config"));
        }
        argv.push(GeneratedArg::pair(APPEND_SYSTEM_PROMPT_FLAG, signage()));
        argv
    }
}

/// The flag the signage rides, spelled once.
///
/// `pub` for the same reason [`REDACTED_VALUE`](super::REDACTED_VALUE) is:
/// `topham plan` summarises that one argument rather than printing it, and had
/// restated the literal to find it (F5). Two spellings of one flag name are two things a rename can
/// leave disagreeing, and the disagreement is silent — the plan would print a
/// thousand lines of signage where it meant to print a count.
pub const APPEND_SYSTEM_PROMPT_FLAG: &str = "--append-system-prompt";

/// One generated argument: a flag, and the value that belongs with it.
///
/// **Structured, where a flat `Vec<String>` of interleaved flags and values
/// would have been shorter to write** — because the flat form loses which is
/// which at the crate boundary and every consumer then guesses it back. Both
/// guesses were wrong (F5): `topham`'s collision check took any `--`-prefixed
/// entry without whitespace to be a flag *this* launch generates, so a value
/// that happened to look like one refused the operator's identical argument;
/// and its plan re-spelled a flag name of its own to find the single value it
/// summarises. Flat argv is one call away ([`flatten_argv`]) and the structure
/// is not recoverable from it, so the pairs are what crosses the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedArg {
    /// Always a literal: a flag is this module's own vocabulary, never data.
    flag: &'static str,
    /// `None` for a switch, which is the whole of the distinction a consumer
    /// cannot make from the flat form.
    value: Option<String>,
}

impl GeneratedArg {
    /// A flag and its value, passed as two argv elements.
    pub fn pair(flag: &'static str, value: impl Into<String>) -> Self {
        Self {
            flag,
            value: Some(value.into()),
        }
    }

    /// A switch, which takes no value.
    pub fn bare(flag: &'static str) -> Self {
        Self { flag, value: None }
    }

    /// The flag name — and the only thing a collision check may compare.
    pub fn flag(&self) -> &'static str {
        self.flag
    }

    /// The value, if this argument has one.
    pub fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }
}

/// The flat argv a spawn takes, from the structured form above.
///
/// The single flattening seam, so "flag then value, in order" is stated once
/// rather than by each launcher that has a `Command` to fill.
pub fn flatten_argv(args: &[GeneratedArg]) -> Vec<String> {
    args.iter()
        .flat_map(|argument| {
            std::iter::once(argument.flag.to_string()).chain(argument.value.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_launch::CUSTOM_HEADERS_ENV;
    use crate::claude_launch::tests::{ROOT, TURN_KEY, env_of, launch};
    use crate::codex_launch::DEFAULT_KEY_ENV;
    use crate::responses_api::API_PREFIX;

    /// The MCP half of the same 2.1.257 capture the fixtures above come from
    /// (§5.8): five requests, `initialize` through `tools/call`, made by the real
    /// client against a stub registered exactly the way this module generates one.
    ///
    /// Read by the test below rather than transcribed, because the two facts it is
    /// consulted for — the path the client posts to and the header name that
    /// survives to the wire — are the two a generated registration can get wrong
    /// while still parsing.
    const MCP_WIRE: &str = include_str!("../../tests/fixtures/claude-2.1.257-mcp-wire.json");

    /// R-M3: the generated registration is the config form the captured client
    /// honoured, and it carries a variable rather than a key.
    ///
    /// **Pinned as a whole string *and* read field by field**, because the two fail
    /// differently: the byte pin catches a shape change nobody meant, and the field
    /// assertions say which field a deliberate change moved. The whole-string pin
    /// is what a launcher's snapshot then renders, so it moving is a change to what
    /// an operator reads in a dry run as much as to what the client is handed.
    #[test]
    fn the_mcp_registration_is_the_config_form_the_captured_client_honoured() {
        let registration = launch().mcp_registration();
        assert_eq!(
            registration,
            r#"{"mcpServers":{"roundhouse":{"headers":{"x-roundhouse-key":"${ROUNDHOUSE_API_KEY}"},"type":"http","url":"http://127.0.0.1:8080/mcp"}}}"#
        );

        let parsed: serde_json::Value = serde_json::from_str(&registration)
            .expect("the registration is JSON the client parses");
        let servers = parsed["mcpServers"]
            .as_object()
            .expect("a `mcpServers` object");
        assert_eq!(
            servers.keys().collect::<Vec<_>>(),
            vec![mcp_server_name()],
            "one server, named so its tools come back under this deployment's own \
             namespace: {registration}"
        );
        let server = &servers[mcp_server_name()];
        assert_eq!(server["type"].as_str(), Some("http"));
        assert_eq!(
            server["url"].as_str(),
            Some(launch().mcp_url().as_str()),
            "the registration and `mcp_url` are one derivation"
        );
        assert_eq!(
            server["headers"]
                .as_object()
                .expect("a headers object")
                .len(),
            1,
            "one header: every additional one is a name that overrides whatever the \
             client would otherwise send under it"
        );
        assert_eq!(
            server["headers"][TURN_KEY_HEADER].as_str(),
            Some(format!("${{{DEFAULT_KEY_ENV}}}").as_str()),
            "the key rides `${{VAR}}`, which the client expands out of the \
             environment this module's own map put the key in"
        );

        // The two facts the capture settles, against the capture. A registration
        // that named the wrong path or the wrong header parses perfectly and
        // produces a client that reaches nothing.
        let wire: serde_json::Value = serde_json::from_str(MCP_WIRE).expect("the capture is JSON");
        let requests = wire.as_array().expect("a list of requests");
        assert!(
            requests.len() >= 5,
            "the capture is the whole handshake: {}",
            requests.len()
        );
        for request in requests {
            assert_eq!(
                request["path"].as_str(),
                Some(MCP_MOUNT_PATH),
                "the real client posted every MCP request to the path this \
                 registration has to name"
            );
            assert!(
                request["headers"][TURN_KEY_HEADER].is_string(),
                "the header the registration declares reached the wire on request \
                 {}: {request}",
                request["n"]
            );
        }
    }

    /// The registration names a variable, so no rendering of it can hold the key.
    ///
    /// Asserted over the whole generated argv rather than over the JSON alone: the
    /// signage is in there too, and the argv is what a launcher prints, logs and
    /// puts in a process listing that any other user of the box can read.
    #[test]
    fn no_generated_argument_carries_the_turn_key() {
        let launch = launch();
        for argument in flatten_argv(&launch.leading_argv()) {
            assert!(
                !argument.contains(TURN_KEY),
                "the turn key is in the generated argv: {argument}"
            );
        }
        // The control: the key *is* somewhere, and it is the one place the module
        // doc says it is.
        assert!(
            env_of(&launch)[CUSTOM_HEADERS_ENV].contains(TURN_KEY),
            "the header block is where the key rides"
        );
    }

    /// The variable named is the launcher's own, and naming another moves nothing
    /// else.
    ///
    /// The failure this prevents is silent and total: a launcher that exports the
    /// key under one name while the registration reads another generates `${…}`
    /// expanding to nothing, and every control call arrives with an empty turn key
    /// — a `401` on the control surface only, while every inference turn keeps
    /// answering.
    #[test]
    fn the_registration_reads_the_variable_the_launcher_exports() {
        let renamed = launch().with_key_env("DEPLOY_TURN_KEY");
        assert_eq!(renamed.key_env(), "DEPLOY_TURN_KEY");
        assert_eq!(
            renamed.mcp_registration(),
            launch()
                .mcp_registration()
                .replace(DEFAULT_KEY_ENV, "DEPLOY_TURN_KEY"),
            "the variable is the only thing that moves"
        );
        // And the environment map is untouched by it: the header block holds the
        // key itself, because that variable is not expanded (§1.6).
        assert_eq!(env_of(&renamed), env_of(&launch()));
    }

    /// R-M3: the generated argv is the registration, the signage, and — only when
    /// asked — the strict flag.
    #[test]
    fn the_generated_argv_is_two_flags_and_a_third_only_when_the_profile_asks() {
        let launch = launch();
        assert_eq!(
            launch.leading_argv(),
            vec![
                GeneratedArg::pair("--mcp-config", launch.mcp_registration()),
                GeneratedArg::pair(APPEND_SYSTEM_PROMPT_FLAG, signage()),
            ],
            "off by default: `--strict-mcp-config` drops an operator's own servers \
             wholesale, which is a deployment decision rather than part of hooking up"
        );
        // The flat form a spawn takes, pinned beside the structured one: the pairs
        // are what crosses the crate boundary (F5), and this is what they become.
        assert_eq!(
            flatten_argv(&launch.leading_argv()),
            vec![
                "--mcp-config".to_string(),
                launch.mcp_registration(),
                "--append-system-prompt".to_string(),
                signage(),
            ]
        );

        let strict = launch.clone().with_strict_mcp_config(true);
        assert_eq!(
            strict.leading_argv(),
            vec![
                GeneratedArg::pair("--mcp-config", strict.mcp_registration()),
                GeneratedArg::bare("--strict-mcp-config"),
                GeneratedArg::pair(APPEND_SYSTEM_PROMPT_FLAG, signage()),
            ]
        );
        assert_eq!(
            launch.clone().with_strict_mcp_config(false).leading_argv(),
            launch.leading_argv(),
            "and asking for it off is asking for the default"
        );
    }

    /// F2 (M12 thermo-nuclear review), now closed: there is no deployment-wide
    /// `mcp_namespace`, and the registration names the one server by construction.
    ///
    /// It landed red and `#[ignore]`d asserting the opposite half — that a
    /// configured `"mcp_namespace": "mcp__acme"` reached `mcp_registration()` and
    /// named the server `acme`. It never could: `ClaudeLaunch::new` and
    /// `mcp_registration` take no plane or dialect argument, and `mcp_server_name()`
    /// reads `DEFAULT_MCP_NAMESPACE` unconditionally, as does the Codex
    /// registration, the signage and the validate fold's recognizer a crate below.
    /// The knob was accepted and validated for two milestones while reaching none
    /// of them.
    ///
    /// So the fix retired the knob rather than threading it, and this test is what
    /// that decision looks like from this surface: a config that asks for its own
    /// namespace is refused at load, and the registration this module generates
    /// names the server the constant does. Both halves matter — a refusal alone
    /// would leave the second free to drift.
    #[test]
    fn there_is_no_configured_mcp_namespace_and_the_registration_names_the_one_server() {
        use crate::control_config::config::ControlPlaneConfig;

        let refusal = ControlPlaneConfig::from_json(
            r#"{
              "projects": [{ "id": "acme" }],
              "users": [{ "id": "ada" }],
              "mcp_namespace": "mcp__acme"
            }"#,
            "test",
        )
        .expect_err("the retired knob is refused at load");
        assert!(
            refusal.to_string().contains("mcp_namespace"),
            "the refusal names the field an operator would go looking for: {refusal}"
        );

        let registration = launch().mcp_registration();
        let parsed: serde_json::Value =
            serde_json::from_str(&registration).expect("the registration is JSON");
        let servers = parsed["mcpServers"]
            .as_object()
            .expect("a `mcpServers` object");
        assert_eq!(
            servers.keys().collect::<Vec<_>>(),
            vec![mcp_server_name()],
            "one server, named by the constant every other site reads"
        );
    }

    /// The MCP route is a sibling of the turn route, not a child of it.
    ///
    /// A registration carrying [`API_PREFIX`] points at a path nothing serves, and
    /// the client reports that as a server it could not reach rather than as a
    /// configuration mistake — the failure `codex_launch::mcp_endpoint` exists to
    /// prevent on the other client, reached here from the other direction because
    /// this launch's `base_url` is already the root.
    #[test]
    fn the_mcp_url_is_mounted_at_the_root_beside_the_turn_route() {
        let launch = launch();
        assert_eq!(launch.mcp_url(), format!("{ROOT}{MCP_MOUNT_PATH}"));
        assert_eq!(
            launch.messages_url(),
            format!("{ROOT}{API_PREFIX}/messages")
        );
        assert!(
            !launch.mcp_url().contains(API_PREFIX),
            "{}",
            launch.mcp_url()
        );
    }
}
