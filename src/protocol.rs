//! The types the TUI and its in-process engine exchange: what the engine is
//! asked to start and decide, what the TUI is watching, the events the engine
//! emits, and the data they carry (§10.6).

use std::collections::{BTreeMap, BTreeSet};

pub use crate::config::{Diagnostic, InputKind, RestartPolicy, ServiceAction, Severity};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub type Timestamp = i64; // unix seconds

#[derive(Debug, Clone, Default)]
pub struct RunStartParams {
    pub project: String,
    pub pipeline: String,
    pub env: Option<String>,
    pub inputs: BTreeMap<String, String>,
    pub dry_run: bool,
    /// Set by the TUI once the user has seen the confirmation dialog of a
    /// protected run. `runs::start` refuses protected runs without it.
    pub confirmed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    BadRequest,
    NotFound,
    Refused,
    AlreadyDecided,
    Internal,
}

/// What the TUI is showing, which tells the engine what to poll: the Health
/// screen's Docker probe, the runs whose step output is followed, and the
/// (project, service) pairs whose output is followed. Each `Engine::watch`
/// replaces the previous one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Watch {
    pub health: bool,
    pub runs: BTreeSet<i64>,
    pub services: BTreeSet<(String, String)>,
}

/// What `Engine::start_run` did: opened a run's window, or only planned it.
#[derive(Debug, Clone)]
pub enum StartOutcome {
    Started(i64),
    DryRun(DryRunPlan),
}

#[derive(Debug, Clone)]
pub enum Event {
    Run(RunSummary),
    Step {
        run_id: i64,
        step: StepInfo,
    },
    Approval(ApprovalInfo),
    Output {
        run_id: i64,
        key: String,
        value: String,
    },
    Service(ServiceStatus),
    Git {
        project: String,
        git: GitStatus,
    },
    /// The project list changed shape (config reload); call `Engine::projects` again.
    Projects,
    Config(ConfigStatus),
    Log(LogChunk),
    /// The service window's whole output, which replaces what was shown.
    ServiceLog {
        project: String,
        service: String,
        data: String,
    },
}

// ── data ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitStatus {
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub modified: u32,
    pub untracked: u32,
    pub conflicted: u32,
    /// `Some(true)` when `origin` matches the declared remote, `None` when no remote is declared.
    pub origin_matches: Option<bool>,
    pub origin_url: Option<String>,
    pub checked_at: Timestamp,
    pub error: Option<String>,
}

impl GitStatus {
    pub fn is_clean(&self) -> bool {
        self.staged + self.modified + self.untracked + self.conflicted == 0
    }
}

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub id: String,
    pub path: String,
    pub disabled: Option<String>,
    pub busy_run: Option<i64>,
    pub git: Option<GitStatus>,
    pub environments: Vec<EnvInfo>,
    pub services: Vec<ServiceStatus>,
    pub groups: Vec<GroupInfo>,
    pub pipelines: Vec<PipelineInfo>,
}

#[derive(Debug, Clone)]
pub struct EnvInfo {
    pub name: String,
    pub protected: bool,
}

#[derive(Debug, Clone)]
pub struct GroupInfo {
    pub id: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PipelineInfo {
    pub id: String,
    pub protected: bool,
    pub environments: Vec<String>,
    pub inputs: Vec<InputInfo>,
    pub steps: Vec<StepOutline>,
    pub warning: Option<String>,
    pub last_run: Option<RunSummary>,
    pub waiting: u32,
}

#[derive(Debug, Clone)]
pub struct InputInfo {
    pub name: String,
    pub kind: InputKind,
    pub pattern: Option<String>,
    pub options: Vec<String>,
    pub default: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StepOutline {
    pub kind: StepKindName,
    pub label: String,
    /// Command, approval message, or `start <target>`.
    pub detail: String,
    /// An approval message as it reads on each of the pipeline's environments
    /// (key `""` when it has none), expanded like a dry run's. Inputs aren't
    /// chosen yet, so `$INPUT_*` and `$OUTPUT_*` stay as written.
    pub previews: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKindName {
    Run,
    Approve,
    Service,
}

impl StepKindName {
    pub fn as_str(self) -> &'static str {
        match self {
            StepKindName::Run => "run",
            StepKindName::Approve => "approve",
            StepKindName::Service => "service",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "approve" => StepKindName::Approve,
            "service" => StepKindName::Service,
            _ => StepKindName::Run,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Stopped,
    Starting,
    Running,
    Unhealthy,
    Stopping,
    /// Backing off before an automatic restart.
    Restarting,
    Crashed,
}

impl ServiceState {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceState::Stopped => "stopped",
            ServiceState::Starting => "starting",
            ServiceState::Running => "running",
            ServiceState::Unhealthy => "unhealthy",
            ServiceState::Stopping => "stopping",
            ServiceState::Restarting => "restarting",
            ServiceState::Crashed => "crashed",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "starting" => ServiceState::Starting,
            "running" => ServiceState::Running,
            "unhealthy" => ServiceState::Unhealthy,
            "stopping" => ServiceState::Stopping,
            "restarting" => ServiceState::Restarting,
            "crashed" => ServiceState::Crashed,
            _ => ServiceState::Stopped,
        }
    }
    /// Whether the service has (or is about to have) a live process.
    pub fn is_up(self) -> bool {
        matches!(self, ServiceState::Running | ServiceState::Unhealthy | ServiceState::Starting)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceStatus {
    pub project: String,
    pub id: String,
    pub state: ServiceState,
    pub healthy: bool,
    pub pid: Option<i32>,
    pub port: Option<u16>,
    pub started_at: Option<Timestamp>,
    pub restart_count: u32,
    pub last_exit_code: Option<i32>,
    /// Short explanation from the worker: "crashed 5× in 5m, gave up 14:02",
    /// "port 5432 is in use (not started by GroundControl)".
    pub message: Option<String>,
    pub depends_on: Vec<String>,
    pub has_health_check: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    AwaitingApproval,
    Succeeded,
    Failed,
    Cancelled,
    Rejected,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::AwaitingApproval => "awaiting_approval",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Rejected => "rejected",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "running" => RunStatus::Running,
            "awaiting_approval" => RunStatus::AwaitingApproval,
            "succeeded" => RunStatus::Succeeded,
            "failed" => RunStatus::Failed,
            "cancelled" => RunStatus::Cancelled,
            "rejected" => RunStatus::Rejected,
            _ => return None,
        })
    }
    pub fn is_active(self) -> bool {
        matches!(self, RunStatus::Running | RunStatus::AwaitingApproval)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub id: i64,
    pub project: String,
    pub pipeline: String,
    pub env: Option<String>,
    pub status: RunStatus,
    /// Extra detail for the status, for example "approval expired".
    pub reason: Option<String>,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub dirty: bool,
    pub unpushed: bool,
    pub protected: bool,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Waiting,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    Rejected,
    Skipped,
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            StepStatus::Pending => "pending",
            StepStatus::Running => "running",
            StepStatus::Waiting => "waiting",
            StepStatus::Succeeded => "succeeded",
            StepStatus::Failed => "failed",
            StepStatus::TimedOut => "timed_out",
            StepStatus::Cancelled => "cancelled",
            StepStatus::Rejected => "rejected",
            StepStatus::Skipped => "skipped",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "running" => StepStatus::Running,
            "waiting" => StepStatus::Waiting,
            "succeeded" => StepStatus::Succeeded,
            "failed" => StepStatus::Failed,
            "timed_out" => StepStatus::TimedOut,
            "cancelled" => StepStatus::Cancelled,
            "rejected" => StepStatus::Rejected,
            "skipped" => StepStatus::Skipped,
            _ => StepStatus::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StepInfo {
    pub index: usize,
    pub name: String,
    pub kind: StepKindName,
    pub command: String,
    pub cwd: Option<String>,
    pub status: StepStatus,
    pub exit_code: Option<i32>,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
    Cancelled,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Rejected => "rejected",
            ApprovalStatus::Expired => "expired",
            ApprovalStatus::Cancelled => "cancelled",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "approved" => ApprovalStatus::Approved,
            "rejected" => ApprovalStatus::Rejected,
            "expired" => ApprovalStatus::Expired,
            "cancelled" => ApprovalStatus::Cancelled,
            _ => ApprovalStatus::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalInfo {
    pub run_id: i64,
    pub step_index: usize,
    pub project: String,
    pub pipeline: String,
    pub env: Option<String>,
    pub protected: bool,
    pub message: String,
    pub status: ApprovalStatus,
    pub expires_at: Timestamp,
}

#[derive(Debug, Clone)]
pub struct RunDetail {
    pub summary: RunSummary,
    pub inputs: BTreeMap<String, String>,
    pub steps: Vec<StepInfo>,
    pub approvals: Vec<ApprovalInfo>,
    pub outputs: Vec<OutputInfo>,
    pub unpushed_count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    pub key: String,
    pub value: String,
    pub step_index: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct RunList {
    /// Runs that still have a window, newest first. There is no history:
    /// a succeeded run's window closes itself after 5 seconds.
    pub runs: Vec<RunSummary>,
    /// Approvals currently waiting, across all projects.
    pub waiting: Vec<ApprovalInfo>,
}

#[derive(Debug, Clone)]
pub struct LogChunk {
    pub run: i64,
    pub step: usize,
    /// The step's whole output so far, as captured from the run's pane
    /// (already masked). It replaces what was shown before.
    pub data: String,
    /// True when the step has finished and no more output will arrive.
    pub complete: bool,
}

#[derive(Debug, Clone)]
pub struct DryRunPlan {
    pub project: String,
    pub pipeline: String,
    pub env: Option<String>,
    pub protected: bool,
    pub inputs: BTreeMap<String, String>,
    pub steps: Vec<DryRunStep>,
    /// Problems that would stop a real run (repository guard, busy project).
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DryRunStep {
    pub index: usize,
    pub kind: StepKindName,
    pub label: String,
    /// Resolved command (unexpanded; inputs reach it only via the environment).
    pub command: String,
    pub cwd: String,
    pub env_names: Vec<String>,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ToolStatus {
    pub name: String,
    pub path: Option<String>,
    pub ok: bool,
}

#[derive(Debug, Clone)]
pub struct Reachability {
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct ConfigStatus {
    pub path: String,
    /// Whether the file on disk is currently valid.
    pub ok: bool,
    /// Whether a valid config is active (possibly an older one).
    pub active: bool,
    pub diagnostics: Vec<Diagnostic>,
    pub last_attempt_at: Option<Timestamp>,
    pub loaded_at: Option<Timestamp>,
}

#[derive(Debug, Clone)]
pub struct HealthInfo {
    pub tools: Vec<ToolStatus>,
    pub tools_checked_at: Option<Timestamp>,
    pub docker: Option<Reachability>,
    pub tmux: Option<Reachability>,
    pub config: ConfigStatus,
    pub version: String,
    /// When this TUI opened.
    pub started_at: Timestamp,
}

pub fn now() -> Timestamp {
    chrono::Utc::now().timestamp()
}
