//! TUI state and message handling. The TUI holds no state of its own beyond
//! the screen: everything comes from the engine's replies and events, which
//! it reads from what the tmux-hosted workers record.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use tokio::sync::mpsc::UnboundedSender;

use super::mouse::{HitMap, Target};
use super::screens::overlay::{self, Overlay, TextOverlay};
use super::screens::{dashboard, project, run_view};
use super::term_view::TermView;
use crate::engine::Engine;
use crate::model::*;

pub enum Msg {
    Input(TermEvent),
    Tick,
    Connected(Arc<Engine>),
    ConnectFailed(String),
    Event(Event),
    Reply(Reply),
    Toast(String, Level),
}

/// The result of one engine call, back on the TUI's loop.
pub enum Reply {
    Projects(Result<Vec<ProjectInfo>, String>),
    Runs(Result<RunList, String>),
    Health(HealthInfo),
    Run(i64, Result<RunDetail, String>),
    StepOutput { run: i64, step: usize, result: Result<LogChunk, String> },
    ServiceOutput { project: String, service: String, result: Result<String, String> },
    FullOutput(Result<String, String>),
    Started(Result<StartOutcome, String>),
    DryRun(Result<StartOutcome, String>),
    Decided(Result<(), String>),
    Cancelled(Result<(), String>),
    ServiceControl(Result<String, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Error,
}

pub enum Conn {
    Connecting,
    Connected,
    /// The engine could not start (for example, the binary's path is unknown).
    Failed(String),
}

pub enum View {
    Dashboard,
    Project(project::ProjectView),
    Run(Box<run_view::RunView>),
    Health,
}

pub struct App {
    pub conn: Conn,
    pub engine: Option<Arc<Engine>>,
    pub tx: UnboundedSender<Msg>,
    pub quit: bool,
    pub nav: Vec<View>,
    pub overlay: Option<Overlay>,
    pub toast: Option<(String, Level, Instant)>,

    pub projects: Vec<ProjectInfo>,
    pub projects_loaded: bool,
    pub recent: Vec<RunSummary>,
    pub waiting: Vec<ApprovalInfo>,
    pub health: Option<HealthInfo>,
    pub config: Option<ConfigStatus>,

    pub dash: dashboard::DashState,

    /// Clickable areas of the last frame.
    pub hits: HitMap,
    last_click: Option<(Instant, Target, usize)>,

    watching: Watch,
    refetch_projects_at: Option<Instant>,
    refetch_recent_at: Option<Instant>,
    last_health: Option<Instant>,
    next_connect: Instant,
}

impl App {
    pub fn new(tx: UnboundedSender<Msg>) -> App {
        App {
            conn: Conn::Connecting,
            engine: None,
            tx,
            quit: false,
            nav: vec![View::Dashboard],
            overlay: None,
            toast: None,
            projects: Vec::new(),
            projects_loaded: false,
            recent: Vec::new(),
            waiting: Vec::new(),
            health: None,
            config: None,
            dash: Default::default(),
            hits: HitMap::default(),
            last_click: None,
            watching: Watch::default(),
            refetch_projects_at: None,
            refetch_recent_at: None,
            last_health: None,
            next_connect: Instant::now(),
        }
    }

    // ── engine calls ──

    /// Runs one engine call in the background and sends its reply back as a message.
    pub fn call<F, Fut>(&self, f: F)
    where
        F: FnOnce(Arc<Engine>) -> Fut + Send + 'static,
        Fut: Future<Output = Reply> + Send + 'static,
    {
        let Some(engine) = self.engine.clone() else { return };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Msg::Reply(f(engine).await));
        });
    }

    pub fn toast(&mut self, msg: impl Into<String>, level: Level) {
        self.toast = Some((msg.into(), level, Instant::now()));
    }

    pub fn project(&self, id: &str) -> Option<&ProjectInfo> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub fn top(&self) -> &View {
        self.nav.last().expect("nav is never empty")
    }

    pub fn top_mut(&mut self) -> &mut View {
        self.nav.last_mut().expect("nav is never empty")
    }

    fn push(&mut self, v: View) {
        self.nav.push(v);
        self.sync_watch();
    }

    pub fn pop(&mut self) {
        if self.nav.len() > 1 {
            self.nav.pop();
            self.sync_watch();
        }
    }

    pub fn open_run(&mut self, id: i64) {
        self.push(View::Run(Box::new(run_view::RunView::new(id))));
        self.fetch_run(id);
    }

    pub fn open_project(&mut self, id: &str) {
        self.push(View::Project(project::ProjectView::new(id)));
    }

    /// Opens the engine for the first time.
    pub fn start(&mut self) {
        self.connect();
    }

    /// Opens the engine and starts its pollers. Nothing runs in the
    /// background once the TUI quits.
    fn connect(&mut self) {
        self.conn = Conn::Connecting;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match crate::ctx::Ctx::from_env() {
                Ok(ctx) => {
                    let (engine, mut rx) = Engine::open(ctx);
                    let _ = tx.send(Msg::Connected(engine));
                    while let Some(e) = rx.recv().await {
                        if tx.send(Msg::Event(e)).is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Msg::ConnectFailed(e));
                }
            }
        });
    }

    fn on_connected(&mut self) {
        self.conn = Conn::Connected;
        if let Some(engine) = &self.engine {
            self.watching = self.wanted_watch();
            engine.watch(self.watching.clone());
        }
        self.fetch_projects();
        self.fetch_runs();
        self.fetch_health();
        // Re-sync anything on screen.
        let runs: Vec<i64> = self.nav.iter().filter_map(|v| if let View::Run(r) = v { Some(r.id) } else { None }).collect();
        for id in runs {
            self.fetch_run(id);
        }
        dashboard::load_log(self, true);
    }

    pub fn fetch_projects(&self) {
        self.call(|e| async move { Reply::Projects(e.projects().await) });
    }

    pub fn fetch_runs(&self) {
        self.call(|e| async move { Reply::Runs(e.runs().await) });
    }

    pub fn fetch_health(&self) {
        self.call(|e| async move { Reply::Health(e.health().await) });
    }

    pub fn fetch_run(&self, id: i64) {
        self.call(move |e| async move { Reply::Run(id, e.run(id).await) });
    }

    pub fn fetch_step_output(&self, run: i64, step: usize) {
        self.call(move |e| async move { Reply::StepOutput { run, step, result: e.step_output(run, step).await } });
    }

    pub fn decide(&self, run: i64, step: usize, decision: Decision, confirmed: bool) {
        self.call(move |e| async move { Reply::Decided(e.decide(run, step, decision, confirmed).await) });
    }

    /// What the current screen needs the engine to follow.
    fn wanted_watch(&self) -> Watch {
        let mut w = Watch { health: matches!(self.top(), View::Health), ..Default::default() };
        for v in &self.nav {
            if let View::Run(r) = v {
                w.runs.insert(r.id);
            }
        }
        if let Some(target) = &self.dash.log_target {
            w.services.insert(target.clone());
        }
        if let Some(Overlay::Text(t)) = &self.overlay
            && let Some(target) = &t.follow_service
        {
            w.services.insert(target.clone());
        }
        w
    }

    /// Tells the engine what to follow when the screen changed it.
    pub fn sync_watch(&mut self) {
        let Some(engine) = &self.engine else { return };
        let wanted = self.wanted_watch();
        if wanted != self.watching {
            self.watching = wanted.clone();
            engine.watch(wanted);
        }
    }

    // ── updates ──

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Input(TermEvent::Key(k)) if k.kind != KeyEventKind::Release => self.on_key(k),
            Msg::Input(TermEvent::Paste(text)) => {
                if let Some(Overlay::Dialog(d)) = &mut self.overlay {
                    d.paste(&text);
                }
            }
            Msg::Input(TermEvent::Mouse(m)) => self.on_mouse(m),
            Msg::Input(_) => {}
            Msg::Tick => self.on_tick(),
            Msg::Connected(engine) => {
                self.engine = Some(engine);
                self.on_connected();
            }
            Msg::ConnectFailed(e) => {
                self.engine = None;
                self.conn = Conn::Failed(e);
                self.next_connect = Instant::now() + Duration::from_secs(2);
            }
            Msg::Event(e) => self.on_event(e),
            Msg::Reply(r) => self.on_reply(r),
            Msg::Toast(m, l) => self.toast(m, l),
        }
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        if self.engine.is_none() && !matches!(self.conn, Conn::Connecting) && now >= self.next_connect {
            self.connect();
        }
        if self.refetch_projects_at.is_some_and(|t| now >= t) {
            self.refetch_projects_at = None;
            self.fetch_projects();
        }
        if self.refetch_recent_at.is_some_and(|t| now >= t) {
            self.refetch_recent_at = None;
            self.fetch_runs();
        }
        // Checked once at start for the attention markers, then only while the
        // Health screen is open: nothing probes the system on a timer.
        let due = match self.last_health {
            None => true,
            Some(t) => matches!(self.top(), View::Health) && t.elapsed() >= Duration::from_secs(5),
        };
        if self.engine.is_some() && due {
            self.last_health = Some(now);
            self.fetch_health();
        }
        if self.toast.as_ref().is_some_and(|(_, _, t)| t.elapsed() > Duration::from_secs(5)) {
            self.toast = None;
        }
    }

    fn schedule_projects(&mut self) {
        self.refetch_projects_at.get_or_insert(Instant::now() + Duration::from_millis(250));
    }

    fn on_event(&mut self, e: Event) {
        match e {
            Event::Run(s) => {
                upsert_run(&mut self.recent, &s);
                self.recent.truncate(50);
                for v in &mut self.nav {
                    if let View::Run(r) = v {
                        r.on_summary(&s);
                    }
                }
                if !s.status.is_active() {
                    self.waiting.retain(|a| a.run_id != s.id);
                }
                self.schedule_projects();
            }
            Event::Step { run_id, step } => {
                for v in &mut self.nav {
                    if let View::Run(r) = v
                        && r.id == run_id
                    {
                        r.on_step(step.clone());
                    }
                }
            }
            Event::Approval(a) => {
                self.waiting.retain(|w| !(w.run_id == a.run_id && w.step_index == a.step_index));
                if a.status == ApprovalStatus::Pending {
                    self.waiting.push(a.clone());
                    if self.dash.focus_banner_on_new {
                        self.dash.focus = dashboard::Focus::Banner;
                    }
                }
                for v in &mut self.nav {
                    if let View::Run(r) = v
                        && r.id == a.run_id
                    {
                        r.on_approval(a.clone());
                    }
                }
                self.schedule_projects();
            }
            Event::Output { run_id, key, value } => {
                for v in &mut self.nav {
                    if let View::Run(r) = v
                        && r.id == run_id
                    {
                        r.on_output(&key, &value);
                    }
                }
            }
            Event::Service(s) => {
                if let Some(p) = self.projects.iter_mut().find(|p| p.id == s.project)
                    && let Some(slot) = p.services.iter_mut().find(|x| x.id == s.id)
                {
                    *slot = s;
                }
            }
            Event::Git { project, git } => {
                if let Some(p) = self.projects.iter_mut().find(|p| p.id == project) {
                    p.git = Some(git);
                }
            }
            // Runs came or went (a window opened or closed): refresh both lists.
            Event::Projects => {
                self.schedule_projects();
                self.refetch_recent_at.get_or_insert(Instant::now() + Duration::from_millis(250));
            }
            Event::Config(c) => {
                if !c.ok {
                    self.toast("config is invalid; the previous config stays active (see 2 health)", Level::Error);
                } else {
                    self.toast("config reloaded", Level::Info);
                }
                self.config = Some(c);
                self.fetch_health();
            }
            Event::Log(chunk) => {
                for v in &mut self.nav {
                    if let View::Run(r) = v
                        && r.id == chunk.run
                    {
                        r.on_log(&chunk);
                    }
                }
            }
            Event::ServiceLog { project, service, data } => self.on_service_log(&project, &service, &data),
        }
    }

    fn on_reply(&mut self, reply: Reply) {
        match reply {
            Reply::Projects(Ok(p)) => {
                self.projects = p;
                self.projects_loaded = true;
                dashboard::load_log(self, false);
            }
            Reply::Runs(Ok(list)) => {
                self.recent = list.runs.into_iter().take(50).collect();
                self.waiting = list.waiting;
                if !self.waiting.is_empty() && self.dash.focus_banner_on_new {
                    self.dash.focus = dashboard::Focus::Banner;
                }
            }
            Reply::Health(h) => {
                self.config = Some(h.config.clone());
                self.health = Some(h);
            }
            Reply::Run(id, Ok(d)) => {
                let mut reads = Vec::new();
                for v in &mut self.nav {
                    if let View::Run(r) = v
                        && r.id == id
                    {
                        reads.extend(r.on_detail(d.clone()));
                    }
                }
                for (run, step) in reads {
                    self.fetch_step_output(run, step);
                }
            }
            Reply::StepOutput { run, result: Ok(chunk), .. } => {
                for v in &mut self.nav {
                    if let View::Run(r) = v
                        && r.id == run
                    {
                        r.on_log(&chunk);
                    }
                }
            }
            Reply::StepOutput { run, step, result: Err(e) } => {
                for v in &mut self.nav {
                    if let View::Run(r) = v
                        && r.id == run
                    {
                        r.on_log_error(step, &e);
                    }
                }
            }
            Reply::ServiceOutput { project, service, result: Ok(text) } => {
                self.on_service_log(&project, &service, &text);
                self.dash.log.loading = false;
            }
            Reply::FullOutput(Ok(text)) => {
                if let Some(Overlay::Text(t)) = &mut self.overlay {
                    t.view.replace(if text.is_empty() { "(no output yet)\n" } else { &text });
                    t.view.loading = false;
                }
            }
            Reply::FullOutput(Err(e)) => {
                if let Some(Overlay::Text(t)) = &mut self.overlay {
                    t.view.reset();
                    t.view.feed(&format!("\x1b[31m{e}\x1b[0m\n"));
                    t.view.loading = false;
                }
            }
            Reply::Started(Ok(StartOutcome::Started(id))) => {
                self.overlay = None;
                self.toast(format!("run #{id} started"), Level::Success);
                self.open_run(id);
            }
            Reply::DryRun(Ok(StartOutcome::DryRun(plan))) => {
                if let Some(Overlay::Dialog(d)) = &mut self.overlay {
                    d.busy = false;
                }
                let mut view = TermView::new();
                view.feed(&overlay::format_plan(&plan));
                view.follow = false;
                view.scroll_up(usize::MAX / 2);
                let title = format!(
                    "dry run  {}/{}{}",
                    plan.project,
                    plan.pipeline,
                    plan.env.as_ref().map(|e| format!(" on {e}")).unwrap_or_default()
                );
                self.overlay = Some(Overlay::Text(Box::new(TextOverlay {
                    title,
                    subtitle: "nothing was executed".into(),
                    view,
                    follow_service: None,
                })));
            }
            Reply::Started(Err(e)) | Reply::DryRun(Err(e)) => {
                if let Some(Overlay::Dialog(d)) = &mut self.overlay {
                    d.busy = false;
                    d.error = Some(e);
                } else {
                    self.toast(e, Level::Error);
                }
            }
            Reply::Started(Ok(StartOutcome::DryRun(_))) | Reply::DryRun(Ok(StartOutcome::Started(_))) => {}
            Reply::Decided(Ok(())) => self.toast("decision sent", Level::Success),
            Reply::Cancelled(Ok(())) => self.toast("cancel requested", Level::Info),
            Reply::ServiceControl(Ok(text)) => self.toast(text, Level::Success),
            Reply::Projects(Err(e))
            | Reply::Runs(Err(e))
            | Reply::Run(_, Err(e))
            | Reply::ServiceOutput { result: Err(e), .. }
            | Reply::Decided(Err(e))
            | Reply::Cancelled(Err(e))
            | Reply::ServiceControl(Err(e)) => self.toast(e, Level::Error),
        }
    }

    /// A service window's whole output arrived: the dashboard shows its last
    /// lines, the full-output overlay all of it.
    fn on_service_log(&mut self, project: &str, service: &str, text: &str) {
        if self.dash.log_target.as_ref().is_some_and(|(p, s)| p == project && s == service) {
            let lines: Vec<&str> = text.lines().collect();
            let tail = lines[lines.len().saturating_sub(dashboard::LOG_LINES)..].join("\n");
            let tail = tail.trim_start_matches('\n');
            self.dash.log.replace(&if tail.is_empty() { String::new() } else { format!("{tail}\n") });
        }
        if let Some(Overlay::Text(t)) = &mut self.overlay
            && t.follow_service.as_ref().is_some_and(|(p, s)| p == project && s == service)
        {
            t.view.replace(text);
        }
    }

    // ── keys ──

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if !matches!(self.conn, Conn::Connected) {
            match k.code {
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Char('r') => {
                    self.next_connect = Instant::now();
                    if !matches!(self.conn, Conn::Connecting) {
                        self.connect();
                    }
                }
                _ => {}
            }
            return;
        }
        if let Some(overlay) = self.overlay.take() {
            self.overlay = overlay::key(self, overlay, k);
            self.sync_watch();
            return;
        }
        match k.code {
            KeyCode::Char('q') => {
                self.quit = true;
                return;
            }
            KeyCode::Char('?') => {
                self.overlay = Some(Overlay::Help);
                return;
            }
            KeyCode::Char('1') => {
                self.go_root(1);
                return;
            }
            KeyCode::Char('2') => {
                self.go_root(2);
                return;
            }
            KeyCode::Esc if self.nav.len() > 1 => {
                self.pop();
                return;
            }
            _ => {}
        }
        match self.top() {
            View::Dashboard => dashboard::key(self, k),
            View::Project(_) => project::key(self, k),
            View::Run(_) => run_view::key(self, k),
            View::Health => {
                if k.code == KeyCode::Char('r') {
                    self.fetch_health();
                }
            }
        }
        self.sync_watch();
    }

    /// Switches to a root screen: 1 dashboard, 2 health.
    fn go_root(&mut self, n: u8) {
        match n {
            1 => self.nav = vec![View::Dashboard],
            2 => {
                self.nav = vec![View::Health];
                self.fetch_health();
            }
            _ => {}
        }
        self.sync_watch();
    }

    // ── mouse ──

    /// Presses a key on the user's behalf: clicks reuse the key handlers.
    fn press(&mut self, code: KeyCode) {
        self.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        if !matches!(self.conn, Conn::Connected) {
            return;
        }
        let Some((target, row)) = self.hits.at(m.column, m.row, self.overlay.is_some()) else { return };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => match self.overlay.take() {
                Some(overlay) => {
                    self.overlay = overlay::click(self, overlay, target);
                    self.sync_watch();
                }
                None => self.on_click(target, row),
            },
            MouseEventKind::ScrollUp => self.on_wheel(target, false),
            MouseEventKind::ScrollDown => self.on_wheel(target, true),
            MouseEventKind::ScrollLeft => self.on_hwheel(target, false),
            MouseEventKind::ScrollRight => self.on_hwheel(target, true),
            _ => {}
        }
    }

    fn on_click(&mut self, target: Target, row: usize) {
        let valid = match target {
            Target::Tab(n) => {
                self.go_root(n);
                false
            }
            Target::DashBanner | Target::DashProjects | Target::DashServices | Target::DashRuns => dashboard::click(self, target, row),
            Target::ProjPipelines | Target::ProjServices => project::click(self, target, row),
            Target::RunSteps => run_view::click(self, row),
            _ => false,
        };
        let now = Instant::now();
        let double = valid
            && target.is_list()
            && self.last_click.is_some_and(|(t, tg, r)| tg == target && r == row && now.duration_since(t) < Duration::from_millis(400));
        self.last_click = if double { None } else { Some((now, target, row)) };
        // A double click opens the row; the approval banner opens on one click.
        if double || (valid && target == Target::DashBanner) {
            self.press(KeyCode::Enter);
        }
        self.sync_watch();
    }

    /// The wheel scrolls output panes and moves the selection in lists.
    fn on_wheel(&mut self, target: Target, down: bool) {
        const LINES: usize = 3;
        let step = if down { KeyCode::Down } else { KeyCode::Up };
        match target {
            Target::DashLog => {
                if down {
                    self.dash.log.scroll_down(LINES)
                } else {
                    self.dash.log.scroll_up(LINES)
                }
            }
            Target::DashProjects | Target::DashServices | Target::DashRuns => {
                dashboard::click(self, target, usize::MAX);
                self.press(step);
            }
            Target::ProjPipelines | Target::ProjServices => {
                project::click(self, target, usize::MAX);
                self.press(step);
            }
            Target::RunSteps => self.press(step),
            Target::RunOutput => run_view::scroll_output(self, |o| if down { o.scroll_down(LINES) } else { o.scroll_up(LINES) }),
            Target::TextOverlay => {
                if let Some(Overlay::Text(t)) = &mut self.overlay {
                    if down { t.view.scroll_down(LINES) } else { t.view.scroll_up(LINES) }
                }
            }
            _ => {}
        }
    }

    fn on_hwheel(&mut self, target: Target, right: bool) {
        match target {
            Target::DashLog if right => self.dash.log.scroll_right(8, 40),
            Target::DashLog => self.dash.log.scroll_left(8),
            Target::RunOutput => run_view::scroll_output(self, |o| if right { o.scroll_right(8, 60) } else { o.scroll_left(8) }),
            Target::TextOverlay => {
                if let Some(Overlay::Text(t)) = &mut self.overlay {
                    if right { t.view.scroll_right(8, 40) } else { t.view.scroll_left(8) }
                }
            }
            _ => {}
        }
    }

    /// Whether a screen tab needs attention (`!` in the top bar).
    pub fn attention(&self, tab: usize) -> bool {
        match tab {
            1 => !self.waiting.is_empty() || self.projects.iter().any(|p| p.services.iter().any(|s| s.state == ServiceState::Crashed)),
            2 => {
                self.config.as_ref().is_some_and(|c| !c.ok)
                    || self.health.as_ref().is_some_and(|h| {
                        h.tools.iter().any(|t| !t.ok) || h.docker.as_ref().is_some_and(|d| !d.ok) || h.tmux.as_ref().is_some_and(|t| !t.ok)
                    })
            }
            _ => false,
        }
    }
}

fn upsert_run(list: &mut Vec<RunSummary>, s: &RunSummary) {
    match list.iter_mut().find(|r| r.id == s.id) {
        Some(r) => *r = s.clone(),
        None => {
            list.push(s.clone());
            list.sort_by_key(|r| std::cmp::Reverse(r.id));
        }
    }
}
