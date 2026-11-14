//! The Projects screen's data: every git repository under `dev_dir`, with the
//! git state `gstat` shows, and the three actions on a repository (fetch, push,
//! pull). Reading state is purely local and never touches the network; only
//! the actions do, and only when asked.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::git::{git, git_network};
use crate::model::*;

/// Repositories are looked for this many folders below `dev_dir`.
pub const MAX_DEPTH: usize = 3;
/// Folders that are never entered.
const SKIP_DIRS: [&str; 3] = ["node_modules", "vendor", "target"];
/// Repositories read at the same time.
const PARALLEL: usize = 8;

/// Paths (relative to `root`, `/`-separated, sorted) of every folder holding a
/// `.git`, repositories nested in repositories included. Hidden folders and
/// symlinks are not entered.
pub fn discover(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, "", 0, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, rel: &str, depth: usize, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        if !is_dir || name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        if e.path().join(".git").exists() {
            out.push(path.clone());
        }
        if depth + 1 < MAX_DEPTH {
            walk(&e.path(), &path, depth + 1, out);
        }
    }
}

/// Reads every repository under `root`.
pub async fn scan(root: &Path, display: &str) -> DevScan {
    let root_found = root.is_dir();
    let paths = {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || discover(&root)).await.unwrap_or_default()
    };
    let sem = Arc::new(Semaphore::new(PARALLEL));
    let mut set = JoinSet::new();
    for path in &paths {
        // Untracked entries that are really nested repositories are theirs.
        let prefix = format!("{path}/");
        let nested: Vec<String> = paths.iter().filter_map(|p| p.strip_prefix(&prefix).map(str::to_string)).collect();
        let (dir, path, sem) = (root.join(path), path.clone(), sem.clone());
        set.spawn(async move {
            let _permit = sem.acquire_owned().await;
            let (mut repo, _) = inspect(&dir, &nested).await;
            repo.path = path;
            repo
        });
    }
    let mut repos = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(r) = r {
            repos.push(r);
        }
    }
    repos.sort_by(|a, b| a.path.cmp(&b.path));
    DevScan { root: display.to_string(), root_found, repos, scanned_at: now() }
}

/// What `git status --porcelain=v2 --branch` says, before the remote is looked at.
#[derive(Debug, Default, PartialEq, Eq)]
struct Porcelain {
    branch: String,
    unborn: bool,
    upstream_set: bool,
    ab: Option<(u32, u32)>,
    staged: u32,
    unstaged: u32,
    conflicted: u32,
    untracked: u32,
}

/// `nested` lists repositories below this one; their untracked entries are not
/// this repository's. Git folds an untracked folder into its topmost folder, so
/// a repository two levels down shows up as its parent.
fn parse_porcelain(out: &str, nested: &[String]) -> Porcelain {
    let mut p = Porcelain { branch: "-".into(), ..Default::default() };
    for line in out.lines() {
        if let Some(r) = line.strip_prefix("# branch.oid ") {
            p.unborn = r == "(initial)";
        } else if let Some(r) = line.strip_prefix("# branch.head ") {
            p.branch = r.to_string();
        } else if line.starts_with("# branch.upstream ") {
            p.upstream_set = true;
        } else if let Some(r) = line.strip_prefix("# branch.ab ") {
            let mut n = r.split_whitespace().map(|x| x.trim_start_matches(['+', '-']).parse::<u32>().unwrap_or(0));
            p.ab = Some((n.next().unwrap_or(0), n.next().unwrap_or(0)));
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let mut xy = line.split_whitespace().nth(1).unwrap_or("..").chars();
            p.staged += u32::from(xy.next().is_some_and(|c| c != '.'));
            p.unstaged += u32::from(xy.next().is_some_and(|c| c != '.'));
        } else if line.starts_with("u ") {
            p.conflicted += 1;
        } else if let Some(path) = line.strip_prefix("? ") {
            let path = path.trim_end_matches('/');
            let prefix = format!("{path}/");
            if !nested.iter().any(|n| n == path || n.starts_with(&prefix)) {
                p.untracked += 1;
            }
        }
    }
    p
}

/// Reads one repository. The remote returned is the one pushes and pulls go
/// to when the branch has no upstream: `origin`, else the first one.
pub async fn inspect(dir: &Path, nested: &[String]) -> (DevRepo, Option<String>) {
    let mut repo = DevRepo {
        path: String::new(),
        branch: "-".into(),
        upstream: DevUpstream::Error,
        ahead: 0,
        behind: 0,
        staged: 0,
        unstaged: 0,
        conflicted: 0,
        untracked: 0,
        last_commit: None,
        last_fetch: None,
    };
    let Ok(status) = git(dir, &["status", "--porcelain=v2", "--branch", "--untracked-files=normal"]).await else {
        return (repo, None);
    };
    let p = parse_porcelain(&status, nested);
    repo.branch = p.branch.clone();
    (repo.staged, repo.unstaged, repo.conflicted, repo.untracked) = (p.staged, p.unstaged, p.conflicted, p.untracked);

    let remotes = git(dir, &["remote"]).await.unwrap_or_default();
    let remotes: Vec<&str> = remotes.split_whitespace().collect();
    let remote = remotes.iter().find(|r| **r == "origin").or(remotes.first()).map(|r| r.to_string());

    repo.upstream = if p.upstream_set {
        match p.ab {
            Some((a, b)) => {
                (repo.ahead, repo.behind) = (a, b);
                DevUpstream::Tracking
            }
            // Configured but nothing to count against: the remote branch is gone.
            None => DevUpstream::Gone,
        }
    } else if p.branch == "(detached)" {
        DevUpstream::Detached
    } else if p.unborn {
        DevUpstream::Unborn
    } else if let Some(remote) = &remote {
        let tracking = format!("refs/remotes/{remote}/{}", p.branch);
        if git(dir, &["rev-parse", "--verify", "--quiet", &tracking]).await.is_ok() {
            let range = format!("{tracking}...HEAD");
            let counts = git(dir, &["rev-list", "--left-right", "--count", &range]).await.unwrap_or_default();
            let mut n = counts.split_whitespace().map(|x| x.parse::<u32>().unwrap_or(0));
            (repo.behind, repo.ahead) = (n.next().unwrap_or(0), n.next().unwrap_or(0));
            DevUpstream::Matching
        } else {
            DevUpstream::Unpushed
        }
    } else {
        DevUpstream::NoRemote
    };

    repo.last_commit = git(dir, &["log", "-1", "--format=%ct"]).await.ok().and_then(|s| s.trim().parse().ok());
    repo.last_fetch = last_fetch(dir).await;
    (repo, remote)
}

/// When `FETCH_HEAD` was last written. A plain repository keeps it in `.git`;
/// worktrees and submodules (where `.git` is a file) are asked.
async fn last_fetch(dir: &Path) -> Option<Timestamp> {
    let mut gitdir = dir.join(".git");
    if !gitdir.is_dir() {
        gitdir = PathBuf::from(git(dir, &["rev-parse", "--absolute-git-dir"]).await.ok()?.trim());
    }
    let modified = std::fs::metadata(gitdir.join("FETCH_HEAD")).ok()?.modified().ok()?;
    Some(modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as Timestamp)
}

/// Fetches, pushes or pulls one repository, and says what was done.
///
/// Never forces anything: a push is a plain push (with `-u` the first time a
/// branch goes up) and a pull only fast-forwards, so neither can rewrite
/// history or leave a half-done merge. What cannot be done safely is refused
/// with the reason.
pub async fn run_op(dir: &Path, op: DevOp) -> Result<String, String> {
    let (repo, remote) = inspect(dir, &[]).await;
    let branch = repo.branch.as_str();
    let refuse = |why: &str| Err(why.to_string());
    match (op, repo.upstream) {
        (_, DevUpstream::Error) => refuse("git cannot read this repository"),
        (_, DevUpstream::NoRemote) => refuse("no remote configured"),
        (DevOp::Fetch, _) => git_network(dir, &["fetch", "--all", "--prune"]).await.map(|_| "fetched".into()),
        (_, DevUpstream::Detached) => refuse("not on a branch"),
        (_, DevUpstream::Unborn) => refuse("nothing committed yet"),
        (DevOp::Push, DevUpstream::Gone) => refuse("the upstream branch was deleted on the remote; push it from a terminal"),
        (DevOp::Pull, DevUpstream::Gone) => refuse("the upstream branch was deleted on the remote"),
        (DevOp::Pull, DevUpstream::Unpushed) => refuse("this branch is not on the remote yet"),
        (DevOp::Push, DevUpstream::Tracking) => git_network(dir, &["push"]).await.map(|_| "pushed".into()),
        (DevOp::Push, DevUpstream::Matching | DevUpstream::Unpushed) => {
            let remote = remote.unwrap_or_default();
            git_network(dir, &["push", "-u", &remote, branch]).await.map(|_| "pushed".into())
        }
        (DevOp::Pull, DevUpstream::Tracking) => pulled(git_network(dir, &["pull", "--ff-only"]).await),
        (DevOp::Pull, DevUpstream::Matching) => {
            let remote = remote.unwrap_or_default();
            pulled(git_network(dir, &["pull", "--ff-only", &remote, branch]).await)
        }
    }
}

fn pulled(r: Result<String, String>) -> Result<String, String> {
    r.map(|out| if out.contains("Already up to date") { "already up to date".into() } else { "pulled".into() })
}

/// The folder a path below `dev_dir` names, or `None` when it would leave it
/// or is not a repository.
pub fn resolve(root: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    let plain = !rel.is_empty() && p.components().all(|c| matches!(c, std::path::Component::Normal(_)));
    (plain && root.join(p).join(".git").exists()).then(|| root.join(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(dir: &Path, cmd: &str) {
        let ok = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).status().unwrap().success();
        assert!(ok, "{cmd}");
    }

    fn repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        sh(
            dir,
            "git init -q -b main && git config user.email t@t && git config user.name t && echo a > a && git add -A && git commit -qm one",
        );
    }

    #[test]
    fn porcelain_counts_and_nested_untracked() {
        let out = "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n1 .M N... 1 1 1 a a x\n1 M. N... 1 1 1 a a y\nu UU N... 1 2 3 4 a b c z\n? new\n? sub/\n? deep/\n? other\n";
        let p = parse_porcelain(out, &["sub".into(), "deep/inner".into()]);
        assert_eq!(p.ab, Some((2, 1)));
        assert_eq!((p.staged, p.unstaged, p.conflicted, p.untracked), (1, 1, 1, 2));
        assert!(p.upstream_set && !p.unborn);
        assert!(parse_porcelain("# branch.oid (initial)\n# branch.head main\n", &[]).unborn);
    }

    #[test]
    fn discovery_finds_nested_repos_and_skips_noise() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        repo(&r.join("a"));
        repo(&r.join("group/b"));
        repo(&r.join("group/b/inner"));
        repo(&r.join("node_modules/x"));
        repo(&r.join(".hidden/y"));
        repo(&r.join("a/b/c/too-deep"));
        std::fs::create_dir_all(r.join("plain")).unwrap();
        assert_eq!(discover(r), ["a", "group/b", "group/b/inner"]);
    }

    #[test]
    fn resolve_stays_inside_the_root() {
        let d = tempfile::tempdir().unwrap();
        repo(&d.path().join("a"));
        assert!(resolve(d.path(), "a").is_some());
        for bad in ["", "..", "a/..", "/a", "nope"] {
            assert!(resolve(d.path(), bad).is_none(), "{bad}");
        }
    }

    #[tokio::test]
    async fn states_and_actions_against_a_real_remote() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        sh(r, "git init -q --bare -b main remote.git");
        let url = r.join("remote.git").display().to_string();

        // no remote, then a branch that has never been pushed
        let local = r.join("local");
        repo(&local);
        let (s, _) = inspect(&local, &[]).await;
        assert_eq!((s.upstream, s.branch.as_str()), (DevUpstream::NoRemote, "main"));
        assert_eq!(run_op(&local, DevOp::Push).await.unwrap_err(), "no remote configured");
        sh(&local, &format!("git remote add origin {url}"));
        assert_eq!(inspect(&local, &[]).await.0.upstream, DevUpstream::Unpushed);
        assert_eq!(run_op(&local, DevOp::Pull).await.unwrap_err(), "this branch is not on the remote yet");

        // push sets the upstream; afterwards it is in sync
        assert_eq!(run_op(&local, DevOp::Push).await.unwrap(), "pushed");
        let (s, _) = inspect(&local, &[]).await;
        assert_eq!((s.upstream, s.ahead, s.behind), (DevUpstream::Tracking, 0, 0));

        // a second clone moves the remote ahead; the first sees it only after a fetch
        let other = r.join("other");
        sh(
            r,
            &format!(
                "git clone -q {url} other && cd other && git config user.email t@t && git config user.name t && echo b > b && git add -A && git commit -qm two && git push -q"
            ),
        );
        assert_eq!(inspect(&local, &[]).await.0.behind, 0);
        assert!(inspect(&local, &[]).await.0.last_fetch.is_none(), "nothing has fetched here yet");
        assert_eq!(run_op(&local, DevOp::Fetch).await.unwrap(), "fetched");
        assert!(inspect(&local, &[]).await.0.last_fetch.is_some());
        assert_eq!(inspect(&local, &[]).await.0.behind, 1);
        assert_eq!(run_op(&local, DevOp::Pull).await.unwrap(), "pulled");
        assert_eq!(run_op(&local, DevOp::Pull).await.unwrap(), "already up to date");

        // work on both sides: diverged, so a pull is refused rather than merged
        sh(&local, "echo c > c && git add -A && git commit -qm three");
        sh(&other, "echo d > d && git add -A && git commit -qm four && git push -q");
        run_op(&local, DevOp::Fetch).await.unwrap();
        let (s, _) = inspect(&local, &[]).await;
        assert_eq!((s.ahead, s.behind), (1, 1));
        assert!(run_op(&local, DevOp::Pull).await.is_err());
        assert!(run_op(&local, DevOp::Push).await.is_err(), "a rejected push is an error, never forced");

        // untracked files count; a branch whose upstream vanished is `gone`
        std::fs::write(local.join("new"), "x").unwrap();
        assert_eq!(inspect(&local, &[]).await.0.untracked, 1);
        sh(&local, "git checkout -q -b side && git branch -q -u origin/main side && git update-ref -d refs/remotes/origin/main");
        assert_eq!(inspect(&local, &[]).await.0.upstream, DevUpstream::Gone);
    }

    #[tokio::test]
    async fn scan_lists_repos_in_order() {
        let d = tempfile::tempdir().unwrap();
        repo(&d.path().join("b"));
        repo(&d.path().join("a"));
        let s = scan(d.path(), "~/x").await;
        assert!(s.root_found);
        assert_eq!(s.repos.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert!(s.repos[0].last_commit.is_some());
        assert!(!scan(&d.path().join("missing"), "x").await.root_found);
    }
}
