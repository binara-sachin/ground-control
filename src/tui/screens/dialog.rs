//! Run dialog (§10.3, §8.2): project, environment, branch and commit; dirty
//! and unpushed warnings; an input form with live pattern checking;
//! destructive warnings; and the step outline. For protected runs this is the
//! confirmation dialog.

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use regex::Regex;

use super::super::app::{App, Overlay, Tag};
use super::super::mouse::{HitMap, Target};
use super::super::theme;
use super::super::widgets::*;
use super::project::env_protected;
use crate::protocol::*;

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

pub struct RunDialog {
    pub project: String,
    pub pipeline: PipelineInfo,
    pub envs: Vec<String>,
    pub env_idx: usize,
    env_protected: Vec<bool>,
    branch: String,
    commit: String,
    warnings: Vec<Line<'static>>,
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
                    warnings.push(Line::from(vec![
                        Span::styled("! ", theme::warning()),
                        Span::styled(format!("The working tree has uncommitted changes ({t})."), theme::warning()),
                        Span::styled(" You can still run it.", theme::text()),
                    ]));
                    warnings.push(Line::from(vec![
                        Span::raw("  The run view shows it as "),
                        Span::styled("dirty", theme::warning()),
                        Span::raw("."),
                    ]));
                }
                if g.upstream.is_none() || g.ahead > 0 {
                    let what = if g.upstream.is_none() {
                        "HEAD has no upstream on origin.".to_string()
                    } else {
                        format!("HEAD is {} commit{} ahead of origin.", g.ahead, if g.ahead == 1 { "" } else { "s" })
                    };
                    warnings.push(Line::from(vec![
                        Span::styled("! ", theme::warning()),
                        Span::styled(what, theme::warning()),
                        Span::styled(" You can still run it.", theme::text()),
                    ]));
                    warnings.push(Line::from(vec![
                        Span::raw("  The run view shows it as "),
                        Span::styled("unpushed", theme::warning()),
                        Span::raw("."),
                    ]));
                }
                if g.origin_matches == Some(false) {
                    warnings.push(Line::styled(
                        "× origin does not match the configured remote; GroundControl will refuse this run.",
                        theme::danger(),
                    ));
                }
                (g.branch.clone().unwrap_or_else(|| "?".into()), g.commit.clone().unwrap_or_else(|| "?".into()))
            }
            None => ("?".into(), "?".into()),
        };
        if let Some(reason) = &p.disabled {
            warnings.push(Line::styled(format!("× project disabled: {reason}"), theme::danger()));
        }
        if let Some(id) = p.busy_run {
            warnings.push(Line::styled(format!("× run #{id} is in progress on this project; a second run is rejected."), theme::danger()));
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

    pub fn env(&self) -> Option<String> {
        self.envs.get(self.env_idx).cloned()
    }

    pub fn protected(&self) -> bool {
        self.pipeline.protected || self.env_protected.get(self.env_idx).copied().unwrap_or(false)
    }

    pub fn inputs_valid(&self) -> bool {
        self.fields.iter().all(|f| f.check().is_ok())
    }

    pub fn request(&self, dry_run: bool) -> Request {
        Request::RunStart(RunStartParams {
            project: self.project.clone(),
            pipeline: self.pipeline.id.clone(),
            env: self.env(),
            inputs: self.fields.iter().map(|f| (f.input.name.clone(), f.value.clone())).collect::<BTreeMap<_, _>>(),
            dry_run,
            confirmed: self.protected() && !dry_run,
        })
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
                app.request(d.request(true), Tag::DryRun);
            } else {
                d.error = Some("fix the inputs first".into());
            }
        }
        KeyCode::Enter => {
            if d.inputs_valid() {
                d.busy = true;
                d.error = None;
                app.request(d.request(false), Tag::Start);
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

pub fn hints(d: &RunDialog) -> Vec<(&'static str, &'static str)> {
    let mut h = vec![("tab", "next field")];
    if d.envs.len() > 1 {
        h.push(("←→", "environment"));
    }
    h.extend([("enter", "start run"), ("ctrl+d", "dry run"), ("esc", "cancel")]);
    h
}

pub fn draw(d: &RunDialog, f: &mut Frame, area: Rect, hits: &HitMap) {
    let w = 104.min(area.width.saturating_sub(4)).max(40);
    let inner_w = w.saturating_sub(4) as usize;
    let protected = d.protected();
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Line numbers of the input rows, for the mouse. The env row is line 1.
    let mut field_lines: Vec<(usize, usize)> = Vec::new();
    let label = |s: &str| Span::styled(fit(s, 9), theme::muted());
    lines.push(Line::from(vec![label("project"), Span::styled(d.project.clone(), theme::text())]));
    let mut env_line = vec![label("env")];
    match d.env() {
        Some(e) => {
            let focused = d.focus == 0 && d.envs.len() > 1;
            if d.envs.len() > 1 {
                env_line.push(Span::styled(if focused { "‹ " } else { "  " }, theme::key()));
            }
            env_line.push(Span::styled(e, if protected { theme::warning() } else { theme::text() }));
            if d.envs.len() > 1 {
                env_line.push(Span::styled(if focused { " ›" } else { "  " }, theme::key()));
            }
        }
        None => env_line.push(Span::styled("none", theme::muted())),
    }
    if protected {
        env_line.push(Span::styled(format!("   {} protected", theme::PROTECTED), theme::warning()));
    }
    lines.push(Line::from(env_line));
    lines.push(Line::from(vec![label("branch"), Span::styled(d.branch.clone(), theme::text())]));
    lines.push(Line::from(vec![label("commit"), Span::styled(d.commit.clone(), theme::text())]));
    if !d.warnings.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(d.warnings.iter().cloned());
    }
    if !d.fields.is_empty() {
        lines.push(Line::styled("╌".repeat(inner_w), theme::dim()));
        lines.push(Line::styled("inputs", theme::bold()));
        let name_w = d.fields.iter().map(|f| width(&f.input.name)).max().unwrap_or(4).clamp(6, 16);
        for (i, fld) in d.fields.iter().enumerate() {
            field_lines.push((i, lines.len()));
            let focused = d.focus == i + 1;
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
            lines.push(Line::from(spans));
            let (mark, msg, style) = match fld.check() {
                Ok(m) if m.is_empty() => ("", String::new(), theme::muted()),
                Ok(m) => ("✓ ", m, theme::success()),
                Err(m) => ("× ", m, theme::danger()),
            };
            if !msg.is_empty() || fld.input.kind == InputKind::Choice {
                let extra = if fld.input.kind == InputKind::Choice { format!("options: {}", fld.input.options.join(", ")) } else { msg };
                lines.push(clip_line(vec![Span::raw(" ".repeat(name_w + 2)), Span::styled(format!("{mark}{extra}"), style)], inner_w));
            }
        }
    }
    lines.push(Line::styled("╌".repeat(inner_w), theme::dim()));
    if let Some(warn) = &d.pipeline.warning {
        for (i, chunk) in wrap(warn, inner_w.saturating_sub(2)).into_iter().enumerate() {
            let prefix = if i == 0 { "× " } else { "  " };
            lines.push(Line::styled(format!("{prefix}{chunk}"), theme::danger()));
        }
        lines.push(Line::raw(""));
    }
    let mut steps = vec![Span::styled("steps  ", theme::muted())];
    for (i, s) in d.pipeline.steps.iter().enumerate() {
        let style = if s.kind == StepKindName::Approve { theme::warning() } else { theme::text() };
        steps.push(Span::styled(format!("{} ", i + 1), theme::muted()));
        steps.push(Span::styled(truncate(&s.label, 28), style));
        steps.push(Span::raw("   "));
    }
    lines.push(clip_line(steps, inner_w));
    let approvals: Vec<String> =
        d.pipeline.steps.iter().enumerate().filter(|(_, s)| s.kind == StepKindName::Approve).map(|(i, _)| (i + 1).to_string()).collect();
    if !approvals.is_empty() {
        lines.push(Line::styled(
            format!("The run stops at step {} for your approval before making changes.", approvals.join(", ")),
            theme::muted(),
        ));
    }
    lines.push(Line::raw(""));
    if let Some(e) = &d.error {
        for chunk in wrap(e, inner_w.saturating_sub(2)) {
            lines.push(Line::styled(format!("× {chunk}"), theme::danger()));
        }
    }
    let start_style = if d.inputs_valid() && !d.busy {
        ratatui::style::Style::new().fg(theme::ON_ACCENT).bg(theme::ACCENT).add_modifier(Modifier::BOLD)
    } else {
        theme::muted().bg(theme::BAR_BG)
    };
    // The button row: each of start, dry run and cancel is clickable.
    let buttons = [
        (Target::DialogStart, vec![Span::styled(if d.busy { " working… " } else { " enter  start run " }, start_style)]),
        (Target::DialogStart, vec![Span::raw("    ")]),
        (Target::DialogDry, vec![Span::styled("ctrl+d", theme::key()), Span::styled(" dry run", theme::text())]),
        (Target::DialogDry, vec![Span::styled("    ", theme::text())]),
        (Target::DialogCancel, vec![Span::styled("esc", theme::key()), Span::styled(" cancel", theme::text())]),
    ];
    let button_line = lines.len();
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
    lines.push(Line::from(button_spans));

    let h = (lines.len() as u16 + 4).min(area.height.saturating_sub(2));
    let r = centered(area, w, h);
    clear(f, r);
    let title = format!("run {}{}", d.pipeline.id, d.env().map(|e| format!(" on {e}")).unwrap_or_default());
    let inner = framed_alert(f, r, &title, protected);
    let inner = Rect { x: inner.x + 1, y: inner.y + 1, width: inner.width.saturating_sub(2), height: inner.height.saturating_sub(1) };
    let lines: Vec<Line<'static>> = lines.into_iter().map(|l| clip_line(l.spans, inner.width as usize)).collect();
    f.render_widget(para(lines), inner);

    // Lines are not wrapped, so line n is row n of the inner area.
    let row_at = |n: usize| Rect { y: inner.y.saturating_add(n as u16), height: 1, ..inner };
    let visible = |r: Rect| r.intersection(inner);
    if d.envs.len() > 1 {
        hits.add(visible(row_at(1)), Target::DialogEnv, 0);
    }
    for (i, n) in field_lines {
        hits.add(visible(row_at(n)), Target::DialogField(i), 0);
    }
    for (target, x, w) in button_rects {
        let r = Rect { x: inner.x.saturating_add(x), width: w, ..row_at(button_line) };
        hits.add(visible(r), target, 0);
    }
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
