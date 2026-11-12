//! The TUI (`groundcontrol` with no arguments). Runs and services live in
//! tmux windows, so quitting it never stops a run or a service.

mod app;
mod clipboard;
mod mouse;
mod screens;
mod term_view;
mod theme;
mod widgets;

#[cfg(test)]
mod tests;

use std::time::Duration;

use ratatui::Frame;
use ratatui::crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;
use tokio::sync::mpsc;

use app::{App, Conn, Level, Msg, View};
use mouse::Target;
use screens::overlay::{Overlay, draw_overlay};
use screens::{dashboard, dialog, health, project, run_view};
use widgets::*;

/// Below this size the TUI shows a "terminal too small" message (§10.1).
const MIN_WIDTH: u16 = 100;
const MIN_HEIGHT: u16 = 28;

pub fn run() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    rt.block_on(main_loop())
}

async fn main_loop() -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture);
    // A panic must not leave the terminal reporting mouse events.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        prev_hook(info);
    }));
    let (tx, mut rx) = mpsc::unbounded_channel();

    let input_tx = tx.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = ratatui::crossterm::event::read() {
            if input_tx.send(Msg::Input(ev)).is_err() {
                break;
            }
        }
    });
    let tick_tx = tx.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_millis(250));
        loop {
            t.tick().await;
            if tick_tx.send(Msg::Tick).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(tx);
    app.start();
    let result = loop {
        if let Err(e) = terminal.draw(|f| draw(&mut app, f)) {
            break Err(e.into());
        }
        let Some(msg) = rx.recv().await else { break Ok(()) };
        app.update(msg);
        // Apply everything already queued before drawing again.
        while let Ok(msg) = rx.try_recv() {
            app.update(msg);
            if app.quit {
                break;
            }
        }
        if app.quit {
            break Ok(());
        }
    };
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}

fn draw(app: &mut App, f: &mut Frame) {
    let area = f.area();
    app.hits.clear();
    f.render_widget(Block::default().style(theme::base()), area);
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let msg = vec![
            Line::styled("terminal too small", theme::warning().add_modifier(Modifier::BOLD)),
            Line::styled(format!("{}×{} — GroundControl needs at least {MIN_WIDTH}×{MIN_HEIGHT}", area.width, area.height), theme::muted()),
            Line::styled("q quit · runs and services keep going", theme::muted()),
        ];
        let r = centered(area, area.width, 3);
        f.render_widget(para(msg).alignment(ratatui::layout::Alignment::Center), r);
        return;
    }
    let [top, body, bottom] = Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(area);
    draw_top(app, f, top);
    match &app.conn {
        Conn::Connected => draw_view(app, f, body),
        Conn::Connecting => {
            f.render_widget(para(vec![Line::styled(" starting…", theme::muted())]), body);
        }
        Conn::Failed(e) => draw_failed(f, body, e),
    }
    if matches!(app.conn, Conn::Connected) {
        draw_overlay(app, f, body);
    }
    draw_bottom(app, f, bottom);
}

fn draw_view(app: &mut App, f: &mut Frame, area: Rect) {
    match app.top() {
        View::Dashboard => dashboard::draw(app, f, area),
        View::Project(_) => project::draw(app, f, area),
        View::Run(_) => run_view::draw(app, f, area),
        View::Health => health::draw(app, f, area),
    }
}

fn draw_failed(f: &mut Frame, area: Rect, detail: &str) {
    let r = centered(area, 84, 8);
    clear(f, r);
    let inner = framed_alert(f, r, "GroundControl could not start", false);
    let w = inner.width as usize;
    let lines = vec![
        Line::raw(""),
        clip_line(vec![Span::styled(format!(" {detail}"), theme::text())], w),
        Line::styled(" Retrying automatically.", theme::muted()),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" r ", theme::key()),
            Span::styled("retry now", theme::text()),
            Span::styled("    q ", theme::key()),
            Span::styled("quit", theme::text()),
        ]),
    ];
    f.render_widget(para(lines), inner);
}

fn draw_top(app: &App, f: &mut Frame, area: Rect) {
    let bar = Style::new().fg(theme::ON_ACCENT).bg(theme::ACCENT);
    let active = Style::new().fg(theme::ACCENT).bg(theme::BG).add_modifier(Modifier::BOLD);
    let tab = bar.add_modifier(Modifier::BOLD);
    f.render_widget(Block::default().style(bar), area);
    let root = match app.nav.first() {
        Some(View::Health) => 2,
        _ => 1,
    };
    let crumbs: Vec<String> = app
        .nav
        .iter()
        .skip(1)
        .map(|v| match v {
            View::Project(p) => p.id.clone(),
            View::Run(r) => format!("run {}", r.id),
            _ => String::new(),
        })
        .collect();
    let mut left = vec![Span::styled(" groundcontrol ", tab)];
    let mut tab_x = area.x + width(" groundcontrol ") as u16;
    for (n, name) in [(1, "dashboard"), (2, "health")] {
        let mut label = format!(" {n} {name}");
        if n == root && !crumbs.is_empty() {
            label.push_str(&format!(" › {}", crumbs.join(" › ")));
        }
        if app.attention(n) {
            label.push_str(" !");
        }
        label.push(' ');
        let label_w = width(&label) as u16;
        app.hits.add(Rect { x: tab_x, y: area.y, width: label_w, height: 1 }, Target::Tab(n as u8), 0);
        tab_x += label_w;
        left.push(Span::styled(label, if n == root { active } else { tab }));
    }
    let mut right = Vec::new();
    if !matches!(app.conn, Conn::Connected) {
        right.push(Span::styled("not started   ", bar));
    }
    if let Some(c) = &app.config {
        let t = c.loaded_at.map(fmt_clock).unwrap_or_default();
        if c.ok {
            right.push(Span::styled(format!("config ok {t}   "), bar));
        } else {
            right.push(Span::styled("config ERROR   ", bar.add_modifier(Modifier::BOLD)));
        }
    }
    right.push(Span::styled(format!("{} ", chrono::Local::now().format("%a %-d %b %H:%M")), bar));
    let rw: usize = right.iter().map(|s| width(&s.content)).sum();
    let lw = (area.width as usize).saturating_sub(rw + 1);
    let mut spans = clip_line(left, lw).spans;
    let used: usize = spans.iter().map(|s| width(&s.content)).sum();
    if used + rw <= area.width as usize {
        spans.push(Span::styled(" ".repeat(area.width as usize - used - rw), bar));
        spans.extend(right);
    }
    f.render_widget(ratatui::widgets::Paragraph::new(Line::from(spans)), area);
}

/// A key and its label in the bottom bar.
type Hint = (&'static str, &'static str);

fn draw_bottom(app: &App, f: &mut Frame, area: Rect) {
    f.render_widget(Block::default().style(Style::new().bg(theme::BAR_BG)), area);
    let connected = matches!(app.conn, Conn::Connected);
    let (hints, right): (Vec<Hint>, Vec<Hint>) = if !connected {
        (vec![("r", "retry")], vec![("q", "quit")])
    } else {
        match &app.overlay {
            Some(Overlay::Dialog(d)) => (dialog::hints(d), vec![]),
            Some(Overlay::Text(_)) => {
                (vec![("↑↓", "scroll"), ("pgup/pgdn", "page"), ("g/G", "top/bottom"), ("←→", "pan"), ("Y", "copy")], vec![("esc", "close")])
            }
            Some(Overlay::Confirm(_)) => (vec![("y", "confirm"), ("n", "cancel")], vec![]),
            Some(Overlay::Help) => (vec![("esc", "close help")], vec![]),
            None => {
                let hints = match app.top() {
                    View::Dashboard => dashboard::hints(app),
                    View::Project(_) => project::hints(app),
                    View::Run(_) => run_view::hints(app),
                    View::Health => health::hints(),
                };
                let right = match app.top() {
                    View::Run(_) => vec![("esc", "back · run keeps going")],
                    _ if app.nav.len() > 1 => vec![("esc", "back")],
                    _ => vec![("q", "quit · services keep running")],
                };
                (hints, right)
            }
        }
    };
    let mut right_spans = key_hints(&right);
    if let Some((msg, level, _)) = &app.toast {
        let style = match level {
            Level::Info => theme::text(),
            Level::Success => theme::success(),
            Level::Error => theme::danger(),
        };
        right_spans = vec![Span::styled(format!("{msg}  "), style.add_modifier(Modifier::BOLD))];
    }
    let rw: usize = right_spans.iter().map(|s| width(&s.content)).sum::<usize>().min(area.width as usize * 2 / 3);
    let left = clip_line(key_hints(&hints), (area.width as usize).saturating_sub(rw + 1));
    let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(rw as u16)]).areas(area);
    f.render_widget(ratatui::widgets::Paragraph::new(left), l);
    f.render_widget(ratatui::widgets::Paragraph::new(clip_line(right_spans, rw)), r);
}
