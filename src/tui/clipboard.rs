//! Copying text to the system clipboard.

use tokio::sync::mpsc;

use super::app::{Level, Msg};

/// Copies text with `pbcopy` (the TUI runs on the Mac); falls back to common
/// Linux clipboard tools.
pub(crate) fn copy_to_clipboard(tx: mpsc::UnboundedSender<Msg>, text: String, ok_msg: &'static str) {
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let candidates: [(&str, &[&str]); 3] = [("pbcopy", &[]), ("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])];
        for (cmd, args) in candidates {
            let Ok(mut child) = tokio::process::Command::new(cmd)
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            else {
                continue;
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes()).await;
            }
            if child.wait().await.is_ok_and(|s| s.success()) {
                let _ = tx.send(Msg::Toast(ok_msg.into(), Level::Success));
                return;
            }
        }
        let _ = tx.send(Msg::Toast("no clipboard tool found (pbcopy)".into(), Level::Error));
    });
}
