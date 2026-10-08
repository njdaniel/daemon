//! A stand-in Ollama for tests (#113): answers `/api/chat` the way Ollama does (a warm-up or an
//! unload, `stream: false`, with an empty reply; a chat with one NDJSON line of `reply` and a
//! closing `done` line), `/api/ps` and `/api/tags` with nothing loaded, and keeps every request
//! so a test can say which model each one named. Loopback only; one thread per connection.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One request as the server saw it.
#[derive(Debug, Clone)]
pub struct Seen {
    /// `POST /api/chat`.
    pub line: String,
    /// The JSON body (`Null` when there was none).
    pub body: serde_json::Value,
}

impl Seen {
    /// The model the request named.
    pub fn model(&self) -> &str {
        self.body["model"].as_str().unwrap_or("")
    }

    /// A load with nothing to generate: Ollama's warm-up (`messages: []`, not an unload).
    pub fn is_warm_up(&self) -> bool {
        self.line.ends_with("/api/chat") && self.messages() == 0 && !self.is_unload()
    }

    /// `keep_alive: 0`: drop the model from memory (#79).
    pub fn is_unload(&self) -> bool {
        self.body["keep_alive"] == 0
    }

    /// A chat that asks for text: a translation or a fast phase.
    pub fn is_chat(&self) -> bool {
        self.line.ends_with("/api/chat") && self.messages() > 0
    }

    fn messages(&self) -> usize {
        self.body["messages"].as_array().map_or(0, Vec::len)
    }
}

pub struct FakeOllama {
    pub url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl FakeOllama {
    /// Answer every chat with `reply`.
    pub fn start(reply: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&seen);
        let reply = reply.to_owned();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let (kept, reply) = (Arc::clone(&kept), reply.clone());
                std::thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let (line, body) = read_request(&mut stream);
                    let body: serde_json::Value =
                        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                    let seen = Seen { line, body };
                    let answer = answer(&seen, &reply);
                    kept.lock().unwrap().push(seen);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                        answer.len()
                    );
                });
            }
        });
        Self { url, seen }
    }

    /// Every request so far, oldest first.
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// Wait (up to 5 s) until a request matches `want`.
    pub fn wait_for(&self, want: impl Fn(&Seen) -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            if self.seen().iter().any(&want) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

fn answer(seen: &Seen, reply: &str) -> String {
    let model = seen.model();
    if seen.line.contains("/api/ps") || seen.line.contains("/api/tags") {
        return r#"{"models":[]}"#.to_owned();
    }
    if seen.body["stream"] == false {
        return format!(
            r#"{{"model":"{model}","message":{{"role":"assistant","content":""}},"done":true}}"#
        );
    }
    let text = serde_json::to_string(reply).unwrap_or_default();
    format!(
        "{{\"model\":\"{model}\",\"message\":{{\"role\":\"assistant\",\"content\":{text}}},\"done\":false}}\n\
         {{\"model\":\"{model}\",\"message\":{{\"role\":\"assistant\",\"content\":\"\"}},\"done\":true,\"prompt_eval_count\":1,\"eval_count\":1}}\n"
    )
}

/// The request line and the body (as many bytes as `Content-Length` says).
fn read_request(stream: &mut std::net::TcpStream) -> (String, String) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let text = String::from_utf8_lossy(&buf).into_owned();
        if let Some((head, rest)) = text.split_once("\r\n\r\n") {
            let len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if rest.len() >= len {
                let line = head.lines().next().unwrap_or("");
                let line = line.rsplit_once(' ').map_or(line, |(l, _)| l).to_owned();
                return (line, rest.to_owned());
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return (String::new(), String::new()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}
