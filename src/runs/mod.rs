//! Pipeline runs without a daemon (docs/redesign-daemonless.md, "Pipeline
//! runs"). `start` validates and records a run, then opens the tmux window
//! `gc-<project>:run-<id>`, whose `groundcontrol run-exec <id>` worker
//! executes it. Approvals and cancel go through the store, which the worker
//! polls. Also: inputs, environments, step env, outputs and dry runs (§6.1–§6.4, §8).

pub mod worker;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use crate::config::{Config, InputKind, Pipeline, Project, StepKind};
use crate::ctx::Ctx;
use crate::protocol::*;
use crate::store::{NewRun, NewStep};
use crate::tmux::NewWindow;

pub const APPROVAL_TTL: Duration = Duration::from_secs(30 * 60);
/// A run whose worker has not taken the project lock this long after the run
/// was recorded, and whose window is gone, is treated as interrupted.
const WORKER_START_GRACE_SECS: Timestamp = 15;

pub type ReqError = (ErrorCode, String);

pub fn err(code: ErrorCode, msg: impl Into<String>) -> ReqError {
    (code, msg.into())
}

/// Window name for a run. Services use `svc-<id>`.
pub fn window_name(run_id: i64) -> String {
    format!("run-{run_id}")
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

pub fn check_env(project: &Project, p: &Pipeline, env: Option<&str>) -> Result<(), String> {
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
pub fn step_env(
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

pub fn gc_vars(run_id: Option<i64>, env: Option<&str>, output_file: &Path) -> BTreeMap<String, String> {
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

/// Variables an approval message can refer to.
pub fn approval_vars(
    config: &Config,
    project: &Project,
    env: Option<&str>,
    inputs: &BTreeMap<String, String>,
    outputs: &BTreeMap<String, String>,
    gc: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut vars: BTreeMap<String, String> = config.env.iter().cloned().collect();
    if let Some(e) = env.and_then(|e| project.environment(e)) {
        vars.extend(e.vars.iter().cloned());
    }
    vars.extend(inputs.iter().map(|(k, v)| (format!("INPUT_{}", k.to_ascii_uppercase()), v.clone())));
    vars.extend(outputs.iter().map(|(k, v)| (format!("OUTPUT_{}", k.to_ascii_uppercase()), v.clone())));
    vars.extend(gc.iter().map(|(k, v)| (k.clone(), v.clone())));
    vars
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

pub fn fmt_secs(s: u64) -> String {
    if s.is_multiple_of(3600) {
        format!("{}h", s / 3600)
    } else if s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// Checks that refuse a run whoever executes it (§8.4, §7.2 log location).
pub async fn common_blockers(project: &Project) -> Vec<String> {
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
    blockers
}

/// The rows a run's steps start with.
pub fn new_steps(pipeline: &Pipeline) -> Vec<NewStep> {
    pipeline
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
        .collect()
}

/// Saves `git diff HEAD` and the untracked list of a dirty tree. Returns the
/// path and the summary line.
pub async fn save_diff(project: &Project, run_id: i64, status: &GitStatus) -> std::io::Result<(PathBuf, String)> {
    let diff = crate::git::capture_diff(&project.path).await;
    let path = crate::paths::diff_path(&project.data_root(), run_id);
    let changed = status.staged + status.modified + status.conflicted;
    let summary = format!("{changed} file{} changed, {} untracked", if changed == 1 { "" } else { "s" }, status.untracked);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, diff)?;
    Ok((path, summary))
}

pub fn dry_run_plan(
    config: &Config,
    project: &Project,
    pipeline: &Pipeline,
    env: Option<&str>,
    inputs: BTreeMap<String, String>,
    protected: bool,
    blockers: Vec<String>,
) -> DryRunPlan {
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

fn run_lock_path(ctx: &Ctx, project: &str) -> PathBuf {
    crate::paths::run_lock_path(&ctx.state_dir, project)
}

/// The project's active run, if its worker holds the project lock.
pub fn busy_run(ctx: &Ctx, project: &str) -> Option<i64> {
    if !crate::lock::is_held(&run_lock_path(ctx, project)).unwrap_or(false) {
        return None;
    }
    let active = ctx.db(|s| s.active_runs()).unwrap_or_default();
    Some(active.into_iter().rev().find(|(_, p, _)| p == project).map(|(id, _, _)| id).unwrap_or(0))
}

async fn blockers(ctx: &Ctx, project: &Project) -> Vec<String> {
    let mut b = common_blockers(project).await;
    if let Some(id) = busy_run(ctx, &project.id) {
        b.push(format!("{} already has run #{id} in progress", project.id));
    }
    b
}

/// Validates and records a run, then opens its worker's window. One run per
/// project: a second start is refused, never queued.
pub async fn start(ctx: &Ctx, config: &Config, params: RunStartParams) -> Result<Response, ReqError> {
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
        let b = blockers(ctx, project).await;
        return Ok(Response::DryRun(dry_run_plan(config, project, pipeline, env, inputs, protected, b)));
    }
    if protected && !params.confirmed {
        return Err(err(ErrorCode::Refused, "protected run needs the confirmation dialog"));
    }
    let b = blockers(ctx, project).await;
    if !b.is_empty() {
        return Err(err(ErrorCode::Refused, b.join("; ")));
    }

    let snap = crate::git::snapshot(&project.path).await;
    let new = NewRun {
        project: &project.id,
        pipeline: &pipeline.id,
        env,
        inputs: &inputs,
        protected,
        branch: snap.branch.as_deref(),
        commit: snap.commit.as_deref(),
        dirty: snap.dirty,
        unpushed: snap.unpushed.is_some_and(|n| n > 0),
        unpushed_count: snap.unpushed,
    };
    let run_id = ctx.db(|s| s.insert_run(&new, &new_steps(pipeline), now())).map_err(|e| err(ErrorCode::Internal, e))?;
    if snap.dirty {
        match save_diff(project, run_id, &snap.status).await {
            Ok((path, summary)) => {
                let _ = ctx.db(|s| s.set_run_diff(run_id, &path.to_string_lossy(), &summary));
            }
            // The run still goes ahead; its record says the diff is missing.
            Err(e) => {
                let _ = ctx.db(|s| s.set_run_diff(run_id, "", &format!("diff not saved: {e}")));
            }
        }
    }

    // Earlier failed runs keep their dead window for reading; the next run replaces them.
    if let Ok(windows) = ctx.tmux.list_windows(&project.id).await {
        for w in windows.iter().filter(|w| w.dead && w.name.starts_with("run-")) {
            let _ = ctx.tmux.kill_window(&project.id, &w.name).await;
        }
    }
    let argv = [ctx.exe.to_string_lossy().into_owned(), "run-exec".into(), run_id.to_string()];
    let name = window_name(run_id);
    let opened = ctx
        .tmux
        .new_window(&NewWindow { project: &project.id, name: &name, cwd: &project.path, argv: &argv, env: &ctx.window_env() })
        .await;
    if let Err(e) = opened {
        let msg = format!("cannot open its tmux window: {e}");
        let _ = ctx.db(|s| s.interrupt_run(run_id, RunStatus::Failed, &msg, now()));
        return Err(err(ErrorCode::Internal, msg));
    }
    Ok(Response::RunStarted { id: run_id })
}

/// Asks a run's worker to cancel it.
pub fn cancel(ctx: &Ctx, id: i64) -> Result<Response, ReqError> {
    match ctx.db(|s| s.request_run_cancel(id)) {
        Ok(true) => Ok(Response::Ok),
        Ok(false) => Err(err(ErrorCode::NotFound, format!("run #{id} is not active"))),
        Err(e) => Err(err(ErrorCode::Internal, e)),
    }
}

/// Records an approval decision; the waiting worker polls for it. The first
/// answer wins.
pub fn decide(ctx: &Ctx, id: i64, step: usize, decision: Decision, confirmed: bool) -> Result<Response, ReqError> {
    let approval = ctx
        .db(|s| s.approval(id, step))
        .map_err(|e| err(ErrorCode::Internal, e))?
        .ok_or_else(|| err(ErrorCode::NotFound, format!("run #{id} has no approval at step {}", step + 1)))?;
    if decision == Decision::Approve && approval.protected && !confirmed {
        return Err(err(ErrorCode::Refused, "approving a protected run needs the confirm keypress"));
    }
    let status = match decision {
        Decision::Approve => ApprovalStatus::Approved,
        Decision::Reject => ApprovalStatus::Rejected,
    };
    match ctx.db(|s| s.decide_approval(id, step, status, now())) {
        Ok(Ok(())) => Ok(Response::Ok),
        Ok(Err(existing)) => Err(err(ErrorCode::AlreadyDecided, format!("already decided: {}", existing.as_str()))),
        Err(e) => Err(err(ErrorCode::Internal, e)),
    }
}

/// Ends active runs whose worker is gone (its window was closed hard, the
/// Mac rebooted, or it crashed). Returns their ids.
pub async fn reap(ctx: &Ctx) -> Vec<i64> {
    let mut ended = Vec::new();
    for (id, project, started_at) in ctx.db(|s| s.active_runs()).unwrap_or_default() {
        if now() - started_at < WORKER_START_GRACE_SECS {
            continue;
        }
        if crate::lock::is_held(&run_lock_path(ctx, &project)).unwrap_or(false) {
            continue;
        }
        if let Ok(Some(w)) = ctx.tmux.window(&project, &window_name(id)).await
            && !w.dead
        {
            continue;
        }
        if ctx.db(|s| s.interrupt_run(id, RunStatus::Cancelled, "worker exited unexpectedly", now())).is_ok() {
            ended.push(id);
        }
    }
    ended
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
