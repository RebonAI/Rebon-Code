//! The keyboard on what a Claude Code mod draws.
//!
//! A mod's site — the band above the prompt, or one docked pane — takes the
//! keyboard the way it does in Claude Code: after ctrl+x tab (each further
//! ctrl+x tab walks to the next site, and past the last one back to the
//! prompt), after a click on one of its controls, or when a pane the mod
//! opened with `focus` is first drawn. While it holds the keyboard:
//!
//! * Tab and Down move the ring to the next control, Shift+Tab and Up to the
//!   one before; Left and Right do the same, except on a Select, which they
//!   turn through its options;
//! * Enter presses a Button or Link, submits an Input (`onSubmit`) and picks
//!   the option a Select is turned to (`onSelect`); Space presses a Button;
//! * typing edits the Input under the ring, each edit an `onInput`;
//! * a Button's `hotkey` presses it from anywhere in its site;
//! * Esc gives the keyboard back to the prompt, closing a pane opened with
//!   `closeOnEscape`; Ctrl+C gives it back and goes on to do its own work.
//!
//! A `Client` (a mod's surface module) under the ring takes every key but
//! Esc, which gives the keyboard back; a click inside it is a pointer event
//! at the cell clicked.
//!
//! Every move of the ring is raised as the mod's `ui.focus` first: a hook
//! may keep the ring where it was or put it on another of its elements, and
//! its answer comes back as a [`ModFocusOutcome`] the loop applies. A mod's
//! own `$.ui.focus` moves the ring while its site holds the keyboard.
//!
//! The controls and their order are the ones the last frame drew
//! (`AppState::mods_hits`), so the ring walks what the person sees. What the
//! person does lands in `AppState::mods_acts` for the loop to hand to the mod.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{json, Value};

use crate::tui::app::{AppState, ModAct, ModFocus, ModFocusOutcome, ModHit, ModHitKind, ModSite};

use super::render::shown_choice;

/// Offers a key to the mod site holding the keyboard, or to the focus chord.
/// `true` when the key was taken.
pub(super) fn mods_key(app: &mut AppState, key: &KeyEvent) -> bool {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return false;
    }
    // A key the site takes ends the loop's pass early, so the settling the
    // pass does later may not have run since the site went away: settle here
    // too, or a pane the mod closed would go on swallowing what is typed.
    settle_focus(app);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if std::mem::take(&mut app.mods_chord_armed) && key.code == KeyCode::Tab {
        next_site(app);
        return true;
    }
    if ctrl && key.code == KeyCode::Char('x') && !app.mods_sites.is_empty() {
        app.mods_chord_armed = true;
        return true;
    }
    if app.mods_focus.is_none() {
        return false;
    }
    if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')) {
        app.mods_focus = None;
        return false;
    }
    let ring = ring_hit(app);
    if key.code != KeyCode::Esc {
        if let Some(hit) = ring.as_ref().filter(|hit| hit.kind == ModHitKind::Client) {
            if let Some(event) = client_key_event(key) {
                app.mods_acts.push(ModAct::ClientKey {
                    site: hit.site.clone(),
                    element: hit.element.clone(),
                    key: event,
                });
            }
            return true;
        }
    }
    match key.code {
        KeyCode::Esc => release(app),
        KeyCode::Tab | KeyCode::Down => move_ring(app, 1),
        KeyCode::BackTab | KeyCode::Up => move_ring(app, -1),
        KeyCode::Left | KeyCode::Right => {
            let step = if key.code == KeyCode::Right { 1 } else { -1 };
            match &ring {
                Some(hit) if matches!(hit.kind, ModHitKind::Select { .. }) => turn(app, hit, step),
                _ => move_ring(app, step),
            }
        }
        KeyCode::Enter => {
            if let Some(hit) = ring {
                activate(app, &hit);
            }
        }
        KeyCode::Backspace => {
            if let Some(hit) = ring.filter(|hit| matches!(hit.kind, ModHitKind::Input { .. })) {
                edit(app, &hit, |draft| {
                    draft.pop();
                });
            }
        }
        KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => match ring {
            Some(hit) if matches!(hit.kind, ModHitKind::Input { .. }) => {
                edit(app, &hit, |draft| draft.push(ch));
            }
            Some(hit) if ch == ' ' && matches!(hit.kind, ModHitKind::Button { .. }) => {
                activate(app, &hit);
            }
            _ => {
                if let Some(hit) = hotkey_hit(app, ch.to_ascii_lowercase()) {
                    set_ring(app, &hit.element);
                    activate(app, &hit);
                }
            }
        },
        // Everything else stays with the site: it holds the keyboard.
        _ => {}
    }
    true
}

/// A click on a mod's control at `column`, `row`: its site takes the
/// keyboard with the ring on it, a Button or Link is pressed, and a Client
/// hears a pointer press and release at the cell inside its region.
pub(super) fn mods_click(app: &mut AppState, hit: ModHit, column: u16, row: u16) {
    take_focus(app, hit.site.clone());
    set_ring(app, &hit.element);
    match hit.kind {
        ModHitKind::Button { .. } | ModHitKind::Link { .. } => activate(app, &hit),
        ModHitKind::Client => {
            let (left, top) = region_origin(app, &hit);
            let (x, y) = (column.saturating_sub(left), row.saturating_sub(top));
            for kind in ["down", "up"] {
                app.mods_acts.push(ModAct::ClientPointer {
                    site: hit.site.clone(),
                    element: hit.element.clone(),
                    pointer: json!({ "type": kind, "x": x, "y": y, "button": "left" }),
                });
            }
        }
        ModHitKind::Input { .. } | ModHitKind::Select { .. } => {}
    }
}

/// The top-left cell of a Client's region: the least of its drawn rows.
fn region_origin(app: &AppState, hit: &ModHit) -> (u16, u16) {
    app.mods_hits
        .iter()
        .filter(|other| other.site == hit.site && other.element == hit.element)
        .fold((hit.area.x, hit.area.y), |(x, y), other| {
            (x.min(other.area.x), y.min(other.area.y))
        })
}

/// What a mod's `ui.focus` hooks answered about a move of the ring: kept
/// where it was, put elsewhere, or let land. Only the move still standing is
/// changed: one the person has since moved past is left alone.
pub(super) fn apply_focus_outcome(app: &mut AppState, outcome: ModFocusOutcome) {
    let Some(focus) = app.mods_focus.as_mut() else {
        return;
    };
    if focus.site != outcome.site || focus.element != outcome.asked {
        return;
    }
    match outcome.answer {
        Err(_) => focus.element = outcome.previous,
        Ok(landed) if landed.is_some() && landed != outcome.asked => focus.element = landed,
        Ok(_) => {}
    }
}

/// A mod's `$.ui.focus({ requestId, key })`: the ring goes there when that
/// mod's site holds the keyboard, and the ask is dropped otherwise.
pub(super) fn apply_focus_request(app: &mut AppState, plugin: &str, request_id: &str, key: &str) {
    let holds = app
        .mods_focus
        .as_ref()
        .is_some_and(|focus| focus.site.plugin == plugin && focus.site.request_id == request_id);
    if holds {
        move_ring_to(app, Some(key.to_owned()), true);
    }
}

/// A key as a surface module reads it (`ClientKeyEvent`): a special key's
/// name or the character typed, with the modifiers held. `None` for a key
/// that names nothing a module could read.
fn client_key_event(key: &KeyEvent) -> Option<Value> {
    let name = match key.code {
        KeyCode::Up => "up".to_owned(),
        KeyCode::Down => "down".to_owned(),
        KeyCode::Left => "left".to_owned(),
        KeyCode::Right => "right".to_owned(),
        KeyCode::Enter => "return".to_owned(),
        KeyCode::Tab | KeyCode::BackTab => "tab".to_owned(),
        KeyCode::Backspace => "backspace".to_owned(),
        KeyCode::Delete => "delete".to_owned(),
        KeyCode::PageUp => "pageup".to_owned(),
        KeyCode::PageDown => "pagedown".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        KeyCode::Char(' ') => "space".to_owned(),
        KeyCode::Char(ch) => ch.to_string(),
        _ => return None,
    };
    let mut event = json!({ "key": name });
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        event["ctrl"] = json!(true);
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) || key.code == KeyCode::BackTab {
        event["shift"] = json!(true);
    }
    if key.modifiers.contains(KeyModifiers::ALT) || key.modifiers.contains(KeyModifiers::META) {
        event["meta"] = json!(true);
    }
    Some(event)
}

/// Once a pass: a pane that asked for the keyboard takes it once it is drawn,
/// a site that is no longer drawn lets it go, and a site that just took it
/// puts the ring on its `autoFocus` control.
pub(super) fn settle_focus(app: &mut AppState) {
    if let Some(wanted) = app.mods_focus_wanted.clone() {
        if app.mods_sites.contains(&wanted) {
            app.mods_focus_wanted = None;
            take_focus(app, wanted);
        }
    }
    let Some(focus) = app.mods_focus.as_ref() else {
        return;
    };
    if !app.mods_sites.contains(&focus.site) {
        app.mods_focus = None;
        return;
    }
    if focus.element.is_none() && !focus.moved {
        let auto = app
            .mods_hits
            .iter()
            .find(|hit| hit.site == focus.site && hit.auto_focus)
            .map(|hit| hit.element.clone());
        if let Some(element) = auto {
            move_ring_to(app, Some(element), true);
            if let Some(focus) = app.mods_focus.as_mut() {
                // Still the autoFocus: a ring the person has not moved.
                focus.moved = false;
            }
        }
    }
}

/// The site's controls in the order the last frame drew them, each once.
fn controls<'a>(app: &'a AppState, site: &ModSite) -> Vec<&'a ModHit> {
    let mut seen = std::collections::HashSet::new();
    app.mods_hits
        .iter()
        .filter(|hit| hit.site == *site && seen.insert(hit.element.as_str()))
        .collect()
}

fn ring_hit(app: &AppState) -> Option<ModHit> {
    let focus = app.mods_focus.as_ref()?;
    let element = focus.element.as_deref()?;
    controls(app, &focus.site)
        .into_iter()
        .find(|hit| hit.element == element)
        .cloned()
}

fn hotkey_hit(app: &AppState, ch: char) -> Option<ModHit> {
    let focus = app.mods_focus.as_ref()?;
    // Two Buttons naming one hotkey: the later one wins, as in Claude Code.
    controls(app, &focus.site)
        .into_iter()
        .filter(|hit| matches!(hit.kind, ModHitKind::Button { hotkey: Some(key) } if key == ch))
        .last()
        .cloned()
}

/// `site` takes the keyboard. Taking it again keeps what was typed there.
fn take_focus(app: &mut AppState, site: ModSite) {
    if app
        .mods_focus
        .as_ref()
        .is_some_and(|focus| focus.site == site)
    {
        return;
    }
    let close_on_escape = app
        .mods_panes
        .iter()
        .any(|pane| pane.site() == site && pane.close_on_escape);
    app.mods_focus = Some(ModFocus {
        site,
        close_on_escape,
        ..ModFocus::default()
    });
}

/// ctrl+x tab: the next drawn site, or back to the prompt after the last.
fn next_site(app: &mut AppState) {
    let at = app
        .mods_focus
        .as_ref()
        .and_then(|focus| app.mods_sites.iter().position(|site| *site == focus.site));
    let next = match at {
        Some(index) => app.mods_sites.get(index + 1).cloned(),
        None => app.mods_sites.first().cloned(),
    };
    app.mods_focus = None;
    if let Some(site) = next {
        take_focus(app, site);
        settle_focus(app);
    }
}

fn release(app: &mut AppState) {
    let Some(focus) = app.mods_focus.take() else {
        return;
    };
    if focus.close_on_escape && focus.site.component == "Pane" {
        app.mods_acts.push(ModAct::ClosePane {
            plugin: focus.site.plugin,
            id: focus.site.request_id,
        });
    }
}

fn set_ring(app: &mut AppState, element: &str) {
    move_ring_to(app, Some(element.to_owned()), false);
}

/// Puts the ring on `element`, raising the move as the mod's `ui.focus`
/// when it is one.
fn move_ring_to(app: &mut AppState, element: Option<String>, by_plugin: bool) {
    let Some(focus) = app.mods_focus.as_mut() else {
        return;
    };
    focus.moved = true;
    if focus.element == element {
        return;
    }
    let previous = std::mem::replace(&mut focus.element, element.clone());
    let site = focus.site.clone();
    app.mods_acts.push(ModAct::Focus {
        site,
        element,
        previous,
        by_plugin,
    });
}

/// The ring `step` controls on, wrapping; from nowhere, Tab starts at the
/// first and Shift+Tab at the last.
fn move_ring(app: &mut AppState, step: isize) {
    let Some(focus) = app.mods_focus.as_ref() else {
        return;
    };
    let elements: Vec<String> = controls(app, &focus.site)
        .into_iter()
        .map(|hit| hit.element.clone())
        .collect();
    if elements.is_empty() {
        return;
    }
    let len = elements.len() as isize;
    let next = match focus
        .element
        .as_deref()
        .and_then(|element| elements.iter().position(|e| e == element))
    {
        Some(index) => (index as isize + step).rem_euclid(len),
        None if step > 0 => 0,
        None => len - 1,
    };
    set_ring(app, &elements[next as usize]);
}

/// Turns a Select `step` options on, wrapping; Enter picks what it shows.
fn turn(app: &mut AppState, hit: &ModHit, step: isize) {
    let ModHitKind::Select { options, value } = &hit.kind else {
        return;
    };
    let Some(focus) = app.mods_focus.as_mut() else {
        return;
    };
    let turned = focus.choices.get(&hit.element).copied();
    let Some(shown) = shown_choice(options, value.as_deref(), turned) else {
        return;
    };
    let next = (shown as isize + step).rem_euclid(options.len() as isize) as usize;
    focus.choices.insert(hit.element.clone(), next);
}

/// One edit of an Input's text, said to the mod as a `change`.
fn edit(app: &mut AppState, hit: &ModHit, change: impl FnOnce(&mut String)) {
    let ModHitKind::Input { value } = &hit.kind else {
        return;
    };
    let Some(focus) = app.mods_focus.as_mut() else {
        return;
    };
    let draft = focus
        .drafts
        .entry(hit.element.clone())
        .or_insert_with(|| value.clone());
    let before = draft.clone();
    change(draft);
    if *draft == before {
        return;
    }
    let value = draft.clone();
    app.mods_acts.push(ModAct::Input {
        site: hit.site.clone(),
        element: hit.element.clone(),
        kind: "change",
        value,
    });
}

/// Enter on the control under the ring (or its hotkey, or a click).
fn activate(app: &mut AppState, hit: &ModHit) {
    let act = match &hit.kind {
        ModHitKind::Button { .. } => ModAct::Press {
            site: hit.site.clone(),
            element: hit.element.clone(),
            href: None,
        },
        ModHitKind::Link { href } => ModAct::Press {
            site: hit.site.clone(),
            element: hit.element.clone(),
            href: href.clone(),
        },
        ModHitKind::Input { value } => {
            let typed = app
                .mods_focus
                .as_mut()
                .and_then(|focus| focus.drafts.remove(&hit.element));
            ModAct::Input {
                site: hit.site.clone(),
                element: hit.element.clone(),
                kind: "submit",
                value: typed.unwrap_or_else(|| value.clone()),
            }
        }
        ModHitKind::Select { options, value } => {
            let turned = app
                .mods_focus
                .as_mut()
                .and_then(|focus| focus.choices.remove(&hit.element));
            let Some(index) = shown_choice(options, value.as_deref(), turned) else {
                return;
            };
            ModAct::Select {
                site: hit.site.clone(),
                element: hit.element.clone(),
                value: options[index].value.clone(),
            }
        }
        // A Client takes keys, not Enter as a press.
        ModHitKind::Client => return,
    };
    app.mods_acts.push(act);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{ModSelectOption, TerminalModPane};
    use ratatui::layout::Rect;

    fn band() -> ModSite {
        ModSite {
            plugin: "m".into(),
            component: "AbovePrompt".into(),
            request_id: "AbovePrompt".into(),
        }
    }

    fn pane_site(id: &str) -> ModSite {
        ModSite {
            plugin: "m".into(),
            component: "Pane".into(),
            request_id: id.into(),
        }
    }

    fn hit(site: &ModSite, element: &str, kind: ModHitKind) -> ModHit {
        ModHit {
            area: Rect::new(0, 0, 4, 1),
            site: site.clone(),
            element: element.into(),
            kind,
            auto_focus: false,
        }
    }

    fn button(site: &ModSite, element: &str, hotkey: Option<char>) -> ModHit {
        hit(site, element, ModHitKind::Button { hotkey })
    }

    fn input(site: &ModSite, element: &str, value: &str) -> ModHit {
        hit(
            site,
            element,
            ModHitKind::Input {
                value: value.into(),
            },
        )
    }

    fn select(site: &ModSite, element: &str, value: Option<&str>) -> ModHit {
        hit(
            site,
            element,
            ModHitKind::Select {
                options: ["low", "mid", "high"]
                    .iter()
                    .map(|v| ModSelectOption {
                        label: v.to_uppercase(),
                        value: (*v).into(),
                    })
                    .collect(),
                value: value.map(str::to_owned),
            },
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    /// An app whose last frame drew `hits` on `sites`.
    fn drawn(sites: Vec<ModSite>, hits: Vec<ModHit>) -> AppState {
        let mut app = AppState::new();
        app.mods_sites = sites;
        app.mods_hits = hits;
        app
    }

    /// The acts other than the moves of the ring, which `ui.focus` hears.
    fn acts(app: &AppState) -> Vec<&ModAct> {
        app.mods_acts
            .iter()
            .filter(|act| !matches!(act, ModAct::Focus { .. }))
            .collect()
    }

    fn acts_owned(app: &AppState) -> Vec<ModAct> {
        acts(app).into_iter().cloned().collect()
    }

    fn ring(app: &AppState) -> Option<&str> {
        app.mods_focus.as_ref()?.element.as_deref()
    }

    #[test]
    fn keys_pass_by_while_no_site_holds_the_keyboard() {
        let mut app = drawn(vec![band()], vec![button(&band(), "go", None)]);
        assert!(!mods_key(&mut app, &key(KeyCode::Tab)));
        assert!(!mods_key(&mut app, &key(KeyCode::Char('a'))));
        assert!(acts(&app).is_empty());
    }

    #[test]
    fn ctrl_x_without_a_drawn_site_is_not_taken() {
        let mut app = drawn(Vec::new(), Vec::new());
        assert!(!mods_key(&mut app, &ctrl('x')));
        assert!(!app.mods_chord_armed);
    }

    #[test]
    fn ctrl_x_tab_walks_the_sites_and_then_back_to_the_prompt() {
        let sites = vec![band(), pane_site("p")];
        let mut app = drawn(
            sites.clone(),
            vec![
                button(&band(), "a", None),
                button(&pane_site("p"), "b", None),
            ],
        );
        for expected in [Some(&sites[0]), Some(&sites[1]), None] {
            assert!(mods_key(&mut app, &ctrl('x')));
            assert!(mods_key(&mut app, &key(KeyCode::Tab)));
            assert_eq!(app.mods_focus.as_ref().map(|focus| &focus.site), expected);
        }
    }

    #[test]
    fn a_chord_not_finished_by_tab_lets_the_key_go_on() {
        let mut app = drawn(vec![band()], vec![button(&band(), "a", None)]);
        assert!(mods_key(&mut app, &ctrl('x')));
        assert!(!mods_key(&mut app, &key(KeyCode::Char('q'))));
        assert!(app.mods_focus.is_none() && !app.mods_chord_armed);
    }

    #[test]
    fn tab_and_arrows_walk_the_controls_wrapping() {
        let site = band();
        let mut app = drawn(
            vec![site.clone()],
            vec![
                button(&site, "a", None),
                button(&site, "a", None), // a wrapped label: one control
                button(&site, "b", None),
                input(&site, "c", ""),
            ],
        );
        next_site(&mut app);
        assert_eq!(ring(&app), None);
        mods_key(&mut app, &key(KeyCode::Tab));
        assert_eq!(ring(&app), Some("a"));
        mods_key(&mut app, &key(KeyCode::Down));
        mods_key(&mut app, &key(KeyCode::Right));
        assert_eq!(ring(&app), Some("c"));
        mods_key(&mut app, &key(KeyCode::Tab));
        assert_eq!(ring(&app), Some("a"), "past the last wraps to the first");
        mods_key(&mut app, &key(KeyCode::BackTab));
        assert_eq!(ring(&app), Some("c"));
        mods_key(&mut app, &key(KeyCode::Up));
        assert_eq!(ring(&app), Some("b"));
    }

    #[test]
    fn shift_tab_from_nowhere_starts_at_the_last() {
        let site = band();
        let mut app = drawn(
            vec![site.clone()],
            vec![button(&site, "a", None), button(&site, "b", None)],
        );
        next_site(&mut app);
        mods_key(&mut app, &key(KeyCode::BackTab));
        assert_eq!(ring(&app), Some("b"));
    }

    #[test]
    fn enter_and_space_press_a_button_and_enter_a_link_with_its_href() {
        let site = band();
        let mut app = drawn(
            vec![site.clone()],
            vec![
                button(&site, "go", None),
                hit(
                    &site,
                    "docs",
                    ModHitKind::Link {
                        href: Some("https://example.com".into()),
                    },
                ),
            ],
        );
        next_site(&mut app);
        mods_key(&mut app, &key(KeyCode::Tab));
        mods_key(&mut app, &key(KeyCode::Enter));
        mods_key(&mut app, &key(KeyCode::Char(' ')));
        mods_key(&mut app, &key(KeyCode::Tab));
        mods_key(&mut app, &key(KeyCode::Enter));
        assert_eq!(
            acts_owned(&app),
            vec![
                ModAct::Press {
                    site: site.clone(),
                    element: "go".into(),
                    href: None
                },
                ModAct::Press {
                    site: site.clone(),
                    element: "go".into(),
                    href: None
                },
                ModAct::Press {
                    site,
                    element: "docs".into(),
                    href: Some("https://example.com".into())
                },
            ]
        );
    }

    #[test]
    fn typing_edits_an_input_each_edit_a_change_and_enter_submits() {
        let site = pane_site("p");
        let mut app = drawn(vec![site.clone()], vec![input(&site, "name", "ab")]);
        next_site(&mut app);
        mods_key(&mut app, &key(KeyCode::Tab));
        mods_key(&mut app, &key(KeyCode::Char('c')));
        mods_key(&mut app, &key(KeyCode::Backspace));
        mods_key(&mut app, &key(KeyCode::Backspace));
        mods_key(&mut app, &key(KeyCode::Char('X')));
        mods_key(&mut app, &key(KeyCode::Enter));
        let values: Vec<(&str, &str)> = acts(&app)
            .into_iter()
            .map(|act| match act {
                ModAct::Input { kind, value, .. } => (*kind, value.as_str()),
                other => panic!("an input act, not {other:?}"),
            })
            .collect();
        assert_eq!(
            values,
            vec![
                ("change", "abc"),
                ("change", "ab"),
                ("change", "a"),
                ("change", "aX"),
                ("submit", "aX"),
            ]
        );
        assert!(
            app.mods_focus.as_ref().unwrap().drafts.is_empty(),
            "a submit ends the draft: the mod's next drawing is the field again"
        );
    }

    #[test]
    fn backspace_on_an_empty_input_says_nothing() {
        let site = band();
        let mut app = drawn(vec![site.clone()], vec![input(&site, "q", "")]);
        next_site(&mut app);
        mods_key(&mut app, &key(KeyCode::Tab));
        mods_key(&mut app, &key(KeyCode::Backspace));
        assert!(acts(&app).is_empty());
    }

    #[test]
    fn enter_on_an_untouched_input_submits_its_drawn_value() {
        let site = band();
        let mut app = drawn(vec![site.clone()], vec![input(&site, "q", "drawn")]);
        next_site(&mut app);
        mods_key(&mut app, &key(KeyCode::Tab));
        mods_key(&mut app, &key(KeyCode::Enter));
        assert_eq!(
            acts_owned(&app),
            vec![ModAct::Input {
                site,
                element: "q".into(),
                kind: "submit",
                value: "drawn".into()
            }]
        );
    }

    #[test]
    fn letters_typed_into_an_input_are_not_hotkeys() {
        let site = band();
        let mut app = drawn(
            vec![site.clone()],
            vec![input(&site, "q", ""), button(&site, "go", Some('g'))],
        );
        next_site(&mut app);
        mods_key(&mut app, &key(KeyCode::Tab));
        mods_key(&mut app, &key(KeyCode::Char('g')));
        assert!(matches!(
            &acts(&app)[..],
            [ModAct::Input { kind: "change", value, .. }] if value == "g"
        ));
    }

    #[test]
    fn arrows_turn_a_select_and_enter_picks_the_option_shown() {
        let site = band();
        let mut app = drawn(
            vec![site.clone()],
            vec![select(&site, "level", Some("mid"))],
        );
        next_site(&mut app);
        mods_key(&mut app, &key(KeyCode::Tab));
        mods_key(&mut app, &key(KeyCode::Right));
        assert!(acts(&app).is_empty(), "turning is not picking");
        mods_key(&mut app, &key(KeyCode::Right));
        mods_key(&mut app, &key(KeyCode::Enter));
        assert_eq!(
            acts_owned(&app),
            vec![ModAct::Select {
                site: site.clone(),
                element: "level".into(),
                value: "low".into()
            }],
            "mid, high, and round to low"
        );
        mods_key(&mut app, &key(KeyCode::Left));
        mods_key(&mut app, &key(KeyCode::Enter));
        assert_eq!(
            acts(&app).last().copied(),
            Some(&ModAct::Select {
                site,
                element: "level".into(),
                value: "low".into()
            }),
            "after a pick the select turns from its drawn value again (mid, one back)"
        );
    }

    #[test]
    fn a_select_with_no_value_starts_at_its_first_option() {
        let site = band();
        let mut app = drawn(vec![site.clone()], vec![select(&site, "s", None)]);
        next_site(&mut app);
        mods_key(&mut app, &key(KeyCode::Tab));
        mods_key(&mut app, &key(KeyCode::Enter));
        assert!(matches!(&acts(&app)[..], [ModAct::Select { value, .. }] if value == "low"));
    }

    #[test]
    fn a_hotkey_presses_its_button_from_anywhere_in_the_site_the_later_winning() {
        let site = band();
        let mut app = drawn(
            vec![site.clone()],
            vec![
                button(&site, "first", Some('y')),
                button(&site, "second", Some('y')),
                button(&site, "other", None),
            ],
        );
        next_site(&mut app);
        mods_key(&mut app, &key(KeyCode::Char('Y')));
        assert_eq!(
            acts_owned(&app),
            vec![ModAct::Press {
                site,
                element: "second".into(),
                href: None
            }]
        );
        assert_eq!(ring(&app), Some("second"));
    }

    #[test]
    fn other_keys_stay_with_the_site() {
        let site = band();
        let mut app = drawn(vec![site.clone()], vec![button(&site, "a", None)]);
        next_site(&mut app);
        assert!(mods_key(&mut app, &key(KeyCode::Char('z'))));
        assert!(mods_key(&mut app, &key(KeyCode::PageUp)));
        assert!(acts(&app).is_empty());
        assert!(
            !mods_key(
                &mut app,
                &KeyEvent::new_with_kind(
                    KeyCode::Char('a'),
                    KeyModifiers::NONE,
                    KeyEventKind::Release
                )
            ),
            "a release is nobody's"
        );
    }

    #[test]
    fn esc_gives_the_keyboard_back_and_closes_a_close_on_escape_pane() {
        let site = pane_site("dialog");
        let mut app = drawn(vec![site.clone()], vec![button(&site, "ok", None)]);
        app.mods_panes.push(TerminalModPane {
            id: "dialog".into(),
            plugin: "m".into(),
            plugin_name: "m".into(),
            title: "D".into(),
            unasked: false,
            close_on_escape: true,
            version: 1,
            drawn_version: 1,
            tree: None,
            error: None,
        });
        next_site(&mut app);
        assert!(app.mods_focus.as_ref().unwrap().close_on_escape);
        assert!(mods_key(&mut app, &key(KeyCode::Esc)));
        assert!(app.mods_focus.is_none());
        assert_eq!(
            acts_owned(&app),
            vec![ModAct::ClosePane {
                plugin: "m".into(),
                id: "dialog".into()
            }]
        );
    }

    #[test]
    fn esc_on_the_band_only_gives_the_keyboard_back() {
        let mut app = drawn(vec![band()], vec![button(&band(), "a", None)]);
        next_site(&mut app);
        assert!(mods_key(&mut app, &key(KeyCode::Esc)));
        assert!(app.mods_focus.is_none() && acts(&app).is_empty());
    }

    #[test]
    fn ctrl_c_gives_the_keyboard_back_and_goes_on() {
        let mut app = drawn(vec![band()], vec![button(&band(), "a", None)]);
        next_site(&mut app);
        assert!(!mods_key(&mut app, &ctrl('c')));
        assert!(app.mods_focus.is_none());
    }

    #[test]
    fn a_click_takes_the_focus_and_presses_a_button_but_only_rings_an_input() {
        let site = band();
        let go = button(&site, "go", None);
        let field = input(&site, "q", "");
        let mut app = drawn(vec![site.clone()], vec![go.clone(), field.clone()]);
        mods_click(&mut app, field, 0, 0);
        assert_eq!(ring(&app), Some("q"));
        assert!(acts(&app).is_empty());
        mods_click(&mut app, go, 0, 0);
        assert_eq!(ring(&app), Some("go"));
        assert!(matches!(&acts(&app)[..], [ModAct::Press { element, .. }] if element == "go"));
    }

    #[test]
    fn a_click_back_in_the_same_site_keeps_what_was_typed() {
        let site = band();
        let field = input(&site, "q", "");
        let mut app = drawn(
            vec![site.clone()],
            vec![field.clone(), button(&site, "b", None)],
        );
        mods_click(&mut app, field.clone(), 0, 0);
        mods_key(&mut app, &key(KeyCode::Char('h')));
        mods_key(&mut app, &key(KeyCode::Tab));
        mods_click(&mut app, field, 0, 0);
        mods_key(&mut app, &key(KeyCode::Char('i')));
        assert!(matches!(
            acts(&app).last().copied(),
            Some(ModAct::Input { value, .. }) if value == "hi"
        ));
    }

    #[test]
    fn settling_puts_the_ring_on_the_auto_focus_control_until_the_person_moves_it() {
        let site = band();
        let mut auto = input(&site, "q", "");
        auto.auto_focus = true;
        let mut app = drawn(vec![site.clone()], vec![button(&site, "a", None), auto]);
        next_site(&mut app);
        assert_eq!(ring(&app), Some("q"), "the chord settles at once");
        mods_key(&mut app, &key(KeyCode::Tab));
        settle_focus(&mut app);
        assert_eq!(ring(&app), Some("a"), "a ring the person moved stays");
    }

    #[test]
    fn a_pane_that_asked_for_the_keyboard_takes_it_once_drawn() {
        let site = pane_site("p");
        let mut app = drawn(Vec::new(), Vec::new());
        app.mods_focus_wanted = Some(site.clone());
        settle_focus(&mut app);
        assert!(
            app.mods_focus.is_none(),
            "not drawn yet: nothing holds the keys"
        );
        app.mods_sites.push(site.clone());
        settle_focus(&mut app);
        assert_eq!(
            app.mods_focus.as_ref().map(|focus| &focus.site),
            Some(&site)
        );
        assert!(app.mods_focus_wanted.is_none());
    }

    #[test]
    fn a_key_after_the_site_went_away_reaches_the_prompt() {
        let mut app = drawn(
            vec![pane_site("p")],
            vec![button(&pane_site("p"), "q", Some('q'))],
        );
        next_site(&mut app);
        // The mod closed its pane; the next frame drew without it, and no
        // pass of the loop has settled the focus since.
        app.mods_sites.clear();
        app.mods_hits.clear();
        assert!(!mods_key(&mut app, &key(KeyCode::Char('/'))));
        assert!(app.mods_focus.is_none());
    }

    #[test]
    fn a_site_no_longer_drawn_lets_the_keyboard_go() {
        let mut app = drawn(vec![band()], vec![button(&band(), "a", None)]);
        next_site(&mut app);
        app.mods_sites.clear();
        settle_focus(&mut app);
        assert!(app.mods_focus.is_none());
    }

    fn client(site: &ModSite, element: &str, x: u16, y: u16) -> ModHit {
        ModHit {
            area: Rect::new(x, y, 10, 1),
            ..hit(site, element, ModHitKind::Client)
        }
    }

    #[test]
    fn every_key_but_esc_goes_to_the_client_under_the_ring() {
        let site = pane_site("game");
        let mut app = drawn(
            vec![site.clone()],
            vec![client(&site, "snake", 4, 2), button(&site, "q", Some('q'))],
        );
        next_site(&mut app);
        mods_key(&mut app, &key(KeyCode::Tab));
        assert_eq!(ring(&app), Some("snake"));
        for code in [
            KeyCode::Up,
            KeyCode::Char('q'),
            KeyCode::Tab,
            KeyCode::Enter,
        ] {
            assert!(mods_key(&mut app, &key(code)));
        }
        let keys: Vec<Value> = app
            .mods_acts
            .iter()
            .filter_map(|act| match act {
                ModAct::ClientKey { element, key, .. } if element == "snake" => Some(key.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                json!({ "key": "up" }),
                json!({ "key": "q" }),
                json!({ "key": "tab" }),
                json!({ "key": "return" }),
            ],
            "a hotkey, Tab and Enter are the client's while it holds the ring"
        );
        assert_eq!(
            ring(&app),
            Some("snake"),
            "Tab did not move the ring off it"
        );
        assert!(mods_key(&mut app, &key(KeyCode::Esc)));
        assert!(app.mods_focus.is_none(), "Esc gives the keys back");
    }

    #[test]
    fn a_client_key_carries_its_modifiers() {
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(
            client_key_event(&ctrl_a),
            Some(json!({ "key": "a", "ctrl": true }))
        );
        let shift_tab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(
            client_key_event(&shift_tab),
            Some(json!({ "key": "tab", "shift": true }))
        );
        assert_eq!(
            client_key_event(&key(KeyCode::Char(' '))),
            Some(json!({ "key": "space" }))
        );
        assert_eq!(client_key_event(&key(KeyCode::F(5))), None);
    }

    #[test]
    fn a_click_in_a_client_is_a_pointer_press_and_release_at_the_cell() {
        let site = pane_site("game");
        let top = client(&site, "snake", 4, 2);
        let mut app = drawn(
            vec![site.clone()],
            vec![top.clone(), client(&site, "snake", 4, 3)],
        );
        mods_click(&mut app, top, 7, 3);
        assert_eq!(ring(&app), Some("snake"));
        let pointers: Vec<Value> = app
            .mods_acts
            .iter()
            .filter_map(|act| match act {
                ModAct::ClientPointer { pointer, .. } => Some(pointer.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            pointers,
            vec![
                json!({ "type": "down", "x": 3, "y": 1, "button": "left" }),
                json!({ "type": "up", "x": 3, "y": 1, "button": "left" }),
            ]
        );
    }

    #[test]
    fn every_move_of_the_ring_is_raised_for_ui_focus() {
        let site = band();
        let mut app = drawn(
            vec![site.clone()],
            vec![button(&site, "a", None), button(&site, "b", None)],
        );
        next_site(&mut app);
        mods_key(&mut app, &key(KeyCode::Tab));
        mods_key(&mut app, &key(KeyCode::Tab));
        let moves: Vec<(Option<String>, Option<String>, bool)> = app
            .mods_acts
            .iter()
            .filter_map(|act| match act {
                ModAct::Focus {
                    element,
                    previous,
                    by_plugin,
                    ..
                } => Some((element.clone(), previous.clone(), *by_plugin)),
                _ => None,
            })
            .collect();
        assert_eq!(
            moves,
            vec![
                (Some("a".into()), None, false),
                (Some("b".into()), Some("a".into()), false),
            ]
        );
    }

    #[test]
    fn an_auto_focus_move_is_the_plugins_own() {
        let site = band();
        let mut auto = button(&site, "a", None);
        auto.auto_focus = true;
        let mut app = drawn(vec![site], vec![auto]);
        next_site(&mut app);
        assert!(app.mods_acts.iter().any(|act| matches!(
            act,
            ModAct::Focus { element: Some(element), by_plugin: true, .. } if element == "a"
        )));
    }

    fn outcome(
        site: &ModSite,
        asked: &str,
        previous: Option<&str>,
        answer: Result<Option<&str>, &str>,
    ) -> ModFocusOutcome {
        ModFocusOutcome {
            site: site.clone(),
            asked: Some(asked.into()),
            previous: previous.map(str::to_owned),
            answer: answer
                .map(|landed| landed.map(str::to_owned))
                .map_err(str::to_owned),
        }
    }

    #[test]
    fn a_ui_focus_answer_keeps_redirects_or_lets_the_ring_land() {
        let site = band();
        let mut app = drawn(
            vec![site.clone()],
            vec![
                button(&site, "a", None),
                button(&site, "b", None),
                button(&site, "c", None),
            ],
        );
        next_site(&mut app);
        mods_key(&mut app, &key(KeyCode::Tab));
        mods_key(&mut app, &key(KeyCode::Tab));
        apply_focus_outcome(&mut app, outcome(&site, "b", Some("a"), Err("locked")));
        assert_eq!(ring(&app), Some("a"), "a denied move goes back");
        mods_key(&mut app, &key(KeyCode::Tab));
        apply_focus_outcome(&mut app, outcome(&site, "b", Some("a"), Ok(Some("c"))));
        assert_eq!(ring(&app), Some("c"), "a hook may land it elsewhere");
        apply_focus_outcome(&mut app, outcome(&site, "b", Some("a"), Err("late")));
        assert_eq!(
            ring(&app),
            Some("c"),
            "an answer about a move since passed is ignored"
        );
    }

    #[test]
    fn a_mods_own_focus_ask_moves_the_ring_only_where_its_site_holds_the_keys() {
        let site = pane_site("p");
        let mut app = drawn(
            vec![site.clone()],
            vec![button(&site, "a", None), input(&site, "q", "")],
        );
        apply_focus_request(&mut app, "m", "p", "q");
        assert!(
            app.mods_focus.is_none(),
            "the keyboard is the person's to give"
        );
        next_site(&mut app);
        apply_focus_request(&mut app, "other-mod", "p", "q");
        assert_eq!(ring(&app), None, "another mod's ask is not this site's");
        apply_focus_request(&mut app, "m", "p", "q");
        assert_eq!(ring(&app), Some("q"));
        assert!(app.mods_acts.iter().any(|act| matches!(
            act,
            ModAct::Focus {
                by_plugin: true,
                ..
            }
        )));
    }
}
