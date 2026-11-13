//! Run view (§10.3, §6.3): header, steps beside the run's inputs and outputs,
//! the live output pane with follow mode, and the approval panel. Approving
//! happens only here; a protected approval needs a confirm keypress, rejecting
//! takes one key.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::super::app::{App, View};
use super::super::clipboard::copy_to_clipboard;
use super::super::mouse::{HitMap, Target};
use super::super::term_view::TermView;
use super::super::theme;
use super::super::widgets::*;
use super::overlay::{Confirm, ConfirmAction, Overlay};
use crate::model::*;

pub struct RunView {
    pub id: i64,
    pub detail: Option<RunDetail>,
    pub sel: usize,
    /// Track the active step until the user picks one.
    pub auto_select: bool,
    pub outputs: HashMap<usize, TermView>,
    pub log_errors: HashMap<usize, String>,
}

impl RunView {
    pub fn new(id: i64) -> RunView {
        RunView { id, detail: None, sel: 0, auto_select: true, outputs: HashMap::new(), log_errors: HashMap::new() }
    }

    fn active_step(&self) -> Option<usize> {
        let d = self.detail.as_ref()?;
        d.steps
            .iter()
            .find(|s| matches!(s.status, StepStatus::Running | StepStatus::Waiting))
            .or_else(|| d.steps.iter().rev().find(|s| s.started_at.is_some() && s.kind != StepKindName::Approve))
            .or_else(|| d.steps.iter().rev().find(|s| s.started_at.is_some()))
            .map(|s| s.index)
    }

    /// For a waiting approval, show the output of the step before it.
    fn auto_target(&self) -> Option<usize> {
        let d = self.detail.as_ref()?;
        let a = self.active_step()?;
        if d.steps.get(a).is_some_and(|s| s.kind == StepKindName::Approve) {
            return (0..a).rev().find(|i| d.steps[*i].kind != StepKindName::Approve).or(Some(a));
        }
        Some(a)
    }

    /// Returns the output reads needed for the selected step.
    fn wanted_reads(&mut self) -> Vec<(i64, usize)> {
        let Some(d) = &self.detail else { return Vec::new() };
        let Some(step) = d.steps.get(self.sel) else { return Vec::new() };
        if step.started_at.is_none() && step.status != StepStatus::Running {
            return Vec::new();
        }
        let v = self.outputs.entry(self.sel).or_default();
        if v.is_empty() && !v.loading && !v.complete {
            v.loading = true;
            return vec![(self.id, self.sel)];
        }
        Vec::new()
    }

    pub fn on_detail(&mut self, d: RunDetail) -> Vec<(i64, usize)> {
        self.detail = Some(d);
        if self.auto_select
            && let Some(t) = self.auto_target()
        {
            self.sel = t;
        }
        self.wanted_reads()
    }

    pub fn on_summary(&mut self, s: &RunSummary) {
        if let Some(d) = &mut self.detail
            && d.summary.id == s.id
        {
            d.summary = s.clone();
        }
    }

    pub fn on_step(&mut self, step: StepInfo) {
        if let Some(d) = &mut self.detail
            && let Some(slot) = d.steps.get_mut(step.index)
        {
            *slot = step;
        }
        if self.auto_select
            && let Some(t) = self.auto_target()
        {
            self.sel = t;
        }
    }

    pub fn on_approval(&mut self, a: ApprovalInfo) {
        if let Some(d) = &mut self.detail {
            d.approvals.retain(|x| x.step_index != a.step_index);
            d.approvals.push(a);
        }
    }

    /// `step_index` is the step that wrote the output, as the worker recorded it.
    pub fn on_output(&mut self, key: &str, value: &str, step_index: Option<usize>) {
        if let Some(d) = &mut self.detail {
            match d.outputs.iter_mut().find(|o| o.key == key) {
                Some(o) => {
                    o.value = value.to_string();
                    o.step_index = step_index;
                }
                None => d.outputs.push(OutputInfo { key: key.into(), value: value.into(), step_index }),
            }
        }
    }

    /// Shows a step's output: each chunk is the step's whole output so far,
    /// captured from the run's tmux pane.
    pub fn on_log(&mut self, c: &LogChunk) {
        let v = self.outputs.entry(c.step).or_default();
        v.loading = false;
        v.replace(&c.data);
        v.complete = c.complete;
    }

    pub fn on_log_error(&mut self, step: usize, msg: &str) {
        if let Some(v) = self.outputs.get_mut(&step) {
            v.loading = false;
        }
        self.log_errors.insert(step, msg.to_string());
    }

    fn pending_approval(&self) -> Option<&ApprovalInfo> {
        self.detail.as_ref()?.approvals.iter().find(|a| a.status == ApprovalStatus::Pending)
    }
}

fn view(app: &App) -> &RunView {
    match app.top() {
        View::Run(v) => v,
        _ => unreachable!("run screen without a run view"),
    }
}

fn view_mut(app: &mut App) -> &mut RunView {
    match app.top_mut() {
        View::Run(v) => v,
        _ => unreachable!("run screen without a run view"),
    }
}

pub fn key(app: &mut App, k: KeyEvent) {
    let steps = view(app).detail.as_ref().map(|d| d.steps.len()).unwrap_or(0);
    match k.code {
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
            let down = matches!(k.code, KeyCode::Down | KeyCode::Char('j'));
            let v = view_mut(app);
            if steps > 0 {
                v.sel = if down { (v.sel + 1).min(steps - 1) } else { v.sel.saturating_sub(1) };
                v.auto_select = false;
            }
            load_selected(app);
        }
        KeyCode::Enter => {
            let v = view_mut(app);
            v.outputs.entry(v.sel).or_default().scroll_to_end();
            load_selected(app);
        }
        KeyCode::Char('f') => {
            let v = view_mut(app);
            let sel = v.sel;
            let out = v.outputs.entry(sel).or_default();
            if out.follow {
                out.scroll_up(0);
                out.follow = false;
            } else {
                out.scroll_to_end();
            }
            v.auto_select = out.follow;
        }
        KeyCode::PageUp | KeyCode::Char('b') => scroll(app, |o| o.scroll_up(20)),
        KeyCode::PageDown | KeyCode::Char(' ') => scroll(app, |o| o.scroll_down(20)),
        KeyCode::Char('g') | KeyCode::Home => scroll(app, |o| o.scroll_up(usize::MAX / 2)),
        KeyCode::Char('G') | KeyCode::End => scroll(app, |o| o.scroll_to_end()),
        KeyCode::Left | KeyCode::Char('h') => scroll(app, |o| o.scroll_left(8)),
        KeyCode::Right | KeyCode::Char('l') => scroll(app, |o| o.scroll_right(8, 60)),
        KeyCode::Char('y') => {
            let cmd = view(app).detail.as_ref().and_then(|d| d.steps.get(view(app).sel)).map(|s| s.command.clone());
            if let Some(cmd) = cmd {
                copy_to_clipboard(app.tx.clone(), cmd, "command copied");
            }
        }
        KeyCode::Char('Y') => {
            let v = view(app);
            let text = v.outputs.get(&v.sel).map(|o| o.plain_text()).unwrap_or_default();
            copy_to_clipboard(app.tx.clone(), text, "output copied (colour codes stripped)");
        }
        KeyCode::Char('c') => {
            let v = view(app);
            if let Some(d) = &v.detail {
                if d.summary.status.is_active() {
                    let id = v.id;
                    app.overlay = Some(Overlay::Confirm(Confirm {
                        title: format!("cancel run #{id}"),
                        lines: vec![
                            Line::raw(format!("Cancel {}/{} #{id}?", d.summary.project, d.summary.pipeline)),
                            Line::styled(
                                "The running step gets SIGTERM, then SIGKILL after 10s. Remaining steps are skipped.",
                                theme::muted(),
                            ),
                        ],
                        action: ConfirmAction::CancelRun(id),
                    }));
                } else {
                    app.toast("the run has already finished", super::super::app::Level::Info);
                }
            }
        }
        KeyCode::Char('a') => {
            let v = view(app);
            let Some(a) = v.pending_approval().cloned() else {
                app.toast("nothing is waiting for approval", super::super::app::Level::Info);
                return;
            };
            if a.protected {
                let env = a.env.clone().unwrap_or_else(|| "-".into());
                app.overlay = Some(Overlay::Confirm(Confirm {
                    title: "confirm approval".into(),
                    lines: vec![
                        Line::from(vec![
                            Span::styled("project ", theme::muted()),
                            Span::styled(a.project.clone(), theme::bold()),
                            Span::styled("   env ", theme::muted()),
                            Span::styled(env, theme::warning()),
                            Span::styled(format!("   run #{}", a.run_id), theme::muted()),
                        ]),
                        Line::styled(format!("\"{}\"", a.message), theme::bold()),
                    ],
                    action: ConfirmAction::Approve { run: a.run_id, step: a.step_index },
                }));
            } else {
                app.decide(a.run_id, a.step_index, Decision::Approve, false);
            }
        }
        KeyCode::Char('x') => {
            if let Some(a) = view(app).pending_approval().cloned() {
                app.decide(a.run_id, a.step_index, Decision::Reject, false);
            } else {
                app.toast("nothing is waiting for approval", super::super::app::Level::Info);
            }
        }
        _ => {}
    }
}

/// A click on a step selects it; returns whether the row was a step.
pub fn click(app: &mut App, row: usize) -> bool {
    let steps = view(app).detail.as_ref().map(|d| d.steps.len()).unwrap_or(0);
    if row >= steps {
        return false;
    }
    let v = view_mut(app);
    v.sel = row;
    v.auto_select = false;
    load_selected(app);
    true
}

/// Scrolls the selected step's output (the mouse wheel).
pub fn scroll_output(app: &mut App, f: impl FnOnce(&mut TermView)) {
    scroll(app, f);
}

fn scroll(app: &mut App, f: impl FnOnce(&mut TermView)) {
    let v = view_mut(app);
    let sel = v.sel;
    f(v.outputs.entry(sel).or_default());
    v.auto_select = v.outputs.get(&sel).is_some_and(|o| o.follow) && v.auto_select;
}

fn load_selected(app: &mut App) {
    let reads = view_mut(app).wanted_reads();
    for (run, step) in reads {
        app.fetch_step_output(run, step);
    }
}

pub fn hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let mut h = Vec::new();
    let v = view(app);
    if v.pending_approval().is_some() {
        h.extend([("a", "approve"), ("x", "reject")]);
    }
    if v.detail.as_ref().is_some_and(|d| d.summary.status.is_active()) {
        h.push(("c", "cancel run"));
    }
    h.extend([("↑↓", "step"), ("enter", "output"), ("f", "follow"), ("y/Y", "copy")]);
    h
}

/// The outputs · inputs pane is at least this tall, an empty one included.
const IO_MIN_H: u16 = 3;

/// Rows of content in the inputs · outputs pane.
fn io_len(d: &RunDetail) -> usize {
    let both = !d.inputs.is_empty() && !d.outputs.is_empty();
    (d.inputs.len() + d.outputs.len() + usize::from(both)).max(1)
}

/// Height of the outputs · inputs pane in a left column `col_h` rows tall: its
/// content, but never so tall that the steps pane loses rows it needs.
fn io_height(d: &RunDetail, col_h: u16) -> u16 {
    let steps_need = (d.steps.len() as u16 + 5).min(col_h.saturating_sub(3));
    (io_len(d) as u16 + 2).min(col_h.saturating_sub(steps_need)).max(IO_MIN_H)
}

pub fn draw(app: &mut App, f: &mut Frame, area: Rect) {
    let View::Run(v) = app.nav.last_mut().unwrap() else { return };
    let Some(d) = v.detail.clone() else {
        f.render_widget(para(vec![Line::styled(format!(" loading run #{}…", v.id), theme::muted())]), area);
        return;
    };
    let approval = d.approvals.iter().find(|a| a.status == ApprovalStatus::Pending).cloned();
    let approval_h = if approval.is_some() { 5 } else { 0 };
    let [header, body, appr] = Layout::vertical([Constraint::Length(2), Constraint::Min(0), Constraint::Length(approval_h)]).areas(area);
    draw_header(&d, f, header);
    let [left, output] = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(body);
    let [steps_area, io_area] = Layout::vertical([Constraint::Min(0), Constraint::Length(io_height(&d, left.height))]).areas(left);
    draw_steps(v, &d, f, steps_area, &app.hits);
    draw_io(&d, f, io_area);
    app.hits.add(output, Target::RunOutput, 0);
    draw_output(v, &d, f, output);
    if let Some(a) = approval {
        draw_approval(&a, f, appr);
    }
}

fn draw_header(d: &RunDetail, f: &mut Frame, area: Rect) {
    let s = &d.summary;
    let badge_style = match s.status {
        RunStatus::AwaitingApproval => Style::new().fg(theme::ON_ACCENT).bg(theme::WARNING),
        RunStatus::Succeeded => Style::new().fg(theme::ON_ACCENT).bg(theme::SUCCESS),
        RunStatus::Failed => Style::new().fg(theme::ON_ACCENT).bg(theme::DANGER),
        RunStatus::Running => Style::new().fg(theme::ON_ACCENT).bg(theme::INFO),
        RunStatus::Rejected => Style::new().fg(theme::ON_ACCENT).bg(theme::WARNING),
        RunStatus::Cancelled => Style::new().fg(theme::ON_ACCENT).bg(theme::MUTED),
    }
    .add_modifier(Modifier::BOLD);
    let mut spans = vec![
        Span::styled(format!(" {} #{} ", s.pipeline, s.id), theme::bold()),
        Span::styled(format!(" {} ", run_status_long(s.status)), badge_style),
    ];
    if let Some(r) = &s.reason {
        spans.push(Span::styled(format!(" {r}"), theme::muted()));
    }
    if let Some(e) = &s.env {
        spans.push(Span::styled("   env ", theme::muted()));
        if s.protected {
            spans.push(Span::styled(format!("{} ", theme::PROTECTED), theme::warning()));
        }
        spans.push(Span::styled(e.clone(), theme::warning()));
    } else if s.protected {
        spans.push(Span::styled(format!("   {} protected", theme::PROTECTED), theme::warning()));
    }
    spans.push(Span::styled(
        format!("   {} {}", s.branch.clone().unwrap_or_else(|| "?".into()), s.commit.clone().unwrap_or_default()),
        theme::text(),
    ));
    let (tree, tree_style) = run_tree_word(s);
    let tree = if tree == "clean" { "clean, pushed".to_string() } else { tree };
    spans.push(Span::styled(format!("   {tree}"), if s.dirty || s.unpushed { tree_style } else { theme::success() }));
    let elapsed = run_duration(s);
    let timing = if s.status.is_active() {
        format!("   started {}, {} elapsed", fmt_when(s.started_at), fmt_duration(elapsed))
    } else {
        format!("   started {}, took {}", fmt_when(s.started_at), fmt_duration(elapsed))
    };
    spans.push(Span::styled(timing, theme::muted()));
    f.render_widget(para(vec![clip_line(spans, area.width as usize)]), area);
}

fn step_detail(s: &StepInfo) -> (String, Style) {
    let dur = step_duration(s).map(fmt_duration).unwrap_or_default();
    match s.status {
        StepStatus::Pending => ("not started".into(), theme::muted()),
        StepStatus::Running => (format!("running {dur}"), theme::info()),
        StepStatus::Waiting => (format!("waiting {dur}"), theme::warning()),
        StepStatus::Succeeded if s.kind == StepKindName::Approve => (format!("approved after {dur}"), theme::muted()),
        StepStatus::Succeeded => (format!("exit {}  {dur}", s.exit_code.unwrap_or(0)), theme::muted()),
        StepStatus::Failed => (
            match s.exit_code {
                Some(c) => format!("exit {c}  {dur}"),
                None => format!("failed  {dur}"),
            },
            theme::danger(),
        ),
        StepStatus::TimedOut => (format!("timed out  {dur}"), theme::danger()),
        StepStatus::Cancelled => ("cancelled".into(), theme::muted()),
        StepStatus::Rejected => ("rejected".into(), theme::warning()),
        StepStatus::Skipped => ("skipped".into(), theme::muted()),
    }
}

fn draw_steps(v: &RunView, d: &RunDetail, f: &mut Frame, area: Rect, hits: &HitMap) {
    let inner = framed(f, area, vec![title_text("steps")], true);
    hits.add(Rect { height: (d.steps.len() as u16).min(inner.height), ..inner }, Target::RunSteps, 0);
    let w = inner.width;
    let mut lines = Vec::new();
    for s in &d.steps {
        let sel = s.index == v.sel;
        let (sym, sym_style) = step_symbol(s.status);
        let (detail, detail_style) = step_detail(s);
        let name_style = if sel {
            theme::bold()
        } else if s.status == StepStatus::Pending || s.status == StepStatus::Skipped {
            theme::muted()
        } else {
            theme::text()
        };
        lines.push(selectable_row(
            vec![cell(sym, sym_style, 1), cell(s.name.clone(), name_style, 0), cell(detail, detail_style, 18)],
            w,
            sel,
            true,
        ));
    }
    lines.push(Line::raw(""));
    if let Some(s) = d.steps.get(v.sel) {
        let label = match s.kind {
            StepKindName::Run => "command  ",
            StepKindName::Approve => "message  ",
            StepKindName::Service => "service  ",
        };
        let first = s.command.lines().next().unwrap_or("").to_string();
        let more = if s.command.lines().count() > 1 { " …" } else { "" };
        lines.push(clip_line(vec![Span::styled(label, theme::muted()), Span::styled(format!("{first}{more}"), theme::info())], w as usize));
        if let Some(cwd) = &s.cwd {
            lines.push(clip_line(vec![Span::styled("cwd      ", theme::muted()), Span::styled(cwd.clone(), theme::text())], w as usize));
        }
    }
    f.render_widget(para(lines), inner);
}

fn draw_io(d: &RunDetail, f: &mut Frame, area: Rect) {
    if area.height < 3 {
        return;
    }
    let inner = framed(f, area, vec![title_text("outputs · inputs")], false);
    let w = inner.width as usize;
    let mut lines = Vec::new();
    for o in &d.outputs {
        let from = o.step_index.map(|i| format!(" from step {}  ", i + 1)).unwrap_or_else(|| "  ".into());
        lines.push(clip_line(
            vec![
                Span::styled(o.key.clone(), theme::text()),
                Span::styled(from, theme::muted()),
                Span::styled(o.value.clone(), theme::info()),
            ],
            w,
        ));
    }
    if !d.inputs.is_empty() && !d.outputs.is_empty() {
        lines.push(Line::raw(""));
    }
    for (k, val) in &d.inputs {
        lines.push(clip_line(vec![Span::styled(format!("input {k}  "), theme::muted()), Span::styled(val.clone(), theme::text())], w));
    }
    if lines.is_empty() {
        lines.push(Line::styled("no inputs or outputs yet", theme::muted()));
    }
    let rows = inner.height as usize;
    if lines.len() > rows {
        if rows <= 1 {
            lines.truncate(1);
        } else {
            let hidden = lines.len() - rows + 1;
            lines.truncate(rows - 1);
            lines.push(Line::styled(format!("… +{hidden} more"), theme::muted()));
        }
    }
    f.render_widget(para(lines), inner);
}

fn draw_output(v: &mut RunView, d: &RunDetail, f: &mut Frame, area: Rect) {
    let Some(step) = d.steps.get(v.sel).cloned() else {
        framed(f, area, vec![title_text("output")], false);
        return;
    };
    let out = v.outputs.entry(v.sel).or_default();
    let state = if step.status == StepStatus::Running {
        if out.follow { "live" } else { "paused" }
    } else if out.follow {
        "end"
    } else {
        "scrolled"
    };
    out.fit(area.height.saturating_sub(2));
    let mut title = vec![title_text("output"), title_muted(format!("  step {}", step.index + 1)), title_muted(format!("  {state}"))];
    if out.scrolled() > 0 {
        title.push(title_muted(format!("  ↑{}", out.scrolled())));
    }
    if out.hscroll > 0 {
        title.push(title_muted(format!("  →{}", out.hscroll)));
    }
    let inner = framed(f, area, title, false);
    if let Some(e) = v.log_errors.get(&step.index) {
        f.render_widget(para(vec![Line::styled(e.clone(), theme::danger())]), inner);
        return;
    }
    if out.is_empty() {
        let msg = match step.status {
            StepStatus::Pending => "not started",
            StepStatus::Skipped => "skipped",
            StepStatus::Waiting => "waiting for approval — the previous step's output is one step up",
            _ if step.kind == StepKindName::Approve => "approval step (no output)",
            _ if out.loading => "loading…",
            _ => "no output",
        };
        f.render_widget(para(vec![Line::styled(msg, theme::muted())]), inner);
        return;
    }
    out.render(inner, f.buffer_mut());
}

fn draw_approval(a: &ApprovalInfo, f: &mut Frame, area: Rect) {
    let inner = framed_alert(f, area, &format!("step {}  approval", a.step_index + 1), true);
    let w = inner.width as usize;
    let left = a.expires_at - now();
    let mut lines = Vec::new();
    for chunk in wrap(&a.message, w.saturating_sub(2)).into_iter().take(2) {
        lines.push(Line::styled(format!(" {chunk}"), theme::bold()));
    }
    let l = vec![
        Span::styled(format!(" No answer by {} cancels the run ({} left).  ", fmt_clock(a.expires_at), fmt_age(left)), theme::muted()),
        Span::styled("a", theme::key()),
        Span::styled(" approve  ", theme::text()),
        Span::styled("x", theme::key()),
        Span::styled(" reject", theme::text()),
    ];
    while lines.len() < 2 {
        lines.push(Line::raw(""));
    }
    lines.push(clip_line(l, w));
    f.render_widget(para(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(data: &str, complete: bool) -> LogChunk {
        LogChunk { run: 1, step: 0, data: data.into(), complete }
    }

    fn text(v: &RunView) -> String {
        v.outputs[&0].plain_text()
    }

    #[test]
    fn each_chunk_replaces_the_step_output() {
        let mut v = RunView::new(1);
        v.on_log(&chunk("ab\n", false));
        v.on_log(&chunk("ab\ncd\n", false));
        assert_eq!(text(&v), "ab\ncd\n");
        // A `\r` progress line rewritten in the pane comes back rewritten.
        v.on_log(&chunk("ab\ncd\n100%\n", true));
        assert_eq!(text(&v), "ab\ncd\n100%\n");
        assert!(v.outputs[&0].complete);
    }

    #[test]
    fn replacing_keeps_the_scroll_position() {
        let mut v = RunView::new(1);
        v.on_log(&chunk(&"line\n".repeat(100), false));
        v.outputs.get_mut(&0).unwrap().scroll_up(10);
        v.on_log(&chunk(&"line\n".repeat(120), false));
        let o = &v.outputs[&0];
        assert!(!o.follow);
        assert_eq!(o.scrolled(), 10);
    }
}
