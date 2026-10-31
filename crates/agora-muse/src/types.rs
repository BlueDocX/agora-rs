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
    /// Images and PDFs sent with the text, inline (user messages only: the model
    /// reads attachments on no other role).
    #[serde(default, skip)]
    pub attachments: Vec<Attachment>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: Role::System, content: content.into(), attachments: Vec::new() }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: content.into(), attachments: Vec::new() }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: content.into(), attachments: Vec::new() }
    }

    /// This user message with `attachments` after its text.
    #[must_use]
    pub fn with(mut self, attachments: Vec<Attachment>) -> Self {
        self.attachments = attachments;
        self
    }

    /// The Responses API shape: the plain `{role, content}` when there is nothing
    /// attached (so cached prefixes stay the same), else typed content blocks.
    fn to_input(&self) -> Value {
        if self.attachments.is_empty() {
            return json!({ "role": self.role, "content": self.content });
        }
        let mut parts = vec![json!({ "type": "input_text", "text": self.content })];
        parts.extend(self.attachments.iter().map(|a| {
            if a.is_image() {
                json!({ "type": "input_image", "image_url": a.data_url() })
            } else {
                json!({ "type": "input_file", "filename": a.name, "file_data": a.data_url() })
            }
        }));
        json!({ "type": "message", "role": self.role, "content": parts })
    }

    /// The Chat Completions shape (Baseten): an image is an `image_url` object, a PDF a
    /// `file` block.
    fn to_chat(&self) -> Value {
        if self.attachments.is_empty() {
            return json!({ "role": self.role, "content": self.content });
        }
        let mut parts = vec![json!({ "type": "text", "text": self.content })];
        parts.extend(self.attachments.iter().map(|a| {
            if a.is_image() {
                json!({ "type": "image_url", "image_url": { "url": a.data_url() } })
            } else {
                json!({ "type": "file", "file": { "filename": a.name, "file_data": a.data_url() } })
            }
        }));
        json!({ "role": self.role, "content": parts })
    }
}

/// A file the person attached: an image (PNG, JPEG, GIF, WebP) or a PDF, sent inline.
/// The caller checks the type from the bytes (see [`Attachment::sniff`]). It holds the
/// file as a ready `data:` URL, shared: a message is rebuilt every turn, and copying or
/// re-encoding megabytes each time would be waste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    url: std::sync::Arc<str>,
}

impl Attachment {
    #[must_use]
    pub fn new(name: impl Into<String>, mime: impl Into<String>, data: &[u8]) -> Self {
        use base64::Engine as _;
        let mime = mime.into();
        let url = format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(data));
        Self { name: name.into(), mime, url: url.into() }
    }

    /// The type the bytes themselves show, among the ones the models read
    /// (dev.meta.ai/docs/file-handling), or `None`. The file's name and the browser's
    /// claimed type are never trusted for this.
    #[must_use]
    pub fn sniff(data: &[u8]) -> Option<&'static str> {
        match data {
            [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some("image/png"),
            [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
            [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some("image/gif"),
            [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
            [b'%', b'P', b'D', b'F', b'-', ..] => Some("application/pdf"),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }

    fn data_url(&self) -> &str {
        &self.url
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
        let input: Vec<Value> = self.input.iter().map(Message::to_input).collect();
        let mut body = json!({ "model": self.model, "input": input });
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

    /// The same request as an OpenAI-style Chat Completions body, for hosts that
    /// only speak that (Baseten serves Muse Spark Open this way).
    ///
    /// Adapted for open-weight chat templates: consecutive system messages are
    /// merged into one (many templates accept a single leading system turn), `max`
    /// effort is sent as `high` (the highest value these hosts accept), and the
    /// schema is marked `strict` so decoding is constrained. Prefix caching is
    /// automatic there, so `prompt_cache_key` is not sent.
    #[must_use]
    pub fn to_chat_body(&self) -> Value {
        let mut messages: Vec<Message> = Vec::with_capacity(self.input.len());
        for m in &self.input {
            match messages.last_mut() {
                Some(last) if last.role == Role::System && m.role == Role::System => {
                    last.content.push_str("\n\n");
                    last.content.push_str(&m.content);
                }
                _ => messages.push(m.clone()),
            }
        }
        let messages: Vec<Value> = messages.iter().map(Message::to_chat).collect();
        let mut body = json!({ "model": self.model, "messages": messages });
        if let Some(effort) = self.effort {
            body["reasoning_effort"] = json!(if effort == Effort::Max { Effort::High } else { effort });
        }
        if let Some((name, schema)) = &self.json_schema {
            body["response_format"] =
                json!({ "type": "json_schema", "json_schema": { "name": name, "schema": schema, "strict": true } });
        }
        if let Some(n) = self.max_output_tokens {
            body["max_tokens"] = json!(n);
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

/// A Chat Completions reply, reduced to what [`Response`] holds.
#[derive(Deserialize)]
pub(crate) struct RawChatResponse {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub choices: Vec<RawChoice>,
    #[serde(default)]
    pub usage: RawChatUsage,
}

#[derive(Deserialize)]
pub(crate) struct RawChoice {
    pub message: RawChatMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct RawChatMessage {
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct RawChatUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<Value>,
    #[serde(default)]
    completion_tokens_details: Option<Value>,
}

impl RawChatResponse {
    /// `finish_reason` becomes a Responses-style status: `stop` is `completed`,
    /// `length` is `incomplete`, anything else (including `tool_calls`, which Agora
    /// never asks for and which carries no text) is passed through, so the engine
    /// refuses it as unfinished.
    pub(crate) fn into_response(self, rate_limit: RateLimit) -> Response {
        let choice = self.choices.into_iter().next();
        let status = match choice.as_ref().and_then(|c| c.finish_reason.as_deref()) {
            Some("stop") => "completed".to_string(),
            Some("length") => "incomplete".to_string(),
            Some(other) => other.to_string(),
            None => "unknown".to_string(),
        };
        let detail = |v: &Option<Value>, key: &str| v.as_ref().and_then(|d| d.get(key)).and_then(Value::as_u64).unwrap_or(0);
        let usage = Usage {
            input_tokens: self.usage.prompt_tokens,
            output_tokens: self.usage.completion_tokens,
            cached_tokens: detail(&self.usage.prompt_tokens_details, "cached_tokens"),
            reasoning_tokens: detail(&self.usage.completion_tokens_details, "reasoning_tokens"),
        };
        let text = choice.and_then(|c| c.message.content).unwrap_or_default();
        Response { id: self.id, model: self.model, status, text, usage, rate_limit }
    }
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
    fn attachments_go_as_typed_blocks_and_plain_messages_stay_plain() {
        let png = Attachment::new("a.png", "image/png", &[1, 2, 3]);
        let pdf = Attachment::new("bill.pdf", "application/pdf", b"%PDF-1.4");
        let r = Request::new("muse-spark-1.3", vec![Message::system("s"), Message::user("Look").with(vec![png, pdf])]);
        let body = r.to_body();
        assert_eq!(body["input"][0], json!({ "role": "system", "content": "s" }));
        assert_eq!(body["input"][1]["content"][0], json!({ "type": "input_text", "text": "Look" }));
        assert_eq!(body["input"][1]["content"][1], json!({ "type": "input_image", "image_url": "data:image/png;base64,AQID" }));
        assert_eq!(body["input"][1]["content"][2]["type"], "input_file");
        assert_eq!(body["input"][1]["content"][2]["filename"], "bill.pdf");
        let chat = r.to_chat_body();
        assert_eq!(chat["messages"][1]["content"][1], json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,AQID" } }));
        assert_eq!(chat["messages"][1]["content"][2]["type"], "file");
    }

    #[test]
    fn a_files_type_comes_from_its_bytes() {
        assert_eq!(Attachment::sniff(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(Attachment::sniff(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(Attachment::sniff(b"RIFF\0\0\0\0WEBPVP8"), Some("image/webp"));
        // A script renamed photo.png is still not an image.
        assert_eq!(Attachment::sniff(b"#!/bin/sh\nrm -rf /"), None);
    }

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
    fn chat_body_carries_the_same_options() {
        let body = Request::new(
            "meta-models/muse-spark-open",
            vec![Message::system("Charter."), Message::system("Role."), Message::user("Check this")],
        )
        .effort(Effort::Max)
            .json_schema("Verdict", json!({"type": "object"}))
            .cache_key("ignored-here")
            .max_output_tokens(500)
            .to_chat_body();
        assert_eq!(
            body,
            json!({
                "model": "meta-models/muse-spark-open",
                "messages": [{"role": "system", "content": "Charter.\n\nRole."}, {"role": "user", "content": "Check this"}],
                "reasoning_effort": "high",
                "response_format": {"type": "json_schema", "json_schema": {"name": "Verdict", "schema": {"type": "object"}, "strict": true}},
                "max_tokens": 500
            })
        );
    }

    #[test]
    fn parses_a_chat_completion_and_maps_finish_reasons() {
        let reply = |finish: &str| {
            serde_json::from_value::<RawChatResponse>(json!({
                "id": "chatcmpl-1", "model": "meta-models/muse-spark-open",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": finish}],
                "usage": {"prompt_tokens": 40, "completion_tokens": 9, "prompt_tokens_details": {"cached_tokens": 32},
                          "completion_tokens_details": {"reasoning_tokens": 5}}
            }))
            .unwrap()
            .into_response(RateLimit::default())
        };
        let done = reply("stop");
        assert_eq!((done.status.as_str(), done.text.as_str()), ("completed", "ok"));
        assert_eq!(done.usage, Usage { input_tokens: 40, output_tokens: 9, cached_tokens: 32, reasoning_tokens: 5 });
        assert_eq!(reply("length").status, "incomplete");
        assert_eq!(reply("content_filter").status, "content_filter");
        assert_eq!(reply("tool_calls").status, "tool_calls");
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
