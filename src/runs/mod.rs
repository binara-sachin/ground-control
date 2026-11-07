//! Pipeline runs (docs/architecture.md, "Pipeline runs"). `start` validates
//! a run and opens its window in the `ground-control` tmux session, whose
//! `groundcontrol run-exec` worker executes it. A run's id is its window's id,
//! and everything known about it lives in that window's `@gc_*` options: the
//! worker publishes its progress there, and the TUI asks for a cancel or
//! answers an approval there. Nothing is written to disk. Also: inputs,
//! environments, step env, outputs and dry runs (§6.1–§6.4, §8).

pub mod worker;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use crate::config::{Config, InputKind, Pipeline, Project, StepKind};
use crate::ctx::Ctx;
use crate::protocol::*;
use crate::tmux::{NewWindow, target};
use crate::window::{self, RunParams, RunWindow, Windows};

pub const APPROVAL_TTL: Duration = Duration::from_secs(30 * 60);

pub type ReqError = (ErrorCode, String);

pub fn err(code: ErrorCode, msg: impl Into<String>) -> ReqError {
    (code, msg.into())
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

/// Checks that refuse a run whoever executes it (§8.4).
pub async fn common_blockers(project: &Project) -> Vec<String> {
    let mut blockers = Vec::new();
    if let Some(reason) = &project.disabled {
        blockers.push(format!("project is disabled: {reason}"));
        return blockers;
    }
    if let Err(e) = crate::git::check_remote(&project.path, project.remote.as_deref()).await {
        blockers.push(e);
    }
    blockers
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
                // The same variables as a real run; outputs aren't known yet, so `$OUTPUT_*` stays as written.
                let vars = approval_vars(config, project, env, &inputs, &BTreeMap::new(), &gc);
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

fn run_status(w: &RunWindow) -> (RunStatus, Option<String>) {
    if w.dead && w.state.is_active() {
        // The worker died without publishing an end (killed hard).
        return (RunStatus::Cancelled, Some("worker exited unexpectedly".into()));
    }
    (w.state, w.reason.clone())
}

pub fn summary(w: &RunWindow) -> RunSummary {
    let (status, reason) = run_status(w);
    let p = &w.params;
    RunSummary {
        id: w.id,
        project: p.project.clone(),
        pipeline: p.pipeline.clone(),
        env: p.env.clone(),
        status,
        reason,
        branch: p.branch.clone(),
        commit: p.commit.clone(),
        dirty: p.dirty,
        unpushed: p.unpushed.is_some_and(|n| n > 0),
        protected: p.protected,
        started_at: p.started,
        finished_at: w.finished,
    }
}

/// A run's detail. Step names and commands come from the config; their
/// progress from the window.
pub fn detail(config: &Config, w: &RunWindow) -> RunDetail {
    let s = summary(w);
    let states = &w.steps;
    let outline: Vec<(String, StepKindName, String, Option<String>)> = config
        .project(&s.project)
        .and_then(|p| p.pipeline(&s.pipeline))
        .map(|pl| {
            pl.steps
                .iter()
                .map(|st| {
                    let (command, cwd) = match &st.kind {
                        StepKind::Run { run, cwd, .. } => (run.clone(), cwd.clone()),
                        StepKind::Approve { message } => (message.clone(), None),
                        StepKind::Service { action, target } => (format!("{} {target}", action.as_str()), None),
                    };
                    (st.label(), StepKindName::parse(st.kind_str()), command, cwd)
                })
                .collect()
        })
        .unwrap_or_default();
    let n = outline.len().max(states.len());
    let steps = (0..n)
        .map(|i| {
            let (name, kind, command, cwd) =
                outline.get(i).cloned().unwrap_or_else(|| (format!("step {}", i + 1), StepKindName::Run, String::new(), None));
            let st = states.get(i).cloned().unwrap_or_default();
            let mut status = st.status.unwrap_or(StepStatus::Pending);
            if w.dead && matches!(status, StepStatus::Running | StepStatus::Waiting) {
                status = StepStatus::Cancelled;
            }
            StepInfo {
                index: i,
                name,
                kind,
                command,
                cwd,
                status,
                exit_code: st.exit_code,
                started_at: st.started_at,
                finished_at: st.finished_at,
                has_log: st.started_at.is_some(),
            }
        })
        .collect();
    let approvals = w
        .approval
        .clone()
        .map(|a| {
            let status = if w.dead && a.status == ApprovalStatus::Pending { ApprovalStatus::Cancelled } else { a.status };
            vec![ApprovalInfo {
                run_id: s.id,
                step_index: a.step,
                project: s.project.clone(),
                pipeline: s.pipeline.clone(),
                env: s.env.clone(),
                protected: s.protected,
                message: a.message,
                status,
                requested_at: a.requested_at,
                decided_at: a.decided_at,
                expires_at: a.expires_at,
            }]
        })
        .unwrap_or_default();
    RunDetail {
        inputs: w.params.inputs.clone(),
        steps,
        approvals,
        outputs: w.outputs.clone(),
        unpushed_count: w.params.unpushed,
        summary: s,
    }
}

/// The project's run in progress: a live run window whose run is active.
pub fn busy_run(windows: &Windows, project: &str) -> Option<i64> {
    windows.runs.iter().find(|w| !w.dead && w.params.project == project && run_status(w).0.is_active()).map(|w| w.id)
}

/// Validates a run and opens its worker's window. One run per project: a
/// second start is refused, never queued.
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

    let windows = window::list(&ctx.tmux).await.map_err(|e| err(ErrorCode::Internal, e))?;
    let mut blockers = common_blockers(project).await;
    if let Some(id) = busy_run(&windows, &project.id) {
        blockers.push(format!("{} already has run #{id} in progress", project.id));
    }
    if params.dry_run {
        return Ok(Response::DryRun(dry_run_plan(config, project, pipeline, env, inputs, protected, blockers)));
    }
    if protected && !params.confirmed {
        return Err(err(ErrorCode::Refused, "protected run needs the confirmation dialog"));
    }
    if !blockers.is_empty() {
        return Err(err(ErrorCode::Refused, blockers.join("; ")));
    }

    // The project's earlier failed runs keep their dead window for reading
    // until this one replaces them.
    for w in &windows.runs {
        if w.dead && w.params.project == project.id {
            let _ = ctx.tmux.kill_window(w.id).await;
        }
    }
    let snap = crate::git::snapshot(&project.path).await;
    let params = RunParams {
        project: project.id.clone(),
        pipeline: pipeline.id.clone(),
        env: env.map(str::to_string),
        inputs,
        protected,
        branch: snap.branch,
        commit: snap.commit,
        dirty: snap.dirty,
        unpushed: snap.unpushed,
        started: now(),
    };
    let options = RunWindow::create(&params, pipeline.steps.len());
    let argv = [ctx.exe.to_string_lossy().into_owned(), "run-exec".into()];
    let name = format!("{}/{}", project.id, pipeline.id);
    let id = ctx
        .tmux
        .new_window(&NewWindow { name: &name, cwd: &project.path, argv: &argv, env: &ctx.window_env(), options: &options })
        .await
        .map_err(|e| err(ErrorCode::Internal, format!("cannot open the run's tmux window: {e}")))?;
    Ok(Response::RunStarted { id })
}

async fn active_window(ctx: &Ctx, id: i64) -> Result<RunWindow, ReqError> {
    match window::run(&ctx.tmux, id).await.map_err(|e| err(ErrorCode::Internal, e))? {
        Some(w) if !w.dead && run_status(&w).0.is_active() => Ok(w),
        _ => Err(err(ErrorCode::NotFound, format!("run #{id} is not active"))),
    }
}

/// Asks a run's worker to cancel it.
pub async fn cancel(ctx: &Ctx, id: i64) -> Result<Response, ReqError> {
    active_window(ctx, id).await?;
    ctx.tmux.request(id, &RunWindow::cancel_request()).await.map_err(|e| err(ErrorCode::Internal, e))?;
    Ok(Response::Ok)
}

/// Records an approval decision on the run's window and wakes the waiting
/// worker. The first answer wins.
pub async fn decide(ctx: &Ctx, id: i64, step: usize, decision: Decision, confirmed: bool) -> Result<Response, ReqError> {
    let w = active_window(ctx, id).await?;
    let approval = w
        .approval
        .filter(|a| a.step == step)
        .ok_or_else(|| err(ErrorCode::NotFound, format!("run #{id} has no approval at step {}", step + 1)))?;
    if decision == Decision::Approve && w.params.protected && !confirmed {
        return Err(err(ErrorCode::Refused, "approving a protected run needs the confirm keypress"));
    }
    // Read fresh: another TUI may have answered since the listing.
    let (_, existing) = window::run_requests(&ctx.tmux, &target(id)).await.map_err(|e| err(ErrorCode::Internal, e))?;
    if approval.status != ApprovalStatus::Pending || existing.is_some() {
        let already = existing.map_or(approval.status, |d| d.status);
        return Err(err(ErrorCode::AlreadyDecided, format!("already decided: {}", already.as_str())));
    }
    let status = match decision {
        Decision::Approve => ApprovalStatus::Approved,
        Decision::Reject => ApprovalStatus::Rejected,
    };
    let request = RunWindow::decision_request(window::Decision { status, step });
    ctx.tmux.request(id, &request).await.map_err(|e| err(ErrorCode::Internal, e))?;
    Ok(Response::Ok)
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
    fn dry_run_expands_approvals_like_a_real_run() {
        let src = r#"
projects:
  p:
    path: /tmp
    environments:
      prod: { protected: true, vars: { DB: ledger } }
    pipelines:
      x:
        environments: [prod]
        steps:
          - approve: "Migrate $DB on $GC_ENV with $OUTPUT_BM?"
          - run: "true"
"#;
        let c = crate::config::load_str(src).config.unwrap();
        let p = c.project("p").unwrap();
        let plan = dry_run_plan(&c, p, p.pipeline("x").unwrap(), Some("prod"), BTreeMap::new(), true, Vec::new());
        assert_eq!(plan.steps[0].command, "pause for approval: Migrate ledger on prod with $OUTPUT_BM?");
    }

    #[test]
    fn outputs_parse() {
        assert_eq!(
            parse_outputs("d1_bookmark=0000-abc\nnot a line\nx=a=b\n"),
            vec![("d1_bookmark".to_string(), "0000-abc".to_string()), ("x".to_string(), "a=b".to_string())]
        );
    }
}
