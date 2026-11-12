//! Reads back what the run worker writes to its pane: splits a run window's
//! captured pane into each step's output at the header the worker prints
//! before each step.

use std::sync::LazyLock;

use regex::Regex;

use crate::protocol::RunDetail;
use crate::tmux::trim_capture;

static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap());

/// Splits a run window's pane into each step's output, using the header the
/// worker prints before each step. The run's own status lines are dropped.
///
/// The scrollback is finite, so the oldest headers can be gone. Lines above
/// the first header still visible belong to the step before it; with no
/// header left at all, they belong to `latest`, the run's latest started step.
pub fn split_steps(pane: &str, latest: Option<usize>) -> Vec<String> {
    let mut sections: Vec<String> = Vec::new();
    let mut current: Option<usize> = None;
    let mut orphans = String::new();
    let header = format!("── {}", super::worker::STEP_HEADER);
    for line in trim_capture(pane).lines() {
        let plain = ANSI_RE.replace_all(line, "");
        let plain = plain.trim();
        if let Some(rest) = plain.strip_prefix(&header)
            && let Some(n) = rest.split('/').next().and_then(|n| n.parse::<usize>().ok())
            && n > 0
        {
            if current.is_none() && n > 1 && !orphans.is_empty() {
                sections.resize(n - 1, String::new());
                sections[n - 2] = std::mem::take(&mut orphans);
            }
            current = Some(n - 1);
            if sections.len() < n {
                sections.resize(n, String::new());
            }
            continue;
        }
        if plain.starts_with("── run #") {
            continue;
        }
        // tmux resets the header's dim style at the start of the next line;
        // each step's view starts unstyled anyway.
        let line = line.strip_prefix("\x1b[0m").unwrap_or(line);
        let section = match current {
            Some(i) => &mut sections[i],
            None => &mut orphans,
        };
        section.push_str(line);
        section.push('\n');
    }
    if current.is_none()
        && !orphans.is_empty()
        && let Some(i) = latest
    {
        sections.resize(i + 1, String::new());
        sections[i] = orphans;
    }
    sections
}

/// The run's latest step that has started, which owns output whose header
/// has scrolled out of the pane.
pub fn latest_started(detail: &RunDetail) -> Option<usize> {
    detail.steps.iter().rev().find(|s| s.started_at.is_some()).map(|s| s.index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_a_run_pane_into_steps() {
        let pane = "\x1b[2m── run #4: app/hello ──\x1b[0m\n\x1b[2m── step 1/2: Greet ──\x1b[0m\n\x1b[0mhi\n\x1b[31mred\x1b[0m\n\
                    \x1b[2m── step 2/2: Use output ──\x1b[0m\ngot it\n\x1b[2m── run #4 succeeded ──\x1b[0m\n\n\n";
        let s = split_steps(pane, Some(1));
        assert_eq!(s, vec!["hi\n\x1b[31mred\x1b[0m\n".to_string(), "got it\n".to_string()]);
        assert_eq!(trim_capture("a\n\n  \n"), "a\n");
    }

    #[test]
    fn output_whose_header_scrolled_away_keeps_its_step() {
        // Step 2's header and the top of its output are gone from the scrollback.
        let pane = "…tail of step 2\n── step 3/3: Deploy ──\ndeployed\n";
        assert_eq!(split_steps(pane, Some(2)), vec![String::new(), "…tail of step 2\n".into(), "deployed\n".into()]);
        // No header left at all: everything is the latest started step's.
        assert_eq!(split_steps("line 9000\nline 9001\n", Some(1)), vec![String::new(), "line 9000\nline 9001\n".into()]);
        assert!(split_steps("line\n", None).is_empty());
        // Lines before step 1 are the run's own, not a step's.
        assert_eq!(split_steps("── run #3: a/b ──\nnote\n── step 1/1: X ──\nx\n", Some(0)), vec!["x\n".to_string()]);
    }
}
