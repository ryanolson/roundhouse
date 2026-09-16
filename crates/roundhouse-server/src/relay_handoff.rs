// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The chained topology's other half: the NeMo Relay configuration that aims a
//! Relay at *this* deployment, and the preflight that proves it landed.
//!
//! [`claude_launch`](crate::claude_launch)'s chained runbook and
//! [`codex_launch`](crate::codex_launch) say what the *client* is handed on the
//! chained path — the same map and the same files as Direct, which is that
//! runbook's whole finding. This module is the piece on Relay's side: the three
//! lines of `config.toml` that point Relay's upstream back here, the argv that
//! runs an agent behind it, and the check that a system Relay layer did not
//! re-aim the run somewhere else.
//!
//! # Why one module rather than a function beside each generator
//!
//! The alternative considered was `claude_launch::relay_config_toml` and
//! `codex_launch::relay_config_toml`, one beside each generator it belongs to.
//! Rejected because the template is the *same* template: an `[upstream]` table
//! with one key and an `[agents.<name>]` table with a `command`, differing only
//! in which two identifiers go in the holes. Two copies would spell the same
//! four lines twice, and Relay's `FileUpstreamConfig` is
//! `#[serde(deny_unknown_fields)]` (Relay evidence §2.4) — so a key that drifted
//! in one copy is a hard parse error on exactly the topology that copy serves,
//! discovered by whichever agent nobody ran this week. One rendering
//! parameterised by [`RelayAgent`] makes the divergence a two-arm `match` a
//! reader can see the whole of.
//!
//! What is *not* shared is the base URL's shape, and that asymmetry is the
//! reason both constructors take a **deployment root** rather than an upstream
//! URL. Relay's `anthropic_base_url` defaults to `https://api.anthropic.com` and
//! its `openai_base_url` to `https://api.openai.com/v1` (Relay evidence
//! §2.4, and both echoed by a 0.8.2 `--dry-run`): the OpenAI one carries the API
//! prefix and the Anthropic one does not, because Relay concatenates the
//! inbound `path_and_query` onto the Anthropic base whole (§A.4) and the codex
//! wire's own base already ends at the version segment. That is the same split
//! the two generators make — `ClaudeLaunch::new` **refuses** a base URL carrying
//! [`API_PREFIX`] and `CodexLaunch`'s base URL requires it — so deriving both
//! from one root here means a caller holding one deployment address cannot hand
//! Relay the other client's shape.
//!
//! # What this deliberately does not write
//!
//! **No `anthropic_auth_header` / `openai_auth_header`.** The reference chained
//! wiring is the client carrying the turn key on its own dedicated header and
//! Relay forwarding it untouched (`claude_launch`'s chained runbook, from Relay
//! evidence §A.7 and §A.13). An upstream auth header here would be the
//! *fallback* wiring, which that runbook records as deliberately untested and
//! which requires a credential-less client. It also cannot survive contact with
//! hazard 4: `replace_upstream_base_url` clears a configured auth header
//! whenever a different layer supplies the base URL (§A.5), so a header written
//! here is one system config file away from silently vanishing while every
//! request still succeeds.
//!
//! **No `[gateway] bind`.** 0.8.2 refuses a non-loopback bind outright
//! (§A.10.1), so the default is the only value that works and naming it would
//! be a knob whose every other setting is an error.
//!
//! **No header comment.** A generated file usually earns one, and this one does
//! not: [`RelayHandoff::config_toml`]'s bytes are pinned by a guard test against
//! the literal template the `claude_e2e` rig used to write inline, which is what
//! makes "the rig and `topham relay` render the same thing" a fact rather than
//! an intention. Prose in the rendering would make that pin partly about the
//! prose.
//!
//! # The preflight, and why it is not optional
//!
//! `--config` replaces Relay's *user* config layer only; the system layer at
//! [`RELAY_SYSTEM_CONFIG`] is folded in **after** it and a leaf appearing in
//! both wins from the system file (Relay evidence §2.4). The switch that would
//! turn that off is behind a test-only cargo feature absent from the published
//! binary (§A.10.4). So an operator box with a system Relay install can re-aim a
//! chained launch — carrying a real turn key — at whatever that file names,
//! while everything downstream reads perfectly green.
//!
//! [`RelayHandoff::verify_resolved`] is the answer, and it costs one
//! `--dry-run`, which resolves configuration and exits without spawning the
//! agent. It is the same check the `claude_e2e` rig runs (M11.2b review F8) and
//! the same one `topham relay` runs before it execs, because the failure it
//! catches is identical in a rig and in an operator's session: a run pointed
//! somewhere nobody chose.

use std::path::Path;

use crate::responses_api::API_PREFIX;

/// Relay's system configuration layer — folded in *after* an explicit
/// `--config`, and the reason [`RelayHandoff::verify_resolved`] exists.
///
/// Named as a constant because it is the one thing a re-aim refusal must say:
/// an operator told their launch was pointed at a foreign upstream needs the
/// path of the file that did it, and it is not the one they passed.
pub const RELAY_SYSTEM_CONFIG: &str = "/etc/nemo-relay/config.toml";

/// The XDG variables a chained run must point at its own scratch.
///
/// Relay reads an XDG *user* config layer and writes its bootstrap and
/// marketplace state under XDG data and state. Left inherited, a chained launch
/// reads configuration nobody wrote for it and writes state the next launch
/// reads back — which is the same failure as the system layer above, one
/// directory further out.
///
/// Here rather than in either caller because both the gated rig and `topham
/// relay` need exactly this set, and the rig asserts a child's key set with
/// `==`: a second copy of this list is precisely the drift that assertion
/// exists to catch.
pub const RELAY_STATE_VARS: [&str; 4] = [
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
];

/// Which agent Relay is asked to run, and therefore which upstream it forwards.
///
/// The two facts this enum carries are not independent: `--agent claude` routes
/// `/v1/messages` to `anthropic_base_url` and `--agent codex` routes
/// `/v1/responses` to `openai_base_url` (Relay evidence §2.4, §A.4). Pairing
/// them in one type is what stops a config that names one agent and aims the
/// other one's upstream — a combination Relay accepts and which forwards every
/// turn to Relay's *default* upstream, i.e. straight to a frontier lab, with
/// nothing in the run saying so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayAgent {
    Claude,
    Codex,
}

impl RelayAgent {
    /// The value of `--agent`, and the `[agents.<name>]` table's name. Relay
    /// spells them the same, which is why this is one method.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// The `[upstream]` key this agent's turns are forwarded through.
    pub fn upstream_key(self) -> &'static str {
        match self {
            Self::Claude => "anthropic_base_url",
            Self::Codex => "openai_base_url",
        }
    }

    /// The `NEMO_RELAY_*` variable that overrides this agent's upstream.
    ///
    /// It layers **above** an explicit `--config` (Relay evidence §2.4), which
    /// is why a launcher has to look at it directly: an isolated `--dry-run`
    /// preflight deliberately clears it, so the one check that would catch it is
    /// the one the preflight cannot make. The same shape as
    /// [`ClaudeLaunch::must_be_unset`](crate::ClaudeLaunch::must_be_unset) —
    /// a variable whose mere presence changes what a launch means.
    pub fn upstream_env_var(self) -> &'static str {
        match self {
            Self::Claude => "NEMO_RELAY_ANTHROPIC_BASE_URL",
            Self::Codex => "NEMO_RELAY_OPENAI_BASE_URL",
        }
    }

    /// Everything before the agent's own argv, `--` included.
    ///
    /// `run` rather than the bare `nemo-relay claude` shortcut: the shortcut
    /// runs an interactive setup wizard when no config layer exists (Relay
    /// evidence §4.2's wizard, which requires a TTY), and a wizard is a hang in
    /// a rig and a surprise in a launcher. The trailing `--` is not optional —
    /// Relay's `RunCommand::command` is `#[arg(last = true)]`, so without it the
    /// agent's own flags are parsed as Relay's.
    pub fn run_argv(self, config: &Path) -> Vec<String> {
        self.argv(config, "--")
    }

    /// The preflight's argv: the same resolution, stopping before the spawn.
    ///
    /// On [`RelayAgent`] rather than on [`RelayHandoff`] because a preflight is
    /// also run against a config file the caller wrote itself — the hazard-4
    /// guards in `claude_e2e` do exactly that — and requiring a rendering that
    /// such a caller then ignores would be a constructor call made to satisfy a
    /// signature.
    pub fn preflight_argv(self, config: &Path) -> Vec<String> {
        self.argv(config, "--dry-run")
    }

    fn argv(self, config: &Path, last: &str) -> Vec<String> {
        vec![
            "run".to_string(),
            "--agent".to_string(),
            self.as_str().to_string(),
            "--config".to_string(),
            config.to_string_lossy().into_owned(),
            last.to_string(),
        ]
    }
}

/// A resolved chained handoff: which agent, aimed where, running what.
///
/// Built through [`RelayHandoff::for_claude`] / [`RelayHandoff::for_codex`]
/// rather than by struct literal, so the deployment-root-to-upstream derivation
/// and the refusals below cannot be stepped around by a caller that already
/// "knows" the URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayHandoff {
    agent: RelayAgent,
    upstream_base_url: String,
    command: String,
}

/// Why a handoff could not be rendered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RelayHandoffError {
    #[error(
        "the deployment root is empty. A chained handoff aims Relay's `{key}` at this deployment, \
         and an empty value leaves Relay pointed at its own default upstream -- which is a \
         frontier lab, reached with whatever credential the client presented, and no part of the \
         run reports it"
    )]
    NoRoot { key: &'static str },
    #[error(
        "the deployment root `{root}` already carries `{API_PREFIX}`. Relay concatenates the \
         inbound path onto `anthropic_base_url` whole, so this would forward every turn to \
         `{API_PREFIX}{API_PREFIX}/messages` and report it as an upstream connection error. Pass \
         the deployment root, the way `ClaudeLaunch::new` takes it -- the prefix codex needs is \
         added by `RelayHandoff::for_codex` and by nothing else"
    )]
    RootCarriesApiPrefix { root: String },
    #[error(
        "the {field} `{value}` cannot be written into a Relay `config.toml`: it carries a quote, a \
         backslash or a control character. TOML would either refuse the file or -- worse -- parse \
         a different value than the one intended, and the launch would then be aimed at an \
         upstream nobody named"
    )]
    Unquotable { field: &'static str, value: String },
}

/// Relay resolved a different upstream than the handoff configured.
///
/// Its own error type rather than a `bool`, because the whole value of the
/// preflight is in the message: an operator whose launch was re-aimed needs to
/// be told the file that did it, and a caller that only knew "the check failed"
/// would have to re-derive that sentence.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "this chained launch's explicit --config names `{key} = {wanted}`, but Relay resolved \
     `{resolved}`. `--config` replaces only Relay's *user* layer, and {RELAY_SYSTEM_CONFIG} is \
     folded in after it, so a system Relay install on this box is re-aiming the run. Refusing to \
     launch an agent carrying this deployment's turn key at an upstream nobody chose. Relay \
     resolved:\n{output}"
)]
pub struct UpstreamReAimed {
    /// The `[upstream]` key that disagreed.
    pub key: &'static str,
    /// What this handoff configured.
    pub wanted: String,
    /// What Relay printed instead — or `(absent)` when the line was not there
    /// at all, which is a Relay whose `--dry-run` vocabulary moved rather than
    /// an operator whose box re-aimed it.
    pub resolved: String,
    /// The whole `--dry-run` output, so the failure is diagnosable without
    /// re-running it.
    pub output: String,
}

/// What [`UpstreamReAimed::resolved`] carries when Relay printed no such line.
const NO_SUCH_LINE: &str = "(absent)";

impl RelayHandoff {
    /// A chained Claude Code handoff against `deployment_root`.
    ///
    /// The root, with no [`API_PREFIX`] — the same string
    /// [`ClaudeLaunch::new`](crate::ClaudeLaunch::new) takes, and refused here
    /// for the same reason it is refused there.
    pub fn for_claude(deployment_root: &str, command: &str) -> Result<Self, RelayHandoffError> {
        let root = Self::root(RelayAgent::Claude, deployment_root)?;
        Self::build(RelayAgent::Claude, root, command)
    }

    /// A chained codex handoff against `deployment_root`.
    ///
    /// Takes the **same** root as [`for_claude`](Self::for_claude) and appends
    /// [`API_PREFIX`] itself, because Relay's `openai_base_url` is the
    /// prefixed base — see the module doc. One field in a caller's profile, two
    /// derivations, so a profile cannot name two deployments.
    pub fn for_codex(deployment_root: &str, command: &str) -> Result<Self, RelayHandoffError> {
        let root = Self::root(RelayAgent::Codex, deployment_root)?;
        Self::build(RelayAgent::Codex, format!("{root}{API_PREFIX}"), command)
    }

    fn root(agent: RelayAgent, deployment_root: &str) -> Result<String, RelayHandoffError> {
        let root = deployment_root.trim().trim_end_matches('/');
        if root.is_empty() {
            return Err(RelayHandoffError::NoRoot {
                key: agent.upstream_key(),
            });
        }
        if root.ends_with(API_PREFIX) {
            return Err(RelayHandoffError::RootCarriesApiPrefix {
                root: root.to_string(),
            });
        }
        Ok(root.to_string())
    }

    fn build(
        agent: RelayAgent,
        upstream_base_url: String,
        command: &str,
    ) -> Result<Self, RelayHandoffError> {
        quotable("upstream base URL", &upstream_base_url)?;
        quotable("agent command", command)?;
        Ok(Self {
            agent,
            upstream_base_url,
            command: command.to_string(),
        })
    }

    pub fn agent(&self) -> RelayAgent {
        self.agent
    }

    /// The value written into `[upstream]`, and the one
    /// [`verify_resolved`](Self::verify_resolved) requires Relay to echo.
    pub fn upstream_base_url(&self) -> &str {
        &self.upstream_base_url
    }

    /// The whole `config.toml`, byte for byte.
    ///
    /// Four lines and no more: every key Relay would accept here is either
    /// something this handoff must not set (see the module doc) or a default
    /// that is the only working value.
    pub fn config_toml(&self) -> String {
        format!(
            "[upstream]\n{key} = \"{url}\"\n\n[agents.{agent}]\ncommand = \"{command}\"\n",
            key = self.agent.upstream_key(),
            url = self.upstream_base_url,
            agent = self.agent.as_str(),
            command = self.command,
        )
    }

    /// See [`RelayAgent::run_argv`].
    pub fn run_argv(&self, config: &Path) -> Vec<String> {
        self.agent.run_argv(config)
    }

    /// See [`RelayAgent::preflight_argv`].
    pub fn preflight_argv(&self, config: &Path) -> Vec<String> {
        self.agent.preflight_argv(config)
    }

    /// The isolated environment a preflight — or a version probe — is spawned
    /// with.
    ///
    /// **Cleared, then exactly this** (M11.2b review F7). Relay applies its
    /// `NEMO_RELAY_*` environment layer *above* the explicit `--config`, so a
    /// preflight that inherited the operator's environment would be checking a
    /// resolution the real launch does not have — and, since a differing
    /// `NEMO_RELAY_ANTHROPIC_BASE_URL` also clears any configured auth header
    /// (§A.5), could report drift that is entirely its own.
    ///
    /// `PATH` is the one thing carried over, because the binary has to find its
    /// loader; `HOME` and [`RELAY_STATE_VARS`] all point at one scratch
    /// directory the caller owns.
    pub fn preflight_env(home: &Path, path: &str) -> Vec<(String, String)> {
        let home = home.to_string_lossy().into_owned();
        let mut env = vec![
            ("PATH".to_string(), path.to_string()),
            ("HOME".to_string(), home.clone()),
        ];
        env.extend(
            RELAY_STATE_VARS
                .iter()
                .map(|name| ((*name).to_string(), home.clone())),
        );
        env
    }

    /// The line `--dry-run` must print for this handoff to have landed.
    pub fn resolved_upstream_line(&self) -> String {
        format!("{} = {}", self.agent.upstream_key(), self.upstream_base_url)
    }

    /// Read a `--dry-run` output and rule on whether Relay resolved *this*
    /// upstream.
    ///
    /// Parsed by key rather than by `contains` of the whole line, so the two
    /// failures are distinguishable: a resolved-but-different upstream (a
    /// system layer re-aimed the run) reports the value Relay chose, and a
    /// missing key reports [`NO_SUCH_LINE`] — which is a Relay whose report
    /// vocabulary moved and is CLAUDE.md's synergy-vigilance case, not an
    /// operator's misconfiguration.
    pub fn verify_resolved(&self, dry_run_output: &str) -> Result<(), UpstreamReAimed> {
        let key = self.agent.upstream_key();
        let resolved = dry_run_output
            .lines()
            .filter_map(|line| line.trim().strip_prefix(key))
            .filter_map(|rest| rest.strip_prefix(" = "))
            .map(str::trim)
            .next();
        match resolved {
            Some(value) if value == self.upstream_base_url => Ok(()),
            other => Err(UpstreamReAimed {
                key,
                wanted: self.upstream_base_url.clone(),
                resolved: other.unwrap_or(NO_SUCH_LINE).to_string(),
                output: dry_run_output.to_string(),
            }),
        }
    }
}

/// Refuse a value TOML would mangle.
///
/// A refusal rather than an escape, because there is no case where the right
/// answer is a deployment root with a quote in it: escaping would accept a
/// value that is certainly a typo and aim a real turn key at whatever it
/// resolved to.
fn quotable(field: &'static str, value: &str) -> Result<(), RelayHandoffError> {
    let bad = value
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control());
    match bad {
        false => Ok(()),
        true => Err(RelayHandoffError::Unquotable {
            field,
            value: value.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests;
