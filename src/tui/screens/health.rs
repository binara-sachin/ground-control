//! Health (§6.6): tool check, tmux and Docker reachability, config status
//! with line and column numbers, last reload time, version and state folder.
//! Docker is only checked while this screen is open.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};

use super::super::app::App;
use super::super::theme;
use super::super::widgets::*;
use crate::protocol::*;

pub fn hints() -> Vec<(&'static str, &'static str)> {
    vec![("r", "refresh"), ("1", "dashboard"), ("2", "history"), ("?", "help")]
}

pub fn draw(app: &App, f: &mut Frame, area: Rect) {
    let Some(h) = &app.health else {
        f.render_widget(para(vec![Line::styled(" loading…", theme::muted())]), area);
        return;
    };
    let [top, config_area] = Layout::vertical([Constraint::Length(13), Constraint::Min(5)]).areas(area);
    let [about_area, tools_area] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(top);

    // GroundControl and its environment.
    let inner = framed(f, about_area, vec![title_text("groundcontrol")], false);
    let w = inner.width as usize;
    let kv =
        |k: &str, v: String, s: ratatui::style::Style| clip_line(vec![Span::styled(fit(k, 12), theme::muted()), Span::styled(v, s)], w);
    let mut lines = vec![
        kv("version", h.version.clone(), theme::text()),
        kv("opened", format!("{} ago (at {})", fmt_duration(now() - h.started_at), fmt_when_long(h.started_at)), theme::text()),
        kv("state", h.state_dir.clone(), theme::text()),
        kv("config", h.config.path.clone(), theme::text()),
        kv("runs", "each run and service is a window in tmux session gc-<project>".into(), theme::muted()),
        Line::raw(""),
    ];
    for (name, r) in [("tmux", &h.tmux), ("docker", &h.docker)] {
        lines.push(match r {
            Some(r) if r.ok => clip_line(
                vec![
                    Span::styled(fit(name, 12), theme::muted()),
                    Span::styled("OK  ", theme::success()),
                    Span::styled(r.detail.clone(), theme::text()),
                ],
                w,
            ),
            Some(r) => clip_line(
                vec![
                    Span::styled(fit(name, 12), theme::muted()),
                    Span::styled("FAIL  ", theme::danger()),
                    Span::styled(r.detail.clone(), theme::muted()),
                ],
                w,
            ),
            None => kv(name, "checking…".into(), theme::muted()),
        });
    }
    f.render_widget(para(lines), inner);

    // Tools.
    let missing = h.tools.iter().filter(|t| !t.ok).count();
    let mut title = vec![title_text("tool check")];
    if missing > 0 {
        title.push(Span::styled(format!("  {missing} missing"), theme::danger()));
    }
    let inner = framed(f, tools_area, title, false);
    let w = inner.width;
    let mut lines = Vec::new();
    if h.tools.is_empty() {
        lines.push(Line::styled("checking…", theme::muted()));
    }
    for t in &h.tools {
        let (sym, word, style) = if t.ok { ("✓", "OK", theme::success()) } else { ("×", "MISSING", theme::danger()) };
        lines.push(row(
            vec![
                cell(sym, style, 1),
                cell(t.name.clone(), theme::text(), 8),
                cell(word, style, 7),
                cell(t.path.clone().unwrap_or_else(|| "not on PATH (steps inherit the tmux server's PATH)".into()), theme::muted(), 0),
            ],
            w,
            1,
        ));
    }
    if let Some(t) = h.tools_checked_at {
        lines.push(Line::styled(format!("checked {}", fmt_when_long(t)), theme::muted()));
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
    let inner = framed(f, config_area, vec![title_text("config"), Span::styled(format!("  {word}"), style)], !c.ok);
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
    for d in &c.diagnostics {
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
    f.render_widget(para(lines), inner);
}
