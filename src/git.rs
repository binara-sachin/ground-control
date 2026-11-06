//! Git helpers: status parsing, the repository guard (§8.4) and run snapshots
//! (§8.2). Git is always run as a plain subprocess; nothing here fetches.

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

use crate::protocol::{GitStatus, now};

/// Normalises SSH and HTTPS remote forms to `host/owner/repo` (lowercase host,
/// no `.git` suffix), so `git@github.com:a/b.git` and `https://github.com/a/b` match.
pub fn normalize_remote(url: &str) -> Option<String> {
    let mut s = url.trim().to_string();
    for prefix in ["https://", "http://", "ssh://", "git://", "git+ssh://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
            break;
        }
    }
    // user@host:path (scp-like) or user@host/path
    if let Some(at) = s.find('@')
        && !s[..at].contains('/')
    {
        s = s[at + 1..].to_string();
    }
    let (host, path) = match (s.find(':'), s.find('/')) {
        (Some(c), Some(sl)) if c < sl => {
            let host = &s[..c];
            let after = &s[c + 1..];
            // host:port/path (ssh://) vs host:owner/repo (scp)
            match after.split_once('/') {
                Some((port, rest)) if port.chars().all(|ch| ch.is_ascii_digit()) => (host.to_string(), rest.to_string()),
                _ => (host.to_string(), after.to_string()),
            }
        }
        (Some(c), None) => (s[..c].to_string(), s[c + 1..].to_string()),
        (_, Some(sl)) => (s[..sl].to_string(), s[sl + 1..].to_string()),
        (None, None) => return None,
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if host.is_empty() || path.is_empty() || !path.contains('/') {
        return None;
    }
    Some(format!("{}/{}", host.to_ascii_lowercase(), path))
}

pub fn remotes_match(declared: &str, actual: &str) -> bool {
    match (normalize_remote(declared), normalize_remote(actual)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Parses `git status --porcelain=v2 --branch` output.
pub fn parse_porcelain_v2(out: &str) -> GitStatus {
    let mut g = GitStatus::default();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("# branch.oid ") {
            if rest != "(initial)" {
                g.commit = Some(rest.chars().take(7).collect());
            }
        } else if let Some(rest) = line.strip_prefix("# branch.head ") {
            g.branch = Some(if rest == "(detached)" { "(detached)".into() } else { rest.to_string() });
        } else if let Some(rest) = line.strip_prefix("# branch.upstream ") {
            g.upstream = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            for part in rest.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    g.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    g.behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let xy = line.split_whitespace().nth(1).unwrap_or("..");
            let mut c = xy.chars();
            if c.next().is_some_and(|x| x != '.') {
                g.staged += 1;
            }
            if c.next().is_some_and(|y| y != '.') {
                g.modified += 1;
            }
        } else if line.starts_with("u ") {
            g.conflicted += 1;
        } else if line.starts_with("? ") {
            g.untracked += 1;
        }
    }
    g
}

/// Runs git with a timeout; returns stdout on success.
pub async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .map_err(|_| format!("git {} timed out", args.join(" ")))?
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().next().unwrap_or("git failed").trim().to_string())
    }
}

/// `origin` URL, or an error message.
pub async fn origin_url(dir: &Path) -> Result<String, String> {
    git(dir, &["remote", "get-url", "origin"]).await.map(|s| s.trim().to_string())
}

/// The repository guard: `origin` must match the declared remote.
pub async fn check_remote(dir: &Path, declared: Option<&str>) -> Result<(), String> {
    let Some(declared) = declared else { return Ok(()) };
    match origin_url(dir).await {
        Ok(url) if remotes_match(declared, &url) => Ok(()),
        Ok(url) => Err(format!("origin is {url}, but the config expects {declared}; refusing")),
        Err(e) => Err(format!("cannot read origin ({e}); the config expects {declared}; refusing")),
    }
}

/// Current status of the working tree plus the origin check.
pub async fn status(dir: &Path, declared_remote: Option<&str>) -> GitStatus {
    let mut g = match git(dir, &["status", "--porcelain=v2", "--branch"]).await {
        Ok(out) => parse_porcelain_v2(&out),
        Err(e) => GitStatus { error: Some(e), ..Default::default() },
    };
    if let Some(declared) = declared_remote {
        match origin_url(dir).await {
            Ok(url) => {
                g.origin_matches = Some(remotes_match(declared, &url));
                g.origin_url = Some(url);
            }
            Err(_) => g.origin_matches = Some(false),
        }
    }
    g.checked_at = now();
    g
}

/// Number of commits in HEAD that no `origin` ref contains.
pub async fn unpushed_count(dir: &Path) -> Result<u32, String> {
    let out = git(dir, &["rev-list", "--count", "HEAD", "--not", "--remotes=origin"]).await?;
    out.trim().parse().map_err(|_| "unexpected rev-list output".to_string())
}

/// Snapshot of the tree at run start.
pub struct Snapshot {
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub dirty: bool,
    pub unpushed: Option<u32>,
    pub status: GitStatus,
}

pub async fn snapshot(dir: &Path) -> Snapshot {
    let st = status(dir, None).await;
    let commit = git(dir, &["rev-parse", "--short=7", "HEAD"]).await.ok().map(|s| s.trim().to_string());
    let unpushed = unpushed_count(dir).await.ok();
    Snapshot { branch: st.branch.clone(), commit, dirty: st.error.is_none() && !st.is_clean(), unpushed, status: st }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_forms_match() {
        let want = Some("github.com/binara-sachin/finance-analyzer".to_string());
        assert_eq!(normalize_remote("github.com/binara-sachin/finance-analyzer"), want);
        assert_eq!(normalize_remote("git@github.com:binara-sachin/finance-analyzer.git"), want);
        assert_eq!(normalize_remote("https://github.com/binara-sachin/finance-analyzer.git"), want);
        assert_eq!(normalize_remote("https://GitHub.com/binara-sachin/finance-analyzer/"), want);
        assert_eq!(normalize_remote("ssh://git@github.com:22/binara-sachin/finance-analyzer.git"), want);
        assert_eq!(normalize_remote("ssh://git@github.com/binara-sachin/finance-analyzer"), want);
        assert!(!remotes_match("github.com/a/b", "git@github.com:a/c.git"));
        assert_eq!(normalize_remote("nonsense"), None);
    }

    #[test]
    fn porcelain_parse() {
        let out = "# branch.oid a41c9e2f00000000000000000000000000000000\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 abc abc src/a.rs\n1 M. N... 100644 100644 100644 abc abc src/b.rs\n2 R. N... 100644 100644 100644 abc abc R100 c\td\nu UU N... 1 2 3 4 a b c x\n? new.txt\n? other\n";
        let g = parse_porcelain_v2(out);
        assert_eq!(g.branch.as_deref(), Some("main"));
        assert_eq!(g.commit.as_deref(), Some("a41c9e2"));
        assert_eq!((g.ahead, g.behind), (2, 1));
        assert_eq!((g.staged, g.modified, g.untracked, g.conflicted), (2, 1, 2, 1));
        assert!(!g.is_clean());
    }
}
