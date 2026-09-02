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
//! still refuse — [`launch::plan`], and for a relay the plan *and* the isolated
//! `--dry-run` preflight, because two of that subcommand's refusals do not
//! exist until Relay itself has been asked — and only on success marks the
//! model as [`leaving`](Model::leaving). The loop returns, ratatui restores the
//! terminal, and *then* the real subcommand runs.
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

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::cli::error_chain;
use crate::env::EnvMap;
use crate::launch::{self, ExecLauncher, LaunchError};
use crate::plan;
use crate::profile::{self, Agent, Profile, ProfileError};
use crate::relay::{self, RELAY_PROGRAM, RelayError};

/// Why the interactive screen could not run, or could not finish what it was
/// asked to do after it closed.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error(
        "the interactive screen needs a terminal. Every action it offers is a subcommand -- run \
         `topham plan`, `topham launch`, `topham relay` or `topham mint` directly"
    )]
    /// The reason the terminal was refused is the `#[source]` and not part of
    /// the sentence (F5). It was `{0}` inline, which [`crate::cli::error_chain`]
    /// then printed a second time as the link below -- and the two readings are
    /// far apart here: "stdout is redirected" and a raw-mode `ioctl` failure
    /// call for different next steps, so the cause is worth its own line rather
    /// than worth saying twice.
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

/// Which of the two subcommands that `exec` a precheck is for.
///
/// Two variants rather than the whole [`Action`], because `precheck` took an
/// `Action` until F10 and a five-variant parameter forced a wildcard: `Show`,
/// `Save` and `Reload` were all prechecked *as relays* and then stamped onto
/// [`Model::leaving`], whose own doc promised only the two that `exec`. That
/// promise was kept by two call sites rather than by a type; now it is the
/// type's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exec {
    Launch,
    Relay,
}

/// What the loop restores the terminal and leaves for, with everything the
/// subcommand needs already resolved.
///
/// The name and the profile ride along rather than being looked up again from
/// `entries[selected]` after the loop returns: `precheck` resolved them
/// through [`Model::actionable`] in order to decide it could leave at all, and
/// a second lookup was a fourth spelling of that accessor carrying two
/// hand-written "unreachable" returns for cases it could no longer reach (F10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaving {
    pub exec: Exec,
    pub name: String,
    pub profile: Profile,
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
    /// What the loop must restore the terminal and leave for — see
    /// [`Leaving`], whose type is what makes "one of the two that `exec`, and
    /// never any of the others" a thing this field cannot hold otherwise.
    ///
    /// Set by [`apply`] only after the subcommand's own refusals have all had
    /// their chance, so an operator never watches the screen close on an error.
    pub leaving: Option<Leaving>,
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

/// One field, described once.
///
/// **The table is the editor** (F4). Adding a field used to mean six edits the
/// compiler forced — the ordering array, `label`, `over`, `value`, `text_mut`,
/// and the struct literal `compose` built — which had to agree with each other,
/// and two more it did *not*: `is_text` and `cycle` were exception lists that
/// defaulted, so a new field arrived silently text-editable and non-cycling
/// whether or not that was true of it, which is the worse half. Here the row
/// carries what is actually different between fields and everything else reads
/// it, so a field is a row plus its variant.
#[derive(Debug, Clone, Copy)]
struct Spec {
    field: Field,
    label: &'static str,
    /// The key this field is in a profile document, and `None` for the profile
    /// *name*, which is the filename rather than anything inside the file.
    key: Option<&'static str>,
    /// The values this field cycles through. **Empty means typing edits it** —
    /// the kind is the list, rather than a second column that could disagree
    /// with it.
    cycles: &'static [&'static str],
    /// Whether an empty value means an absent key rather than an empty one.
    ///
    /// True for exactly the two `Option` fields on [`Profile`]: an empty text
    /// field and an absent one are the same thing to a person typing. False
    /// elsewhere, so that clearing a required field reaches the loader as the
    /// empty value it is and is refused there, rather than silently resolving
    /// to that field's serde default.
    absent_when_empty: bool,
}

/// Every field, in the order the editor shows them.
///
/// `static` rather than `const` so a row can be borrowed for `'static`, and one
/// list rather than one per direction: the cursor's `next`/`previous` are index
/// arithmetic over it, so a field with a row is a field the cursor reaches.
///
/// The cycle values are spelled the way the profile spells them — they are
/// written into the document [`Editor::compose`] hands the loader, so a value
/// this table invented would be refused by `from_toml` as an unknown variant
/// rather than quietly stored. `every_table_row_round_trips_through_the_loader`
/// is what checks that every row's key and values are real.
static FIELDS: [Spec; 8] = [
    Spec {
        field: Field::Name,
        label: "profile name",
        key: None,
        cycles: &[],
        absent_when_empty: false,
    },
    Spec {
        field: Field::Agent,
        label: "agent",
        key: Some("agent"),
        cycles: &["claude", "codex"],
        absent_when_empty: false,
    },
    Spec {
        field: Field::DeploymentRoot,
        label: "deployment root",
        key: Some("deployment-root"),
        cycles: &[],
        absent_when_empty: false,
    },
    Spec {
        field: Field::Auth,
        label: "auth",
        key: Some("auth"),
        cycles: &["roundhouse-key", "forwarded-login"],
        absent_when_empty: false,
    },
    Spec {
        field: Field::KeyEnv,
        label: "key-env",
        key: Some("key-env"),
        cycles: &[],
        absent_when_empty: false,
    },
    Spec {
        field: Field::Topology,
        label: "topology",
        key: Some("topology"),
        cycles: &["direct", "chained"],
        absent_when_empty: false,
    },
    Spec {
        field: Field::Model,
        label: "model (codex)",
        key: Some("model"),
        cycles: &[],
        absent_when_empty: true,
    },
    Spec {
        field: Field::CatalogPath,
        label: "catalog path (codex)",
        key: Some("model-catalog-path"),
        cycles: &[],
        absent_when_empty: true,
    },
];

impl Field {
    /// Every field, in the order the editor shows them.
    pub fn all() -> impl Iterator<Item = Field> {
        FIELDS.iter().map(|spec| spec.field)
    }

    fn spec(self) -> &'static Spec {
        &FIELDS[self.index()]
    }

    fn index(self) -> usize {
        FIELDS
            .iter()
            .position(|spec| spec.field == self)
            .expect("every field has a row in FIELDS")
    }

    pub fn label(self) -> &'static str {
        self.spec().label
    }

    /// Whether typing edits this field, or cycles it.
    pub fn is_text(self) -> bool {
        self.spec().cycles.is_empty()
    }

    pub fn next(self) -> Self {
        FIELDS[(self.index() + 1) % FIELDS.len()].field
    }

    pub fn previous(self) -> Self {
        FIELDS[(self.index() + FIELDS.len() - 1) % FIELDS.len()].field
    }
}

/// One profile's fields, mid-edit.
///
/// **Values keyed by [`Field`], not a second copy of [`Profile`]'s shape.**
/// Mirroring the profile field for field is what forced the per-field arms F4
/// is about; here the editor is what it looks like on the screen — a row per
/// `FIELDS` entry and a cursor — and being a profile again is [`compose`]'s
/// job, done once, through the loader.
///
/// Strings throughout, including for the fields that cycle and the two that are
/// optional: an empty text field and an absent one are the same thing to a
/// person typing, and a half-typed enum is not an enum at all. Every conversion
/// happens in one place, where the document is handed to `Profile::from_toml`.
///
/// [`compose`]: Editor::compose
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    /// One value per [`FIELDS`] row, in that order — the invariant every
    /// accessor below relies on, and the reason nothing constructs an `Editor`
    /// but [`Editor::over`].
    values: Vec<String>,
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
    ///
    /// Filled from the profile's **own rendering** rather than field by field:
    /// `to_toml` is what a saved file is, so what the editor shows is what the
    /// loader would read back, and a field added to [`Profile`] needs no arm
    /// here — only its row in `FIELDS`.
    pub fn over(name: &str, profile: &Profile, opened_as: Option<String>) -> Self {
        let document: toml::Table = toml::from_str(&profile.to_toml())
            .expect("a profile renders as a flat table of strings");
        let values = FIELDS
            .iter()
            .map(|spec| match spec.key {
                None => name.to_string(),
                // A key the document does not carry is an absent optional
                // field, which is an empty row: the two are the same thing
                // here, and turning one into the other is `compose`'s job.
                Some(key) => document
                    .get(key)
                    .and_then(toml::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect();
        Self {
            values,
            field: Field::Name,
            opened_as,
        }
    }

    /// What one field currently reads as, for the view and for a test.
    pub fn value(&self, field: Field) -> String {
        self.values[field.index()].clone()
    }

    fn text_mut(&mut self, field: Field) -> Option<&mut String> {
        match field.is_text() {
            true => Some(&mut self.values[field.index()]),
            false => None,
        }
    }

    /// Move the focused field to the next of the values its row lists.
    ///
    /// A field whose row lists none is left alone, which is what a text field
    /// under an arrow key should do. A value that is in no list starts the walk
    /// over — unreachable from [`Editor::over`], since what it read came out of
    /// a profile that parsed, and cheaper than a panic on a screen.
    fn cycle(&mut self) {
        let cycles = self.field.spec().cycles;
        let Some(first) = cycles.first() else {
            return;
        };
        let slot = &mut self.values[self.field.index()];
        *slot = match cycles.iter().position(|value| value == slot) {
            Some(index) => cycles[(index + 1) % cycles.len()],
            None => first,
        }
        .to_string();
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
        let name = self.value(Field::Name).trim().to_string();
        if name.is_empty() {
            return Err(
                "a profile needs a name: it is the filename `topham <cmd> <name>` \
                        resolves"
                    .to_string(),
            );
        }
        // Built as a document rather than as a struct literal, which is what
        // makes a new field a table row and nothing else (F4). Through
        // `toml`'s own serializer rather than a `format!`, because a value with
        // a quote in it would otherwise close the string and the rest of what
        // was typed would be read as further keys -- a field an operator never
        // touched, set by a stray character in one they did.
        let mut document = toml::Table::new();
        for (spec, value) in FIELDS.iter().zip(&self.values) {
            let Some(key) = spec.key else { continue };
            let value = value.trim();
            if value.is_empty() && spec.absent_when_empty {
                continue;
            }
            document.insert(key.to_string(), toml::Value::String(value.to_string()));
        }
        let text = toml::to_string(&document).expect("a flat table of strings serializes");
        let profile =
            Profile::from_toml(&text, &name).map_err(|error| error_chain(&error).join(" — "))?;
        Ok((name, profile))
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

    // ^C, answered before any screen's own bindings and on every screen.
    //
    // Raw mode turns off `ISIG`, so the terminal never raises `SIGINT` and no
    // handler anywhere in this process will ever see one: ^C arrives here as an
    // ordinary key. Left to the per-screen matches it was two different silent
    // wrongs -- nothing at all on the list, where an operator who opened the
    // screen by accident had no keyboard way out of it, and a literal `'c'`
    // appended to whatever field had the cursor in the editor, which a
    // `Backspace` reflex does not catch because ^C leaves no visible keystroke
    // to undo. Exiting is what an operator means by it, and it is the one
    // meaning that is the same on all three screens.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        model.exit = true;
        return model;
    }

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
        // Both arms run the subcommand's own prefix, which is where every
        // refusal either one owns already lives -- `launch::plan` refuses a
        // chained profile, `relay::plan` refuses a direct one and an ambient
        // upstream override, the relay preflight refuses a missing Relay and a
        // re-aimed upstream, and `plan::resolve` under both refuses an
        // unexported key and a suppressor. What is left for after the terminal
        // is restored is the exec.
        Action::Launch => model = precheck(model, env, Exec::Launch),
        Action::Relay => model = precheck(model, env, Exec::Relay),
    }
    model
}

/// Everything a launch or a relay can refuse before a terminal has to be
/// restored, run as the subcommand's own functions.
fn precheck(mut model: Model, env: &EnvMap, exec: Exec) -> Model {
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
    let refused = match exec {
        Exec::Launch => launch::plan(&resolution, env, Vec::new())
            .err()
            .map(|error| error_chain(&error).join(" — ")),
        // **The subcommand's own prefix, called rather than re-composed**
        // (F22): `relay::plan` alone was not the whole precheck. It is pure, so
        // a missing `nemo-relay` and a system Relay layer that re-aims the
        // upstream both fired later, inside `relay::run` -- which the loop
        // calls only after the screen has been torn down, so the operator
        // watched the screen close and then read the refusal on a restored
        // terminal, which is exactly the ordering `Model::leaving` exists to
        // prevent. `relay::dry_run` is everything `relay::run` does short of
        // the banner and the exec, spawn included; asking Relay what it
        // resolved is the only way to learn either answer.
        //
        // The cost is the generated config written here and one spawn repeated
        // when the launch does go ahead -- the same rendering of the same
        // profile into the place `relay::run` writes it, so the launch's own
        // write is the same bytes again. No profile is touched: a precheck is
        // not a save.
        //
        // `--relay` stays a flag on the subcommand and is not offered here: it
        // names *this box's* binary, and a profile names a deployment (R-T2). A
        // box with no `nemo-relay` on `PATH` now gets a refusal on the screen
        // that says so, and `topham relay <name> --relay <path>` answers it.
        //
        // It resolves a second time, which is deliberate: the value of calling
        // the subcommand's entry point is that the screen runs *it* and not a
        // variant of it, and a `dry_run` taking an already-resolved profile
        // would be a second entry point to keep in step with the first. The
        // resolve is pure apart from reading the settings files, and the
        // profile handed back is the one the first resolve already round-tripped.
        Exec::Relay => relay::dry_run(
            env,
            &resolution.name,
            resolution.profile.clone(),
            RELAY_PROGRAM,
            Vec::new(),
        )
        .err()
        .map(|error| error_chain(&error).join(" — ")),
    };
    match refused {
        Some(why) => model.status = why,
        None => {
            model.leaving = Some(Leaving {
                exec,
                name: resolution.name,
                profile: resolution.profile,
            })
        }
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
    let rows: Vec<Line> = Field::all()
        .map(|field| {
            let focused = field == editor.field;
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
                    editor.value(field)
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
    // **The piped refusal, asked of stdout rather than left to `try_init`.**
    //
    // crossterm answers "is there a terminal" by opening `/dev/tty`, so a
    // `topham > out.txt` run from a shell still had one: `try_init` succeeded
    // and the whole screen was drawn into the file, 47 bytes of
    // alternate-screen escape sequence first (F25). The contract this launcher
    // documents is the stronger one -- a piped `topham` refuses, names the
    // subcommands, and writes nothing to stdout -- and stdout is the stream the
    // screen draws into, so stdout is what has to be a terminal for the screen
    // to be a screen.
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return Err(TuiError::Terminal(std::io::Error::other(
            "stdout is redirected, and the screen draws into stdout",
        )));
    }

    let (entries, status) = listing(env);
    let mut model = Model::new(entries);
    model.status = status;

    let mut terminal = ratatui::try_init().map_err(|error| {
        // `try_init` enables raw mode *before* it enters the alternate screen,
        // so a failure of the second step leaves the first applied. Undone
        // here -- but with `disable_raw_mode` alone rather than
        // `ratatui::restore`, which also writes a leave-alternate-screen
        // sequence to stdout. A launcher that failed to open a screen and then
        // injected an escape sequence into whatever was reading its stdout
        // would have corrupted the output on its way to reporting that it did
        // nothing -- and while the redirected case is refused above, a terminal
        // that refuses raw mode still reaches here. Raw mode is a terminal
        // attribute and costs no bytes.
        let _ = crossterm::terminal::disable_raw_mode();
        TuiError::Terminal(error)
    })?;
    let outcome = event_loop(&mut terminal, model, env);
    // Before the `?`, so a read error still hands the operator their terminal
    // back. `restore` reports its own failures to stderr and does not panic,
    // which is the right shape for a teardown running on an error path.
    show_cursor(&mut terminal);
    ratatui::restore();
    let model = outcome.map_err(TuiError::Terminal)?;

    // Carried on the value rather than looked up again from
    // `entries[selected]`: the precheck resolved this name and this profile in
    // order to decide it could leave at all, and re-deriving them here was a
    // fourth spelling of `actionable()` with two hand-written "unreachable"
    // returns for the empty list and the unparseable file it could no longer
    // be handed (F10).
    let Some(Leaving {
        exec,
        name,
        profile,
    }) = model.leaving
    else {
        return Ok(());
    };
    // The whole subcommands, the same functions `topham launch` and `topham
    // relay` call. Neither returns on success.
    match exec {
        Exec::Launch => {
            launch::run(env, &name, profile, Vec::new(), &ExecLauncher)?;
        }
        Exec::Relay => {
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
    }
    Ok(())
}

/// Make the cursor visible again, which nothing else on the way out does.
///
/// Every `draw` hides the cursor, `ratatui::restore()` disables raw mode and
/// leaves the alternate screen and writes no cursor byte at all, and the one
/// place in ratatui that shows it again is `Terminal`'s own `Drop` — which
/// [`run`] never reaches, because both subcommands below end in `execve` and
/// that replaces the process image without unwinding the stack. So a launcher
/// that left this to the destructor handed the operator a shell with an
/// invisible cursor for the rest of its life, and only for the child processes
/// that did not happen to manage the cursor themselves: a Relay failing its own
/// version gate, a client failing at startup, any `-p`-style run (F12).
///
/// Generic over the backend rather than taking a [`ratatui::DefaultTerminal`]
/// so the byte stream this writes is observable by a test over a sink — the
/// sequence is the whole of the behaviour, and `TestBackend` buffers cells
/// rather than bytes.
fn show_cursor<B: ratatui::backend::Backend>(terminal: &mut ratatui::Terminal<B>) {
    // Ignored for the reason `restore` ignores its own: this runs on the way
    // out, including the error path, and a teardown that failed loudly would
    // replace the operator's real error with its own.
    let _ = terminal.show_cursor();
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
