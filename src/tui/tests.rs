//! App-level tests: mouse input and overlays against a drawn frame.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::*;
use crate::model::ProjectInfo;

fn project(id: &str) -> ProjectInfo {
    ProjectInfo {
        id: id.into(),
        path: format!("/tmp/{id}"),
        disabled: None,
        busy_run: None,
        git: None,
        environments: vec![],
        services: vec![],
        groups: vec![],
        pipelines: vec![],
    }
}

/// A connected app with three projects, drawn once at 120×40.
fn app() -> (App, Terminal<TestBackend>) {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx);
    app.conn = Conn::Connected;
    app.projects = ["alpha", "beta", "gamma"].map(project).into();
    app.projects_loaded = true;
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    term.draw(|f| draw(&mut app, f)).unwrap();
    (app, term)
}

/// The screen cell of a hit target's row, found by scanning the hit map.
fn cell_of(app: &App, target: Target, row: usize, overlay: bool) -> (u16, u16) {
    for y in 0..40 {
        for x in 0..120 {
            if app.hits.at(x, y, overlay) == Some((target, row)) {
                return (x, y);
            }
        }
    }
    panic!("no cell for {target:?} row {row}");
}

fn mouse(app: &mut App, kind: MouseEventKind, (column, row): (u16, u16)) {
    app.update(Msg::Input(Event::Mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE })));
}

fn click(app: &mut App, at: (u16, u16)) {
    mouse(app, MouseEventKind::Down(MouseButton::Left), at);
}

fn click_on(app: &mut App, target: Target, row: usize, overlay: bool) {
    let at = cell_of(app, target, row, overlay);
    click(app, at);
}

fn redraw(app: &mut App, term: &mut Terminal<TestBackend>) {
    term.draw(|f| draw(app, f)).unwrap();
}

#[test]
fn click_selects_a_project_and_focuses_its_pane() {
    let (mut app, _t) = app();
    app.dash.focus = dashboard::Focus::Runs;
    click_on(&mut app, Target::DashProjects, 2, false);
    assert_eq!(app.dash.project_sel, 2);
    assert_eq!(app.dash.focus, dashboard::Focus::Projects);
    assert_eq!(app.nav.len(), 1, "one click does not open the project");
}

#[test]
fn double_click_opens_the_project() {
    let (mut app, _t) = app();
    let at = cell_of(&app, Target::DashProjects, 1, false);
    click(&mut app, at);
    click(&mut app, at);
    assert_eq!(app.nav.len(), 2);
    assert!(matches!(app.top(), View::Project(p) if p.id == "beta"));
}

#[test]
fn two_clicks_on_different_rows_are_not_a_double_click() {
    let (mut app, _t) = app();
    click_on(&mut app, Target::DashProjects, 0, false);
    click_on(&mut app, Target::DashProjects, 1, false);
    assert_eq!(app.nav.len(), 1);
    assert_eq!(app.dash.project_sel, 1);
}

#[test]
fn a_click_below_the_last_row_only_focuses() {
    let (mut app, _t) = app();
    app.dash.focus = dashboard::Focus::Runs;
    let at = cell_of(&app, Target::DashProjects, 3, false);
    click(&mut app, at);
    click(&mut app, at);
    assert_eq!(app.dash.focus, dashboard::Focus::Projects);
    assert_eq!(app.dash.project_sel, 0);
    assert_eq!(app.nav.len(), 1, "no project there to open");
}

#[test]
fn the_wheel_moves_the_selection() {
    let (mut app, _t) = app();
    let at = cell_of(&app, Target::DashProjects, 0, false);
    mouse(&mut app, MouseEventKind::ScrollDown, at);
    mouse(&mut app, MouseEventKind::ScrollDown, at);
    assert_eq!(app.dash.project_sel, 2);
    mouse(&mut app, MouseEventKind::ScrollUp, at);
    assert_eq!(app.dash.project_sel, 1);
}

#[test]
fn top_bar_tabs_switch_screens() {
    let (mut app, mut t) = app();
    click_on(&mut app, Target::Tab(2), 0, false);
    assert!(matches!(app.top(), View::Health));
    redraw(&mut app, &mut t);
    click_on(&mut app, Target::Tab(1), 0, false);
    assert!(matches!(app.top(), View::Dashboard));
}

#[test]
fn an_open_overlay_takes_the_clicks() {
    let (mut app, mut t) = app();
    app.overlay = Some(Overlay::Help);
    redraw(&mut app, &mut t);
    // The projects underneath cannot be clicked.
    click_on(&mut app, Target::Tab(2), 0, false);
    assert!(matches!(app.top(), View::Dashboard), "tabs are behind the overlay");
    assert!(app.overlay.is_some());
    // A click on the help panel closes it.
    click_on(&mut app, Target::HelpPanel, 0, true);
    assert!(app.overlay.is_none());
}

#[test]
fn confirm_buttons_answer_the_dialog() {
    let (mut app, mut t) = app();
    let confirm = || screens::overlay::Confirm {
        title: "cancel run #1".into(),
        lines: vec![Line::raw("sure?")],
        action: screens::overlay::ConfirmAction::CancelRun(1),
    };
    app.overlay = Some(Overlay::Confirm(confirm()));
    redraw(&mut app, &mut t);
    click_on(&mut app, Target::ConfirmNo, 0, true);
    assert!(app.overlay.is_none(), "n closes the confirm");
}

#[test]
fn the_text_overlay_scrolls_with_the_wheel() {
    let (mut app, mut t) = app();
    let mut view = term_view::TermView::new();
    view.feed(&"line\r\n".repeat(200));
    app.overlay = Some(Overlay::Text(Box::new(screens::overlay::TextOverlay {
        title: "log".into(),
        subtitle: String::new(),
        view,
        follow_service: None,
    })));
    redraw(&mut app, &mut t);
    let at = cell_of(&app, Target::TextOverlay, 0, true);
    mouse(&mut app, MouseEventKind::ScrollUp, at);
    let Some(Overlay::Text(o)) = &app.overlay else { panic!() };
    assert_eq!(o.view.scrolled(), 3);
}

#[test]
fn mouse_is_ignored_until_connected() {
    let (mut app, _t) = app();
    let at = cell_of(&app, Target::DashProjects, 2, false);
    app.conn = Conn::Connecting;
    click(&mut app, at);
    assert_eq!(app.dash.project_sel, 0);
}
