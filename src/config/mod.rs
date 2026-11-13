//! `groundcontrol.yaml`: typed configuration, parsing with positions (§7.1)
//! and validation (§7.3).

mod parse;
pub mod validate;

pub use parse::is_env_name;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::yaml::Pos;

pub const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// A validation finding with its 1-based position in the config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub line: usize,
    pub col: usize,
    pub message: String,
}

impl Diagnostic {
    pub fn error(pos: Pos, message: impl Into<String>) -> Self {
        Diagnostic { severity: Severity::Error, line: pos.line, col: pos.col, message: message.into() }
    }
    pub fn warning(pos: Pos, message: impl Into<String>) -> Self {
        Diagnostic { severity: Severity::Warning, line: pos.line, col: pos.col, message: message.into() }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub env: Vec<(String, String)>,
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone)]
pub struct Project {
    pub id: String,
    pub pos: Pos,
    /// `path` exactly as written, for display.
    pub path_display: String,
    /// `path` with `~` expanded.
    pub path: PathBuf,
    pub remote: Option<String>,
    pub environments: Vec<Environment>,
    pub services: Vec<Service>,
    pub groups: Vec<Group>,
    pub pipelines: Vec<Pipeline>,
    /// Set when the project can't be used (for example its path is missing).
    pub disabled: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Environment {
    pub name: String,
    pub protected: bool,
    pub vars: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    OnFailure,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Url(String),
    Port(u16),
}

impl Health {
    /// The TCP port the check uses, if it can be determined.
    pub fn port(&self) -> Option<u16> {
        match self {
            Health::Port(p) => Some(*p),
            Health::Url(u) => parse_http_url(u).map(|(_, port, _)| port),
        }
    }
}

/// Splits `http://host:port/path` into its parts. Only `http` is supported.
pub fn parse_http_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port, path))
}

#[derive(Debug, Clone)]
pub struct Service {
    pub id: String,
    pub pos: Pos,
    pub start: String,
    pub cwd: Option<String>,
    pub env_file: Option<String>,
    pub health: Option<Health>,
    pub depends_on: Vec<String>,
    pub restart: RestartPolicy,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub id: String,
    pub pos: Pos,
    pub members: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub id: String,
    pub pos: Pos,
    pub protected: bool,
    pub environments: Vec<String>,
    pub inputs: Vec<Input>,
    pub steps: Vec<Step>,
    /// Destructive-action warning shown in the run dialog (GroundControl extension).
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    String,
    Bool,
    Choice,
}

#[derive(Debug, Clone)]
pub struct Input {
    pub name: String,
    pub pos: Pos,
    pub kind: InputKind,
    pub pattern: Option<String>,
    pub options: Vec<String>,
    pub default: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
}

impl ServiceAction {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceAction::Start => "start",
            ServiceAction::Stop => "stop",
            ServiceAction::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone)]
pub enum StepKind {
    Run { run: String, cwd: Option<String>, env_file: Option<String>, timeout: Duration },
    Approve { message: String },
    Service { action: ServiceAction, target: String },
}

#[derive(Debug, Clone)]
pub struct Step {
    pub name: Option<String>,
    pub pos: Pos,
    pub kind: StepKind,
}

impl Step {
    /// Label shown in the TUI: the step name, or a short form of the step.
    pub fn label(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        match &self.kind {
            StepKind::Run { run, .. } => run.lines().next().unwrap_or("").trim().to_string(),
            StepKind::Approve { .. } => "approve".into(),
            StepKind::Service { action, target } => format!("{} {}", action.as_str(), target),
        }
    }

    pub fn kind_str(&self) -> &'static str {
        match self.kind {
            StepKind::Run { .. } => "run",
            StepKind::Approve { .. } => "approve",
            StepKind::Service { .. } => "service",
        }
    }
}

impl Config {
    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }
}

impl Project {
    pub fn service(&self, id: &str) -> Option<&Service> {
        self.services.iter().find(|s| s.id == id)
    }
    pub fn group(&self, id: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }
    pub fn pipeline(&self, id: &str) -> Option<&Pipeline> {
        self.pipelines.iter().find(|p| p.id == id)
    }
    pub fn environment(&self, name: &str) -> Option<&Environment> {
        self.environments.iter().find(|e| e.name == name)
    }
    pub fn resolve_dir(&self, cwd: Option<&str>) -> PathBuf {
        match cwd {
            Some(c) => self.path.join(c),
            None => self.path.clone(),
        }
    }
}

impl Pipeline {
    /// Whether running this pipeline against `env` needs the protected flow.
    pub fn is_protected(&self, project: &Project, env: Option<&str>) -> bool {
        self.protected || env.and_then(|e| project.environment(e)).is_some_and(|e| e.protected)
    }
}

/// Result of loading the config file.
#[derive(Debug, Clone)]
pub struct LoadResult {
    /// `None` when any error-level diagnostic was found.
    pub config: Option<Config>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn load_str(src: &str) -> LoadResult {
    let mut diags = Vec::new();
    let config = match crate::yaml::parse(src) {
        Ok(root) => parse::config_from_node(&root, &mut diags),
        Err(errs) => {
            diags.extend(errs.into_iter().map(|e| Diagnostic::error(e.pos, format!("YAML: {}", e.message))));
            None
        }
    };
    let config = config.map(|mut c| {
        validate::validate(&mut c, &mut diags);
        c
    });
    diags.sort_by_key(|d| (d.line, d.col));
    let has_error = diags.iter().any(|d| d.severity == Severity::Error);
    LoadResult { config: if has_error { None } else { config }, diagnostics: diags }
}

pub fn load_file(path: &Path) -> LoadResult {
    match std::fs::read_to_string(path) {
        Ok(src) => load_str(&src),
        Err(e) => LoadResult {
            config: None,
            diagnostics: vec![Diagnostic::error(Pos { line: 0, col: 0 }, format!("cannot read {}: {e}", path.display()))],
        },
    }
}

/// Parses `15m`, `30s`, `1h` or a bare number of seconds.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    let n: u64 = num.trim().parse().ok()?;
    (n > 0).then(|| Duration::from_secs(n * mult))
}

#[cfg(test)]
mod tests;
