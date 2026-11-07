//! Project screen (§10.3): header, pipelines with protected badges, the
//! selected pipeline's detail, and the project's services.

use std::collections::{BTreeMap, HashMap};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};

use super::super::app::{App, Level, Overlay, Tag, View};
use super::super::theme;
use super::super::widgets::*;
use super::dashboard::{self, service_rows};
use super::dialog::RunDialog;
use crate::protocol::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Pipelines,
    Services,
}

pub struct ProjectView {
    pub id: String,
    pub focus: Focus,
    pub sel: usize,
    pub svc_sel: usize,
    /// Chosen environment index per pipeline.
    pub env_choice: HashMap<String, usize>,
}

impl ProjectView {
    pub fn new(id: &str) -> Self {
        ProjectView { id: id.to_string(), focus: Focus::Pipelines, sel: 0, svc_sel: 0, env_choice: HashMap::new() }
    }

    pub fn env_for(&self, p: &PipelineInfo) -> Option<String> {
        if p.environments.is_empty() {
            return None;
        }
        let i = self.env_choice.get(&p.id).copied().unwrap_or(0) % p.environments.len();
        Some(p.environments[i].clone())
    }
}

fn view(app: &App) -> &ProjectView {
    match app.top() {
        View::Project(v) => v,
        _ => unreachable!("project screen without a project view"),
    }
}

fn view_mut(app: &mut App) -> &mut ProjectView {
    match app.top_mut() {
        View::Project(v) => v,
        _ => unreachable!("project screen without a project view"),
    }
}

pub fn env_protected(p: &ProjectInfo, env: Option<&str>) -> bool {
    env.is_some_and(|e| p.environments.iter().any(|x| x.name == e && x.protected))
}

pub fn key(app: &mut App, k: KeyEvent) {
    let Some(p) = app.project(&view(app).id).cloned() else {
        if k.code == KeyCode::Esc {
            app.pop();
        }
        return;
    };
    let v = view(app);
    let pipelines = p.pipelines.len();
    let svc_len = service_rows(&p).len();
    let pipeline = p.pipelines.get(v.sel.min(pipelines.saturating_sub(1))).cloned();
    match k.code {
        KeyCode::Tab | KeyCode::BackTab => {
            let v = view_mut(app);
            v.focus = if v.focus == Focus::Pipelines { Focus::Services } else { Focus::Pipelines };
        }
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
            let down = matches!(k.code, KeyCode::Down | KeyCode::Char('j'));
            let v = view_mut(app);
            let (sel, len) = match v.focus {
                Focus::Pipelines => (&mut v.sel, pipelines),
                Focus::Services => (&mut v.svc_sel, svc_len),
            };
            if len > 0 {
                *sel = if down { (*sel + 1).min(len - 1) } else { sel.saturating_sub(1) };
            }
        }
        KeyCode::Char('e') => {
            if let Some(pl) = &pipeline {
                if pl.environments.len() > 1 {
                    let v = view_mut(app);
                    *v.env_choice.entry(pl.id.clone()).or_default() += 1;
                } else if pl.environments.is_empty() {
                    app.toast(format!("{} does not run against an environment", pl.id), Level::Info);
                } else {
                    app.toast(format!("{} only runs on {}", pl.id, pl.environments[0]), Level::Info);
                }
            }
        }
        KeyCode::Enter if v.focus == Focus::Pipelines => {
            if let Some(pl) = pipeline {
                open_dialog(app, &p, &pl, false);
            }
        }
        KeyCode::Char('d') if v.focus == Focus::Pipelines => {
            if let Some(pl) = pipeline {
                open_dialog(app, &p, &pl, true);
            }
        }
        // The services pane's keys act on its selection, which shows only while it has focus.
        KeyCode::Char('s') | KeyCode::Char('r') | KeyCode::Char('l') | KeyCode::Enter if v.focus == Focus::Services => {
            let rows = service_rows(&p);
            let Some(row) = rows.get(v.svc_sel.min(rows.len().saturating_sub(1))).cloned() else {
                app.toast(format!("{} has no services", p.id), Level::Info);
                return;
            };
            match k.code {
                KeyCode::Char('l') | KeyCode::Enter => {
                    if let Some(svc) = dashboard::row_service(&p, &row) {
                        dashboard::open_full_log(app, &p.id, &svc);
                    }
                }
                c => dashboard::control(app, &p, &row, c == KeyCode::Char('r')),
            }
        }
        _ => {}
    }
}

fn open_dialog(app: &mut App, p: &ProjectInfo, pl: &PipelineInfo, dry: bool) {
    let env = view(app).env_for(pl);
    let mut d = RunDialog::new(p, pl, env);
    if dry && d.inputs_valid() {
        d.busy = true;
        let req = d.request(true);
        app.overlay = Some(Overlay::Dialog(Box::new(d)));
        app.request(req, Tag::DryRun);
        return;
    }
    if dry {
        d.error = Some("fill in the inputs, then press ctrl+d for the dry run".into());
    }
    app.overlay = Some(Overlay::Dialog(Box::new(d)));
}

pub fn hints(app: &App) -> Vec<(&'static str, &'static str)> {
    match view(app).focus {
        Focus::Pipelines => {
            vec![("↑↓", "select"), ("enter", "run"), ("d", "dry run"), ("e", "environment"), ("tab", "next pane"), ("?", "help")]
        }
        Focus::Services => vec![("↑↓", "select"), ("s", "start/stop"), ("r", "restart"), ("l", "log"), ("tab", "next pane"), ("?", "help")],
    }
}

pub fn draw(app: &App, f: &mut Frame, area: Rect) {
    let v = view(app);
    let Some(p) = app.project(&v.id) else {
        f.render_widget(
            para(vec![Line::styled(format!(" project `{}` is no longer in the config (esc to go back)", v.id), theme::danger())]),
            area,
        );
        return;
    };
    let [header, body] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);
    draw_header(p, f, header);
    let [left, right] = Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(body);
    draw_pipelines(p, v, f, left);
    let pl = p.pipelines.get(v.sel.min(p.pipelines.len().saturating_sub(1)));
    let detail_h =
        pl.map(|pl| pl.steps.len() as u16 * 2 + 6 + u16::from(pl.warning.is_some()) + u16::from(!pl.inputs.is_empty())).unwrap_or(4);
    let svc_h = service_rows(p).len() as u16 + 2;
    let [detail, services] =
        Layout::vertical([Constraint::Max(detail_h.max(5)), Constraint::Min(svc_h.min(right.height / 2))]).areas(right);
    draw_detail(p, v, pl, f, detail);
    let focused = v.focus == Focus::Services;
    let inner = framed(f, services, vec![title_text("services")], focused);
    let rows = service_rows(p);
    let sel = focused.then_some(v.svc_sel.min(rows.len().saturating_sub(1)));
    f.render_widget(para(dashboard::service_lines(p, sel, inner.width)), inner);
}

fn draw_header(p: &ProjectInfo, f: &mut Frame, area: Rect) {
    let mut s =
        vec![Span::styled(format!(" {}", p.id), theme::bold()), Span::styled(format!("  {}", truncate_left(&p.path, 36)), theme::muted())];
    if let Some(reason) = &p.disabled {
        s.push(Span::styled(format!("   × disabled: {reason}"), theme::danger()));
    } else if let Some(g) = &p.git {
        match g.origin_matches {
            Some(true) => s.push(Span::styled("   ✓ origin matches", theme::success())),
            Some(false) => s.push(Span::styled(
                format!("   × origin mismatch ({})", g.origin_url.clone().unwrap_or_else(|| "no origin".into())),
                theme::danger(),
            )),
            None => s.push(Span::styled("   no remote declared", theme::muted())),
        }
        s.push(Span::styled(
            format!("   {} {}", g.branch.clone().unwrap_or_else(|| "?".into()), g.commit.clone().unwrap_or_default()),
            theme::text(),
        ));
        let (t, st) = tree_word(g);
        s.push(Span::styled(format!("  {t}"), st));
        if g.upstream.is_some() {
            s.push(Span::styled(format!("  ↑{} ↓{}", g.ahead, g.behind), theme::text()));
        } else {
            s.push(Span::styled("  no upstream", theme::muted()));
        }
    }
    if let Some(id) = p.busy_run {
        s.push(Span::styled(format!("   ● run #{id} in progress"), theme::info()));
    }
    f.render_widget(para(vec![clip_line(s, area.width as usize)]), area);
}

fn draw_pipelines(p: &ProjectInfo, v: &ProjectView, f: &mut Frame, area: Rect) {
    let focused = v.focus == Focus::Pipelines;
    let inner = framed(f, area, vec![title_text("pipelines"), title_muted(format!(" {}", p.pipelines.len()))], focused);
    let w = inner.width;
    let mut lines = Vec::new();
    if p.pipelines.is_empty() {
        lines.push(Line::styled(" no pipelines (services only)", theme::muted()));
    }
    for (i, pl) in p.pipelines.iter().enumerate() {
        let sel = i == v.sel;
        let env = v.env_for(pl);
        let protected = pl.protected || env_protected(p, env.as_deref());
        let badge = match (&env, protected) {
            (Some(e), true) => format!("{} {e}", theme::PROTECTED),
            (Some(e), false) => e.clone(),
            (None, true) => format!("{} protected", theme::PROTECTED),
            (None, false) => String::new(),
        };
        let (hint, hint_style) = if pl.waiting > 0 {
            (format!("{} waiting", pl.waiting), theme::warning())
        } else if pl.last_run.as_ref().is_some_and(|r| r.status.is_active()) {
            (format!("running #{}", pl.last_run.as_ref().unwrap().id), theme::info())
        } else if pl.warning.is_some() {
            ("destructive".to_string(), theme::danger())
        } else if !pl.inputs.is_empty() {
            (format!("input: {}", pl.inputs.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().join(", ")), theme::muted())
        } else if pl.steps.len() == 1 {
            (pl.steps[0].detail.lines().next().unwrap_or("").to_string(), theme::muted())
        } else {
            (pl.steps.iter().map(|s| s.label.clone()).collect::<Vec<_>>().join(" + "), theme::muted())
        };
        let mut l = row(
            vec![
                cell(if sel { "▶" } else { " " }, theme::key(), 1),
                cell(pl.id.clone(), if sel { theme::bold() } else { theme::text() }, 18),
                cell(badge, if protected { theme::warning() } else { theme::muted() }, 12),
                cell(hint, hint_style, 0),
            ],
            w,
            1,
        );
        if sel && focused {
            l = l.style(theme::selected());
        }
        lines.push(l);
    }
    f.render_widget(para(lines), inner);
}

fn draw_detail(p: &ProjectInfo, v: &ProjectView, pl: Option<&PipelineInfo>, f: &mut Frame, area: Rect) {
    let Some(pl) = pl else {
        framed(f, area, vec![title_text("pipeline")], false);
        return;
    };
    let env = v.env_for(pl);
    let protected = pl.protected || env_protected(p, env.as_deref());
    let mut title = vec![title_text(pl.id.clone())];
    match &env {
        Some(e) => title.push(title_muted(format!("  env {e}{}", if protected { " (protected)" } else { "" }))),
        None if protected => title.push(title_muted("  (protected)")),
        None => {}
    }
    if pl.environments.len() > 1 {
        title.push(title_muted("  e: switch"));
    }
    let inner = framed(f, area, title, false);
    let w = inner.width as usize;
    let mut lines = Vec::new();
    let vars: BTreeMap<&str, &str> = BTreeMap::from([("GC_ENV", env.as_deref().unwrap_or(""))]);
    for (i, s) in pl.steps.iter().enumerate() {
        let (kind_style, label_style) = match s.kind {
            StepKindName::Approve => (theme::warning(), theme::warning()),
            StepKindName::Service => (theme::info(), theme::text()),
            StepKindName::Run => (theme::success(), theme::text()),
        };
        let label = if s.kind == StepKindName::Approve {
            let mut m = s.detail.clone();
            for (k, val) in &vars {
                if !val.is_empty() {
                    m = m.replace(&format!("${k}"), val);
                }
            }
            format!("\"{m}\"")
        } else {
            s.label.clone()
        };
        lines.push(clip_line(
            vec![
                Span::styled(fit(s.kind.as_str(), 8), kind_style),
                Span::styled(format!("{:>2}  ", i + 1), theme::muted()),
                Span::styled(label, label_style),
            ],
            w,
        ));
        if s.kind == StepKindName::Run && s.detail.trim() != s.label.trim() {
            let first = s.detail.lines().next().unwrap_or("");
            let more = if s.detail.lines().count() > 1 { " …" } else { "" };
            lines.push(clip_line(vec![Span::raw(" ".repeat(12)), Span::styled(format!("{first}{more}"), theme::info())], w));
        }
    }
    if !pl.inputs.is_empty() {
        let ins: Vec<String> = pl
            .inputs
            .iter()
            .map(|i| format!("{} ({})", i.name, i.pattern.clone().unwrap_or_else(|| format!("{:?}", i.kind).to_lowercase())))
            .collect();
        lines.push(clip_line(vec![Span::styled("inputs  ", theme::muted()), Span::styled(ins.join(", "), theme::text())], w));
    }
    if let Some(warn) = &pl.warning {
        lines.push(clip_line(vec![Span::styled(format!("× {warn}"), theme::danger())], w));
    }
    lines.push(Line::raw(""));
    match &pl.last_run {
        Some(r) => {
            let (word, style) = run_status_word(r.status);
            lines.push(clip_line(
                vec![
                    Span::styled("last run  ", theme::muted()),
                    Span::styled(format!("#{}  ", r.id), theme::text()),
                    Span::styled(word, style),
                    Span::styled(format!("  {}", fmt_when_long(r.started_at)), theme::muted()),
                    Span::styled(format!("  {}", r.commit.clone().unwrap_or_default()), theme::text()),
                    Span::styled(r.env.as_ref().map(|e| format!("  {e}")).unwrap_or_default(), theme::warning()),
                ],
                w,
            ));
        }
        None => lines.push(Line::styled("no run with a window", theme::muted())),
    }
    f.render_widget(para(lines), inner);
}
