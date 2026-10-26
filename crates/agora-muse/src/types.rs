//! Request and response shapes for `POST /v1/responses`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: Role::System, content: content.into() }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: content.into() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    Max,
}

/// One call to the Responses API.
///
/// Put stable content first (system messages), variable content last: Muse caches
/// the longest matching prefix automatically, and cached input costs ~8x less.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub model: String,
    pub input: Vec<Message>,
    pub effort: Option<Effort>,
    /// Constrain the output to this JSON Schema (decoding is constrained server-side).
    pub json_schema: Option<(String, Value)>,
    /// Groups requests that share a prefix onto the same cache. One key per prompt
    /// shape (e.g. a role), never per user or per call.
    pub prompt_cache_key: Option<String>,
    pub max_output_tokens: Option<u32>,
}

impl Request {
    pub fn new(model: impl Into<String>, input: Vec<Message>) -> Self {
        Self { model: model.into(), input, effort: None, json_schema: None, prompt_cache_key: None, max_output_tokens: None }
    }

    #[must_use]
    pub fn effort(mut self, effort: Effort) -> Self {
        self.effort = Some(effort);
        self
    }

    #[must_use]
    pub fn json_schema(mut self, name: impl Into<String>, schema: Value) -> Self {
        self.json_schema = Some((name.into(), schema));
        self
    }

    #[must_use]
    pub fn cache_key(mut self, key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(key.into());
        self
    }

    #[must_use]
    pub fn max_output_tokens(mut self, n: u32) -> Self {
        self.max_output_tokens = Some(n);
        self
    }

    /// The JSON body sent to the API.
    #[must_use]
    pub fn to_body(&self) -> Value {
        let mut body = json!({ "model": self.model, "input": self.input });
        if let Some(effort) = self.effort {
            body["reasoning"] = json!({ "effort": effort });
        }
        if let Some((name, schema)) = &self.json_schema {
            body["text"] = json!({ "format": { "type": "json_schema", "name": name, "schema": schema } });
        }
        if let Some(key) = &self.prompt_cache_key {
            body["prompt_cache_key"] = json!(key);
        }
        if let Some(n) = self.max_output_tokens {
            body["max_output_tokens"] = json!(n);
        }
        body
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, deserialize_with = "cached_tokens")]
    #[serde(rename = "input_tokens_details")]
    pub cached_tokens: u64,
    #[serde(default, deserialize_with = "reasoning_tokens")]
    #[serde(rename = "output_tokens_details")]
    pub reasoning_tokens: u64,
}

fn cached_tokens<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(v.get("cached_tokens").and_then(Value::as_u64).unwrap_or(0))
}

fn reasoning_tokens<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(v.get("reasoning_tokens").and_then(Value::as_u64).unwrap_or(0))
}

impl Usage {
    /// Share of input tokens served from cache, 0.0-1.0.
    #[must_use]
    pub fn cache_ratio(&self) -> f64 {
        if self.input_tokens == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let ratio = self.cached_tokens as f64 / self.input_tokens as f64;
        ratio
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, other: Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_tokens += other.cached_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
    }
}

/// What the `x-ratelimit-remaining-*` headers said on this response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateLimit {
    pub remaining_requests: Option<u64>,
    pub remaining_tokens: Option<u64>,
}

/// The parts of a Responses API reply that Agora reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub id: String,
    pub model: String,
    pub status: String,
    /// All `output_text` parts of all message items, joined.
    pub text: String,
    pub usage: Usage,
    pub rate_limit: RateLimit,
}

#[derive(Deserialize)]
pub(crate) struct RawResponse {
    pub id: String,
    pub model: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub output: Vec<RawItem>,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub(crate) enum RawItem {
    #[serde(rename = "message")]
    Message {
        #[serde(default)]
        content: Vec<RawContent>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub(crate) enum RawContent {
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(other)]
    Other,
}

impl RawResponse {
    pub(crate) fn into_response(self, rate_limit: RateLimit) -> Response {
        let text = self
            .output
            .iter()
            .filter_map(|item| match item {
                RawItem::Message { content } => Some(content),
                RawItem::Other => None,
            })
            .flatten()
            .filter_map(|part| match part {
                RawContent::OutputText { text } => Some(text.as_str()),
                RawContent::Other => None,
            })
            .collect::<String>();
        Response { id: self.id, model: self.model, status: self.status, text, usage: self.usage, rate_limit }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn body_puts_options_where_the_api_expects_them() {
        let body = Request::new("muse-spark-1.3", vec![Message::system("You plan."), Message::user("Brief")])
            .effort(Effort::Low)
            .json_schema("Plan", json!({"type": "object"}))
            .cache_key("planner")
            .max_output_tokens(2000)
            .to_body();
        assert_eq!(
            body,
            json!({
                "model": "muse-spark-1.3",
                "input": [{"role": "system", "content": "You plan."}, {"role": "user", "content": "Brief"}],
                "reasoning": {"effort": "low"},
                "text": {"format": {"type": "json_schema", "name": "Plan", "schema": {"type": "object"}}},
                "prompt_cache_key": "planner",
                "max_output_tokens": 2000
            })
        );
    }

    #[test]
    fn parses_the_documented_response_shape() {
        // The example from dev.meta.ai/docs/quickstart, plus a cache hit.
        let raw: RawResponse = serde_json::from_value(json!({
            "id": "resp_abc123", "object": "response", "created_at": 1_792_177_394,
            "status": "completed", "model": "muse-spark-1.3", "reasoning": {"effort": "high"},
            "output": [
                {"id": "rs_abc123", "type": "reasoning", "summary": [], "status": "completed"},
                {"id": "msg_abc123", "type": "message", "role": "assistant", "status": "completed",
                 "content": [{"type": "output_text", "text": "The capital of France is **Paris**.", "annotations": []}]}
            ],
            "usage": {"input_tokens": 15, "input_tokens_details": {"cached_tokens": 5},
                      "output_tokens": 258, "output_tokens_details": {"reasoning_tokens": 241}, "total_tokens": 273}
        }))
        .unwrap();
        let r = raw.into_response(RateLimit::default());
        assert_eq!(r.text, "The capital of France is **Paris**.");
        assert_eq!(r.usage, Usage { input_tokens: 15, output_tokens: 258, cached_tokens: 5, reasoning_tokens: 241 });
    }
}
