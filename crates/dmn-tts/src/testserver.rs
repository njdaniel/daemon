//! A scripted HTTP server on a raw `TcpListener` for tests, the same pattern as `dmn-llm`'s
//! testserver but with byte bodies (the WAV answer and the multipart enrollment are binary) and
//! a list of replies, one per connection, since one synthesis is two requests (the voice check,
//! then the speech) and a re-enrollment three. Each reply's body goes out in pieces of a given
//! size with a pause between them, with a `Content-Length` or chunked like the server's
//! streaming route, so consumers see real streaming and pieces that split the WAV header and
//! samples at odd offsets. Loopback only, so tests never touch the network.
//!
//! Compiled for this crate's tests and, with the `test-server` feature, for `dmn-cli`'s.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

/// How long a partial read waits before giving up (a hung test fails instead of hanging).
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause between body pieces, so consumers see real streaming: the WAV header and the first
/// samples arrive in separate reads. Only the first [`PAUSED_PIECES`] pieces pause, so a body
/// sent in 7-byte pieces does not take seconds.
const PIECE_DELAY: Duration = Duration::from_millis(2);
const PAUSED_PIECES: usize = 32;

/// One scripted answer.
#[derive(Debug, Clone)]
pub struct Reply {
    status: String,
    content_type: String,
    body: Vec<u8>,
    piece: usize,
    chunked: bool,
}

impl Reply {
    /// `status` like `"200 OK"`, sent whole with a `Content-Length`.
    pub fn new(status: &str, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: status.to_owned(),
            content_type: content_type.to_owned(),
            body: body.into(),
            piece: usize::MAX,
            chunked: false,
        }
    }

    /// `200 OK` with a JSON body.
    pub fn json(body: &str) -> Self {
        Self::new("200 OK", "application/json", body)
    }

    /// `200 OK` with a WAV body.
    pub fn wav(body: Vec<u8>) -> Self {
        Self::new("200 OK", "audio/wav", body)
    }

    /// The voice library listing chatterbox-tts-api answers `GET /v1/voices` with.
    pub fn voices(names: &[&str]) -> Self {
        let voices: Vec<String> = names
            .iter()
            .map(|n| format!(r#"{{"name":"{n}","language":"en","aliases":[],"exists":true}}"#))
            .collect();
        Self::json(&format!(
            r#"{{"voices":[{}],"count":{}}}"#,
            voices.join(","),
            names.len()
        ))
    }

    /// Send the body in pieces of `bytes` bytes.
    pub fn in_pieces_of(mut self, bytes: usize) -> Self {
        self.piece = bytes.max(1);
        self
    }

    /// `Transfer-Encoding: chunked` instead of a `Content-Length`, one HTTP chunk per piece.
    pub fn chunked(mut self) -> Self {
        self.chunked = true;
        self
    }
}

/// The running server: its URL, and every request it has received.
pub struct MockServer {
    pub url: String,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockServer {
    /// Every request received so far, oldest first, as raw bytes (head + body).
    pub fn requests(&self) -> Vec<Vec<u8>> {
        self.requests
            .lock()
            .map(|r| r.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }

    /// The captured request's head and body, split at the blank line.
    pub fn head_and_body(request: &[u8]) -> (&str, &[u8]) {
        match request
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 4)
        {
            Some(split) => (
                std::str::from_utf8(&request[..split]).unwrap_or("<non-utf8 head>"),
                &request[split..],
            ),
            None => (
                std::str::from_utf8(request).unwrap_or("<non-utf8 head>"),
                &[],
            ),
        }
    }

    /// The request line of every request so far: `["GET /v1/voices", "POST /v1/audio/speech"]`.
    pub fn request_lines(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| {
                let (head, _) = Self::head_and_body(r);
                let line = head.lines().next().unwrap_or("");
                line.rsplit_once(' ')
                    .map_or(line, |(l, _version)| l)
                    .to_owned()
            })
            .collect()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            // A test that failed (or made fewer requests than scripted) leaves the thread in
            // `accept`; one connection wakes it, it sees `stop`, and it ends.
            if !h.is_finished()
                && let Some(addr) = self.url.strip_prefix("http://")
            {
                let _ = TcpStream::connect(addr);
            }
            let _ = h.join();
        }
    }
}

/// Answer one connection per reply, in order, then exit.
pub fn serve(replies: Vec<Reply>) -> std::io::Result<MockServer> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (captured, stopped) = (Arc::clone(&requests), Arc::clone(&stop));
    let handle = std::thread::spawn(move || {
        for reply in replies {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            if stopped.load(Ordering::SeqCst) {
                return;
            }
            let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
            let request = read_request(&mut stream);
            captured
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request);
            let _ = answer(&mut stream, &reply);
        }
    });
    Ok(MockServer {
        url: format!("http://127.0.0.1:{port}"),
        requests,
        stop,
        handle: Some(handle),
    })
}

/// A port nothing listens on: bound, then released.
pub fn closed_port_url() -> std::io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(format!("http://127.0.0.1:{port}"))
}

fn answer(stream: &mut TcpStream, reply: &Reply) -> std::io::Result<()> {
    let length = if reply.chunked {
        "Transfer-Encoding: chunked".to_owned()
    } else {
        format!("Content-Length: {}", reply.body.len())
    };
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\n{length}\r\nConnection: close\r\n\r\n",
        reply.status, reply.content_type
    );
    stream.write_all(head.as_bytes())?;
    for (i, piece) in reply.body.chunks(reply.piece).enumerate() {
        if reply.chunked {
            stream.write_all(format!("{:x}\r\n", piece.len()).as_bytes())?;
            stream.write_all(piece)?;
            stream.write_all(b"\r\n")?;
        } else {
            stream.write_all(piece)?;
        }
        stream.flush()?;
        if reply.piece != usize::MAX && i < PAUSED_PIECES {
            std::thread::sleep(PIECE_DELAY);
        }
    }
    if reply.chunked {
        stream.write_all(b"0\r\n\r\n")?;
    }
    stream.flush()
}

/// Read one request: headers up to the blank line, then as many body bytes as Content-Length
/// says (0 when absent).
fn read_request(stream: &mut TcpStream) -> Vec<u8> {
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

    /// A test that fails before connecting, or makes fewer requests than scripted, drops its
    /// server with the thread still in `accept`; the drop must return, not hang the suite.
    #[test]
    fn dropping_a_server_with_unused_replies_returns_promptly() {
        let server = serve(vec![
            Reply::json("{}"),
            Reply::json("{}"),
            Reply::json("{}"),
        ])
        .unwrap();
        let started = Instant::now();
        drop(server);
        assert!(started.elapsed() < READ_TIMEOUT, "{:?}", started.elapsed());
    }
}
