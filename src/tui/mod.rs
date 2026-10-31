//! The TUI (`groundcontrol` with no arguments): a client of the daemon.
//! Quitting it never stops a run or a service.

mod app;
mod screens;
mod term_view;
mod theme;
mod widgets;

use std::time::Duration;

use ratatui::Frame;
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;
use tokio::sync::mpsc;

use app::{App, Conn, Level, Msg, Overlay, View};
use screens::{dashboard, dialog, health, history, project, run_view};
use widgets::*;

/// Below this size the TUI shows a "terminal too small" message (§10.1).
pub const MIN_WIDTH: u16 = 100;
pub const MIN_HEIGHT: u16 = 28;

pub fn run() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    rt.block_on(main_loop())
}

async fn main_loop() -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
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
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

/// Copies text with `pbcopy` (the TUI runs on the Mac); falls back to common
/// Linux clipboard tools.
pub(crate) fn copy_to_clipboard(tx: mpsc::UnboundedSender<Msg>, text: String, ok_msg: &'static str) {
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let candidates: [(&str, &[&str]); 3] = [("pbcopy", &[]), ("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])];
        for (cmd, args) in candidates {
            let Ok(mut child) = tokio::process::Command::new(cmd)
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            else {
                continue;
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes()).await;
            }
            if child.wait().await.is_ok_and(|s| s.success()) {
                let _ = tx.send(Msg::Toast(ok_msg.into(), Level::Success));
                return;
            }
        }
        let _ = tx.send(Msg::Toast("no clipboard tool found (pbcopy)".into(), Level::Error));
    });
}

fn draw(app: &mut App, f: &mut Frame) {
    let area = f.area();
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
    let [top, gap, body, bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(area);
    let _ = gap;
    let body = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
    draw_top(app, f, top);
    match &app.conn {
        Conn::Connected => draw_view(app, f, body),
        Conn::Connecting => {
            f.render_widget(para(vec![Line::styled(" connecting to the daemon…", theme::muted())]), body);
        }
        Conn::NotRunning(e) => draw_disconnected(f, body, "GroundControl daemon is not running", e, "k", "start it (launchctl kickstart)"),
        Conn::Mismatch(e) => draw_disconnected(f, body, "Daemon version mismatch", e, "k", "restart the daemon (launchctl kickstart -k)"),
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
        View::History => history::draw(app, f, area),
        View::Health => health::draw(app, f, area),
    }
}

fn draw_disconnected(f: &mut Frame, area: Rect, title: &str, detail: &str, key: &str, action: &str) {
    let r = centered(area, 84, 9);
    clear(f, r);
    let inner = framed_alert(f, r, title, false);
    let w = inner.width as usize;
    let lines = vec![
        Line::raw(""),
        clip_line(vec![Span::styled(format!(" {detail}"), theme::text())], w),
        Line::styled(" The TUI never runs the daemon itself; it is a launchd user agent.", theme::muted()),
        Line::styled(" Retrying automatically.", theme::muted()),
        Line::raw(""),
        Line::from(vec![
            Span::styled(format!(" {key} "), theme::key()),
            Span::styled(action.to_string(), theme::text()),
            Span::styled("    r ", theme::key()),
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
        Some(View::History) => 2,
        Some(View::Health) => 3,
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
    for (n, name) in [(1, "dashboard"), (2, "history"), (3, "health")] {
        let mut label = format!(" {n} {name}");
        if n == root && !crumbs.is_empty() {
            label.push_str(&format!(" › {}", crumbs.join(" › ")));
        }
        if app.attention(n) {
            label.push_str(" !");
        }
        label.push(' ');
        left.push(Span::styled(label, if n == root { active } else { tab }));
    }
    let mut right = Vec::new();
    if let Some(h) = &app.health {
        right.push(Span::styled(format!("daemon up {}   ", fmt_age_long(crate::protocol::now() - h.started_at)), bar));
    } else if !matches!(app.conn, Conn::Connected) {
        right.push(Span::styled("daemon down   ", bar));
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

fn fmt_age_long(secs: i64) -> String {
    let s = secs.max(0);
    if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d{}h", s / 86400, (s % 86400) / 3600)
    }
}

/// A key and its label in the bottom bar.
type Hint = (&'static str, &'static str);

fn draw_bottom(app: &App, f: &mut Frame, area: Rect) {
    f.render_widget(Block::default().style(Style::new().bg(theme::BAR_BG)), area);
    let connected = matches!(app.conn, Conn::Connected);
    let (hints, right): (Vec<Hint>, Vec<Hint>) = if !connected {
        (vec![("k", "start daemon"), ("r", "retry")], vec![("q", "quit")])
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
                    View::History => history::hints(app),
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

fn draw_overlay(app: &mut App, f: &mut Frame, area: Rect) {
    match &mut app.overlay {
        None => {}
        Some(Overlay::Dialog(d)) => dialog::draw(d, f, area),
        Some(Overlay::Text(t)) => {
            clear(f, area);
            let mut title = vec![title_text(t.title.clone())];
            if !t.subtitle.is_empty() {
                title.push(title_muted(format!("  {}", t.subtitle)));
            }
            if t.view.scrolled() > 0 {
                title.push(title_muted(format!("  ↑{}", t.view.scrolled())));
            }
            let inner = framed(f, area, title, true);
            if t.view.is_empty() {
                let msg = if t.view.loading { "loading…" } else { "(empty)" };
                f.render_widget(para(vec![Line::styled(msg, theme::muted())]), inner);
            } else {
                t.view.render(inner, f.buffer_mut());
            }
        }
        Some(Overlay::Confirm(c)) => {
            let w = 90.min(area.width.saturating_sub(4));
            let h = c.lines.len() as u16 + 6;
            let r = centered(area, w, h);
            clear(f, r);
            let inner = framed_alert(f, r, &c.title, true);
            let iw = inner.width.saturating_sub(2) as usize;
            let mut lines = vec![Line::raw("")];
            lines.extend(c.lines.iter().map(|l| clip_line([vec![Span::raw(" ")], l.spans.clone()].concat(), iw)));
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled(" y ", theme::key()),
                Span::styled("confirm", theme::text()),
                Span::styled("    n / esc ", theme::key()),
                Span::styled("cancel", theme::text()),
            ]));
            f.render_widget(para(lines), inner);
        }
        Some(Overlay::Help) => draw_help(f, area),
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let sections: [(&str, &[(&str, &str)]); 6] = [
        (
            "global",
            &[
                ("1 2 3", "dashboard, history, health"),
                ("tab / shift+tab", "next / previous pane"),
                ("↑↓ / j k", "move selection"),
                ("enter", "open the selection"),
                ("esc", "back"),
                ("?", "this help"),
                ("q", "quit the TUI (runs and services keep going)"),
            ],
        ),
        (
            "dashboard",
            &[
                ("enter", "on the approval banner: review the run"),
                ("s", "start/stop the selected service or group"),
                ("r", "restart"),
                ("l", "full service log"),
            ],
        ),
        (
            "project",
            &[
                ("enter", "run the selected pipeline"),
                ("d", "dry run"),
                ("e", "choose environment"),
                ("s / r / l", "start/stop, restart, log (services pane)"),
            ],
        ),
        (
            "run dialog",
            &[
                ("tab", "next field"),
                ("←→ / space", "change environment, choice or bool"),
                ("enter", "start run"),
                ("ctrl+d", "dry run"),
                ("esc", "cancel"),
            ],
        ),
        (
            "run view",
            &[
                ("↑↓ / enter", "pick a step, show its output"),
                ("f", "follow output"),
                ("pgup/pgdn ←→", "scroll output"),
                ("y / Y", "copy command / output"),
                ("a / x", "approve (confirm if protected) / reject"),
                ("c", "cancel run"),
            ],
        ),
        (
            "history",
            &[
                ("/", "filter: project: pipeline: env: status:"),
                ("s", "cycle status filter"),
                ("ctrl+u", "clear filter"),
                ("enter", "open run"),
                ("D", "view saved diff"),
            ],
        ),
    ];
    let mut lines = Vec::new();
    for (name, keys) in sections {
        lines.push(Line::styled(name.to_string(), theme::accent().add_modifier(Modifier::BOLD)));
        for (k, d) in keys {
            lines
                .push(Line::from(vec![Span::styled(fit(&format!("  {k}"), 20), theme::key()), Span::styled(d.to_string(), theme::text())]));
        }
        lines.push(Line::raw(""));
    }
    let h = (lines.len() as u16 + 2).min(area.height);
    let r = centered(area, 76, h);
    clear(f, r);
    let inner = framed(f, r, vec![title_text("help")], true);
    f.render_widget(para(lines), inner);
}
