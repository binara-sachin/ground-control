//! Host integration: tool check (§6.6), Docker/tmux reachability, boot time
//! (§6.5) and macOS notifications (§10.5).

use std::time::Duration;

use crate::protocol::{Reachability, ToolStatus};

use super::process::shell_output;

pub const REQUIRED_TOOLS: [&str; 9] = ["git", "gh", "pnpm", "npm", "cargo", "go", "docker", "tmux", "sqlite3"];

/// Resolves each required tool on the step shell's PATH (`/bin/zsh -l`).
pub async fn tool_check() -> Vec<ToolStatus> {
    let script: String = REQUIRED_TOOLS
        .iter()
        .map(|t| format!("printf '%s\\t%s\\n' {t} \"$(command -v {t} 2>/dev/null)\";"))
        .collect();
    let out = shell_output(&script, Duration::from_secs(20)).await.unwrap_or_default();
    REQUIRED_TOOLS
        .iter()
        .map(|t| {
            let path = out
                .lines()
                .filter_map(|l| l.split_once('\t'))
                .find(|(name, _)| name == t)
                .map(|(_, p)| p.trim().to_string())
                .filter(|p| !p.is_empty());
            ToolStatus { name: t.to_string(), ok: path.is_some(), path }
        })
        .collect()
}

pub async fn docker_reachable() -> Reachability {
    match shell_output("docker info --format '{{.ServerVersion}}'", Duration::from_secs(8)).await {
        Ok(v) if !v.is_empty() => Reachability { ok: true, detail: format!("server {v}") },
        Ok(_) => Reachability { ok: false, detail: "no server version reported".into() },
        Err(e) => Reachability { ok: false, detail: short(&e) },
    }
}

pub async fn tmux_reachable() -> Reachability {
    match shell_output("tmux display-message -p '#{version} · #{server_sessions} sessions'", Duration::from_secs(5)).await {
        Ok(v) => Reachability { ok: true, detail: v },
        Err(e) => Reachability { ok: false, detail: short(&e) },
    }
}

fn short(s: &str) -> String {
    let s = s.lines().next().unwrap_or(s).trim();
    if s.chars().count() > 120 { format!("{}…", s.chars().take(119).collect::<String>()) } else { s.to_string() }
}

/// Unix time the machine booted: `sysctl kern.boottime` on macOS, `/proc/stat` elsewhere.
pub async fn boot_time() -> Option<i64> {
    if cfg!(target_os = "macos") {
        let out = tokio::process::Command::new("/usr/sbin/sysctl").args(["-n", "kern.boottime"]).output().await.ok()?;
        parse_kern_boottime(&String::from_utf8_lossy(&out.stdout))
    } else {
        let stat = tokio::fs::read_to_string("/proc/stat").await.ok()?;
        stat.lines().find_map(|l| l.strip_prefix("btime ")).and_then(|v| v.trim().parse().ok())
    }
}

/// Parses `{ sec = 1730000000, usec = 123 } Sat Oct 31 09:00:00 2026`.
pub fn parse_kern_boottime(s: &str) -> Option<i64> {
    let rest = &s[s.find("sec =")? + 5..];
    rest.trim_start().split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// Sends a macOS notification. Informational only; approving needs the TUI.
pub async fn notify(title: &str, body: &str) {
    tracing::info!(title, body, "notification");
    if !cfg!(target_os = "macos") {
        return;
    }
    let script = format!(
        "display notification {} with title \"GroundControl\" subtitle {}",
        applescript_string(body),
        applescript_string(title)
    );
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("/usr/bin/osascript").arg("-e").arg(script).stdin(std::process::Stdio::null()).status(),
    )
    .await;
}

fn applescript_string(s: &str) -> String {
    let clean: String = s.chars().filter(|c| !c.is_control()).collect();
    format!("\"{}\"", clean.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kern_boottime() {
        assert_eq!(parse_kern_boottime("{ sec = 1730361600, usec = 412000 } Sat Oct 31 09:00:00 2026\n"), Some(1730361600));
        assert_eq!(parse_kern_boottime("garbage"), None);
    }

    #[test]
    fn applescript_escaping() {
        assert_eq!(applescript_string("say \"hi\" \\ now\n"), "\"say \\\"hi\\\" \\\\ now\"");
    }
}
