//! Synchronous TCP client for `folkering-proxy` endpoints.
//!
//! Same wire format the kernel uses, so we can validate end-to-end
//! against the real proxy without spinning up the OS:
//!
//! Reply frame (shared by LLM, PATCH, GRAPH_CALLERS):
//! ```text
//!   [u32 status LE][u32 output_len LE][output_len bytes payload]
//! ```
//!
//! LLM request:
//! ```text
//!   LLM <model>\n
//!   <prompt_byte_len decimal>\n
//!   <prompt bytes>
//! ```
//!
//! GRAPH_CALLERS request:
//! ```text
//!   GRAPH_CALLERS <fn_name>\n
//! ```
//!
//! All status codes match the proxy crate's published constants
//! (see `folkering-proxy/src/llm.rs`, `codegraph.rs`, `patch.rs`).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Default proxy endpoint. Matches the address the kernel pins to
/// (`10.0.0.2:14711` from inside QEMU; `127.0.0.1:14711` from host).
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 14711;

#[derive(Debug)]
pub enum ProxyError {
    Io(io::Error),
    NoAddr,
    ShortResponse { wanted: usize, got: usize },
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProxyError::Io(e) => write!(f, "io: {e}"),
            ProxyError::NoAddr => write!(f, "no socket address resolved"),
            ProxyError::ShortResponse { wanted, got } =>
                write!(f, "short response: wanted {wanted} bytes, got {got}"),
        }
    }
}

impl std::error::Error for ProxyError {}

impl From<io::Error> for ProxyError {
    fn from(e: io::Error) -> Self { ProxyError::Io(e) }
}

#[derive(Debug)]
pub struct ProxyResponse {
    pub status: u32,
    pub body: Vec<u8>,
}

impl ProxyResponse {
    pub fn body_str(&self) -> Option<&str> { core::str::from_utf8(&self.body).ok() }
}

/// Send `LLM <model>\n<len>\n<prompt>` and read the framed response.
/// Timeout includes the full Ollama round-trip; cloud-backed models
/// can take ~60s cold.
pub fn llm_generate(
    addr: (&str, u16),
    model: &str,
    prompt: &str,
    timeout: Duration,
) -> Result<ProxyResponse, ProxyError> {
    let mut req = Vec::with_capacity(prompt.len() + 64);
    req.extend_from_slice(b"LLM ");
    req.extend_from_slice(model.as_bytes());
    req.push(b'\n');
    req.extend_from_slice(prompt.len().to_string().as_bytes());
    req.push(b'\n');
    req.extend_from_slice(prompt.as_bytes());
    send_and_read(addr, &req, timeout)
}

/// Send `GRAPH_CALLERS <fn>\n` and read the framed response. The
/// proxy returns `\n`-separated qualified names in the body on status=0.
pub fn graph_callers(
    addr: (&str, u16),
    fn_name: &str,
    timeout: Duration,
) -> Result<ProxyResponse, ProxyError> {
    let mut req = Vec::with_capacity(fn_name.len() + 16);
    req.extend_from_slice(b"GRAPH_CALLERS ");
    req.extend_from_slice(fn_name.as_bytes());
    req.push(b'\n');
    send_and_read(addr, &req, timeout)
}

// ── plumbing ────────────────────────────────────────────────────────

fn send_and_read(
    addr: (&str, u16),
    request: &[u8],
    timeout: Duration,
) -> Result<ProxyResponse, ProxyError> {
    let target: SocketAddr = (addr.0, addr.1)
        .to_socket_addrs()?
        .next()
        .ok_or(ProxyError::NoAddr)?;

    let mut stream = TcpStream::connect_timeout(&target, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(request)?;
    stream.flush()?;

    // Read [u32 status][u32 len] header.
    let mut hdr = [0u8; 8];
    stream.read_exact(&mut hdr)?;
    let status = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;

    let mut body = vec![0u8; len];
    if len > 0 {
        match stream.read_exact(&mut body) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(ProxyError::ShortResponse { wanted: len, got: 0 });
            }
            Err(e) => return Err(ProxyError::Io(e)),
        }
    }

    Ok(ProxyResponse { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live test against a running proxy. Skipped when port 14711 isn't
    /// open — keeps `cargo test` green on machines that don't have the
    /// proxy running.
    #[test]
    fn live_graph_callers_when_proxy_up() {
        let addr = (DEFAULT_HOST, DEFAULT_PORT);
        let probe = TcpStream::connect_timeout(
            &(addr.0, addr.1).to_socket_addrs().unwrap().next().unwrap(),
            Duration::from_millis(200),
        );
        if probe.is_err() {
            eprintln!("[skip] proxy not reachable on {DEFAULT_HOST}:{DEFAULT_PORT}");
            return;
        }
        drop(probe);

        // Use a fn we know exists in the running proxy's loaded CSR.
        // pop_i32_slot has 29 callers; status=0 + non-empty body expected.
        let resp = graph_callers(addr, "pop_i32_slot", Duration::from_secs(5))
            .expect("graph_callers");
        assert_eq!(resp.status, 0, "GRAPH_CALLERS pop_i32_slot expected status=0");
        assert!(!resp.body.is_empty(), "expected non-empty body");
        let s = resp.body_str().unwrap();
        assert!(s.contains("Lowerer"), "expected qualified caller, got {s:?}");
    }
}
