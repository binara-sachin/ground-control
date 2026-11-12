//! `groundcontrol run-exec`: executes the run its tmux window was opened for,
//! in the foreground of that window (docs/architecture.md, "Pipeline runs").
//!
//! The run's parameters (project, pipeline, environment, inputs) are the
//! window's `@gc_*` options, set when the window was created. The worker
//! publishes its progress there too: run state, per-step state, outputs and
//! the pending approval. The TUI asks for a cancel (`@gc_cancel`) and answers
//! approvals (`@gc_decision`) there and wakes the worker, which otherwise
//! waits without starting any process. C-c, SIGTERM and SIGHUP (the window
//! closed) cancel the run too.
//!
//! Step output goes, masked, to the pane only. Each step starts with a header
//! line (`STEP_HEADER`), which is how the TUI splits the pane into steps.

use std::collections::BTreeMap;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;

use super::{APPROVAL_TTL, fmt_secs, parse_outputs};
use crate::config::{Config, Pipeline, Project, ServiceAction, StepKind};
use crate::envlayers::{self, RunScope};
use crate::mask::Masker;
use crate::process;
use crate::protocol::*;
use crate::tmux::Tmux;
use crate::window::{self, ApprovalState, RunUpdate, RunWindow, StepState};
use crate::worker::{Signals, Waker, close_own_pane, pane, say, tell};

/// How long a succeeded run's window stays open before it closes itself.
const LINGER: Duration = Duration::from_secs(5);
/// The first words of the line printed before each step (after `say`'s `── `).
pub const STEP_HEADER: &str = "step ";

struct Exec {
    tmux: Tmux,
    /// `$TMUX_PANE`: options set on it land on the worker's window.
    pane: String,
    id: i64,
    signals: Signals,
    waker: Waker,
    /// A wake arrived and its `@gc_cancel` check has not finished yet.
    woken: bool,
    cancelled: bool,
    steps: Vec<StepState>,
    outputs: Vec<OutputInfo>,
}

impl Exec {
    /// Publishes a change on the run's window; a failure is reported in the pane.
    async fn publish(&self, update: RunUpdate<'_>) {
        if let Err(e) = self.tmux.set_options(&self.pane, &RunWindow::update(update)).await {
            say(&format!("cannot publish state: {e}"));
        }
    }

    async fn publish_step(&mut self, idx: usize, f: impl FnOnce(&mut StepState)) {
        if let Some(s) = self.steps.get_mut(idx) {
            f(s);
        }
        self.publish(RunUpdate::Steps(&self.steps)).await;
    }

    async fn cancel_requested(&self) -> bool {
        window::run_requests(&self.tmux, &self.pane).await.is_ok_and(|(cancel, _)| cancel)
    }

    /// Resolves once the run is cancelled: a signal, or `@gc_cancel` set by
    /// the TUI, which wakes the worker. Safe to drop unfinished, so a step that
    /// prints constantly (and so keeps restarting the `select!` around this)
    /// cannot lose a wake or a cancel.
    async fn until_cancelled(&mut self) {
        while !self.cancelled {
            // A wake is kept until its check has finished: this future is
            // dropped whenever the step prints, possibly in the middle of it.
            if self.woken {
                if self.cancel_requested().await {
                    self.cancelled = true;
                }
                self.woken = false;
                continue;
            }
            tokio::select! {
                _ = self.signals.recv() => self.cancelled = true,
                _ = self.waker.woken() => self.woken = true,
            }
        }
    }

    async fn check_cancelled(&mut self) -> bool {
        if !self.cancelled && self.cancel_requested().await {
            self.cancelled = true;
        }
        self.cancelled
    }
}

enum StepOutcome {
    Ok,
    Failed(Option<String>),
    Rejected,
    Cancelled(Option<String>),
}

/// Ends the run before it started: publishes why and returns exit code 1.
async fn fail(tmux: &Tmux, pane_id: &str, msg: &str) -> i32 {
    say(msg);
    let ended = RunUpdate::Ended { state: RunStatus::Failed, reason: Some(msg), finished: now(), steps: None };
    let _ = tmux.set_options(pane_id, &RunWindow::update(ended)).await;
    pane(b"\x07");
    1
}

/// Entry point. Returns the process exit code: 0 when the run succeeded.
pub async fn run() -> i32 {
    let Ok(pane_id) = std::env::var("TMUX_PANE") else {
        say("run-exec runs inside a tmux window that GroundControl opens");
        return 1;
    };
    let tmux = Tmux::default_server();
    let id = match tmux.pane_window(&pane_id).await {
        Ok(id) => id,
        Err(e) => {
            say(&format!("cannot find this run's window: {e}"));
            return 1;
        }
    };
    let me = match window::run(&tmux, id).await {
        Ok(Some(me)) => me,
        Ok(None) => {
            say("this window was not opened for a run");
            return 1;
        }
        Err(e) => {
            say(&format!("cannot read this run's window: {e}"));
            return 1;
        }
    };
    let window::RunParams { project: project_id, pipeline: pipeline_id, env, inputs, .. } = me.params;

    // One run per project: an older live run of the same project wins.
    let windows = window::list(&tmux).await.unwrap_or_default();
    if let Some(older) = super::busy_run(&windows, &project_id)
        && older < id
    {
        let code = fail(&tmux, &pane_id, &format!("{project_id} already has run #{older} in progress")).await;
        close_own_pane().await;
        return code;
    }
    let signals = match Signals::install() {
        Ok(s) => s,
        Err(e) => return fail(&tmux, &pane_id, &format!("cannot install signal handlers: {e}")).await,
    };
    let Some(config) = crate::config::load_file(&crate::paths::config_path()).config else {
        return fail(&tmux, &pane_id, "groundcontrol.yaml is invalid; see the Health screen").await;
    };
    let Some(project) = config.project(&project_id) else {
        return fail(&tmux, &pane_id, &format!("project `{project_id}` is no longer in the config")).await;
    };
    let Some(pipeline) = project.pipeline(&pipeline_id) else {
        return fail(&tmux, &pane_id, &format!("pipeline `{pipeline_id}` is no longer in {project_id}")).await;
    };

    say(&format!("run #{id}: {project_id}/{pipeline_id}{}", env.as_deref().map(|e| format!(" on {e}")).unwrap_or_default()));
    let steps = vec![StepState { status: Some(StepStatus::Pending), ..Default::default() }; pipeline.steps.len()];
    let waker = Waker::new(tmux.clone(), id);
    let mut exec = Exec { tmux, pane: pane_id, id, signals, waker, woken: false, cancelled: false, steps, outputs: Vec::new() };
    let (status, reason) = execute(&mut exec, &config, project, pipeline, env.as_deref(), &inputs).await;

    let text = format!("run #{id} {}{}", status.as_str(), reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default());
    say(&text);
    if status == RunStatus::Succeeded {
        tokio::time::sleep(LINGER).await;
        close_own_pane().await;
        0
    } else {
        if status == RunStatus::Failed {
            pane(b"\x07");
            tell(&format!("{project_id}/{pipeline_id} {text}")).await;
        }
        1
    }
}

/// A scratch folder for `$GC_OUTPUT`, removed when the run ends.
fn scratch_dir(id: i64) -> PathBuf {
    std::env::temp_dir().join(format!("ground-control-run-{id}-{}", std::process::id()))
}

async fn execute(
    exec: &mut Exec,
    config: &Config,
    project: &Project,
    pipeline: &Pipeline,
    env: Option<&str>,
    inputs: &BTreeMap<String, String>,
) -> (RunStatus, Option<String>) {
    let tmp = scratch_dir(exec.id);
    let out_file = tmp.join("outputs");
    let prepared = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&tmp).and_then(|_| std::fs::write(&out_file, ""));
    let (status, reason) = match prepared {
        Err(e) => (RunStatus::Failed, Some(format!("cannot prepare $GC_OUTPUT: {e}"))),
        Ok(()) => run_steps(exec, config, project, pipeline, env, inputs, &out_file).await,
    };
    let _ = std::fs::remove_dir_all(&tmp);
    for s in exec.steps.iter_mut().filter(|s| s.status == Some(StepStatus::Pending)) {
        s.status = Some(StepStatus::Skipped);
    }
    exec.publish(RunUpdate::Ended { state: status, reason: reason.as_deref(), finished: now(), steps: Some(&exec.steps) }).await;
    (status, reason)
}

async fn run_steps(
    exec: &mut Exec,
    config: &Config,
    project: &Project,
    pipeline: &Pipeline,
    env: Option<&str>,
    inputs: &BTreeMap<String, String>,
    out_file: &Path,
) -> (RunStatus, Option<String>) {
    let mut outputs: BTreeMap<String, String> = BTreeMap::new();
    let mut outputs_seen = 0usize;
    let mut all_secrets: Vec<String> = Vec::new();
    let total = pipeline.steps.len();

    for (idx, step) in pipeline.steps.iter().enumerate() {
        if exec.check_cancelled().await {
            return (RunStatus::Cancelled, Some("cancelled".into()));
        }
        say(&format!("{STEP_HEADER}{}/{total}: {}", idx + 1, step.label()));
        let scope = RunScope { env, inputs, outputs: &outputs, run_id: Some(exec.id), output_file: out_file };
        let outcome = match &step.kind {
            StepKind::Run { run, cwd, env_file, timeout } => {
                let dir = project.resolve_dir(cwd.as_deref());
                let env_file = env_file.as_ref().map(|f| dir.join(f));
                match envlayers::step(config, project, &scope, env_file.as_deref()) {
                    Err(e) => fail_step_early(exec, idx, &e).await,
                    Ok(resolved) => {
                        all_secrets.extend(resolved.secrets().iter().cloned());
                        run_step(exec, idx, run, &dir, resolved.vars(), Masker::new(resolved.secrets()), *timeout).await
                    }
                }
            }
            StepKind::Approve { message } => {
                let text = envlayers::approval_message(config, project, &scope, message);
                approve_step(exec, idx, &text).await
            }
            StepKind::Service { action, target } => service_step(exec, config, project, idx, *action, target).await,
        };

        // Collect outputs appended during this step (§6.4).
        if let Ok(text) = std::fs::read_to_string(out_file) {
            let masker = Masker::new(&all_secrets);
            let mut changed = false;
            for (k, v) in parse_outputs(&text).into_iter().skip(outputs_seen) {
                outputs_seen += 1;
                let v = String::from_utf8_lossy(&masker.mask_all(v.as_bytes())).into_owned();
                exec.outputs.retain(|o| o.key != k);
                exec.outputs.push(OutputInfo { key: k.clone(), value: v.clone(), step_index: Some(idx) });
                outputs.insert(k, v);
                changed = true;
            }
            if changed {
                exec.publish(RunUpdate::Outputs(&exec.outputs)).await;
            }
        }

        match outcome {
            StepOutcome::Ok => {}
            StepOutcome::Failed(r) => return (RunStatus::Failed, r),
            StepOutcome::Rejected => return (RunStatus::Rejected, None),
            StepOutcome::Cancelled(r) => return (RunStatus::Cancelled, r),
        }
    }
    (RunStatus::Succeeded, None)
}

async fn begin_step(exec: &mut Exec, idx: usize, status: StepStatus) {
    exec.publish_step(idx, |s| {
        s.status = Some(status);
        s.started_at = Some(now());
    })
    .await;
}

async fn finish_step(exec: &mut Exec, idx: usize, status: StepStatus, exit_code: Option<i32>) {
    exec.publish_step(idx, |s| {
        s.status = Some(status);
        s.exit_code = exit_code;
        s.finished_at = Some(now());
    })
    .await;
}

async fn fail_step_early(exec: &mut Exec, idx: usize, msg: &str) -> StepOutcome {
    begin_step(exec, idx, StepStatus::Running).await;
    pane(format!("groundcontrol: {msg}\n").as_bytes());
    finish_step(exec, idx, StepStatus::Failed, None).await;
    StepOutcome::Failed(Some(msg.to_string()))
}

async fn run_step(
    exec: &mut Exec,
    idx: usize,
    command: &str,
    dir: &Path,
    vars: &BTreeMap<String, String>,
    mut masker: Masker,
    timeout: Duration,
) -> StepOutcome {
    if !dir.is_dir() {
        return fail_step_early(exec, idx, &format!("working directory {} does not exist", dir.display())).await;
    }
    begin_step(exec, idx, StepStatus::Running).await;
    let mut spawned = match process::spawn_shell(command, dir, vars) {
        Ok(s) => s,
        Err(e) => {
            pane(format!("groundcontrol: cannot start step: {e}\n").as_bytes());
            finish_step(exec, idx, StepStatus::Failed, None).await;
            return StepOutcome::Failed(Some(format!("cannot start step: {e}")));
        }
    };
    let pgid = spawned.pid;
    let deadline = Instant::now() + timeout;
    enum End {
        Exited(Option<i32>),
        TimedOut,
        Cancelled,
    }
    let mut output_open = true;
    let end = loop {
        tokio::select! {
            chunk = spawned.output.recv(), if output_open => match chunk {
                Some(c) => pane(&masker.push(&c)),
                None => output_open = false,
            },
            status = spawned.child.wait() => break End::Exited(status.ok().and_then(|s| s.code())),
            _ = tokio::time::sleep_until(deadline) => break End::TimedOut,
            _ = exec.until_cancelled() => break End::Cancelled,
        }
    };
    let code = match end {
        End::Exited(c) => c,
        End::TimedOut | End::Cancelled => {
            let what = if matches!(end, End::TimedOut) { "timed out; stopping" } else { "cancel requested; stopping" };
            pane(&masker.push(format!("\ngroundcontrol: {what} (SIGTERM, then SIGKILL after 10s)\n").as_bytes()));
            process::terminate(&mut spawned.child, pgid).await
        }
    };
    // Drain what is left; something the step left running may hold the pipe open.
    let drain_until = Instant::now() + Duration::from_secs(2);
    while output_open {
        match tokio::time::timeout_at(drain_until, spawned.output.recv()).await {
            Ok(Some(c)) => pane(&masker.push(&c)),
            _ => output_open = false,
        }
    }
    pane(&masker.flush());

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
    finish_step(exec, idx, status, code).await;
    outcome
}

/// The decision the TUI recorded for step `idx`, if any.
async fn decision(exec: &Exec, idx: usize) -> Option<ApprovalStatus> {
    let (_, d) = window::run_requests(&exec.tmux, &exec.pane).await.ok()?;
    d.filter(|d| d.step == idx).map(|d| d.status)
}

/// Pauses for a decision recorded by the TUI. Expires after 30 minutes.
async fn approve_step(exec: &mut Exec, idx: usize, message: &str) -> StepOutcome {
    let requested = now();
    let mut approval = ApprovalState {
        step: idx,
        status: ApprovalStatus::Pending,
        requested_at: requested,
        expires_at: requested + APPROVAL_TTL.as_secs() as i64,
        decided_at: None,
        message: message.to_string(),
    };
    exec.publish(RunUpdate::Approval(&approval)).await;
    exec.publish_step(idx, |s| {
        s.status = Some(StepStatus::Waiting);
        s.started_at = Some(requested);
    })
    .await;
    exec.publish(RunUpdate::State(RunStatus::AwaitingApproval)).await;
    say(&format!("waiting for approval in GroundControl: {message}"));
    pane(b"\x07");
    tell(&format!("run #{} is waiting for approval: {message}", exec.id)).await;

    let deadline = Instant::now() + APPROVAL_TTL;
    let decided = loop {
        tokio::select! {
            _ = exec.signals.recv() => {
                exec.cancelled = true;
                break None;
            }
            _ = tokio::time::sleep_until(deadline) => break None,
            // The TUI wrote a decision or a cancel.
            _ = exec.waker.woken() => {
                if exec.cancel_requested().await {
                    exec.cancelled = true;
                    break None;
                }
                if let Some(d) = decision(exec, idx).await.filter(|d| *d != ApprovalStatus::Pending) {
                    break Some(d);
                }
            }
        }
    };
    // Expired or cancelled: an answer that landed at the same moment still wins.
    let status = match decided {
        Some(s) => s,
        None => match decision(exec, idx).await {
            Some(d @ (ApprovalStatus::Approved | ApprovalStatus::Rejected)) => d,
            _ if exec.cancelled => ApprovalStatus::Cancelled,
            _ => ApprovalStatus::Expired,
        },
    };
    approval.status = status;
    approval.decided_at = Some(now());
    exec.publish(RunUpdate::Approval(&approval)).await;
    match status {
        ApprovalStatus::Approved => {
            say("approved");
            finish_step(exec, idx, StepStatus::Succeeded, None).await;
            exec.publish(RunUpdate::State(RunStatus::Running)).await;
            StepOutcome::Ok
        }
        ApprovalStatus::Rejected => {
            say("rejected");
            finish_step(exec, idx, StepStatus::Rejected, None).await;
            StepOutcome::Rejected
        }
        _ => {
            finish_step(exec, idx, StepStatus::Cancelled, None).await;
            StepOutcome::Cancelled(Some(if exec.cancelled { "cancelled".into() } else { "approval expired".into() }))
        }
    }
}

async fn service_step(exec: &mut Exec, config: &Config, project: &Project, idx: usize, action: ServiceAction, target: &str) -> StepOutcome {
    begin_step(exec, idx, StepStatus::Running).await;
    pane(format!("groundcontrol: {} {target}…\n", action.as_str()).as_bytes());
    let result = match crate::ctx::Ctx::from_env() {
        Ok(ctx) => crate::services::control(&ctx, config, &project.id, target, action).await,
        Err(e) => Err(e),
    };
    let (status, outcome) = match result {
        Ok(msg) => {
            pane(format!("groundcontrol: {msg}\n").as_bytes());
            (StepStatus::Succeeded, StepOutcome::Ok)
        }
        Err(e) => {
            pane(format!("groundcontrol: {e}\n").as_bytes());
            (StepStatus::Failed, StepOutcome::Failed(Some(e)))
        }
    };
    finish_step(exec, idx, status, None).await;
    outcome
}
