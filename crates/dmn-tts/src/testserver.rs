//! A one-shot HTTP server on a raw `TcpListener` for provider tests, the same pattern as
//! `dmn-llm`'s testserver but with byte bodies: the WAV answer and the multipart enrollment
//! are binary. Reads one request, replies with `status` and `body` (in `pieces` chunks so the
//! stream lands in pieces), and keeps the request bytes for assertions. Loopback only, so
//! tests never touch the network.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// How long a partial read waits before giving up (a hung test fails instead of hanging).
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause between body pieces, so consumers see real streaming.
const PIECE_DELAY: Duration = Duration::from_millis(5);

pub(crate) struct MockServer {
    pub url: String,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    handle: Option<JoinHandle<()>>,
}

impl MockServer {
    /// Every request received so far, oldest first, as raw bytes (head + body).
    pub(crate) fn requests(&self) -> Vec<Vec<u8>> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// The captured request's head and body, split at the blank line.
    pub(crate) fn head_and_body<'a>(request: &'a [u8]) -> (&'a str, &'a [u8]) {
        match request
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 4)
        {
            Some(split) => (
                std::str::from_utf8(&request[..split]).unwrap_or("<non-utf8 head>"),
                &request[split..],
            ),
            None => (std::str::from_utf8(request).unwrap_or("<non-utf8 head>"), &[]),
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            // A test that failed before connecting leaves the thread waiting in `accept`, and
            // joining it would hang the suite on the unwind. Connect once so `accept` returns;
            // the thread reads an empty request, fails to write to the closed socket, and ends.
            if !h.is_finished()
                && let Some(addr) = self.url.strip_prefix("http://")
            {
                let _ = std::net::TcpStream::connect(addr);
            }
            let _ = h.join();
        }
    }
}

/// Accept one connection and answer it with `status` and `body` (in `pieces` chunks), then exit.
pub(crate) fn serve_once(
    status: &str,
    content_type: &str,
    body: Vec<u8>,
    pieces: usize,
) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let status = status.to_owned();
    let content_type = content_type.to_owned();
    let handle = std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
        let request = read_request(&mut stream);
        captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        let piece = body.len().div_ceil(pieces.max(1));
        for chunk in body.chunks(piece.max(1)) {
            if stream.write_all(chunk).is_err() {
                return;
            }
            let _ = stream.flush();
            std::thread::sleep(PIECE_DELAY);
        }
    });
    MockServer {
        url: format!("http://127.0.0.1:{port}"),
        requests,
        handle: Some(handle),
    }
}

/// Read one request: headers up to the blank line, then as many body bytes as Content-Length
/// says (0 when absent).
fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4) {
            let len = content_length(&buf[..head_end]);
            if buf.len() >= head_end + len {
                return buf;
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return buf,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

fn content_length(head: &[u8]) -> usize {
    String::from_utf8_lossy(head)
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A test that fails before connecting drops its server with the thread still in `accept`;
    /// the drop must return, not hang the suite.
    #[test]
    fn dropping_a_server_nobody_connected_to_returns_promptly() {
        let server = serve_once("200 OK", "text/plain", b"never sent".to_vec(), 1);
        let started = Instant::now();
        drop(server);
        assert!(started.elapsed() < READ_TIMEOUT, "{:?}", started.elapsed());
    }
}
