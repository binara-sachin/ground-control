//! The TUI (`groundcontrol` with no arguments). Runs and services live in
//! tmux windows, so quitting it never stops a run or a service.

mod app;
mod mouse;
mod screens;
mod term_view;
mod theme;
mod widgets;

use std::time::Duration;

use ratatui::Frame;
use ratatui::crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;
use tokio::sync::mpsc;

use app::{App, Conn, Level, Msg, Overlay, View};
use mouse::Target;
use screens::{dashboard, dialog, health, project, run_view};
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
    let [top, gap, body, bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(area);
    let _ = gap;
    let body = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
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

fn draw_overlay(app: &mut App, f: &mut Frame, area: Rect) {
    match &mut app.overlay {
        None => {}
        Some(Overlay::Dialog(d)) => dialog::draw(d, f, area, &app.hits),
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
            app.hits.add(inner, Target::TextOverlay, 0);
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
            // The button row is the last line: " y confirm" then "n / esc cancel".
            let row = Rect { y: inner.y + c.lines.len() as u16 + 2, height: 1, ..inner };
            app.hits.add(Rect { width: 10, ..row }.intersection(inner), Target::ConfirmYes, 0);
            app.hits.add(Rect { x: inner.x + 10, width: 18, ..row }.intersection(inner), Target::ConfirmNo, 0);
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
        Some(Overlay::Help) => draw_help(f, area, &app.hits),
    }
}

fn draw_help(f: &mut Frame, area: Rect, hits: &mouse::HitMap) {
    let sections: [(&str, &[(&str, &str)]); 5] = [
        (
            "global",
            &[
                ("1 2", "dashboard, health"),
                ("tab / shift+tab", "next / previous pane"),
                ("↑↓ / j k", "move selection"),
                ("enter", "open the selection"),
                ("click / wheel", "focus and select / scroll or move"),
                ("double-click", "open the selection"),
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
    hits.add(r, Target::HelpPanel, 0);
    let inner = framed(f, r, vec![title_text("help")], true);
    f.render_widget(para(lines), inner);
}

#[cfg(test)]
mod mouse_tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    use super::*;
    use crate::protocol::ProjectInfo;

    fn project(id: &str) -> ProjectInfo {
        ProjectInfo {
            id: id.into(),
            path: format!("/tmp/{id}"),
            disabled: None,
            busy_run: None,
            git: None,
            environments: vec![],
            services: vec![],
            groups: vec![],
            pipelines: vec![],
        }
    }

    /// A connected app with three projects, drawn once at 120×40.
    fn app() -> (App, Terminal<TestBackend>) {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx);
        app.conn = Conn::Connected;
        app.projects = ["alpha", "beta", "gamma"].map(project).into();
        app.projects_loaded = true;
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| draw(&mut app, f)).unwrap();
        (app, term)
    }

    /// The screen cell of a hit target's row, found by scanning the hit map.
    fn cell_of(app: &App, target: Target, row: usize, overlay: bool) -> (u16, u16) {
        for y in 0..40 {
            for x in 0..120 {
                if app.hits.at(x, y, overlay) == Some((target, row)) {
                    return (x, y);
                }
            }
        }
        panic!("no cell for {target:?} row {row}");
    }

    fn mouse(app: &mut App, kind: MouseEventKind, (column, row): (u16, u16)) {
        app.update(Msg::Input(Event::Mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE })));
    }

    fn click(app: &mut App, at: (u16, u16)) {
        mouse(app, MouseEventKind::Down(MouseButton::Left), at);
    }

    fn click_on(app: &mut App, target: Target, row: usize, overlay: bool) {
        let at = cell_of(app, target, row, overlay);
        click(app, at);
    }

    fn redraw(app: &mut App, term: &mut Terminal<TestBackend>) {
        term.draw(|f| draw(app, f)).unwrap();
    }

    #[test]
    fn click_selects_a_project_and_focuses_its_pane() {
        let (mut app, _t) = app();
        app.dash.focus = dashboard::Focus::Runs;
        click_on(&mut app, Target::DashProjects, 2, false);
        assert_eq!(app.dash.project_sel, 2);
        assert_eq!(app.dash.focus, dashboard::Focus::Projects);
        assert_eq!(app.nav.len(), 1, "one click does not open the project");
    }

    #[test]
    fn double_click_opens_the_project() {
        let (mut app, _t) = app();
        let at = cell_of(&app, Target::DashProjects, 1, false);
        click(&mut app, at);
        click(&mut app, at);
        assert_eq!(app.nav.len(), 2);
        assert!(matches!(app.top(), View::Project(p) if p.id == "beta"));
    }

    #[test]
    fn two_clicks_on_different_rows_are_not_a_double_click() {
        let (mut app, _t) = app();
        click_on(&mut app, Target::DashProjects, 0, false);
        click_on(&mut app, Target::DashProjects, 1, false);
        assert_eq!(app.nav.len(), 1);
        assert_eq!(app.dash.project_sel, 1);
    }

    #[test]
    fn a_click_below_the_last_row_only_focuses() {
        let (mut app, _t) = app();
        app.dash.focus = dashboard::Focus::Runs;
        let at = cell_of(&app, Target::DashProjects, 3, false);
        click(&mut app, at);
        click(&mut app, at);
        assert_eq!(app.dash.focus, dashboard::Focus::Projects);
        assert_eq!(app.dash.project_sel, 0);
        assert_eq!(app.nav.len(), 1, "no project there to open");
    }

    #[test]
    fn the_wheel_moves_the_selection() {
        let (mut app, _t) = app();
        let at = cell_of(&app, Target::DashProjects, 0, false);
        mouse(&mut app, MouseEventKind::ScrollDown, at);
        mouse(&mut app, MouseEventKind::ScrollDown, at);
        assert_eq!(app.dash.project_sel, 2);
        mouse(&mut app, MouseEventKind::ScrollUp, at);
        assert_eq!(app.dash.project_sel, 1);
    }

    #[test]
    fn top_bar_tabs_switch_screens() {
        let (mut app, mut t) = app();
        click_on(&mut app, Target::Tab(2), 0, false);
        assert!(matches!(app.top(), View::Health));
        redraw(&mut app, &mut t);
        click_on(&mut app, Target::Tab(1), 0, false);
        assert!(matches!(app.top(), View::Dashboard));
    }

    #[test]
    fn an_open_overlay_takes_the_clicks() {
        let (mut app, mut t) = app();
        app.overlay = Some(Overlay::Help);
        redraw(&mut app, &mut t);
        // The projects underneath cannot be clicked.
        click_on(&mut app, Target::Tab(2), 0, false);
        assert!(matches!(app.top(), View::Dashboard), "tabs are behind the overlay");
        assert!(app.overlay.is_some());
        // A click on the help panel closes it.
        click_on(&mut app, Target::HelpPanel, 0, true);
        assert!(app.overlay.is_none());
    }

    #[test]
    fn confirm_buttons_answer_the_dialog() {
        let (mut app, mut t) = app();
        let confirm =
            || app::Confirm { title: "cancel run #1".into(), lines: vec![Line::raw("sure?")], action: app::ConfirmAction::CancelRun(1) };
        app.overlay = Some(Overlay::Confirm(confirm()));
        redraw(&mut app, &mut t);
        click_on(&mut app, Target::ConfirmNo, 0, true);
        assert!(app.overlay.is_none(), "n closes the confirm");
    }

    #[test]
    fn the_text_overlay_scrolls_with_the_wheel() {
        let (mut app, mut t) = app();
        let mut view = term_view::TermView::new();
        view.feed(&"line\r\n".repeat(200));
        app.overlay =
            Some(Overlay::Text(Box::new(app::TextOverlay { title: "log".into(), subtitle: String::new(), view, follow_service: None })));
        redraw(&mut app, &mut t);
        let at = cell_of(&app, Target::TextOverlay, 0, true);
        mouse(&mut app, MouseEventKind::ScrollUp, at);
        let Some(Overlay::Text(o)) = &app.overlay else { panic!() };
        assert_eq!(o.view.scrolled(), 3);
    }

    #[test]
    fn mouse_is_ignored_until_connected() {
        let (mut app, _t) = app();
        let at = cell_of(&app, Target::DashProjects, 2, false);
        app.conn = Conn::Connecting;
        click(&mut app, at);
        assert_eq!(app.dash.project_sel, 0);
    }
}
