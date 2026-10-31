//! History (§10.3): every run with a filter line, and a detail pane with the
//! dirty/unpushed record and the saved diff.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};

use super::super::app::{App, Overlay, Tag, TextOverlay, upsert_run};
use super::super::term_view::TermView;
use super::super::theme;
use super::super::widgets::*;
use crate::protocol::*;

#[derive(Default)]
pub struct HistState {
    pub filter: RunFilter,
    pub filter_text: String,
    pub editing: bool,
    pub filter_error: Option<String>,
    pub runs: Vec<RunSummary>,
    pub total: u64,
    pub next_page: u32,
    pub loading: bool,
    pub sel: usize,
    pub details: HashMap<i64, RunDetail>,
    requested: Option<i64>,
}

impl HistState {
    pub fn on_page(&mut self, page: u32, p: RunPage) {
        self.loading = false;
        if page == 0 {
            self.runs = p.runs;
            self.sel = self.sel.min(self.runs.len().saturating_sub(1));
        } else {
            for r in p.runs {
                if !self.runs.iter().any(|x| x.id == r.id) {
                    self.runs.push(r);
                }
            }
        }
        self.total = p.total;
        self.next_page = page + 1;
    }

    fn matches(&self, s: &RunSummary) -> bool {
        let f = &self.filter;
        f.project.as_ref().is_none_or(|p| p == &s.project)
            && f.pipeline.as_ref().is_none_or(|p| p == &s.pipeline)
            && f.env.as_ref().is_none_or(|e| if e == "-" || e == "none" { s.env.is_none() } else { s.env.as_ref() == Some(e) })
            && f.status.is_none_or(|st| st == s.status)
    }

    pub fn on_run(&mut self, s: &RunSummary) {
        let known = self.runs.iter().any(|r| r.id == s.id);
        if self.matches(s) {
            if !known {
                self.total += 1;
                if self.runs.first().is_some_and(|r| r.id < s.id) || self.runs.is_empty() {
                    self.sel += usize::from(!self.runs.is_empty() && self.sel > 0);
                }
            }
            upsert_run(&mut self.runs, s);
        } else if known {
            // Status filter no longer matches: keep the row until the next reload.
            upsert_run(&mut self.runs, s);
        }
        if let Some(d) = self.details.get_mut(&s.id) {
            d.summary = s.clone();
        }
    }

    pub fn on_step(&mut self, run: i64, step: &StepInfo) {
        if let Some(d) = self.details.get_mut(&run)
            && let Some(slot) = d.steps.get_mut(step.index)
        {
            *slot = step.clone();
        }
    }

    pub fn on_detail(&mut self, id: i64, d: RunDetail) {
        self.details.insert(id, d);
        if self.details.len() > 500 {
            let keep: Vec<i64> = self.runs.iter().take(200).map(|r| r.id).collect();
            self.details.retain(|k, _| keep.contains(k));
        }
    }

    fn selected(&self) -> Option<&RunSummary> {
        self.runs.get(self.sel)
    }
}

pub fn reload(app: &mut App) {
    app.hist.loading = true;
    app.hist.sel = 0;
    let filter = app.hist.filter.clone();
    app.request(Request::RunsQuery { filter, page: 0 }, Tag::History { page: 0 });
}

fn ensure_detail(app: &mut App) {
    let Some(r) = app.hist.selected() else { return };
    let id = r.id;
    let active = r.status.is_active();
    if (!app.hist.details.contains_key(&id) || active) && app.hist.requested != Some(id) {
        app.hist.requested = Some(id);
        app.request(Request::RunGet { id }, Tag::HistoryDetail(id));
    }
}

fn apply_filter(app: &mut App) {
    match RunFilter::parse(&app.hist.filter_text) {
        Ok(f) => {
            app.hist.filter = f;
            app.hist.filter_error = None;
            app.hist.editing = false;
            reload(app);
        }
        Err(e) => app.hist.filter_error = Some(e),
    }
}

pub fn key(app: &mut App, k: KeyEvent) {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if app.hist.editing {
        match k.code {
            KeyCode::Enter => apply_filter(app),
            KeyCode::Esc => {
                app.hist.editing = false;
                app.hist.filter_text = app.hist.filter.to_text();
                app.hist.filter_error = None;
            }
            KeyCode::Char('u') if ctrl => app.hist.filter_text.clear(),
            KeyCode::Backspace => {
                app.hist.filter_text.pop();
            }
            KeyCode::Char(c) if !ctrl => app.hist.filter_text.push(c),
            _ => {}
        }
        return;
    }
    match k.code {
        KeyCode::Char('/') => {
            app.hist.editing = true;
            app.hist.filter_text = app.hist.filter.to_text();
            if !app.hist.filter_text.is_empty() {
                app.hist.filter_text.push(' ');
            }
        }
        KeyCode::Char('u') if ctrl => {
            app.hist.filter = RunFilter::default();
            app.hist.filter_text.clear();
            reload(app);
        }
        KeyCode::Char('s') => {
            let order: Vec<Option<RunStatus>> = std::iter::once(None).chain(RunStatus::ALL.iter().copied().map(Some)).collect();
            let i = order.iter().position(|s| *s == app.hist.filter.status).unwrap_or(0);
            app.hist.filter.status = order[(i + 1) % order.len()];
            app.hist.filter_text = app.hist.filter.to_text();
            reload(app);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.hist.sel = app.hist.sel.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::PageDown | KeyCode::PageUp => {
            let n = app.hist.runs.len();
            if n > 0 {
                app.hist.sel = match k.code {
                    KeyCode::PageDown => (app.hist.sel + 20).min(n - 1),
                    KeyCode::PageUp => app.hist.sel.saturating_sub(20),
                    _ => (app.hist.sel + 1).min(n - 1),
                };
            }
            if app.hist.sel + 30 >= n && (n as u64) < app.hist.total && !app.hist.loading {
                app.hist.loading = true;
                let (filter, page) = (app.hist.filter.clone(), app.hist.next_page);
                app.request(Request::RunsQuery { filter, page }, Tag::History { page });
            }
        }
        KeyCode::Enter => {
            if let Some(r) = app.hist.selected() {
                let id = r.id;
                app.open_run(id);
            }
        }
        KeyCode::Char('D') => {
            let Some(r) = app.hist.selected().cloned() else { return };
            if !r.dirty {
                app.toast(format!("run #{} ran on a clean tree; no diff was saved", r.id), super::super::app::Level::Info);
                return;
            }
            let mut view = TermView::new();
            view.loading = true;
            app.overlay = Some(Overlay::Text(Box::new(TextOverlay {
                title: format!("diff of run #{}", r.id),
                subtitle: String::new(),
                view,
                follow_service: None,
            })));
            app.request(Request::RunDiff { id: r.id }, Tag::Diff);
        }
        _ => {}
    }
    ensure_detail(app);
}

pub fn hints(app: &App) -> Vec<(&'static str, &'static str)> {
    if app.hist.editing {
        return vec![("enter", "apply"), ("esc", "cancel"), ("ctrl+u", "clear")];
    }
    vec![("/", "filter"), ("↑↓", "select"), ("enter", "open run"), ("D", "view diff"), ("s", "status cycle"), ("ctrl+u", "clear filter")]
}

pub fn draw(app: &mut App, f: &mut Frame, area: Rect) {
    ensure_detail(app);
    let h = &app.hist;
    let [filter_area, runs_area, detail_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(5), Constraint::Length(7)]).areas(area);

    let mut fl = vec![Span::styled(" / ", theme::key())];
    let text = if h.editing { format!("{}▏", h.filter_text) } else { h.filter.to_text() };
    let text = if text.is_empty() { "all runs".to_string() } else { text };
    let style = if h.editing { theme::text().bg(theme::SELECTION) } else { theme::text() };
    fl.push(Span::styled(format!(" {} ", text), style));
    fl.push(Span::styled("   filters ", theme::muted()));
    fl.push(Span::styled("project: pipeline: env: status:", theme::muted()));
    fl.push(Span::styled(format!("   {} of {} runs", h.runs.len().min(h.total as usize), h.total), theme::text()));
    if h.loading {
        fl.push(Span::styled("  loading…", theme::muted()));
    }
    let mut flines = vec![clip_line(fl, filter_area.width as usize)];
    if let Some(e) = &h.filter_error {
        flines.push(Line::styled(format!("   × {e}"), theme::danger()));
    }
    f.render_widget(para(flines), filter_area);

    let inner = framed(f, runs_area, vec![title_text("runs")], !h.editing);
    let w = inner.width;
    let header = row(
        vec![
            cell("", theme::muted(), 1),
            cell("id", theme::muted(), 5),
            cell("status", theme::muted(), 6),
            cell("project", theme::muted(), 17),
            cell("pipeline", theme::muted(), 0),
            cell("env", theme::muted(), 7),
            cell("commit", theme::muted(), 8),
            cell("tree", theme::muted(), 15),
            cell("duration", theme::muted(), 8),
            cell("started", theme::muted(), 13),
        ],
        w,
        1,
    );
    let mut lines = vec![header];
    let visible = inner.height.saturating_sub(1) as usize;
    let start = h.sel.saturating_sub(visible.saturating_sub(1));
    for (i, r) in h.runs.iter().enumerate().skip(start).take(visible) {
        let sel = i == h.sel;
        let (word, wstyle) = run_status_word(r.status);
        let (tree, tstyle) = run_tree_word(r);
        let env_style = if r.protected { theme::warning() } else { theme::text() };
        let mut l = row(
            vec![
                cell(if sel { "▶" } else { " " }, theme::key(), 1),
                cell(r.id.to_string(), theme::text(), 5),
                cell(word, wstyle, 6),
                cell(r.project.clone(), theme::text(), 17),
                cell(r.pipeline.clone(), if sel { theme::bold() } else { theme::text() }, 0),
                cell(r.env.clone().unwrap_or_else(|| "-".into()), env_style, 7),
                cell(r.commit.clone().unwrap_or_else(|| "-".into()), theme::text(), 8),
                cell(tree, tstyle, 15),
                cell(fmt_duration(run_duration(r)), theme::text(), 8),
                cell(fmt_when_long(r.started_at), theme::muted(), 13),
            ],
            w,
            1,
        );
        if sel {
            l = l.style(theme::selected());
        }
        lines.push(l);
    }
    if h.runs.is_empty() && !h.loading {
        lines.push(Line::styled(
            if h.filter == RunFilter::default() { " no runs yet" } else { " no runs match this filter" },
            theme::muted(),
        ));
    }
    f.render_widget(para(lines), inner);

    draw_detail(h, f, detail_area);
}

fn draw_detail(h: &HistState, f: &mut Frame, area: Rect) {
    let Some(r) = h.selected() else {
        framed(f, area, vec![title_text("run")], false);
        return;
    };
    let inner = framed(f, area, vec![title_text(format!("run {}", r.id))], false);
    let w = inner.width as usize;
    let d = h.details.get(&r.id);
    let mut lines = Vec::new();
    let mut l1 = vec![Span::styled(r.pipeline.clone(), theme::bold())];
    if let Some(e) = &r.env {
        l1.push(Span::styled(" on ", theme::text()));
        l1.push(Span::styled(e.clone(), theme::warning()));
    }
    l1.push(Span::styled(format!(" at {}", r.commit.clone().unwrap_or_else(|| "?".into())), theme::text()));
    l1.push(Span::styled(format!(" on {}", r.branch.clone().unwrap_or_else(|| "?".into())), theme::text()));
    l1.push(Span::styled(format!(", started {}", fmt_when_long(r.started_at)), theme::text()));
    let outcome = match r.status {
        RunStatus::Running => ", running".to_string(),
        RunStatus::AwaitingApproval => ", awaiting approval".to_string(),
        s => format!(", {} in {}", s.as_str(), fmt_duration(run_duration(r))),
    };
    let (_, ostyle) = run_status_word(r.status);
    l1.push(Span::styled(outcome, ostyle));
    if let Some(reason) = &r.reason {
        l1.push(Span::styled(format!(" ({reason})"), theme::muted()));
    }
    lines.push(clip_line(l1, w));
    if r.dirty {
        let (summary, path) = d.map(|d| (d.diff_summary.clone().unwrap_or_default(), d.diff_path.clone())).unwrap_or_default();
        let mut l = vec![Span::styled("dirty  ", theme::warning()), Span::styled(summary, theme::text())];
        if let Some(p) = path {
            l.push(Span::styled(format!(". Diff saved to {p}"), theme::text()));
        }
        lines.push(clip_line(l, w));
    }
    if r.unpushed {
        let n = d.and_then(|d| d.unpushed_count).unwrap_or(0);
        lines.push(clip_line(
            vec![
                Span::styled("unpushed  ", theme::warning()),
                Span::styled(format!("HEAD was {n} commit{} ahead of origin when it ran", if n == 1 { "" } else { "s" }), theme::text()),
            ],
            w,
        ));
    }
    if let Some(d) = d {
        let mut l = vec![Span::styled("steps  ", theme::muted())];
        for s in &d.steps {
            let (sym, style) = step_symbol(s.status);
            l.push(Span::styled(format!("{sym} "), style));
            l.push(Span::styled(s.name.clone(), theme::text()));
            if let Some(dur) = step_duration(s).filter(|_| s.started_at.is_some()) {
                l.push(Span::styled(format!(" {}", fmt_duration(dur)), theme::muted()));
            }
            l.push(Span::raw("   "));
        }
        lines.push(clip_line(l, w));
        if !d.inputs.is_empty() {
            let ins: Vec<String> = d.inputs.iter().map(|(k, v)| format!("{k}={v}")).collect();
            lines.push(clip_line(vec![Span::styled("inputs  ", theme::muted()), Span::styled(ins.join("  "), theme::text())], w));
        }
    } else {
        lines.push(Line::styled("loading…", theme::muted()));
    }
    f.render_widget(para(lines), inner);
}
