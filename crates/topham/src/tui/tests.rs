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

use super::*;
// Named here rather than inherited through `super::*`: the editor holds its
// values as text now, so `tui.rs` itself has no use for the enum.
use crate::profile::{AuthKind, Topology};
use crate::test_support::{AIMED_HERE, RE_AIMED, ROOT, TURN_KEY, relay_double_on_path, scratch};

/// An environment with the turn key exported and both homes under `root`.
///
/// Not [`crate::test_support::env`]: that one fixes the two homes at literals
/// because resolution touches no filesystem, and every case here either writes
/// a profile or lets a precheck write a generated config.
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

/// `apply`, retried past the `ETXTBSY` window the double opens.
///
/// Not a flaky test papered over but a property of writing an executable in a
/// multi-threaded harness, and the same retry `relay::tests` documents at
/// length: another test thread that forks while this one still holds a write
/// handle to the script inherits it, and the kernel refuses to execute the file
/// until that unrelated child reaches its own `exec`. Every other outcome is
/// returned untouched, so a real refusal is never retried into a pass.
fn apply_past_text_file_busy(model: Model, action: Action, env: &EnvMap) -> Model {
    for _ in 0..1_000 {
        let attempted = apply(model.clone(), action.clone(), env);
        if attempted.status.contains("Text file busy") {
            std::thread::yield_now();
            continue;
        }
        return attempted;
    }
    panic!("the double stayed `Text file busy` for a thousand attempts, which is not the race");
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
    assert_eq!(editor.value(Field::Name), "chained");
    assert_eq!(editor.value(Field::Topology), "chained");
    assert_eq!(editor.value(Field::DeploymentRoot), ROOT);
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
    assert_eq!(editor.value(Field::Name), "wor");
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
    assert_eq!(
        editor.value(Field::Agent),
        Agent::Claude.as_str(),
        "typing must not edit an enum"
    );

    let model = keys(model, [press(KeyCode::Right)]);
    assert_eq!(
        model.editor.clone().unwrap().value(Field::Agent),
        Agent::Codex.as_str()
    );
    let model = keys(model, [press(KeyCode::Char(' '))]);
    assert_eq!(
        model.editor.unwrap().value(Field::Agent),
        Agent::Claude.as_str()
    );
}

/// The cursor reaches every field, in both directions, and wraps.
#[test]
fn the_cursor_walks_every_field() {
    let model = keys(listed(), [press(KeyCode::Char('n'))]);
    let mut seen = vec![model.editor.clone().unwrap().field];
    let mut walking = model;
    for _ in 1..Field::all().count() {
        walking = keys(walking, [press(KeyCode::Tab)]);
        seen.push(walking.editor.clone().unwrap().field);
    }
    assert_eq!(
        seen,
        Field::all().collect::<Vec<_>>(),
        "tab must reach every field"
    );
    // One more wraps to the first, and BackTab walks back.
    walking = keys(walking, [press(KeyCode::Tab)]);
    assert_eq!(walking.editor.clone().unwrap().field, Field::Name);
    walking = keys(walking, [press(KeyCode::BackTab)]);
    assert_eq!(walking.editor.unwrap().field, Field::CatalogPath);
}

/// F4: which fields are typed into and which are cycled is now one column of
/// the table rather than two exception lists the compiler never forced anyone
/// to update — a new field used to arrive silently text-editable and
/// non-cycling whether or not that was true of it.
///
/// The property is that `is_text` *is* "this row lists no values", so the two
/// halves can no longer disagree: a field with values cycles and takes no
/// characters, a field without them is typed into and an arrow key leaves it
/// alone.
#[test]
fn the_table_row_decides_whether_a_field_is_typed_or_cycled() {
    for field in Field::all() {
        assert_eq!(
            field.is_text(),
            field.spec().cycles.is_empty(),
            "{field:?}: whether it is text is the row's value list, and nothing else"
        );
    }

    // Driven through the editor, because `cycle` is keyed off the focused
    // field rather than an argument. A text field is left alone by an arrow
    // key; the cycling one beside it is not.
    let model = keys(listed(), [press(KeyCode::Char('n'))]);
    let before = model.editor.clone().unwrap().value(Field::Name);
    let model = keys(model, [press(KeyCode::Right)]);
    assert_eq!(
        model.editor.clone().unwrap().value(Field::Name),
        before,
        "a field whose row lists no values has nothing to cycle to"
    );
    let model = keys(model, [press(KeyCode::Tab), press(KeyCode::Right)]);
    assert_eq!(
        model.editor.unwrap().value(Field::Agent),
        Agent::Codex.as_str(),
        "and one whose row lists values moves through them"
    );
}

/// The table's own claims, checked against the loader rather than asserted.
///
/// Two ways a row can be quietly wrong, and neither is a compile error: the
/// `key` it names is not the key [`Profile`] serializes (so the editor opens
/// blank and a save drops the field), and a value in `cycles` is not one the
/// profile vocabulary has (so composing is refused as an unknown variant, on a
/// value the operator can only have reached by pressing an arrow key). A full
/// round trip catches both at once: every field reads something out of a
/// profile that has every field set, and composing it again is the identity.
#[test]
fn every_table_row_round_trips_through_the_loader() {
    let profile = Profile {
        auth: AuthKind::ForwardedLogin,
        key_env: "MY_TURN_KEY".to_string(),
        topology: Topology::Chained,
        model: Some("a-slug".to_string()),
        model_catalog_path: Some("/op/catalog.json".into()),
        ..Profile::new(Agent::Codex, ROOT)
    };
    let editor = Editor::over("work", &profile, None);
    for field in Field::all() {
        assert!(
            !editor.value(field).is_empty(),
            "{field:?} read nothing out of a profile that sets every field -- its row's \
             key is not the one `Profile` serializes"
        );
    }

    let (name, composed) = editor.compose().expect("what it was opened on composes");
    assert_eq!(name, "work");
    assert_eq!(
        composed, profile,
        "an open-and-compose round trip must be the identity"
    );

    // And every value an arrow key can reach is one the loader accepts. Over a
    // bare profile rather than the one above, because cycling the agent to
    // `claude` while a codex-only slug is set is refused by a cross-field rule
    // that has nothing to do with the table.
    let bare = Editor::over("work", &Profile::new(Agent::Claude, ROOT), None);
    for field in Field::all().filter(|field| !field.is_text()) {
        let mut walking = bare.clone();
        walking.field = field;
        for _ in 0..=field.spec().cycles.len() {
            walking.cycle();
            let reached = walking.value(field);
            let (_, composed) = walking.compose().unwrap_or_else(|why| {
                panic!(
                    "`{}` cycled to `{reached}`, which the loader refuses: {why}",
                    field.label()
                )
            });
            assert_eq!(
                Editor::over("work", &composed, None).value(field),
                reached,
                "`{}` cycled to `{reached}` and the profile came back saying otherwise",
                field.label()
            );
        }
    }
}

/// M12: the claude-only switch survives an edit of an unrelated field.
///
/// The round trip above cannot cover it — its fixture is a codex profile, and
/// `strict-mcp` on one of those is refused — and the failure it guards is the
/// silent one: an editor whose table did not carry the field would compose a
/// document without it, and an operator who opened a strict profile to fix a
/// typo would save a profile that had quietly stopped being strict.
#[test]
fn the_mcp_switch_survives_an_edit_of_something_else() {
    let profile = Profile {
        strict_mcp: true,
        ..Profile::new(Agent::Claude, ROOT)
    };
    let editor = Editor::over("work", &profile, None);
    assert_eq!(editor.value(Field::StrictMcp), "true");
    let (_, composed) = editor.compose().expect("what it was opened on composes");
    assert_eq!(composed, profile);
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
    for _ in 0..5 {
        events.push(press(KeyCode::Tab)); // auth, key-env, topology, strict mcp, model
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

/// F9: raw-mode crossterm reports ^C as `KeyEvent{Char('c'), CONTROL}`, not a
/// signal. `update_edit`'s `Char(c)` arm ignores `key.modifiers`, so a
/// reflexive ^C mid-edit must not be typed as a literal `'c'` into the
/// focused field — that would be corruption a `Backspace` habit does not
/// catch, since ^C does not look like a keystroke on the screen.
#[test]
fn ctrl_c_does_not_type_a_literal_c_into_the_focused_field() {
    let model = keys(listed(), [press(KeyCode::Char('n'))]);
    let model = keys(model, typed("abc"));
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL);
    let model = update(model, ctrl_c);
    let editor = model.editor.expect("the editor is open");
    assert_eq!(
        editor.value(Field::Name),
        "abc",
        "ctrl-c must not be appended as a literal 'c'"
    );
}

/// F9's control: on the list screen there is no keyboard way to interrupt the
/// TUI at all — `update_list` has no `CONTROL` arm, so ^C falls into the
/// wildcard and does nothing, unlike `q`/`Esc`.
#[test]
fn ctrl_c_exits_the_list_screen_like_q_does() {
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL);
    let model = update(listed(), ctrl_c);
    assert!(model.exit, "ctrl-c must exit the TUI like q does");
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
    // The resolved name and profile ride on `leaving` rather than being looked
    // up again after the loop (F10), so they are what this asserts.
    assert_eq!(
        model.leaving,
        Some(Leaving {
            exec: Exec::Launch,
            name: "work".to_string(),
            profile: Profile::new(Agent::Claude, ROOT),
        })
    );
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

/// The chained profile relays — and the precheck got there by writing the
/// generated config and asking a real Relay about it.
///
/// The write is the F22 fix's own cost, and it is bounded: the config the
/// preflight resolves, in the place `relay::run` writes it, and nothing else.
/// No profile is touched, because a precheck is not a save.
#[test]
fn a_chained_profile_leaves_the_screen_to_relay() {
    let root = scratch("relay-ok");
    let mut env = env_at(&root);
    relay_double_on_path(&mut env, &root, AIMED_HERE);
    let mut model = listed();
    model.selected = 1;
    let model = apply_past_text_file_busy(model, Action::Relay, &env);

    assert_eq!(
        model.leaving,
        Some(Leaving {
            exec: Exec::Relay,
            name: "chained".to_string(),
            profile: Profile {
                topology: Topology::Chained,
                ..Profile::new(Agent::Claude, ROOT)
            },
        }),
        "{}",
        model.status
    );
    assert!(
        relay::scratch_dir(&env, "chained")
            .expect("the chained scratch resolves")
            .join("relay-config.toml")
            .exists(),
        "the preflight resolves the generated config, so the precheck writes it"
    );
    assert!(
        !root.join("config").exists(),
        "a precheck writes no profile: the profiles directory is untouched"
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

/// F22: `precheck` ran only `relay::plan`, which builds the handoff and checks
/// the topology and the upstream-override env var -- it never spawns anything.
/// The preflight spawn that fails with `RelayError::PreflightSpawn` (no
/// `nemo-relay` on `PATH`, or a bad `--relay`) lived only in `relay::run`,
/// called after `ratatui::restore()` had already torn the screen down. So a box
/// with no `nemo-relay` anywhere on `PATH` sailed through with
/// `leaving = Some(Relay)` and the screen closed on a refusal it was never
/// asked to check for -- contradicting [`Model::leaving`]'s own doc, that the
/// screen only closes once a subcommand's refusals have all had their chance.
#[test]
fn precheck_refuses_a_missing_relay_binary_on_path() {
    let root = scratch("precheck-missing-relay");
    let mut env = env_at(&root);
    // An empty directory on `PATH`: nowhere the preflight spawn could find
    // `nemo-relay`, matching the finding's own repro (no nemo-relay on PATH).
    let empty_path = scratch("precheck-missing-relay-path");
    env.insert("PATH".to_string(), empty_path.display().to_string());

    let mut model = listed();
    model.selected = 1; // "chained": what `relay::plan` accepts.
    let model = apply(model, Action::Relay, &env);

    assert!(
        model.leaving.is_none(),
        "precheck let a missing nemo-relay binary through unrefused -- it only \
         ran relay::plan, which never spawns anything and so never sees \
         RelayError::PreflightSpawn: leaving = {:?}",
        model.leaving
    );
    assert!(
        model.status.contains("nemo-relay"),
        "the screen should have refused before closing, naming nemo-relay: {}",
        model.status
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&empty_path);
}

/// F22's other half: the screen's precheck and `relay::run` are the *same
/// function* now, so what an operator reads on the screen is what the
/// subcommand would have printed — byte for byte, not merely "similar".
///
/// The re-aimed upstream is the case that pins it. It is the refusal only the
/// spawn can produce, it is composed from the handoff and Relay's own report
/// rather than from a constant, and it was the one the screen would have
/// silently skipped: two hand-written compositions of plan/write/preflight can
/// each be correct on their own and still answer differently — a different
/// order, a preflight home somewhere else, one side passing `--relay` and the
/// other the bare name — and nothing about a screen refusal that is merely
/// *plausible* would ever look wrong.
#[test]
fn the_screens_relay_precheck_refuses_exactly_as_the_subcommand_does() {
    let root = scratch("precheck-parity");
    let mut env = env_at(&root);
    relay_double_on_path(&mut env, &root, RE_AIMED);

    let mut model = listed();
    model.selected = 1; // "chained": what `relay::plan` accepts.
    let model = apply_past_text_file_busy(model, Action::Relay, &env);
    assert_eq!(model.leaving, None, "{}", model.status);

    let chained = Profile {
        topology: Topology::Chained,
        ..Profile::new(Agent::Claude, ROOT)
    };
    let launcher = crate::launch::RecordingLauncher::new();
    let refusal = crate::test_support::past_text_file_busy(|| {
        relay::run(
            &env,
            "chained",
            chained.clone(),
            RELAY_PROGRAM,
            Vec::new(),
            &launcher,
            &mut std::io::sink(),
        )
    })
    .expect_err("the double reports an upstream this launch did not ask for");

    assert_eq!(
        model.status,
        error_chain(&refusal).join(" — "),
        "the screen refused differently from the subcommand it fronts"
    );
    assert!(
        model.status.contains("10.0.0.9"),
        "and the refusal is the re-aim, not something both sides get wrong the same way: {}",
        model.status
    );
    assert!(
        launcher.launched().is_empty(),
        "a refused preflight execs nothing"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// F10: `leaving` promised "one of the two that `exec`, and never any of the
/// others", and `precheck`'s parameter was the whole [`Action`] enum — so
/// `precheck(model, &env, Action::Reload)` compiled, ran *relay's* precheck
/// through the `_ =>` wildcard, and on success stamped `leaving` with `Reload`.
/// Only the two call sites in [`apply`] kept the promise.
///
/// **The half that was failing is now a compile error**, which is the fix: the
/// parameter is [`Exec`] and the field is a [`Leaving`], and neither can be
/// handed a non-exec action at all. What is left to assert at run time is the
/// other half of the same invariant — that the actions which are *not* execs
/// reach no precheck and leave the screen open — and that a precheck which does
/// leave carries the resolved values `run` uses rather than a bare marker.
#[test]
fn only_an_exec_action_ever_sets_leaving() {
    let root = scratch("precheck-wildcard");
    let mut env = env_at(&root);
    relay_double_on_path(&mut env, &root, AIMED_HERE);
    let mut model = listed();
    model.selected = 1; // "chained": what `relay::plan` accepts.

    for action in [Action::Show, Action::Reload] {
        let after = apply(model.clone(), action.clone(), &env);
        assert_eq!(
            after.leaving, None,
            "{action:?} is not an exec and must never leave the screen"
        );
    }

    let model = apply_past_text_file_busy(model, Action::Relay, &env);
    match model.leaving {
        Some(Leaving { exec, name, .. }) => {
            assert_eq!(exec, Exec::Relay);
            assert_eq!(name, "chained", "the resolved name rides along");
        }
        None => panic!(
            "the chained profile should have left for a relay: {}",
            model.status
        ),
    }
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

// ---------------------------------------------------------------------------
// F12: the cursor across exec
// ---------------------------------------------------------------------------

/// A `Write` sink `Rc`-shared with the test, so the bytes a `CrosstermBackend`
/// wrote survive even a `terminal` the test deliberately never drops.
/// `TestBackend` (used above) buffers cells, not the ANSI byte stream, so it
/// cannot see a cursor-visibility escape code at all -- F12 is about exactly
/// those bytes, which only a real `CrosstermBackend` writes.
#[derive(Clone, Default)]
struct SharedBytes(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

impl std::io::Write for SharedBytes {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// F12's control: `Terminal`'s own `Drop` is what writes the show-cursor
/// sequence, and nothing else in the sequence `tui::run` follows does.
///
/// This pins the mechanism the finding rests on, using the exact pinned
/// `ratatui`/`ratatui-core` (0.30.2 / 0.1.2): a `draw` hides the cursor
/// (`ESC[?25l`), and the module's `restore()` -- disabling raw mode and
/// leaving the alternate screen, modeled here as the byte-relevant half,
/// `LeaveAlternateScreen` -- writes no cursor-visibility byte at all. Only
/// once `terminal` actually drops does `ESC[?25h` appear. If this test ever
/// goes red, the ignored test below is asserting a mechanism that no longer
/// holds and both need a second look.
#[test]
fn f12_control_terminal_drop_is_the_only_thing_that_shows_the_cursor() {
    let sink = SharedBytes::default();
    let backend = ratatui::backend::CrosstermBackend::new(sink.clone());
    {
        let mut terminal = ratatui::Terminal::new(backend).expect("a crossterm-backed terminal");
        terminal
            .draw(|_frame| {})
            .expect("a draw that hides the cursor");
        crossterm::execute!(
            terminal.backend_mut(),
            crossterm::terminal::LeaveAlternateScreen
        )
        .expect("the leave-alternate-screen half of ratatui::restore()");
        // `terminal` drops here, at the end of this block -- unlike in
        // `tui::run`, where it stays in scope across the exec below.
    }
    let written = String::from_utf8_lossy(&sink.0.borrow()).into_owned();
    assert!(
        written.contains("\u{1b}[?25l"),
        "the draw must hide the cursor or this test proves nothing: {written:?}"
    );
    assert!(
        written.contains("\u{1b}[?25h"),
        "Terminal::drop must show the cursor once hidden_cursor is set: {written:?}"
    );
}

/// F12: `ratatui::restore()` (`tui.rs:890`) only disables raw mode and leaves
/// the alternate screen -- per the control above, cursor visibility is
/// restored nowhere but `Terminal`'s `Drop`. But `tui::run`'s `terminal`
/// local is still in scope when it calls `launch::run`/`relay::run`, and
/// those `exec` via `std::os::unix::process::CommandExt::exec` --
/// `launch.rs`'s own doc: "`execve` does not return on success". `execve`
/// replaces the process image without unwinding the stack, so no local's
/// destructor runs; `mem::forget` is the standard in-process stand-in for
/// exactly that -- a value whose destructor is skipped rather than run.
///
/// So this drives `run`'s own teardown byte-for-byte over a sink with the
/// destructor deliberately skipped: draw, [`show_cursor`], the
/// leave-alternate-screen half of `restore()`, no drop. The fix is that
/// `ESC[?25h` is in the stream before the forget rather than only in a
/// destructor an `execve` never runs.
#[test]
fn f12_the_cursor_is_shown_before_an_exec_that_skips_the_terminals_drop() {
    let sink = SharedBytes::default();
    let backend = ratatui::backend::CrosstermBackend::new(sink.clone());
    let mut terminal = ratatui::Terminal::new(backend).expect("a crossterm-backed terminal");
    terminal
        .draw(|_frame| {})
        .expect("a draw that hides the cursor");
    // The production teardown, the same call in the same place `run` makes it
    // -- which is what this test is here to pin.
    show_cursor(&mut terminal);
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    )
    .expect("the leave-alternate-screen half of ratatui::restore()");
    // Stands in for `tui::run` calling `launch::run`/`relay::run` while
    // `terminal` is still alive: the real `execve` skips this destructor
    // exactly as `mem::forget` does, for the same reason (no unwind).
    std::mem::forget(terminal);

    let written = String::from_utf8_lossy(&sink.0.borrow()).into_owned();
    assert!(
        written.contains("\u{1b}[?25l"),
        "the draw must hide the cursor or this test proves nothing: {written:?}"
    );
    // The cursor must be visible again before the operator gets a shell back.
    // Drop the `show_cursor` call above -- or `run`'s -- and this goes red with
    // the byte stream the finding captured from the built binary.
    assert!(
        written.contains("\u{1b}[?25h"),
        "the cursor was left hidden: nothing between the draw and the exec ever showed it again: {written:?}"
    );

    // And the call site, which the sequence above cannot reach: `run` opens a
    // real terminal and blocks on `event::read`, so no test in this crate calls
    // it, and a `show_cursor` that nothing on the way out invokes restores
    // nobody's cursor. Scanned as source, in the order the two lines have to be
    // in -- `restore` leaves the alternate screen, and a cursor shown after
    // that is one shown into the wrong screen.
    let source = include_str!("../tui.rs");
    assert!(
        source.contains("show_cursor(&mut terminal);\n    ratatui::restore();"),
        "`run` must show the cursor immediately before it restores the terminal"
    );
}

/// F13: README.md:72 and :698 described the interactive screen as fronting
/// "the same four" subcommands — plan, launch, relay, and **mint** — but
/// [`Action`] has no `Mint` variant: the screen's own [`TuiError::Terminal`]
/// message names `mint` as one of the subcommands to run *instead of* the
/// screen, precisely because the screen cannot do it.
///
/// Pinned in both directions, because either half can drift into the lie: the
/// README must not go back to claiming four, and `Action` must not grow the
/// missing variant without those sentences being rewritten with it. The source
/// scan is a substring rather than a parse, so the second assertion is also
/// tripped by a *comment* mentioning the variant — deliberately, since a
/// comment about a `Mint` action is itself a claim this test exists to keep
/// honest.
#[test]
fn readme_names_the_three_subcommands_the_screen_fronts() {
    let readme = include_str!("../../../../README.md");
    let source = include_str!("../tui.rs");

    assert!(
        !source.contains("Mint"),
        "`Action` grew a mint variant: the README sentences about the screen \
         now undercount what it fronts"
    );
    assert!(
        !readme.contains("the same four"),
        "README claims the screen fronts four subcommands, but `Action` is \
         Show/Save/Reload/Launch/Relay -- mint takes the tenancy arguments a \
         profile does not carry, so the screen has no business holding it"
    );
    assert!(
        readme.contains("plan, launch and relay, on a screen"),
        "the walkthrough must name what the screen actually fronts"
    );
}
