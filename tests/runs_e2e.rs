//! End-to-end tests for daemonless runs: real `groundcontrol run-exec`
//! workers in windows of a private tmux server, driven through `runs::start`,
//! `decide` and `cancel`. Skipped when tmux is not installed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use groundcontrol::config::Config;
use groundcontrol::ctx::Ctx;
use groundcontrol::protocol::*;
use groundcontrol::runs;
use groundcontrol::store::NewRun;
use groundcontrol::tmux::Tmux;

struct Env {
    ctx: Ctx,
    config: Config,
    socket: PathBuf,
    base: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new("tmux").arg("-S").arg(&self.socket).arg("kill-server").output();
    }
}

fn sh(dir: &Path, cmd: &str) {
    let ok = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).status().unwrap().success();
    assert!(ok, "{cmd}");
}

fn git_repo(dir: &Path, remote: &str) {
    std::fs::create_dir_all(dir).unwrap();
    sh(
        dir,
        &format!(
            "git init -q -b main && git config user.email t@t && git config user.name t && \
             git remote add origin {remote} && printf 'secrets.env\\n.groundcontrol/\\n' > .gitignore && \
             echo hi > README && git add -A && git commit -qm init"
        ),
    );
}

fn setup() -> Option<Env> {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not installed; skipping");
        return None;
    }
    // Short: the tmux socket path must stay under ~104 bytes.
    let dir = tempfile::Builder::new().prefix("gcr").tempdir_in(std::env::temp_dir()).unwrap();
    let base = dir.path().to_path_buf();
    let repo = base.join("app");
    let sens = base.join("sens");
    git_repo(&repo, "git@github.com:me/app.git");
    git_repo(&sens, "git@github.com:me/sens.git");
    std::fs::write(repo.join("secrets.env"), "API_TOKEN=sup3r-s3cret-value\n").unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cfg = format!(
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
  sens:
    path: {sens}
    sensitive: true
    pipelines:
      p:
        steps:
          - run: echo sensitive-output
"#,
        repo = repo.display(),
        sens = sens.display(),
    );
    let cfg_path = base.join("groundcontrol.yaml");
    std::fs::write(&cfg_path, &cfg).unwrap();
    let config = groundcontrol::config::load_str(&cfg).config.expect("valid config");
    let socket = base.join("s");
    let ctx = Ctx::new(Tmux::with_socket(&socket), env!("CARGO_BIN_EXE_groundcontrol").into(), base.join("state"), cfg_path).unwrap();
    Some(Env { ctx, config, socket, base, _dir: dir })
}

fn params(project: &str, pipeline: &str, env: Option<&str>, inputs: &[(&str, &str)], confirmed: bool) -> RunStartParams {
    RunStartParams {
        project: project.into(),
        pipeline: pipeline.into(),
        env: env.map(Into::into),
        inputs: inputs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
        dry_run: false,
        confirmed,
    }
}

impl Env {
    async fn start(&self, p: RunStartParams) -> Result<i64, (ErrorCode, String)> {
        match runs::start(&self.ctx, &self.config, p).await? {
            Response::RunStarted { id } => Ok(id),
            other => panic!("{other:?}"),
        }
    }

    fn detail(&self, id: i64) -> RunDetail {
        self.ctx.db(|s| s.run_detail(id)).unwrap().unwrap()
    }

    async fn wait(&self, id: i64, want: impl Fn(&RunDetail) -> bool) -> RunDetail {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            let r = self.detail(id);
            if want(&r) {
                return r;
            }
            assert!(Instant::now() < deadline, "run {id} did not reach the expected state: {r:?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn finished(&self, id: i64) -> RunDetail {
        self.wait(id, |r| !r.summary.status.is_active()).await
    }

    /// The run's lock is released after the final status is written; wait for
    /// that too, so the next start is not refused as busy.
    async fn idle(&self, project: &str) {
        let lock = groundcontrol::paths::run_lock_path(&self.ctx.state_dir, project);
        let deadline = Instant::now() + Duration::from_secs(10);
        while groundcontrol::lock::is_held(&lock).unwrap() {
            assert!(Instant::now() < deadline, "{project}'s run lock was not released");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn log(&self, id: i64, step: usize) -> String {
        let path = self.ctx.db(|s| s.step_log_path(id, step)).unwrap().expect("step log");
        std::fs::read_to_string(path).unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pipelines_end_to_end() {
    let Some(env) = setup() else { return };

    // Input validation happens before anything runs.
    let (code, msg) = env.start(params("app", "hello", None, &[("name", "Bad Name")], false)).await.unwrap_err();
    assert_eq!(code, ErrorCode::BadRequest);
    assert!(msg.contains("does not match"));

    // Steps, env precedence, masking and outputs. A succeeded run's window closes itself.
    let id = env.start(params("app", "hello", None, &[("name", "sam")], false)).await.unwrap();
    let r = env.finished(id).await;
    assert_eq!(r.summary.status, RunStatus::Succeeded, "{r:?}");
    assert_eq!(r.summary.branch.as_deref(), Some("main"));
    assert!(!r.summary.dirty, "secrets.env is ignored, so the tree is clean");
    let out = env.log(id, 0);
    assert!(out.contains("hi sam hello 120"), "{out}");
    assert!(out.contains("token=***"), "secret must be masked: {out}");
    assert!(!out.contains("sup3r"), "{out}");
    assert_eq!(r.outputs[0].key, "bm");
    assert_eq!(r.outputs[0].value, "abc-123");
    assert!(env.log(id, 1).contains("got abc-123"));
    env.idle("app").await;

    // Dry run executes nothing and lists env var names only.
    let mut dry = params("app", "hello", None, &[("name", "sam")], false);
    dry.dry_run = true;
    let Response::DryRun(plan) = runs::start(&env.ctx, &env.config, dry).await.unwrap() else { panic!() };
    assert_eq!(plan.steps.len(), 2);
    assert!(plan.steps[0].env_names.iter().any(|n| n.starts_with("API_TOKEN")));
    assert!(plan.steps[0].env_names.iter().any(|n| n == "INPUT_NAME"));
    assert!(plan.blockers.is_empty(), "{:?}", plan.blockers);

    // Failure ends the run, skips the rest and keeps the window for reading.
    let id = env.start(params("app", "fail", None, &[], false)).await.unwrap();
    let r = env.finished(id).await;
    assert_eq!(r.summary.status, RunStatus::Failed);
    assert_eq!(r.steps[0].exit_code, Some(7));
    assert_eq!(r.steps[1].status, StepStatus::Skipped);
    env.idle("app").await;
    let failed_window = runs::window_name(id);
    assert!(env.ctx.tmux.window("app", &failed_window).await.unwrap().is_some());

    // Protected runs need the confirmation flag.
    let (code, _) = env.start(params("app", "deploy", Some("prod"), &[], false)).await.unwrap_err();
    assert_eq!(code, ErrorCode::Refused);

    // Approval: pause, busy lock, protected confirm, first answer wins.
    let id = env.start(params("app", "deploy", Some("prod"), &[], true)).await.unwrap();
    assert!(env.ctx.tmux.window("app", &failed_window).await.unwrap().is_none(), "the next run replaces dead run windows");
    let r = env.wait(id, |r| r.summary.status == RunStatus::AwaitingApproval).await;
    assert_eq!(r.approvals[0].message, "Deploy to prod? shared=hello");
    assert_eq!(r.steps[1].status, StepStatus::Waiting);
    let (_, busy) = env.start(params("app", "hello", None, &[("name", "x")], false)).await.unwrap_err();
    assert!(busy.contains(&format!("already has run #{id}")), "{busy}");
    let (code, _) = runs::decide(&env.ctx, id, 1, Decision::Approve, false).unwrap_err();
    assert_eq!(code, ErrorCode::Refused, "protected approval needs the confirm keypress");
    runs::decide(&env.ctx, id, 1, Decision::Approve, true).unwrap();
    let (code, msg) = runs::decide(&env.ctx, id, 1, Decision::Reject, false).unwrap_err();
    assert_eq!(code, ErrorCode::AlreadyDecided);
    assert!(msg.contains("approved"));
    let r = env.finished(id).await;
    assert_eq!(r.summary.status, RunStatus::Succeeded);
    assert!(env.log(id, 2).contains("deployed"));
    env.idle("app").await;

    // Reject is distinct from failed.
    let id = env.start(params("app", "deploy", Some("prod"), &[], true)).await.unwrap();
    env.wait(id, |r| r.summary.status == RunStatus::AwaitingApproval).await;
    runs::decide(&env.ctx, id, 1, Decision::Reject, false).unwrap();
    let r = env.finished(id).await;
    assert_eq!(r.summary.status, RunStatus::Rejected);
    assert_eq!(r.steps[2].status, StepStatus::Skipped);
    env.idle("app").await;

    // Cancel kills the step's process group, even while it prints constantly.
    let id = env.start(params("app", "chatty", None, &[], false)).await.unwrap();
    env.wait(id, |r| r.steps[0].status == StepStatus::Running).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    runs::cancel(&env.ctx, id).unwrap();
    let r = env.finished(id).await;
    assert_eq!(r.summary.status, RunStatus::Cancelled);
    assert_eq!(r.steps[0].status, StepStatus::Cancelled);
    env.idle("app").await;
    assert_eq!(runs::cancel(&env.ctx, id).unwrap_err().0, ErrorCode::NotFound);

    // Closing the window (SIGHUP) cancels the run too.
    let id = env.start(params("app", "slow", None, &[], false)).await.unwrap();
    env.wait(id, |r| r.steps[0].status == StepStatus::Running).await;
    env.ctx.tmux.kill_window("app", &runs::window_name(id)).await.unwrap();
    let r = env.finished(id).await;
    assert_eq!(r.summary.status, RunStatus::Cancelled);
    env.idle("app").await;

    // service: steps start and stop services from inside the run's worker.
    let id = env.start(params("app", "up", None, &[], false)).await.unwrap();
    let r = env.finished(id).await;
    assert_eq!(r.summary.status, RunStatus::Succeeded, "{r:?} {}", env.log(id, 0));
    assert!(env.log(id, 0).contains("web started"), "{}", env.log(id, 0));
    assert!(env.log(id, 1).contains("reachable"));
    env.idle("app").await;

    // Dirty tree is recorded with a saved diff.
    let repo = env.base.join("app");
    std::fs::write(repo.join("README"), "changed\n").unwrap();
    std::fs::write(repo.join("new.txt"), "new\n").unwrap();
    let id = env.start(params("app", "hello", None, &[("name", "sam")], false)).await.unwrap();
    let r = env.finished(id).await;
    assert!(r.summary.dirty);
    assert_eq!(r.diff_summary.as_deref(), Some("1 file changed, 1 untracked"));
    let diff = std::fs::read_to_string(r.diff_path.unwrap()).unwrap();
    assert!(diff.contains("+changed"));
    assert!(diff.contains("new.txt"));

    // History filter.
    let f = RunFilter { project: Some("app".into()), status: Some(RunStatus::Rejected), ..Default::default() };
    assert_eq!(env.ctx.db(|s| s.query_runs(&f, 0)).unwrap().total, 1);

    // Sensitive project: logs stay in the repo.
    let id = env.start(params("sens", "p", None, &[], false)).await.unwrap();
    env.finished(id).await;
    assert!(env.base.join(format!("sens/.groundcontrol/logs/{id}/0.log")).exists());
    assert!(!env.ctx.state_dir.join(format!("logs/{id}")).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn guards_and_interrupted_runs() {
    let Some(env) = setup() else { return };

    // Sensitive project whose .groundcontrol/ is not ignored.
    std::fs::write(env.base.join("sens/.gitignore"), "").unwrap();
    let (_, msg) = env.start(params("sens", "p", None, &[], false)).await.unwrap_err();
    assert!(msg.contains("not gitignored"), "{msg}");

    // A run whose worker never started (or died without a trace) is ended by `reap`.
    let inputs = BTreeMap::new();
    let new = NewRun {
        project: "app",
        pipeline: "slow",
        env: None,
        inputs: &inputs,
        protected: false,
        branch: None,
        commit: None,
        dirty: false,
        unpushed: false,
        unpushed_count: None,
    };
    let pipeline = env.config.project("app").unwrap().pipeline("slow").unwrap();
    let id = env.ctx.db(|s| s.insert_run(&new, &runs::new_steps(pipeline), now() - 60)).unwrap();
    assert_eq!(runs::reap(&env.ctx).await, vec![id]);
    let r = env.detail(id);
    assert_eq!(r.summary.status, RunStatus::Cancelled);
    assert_eq!(r.summary.reason.as_deref(), Some("worker exited unexpectedly"));

    // A live run is left alone.
    let live = env.start(params("app", "slow", None, &[], false)).await.unwrap();
    env.wait(live, |r| r.steps[0].status == StepStatus::Running).await;
    assert!(runs::reap(&env.ctx).await.is_empty());
    runs::cancel(&env.ctx, live).unwrap();
    env.finished(live).await;
    env.idle("app").await;

    // Origin mismatch.
    sh(&env.base.join("app"), "git remote set-url origin https://github.com/someone/else");
    let (_, msg) = env.start(params("app", "hello", None, &[("name", "x")], false)).await.unwrap_err();
    assert!(msg.contains("refusing"), "{msg}");
}
