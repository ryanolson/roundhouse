// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The interactive screen — a front end over the subcommands, and nothing else.
//!
//! `topham` with no subcommand opens a profile list, an editor for the fields
//! [`Profile`] carries, a plan pane, and launch/relay actions. R-T6's rule is
//! that **every one of those is a subcommand a script can run**, and this module
//! is written so that the rule is structural rather than remembered:
//!
//! - the plan pane holds [`Resolution::render`](crate::plan::Resolution::render)
//!   and nothing else, so a pane that redacted differently from `topham plan`
//!   would have to be a second renderer somebody wrote on purpose;
//! - the launch and relay actions call [`launch::run`] and [`relay::run`], the
//!   whole subcommands, after the terminal is restored;
//! - the model holds no state the profile files do not, apart from the cursor
//!   and the unsaved buffer in the editor. Nothing is derived and cached: the
//!   list is re-read from disk after every write, so a screen that disagrees
//!   with `ls` is not a state this module can be in.
//!
//! # Why the state machine is pure and the loop is not
//!
//! [`update`] is `(Model, KeyEvent) -> Model` with no I/O in it at all, which is
//! what lets every key binding in this file be tested by calling a function —
//! no terminal, no backend, no spawned process. What a key *causes* is an
//! [`Action`] left on the model; [`apply`] is the half that touches the world,
//! and it takes the environment as a parameter for the same reason everything
//! else in this crate does.
//!
//! The alternative — an `update` that read profiles and resolved plans itself —
//! would have made the whole screen testable only against a real filesystem and
//! a real `$HOME`, which is how a TUI ends up with no tests at all.
//!
//! # Why launch and relay leave the screen before they run
//!
//! Both end in `execve`, and after it this process **is** the agent. A client
//! inheriting a terminal still in raw mode with the alternate screen active
//! would draw over the screen it did not open and leave the operator's shell
//! wrecked when it exits. So [`apply`] does the part of each subcommand that can
//! still refuse — [`launch::plan`] and [`relay::plan`], which write nothing and
//! spawn nothing — and only on success marks the model as
//! [`leaving`](Model::leaving). The loop returns, ratatui restores the terminal,
//! and *then* the real subcommand runs.
//!
//! That ordering is also what makes a refusal readable: a chained profile
//! launched with `l` says so in the status line, on the screen, rather than
//! after a screen tear-down that scrolls it away.
//!
//! # What is not unit-tested here, and why that is the whole of it
//!
//! `event_loop` — `event::read` and the draw call — is the untested
//! remainder, and it is kept to a dozen lines for exactly that reason. Around
//! it, [`run`] owns raw mode and the alternate screen, and `ratatui::try_init`
//! installs the panic hook that restores both, so there is no exit path from
//! this module that leaves a terminal in raw mode: not a return, not a read
//! error, not a panic.

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::cli::error_chain;
use crate::env::EnvMap;
use crate::launch::{self, ExecLauncher, LaunchError};
use crate::plan;
use crate::profile::{self, Agent, AuthKind, Profile, ProfileError, Topology};
use crate::relay::{self, RELAY_PROGRAM, RelayError};

/// Why the interactive screen could not run, or could not finish what it was
/// asked to do after it closed.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error(
        "the interactive screen needs a terminal: {0}. Every action it offers is a subcommand -- \
         run `topham plan`, `topham launch`, `topham relay` or `topham mint` directly"
    )]
    Terminal(#[source] std::io::Error),
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(transparent)]
    Launch(#[from] LaunchError),
    #[error(transparent)]
    Relay(#[from] RelayError),
}

/// Which of the three panes has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// The profile list, and the summary of whichever one is selected.
    #[default]
    List,
    /// The field editor, over one profile's saved or unsaved fields.
    Edit,
    /// One resolution, rendered exactly as `topham plan` prints it.
    Plan,
}

/// One profile as the list shows it.
///
/// The failure is a `String` rather than a [`ProfileError`] deliberately: the
/// model is compared with `==` by every test in this file, and an error that
/// carried a `toml::de::Error` would make the model neither `Clone` nor
/// `PartialEq`. What is lost is the ability to match on the variant, which
/// nothing in a list wants — what a list needs from a broken profile is the
/// sentence to print beside its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub profile: Result<Profile, String>,
}

/// What a key asked for, which the loop performs by calling a subcommand.
///
/// Never performed inside [`update`]: an enum on the model is what keeps the
/// state machine a pure function, and it is also what lets a test assert *which
/// subcommand a key runs* without running it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Resolve the selection and fill the plan pane. `topham plan`.
    Show,
    /// Write the editor's fields. The one action that is not a subcommand of
    /// its own — an operator's editor is `$EDITOR` on a file `topham` refuses
    /// to load if it is wrong, and this is the same write with the same refusal
    /// in front of it.
    Save { name: String, profile: Profile },
    /// Re-read the profiles directory. Raised after a write, and by `R`.
    Reload,
    /// `topham launch <selection>`.
    Launch,
    /// `topham relay <selection>`.
    Relay,
}

/// Everything on the screen, and nothing else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Model {
    pub screen: Screen,
    pub entries: Vec<Entry>,
    /// Index into [`entries`](Self::entries). Clamped by every mutation, so a
    /// reload that shortened the list cannot leave a selection past its end.
    pub selected: usize,
    /// The editor's buffer, present only on [`Screen::Edit`].
    pub editor: Option<Editor>,
    /// The plan pane, as lines. Lines rather than one string so the scroll is a
    /// slice rather than a widget's opinion about wrapping.
    pub pane: Vec<String>,
    pub scroll: usize,
    /// The one-line message under the panes: the last refusal, or the last
    /// thing that happened.
    pub status: String,
    /// What the last key asked for. Taken by the loop, never performed here.
    pub pending: Option<Action>,
    /// The action the loop must restore the terminal and leave for — one of
    /// the two that `exec`, [`Action::Launch`] or [`Action::Relay`], and never
    /// any of the others.
    ///
    /// Set by [`apply`] only after the subcommand's own refusals have all had
    /// their chance, so an operator never watches the screen close on an error.
    pub leaving: Option<Action>,
    pub exit: bool,
}

impl Model {
    /// A model over a listing, with the first profile selected.
    pub fn new(entries: Vec<Entry>) -> Self {
        Self {
            entries,
            ..Self::default()
        }
    }

    /// The selected entry, if the list is not empty.
    pub fn selection(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    /// The selected entry's name and parsed profile, or the reason there is
    /// nothing to act on.
    ///
    /// One accessor for all four actions because all four need the same two
    /// things and fail the same two ways — an empty list and an unreadable
    /// file. Spelling that per action is how three of them end up with a
    /// message and the fourth with a panic.
    pub fn actionable(&self) -> Result<(&str, &Profile), String> {
        let entry = self.selection().ok_or_else(|| {
            "no profiles yet. `n` writes one -- it names a deployment and a variable, never a key"
                .to_string()
        })?;
        match &entry.profile {
            Ok(profile) => Ok((entry.name.as_str(), profile)),
            Err(why) => Err(format!("`{}` cannot be read: {why}", entry.name)),
        }
    }

    /// Take the pending action, leaving none behind.
    pub fn take_pending(&mut self) -> Option<Action> {
        self.pending.take()
    }
}

/// Which field the editor's cursor is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Agent,
    DeploymentRoot,
    Auth,
    KeyEnv,
    Topology,
    Model,
    CatalogPath,
}

impl Field {
    /// Every field, in the order the editor shows them.
    ///
    /// One array rather than a `match` per direction: `next` and `previous` are
    /// derived from it, so a field added to the enum and to this list is a
    /// field the cursor reaches — the failure mode being an editor with a row
    /// nothing can focus.
    pub const ALL: [Field; 8] = [
        Field::Name,
        Field::Agent,
        Field::DeploymentRoot,
        Field::Auth,
        Field::KeyEnv,
        Field::Topology,
        Field::Model,
        Field::CatalogPath,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Field::Name => "profile name",
            Field::Agent => "agent",
            Field::DeploymentRoot => "deployment root",
            Field::Auth => "auth",
            Field::KeyEnv => "key-env",
            Field::Topology => "topology",
            Field::Model => "model (codex)",
            Field::CatalogPath => "catalog path (codex)",
        }
    }

    /// Whether typing edits this field, or cycles it.
    pub fn is_text(self) -> bool {
        !matches!(self, Field::Agent | Field::Auth | Field::Topology)
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|field| *field == self)
            .expect("every field is in ALL")
    }

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn previous(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// One profile's fields, mid-edit.
///
/// Strings for the optional fields rather than `Option<String>`: an empty text
/// field and an absent one are the same thing to a person typing, and the
/// conversion happens once, in [`Editor::compose`], where the profile is built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    pub name: String,
    pub agent: Agent,
    pub deployment_root: String,
    pub auth: AuthKind,
    pub key_env: String,
    pub topology: Topology,
    pub model: String,
    pub catalog_path: String,
    pub field: Field,
    /// The name this editor was opened on, if it was opened on a saved profile.
    ///
    /// Kept so a rename is a *new* file rather than an in-place move nobody
    /// asked for: saving under a changed name writes the new one and leaves the
    /// old, which the status line says. A launcher that silently deleted a
    /// profile because a character was typed into a name field would be a
    /// launcher nobody edits twice.
    pub opened_as: Option<String>,
}

impl Editor {
    /// A blank profile for `n`, carrying the same defaults [`Profile::new`]
    /// carries — read from it rather than restated, so a changed default is
    /// changed once.
    pub fn blank() -> Self {
        Self::over("", &Profile::new(Agent::Claude, ""), None)
    }

    /// An editor over a saved profile.
    pub fn over(name: &str, profile: &Profile, opened_as: Option<String>) -> Self {
        Self {
            name: name.to_string(),
            agent: profile.agent,
            deployment_root: profile.deployment_root.clone(),
            auth: profile.auth,
            key_env: profile.key_env.clone(),
            topology: profile.topology,
            model: profile.model.clone().unwrap_or_default(),
            catalog_path: profile
                .model_catalog_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            field: Field::Name,
            opened_as,
        }
    }

    /// What one field currently reads as, for the view and for a test.
    pub fn value(&self, field: Field) -> String {
        match field {
            Field::Name => self.name.clone(),
            Field::Agent => self.agent.as_str().to_string(),
            Field::DeploymentRoot => self.deployment_root.clone(),
            Field::Auth => self.auth.as_str().to_string(),
            Field::KeyEnv => self.key_env.clone(),
            Field::Topology => self.topology.as_str().to_string(),
            Field::Model => self.model.clone(),
            Field::CatalogPath => self.catalog_path.clone(),
        }
    }

    fn text_mut(&mut self, field: Field) -> Option<&mut String> {
        match field {
            Field::Name => Some(&mut self.name),
            Field::DeploymentRoot => Some(&mut self.deployment_root),
            Field::KeyEnv => Some(&mut self.key_env),
            Field::Model => Some(&mut self.model),
            Field::CatalogPath => Some(&mut self.catalog_path),
            Field::Agent | Field::Auth | Field::Topology => None,
        }
    }

    /// Cycle the focused field's value, if it is one of the three that cycle.
    fn cycle(&mut self) {
        match self.field {
            Field::Agent => {
                self.agent = match self.agent {
                    Agent::Claude => Agent::Codex,
                    Agent::Codex => Agent::Claude,
                }
            }
            Field::Auth => {
                self.auth = match self.auth {
                    AuthKind::RoundhouseKey => AuthKind::ForwardedLogin,
                    AuthKind::ForwardedLogin => AuthKind::RoundhouseKey,
                }
            }
            Field::Topology => {
                self.topology = match self.topology {
                    Topology::Direct => Topology::Chained,
                    Topology::Chained => Topology::Direct,
                }
            }
            _ => {}
        }
    }

    /// The profile these fields describe, or why they do not describe one.
    ///
    /// **Rendered and parsed back through [`Profile::from_toml`]**, rather than
    /// validated here. That is the point: the refusals an operator meets when
    /// they hand-edit a file — a key pasted into a field, a codex-only field on
    /// a claude profile — are the ones they meet in this editor, in the same
    /// words, because it is the same function. A second set of checks written
    /// against the same rules is the thing that eventually disagrees.
    pub fn compose(&self) -> Result<(String, Profile), String> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err(
                "a profile needs a name: it is the filename `topham <cmd> <name>` \
                        resolves"
                    .to_string(),
            );
        }
        let drafted = Profile {
            agent: self.agent,
            deployment_root: self.deployment_root.trim().to_string(),
            auth: self.auth,
            key_env: self.key_env.trim().to_string(),
            topology: self.topology,
            model: some_if_set(&self.model),
            model_catalog_path: some_if_set(&self.catalog_path).map(Into::into),
        };
        let profile = Profile::from_toml(&drafted.to_toml(), &name)
            .map_err(|error| error_chain(&error).join(" — "))?;
        Ok((name, profile))
    }
}

/// An empty field is an absent one. See [`Editor`].
fn some_if_set(value: &str) -> Option<String> {
    let value = value.trim();
    match value.is_empty() {
        true => None,
        false => Some(value.to_string()),
    }
}

// ---------------------------------------------------------------------------
// The state machine
// ---------------------------------------------------------------------------

/// One key event, folded into the model. Pure: no I/O, no environment.
pub fn update(mut model: Model, key: KeyEvent) -> Model {
    // A release is not a press. Terminals that report both would otherwise run
    // every binding twice — `l` launching, and then launching again against a
    // model that has already left.
    if key.kind == KeyEventKind::Release {
        return model;
    }
    // One key, at most one action. Left set, a `Show` that the loop performed
    // would be performed again on the next arrow key.
    model.pending = None;

    match model.screen {
        Screen::List => update_list(model, key),
        Screen::Edit => update_edit(model, key),
        Screen::Plan => update_plan(model, key),
    }
}

fn update_list(mut model: Model, key: KeyEvent) -> Model {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => model.exit = true,
        KeyCode::Down | KeyCode::Char('j') => {
            if !model.entries.is_empty() {
                model.selected = (model.selected + 1) % model.entries.len();
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if !model.entries.is_empty() {
                model.selected = (model.selected + model.entries.len() - 1) % model.entries.len();
            }
        }
        KeyCode::Enter | KeyCode::Char('p') => model.pending = Some(Action::Show),
        KeyCode::Char('l') => model.pending = Some(Action::Launch),
        KeyCode::Char('r') => model.pending = Some(Action::Relay),
        KeyCode::Char('R') => model.pending = Some(Action::Reload),
        KeyCode::Char('n') => {
            model.editor = Some(Editor::blank());
            model.screen = Screen::Edit;
            model.status =
                "a new profile. It names a deployment and a variable, never a key".to_string();
        }
        KeyCode::Char('e') => match model.actionable() {
            Ok((name, profile)) => {
                let editor = Editor::over(name, profile, Some(name.to_string()));
                model.editor = Some(editor);
                model.screen = Screen::Edit;
                model.status.clear();
            }
            // A profile too broken to parse is exactly the one an operator
            // wants to open — and it is the one this editor cannot show,
            // because there are no fields until it parses. Saying so, with the
            // parse error, is the honest answer; inventing defaults for the
            // fields that did parse would overwrite the file with a guess.
            Err(why) => model.status = format!("{why}. Fix it in an editor, or `n` a new one"),
        },
        _ => {}
    }
    model
}

fn update_edit(mut model: Model, key: KeyEvent) -> Model {
    let Some(editor) = model.editor.as_mut() else {
        // Unreachable through the bindings above, and handled rather than
        // asserted: a panic here would take the operator's terminal down with
        // it, which is a worse answer than a status line.
        model.screen = Screen::List;
        return model;
    };
    match key.code {
        KeyCode::Esc => {
            model.editor = None;
            model.screen = Screen::List;
            model.status = "edit cancelled; nothing was written".to_string();
        }
        KeyCode::Tab | KeyCode::Down => editor.field = editor.field.next(),
        KeyCode::BackTab | KeyCode::Up => editor.field = editor.field.previous(),
        KeyCode::Left | KeyCode::Right => editor.cycle(),
        KeyCode::Char(' ') if !editor.field.is_text() => editor.cycle(),
        KeyCode::Char(c) => {
            if let Some(text) = editor.text_mut(editor.field) {
                text.push(c);
            }
        }
        KeyCode::Backspace => {
            if let Some(text) = editor.text_mut(editor.field) {
                text.pop();
            }
        }
        KeyCode::Enter => match editor.compose() {
            Ok((name, profile)) => model.pending = Some(Action::Save { name, profile }),
            Err(why) => model.status = why,
        },
        _ => {}
    }
    model
}

fn update_plan(mut model: Model, key: KeyEvent) -> Model {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            model.screen = Screen::List;
            model.scroll = 0;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            model.scroll = (model.scroll + 1).min(model.pane.len().saturating_sub(1));
        }
        KeyCode::Up | KeyCode::Char('k') => model.scroll = model.scroll.saturating_sub(1),
        KeyCode::Char('l') => model.pending = Some(Action::Launch),
        KeyCode::Char('r') => model.pending = Some(Action::Relay),
        _ => {}
    }
    model
}

// ---------------------------------------------------------------------------
// The effects
// ---------------------------------------------------------------------------

/// Read every profile, as the list shows them.
///
/// A directory that cannot be listed at all is one entry describing that, not
/// an error out of the screen: an operator whose `XDG_CONFIG_HOME` is wrong
/// needs to be told which directory was looked in, and a launcher that exited
/// before drawing anything cannot tell them.
pub fn listing(env: &EnvMap) -> (Vec<Entry>, String) {
    match profile::load_all(env) {
        Ok(listing) => {
            let entries: Vec<Entry> = listing
                .into_iter()
                .map(|(name, profile)| Entry {
                    name,
                    profile: profile.map_err(|error| error_chain(&error).join(" — ")),
                })
                .collect();
            let status = match entries.is_empty() {
                true => "no profiles yet -- `n` writes one".to_string(),
                false => String::new(),
            };
            (entries, status)
        }
        Err(error) => (Vec::new(), error_chain(&error).join(" — ")),
    }
}

/// Perform one action and fold its outcome back into the model.
///
/// The effectful half, and the *only* one: everything above this line is a
/// function of a model and a key. Launch and relay are performed here only as
/// far as they can be without writing or spawning — see the module doc.
pub fn apply(mut model: Model, action: Action, env: &EnvMap) -> Model {
    match action {
        Action::Reload => {
            let (entries, status) = listing(env);
            model.selected = model.selected.min(entries.len().saturating_sub(1));
            model.entries = entries;
            if !status.is_empty() {
                model.status = status;
            }
        }
        Action::Save { name, profile } => match profile.save(env, &name) {
            Ok(path) => {
                let renamed = model
                    .editor
                    .as_ref()
                    .and_then(|editor| editor.opened_as.clone())
                    .filter(|opened| *opened != name);
                model.editor = None;
                model.screen = Screen::List;
                model.status = match renamed {
                    Some(old) => format!(
                        "wrote {} -- `{old}` is still there; a rename writes the new file and \
                         removes nothing",
                        path.display()
                    ),
                    None => format!("wrote {}", path.display()),
                };
                let (entries, _) = listing(env);
                model.entries = entries;
                model.selected = model
                    .entries
                    .iter()
                    .position(|entry| entry.name == name)
                    .unwrap_or(0);
            }
            Err(error) => model.status = error_chain(&error).join(" — "),
        },
        Action::Show => match model.actionable() {
            Ok((name, profile)) => match plan::resolve(env, name, profile.clone()) {
                Ok(resolution) => {
                    model.pane = resolution.render().lines().map(str::to_string).collect();
                    model.scroll = 0;
                    model.screen = Screen::Plan;
                    model.status.clear();
                }
                Err(error) => model.status = error_chain(&error).join(" — "),
            },
            Err(why) => model.status = why,
        },
        // Both arms run the subcommand's own pure prefix, which is where every
        // refusal either one owns already lives -- `launch::plan` refuses a
        // chained profile, `relay::plan` refuses a direct one and an ambient
        // upstream override, and `plan::resolve` under both refuses an
        // unexported key and a suppressor. What is left for after the terminal
        // is restored is the writing and the exec.
        Action::Launch => model = precheck(model, env, Action::Launch),
        Action::Relay => model = precheck(model, env, Action::Relay),
    }
    model
}

/// Everything a launch or a relay can refuse before a terminal has to be
/// restored, run as the subcommand's own functions.
fn precheck(mut model: Model, env: &EnvMap, action: Action) -> Model {
    let (name, profile) = match model.actionable() {
        Ok((name, profile)) => (name.to_string(), profile.clone()),
        Err(why) => {
            model.status = why;
            return model;
        }
    };
    let resolution = match plan::resolve(env, &name, profile) {
        Ok(resolution) => resolution,
        Err(error) => {
            model.status = error_chain(&error).join(" — ");
            return model;
        }
    };
    let refused = match action {
        Action::Launch => launch::plan(&resolution, env, Vec::new())
            .err()
            .map(|error| error_chain(&error).join(" — ")),
        _ => relay::plan(&resolution, env, RELAY_PROGRAM, Vec::new())
            .err()
            .map(|error| error_chain(&error).join(" — ")),
    };
    match refused {
        Some(why) => model.status = why,
        None => model.leaving = Some(action),
    }
    model
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

/// Draw the model. A function of the model and the frame, and of nothing else.
pub fn view(model: &Model, frame: &mut Frame) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("topham", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(format!(
                "  --  {} profile(s); every action here is a subcommand",
                model.entries.len()
            )),
        ])),
        header,
    );

    match model.screen {
        Screen::List => view_list(model, frame, body),
        Screen::Edit => view_edit(model, frame, body),
        Screen::Plan => view_plan(model, frame, body),
    }

    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(hints(model.screen)),
            Line::raw(&*model.status),
        ]),
        footer,
    );
}

/// The key hints for one screen, which are also this screen's documentation.
fn hints(screen: Screen) -> &'static str {
    match screen {
        Screen::List => {
            "up/down select · enter plan · e edit · n new · l launch · r relay · R reload · q quit"
        }
        Screen::Edit => {
            "tab/up/down field · left/right cycle · type to edit · enter save · esc cancel"
        }
        Screen::Plan => "up/down scroll · l launch · r relay · esc back",
    }
}

fn view_list(model: &Model, frame: &mut Frame, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).areas(area);

    let rows: Vec<Line> = model
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let marker = match index == model.selected {
                true => "> ",
                false => "  ",
            };
            let style = match (index == model.selected, entry.profile.is_ok()) {
                (true, _) => Style::new().add_modifier(Modifier::REVERSED),
                (false, true) => Style::new(),
                // A broken profile is dimmed rather than hidden: the list is
                // the surface an operator uses to *find* the broken one.
                (false, false) => Style::new().add_modifier(Modifier::DIM),
            };
            Line::styled(format!("{marker}{}", entry.name), style)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(rows).block(Block::bordered().title(" profiles ")),
        left,
    );

    let detail: Vec<Line> = match model.selection() {
        None => vec![Line::raw("nothing here yet. `n` writes a profile.")],
        Some(entry) => match &entry.profile {
            Err(why) => vec![
                Line::raw(format!("{} could not be read:", entry.name)),
                Line::raw(""),
                Line::raw(why.clone()),
            ],
            Ok(profile) => summary(profile),
        },
    };
    frame.render_widget(
        Paragraph::new(detail)
            .block(Block::bordered().title(" profile "))
            .wrap(Wrap { trim: false }),
        right,
    );
}

/// A saved profile's fields, as the list's right pane shows them.
///
/// The *profile*, not a resolution: nothing here reads the environment, so the
/// list pane can be drawn for a profile whose key is not exported and whose
/// plan therefore refuses. `enter` is where the resolution happens, and where
/// that refusal belongs.
fn summary(profile: &Profile) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::raw(format!("agent            : {}", profile.agent.as_str())),
        Line::raw(format!("topology         : {}", profile.topology.as_str())),
        Line::raw(format!("auth             : {}", profile.auth.as_str())),
        Line::raw(format!("deployment root  : {}", profile.deployment_root)),
        Line::raw(format!("turn key         : read from ${}", profile.key_env)),
    ];
    if profile.agent == Agent::Codex {
        lines.push(Line::raw(format!(
            "model slug       : {}",
            profile.model_slug()
        )));
        lines.push(Line::raw(format!(
            "catalog          : {}",
            profile
                .model_catalog_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(generated beside the config)".to_string())
        )));
    }
    lines.push(Line::raw(""));
    lines.push(Line::raw("enter to resolve this against the environment."));
    lines
}

fn view_edit(model: &Model, frame: &mut Frame, area: Rect) {
    let Some(editor) = model.editor.as_ref() else {
        return;
    };
    let rows: Vec<Line> = Field::ALL
        .iter()
        .map(|field| {
            let focused = *field == editor.field;
            let marker = match focused {
                true => ">",
                false => " ",
            };
            let cursor = match focused && field.is_text() {
                true => "_",
                false => "",
            };
            let style = match focused {
                true => Style::new().add_modifier(Modifier::REVERSED),
                false => Style::new(),
            };
            Line::styled(
                format!(
                    "{marker} {:<22}: {}{cursor}",
                    field.label(),
                    editor.value(*field)
                ),
                style,
            )
        })
        .collect();
    frame.render_widget(
        Paragraph::new(rows).block(Block::bordered().title(" edit -- no secret belongs here ")),
        area,
    );
}

fn view_plan(model: &Model, frame: &mut Frame, area: Rect) {
    // Sliced here rather than handed to the widget's own scroll: the pane is
    // already lines, and a scroll that counted wrapped rows would disagree with
    // the `j`/`k` the state machine is tested against. Wrapping is still on,
    // because the section of this rendering that matters most — the notes — is
    // the section whose lines are longest, and a pane that truncated them would
    // hide the sentence about what a launch cannot enforce.
    let rows: Vec<Line> = model
        .pane
        .iter()
        .skip(model.scroll)
        .map(|line| Line::raw(line.clone()))
        .collect();
    frame.render_widget(
        Paragraph::new(rows)
            .block(Block::bordered().title(" plan -- every secret redacted "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

// ---------------------------------------------------------------------------
// The terminal
// ---------------------------------------------------------------------------

/// Open the screen, and do whatever the operator left it for.
///
/// The two halves are deliberately sequential: raw mode, the alternate screen
/// and the panic hook are all restored before either subcommand below is
/// called. See the module doc.
///
/// **`try_init` rather than `ratatui::run`**, and that is not a style
/// preference: `run` calls `init`, which *panics* when the terminal cannot be
/// entered — and the commonest way to reach this function by accident is to
/// pipe `topham` into something, or to run it from a harness with no tty. A
/// panicking launcher there says "failed to initialize terminal" over a
/// backtrace note; [`TuiError::Terminal`] says which subcommands to run
/// instead. The restore is then ours to place, which is why every path out of
/// this function passes through one.
pub fn run(env: &EnvMap) -> Result<(), TuiError> {
    let (entries, status) = listing(env);
    let mut model = Model::new(entries);
    model.status = status;

    let mut terminal = ratatui::try_init().map_err(|error| {
        // `try_init` enables raw mode *before* it enters the alternate screen,
        // so a failure of the second step leaves the first applied. Undone
        // here -- but with `disable_raw_mode` alone rather than
        // `ratatui::restore`, which also writes a leave-alternate-screen
        // sequence to stdout. The commonest way to arrive here is a piped
        // `topham`, and a launcher that failed to open a screen and then
        // injected an escape sequence into whatever was reading its stdout
        // would have corrupted the output on its way to reporting that it did
        // nothing. Raw mode is a terminal attribute and costs no bytes.
        let _ = crossterm::terminal::disable_raw_mode();
        TuiError::Terminal(error)
    })?;
    let outcome = event_loop(&mut terminal, model, env);
    // Before the `?`, so a read error still hands the operator their terminal
    // back. `restore` reports its own failures to stderr and does not panic,
    // which is the right shape for a teardown running on an error path.
    ratatui::restore();
    let model = outcome.map_err(TuiError::Terminal)?;

    let Some(action) = model.leaving else {
        return Ok(());
    };
    // Answered again rather than carried on the action, and handled rather
    // than asserted: `leaving` is only ever set after `actionable()` answered,
    // so a missing selection here is unreachable -- and a launcher that
    // panicked on the unreachable branch would do it *after* the terminal was
    // restored, where it reads as a crash on exit rather than as a bug.
    let (name, profile) = match model.entries.get(model.selected) {
        Some(entry) => match &entry.profile {
            Ok(profile) => (entry.name.clone(), profile.clone()),
            Err(_) => return Ok(()),
        },
        None => return Ok(()),
    };
    // The whole subcommands, the same functions `topham launch` and `topham
    // relay` call. Neither returns on success.
    match action {
        Action::Launch => {
            launch::run(env, &name, profile, Vec::new(), &ExecLauncher)?;
        }
        Action::Relay => {
            // stderr for the reason `relay::run`'s doc gives, and with one more
            // of its own here: the screen has just been torn down, and the
            // banner is the last thing the operator reads before Relay's own
            // output takes the terminal.
            relay::run(
                env,
                &name,
                profile,
                RELAY_PROGRAM,
                Vec::new(),
                &ExecLauncher,
                &mut std::io::stderr(),
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// Draw, read one key, fold it in, perform what it asked for. The untested
/// remainder — see the module doc.
fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    mut model: Model,
    env: &EnvMap,
) -> std::io::Result<Model> {
    loop {
        terminal.draw(|frame| view(&model, frame))?;
        // A resize is not a key and needs no state change: the next `draw` is
        // the redraw.
        let Event::Key(key) = event::read()? else {
            continue;
        };
        model = update(model, key);
        if model.exit {
            return Ok(model);
        }
        if let Some(action) = model.take_pending() {
            model = apply(model, action, env);
            if model.leaving.is_some() {
                return Ok(model);
            }
        }
    }
}

#[cfg(test)]
mod tests;
