//! Dashboard (§10.3): approval banner, projects and open runs in full-width
//! bands, then the selected project's services beside the selected service's
//! output.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};

use super::super::app::{App, Level, Reply};
use super::super::mouse::Target;
use super::super::term_view::TermView;
use super::super::theme;
use super::super::widgets::*;
use super::services::{control, open_full_log, row_service, service_lines, service_rows, service_summary};
use crate::model::*;

pub const LOG_LINES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Banner,
    Projects,
    Services,
    Runs,
}

pub struct DashState {
    pub focus: Focus,
    pub project_sel: usize,
    pub service_sel: usize,
    pub run_sel: usize,
    pub log: TermView,
    pub log_target: Option<(String, String)>,
    pub focus_banner_on_new: bool,
}

impl Default for DashState {
    fn default() -> Self {
        DashState {
            focus: Focus::Projects,
            project_sel: 0,
            service_sel: 0,
            run_sel: 0,
            log: TermView::new(),
            log_target: None,
            focus_banner_on_new: true,
        }
    }
}

fn selected_project(app: &App) -> Option<&ProjectInfo> {
    app.projects.get(app.dash.project_sel.min(app.projects.len().saturating_sub(1)))
}

/// The service whose output the output pane shows: the selected service, or
/// the first member of the selected group.
fn log_service(app: &App) -> Option<(String, String)> {
    let p = selected_project(app)?;
    let rows = service_rows(p);
    let row = rows.get(app.dash.service_sel.min(rows.len().saturating_sub(1)))?;
    Some((p.id.clone(), row_service(p, row)?))
}

/// Loads the last lines of the selected service's output when the selection changes.
pub fn load_log(app: &mut App, force: bool) {
    let target = log_service(app);
    if target == app.dash.log_target && !force {
        return;
    }
    app.dash.log_target = target.clone();
    app.dash.log.reset();
    if let Some((project, service)) = target {
        app.dash.log.loading = true;
        app.call(move |e| async move {
            let result = e.service_output(&project, &service).await;
            Reply::ServiceOutput { project, service, result }
        });
    }
    app.sync_watch();
}

pub fn key(app: &mut App, k: KeyEvent) {
    let has_banner = !app.waiting.is_empty();
    let order: Vec<Focus> = if has_banner {
        vec![Focus::Banner, Focus::Projects, Focus::Runs, Focus::Services]
    } else {
        vec![Focus::Projects, Focus::Runs, Focus::Services]
    };
    if !has_banner && app.dash.focus == Focus::Banner {
        app.dash.focus = Focus::Projects;
    }
    let pos = order.iter().position(|f| *f == app.dash.focus).unwrap_or(0);
    let svc_len = selected_project(app).map(|p| service_rows(p).len()).unwrap_or(0);
    match k.code {
        KeyCode::Tab => {
            app.dash.focus = order[(pos + 1) % order.len()];
            app.dash.focus_banner_on_new = false;
        }
        KeyCode::BackTab => {
            app.dash.focus = order[(pos + order.len() - 1) % order.len()];
            app.dash.focus_banner_on_new = false;
        }
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
            let down = matches!(k.code, KeyCode::Down | KeyCode::Char('j'));
            let step = |sel: &mut usize, len: usize| {
                if len == 0 {
                    *sel = 0;
                } else if down {
                    *sel = (*sel + 1).min(len - 1);
                } else {
                    *sel = sel.saturating_sub(1);
                }
            };
            match app.dash.focus {
                Focus::Projects | Focus::Banner => {
                    app.dash.focus = Focus::Projects;
                    let before = app.dash.project_sel;
                    step(&mut app.dash.project_sel, app.projects.len());
                    if before != app.dash.project_sel {
                        app.dash.service_sel = 0;
                    }
                }
                Focus::Services => step(&mut app.dash.service_sel, svc_len),
                Focus::Runs => step(&mut app.dash.run_sel, app.recent.len()),
            }
            load_log(app, false);
        }
        KeyCode::Enter => match app.dash.focus {
            Focus::Banner => {
                if let Some(a) = app.waiting.first() {
                    let id = a.run_id;
                    app.open_run(id);
                }
            }
            Focus::Projects => {
                if let Some(p) = selected_project(app) {
                    let id = p.id.clone();
                    app.open_project(&id);
                }
            }
            Focus::Services => {
                if let Some((p, s)) = log_service(app) {
                    open_full_log(app, &p, &s);
                }
            }
            Focus::Runs => {
                if let Some(r) = app.recent.get(app.dash.run_sel) {
                    let id = r.id;
                    app.open_run(id);
                }
            }
        },
        KeyCode::Char('s') | KeyCode::Char('r') if app.dash.focus == Focus::Services => {
            let Some(p) = selected_project(app).cloned() else { return };
            let rows = service_rows(&p);
            if let Some(row) = rows.get(app.dash.service_sel.min(rows.len().saturating_sub(1))) {
                control(app, &p, row, k.code == KeyCode::Char('r'));
            } else {
                app.toast(format!("{} has no services", p.id), Level::Info);
            }
        }
        KeyCode::Char('l') if app.dash.focus == Focus::Services => {
            if let Some((p, s)) = log_service(app) {
                open_full_log(app, &p, &s);
            }
        }
        _ => {}
    }
}

/// A click on a pane focuses it and selects the row under the pointer.
/// Returns whether that was a real row (a row index past the end only focuses).
pub fn click(app: &mut App, target: Target, row: usize) -> bool {
    app.dash.focus_banner_on_new = false;
    match target {
        Target::DashBanner => {
            app.dash.focus = Focus::Banner;
            true
        }
        Target::DashProjects => {
            app.dash.focus = Focus::Projects;
            if row >= app.projects.len() {
                return false;
            }
            if row != app.dash.project_sel {
                app.dash.project_sel = row;
                app.dash.service_sel = 0;
            }
            load_log(app, false);
            true
        }
        Target::DashServices => {
            app.dash.focus = Focus::Services;
            let len = selected_project(app).map(|p| service_rows(p).len()).unwrap_or(0);
            if row >= len {
                return false;
            }
            app.dash.service_sel = row;
            load_log(app, false);
            true
        }
        Target::DashRuns => {
            app.dash.focus = Focus::Runs;
            if row >= app.recent.len() {
                return false;
            }
            app.dash.run_sel = row;
            true
        }
        _ => false,
    }
}

pub fn hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let enter = if app.dash.focus == Focus::Banner { ("enter", "review") } else { ("enter", "open") };
    if app.dash.focus == Focus::Services {
        vec![("s", "start/stop"), ("r", "restart"), ("l", "full output"), ("↑↓", "select"), enter, ("tab", "next pane")]
    } else {
        vec![("↑↓", "select"), enter, ("tab", "next pane")]
    }
}

const BANNER_H: u16 = 3;
const RUNS_MIN_H: u16 = 3;
const RUNS_MAX_H: u16 = 7;
const SERVICES_MIN_H: u16 = 6;

pub fn draw(app: &mut App, f: &mut Frame, area: Rect) {
    let banner_h = if app.waiting.is_empty() { 0 } else { BANNER_H.min(area.height) };
    let rest = area.height - banner_h;
    let projects_h = (app.projects.len().max(1) as u16 + 3).min(rest.saturating_sub(RUNS_MIN_H + SERVICES_MIN_H));
    let runs_h = (app.recent.len() as u16 + 2).clamp(RUNS_MIN_H, RUNS_MAX_H).min(rest.saturating_sub(projects_h + SERVICES_MIN_H));
    let [banner, projects, runs, bottom] =
        Layout::vertical([Constraint::Length(banner_h), Constraint::Length(projects_h), Constraint::Length(runs_h), Constraint::Min(0)])
            .areas(area);
    let [services, log] = Layout::horizontal([Constraint::Percentage(44), Constraint::Percentage(56)]).areas(bottom);
    if banner_h > 0 {
        draw_banner(app, f, banner);
    }
    draw_projects(app, f, projects);
    draw_runs(app, f, runs);
    draw_services(app, f, services);
    draw_log(app, f, log);
}

fn draw_banner(app: &App, f: &mut Frame, area: Rect) {
    let focused = app.dash.focus == Focus::Banner;
    let title = if app.waiting.len() == 1 { "approval waiting".to_string() } else { format!("{} approvals waiting", app.waiting.len()) };
    let inner = framed_alert(f, area, &title, true);
    app.hits.add(area, Target::DashBanner, 0);
    let a = &app.waiting[0];
    let w = inner.width as usize;
    let mark = if focused { Span::styled("▶ ", theme::key()) } else { Span::styled("▲ ", theme::warning()) };
    let mut head = vec![
        mark,
        Span::styled(format!("{}/{}", a.project, a.pipeline), theme::bold()),
        Span::styled(format!(" #{}", a.run_id), theme::muted()),
    ];
    if let Some(env) = &a.env {
        head.push(Span::styled("  env ", theme::muted()));
        head.push(Span::styled(env.clone(), theme::warning()));
    }
    let more = if app.waiting.len() > 1 { format!(" (+{} more)", app.waiting.len() - 1) } else { String::new() };
    let right = vec![
        Span::styled(format!("expires {} ({})   ", fmt_clock(a.expires_at), fmt_age(a.expires_at - now())), theme::muted()),
        Span::styled("enter", theme::key()),
        Span::styled(" review", theme::text()),
    ];
    let right_w: usize = right.iter().map(|s| width(&s.content)).sum();
    let avail = w.saturating_sub(right_w + 2);
    let head_w: usize = head.iter().map(|s| width(&s.content)).sum::<usize>() + 2;
    let message = Span::styled(format!("\"{}\"", a.message), theme::text());
    head.push(Span::raw("  "));
    head.extend(clip_line(vec![message], avail.saturating_sub(head_w + width(&more))).spans);
    head.push(Span::styled(more, theme::muted()));
    let mut line = clip_line(head, avail).spans;
    let used: usize = line.iter().map(|s| width(&s.content)).sum();
    line.push(Span::raw(" ".repeat(w.saturating_sub(used + right_w))));
    line.extend(right);
    f.render_widget(para(vec![Line::from(line)]), inner);
}

fn draw_projects(app: &App, f: &mut Frame, area: Rect) {
    let focused = matches!(app.dash.focus, Focus::Projects);
    let inner = framed(f, area, vec![title_text("projects")], focused);
    let w = inner.width;
    let visible = (inner.height as usize).saturating_sub(1);
    let start = app.dash.project_sel.saturating_sub(visible.saturating_sub(1));
    // Project rows start below the column header.
    app.hits.add(Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner }, Target::DashProjects, start);
    let mut lines = vec![row(
        vec![
            cell("", theme::muted(), 1),
            cell("project", theme::muted(), 0),
            cell("branch", theme::muted(), 20),
            cell("tree", theme::muted(), 9),
            cell("↑ ↓", theme::muted(), 7),
            cell("services", theme::muted(), 18),
        ],
        w,
        1,
    )];
    if !app.projects_loaded {
        lines.push(Line::styled("  loading…", theme::muted()));
    } else if app.projects.is_empty() {
        lines.push(Line::styled("  no projects in groundcontrol.yaml", theme::muted()));
    }
    for (i, p) in app.projects.iter().enumerate().skip(start).take(visible) {
        let sel = i == app.dash.project_sel;
        let (branch, tree, ab) = match (&p.disabled, &p.git) {
            (Some(_), _) => (("disabled".to_string(), theme::danger()), ("-".to_string(), theme::muted()), "".to_string()),
            (None, Some(g)) => {
                let b = g.branch.clone().unwrap_or_else(|| "?".into());
                let style = if g.origin_matches == Some(false) { theme::danger() } else { theme::text() };
                ((b, style), tree_word(g), if g.upstream.is_some() { format!("{} {}", g.ahead, g.behind) } else { "- -".into() })
            }
            (None, None) => (("…".to_string(), theme::muted()), ("…".to_string(), theme::muted()), String::new()),
        };
        let (svc_text, svc_style) = service_summary(p);
        let name_style = if sel { theme::bold() } else { theme::text() };
        lines.push(selectable_row(
            vec![
                cell(if p.busy_run.is_some() { format!("{} ●", p.id) } else { p.id.clone() }, name_style, 0),
                cell(branch.0, branch.1, 20),
                cell(tree.0, tree.1, 9),
                cell(ab, theme::text(), 7),
                cell(svc_text, svc_style, 18),
            ],
            w,
            sel,
            focused,
        ));
    }
    f.render_widget(para(lines), inner);
}

fn draw_services(app: &App, f: &mut Frame, area: Rect) {
    let focused = app.dash.focus == Focus::Services;
    let Some(p) = selected_project(app) else {
        framed(f, area, vec![title_text("services")], focused);
        return;
    };
    let inner = framed(f, area, vec![title_text(p.id.clone()), title_muted(" services")], focused);
    let sel = app.dash.service_sel.min(service_rows(p).len().saturating_sub(1));
    let start = sel.saturating_sub((inner.height as usize).saturating_sub(1));
    app.hits.add(inner, Target::DashServices, start);
    let lines = service_lines(p, sel, focused, inner.width).into_iter().skip(start).collect();
    f.render_widget(para(lines), inner);
}

fn draw_runs(app: &App, f: &mut Frame, area: Rect) {
    let focused = app.dash.focus == Focus::Runs;
    let inner = framed(f, area, vec![title_text("runs")], focused);
    let w = inner.width;
    let mut lines = Vec::new();
    if app.recent.is_empty() {
        lines.push(Line::styled(" no open runs — open a project and press enter on a pipeline", theme::muted()));
    }
    let visible = inner.height as usize;
    let start = app.dash.run_sel.saturating_sub(visible.saturating_sub(1));
    app.hits.add(inner, Target::DashRuns, start);
    for (i, r) in app.recent.iter().enumerate().skip(start).take(visible) {
        let (word, style) = run_status_word(r.status);
        let mut name = format!("{}/{}", r.project, r.pipeline);
        if r.dirty {
            name.push_str(" dirty");
        }
        let (env, env_style) = match (&r.env, r.protected) {
            (Some(e), true) => (format!("{} {e}", theme::PROTECTED), theme::warning()),
            (None, true) => (theme::PROTECTED.to_string(), theme::warning()),
            (Some(e), false) => (e.clone(), theme::text()),
            (None, false) => ("-".to_string(), theme::text()),
        };
        let selected = i == app.dash.run_sel;
        lines.push(selectable_row(
            vec![
                cell(word, style, 5),
                cell(r.id.to_string(), theme::muted(), 5),
                cell(name, theme::text(), 0),
                cell(env, env_style, 9),
                cell(r.commit.clone().unwrap_or_else(|| "-".into()), theme::text(), 8),
                cell(fmt_duration(run_duration(r)), theme::text(), 7),
                cell(fmt_when(r.started_at), theme::muted(), 12),
            ],
            w,
            selected,
            focused,
        ));
    }
    f.render_widget(para(lines), inner);
}

fn draw_log(app: &mut App, f: &mut Frame, area: Rect) {
    let title = match &app.dash.log_target {
        Some((_, s)) => vec![title_text(format!("{s} output")), title_muted(format!("  last {LOG_LINES} lines"))],
        None => vec![title_text("output")],
    };
    let inner = framed(f, area, title, false);
    app.hits.add(inner, Target::DashLog, 0);
    if app.dash.log_target.is_none() {
        f.render_widget(para(vec![Line::styled(" select a service to see its output", theme::muted())]), inner);
        return;
    }
    if app.dash.log.is_empty() {
        let msg = if app.dash.log.loading { " loading…" } else { " no output yet" };
        f.render_widget(para(vec![Line::styled(msg, theme::muted())]), inner);
        return;
    }
    app.dash.log.render(inner, f.buffer_mut());
}
