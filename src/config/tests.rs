use super::*;

fn errors(src: &str) -> Vec<Diagnostic> {
    load_str(src).diagnostics.into_iter().filter(|d| d.severity == Severity::Error).collect()
}

fn has_error(src: &str, needle: &str) -> bool {
    errors(src).iter().any(|d| d.message.contains(needle))
}

const BASE: &str = r#"
env:
  LOG_LEVEL: info
projects:
  app:
    path: /tmp
    remote: github.com/me/app
    environments:
      prod: { protected: true }
      staging: { vars: { REGION: eu } }
    services:
      db:
        start: docker compose up postgres
        health: { port: 5432 }
      api:
        start: go run .
        cwd: backend
        env_file: .env
        depends_on: [db]
        health: { url: "http://127.0.0.1:8080/health" }
        restart: never
    groups:
      local: [db, api]
    pipelines:
      pull:
        steps:
          - run: git pull --ff-only
      new-branch:
        inputs:
          branch: { type: string, pattern: "^[a-z0-9._/-]+$" }
          kind: { type: choice, options: [feat, fix], default: feat }
          draft: { type: bool, default: "false" }
        steps:
          - run: git switch -c "$INPUT_BRANCH"
      deploy:
        environments: [prod, staging]
        steps:
          - name: Test
            run: pnpm test
            timeout: 5m
          - approve: "Deploy to $GC_ENV?"
          - service: restart local
"#;

#[test]
fn pipeline_description_is_parsed() {
    let src = BASE.replace("      pull:\n", "      pull:\n        description: Pulls the latest code.\n");
    assert!(errors(&src).is_empty(), "{:?}", load_str(&src).diagnostics);
    let c = load_str(&src).config.unwrap();
    assert_eq!(c.project("app").unwrap().pipeline("pull").unwrap().description.as_deref(), Some("Pulls the latest code."));
}

#[test]
fn base_config_loads() {
    let r = load_str(BASE);
    assert!(errors(BASE).is_empty(), "{:?}", r.diagnostics);
    let c = r.config.unwrap();
    assert_eq!(c.env, vec![("LOG_LEVEL".to_string(), "info".to_string())]);
    let p = c.project("app").unwrap();
    assert_eq!(p.services.len(), 2);
    assert_eq!(p.service("api").unwrap().restart, RestartPolicy::Never);
    assert_eq!(p.service("api").unwrap().health.as_ref().unwrap().port(), Some(8080));
    let d = p.pipeline("deploy").unwrap();
    assert!(d.is_protected(p, Some("prod")));
    assert!(!d.is_protected(p, Some("staging")));
    assert_eq!(d.steps.len(), 3);
    match &d.steps[0].kind {
        StepKind::Run { timeout, .. } => assert_eq!(*timeout, Duration::from_secs(300)),
        _ => panic!(),
    }
    match &d.steps[2].kind {
        StepKind::Service { action, target } => {
            assert_eq!(*action, ServiceAction::Restart);
            assert_eq!(target, "local");
        }
        _ => panic!(),
    }
    // Pipelines keep declaration order.
    let ids: Vec<_> = p.pipelines.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["pull", "new-branch", "deploy"]);
}

#[test]
fn protected_env_requires_approve() {
    let src = BASE.replace("          - approve: \"Deploy to $GC_ENV?\"\n", "");
    assert!(has_error(&src, "must contain an `approve:` step"));
}

#[test]
fn protected_pipeline_requires_approve() {
    let src = BASE.replace("      pull:\n", "      pull:\n        protected: true\n");
    assert!(has_error(&src, "must contain an `approve:` step"));
}

#[test]
fn blocked_commands() {
    for cmd in [
        "git push --force",
        "git push -f origin main",
        "git push origin main --force-with-lease",
        "git reset --hard HEAD~1",
        "git clean -fdx",
        "git branch -D old",
        "cd x && git push -u -f origin HEAD",
    ] {
        let src = BASE.replace("git pull --ff-only", cmd);
        assert!(has_error(&src, "blocked command"), "{cmd} should be blocked");
    }
    for cmd in ["git push -u origin HEAD", "git branch -d merged", "git fetch --force-ish", "git pull --ff-only"] {
        let src = BASE.replace("git pull --ff-only", cmd);
        assert!(!has_error(&src, "blocked command"), "{cmd} should be allowed");
    }
}

#[test]
fn remote_flag_in_service_start() {
    let src = BASE.replace("start: go run .", "start: wrangler dev --remote");
    assert!(has_error(&src, "--remote"));
    let ok = BASE.replace("start: go run .", "start: wrangler dev --remote-ish-not");
    assert!(!has_error(&ok, "contains `--remote`"));
}

#[test]
fn dependency_cycle() {
    let src = BASE
        .replace("        start: docker compose up postgres\n", "        start: docker compose up postgres\n        depends_on: [api]\n");
    assert!(has_error(&src, "cycle"));
}

#[test]
fn input_rules() {
    assert!(has_error(&BASE.replace(", pattern: \"^[a-z0-9._/-]+$\"", ""), "must have a `pattern`"));
    assert!(has_error(&BASE.replace(", options: [feat, fix]", ""), "must have `options`"));
    assert!(has_error(&BASE.replace("pattern: \"^[a-z0-9._/-]+$\"", "pattern: \"([\""), "not a valid regex"));
}

#[test]
fn unresolved_references() {
    assert!(has_error(&BASE.replace("local: [db, api]", "local: [db, ghost]"), "unknown service `ghost`"));
    assert!(has_error(&BASE.replace("service: restart local", "service: restart nope"), "unknown service or group"));
    assert!(has_error(&BASE.replace("environments: [prod, staging]", "environments: [prod, qa]"), "unknown environment `qa`"));
    assert!(has_error(&BASE.replace("depends_on: [db]", "depends_on: [cache]"), "unknown service `cache`"));
}

#[test]
fn unknown_keys_and_types_have_positions() {
    let src = "projects:\n  app:\n    path: /tmp\n    remot: x\n";
    let errs = errors(src);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("unknown key `remot`"));
    assert_eq!((errs[0].line, errs[0].col), (4, 5));
    let errs = errors("projects:\n  app:\n    path: /tmp\n    environments:\n      prod: { protected: maybe }\n");
    assert_eq!(errs[0].line, 5);
}

#[test]
fn missing_path_and_start() {
    assert!(has_error("projects:\n  app:\n    remote: github.com/a/b\n", "missing the required `path`"));
    assert!(has_error("projects:\n  app:\n    path: /tmp\n    services:\n      x: { cwd: a }\n", "missing the required `start`"));
}

#[test]
fn shared_namespace() {
    let src = BASE.replace("local: [db, api]", "db: [db, api]");
    assert!(has_error(&src, "same id as a service"));
}

#[test]
fn literal_secret_warning() {
    let src = BASE.replace("LOG_LEVEL: info", "API_TOKEN: abc123");
    let r = load_str(&src);
    assert!(r.config.is_some());
    assert!(r.diagnostics.iter().any(|d| d.severity == Severity::Warning && d.message.contains("API_TOKEN")));
    let src = BASE.replace("LOG_LEVEL: info", "API_TOKEN: $FROM_ENV");
    assert!(!load_str(&src).diagnostics.iter().any(|d| d.message.contains("API_TOKEN")));
}

#[test]
fn missing_path_disables_project() {
    let src = BASE.replace("path: /tmp", "path: /definitely/not/here");
    let r = load_str(&src);
    let c = r.config.expect("a missing path is only a warning");
    assert!(c.project("app").unwrap().disabled.is_some());
    assert!(r.diagnostics.iter().any(|d| d.severity == Severity::Warning && d.message.contains("disabled")));
}

#[test]
fn step_shape() {
    let src = BASE.replace("          - run: git pull --ff-only", "          - run: git pull\n            approve: both?");
    assert!(has_error(&src, "exactly one of"));
    let src = BASE.replace("service: restart local", "service: explode local");
    assert!(has_error(&src, "service step must be"));
    let src = BASE.replace("timeout: 5m", "timeout: soon");
    assert!(has_error(&src, "timeout must look like"));
}

#[test]
fn health_rules() {
    assert!(has_error(&BASE.replace("health: { port: 5432 }", "health: { port: 99999 }"), "port number"));
    assert!(has_error(&BASE.replace("http://127.0.0.1:8080/health", "https://x/health"), "http://"));
}

#[test]
fn yaml_errors_are_reported() {
    let r = load_str("projects:\n  app: [\n");
    assert!(r.config.is_none());
    assert!(r.diagnostics[0].message.starts_with("YAML"));
}

#[test]
fn durations() {
    assert_eq!(parse_duration("15m"), Some(Duration::from_secs(900)));
    assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
    assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
    assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
    assert_eq!(parse_duration("0m"), None);
    assert_eq!(parse_duration("x"), None);
}

#[test]
fn repository_example_config_is_valid() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/groundcontrol.yaml")).unwrap();
    let errs = errors(&src);
    assert!(errs.is_empty(), "{errs:#?}");
}
