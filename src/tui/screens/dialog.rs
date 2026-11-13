//! Run dialog (§10.3, §8.2), titled `run <project>/<pipeline>`: environment,
//! branch and commit rows; dirty and unpushed warnings; an input form with
//! live pattern checking; destructive warnings; and the step outline. For
//! protected runs this is the confirmation dialog.
//!
//! The error row and the button row are pinned to the bottom. When the dialog
//! does not fit, the blank row above them goes first, then the outline, then
//! the warnings, and a `… +N more` row says what was cut; then the inputs
//! scroll around the focused one, and only after that does the destructive
//! warning give way.

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use regex::Regex;

use super::super::app::{App, Reply};
use super::super::mouse::{HitMap, Target};
use super::super::theme;
use super::super::widgets::*;
use super::overlay::Overlay;
use super::project::env_protected;
use crate::config::InputKind;
use crate::model::*;

pub struct Field {
    pub input: InputInfo,
    pub value: String,
    re: Option<Result<Regex, String>>,
}

impl Field {
    fn check(&self) -> Result<String, String> {
        match self.input.kind {
            InputKind::Bool => Ok(String::new()),
            InputKind::Choice => Ok(String::new()),
            InputKind::String => {
                let pat = self.input.pattern.clone().unwrap_or_default();
                if self.value.is_empty() {
                    return Err(format!("required · must match {pat}"));
                }
                match &self.re {
                    Some(Ok(re)) if re.is_match(&self.value) => Ok(format!("matches {pat}")),
                    Some(Ok(_)) => Err(format!("must match {pat}")),
                    Some(Err(e)) => Err(e.clone()),
                    None => Ok(String::new()),
                }
            }
        }
    }
}

/// A line in the dialog's warning block. A note is wrapped to the dialog's
/// width when drawn; a plain line is clipped.
enum Warning {
    Plain(Line<'static>),
    Note { what: String, state: &'static str },
}

impl Warning {
    fn lines(&self, w: usize) -> Vec<Line<'static>> {
        let (what, state) = match self {
            Warning::Plain(l) => return vec![l.clone()],
            Warning::Note { what, state } => (what, state),
        };
        let segments = [
            (what.clone(), theme::warning()),
            (" You can still run it; the run view shows it as ".to_string(), theme::text()),
            (state.to_string(), theme::warning()),
            (".".to_string(), theme::text()),
        ];
        let styles: Vec<ratatui::style::Style> = segments.iter().flat_map(|(t, s)| t.chars().map(move |_| *s)).collect();
        let text: String = segments.iter().map(|(t, _)| t.as_str()).collect();
        let mut pos = 0;
        wrap(&text, w.saturating_sub(2))
            .into_iter()
            .enumerate()
            .map(|(i, chunk)| {
                let n = chunk.chars().count();
                let mut spans = vec![if i == 0 { Span::styled("! ", theme::warning()) } else { Span::raw("  ") }];
                let mut run = String::new();
                let mut run_style = styles[pos];
                for (c, st) in chunk.chars().zip(&styles[pos..pos + n]) {
                    if *st != run_style {
                        spans.push(Span::styled(std::mem::take(&mut run), run_style));
                        run_style = *st;
                    }
                    run.push(c);
                }
                spans.push(Span::styled(run, run_style));
                pos += n + 1;
                Line::from(spans)
            })
            .collect()
    }
}

pub struct RunDialog {
    pub project: String,
    pub pipeline: PipelineInfo,
    pub envs: Vec<String>,
    pub env_idx: usize,
    env_protected: Vec<bool>,
    branch: String,
    commit: String,
    warnings: Vec<Warning>,
    pub fields: Vec<Field>,
    /// 0 = environment selector (when there is a choice), then the fields.
    pub focus: usize,
    pub busy: bool,
    pub error: Option<String>,
}

impl RunDialog {
    pub fn new(p: &ProjectInfo, pl: &PipelineInfo, env: Option<String>) -> RunDialog {
        let envs = pl.environments.clone();
        let env_idx = env.and_then(|e| envs.iter().position(|x| *x == e)).unwrap_or(0);
        let env_protected = envs.iter().map(|e| env_protected(p, Some(e))).collect();
        let fields = pl
            .inputs
            .iter()
            .map(|i| Field {
                input: i.clone(),
                value: i.default.clone().unwrap_or_else(|| match i.kind {
                    InputKind::Bool => "false".into(),
                    InputKind::Choice => i.options.first().cloned().unwrap_or_default(),
                    InputKind::String => String::new(),
                }),
                re: i.pattern.as_ref().map(|pat| Regex::new(pat).map_err(|e| format!("invalid pattern: {e}"))),
            })
            .collect();
        let mut warnings = Vec::new();
        let (branch, commit) = match &p.git {
            Some(g) => {
                if !g.is_clean() {
                    let (t, _) = tree_word(g);
                    warnings.push(Warning::Note { what: format!("The working tree has uncommitted changes ({t})."), state: "dirty" });
                }
                if g.upstream.is_none() || g.ahead > 0 {
                    let what = if g.upstream.is_none() {
                        "HEAD has no upstream on origin.".to_string()
                    } else {
                        format!("HEAD is {} commit{} ahead of origin.", g.ahead, if g.ahead == 1 { "" } else { "s" })
                    };
                    warnings.push(Warning::Note { what, state: "unpushed" });
                }
                if g.origin_matches == Some(false) {
                    warnings.push(Warning::Plain(Line::styled(
                        "× origin does not match the configured remote; GroundControl will refuse this run.",
                        theme::danger(),
                    )));
                }
                (g.branch.clone().unwrap_or_else(|| "?".into()), g.commit.clone().unwrap_or_else(|| "?".into()))
            }
            None => ("?".into(), "?".into()),
        };
        if let Some(reason) = &p.disabled {
            warnings.push(Warning::Plain(Line::styled(format!("× project disabled: {reason}"), theme::danger())));
        }
        if let Some(id) = p.busy_run {
            warnings.push(Warning::Plain(Line::styled(
                format!("× run #{id} is in progress on this project; a second run is rejected."),
                theme::danger(),
            )));
        }
        let focus = if envs.len() > 1 { 0 } else { 1 };
        RunDialog {
            project: p.id.clone(),
            pipeline: pl.clone(),
            envs,
            env_idx,
            env_protected,
            branch,
            commit,
            warnings,
            fields,
            focus,
            busy: false,
            error: None,
        }
    }

    fn env(&self) -> Option<String> {
        self.envs.get(self.env_idx).cloned()
    }

    fn protected(&self) -> bool {
        self.pipeline.protected || self.env_protected.get(self.env_idx).copied().unwrap_or(false)
    }

    pub fn inputs_valid(&self) -> bool {
        self.fields.iter().all(|f| f.check().is_ok())
    }

    pub fn start_params(&self, dry_run: bool) -> RunStartParams {
        RunStartParams {
            project: self.project.clone(),
            pipeline: self.pipeline.id.clone(),
            env: self.env(),
            inputs: self.fields.iter().map(|f| (f.input.name.clone(), f.value.clone())).collect::<BTreeMap<_, _>>(),
            dry_run,
            confirmed: self.protected() && !dry_run,
        }
    }

    fn first_focus(&self) -> usize {
        if self.envs.len() > 1 { 0 } else { 1 }
    }

    fn last_focus(&self) -> usize {
        self.fields.len().max(1)
    }

    fn field_mut(&mut self) -> Option<&mut Field> {
        if self.focus == 0 { None } else { self.fields.get_mut(self.focus - 1) }
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(f) = self.field_mut()
            && f.input.kind == InputKind::String
        {
            f.value.push_str(text.trim());
        }
    }
}

pub fn key(app: &mut App, mut d: Box<RunDialog>, k: KeyEvent) -> Option<Overlay> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if d.busy && k.code != KeyCode::Esc {
        return Some(Overlay::Dialog(d));
    }
    match k.code {
        KeyCode::Esc => return None,
        KeyCode::Char('d') if ctrl => {
            if d.inputs_valid() {
                d.busy = true;
                d.error = None;
                let params = d.start_params(true);
                app.call(move |e| async move { Reply::DryRun(e.start_run(params).await) });
            } else {
                d.error = Some("fix the inputs first".into());
            }
        }
        KeyCode::Enter => {
            if d.inputs_valid() {
                d.busy = true;
                d.error = None;
                let params = d.start_params(false);
                app.call(move |e| async move { Reply::Started(e.start_run(params).await) });
            } else {
                d.error = Some("fix the inputs first".into());
            }
        }
        KeyCode::Tab | KeyCode::Down => {
            d.focus = if d.focus >= d.last_focus() { d.first_focus() } else { d.focus + 1 };
        }
        KeyCode::BackTab | KeyCode::Up => {
            d.focus = if d.focus <= d.first_focus() { d.last_focus() } else { d.focus - 1 };
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if d.focus == 0 && d.envs.len() > 1 => {
            let n = d.envs.len();
            d.env_idx = if k.code == KeyCode::Left { (d.env_idx + n - 1) % n } else { (d.env_idx + 1) % n };
        }
        KeyCode::Char('e') if ctrl && d.envs.len() > 1 => {
            d.env_idx = (d.env_idx + 1) % d.envs.len();
        }
        _ => {
            if let Some(f) = d.field_mut() {
                match f.input.kind {
                    InputKind::String => match k.code {
                        KeyCode::Char('u') if ctrl => f.value.clear(),
                        KeyCode::Char(c) if !ctrl => f.value.push(c),
                        KeyCode::Backspace => {
                            f.value.pop();
                        }
                        _ => {}
                    },
                    InputKind::Bool => {
                        if matches!(k.code, KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right) {
                            f.value = if f.value == "true" { "false".into() } else { "true".into() };
                        }
                    }
                    InputKind::Choice => {
                        let n = f.input.options.len().max(1);
                        let i = f.input.options.iter().position(|o| *o == f.value).unwrap_or(0);
                        let next = match k.code {
                            KeyCode::Left => (i + n - 1) % n,
                            KeyCode::Right | KeyCode::Char(' ') => (i + 1) % n,
                            _ => i,
                        };
                        f.value = f.input.options.get(next).cloned().unwrap_or_default();
                    }
                }
            }
        }
    }
    Some(Overlay::Dialog(d))
}

/// What a click in the dialog does, as the key that does the same. Clicking
/// an input focuses it; a bool or choice also flips to its next value.
pub fn click_key(d: &mut RunDialog, target: Target) -> Option<KeyEvent> {
    let key = |c, m| Some(KeyEvent::new(c, m));
    if d.busy && target != Target::DialogCancel {
        return None;
    }
    match target {
        Target::DialogStart => key(KeyCode::Enter, KeyModifiers::NONE),
        Target::DialogDry => key(KeyCode::Char('d'), KeyModifiers::CONTROL),
        Target::DialogCancel => key(KeyCode::Esc, KeyModifiers::NONE),
        Target::DialogEnv if d.envs.len() > 1 => {
            d.focus = 0;
            key(KeyCode::Right, KeyModifiers::NONE)
        }
        Target::DialogField(i) => {
            let kind = d.fields.get(i)?.input.kind;
            d.focus = i + 1;
            match kind {
                InputKind::String => None,
                InputKind::Bool | InputKind::Choice => key(KeyCode::Right, KeyModifiers::NONE),
            }
        }
        _ => None,
    }
}

/// Bottom-bar keys for the focused row, from what `key` does with it: the
/// keys that change it, how focus moves, and the keys that always work.
pub fn hints(d: &RunDialog) -> Vec<(&'static str, &'static str)> {
    let mut h = Vec::new();
    let multi_env = d.envs.len() > 1;
    if d.focus == 0 && multi_env {
        h.push(("←→/space", "environment"));
    } else if let Some(f) = d.fields.get(d.focus.wrapping_sub(1)) {
        match f.input.kind {
            InputKind::String => h.push(("ctrl+u", "clear")),
            InputKind::Bool => h.push(("space/←→", "toggle")),
            InputKind::Choice => h.push(("←→/space", "change")),
        }
    }
    h.push(("↑↓/tab", "field"));
    if multi_env && d.focus != 0 {
        h.push(("ctrl+e", "environment"));
    }
    h.extend([("enter", "start run"), ("ctrl+d", "dry run"), ("esc", "cancel")]);
    h
}

/// The dialog's rows, with where the mouse targets are among them.
struct Built {
    lines: Vec<Line<'static>>,
    /// (field index, line) of each visible input row.
    field_lines: Vec<(usize, usize)>,
    button_line: usize,
    button_rects: Vec<(Target, u16, u16)>,
}

/// Rows of the dialog's middle part that are still shown. Dropped in this
/// order: the spacer, the outline, the warnings last to first; then the inputs
/// scroll; the destructive warning goes only when that is not enough.
#[derive(Clone, Copy)]
struct Keep {
    spacer: bool,
    outline: bool,
    warnings: usize,
    destructive: bool,
}

impl RunDialog {
    /// Lays the dialog out in at most `budget` rows where it can.
    fn build(&self, inner_w: usize, budget: usize) -> Built {
        let protected = self.protected();
        let label = |s: &str| Span::styled(fit(s, 9), theme::muted());
        let mut head: Vec<Line<'static>> = Vec::new();
        let mut env_line = vec![label("env")];
        match self.env() {
            Some(e) => {
                // The value stays in the column of branch and commit. When the
                // selector is focused, `‹` takes the last two cells of the label.
                let focused = self.focus == 0 && self.envs.len() > 1;
                if focused {
                    env_line = vec![Span::styled(fit("env", 7), theme::muted()), Span::styled("‹ ", theme::key())];
                }
                env_line.push(Span::styled(e, if protected { theme::warning() } else { theme::text() }));
                if focused {
                    env_line.push(Span::styled(" ›", theme::key()));
                }
            }
            None => env_line.push(Span::styled("none", theme::muted())),
        }
        if protected {
            env_line.push(Span::styled(format!("   {} protected", theme::PROTECTED), theme::warning()));
        }
        head.push(Line::from(env_line));
        head.push(Line::from(vec![label("branch"), Span::styled(self.branch.clone(), theme::text())]));
        head.push(Line::from(vec![label("commit"), Span::styled(self.commit.clone(), theme::text())]));

        let warnings: Vec<Vec<Line<'static>>> = self.warnings.iter().map(|w| w.lines(inner_w)).collect();
        let mut destructive: Vec<Line<'static>> = Vec::new();
        if let Some(warn) = &self.pipeline.warning {
            for (i, chunk) in wrap(warn, inner_w.saturating_sub(2)).into_iter().enumerate() {
                let prefix = if i == 0 { "× " } else { "  " };
                destructive.push(Line::styled(format!("{prefix}{chunk}"), theme::danger()));
            }
        }

        // One unit per input: its row, then its check or option line.
        let name_w = self.fields.iter().map(|f| width(&f.input.name)).max().unwrap_or(4).clamp(6, 16);
        let units: Vec<Vec<Line<'static>>> = self
            .fields
            .iter()
            .enumerate()
            .map(|(i, fld)| {
                let focused = self.focus == i + 1;
                let mut spans = vec![Span::styled(fit(&fld.input.name, name_w + 2), if focused { theme::key() } else { theme::muted() })];
                let shown = match fld.input.kind {
                    InputKind::String => format!(" {}{} ", fld.value, if focused { "▏" } else { "" }),
                    InputKind::Bool => format!(" [{}] {} ", if fld.value == "true" { "x" } else { " " }, fld.value),
                    InputKind::Choice => format!(" ‹ {} › ", fld.value),
                };
                let field_w = inner_w.saturating_sub(name_w + 2).clamp(12, 64);
                let field_style = if focused { theme::text().bg(theme::SELECTION) } else { theme::text().bg(theme::BAR_BG) };
                // Long values show their end, where typing happens.
                let shown_fit = if width(&shown) > field_w {
                    let chars: Vec<char> = shown.chars().collect();
                    let tail: String = chars[chars.len().saturating_sub(field_w - 1)..].iter().collect();
                    format!("…{tail}")
                } else {
                    fit(&shown, field_w)
                };
                spans.push(Span::styled(shown_fit, field_style));
                let mut unit = vec![Line::from(spans)];
                let (mark, msg, style) = match fld.check() {
                    Ok(m) if m.is_empty() => ("", String::new(), theme::muted()),
                    Ok(m) => ("✓ ", m, theme::success()),
                    Err(m) => ("× ", m, theme::danger()),
                };
                if !msg.is_empty() || fld.input.kind == InputKind::Choice {
                    let extra =
                        if fld.input.kind == InputKind::Choice { format!("options: {}", fld.input.options.join(", ")) } else { msg };
                    unit.push(clip_line(vec![Span::raw(" ".repeat(name_w + 2)), Span::styled(format!("{mark}{extra}"), style)], inner_w));
                }
                unit
            })
            .collect();

        let mut steps = vec![Span::styled("steps  ", theme::muted())];
        for (i, s) in self.pipeline.steps.iter().enumerate() {
            let style = if s.kind == StepKindName::Approve { theme::warning() } else { theme::text() };
            steps.push(Span::styled(format!("{} ", i + 1), theme::muted()));
            steps.push(Span::styled(truncate(&s.label, 28), style));
            steps.push(Span::raw("   "));
        }
        let mut outline = vec![clip_line(steps, inner_w)];
        let approvals: Vec<String> = self
            .pipeline
            .steps
            .iter()
            .enumerate()
            .filter(|(_, s)| s.kind == StepKindName::Approve)
            .map(|(i, _)| (i + 1).to_string())
            .collect();
        if !approvals.is_empty() {
            outline.push(Line::styled(
                format!("The run stops at step {} for your approval before making changes.", approvals.join(", ")),
                theme::muted(),
            ));
        }

        let error_chunks: Vec<String> = self.error.as_ref().map(|e| wrap(e, inner_w.saturating_sub(2))).unwrap_or_default();
        let start_style = if self.inputs_valid() && !self.busy {
            ratatui::style::Style::new().fg(theme::ON_ACCENT).bg(theme::ACCENT).add_modifier(Modifier::BOLD)
        } else {
            theme::muted().bg(theme::BAR_BG)
        };
        // The button row: each of start, dry run and cancel is clickable.
        let buttons = [
            (Target::DialogStart, vec![Span::styled(if self.busy { " working… " } else { " enter  start run " }, start_style)]),
            (Target::DialogStart, vec![Span::raw("    ")]),
            (Target::DialogDry, vec![Span::styled("ctrl+d", theme::key()), Span::styled(" dry run", theme::text())]),
            (Target::DialogDry, vec![Span::styled("    ", theme::text())]),
            (Target::DialogCancel, vec![Span::styled("esc", theme::key()), Span::styled(" cancel", theme::text())]),
        ];
        let mut button_spans = Vec::new();
        let mut button_rects: Vec<(Target, u16, u16)> = Vec::new();
        let mut x = 0u16;
        for (i, (target, spans)) in buttons.into_iter().enumerate() {
            let w: u16 = spans.iter().map(|s| width(&s.content) as u16).sum();
            // The gaps between buttons (odd entries) are not clickable.
            if i % 2 == 0 {
                button_rects.push((target, x, w));
            }
            x += w;
            button_spans.extend(spans);
        }
        // The error, cut to `err_rows` rows (ending in `…`), then the buttons.
        let footer = |err_rows: usize| -> Vec<Line<'static>> {
            let cut = error_chunks.len() > err_rows;
            let mut footer: Vec<Line<'static>> = Vec::new();
            for (i, chunk) in error_chunks.iter().take(err_rows).enumerate() {
                let chunk =
                    if cut && i + 1 == err_rows { truncate(&format!("{chunk} …"), inner_w.saturating_sub(2)) } else { chunk.clone() };
                footer.push(Line::styled(format!("× {chunk}"), theme::danger()));
            }
            footer.push(Line::from(button_spans.clone()));
            footer
        };

        let sep = || Line::styled("╌".repeat(inner_w), theme::dim());
        let assemble = |keep: Keep, window: Option<(usize, usize)>, err_rows: usize| -> Built {
            let mut lines = head.clone();
            let mut field_lines = Vec::new();
            if keep.warnings > 0 {
                lines.push(Line::raw(""));
                lines.extend(warnings[..keep.warnings].iter().flatten().cloned());
            }
            if !units.is_empty() {
                let (a, b) = window.unwrap_or((0, units.len()));
                lines.push(sep());
                lines.push(Line::styled("inputs", theme::bold()));
                if a > 0 {
                    lines.push(Line::styled(format!("… {a} more above"), theme::muted()));
                }
                for (i, unit) in units.iter().enumerate().take(b).skip(a) {
                    field_lines.push((i, lines.len()));
                    lines.extend(unit.iter().cloned());
                }
                if b < units.len() {
                    lines.push(Line::styled(format!("… {} more below", units.len() - b), theme::muted()));
                }
            }
            if (keep.destructive && !destructive.is_empty()) || keep.outline {
                lines.push(sep());
            }
            if keep.destructive {
                lines.extend(destructive.iter().cloned());
                if keep.outline && !destructive.is_empty() {
                    lines.push(Line::raw(""));
                }
            }
            if keep.outline {
                lines.extend(outline.iter().cloned());
            }
            let mut hidden = Vec::new();
            let mut hidden_lines = 0;
            if !keep.outline {
                hidden.push("steps");
                hidden_lines += outline.len();
            }
            if keep.warnings < warnings.len() {
                hidden.push("warnings");
                hidden_lines += warnings[keep.warnings..].iter().map(Vec::len).sum::<usize>();
            }
            if !keep.destructive && !destructive.is_empty() {
                hidden.push("destructive warning");
                hidden_lines += destructive.len();
            }
            if !hidden.is_empty() {
                lines.push(Line::styled(format!("… +{hidden_lines} more ({})", hidden.join(", ")), theme::muted()));
            }
            if keep.spacer {
                lines.push(Line::raw(""));
            }
            lines.extend(footer(err_rows));
            let button_line = lines.len() - 1;
            Built { lines, field_lines, button_line, button_rects: button_rects.clone() }
        };

        // The error takes the rows it needs. The rest gives way first, in the
        // order above; only when that is not enough is the error cut, a row at
        // a time, so the buttons always stay.
        let layout = |err_rows: usize| -> Result<Built, Built> {
            let mut keep = Keep { spacer: true, outline: true, warnings: warnings.len(), destructive: true };
            let fits = |b: &Built| b.lines.len() <= budget;
            loop {
                let built = assemble(keep, None, err_rows);
                if fits(&built) {
                    return Ok(built);
                }
                if keep.spacer {
                    keep.spacer = false;
                } else if keep.outline {
                    keep.outline = false;
                } else if keep.warnings > 0 {
                    keep.warnings -= 1;
                } else {
                    break;
                }
            }
            // Still too tall: scroll the inputs around the focused one, keeping the
            // destructive warning for as long as anything else can give.
            let n = units.len();
            let focus = self.focus.saturating_sub(1).min(n.saturating_sub(1));
            for destructive in [true, false] {
                keep.destructive = destructive;
                let full = assemble(keep, None, err_rows);
                if fits(&full) {
                    return Ok(full);
                }
                for k in (1..n).rev() {
                    let a = focus.saturating_sub(k / 2).min(n - k);
                    let built = assemble(keep, Some((a, a + k)), err_rows);
                    if fits(&built) {
                        return Ok(built);
                    }
                }
            }
            Err(assemble(keep, (n > 1).then(|| (focus, focus + 1)), err_rows))
        };
        let mut err_rows = error_chunks.len();
        loop {
            match layout(err_rows) {
                Ok(built) => return built,
                Err(built) if err_rows <= 1 => return built,
                Err(_) => err_rows -= 1,
            }
        }
    }
}

pub fn draw(d: &RunDialog, f: &mut Frame, area: Rect, hits: &HitMap) {
    let w = 104.min(area.width.saturating_sub(4)).max(40);
    let inner_w = w.saturating_sub(4) as usize;
    let protected = d.protected();
    // The frame takes 2 rows, the top padding 1 and a blank row stays below.
    let max_h = area.height.saturating_sub(2);
    let built = d.build(inner_w, (max_h as usize).saturating_sub(4));
    let Built { lines, field_lines, button_line, button_rects } = built;

    let h = (lines.len() as u16 + 4).min(max_h);
    let r = centered(area, w, h);
    clear_band(f, area, r);
    let title = format!("run {}/{}{}", d.project, d.pipeline.id, d.env().map(|e| format!(" on {e}")).unwrap_or_default());
    let inner = framed_alert(f, r, &title, protected);
    let inner = Rect { x: inner.x + 1, y: inner.y + 1, width: inner.width.saturating_sub(2), height: inner.height.saturating_sub(1) };
    let lines: Vec<Line<'static>> = lines.into_iter().map(|l| clip_line(l.spans, inner.width as usize)).collect();
    f.render_widget(para(lines), inner);

    // Lines are not wrapped, so line n is row n of the inner area.
    let row_at = |n: usize| Rect { y: inner.y.saturating_add(n as u16), height: 1, ..inner };
    let visible = |r: Rect| r.intersection(inner);
    if d.envs.len() > 1 {
        hits.add(visible(row_at(0)), Target::DialogEnv, 0);
    }
    for (i, n) in field_lines {
        hits.add(visible(row_at(n)), Target::DialogField(i), 0);
    }
    for (target, x, w) in button_rects {
        let r = Rect { x: inner.x.saturating_add(x), width: w, ..row_at(button_line) };
        hits.add(visible(r), target, 0);
    }
}
