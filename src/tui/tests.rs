//! App-level tests: mouse input and overlays against a drawn frame.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::app::Reply;
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
    for y in 0..60 {
        for x in 0..200 {
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
        dialog: None,
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

/// The text of `n` cells starting at (x, y).
fn text_at(term: &Terminal<TestBackend>, x: u16, y: u16, n: u16) -> String {
    (x..x + n).map(|x| symbol(term, x, y)).collect()
}

fn screen_text(term: &Terminal<TestBackend>) -> String {
    let h = term.backend().buffer().area.height;
    (0..h).map(|y| screen_row(term, y)).collect::<Vec<_>>().join("\n")
}

fn step(index: usize, name: &str, kind: StepKindName, status: StepStatus) -> StepInfo {
    StepInfo {
        index,
        name: name.into(),
        kind,
        command: format!("make {name}"),
        cwd: Some("/tmp/alpha".into()),
        status,
        exit_code: None,
        started_at: Some(now() - 5),
        finished_at: None,
    }
}

fn run_detail(steps: usize, inputs: usize, outputs: usize) -> RunDetail {
    RunDetail {
        summary: run(7, "alpha", "deploy", false),
        inputs: (0..inputs).map(|i| (format!("in{i}"), format!("value{i}"))).collect(),
        steps: (0..steps)
            .map(|i| step(i, &format!("step-{i}"), StepKindName::Run, if i == 0 { StepStatus::Running } else { StepStatus::Pending }))
            .collect(),
        approvals: vec![],
        outputs: (0..outputs).map(|i| OutputInfo { key: format!("out{i}"), value: format!("result{i}"), step_index: Some(0) }).collect(),
    }
}

/// The app on a run view of `detail`, drawn at `w`×`h`.
fn run_app(detail: RunDetail, w: u16, h: u16) -> (App, Terminal<TestBackend>) {
    let (mut app, _) = app();
    let mut view = screens::run_view::RunView::new(7);
    view.detail = Some(detail);
    app.nav.push(View::Run(Box::new(view)));
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    redraw(&mut app, &mut term);
    (app, term)
}

/// Rows of a hit target's rectangle in the last frame.
fn hit_rows(app: &App, target: Target, overlay: bool, w: u16, h: u16) -> usize {
    (0..h).filter(|y| (0..w).any(|x| app.hits.at(x, *y, overlay).is_some_and(|(t, _)| t == target))).count()
}

/// Height of the pane whose title contains `title`, from its top border down to its bottom corner.
fn pane_height(term: &Terminal<TestBackend>, title: &str) -> u16 {
    let h = term.backend().buffer().area.height;
    let top = (0..h).find(|y| screen_row(term, *y).contains(&format!("┌ {title}"))).expect("pane title on screen");
    let x = screen_row(term, top).chars().position(|c| c == '┌').expect("pane corner") as u16;
    (top..h).find(|y| symbol(term, x, *y) == "└").expect("pane bottom") - top + 1
}

#[test]
fn the_outputs_inputs_pane_is_as_tall_as_its_content() {
    for (w, h) in [(100u16, 28u16), (140, 40)] {
        let heights: Vec<u16> = [0, 5, 6, 20]
            .into_iter()
            .map(|n| {
                let (_, term) = run_app(run_detail(1, 0, n), w, h);
                pane_height(&term, "outputs · inputs")
            })
            .collect();
        // One step needs 6 of the column's rows; the pane takes the rest, at most.
        let room = h - 4 - 6;
        assert_eq!(heights, [3, 7, 8, 22.min(room)], "{w}x{h}");
    }
}

#[test]
fn the_step_output_keeps_the_full_body_height_and_the_steps_never_clip() {
    for (w, h) in [(100u16, 28u16), (140, 40)] {
        for approval_up in [false, true] {
            for (steps, outputs) in [(1, 0), (4, 1), (8, 20), (12, 20)] {
                let mut d = run_detail(steps, 1, outputs);
                if approval_up {
                    d.approvals = vec![approval("go?")];
                }
                let (app, term) = run_app(d, w, h);
                let body = h - 2 - 2 - if approval_up { 5 } else { 0 };
                assert_eq!(hit_rows(&app, Target::RunOutput, false, w, h), body as usize, "{w}x{h} {steps} steps");
                // The steps pane keeps the rows it needed before the pane below was sized to content.
                let left = body;
                let need = (steps as u16 + 5).min(left - 3);
                let io = pane_height(&term, "outputs · inputs");
                assert!(io >= 3 && io <= left - need, "{w}x{h} {steps} steps: io {io}");
                let (_, y) = cell_of(&app, Target::RunSteps, 0, false);
                let steps_rows = hit_rows(&app, Target::RunSteps, false, w, h);
                assert_eq!(steps_rows, steps.min((left - io - 2) as usize), "{w}x{h}");
                assert!(y >= 4);
                if steps_rows == steps && need == steps as u16 + 5 {
                    assert!(screen_text(&term).contains("cwd"), "{w}x{h} {steps} steps");
                }
            }
        }
    }
}

#[test]
fn an_output_event_is_labelled_with_the_step_that_wrote_it() {
    // Step 2 is the active one when the event for step 0's output arrives.
    let mut d = run_detail(4, 0, 0);
    d.steps[0].status = StepStatus::Succeeded;
    d.steps[2].status = StepStatus::Running;
    let (mut app, mut term) = run_app(d, 100, 28);
    app.update(Msg::Event(crate::model::Event::Output { run_id: 7, key: "late".into(), value: "v".into(), step_index: Some(0) }));
    redraw(&mut app, &mut term);
    let screen = screen_text(&term);
    assert!(screen.contains("late from step 1"), "{screen}");
    assert!(!screen.contains("late from step 3"), "{screen}");
}

#[test]
fn the_outputs_inputs_pane_lists_everything_and_says_what_is_cut() {
    let (_app, term) = run_app(run_detail(2, 2, 3), 100, 28);
    let screen = screen_text(&term);
    for want in ["outputs · inputs", "input in0  value0", "input in1  value1", "out2 from step 1  result2"] {
        assert!(screen.contains(want), "{want}\n{screen}");
    }
    let (_app, term) = run_app(run_detail(1, 0, 20), 100, 28);
    assert!(screen_text(&term).contains("… +5 more"), "{}", screen_text(&term));
    // Nine inputs and ten outputs: outputs come first and show.
    let (_app, term) = run_app(run_detail(1, 9, 10), 100, 28);
    let screen = screen_text(&term);
    assert!(screen.contains("out0 from step 1") && screen.contains("out9 from step 1"), "{screen}");
    // A pane with one inner row shows content, never a lone count.
    let mut d = run_detail(12, 9, 10);
    d.approvals = vec![approval("go?")];
    let (_app, term) = run_app(d, 100, 28);
    let screen = screen_text(&term);
    assert_eq!(pane_height(&term, "outputs · inputs"), 3);
    assert!(screen.contains("out0 from step 1") && !screen.contains("… +"), "{screen}");
    let (_app, term) = run_app(run_detail(1, 0, 0), 100, 28);
    assert!(screen_text(&term).contains("no inputs or outputs yet"));
}

#[test]
fn clicks_on_step_rows_select_those_steps() {
    for (w, h) in [(100, 28), (140, 40)] {
        let (mut app, mut term) = run_app(run_detail(5, 1, 1), w, h);
        for i in 0..5 {
            assert!(row_at(&app, &term, Target::RunSteps, i).contains(&format!("step-{i}")), "{w}x{h} row {i}");
        }
        click_on(&mut app, Target::RunSteps, 3, false);
        let View::Run(v) = app.top() else { panic!() };
        assert_eq!(v.sel, 3, "{w}x{h}");
        redraw(&mut app, &mut term);
        assert!(row_at(&app, &term, Target::RunSteps, 3).contains('▶'));
        // Wheel over the output pane reaches the output, not the steps.
        let at = cell_of(&app, Target::RunOutput, 0, false);
        mouse(&mut app, MouseEventKind::ScrollDown, at);
        let View::Run(v) = app.top() else { panic!() };
        assert_eq!(v.sel, 3);
    }
}

fn health_info() -> HealthInfo {
    let tool = |name: &str, ok: bool| ToolStatus { name: name.into(), ok, path: ok.then(|| format!("/usr/bin/{name}")) };
    HealthInfo {
        tools: ["git", "gh", "pnpm", "npm", "cargo", "go", "docker", "tmux"].map(|n| tool(n, n != "pnpm")).into(),
        tools_checked_at: Some(now()),
        docker: Some(Reachability { ok: true, detail: "server 27.0.1".into() }),
        tmux: Some(Reachability { ok: true, detail: "tmux 3.4".into() }),
        config: ConfigStatus {
            path: "/tmp/groundcontrol.yaml".into(),
            ok: true,
            active: true,
            diagnostics: vec![],
            last_attempt_at: Some(now()),
            loaded_at: Some(now()),
        },
        version: "0.1.0".into(),
        started_at: now() - 90,
    }
}

fn health_app(h: HealthInfo, w: u16, rows: u16) -> (App, Terminal<TestBackend>) {
    let (mut app, _) = app();
    app.nav = vec![View::Health];
    app.health = Some(h);
    let mut term = Terminal::new(TestBackend::new(w, rows)).unwrap();
    redraw(&mut app, &mut term);
    (app, term)
}

fn lines_with<'a>(screen: &'a str, needle: &str) -> Vec<&'a str> {
    screen.lines().filter(|l| l.contains(needle)).collect()
}

/// The tool-check rows for `name`: a glyph, then the name, right after the border.
fn tool_rows<'a>(screen: &'a str, name: &str) -> Vec<&'a str> {
    screen.lines().filter(|l| l.chars().skip(3).collect::<String>().starts_with(name) && l.starts_with('│')).collect()
}

#[test]
fn health_lists_tmux_and_docker_once_each() {
    for (w, h) in [(100, 28), (140, 40)] {
        let mut info = health_info();
        info.tools.iter_mut().for_each(|t| {
            t.ok = true;
            t.path = Some(format!("/usr/bin/{}", t.name));
        });
        let (_app, term) = health_app(info, w, h);
        let screen = screen_text(&term);
        for name in ["tmux", "docker"] {
            assert_eq!(lines_with(&screen, name).len(), 1, "{name} at {w}x{h}\n{screen}");
            assert_eq!(tool_rows(&screen, name).len(), 1, "{name} at {w}x{h}\n{screen}");
        }
        let tmux = lines_with(&screen, "tmux")[0];
        assert!(tmux.contains("✓") && tmux.contains("tmux 3.4") && tmux.contains("/usr/bin/tmux"), "{tmux}");
        let docker = lines_with(&screen, "docker")[0];
        assert!(docker.contains("server 27.0.1") && docker.contains("/usr/bin/docker"), "{docker}");
        // Tools that work carry a glyph and their path, not the word OK.
        let git = tool_rows(&screen, "git")[0];
        assert!(git.contains('✓') && git.contains("/usr/bin/git") && !git.contains("OK"), "{git}");
    }
}

#[test]
fn a_failing_tool_has_a_glyph_and_a_word() {
    let (_app, term) = health_app(health_info(), 100, 28);
    let screen = screen_text(&term);
    let pnpm = tool_rows(&screen, "pnpm")[0];
    assert!(pnpm.contains('×') && pnpm.contains("MISSING") && pnpm.contains("not on PATH"), "{pnpm}");
    assert!(lines_with(&screen, "tool check")[0].contains("1 missing"));

    let mut h = health_info();
    h.docker = Some(Reachability { ok: false, detail: "Cannot connect to the Docker daemon".into() });
    h.tmux = None;
    let (_app, term) = health_app(h, 100, 28);
    let screen = screen_text(&term);
    let docker = tool_rows(&screen, "docker")[0];
    assert!(docker.contains('×') && docker.contains("FAIL") && docker.contains("Cannot connect"), "{docker}");
    assert_eq!(tool_rows(&screen, "docker").len(), 1);
    let tmux = tool_rows(&screen, "tmux")[0];
    assert!(tmux.contains("checking…"), "{tmux}");
    assert!(lines_with(&screen, "tool check")[0].contains("docker not answering"));
}

#[test]
fn health_bands_follow_their_content() {
    let (_app, term) = health_app(health_info(), 100, 28);
    assert_eq!(pane_height(&term, "groundcontrol"), 3);
    assert_eq!(pane_height(&term, "tool check"), 10);
    let mut h = health_info();
    h.tools.truncate(3);
    let (_app, term) = health_app(h, 100, 28);
    assert_eq!(pane_height(&term, "tool check"), 5);
    let screen = screen_text(&term);
    assert!(screen.contains("0.1.0") && screen.contains("opened 1m30s ago"), "{screen}");
    assert!(screen.contains("/tmp/groundcontrol.yaml") && screen.contains("no errors or warnings"));
}

#[test]
fn many_diagnostics_end_in_a_count() {
    let mut h = health_info();
    h.config.ok = false;
    h.config.diagnostics = (0..30)
        .map(|i| crate::config::Diagnostic {
            severity: crate::config::Severity::Error,
            line: i + 1,
            col: 1,
            message: format!("problem {i}"),
        })
        .collect();
    let (_app, term) = health_app(h, 100, 28);
    let screen = screen_text(&term);
    assert!(screen.contains("problem 0") && !screen.contains("problem 29") && screen.contains("… +23 more"), "{screen}");
}

/// The rectangle of the help panel in the last frame.
fn help_panel(app: &App, w: u16, h: u16) -> Rect {
    let cells: Vec<(u16, u16)> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .filter(|(x, y)| app.hits.at(*x, *y, true).is_some_and(|(t, _)| t == Target::HelpPanel))
        .collect();
    let (x0, y0) = cells[0];
    let (x1, y1) = *cells.last().unwrap();
    Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1)
}

fn help_on_every_screen(w: u16, h: u16) -> Vec<(screens::overlay::HelpScreen, App, Terminal<TestBackend>)> {
    use screens::overlay::HelpScreen;
    let mut out = Vec::new();
    for screen in [HelpScreen::Dashboard, HelpScreen::Project, HelpScreen::Run, HelpScreen::Health] {
        let (mut app, mut term) = match screen {
            HelpScreen::Dashboard => busy_app(w, h),
            HelpScreen::Project => {
                let (mut a, t) = busy_app(w, h);
                a.open_project("alpha");
                (a, t)
            }
            HelpScreen::Run => run_app(run_detail(9, 2, 2), w, h),
            HelpScreen::Health => health_app(health_info(), w, h),
        };
        app.update(Msg::Input(Event::Key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE))));
        assert!(matches!(app.overlay, Some(Overlay::Help)), "{screen:?} opens help");
        redraw(&mut app, &mut term);
        out.push((screen, app, term));
    }
    out
}

#[test]
fn help_fits_on_every_screen_and_lists_ctrl_c() {
    use screens::overlay::{HelpScreen, help_lines};
    for (w, h) in [(100u16, 28u16), (140, 40)] {
        for (screen, app, term) in help_on_every_screen(w, h) {
            let panel = help_panel(&app, w, h);
            let lines = help_lines(screen);
            assert_eq!(panel.height as usize, lines.len() + 2, "{screen:?} {w}x{h}: panel clipped");
            assert!(panel.height <= h - 4, "{screen:?} {w}x{h}");
            // Every line is drawn in full inside the panel.
            for (i, line) in lines.iter().enumerate() {
                let want: String = line.spans.iter().map(|s| s.content.to_string()).collect();
                let drawn = text_at(&term, panel.x + 1, panel.y + 1 + i as u16, panel.width - 2);
                assert_eq!(drawn.trim_end(), want.trim_end(), "{screen:?} {w}x{h} line {i}");
            }
            let screen_text = screen_text(&term);
            assert!(screen_text.contains("ctrl+c") && screen_text.contains("everywhere"), "{screen:?}");
            let own = match screen {
                HelpScreen::Dashboard => "services pane: start/stop the selected service or group",
                HelpScreen::Project => "open the run dialog",
                HelpScreen::Run => "approve (asks first if protected)",
                HelpScreen::Health => "re-check tools, tmux, Docker and config now",
            };
            assert!(screen_text.contains(own), "{screen:?}");
        }
    }
}

#[test]
fn help_closes_with_its_keys_and_other_keys_do_nothing() {
    let (mut app, _t) = busy_app(100, 28);
    let press = |app: &mut App, c| app.update(Msg::Input(Event::Key(KeyEvent::new(c, KeyModifiers::NONE))));
    for close in [KeyCode::Char('?'), KeyCode::Esc, KeyCode::Char('q'), KeyCode::Enter] {
        press(&mut app, KeyCode::Char('?'));
        assert!(matches!(app.overlay, Some(Overlay::Help)));
        press(&mut app, KeyCode::Char('s'));
        assert!(matches!(app.overlay, Some(Overlay::Help)), "other keys leave help open");
        press(&mut app, close);
        assert!(app.overlay.is_none(), "{close:?}");
        assert!(!app.quit, "q closes help instead of quitting");
    }
}

#[test]
fn help_cannot_open_over_the_run_dialog() {
    let (mut app, mut term) = busy_app(100, 28);
    let (p, pl) = dialog_project(2, false);
    app.overlay = Some(Overlay::Dialog(Box::new(screens::dialog::RunDialog::new(&p, &pl, None))));
    redraw(&mut app, &mut term);
    app.update(Msg::Input(Event::Key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE))));
    assert!(matches!(app.overlay, Some(Overlay::Dialog(_))), "the key is typed into the form instead");
}

/// A project with a pipeline of `inputs` required string inputs and a long outline.
fn dialog_project(inputs: usize, long: bool) -> (ProjectInfo, PipelineInfo) {
    let mut p = project("alpha");
    let mut g = git_status();
    g.upstream = None;
    g.origin_matches = Some(false);
    p.git = Some(g);
    let steps = (0..if long { 14 } else { 3 })
        .map(|i| StepOutline {
            kind: if i == 5 { StepKindName::Approve } else { StepKindName::Run },
            label: format!("a-rather-long-step-label-{i}"),
            detail: "make".into(),
            previews: Default::default(),
        })
        .collect();
    let pl = PipelineInfo {
        id: "deploy".into(),
        protected: false,
        environments: vec![],
        inputs: (0..inputs)
            .map(|i| InputInfo {
                name: format!("input{i}"),
                kind: crate::config::InputKind::String,
                pattern: Some("^[a-z]+$".into()),
                options: vec![],
                default: None,
            })
            .collect(),
        steps,
        warning: Some("This overwrites the production database with the chosen snapshot and cannot be undone.".into()),
        last_run: None,
        waiting: 0,
    };
    (p, pl)
}

fn dry_run_plan(steps: usize) -> DryRunPlan {
    DryRunPlan {
        project: "alpha".into(),
        pipeline: "deploy".into(),
        env: None,
        protected: false,
        inputs: Default::default(),
        steps: (0..steps)
            .map(|i| DryRunStep {
                index: i,
                kind: StepKindName::Run,
                label: format!("step-{i}"),
                command: format!("echo {i}"),
                cwd: "/tmp".into(),
                env_names: vec![],
                timeout_secs: None,
            })
            .collect(),
        blockers: vec![],
    }
}

#[test]
fn closing_the_dry_run_returns_to_the_dialog_as_it_was() {
    let (mut app, mut term) = dialog_app(2, false, 100, 28);
    for c in "v1".chars() {
        app.update(Msg::Input(Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))));
    }
    app.update(Msg::Reply(Reply::DryRun(Ok(StartOutcome::DryRun(dry_run_plan(2))))));
    redraw(&mut app, &mut term);
    assert!(matches!(app.overlay, Some(Overlay::Text(_))));
    app.update(Msg::Input(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))));
    redraw(&mut app, &mut term);
    let Some(Overlay::Dialog(d)) = &app.overlay else { panic!("the dialog is back") };
    assert_eq!(d.fields[0].value, "v1");
    assert!(!d.busy);
    // A second Esc closes the dialog as before.
    app.update(Msg::Input(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))));
    assert!(app.overlay.is_none());
}

#[test]
fn closing_a_dry_run_started_without_a_dialog_closes_the_overlay() {
    let (mut app, _term) = app();
    app.update(Msg::Reply(Reply::DryRun(Ok(StartOutcome::DryRun(dry_run_plan(2))))));
    app.update(Msg::Input(Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))));
    assert!(app.overlay.is_none());
}

#[test]
fn the_dry_run_plan_opens_on_its_first_line() {
    for steps in [1, 3, 8, 20, 60] {
        for (w, h) in [(100, 28), (120, 40)] {
            let (mut app, mut term) = dialog_app(1, false, w, h);
            app.update(Msg::Reply(Reply::DryRun(Ok(StartOutcome::DryRun(dry_run_plan(steps))))));
            redraw(&mut app, &mut term);
            let screen = screen_text(&term);
            assert!(screen.contains("alpha/deploy"), "{steps} steps at {w}x{h}\n{screen}");
            let lines: Vec<&str> = screen.lines().collect();
            assert!(lines[2].starts_with("┃alpha/deploy"), "{steps} steps at {w}x{h}: first line is not on top\n{screen}");
            assert!(!lines[1].contains("9223372036854775807"), "{steps} steps at {w}x{h}\n{screen}");
        }
    }
}

fn dialog_app(inputs: usize, long: bool, w: u16, h: u16) -> (App, Terminal<TestBackend>) {
    let (mut app, _) = app();
    let (p, pl) = dialog_project(inputs, long);
    let mut d = screens::dialog::RunDialog::new(&p, &pl, None);
    d.error = Some("fix the inputs first".into());
    app.overlay = Some(Overlay::Dialog(Box::new(d)));
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    redraw(&mut app, &mut term);
    (app, term)
}

/// Checks that the cells of a button, as drawn, map to that button.
fn assert_buttons(app: &App, term: &Terminal<TestBackend>, w: u16, h: u16) {
    for (target, text) in [(Target::DialogStart, "start run"), (Target::DialogDry, "dry run"), (Target::DialogCancel, "cancel")] {
        let (x, y) = cell_of(app, target, 0, true);
        let row = screen_row(term, y);
        let at = row.chars().position(|c| c == text.chars().next().unwrap()).is_some();
        assert!(at && row.contains(text), "{text} drawn at {w}x{h}: {row}");
        let start = row.find(text).unwrap();
        let col = row[..start].chars().count() as u16;
        assert_eq!(app.hits.at(col, y, true).map(|(t, _)| t), Some(target), "{text} at {w}x{h}");
        assert!(x <= col && y < h);
    }
}

#[test]
fn the_dialog_keeps_its_buttons_and_error_row_when_the_content_is_long() {
    let (app, term) = dialog_app(6, true, 100, 28);
    let screen = screen_text(&term);
    assert!(screen.contains("× fix the inputs first"), "{screen}");
    assert_buttons(&app, &term, 100, 28);
    assert!(screen.contains("… +") && screen.contains("more ("), "something was cut and says so\n{screen}");
    // The destructive warning stays; the inputs scroll around the focused one.
    assert!(screen.contains("overwrites the production database"), "{screen}");
    assert!(screen.contains("input0") && screen.contains("more below"), "{screen}");
    // The error row sits right above the button row, both inside the dialog's border.
    let (_, y) = cell_of(&app, Target::DialogStart, 0, true);
    let (buttons, error) = (screen_row(&term, y), screen_row(&term, y - 1));
    assert!(buttons.contains('║') && buttons.contains("dry run") && buttons.contains("cancel"), "{buttons}");
    assert!(error.contains('║') && error.contains("× fix the inputs first"), "{error}");
    assert!(screen_row(&term, y + 2).contains('╚'), "the dialog's bottom border follows");
}

#[test]
fn the_dialog_shows_everything_when_there_is_room() {
    let (app, term) = dialog_app(6, true, 140, 40);
    let screen = screen_text(&term);
    assert!(!screen.contains("more ("), "{screen}");
    assert!(screen.contains("steps  1 a-rather-long-step-label-0") && screen.contains("The run stops at step 6"), "{screen}");
    assert!(screen.contains("overwrites the production database") && screen.contains("uncommitted changes"), "{screen}");
    assert_buttons(&app, &term, 140, 40);
    let (app, term) = dialog_app(1, false, 100, 28);
    assert!(!screen_text(&term).contains("more ("));
    assert_buttons(&app, &term, 100, 28);
}

#[test]
fn the_outline_gives_way_before_the_warnings() {
    let (_app, term) = dialog_app(2, true, 100, 28);
    let screen = screen_text(&term);
    assert!(screen.contains("uncommitted changes") && screen.contains("overwrites the production database"), "{screen}");
    assert!(!screen.contains("steps  1") && screen.contains("more (steps)"), "{screen}");
}

#[test]
fn many_inputs_scroll_around_the_focused_one_and_stay_clickable() {
    let (mut app, mut term) = dialog_app(12, true, 100, 28);
    for n in [1, 12] {
        let Some(Overlay::Dialog(d)) = &mut app.overlay else { panic!() };
        d.focus = n;
        redraw(&mut app, &mut term);
        let screen = screen_text(&term);
        assert!(screen.contains(&format!("input{}", n - 1)), "focus {n}\n{screen}");
        assert_buttons(&app, &term, 100, 28);
        click_on(&mut app, Target::DialogField(n - 1), 0, true);
        let Some(Overlay::Dialog(d)) = &app.overlay else { panic!() };
        assert_eq!(d.focus, n);
        let (_, y) = cell_of(&app, Target::DialogField(n - 1), 0, true);
        assert!(screen_row(&term, y).contains(&format!("input{}", n - 1)));
    }
    assert!(screen_text(&term).contains("more above"));
}

#[test]
fn the_dialog_buttons_act_at_both_sizes() {
    for (w, h) in [(100, 28), (140, 40)] {
        let (mut app, _term) = dialog_app(6, true, w, h);
        click_on(&mut app, Target::DialogStart, 0, true);
        let Some(Overlay::Dialog(d)) = &app.overlay else { panic!() };
        assert!(!d.busy, "invalid inputs do not start the run");
        assert_eq!(d.error.as_deref(), Some("fix the inputs first"));
        click_on(&mut app, Target::DialogCancel, 0, true);
        assert!(app.overlay.is_none(), "{w}x{h}");

        let (mut app, mut term2) = dialog_app(0, true, w, h);
        click_on(&mut app, Target::DialogDry, 0, true);
        let Some(Overlay::Dialog(d)) = &app.overlay else { panic!() };
        assert!(d.busy, "valid inputs send the dry run");
        redraw(&mut app, &mut term2);
        assert!(screen_text(&term2).contains("working…"));
    }
}

#[test]
fn a_toast_cut_only_in_its_padding_gets_no_ellipsis() {
    let (mut app, mut term) = app_at(100, 28);
    let rw: usize = widgets::key_hints(&[("?", "help"), ("q", "quit")]).iter().map(|s| widgets::width(&s.content)).sum();
    let cap = 100 * 2 / 3 - rw;
    for len in [cap - 4, cap - 3, cap - 2] {
        let msg = "m".repeat(len);
        app.toast(msg.clone(), Level::Info);
        redraw(&mut app, &mut term);
        assert!(screen_row(&term, 27).contains(&msg) && !screen_row(&term, 27).contains(&format!("{}…", &msg[..len - 1])), "{len}");
    }
    let msg = "m".repeat(cap - 1);
    app.toast(msg.clone(), Level::Info);
    redraw(&mut app, &mut term);
    assert!(!screen_row(&term, 27).contains(&msg) && screen_row(&term, 27).contains('…'));
}

#[test]
fn the_project_services_hints_fit_beside_the_pinned_keys() {
    let (mut app, mut term) = busy_app(100, 28);
    app.open_project("alpha");
    app.update(Msg::Input(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))));
    redraw(&mut app, &mut term);
    let bar = screen_row(&term, 27);
    assert!(bar.contains("enter/l full output") && bar.contains("tab next pane") && bar.trim_end().ends_with("? help  esc back"), "{bar}");
    assert!(!bar.contains('…'), "{bar}");
}

fn kind_dialog(kind: crate::config::InputKind, envs: bool) -> screens::dialog::RunDialog {
    let (p, mut pl) = dialog_project(1, false);
    pl.environments = if envs { vec!["staging".into(), "prod".into()] } else { vec![] };
    pl.inputs[0].kind = kind;
    pl.inputs[0].pattern = None;
    pl.inputs[0].options = vec!["a".into(), "b".into()];
    screens::dialog::RunDialog::new(&p, &pl, None)
}

#[test]
fn the_dialog_hints_follow_the_focused_field_and_fit() {
    use crate::config::InputKind::*;
    let cases = [
        (String, 1, "ctrl+u clear  ↑↓/tab field  ctrl+e environment  enter start run"),
        (Bool, 1, "space/←→ toggle  ↑↓/tab field  ctrl+e environment  enter start run"),
        (Choice, 1, "←→/space change  ↑↓/tab field  ctrl+e environment  enter start run"),
        (String, 0, "←→/space environment  ↑↓/tab field  enter start run"),
    ];
    for (kind, focus, want) in cases {
        let (mut app, mut term) = app_at(100, 28);
        let mut d = kind_dialog(kind, true);
        d.focus = focus;
        app.overlay = Some(Overlay::Dialog(Box::new(d)));
        redraw(&mut app, &mut term);
        let bar = screen_row(&term, 27);
        assert!(bar.contains(want) && bar.trim_end().ends_with("ctrl+d dry run  esc cancel"), "{kind:?} {focus}: {bar}");
        assert!(!bar.contains('…'), "{bar}");
    }
    // Without an environment choice there is no environment key.
    let (mut app, mut term) = app_at(100, 28);
    app.overlay = Some(Overlay::Dialog(Box::new(kind_dialog(Bool, false))));
    redraw(&mut app, &mut term);
    let bar = screen_row(&term, 27);
    assert!(bar.contains("space/←→ toggle  ↑↓/tab field  enter start run") && !bar.contains("ctrl+e"), "{bar}");
}

#[test]
fn only_quit_works_while_the_terminal_is_too_small() {
    let (mut app, mut term) = app();
    // Hidden behind the message: nothing may act, open or navigate.
    let mut small = Terminal::new(TestBackend::new(80, 20)).unwrap();
    redraw(&mut app, &mut small);
    assert!(app.too_small);
    let key = |app: &mut App, c| app.update(Msg::Input(Event::Key(KeyEvent::new(c, KeyModifiers::NONE))));
    for c in ['2', '?', 's', 'a', 'c', 'r', 'l'] {
        key(&mut app, KeyCode::Char(c));
    }
    key(&mut app, KeyCode::Enter);
    app.update(Msg::Input(Event::Paste("x".into())));
    click(&mut app, (5, 5));
    assert!(matches!(app.top(), View::Dashboard) && app.overlay.is_none() && !app.quit, "nothing responded");
    // Growing back restores the screen exactly as it was, and keys work again.
    redraw(&mut app, &mut term);
    assert!(!app.too_small);
    key(&mut app, KeyCode::Char('?'));
    assert!(matches!(app.overlay, Some(Overlay::Help)));
    key(&mut app, KeyCode::Esc);
    // `q` quits from the small screen, as the message says.
    redraw(&mut app, &mut small);
    key(&mut app, KeyCode::Char('q'));
    assert!(app.quit);
}

#[test]
fn ctrl_c_quits_while_the_terminal_is_too_small() {
    let (mut app, _term) = app();
    let mut small = Terminal::new(TestBackend::new(80, 20)).unwrap();
    redraw(&mut app, &mut small);
    app.update(Msg::Input(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))));
    assert!(app.quit);
}

#[test]
fn dashboard_service_keys_act_only_in_the_services_pane() {
    // `alpha` has no services, so a service key that acts says so in a toast.
    let (mut app, _t) = app();
    let press = |app: &mut App, c| app.update(Msg::Input(Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))));
    for focus in [dashboard::Focus::Projects, dashboard::Focus::Runs] {
        app.dash.focus = focus;
        for c in ['s', 'r', 'l'] {
            press(&mut app, c);
        }
        assert!(app.toast.is_none() && app.overlay.is_none(), "{focus:?}: the service keys must do nothing");
        let hints: Vec<_> = dashboard::hints(&app).iter().map(|h| h.0).collect();
        assert!(!hints.contains(&"s") && !hints.contains(&"r") && !hints.contains(&"l"), "{hints:?}");
    }
    app.dash.focus = dashboard::Focus::Services;
    press(&mut app, 's');
    assert!(app.toast.as_ref().is_some_and(|t| t.0.contains("alpha has no services")), "{:?}", app.toast);
    let hints: Vec<_> = dashboard::hints(&app).iter().map(|h| h.0).collect();
    assert!(hints.contains(&"s") && hints.contains(&"r") && hints.contains(&"l"), "{hints:?}");
}
