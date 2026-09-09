//! HTTP client for the Asana REST API.
//!
//! Every response is `{ "data": ... }`; errors are
//! `{ "errors": [ { "message": "...", "help": "..." } ] }`. Lists page with
//! `limit` (max 100) and `offset` from `next_page.offset`.

use std::time::Duration;

use reqwest::{header, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

pub const BASE_URL: &str = "https://app.asana.com/api/1.0";
pub const USER_AGENT: &str = concat!("asana-cli/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(30);
const PAGE_SIZE: usize = 100;

/// Everything that can go wrong talking to Asana. `main` maps these to
/// gh-style messages and exit codes (401 exits 4, everything else 1).
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// 401: the token is missing, expired or wrong.
    #[error("HTTP 401: {message} ({url})")]
    Unauthorized { message: String, url: String },
    /// 403: the token works but may not touch this object.
    #[error("HTTP 403: {message} ({url})")]
    Forbidden { message: String, url: String },
    /// 404: Asana's own message, e.g. `task: Unknown object: 123`, in the
    /// same `HTTP <status>: <message> (<url>)` shape gh gives REST errors.
    #[error("HTTP 404: {message} ({url})")]
    NotFound { message: String, url: String },
    /// 400, 402, 5xx and anything else: Asana's message with the status.
    #[error("HTTP {status}: {message} ({url})")]
    Status {
        status: u16,
        message: String,
        url: String,
    },
    /// 429 twice in a row (the first one was retried after `Retry-After`).
    #[error("HTTP 429: Rate Limit Enforced; try again in {retry_after}s")]
    RateLimited { retry_after: u64 },
    /// Could not reach app.asana.com at all (DNS, connect, TLS, timeout, or
    /// the connection dropped mid-response). `main` prints the host and the
    /// innermost reason.
    #[error("error connecting to app.asana.com")]
    Network(#[from] reqwest::Error),
    /// The body was not the JSON we expected.
    #[error("could not parse the response from {url}: {source}")]
    Decode {
        url: String,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: T,
    next_page: Option<NextPage>,
}

#[derive(Debug, Deserialize)]
struct NextPage {
    offset: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    errors: Vec<ErrorItem>,
}

#[derive(Debug, Deserialize)]
struct ErrorItem {
    message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    pub fn new(token: impl Into<String>) -> Result<Self, ApiError> {
        Self::with_base(BASE_URL, token)
    }

    pub fn with_base(base: &str, token: impl Into<String>) -> Result<Self, ApiError> {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/json"),
        );
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .timeout(TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.into(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    /// `GET path?query` returning the `data` member decoded as `T`.
    pub async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, ApiError> {
        let value = self.request(Method::GET, path, query, None).await?;
        decode(value, &self.url(path))
    }

    /// `GET` a paginated collection, following `next_page.offset` until
    /// `limit` items have been collected (or the collection ends).
    pub async fn get_all<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
        limit: Option<usize>,
    ) -> Result<Vec<T>, ApiError> {
        let mut items: Vec<T> = Vec::new();
        let mut offset: Option<String> = None;
        loop {
            let remaining = limit.map(|l| l.saturating_sub(items.len()));
            if remaining == Some(0) {
                break;
            }
            let page = remaining
                .map_or(PAGE_SIZE, |r| r.clamp(1, PAGE_SIZE))
                .to_string();
            let mut q: Vec<(&str, &str)> = query.to_vec();
            q.push(("limit", page.as_str()));
            if let Some(off) = offset.as_deref() {
                q.push(("offset", off));
            }
            let raw = self.request(Method::GET, path, &q, None).await?;
            let url = self.url(path);
            let envelope: Envelope<Vec<T>> =
                serde_json::from_value(raw).map_err(|source| ApiError::Decode {
                    url: url.clone(),
                    source,
                })?;
            items.extend(envelope.data);
            match envelope.next_page.and_then(|n| n.offset) {
                Some(next) => offset = Some(next),
                None => break,
            }
        }
        if let Some(l) = limit {
            items.truncate(l);
        }
        Ok(items)
    }

    pub async fn post<T: DeserializeOwned>(&self, path: &str, data: Value) -> Result<T, ApiError> {
        let value = self.request(Method::POST, path, &[], Some(data)).await?;
        decode(value, &self.url(path))
    }

    pub async fn put<T: DeserializeOwned>(&self, path: &str, data: Value) -> Result<T, ApiError> {
        let value = self.request(Method::PUT, path, &[], Some(data)).await?;
        decode(value, &self.url(path))
    }

    pub async fn delete(&self, path: &str) -> Result<(), ApiError> {
        self.request(Method::DELETE, path, &[], None)
            .await
            .map(|_| ())
    }

    /// One request with bearer auth, one retry on 429 honouring `Retry-After`.
    /// Returns the whole JSON body (`data` still wrapped) so callers can page.
    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        data: Option<Value>,
    ) -> Result<Value, ApiError> {
        let url = self.url(path);
        let mut retried = false;
        loop {
            let mut req = self
                .http
                .request(method.clone(), &url)
                .bearer_auth(&self.token)
                .query(query);
            if let Some(body) = &data {
                req = req.json(&serde_json::json!({ "data": body }));
            }
            let resp = req.send().await?;
            let status = resp.status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                let retry_after = resp
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(1);
                if !retried {
                    retried = true;
                    tokio::time::sleep(Duration::from_secs(retry_after.min(60))).await;
                    continue;
                }
                return Err(ApiError::RateLimited { retry_after });
            }
            let display_url = shown_url(&url, query);
            let text = resp.text().await?;
            if status.is_success() {
                if text.trim().is_empty() {
                    return Ok(Value::Null);
                }
                return serde_json::from_str(&text).map_err(|source| ApiError::Decode {
                    url: display_url,
                    source,
                });
            }
            let message = error_message(&text, status);
            return Err(match status {
                StatusCode::UNAUTHORIZED => ApiError::Unauthorized {
                    message,
                    url: display_url,
                },
                StatusCode::FORBIDDEN => ApiError::Forbidden {
                    message,
                    url: display_url,
                },
                StatusCode::NOT_FOUND => ApiError::NotFound {
                    message,
                    url: display_url,
                },
                other => ApiError::Status {
                    status: other.as_u16(),
                    message,
                    url: display_url,
                },
            });
        }
    }
}

fn decode<T: DeserializeOwned>(value: Value, url: &str) -> Result<T, ApiError> {
    let envelope: Envelope<T> =
        serde_json::from_value(value).map_err(|source| ApiError::Decode {
            url: url.to_string(),
            source,
        })?;
    Ok(envelope.data)
}

/// The URL as shown in error messages: path plus the identifying query
/// parameters, without the long `opt_fields` list.
fn shown_url(url: &str, query: &[(&str, &str)]) -> String {
    let shown: Vec<String> = query
        .iter()
        .filter(|(k, _)| !matches!(*k, "opt_fields" | "limit" | "offset"))
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    if shown.is_empty() {
        url.to_string()
    } else {
        format!("{url}?{}", shown.join("&"))
    }
}

fn error_message(body: &str, status: StatusCode) -> String {
    let parsed: Option<ErrorBody> = serde_json::from_str(body).ok();
    let from_body = parsed
        .and_then(|b| b.errors.into_iter().find_map(|e| e.message))
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    from_body.unwrap_or_else(|| {
        status
            .canonical_reason()
            .unwrap_or("request failed")
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;
    use crate::api::types::{Named, Task};

    #[test]
    fn error_message_prefers_asana_text() {
        let body = r#"{"errors":[{"message":"task: Unknown object: 1","help":"x"}]}"#;
        assert_eq!(
            error_message(body, StatusCode::NOT_FOUND),
            "task: Unknown object: 1"
        );
        assert_eq!(
            error_message("<html>", StatusCode::BAD_GATEWAY),
            "Bad Gateway"
        );
        assert_eq!(
            error_message(r#"{"errors":[{"message":"  "}]}"#, StatusCode::IM_A_TEAPOT),
            "I'm a teapot"
        );
    }

    #[test]
    fn shown_url_hides_opt_fields() {
        let q = [("opt_fields", "name"), ("workspace", "1"), ("limit", "100")];
        assert_eq!(
            shown_url("https://x/api/1.0/tasks", &q),
            "https://x/api/1.0/tasks?workspace=1"
        );
        assert_eq!(
            shown_url("https://x/api/1.0/tasks", &[]),
            "https://x/api/1.0/tasks"
        );
    }

    #[test]
    fn errors_display_like_gh() {
        let e = ApiError::NotFound {
            message: "task: Unknown object: 1".into(),
            url: "https://app.asana.com/api/1.0/tasks/1".into(),
        };
        assert_eq!(
            e.to_string(),
            "HTTP 404: task: Unknown object: 1 (https://app.asana.com/api/1.0/tasks/1)"
        );
        let e = ApiError::Unauthorized {
            message: "Not Authorized".into(),
            url: "https://app.asana.com/api/1.0/users/me".into(),
        };
        assert_eq!(
            e.to_string(),
            "HTTP 401: Not Authorized (https://app.asana.com/api/1.0/users/me)"
        );
        let e = ApiError::Status {
            status: 402,
            message: "Payment Required".into(),
            url: "u".into(),
        };
        assert_eq!(e.to_string(), "HTTP 402: Payment Required (u)");
    }

    /// One canned HTTP response.
    struct Canned {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    }

    fn canned(status: u16, body: &str) -> Canned {
        Canned {
            status,
            headers: Vec::new(),
            body: body.to_string(),
        }
    }

    fn asana_error(status: u16, message: &str) -> Canned {
        canned(
            status,
            &serde_json::json!({ "errors": [{ "message": message, "help": "see the docs" }] })
                .to_string(),
        )
    }

    /// A one-thread HTTP/1.1 server that answers each connection with the
    /// next canned response and records the request lines it saw.
    fn serve(responses: Vec<Canned>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for resp in responses {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = stream.read(&mut chunk).expect("read");
                    if n == 0 {
                        break buf.len();
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let content_length: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while buf.len() < head_end + content_length {
                    let n = stream.read(&mut chunk).expect("read body");
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let request_line = head.lines().next().unwrap_or("").to_string();
                let auth = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                    .map(|l| l.trim().to_string())
                    .unwrap_or_default();
                let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
                seen.push(format!("{request_line} | {auth} | {body}"));
                let mut out = format!(
                    "HTTP/1.1 {} Whatever\r\nContent-Type: application/json; charset=UTF-8\r\nContent-Length: {}\r\nConnection: close\r\n",
                    resp.status,
                    resp.body.len()
                );
                for (k, v) in &resp.headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                out.push_str(&resp.body);
                stream.write_all(out.as_bytes()).expect("write");
                stream.flush().expect("flush");
            }
            seen
        });
        (base, handle)
    }

    #[tokio::test]
    async fn get_unwraps_data_and_sends_bearer_token() {
        let (base, server) = serve(vec![canned(200, r#"{"data":{"gid":"7","name":"Seven"}}"#)]);
        let client = Client::with_base(&base, "tok").unwrap();
        let task: Task = client
            .get("/tasks/7", &[("opt_fields", "name")])
            .await
            .unwrap();
        assert_eq!(task.gid, "7");
        assert_eq!(task.name.as_deref(), Some("Seven"));
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0]
                .starts_with("GET /tasks/7?opt_fields=name HTTP/1.1 | authorization: Bearer tok |"),
            "{}",
            seen[0]
        );
    }

    #[tokio::test]
    async fn statuses_map_to_variants_with_asana_message() {
        let cases: Vec<(u16, &str)> = vec![
            (401, "Not Authorized"),
            (403, "Forbidden"),
            (404, "task: Unknown object: 1"),
            (400, "Not a recognized ID: x"),
            (500, "Server Error"),
        ];
        let (base, server) = serve(cases.iter().map(|(s, m)| asana_error(*s, m)).collect());
        let client = Client::with_base(&base, "tok").unwrap();
        for (status, message) in &cases {
            let err = client
                .get::<Value>("/tasks/1", &[("opt_fields", "name"), ("workspace", "9")])
                .await
                .unwrap_err();
            let url = format!("{base}/tasks/1?workspace=9");
            match (status, &err) {
                (401, ApiError::Unauthorized { message: m, url: u })
                | (403, ApiError::Forbidden { message: m, url: u })
                | (404, ApiError::NotFound { message: m, url: u }) => {
                    assert_eq!(m, message);
                    assert_eq!(u, &url);
                }
                (
                    s,
                    ApiError::Status {
                        status,
                        message: m,
                        url: u,
                    },
                ) => {
                    assert_eq!(status, s);
                    assert_eq!(m, message);
                    assert_eq!(u, &url);
                }
                other => panic!("unexpected mapping: {other:?}"),
            }
        }
        server.join().unwrap();
    }

    #[tokio::test]
    async fn non_json_error_body_falls_back_to_reason_phrase() {
        let (base, server) = serve(vec![canned(502, "<html>bad gateway</html>")]);
        let client = Client::with_base(&base, "tok").unwrap();
        let err = client.get::<Value>("/tasks", &[]).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("HTTP 502: Bad Gateway ({base}/tasks)")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn success_with_bad_json_is_a_decode_error() {
        let (base, server) = serve(vec![canned(200, "{not json")]);
        let client = Client::with_base(&base, "tok").unwrap();
        let err = client.get::<Value>("/tasks", &[]).await.unwrap_err();
        assert!(matches!(err, ApiError::Decode { .. }), "{err:?}");
        assert!(err
            .to_string()
            .starts_with(&format!("could not parse the response from {base}/tasks: ")));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn rate_limit_is_retried_once() {
        let mut first = asana_error(429, "Rate Limit Enforced");
        first.headers.push(("Retry-After", "0".to_string()));
        let (base, server) = serve(vec![first, canned(200, r#"{"data":{"gid":"1"}}"#)]);
        let client = Client::with_base(&base, "tok").unwrap();
        let task: Task = client.get("/tasks/1", &[]).await.unwrap();
        assert_eq!(task.gid, "1");
        assert_eq!(server.join().unwrap().len(), 2);

        let mut a = asana_error(429, "Rate Limit Enforced");
        a.headers.push(("Retry-After", "0".to_string()));
        let mut b = asana_error(429, "Rate Limit Enforced");
        b.headers.push(("Retry-After", "42".to_string()));
        let (base, server) = serve(vec![a, b]);
        let client = Client::with_base(&base, "tok").unwrap();
        let err = client.get::<Value>("/tasks/1", &[]).await.unwrap_err();
        assert!(
            matches!(err, ApiError::RateLimited { retry_after: 42 }),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "HTTP 429: Rate Limit Enforced; try again in 42s"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn get_all_follows_next_page_and_honours_limit() {
        let (base, server) = serve(vec![
            canned(
                200,
                r#"{"data":[{"gid":"1"},{"gid":"2"}],"next_page":{"offset":"tok2","path":"/x","uri":"u"}}"#,
            ),
            canned(200, r#"{"data":[{"gid":"3"}],"next_page":null}"#),
        ]);
        let client = Client::with_base(&base, "tok").unwrap();
        let all: Vec<Named> = client
            .get_all("/tasks", &[("project", "9")], None)
            .await
            .unwrap();
        assert_eq!(
            all.iter().map(|n| n.gid.as_str()).collect::<Vec<_>>(),
            ["1", "2", "3"]
        );
        let seen = server.join().unwrap();
        assert!(
            seen[0].starts_with("GET /tasks?project=9&limit=100 HTTP/1.1"),
            "{}",
            seen[0]
        );
        assert!(
            seen[1].starts_with("GET /tasks?project=9&limit=100&offset=tok2 HTTP/1.1"),
            "{}",
            seen[1]
        );

        // A limit smaller than the first page stops after one request.
        let (base, server) = serve(vec![canned(
            200,
            r#"{"data":[{"gid":"1"},{"gid":"2"}],"next_page":{"offset":"more"}}"#,
        )]);
        let client = Client::with_base(&base, "tok").unwrap();
        let two: Vec<Named> = client.get_all("/tasks", &[], Some(2)).await.unwrap();
        assert_eq!(two.len(), 2);
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0].starts_with("GET /tasks?limit=2 HTTP/1.1"),
            "{}",
            seen[0]
        );
    }

    #[tokio::test]
    async fn post_wraps_body_in_data() {
        let (base, server) = serve(vec![canned(201, r#"{"data":{"gid":"5","name":"New"}}"#)]);
        let client = Client::with_base(&base, "tok").unwrap();
        let task: Task = client
            .post("/tasks", serde_json::json!({ "name": "New" }))
            .await
            .unwrap();
        assert_eq!(task.gid, "5");
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /tasks HTTP/1.1"), "{}", seen[0]);
        assert!(
            seen[0].ends_with(r#"| {"data":{"name":"New"}}"#),
            "{}",
            seen[0]
        );
    }

    #[tokio::test]
    async fn connection_refused_is_a_network_error() {
        // Bind and drop so the port is (almost certainly) closed.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let client = Client::with_base(&format!("http://127.0.0.1:{port}"), "tok").unwrap();
        let err = client.get::<Value>("/tasks", &[]).await.unwrap_err();
        assert!(matches!(err, ApiError::Network(_)), "{err:?}");
    }
}
