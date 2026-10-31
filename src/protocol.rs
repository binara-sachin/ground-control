//! Client protocol (§10.6): newline-delimited JSON over the Unix socket.
//!
//! The first message from each side is a [`Hello`]. After that the client sends
//! [`ClientMessage`]s and the daemon answers with [`ServerMessage`]s: a response
//! carrying the request's `id`, or an unsolicited event.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use crate::config::{Diagnostic, Health, InputKind, RestartPolicy, ServiceAction, Severity};

pub const PROTOCOL_VERSION: u32 = 1;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub type Timestamp = i64; // unix seconds

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub hello: HelloBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloBody {
    pub protocol: u32,
    pub version: String,
}

impl Hello {
    pub fn current() -> Self {
        Hello { hello: HelloBody { protocol: PROTOCOL_VERSION, version: VERSION.to_string() } }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMessage {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    #[serde(rename = "projects.list")]
    ProjectsList,
    #[serde(rename = "run.start")]
    RunStart(RunStartParams),
    #[serde(rename = "run.cancel")]
    RunCancel { id: i64 },
    #[serde(rename = "run.decide")]
    RunDecide { id: i64, step: usize, decision: Decision, confirmed: bool },
    #[serde(rename = "runs.query")]
    RunsQuery { filter: RunFilter, page: u32 },
    #[serde(rename = "run.get")]
    RunGet { id: i64 },
    #[serde(rename = "run.diff")]
    RunDiff { id: i64 },
    #[serde(rename = "log.read")]
    LogRead { run: i64, step: usize, from_offset: u64 },
    #[serde(rename = "service.control")]
    ServiceControl { project: String, id: String, action: ServiceAction },
    #[serde(rename = "service.log")]
    ServiceLog { project: String, id: String, max_bytes: u64 },
    #[serde(rename = "health.get")]
    HealthGet,
    #[serde(rename = "subscribe")]
    Subscribe { topics: Vec<String> },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunStartParams {
    pub project: String,
    pub pipeline: String,
    pub env: Option<String>,
    pub inputs: BTreeMap<String, String>,
    pub dry_run: bool,
    /// Set by the TUI once the user has seen the confirmation dialog of a
    /// protected run. The daemon refuses protected runs without it.
    pub confirmed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Approve,
    Reject,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFilter {
    pub project: Option<String>,
    pub pipeline: Option<String>,
    pub env: Option<String>,
    pub status: Option<RunStatus>,
}

impl RunFilter {
    /// Parses `project:x pipeline:y env:z status:failed`. Bare words filter by pipeline.
    pub fn parse(s: &str) -> Result<RunFilter, String> {
        let mut f = RunFilter::default();
        for tok in s.split_whitespace() {
            let (k, v) = tok.split_once(':').unwrap_or(("pipeline", tok));
            let v = (!v.is_empty()).then(|| v.to_string());
            match k {
                "project" => f.project = v,
                "pipeline" => f.pipeline = v,
                "env" => f.env = v,
                "status" => {
                    f.status = match v {
                        None => None,
                        Some(s) => Some(RunStatus::parse(&s).ok_or_else(|| format!("unknown status `{s}`"))?),
                    }
                }
                _ => return Err(format!("unknown filter `{k}:` (use project:, pipeline:, env:, status:)")),
            }
        }
        Ok(f)
    }

    pub fn to_text(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = &self.project {
            parts.push(format!("project:{v}"));
        }
        if let Some(v) = &self.pipeline {
            parts.push(format!("pipeline:{v}"));
        }
        if let Some(v) = &self.env {
            parts.push(format!("env:{v}"));
        }
        if let Some(v) = &self.status {
            parts.push(format!("status:{}", v.as_str()));
        }
        parts.join(" ")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Response { id: u64, #[serde(flatten)] body: ResponseBody },
    Event { event: Event },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseBody {
    Result(Response),
    Error(ErrorBody),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    NotFound,
    Busy,
    Refused,
    AlreadyDecided,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Projects(Vec<ProjectInfo>),
    RunStarted { id: i64 },
    DryRun(DryRunPlan),
    Runs(RunPage),
    Run(RunDetail),
    Log(LogChunk),
    Text { text: String, path: Option<String> },
    Health(HealthInfo),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    Run(RunSummary),
    Step { run_id: i64, step: StepInfo },
    Approval(ApprovalInfo),
    Output { run_id: i64, key: String, value: String },
    Service(ServiceStatus),
    Git { project: String, git: GitStatus },
    /// The project list changed shape (config reload); refetch `projects.list`.
    Projects,
    Config(ConfigStatus),
    Log(LogChunk),
    ServiceLog { project: String, service: String, data: String },
}

impl Event {
    /// The subscription topic this event belongs to.
    pub fn topic(&self) -> String {
        match self {
            Event::Run(_) | Event::Step { .. } | Event::Approval(_) | Event::Output { .. } => "runs".into(),
            Event::Service(_) => "services".into(),
            Event::Git { .. } => "git".into(),
            Event::Projects | Event::Config(_) => "config".into(),
            Event::Log(c) => format!("log:{}", c.run),
            Event::ServiceLog { project, service, .. } => format!("service-log:{project}/{service}"),
        }
    }
}

// ── data ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub id: String,
    pub path: String,
    pub remote: Option<String>,
    pub sensitive: bool,
    pub disabled: Option<String>,
    pub busy_run: Option<i64>,
    pub git: Option<GitStatus>,
    pub environments: Vec<EnvInfo>,
    pub services: Vec<ServiceStatus>,
    pub groups: Vec<GroupInfo>,
    pub pipelines: Vec<PipelineInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvInfo {
    pub name: String,
    pub protected: bool,
    pub var_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupInfo {
    pub id: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputInfo {
    pub name: String,
    pub kind: InputKind,
    pub pattern: Option<String>,
    pub options: Vec<String>,
    pub default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepOutline {
    pub kind: StepKindName,
    pub label: String,
    /// Command, approval message, or `start <target>`.
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Stopped,
    /// Waiting for dependencies to become healthy.
    Waiting,
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
            ServiceState::Waiting => "waiting",
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
            "waiting" => ServiceState::Waiting,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Short explanation: "needs backend", "crashed 5× in 5m", "port 5432 is in use by PID 77 (not managed)".
    pub message: Option<String>,
    pub depends_on: Vec<String>,
    pub restart: RestartPolicy,
    pub has_health_check: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    AwaitingApproval,
    Succeeded,
    Failed,
    Cancelled,
    Rejected,
}

impl RunStatus {
    pub const ALL: [RunStatus; 6] = [
        RunStatus::Running,
        RunStatus::AwaitingApproval,
        RunStatus::Succeeded,
        RunStatus::Failed,
        RunStatus::Cancelled,
        RunStatus::Rejected,
    ];
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
            "awaiting_approval" | "waiting" | "wait" => RunStatus::AwaitingApproval,
            "succeeded" | "ok" => RunStatus::Succeeded,
            "failed" | "fail" => RunStatus::Failed,
            "cancelled" | "canceled" => RunStatus::Cancelled,
            "rejected" => RunStatus::Rejected,
            _ => return None,
        })
    }
    pub fn is_active(self) -> bool {
        matches!(self, RunStatus::Running | RunStatus::AwaitingApproval)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub has_log: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalInfo {
    pub run_id: i64,
    pub step_index: usize,
    pub project: String,
    pub pipeline: String,
    pub env: Option<String>,
    pub protected: bool,
    pub message: String,
    pub status: ApprovalStatus,
    pub requested_at: Timestamp,
    pub decided_at: Option<Timestamp>,
    pub expires_at: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunDetail {
    pub summary: RunSummary,
    pub inputs: BTreeMap<String, String>,
    pub steps: Vec<StepInfo>,
    pub approvals: Vec<ApprovalInfo>,
    pub outputs: Vec<OutputInfo>,
    pub diff_path: Option<String>,
    pub diff_summary: Option<String>,
    pub unpushed_count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub key: String,
    pub value: String,
    pub step_index: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunPage {
    pub runs: Vec<RunSummary>,
    pub total: u64,
    pub page: u32,
    pub page_size: u32,
    /// Approvals currently waiting, across all projects.
    pub waiting: Vec<ApprovalInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogChunk {
    pub run: i64,
    pub step: usize,
    pub offset: u64,
    /// Masked output. Raw bytes are decoded lossily as UTF-8.
    pub data: String,
    /// Offset just past this chunk.
    pub end: u64,
    /// True when the step has finished and no more output will arrive.
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DryRunPlan {
    pub project: String,
    pub pipeline: String,
    pub env: Option<String>,
    pub protected: bool,
    pub inputs: BTreeMap<String, String>,
    pub steps: Vec<DryRunStep>,
    /// Problems that would stop a real run (repository guard, gitignore, busy project).
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolStatus {
    pub name: String,
    pub path: Option<String>,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reachability {
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthInfo {
    pub tools: Vec<ToolStatus>,
    pub tools_checked_at: Option<Timestamp>,
    pub docker: Option<Reachability>,
    pub tmux: Option<Reachability>,
    pub config: ConfigStatus,
    pub daemon_version: String,
    pub protocol: u32,
    pub started_at: Timestamp,
    pub pid: u32,
    pub state_dir: String,
}

pub fn now() -> Timestamp {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let m = ClientMessage { id: 7, request: Request::RunCancel { id: 412 } };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"id":7,"method":"run.cancel","params":{"id":412}}"#);
        let back: ClientMessage = serde_json::from_str(&s).unwrap();
        assert!(matches!(back.request, Request::RunCancel { id: 412 }));
        let unit: ClientMessage = serde_json::from_str(r#"{"id":1,"method":"projects.list"}"#).unwrap();
        assert!(matches!(unit.request, Request::ProjectsList));
    }

    #[test]
    fn server_messages_are_distinguishable() {
        let r = ServerMessage::Response { id: 3, body: ResponseBody::Result(Response::Ok) };
        let s = serde_json::to_string(&r).unwrap();
        assert!(matches!(serde_json::from_str::<ServerMessage>(&s).unwrap(), ServerMessage::Response { id: 3, .. }));
        let e = ServerMessage::Event { event: Event::Projects };
        let s = serde_json::to_string(&e).unwrap();
        assert!(matches!(serde_json::from_str::<ServerMessage>(&s).unwrap(), ServerMessage::Event { .. }));
        let err = ServerMessage::Response {
            id: 4,
            body: ResponseBody::Error(ErrorBody { code: ErrorCode::Busy, message: "x".into() }),
        };
        let s = serde_json::to_string(&err).unwrap();
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(&s).unwrap(),
            ServerMessage::Response { body: ResponseBody::Error(_), .. }
        ));
    }

    #[test]
    fn filter_parse() {
        let f = RunFilter::parse("project:finance-analyzer env:prod status:failed").unwrap();
        assert_eq!(f.project.as_deref(), Some("finance-analyzer"));
        assert_eq!(f.env.as_deref(), Some("prod"));
        assert_eq!(f.status, Some(RunStatus::Failed));
        assert_eq!(RunFilter::parse(&f.to_text()).unwrap(), f);
        assert!(RunFilter::parse("color:red").is_err());
        assert_eq!(RunFilter::parse("migrate").unwrap().pipeline.as_deref(), Some("migrate"));
    }
}
