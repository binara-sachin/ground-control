//! Shared drawing helpers: panes, fixed-width columns measured in display
//! cells, truncation with `…`, status words, and time formatting (§10.1).

use chrono::{Local, TimeZone};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::theme;
use crate::git::GitStatus;
use crate::model::*;

/// Display width in terminal cells.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncates to at most `w` cells, ending with `…` when cut.
pub fn truncate(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > w - 1 {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}

/// Truncates from the left (`…tail`), for paths.
pub fn truncate_left(s: &str, w: usize) -> String {
    if width(s) <= w || w == 0 {
        return if w == 0 { String::new() } else { s.to_string() };
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 1;
    for c in s.chars().rev() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > w {
            break;
        }
        tail.push(c);
        used += cw;
    }
    format!("…{}", tail.into_iter().rev().collect::<String>())
}

/// Truncates and pads to exactly `w` cells.
pub fn fit(s: &str, w: usize) -> String {
    let t = truncate(s, w);
    let pad = w.saturating_sub(width(&t));
    format!("{t}{}", " ".repeat(pad))
}

/// One table cell: text, style, and width (0 = share the remaining width).
pub struct Cell {
    pub text: String,
    pub style: Style,
    pub width: u16,
}

pub fn cell(text: impl Into<String>, style: Style, width: u16) -> Cell {
    Cell { text: text.into(), style, width }
}

/// Lays cells out in fixed columns separated by `gap` spaces. Text never
/// spills into the next column.
pub fn row(cells: Vec<Cell>, total: u16, gap: u16) -> Line<'static> {
    let fixed: u16 = cells.iter().map(|c| c.width).sum::<u16>() + gap * cells.len().saturating_sub(1) as u16;
    let flex_count = cells.iter().filter(|c| c.width == 0).count().max(1) as u16;
    let flex = total.saturating_sub(fixed) / flex_count;
    let n = cells.len();
    let mut spans = Vec::with_capacity(n * 2);
    let mut used: u16 = 0;
    for (i, c) in cells.into_iter().enumerate() {
        let mut w = if c.width == 0 { flex } else { c.width };
        w = w.min(total.saturating_sub(used));
        if w == 0 {
            break;
        }
        spans.push(Span::styled(fit(&c.text, w as usize), c.style));
        used += w;
        if i + 1 < n && used + gap <= total {
            spans.push(Span::raw(" ".repeat(gap as usize)));
            used += gap;
        }
    }
    Line::from(spans)
}

/// Concatenates spans, truncating the line to `w` cells.
pub fn clip_line(spans: Vec<Span<'static>>, w: usize) -> Line<'static> {
    let mut out = Vec::new();
    let mut used = 0;
    for s in spans {
        let sw = width(&s.content);
        if used + sw <= w {
            used += sw;
            out.push(s);
        } else {
            let rest = w.saturating_sub(used);
            if rest > 0 {
                out.push(Span::styled(truncate(&s.content, rest), s.style));
            }
            break;
        }
    }
    Line::from(out)
}

/// A bordered pane with its title set into the top border.
fn pane(title: Vec<Span<'static>>, focused: bool) -> Block<'static> {
    let border = if focused { theme::accent() } else { theme::dim() };
    let mut t = vec![Span::raw(" ")];
    for s in title {
        let style =
            if focused { s.style.patch(theme::accent()).add_modifier(Modifier::BOLD) } else { s.style.add_modifier(Modifier::BOLD) };
        t.push(Span::styled(s.content, style));
    }
    t.push(Span::raw(" "));
    Block::default().borders(Borders::ALL).border_type(BorderType::Plain).border_style(border).title(Line::from(t))
}

/// Draws a pane with its title clipped to the pane width; returns the inner area.
pub fn framed(f: &mut Frame, area: Rect, title: Vec<Span<'static>>, focused: bool) -> Rect {
    let max = (area.width as usize).saturating_sub(6);
    let block = pane(clip_line(title, max).spans, focused);
    let inner = block.inner(area);
    f.render_widget(block, area);
    inner
}

/// Like [`framed`] for approval banners and dialogs.
pub fn framed_alert(f: &mut Frame, area: Rect, title: &str, striped: bool) -> Rect {
    let max = (area.width as usize).saturating_sub(if striped { 16 } else { 6 });
    let block = alert_pane(&truncate(title, max), striped);
    let inner = block.inner(area);
    f.render_widget(block, area);
    inner
}

/// A double-bordered, striped pane for approvals and protected dialogs.
fn alert_pane(title: &str, striped: bool) -> Block<'static> {
    let mut t = vec![Span::raw(" ")];
    if striped {
        t.push(Span::styled(format!("{} ", theme::STRIPES), theme::accent()));
    }
    t.push(Span::styled(title.to_string(), theme::accent().add_modifier(Modifier::BOLD)));
    if striped {
        t.push(Span::styled(format!(" {}", theme::STRIPES), theme::accent()));
    }
    t.push(Span::raw(" "));
    Block::default().borders(Borders::ALL).border_type(BorderType::Double).border_style(theme::accent()).title(Line::from(t))
}

pub fn title_text(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), theme::text())
}

pub fn title_muted(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), theme::muted())
}

/// Centered rectangle for modal dialogs.
pub fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

pub fn clear(f: &mut Frame, r: Rect) {
    f.render_widget(Clear, r);
    f.render_widget(Block::default().style(theme::base()), r);
}

pub fn para(lines: Vec<Line<'static>>) -> Paragraph<'static> {
    Paragraph::new(lines).style(theme::base())
}

/// Key hints for the bottom bar: (key, label) pairs.
pub fn key_hints(hints: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for (k, l) in hints {
        out.push(Span::styled(format!(" {k} "), theme::key()));
        out.push(Span::styled(format!("{l}  "), theme::muted()));
    }
    out
}

// ── status words ──

pub fn run_status_word(s: RunStatus) -> (&'static str, Style) {
    match s {
        RunStatus::Running => ("RUN", theme::info()),
        RunStatus::AwaitingApproval => ("WAIT", theme::warning()),
        RunStatus::Succeeded => ("OK", theme::success()),
        RunStatus::Failed => ("FAIL", theme::danger()),
        RunStatus::Cancelled => ("CANC", theme::muted()),
        RunStatus::Rejected => ("REJ", theme::warning()),
    }
}

pub fn run_status_long(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Running => "RUNNING",
        RunStatus::AwaitingApproval => "AWAITING APPROVAL",
        RunStatus::Succeeded => "SUCCEEDED",
        RunStatus::Failed => "FAILED",
        RunStatus::Cancelled => "CANCELLED",
        RunStatus::Rejected => "REJECTED",
    }
}

pub fn step_symbol(s: StepStatus) -> (&'static str, Style) {
    match s {
        StepStatus::Pending => ("·", theme::muted()),
        StepStatus::Running => ("●", theme::info()),
        StepStatus::Waiting => ("◆", theme::warning()),
        StepStatus::Succeeded => ("✓", theme::success()),
        StepStatus::Failed | StepStatus::TimedOut => ("×", theme::danger()),
        StepStatus::Rejected => ("×", theme::warning()),
        StepStatus::Cancelled => ("⊘", theme::muted()),
        StepStatus::Skipped => ("-", theme::muted()),
    }
}

/// Symbol, word and style for a service state.
pub fn service_word(s: &ServiceStatus) -> (&'static str, String, Style) {
    match s.state {
        ServiceState::Running if s.healthy => ("●", if s.has_health_check { "healthy".into() } else { "running".into() }, theme::success()),
        ServiceState::Running => ("●", "running".into(), theme::success()),
        ServiceState::Unhealthy => ("◐", "unhealthy".into(), theme::warning()),
        ServiceState::Starting => ("◐", "starting".into(), theme::warning()),
        ServiceState::Restarting => ("◐", format!("restarting ({})", s.restart_count), theme::warning()),
        ServiceState::Stopping => ("◐", "stopping".into(), theme::warning()),
        ServiceState::Crashed => ("×", "crashed".into(), theme::danger()),
        ServiceState::Stopped => ("○", "stopped".into(), theme::muted()),
    }
}

/// Tree state words: "clean", "4M 1?" etc.
pub fn tree_word(g: &GitStatus) -> (String, Style) {
    if let Some(e) = &g.error {
        return (truncate(e, 30), theme::danger());
    }
    if g.is_clean() {
        return ("clean".into(), theme::success());
    }
    let mut parts = Vec::new();
    if g.staged + g.modified > 0 {
        parts.push(format!("{}M", g.staged + g.modified));
    }
    if g.conflicted > 0 {
        parts.push(format!("{}U", g.conflicted));
    }
    if g.untracked > 0 {
        parts.push(format!("{}?", g.untracked));
    }
    (parts.join(" "), theme::warning())
}

pub fn run_tree_word(r: &RunSummary) -> (String, Style) {
    match (r.dirty, r.unpushed) {
        (false, false) => ("clean".into(), theme::muted()),
        (true, false) => ("dirty".into(), theme::warning()),
        (false, true) => ("unpushed".into(), theme::warning()),
        (true, true) => ("dirty unpushed".into(), theme::warning()),
    }
}

// ── time ──

pub fn fmt_duration(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else if s < 86400 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d{:02}h", s / 86400, (s % 86400) / 3600)
    }
}

/// Short relative age: "2h", "5m".
pub fn fmt_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

pub fn fmt_clock(ts: Timestamp) -> String {
    Local.timestamp_opt(ts, 0).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default()
}

/// "14:31", "yest 18:05", "28 Oct 21:10".
pub fn fmt_when(ts: Timestamp) -> String {
    let Some(t) = Local.timestamp_opt(ts, 0).single() else { return String::new() };
    let today = Local::now().date_naive();
    let d = t.date_naive();
    if d == today {
        t.format("%H:%M").to_string()
    } else if Some(d) == today.pred_opt() {
        t.format("yest %H:%M").to_string()
    } else {
        t.format("%-d %b %H:%M").to_string()
    }
}

/// "today 14:31", "yesterday 18:05", "28 Oct 21:10".
pub fn fmt_when_long(ts: Timestamp) -> String {
    let s = fmt_when(ts);
    if let Some(rest) = s.strip_prefix("yest ") {
        format!("yesterday {rest}")
    } else if s.len() == 5 {
        format!("today {s}")
    } else {
        s
    }
}

pub fn run_duration(r: &RunSummary) -> i64 {
    r.finished_at.unwrap_or_else(now) - r.started_at
}

pub fn step_duration(s: &StepInfo) -> Option<i64> {
    Some(s.finished_at.unwrap_or_else(now) - s.started_at?)
}

/// Greedy word wrap by display width.
pub fn wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(10);
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && width(&cur) + 1 + width(word) > w {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_counts_display_cells() {
        assert_eq!(truncate("feat/dashboard-filters", 15), "feat/dashboard…");
        assert_eq!(width(&truncate("⛅ wrangler 4.15", 5)), 5);
        assert_eq!(truncate("⛅⛅⛅", 4), "⛅…");
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(truncate("x", 0), "");
        assert_eq!(truncate_left("/very/long/path/to/repo", 10), "…h/to/repo");
    }

    #[test]
    fn rows_never_overflow() {
        let line = row(vec![cell("project-name-too-long", theme::text(), 8), cell("⛅⛅⛅⛅⛅⛅", theme::text(), 0)], 20, 2);
        let w: usize = line.spans.iter().map(|s| width(&s.content)).sum();
        assert!(w <= 20, "{w}");
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(9), "9s");
        assert_eq!(fmt_duration(66), "1m06s");
        assert_eq!(fmt_duration(3720), "1h02m");
    }
}
