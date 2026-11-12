//! TUI state and message handling. The TUI holds no state of its own beyond
//! the screen: everything comes from the engine's responses and events, which
//! it reads from what the tmux-hosted workers record.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use tokio::sync::mpsc::UnboundedSender;

use super::mouse::{HitMap, Target};
use super::screens::{dashboard, dialog, project, run_view};
use super::term_view::TermView;
use crate::engine::{Engine, RequestError};
use crate::protocol::*;

pub enum Msg {
    Input(TermEvent),
    Tick,
    Connected(std::sync::Arc<Engine>),
    ConnectFailed(String),
    Event(Event),
    Reply(Tag, Result<Response, RequestError>),
    Toast(String, Level),
}

#[derive(Debug, Clone)]
pub enum Tag {
    Projects,
    Recent,
    Health,
    Run(i64),
    Log { run: i64, step: usize },
    ServiceLog { project: String, service: String },
    FullLog,
    Start,
    DryRun,
    Decide,
    Cancel,
    Service,
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
    /// For logs fed live: the service whose events extend this view.
    pub follow_service: Option<(String, String)>,
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

pub struct App {
    pub conn: Conn,
    pub engine: Option<std::sync::Arc<Engine>>,
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

    subscribed: BTreeSet<String>,
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
            subscribed: BTreeSet::new(),
            refetch_projects_at: None,
            refetch_recent_at: None,
            last_health: None,
            next_connect: Instant::now(),
        }
    }

    // ── requests ──

    pub fn request(&self, req: Request, tag: Tag) {
        let Some(engine) = self.engine.clone() else { return };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let r = engine.request(req).await;
            let _ = tx.send(Msg::Reply(tag, r));
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
        self.sync_subscription();
    }

    pub fn pop(&mut self) {
        if self.nav.len() > 1 {
            self.nav.pop();
            self.sync_subscription();
        }
    }

    pub fn open_run(&mut self, id: i64) {
        self.push(View::Run(Box::new(run_view::RunView::new(id))));
        self.request(Request::RunGet { id }, Tag::Run(id));
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
        self.subscribed.clear();
        self.sync_subscription();
        self.request(Request::ProjectsList, Tag::Projects);
        self.request(Request::RunsList, Tag::Recent);
        self.request(Request::HealthGet, Tag::Health);
        // Re-sync anything on screen.
        let runs: Vec<i64> = self.nav.iter().filter_map(|v| if let View::Run(r) = v { Some(r.id) } else { None }).collect();
        for id in runs {
            self.request(Request::RunGet { id }, Tag::Run(id));
        }
        dashboard::load_log(self, true);
    }

    /// Subscribes to the topics the current screen needs (§10.6).
    pub fn sync_subscription(&mut self) {
        if self.engine.is_none() {
            return;
        }
        let mut topics: BTreeSet<String> = ["runs", "services", "git", "config"].iter().map(|s| s.to_string()).collect();
        if matches!(self.top(), View::Health) {
            topics.insert("health".into());
        }
        for v in &self.nav {
            if let View::Run(r) = v {
                topics.insert(format!("log:{}", r.id));
            }
        }
        if let Some((p, s)) = &self.dash.log_target {
            topics.insert(format!("service-log:{p}/{s}"));
        }
        if let Some(Overlay::Text(t)) = &self.overlay
            && let Some((p, s)) = &t.follow_service
        {
            topics.insert(format!("service-log:{p}/{s}"));
        }
        if topics != self.subscribed {
            self.subscribed = topics.clone();
            self.request(Request::Subscribe { topics: topics.into_iter().collect() }, Tag::Service);
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
            Msg::Reply(tag, r) => self.on_reply(tag, r),
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
            self.request(Request::ProjectsList, Tag::Projects);
        }
        if self.refetch_recent_at.is_some_and(|t| now >= t) {
            self.refetch_recent_at = None;
            self.request(Request::RunsList, Tag::Recent);
        }
        // Checked once at start for the attention markers, then only while the
        // Health screen is open: nothing probes the system on a timer.
        let due = match self.last_health {
            None => true,
            Some(t) => matches!(self.top(), View::Health) && t.elapsed() >= Duration::from_secs(5),
        };
        if self.engine.is_some() && due {
            self.last_health = Some(now);
            self.request(Request::HealthGet, Tag::Health);
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
                self.request(Request::HealthGet, Tag::Health);
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

    fn on_reply(&mut self, tag: Tag, r: Result<Response, RequestError>) {
        match (tag, r) {
            (Tag::Projects, Ok(Response::Projects(p))) => {
                self.projects = p;
                self.projects_loaded = true;
                dashboard::load_log(self, false);
            }
            (Tag::Recent, Ok(Response::Runs(page))) => {
                self.recent = page.runs.into_iter().take(50).collect();
                self.waiting = page.waiting;
                if !self.waiting.is_empty() && self.dash.focus_banner_on_new {
                    self.dash.focus = dashboard::Focus::Banner;
                }
            }
            (Tag::Health, Ok(Response::Health(h))) => {
                self.config = Some(h.config.clone());
                self.health = Some(h);
            }
            (Tag::Run(id), Ok(Response::Run(d))) => {
                let mut reads = Vec::new();
                for v in &mut self.nav {
                    if let View::Run(r) = v
                        && r.id == id
                    {
                        reads.extend(r.on_detail(d.clone()));
                    }
                }
                for (run, step) in reads {
                    self.request(Request::LogRead { run, step }, Tag::Log { run, step });
                }
            }
            (Tag::Log { run, .. }, Ok(Response::Log(chunk))) => {
                for v in &mut self.nav {
                    if let View::Run(r) = v
                        && r.id == run
                    {
                        r.on_log(&chunk);
                    }
                }
            }
            (Tag::ServiceLog { project, service }, Ok(Response::Text { text })) => {
                self.on_service_log(&project, &service, &text);
                self.dash.log.loading = false;
            }
            (Tag::FullLog, Ok(Response::Text { text })) => {
                if let Some(Overlay::Text(t)) = &mut self.overlay {
                    t.view.replace(if text.is_empty() { "(no output yet)\n" } else { &text });
                    t.view.loading = false;
                }
            }
            (Tag::Start, Ok(Response::RunStarted { id })) => {
                self.overlay = None;
                self.toast(format!("run #{id} started"), Level::Success);
                self.open_run(id);
            }
            (Tag::Start, Err(e)) => {
                if let Some(Overlay::Dialog(d)) = &mut self.overlay {
                    d.busy = false;
                    d.error = Some(e.message);
                } else {
                    self.toast(e.message, Level::Error);
                }
            }
            (Tag::DryRun, Ok(Response::DryRun(plan))) => {
                if let Some(Overlay::Dialog(d)) = &mut self.overlay {
                    d.busy = false;
                }
                let mut view = TermView::new();
                view.feed(&dialog::format_plan(&plan));
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
            (Tag::DryRun, Err(e)) => {
                if let Some(Overlay::Dialog(d)) = &mut self.overlay {
                    d.busy = false;
                    d.error = Some(e.message);
                } else {
                    self.toast(e.message, Level::Error);
                }
            }
            (Tag::Decide, Ok(_)) => self.toast("decision sent", Level::Success),
            (Tag::Cancel, Ok(_)) => self.toast("cancel requested", Level::Info),
            (Tag::Service, Ok(Response::Text { text, .. })) => self.toast(text, Level::Success),
            (Tag::Service, Ok(_)) => {}
            (Tag::FullLog, Err(e)) => {
                if let Some(Overlay::Text(t)) = &mut self.overlay {
                    t.view.reset();
                    t.view.feed(&format!("\x1b[31m{}\x1b[0m\n", e.message));
                    t.view.loading = false;
                }
            }
            (Tag::Log { run, step }, Err(e)) => {
                for v in &mut self.nav {
                    if let View::Run(r) = v
                        && r.id == run
                    {
                        r.on_log_error(step, &e.message);
                    }
                }
            }
            (Tag::Run(_), Err(e)) => self.toast(e.message, Level::Error),
            (_, Err(e)) => self.toast(e.message, Level::Error),
            (_, Ok(_)) => {}
        }
    }

    /// A service window's whole output arrived: the dashboard shows its last
    /// lines, the full-log overlay all of it.
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
            self.overlay = self.overlay_key(overlay, k);
            self.sync_subscription();
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
                    self.request(Request::HealthGet, Tag::Health);
                }
            }
        }
        self.sync_subscription();
    }

    /// Switches to a root screen: 1 dashboard, 2 health.
    fn go_root(&mut self, n: u8) {
        match n {
            1 => self.nav = vec![View::Dashboard],
            2 => {
                self.nav = vec![View::Health];
                self.request(Request::HealthGet, Tag::Health);
            }
            _ => {}
        }
        self.sync_subscription();
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
                    self.overlay = self.overlay_click(overlay, target);
                    self.sync_subscription();
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
        self.sync_subscription();
    }

    /// Clicks in an overlay turn into the key that does the same.
    fn overlay_click(&mut self, mut overlay: Overlay, target: Target) -> Option<Overlay> {
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
        let next = self.overlay_key(overlay, k);
        self.sync_subscription();
        next
    }

    /// The wheel scrolls output and logs, and moves the selection in lists.
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

    fn overlay_key(&mut self, overlay: Overlay, k: KeyEvent) -> Option<Overlay> {
        match overlay {
            Overlay::Help => match k.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => None,
                _ => Some(Overlay::Help),
            },
            Overlay::Dialog(d) => dialog::key(self, d, k),
            Overlay::Text(mut t) => {
                let page = 20;
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => return None,
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
                        super::copy_to_clipboard(self.tx.clone(), text, "copied");
                    }
                    _ => {}
                }
                Some(Overlay::Text(t))
            }
            Overlay::Confirm(c) => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    match c.action {
                        ConfirmAction::CancelRun(id) => self.request(Request::RunCancel { id }, Tag::Cancel),
                        ConfirmAction::Approve { run, step } => {
                            self.request(Request::RunDecide { id: run, step, decision: Decision::Approve, confirmed: true }, Tag::Decide)
                        }
                    }
                    None
                }
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => None,
                _ => Some(Overlay::Confirm(c)),
            },
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
