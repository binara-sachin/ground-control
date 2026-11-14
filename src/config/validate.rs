//! Semantic validation rules (§7.3). The text checks are seatbelts, not guarantees.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use super::*;

/// Commands that are never allowed in a step: (pattern, description).
static BLOCKED: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let sep = r"[^\n;&|]*";
    [
        (format!(r"\bpush\b{sep}\s(--force|--force-with-lease|-f)(\s|=|$)"), "push --force"),
        (format!(r"\bpush\b{sep}\s-[a-zA-Z]*f[a-zA-Z]*(\s|$)"), "push -f"),
        (format!(r"\breset\b{sep}\s--hard\b"), "reset --hard"),
        (format!(r"\bclean\b{sep}\s(-[a-zA-Z]*f|--force)"), "clean -f"),
        (format!(r"\bbranch\b{sep}\s(-[a-zA-Z]*D|--delete\s+--force|--force\s+--delete)"), "branch -D"),
    ]
    .into_iter()
    .map(|(re, name)| (Regex::new(&re).unwrap(), name))
    .collect()
});

static REMOTE_FLAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^|\s)--remote(\s|=|$)").unwrap());

/// Returns the blocked command found in `cmd`, if any.
fn blocked_command(cmd: &str) -> Option<&'static str> {
    BLOCKED.iter().find(|(re, _)| re.is_match(cmd)).map(|(_, name)| *name)
}

fn has_remote_flag(cmd: &str) -> bool {
    REMOTE_FLAG.is_match(cmd)
}

pub(super) fn looks_secret(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    ["TOKEN", "SECRET", "KEY", "PASSWORD"].iter().any(|w| k.contains(w))
}

pub fn validate(config: &mut Config, diags: &mut Vec<Diagnostic>) {
    if !config.dev_dir.is_dir() {
        // No position is kept for the key; line 1 is where the file starts.
        diags.push(Diagnostic::warning(
            Pos { line: 1, col: 1 },
            format!("dev_dir {} is not a folder; the Projects screen has nothing to scan", config.dev_dir_display),
        ));
    }
    for project in &mut config.projects {
        validate_project(project, diags);
    }
}

fn validate_project(p: &mut Project, diags: &mut Vec<Diagnostic>) {
    let pid = p.id.clone();

    if let Some(remote) = &p.remote
        && crate::git::normalize_remote(remote).is_none()
    {
        diags.push(Diagnostic::error(p.pos, format!("project `{pid}`: remote `{remote}` is not a recognisable git remote")));
    }

    // Services and groups share a namespace.
    let service_ids: HashSet<&str> = p.services.iter().map(|s| s.id.as_str()).collect();
    for g in &p.groups {
        if service_ids.contains(g.id.as_str()) {
            diags.push(Diagnostic::error(g.pos, format!("group `{}` has the same id as a service in `{pid}`", g.id)));
        }
        if g.members.is_empty() {
            diags.push(Diagnostic::error(g.pos, format!("group `{}` has no services", g.id)));
        }
        for m in &g.members {
            if !service_ids.contains(m.as_str()) {
                diags.push(Diagnostic::error(g.pos, format!("group `{}` refers to unknown service `{m}`", g.id)));
            }
        }
    }

    for s in &p.services {
        if has_remote_flag(&s.start) {
            diags.push(Diagnostic::error(
                s.pos,
                format!("service `{}` start command contains `--remote`; local stacks must never touch remote resources", s.id),
            ));
        }
        if let Some(b) = blocked_command(&s.start) {
            diags.push(Diagnostic::error(s.pos, format!("service `{}` start command contains blocked `{b}`", s.id)));
        }
        for d in &s.depends_on {
            if d == &s.id {
                diags.push(Diagnostic::error(s.pos, format!("service `{}` depends on itself", s.id)));
            } else if !service_ids.contains(d.as_str()) {
                diags.push(Diagnostic::error(s.pos, format!("service `{}` depends on unknown service `{d}`", s.id)));
            }
        }
    }
    if let Some(cycle) = find_cycle(&p.services) {
        let pos = p.service(&cycle[0]).map(|s| s.pos).unwrap_or(p.pos);
        diags.push(Diagnostic::error(pos, format!("depends_on cycle in `{pid}`: {}", cycle.join(" → "))));
    }

    for pl in &p.pipelines {
        validate_pipeline(p, pl, &service_ids, diags);
    }

    if !p.path.is_dir() {
        p.disabled = Some(format!("path {} does not exist", p.path_display));
        diags.push(Diagnostic::warning(p.pos, format!("project `{pid}`: path {} does not exist; the project is disabled", p.path_display)));
    }
}

fn validate_pipeline(p: &Project, pl: &Pipeline, service_ids: &HashSet<&str>, diags: &mut Vec<Diagnostic>) {
    let what = format!("pipeline `{}`", pl.id);
    for e in &pl.environments {
        if p.environment(e).is_none() {
            diags.push(Diagnostic::error(pl.pos, format!("{what} refers to unknown environment `{e}`")));
        }
    }
    let runs_on_protected_env = pl.environments.iter().any(|e| p.environment(e).is_some_and(|e| e.protected));
    let has_approve = pl.steps.iter().any(|s| matches!(s.kind, StepKind::Approve { .. }));
    if (pl.protected || runs_on_protected_env) && !has_approve {
        diags.push(Diagnostic::error(
            pl.pos,
            format!("{what} is protected (or can run on a protected environment) and must contain an `approve:` step"),
        ));
    }

    let mut seen = HashSet::new();
    for i in &pl.inputs {
        if !seen.insert(i.name.to_ascii_uppercase()) {
            diags.push(Diagnostic::error(i.pos, format!("input `{}` clashes with another input in {what}", i.name)));
        }
        match i.kind {
            InputKind::String => match &i.pattern {
                None => diags.push(Diagnostic::error(i.pos, format!("string input `{}` must have a `pattern`", i.name))),
                Some(pat) => match Regex::new(pat) {
                    Err(e) => diags.push(Diagnostic::error(i.pos, format!("input `{}` pattern is not a valid regex: {e}", i.name))),
                    Ok(re) => {
                        if let Some(d) = &i.default
                            && !re.is_match(d)
                        {
                            diags.push(Diagnostic::error(i.pos, format!("input `{}` default does not match its pattern", i.name)));
                        }
                    }
                },
            },
            InputKind::Choice => {
                if i.options.is_empty() {
                    diags.push(Diagnostic::error(i.pos, format!("choice input `{}` must have `options`", i.name)));
                }
                if let Some(d) = &i.default
                    && !i.options.contains(d)
                {
                    diags.push(Diagnostic::error(i.pos, format!("input `{}` default is not one of its options", i.name)));
                }
            }
            InputKind::Bool => {
                if let Some(d) = &i.default
                    && d != "true"
                    && d != "false"
                {
                    diags.push(Diagnostic::error(i.pos, format!("bool input `{}` default must be true or false", i.name)));
                }
            }
        }
    }

    for s in &pl.steps {
        match &s.kind {
            StepKind::Run { run, .. } => {
                if let Some(b) = blocked_command(run) {
                    diags.push(Diagnostic::error(s.pos, format!("{what}: step contains blocked command `{b}`")));
                }
            }
            StepKind::Service { target, .. } => {
                if !service_ids.contains(target.as_str()) && p.group(target).is_none() {
                    diags.push(Diagnostic::error(s.pos, format!("{what}: unknown service or group `{target}`")));
                }
            }
            StepKind::Approve { message } => {
                if message.trim().is_empty() {
                    diags.push(Diagnostic::error(s.pos, format!("{what}: approve message is empty")));
                }
            }
        }
    }
}

/// Returns one dependency cycle as a list of service ids, if any exists.
fn find_cycle(services: &[Service]) -> Option<Vec<String>> {
    let deps: HashMap<&str, &[String]> = services.iter().map(|s| (s.id.as_str(), s.depends_on.as_slice())).collect();
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    fn visit<'a>(
        n: &'a str,
        deps: &HashMap<&'a str, &'a [String]>,
        marks: &mut HashMap<&'a str, Mark>,
        path: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        match marks.get(n) {
            Some(Mark::Done) => return None,
            Some(Mark::Visiting) => {
                let start = path.iter().position(|p| *p == n).unwrap_or(0);
                let mut cycle: Vec<String> = path[start..].iter().map(|s| s.to_string()).collect();
                cycle.push(n.to_string());
                return Some(cycle);
            }
            None => {}
        }
        marks.insert(n, Mark::Visiting);
        path.push(n);
        for d in deps.get(n).copied().unwrap_or(&[]) {
            if d != n
                && deps.contains_key(d.as_str())
                && let Some(c) = visit(d.as_str(), deps, marks, path)
            {
                return Some(c);
            }
        }
        path.pop();
        marks.insert(n, Mark::Done);
        None
    }
    let mut marks = HashMap::new();
    for s in services {
        if let Some(c) = visit(&s.id, &deps, &mut marks, &mut Vec::new()) {
            return Some(c);
        }
    }
    None
}
