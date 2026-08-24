// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The third thing an operator can hand a Codex client: files that tell the
//! *model* the control tools exist, and under what circumstances to reach for
//! one.
//!
//! [`super`] emits the two files that make a client *reach* roundhouse. This
//! module emits the ones that make the tools on the other end get *used*. The
//! gap is real and was measurable before this existed: `config.toml` registers
//! the MCP server, codex offers the model one deferred `mcp__roundhouse`
//! namespace (see `codex_e2e.rs`'s handshake assertion), and a model that has
//! never been told what routing control is available has no reason to expand
//! it. Nothing routes worse for the lack of these files — they add no
//! capability — but the plugin half of R1's "mcp + slash-commands + skills are
//! the surface for changing roundhouse's behavior" is otherwise a surface with
//! no signage.
//!
//! # Skills, not `prompts/` — a deviation from the M10.1 brief, with the reads
//!
//! The plan (R1) and this milestone's brief (P7) both say codex loads
//! `$CODEX_HOME/prompts` and that this module should emit slash-command files
//! into it. **That is stale, and emitting them would have shipped files
//! nothing reads.** Re-read per CLAUDE.md's "re-verify pinned-source claims
//! before milestones that rely on them", at both revisions that matter:
//!
//! - `e363b08` — the binary on the box, `codex-cli 0.146.0`, the one
//!   `codex_e2e.rs` drives;
//! - `6344a655` — the Cargo pin this workspace builds its conformance oracle
//!   from.
//!
//! At both: no loader reads a `prompts` directory (`grep '"prompts"'` over
//! every `.rs` returns nothing), and `custom_prompt*` resolves only to
//! `tui/src/bottom_pane/custom_prompt_view.rs`, which is the textarea that
//! collects a *review instruction* — not a slash-command registry, and TUI-only
//! besides, so `codex exec` could never have reached it.
//!
//! What does exist, and is reached from `core` rather than `tui`, is skills:
//!
//! - `core-skills/src/loader.rs:139,143` — `SKILLS_FILENAME = "SKILL.md"`,
//!   `SKILLS_DIR_NAME = "skills"`;
//! - `core-skills/src/loader.rs:320-331` — for the `User` config layer,
//!   `config_folder.join(SKILLS_DIR_NAME)`, commented "Deprecated user skills
//!   location (`$CODEX_HOME/skills`), kept for backward compatibility",
//!   discovered recursively. Deprecated is worth stating plainly: the
//!   *undeprecated* root is `$HOME/.agents/skills`, which is the user's own
//!   machine-wide directory rather than something a deployment may write, so
//!   the deprecated-but-supported path is the only one that is still hermetic.
//!   [`SKILLS_DIR`] is where that decision is written down;
//! - `core/src/session/mod.rs:3350-3380` — the listing is rendered into a
//!   `developer` message at thread start, gated on
//!   `config.include_skill_instructions`, which defaults **true**
//!   (`core/src/config/mod.rs:3812-3816`);
//! - `core-skills/src/render.rs:520-532` — each skill is listed to the model as
//!   `- {name}: {description} (file: {path})`, which is why
//!   [`SkillTemplate::description`] is written as a *condition* and not as a
//!   summary. The description is the entire selection surface: the model
//!   decides from that one line whether to open the `SKILL.md` at all.
//!
//! The ruling's intent survives the correction unchanged — emit plugin-surface
//! files whose text sends the model to the MCP tools, with the tool names
//! derived from the descriptors rather than retyped. Only the directory was
//! wrong. `codex_e2e.rs`'s
//! `a_real_codex_binary_is_told_about_the_generated_skills` is what turns that
//! paragraph from a code read into evidence: it writes these files into a
//! hermetic `CODEX_HOME` and asserts the real binary put them in the turn it
//! sent us.
//!
//! # Why nothing here takes a [`CodexLaunch`]
//!
//! A skill names a tool, never an address. The MCP server is registered once,
//! in `config.toml`, and the model reaches these tools through that
//! registration; a skill that spelled a `base_url` would be a second place the
//! deployment's address lives, and the two would part company on the first
//! redeploy — silently, because a stale URL inside a markdown file fails as a
//! model that quietly cannot call anything rather than as a config error.
//!
//! [`CodexLaunch`]: super::CodexLaunch

use roundhouse_mcp::tools::{ToolDescriptor, descriptors};

use crate::dialect::DEFAULT_MCP_NAMESPACE;

/// Where a generated skill goes, relative to the client's `CODEX_HOME`.
///
/// `$CODEX_HOME/skills` rather than `$HOME/.agents/skills`, although the
/// loader reads both and calls this one deprecated
/// (`core-skills/src/loader.rs:320-331` @ `e363b08`). The undeprecated root is
/// the *user's*, shared by every codex on the machine; writing a deployment's
/// files there would put roundhouse's skills into sessions that never pointed
/// at roundhouse. Everything this module emits belongs to one `CODEX_HOME`, so
/// it goes in the one directory that is scoped to it — and the day the
/// deprecated root is removed, this constant is the single line that moves.
pub const SKILLS_DIR: &str = "skills";

/// The file name codex looks for inside a skill directory
/// (`core-skills/src/loader.rs:139` @ `e363b08`).
const SKILL_FILE: &str = "SKILL.md";

/// What codex puts between an MCP server's namespace and a tool's own name to
/// build the name a model calls.
///
/// `codex-mcp/src/mcp/mod.rs:78-81` @ `e363b08` builds the namespace as
/// `mcp__{server}__` and `core/src/tools/handlers/mcp.rs:53` joins
/// `{namespace}{DELIMITER}{name}`; [`DEFAULT_MCP_NAMESPACE`] is the `mcp__{server}`
/// half without the trailing delimiter, which is the form codex advertises the
/// namespace tool itself under. Spelled as a constant beside the two citations
/// because a skill that named a tool codex cannot resolve is the failure this
/// whole module is downstream of, and it is silent: the model calls a name,
/// gets "no such tool", and moves on.
const MCP_TOOL_NAME_DELIMITER: &str = "__";

/// One file a client will find, as the client will find it.
///
/// Path and bytes together rather than a `write_all_into(dir)` helper, because
/// the two callers want different things with them: the e2e rig writes them
/// into a temporary `CODEX_HOME`, and an operator's tooling may want to diff
/// them against what is already on disk. A function that only wrote to a
/// directory would make the second one impossible to do without a scratch dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    /// Relative to the client's `CODEX_HOME`, always `/`-separated.
    ///
    /// Relative and not absolute because the whole point is that these compose
    /// with a `CODEX_HOME` the caller chose; `/`-separated because that is what
    /// both a `Path::join` on every platform this runs on and a human reading
    /// a report will accept.
    pub relative_path: String,
    pub contents: String,
}

/// One skill, as this crate declares it.
///
/// The tool *names* are `&'static str` here and are then resolved against
/// [`descriptors`] before any text is rendered — see [`descriptor`]. A literal
/// that does not resolve panics rather than producing a file, so the "derived,
/// never retyped" rule holds where it matters: every sentence about what a tool
/// does is the descriptor's own, and the only thing this table contributes is
/// *which* tool and *when*.
struct SkillTemplate {
    /// The directory under [`SKILLS_DIR`], and the frontmatter `name`.
    name: &'static str,
    /// The frontmatter `description` — the single line the model chooses from.
    ///
    /// Written as the condition under which the skill applies, never as a
    /// restatement of the tool's name. Three skills whose descriptions all read
    /// "control roundhouse's routing" are three skills a model cannot tell
    /// apart, and it will either open all three or none.
    description: &'static str,
    /// The tools this skill exists to get called, primary first.
    tools: &'static [&'static str],
    /// What the model should do with the answer.
    ///
    /// Per skill rather than shared, because the read tools and the writing
    /// tools fail in opposite directions: a read that returns nothing useful is
    /// worth reporting, while a write that comes back `narrowed: true` is a
    /// final answer that a retry cannot improve.
    outcome: &'static str,
}

/// The shipped three.
///
/// Three and not eight, deliberately. `init_session`, `declare_intent`,
/// `fetch_steer` and `report_outcome` are parts of a loop roundhouse drives —
/// the client is told about them by the turn it is in, not by a file it read at
/// startup — and a skill inviting a model to call them out of that context
/// would produce calls with nothing to answer. These three are the ones a
/// *user* asks for in words ("use the local models", "this answer is weak",
/// "what can you even route to?") and that a model therefore has to recognize
/// from the conversation alone.
const SKILLS: &[SkillTemplate] = &[
    SkillTemplate {
        name: "rh-status",
        description: "Use when the user asks what this session is allowed to route to right \
                      now -- which models are admissible, what budget is left, what policy is \
                      in force -- or asks why the previous turn went to the model it went to.",
        tools: &["status", "explain_last_route"],
        outcome: "Both tools only read. Quote the model names and the budget figure back to \
                  the user as they came; do not convert an admissible-model list into a \
                  promise that the next turn will use one of them, because roundhouse still \
                  chooses per turn.",
    },
    SkillTemplate {
        name: "rh-prefer",
        description: "Use when the user asks to keep this session on this deployment's own \
                      local models, or on hosted models, or to drop a preference they set \
                      earlier -- for the next turn or for a stretch of turns.",
        tools: &["prefer"],
        outcome: "The answer may come back `narrowed: true`, which means the request asked \
                  for more than this key is already allowed and nothing changed. That is an \
                  answer, not a failure: say so and continue. Repeating the call returns the \
                  same thing, because what refused it is the key's own policy and no argument \
                  to this tool can widen it.",
    },
    SkillTemplate {
        name: "rh-quality-floor",
        description: "Use when the user says the answers are not good enough and asks for a \
                      stronger model for a while, or names a minimum quality roundhouse \
                      should not route below.",
        tools: &["set_quality_floor"],
        outcome: "As with any narrowing tool here, a floor lower than the key's own -- or one \
                  that would leave nothing routable at all -- comes back `narrowed: true` with \
                  nothing changed. Report that rather than lowering the floor and trying \
                  again; a floor nothing can satisfy is refused for the user's benefit, not \
                  as an error to work around.",
    },
];

/// What every skill says before it says anything of its own.
///
/// Shared because it is the one fact that makes all three tools make sense, and
/// a model that read it three times in three phrasings would have three
/// slightly different beliefs about who picks the model. It costs nothing when
/// unread: a skill body only enters the context if the model opens the file.
const PREAMBLE: &str = "This session runs through roundhouse, which chooses the model for \
                        every turn. Naming a model in the request does not select one -- the \
                        `model` field is recorded and ignored. What can be changed is the set \
                        roundhouse chooses *from*, through the MCP tools below.";

/// Every generated skill file, ready to write under a `CODEX_HOME`.
///
/// Takes nothing: see the module doc on why a skill names a tool and never an
/// address.
pub fn skill_files() -> Vec<GeneratedFile> {
    SKILLS
        .iter()
        .map(|skill| GeneratedFile {
            relative_path: format!("{SKILLS_DIR}/{}/{SKILL_FILE}", skill.name),
            contents: render(skill),
        })
        .collect()
}

/// The name a model calls a roundhouse tool by, once codex has namespaced it.
///
/// Public because the e2e rig asserts on it and because an operator writing
/// their own skill file needs the same construction — one that is derived from
/// [`DEFAULT_MCP_NAMESPACE`] rather than typed out is one that survives a
/// namespace rename.
pub fn namespaced_tool_name(tool: &str) -> String {
    format!("{DEFAULT_MCP_NAMESPACE}{MCP_TOOL_NAME_DELIMITER}{tool}")
}

/// The descriptor a template names, or a panic naming what is missing.
///
/// A panic and not an `Option`, because there is no useful degraded output: a
/// skill file that omitted the one tool it exists to point at would still be a
/// valid file, would still be listed to the model, and would waste the model's
/// attention every session forever. The unreachable-by-construction claim is
/// held up by `every_generated_skill_names_a_tool_this_deployment_serves`
/// below, which is what turns a rename in `roundhouse-mcp` into a red test
/// here rather than a panic on somebody's deployment.
fn descriptor(tool: &str) -> ToolDescriptor {
    descriptors()
        .into_iter()
        .find(|candidate| candidate.name == tool)
        .unwrap_or_else(|| {
            panic!(
                "`{tool}` is not in roundhouse-mcp's tool list, so a skill pointing at it \
                 would send the model to a name codex cannot resolve"
            )
        })
}

/// The arguments a tool's schema says are required, as the schema spells them.
///
/// Read off `input_schema["required"]` rather than written beside the template,
/// so a schema that gains a required field updates the file that tells a model
/// how to call it. The alternative — a hand-kept list — fails in the direction
/// nobody notices: the model omits the new field, the surface refuses the call
/// with a decode error, and the transcript reads as a flaky tool.
fn required_arguments(descriptor: &ToolDescriptor) -> Vec<String> {
    descriptor.input_schema["required"]
        .as_array()
        .map(|required| {
            required
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// One skill file.
fn render(skill: &SkillTemplate) -> String {
    let mut out = String::new();
    // The frontmatter's two scalars go through `yaml_scalar`: both contain
    // `--` and one contains `-- which`, and a plain YAML scalar containing
    // `: ` would parse as a nested mapping. See that function.
    out.push_str("---\n");
    out.push_str(&format!("name: {}\n", yaml_scalar(skill.name)));
    out.push_str(&format!(
        "description: {}\n",
        yaml_scalar(skill.description)
    ));
    out.push_str("---\n\n");

    out.push_str(&format!("# {}\n\n", skill.name));
    out.push_str(PREAMBLE);
    out.push_str("\n\n## When this applies\n\n");
    out.push_str(skill.description);
    out.push_str("\n\n## What to call\n");

    for tool in skill.tools {
        let descriptor = descriptor(tool);
        out.push_str(&format!("\n### `{}`\n\n", namespaced_tool_name(tool)));
        // The descriptor's own sentence, quoted verbatim. This module states
        // *when*; `roundhouse-mcp` states *what*, and it states it to every
        // client, including the ones roundhouse generated no files for. Two
        // descriptions of one tool would disagree the first time either was
        // edited, and the one in the markdown is the copy nothing tests.
        out.push_str("> ");
        out.push_str(descriptor.description);
        out.push('\n');

        let required = required_arguments(&descriptor);
        out.push('\n');
        if required.is_empty() {
            out.push_str("No required arguments.\n");
        } else {
            let named = required
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("Required arguments: {named}.\n"));
        }
    }

    out.push_str("\n## What the answer means\n\n");
    out.push_str(skill.outcome);
    out.push('\n');
    out
}

/// One scalar, spelled so `serde_yaml` reads it back as the string it is.
///
/// Through `serde_json` rather than `format!("\"{s}\"")` for the reason
/// [`super::quote`] gives for TOML, plus one specific to this file: YAML 1.2 is
/// a superset of JSON, so a JSON string literal is a valid double-quoted YAML
/// scalar with the escaping already correct. Left unquoted, a description
/// containing `: ` — which a sentence naming a tool output easily does — parses
/// as a nested mapping, and `SkillFrontmatter`'s `description` field would come
/// back `None`. The skill still loads and is still listed, with the one line
/// the model selects on simply missing: exactly the silent half-failure this
/// crate refuses everywhere else.
fn yaml_scalar(value: &str) -> String {
    serde_json::to_string(value).expect("a str always encodes as a JSON string")
}

#[cfg(test)]
mod tests {
    use super::*;
    use roundhouse_mcp::tools::TOOL_NAMES;

    /// The frontmatter block of a generated file, as codex's
    /// `extract_frontmatter` would take it (`core-skills/src/loader.rs:1221-1241`
    /// @ `e363b08`: first line `---`, up to the next `---`).
    fn frontmatter(contents: &str) -> Vec<(String, String)> {
        let mut lines = contents.lines();
        assert_eq!(
            lines.next().map(str::trim),
            Some("---"),
            "a skill with no opening `---` is `SkillParseError::MissingFrontmatter`"
        );
        let mut fields = Vec::new();
        let mut closed = false;
        for line in lines {
            if line.trim() == "---" {
                closed = true;
                break;
            }
            let (key, value) = line
                .split_once(':')
                .unwrap_or_else(|| panic!("frontmatter line is not `key: value`: {line}"));
            let value: String = serde_json::from_str(value.trim())
                .unwrap_or_else(|error| panic!("`{key}` is not a quoted scalar: {error}"));
            fields.push((key.trim().to_string(), value));
        }
        assert!(closed, "the frontmatter block is never closed:\n{contents}");
        fields
    }

    fn field(contents: &str, key: &str) -> String {
        frontmatter(contents)
            .into_iter()
            .find(|(name, _)| name == key)
            .unwrap_or_else(|| panic!("no `{key}` in the frontmatter"))
            .1
    }

    /// P7's own test: every tool a generated file names is one this deployment
    /// actually serves.
    ///
    /// Scanned out of the rendered text rather than read off [`SKILLS`],
    /// because the table is not what a model reads — the file is, and a
    /// hand-written tool name that crept into a body or an `outcome` string
    /// would be invisible to a check that only looked at the table.
    #[test]
    fn every_generated_skill_names_a_tool_this_deployment_serves() {
        let prefix = format!("{DEFAULT_MCP_NAMESPACE}{MCP_TOOL_NAME_DELIMITER}");
        let mut found = 0usize;
        for file in skill_files() {
            for occurrence in file.contents.match_indices(&prefix) {
                let rest = &file.contents[occurrence.0 + prefix.len()..];
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                assert!(
                    TOOL_NAMES.contains(&name.as_str()),
                    "`{}` names `{prefix}{name}`, which is not in roundhouse-mcp's tool list \
                     {TOOL_NAMES:?} -- the model would call a name codex cannot resolve",
                    file.relative_path
                );
                found += 1;
            }
        }
        // The control: a scan that found nothing would pass the loop above for
        // the wrong reason. Every template names at least one tool, so the
        // count is the templates' own.
        assert_eq!(
            found,
            SKILLS.iter().map(|skill| skill.tools.len()).sum::<usize>(),
            "the scan must have seen every tool the templates name"
        );
    }

    /// The files land where the loader looks, under the name it will list.
    #[test]
    fn a_generated_skill_lands_where_the_client_looks_for_one() {
        let files = skill_files();
        assert_eq!(files.len(), SKILLS.len());
        for (file, skill) in files.iter().zip(SKILLS) {
            assert_eq!(
                file.relative_path,
                format!("skills/{}/SKILL.md", skill.name),
                "codex scans `$CODEX_HOME/skills` recursively for files named `SKILL.md`; \
                 anything else in there is not a skill and is not listed"
            );
            // The loader falls back to the directory name when frontmatter
            // carries none, so a mismatch would not fail to load -- it would
            // load under whichever of the two names won, and the file's own
            // heading would then be about a different skill than the one the
            // model selected.
            assert_eq!(field(&file.contents, "name"), skill.name);
            assert_eq!(field(&file.contents, "description"), skill.description);
        }
    }

    /// The description is the whole selection surface, so the three have to be
    /// tellable apart *as descriptions*.
    ///
    /// Not a style check. `core-skills/src/render.rs:524` lists each skill to
    /// the model as `- {name}: {description} (file: {path})` and nothing else
    /// reaches it until it opens one. Three descriptions that differ only in
    /// which tool they name give the model no basis to choose, and the failure
    /// mode is not "picks the wrong one" but "opens all three", which spends
    /// the context this surface was supposed to be cheap in.
    #[test]
    fn the_three_skills_are_told_apart_by_the_condition_each_states() {
        let files = skill_files();
        let descriptions: Vec<String> = files
            .iter()
            .map(|file| field(&file.contents, "description"))
            .collect();
        for (index, description) in descriptions.iter().enumerate() {
            assert!(
                description.to_ascii_lowercase().starts_with("use when"),
                "a description is a condition, not a summary: {description}"
            );
            for other in &descriptions[index + 1..] {
                assert_ne!(description, other);
                // Sharper than inequality: the trigger phrase is what the model
                // matches on, so two descriptions sharing their first clause are
                // as ambiguous as two identical ones.
                let clause = |text: &String| {
                    text.split(&['-', ','][..])
                        .next()
                        .unwrap_or_default()
                        .to_string()
                };
                assert_ne!(
                    clause(description),
                    clause(other),
                    "two skills open with the same condition, so the model has nothing to \
                     choose between them on"
                );
            }
        }
    }

    /// What a tool does is said once, by the tool.
    ///
    /// The body quotes `ToolDescriptor::description` verbatim. Asserting that
    /// is asserting the absence of a second copy: a paraphrase here would read
    /// fine, would pass every other test in this file, and would be the version
    /// that goes stale when the descriptor is edited -- while the descriptor's
    /// own golden pin (`roundhouse-mcp`) would stay green, because it has never
    /// heard of this file.
    #[test]
    fn a_skill_quotes_the_descriptors_own_sentence_rather_than_paraphrasing_it() {
        for (file, skill) in skill_files().iter().zip(SKILLS) {
            for tool in skill.tools {
                let described = descriptor(tool).description;
                assert!(
                    file.contents.contains(described),
                    "`{}` must carry `{tool}`'s own description verbatim, not a second \
                     wording of it",
                    file.relative_path
                );
            }
        }
    }

    /// The required-argument list is the schema's, not a copy of it.
    ///
    /// `prefer` is the interesting case -- three required fields, one of them
    /// (`reason`) easy to think optional. The mutation this catches is a
    /// hand-written list in the template that drifts from
    /// `input_schema["required"]`; the model then omits a field, the surface
    /// refuses the call as malformed, and the transcript reads as a flaky tool
    /// rather than as a stale file.
    #[test]
    fn the_required_arguments_a_skill_lists_are_the_schemas_own() {
        let prefer = skill_files()
            .into_iter()
            .find(|file| file.relative_path.contains("rh-prefer"))
            .expect("rh-prefer is shipped");
        let required = required_arguments(&descriptor("prefer"));
        assert_eq!(
            required,
            vec!["mode", "scope", "reason"],
            "if the schema changed, this assertion is the reminder that the generated file \
             changed with it"
        );
        for name in &required {
            assert!(
                prefer.contents.contains(&format!("`{name}`")),
                "`prefer` requires `{name}` and the skill file does not name it:\n{}",
                prefer.contents
            );
        }
        // The control: `status` requires nothing, and says so rather than
        // leaving a model to infer it from an absent line.
        let status = skill_files()
            .into_iter()
            .find(|file| file.relative_path.contains("rh-status"))
            .expect("rh-status is shipped");
        assert!(status.contents.contains("No required arguments."));
    }

    /// The namespaced name is built the way codex builds it.
    ///
    /// Pinned against [`DEFAULT_MCP_NAMESPACE`] rather than spelled out, so the
    /// day the namespace moves this fails here instead of in a model's
    /// transcript.
    #[test]
    fn the_tool_name_a_skill_gives_the_model_is_the_one_codex_resolves() {
        assert_eq!(namespaced_tool_name("prefer"), "mcp__roundhouse__prefer");
        assert_eq!(
            namespaced_tool_name("prefer"),
            format!("{DEFAULT_MCP_NAMESPACE}__prefer")
        );
    }

    /// No generated file can carry a secret.
    ///
    /// Structural here in the same way it is for the other two files: nothing
    /// in this module has access to one. Asserted anyway, because the change
    /// that would break it -- "put the turn key in the skill so the model can
    /// show it" -- is exactly the kind of convenience a later edit reaches for,
    /// and these files are markdown that gets copied into dotfile repos.
    #[test]
    fn no_generated_skill_can_carry_a_key() {
        for file in skill_files() {
            assert!(
                !file.contents.contains("rh_turn_") && !file.contents.contains("rh_admin_"),
                "a generated skill must never name a secret:\n{}",
                file.contents
            );
        }
    }

    /// A description containing `: ` must survive the frontmatter round trip.
    ///
    /// The probe is not one of the shipped three -- it is the shape any future
    /// edit could introduce, and the reason [`yaml_scalar`] exists. Unquoted,
    /// `description: Use when: the user asks` parses as a mapping and
    /// `SkillFrontmatter::description` comes back `None`; the skill still loads
    /// and is still listed, with the selection line blank.
    #[test]
    fn a_description_that_contains_a_colon_is_still_one_scalar() {
        let quoted = yaml_scalar("Use when: the user asks, and says \"now\".");
        let decoded: String =
            serde_json::from_str(&quoted).expect("the quoted form decodes as one string");
        assert_eq!(decoded, "Use when: the user asks, and says \"now\".");
        assert!(
            quoted.starts_with('"') && quoted.ends_with('"'),
            "the scalar must be quoted, or YAML reads the colon as a mapping: {quoted}"
        );
    }
}
