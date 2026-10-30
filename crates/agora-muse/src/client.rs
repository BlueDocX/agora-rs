//! HTTP client with retries.

use std::fmt;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde::de::DeserializeOwned;

use crate::types::{RawChatResponse, RawResponse};
use crate::{RateLimit, Request, Response};

pub const DEFAULT_BASE_URL: &str = "https://api.meta.ai/v1";
/// Meta's speech-to-text model (`POST /v1/asr/transcribe`; $0.18 per hour of audio).
pub const TRANSCRIBE_MODEL: &str = "muse-voice-transcribe-1.0";
/// Baseten's OpenAI-compatible Model APIs, which serve Muse Spark Open.
pub const BASETEN_BASE_URL: &str = "https://inference.baseten.co/v1";

/// Which wire format the host speaks. Meta's Model API speaks both; partner hosts of
/// the open-weight models speak Chat Completions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    Responses,
    ChatCompletions,
}
const MAX_ATTEMPTS: u32 = 4;

#[derive(Debug)]
pub enum MuseError {
    /// The named environment variable is unset or empty.
    MissingKey(&'static str),
    /// The API answered with an error status. `message` is the API's own error text.
    Api { status: u16, message: String },
    Transport(String),
    Decode(String),
}

impl fmt::Display for MuseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingKey(var) => write!(f, "{var} is not set"),
            Self::Api { status, message } => write!(f, "the model host returned {status}: {message}"),
            Self::Transport(e) => write!(f, "could not reach the model host: {e}"),
            Self::Decode(e) => write!(f, "unexpected response from the model host: {e}"),
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
    api: Api,
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
            api: Api::Responses,
            first_backoff: Duration::from_millis(500),
        }
    }

    /// Reads `MODEL_API_KEY`, and `MUSE_BASE_URL` if set.
    ///
    /// # Errors
    /// [`MuseError::MissingKey`] if `MODEL_API_KEY` is unset or empty.
    pub fn from_env() -> Result<Self, MuseError> {
        let base = std::env::var("MUSE_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Ok(Self::new(base, env_key("MODEL_API_KEY")?))
    }

    /// Baseten (Chat Completions) with `BASETEN_API_KEY`, for Muse Spark Open.
    ///
    /// # Errors
    /// [`MuseError::MissingKey`] if `BASETEN_API_KEY` is unset or empty.
    pub fn baseten_from_env() -> Result<Self, MuseError> {
        Ok(Self::new(BASETEN_BASE_URL, env_key("BASETEN_API_KEY")?).with_api(Api::ChatCompletions))
    }

    #[must_use]
    pub fn with_api(mut self, api: Api) -> Self {
        self.api = api;
        self
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
        let (url, body) = match self.api {
            Api::Responses => (format!("{}/responses", self.base_url), request.to_body()),
            Api::ChatCompletions => (format!("{}/chat/completions", self.base_url), request.to_chat_body()),
        };
        let mut attempt = 0;
        loop {
            attempt += 1;
            let sent = self.http.post(&url).bearer_auth(&self.key.0).json(&body).send().await;
            let (error, retry_after) = match sent {
                Ok(res) if res.status().is_success() => {
                    let rate_limit = rate_limit(res.headers());
                    let decode = |e: reqwest::Error| MuseError::Decode(e.to_string());
                    let mut response = match self.api {
                        Api::Responses => res.json::<RawResponse>().await.map_err(decode)?.into_response(rate_limit),
                        Api::ChatCompletions => res.json::<RawChatResponse>().await.map_err(decode)?.into_response(rate_limit),
                    };
                    if request.json_schema.is_some() {
                        response.text = json_part(&response.text).to_string();
                    }
                    return Ok(response);
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

    /// Muse Voice Transcribe: a WAV recording (mono, 16-bit PCM, 16 or 24 kHz, at
    /// most 10 minutes) to text, one push-to-talk turn. `keywords` bias recognition
    /// toward names it might mishear. `POST {base}/asr/transcribe`, multipart.
    ///
    /// # Errors
    /// The host's error (e.g. 400 for audio it can't read), or a transport failure.
    pub async fn transcribe(&self, wav: Vec<u8>, keywords: &[String]) -> Result<String, MuseError> {
        use reqwest::multipart::{Form, Part};
        let mut settings = serde_json::json!({ "mode": "PUSH_TO_TALK", "model": TRANSCRIBE_MODEL, "audioEncoding": "WAV" });
        if !keywords.is_empty() {
            settings["keywords"] = serde_json::json!(keywords);
        }
        let part = |p: Part, mime: &str| p.mime_str(mime).map_err(|e| MuseError::Transport(e.to_string()));
        let form = Form::new()
            .part("request", part(Part::text(settings.to_string()), "application/json")?)
            .part("audio", part(Part::bytes(wav).file_name("speech.wav"), "audio/wav")?);
        let res = self
            .http
            .post(format!("{}/asr/transcribe", self.base_url))
            .bearer_auth(&self.key.0)
            .multipart(form)
            .send()
            .await
            .map_err(|e| MuseError::Transport(e.without_url().to_string()))?;
        let status = res.status();
        let text = res.text().await.map_err(|e| MuseError::Decode(e.to_string()))?;
        if !status.is_success() {
            return Err(MuseError::Api { status: status.as_u16(), message: api_message(&text) });
        }
        let body: serde_json::Value = serde_json::from_str(&text).map_err(|e| MuseError::Decode(e.to_string()))?;
        body["transcript"].as_str().map(|t| t.trim().to_string()).ok_or_else(|| MuseError::Decode("no transcript in the reply".into()))
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

/// The JSON in a structured reply. Muse sometimes writes a one-line preamble
/// ("I'll look up Priya first.") before the schema-shaped JSON, in the same reply
/// (seen live 2026-10-30), and a model can add a line after it. So a reply that
/// isn't JSON as a whole is cut to the first complete JSON value that starts at a
/// `{` or `[` (trying at most [`MAX_JSON_STARTS`] starts). Anything else is returned
/// unchanged, so the caller's own parse error still says what came back.
#[must_use]
pub fn json_part(text: &str) -> &str {
    let trimmed = text.trim();
    if serde_json::from_str::<serde::de::IgnoredAny>(trimmed).is_ok() {
        return trimmed;
    }
    for (start, _) in trimmed.char_indices().filter(|&(_, c)| c == '{' || c == '[').take(MAX_JSON_STARTS) {
        let rest = &trimmed[start..];
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<serde::de::IgnoredAny>();
        if let Some(Ok(_)) = values.next() {
            return &rest[..values.byte_offset()];
        }
    }
    text
}

/// How many `{`/`[` positions [`json_part`] tries before giving up.
const MAX_JSON_STARTS: usize = 16;

fn env_key(var: &'static str) -> Result<String, MuseError> {
    std::env::var(var).ok().filter(|k| !k.is_empty()).ok_or(MuseError::MissingKey(var))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preamble_before_structured_json_is_cut_off() {
        let raw = "Scouting your lunch invite \u{2014} I'll check {the} contacts.{\"tool\": \"contacts.search\", \"target\": \"Priya\"}";
        assert_eq!(json_part(raw), "{\"tool\": \"contacts.search\", \"target\": \"Priya\"}");
        assert_eq!(json_part(" [1, 2] "), "[1, 2]");
        assert_eq!(json_part("no json here"), "no json here");
        assert_eq!(json_part("{\"tool\": \"finish\"} Let me know if you need more."), "{\"tool\": \"finish\"}");
    }
}
