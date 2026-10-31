//! End-to-end tests: start the real `groundcontrol daemon` binary against a
//! temporary state folder and config, then drive it over the socket.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use groundcontrol::client::{Client, Incoming};
use groundcontrol::protocol::*;
use tokio::sync::mpsc::UnboundedReceiver;

struct Daemon {
    child: Child,
    state: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(self.child.id() as i32), nix::sys::signal::Signal::SIGTERM);
        for _ in 0..100 {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn config(repo: &Path, sens: &Path, port: u16) -> String {
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
        health: {{ url: "http://127.0.0.1:{port}/" }}
      worker:
        start: "sleep 1; exit 3"
        depends_on: [web]
      oneshot:
        start: "exit 4"
        restart: never
    groups:
      stack: [web, worker]
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
      fail:
        steps:
          - run: "echo boom; exit 7"
          - run: echo never
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
    )
}

async fn start_daemon() -> (Daemon, Client, UnboundedReceiver<Incoming>, PathBuf, u16) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("app");
    let sens = dir.path().join("sens");
    git_repo(&repo, "git@github.com:me/app.git");
    git_repo(&sens, "git@github.com:me/sens.git");
    std::fs::write(repo.join("secrets.env"), "API_TOKEN=sup3r-s3cret-value\n").unwrap();
    let port = free_port();
    let cfg = config(&repo, &sens, port);
    let cfg_path = dir.path().join("groundcontrol.yaml");
    std::fs::write(&cfg_path, &cfg).unwrap();
    let state = dir.path().join("state");
    let child = Command::new(env!("CARGO_BIN_EXE_groundcontrol"))
        .arg("daemon")
        .env("GC_STATE_DIR", &state)
        .env("GC_CONFIG", &cfg_path)
        .spawn()
        .unwrap();
    let sock = state.join("gc.sock");
    let mut connected = None;
    for _ in 0..100 {
        if let Ok(c) = Client::connect(&sock).await {
            connected = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (client, events) = connected.expect("daemon socket");
    client
        .request(Request::Subscribe { topics: vec!["runs".into(), "services".into(), "config".into(), "git".into(), "logs".into()] })
        .await
        .unwrap();
    (Daemon { child, state, _dir: dir }, client, events, cfg_path, port)
}

async fn wait_run(client: &Client, id: i64, want: impl Fn(&RunDetail) -> bool) -> RunDetail {
    for _ in 0..200 {
        if let Ok(Response::Run(r)) = client.request(Request::RunGet { id }).await {
            if want(&r) {
                return r;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("run {id} did not reach the expected state");
}

async fn log_text(client: &Client, run: i64, step: usize) -> String {
    match client.request(Request::LogRead { run, step, from_offset: 0 }).await.unwrap() {
        Response::Log(c) => c.data,
        other => panic!("{other:?}"),
    }
}

fn start(project: &str, pipeline: &str, env: Option<&str>, inputs: &[(&str, &str)], confirmed: bool) -> Request {
    Request::RunStart(RunStartParams {
        project: project.into(),
        pipeline: pipeline.into(),
        env: env.map(Into::into),
        inputs: inputs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
        dry_run: false,
        confirmed,
    })
}

fn run_id(r: Response) -> i64 {
    match r {
        Response::RunStarted { id } => id,
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pipelines_end_to_end() {
    let (daemon, client, _events, _cfg, _port) = start_daemon().await;

    // Projects, remote guard and git status.
    let Response::Projects(projects) = client.request(Request::ProjectsList).await.unwrap() else { panic!() };
    assert_eq!(projects.len(), 2);
    assert_eq!(projects[0].pipelines.len(), 4);

    // Input validation happens before anything runs.
    let e = client.request(start("app", "hello", None, &[("name", "Bad Name")], false)).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::BadRequest);
    assert!(e.message.contains("does not match"));

    // Steps, env precedence, masking and outputs.
    let id = run_id(client.request(start("app", "hello", None, &[("name", "sam")], false)).await.unwrap());
    let r = wait_run(&client, id, |r| !r.summary.status.is_active()).await;
    assert_eq!(r.summary.status, RunStatus::Succeeded, "{r:?}");
    assert_eq!(r.summary.branch.as_deref(), Some("main"));
    assert!(!r.summary.dirty, "secrets.env is ignored, so the tree is clean");
    let out = log_text(&client, id, 0).await;
    assert!(out.contains("hi sam hello 120"), "{out}");
    assert!(out.contains("token=***"), "secret must be masked: {out}");
    assert!(!out.contains("sup3r"), "{out}");
    assert_eq!(r.outputs[0].key, "bm");
    assert_eq!(r.outputs[0].value, "abc-123");
    assert!(log_text(&client, id, 1).await.contains("got abc-123"));

    // Dry run executes nothing and lists env var names only.
    let mut dry = start("app", "hello", None, &[("name", "sam")], false);
    if let Request::RunStart(p) = &mut dry {
        p.dry_run = true;
    }
    let Response::DryRun(plan) = client.request(dry).await.unwrap() else { panic!() };
    assert_eq!(plan.steps.len(), 2);
    assert!(plan.steps[0].env_names.iter().any(|n| n.starts_with("API_TOKEN")));
    assert!(plan.steps[0].env_names.iter().any(|n| n == "INPUT_NAME"));
    assert!(plan.blockers.is_empty(), "{:?}", plan.blockers);

    // Failure ends the run and skips the rest.
    let id = run_id(client.request(start("app", "fail", None, &[], false)).await.unwrap());
    let r = wait_run(&client, id, |r| !r.summary.status.is_active()).await;
    assert_eq!(r.summary.status, RunStatus::Failed);
    assert_eq!(r.steps[0].exit_code, Some(7));
    assert_eq!(r.steps[1].status, StepStatus::Skipped);

    // Protected runs need the confirmation flag.
    let e = client.request(start("app", "deploy", Some("prod"), &[], false)).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Refused);

    // Approval: pause, busy lock, protected confirm, first answer wins.
    let id = run_id(client.request(start("app", "deploy", Some("prod"), &[], true)).await.unwrap());
    let r = wait_run(&client, id, |r| r.summary.status == RunStatus::AwaitingApproval).await;
    assert_eq!(r.approvals[0].message, "Deploy to prod? shared=hello");
    assert_eq!(r.steps[1].status, StepStatus::Waiting);
    let busy = client.request(start("app", "hello", None, &[("name", "x")], false)).await.unwrap_err();
    assert!(busy.message.contains("already has run"), "{busy:?}");
    let e = client.request(Request::RunDecide { id, step: 1, decision: Decision::Approve, confirmed: false }).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Refused, "protected approval needs the confirm keypress");
    client.request(Request::RunDecide { id, step: 1, decision: Decision::Approve, confirmed: true }).await.unwrap();
    let again = client.request(Request::RunDecide { id, step: 1, decision: Decision::Reject, confirmed: false }).await.unwrap_err();
    assert_eq!(again.code, ErrorCode::AlreadyDecided);
    assert!(again.message.contains("approved"));
    let r = wait_run(&client, id, |r| !r.summary.status.is_active()).await;
    assert_eq!(r.summary.status, RunStatus::Succeeded);
    assert!(log_text(&client, id, 2).await.contains("deployed"));

    // Reject is distinct from failed.
    let id = run_id(client.request(start("app", "deploy", Some("prod"), &[], true)).await.unwrap());
    wait_run(&client, id, |r| r.summary.status == RunStatus::AwaitingApproval).await;
    client.request(Request::RunDecide { id, step: 1, decision: Decision::Reject, confirmed: false }).await.unwrap();
    let r = wait_run(&client, id, |r| !r.summary.status.is_active()).await;
    assert_eq!(r.summary.status, RunStatus::Rejected);
    assert_eq!(r.steps[2].status, StepStatus::Skipped);

    // Cancel kills the step's process group.
    let id = run_id(client.request(start("app", "slow", None, &[], false)).await.unwrap());
    wait_run(&client, id, |r| r.steps[0].status == StepStatus::Running).await;
    client.request(Request::RunCancel { id }).await.unwrap();
    let r = wait_run(&client, id, |r| !r.summary.status.is_active()).await;
    assert_eq!(r.summary.status, RunStatus::Cancelled);

    // Dirty tree is recorded with a saved diff.
    let repo = daemon.state.parent().unwrap().join("app");
    std::fs::write(repo.join("README"), "changed\n").unwrap();
    std::fs::write(repo.join("new.txt"), "new\n").unwrap();
    let id = run_id(client.request(start("app", "hello", None, &[("name", "sam")], false)).await.unwrap());
    let r = wait_run(&client, id, |r| !r.summary.status.is_active()).await;
    assert!(r.summary.dirty);
    assert_eq!(r.diff_summary.as_deref(), Some("1 file changed, 1 untracked"));
    let Response::Text { text, .. } = client.request(Request::RunDiff { id }).await.unwrap() else { panic!() };
    assert!(text.contains("+changed"));
    assert!(text.contains("new.txt"));

    // History filter.
    let f = RunFilter { project: Some("app".into()), status: Some(RunStatus::Rejected), ..Default::default() };
    let Response::Runs(page) = client.request(Request::RunsQuery { filter: f, page: 0 }).await.unwrap() else { panic!() };
    assert_eq!(page.total, 1);

    // Sensitive project: logs stay in the repo.
    let id = run_id(client.request(start("sens", "p", None, &[], false)).await.unwrap());
    wait_run(&client, id, |r| !r.summary.status.is_active()).await;
    let sens = daemon.state.parent().unwrap().join("sens");
    assert!(sens.join(format!(".groundcontrol/logs/{id}/0.log")).exists());
    assert!(!daemon.state.join(format!("logs/{id}")).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn guards_refuse_runs() {
    let (daemon, client, _events, _cfg, _port) = start_daemon().await;
    let base = daemon.state.parent().unwrap().to_path_buf();
    // Sensitive project whose .groundcontrol/ is not ignored.
    std::fs::write(base.join("sens/.gitignore"), "").unwrap();
    let e = client.request(start("sens", "p", None, &[], false)).await.unwrap_err();
    assert!(e.message.contains("not gitignored"), "{e:?}");
    // Origin mismatch.
    sh(&base.join("app"), "git remote set-url origin https://github.com/someone/else");
    let e = client.request(start("app", "hello", None, &[("name", "x")], false)).await.unwrap_err();
    assert!(e.message.contains("refusing"), "{e:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn services_end_to_end() {
    let (_daemon, client, _events, _cfg, port) = start_daemon().await;
    let status = |projects: &[ProjectInfo], id: &str| projects[0].services.iter().find(|s| s.id == id).unwrap().clone();

    // Port in use by an unmanaged process.
    let blocker = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    let e = client
        .request(Request::ServiceControl { project: "app".into(), id: "web".into(), action: ServiceAction::Start })
        .await
        .unwrap_err();
    assert!(e.message.contains(&format!("port {port} is in use")), "{e:?}");
    assert!(e.message.contains("not managed"));
    drop(blocker);

    // Start with an HTTP health check.
    client.request(Request::ServiceControl { project: "app".into(), id: "web".into(), action: ServiceAction::Start }).await.unwrap();
    let Response::Projects(p) = client.request(Request::ProjectsList).await.unwrap() else { panic!() };
    let web = status(&p, "web");
    assert_eq!(web.state, ServiceState::Running);
    assert!(web.healthy);

    // restart: never with a non-zero exit is reported as a failed start.
    let e = client
        .request(Request::ServiceControl { project: "app".into(), id: "oneshot".into(), action: ServiceAction::Start })
        .await
        .unwrap_err();
    assert!(e.message.contains("exit code 4"), "{e:?}");

    // on-failure restarts with back-off.
    client.request(Request::ServiceControl { project: "app".into(), id: "worker".into(), action: ServiceAction::Start }).await.unwrap();
    let mut restarted = false;
    for _ in 0..60 {
        let Response::Projects(p) = client.request(Request::ProjectsList).await.unwrap() else { panic!() };
        let w = status(&p, "worker");
        if w.restart_count >= 1 {
            restarted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(restarted, "worker should be restarted after crashing");

    // Group stop: reverse order, everything stopped.
    client.request(Request::ServiceControl { project: "app".into(), id: "stack".into(), action: ServiceAction::Stop }).await.unwrap();
    let Response::Projects(p) = client.request(Request::ProjectsList).await.unwrap() else { panic!() };
    assert_eq!(status(&p, "web").state, ServiceState::Stopped);
    assert_eq!(status(&p, "worker").state, ServiceState::Stopped);
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err(), "web must be stopped");

    // Service log captured.
    let Response::Text { text, .. } = client
        .request(Request::ServiceLog { project: "app".into(), id: "web".into(), max_bytes: 10_000 })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(text.contains("starting: python3"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn config_reload_keeps_last_good() {
    let (_daemon, client, mut events, cfg, _port) = start_daemon().await;
    let good = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, good.replace("          - approve: \"Deploy to $GC_ENV? shared=$SHARED\"\n", "")).unwrap();
    let mut status = None;
    for _ in 0..50 {
        if let Ok(Some(Incoming::Event(Event::Config(c)))) = tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
            status = Some(c);
            break;
        }
    }
    let status = status.expect("config event");
    assert!(!status.ok);
    assert!(status.active, "previous config stays active");
    assert!(status.diagnostics.iter().any(|d| d.message.contains("approve") && d.line > 0));
    // The old pipelines still work.
    let Response::Projects(p) = client.request(Request::ProjectsList).await.unwrap() else { panic!() };
    assert_eq!(p[0].pipelines.len(), 4);
    let Response::Health(h) = client.request(Request::HealthGet).await.unwrap() else { panic!() };
    assert!(!h.config.ok);
}
