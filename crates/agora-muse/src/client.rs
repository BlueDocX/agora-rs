//! HTTP client with retries.

use std::fmt;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde::de::DeserializeOwned;

use crate::types::RawResponse;
use crate::{RateLimit, Request, Response};

pub const DEFAULT_BASE_URL: &str = "https://api.meta.ai/v1";
const MAX_ATTEMPTS: u32 = 4;

#[derive(Debug)]
pub enum MuseError {
    MissingKey,
    /// The API answered with an error status. `message` is the API's own error text.
    Api { status: u16, message: String },
    Transport(String),
    Decode(String),
}

impl fmt::Display for MuseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingKey => write!(f, "MODEL_API_KEY is not set"),
            Self::Api { status, message } => write!(f, "Model API returned {status}: {message}"),
            Self::Transport(e) => write!(f, "could not reach the Model API: {e}"),
            Self::Decode(e) => write!(f, "unexpected response from the Model API: {e}"),
        }
    }
}

impl std::error::Error for MuseError {}

/// Holds the API key without ever printing it.
#[derive(Clone)]
struct ApiKey(String);

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

#[derive(Debug, Clone)]
pub struct MuseClient {
    http: reqwest::Client,
    base_url: String,
    key: ApiKey,
    first_backoff: Duration,
}

impl MuseClient {
    /// # Panics
    /// Panics if the TLS backend cannot be initialised.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(600))
            .build()
            .expect("HTTP client");
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            key: ApiKey(api_key.into()),
            first_backoff: Duration::from_millis(500),
        }
    }

    /// Reads `MODEL_API_KEY`, and `MUSE_BASE_URL` if set.
    ///
    /// # Errors
    /// [`MuseError::MissingKey`] if `MODEL_API_KEY` is unset or empty.
    pub fn from_env() -> Result<Self, MuseError> {
        let key = std::env::var("MODEL_API_KEY").ok().filter(|k| !k.is_empty()).ok_or(MuseError::MissingKey)?;
        let base = std::env::var("MUSE_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Ok(Self::new(base, key))
    }

    /// The first retry waits this long; each later one doubles it (plus jitter).
    #[must_use]
    pub fn with_first_backoff(mut self, delay: Duration) -> Self {
        self.first_backoff = delay;
        self
    }

    /// One call to `POST /responses`. Retries rate limits (429), server errors (5xx)
    /// and transport failures up to 4 attempts, waiting at least `Retry-After`.
    ///
    /// # Errors
    /// The last error once retries are exhausted, or the first non-retryable one.
    pub async fn respond(&self, request: &Request) -> Result<Response, MuseError> {
        let url = format!("{}/responses", self.base_url);
        let body = request.to_body();
        let mut attempt = 0;
        loop {
            attempt += 1;
            let sent = self.http.post(&url).bearer_auth(&self.key.0).json(&body).send().await;
            let (error, retry_after) = match sent {
                Ok(res) if res.status().is_success() => {
                    let rate_limit = rate_limit(res.headers());
                    let raw: RawResponse = res.json().await.map_err(|e| MuseError::Decode(e.to_string()))?;
                    return Ok(raw.into_response(rate_limit));
                }
                Ok(res) => {
                    let status = res.status();
                    let retry_after = retry_after(res.headers());
                    let message = api_message(&res.text().await.unwrap_or_default());
                    let error = MuseError::Api { status: status.as_u16(), message };
                    if !(status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()) {
                        return Err(error);
                    }
                    (error, retry_after)
                }
                Err(e) => (MuseError::Transport(e.without_url().to_string()), None),
            };
            if attempt >= MAX_ATTEMPTS {
                return Err(error);
            }
            tokio::time::sleep(self.backoff(attempt).max(retry_after.unwrap_or_default())).await;
        }
    }

    /// [`Self::respond`], then parse the output text as JSON. Use with
    /// [`Request::json_schema`] so the model is constrained to the schema.
    ///
    /// # Errors
    /// Any [`Self::respond`] error, or [`MuseError::Decode`] if the text is not the
    /// expected JSON.
    pub async fn respond_json<T: DeserializeOwned>(&self, request: &Request) -> Result<(T, Response), MuseError> {
        let response = self.respond(request).await?;
        let value = serde_json::from_str(&response.text).map_err(|e| MuseError::Decode(e.to_string()))?;
        Ok((value, response))
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let base = self.first_backoff * 2u32.pow(attempt - 1);
        // Jitter up to 50%, from the clock: enough to keep parallel workers apart.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos();
        base + base.mul_f64(f64::from(nanos % 1000) / 2000.0)
    }
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn rate_limit(headers: &HeaderMap) -> RateLimit {
    RateLimit {
        remaining_requests: header_u64(headers, "x-ratelimit-remaining-requests"),
        remaining_tokens: header_u64(headers, "x-ratelimit-remaining-tokens"),
    }
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    header_u64(headers, "retry-after").map(Duration::from_secs)
}

/// The API's `error.message` if the body has one, else the first 300 characters.
fn api_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or_else(|| body.chars().take(300).collect())
}
