//! The plumbing the service worker and the run worker share: writing to the
//! pane, the signals that end a worker, waking on a request, telling attached
//! tmux clients, and closing its own pane.

use std::io::Write;
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};

use crate::tmux::Tmux;

/// Writes to the pane. Never fails: once the window is closed the terminal is
/// gone and writes return EIO, which must not stop the cleanup.
pub(crate) fn pane(bytes: &[u8]) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

pub(crate) fn say(msg: &str) {
    pane(format!("\x1b[2m── {msg} ──\x1b[0m\n").as_bytes());
}

pub(crate) struct Signals {
    int: tokio::signal::unix::Signal,
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

impl Signals {
    pub(crate) fn install() -> std::io::Result<Signals> {
        Ok(Signals { int: signal(SignalKind::interrupt())?, term: signal(SignalKind::terminate())?, hup: signal(SignalKind::hangup())? })
    }

    pub(crate) async fn recv(&mut self) {
        tokio::select! {
            _ = self.int.recv() => {}
            _ = self.term.recv() => {}
            _ = self.hup.recv() => {}
        }
    }
}

/// Shows a message to every attached tmux client (see `Tmux::tell_clients`).
pub(crate) async fn tell(msg: &str) {
    if std::env::var_os("TMUX").is_some() {
        let _ = crate::tmux::Tmux::default_server().tell_clients(msg).await;
    }
}

/// Waits for the TUI to write a request to the worker's window. One `tmux
/// wait-for` child stays blocked for as long as nothing happens, so an idle
/// worker starts no processes; the TUI signals the channel after each write.
pub(crate) struct Waker {
    tmux: Tmux,
    channel: String,
    waiting: Option<Wait>,
}

/// A blocked `tmux wait-for`.
type Wait = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;

impl Waker {
    pub(crate) fn new(tmux: Tmux, window: i64) -> Waker {
        Waker { tmux, channel: crate::tmux::wake_channel(window), waiting: None }
    }

    /// Resolves when the window has been sent a request. Safe to drop
    /// unfinished (in a `select!`): the same wait carries on next time.
    pub(crate) async fn woken(&mut self) {
        let waiting = self.waiting.get_or_insert_with(|| {
            let (tmux, channel) = (self.tmux.clone(), self.channel.clone());
            Box::pin(async move { tmux.wait_for(&channel).await })
        });
        let result = waiting.await;
        self.waiting = None;
        if result.is_err() {
            // The tmux server is going away; don't spin.
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Closes the pane this worker runs in, when it runs in tmux.
pub(crate) async fn close_own_pane() {
    if let Ok(pane_id) = std::env::var("TMUX_PANE") {
        let _ = crate::tmux::Tmux::default_server().kill_pane(&pane_id).await;
    }
}
