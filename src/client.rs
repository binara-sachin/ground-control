//! Socket client used by the TUI (and the end-to-end tests): hello handshake,
//! request/response correlation, and a stream of daemon events.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

use crate::protocol::*;

#[derive(Debug, Clone)]
pub enum ConnectError {
    /// No daemon is listening (socket missing or refused).
    NotRunning(String),
    /// The daemon speaks another protocol version.
    VersionMismatch {
        daemon: u32,
        daemon_version: String,
    },
    Other(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NotRunning(e) => write!(f, "daemon not running ({e})"),
            ConnectError::VersionMismatch { daemon, daemon_version } => {
                write!(f, "daemon speaks protocol {daemon} (v{daemon_version}); this TUI speaks protocol {PROTOCOL_VERSION} (v{VERSION})")
            }
            ConnectError::Other(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RequestError {
    pub code: ErrorCode,
    pub message: String,
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseBody>>>>;

#[derive(Clone)]
pub struct Client {
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: Arc<AtomicU64>,
    pub daemon_version: String,
}

/// Messages the client delivers to its owner besides responses.
#[derive(Debug)]
pub enum Incoming {
    Event(Event),
    Disconnected,
}

impl Client {
    pub async fn connect(path: &Path) -> Result<(Client, mpsc::UnboundedReceiver<Incoming>), ConnectError> {
        let stream = UnixStream::connect(path).await.map_err(|e| ConnectError::NotRunning(e.to_string()))?;
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let mut hello = serde_json::to_string(&Hello::current()).map_err(|e| ConnectError::Other(e.to_string()))?;
        hello.push('\n');
        write.write_all(hello.as_bytes()).await.map_err(|e| ConnectError::NotRunning(e.to_string()))?;
        let first = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .map_err(|_| ConnectError::Other("daemon did not answer the hello".into()))?
            .map_err(|e| ConnectError::NotRunning(e.to_string()))?
            .ok_or_else(|| ConnectError::NotRunning("connection closed".into()))?;
        let theirs: Hello = serde_json::from_str(&first).map_err(|e| ConnectError::Other(format!("bad hello: {e}")))?;
        if theirs.hello.protocol != PROTOCOL_VERSION {
            return Err(ConnectError::VersionMismatch { daemon: theirs.hello.protocol, daemon_version: theirs.hello.version });
        }

        let (tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(mut line) = out_rx.recv().await {
                line.push('\n');
                if write.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let pending: Pending = Arc::default();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let p2 = pending.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<ServerMessage>(&line) {
                    Ok(ServerMessage::Response { id, body }) => {
                        if let Some(tx) = p2.lock().unwrap().remove(&id) {
                            let _ = tx.send(body);
                        }
                    }
                    Ok(ServerMessage::Event { event }) => {
                        let _ = in_tx.send(Incoming::Event(event));
                    }
                    Err(_) => {}
                }
            }
            p2.lock().unwrap().clear();
            let _ = in_tx.send(Incoming::Disconnected);
        });
        Ok((Client { tx, pending, next: Arc::new(AtomicU64::new(1)), daemon_version: theirs.hello.version }, in_rx))
    }

    pub async fn request(&self, request: Request) -> Result<Response, RequestError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = serde_json::to_string(&ClientMessage { id, request })
            .map_err(|e| RequestError { code: ErrorCode::BadRequest, message: e.to_string() })?;
        let gone = || RequestError { code: ErrorCode::Internal, message: "disconnected from the daemon".into() };
        self.tx.send(line).map_err(|_| gone())?;
        match rx.await.map_err(|_| gone())? {
            ResponseBody::Result(r) => Ok(*r),
            ResponseBody::Error(e) => Err(RequestError { code: e.code, message: e.message }),
        }
    }
}
