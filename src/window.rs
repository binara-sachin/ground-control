//! The window schema (GLOSSARY.md): every `@gc_*` option a run window or a
//! service window carries, what it means, and how its value is written.
//! Nothing else knows a key's name or a value's format; `tmux` only moves
//! the options (spec §11 lists them).
//!
//! Reading is lenient about status and strict about identity: a window with
//! no `kind`, or without the project and service or pipeline its kind needs,
//! is not a GroundControl window and is skipped (tmux-resurrect restores
//! windows like that). Status values that are missing or unreadable fall
//! back to a default, so a window published by an older binary still reads.

use std::collections::BTreeMap;

use crate::protocol::{ApprovalStatus, OutputInfo, RunStatus, ServiceState, StepStatus, Timestamp};
use crate::tmux::{Tmux, Window, decode, encode};

/// `@gc_kind` of a run window.
const RUN: &str = "run";
/// `@gc_kind` of a service window.
const SERVICE: &str = "svc";

/// Every key, read back in one `list-windows`.
const KEYS: [&str; 26] = [
    // both kinds
    "kind",
    "project",
    "state",
    "started",
    // service windows
    "service",
    "healthy",
    "msg",
    "pid",
    "restarts",
    "exit",
    "stop",
    // run windows
    "pipeline",
    "env",
    "inputs",
    "branch",
    "commit",
    "dirty",
    "unpushed",
    "protected",
    "reason",
    "finished",
    "steps",
    "outputs",
    "approval",
    "cancel",
    "decision",
];

/// Options as `(key, value)` pairs, ready for `tmux`.
type Options = Vec<(&'static str, String)>;

/// The windows of the `ground-control` session that GroundControl opened,
/// each kind oldest first.
#[derive(Debug, Clone, Default)]
pub struct Windows {
    pub runs: Vec<RunWindow>,
    pub services: Vec<ServiceWindow>,
}

impl Windows {
    fn from_tmux(windows: &[Window]) -> Self {
        let mut w = Windows {
            runs: windows.iter().filter_map(RunWindow::read).collect(),
            services: windows.iter().filter_map(ServiceWindow::read).collect(),
        };
        w.runs.sort_by_key(|r| r.id);
        w.services.sort_by_key(|s| s.id);
        w
    }

    pub fn run(&self, id: i64) -> Option<&RunWindow> {
        self.runs.iter().find(|r| r.id == id)
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty() && self.services.is_empty()
    }
}

/// Every GroundControl window; empty when the session or server is absent.
pub async fn list(tmux: &Tmux) -> Result<Windows, String> {
    Ok(Windows::from_tmux(&tmux.windows(&KEYS).await?))
}

/// The run window with this id, if it is one.
pub async fn run(tmux: &Tmux, id: i64) -> Result<Option<RunWindow>, String> {
    Ok(tmux.window(id, &KEYS).await?.as_ref().and_then(RunWindow::read))
}

/// The service window with this id, if it is one.
pub async fn service(tmux: &Tmux, id: i64) -> Result<Option<ServiceWindow>, String> {
    Ok(tmux.window(id, &KEYS).await?.as_ref().and_then(ServiceWindow::read))
}

fn get<'a>(w: &'a Window, key: &str) -> Option<&'a str> {
    w.opts.get(key).map(String::as_str).filter(|v| !v.is_empty())
}

fn bit(b: bool) -> String {
    if b { "1" } else { "0" }.to_string()
}

fn num<T: ToString>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

// ── run windows ─────────────────────────────────────────────────────────

/// A run window: the run's parameters and git snapshot, set when the window
/// is opened, and the progress its worker publishes.
#[derive(Debug, Clone, PartialEq)]
pub struct RunWindow {
    pub id: i64,
    /// The worker has exited (the window remains on exit).
    pub dead: bool,
    pub params: RunParams,
    /// `Running` until the worker publishes otherwise.
    pub state: RunStatus,
    pub reason: Option<String>,
    pub finished: Option<Timestamp>,
    pub steps: Vec<StepState>,
    pub outputs: Vec<OutputInfo>,
    pub approval: Option<ApprovalState>,
    /// The TUI asked for a cancel.
    pub cancel: bool,
    /// The TUI's answer to the approval.
    pub decision: Option<Decision>,
}

/// What a run window is opened with.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunParams {
    pub project: String,
    pub pipeline: String,
    pub env: Option<String>,
    pub inputs: BTreeMap<String, String>,
    pub protected: bool,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub dirty: bool,
    pub unpushed: Option<u32>,
    pub started: Timestamp,
}

/// One step's progress.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepState {
    pub status: Option<StepStatus>,
    pub exit_code: Option<i32>,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
}

/// The run's approval.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalState {
    pub step: usize,
    pub status: ApprovalStatus,
    pub requested_at: Timestamp,
    pub expires_at: Timestamp,
    pub decided_at: Option<Timestamp>,
    pub message: String,
}

/// An answer to the approval at `step`, as the TUI records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub status: ApprovalStatus,
    pub step: usize,
}

/// A change a run's worker publishes. Each writes only its own keys.
pub enum RunUpdate<'a> {
    State(RunStatus),
    Steps(&'a [StepState]),
    Outputs(&'a [OutputInfo]),
    /// A pending approval also clears the previous decision, so an answer to
    /// an earlier approval cannot decide this one.
    Approval(&'a ApprovalState),
    /// The run is over. `steps` is their final state, when the run got as
    /// far as starting them.
    Ended {
        state: RunStatus,
        reason: Option<&'a str>,
        finished: Timestamp,
        steps: Option<&'a [StepState]>,
    },
}

impl RunWindow {
    fn read(w: &Window) -> Option<Self> {
        if get(w, "kind")? != RUN {
            return None;
        }
        let params = RunParams {
            project: get(w, "project")?.to_string(),
            pipeline: get(w, "pipeline")?.to_string(),
            env: get(w, "env").map(str::to_string),
            inputs: get(w, "inputs").and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default(),
            protected: get(w, "protected") == Some("1"),
            branch: get(w, "branch").map(str::to_string),
            commit: get(w, "commit").map(str::to_string),
            dirty: get(w, "dirty") == Some("1"),
            unpushed: get(w, "unpushed").and_then(|v| v.parse().ok()),
            started: get(w, "started").and_then(|v| v.parse().ok()).unwrap_or(0),
        };
        Some(RunWindow {
            id: w.id,
            dead: w.dead,
            params,
            state: get(w, "state").and_then(RunStatus::parse).unwrap_or(RunStatus::Running),
            reason: get(w, "reason").map(str::to_string),
            finished: get(w, "finished").and_then(|v| v.parse().ok()),
            steps: decode_steps(get(w, "steps").unwrap_or("")),
            outputs: decode_outputs(get(w, "outputs").unwrap_or("")),
            approval: get(w, "approval").and_then(decode_approval),
            cancel: get(w, "cancel") == Some("1"),
            decision: get(w, "decision").and_then(decode_decision),
        })
    }

    /// The options a run window is opened with: its parameters, `Running`,
    /// and `steps` pending steps.
    pub fn create(p: &RunParams, steps: usize) -> Options {
        let pending = StepState { status: Some(StepStatus::Pending), ..Default::default() };
        vec![
            ("kind", RUN.to_string()),
            ("project", p.project.clone()),
            ("pipeline", p.pipeline.clone()),
            ("env", p.env.clone().unwrap_or_default()),
            ("inputs", serde_json::to_string(&p.inputs).unwrap_or_else(|_| "{}".into())),
            ("protected", bit(p.protected)),
            ("branch", p.branch.clone().unwrap_or_default()),
            ("commit", p.commit.clone().unwrap_or_default()),
            ("dirty", bit(p.dirty)),
            ("unpushed", num(p.unpushed)),
            ("started", p.started.to_string()),
            ("state", RunStatus::Running.as_str().to_string()),
            ("steps", encode_steps(&vec![pending; steps])),
        ]
    }

    pub fn update(u: RunUpdate) -> Options {
        match u {
            RunUpdate::State(s) => vec![("state", s.as_str().to_string())],
            RunUpdate::Steps(steps) => vec![("steps", encode_steps(steps))],
            RunUpdate::Outputs(outputs) => vec![("outputs", encode_outputs(outputs))],
            RunUpdate::Approval(a) if a.status == ApprovalStatus::Pending => {
                vec![("decision", String::new()), ("approval", encode_approval(a))]
            }
            RunUpdate::Approval(a) => vec![("approval", encode_approval(a))],
            RunUpdate::Ended { state, reason, finished, steps } => {
                let mut o: Options = steps.map(|s| ("steps", encode_steps(s))).into_iter().collect();
                o.extend([
                    ("state", state.as_str().to_string()),
                    ("reason", reason.unwrap_or_default().to_string()),
                    ("finished", finished.to_string()),
                ]);
                o
            }
        }
    }

    /// Asks the worker to cancel the run.
    pub fn cancel_request() -> Options {
        vec![("cancel", "1".to_string())]
    }

    /// Answers the approval.
    pub fn decision_request(d: Decision) -> Options {
        vec![("decision", format!("{}:{}", d.status.as_str(), d.step))]
    }
}

/// The requests on a run window, read fresh rather than from a listing.
pub async fn run_requests(tmux: &Tmux, target: &str) -> Result<(bool, Option<Decision>), String> {
    let v = tmux.options(target, &["cancel", "decision"]).await?;
    Ok((v[0] == "1", decode_decision(&v[1])))
}

/// `status:exit:started:finished` per step, joined by `,`.
fn encode_steps(steps: &[StepState]) -> String {
    steps
        .iter()
        .map(|s| {
            format!(
                "{}:{}:{}:{}",
                s.status.map(StepStatus::as_str).unwrap_or("pending"),
                num(s.exit_code),
                num(s.started_at),
                num(s.finished_at)
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn decode_steps(s: &str) -> Vec<StepState> {
    s.split(',')
        .filter(|e| !e.is_empty())
        .map(|e| {
            let mut it = e.split(':');
            let status = it.next().map(StepStatus::parse);
            let mut num = || it.next().and_then(|v| v.parse::<i64>().ok());
            StepState { status, exit_code: num().map(|c| c as i32), started_at: num(), finished_at: num() }
        })
        .collect()
}

/// `key=<step index>=<encoded value>` per output, joined by `,`.
fn encode_outputs(outputs: &[OutputInfo]) -> String {
    outputs.iter().map(|o| format!("{}={}={}", o.key, num(o.step_index), encode(&o.value))).collect::<Vec<_>>().join(",")
}

fn decode_outputs(s: &str) -> Vec<OutputInfo> {
    s.split(',')
        .filter_map(|e| {
            let mut it = e.splitn(3, '=');
            let key = it.next()?.to_string();
            let step_index = it.next()?.parse().ok();
            Some(OutputInfo { key, value: decode(it.next()?), step_index })
        })
        .collect()
}

/// `step:status:requested:expires:decided:<encoded message>`.
fn encode_approval(a: &ApprovalState) -> String {
    format!("{}:{}:{}:{}:{}:{}", a.step, a.status.as_str(), a.requested_at, a.expires_at, num(a.decided_at), encode(&a.message))
}

fn decode_approval(s: &str) -> Option<ApprovalState> {
    let mut it = s.splitn(6, ':');
    Some(ApprovalState {
        step: it.next()?.parse().ok()?,
        status: ApprovalStatus::parse(it.next()?),
        requested_at: it.next()?.parse().ok()?,
        expires_at: it.next()?.parse().ok()?,
        decided_at: it.next()?.parse().ok(),
        message: decode(it.next()?),
    })
}

/// `<status>:<step>`.
fn decode_decision(s: &str) -> Option<Decision> {
    let (status, step) = s.split_once(':')?;
    Some(Decision { status: ApprovalStatus::parse(status), step: step.parse().ok()? })
}

// ── service windows ─────────────────────────────────────────────────────

/// A service window: which service it runs and the status its worker
/// publishes.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceWindow {
    pub id: i64,
    /// The worker has exited (the window remains on exit).
    pub dead: bool,
    /// The worker's exit status, when it exited normally.
    pub dead_status: Option<i32>,
    pub project: String,
    pub service: String,
    pub report: ServiceReport,
    /// The TUI asked for a stop.
    pub stop: bool,
}

/// What a service's worker publishes, all at once.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceReport {
    /// `Starting` until the worker publishes otherwise.
    pub state: ServiceState,
    pub healthy: bool,
    pub pid: Option<i32>,
    pub started: Option<Timestamp>,
    pub restarts: u32,
    pub exit: Option<i32>,
    pub msg: Option<String>,
}

impl Default for ServiceReport {
    fn default() -> Self {
        ServiceReport { state: ServiceState::Starting, healthy: false, pid: None, started: None, restarts: 0, exit: None, msg: None }
    }
}

impl ServiceWindow {
    fn read(w: &Window) -> Option<Self> {
        if get(w, "kind")? != SERVICE {
            return None;
        }
        let report = ServiceReport {
            state: get(w, "state").map(ServiceState::parse).unwrap_or(ServiceState::Starting),
            healthy: get(w, "healthy") == Some("1"),
            pid: get(w, "pid").and_then(|v| v.parse().ok()),
            started: get(w, "started").and_then(|v| v.parse().ok()),
            restarts: get(w, "restarts").and_then(|v| v.parse().ok()).unwrap_or(0),
            exit: get(w, "exit").and_then(|v| v.parse().ok()),
            msg: get(w, "msg").map(str::to_string),
        };
        Some(ServiceWindow {
            id: w.id,
            dead: w.dead,
            dead_status: w.dead_status,
            project: get(w, "project")?.to_string(),
            service: get(w, "service")?.to_string(),
            report,
            stop: get(w, "stop") == Some("1"),
        })
    }

    /// The options a service window is opened with.
    pub fn create(project: &str, service: &str) -> Options {
        vec![
            ("kind", SERVICE.to_string()),
            ("project", project.to_string()),
            ("service", service.to_string()),
            ("state", ServiceState::Starting.as_str().to_string()),
        ]
    }

    pub fn update(r: &ServiceReport) -> Options {
        vec![
            ("state", r.state.as_str().to_string()),
            ("healthy", bit(r.healthy)),
            ("pid", num(r.pid)),
            ("started", num(r.started)),
            ("restarts", r.restarts.to_string()),
            ("exit", num(r.exit)),
            ("msg", r.msg.clone().unwrap_or_default()),
        ]
    }

    /// Asks the worker to stop the service.
    pub fn stop_request() -> Options {
        vec![("stop", "1".to_string())]
    }
}

/// Whether a stop was requested on a service window, read fresh.
pub async fn stop_requested(tmux: &Tmux, target: &str) -> Result<bool, String> {
    Ok(tmux.options(target, &["stop"]).await?[0] == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `tmux` would hand back after these options were set.
    fn window(id: i64, dead: bool, options: &[Options]) -> Window {
        let mut w = Window { id, dead, dead_status: None, opts: Default::default() };
        for (k, v) in options.iter().flatten() {
            assert!(KEYS.contains(k), "`{k}` is written but not read back");
            w.opts.insert(k.to_string(), decode(&encode(v)));
        }
        w
    }

    fn params() -> RunParams {
        RunParams {
            project: "app".into(),
            pipeline: "deploy".into(),
            env: Some("prod".into()),
            inputs: BTreeMap::from([("pr".into(), "42".into())]),
            protected: true,
            branch: Some("main".into()),
            commit: Some("abc123".into()),
            dirty: false,
            unpushed: Some(2),
            started: 100,
        }
    }

    #[test]
    fn run_window_round_trips() {
        let steps = vec![
            StepState { status: Some(StepStatus::Succeeded), exit_code: Some(0), started_at: Some(10), finished_at: Some(12) },
            StepState { status: Some(StepStatus::Waiting), exit_code: None, started_at: Some(12), finished_at: None },
        ];
        let outputs = vec![OutputInfo { key: "bm".into(), value: "a=b,c|d\nx".into(), step_index: Some(0) }];
        let approval = ApprovalState {
            step: 1,
            status: ApprovalStatus::Pending,
            requested_at: 12,
            expires_at: 99,
            decided_at: None,
            message: "Go: now?".into(),
        };
        let decision = Decision { status: ApprovalStatus::Approved, step: 1 };
        let w = window(
            7,
            false,
            &[
                RunWindow::create(&params(), 2),
                RunWindow::update(RunUpdate::Steps(&steps)),
                RunWindow::update(RunUpdate::Outputs(&outputs)),
                RunWindow::update(RunUpdate::Approval(&approval)),
                RunWindow::update(RunUpdate::State(RunStatus::AwaitingApproval)),
                RunWindow::cancel_request(),
                RunWindow::decision_request(decision),
            ],
        );
        assert_eq!(
            RunWindow::read(&w),
            Some(RunWindow {
                id: 7,
                dead: false,
                params: params(),
                state: RunStatus::AwaitingApproval,
                reason: None,
                finished: None,
                steps,
                outputs,
                approval: Some(approval),
                cancel: true,
                decision: Some(decision),
            })
        );
    }

    #[test]
    fn a_new_approval_clears_the_old_decision() {
        let mut a = ApprovalState {
            step: 2,
            status: ApprovalStatus::Pending,
            requested_at: 1,
            expires_at: 2,
            decided_at: None,
            message: String::new(),
        };
        let old = RunWindow::decision_request(Decision { status: ApprovalStatus::Approved, step: 0 });
        let w = window(1, false, &[RunWindow::create(&params(), 3), old.clone(), RunWindow::update(RunUpdate::Approval(&a))]);
        assert_eq!(RunWindow::read(&w).unwrap().decision, None);
        a.status = ApprovalStatus::Approved;
        let w = window(1, false, &[RunWindow::create(&params(), 3), old, RunWindow::update(RunUpdate::Approval(&a))]);
        assert!(RunWindow::read(&w).unwrap().decision.is_some(), "a decided approval keeps its decision");
    }

    #[test]
    fn ended_runs_publish_their_end() {
        let steps = vec![StepState { status: Some(StepStatus::Skipped), ..Default::default() }];
        let ended = RunUpdate::Ended { state: RunStatus::Failed, reason: Some("boom"), finished: 50, steps: Some(&steps) };
        let r = RunWindow::read(&window(1, true, &[RunWindow::create(&params(), 1), RunWindow::update(ended)])).unwrap();
        assert_eq!((r.state, r.reason.as_deref(), r.finished, r.steps), (RunStatus::Failed, Some("boom"), Some(50), steps));
    }

    #[test]
    fn service_window_round_trips() {
        let report = ServiceReport {
            state: ServiceState::Unhealthy,
            healthy: false,
            pid: Some(4242),
            started: Some(10),
            restarts: 3,
            exit: Some(1),
            msg: Some("port 5432 is in use, | by someone".into()),
        };
        let w = window(3, false, &[ServiceWindow::create("app", "db"), ServiceWindow::update(&report), ServiceWindow::stop_request()]);
        assert_eq!(
            ServiceWindow::read(&w),
            Some(ServiceWindow { id: 3, dead: false, dead_status: None, project: "app".into(), service: "db".into(), report, stop: true })
        );
    }

    #[test]
    fn status_falls_back_to_defaults() {
        let mut w = window(1, false, &[ServiceWindow::create("app", "db")]);
        w.opts.insert("state".into(), String::new());
        w.opts.insert("pid".into(), "not a pid".into());
        assert_eq!(ServiceWindow::read(&w).unwrap().report, ServiceReport::default());

        let mut w = window(2, false, &[RunWindow::create(&params(), 1)]);
        w.opts.insert("state".into(), "nonsense".into());
        w.opts.insert("approval".into(), "garbled".into());
        let r = RunWindow::read(&w).unwrap();
        assert_eq!((r.state, r.approval), (RunStatus::Running, None));
    }

    #[test]
    fn windows_without_identity_are_skipped() {
        let svc = |opts: &[(&str, &str)]| {
            let mut w = Window { id: 1, dead: false, dead_status: None, opts: Default::default() };
            w.opts.extend(opts.iter().map(|(k, v)| (k.to_string(), v.to_string())));
            Windows::from_tmux(&[w])
        };
        assert!(svc(&[]).is_empty(), "a restored window has no options");
        assert!(svc(&[("kind", "svc"), ("project", "app")]).is_empty());
        assert!(svc(&[("kind", "run"), ("project", "app")]).is_empty());
        assert!(svc(&[("kind", "other"), ("project", "app"), ("service", "db")]).is_empty());
        assert_eq!(svc(&[("kind", "svc"), ("project", "app"), ("service", "db")]).services.len(), 1);
    }

    #[test]
    fn windows_are_sorted_oldest_first() {
        let w = |id| window(id, false, &[ServiceWindow::create("app", "db")]);
        let ids: Vec<i64> = Windows::from_tmux(&[w(9), w(2), w(5)]).services.iter().map(|s| s.id).collect();
        assert_eq!(ids, [2, 5, 9]);
    }

    /// Spec §11's table of window options lists exactly the keys this module uses.
    #[test]
    fn spec_lists_every_key() {
        let spec = include_str!("../docs/groundcontrol-spec.md");
        let mut documented: Vec<&str> = spec
            .lines()
            .filter(|l| l.starts_with('|'))
            .flat_map(|l| l.split('`'))
            .filter_map(|s| s.strip_prefix("@gc_"))
            .filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
            .collect();
        documented.sort();
        documented.dedup();
        let mut keys = KEYS.to_vec();
        keys.sort();
        assert_eq!(documented, keys);
    }
}
