//! Overlays drawn over a screen (§10.3): help, the text viewer and the confirm
//! dialog, with the run dialog routed through. Their types, key and click
//! handling, drawing, and the dry-run plan text.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::super::app::{App, Reply, View};
use super::super::clipboard::copy_to_clipboard;
use super::super::mouse::{HitMap, Target};
use super::super::term_view::TermView;
use super::super::theme;
use super::super::widgets::*;
use super::dialog;
use crate::model::*;

pub enum Overlay {
    Help,
    Dialog(Box<dialog::RunDialog>),
    Text(Box<TextOverlay>),
    Confirm(Confirm),
}

pub struct TextOverlay {
    pub title: String,
    pub subtitle: String,
    pub view: TermView,
    /// For output fed live: the service whose events extend this view.
    pub follow_service: Option<(String, String)>,
    /// The run dialog this view was opened from; closing the view returns to it.
    pub dialog: Option<Box<dialog::RunDialog>>,
}

pub struct Confirm {
    pub title: String,
    pub lines: Vec<ratatui::text::Line<'static>>,
    pub action: ConfirmAction,
}

pub enum ConfirmAction {
    CancelRun(i64),
    Approve { run: i64, step: usize },
}

/// Clicks in an overlay turn into the key that does the same.
pub fn click(app: &mut App, mut overlay: Overlay, target: Target) -> Option<Overlay> {
    let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
    let k = match (&mut overlay, target) {
        (Overlay::Help, Target::HelpPanel) => key(KeyCode::Esc),
        (Overlay::Confirm(_), Target::ConfirmYes) => key(KeyCode::Char('y')),
        (Overlay::Confirm(_), Target::ConfirmNo) => key(KeyCode::Char('n')),
        (Overlay::Dialog(d), t) => match dialog::click_key(d, t) {
            Some(k) => k,
            None => return Some(overlay),
        },
        _ => return Some(overlay),
    };
    let next = self::key(app, overlay, k);
    app.sync_watch();
    next
}

pub fn key(app: &mut App, overlay: Overlay, k: KeyEvent) -> Option<Overlay> {
    match overlay {
        Overlay::Help => match k.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => None,
            _ => Some(Overlay::Help),
        },
        Overlay::Dialog(d) => dialog::key(app, d, k),
        Overlay::Text(mut t) => {
            let page = 20;
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => return t.dialog.take().map(Overlay::Dialog),
                KeyCode::Up | KeyCode::Char('k') => t.view.scroll_up(1),
                KeyCode::Down | KeyCode::Char('j') => t.view.scroll_down(1),
                KeyCode::PageUp | KeyCode::Char('b') => t.view.scroll_up(page),
                KeyCode::PageDown | KeyCode::Char(' ') => t.view.scroll_down(page),
                KeyCode::Char('g') | KeyCode::Home => t.view.scroll_up(usize::MAX / 2),
                KeyCode::Char('G') | KeyCode::End | KeyCode::Char('f') => t.view.scroll_to_end(),
                KeyCode::Left | KeyCode::Char('h') => t.view.scroll_left(8),
                KeyCode::Right | KeyCode::Char('l') => t.view.scroll_right(8, 40),
                KeyCode::Char('Y') => {
                    let text = t.view.plain_text();
                    copy_to_clipboard(app.tx.clone(), text, "copied");
                }
                _ => {}
            }
            Some(Overlay::Text(t))
        }
        Overlay::Confirm(c) => match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                match c.action {
                    ConfirmAction::CancelRun(id) => app.call(move |e| async move { Reply::Cancelled(e.cancel_run(id).await) }),
                    ConfirmAction::Approve { run, step } => app.decide(run, step, Decision::Approve, true),
                }
                None
            }
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => None,
            _ => Some(Overlay::Confirm(c)),
        },
    }
}

pub fn draw_overlay(app: &mut App, f: &mut Frame, area: Rect) {
    let screen = HelpScreen::of(app.top());
    match &mut app.overlay {
        None => {}
        Some(Overlay::Dialog(d)) => dialog::draw(d, f, area, &app.hits),
        Some(Overlay::Text(t)) => {
            clear(f, area);
            t.view.fit(area.height.saturating_sub(2));
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
            clear_band(f, area, r);
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
        Some(Overlay::Help) => draw_help(f, area, &app.hits, screen),
    }
}

/// The screen help is opened over. Help only opens with no overlay showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpScreen {
    Dashboard,
    Project,
    Run,
    Health,
}

impl HelpScreen {
    pub fn of(view: &View) -> HelpScreen {
        match view {
            View::Dashboard => HelpScreen::Dashboard,
            View::Project(_) => HelpScreen::Project,
            View::Run(_) => HelpScreen::Run,
            View::Health => HelpScreen::Health,
        }
    }

    fn name(self) -> &'static str {
        match self {
            HelpScreen::Dashboard => "dashboard",
            HelpScreen::Project => "project",
            HelpScreen::Run => "run view",
            HelpScreen::Health => "health",
        }
    }

    /// Entries are (key, what it does); an empty key continues the line above.
    fn keys(self) -> &'static [(&'static str, &'static str)] {
        match self {
            HelpScreen::Dashboard => &[
                ("tab / shift+tab", "next / previous pane"),
                ("↑↓ / j k", "move the selection in the focused pane"),
                ("enter", "open the selection (approval banner: review the run)"),
                ("s", "services pane: start/stop the selected service or group"),
                ("r", "services pane: restart it"),
                ("l", "services pane: show its full output"),
                ("↑ ↓ column", "commits ahead/behind as of the last fetch (never fetched)"),
            ],
            HelpScreen::Project => &[
                ("tab / shift+tab", "switch between pipelines and services"),
                ("↑↓ / j k", "move the selection in the focused pane"),
                ("e", "next environment of the selected pipeline"),
                ("enter", "pipelines: open the run dialog"),
                ("d", "pipelines: dry run; asks for missing inputs first"),
                ("enter / l", "services: open the service's output"),
                ("s", "services: start/stop the selected service or group"),
                ("r", "services: restart it"),
            ],
            HelpScreen::Run => &[
                ("↑↓ / j k", "select a step and show its output"),
                ("enter", "show the step's output, jumping to its end"),
                ("f", "follow the output / pause it"),
                ("pgup / pgdn", "scroll the output by page (b and space also)"),
                ("g / G", "jump to the top / bottom of the output"),
                ("← → / h l", "pan the output sideways"),
                ("y / Y", "copy the step's command / its output"),
                ("a / x", "approve (asks first if protected) / reject"),
                ("c", "cancel the run (asks first)"),
            ],
            HelpScreen::Health => &[
                ("r", "re-check tools, tmux, Docker and config now"),
                ("", "it also re-checks every 5 s while this screen is open"),
                ("", "Docker's answer is reused for up to 30 s"),
            ],
        }
    }
}

const EVERYWHERE: &[(&str, &str)] = &[
    ("1 2", "dashboard, health"),
    ("esc", "go back; also closes help, viewers, confirms and dialogs"),
    ("?", "this help"),
    ("q", "quit; closes help, output viewers and confirms instead"),
    ("ctrl+c", "quit from anywhere; runs and services keep going"),
    ("click", "select a row, focus a pane; tabs switch screens"),
    ("double-click", "open the row, like enter (the banner opens on one click)"),
    ("wheel", "move the selection in lists; scroll output (sideways pans)"),
];

/// The help text for a screen: its own keys, then the ones that work everywhere.
pub fn help_lines(screen: HelpScreen) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, (name, keys)) in [(screen.name(), screen.keys()), ("everywhere", EVERYWHERE)].into_iter().enumerate() {
        if i > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(name.to_string(), theme::accent().add_modifier(Modifier::BOLD)));
        for (k, d) in keys {
            lines
                .push(Line::from(vec![Span::styled(fit(&format!("  {k}"), 20), theme::key()), Span::styled(d.to_string(), theme::text())]));
        }
    }
    lines
}

fn draw_help(f: &mut Frame, area: Rect, hits: &HitMap, screen: HelpScreen) {
    let lines = help_lines(screen);
    let h = (lines.len() as u16 + 2).min(area.height);
    let r = centered(area, 84, h);
    clear_band(f, area, r);
    hits.add(r, Target::HelpPanel, 0);
    let inner = framed(f, r, vec![title_text("help"), title_muted(format!("  {}", screen.name()))], true);
    f.render_widget(para(lines), inner);
}

/// Renders a dry-run plan as coloured text for the viewer (§8.6).
pub fn format_plan(plan: &DryRunPlan) -> String {
    let mut s = String::new();
    s.push_str(&format!("\x1b[1m{}/{}\x1b[0m", plan.project, plan.pipeline));
    if let Some(e) = &plan.env {
        s.push_str(&format!(" on \x1b[33m{e}\x1b[0m"));
    }
    if plan.protected {
        s.push_str(" \x1b[33m(protected)\x1b[0m");
    }
    s.push_str("\n\x1b[90mDry run: nothing was executed. Inputs reach commands only as environment variables.\x1b[0m\n\n");
    if !plan.inputs.is_empty() {
        s.push_str("\x1b[1minputs\x1b[0m\n");
        for (k, v) in &plan.inputs {
            s.push_str(&format!("  INPUT_{} = {v}\n", k.to_ascii_uppercase()));
        }
        s.push('\n');
    }
    if !plan.blockers.is_empty() {
        s.push_str("\x1b[31;1ma real run would be refused:\x1b[0m\n");
        for b in &plan.blockers {
            s.push_str(&format!("\x1b[31m  × {b}\x1b[0m\n"));
        }
        s.push('\n');
    }
    for st in &plan.steps {
        let kind = match st.kind {
            StepKindName::Approve => "\x1b[33mapprove\x1b[0m",
            StepKindName::Service => "\x1b[34mservice\x1b[0m",
            StepKindName::Run => "\x1b[32mrun\x1b[0m",
        };
        s.push_str(&format!("\x1b[1m{}. {}\x1b[0m  [{kind}]\n", st.index + 1, st.label));
        match st.kind {
            StepKindName::Run => {
                for (i, l) in st.command.lines().enumerate() {
                    s.push_str(&format!("   {} \x1b[34m{l}\x1b[0m\n", if i == 0 { "$" } else { " " }));
                }
                s.push_str(&format!("   \x1b[90mcwd\x1b[0m      {}\n", st.cwd));
                if let Some(t) = st.timeout_secs {
                    s.push_str(&format!("   \x1b[90mtimeout\x1b[0m  {}\n", fmt_duration(t as i64)));
                }
                s.push_str(&format!("   \x1b[90menv\x1b[0m      {}\n", st.env_names.join(" ")));
            }
            StepKindName::Approve => {
                s.push_str(&format!("   \x1b[33m{}\x1b[0m\n", st.command));
                s.push_str("   \x1b[90mexpires after 30m without an answer\x1b[0m\n");
            }
            StepKindName::Service => s.push_str(&format!("   {}\n", st.command)),
        }
        s.push('\n');
    }
    s
}
