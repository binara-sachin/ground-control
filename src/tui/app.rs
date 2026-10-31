//! TUI state and message handling. The TUI holds no state of its own beyond
//! the screen: everything comes from the daemon's responses and events.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc::UnboundedSender;

use super::screens::{dashboard, dialog, history, project, run_view};
use super::term_view::TermView;
use crate::client::{Client, ConnectError, Incoming, RequestError};
use crate::protocol::*;

pub enum Msg {
    Input(TermEvent),
    Tick,
    Connected(Client),
    ConnectFailed(ConnectError),
    Daemon(Incoming),
    Reply(Tag, Result<Response, RequestError>),
    Toast(String, Level),
}

#[derive(Debug, Clone)]
pub enum Tag {
    Projects,
    Recent,
    History { page: u32 },
    Health,
    Run(i64),
    Log { run: i64, step: usize },
    ServiceLog { project: String, service: String },
    FullLog,
    Diff,
    Start,
    DryRun,
    Decide,
    Cancel,
    Service,
    HistoryDetail(i64),
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
    NotRunning(String),
    Mismatch(String),
}

pub enum View {
    Dashboard,
    Project(project::ProjectView),
    Run(Box<run_view::RunView>),
    History,
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
    pub client: Option<Client>,
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
    pub hist: history::HistState,

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
            client: None,
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
            hist: Default::default(),
            subscribed: BTreeSet::new(),
            refetch_projects_at: None,
            refetch_recent_at: None,
            last_health: None,
            next_connect: Instant::now(),
        }
    }

    // ── requests ──

    pub fn request(&self, req: Request, tag: Tag) {
        let Some(client) = self.client.clone() else { return };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let r = client.request(req).await;
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

    pub fn push(&mut self, v: View) {
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

    /// First connection attempt.
    pub fn start(&mut self) {
        self.connect();
    }

    fn connect(&mut self) {
        self.conn = Conn::Connecting;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match Client::connect(&crate::paths::socket_path()).await {
                Ok((client, mut rx)) => {
                    let _ = tx.send(Msg::Connected(client));
                    while let Some(m) = rx.recv().await {
                        let gone = matches!(m, Incoming::Disconnected);
                        if tx.send(Msg::Daemon(m)).is_err() || gone {
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
        self.request(Request::RunsQuery { filter: RunFilter::default(), page: 0 }, Tag::Recent);
        self.request(Request::HealthGet, Tag::Health);
        history::reload(self);
        // Re-sync anything on screen.
        let runs: Vec<i64> = self.nav.iter().filter_map(|v| if let View::Run(r) = v { Some(r.id) } else { None }).collect();
        for id in runs {
            self.request(Request::RunGet { id }, Tag::Run(id));
        }
        dashboard::load_log(self, true);
    }

    /// Subscribes to the topics the current screen needs (§10.6).
    pub fn sync_subscription(&mut self) {
        if self.client.is_none() {
            return;
        }
        let mut topics: BTreeSet<String> = ["runs", "services", "git", "config"].iter().map(|s| s.to_string()).collect();
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
                } else if self.hist.editing {
                    self.hist.filter_text.push_str(text.trim());
                }
            }
            Msg::Input(_) => {}
            Msg::Tick => self.on_tick(),
            Msg::Connected(client) => {
                self.client = Some(client);
                self.on_connected();
            }
            Msg::ConnectFailed(e) => {
                self.client = None;
                self.conn = match e {
                    ConnectError::VersionMismatch { .. } => Conn::Mismatch(e.to_string()),
                    _ => Conn::NotRunning(e.to_string()),
                };
                self.next_connect = Instant::now() + Duration::from_secs(2);
            }
            Msg::Daemon(Incoming::Disconnected) => {
                self.client = None;
                self.conn = Conn::NotRunning("connection to the daemon was lost".into());
                self.next_connect = Instant::now() + Duration::from_secs(1);
            }
            Msg::Daemon(Incoming::Event(e)) => self.on_event(e),
            Msg::Reply(tag, r) => self.on_reply(tag, r),
            Msg::Toast(m, l) => self.toast(m, l),
        }
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        if self.client.is_none() && !matches!(self.conn, Conn::Connecting) && now >= self.next_connect {
            self.connect();
        }
        if self.refetch_projects_at.is_some_and(|t| now >= t) {
            self.refetch_projects_at = None;
            self.request(Request::ProjectsList, Tag::Projects);
        }
        if self.refetch_recent_at.is_some_and(|t| now >= t) {
            self.refetch_recent_at = None;
            self.request(Request::RunsQuery { filter: RunFilter::default(), page: 0 }, Tag::Recent);
        }
        let health_every = if matches!(self.top(), View::Health) { Duration::from_secs(5) } else { Duration::from_secs(60) };
        if self.client.is_some() && self.last_health.is_none_or(|t| t.elapsed() >= health_every) {
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
                self.hist.on_run(&s);
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
                self.hist.on_step(run_id, &step);
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
            Event::Projects => self.schedule_projects(),
            Event::Config(c) => {
                if !c.ok {
                    self.toast("config is invalid; the previous config stays active (see 3 health)", Level::Error);
                } else {
                    self.toast("config reloaded", Level::Info);
                }
                self.config = Some(c);
                self.request(Request::HealthGet, Tag::Health);
            }
            Event::Log(chunk) => {
                let mut gap = None;
                for v in &mut self.nav {
                    if let View::Run(r) = v
                        && r.id == chunk.run
                    {
                        gap = r.on_log(&chunk);
                    }
                }
                if let Some(from) = gap {
                    self.request(
                        Request::LogRead { run: chunk.run, step: chunk.step, from_offset: from },
                        Tag::Log { run: chunk.run, step: chunk.step },
                    );
                }
            }
            Event::ServiceLog { project, service, data } => {
                if self.dash.log_target.as_ref().is_some_and(|(p, s)| p == &project && s == &service) {
                    self.dash.log.feed(&data);
                }
                if let Some(Overlay::Text(t)) = &mut self.overlay
                    && t.follow_service.as_ref().is_some_and(|(p, s)| p == &project && s == &service)
                {
                    t.view.feed(&data);
                }
            }
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
            (Tag::History { page }, Ok(Response::Runs(p))) => self.hist.on_page(page, p),
            (Tag::HistoryDetail(id), Ok(Response::Run(d))) => self.hist.on_detail(id, d),
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
                for (run, step, from) in reads {
                    self.request(Request::LogRead { run, step, from_offset: from }, Tag::Log { run, step });
                }
            }
            (Tag::Log { run, step }, Ok(Response::Log(chunk))) => {
                let mut more = None;
                for v in &mut self.nav {
                    if let View::Run(r) = v
                        && r.id == run
                    {
                        more = r.on_log_read(step, &chunk);
                    }
                }
                if let Some(from) = more {
                    self.request(Request::LogRead { run, step, from_offset: from }, Tag::Log { run, step });
                }
            }
            (Tag::ServiceLog { project, service }, Ok(Response::Text { text, .. })) => {
                if self.dash.log_target.as_ref().is_some_and(|(p, s)| p == &project && s == &service) {
                    let lines: Vec<&str> = text.lines().collect();
                    let tail = lines[lines.len().saturating_sub(dashboard::LOG_LINES)..].join("\n");
                    let tail = tail.trim_start_matches('\n');
                    self.dash.log.reset();
                    if !tail.is_empty() {
                        self.dash.log.feed(tail);
                        self.dash.log.feed("\n");
                    }
                    self.dash.log.loading = false;
                }
            }
            (Tag::FullLog, Ok(Response::Text { text, path })) => {
                if let Some(Overlay::Text(t)) = &mut self.overlay {
                    t.view.reset();
                    if text.is_empty() {
                        t.view.feed("(no output yet)\n");
                    } else {
                        t.view.feed(&text);
                    }
                    t.view.loading = false;
                    if let Some(p) = path {
                        t.subtitle = p;
                    }
                }
            }
            (Tag::Diff, Ok(Response::Text { text, path })) => {
                if let Some(Overlay::Text(t)) = &mut self.overlay {
                    t.view.reset();
                    t.view.feed(&colorize_diff(&text));
                    t.view.loading = false;
                    t.view.follow = false;
                    t.view.scroll_up(usize::MAX / 2);
                    if let Some(p) = path {
                        t.subtitle = p;
                    }
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
            (Tag::Decide, Ok(_)) => self.toast("decision recorded", Level::Success),
            (Tag::Cancel, Ok(_)) => self.toast("cancel requested", Level::Info),
            (Tag::Service, Ok(Response::Text { text, .. })) => self.toast(text, Level::Success),
            (Tag::Service, Ok(_)) => {}
            (Tag::FullLog | Tag::Diff, Err(e)) => {
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
                KeyCode::Char('k') => self.kickstart(),
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
        // Text input on the history screen owns the keyboard.
        if matches!(self.top(), View::History) && self.hist.editing {
            history::key(self, k);
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
                self.nav = vec![View::Dashboard];
                self.sync_subscription();
                return;
            }
            KeyCode::Char('2') => {
                self.nav = vec![View::History];
                self.sync_subscription();
                return;
            }
            KeyCode::Char('3') => {
                self.nav = vec![View::Health];
                self.request(Request::HealthGet, Tag::Health);
                self.sync_subscription();
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
            View::History => history::key(self, k),
            View::Health => {
                if k.code == KeyCode::Char('r') {
                    self.request(Request::HealthGet, Tag::Health);
                }
            }
        }
        self.sync_subscription();
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

    /// Offers `launchctl kickstart` for the agent (§5.4). Never runs the daemon in-process.
    fn kickstart(&mut self) {
        let restart = matches!(self.conn, Conn::Mismatch(_));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let uid = nix::unistd::getuid().as_raw();
            let target = format!("gui/{uid}/{}", crate::paths::LAUNCHD_LABEL);
            let mut cmd = tokio::process::Command::new("launchctl");
            cmd.arg("kickstart");
            if restart {
                cmd.arg("-k");
            }
            cmd.arg(&target);
            let msg = match cmd.output().await {
                Ok(o) if o.status.success() => (format!("launchctl kickstart {target}: ok"), Level::Success),
                Ok(o) => (format!("launchctl failed: {}", String::from_utf8_lossy(&o.stderr).trim()), Level::Error),
                Err(e) => (format!("cannot run launchctl: {e} (is the agent installed? run `make install`)"), Level::Error),
            };
            let _ = tx.send(Msg::Toast(msg.0, msg.1));
        });
        self.next_connect = Instant::now() + Duration::from_millis(800);
        self.conn = Conn::NotRunning("waiting for the daemon to start…".into());
    }

    /// Whether a screen tab needs attention (`!` in the top bar).
    pub fn attention(&self, tab: usize) -> bool {
        match tab {
            1 => !self.waiting.is_empty() || self.projects.iter().any(|p| p.services.iter().any(|s| s.state == ServiceState::Crashed)),
            3 => {
                self.config.as_ref().is_some_and(|c| !c.ok)
                    || self.health.as_ref().is_some_and(|h| {
                        h.tools.iter().any(|t| !t.ok) || h.docker.as_ref().is_some_and(|d| !d.ok) || h.tmux.as_ref().is_some_and(|t| !t.ok)
                    })
            }
            _ => false,
        }
    }
}

pub fn upsert_run(list: &mut Vec<RunSummary>, s: &RunSummary) {
    match list.iter_mut().find(|r| r.id == s.id) {
        Some(r) => *r = s.clone(),
        None => {
            list.push(s.clone());
            list.sort_by_key(|r| std::cmp::Reverse(r.id));
        }
    }
}

/// Adds colour to a unified diff for the viewer.
pub fn colorize_diff(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    for line in text.lines() {
        let code = if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") || line.starts_with("index ") {
            "1"
        } else if line.starts_with('+') {
            "32"
        } else if line.starts_with('-') {
            "31"
        } else if line.starts_with("@@") {
            "36"
        } else if line.starts_with('#') {
            "33"
        } else {
            ""
        };
        if code.is_empty() {
            out.push_str(line);
        } else {
            out.push_str(&format!("\x1b[{code}m{line}\x1b[0m"));
        }
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("(empty diff)\n");
    }
    out
}
