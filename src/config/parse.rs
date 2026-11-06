//! Converts the positioned YAML tree into typed config, reporting schema
//! errors (required fields, types, unknown keys) with their positions.

use std::path::PathBuf;

use super::*;
use crate::yaml::{Key, Node, Value};

struct Ctx<'a> {
    diags: &'a mut Vec<Diagnostic>,
}

impl Ctx<'_> {
    fn err(&mut self, pos: Pos, msg: impl Into<String>) {
        self.diags.push(Diagnostic::error(pos, msg));
    }

    fn map<'n>(&mut self, node: &'n Node, what: &str) -> Option<&'n [(Key, Node)]> {
        match &node.value {
            Value::Map(entries) => Some(entries),
            Value::Null => Some(&[]),
            _ => {
                self.err(node.pos, format!("{what} must be a mapping, found a {}", node.kind()));
                None
            }
        }
    }

    fn check_keys(&mut self, entries: &[(Key, Node)], allowed: &[&str], what: &str) {
        for (k, _) in entries {
            if !allowed.contains(&k.text.as_str()) {
                self.err(k.pos, format!("unknown key `{}` in {what} (allowed: {})", k.text, allowed.join(", ")));
            }
        }
    }

    fn string(&mut self, node: &Node, what: &str) -> Option<String> {
        match &node.value {
            Value::Scalar { text, .. } => Some(text.clone()),
            _ => {
                self.err(node.pos, format!("{what} must be a string, found a {}", node.kind()));
                None
            }
        }
    }

    fn bool(&mut self, node: &Node, what: &str) -> Option<bool> {
        match node.as_str() {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => {
                self.err(node.pos, format!("{what} must be true or false"));
                None
            }
        }
    }

    fn string_list(&mut self, node: &Node, what: &str) -> Vec<String> {
        match &node.value {
            Value::Seq(items) => items.iter().filter_map(|i| self.string(i, what)).collect(),
            Value::Null => Vec::new(),
            _ => {
                self.err(node.pos, format!("{what} must be a list"));
                Vec::new()
            }
        }
    }

    fn string_map(&mut self, node: &Node, what: &str) -> Vec<(String, String)> {
        let Some(entries) = self.map(node, what) else { return Vec::new() };
        let mut out = Vec::new();
        for (k, v) in entries {
            if !is_env_name(&k.text) {
                self.err(k.pos, format!("`{}` is not a valid environment variable name", k.text));
            }
            let value = match &v.value {
                Value::Null => String::new(),
                _ => self.string(v, &format!("{what}.{}", k.text)).unwrap_or_default(),
            };
            if super::validate::looks_secret(&k.text) && !value.is_empty() && !value.starts_with('$') {
                self.diags.push(Diagnostic::warning(
                    k.pos,
                    format!(
                        "`{}` in {what} looks like a secret with a literal value; the config is in git, move it to an env_file",
                        k.text
                    ),
                ));
            }
            out.push((k.text.clone(), value));
        }
        out
    }

    fn id(&mut self, key: &Key, what: &str) -> bool {
        let ok = !key.text.is_empty() && key.text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
        if !ok {
            self.err(key.pos, format!("{what} id `{}` may only contain letters, digits, `-`, `_` and `.`", key.text));
        }
        ok
    }
}

pub fn is_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(super) fn config_from_node(root: &Node, diags: &mut Vec<Diagnostic>) -> Option<Config> {
    let mut cx = Ctx { diags };
    let entries = cx.map(root, "the config file")?;
    cx.check_keys(entries, &["env", "projects"], "the top level");
    let mut config = Config::default();
    for (k, v) in entries {
        match k.text.as_str() {
            "env" => config.env = cx.string_map(v, "env"),
            "projects" => {
                if let Some(projects) = cx.map(v, "projects") {
                    for (pk, pv) in projects {
                        if cx.id(pk, "project")
                            && let Some(p) = project(&mut cx, pk, pv)
                        {
                            config.projects.push(p);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Some(config)
}

fn project(cx: &mut Ctx, key: &Key, node: &Node) -> Option<Project> {
    let entries = cx.map(node, &format!("project `{}`", key.text))?;
    cx.check_keys(entries, &["path", "remote", "environments", "services", "groups", "pipelines"], &format!("project `{}`", key.text));
    let mut p = Project {
        id: key.text.clone(),
        pos: key.pos,
        path_display: String::new(),
        path: PathBuf::new(),
        remote: None,
        environments: Vec::new(),
        services: Vec::new(),
        groups: Vec::new(),
        pipelines: Vec::new(),
        disabled: None,
    };
    let mut has_path = false;
    for (k, v) in entries {
        match k.text.as_str() {
            "path" => {
                if let Some(s) = cx.string(v, "path") {
                    p.path = crate::paths::expand_tilde(&PathBuf::from(&s));
                    p.path_display = s;
                    has_path = true;
                }
            }
            "remote" => p.remote = cx.string(v, "remote"),
            "environments" => {
                for (ek, ev) in cx.map(v, "environments").unwrap_or(&[]) {
                    if cx.id(ek, "environment")
                        && let Some(e) = environment(cx, ek, ev)
                    {
                        p.environments.push(e);
                    }
                }
            }
            "services" => {
                for (sk, sv) in cx.map(v, "services").unwrap_or(&[]) {
                    if cx.id(sk, "service")
                        && let Some(s) = service(cx, sk, sv)
                    {
                        p.services.push(s);
                    }
                }
            }
            "groups" => {
                for (gk, gv) in cx.map(v, "groups").unwrap_or(&[]) {
                    if cx.id(gk, "group") {
                        let members = cx.string_list(gv, &format!("group `{}`", gk.text));
                        p.groups.push(Group { id: gk.text.clone(), pos: gk.pos, members });
                    }
                }
            }
            "pipelines" => {
                for (pk, pv) in cx.map(v, "pipelines").unwrap_or(&[]) {
                    if cx.id(pk, "pipeline")
                        && let Some(pl) = pipeline(cx, pk, pv)
                    {
                        p.pipelines.push(pl);
                    }
                }
            }
            _ => {}
        }
    }
    if !has_path {
        cx.err(key.pos, format!("project `{}` is missing the required `path`", key.text));
        return None;
    }
    Some(p)
}

fn environment(cx: &mut Ctx, key: &Key, node: &Node) -> Option<Environment> {
    let entries = cx.map(node, &format!("environment `{}`", key.text))?;
    cx.check_keys(entries, &["protected", "vars"], &format!("environment `{}`", key.text));
    let mut e = Environment { name: key.text.clone(), pos: key.pos, protected: false, vars: Vec::new() };
    for (k, v) in entries {
        match k.text.as_str() {
            "protected" => e.protected = cx.bool(v, "protected").unwrap_or(false),
            "vars" => e.vars = cx.string_map(v, "vars"),
            _ => {}
        }
    }
    Some(e)
}

fn service(cx: &mut Ctx, key: &Key, node: &Node) -> Option<Service> {
    let what = format!("service `{}`", key.text);
    let entries = cx.map(node, &what)?;
    cx.check_keys(entries, &["start", "cwd", "env_file", "health", "depends_on", "restart"], &what);
    let mut s = Service {
        id: key.text.clone(),
        pos: key.pos,
        start: String::new(),
        cwd: None,
        env_file: None,
        health: None,
        depends_on: Vec::new(),
        restart: RestartPolicy::OnFailure,
    };
    let mut has_start = false;
    for (k, v) in entries {
        match k.text.as_str() {
            "start" => {
                if let Some(c) = cx.string(v, "start") {
                    s.start = c;
                    has_start = true;
                }
            }
            "cwd" => s.cwd = cx.string(v, "cwd"),
            "env_file" => s.env_file = cx.string(v, "env_file"),
            "depends_on" => s.depends_on = cx.string_list(v, "depends_on"),
            "restart" => match v.as_str() {
                Some("on-failure") => s.restart = RestartPolicy::OnFailure,
                Some("never") => s.restart = RestartPolicy::Never,
                _ => cx.err(v.pos, "restart must be `on-failure` or `never`"),
            },
            "health" => s.health = health(cx, v),
            _ => {}
        }
    }
    if !has_start {
        cx.err(key.pos, format!("{what} is missing the required `start` command"));
        return None;
    }
    Some(s)
}

fn health(cx: &mut Ctx, node: &Node) -> Option<Health> {
    let entries = cx.map(node, "health")?;
    cx.check_keys(entries, &["url", "port"], "health");
    if entries.len() != 1 {
        cx.err(node.pos, "health must have exactly one of `url` or `port`");
        return None;
    }
    let (k, v) = &entries[0];
    match k.text.as_str() {
        "url" => {
            let url = cx.string(v, "health.url")?;
            if parse_http_url(&url).is_none() {
                cx.err(v.pos, format!("health url `{url}` must be an http:// URL with a host (https is not supported for local checks)"));
                return None;
            }
            Some(Health::Url(url))
        }
        "port" => match v.as_str().and_then(|s| s.parse::<u16>().ok()) {
            Some(p) if p > 0 => Some(Health::Port(p)),
            _ => {
                cx.err(v.pos, "health.port must be a port number between 1 and 65535");
                None
            }
        },
        _ => None,
    }
}

fn pipeline(cx: &mut Ctx, key: &Key, node: &Node) -> Option<Pipeline> {
    let what = format!("pipeline `{}`", key.text);
    let entries = cx.map(node, &what)?;
    cx.check_keys(entries, &["protected", "environments", "inputs", "steps", "warning"], &what);
    let mut p = Pipeline {
        id: key.text.clone(),
        pos: key.pos,
        protected: false,
        environments: Vec::new(),
        inputs: Vec::new(),
        steps: Vec::new(),
        warning: None,
    };
    let mut has_steps = false;
    for (k, v) in entries {
        match k.text.as_str() {
            "protected" => p.protected = cx.bool(v, "protected").unwrap_or(false),
            "environments" => p.environments = cx.string_list(v, "environments"),
            "warning" => p.warning = cx.string(v, "warning"),
            "inputs" => {
                for (ik, iv) in cx.map(v, "inputs").unwrap_or(&[]) {
                    if !is_env_name(&ik.text) {
                        cx.err(ik.pos, format!("input name `{}` must be usable as an environment variable name", ik.text));
                        continue;
                    }
                    if let Some(i) = input(cx, ik, iv) {
                        p.inputs.push(i);
                    }
                }
            }
            "steps" => match &v.value {
                Value::Seq(items) => {
                    has_steps = true;
                    for item in items {
                        if let Some(s) = step(cx, item) {
                            p.steps.push(s);
                        }
                    }
                }
                _ => cx.err(v.pos, "steps must be a list"),
            },
            _ => {}
        }
    }
    if !has_steps || p.steps.is_empty() {
        cx.err(key.pos, format!("{what} needs at least one step"));
    }
    Some(p)
}

fn input(cx: &mut Ctx, key: &Key, node: &Node) -> Option<Input> {
    let what = format!("input `{}`", key.text);
    let entries = cx.map(node, &what)?;
    cx.check_keys(entries, &["type", "pattern", "options", "default"], &what);
    let mut i = Input { name: key.text.clone(), pos: key.pos, kind: InputKind::String, pattern: None, options: Vec::new(), default: None };
    for (k, v) in entries {
        match k.text.as_str() {
            "type" => match v.as_str() {
                Some("string") => i.kind = InputKind::String,
                Some("bool") => i.kind = InputKind::Bool,
                Some("choice") => i.kind = InputKind::Choice,
                _ => cx.err(v.pos, "input type must be string, bool or choice"),
            },
            "pattern" => i.pattern = cx.string(v, "pattern"),
            "options" => i.options = cx.string_list(v, "options"),
            "default" => i.default = cx.string(v, "default"),
            _ => {}
        }
    }
    Some(i)
}

fn step(cx: &mut Ctx, node: &Node) -> Option<Step> {
    let entries = cx.map(node, "a step")?;
    cx.check_keys(entries, &["name", "run", "cwd", "env_file", "timeout", "approve", "service"], "a step");
    let get = |name: &str| entries.iter().find(|(k, _)| k.text == name).map(|(_, v)| v);
    let name = get("name").and_then(|v| cx.string(v, "name"));
    let kinds: Vec<&str> = ["run", "approve", "service"].into_iter().filter(|k| get(k).is_some()).collect();
    if kinds.len() != 1 {
        cx.err(node.pos, "a step needs exactly one of `run`, `approve` or `service`");
        return None;
    }
    let kind = match kinds[0] {
        "run" => {
            let run = cx.string(get("run")?, "run")?;
            let timeout = match get("timeout") {
                Some(t) => match t.as_str().and_then(parse_duration) {
                    Some(d) => d,
                    None => {
                        cx.err(t.pos, "timeout must look like `30s`, `15m` or `1h`");
                        DEFAULT_STEP_TIMEOUT
                    }
                },
                None => DEFAULT_STEP_TIMEOUT,
            };
            StepKind::Run {
                run,
                cwd: get("cwd").and_then(|v| cx.string(v, "cwd")),
                env_file: get("env_file").and_then(|v| cx.string(v, "env_file")),
                timeout,
            }
        }
        "approve" => {
            for extra in ["cwd", "env_file", "timeout"] {
                if let Some(v) = get(extra) {
                    cx.err(v.pos, format!("`{extra}` has no effect on an approve step"));
                }
            }
            StepKind::Approve { message: cx.string(get("approve")?, "approve")? }
        }
        _ => {
            let v = get("service")?;
            for extra in ["cwd", "env_file", "timeout"] {
                if let Some(x) = get(extra) {
                    cx.err(x.pos, format!("`{extra}` has no effect on a service step"));
                }
            }
            let text = cx.string(v, "service")?;
            let mut parts = text.split_whitespace();
            let action = match parts.next() {
                Some("start") => ServiceAction::Start,
                Some("stop") => ServiceAction::Stop,
                Some("restart") => ServiceAction::Restart,
                _ => {
                    cx.err(v.pos, "service step must be `start|stop|restart <service-or-group>`");
                    return None;
                }
            };
            let (Some(target), None) = (parts.next(), parts.next()) else {
                cx.err(v.pos, "service step must name exactly one service or group");
                return None;
            };
            StepKind::Service { action, target: target.to_string() }
        }
    };
    Some(Step { name, pos: node.pos, kind })
}
