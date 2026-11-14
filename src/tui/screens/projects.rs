//! Projects: every git repository under `dev_dir` (default `~/dev`) as a tree,
//! with the git state `gstat` shows and a fetch, push and pull button in front
//! of each repository. These are not the projects of `groundcontrol.yaml`:
//! anything with a `.git` counts, registered or not. Reading is local; nothing
//! is fetched until a button or key asks for it.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::Instant;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::super::app::{App, Level, Reply};
use super::super::mouse::Target;
use super::super::theme;
use super::super::widgets::*;
use crate::model::*;

/// How long the list is left alone while the screen is open.
pub const REFRESH_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// Width of one button: an icon with a cell of room either side, so it is a
/// target a pointer can hit.
const BUTTON_W: u16 = 3;
const BUTTONS_W: u16 = BUTTON_W * 3;
const FETCH_ICON: &str = "↻";
const PUSH_ICON: &str = "↑";
const PULL_ICON: &str = "↓";

#[derive(Default)]
pub struct DevState {
    pub scan: Option<DevScan>,
    pub loading: bool,
    pub only_attention: bool,
    /// The selected repository, by path, so a refresh does not move it.
    pub selected: Option<String>,
    pub busy: HashMap<String, DevOp>,
    pub top: Cell<usize>,
    pub loaded_at: Option<Instant>,
    batch: Option<Batch>,
}

/// A "fetch all" in progress: one toast when the last one finishes.
struct Batch {
    left: usize,
    total: usize,
    failed: Vec<String>,
}

/// One line of the tree: a repository, or a folder that only groups some.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevRow {
    pub path: String,
    /// Tree connectors drawn before the name.
    pub prefix: String,
    pub name: String,
    /// Index into the scan's repos; `None` for a grouping folder.
    pub repo: Option<usize>,
}

/// The tree, in order. A folder holding exactly one thing is folded into it
/// (`csm-portal-improvements/cs-tools`), and with `only_attention` a repository
/// stays only when it needs attention, a folder only when something below it does.
pub fn tree_rows(repos: &[DevRepo], only_attention: bool) -> Vec<DevRow> {
    let mut kids: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut known: BTreeSet<String> = BTreeSet::new();
    let mut by_path: HashMap<&str, usize> = HashMap::new();
    for (i, r) in repos.iter().enumerate() {
        by_path.insert(&r.path, i);
        let mut parent = String::new();
        for part in r.path.split('/') {
            let path = if parent.is_empty() { part.to_string() } else { format!("{parent}/{part}") };
            if known.insert(path.clone()) {
                kids.entry(parent.clone()).or_default().push(path.clone());
            }
            parent = path;
        }
    }
    for k in kids.values_mut() {
        k.sort();
    }

    // Shown when it needs attention itself, or something below it does.
    fn shown(path: &str, repos: &[DevRepo], by_path: &HashMap<&str, usize>, kids: &BTreeMap<String, Vec<String>>) -> bool {
        by_path.get(path).is_some_and(|&i| repos[i].needs_attention())
            || kids.get(path).is_some_and(|ks| ks.iter().any(|k| shown(k, repos, by_path, kids)))
    }
    fn fold(mut path: String, by_path: &HashMap<&str, usize>, kids: &BTreeMap<String, Vec<String>>) -> String {
        while !by_path.contains_key(path.as_str()) && kids.get(&path).is_some_and(|k| k.len() == 1) {
            path = kids[&path][0].clone();
        }
        path
    }
    fn walk(
        path: &str,
        prefix: &str,
        only: bool,
        repos: &[DevRepo],
        by_path: &HashMap<&str, usize>,
        kids: &BTreeMap<String, Vec<String>>,
        out: &mut Vec<DevRow>,
    ) {
        let targets: Vec<String> = kids
            .get(path)
            .map(|ks| ks.iter().map(|k| fold(k.clone(), by_path, kids)).filter(|t| !only || shown(t, repos, by_path, kids)).collect())
            .unwrap_or_default();
        for (i, target) in targets.iter().enumerate() {
            let last = i + 1 == targets.len();
            let name = if path.is_empty() { target.clone() } else { target[path.len() + 1..].to_string() };
            out.push(DevRow {
                path: target.clone(),
                prefix: format!("{prefix}{}", if last { "└── " } else { "├── " }),
                name,
                repo: by_path.get(target.as_str()).copied(),
            });
            walk(target, &format!("{prefix}{}", if last { "    " } else { "│   " }), only, repos, by_path, kids, out);
        }
    }
    let mut out = Vec::new();
    walk("", "", only_attention, repos, &by_path, &kids, &mut out);
    out
}

fn rows(app: &App) -> Vec<DevRow> {
    app.dev.scan.as_ref().map(|s| tree_rows(&s.repos, app.dev.only_attention)).unwrap_or_default()
}

/// The selected row's index: the selected repository if it is still shown,
/// else the first repository.
fn selected_index(app: &App, rows: &[DevRow]) -> Option<usize> {
    let sel = app.dev.selected.as_deref();
    rows.iter().position(|r| r.repo.is_some() && Some(r.path.as_str()) == sel).or_else(|| rows.iter().position(|r| r.repo.is_some()))
}

// ── actions ──

impl App {
    /// Reads the folder again. One read at a time.
    pub fn fetch_dev_scan(&mut self) {
        if self.engine.is_none() || self.dev.loading {
            return;
        }
        self.dev.loading = true;
        self.call(|e| async move { Reply::DevScan(e.dev_scan().await) });
    }
}

pub fn on_scan(app: &mut App, scan: DevScan) {
    app.dev.loading = false;
    app.dev.loaded_at = Some(Instant::now());
    if app.dev.selected.as_ref().is_some_and(|s| !scan.repos.iter().any(|r| &r.path == s)) {
        app.dev.selected = None;
    }
    app.dev.scan = Some(scan);
}

fn repo<'a>(app: &'a App, path: &str) -> Option<&'a DevRepo> {
    app.dev.scan.as_ref()?.repos.iter().find(|r| r.path == path)
}

/// Starts one action on one repository. Returns whether it started.
fn start_op(app: &mut App, path: &str, op: DevOp, in_batch: bool) -> bool {
    let Some(r) = repo(app, path) else { return false };
    if app.dev.busy.contains_key(path) {
        if !in_batch {
            app.toast(format!("{path} is busy"), Level::Info);
        }
        return false;
    }
    if !r.has_remote() {
        if !in_batch {
            app.toast(format!("{path}: no remote configured"), Level::Info);
        }
        return false;
    }
    app.dev.busy.insert(path.to_string(), op);
    let path = path.to_string();
    app.call(move |e| async move {
        let result = e.dev_op(&path, op).await;
        Reply::DevOp { path, op, in_batch, result }
    });
    true
}

fn fetch_all(app: &mut App) {
    let paths: Vec<String> = app.dev.scan.iter().flat_map(|s| s.repos.iter().filter(|r| r.has_remote()).map(|r| r.path.clone())).collect();
    let started = paths.iter().filter(|p| start_op(app, p, DevOp::Fetch, true)).count();
    if started == 0 {
        app.toast("nothing to fetch", Level::Info);
    } else {
        app.dev.batch = Some(Batch { left: started, total: started, failed: Vec::new() });
    }
}

pub fn on_op_done(app: &mut App, path: String, op: DevOp, in_batch: bool, result: Result<String, String>) {
    app.dev.busy.remove(&path);
    if in_batch {
        let mut done = None;
        if let Some(b) = &mut app.dev.batch {
            b.left = b.left.saturating_sub(1);
            if let Err(e) = &result {
                b.failed.push(format!("{path}: {e}"));
            }
            if b.left == 0 {
                done = app.dev.batch.take();
            }
        }
        if let Some(b) = done {
            if b.failed.is_empty() {
                app.toast(format!("fetched {} repositories", b.total), Level::Success);
            } else {
                app.toast(format!("{} of {} failed to fetch: {}", b.failed.len(), b.total, b.failed.join("; ")), Level::Error);
            }
        }
    } else {
        match result {
            Ok(done) => app.toast(format!("{path}: {done}"), Level::Success),
            Err(e) => app.toast(format!("{path}: {} failed: {e}", op.verb()), Level::Error),
        }
    }
    app.fetch_dev_scan();
}

/// The project of `groundcontrol.yaml` that lives in this folder, if any.
fn registered_project(app: &App, path: &str) -> Option<String> {
    let root = crate::paths::expand_tilde(Path::new(&app.dev.scan.as_ref()?.root));
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    let here = root.join(path);
    app.projects.iter().find(|p| same(&crate::paths::expand_tilde(Path::new(&p.path)), &here)).map(|p| p.id.clone())
}

// ── keys and clicks ──

pub fn key(app: &mut App, k: KeyEvent) {
    let rows = rows(app);
    let current = selected_index(app, &rows);
    match k.code {
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
            let down = matches!(k.code, KeyCode::Down | KeyCode::Char('j'));
            let from = current.unwrap_or(0);
            let next = if down {
                (from + 1..rows.len()).find(|&i| rows[i].repo.is_some())
            } else {
                (0..from).rev().find(|&i| rows[i].repo.is_some())
            };
            if let Some(i) = next.or(current) {
                app.dev.selected = Some(rows[i].path.clone());
            }
        }
        KeyCode::Char('f') | KeyCode::Char('p') | KeyCode::Char('P') => {
            let op = match k.code {
                KeyCode::Char('f') => DevOp::Fetch,
                KeyCode::Char('p') => DevOp::Pull,
                _ => DevOp::Push,
            };
            if let Some(i) = current {
                start_op(app, &rows[i].path.clone(), op, false);
            }
        }
        KeyCode::Char('F') => fetch_all(app),
        KeyCode::Char('e') => app.dev.only_attention = !app.dev.only_attention,
        KeyCode::Char('r') => app.fetch_dev_scan(),
        KeyCode::Enter => {
            let Some(i) = current else { return };
            match registered_project(app, &rows[i].path) {
                Some(id) => app.open_project(&id),
                None => {
                    app.toast(format!("{} is not in groundcontrol.yaml, so there is no project screen for it", rows[i].path), Level::Info)
                }
            }
        }
        _ => {}
    }
}

/// A click selects the row under the pointer; on a button it also starts the
/// action. Returns whether that was a repository row.
pub fn click(app: &mut App, target: Target, row: usize) -> bool {
    let rows = rows(app);
    let Some(r) = rows.get(row).filter(|r| r.repo.is_some()) else { return false };
    app.dev.selected = Some(r.path.clone());
    let op = match target {
        Target::DevFetch => DevOp::Fetch,
        Target::DevPush => DevOp::Push,
        Target::DevPull => DevOp::Pull,
        _ => return true,
    };
    start_op(app, &r.path.clone(), op, false);
    true
}

pub fn hints(_app: &App) -> Vec<(&'static str, &'static str)> {
    vec![
        ("↑↓", "select"),
        ("f", "fetch"),
        ("p", "pull"),
        ("P", "push"),
        ("F", "fetch all"),
        ("e", "needs attention"),
        ("r", "refresh"),
        ("enter", "open"),
    ]
}

// ── drawing ──

/// How long ago the repository was fetched. The behind counts are as old as
/// this, and nothing fetches on its own, so a repository that never was says so.
fn fetched_span(r: &DevRepo) -> Span<'static> {
    if !r.has_remote() {
        return Span::styled("-", theme::dim());
    }
    match r.last_fetch {
        Some(t) => Span::styled(fmt_age(now() - t), theme::muted()),
        None => Span::styled("never", theme::warning()),
    }
}

/// The dashboard's tree column: "clean", or "4M 1?" (modified, unmerged, untracked).
fn tree_cell(r: &DevRepo) -> (String, ratatui::style::Style) {
    if r.upstream == DevUpstream::Error {
        return ("error".into(), theme::danger());
    }
    let mut parts = Vec::new();
    if r.staged + r.unstaged > 0 {
        parts.push(format!("{}M", r.staged + r.unstaged));
    }
    if r.conflicted > 0 {
        parts.push(format!("{}U", r.conflicted));
    }
    if r.untracked > 0 {
        parts.push(format!("{}?", r.untracked));
    }
    if parts.is_empty() { ("clean".into(), theme::success()) } else { (parts.join(" "), theme::warning()) }
}

/// The dashboard's `↑ ↓` column: commits ahead and behind as of the last fetch.
fn ahead_behind(r: &DevRepo) -> String {
    match r.upstream {
        DevUpstream::Tracking | DevUpstream::Matching => format!("{} {}", r.ahead, r.behind),
        _ => "- -".into(),
    }
}

/// The three buttons of one row. The busy one shows `…`; the others wait.
fn button_spans(r: &DevRepo, busy: Option<DevOp>) -> Vec<Vec<Span<'static>>> {
    let button = |icon: &str, op: DevOp, live: ratatui::style::Style| {
        let (glyph, style) = match busy {
            Some(b) if b == op => ("…", theme::info()),
            Some(_) => (icon, theme::dim()),
            None if !r.has_remote() => (icon, theme::dim()),
            None => (icon, live),
        };
        vec![Span::styled(format!(" {glyph} "), style)]
    };
    let to_push = r.ahead > 0 || r.upstream == DevUpstream::Unpushed;
    vec![
        button(FETCH_ICON, DevOp::Fetch, theme::text()),
        button(PUSH_ICON, DevOp::Push, if to_push { theme::success() } else { theme::muted() }),
        button(PULL_ICON, DevOp::Pull, if r.behind > 0 { theme::warning() } else { theme::muted() }),
    ]
}

pub fn draw(app: &App, f: &mut Frame, area: Rect) {
    let d = &app.dev;
    let rows = rows(app);
    let mut title = vec![title_text("projects")];
    if let Some(s) = &d.scan {
        title.push(title_muted(format!("  {}", s.root)));
        title.push(title_muted(format!("  {} repositories", s.repos.len())));
    }
    if d.only_attention {
        title.push(Span::styled("  needs attention only", theme::warning()));
    }
    if d.loading {
        title.push(title_muted("  reading…"));
    }
    let inner = framed(f, area, title, true);
    let Some(scan) = &d.scan else {
        f.render_widget(para(vec![Line::styled(" loading…", theme::muted())]), inner);
        return;
    };
    if !scan.root_found || scan.repos.is_empty() || rows.is_empty() {
        let msg = if !scan.root_found {
            format!(" {} is not a folder — set dev_dir in groundcontrol.yaml", scan.root)
        } else if scan.repos.is_empty() {
            format!(" no git repositories under {} (looked {} levels down)", scan.root, crate::devscan::MAX_DEPTH)
        } else {
            " everything clean and in sync".to_string()
        };
        f.render_widget(para(vec![Line::styled(msg, theme::muted())]), inner);
        return;
    }

    let w = inner.width;
    let visible = inner.height.saturating_sub(1) as usize;
    let sel = selected_index(app, &rows).unwrap_or(0);
    let start = {
        let mut t = d.top.get().min(rows.len().saturating_sub(visible.max(1)));
        if sel < t {
            t = sel;
        } else if sel >= t + visible.max(1) {
            t = sel + 1 - visible.max(1);
        }
        d.top.set(t);
        t
    };

    // Marker, the dashboard's columns, then the buttons at the right end.
    let cols = |marker: Span<'static>, repo, branch, tree, ab, commit, fetched, buttons| -> Vec<(Vec<Span<'static>>, u16)> {
        vec![(vec![marker], 1), (repo, 0), (branch, 20), (tree, 9), (ab, 7), (commit, 7), (fetched, 7), (buttons, BUTTONS_W)]
    };
    let muted = |s: &'static str| vec![Span::styled(s, theme::muted())];
    let mut lines = vec![rich_row(
        cols(Span::raw(" "), muted("repo"), muted("branch"), muted("tree"), muted("↑ ↓"), muted("commit"), muted("fetched"), vec![]),
        w,
        1,
    )];

    let list = Rect { y: inner.y + 1, height: visible as u16, ..inner };
    for (i, r) in rows.iter().enumerate().skip(start).take(visible) {
        let selected = i == sel;
        let line = match r.repo {
            Some(ri) => {
                let repo = &scan.repos[ri];
                let name_style = if selected { theme::bold() } else { theme::text() };
                let commit = repo.last_commit.map(|t| fmt_age(now() - t)).unwrap_or_else(|| "-".into());
                let (tree, tree_style) = tree_cell(repo);
                let line = rich_row(
                    cols(
                        Span::styled(if selected { "▶" } else { " " }, theme::key()),
                        vec![Span::styled(r.prefix.clone(), theme::dim()), Span::styled(r.name.clone(), name_style)],
                        vec![Span::styled(
                            repo.branch.clone(),
                            if repo.upstream == DevUpstream::Detached { theme::warning() } else { theme::text() },
                        )],
                        vec![Span::styled(tree, tree_style)],
                        vec![Span::styled(ahead_behind(repo), theme::text())],
                        vec![Span::styled(commit, theme::muted())],
                        vec![fetched_span(repo)],
                        button_spans(repo, d.busy.get(&repo.path).copied()).into_iter().flatten().collect(),
                    ),
                    w,
                    1,
                );
                if selected { line.style(theme::selected()) } else { line }
            }
            None => {
                let dash = || vec![Span::styled("-", theme::muted())];
                rich_row(
                    cols(
                        Span::raw(" "),
                        vec![Span::styled(format!("{}{}", r.prefix, r.name), theme::dim())],
                        dash(),
                        dash(),
                        dash(),
                        dash(),
                        dash(),
                        vec![],
                    ),
                    w,
                    1,
                )
            }
        };
        lines.push(line);
    }
    f.render_widget(para(lines), inner);

    // The list as a whole, then the button columns on top of it, at the right edge.
    app.hits.add(list, Target::DevRepos, start);
    let x = inner.x + w.saturating_sub(BUTTONS_W);
    for (i, target) in [Target::DevFetch, Target::DevPush, Target::DevPull].into_iter().enumerate() {
        app.hits.add(Rect { x: x + BUTTON_W * i as u16, width: BUTTON_W, ..list }, target, start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(path: &str) -> DevRepo {
        DevRepo {
            path: path.into(),
            branch: "main".into(),
            upstream: DevUpstream::Tracking,
            ahead: 0,
            behind: 0,
            staged: 0,
            unstaged: 0,
            conflicted: 0,
            untracked: 0,
            last_commit: Some(0),
            last_fetch: None,
        }
    }

    fn labels(rows: &[DevRow]) -> Vec<String> {
        rows.iter().map(|r| format!("{}{}", r.prefix, r.name)).collect()
    }

    #[test]
    fn the_tree_matches_gstat() {
        let repos = [r("binara-sachin"), r("csm/cs-tools"), r("learn-rust/rune"), r("learn-rust/rune-mentor"), r("salli")];
        let rows = tree_rows(&repos, false);
        assert_eq!(
            labels(&rows),
            ["├── binara-sachin", "├── csm/cs-tools", "├── learn-rust", "│   ├── rune", "│   └── rune-mentor", "└── salli"]
        );
        assert_eq!(rows[1].repo, Some(1));
        assert_eq!(rows[2].repo, None, "learn-rust is only a folder");
        assert_eq!(rows[3].path, "learn-rust/rune");
    }

    #[test]
    fn a_repo_with_nested_repos_keeps_its_children() {
        let rows = tree_rows(&[r("a"), r("a/b/c")], false);
        assert_eq!(labels(&rows), ["└── a", "    └── b/c"]);
    }

    #[test]
    fn only_attention_keeps_dirty_repos_and_their_folders() {
        let mut dirty = r("learn-rust/rune");
        dirty.untracked = 2;
        let repos = [r("a"), dirty, r("learn-rust/other"), r("z")];
        let rows = tree_rows(&repos, true);
        assert_eq!(labels(&rows), ["└── learn-rust", "    └── rune"]);
        assert!(tree_rows(&[r("a")], true).is_empty());
    }

    #[test]
    fn attention_means_something_to_do() {
        assert!(!r("a").needs_attention());
        let cases: [fn(&mut DevRepo); 5] = [
            |r| r.ahead = 1,
            |r| r.behind = 1,
            |r| r.conflicted = 1,
            |r| r.upstream = DevUpstream::Unpushed,
            |r| r.upstream = DevUpstream::Error,
        ];
        for f in cases {
            let mut x = r("a");
            f(&mut x);
            assert!(x.needs_attention());
        }
        let mut local = r("a");
        local.upstream = DevUpstream::NoRemote;
        assert!(!local.needs_attention() && !local.has_remote());
    }
}
