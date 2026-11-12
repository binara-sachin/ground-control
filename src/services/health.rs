//! Service health checks (§6.5): a TCP port that accepts connections, or a
//! plain-HTTP URL that answers 200.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::Health;

pub async fn check_health(h: &Health) -> bool {
    match h {
        Health::Port(p) => port_in_use(*p).await,
        Health::Url(u) => http_ok(u).await,
    }
}

/// True when something accepts TCP connections on the port (IPv4 or IPv6 loopback).
pub async fn port_in_use(port: u16) -> bool {
    for addr in [format!("127.0.0.1:{port}"), format!("[::1]:{port}")] {
        if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(&addr)).await {
            return true;
        }
    }
    false
}

/// Minimal HTTP/1.1 GET that expects status 200. Local, plain HTTP only.
async fn http_ok(url: &str) -> bool {
    let Some((host, port, path)) = crate::config::parse_http_url(url) else { return false };
    let attempt = async {
        let mut stream = TcpStream::connect((host.as_str(), port)).await.ok()?;
        let req = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUser-Agent: groundcontrol\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.ok()?;
        let mut buf = [0u8; 64];
        let mut n = 0;
        while n < 12 {
            let m = stream.read(&mut buf[n..]).await.ok()?;
            if m == 0 {
                break;
            }
            n += m;
        }
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        Some(head.split_whitespace().nth(1) == Some("200"))
    };
    matches!(tokio::time::timeout(Duration::from_secs(3), attempt).await, Ok(Some(true)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_check() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            for status in ["200 OK", "503 Service Unavailable"] {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes()).await;
            }
        });
        assert!(http_ok(&format!("http://127.0.0.1:{port}/health")).await);
        assert!(!http_ok(&format!("http://127.0.0.1:{port}/health")).await);
    }

    #[tokio::test]
    async fn open_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(port_in_use(listener.local_addr().unwrap().port()).await);
    }

    #[tokio::test]
    async fn closed_port() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(!port_in_use(port).await);
        assert!(!http_ok(&format!("http://127.0.0.1:{port}/")).await);
    }
}
