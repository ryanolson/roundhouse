// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The screen, driven by key events and asserted on the model.
//!
//! **No terminal, no backend, not even a `TestBackend`.** Every test here calls
//! [`update`] or [`apply`] and reads a struct, which is the whole reason the
//! state machine was split from the loop: a suite that rendered into a buffer
//! and grepped for a string would be asserting a layout, and a layout is the one
//! part of a TUI that is *supposed* to change. What must not change is which
//! subcommand a key runs and what it is allowed to run it on.
//!
//! The one thing that is asserted about the drawing is that it does not panic
//! and that the pane it draws is the plan's own rendering — see
//! [`the_plan_pane_is_the_subcommands_own_rendering`].

use std::path::PathBuf;

use super::*;

/// A well-shaped turn key, the house fixture form.
const TURN_KEY: &str = "rh_turn_liveAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const ROOT: &str = "http://127.0.0.1:8080";

/// A scratch directory, per the house pattern: the temp dir plus a UUID.
fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("topham-tui-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("a scratch directory");
    root
}

/// An environment with the turn key exported and both homes under `root`.
fn env_at(root: &std::path::Path) -> EnvMap {
    EnvMap::from([
        ("ROUNDHOUSE_API_KEY".to_string(), TURN_KEY.to_string()),
        (
            "XDG_CONFIG_HOME".to_string(),
            root.join("config").display().to_string(),
        ),
        (
            "XDG_DATA_HOME".to_string(),
            root.join("data").display().to_string(),
        ),
    ])
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
}

fn typed(text: &str) -> Vec<KeyEvent> {
    text.chars().map(|c| press(KeyCode::Char(c))).collect()
}

/// Fold a run of keys into a model, the way the loop does minus the effects.
fn keys(mut model: Model, events: impl IntoIterator<Item = KeyEvent>) -> Model {
    for event in events {
        model = update(model, event);
    }
    model
}

fn entry(name: &str, profile: Profile) -> Entry {
    Entry {
        name: name.to_string(),
        profile: Ok(profile),
    }
}

fn listed() -> Model {
    Model::new(vec![
        entry("work", Profile::new(Agent::Claude, ROOT)),
        entry(
            "chained",
            Profile {
                topology: Topology::Chained,
                ..Profile::new(Agent::Claude, ROOT)
            },
        ),
    ])
}

// ---------------------------------------------------------------------------
// The list
// ---------------------------------------------------------------------------

#[test]
fn the_selection_moves_and_wraps() {
    let model = keys(listed(), [press(KeyCode::Down)]);
    assert_eq!(model.selected, 1);
    // Wrapping rather than clamping: a two-entry list where `down` at the
    // bottom did nothing is a list an operator presses twice and then looks for
    // a scrollbar.
    let model = keys(model, [press(KeyCode::Down)]);
    assert_eq!(model.selected, 0);
    let model = keys(model, [press(KeyCode::Up)]);
    assert_eq!(model.selected, 1);
}

/// An empty list is the ordinary first run, and no key may index into it.
#[test]
fn an_empty_list_has_no_selection_and_no_action_to_run() {
    let model = keys(
        Model::default(),
        [
            press(KeyCode::Down),
            press(KeyCode::Up),
            press(KeyCode::Enter),
        ],
    );
    assert_eq!(model.selected, 0);
    assert_eq!(model.selection(), None);
    // The key still asks for the action; it is `actionable` that refuses, in
    // one place, with a message naming what to do instead.
    assert_eq!(model.pending, Some(Action::Show));
    let refused = model.actionable().unwrap_err();
    assert!(refused.contains("no profiles yet"), "{refused}");
}

/// Every key that runs something raises the action for it, and nothing else.
///
/// The R-T6 assertion: what a key *does* is name a subcommand. If a binding
/// ever grew its own behaviour, it would show up here as a model that changed
/// without an action being raised.
#[test]
fn each_action_key_names_the_subcommand_it_runs() {
    for (code, expected) in [
        (KeyCode::Enter, Action::Show),
        (KeyCode::Char('p'), Action::Show),
        (KeyCode::Char('l'), Action::Launch),
        (KeyCode::Char('r'), Action::Relay),
        (KeyCode::Char('R'), Action::Reload),
    ] {
        let model = keys(listed(), [press(code)]);
        assert_eq!(model.pending, Some(expected), "key {code:?}");
        assert_eq!(
            model.screen,
            Screen::List,
            "key {code:?} changed the screen"
        );
        assert!(!model.exit, "key {code:?} exited");
    }
}

/// One key, at most one action: a pending action left behind would be performed
/// a second time by the next arrow press.
#[test]
fn a_key_that_asks_for_nothing_clears_the_previous_ask() {
    let model = keys(listed(), [press(KeyCode::Char('l')), press(KeyCode::Down)]);
    assert_eq!(model.pending, None);
}

/// A key *release* is not a press. A terminal reporting both would otherwise
/// launch twice from one keystroke.
#[test]
fn a_release_event_changes_nothing() {
    let mut release = press(KeyCode::Char('l'));
    release.kind = KeyEventKind::Release;
    let model = update(listed(), release);
    assert_eq!(model.pending, None);
}

#[test]
fn q_leaves_the_screen_without_leaving_anything_behind() {
    let model = keys(listed(), [press(KeyCode::Char('q'))]);
    assert!(model.exit);
    assert_eq!(model.leaving, None);
    assert_eq!(model.pending, None);
}

/// A profile too broken to parse is named, with its parse error, and does not
/// open an editor over invented defaults.
#[test]
fn a_broken_profile_can_be_selected_and_refuses_to_be_edited() {
    let model = Model::new(vec![Entry {
        name: "broken".to_string(),
        profile: Err("expected an equals sign".to_string()),
    }]);
    let model = keys(model, [press(KeyCode::Char('e'))]);
    assert_eq!(model.screen, Screen::List);
    assert_eq!(model.editor, None);
    assert!(
        model.status.contains("broken") && model.status.contains("expected an equals sign"),
        "the status must name the profile and why it could not be read: {}",
        model.status
    );
}

// ---------------------------------------------------------------------------
// The editor
// ---------------------------------------------------------------------------

#[test]
fn e_opens_the_editor_over_the_selected_profiles_saved_fields() {
    let model = keys(listed(), [press(KeyCode::Down), press(KeyCode::Char('e'))]);
    assert_eq!(model.screen, Screen::Edit);
    let editor = model.editor.expect("the editor is open");
    assert_eq!(editor.name, "chained");
    assert_eq!(editor.topology, Topology::Chained);
    assert_eq!(editor.deployment_root, ROOT);
    assert_eq!(editor.opened_as.as_deref(), Some("chained"));
    assert_eq!(editor.field, Field::Name);
}

#[test]
fn typing_edits_the_focused_text_field_and_backspace_undoes_it() {
    let model = keys(listed(), [press(KeyCode::Char('n'))]);
    let mut events = typed("work");
    events.push(press(KeyCode::Backspace));
    let model = keys(model, events);
    let editor = model.editor.expect("the editor is open");
    assert_eq!(editor.name, "wor");
}

/// The three enum fields cycle and never take a character.
#[test]
fn an_enum_field_cycles_and_ignores_typing() {
    let model = keys(listed(), [press(KeyCode::Char('n'))]);
    // Name -> agent.
    let model = keys(model, [press(KeyCode::Tab)]);
    let model = keys(model, typed("zzz"));
    let editor = model.editor.clone().expect("the editor is open");
    assert_eq!(editor.field, Field::Agent);
    assert_eq!(editor.agent, Agent::Claude, "typing must not edit an enum");

    let model = keys(model, [press(KeyCode::Right)]);
    assert_eq!(model.editor.clone().unwrap().agent, Agent::Codex);
    let model = keys(model, [press(KeyCode::Char(' '))]);
    assert_eq!(model.editor.unwrap().agent, Agent::Claude);
}

/// The cursor reaches every field, in both directions, and wraps.
#[test]
fn the_cursor_walks_every_field() {
    let model = keys(listed(), [press(KeyCode::Char('n'))]);
    let mut seen = vec![model.editor.clone().unwrap().field];
    let mut walking = model;
    for _ in 1..Field::ALL.len() {
        walking = keys(walking, [press(KeyCode::Tab)]);
        seen.push(walking.editor.clone().unwrap().field);
    }
    assert_eq!(seen, Field::ALL.to_vec(), "tab must reach every field");
    // One more wraps to the first, and BackTab walks back.
    walking = keys(walking, [press(KeyCode::Tab)]);
    assert_eq!(walking.editor.clone().unwrap().field, Field::Name);
    walking = keys(walking, [press(KeyCode::BackTab)]);
    assert_eq!(walking.editor.unwrap().field, Field::CatalogPath);
}

#[test]
fn esc_discards_the_edit_and_writes_nothing() {
    let model = keys(listed(), [press(KeyCode::Char('e'))]);
    let model = keys(model, typed("-renamed"));
    let model = keys(model, [press(KeyCode::Esc)]);
    assert_eq!(model.screen, Screen::List);
    assert_eq!(model.editor, None);
    assert_eq!(model.pending, None, "esc must not raise a write");
    assert!(
        model.status.contains("nothing was written"),
        "{}",
        model.status
    );
}

/// **The refusal that matters most, reached through the editor.**
///
/// A key typed into a field is a key in a file the moment `enter` is pressed,
/// and the editor must refuse it in the same words the loader does. It does
/// because it *is* the loader: `compose` renders and parses back.
#[test]
fn a_key_typed_into_a_field_is_refused_by_name() {
    let model = keys(listed(), [press(KeyCode::Char('n'))]);
    let mut events = typed("work");
    events.push(press(KeyCode::Tab)); // agent
    events.push(press(KeyCode::Tab)); // deployment root
    events.extend(typed(TURN_KEY));
    events.push(press(KeyCode::Enter));
    let model = keys(model, events);

    assert_eq!(model.pending, None, "a refused profile must not be written");
    assert_eq!(
        model.screen,
        Screen::Edit,
        "the operator stays on the field"
    );
    assert!(
        model.status.contains("deployment-root") && model.status.contains("roundhouse secret"),
        "the refusal must name the field carrying the key: {}",
        model.status
    );
    assert!(
        !model.status.contains(TURN_KEY),
        "a refusal must never copy the value it refused: {}",
        model.status
    );
}

/// A codex-only field on a claude profile is the other refusal `from_toml`
/// owns, and the editor inherits it for free.
#[test]
fn a_codex_field_on_a_claude_profile_is_refused() {
    let model = keys(listed(), [press(KeyCode::Char('n'))]);
    let mut events = typed("work");
    events.push(press(KeyCode::Tab)); // agent (claude)
    events.push(press(KeyCode::Tab)); // deployment root
    events.extend(typed(ROOT));
    for _ in 0..4 {
        events.push(press(KeyCode::Tab)); // auth, key-env, topology, model
    }
    events.extend(typed("gpt-5"));
    events.push(press(KeyCode::Enter));
    let model = keys(model, events);

    assert_eq!(model.pending, None);
    assert!(
        model.status.contains("model") && model.status.contains("codex"),
        "{}",
        model.status
    );
}

#[test]
fn a_nameless_profile_is_refused_before_anything_is_composed() {
    let model = keys(listed(), [press(KeyCode::Char('n')), press(KeyCode::Enter)]);
    assert_eq!(model.pending, None);
    assert!(model.status.contains("needs a name"), "{}", model.status);
}

/// Enter raises the write as an action carrying the composed profile — the
/// editor decides *what* to write and the effect layer decides *where*.
#[test]
fn enter_raises_the_write_with_the_composed_profile() {
    let model = keys(listed(), [press(KeyCode::Char('n'))]);
    let mut events = typed("second");
    events.push(press(KeyCode::Tab)); // agent
    events.push(press(KeyCode::Right)); // -> codex
    events.push(press(KeyCode::Tab)); // deployment root
    events.extend(typed(ROOT));
    events.push(press(KeyCode::Enter));
    let model = keys(model, events);

    match model.pending {
        Some(Action::Save { name, profile }) => {
            assert_eq!(name, "second");
            assert_eq!(profile.agent, Agent::Codex);
            assert_eq!(profile.deployment_root, ROOT);
            assert_eq!(profile.key_env, "ROUNDHOUSE_API_KEY");
        }
        other => panic!("enter must raise a save, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The plan pane
// ---------------------------------------------------------------------------

/// **The pane is the subcommand's rendering, byte for byte.**
///
/// R-T6's rule made checkable: a pane assembled from the profile's fields would
/// pass every other test in this file and would be a second answer to "what
/// does this profile mean" — shown to the operator at the moment they are about
/// to launch it.
#[test]
fn the_plan_pane_is_the_subcommands_own_rendering() {
    let root = scratch("plan-pane");
    let env = env_at(&root);
    let model = apply(listed(), Action::Show, &env);

    assert_eq!(model.screen, Screen::Plan);
    let expected = plan::resolve(&env, "work", Profile::new(Agent::Claude, ROOT))
        .expect("the fixture resolves")
        .render();
    assert_eq!(model.pane.join("\n"), expected.trim_end_matches('\n'));
    assert!(
        !model.pane.join("\n").contains(TURN_KEY),
        "the pane must carry the generator's redaction and never the key"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_plan_pane_scrolls_within_its_own_lines() {
    let mut model = Model::new(vec![]);
    model.screen = Screen::Plan;
    model.pane = vec!["a".into(), "b".into()];

    let model = keys(model, [press(KeyCode::Down), press(KeyCode::Down)]);
    assert_eq!(model.scroll, 1, "the scroll stops at the last line");
    let model = keys(model, [press(KeyCode::Up), press(KeyCode::Up)]);
    assert_eq!(model.scroll, 0, "and at the first");
}

#[test]
fn esc_leaves_the_plan_pane_and_resets_the_scroll() {
    let mut model = listed();
    model.screen = Screen::Plan;
    model.pane = vec!["a".into(), "b".into()];
    model.scroll = 1;
    let model = keys(model, [press(KeyCode::Esc)]);
    assert_eq!(model.screen, Screen::List);
    assert_eq!(model.scroll, 0);
}

/// An unexported turn key is refused where `topham plan` refuses it, with the
/// generator's own sentence rather than a summary of it.
#[test]
fn a_plan_with_no_key_exported_shows_the_subcommands_refusal() {
    let root = scratch("plan-nokey");
    let mut env = env_at(&root);
    env.remove("ROUNDHOUSE_API_KEY");
    let model = apply(listed(), Action::Show, &env);

    assert_eq!(model.screen, Screen::List, "a refused plan opens no pane");
    assert!(
        model.status.contains("ROUNDHOUSE_API_KEY") && model.status.contains("local-only"),
        "{}",
        model.status
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Launch and relay
// ---------------------------------------------------------------------------

/// A launch that would succeed marks the model as leaving, and leaves the
/// spawning to the caller — after the terminal is restored.
#[test]
fn a_direct_profile_leaves_the_screen_to_launch() {
    let root = scratch("launch-ok");
    let env = env_at(&root);
    let model = apply(listed(), Action::Launch, &env);
    assert_eq!(model.leaving, Some(Action::Launch));
    assert!(model.status.is_empty(), "{}", model.status);
    let _ = std::fs::remove_dir_all(&root);
}

/// **The refusal an operator must see on the screen and not after it closes.**
///
/// `topham launch` refuses a chained profile, and the TUI runs that refusal
/// before it decides to tear the screen down — otherwise the message scrolls
/// past under a restored terminal, which is exactly when nobody reads it.
#[test]
fn a_chained_profile_refuses_to_launch_without_leaving_the_screen() {
    let root = scratch("launch-chained");
    let env = env_at(&root);
    let mut model = listed();
    model.selected = 1;
    let model = apply(model, Action::Launch, &env);

    assert_eq!(model.leaving, None, "the screen must stay open");
    assert!(
        model.status.contains("topham relay chained"),
        "the refusal must name the subcommand that does run it: {}",
        model.status
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// And the mirror: `topham relay` refuses a direct profile, in the model.
#[test]
fn a_direct_profile_refuses_to_relay_without_leaving_the_screen() {
    let root = scratch("relay-direct");
    let env = env_at(&root);
    let model = apply(listed(), Action::Relay, &env);
    assert_eq!(model.leaving, None);
    assert!(
        model.status.contains("topham launch work"),
        "{}",
        model.status
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The chained profile relays, and nothing is written or spawned to find that
/// out: the precheck is `relay::plan`, which is pure but for two path joins.
#[test]
fn a_chained_profile_leaves_the_screen_to_relay() {
    let root = scratch("relay-ok");
    let env = env_at(&root);
    let mut model = listed();
    model.selected = 1;
    let model = apply(model, Action::Relay, &env);

    assert_eq!(model.leaving, Some(Action::Relay));
    assert!(
        !root.join("data").exists(),
        "the precheck must write nothing: `relay::plan` renders, it does not save"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Relay's environment override outranks the config a launch passes, so
/// `relay::plan` refuses it — and the screen inherits that refusal too.
#[test]
fn an_ambient_relay_upstream_override_refuses_on_the_screen() {
    let root = scratch("relay-override");
    let mut env = env_at(&root);
    env.insert(
        "NEMO_RELAY_ANTHROPIC_BASE_URL".to_string(),
        "http://192.0.2.9:9999".to_string(),
    );
    let mut model = listed();
    model.selected = 1;
    let model = apply(model, Action::Relay, &env);

    assert_eq!(model.leaving, None);
    assert!(
        model.status.contains("NEMO_RELAY_ANTHROPIC_BASE_URL"),
        "{}",
        model.status
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Saving, listing, and the claim that the model owns no state the files do not
// ---------------------------------------------------------------------------

/// A write goes through `Profile::save`, and the list the screen then shows is
/// **re-read from the directory** rather than patched in memory.
///
/// That is R-T6's "the TUI owns no state the profile files do not", made
/// checkable: the entry the list holds afterwards is compared against a fresh
/// `Profile::load` of the same name, so a model that had cached the composed
/// value instead of the parsed file would only agree by accident.
#[test]
fn a_save_writes_the_file_and_the_list_is_re_read_from_disk() {
    let root = scratch("save");
    let env = env_at(&root);
    let profile = Profile {
        agent: Agent::Codex,
        ..Profile::new(Agent::Codex, ROOT)
    };
    let mut model = Model::new(vec![]);
    model.editor = Some(Editor::over("fresh", &profile, None));
    let model = apply(
        model,
        Action::Save {
            name: "fresh".to_string(),
            profile: profile.clone(),
        },
        &env,
    );

    assert_eq!(model.screen, Screen::List);
    assert_eq!(model.editor, None, "a written editor is closed");
    assert!(model.status.contains("fresh.toml"), "{}", model.status);

    let on_disk = Profile::load(&env, "fresh").expect("the file the save wrote");
    assert_eq!(
        model.entries,
        vec![entry("fresh", on_disk)],
        "the list must be what the directory holds, not what the editor composed"
    );
    assert_eq!(model.selected, 0, "the written profile is selected");
    let _ = std::fs::remove_dir_all(&root);
}

/// A rename writes the new file and removes nothing — and the screen says so,
/// because a launcher that deleted a profile over a typed character is one
/// nobody edits twice.
#[test]
fn a_rename_writes_a_second_file_and_says_the_first_is_still_there() {
    let root = scratch("rename");
    let env = env_at(&root);
    let profile = Profile::new(Agent::Claude, ROOT);
    profile.save(&env, "work").expect("the original");

    let mut model = Model::new(vec![entry("work", profile.clone())]);
    model.editor = Some(Editor::over("work2", &profile, Some("work".to_string())));
    let model = apply(
        model,
        Action::Save {
            name: "work2".to_string(),
            profile,
        },
        &env,
    );

    assert!(
        model.status.contains("`work` is still there"),
        "{}",
        model.status
    );
    assert_eq!(
        model
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        vec!["work", "work2"]
    );
    assert_eq!(model.entries[model.selected].name, "work2");
    let _ = std::fs::remove_dir_all(&root);
}

/// A save the loader would refuse fails on the screen, leaving the file alone.
#[test]
fn a_save_that_the_directory_refuses_is_reported_and_writes_nothing() {
    let root = scratch("save-refused");
    let env = env_at(&root);
    let model = apply(
        Model::new(vec![]),
        Action::Save {
            // A name with a separator in it is one path segment `Profile::path`
            // refuses, and it is refused before any directory is created.
            name: "../escape".to_string(),
            profile: Profile::new(Agent::Claude, ROOT),
        },
        &env,
    );
    assert!(model.status.contains("single filename"), "{}", model.status);
    assert!(!root.join("config").exists(), "nothing was created");
    let _ = std::fs::remove_dir_all(&root);
}

/// One unreadable profile must not hide the other, which is `load_all`'s
/// promise and the list's whole reason to show a broken entry.
#[test]
fn the_listing_keeps_a_broken_profile_from_hiding_the_rest() {
    let root = scratch("listing");
    let env = env_at(&root);
    Profile::new(Agent::Claude, ROOT)
        .save(&env, "good")
        .expect("the readable one");
    let directory = Profile::directory(&env).expect("the profiles directory");
    std::fs::write(directory.join("bad.toml"), "agent = ").expect("the unreadable one");

    let (entries, status) = listing(&env);
    assert_eq!(entries.len(), 2, "both are listed");
    assert!(
        entries[0].profile.is_err(),
        "`bad` sorts first and is broken"
    );
    assert!(entries[1].profile.is_ok(), "`good` is still readable");
    assert!(status.is_empty(), "a listing with entries needs no status");
    let _ = std::fs::remove_dir_all(&root);
}

/// A reload that shortened the list must not leave the cursor past its end.
#[test]
fn a_reload_clamps_a_selection_the_directory_no_longer_has() {
    let root = scratch("reload");
    let env = env_at(&root);
    Profile::new(Agent::Claude, ROOT)
        .save(&env, "only")
        .expect("one profile");

    let mut model = listed();
    model.selected = 1;
    let model = apply(model, Action::Reload, &env);
    assert_eq!(model.entries.len(), 1);
    assert_eq!(model.selected, 0);
    assert!(model.selection().is_some());
    let _ = std::fs::remove_dir_all(&root);
}

/// An unresolvable configuration directory is a status line, not an exit: the
/// operator needs to be told which variable was consulted.
#[test]
fn a_directory_that_cannot_be_resolved_is_reported_on_the_screen() {
    let (entries, status) = listing(&EnvMap::new());
    assert!(entries.is_empty());
    assert!(status.contains("XDG_CONFIG_HOME"), "{status}");
}

// ---------------------------------------------------------------------------
// The drawing
// ---------------------------------------------------------------------------

/// Every screen draws, at a size small enough to expose a layout that assumed
/// room it does not have.
///
/// Deliberately the *only* assertion about the view: what a pane looks like is
/// the part of a TUI that is supposed to change, and a suite that pinned it
/// would go red on every improvement. What must not happen is a panic, which
/// under a restored-on-panic terminal is still an operator losing whatever they
/// were doing.
#[test]
fn every_screen_draws_without_panicking() {
    let mut model = listed();
    model.status = "a refusal long enough to need more room than a narrow pane has".to_string();
    model.pane = (0..40).map(|n| format!("line {n}")).collect();
    model.editor = Some(Editor::blank());

    for screen in [Screen::List, Screen::Edit, Screen::Plan] {
        for size in [(80u16, 24u16), (20, 5)] {
            let backend = ratatui::backend::TestBackend::new(size.0, size.1);
            let mut terminal = ratatui::Terminal::new(backend).expect("a test terminal");
            let mut drawn = model.clone();
            drawn.screen = screen;
            drawn.scroll = 30;
            terminal
                .draw(|frame| view(&drawn, frame))
                .unwrap_or_else(|error| panic!("{screen:?} at {size:?} failed to draw: {error}"));
        }
    }
}

/// An empty list draws too — the first run is the one nobody tests by hand.
#[test]
fn an_empty_list_draws() {
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("a test terminal");
    let model = Model::new(vec![]);
    terminal
        .draw(|frame| view(&model, frame))
        .expect("an empty list draws");
}
