//! Pipeline runner (§6.1–§6.4, §8): preflight safety checks, per-project
//! lock, step execution, approval pauses, outputs, timeouts and cancellation.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use regex::Regex;
use tokio::sync::{oneshot, watch};

use super::process;
use super::store::{NewRun, NewStep};
use super::{Daemon, supervisor, system};
use crate::config::{Config, InputKind, Pipeline, Project, StepKind};
use crate::mask::Masker;
use crate::protocol::*;

pub const APPROVAL_TTL: Duration = Duration::from_secs(30 * 60);

pub type ReqError = (ErrorCode, String);

fn err(code: ErrorCode, msg: impl Into<String>) -> ReqError {
    (code, msg.into())
}

/// Live state of runs this daemon process is executing.
#[derive(Default)]
pub struct Runs {
    /// project id → run id holding its lock.
    busy: HashMap<String, i64>,
    active: HashMap<i64, ActiveRun>,
}

struct ActiveRun {
    cancel: watch::Sender<bool>,
    approval: Option<(usize, oneshot::Sender<Decision>)>,
}

impl Runs {
    pub fn busy_run(&self, project: &str) -> Option<i64> {
        self.busy.get(project).copied()
    }
    pub fn active_count(&self) -> usize {
        self.active.len()
    }
    pub fn active_ids(&self) -> Vec<i64> {
        self.active.keys().copied().collect()
    }
}

/// Validates inputs against the pipeline's declarations, applying defaults.
pub fn resolve_inputs(p: &Pipeline, given: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>, String> {
    for k in given.keys() {
        if !p.inputs.iter().any(|i| &i.name == k) {
            return Err(format!("unknown input `{k}`"));
        }
    }
    let mut out = BTreeMap::new();
    for i in &p.inputs {
        let value = match given.get(&i.name).filter(|v| !v.is_empty()).or(i.default.as_ref()) {
            Some(v) => v.clone(),
            None if i.kind == InputKind::Bool => "false".to_string(),
            None => return Err(format!("input `{}` is required", i.name)),
        };
        match i.kind {
            InputKind::String => {
                let pat = i.pattern.as_deref().unwrap_or("^$");
                let re = Regex::new(pat).map_err(|e| format!("input `{}` pattern is invalid: {e}", i.name))?;
                if !re.is_match(&value) {
                    return Err(format!("input `{}` does not match {pat}", i.name));
                }
            }
            InputKind::Bool => {
                if value != "true" && value != "false" {
                    return Err(format!("input `{}` must be true or false", i.name));
                }
            }
            InputKind::Choice => {
                if !i.options.contains(&value) {
                    return Err(format!("input `{}` must be one of: {}", i.name, i.options.join(", ")));
                }
            }
        }
        out.insert(i.name.clone(), value);
    }
    Ok(out)
}

fn check_env(project: &Project, p: &Pipeline, env: Option<&str>) -> Result<(), String> {
    match (p.environments.is_empty(), env) {
        (true, None) => Ok(()),
        (true, Some(e)) => Err(format!("pipeline `{}` does not run against an environment (got `{e}`)", p.id)),
        (false, None) => Err(format!("pipeline `{}` needs an environment: {}", p.id, p.environments.join(", "))),
        (false, Some(e)) if !p.environments.iter().any(|x| x == e) => {
            Err(format!("pipeline `{}` cannot run on `{e}` (allowed: {})", p.id, p.environments.join(", ")))
        }
        (false, Some(e)) if project.environment(e).is_none() => Err(format!("unknown environment `{e}`")),
        _ => Ok(()),
    }
}

/// Environment for a step, in increasing precedence (§6.1). Returns the
/// variables and the `env_file` values (for masking).
fn step_env(
    config: &Config,
    project: &Project,
    env: Option<&str>,
    env_file: Option<&Path>,
    inputs: &BTreeMap<String, String>,
    outputs: &BTreeMap<String, String>,
    gc: &BTreeMap<String, String>,
) -> Result<(BTreeMap<String, String>, Vec<String>), String> {
    let mut vars: BTreeMap<String, String> = config.env.iter().cloned().collect();
    if let Some(e) = env.and_then(|e| project.environment(e)) {
        vars.extend(e.vars.iter().cloned());
    }
    let mut secrets = Vec::new();
    if let Some(f) = env_file {
        for (k, v) in crate::envfile::load(f)? {
            secrets.push(v.clone());
            vars.insert(k, v);
        }
    }
    for (k, v) in inputs {
        vars.insert(format!("INPUT_{}", k.to_ascii_uppercase()), v.clone());
    }
    for (k, v) in outputs {
        vars.insert(format!("OUTPUT_{}", k.to_ascii_uppercase()), v.clone());
    }
    vars.extend(gc.iter().map(|(k, v)| (k.clone(), v.clone())));
    Ok((vars, secrets))
}

fn gc_vars(run_id: Option<i64>, env: Option<&str>, output_file: &Path) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("GC_RUN_ID".into(), run_id.map(|i| i.to_string()).unwrap_or_else(|| "dry-run".into())),
        ("GC_ENV".into(), env.unwrap_or("").into()),
        ("GC_OUTPUT".into(), output_file.to_string_lossy().into_owned()),
        ("FORCE_COLOR".into(), "1".into()),
        ("CLICOLOR_FORCE".into(), "1".into()),
        ("TERM".into(), "xterm-256color".into()),
        ("COLUMNS".into(), "120".into()),
    ])
}

/// `key=... >> "$GC_OUTPUT"` in a step, to list outputs a dry run can expect.
static OUTPUT_WRITE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"([A-Za-z_][A-Za-z0-9_]*)=[^\n]*>>\s*"?\$\{?GC_OUTPUT"#).unwrap());

static VAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}|\$([A-Za-z_][A-Za-z0-9_]*)").unwrap());

/// Expands `$NAME` / `${NAME}` in an approval message. Unknown names stay as written.
pub fn expand_message(msg: &str, vars: &BTreeMap<String, String>) -> String {
    VAR_RE
        .replace_all(msg, |c: &regex::Captures| {
            let name = c.get(1).or_else(|| c.get(2)).unwrap().as_str();
            vars.get(name).cloned().unwrap_or_else(|| c[0].to_string())
        })
        .into_owned()
}

/// Parses `key=value` lines appended to `$GC_OUTPUT`.
pub fn parse_outputs(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            let k = k.trim();
            crate::config::is_env_name(k).then(|| (k.to_string(), v.trim_end_matches('\r').to_string()))
        })
        .collect()
}

/// Checks that would refuse a real run (§8.4, §7.2 log location, §6.2).
async fn preflight(d: &Daemon, project: &Project) -> Vec<String> {
    let mut blockers = Vec::new();
    if let Some(reason) = &project.disabled {
        blockers.push(format!("project is disabled: {reason}"));
        return blockers;
    }
    if let Err(e) = crate::git::check_remote(&project.path, project.remote.as_deref()).await {
        blockers.push(e);
    }
    if project.sensitive && !crate::git::state_dir_ignored(&project.path).await {
        blockers.push(format!(
            "{} is sensitive but {}/ is not gitignored; add it to .gitignore before running",
            project.id,
            crate::paths::REPO_STATE_DIR
        ));
    }
    if let Some(id) = d.runs.lock().unwrap().busy_run(&project.id) {
        blockers.push(format!("{} already has run #{id} in progress", project.id));
    }
    blockers
}

pub async fn start(d: &Arc<Daemon>, params: RunStartParams) -> Result<Response, ReqError> {
    let config = d.config();
    let project =
        config.project(&params.project).ok_or_else(|| err(ErrorCode::NotFound, format!("unknown project `{}`", params.project)))?;
    let pipeline = project
        .pipeline(&params.pipeline)
        .ok_or_else(|| err(ErrorCode::NotFound, format!("unknown pipeline `{}` in {}", params.pipeline, project.id)))?;
    let env = params.env.as_deref().filter(|e| !e.is_empty());
    check_env(project, pipeline, env).map_err(|e| err(ErrorCode::BadRequest, e))?;
    let inputs = resolve_inputs(pipeline, &params.inputs).map_err(|e| err(ErrorCode::BadRequest, e))?;
    let protected = pipeline.is_protected(project, env);

    if params.dry_run {
        return Ok(Response::DryRun(dry_run(d, &config, project, pipeline, env, inputs, protected).await));
    }
    if protected && !params.confirmed {
        return Err(err(ErrorCode::Refused, "protected run needs the confirmation dialog"));
    }
    let blockers = preflight(d, project).await;
    if !blockers.is_empty() {
        return Err(err(ErrorCode::Refused, blockers.join("; ")));
    }
    // Take the project lock (rejected, never queued).
    let (cancel_tx, cancel_rx) = watch::channel(false);
    {
        let mut runs = d.runs.lock().unwrap();
        if let Some(id) = runs.busy_run(&project.id) {
            return Err(err(ErrorCode::Busy, format!("{} already has run #{id} in progress", project.id)));
        }
        runs.busy.insert(project.id.clone(), -1);
    }
    let release_placeholder = |d: &Daemon| {
        d.runs.lock().unwrap().busy.remove(&project.id);
    };

    let snap = crate::git::snapshot(&project.path).await;
    let unpushed = snap.unpushed.is_some_and(|n| n > 0);
    let steps: Vec<NewStep> = pipeline
        .steps
        .iter()
        .map(|s| {
            let (command, cwd) = match &s.kind {
                StepKind::Run { run, cwd, .. } => (run.clone(), cwd.clone()),
                StepKind::Approve { message } => (message.clone(), None),
                StepKind::Service { action, target } => (format!("{} {}", action.as_str(), target), None),
            };
            NewStep { name: s.label(), kind: StepKindName::parse(s.kind_str()), command, cwd }
        })
        .collect();
    let new = NewRun {
        project: &project.id,
        pipeline: &pipeline.id,
        env,
        inputs: &inputs,
        protected,
        branch: snap.branch.as_deref(),
        commit: snap.commit.as_deref(),
        dirty: snap.dirty,
        unpushed,
        unpushed_count: snap.unpushed,
    };
    let run_id = match d.with_store(|db| db.insert_run(&new, &steps, now())) {
        Some(id) => id,
        None => {
            release_placeholder(d);
            return Err(err(ErrorCode::Internal, "could not record the run"));
        }
    };
    {
        let mut runs = d.runs.lock().unwrap();
        runs.busy.insert(project.id.clone(), run_id);
        runs.active.insert(run_id, ActiveRun { cancel: cancel_tx, approval: None });
    }
    if snap.dirty {
        let diff = crate::git::capture_diff(&project.path).await;
        let path = crate::paths::diff_path(&project.data_root(), run_id);
        let s = &snap.status;
        let summary = format!(
            "{} file{} changed, {} untracked",
            s.staged + s.modified + s.conflicted,
            if s.staged + s.modified + s.conflicted == 1 { "" } else { "s" },
            s.untracked
        );
        let written = path.parent().map(std::fs::create_dir_all).transpose().and_then(|_| std::fs::write(&path, diff));
        match written {
            Ok(()) => {
                d.with_store(|db| db.set_run_diff(run_id, &path.to_string_lossy(), &summary));
            }
            Err(e) => tracing::warn!(run_id, "could not save diff: {e}"),
        }
    }
    if let Some(s) = d.with_store(|db| db.run_summary(run_id)).flatten() {
        d.emit(Event::Run(s));
    }
    let (d2, config2) = (d.clone(), config.clone());
    let (pid, plid, env) = (project.id.clone(), pipeline.id.clone(), env.map(str::to_string));
    tokio::spawn(async move {
        execute(&d2, &config2, &pid, &plid, run_id, env, inputs, protected, cancel_rx).await;
    });
    Ok(Response::RunStarted { id: run_id })
}

async fn dry_run(
    d: &Daemon,
    config: &Config,
    project: &Project,
    pipeline: &Pipeline,
    env: Option<&str>,
    inputs: BTreeMap<String, String>,
    protected: bool,
) -> DryRunPlan {
    let blockers = preflight(d, project).await;
    let out_file = PathBuf::from("$GC_OUTPUT");
    let mut steps = Vec::new();
    let mut known_outputs: Vec<String> = Vec::new();
    for (i, s) in pipeline.steps.iter().enumerate() {
        let gc = gc_vars(None, env, &out_file);
        let (command, cwd, env_names, timeout) = match &s.kind {
            StepKind::Run { run, cwd, env_file, timeout } => {
                let dir = project.resolve_dir(cwd.as_deref());
                let mut names: Vec<String> = config.env.iter().map(|(k, _)| k.clone()).collect();
                if let Some(e) = env.and_then(|e| project.environment(e)) {
                    names.extend(e.vars.iter().map(|(k, _)| k.clone()));
                }
                if let Some(f) = env_file {
                    match crate::envfile::load(&dir.join(f)) {
                        Ok(vars) => names.extend(vars.into_iter().map(|(k, _)| format!("{k} (env_file)"))),
                        Err(e) => names.push(format!("<env_file error: {e}>")),
                    }
                }
                names.extend(inputs.keys().map(|k| format!("INPUT_{}", k.to_ascii_uppercase())));
                names.extend(known_outputs.iter().map(|k| format!("OUTPUT_{k} (if set)")));
                names.extend(gc.keys().cloned());
                names.dedup();
                (run.clone(), dir.display().to_string(), names, Some(timeout.as_secs()))
            }
            StepKind::Approve { message } => {
                let mut vars: BTreeMap<String, String> = config.env.iter().cloned().collect();
                vars.extend(inputs.iter().map(|(k, v)| (format!("INPUT_{}", k.to_ascii_uppercase()), v.clone())));
                vars.insert("GC_ENV".into(), env.unwrap_or("").into());
                (format!("pause for approval: {}", expand_message(message, &vars)), String::new(), Vec::new(), Some(APPROVAL_TTL.as_secs()))
            }
            StepKind::Service { action, target } => (format!("{} {target}", action.as_str()), String::new(), Vec::new(), None),
        };
        if let StepKind::Run { run, .. } = &s.kind {
            for cap in OUTPUT_WRITE_RE.captures_iter(run) {
                known_outputs.push(cap[1].to_ascii_uppercase());
            }
        }
        steps.push(DryRunStep {
            index: i,
            kind: StepKindName::parse(s.kind_str()),
            label: s.label(),
            command,
            cwd,
            env_names,
            timeout_secs: timeout,
        });
    }
    DryRunPlan {
        project: project.id.clone(),
        pipeline: pipeline.id.clone(),
        env: env.map(str::to_string),
        protected,
        inputs,
        steps,
        blockers,
    }
}

enum StepOutcome {
    Ok,
    Failed(Option<String>),
    Rejected,
    Cancelled(Option<String>),
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    d: &Arc<Daemon>,
    config: &Config,
    project_id: &str,
    pipeline_id: &str,
    run_id: i64,
    env: Option<String>,
    inputs: BTreeMap<String, String>,
    protected: bool,
    mut cancel: watch::Receiver<bool>,
) {
    let project = config.project(project_id).expect("checked at start");
    let pipeline = project.pipeline(pipeline_id).expect("checked at start");
    let tmp = crate::paths::run_tmp_dir(run_id);
    let out_file = tmp.join("outputs");
    let prepared = std::fs::create_dir_all(&tmp).and_then(|_| std::fs::write(&out_file, ""));
    let mut outputs: BTreeMap<String, String> = BTreeMap::new();
    let mut outputs_seen = 0usize;
    let mut final_status = RunStatus::Succeeded;
    let mut reason: Option<String> = None;
    if let Err(e) = prepared {
        final_status = RunStatus::Failed;
        reason = Some(format!("cannot prepare run folder: {e}"));
    }
    let gc = gc_vars(Some(run_id), env.as_deref(), &out_file);
    let mut all_secrets: Vec<String> = Vec::new();

    for (idx, step) in pipeline.steps.iter().enumerate() {
        if final_status != RunStatus::Succeeded {
            break;
        }
        if *cancel.borrow() {
            final_status = RunStatus::Cancelled;
            break;
        }
        let outcome = match &step.kind {
            StepKind::Run { run, cwd, env_file, timeout } => {
                let dir = project.resolve_dir(cwd.as_deref());
                let env_file_path = env_file.as_ref().map(|f| dir.join(f));
                match step_env(config, project, env.as_deref(), env_file_path.as_deref(), &inputs, &outputs, &gc) {
                    Err(e) => {
                        fail_step_early(d, project, run_id, idx, &e);
                        StepOutcome::Failed(Some(e))
                    }
                    Ok((vars, secrets)) => {
                        all_secrets.extend(secrets.iter().cloned());
                        run_step(d, project, run_id, idx, run, &dir, &vars, Masker::new(secrets), *timeout, &mut cancel).await
                    }
                }
            }
            StepKind::Approve { message } => {
                let mut vars: BTreeMap<String, String> = config.env.iter().cloned().collect();
                if let Some(e) = env.as_deref().and_then(|e| project.environment(e)) {
                    vars.extend(e.vars.iter().cloned());
                }
                vars.extend(inputs.iter().map(|(k, v)| (format!("INPUT_{}", k.to_ascii_uppercase()), v.clone())));
                vars.extend(outputs.iter().map(|(k, v)| (format!("OUTPUT_{}", k.to_ascii_uppercase()), v.clone())));
                vars.extend(gc.iter().map(|(k, v)| (k.clone(), v.clone())));
                let text = expand_message(message, &vars);
                approve_step(d, project, pipeline, run_id, idx, &text, env.as_deref(), protected, &mut cancel).await
            }
            StepKind::Service { action, target } => service_step(d, project, run_id, idx, *action, target).await,
        };

        // Collect outputs appended during this step (§6.4).
        if let Ok(text) = std::fs::read_to_string(&out_file) {
            let lines = parse_outputs(&text);
            let masker = Masker::new(&all_secrets);
            for (k, v) in lines.into_iter().skip(outputs_seen) {
                outputs_seen += 1;
                let v = String::from_utf8_lossy(&masker.mask_all(v.as_bytes())).into_owned();
                outputs.insert(k.clone(), v.clone());
                d.with_store(|db| db.set_output(run_id, &k, &v, idx));
                d.emit(Event::Output { run_id, key: k, value: v });
            }
        }

        match outcome {
            StepOutcome::Ok => {}
            StepOutcome::Failed(r) => {
                final_status = RunStatus::Failed;
                reason = r;
            }
            StepOutcome::Rejected => {
                final_status = RunStatus::Rejected;
            }
            StepOutcome::Cancelled(r) => {
                final_status = RunStatus::Cancelled;
                reason = r;
            }
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);
    d.with_store(|db| db.skip_pending_steps(run_id));
    d.with_store(|db| db.set_run_status(run_id, final_status, reason.as_deref(), Some(now())));
    {
        let mut runs = d.runs.lock().unwrap();
        runs.active.remove(&run_id);
        if runs.busy.get(project_id) == Some(&run_id) {
            runs.busy.remove(project_id);
        }
    }
    if let Some(detail) = d.with_store(|db| db.run_detail(run_id)).flatten() {
        for s in detail.steps {
            d.emit(Event::Step { run_id, step: s });
        }
        d.emit(Event::Run(detail.summary));
    }
    d.request_git_refresh(Some(project_id.to_string()));
    if final_status == RunStatus::Failed {
        let body = format!("#{run_id} failed{}", reason.map(|r| format!(": {r}")).unwrap_or_default());
        system::notify(&format!("{project_id}/{pipeline_id}"), &body).await;
    }
}

fn emit_step(d: &Daemon, run_id: i64, idx: usize) {
    if let Some(step) = d.with_store(|db| db.step(run_id, idx)).flatten() {
        d.emit(Event::Step { run_id, step });
    }
}

fn emit_run(d: &Daemon, run_id: i64) {
    if let Some(s) = d.with_store(|db| db.run_summary(run_id)).flatten() {
        d.emit(Event::Run(s));
    }
}

/// Opens the step's log file and returns it with its path.
fn open_step_log(project: &Project, run_id: i64, idx: usize) -> std::io::Result<(std::fs::File, PathBuf)> {
    let path = crate::paths::step_log_path(&project.data_root(), run_id, idx);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    Ok((f, path))
}

/// Writes masked bytes to the step log and broadcasts them as a log chunk.
struct StepLog<'a> {
    d: &'a Daemon,
    file: Option<std::fs::File>,
    run_id: i64,
    idx: usize,
    /// Bytes already announced to clients.
    offset: u64,
    /// Trailing bytes of an incomplete UTF-8 sequence, not yet announced.
    pending: Vec<u8>,
}

impl StepLog<'_> {
    fn new<'a>(d: &'a Daemon, file: Option<std::fs::File>, run_id: i64, idx: usize) -> StepLog<'a> {
        StepLog { d, file, run_id, idx, offset: 0, pending: Vec::new() }
    }

    fn write(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if let Some(f) = &mut self.file {
            let _ = f.write_all(data);
        }
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(data);
        let valid = match std::str::from_utf8(&buf) {
            Ok(_) => buf.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => buf.len(),
        };
        self.pending = buf.split_off(valid);
        self.announce(&buf);
    }

    fn announce(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let start = self.offset;
        self.offset += bytes.len() as u64;
        self.d.emit(Event::Log(LogChunk {
            run: self.run_id,
            step: self.idx,
            offset: start,
            data: String::from_utf8_lossy(bytes).into_owned(),
            end: self.offset,
            complete: false,
        }));
    }

    fn finish(&mut self) {
        let rest = std::mem::take(&mut self.pending);
        self.announce(&rest);
        if let Some(f) = &mut self.file {
            let _ = f.flush();
        }
        self.d.emit(Event::Log(LogChunk {
            run: self.run_id,
            step: self.idx,
            offset: self.offset,
            data: String::new(),
            end: self.offset,
            complete: true,
        }));
    }
}

fn fail_step_early(d: &Daemon, project: &Project, run_id: i64, idx: usize, msg: &str) {
    let log = open_step_log(project, run_id, idx).ok();
    let path = log.as_ref().map(|(_, p)| p.to_string_lossy().into_owned());
    d.with_store(|db| db.start_step(run_id, idx, now(), StepStatus::Running, path.as_deref()));
    let mut sl = StepLog::new(d, log.map(|(f, _)| f), run_id, idx);
    sl.write(format!("groundcontrol: {msg}\n").as_bytes());
    sl.finish();
    d.with_store(|db| db.finish_step(run_id, idx, StepStatus::Failed, None, now()));
    emit_step(d, run_id, idx);
}

#[allow(clippy::too_many_arguments)]
async fn run_step(
    d: &Arc<Daemon>,
    project: &Project,
    run_id: i64,
    idx: usize,
    command: &str,
    dir: &Path,
    vars: &BTreeMap<String, String>,
    mut masker: Masker,
    timeout: Duration,
    cancel: &mut watch::Receiver<bool>,
) -> StepOutcome {
    if !dir.is_dir() {
        let msg = format!("working directory {} does not exist", dir.display());
        fail_step_early(d, project, run_id, idx, &msg);
        return StepOutcome::Failed(Some(msg));
    }
    let (file, path) = match open_step_log(project, run_id, idx) {
        Ok(x) => x,
        Err(e) => {
            let msg = format!("cannot open step log: {e}");
            d.with_store(|db| db.finish_step(run_id, idx, StepStatus::Failed, None, now()));
            emit_step(d, run_id, idx);
            return StepOutcome::Failed(Some(msg));
        }
    };
    d.with_store(|db| db.start_step(run_id, idx, now(), StepStatus::Running, Some(&path.to_string_lossy())));
    emit_step(d, run_id, idx);
    let mut log = StepLog::new(d, Some(file), run_id, idx);

    let mut spawned = match process::spawn_shell(command, dir, vars) {
        Ok(s) => s,
        Err(e) => {
            log.write(format!("groundcontrol: cannot start step: {e}\n").as_bytes());
            log.finish();
            d.with_store(|db| db.finish_step(run_id, idx, StepStatus::Failed, None, now()));
            emit_step(d, run_id, idx);
            return StepOutcome::Failed(Some(format!("cannot start step: {e}")));
        }
    };
    let pgid = spawned.pid;
    let deadline = tokio::time::Instant::now() + timeout;
    enum End {
        Exited(Option<i32>),
        TimedOut,
        Cancelled,
    }
    let mut output_open = true;
    let end = loop {
        tokio::select! {
            chunk = spawned.output.recv(), if output_open => match chunk {
                Some(c) => log.write(&masker.push(&c)),
                None => output_open = false,
            },
            status = spawned.child.wait() => break End::Exited(status.ok().and_then(|s| s.code())),
            _ = tokio::time::sleep_until(deadline) => break End::TimedOut,
            _ = cancel.wait_for(|c| *c) => break End::Cancelled,
        }
    };
    let code = match end {
        End::Exited(c) => c,
        End::TimedOut | End::Cancelled => {
            let what = if matches!(end, End::TimedOut) { "timed out; stopping" } else { "cancel requested; stopping" };
            log.write(&masker.push(format!("\ngroundcontrol: {what} (SIGTERM, then SIGKILL after 10s)\n").as_bytes()));
            process::terminate(&mut spawned.child, pgid).await
        }
    };
    // Drain what is left; something the step left running may hold the pipe open.
    let drain_until = tokio::time::Instant::now() + Duration::from_secs(2);
    while output_open {
        match tokio::time::timeout_at(drain_until, spawned.output.recv()).await {
            Ok(Some(c)) => log.write(&masker.push(&c)),
            _ => output_open = false,
        }
    }
    let rest = masker.flush();
    log.write(&rest);
    log.finish();

    let (status, outcome) = match end {
        End::TimedOut => {
            (StepStatus::TimedOut, StepOutcome::Failed(Some(format!("step {} timed out after {}", idx + 1, fmt_secs(timeout.as_secs())))))
        }
        End::Cancelled => (StepStatus::Cancelled, StepOutcome::Cancelled(Some("cancelled".into()))),
        End::Exited(Some(0)) => (StepStatus::Succeeded, StepOutcome::Ok),
        End::Exited(c) => (
            StepStatus::Failed,
            StepOutcome::Failed(Some(match c {
                Some(c) => format!("step {} exited with code {c}", idx + 1),
                None => format!("step {} was killed by a signal", idx + 1),
            })),
        ),
    };
    d.with_store(|db| db.finish_step(run_id, idx, status, code, now()));
    emit_step(d, run_id, idx);
    outcome
}

fn fmt_secs(s: u64) -> String {
    if s.is_multiple_of(3600) {
        format!("{}h", s / 3600)
    } else if s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

#[allow(clippy::too_many_arguments)]
async fn approve_step(
    d: &Arc<Daemon>,
    project: &Project,
    pipeline: &Pipeline,
    run_id: i64,
    idx: usize,
    message: &str,
    env: Option<&str>,
    _protected: bool,
    cancel: &mut watch::Receiver<bool>,
) -> StepOutcome {
    let requested = now();
    let expires = requested + APPROVAL_TTL.as_secs() as i64;
    let (tx, rx) = oneshot::channel();
    if let Some(a) = d.runs.lock().unwrap().active.get_mut(&run_id) {
        a.approval = Some((idx, tx));
    }
    d.with_store(|db| db.insert_approval(run_id, idx, message, requested, expires));
    d.with_store(|db| db.start_step(run_id, idx, requested, StepStatus::Waiting, None));
    d.with_store(|db| db.set_run_status(run_id, RunStatus::AwaitingApproval, None, None));
    emit_step(d, run_id, idx);
    emit_run(d, run_id);
    if let Some(a) = d.with_store(|db| db.approval(run_id, idx)).flatten() {
        d.emit(Event::Approval(a));
    }
    let title = format!("Approval waiting: {}/{}{}", project.id, pipeline.id, env.map(|e| format!(" on {e}")).unwrap_or_default());
    system::notify(&title, message).await;

    let decision = tokio::select! {
        r = rx => r.ok(),
        _ = tokio::time::sleep(APPROVAL_TTL) => None,
        _ = cancel.wait_for(|c| *c) => None,
    };
    if let Some(a) = d.runs.lock().unwrap().active.get_mut(&run_id) {
        a.approval = None;
    }
    let cancelled = *cancel.borrow();
    let at = now();
    let outcome = match decision {
        Some(Decision::Approve) => {
            d.with_store(|db| db.finish_step(run_id, idx, StepStatus::Succeeded, None, at));
            d.with_store(|db| db.set_run_status(run_id, RunStatus::Running, None, None));
            StepOutcome::Ok
        }
        Some(Decision::Reject) => {
            d.with_store(|db| db.finish_step(run_id, idx, StepStatus::Rejected, None, at));
            StepOutcome::Rejected
        }
        None => {
            // Expired or cancelled: record it unless someone answered at the same moment.
            let status = if cancelled { ApprovalStatus::Cancelled } else { ApprovalStatus::Expired };
            match d.with_store(|db| db.decide_approval(run_id, idx, status, at)) {
                Some(Err(ApprovalStatus::Approved)) => {
                    d.with_store(|db| db.finish_step(run_id, idx, StepStatus::Succeeded, None, at));
                    d.with_store(|db| db.set_run_status(run_id, RunStatus::Running, None, None));
                    StepOutcome::Ok
                }
                Some(Err(ApprovalStatus::Rejected)) => {
                    d.with_store(|db| db.finish_step(run_id, idx, StepStatus::Rejected, None, at));
                    StepOutcome::Rejected
                }
                _ => {
                    d.with_store(|db| db.finish_step(run_id, idx, StepStatus::Cancelled, None, at));
                    StepOutcome::Cancelled(Some(if cancelled { "cancelled".into() } else { "approval expired".into() }))
                }
            }
        }
    };
    emit_step(d, run_id, idx);
    emit_run(d, run_id);
    if let Some(a) = d.with_store(|db| db.approval(run_id, idx)).flatten() {
        d.emit(Event::Approval(a));
    }
    outcome
}

async fn service_step(
    d: &Arc<Daemon>,
    project: &Project,
    run_id: i64,
    idx: usize,
    action: crate::config::ServiceAction,
    target: &str,
) -> StepOutcome {
    let log = open_step_log(project, run_id, idx).ok();
    let path = log.as_ref().map(|(_, p)| p.to_string_lossy().into_owned());
    d.with_store(|db| db.start_step(run_id, idx, now(), StepStatus::Running, path.as_deref()));
    emit_step(d, run_id, idx);
    let mut sl = StepLog::new(d, log.map(|(f, _)| f), run_id, idx);
    sl.write(format!("groundcontrol: {} {target}…\n", action.as_str()).as_bytes());
    let result = supervisor::control(d, &project.id, target, action).await;
    let (status, outcome) = match result {
        Ok(msg) => {
            sl.write(format!("groundcontrol: {msg}\n").as_bytes());
            (StepStatus::Succeeded, StepOutcome::Ok)
        }
        Err(e) => {
            sl.write(format!("groundcontrol: {e}\n").as_bytes());
            (StepStatus::Failed, StepOutcome::Failed(Some(e)))
        }
    };
    sl.finish();
    d.with_store(|db| db.finish_step(run_id, idx, status, None, now()));
    emit_step(d, run_id, idx);
    outcome
}

pub fn cancel(d: &Daemon, id: i64) -> Result<Response, ReqError> {
    let runs = d.runs.lock().unwrap();
    match runs.active.get(&id) {
        Some(a) => {
            let _ = a.cancel.send(true);
            Ok(Response::Ok)
        }
        None => Err(err(ErrorCode::NotFound, format!("run #{id} is not active"))),
    }
}

pub fn decide(d: &Daemon, id: i64, step: usize, decision: Decision, confirmed: bool) -> Result<Response, ReqError> {
    let approval = d
        .with_store(|db| db.approval(id, step))
        .flatten()
        .ok_or_else(|| err(ErrorCode::NotFound, format!("run #{id} has no approval at step {}", step + 1)))?;
    if decision == Decision::Approve && approval.protected && !confirmed {
        return Err(err(ErrorCode::Refused, "approving a protected run needs the confirm keypress"));
    }
    let status = match decision {
        Decision::Approve => ApprovalStatus::Approved,
        Decision::Reject => ApprovalStatus::Rejected,
    };
    match d.with_store(|db| db.decide_approval(id, step, status, now())) {
        Some(Ok(())) => {}
        Some(Err(existing)) => {
            return Err(err(ErrorCode::AlreadyDecided, format!("already decided: {}", existing.as_str())));
        }
        None => return Err(err(ErrorCode::Internal, "could not record the decision")),
    }
    let sender = {
        let mut runs = d.runs.lock().unwrap();
        runs.active.get_mut(&id).and_then(|a| match a.approval.take() {
            Some((s, tx)) if s == step => Some(tx),
            other => {
                a.approval = other;
                None
            }
        })
    };
    match sender {
        Some(tx) => {
            let _ = tx.send(decision);
            Ok(Response::Ok)
        }
        // The run stopped waiting at the same moment; it reads the recorded decision.
        None => Ok(Response::Ok),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipeline() -> (Config, String) {
        let src = r#"
projects:
  p:
    path: /tmp
    pipelines:
      x:
        inputs:
          branch: { type: string, pattern: "^[a-z]+$" }
          kind: { type: choice, options: [a, b], default: a }
          force: { type: bool }
        steps:
          - run: "true"
"#;
        (crate::config::load_str(src).config.unwrap(), "x".into())
    }

    #[test]
    fn input_resolution() {
        let (c, id) = pipeline();
        let p = c.project("p").unwrap().pipeline(&id).unwrap();
        let ok = resolve_inputs(p, &BTreeMap::from([("branch".into(), "feat".into())])).unwrap();
        assert_eq!(ok["kind"], "a");
        assert_eq!(ok["force"], "false");
        assert!(resolve_inputs(p, &BTreeMap::new()).unwrap_err().contains("required"));
        assert!(resolve_inputs(p, &BTreeMap::from([("branch".into(), "Feat; rm".into())])).unwrap_err().contains("does not match"));
        assert!(resolve_inputs(p, &BTreeMap::from([("branch".into(), "a".into()), ("kind".into(), "c".into())])).is_err());
        assert!(resolve_inputs(p, &BTreeMap::from([("branch".into(), "a".into()), ("nope".into(), "c".into())])).is_err());
    }

    #[test]
    fn message_expansion() {
        let vars = BTreeMap::from([("GC_ENV".to_string(), "prod".to_string()), ("INPUT_PR".to_string(), "42".to_string())]);
        assert_eq!(expand_message("Merge PR #$INPUT_PR on ${GC_ENV}? $UNKNOWN", &vars), "Merge PR #42 on prod? $UNKNOWN");
    }

    #[test]
    fn outputs_parse() {
        assert_eq!(
            parse_outputs("d1_bookmark=0000-abc\nnot a line\nx=a=b\n"),
            vec![("d1_bookmark".to_string(), "0000-abc".to_string()), ("x".to_string(), "a=b".to_string())]
        );
    }
}
