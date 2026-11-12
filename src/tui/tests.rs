//! App-level tests: mouse input and overlays against a drawn frame.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::*;
use crate::model::*;

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
    app.dash.focus = dashboard::Focus::Projects;
    let at = cell_of(&app, Target::DashRuns, 0, false);
    click(&mut app, at);
    click(&mut app, at);
    assert_eq!(app.dash.focus, dashboard::Focus::Runs);
    assert_eq!(app.dash.run_sel, 0);
    assert_eq!(app.nav.len(), 1, "no run there to open");
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

fn app_at(width: u16, height: u16) -> (App, Terminal<TestBackend>) {
    let (mut app, _) = app();
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| draw(&mut app, f)).unwrap();
    (app, term)
}

fn symbol(term: &Terminal<TestBackend>, x: u16, y: u16) -> String {
    term.backend().buffer()[(x, y)].symbol().to_string()
}

fn screen_row(term: &Terminal<TestBackend>, y: u16) -> String {
    let w = term.backend().buffer().area.width;
    (0..w).map(|x| symbol(term, x, y)).collect()
}

#[test]
fn the_focused_pane_has_a_heavy_border() {
    let (app, term) = app_at(100, 28);
    assert_eq!(app.dash.focus, dashboard::Focus::Projects);
    assert_eq!(symbol(&term, 0, 1), "┏");
    let (x, y) = cell_of(&app, Target::DashServices, 0, false);
    assert_eq!(symbol(&term, x - 1, y - 1), "┌");
}

#[test]
fn an_unfocused_selection_keeps_its_marker_without_the_highlight() {
    let (mut app, mut term) = app_at(100, 28);
    app.dash.focus = dashboard::Focus::Runs;
    redraw(&mut app, &mut term);
    let (x, y) = cell_of(&app, Target::DashProjects, 0, false);
    assert!(screen_row(&term, y).contains('▶'));
    assert_ne!(term.backend().buffer()[(x + 10, y)].bg, theme::SELECTION);
    app.dash.focus = dashboard::Focus::Projects;
    redraw(&mut app, &mut term);
    assert_eq!(term.backend().buffer()[(x + 10, y)].bg, theme::SELECTION);
}

#[test]
fn the_bottom_bar_keeps_its_right_keys_with_a_toast() {
    for (w, h) in [(100, 28), (140, 40)] {
        let (mut app, mut term) = app_at(w, h);
        assert!(screen_row(&term, h - 1).trim_end().ends_with("? help  q quit"));
        app.toast("a toast that is long enough to need clipping ".repeat(4), Level::Info);
        redraw(&mut app, &mut term);
        let bar = screen_row(&term, h - 1);
        assert!(bar.trim_end().ends_with("? help  q quit"), "{bar}");
        assert!(bar.contains("a toast") && bar.trim_end().ends_with("… ? help  q quit"), "{bar}");
    }
}

fn git_status() -> crate::git::GitStatus {
    crate::git::GitStatus {
        branch: Some("main".into()),
        commit: Some("abc1234".into()),
        upstream: None,
        ahead: 0,
        behind: 0,
        staged: 0,
        modified: 1,
        untracked: 1,
        conflicted: 0,
        origin_matches: None,
        origin_url: None,
        checked_at: 0,
        error: None,
    }
}

fn service(id: &str, state: ServiceState) -> ServiceStatus {
    ServiceStatus {
        project: "alpha".into(),
        id: id.into(),
        state,
        healthy: true,
        pid: None,
        port: None,
        started_at: None,
        restart_count: 0,
        last_exit_code: None,
        message: None,
        depends_on: vec![],
        has_health_check: false,
    }
}

fn run(id: i64, project: &str, pipeline: &str, protected: bool) -> RunSummary {
    RunSummary {
        id,
        project: project.into(),
        pipeline: pipeline.into(),
        env: protected.then(|| "prod".into()),
        status: RunStatus::Running,
        reason: None,
        branch: None,
        commit: Some("d7b1002".into()),
        dirty: false,
        unpushed: false,
        protected,
        started_at: now(),
        finished_at: None,
    }
}

fn approval(message: &str) -> ApprovalInfo {
    ApprovalInfo {
        run_id: 7,
        step_index: 1,
        project: "alpha".into(),
        pipeline: "gated".into(),
        env: Some("prod".into()),
        protected: true,
        message: message.into(),
        status: ApprovalStatus::Pending,
        expires_at: now() + 1800,
    }
}

/// Three projects (the first with git and services), two runs, drawn at `w`×`h`.
fn busy_app(w: u16, h: u16) -> (App, Terminal<TestBackend>) {
    let (mut app, _) = app();
    app.projects[0].git = Some(git_status());
    app.projects[0].services = vec![service("web", ServiceState::Running), service("db", ServiceState::Crashed)];
    app.recent = vec![run(8, "alpha", "deploy-everything", false), run(7, "beta", "gated", true)];
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(&mut app, f)).unwrap();
    (app, term)
}

fn row_at(app: &App, term: &Terminal<TestBackend>, target: Target, row: usize) -> String {
    let (_, y) = cell_of(app, target, row, false);
    screen_row(term, y)
}

#[test]
fn the_projects_row_keeps_its_columns_at_the_minimum_size() {
    let (app, term) = busy_app(100, 28);
    let line = row_at(&app, &term, Target::DashProjects, 0);
    assert!(line.contains("alpha") && line.contains("main") && line.contains("1M 1?"), "{line}");
    assert!(line.contains("1/2 up  1 crashed"), "{line}");
}

#[test]
fn a_run_row_shows_its_full_name_and_the_protected_marker() {
    let (app, term) = busy_app(100, 28);
    assert!(row_at(&app, &term, Target::DashRuns, 0).contains("alpha/deploy-everything"));
    let protected = row_at(&app, &term, Target::DashRuns, 1);
    assert!(protected.contains("beta/gated") && protected.contains("▞ prod"), "{protected}");
    assert!(!row_at(&app, &term, Target::DashRuns, 0).contains('▞'));
}

#[test]
fn tab_follows_the_screen_top_to_bottom() {
    let (mut app, _t) = busy_app(100, 28);
    let tab = |app: &mut App| app.update(Msg::Input(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))));
    assert_eq!(app.dash.focus, dashboard::Focus::Projects);
    tab(&mut app);
    assert_eq!(app.dash.focus, dashboard::Focus::Runs);
    tab(&mut app);
    assert_eq!(app.dash.focus, dashboard::Focus::Services);
    tab(&mut app);
    assert_eq!(app.dash.focus, dashboard::Focus::Projects);
    app.waiting = vec![approval("go?")];
    app.dash.focus = dashboard::Focus::Services;
    tab(&mut app);
    assert_eq!(app.dash.focus, dashboard::Focus::Banner);
}

#[test]
fn the_banner_is_one_line_with_the_message_and_the_review_key() {
    let (mut app, mut term) = busy_app(100, 28);
    app.waiting = vec![approval("Proceed with the gated change?")];
    redraw(&mut app, &mut term);
    let (_, y) = cell_of(&app, Target::DashBanner, 0, false);
    assert_eq!(y, 1, "right under the top bar");
    assert_eq!(symbol(&term, 0, y), "╔");
    assert_eq!(symbol(&term, 0, y + 2), "╚");
    assert_eq!(symbol(&term, 0, y + 3), "┏", "the projects pane follows at once");
    let line = screen_row(&term, y + 1);
    assert!(line.contains("\"Proceed with the gated change?\"") && line.contains("enter review"), "{line}");
    app.waiting = vec![approval(&"⛅ a very long approval message ".repeat(6)), approval("two")];
    redraw(&mut app, &mut term);
    let line = screen_row(&term, y + 1);
    assert!(line.contains("alpha/gated #7") && line.contains("env prod") && line.contains("(+1 more)"), "{line}");
    assert!(line.contains('…') && line.contains("enter review"), "{line}");
}

#[test]
fn clicking_a_row_in_each_list_selects_it() {
    for (w, h) in [(100, 28), (140, 40)] {
        let (mut app, mut term) = busy_app(w, h);
        app.waiting = vec![approval("go?")];
        redraw(&mut app, &mut term);
        click_on(&mut app, Target::DashProjects, 1, false);
        assert_eq!(app.dash.project_sel, 1, "{w}x{h}");
        click_on(&mut app, Target::DashRuns, 1, false);
        assert_eq!((app.dash.run_sel, app.dash.focus), (1, dashboard::Focus::Runs), "{w}x{h}");
        redraw(&mut app, &mut term);
        click_on(&mut app, Target::DashProjects, 0, false);
        assert_eq!(app.dash.project_sel, 0);
        click_on(&mut app, Target::DashServices, 1, false);
        assert_eq!((app.dash.service_sel, app.dash.focus), (1, dashboard::Focus::Services), "{w}x{h}");
    }
}

#[test]
fn long_lists_scroll_with_the_selection_and_clicks_follow() {
    let (mut app, mut term) = busy_app(100, 28);
    app.projects = (0..30).map(|i| project(&format!("p{i:02}"))).collect();
    app.recent = (0..8).map(|i| run(i, "alpha", "deploy", false)).collect();
    app.projects[0].services = (0..30).map(|i| service(&format!("s{i:02}"), ServiceState::Running)).collect();
    app.dash.project_sel = 29;
    app.dash.run_sel = 7;
    redraw(&mut app, &mut term);
    let last = row_at(&app, &term, Target::DashProjects, 29);
    assert!(last.contains("p29") && last.contains('▶'), "{last}");
    click_on(&mut app, Target::DashProjects, 27, false);
    assert_eq!(app.dash.project_sel, 27);
    let shown = row_at(&app, &term, Target::DashProjects, 27);
    assert!(shown.contains("p27"), "{shown}");
    let (_, y) = cell_of(&app, Target::DashRuns, 7, false);
    assert!(screen_row(&term, y).contains('▶'));

    app.dash.project_sel = 0;
    app.dash.service_sel = 29;
    redraw(&mut app, &mut term);
    let last = row_at(&app, &term, Target::DashServices, 29);
    assert!(last.contains("s29") && last.contains('▶'), "{last}");
    click_on(&mut app, Target::DashServices, 27, false);
    assert_eq!(app.dash.service_sel, 27);
    assert!(row_at(&app, &term, Target::DashServices, 27).contains("s27"));
}

#[test]
fn an_empty_projects_pane_says_so() {
    let (mut app, mut term) = busy_app(100, 28);
    app.projects.clear();
    redraw(&mut app, &mut term);
    let screen: String = (0..28).map(|y| screen_row(&term, y)).collect();
    assert!(screen.contains("no projects in groundcontrol.yaml"));
    app.projects_loaded = false;
    redraw(&mut app, &mut term);
    let screen: String = (0..28).map(|y| screen_row(&term, y)).collect();
    assert!(screen.contains("loading…"));
}

#[test]
fn shift_tab_runs_the_order_backwards() {
    let (mut app, _t) = busy_app(100, 28);
    let back = |app: &mut App| app.update(Msg::Input(Event::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))));
    assert_eq!(app.dash.focus, dashboard::Focus::Projects);
    back(&mut app);
    assert_eq!(app.dash.focus, dashboard::Focus::Services);
    back(&mut app);
    assert_eq!(app.dash.focus, dashboard::Focus::Runs);
    back(&mut app);
    assert_eq!(app.dash.focus, dashboard::Focus::Projects);
    app.waiting = vec![approval("go?")];
    back(&mut app);
    assert_eq!(app.dash.focus, dashboard::Focus::Banner);
    back(&mut app);
    assert_eq!(app.dash.focus, dashboard::Focus::Services);
    back(&mut app);
    assert_eq!(app.dash.focus, dashboard::Focus::Runs);
    back(&mut app);
    assert_eq!(app.dash.focus, dashboard::Focus::Projects);
}

#[test]
fn a_click_below_the_last_service_only_focuses() {
    let (mut app, _t) = busy_app(100, 28);
    app.dash.focus = dashboard::Focus::Projects;
    app.dash.service_sel = 1;
    let at = cell_of(&app, Target::DashServices, 5, false);
    click(&mut app, at);
    click(&mut app, at);
    assert_eq!(app.dash.focus, dashboard::Focus::Services);
    assert_eq!(app.dash.service_sel, 1);
    assert_eq!(app.nav.len(), 1);
}
