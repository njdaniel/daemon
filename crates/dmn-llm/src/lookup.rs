//! #102: the web lookup. When the fast phase says an answer depends on what is current (its
//! `lookup` query, #101) and `[lookup].enabled` is on, the suggester sends that query, and only
//! that query, to a web search; the results come back here as [`SearchResult`]s, are cut to
//! [`FreshFact`]s in `limits.rs` and go into the fast phase's second request under
//! `## Fresh facts` ([`crate::FastRequest::facts`]).
//!
//! [`WebSearch`] is the seam: [`OllamaSearch`] is Ollama's hosted web search, `POST
//! <base_url>/api/web_search` with `{"query", "max_results"}` and a bearer key
//! (<https://docs.ollama.com/capabilities/web-search>), and another provider is one more
//! implementation. The reply is read tolerantly: a `results` array whose items carry `title`,
//! `url` and `content`; an item missing any of them is skipped, unknown fields are ignored.
//!
//! Every failure is a [`LookupError`] and none is retried: the caller keeps the first answer.
//! The key goes into the `Authorization` header and nowhere else: [`OllamaSearch`] has no
//! `Debug`, [`OllamaSearchSettings`]'s `Debug` prints `<redacted>`, and error text is scrubbed
//! of it. Without a key nothing is sent at all ([`LookupError::NoKey`]).

use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const NAME: &str = "ollama";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// The most results Ollama's web search returns for one query.
pub const MAX_RESULTS: usize = 10;
/// How much of a reply is read. Three results of page text are tens of KB; a reply past this is
/// not a search result list, and reading it would only spend the deadline.
pub const BODY_CAP: usize = 2 * 1024 * 1024;
/// How much of a non-2xx body goes into the error.
const ERROR_BODY_CHARS: usize = 300;

/// Why a lookup produced nothing. The first answer and its `may be out of date` tag stay.
#[derive(Debug, thiserror::Error)]
pub enum LookupError {
    #[error("no key: ${0} is unset or empty")]
    NoKey(String),
    #[error("{provider} web search unreachable at {url}: {detail}")]
    Unreachable {
        provider: &'static str,
        url: String,
        detail: String,
    },
    #[error("{provider} web search returned HTTP {status}: {body}")]
    HttpStatus {
        provider: &'static str,
        status: u16,
        body: String,
    },
    #[error("{provider} web search reply malformed: {detail}")]
    Malformed {
        provider: &'static str,
        detail: String,
    },
    #[error("web search timed out after {0:?}")]
    Timeout(Duration),
    #[error("web search cancelled")]
    Cancelled,
    #[error("web search found nothing usable")]
    NoResults,
}

/// One search result as the provider returned it: untrusted text from the web, not yet cut.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub content: String,
}

/// One result as the prompt carries it, from [`crate::limits::fresh_facts`]: one line each,
/// control characters gone, the snippet at most 300 characters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FreshFact {
    pub title: String,
    pub snippet: String,
    /// The result's host without `www.`, like `go.dev`.
    pub domain: String,
}

/// What the second fast request (and the slow phase after it) carries under `## Fresh facts`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FreshFacts {
    /// The query that found them, the fast phase's own `lookup`.
    pub query: String,
    pub facts: Vec<FreshFact>,
}

impl FreshFacts {
    /// The first result's domain: what the Approach's tag names (`checked · go.dev`).
    pub fn domain(&self) -> Option<&str> {
        self.facts.first().map(|f| f.domain.as_str())
    }
}

/// A web search. Object-safe for the same reason [`crate::Provider`] is: the suggester holds the
/// one `[lookup].provider` names behind an `Arc<dyn WebSearch>`.
pub trait WebSearch: Send + Sync {
    /// Stable name for logs ("ollama").
    fn name(&self) -> &'static str;

    /// Search for `query` and return what came back, in the provider's order. Ends with
    /// [`LookupError::Cancelled`] once `cancel` fires and [`LookupError::Timeout`] at the
    /// search's own deadline; an empty list is [`LookupError::NoResults`].
    fn search<'a>(
        &'a self,
        query: &'a str,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SearchResult>, LookupError>>;
}

/// Everything [`OllamaSearch`] needs from `[lookup]`; built in `dmn-core`, which reads the key
/// from the environment.
#[derive(Clone, PartialEq, Eq)]
pub struct OllamaSearchSettings {
    /// `https://ollama.com` by default; tests point it at a loopback server.
    pub base_url: String,
    /// From the variable `key_env` names; `None` when it is unset or empty.
    pub api_key: Option<String>,
    /// The variable's name, for the error that says it is unset.
    pub key_env: String,
    /// 1 to [`MAX_RESULTS`]; anything else is clamped into that range.
    pub max_results: usize,
    /// For the whole search: connect, headers and body.
    pub timeout: Duration,
}

impl Default for OllamaSearchSettings {
    fn default() -> Self {
        Self {
            base_url: OllamaSearch::DEFAULT_BASE_URL.to_owned(),
            api_key: None,
            key_env: "OLLAMA_API_KEY".to_owned(),
            max_results: 3,
            timeout: Duration::from_millis(3000),
        }
    }
}

impl std::fmt::Debug for OllamaSearchSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaSearchSettings")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("key_env", &self.key_env)
            .field("max_results", &self.max_results)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Ollama's hosted web search. Deliberately no `Debug`: it holds the key.
pub struct OllamaSearch {
    client: reqwest::Client,
    url: String,
    api_key: Option<String>,
    key_env: String,
    max_results: usize,
    timeout: Duration,
}

impl OllamaSearch {
    pub const DEFAULT_BASE_URL: &'static str = "https://ollama.com";

    pub fn new(settings: &OllamaSearchSettings) -> Result<Self, LookupError> {
        let base = settings.base_url.trim_end_matches('/');
        let url = format!("{base}/api/web_search");
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT.min(settings.timeout))
            .build()
            .map_err(|e| LookupError::Unreachable {
                provider: NAME,
                url: url.clone(),
                detail: format!("cannot build HTTP client: {e}"),
            })?;
        Ok(Self {
            client,
            url,
            api_key: settings.api_key.clone().filter(|k| !k.is_empty()),
            key_env: settings.key_env.clone(),
            max_results: settings.max_results.clamp(1, MAX_RESULTS),
            timeout: settings.timeout,
        })
    }

    /// Where the query goes.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The whole request body: the query and how many results, nothing else. Public so tests
    /// can assert it.
    pub fn body(&self, query: &str) -> serde_json::Value {
        serde_json::json!({ "query": query, "max_results": self.max_results })
    }

    async fn run(
        &self,
        query: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<SearchResult>, LookupError> {
        let Some(key) = self.api_key.as_deref() else {
            return Err(LookupError::NoKey(self.key_env.clone()));
        };
        let deadline = Instant::now() + self.timeout;
        let send = self
            .client
            .post(&self.url)
            .bearer_auth(key)
            .json(&self.body(query))
            .send();
        let resp = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(LookupError::Cancelled),
            () = tokio::time::sleep_until(deadline) => return Err(LookupError::Timeout(self.timeout)),
            r = send => r.map_err(|e| LookupError::Unreachable {
                provider: NAME,
                url: self.url.clone(),
                detail: e.to_string(),
            })?,
        };
        let status = resp.status();
        let mut body: Vec<u8> = Vec::new();
        let mut bytes = resp.bytes_stream();
        let read = async {
            while let Some(chunk) = bytes.next().await {
                let chunk = chunk.map_err(|e| LookupError::Malformed {
                    provider: NAME,
                    detail: format!("body read failed: {e}"),
                })?;
                body.extend_from_slice(&chunk);
                if body.len() > BODY_CAP {
                    return Err(LookupError::Malformed {
                        provider: NAME,
                        detail: format!("reply over {BODY_CAP} bytes"),
                    });
                }
            }
            Ok(())
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(LookupError::Cancelled),
            () = tokio::time::sleep_until(deadline) => return Err(LookupError::Timeout(self.timeout)),
            r = read => r?,
        }
        let text = String::from_utf8_lossy(&body);
        if !status.is_success() {
            return Err(LookupError::HttpStatus {
                provider: NAME,
                status: status.as_u16(),
                body: text.chars().take(ERROR_BODY_CHARS).collect(),
            });
        }
        let results = parse_results(&text)?;
        if results.is_empty() {
            return Err(LookupError::NoResults);
        }
        Ok(results)
    }
}

impl WebSearch for OllamaSearch {
    fn name(&self) -> &'static str {
        NAME
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SearchResult>, LookupError>> {
        Box::pin(async move {
            let result = self.run(query, &cancel).await;
            result.map_err(|e| redact(e, self.api_key.as_deref()))
        })
    }
}

/// `{"results": [{"title", "url", "content"}, …]}`, read tolerantly: an item missing a field, or
/// with one that is not a string, is skipped; unknown fields are ignored. A body that is not JSON,
/// or has no `results` array, is [`LookupError::Malformed`].
pub fn parse_results(body: &str) -> Result<Vec<SearchResult>, LookupError> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| LookupError::Malformed {
            provider: NAME,
            detail: e.to_string(),
        })?;
    let Some(items) = value.get("results").and_then(serde_json::Value::as_array) else {
        return Err(LookupError::Malformed {
            provider: NAME,
            detail: "no `results` array".to_owned(),
        });
    };
    let field = |item: &serde_json::Value, name: &str| {
        item.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    Ok(items
        .iter()
        .filter_map(|item| {
            Some(SearchResult {
                title: field(item, "title")?,
                url: field(item, "url")?,
                content: field(item, "content")?,
            })
        })
        .collect())
}

/// `err` with every occurrence of `secret` replaced: a server may quote the key back in an
/// error body, and errors reach the log.
fn redact(err: LookupError, secret: Option<&str>) -> LookupError {
    let Some(secret) = secret.filter(|s| !s.is_empty()) else {
        return err;
    };
    let scrub = |s: String| s.replace(secret, "<redacted>");
    match err {
        LookupError::Unreachable {
            provider,
            url,
            detail,
        } => LookupError::Unreachable {
            provider,
            url: scrub(url),
            detail: scrub(detail),
        },
        LookupError::HttpStatus {
            provider,
            status,
            body,
        } => LookupError::HttpStatus {
            provider,
            status,
            body: scrub(body),
        },
        LookupError::Malformed { provider, detail } => LookupError::Malformed {
            provider,
            detail: scrub(detail),
        },
        e @ (LookupError::NoKey(_)
        | LookupError::Timeout(_)
        | LookupError::Cancelled
        | LookupError::NoResults) => e,
    }
}

/// The host of a result's `url` as the tag names it: lowercase, without `www.`; `None` for
/// anything but an `http(s)` URL with a host.
pub fn domain_of(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?.trim_end_matches('.').to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_owned();
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::{MockServer, serve_once};

    const GO_REPLY: &str = r#"{"results": [
        {"title": "All releases - The Go Programming Language", "url": "https://go.dev/doc/devel/release", "content": "go1.25.1 (released 2025-09-03) includes security fixes to the net/http package."},
        {"title": "Go 1.25 is released", "url": "https://www.go.dev/blog/go1.25", "content": "Today the Go team is happy to release Go 1.25."}
    ]}"#;

    fn settings(base_url: &str) -> OllamaSearchSettings {
        OllamaSearchSettings {
            base_url: base_url.to_owned(),
            api_key: Some("sk-test-key".into()),
            ..OllamaSearchSettings::default()
        }
    }

    async fn search(
        s: &OllamaSearchSettings,
        query: &str,
    ) -> Result<Vec<SearchResult>, LookupError> {
        OllamaSearch::new(s)
            .unwrap()
            .search(query, CancellationToken::new())
            .await
    }

    #[tokio::test]
    async fn lookup_posts_the_query_alone_with_the_key_as_a_bearer_header() {
        let server = serve_once("200 OK", "application/json", GO_REPLY.to_owned(), 3);
        let results = search(&settings(&server.url), "latest stable Go release")
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://go.dev/doc/devel/release");

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let (head, body) = MockServer::head_and_body(&requests[0]);
        assert!(head.starts_with("POST /api/web_search HTTP/1.1"), "{head}");
        assert!(
            head.to_lowercase()
                .contains("authorization: bearer sk-test-key"),
            "{head}"
        );
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"query": "latest stable Go release", "max_results": 3}),
            "the query and the count, nothing else"
        );
    }

    #[tokio::test]
    async fn lookup_without_a_key_sends_nothing() {
        let server = serve_once("200 OK", "application/json", GO_REPLY.to_owned(), 1);
        let s = OllamaSearchSettings {
            api_key: Some(String::new()),
            ..settings(&server.url)
        };
        let err = search(&s, "latest Go").await.unwrap_err();
        assert!(
            matches!(&err, LookupError::NoKey(var) if var == "OLLAMA_API_KEY"),
            "{err}"
        );
        assert!(server.requests().is_empty());
    }

    #[test]
    fn lookup_reads_results_tolerantly() {
        let body = r#"{"results": [
            {"title": "A", "url": "https://a.example/x", "content": "alpha", "score": 0.9},
            {"title": "no url", "content": "skipped"},
            {"title": "B", "url": "https://b.example/", "content": 42},
            {"url": "https://c.example/", "content": "no title"},
            {"title": "D", "url": "https://d.example/", "content": "delta"}
        ], "extra": true}"#;
        let r = parse_results(body).unwrap();
        let titles: Vec<&str> = r.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, ["A", "D"]);
        assert_eq!(parse_results(r#"{"results": []}"#).unwrap(), []);
        for bad in ["", "not json", "{}", r#"{"results": {}}"#, "[1, 2]"] {
            assert!(
                matches!(parse_results(bad), Err(LookupError::Malformed { .. })),
                "{bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn lookup_failures_are_errors_with_the_key_scrubbed() {
        // A server that quotes the key back.
        let server = serve_once(
            "401 Unauthorized",
            "application/json",
            r#"{"error": "bad key sk-test-key"}"#.to_owned(),
            1,
        );
        let err = search(&settings(&server.url), "q").await.unwrap_err();
        assert!(
            matches!(&err, LookupError::HttpStatus { status: 401, .. }),
            "{err}"
        );
        assert!(!err.to_string().contains("sk-test-key"), "{err}");

        let server = serve_once("200 OK", "application/json", r#"{"results": []}"#.into(), 1);
        let err = search(&settings(&server.url), "q").await.unwrap_err();
        assert!(matches!(err, LookupError::NoResults), "{err}");

        let server = serve_once("200 OK", "text/html", "<html>".into(), 1);
        let err = search(&settings(&server.url), "q").await.unwrap_err();
        assert!(matches!(err, LookupError::Malformed { .. }), "{err}");

        // Port 9 on loopback refuses at once.
        let err = search(&settings("http://127.0.0.1:9"), "q")
            .await
            .unwrap_err();
        assert!(matches!(err, LookupError::Unreachable { .. }), "{err}");
    }

    #[tokio::test]
    async fn lookup_stops_at_its_deadline_and_on_cancel() {
        // Accepts and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hold = std::thread::spawn(move || {
            let held: Vec<_> = listener.incoming().take(2).collect();
            std::thread::sleep(Duration::from_millis(600));
            drop(held);
        });
        let s = OllamaSearchSettings {
            timeout: Duration::from_millis(150),
            ..settings(&url)
        };
        let started = std::time::Instant::now();
        let err = search(&s, "q").await.unwrap_err();
        assert!(matches!(err, LookupError::Timeout(_)), "{err}");
        assert!(started.elapsed() < Duration::from_millis(500));

        let s = OllamaSearchSettings {
            timeout: Duration::from_secs(5),
            ..s
        };
        let search = OllamaSearch::new(&s).unwrap();
        let cancel = CancellationToken::new();
        let later = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            later.cancel();
        });
        let err = search.search("q", cancel).await.unwrap_err();
        assert!(matches!(err, LookupError::Cancelled), "{err}");
        let _ = hold.join();
    }

    #[test]
    fn lookup_domains_drop_www_and_refuse_what_is_not_a_web_url() {
        assert_eq!(
            domain_of("https://www.Go.dev/doc/").as_deref(),
            Some("go.dev")
        );
        assert_eq!(
            domain_of("http://pkg.go.dev:8080/x?y").as_deref(),
            Some("pkg.go.dev")
        );
        for bad in [
            "",
            "go.dev",
            "ftp://go.dev/x",
            "javascript:alert(1)",
            "https://",
        ] {
            assert_eq!(domain_of(bad), None, "{bad:?}");
        }
        let settings = OllamaSearchSettings {
            api_key: Some("sk-secret".into()),
            ..OllamaSearchSettings::default()
        };
        assert!(!format!("{settings:?}").contains("sk-secret"));
        assert_eq!(
            OllamaSearch::new(&OllamaSearchSettings {
                max_results: 50,
                ..settings
            })
            .unwrap()
            .body("q")["max_results"],
            10,
            "clamped to what the API allows"
        );
    }
}
