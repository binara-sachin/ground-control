//! Socket server (§10.6, §8.7): newline-delimited JSON over a Unix socket,
//! peer UID check on every connection, protocol version handshake, and event
//! fan-out to subscribed clients.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

use super::{Daemon, runner, supervisor};
use crate::protocol::*;

const MAX_LOG_READ: u64 = 1024 * 1024;
const MAX_TEXT: u64 = 4 * 1024 * 1024;

pub async fn serve(d: Arc<Daemon>, listener: UnixListener) -> anyhow::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let d = d.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(d, stream).await {
                tracing::debug!("client connection ended: {e}");
            }
        });
    }
}

async fn handle_conn(d: Arc<Daemon>, stream: UnixStream) -> anyhow::Result<()> {
    let me = nix::unistd::getuid().as_raw();
    let peer = stream.peer_cred()?;
    if peer.uid() != me {
        tracing::warn!(peer_uid = peer.uid(), "refused connection from another user");
        return Ok(());
    }
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    let first = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await??;
    let Some(first) = first else { return Ok(()) };
    let hello: Hello = serde_json::from_str(&first)?;
    let mut ours = serde_json::to_string(&Hello::current())?;
    ours.push('\n');
    write.write_all(ours.as_bytes()).await?;
    if hello.hello.protocol != PROTOCOL_VERSION {
        tracing::info!(client = hello.hello.protocol, ours = PROTOCOL_VERSION, "protocol mismatch; closing");
        return Ok(());
    }

    let (out_tx, mut out_rx) = mpsc::channel::<String>(1024);
    let writer = tokio::spawn(async move {
        while let Some(mut line) = out_rx.recv().await {
            line.push('\n');
            if write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let topics: Arc<Mutex<HashSet<String>>> = Arc::default();
    let mut events = d.subscribe();
    let (ev_tx, ev_topics) = (out_tx.clone(), topics.clone());
    let forwarder = tokio::spawn(async move {
        loop {
            let ev = match events.recv().await {
                Ok(ev) => ev,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(n, "client lagged; asking it to resync");
                    Event::Projects
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let wanted = {
                let t = ev_topics.lock().unwrap();
                t.contains(&ev.topic()) || (t.contains("logs") && matches!(ev, Event::Log(_)))
            };
            if wanted {
                let Ok(line) = serde_json::to_string(&ServerMessage::Event { event: ev }) else { continue };
                if ev_tx.send(line).await.is_err() {
                    break;
                }
            }
        }
    });

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: ClientMessage = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                let id = serde_json::from_str::<serde_json::Value>(&line).ok().and_then(|v| v.get("id")?.as_u64()).unwrap_or(0);
                let body = ResponseBody::Error(ErrorBody { code: ErrorCode::BadRequest, message: format!("bad request: {e}") });
                let _ = out_tx.send(serde_json::to_string(&ServerMessage::Response { id, body })?).await;
                continue;
            }
        };
        let (d, tx, topics) = (d.clone(), out_tx.clone(), topics.clone());
        tokio::spawn(async move {
            let body = match handle(&d, msg.request, &topics).await {
                Ok(r) => ResponseBody::Result(Box::new(r)),
                Err((code, message)) => ResponseBody::Error(ErrorBody { code, message }),
            };
            if let Ok(line) = serde_json::to_string(&ServerMessage::Response { id: msg.id, body }) {
                let _ = tx.send(line).await;
            }
        });
    }
    forwarder.abort();
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

type Reply = Result<Response, runner::ReqError>;

fn internal(msg: &str) -> runner::ReqError {
    (ErrorCode::Internal, msg.to_string())
}

async fn handle(d: &Arc<Daemon>, req: Request, topics: &Mutex<HashSet<String>>) -> Reply {
    match req {
        Request::ProjectsList => Ok(Response::Projects(d.projects())),
        Request::RunStart(p) => runner::start(d, p).await,
        Request::RunCancel { id } => runner::cancel(d, id),
        Request::RunDecide { id, step, decision, confirmed } => runner::decide(d, id, step, decision, confirmed),
        Request::RunsQuery { filter, page } => {
            d.with_store(|db| db.query_runs(&filter, page)).map(Response::Runs).ok_or_else(|| internal("query failed"))
        }
        Request::RunGet { id } => match d.with_store(|db| db.run_detail(id)) {
            Some(Some(r)) => Ok(Response::Run(r)),
            Some(None) => Err((ErrorCode::NotFound, format!("run #{id} not found"))),
            None => Err(internal("query failed")),
        },
        Request::RunDiff { id } => {
            let detail = d.with_store(|db| db.run_detail(id)).flatten().ok_or((ErrorCode::NotFound, format!("run #{id} not found")))?;
            let path = detail.diff_path.ok_or((ErrorCode::NotFound, format!("run #{id} has no saved diff (the tree was clean)")))?;
            let text = read_capped(&path, MAX_TEXT)
                .map_err(|e| (ErrorCode::NotFound, format!("cannot read {path}: {e} (diffs are pruned after 90 days)")))?;
            Ok(Response::Text { text, path: Some(path) })
        }
        Request::LogRead { run, step, from_offset } => log_read(d, run, step, from_offset),
        Request::ServiceControl { project, id, action } => supervisor::control(d, &project, &id, action)
            .await
            .map(|msg| Response::Text { text: msg, path: None })
            .map_err(|e| (ErrorCode::Refused, e)),
        Request::ServiceLog { project, id, max_bytes } => {
            let config = d.config();
            let p = config.project(&project).ok_or((ErrorCode::NotFound, format!("unknown project `{project}`")))?;
            if p.service(&id).is_none() {
                return Err((ErrorCode::NotFound, format!("unknown service `{id}`")));
            }
            let path = crate::paths::service_log_path(&p.id, &p.path, p.sensitive, &id);
            let text = match super::logfile::tail(&path, max_bytes.min(MAX_TEXT)) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => return Err((ErrorCode::Internal, format!("cannot read service log: {e}"))),
            };
            Ok(Response::Text { text, path: Some(path.display().to_string()) })
        }
        Request::HealthGet => Ok(Response::Health(d.health())),
        Request::Subscribe { topics: t } => {
            *topics.lock().unwrap() = t.into_iter().collect();
            Ok(Response::Ok)
        }
    }
}

fn read_capped(path: &str, max: u64) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    (&mut f).take(max).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn log_read(d: &Daemon, run: i64, step: usize, from: u64) -> Reply {
    let info =
        d.with_store(|db| db.step(run, step)).flatten().ok_or((ErrorCode::NotFound, format!("run #{run} has no step {}", step + 1)))?;
    let finished = !matches!(info.status, StepStatus::Running | StepStatus::Pending | StepStatus::Waiting);
    let Some(path) = d.with_store(|db| db.step_log_path(run, step)).flatten() else {
        return Ok(Response::Log(LogChunk { run, step, offset: from, data: String::new(), end: from, complete: finished }));
    };
    let mut f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let data = if finished { "(log pruned after 90 days)\n".to_string() } else { String::new() };
            return Ok(Response::Log(LogChunk { run, step, offset: from, data, end: from, complete: finished }));
        }
        Err(e) => return Err((ErrorCode::Internal, format!("cannot read log: {e}"))),
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = from.min(len);
    f.seek(SeekFrom::Start(from)).map_err(|e| internal(&e.to_string()))?;
    let mut buf = Vec::new();
    (&mut f).take(MAX_LOG_READ).read_to_end(&mut buf).map_err(|e| internal(&e.to_string()))?;
    let end = from + buf.len() as u64;
    // Don't split a UTF-8 sequence across reads.
    let cut = match std::str::from_utf8(&buf) {
        Ok(_) => buf.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => buf.len(),
    };
    let end = end - (buf.len() - cut) as u64;
    buf.truncate(cut);
    Ok(Response::Log(LogChunk {
        run,
        step,
        offset: from,
        data: String::from_utf8_lossy(&buf).into_owned(),
        end,
        complete: finished && end >= len,
    }))
}
