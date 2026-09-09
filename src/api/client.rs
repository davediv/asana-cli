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
    /// 404: Asana's own message, e.g. `task: Unknown object: 123`.
    #[error("{message}")]
    NotFound { message: String, url: String },
    /// 400, 402, 5xx and anything else: Asana's message with the status.
    #[error("HTTP {status}: {message} ({url})")]
    Status {
        status: u16,
        message: String,
        url: String,
    },
    /// 429 twice in a row.
    #[error("HTTP 429: rate limited by Asana; try again in {retry_after}s")]
    RateLimited { retry_after: u64 },
    /// Could not reach app.asana.com at all.
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
    use super::*;

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
}
