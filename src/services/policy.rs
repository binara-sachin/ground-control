//! What happens after a service process exits (§6.5). Pure, so every branch
//! is unit-tested without spawning anything.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::config::RestartPolicy;

const CRASH_WINDOW: Duration = Duration::from_secs(5 * 60);
const CRASH_LIMIT: usize = 5;
const MAX_BACKOFF: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Exit code 0: the service finished on its own.
    Stopped { message: String },
    /// Not restarted. `gave_up` marks the cases worth a bell: a running
    /// service that crashed under `restart: never`, or the crash limit.
    Crashed { message: String, gave_up: bool },
    /// `restart: on-failure`, after `delay`.
    Restart { delay: Duration, message: String },
}

fn describe_exit(code: Option<i32>) -> String {
    match code {
        Some(c) => format!("exit code {c}"),
        None => "a signal".into(),
    }
}

/// Decides the next step after an exit GroundControl did not ask for.
///
/// `during_start` is true while the first health check has not passed yet; a
/// failed start is reported to whoever started it and never restarted.
/// `crashes` is the service's crash history and is updated here.
pub fn after_exit(policy: RestartPolicy, during_start: bool, code: Option<i32>, crashes: &mut VecDeque<Instant>, now: Instant) -> Next {
    if during_start {
        return Next::Crashed { message: format!("exited with {} during start", describe_exit(code)), gave_up: false };
    }
    if code == Some(0) {
        return Next::Stopped { message: "exited with code 0".into() };
    }
    if policy == RestartPolicy::Never {
        return Next::Crashed { message: format!("exited with {}", describe_exit(code)), gave_up: true };
    }
    crashes.push_back(now);
    while crashes.front().is_some_and(|t| now.duration_since(*t) > CRASH_WINDOW) {
        crashes.pop_front();
    }
    if crashes.len() >= CRASH_LIMIT {
        let at = chrono::Local::now().format("%H:%M");
        return Next::Crashed { message: format!("crashed {}× in 5m, gave up {at}", crashes.len()), gave_up: true };
    }
    let n = crashes.len() as u32;
    let delay = (1u64 << (n - 1).min(6)).min(MAX_BACKOFF);
    Next::Restart { delay: Duration::from_secs(delay), message: format!("exited with {}, restarting in {delay}s", describe_exit(code)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON_FAILURE: RestartPolicy = RestartPolicy::OnFailure;

    #[test]
    fn failed_start_is_never_restarted() {
        let mut c = VecDeque::new();
        let n = after_exit(ON_FAILURE, true, Some(4), &mut c, Instant::now());
        assert_eq!(n, Next::Crashed { message: "exited with exit code 4 during start".into(), gave_up: false });
        assert!(c.is_empty());
    }

    #[test]
    fn clean_exit_stops() {
        let mut c = VecDeque::new();
        assert!(matches!(after_exit(ON_FAILURE, false, Some(0), &mut c, Instant::now()), Next::Stopped { .. }));
        assert!(matches!(after_exit(RestartPolicy::Never, false, Some(0), &mut c, Instant::now()), Next::Stopped { .. }));
    }

    #[test]
    fn never_gives_up_at_once() {
        let mut c = VecDeque::new();
        let n = after_exit(RestartPolicy::Never, false, None, &mut c, Instant::now());
        assert_eq!(n, Next::Crashed { message: "exited with a signal".into(), gave_up: true });
    }

    #[test]
    fn back_off_doubles_then_gives_up() {
        let mut c = VecDeque::new();
        let t0 = Instant::now();
        let delays: Vec<u64> = (0..4)
            .map(|i| match after_exit(ON_FAILURE, false, Some(1), &mut c, t0 + Duration::from_secs(i)) {
                Next::Restart { delay, .. } => delay.as_secs(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(delays, [1, 2, 4, 8]);
        match after_exit(ON_FAILURE, false, Some(1), &mut c, t0 + Duration::from_secs(5)) {
            Next::Crashed { message, gave_up: true } => assert!(message.starts_with("crashed 5× in 5m, gave up "), "{message}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn old_crashes_fall_out_of_the_window() {
        let mut c = VecDeque::new();
        let t0 = Instant::now();
        for i in 0..4 {
            after_exit(ON_FAILURE, false, Some(1), &mut c, t0 + Duration::from_secs(i));
        }
        // Six minutes later the earlier four no longer count.
        let later = t0 + Duration::from_secs(6 * 60);
        assert_eq!(
            after_exit(ON_FAILURE, false, Some(1), &mut c, later),
            Next::Restart { delay: Duration::from_secs(1), message: "exited with exit code 1, restarting in 1s".into() }
        );
        assert_eq!(c.len(), 1);
    }
}
