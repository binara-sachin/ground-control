//! End-to-end tests for runs: real `groundcontrol run-exec` workers in
//! windows of a private tmux server, driven through `runs::start`, `decide`
//! and `cancel`. Skipped when tmux is not installed.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use common::Env;
use groundcontrol::protocol::*;
use groundcontrol::runs;
use groundcontrol::runs::pane::{latest_started, split_steps};
use groundcontrol::tmux::NewWindow;
use groundcontrol::window::{RunParams, RunWindow};

fn config(repo: &Path, port: u16) -> String {
    format!(
        r#"
env:
  SHARED: hello
projects:
  app:
    path: {repo}
    remote: github.com/me/app
    environments:
      prod: {{ protected: true }}
    services:
      web:
        start: python3 -m http.server {port} --bind 127.0.0.1
        health: {{ port: {port} }}
    pipelines:
      hello:
        inputs:
          name: {{ type: string, pattern: "^[a-z]+$" }}
        steps:
          - name: Greet
            env_file: secrets.env
            run: 'echo "hi $INPUT_NAME $SHARED $COLUMNS"; echo "token=$API_TOKEN"; echo "bm=abc-123" >> "$GC_OUTPUT"'
          - name: Use output
            run: 'echo "got $OUTPUT_BM"'
      deploy:
        environments: [prod]
        steps:
          - run: echo checking
          - approve: "Deploy to $GC_ENV? shared=$SHARED"
          - run: echo deployed
      slow:
        steps:
          - run: sleep 30
      chatty:
        steps:
          - run: "while true; do echo tick; sleep 0.05; done"
      fail:
        steps:
          - run: "echo boom; exit 7"
          - run: echo never
      up:
        steps:
          - service: start web
          - run: "curl -s -o /dev/null http://127.0.0.1:{port}/ && echo reachable"
          - service: stop web
"#,
        repo = repo.display()
    )
}

fn params(pipeline: &str, env: Option<&str>, inputs: &[(&str, &str)], confirmed: bool) -> RunStartParams {
    RunStartParams {
        project: "app".into(),
        pipeline: pipeline.into(),
        env: env.map(Into::into),
        inputs: inputs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
        dry_run: false,
        confirmed,
    }
}

async fn start(env: &Env, p: RunStartParams) -> Result<i64, (ErrorCode, String)> {
    match runs::start(&env.ctx, &env.config, p).await? {
        Response::RunStarted { id } => Ok(id),
        other => panic!("{other:?}"),
    }
}

async fn detail(env: &Env, id: i64) -> Option<RunDetail> {
    env.run_window(id).await.map(|w| runs::detail(&env.config, &w))
}

async fn wait(env: &Env, id: i64, want: impl Fn(&RunDetail) -> bool) -> RunDetail {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let r = detail(env, id).await.expect("run window");
        if want(&r) {
            return r;
        }
        assert!(Instant::now() < deadline, "run {id} did not reach the expected state: {r:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn finished(env: &Env, id: i64) -> RunDetail {
    wait(env, id, |r| !r.summary.status.is_active()).await
}

/// The run's worker publishes its final state just before it exits; wait for
/// the window to be dead or gone, so the next start is not refused as busy.
async fn idle(env: &Env, id: i64) {
    env.wait_for(10, "run window exits", |w| w.iter().find(|w| w.id == id).is_none_or(|w| w.dead)).await;
}

async fn output(env: &Env, id: i64, step: usize) -> String {
    let pane = env.ctx.tmux.capture(id).await.unwrap();
    let latest = detail(env, id).await.and_then(|d| latest_started(&d));
    split_steps(&pane, latest).get(step).cloned().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn pipelines_end_to_end() {
    let Some(env) = common::setup(config) else { return };

    // Input validation happens before anything runs.
    let (code, msg) = start(&env, params("hello", None, &[("name", "Bad Name")], false)).await.unwrap_err();
    assert_eq!(code, ErrorCode::BadRequest);
    assert!(msg.contains("does not match"));
    assert!(env.windows().await.is_empty(), "nothing was opened");

    // Steps, env precedence, masking and outputs, all from the window.
    let id = start(&env, params("hello", None, &[("name", "sam")], false)).await.unwrap();
    let r = finished(&env, id).await;
    assert_eq!(r.summary.status, RunStatus::Succeeded, "{r:?}");
    assert_eq!(r.summary.branch.as_deref(), Some("main"));
    assert!(!r.summary.dirty, "secrets.env is ignored, so the tree is clean");
    let out = output(&env, id, 0).await;
    assert!(out.contains("hi sam hello 120"), "{out}");
    assert!(out.contains("token=***"), "secret must be masked: {out}");
    assert!(!out.contains("sup3r"), "{out}");
    assert_eq!((r.outputs[0].key.as_str(), r.outputs[0].value.as_str()), ("bm", "abc-123"));
    assert!(output(&env, id, 1).await.contains("got abc-123"));
    // A succeeded run's window closes itself after 5 seconds: there is no history.
    env.wait_for(10, "succeeded run window closes", |w| !w.iter().any(|w| w.id == id)).await;

    // Dry run executes nothing and lists env var names only.
    let mut dry = params("hello", None, &[("name", "sam")], false);
    dry.dry_run = true;
    let Response::DryRun(plan) = runs::start(&env.ctx, &env.config, dry).await.unwrap() else { panic!() };
    assert_eq!(plan.steps.len(), 2);
    assert!(plan.steps[0].env_names.iter().any(|n| n.starts_with("API_TOKEN")));
    assert!(plan.steps[0].env_names.iter().any(|n| n == "INPUT_NAME"));
    assert!(plan.blockers.is_empty(), "{:?}", plan.blockers);

    // Failure ends the run, skips the rest and keeps the window for reading.
    let failed = start(&env, params("fail", None, &[], false)).await.unwrap();
    let r = finished(&env, failed).await;
    assert_eq!(r.summary.status, RunStatus::Failed);
    assert_eq!(r.steps[0].exit_code, Some(7));
    assert_eq!(r.steps[1].status, StepStatus::Skipped);
    idle(&env, failed).await;
    assert!(env.run_window(failed).await.is_some());

    // Protected runs need the confirmation flag.
    let (code, _) = start(&env, params("deploy", Some("prod"), &[], false)).await.unwrap_err();
    assert_eq!(code, ErrorCode::Refused);

    // Approval: pause, busy project, protected confirm, first answer wins.
    let id = start(&env, params("deploy", Some("prod"), &[], true)).await.unwrap();
    // The next run replaces the project's dead run windows. (Window ids, and
    // so run ids, restart when the tmux server does, which can happen here.)
    assert!(!env.windows().await.runs.iter().any(|w| w.params.pipeline == "fail"), "the next run replaces the project's dead run windows");
    let r = wait(&env, id, |r| r.summary.status == RunStatus::AwaitingApproval).await;
    assert_eq!(r.approvals[0].message, "Deploy to prod? shared=hello");
    assert_eq!(r.steps[1].status, StepStatus::Waiting);
    let (_, busy) = start(&env, params("hello", None, &[("name", "x")], false)).await.unwrap_err();
    assert!(busy.contains(&format!("already has run #{id}")), "{busy}");
    let (code, _) = runs::decide(&env.ctx, id, 1, Decision::Approve, false).await.unwrap_err();
    assert_eq!(code, ErrorCode::Refused, "protected approval needs the confirm keypress");
    runs::decide(&env.ctx, id, 1, Decision::Approve, true).await.unwrap();
    let (code, msg) = runs::decide(&env.ctx, id, 1, Decision::Reject, false).await.unwrap_err();
    assert_eq!(code, ErrorCode::AlreadyDecided);
    assert!(msg.contains("approved"), "{msg}");
    let r = finished(&env, id).await;
    assert_eq!(r.summary.status, RunStatus::Succeeded);
    assert!(output(&env, id, 2).await.contains("deployed"));
    idle(&env, id).await;

    // Reject is distinct from failed.
    let id = start(&env, params("deploy", Some("prod"), &[], true)).await.unwrap();
    wait(&env, id, |r| r.summary.status == RunStatus::AwaitingApproval).await;
    runs::decide(&env.ctx, id, 1, Decision::Reject, false).await.unwrap();
    let r = finished(&env, id).await;
    assert_eq!(r.summary.status, RunStatus::Rejected);
    assert_eq!(r.steps[2].status, StepStatus::Skipped);
    idle(&env, id).await;

    // Cancel kills the step's process group, even while it prints constantly.
    let id = start(&env, params("chatty", None, &[], false)).await.unwrap();
    wait(&env, id, |r| r.steps[0].status == StepStatus::Running).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    runs::cancel(&env.ctx, id).await.unwrap();
    let r = finished(&env, id).await;
    assert_eq!(r.summary.status, RunStatus::Cancelled);
    assert_eq!(r.steps[0].status, StepStatus::Cancelled);
    idle(&env, id).await;
    assert_eq!(runs::cancel(&env.ctx, id).await.unwrap_err().0, ErrorCode::NotFound);

    // service: steps start and stop services from inside the run's worker.
    let id = start(&env, params("up", None, &[], false)).await.unwrap();
    let r = finished(&env, id).await;
    assert_eq!(r.summary.status, RunStatus::Succeeded, "{r:?}\n{}", output(&env, id, 0).await);
    assert!(output(&env, id, 0).await.contains("web started"));
    assert!(output(&env, id, 1).await.contains("reachable"));
    idle(&env, id).await;

    // A dirty tree is shown as such.
    std::fs::write(env.base.join("app/README"), "changed\n").unwrap();
    let id = start(&env, params("hello", None, &[("name", "sam")], false)).await.unwrap();
    assert!(finished(&env, id).await.summary.dirty);

    // Everything lives in the one `ground-control` session.
    let sessions = String::from_utf8(env.tmux(&["list-sessions", "-F", "#{session_name}"]).stdout).unwrap();
    assert_eq!(sessions.trim(), "ground-control");
}

#[tokio::test(flavor = "multi_thread")]
async fn closed_windows_dead_workers_and_guards() {
    let Some(env) = common::setup(config) else { return };

    // Closing the window (SIGHUP) cancels the run.
    let id = start(&env, params("slow", None, &[], false)).await.unwrap();
    wait(&env, id, |r| r.steps[0].status == StepStatus::Running).await;
    env.ctx.tmux.kill_window(id).await.unwrap();
    env.wait_for(10, "window closes", |w| !w.iter().any(|w| w.id == id)).await;

    // A run whose worker died without publishing an end shows as cancelled.
    let argv = vec!["true".to_string()];
    let options = RunWindow::create(&RunParams { project: "app".into(), pipeline: "slow".into(), ..Default::default() }, 0);
    let dead = env
        .ctx
        .tmux
        .new_window(&NewWindow { name: "app/slow", cwd: Path::new("/"), argv: &argv, env: &[], options: &options })
        .await
        .unwrap();
    env.wait_for(5, "fake worker exits", |w| w.iter().any(|w| w.id == dead && w.dead)).await;
    let r = detail(&env, dead).await.unwrap();
    assert_eq!(r.summary.status, RunStatus::Cancelled);
    assert_eq!(r.summary.reason.as_deref(), Some("worker exited unexpectedly"));
    assert_eq!(runs::busy_run(&env.windows().await, "app"), None, "a dead run does not hold the project");

    // Origin mismatch.
    common::sh(&env.base.join("app"), "git remote set-url origin https://github.com/someone/else");
    let (_, msg) = start(&env, params("hello", None, &[("name", "x")], false)).await.unwrap_err();
    assert!(msg.contains("refusing"), "{msg}");
}
