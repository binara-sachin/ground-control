//! Health (§6.6): tool check, with tmux and Docker rows that also say whether
//! they answered, config status with line and column numbers, last reload
//! time, version and config path, in full-width bands sized by their content.
//! Docker is only checked while this screen is open.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};

use super::super::app::App;
use super::super::theme;
use super::super::widgets::*;
use crate::config::Severity;
use crate::model::*;

pub fn hints() -> Vec<(&'static str, &'static str)> {
    vec![("r", "refresh"), ("1", "dashboard")]
}

/// The about band is one row of content; the config pane keeps `CONFIG_MIN_H`.
const ABOUT_H: u16 = 3;
const CONFIG_MIN_H: u16 = 5;

/// One row of the tool check: glyph, word (only when something is wrong),
/// and what adds information. tmux and Docker also report whether they
/// answered, so each is listed once.
fn tool_row(t: &ToolStatus, h: &HealthInfo) -> (&'static str, &'static str, ratatui::style::Style, String) {
    let reach = match t.name.as_str() {
        "tmux" => Some(&h.tmux),
        "docker" => Some(&h.docker),
        _ => None,
    };
    let path = t.path.clone().unwrap_or_default();
    if !t.ok {
        return ("×", "MISSING", theme::danger(), "not on PATH (steps inherit the tmux server's PATH)".into());
    }
    match reach {
        None => ("✓", "", theme::success(), path),
        Some(None) => ("·", "", theme::muted(), format!("checking… · {path}")),
        Some(Some(r)) if r.ok => ("✓", "", theme::success(), format!("{} · {path}", r.detail)),
        Some(Some(r)) => ("×", "FAIL", theme::danger(), format!("{} · {path}", r.detail)),
    }
}

pub fn draw(app: &App, f: &mut Frame, area: Rect) {
    let Some(h) = &app.health else {
        f.render_widget(para(vec![Line::styled(" loading…", theme::muted())]), area);
        return;
    };
    let tools_h = (h.tools.len().max(1) as u16 + 2).min(area.height.saturating_sub(ABOUT_H + CONFIG_MIN_H));
    let [about_area, tools_area, config_area] =
        Layout::vertical([Constraint::Length(ABOUT_H), Constraint::Length(tools_h), Constraint::Min(CONFIG_MIN_H)]).areas(area);

    // GroundControl itself.
    let inner = framed(f, about_area, vec![title_text("groundcontrol")], false);
    let line = clip_line(
        vec![
            Span::styled(fit("version", 9), theme::muted()),
            Span::styled(h.version.clone(), theme::text()),
            Span::styled("   opened ", theme::muted()),
            Span::styled(format!("{} ago (at {})", fmt_duration(now() - h.started_at), fmt_when_long(h.started_at)), theme::text()),
        ],
        inner.width as usize,
    );
    f.render_widget(para(vec![line]), inner);

    // Tools.
    let missing = h.tools.iter().filter(|t| !t.ok).count();
    let failing: Vec<&str> = [("tmux", &h.tmux), ("docker", &h.docker)]
        .into_iter()
        .filter(|(name, r)| r.as_ref().is_some_and(|r| !r.ok) && h.tools.iter().any(|t| t.name == *name && t.ok))
        .map(|(name, _)| name)
        .collect();
    let mut title = vec![title_text("tool check")];
    if missing > 0 {
        title.push(Span::styled(format!("  {missing} missing"), theme::danger()));
    }
    if !failing.is_empty() {
        title.push(Span::styled(format!("  {} not answering", failing.join(", ")), theme::danger()));
    }
    if let Some(t) = h.tools_checked_at {
        title.push(title_muted(format!("  checked {}", fmt_when_long(t))));
    }
    let inner = framed(f, tools_area, title, false);
    let w = inner.width;
    let mut lines = Vec::new();
    if h.tools.is_empty() {
        lines.push(Line::styled("checking…", theme::muted()));
    }
    for t in &h.tools {
        let (sym, word, style, detail) = tool_row(t, h);
        lines.push(row(
            vec![cell(sym, style, 1), cell(t.name.clone(), theme::text(), 8), cell(word, style, 7), cell(detail, theme::muted(), 0)],
            w,
            1,
        ));
    }
    f.render_widget(para(lines), inner);

    // Config.
    let c = &h.config;
    let (word, style) = if c.ok {
        ("OK", theme::success())
    } else if c.active {
        ("INVALID — previous config active", theme::danger())
    } else {
        ("INVALID — no config loaded", theme::danger())
    };
    let inner = framed(f, config_area, vec![title_text("config"), Span::styled(format!("  {word}"), style)], false);
    let w = inner.width as usize;
    let mut lines = vec![
        clip_line(vec![Span::styled(fit("file", 12), theme::muted()), Span::styled(c.path.clone(), theme::text())], w),
        clip_line(
            vec![
                Span::styled(fit("loaded", 12), theme::muted()),
                Span::styled(c.loaded_at.map(fmt_when_long).unwrap_or_else(|| "never".into()), theme::text()),
                Span::styled("   last reload attempt ", theme::muted()),
                Span::styled(c.last_attempt_at.map(fmt_when_long).unwrap_or_else(|| "-".into()), theme::text()),
            ],
            w,
        ),
        Line::raw(""),
    ];
    if c.diagnostics.is_empty() {
        lines.push(Line::styled("no errors or warnings", theme::success()));
    }
    let room = (inner.height as usize).saturating_sub(lines.len());
    let shown = if c.diagnostics.len() > room { room.saturating_sub(1) } else { c.diagnostics.len() };
    for d in &c.diagnostics[..shown] {
        let (sev, s) = match d.severity {
            Severity::Error => ("error  ", theme::danger()),
            Severity::Warning => ("warn   ", theme::warning()),
        };
        lines.push(clip_line(
            vec![
                Span::styled(format!("{:>4}:{:<3} ", d.line, d.col), theme::muted()),
                Span::styled(sev, s),
                Span::styled(d.message.clone(), theme::text()),
            ],
            w,
        ));
    }
    if shown < c.diagnostics.len() {
        lines.push(Line::styled(format!("… +{} more", c.diagnostics.len() - shown), theme::muted()));
    }
    f.render_widget(para(lines), inner);
}
