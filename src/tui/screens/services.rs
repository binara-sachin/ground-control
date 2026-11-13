//! The services pane both the Dashboard and Project screens show: its rows,
//! their text, and the start/stop/restart and full-output actions on a row.

use ratatui::text::Line;

use super::super::app::{App, Level, Reply};
use super::super::term_view::TermView;
use super::super::theme;
use super::super::widgets::*;
use super::overlay::{Overlay, TextOverlay};
use crate::config::ServiceAction;
use crate::model::*;

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

/// The service a row's output comes from: the service itself, or for a group a
/// member that crashed or is unhealthy, otherwise its last member.
pub fn row_service(p: &ProjectInfo, row: &SvcRow) -> Option<String> {
    match row {
        SvcRow::Service(s) => Some(s.clone()),
        SvcRow::Group(g) => {
            let g = p.groups.iter().find(|x| &x.id == g)?;
            g.members
                .iter()
                .find(|m| p.services.iter().any(|s| &s.id == *m && matches!(s.state, ServiceState::Crashed | ServiceState::Unhealthy)))
                .or(g.members.last())
                .cloned()
        }
    }
}

pub fn open_full_log(app: &mut App, project: &str, service: &str) {
    let mut view = TermView::new();
    view.loading = true;
    app.overlay = Some(Overlay::Text(Box::new(TextOverlay {
        title: format!("{project}/{service} output"),
        subtitle: String::new(),
        view,
        follow_service: Some((project.to_string(), service.to_string())),
        dialog: None,
    })));
    let (project, service) = (project.to_string(), service.to_string());
    app.call(move |e| async move { Reply::FullOutput(e.service_output(&project, &service).await) });
    app.sync_watch();
}

/// Start/stop toggle or restart for a service or group row.
pub fn control(app: &mut App, project: &ProjectInfo, row: &SvcRow, restart: bool) {
    // Up while its worker has anything to stop, restarting and stopping included.
    let up = |x: &ServiceStatus| !matches!(x.state, ServiceState::Stopped | ServiceState::Crashed);
    let (id, up) = match row {
        SvcRow::Service(s) => (s.clone(), project.services.iter().any(|x| &x.id == s && up(x))),
        SvcRow::Group(g) => {
            let members = project.groups.iter().find(|x| &x.id == g).map(|x| x.members.clone()).unwrap_or_default();
            (g.clone(), project.services.iter().any(|x| members.contains(&x.id) && up(x)))
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
    let project = project.id.clone();
    app.call(move |e| async move { Reply::ServiceControl(e.control_service(&project, &id, action).await) });
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

fn group_word(p: &ProjectInfo, g: &GroupInfo) -> (String, ratatui::style::Style) {
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

/// Lines for a services pane.
pub fn service_lines(p: &ProjectInfo, sel: usize, focused: bool, w: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, r) in service_rows(p).iter().enumerate() {
        let is_sel = sel == i;
        let l = match r {
            SvcRow::Group(g) => {
                let gi = p.groups.iter().find(|x| &x.id == g).unwrap();
                let (word, style) = group_word(p, gi);
                selectable_row(
                    vec![
                        cell("grp", theme::muted(), 3),
                        cell(g.clone(), theme::text(), 14),
                        cell(word, style, 16),
                        cell(gi.members.join(" → "), theme::muted(), 0),
                    ],
                    w,
                    is_sel,
                    focused,
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
                selectable_row(
                    vec![
                        cell(sym, style, 3),
                        cell(id.clone(), theme::text(), 14),
                        cell(word, style, 16),
                        cell(port, theme::muted(), 6),
                        cell(detail, detail_style, 0),
                    ],
                    w,
                    is_sel,
                    focused,
                )
            }
        };
        lines.push(l);
    }
    if lines.is_empty() {
        lines.push(Line::styled(" no services", theme::muted()));
    }
    lines
}
