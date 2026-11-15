//! A forward HTTP proxy for tests: takes `GET http://host/path` requests,
//! fetches them from the origin and relays the answer, counting what it
//! carried. With credentials set it answers 407 to requests without them.
//! Plain HTTP only (no CONNECT), which is all the fixture sites need.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The fake credentials an authenticated test proxy expects.
pub const PROXY_USER: &str = "proxyuser";
pub const PROXY_PASSWORD: &str = "proxypass";

pub struct ProxyServer {
    addr: SocketAddr,
    carried: Arc<AtomicUsize>,
    rejected: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl ProxyServer {
    /// Starts on a free localhost port; `auth` requires [`PROXY_USER`] and
    /// [`PROXY_PASSWORD`].
    pub async fn start(auth: bool) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let carried = Arc::new(AtomicUsize::new(0));
        let rejected = Arc::new(AtomicUsize::new(0));
        let (c, r) = (Arc::clone(&carried), Arc::clone(&rejected));
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (c, r) = (Arc::clone(&c), Arc::clone(&r));
                tokio::spawn(async move {
                    let _ = serve(stream, auth, &c, &r).await;
                });
            }
        });
        Ok(Self {
            addr,
            carried,
            rejected,
            task,
        })
    }

    /// `http://127.0.0.1:PORT`, credentials included when `with_auth`.
    pub fn url(&self, with_auth: bool) -> String {
        if with_auth {
            format!("http://{PROXY_USER}:{PROXY_PASSWORD}@{}", self.addr)
        } else {
            format!("http://{}", self.addr)
        }
    }

    /// Requests relayed to an origin.
    pub fn carried(&self) -> usize {
        self.carried.load(Ordering::SeqCst)
    }

    /// Requests turned away for missing credentials.
    pub fn rejected(&self) -> usize {
        self.rejected.load(Ordering::SeqCst)
    }
}

impl Drop for ProxyServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One client connection: a request is relayed over a fresh connection to
/// the origin, which is asked to close, and the client connection closes
/// with it. A 407 keeps the connection for the retry with credentials.
async fn serve(
    mut client: TcpStream,
    auth: bool,
    carried: &AtomicUsize,
    rejected: &AtomicUsize,
) -> std::io::Result<()> {
    let expected = format!(
        "basic {}",
        base64(format!("{PROXY_USER}:{PROXY_PASSWORD}").as_bytes())
    );
    let mut buffer = Vec::new();
    loop {
        let Some(head_end) = read_head(&mut client, &mut buffer).await? else {
            return Ok(());
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
        let mut rest = buffer.split_off(head_end + 4);
        buffer.clear();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default().to_owned();
        let mut parts = request_line.split(' ');
        let (method, target, version) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or("HTTP/1.1"),
        );
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
            .collect();
        let header = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        };
        let length: usize = header("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        while rest.len() < length {
            let mut chunk = [0u8; 4096];
            let n = client.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            rest.extend_from_slice(&chunk[..n]);
        }
        let body: Vec<u8> = rest.drain(..length.min(rest.len())).collect();
        buffer = rest;

        if auth
            && header("proxy-authorization").map(|v| v.to_ascii_lowercase())
                != Some(expected.to_ascii_lowercase())
        {
            rejected.fetch_add(1, Ordering::SeqCst);
            client
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\ncontent-length: 0\r\n\r\n",
                )
                .await?;
            continue;
        }
        let Some(rest_of_url) = target.strip_prefix("http://") else {
            client
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await?;
            return Ok(());
        };
        let (authority, path) = match rest_of_url.find('/') {
            Some(i) => (&rest_of_url[..i], &rest_of_url[i..]),
            None => (rest_of_url, "/"),
        };

        let mut origin = TcpStream::connect(authority).await?;
        let mut forward = format!("{method} {path} {version}\r\n");
        for (name, value) in &headers {
            let lower = name.to_ascii_lowercase();
            if lower.starts_with("proxy-") || lower == "connection" || lower == "keep-alive" {
                continue;
            }
            forward.push_str(&format!("{name}: {value}\r\n"));
        }
        forward.push_str("connection: close\r\n\r\n");
        origin.write_all(forward.as_bytes()).await?;
        origin.write_all(&body).await?;
        let mut answer = Vec::new();
        origin.read_to_end(&mut answer).await?;
        carried.fetch_add(1, Ordering::SeqCst);
        client.write_all(&answer).await?;
        return Ok(());
    }
}

/// Reads until a blank line; returns where the head ends, or `None` at EOF.
async fn read_head(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> std::io::Result<Option<usize>> {
    loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            return Ok(Some(end));
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
