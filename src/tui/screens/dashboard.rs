//! Dashboard (§10.3): approval banner, projects, services of the selected
//! project, recent runs, and the selected service's log.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};

use super::super::app::{App, Level, Overlay, Tag, TextOverlay};
use super::super::term_view::TermView;
use super::super::theme;
use super::super::widgets::*;
use crate::protocol::*;

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

/// A row of a services pane: a group or a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SvcRow {
    Group(String),
    Service(String),
}

pub fn service_rows(p: &ProjectInfo) -> Vec<SvcRow> {
    let mut rows: Vec<SvcRow> = p.groups.iter().map(|g| SvcRow::Group(g.id.clone())).collect();
    rows.extend(p.services.iter().map(|s| SvcRow::Service(s.id.clone())));
    rows
}

fn selected_project(app: &App) -> Option<&ProjectInfo> {
    app.projects.get(app.dash.project_sel.min(app.projects.len().saturating_sub(1)))
}

/// The service whose log the log pane shows: the selected service, or the
/// first member of the selected group.
fn log_service(app: &App) -> Option<(String, String)> {
    let p = selected_project(app)?;
    let rows = service_rows(p);
    let row = rows.get(app.dash.service_sel.min(rows.len().saturating_sub(1)))?;
    let id = match row {
        SvcRow::Service(s) => s.clone(),
        SvcRow::Group(g) => {
            let g = p.groups.iter().find(|x| &x.id == g)?;
            // Prefer a member that crashed or is unhealthy; otherwise the last one.
            g.members
                .iter()
                .find(|m| p.services.iter().any(|s| &s.id == *m && matches!(s.state, ServiceState::Crashed | ServiceState::Unhealthy)))
                .or(g.members.last())?
                .clone()
        }
    };
    Some((p.id.clone(), id))
}

/// Loads the last lines of the selected service's log when the selection changes.
pub fn load_log(app: &mut App, force: bool) {
    let target = log_service(app);
    if target == app.dash.log_target && !force {
        return;
    }
    app.dash.log_target = target.clone();
    app.dash.log.reset();
    if let Some((project, service)) = target {
        app.dash.log.loading = true;
        app.request(Request::ServiceLog { project: project.clone(), id: service.clone() }, Tag::ServiceLog { project, service });
    }
    app.sync_subscription();
}

pub fn open_full_log(app: &mut App, project: &str, service: &str) {
    let mut view = TermView::new();
    view.loading = true;
    app.overlay = Some(Overlay::Text(Box::new(TextOverlay {
        title: format!("{project}/{service} log"),
        subtitle: String::new(),
        view,
        follow_service: Some((project.to_string(), service.to_string())),
    })));
    app.request(Request::ServiceLog { project: project.into(), id: service.into() }, Tag::FullLog);
    app.sync_subscription();
}

/// Start/stop toggle or restart for a service or group row.
pub fn control(app: &mut App, project: &ProjectInfo, row: &SvcRow, restart: bool) {
    let (id, up) = match row {
        SvcRow::Service(s) => {
            (s.clone(), project.services.iter().any(|x| &x.id == s && x.state != ServiceState::Stopped && x.state != ServiceState::Crashed))
        }
        SvcRow::Group(g) => {
            let members = project.groups.iter().find(|x| &x.id == g).map(|x| x.members.clone()).unwrap_or_default();
            (g.clone(), project.services.iter().any(|x| members.contains(&x.id) && x.state.is_up()))
        }
    };
    let action = if restart {
        ServiceAction::Restart
    } else if up {
        ServiceAction::Stop
    } else {
        ServiceAction::Start
    };
    let verb = match action {
        ServiceAction::Start => "starting",
        ServiceAction::Stop => "stopping",
        ServiceAction::Restart => "restarting",
    };
    app.toast(format!("{verb} {id}…"), Level::Info);
    app.request(Request::ServiceControl { project: project.id.clone(), id, action }, Tag::Service);
}

pub fn key(app: &mut App, k: KeyEvent) {
    let has_banner = !app.waiting.is_empty();
    let order: Vec<Focus> = if has_banner {
        vec![Focus::Banner, Focus::Projects, Focus::Services, Focus::Runs]
    } else {
        vec![Focus::Projects, Focus::Services, Focus::Runs]
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
        KeyCode::Char('s') | KeyCode::Char('r') => {
            let Some(p) = selected_project(app).cloned() else { return };
            let rows = service_rows(&p);
            if let Some(row) = rows.get(app.dash.service_sel.min(rows.len().saturating_sub(1))) {
                control(app, &p, row, k.code == KeyCode::Char('r'));
            } else {
                app.toast(format!("{} has no services", p.id), Level::Info);
            }
        }
        KeyCode::Char('l') => {
            if let Some((p, s)) = log_service(app) {
                open_full_log(app, &p, &s);
            }
        }
        _ => {}
    }
}

pub fn hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let mut h = vec![("↑↓", "select"), ("enter", "open"), ("tab", "next pane")];
    if app.dash.focus == Focus::Banner {
        h[1] = ("enter", "review");
    }
    h.extend([("s", "start/stop"), ("r", "restart"), ("l", "full log"), ("?", "help")]);
    h
}

pub fn draw(app: &mut App, f: &mut Frame, area: Rect) {
    let banner_h = if app.waiting.is_empty() { 0 } else { 4 };
    let [banner, body] = Layout::vertical([Constraint::Length(banner_h), Constraint::Min(0)]).areas(area);
    if banner_h > 0 {
        draw_banner(app, f, banner);
    }
    let svc_rows = selected_project(app).map(|p| service_rows(p).len()).unwrap_or(0) as u16;
    let top_h = (app.projects.len() as u16 + 5).max(svc_rows + 2).max(7).min(body.height / 2 + 2);
    let [top, bottom] = Layout::vertical([Constraint::Length(top_h), Constraint::Min(0)]).areas(body);
    let [tl, tr] = Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)]).areas(top);
    let [bl, br] = Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)]).areas(bottom);
    draw_projects(app, f, tl);
    draw_services(app, f, tr);
    draw_runs(app, f, bl);
    draw_log(app, f, br);
}

fn draw_banner(app: &App, f: &mut Frame, area: Rect) {
    let focused = app.dash.focus == Focus::Banner;
    let title = if app.waiting.len() == 1 { "approval waiting".to_string() } else { format!("{} approvals waiting", app.waiting.len()) };
    let inner = framed_alert(f, area, &title, true);
    let a = &app.waiting[0];
    let run = app.recent.iter().find(|r| r.id == a.run_id);
    let w = inner.width as usize;
    let left_mark = if focused { Span::styled("▶ ", theme::key()) } else { Span::styled("▲ ", theme::warning()) };
    let mut l1 = vec![
        left_mark,
        Span::styled(format!("{}/{}", a.project, a.pipeline), theme::bold()),
        Span::styled(format!(" #{}", a.run_id), theme::muted()),
    ];
    if let Some(env) = &a.env {
        l1.push(Span::styled("   env ", theme::muted()));
        l1.push(Span::styled(env.clone(), theme::warning()));
    }
    if let Some(r) = run {
        l1.push(Span::styled("   commit ", theme::muted()));
        l1.push(Span::styled(r.commit.clone().unwrap_or_else(|| "-".into()), theme::text()));
        let (t, s) = run_tree_word(r);
        let t = if t == "clean" { "clean, pushed".to_string() } else { t };
        l1.push(Span::styled(format!("   {t}"), if r.dirty || r.unpushed { s } else { theme::text() }));
    }
    let left_secs = a.expires_at - now();
    let right = vec![
        Span::styled(format!("expires {} ({})   ", fmt_clock(a.expires_at), fmt_age(left_secs)), theme::muted()),
        Span::styled("enter", theme::key()),
        Span::styled(" review", theme::text()),
    ];
    let right_w: usize = right.iter().map(|s| width(&s.content)).sum();
    let mut line1 = clip_line(l1, w.saturating_sub(right_w + 2)).spans;
    let used: usize = line1.iter().map(|s| width(&s.content)).sum();
    line1.push(Span::raw(" ".repeat(w.saturating_sub(used + right_w))));
    line1.extend(right);
    let more = if app.waiting.len() > 1 { format!("  (+{} more)", app.waiting.len() - 1) } else { String::new() };
    let l2 =
        clip_line(vec![Span::raw("  "), Span::styled(format!("\"{}\"", a.message), theme::text()), Span::styled(more, theme::muted())], w);
    f.render_widget(para(vec![clip_line(line1, w), l2]), inner);
}

fn draw_projects(app: &App, f: &mut Frame, area: Rect) {
    let focused = matches!(app.dash.focus, Focus::Projects);
    let inner = framed(f, area, vec![title_text("projects"), title_muted(format!(" {}", app.projects.len()))], focused);
    let w = inner.width;
    let mut lines = vec![row(
        vec![
            cell("", theme::muted(), 1),
            cell("project", theme::muted(), 18),
            cell("branch", theme::muted(), 0),
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
    for (i, p) in app.projects.iter().enumerate() {
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
        let mut l = row(
            vec![
                cell(if sel { "▶" } else { " " }, theme::key(), 1),
                cell(if p.busy_run.is_some() { format!("{} ●", p.id) } else { p.id.clone() }, name_style, 18),
                cell(branch.0, branch.1, 0),
                cell(tree.0, tree.1, 9),
                cell(ab, theme::text(), 7),
                cell(svc_text, svc_style, 18),
            ],
            w,
            1,
        );
        if sel && focused {
            l = l.style(theme::selected());
        } else if sel {
            l = l.style(ratatui::style::Style::new().bg(theme::BAR_BG));
        }
        lines.push(l);
    }
    lines.push(Line::raw(""));
    lines.push(clip_line(
        vec![Span::styled("↑ ↓ are relative to the last fetch. GroundControl never fetches on its own.", theme::muted())],
        w as usize,
    ));
    f.render_widget(para(lines), inner);
}

fn is_running(s: &ServiceStatus) -> bool {
    matches!(s.state, ServiceState::Running | ServiceState::Unhealthy)
}

/// "2/3 up", "1 crashed", "none".
pub fn service_summary(p: &ProjectInfo) -> (String, ratatui::style::Style) {
    if p.services.is_empty() {
        return ("none".into(), theme::muted());
    }
    let up = p.services.iter().filter(|s| is_running(s)).count();
    let crashed = p.services.iter().filter(|s| s.state == ServiceState::Crashed).count();
    let total = p.services.len();
    if crashed > 0 {
        return (format!("{up}/{total} up  {crashed} crashed"), theme::danger());
    }
    let style = if up == 0 {
        theme::muted()
    } else if up == total {
        theme::success()
    } else {
        theme::warning()
    };
    (format!("{up}/{total} up"), style)
}

pub fn group_word(p: &ProjectInfo, g: &GroupInfo) -> (String, ratatui::style::Style) {
    let up = g.members.iter().filter(|m| p.services.iter().any(|s| &s.id == *m && is_running(s))).count();
    let crashed = g.members.iter().any(|m| p.services.iter().any(|s| s.id == *m && s.state == ServiceState::Crashed));
    let n = g.members.len();
    if up == n {
        (format!("running {up}/{n}"), theme::success())
    } else if up == 0 && !crashed {
        (format!("stopped 0/{n}"), theme::muted())
    } else if crashed {
        (format!("partial {up}/{n}"), theme::danger())
    } else {
        (format!("partial {up}/{n}"), theme::warning())
    }
}

/// Lines for a services pane (shared with the project screen).
pub fn service_lines(p: &ProjectInfo, sel: Option<usize>, w: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, r) in service_rows(p).iter().enumerate() {
        let is_sel = sel == Some(i);
        let marker = if is_sel { "▶" } else { " " };
        let mut l = match r {
            SvcRow::Group(g) => {
                let gi = p.groups.iter().find(|x| &x.id == g).unwrap();
                let (word, style) = group_word(p, gi);
                row(
                    vec![
                        cell(marker, theme::key(), 1),
                        cell("grp", theme::muted(), 3),
                        cell(g.clone(), theme::text(), 14),
                        cell(word, style, 16),
                        cell(gi.members.join(" → "), theme::muted(), 0),
                    ],
                    w,
                    1,
                )
            }
            SvcRow::Service(id) => {
                let s = p.services.iter().find(|x| &x.id == id).unwrap();
                let (sym, word, style) = service_word(s);
                let port = s.port.map(|p| format!(":{p}")).unwrap_or_default();
                let detail = match (&s.message, s.started_at) {
                    (Some(m), _) => m.clone(),
                    (None, Some(t)) if s.state.is_up() => format!("up {}", fmt_age(now() - t)),
                    _ if !s.depends_on.is_empty() && s.state == ServiceState::Stopped => format!("needs {}", s.depends_on.join(", ")),
                    _ => String::new(),
                };
                let detail_style = if s.state == ServiceState::Crashed { theme::danger() } else { theme::muted() };
                row(
                    vec![
                        cell(marker, theme::key(), 1),
                        cell(sym, style, 3),
                        cell(id.clone(), theme::text(), 14),
                        cell(word, style, 16),
                        cell(port, theme::muted(), 6),
                        cell(detail, detail_style, 0),
                    ],
                    w,
                    1,
                )
            }
        };
        if is_sel {
            l = l.style(theme::selected());
        }
        lines.push(l);
    }
    if lines.is_empty() {
        lines.push(Line::styled(" no services", theme::muted()));
    }
    lines
}

fn draw_services(app: &App, f: &mut Frame, area: Rect) {
    let focused = app.dash.focus == Focus::Services;
    let Some(p) = selected_project(app) else {
        framed(f, area, vec![title_text("services")], focused);
        return;
    };
    let inner = framed(f, area, vec![title_text(p.id.clone()), title_muted(" services")], focused);
    let rows = service_rows(p);
    let sel = focused.then_some(app.dash.service_sel.min(rows.len().saturating_sub(1)));
    let mut lines = service_lines(p, sel, inner.width);
    if !focused && !rows.is_empty() {
        let i = app.dash.service_sel.min(rows.len() - 1);
        lines[i] = lines[i].clone().style(ratatui::style::Style::new().bg(theme::BAR_BG));
    }
    f.render_widget(para(lines), inner);
}

fn draw_runs(app: &App, f: &mut Frame, area: Rect) {
    let focused = app.dash.focus == Focus::Runs;
    let inner = framed(f, area, vec![title_text("recent runs")], focused);
    let w = inner.width;
    let mut lines = Vec::new();
    if app.recent.is_empty() {
        lines.push(Line::styled(" no runs yet — open a project and press enter on a pipeline", theme::muted()));
    }
    let visible = inner.height as usize;
    let start = app.dash.run_sel.saturating_sub(visible.saturating_sub(1));
    for (i, r) in app.recent.iter().enumerate().skip(start).take(visible) {
        let (word, style) = run_status_word(r.status);
        let mut name = format!("{}/{}", r.project, r.pipeline);
        if r.dirty {
            name.push_str(" dirty");
        }
        let env_style = if r.protected { theme::warning() } else { theme::text() };
        let mut l = row(
            vec![
                cell(word, style, 5),
                cell(r.id.to_string(), theme::muted(), 5),
                cell(name, theme::text(), 0),
                cell(r.env.clone().unwrap_or_else(|| "-".into()), env_style, 7),
                cell(r.commit.clone().unwrap_or_else(|| "-".into()), theme::text(), 8),
                cell(fmt_duration(run_duration(r)), theme::text(), 7),
                cell(fmt_when(r.started_at), theme::muted(), 12),
            ],
            w,
            1,
        );
        if focused && i == app.dash.run_sel {
            l = l.style(theme::selected());
        }
        lines.push(l);
    }
    f.render_widget(para(lines), inner);
}

fn draw_log(app: &mut App, f: &mut Frame, area: Rect) {
    let title = match &app.dash.log_target {
        Some((_, s)) => vec![title_text(format!("{s} log")), title_muted(format!("  last {LOG_LINES} lines"))],
        None => vec![title_text("log")],
    };
    let inner = framed(f, area, title, false);
    if app.dash.log_target.is_none() {
        f.render_widget(para(vec![Line::styled(" select a service to see its log", theme::muted())]), inner);
        return;
    }
    if app.dash.log.is_empty() {
        let msg = if app.dash.log.loading { " loading…" } else { " no output yet" };
        f.render_widget(para(vec![Line::styled(msg, theme::muted())]), inner);
        return;
    }
    app.dash.log.render(inner, f.buffer_mut());
}
