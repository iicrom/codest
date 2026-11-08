//! OpenAI-compatible streaming client (`POST /chat/completions`, SSE).
//!
//! Codex's real client speaks the Responses API and does a lot more (auth,
//! retries, proxies, rate limits, telemetry). For a minimal chat agent the
//! useful extraction is the transport shape: build a JSON request, stream the
//! response, decode `data:` frames, and yield text deltas.
//!
//! The client is protocol-`openai-compat`'s implementation behind the
//! provider registry: an instance's base URL, key, static headers, and
//! `body_overrides` are baked in at construction. Besides `delta.content` it
//! forwards `delta.reasoning_content` as [`StreamItem::Thinking`] and honors
//! usage chunks.
//!
//! This module deliberately knows nothing about terminals or conversations.

use std::time::Duration;

use futures_util::StreamExt;
use reqwest::Client;
use serde_json::Value;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::core::model::ModelClient;
use crate::core::model::ModelError;
use crate::core::model::ModelStream;
use crate::core::model::StreamItem;
use crate::core::protocol::Message;
use crate::core::sse::SseDecoder;

#[derive(Debug, Clone)]
pub struct OpenAiCompatClient {
    http: Client,
    base_url: String,
    api_key: String,
    model: String,
    headers: Vec<(String, String)>,
    body_overrides: serde_json::Map<String, Value>,
}

impl OpenAiCompatClient {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, ModelError> {
        Ok(Self {
            http: Self::build_http()?,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            headers: Vec::new(),
            body_overrides: serde_json::Map::new(),
        })
    }

    /// Static headers sent with every request. Callers' values override the
    /// transport defaults (`Authorization` excepted, which stays the key).
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }

    /// JSON fields merged over the request body; overrides win over the
    /// standard fields, which lets an instance fix up provider quirks
    /// (`temperature`, `extra_body`-style nesting, …) without code.
    pub fn with_body_overrides(mut self, overrides: serde_json::Map<String, Value>) -> Self {
        self.body_overrides = overrides;
        self
    }

    fn build_http() -> Result<Client, ModelError> {
        Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(ModelError::network)
    }

    /// Accepts either a base URL (`https://host/v1`) or the full endpoint.
    fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }

    /// The request body: the standard fields with `body_overrides` merged on
    /// top (overrides win).
    fn request_body(&self, messages: &[Message]) -> Value {
        let mut body = json!({
            "model": self.model,
            "stream": true,
            "messages": messages.iter().map(|message| {
                json!({
                    "role": message.role.as_str(),
                    "content": message.content,
                })
            }).collect::<Vec<_>>(),
        });
        let object = body
            .as_object_mut()
            .expect("the standard request body is an object");
        for (key, value) in &self.body_overrides {
            object.insert(key.clone(), value.clone());
        }
        body
    }
}

impl ModelClient for OpenAiCompatClient {
    fn label(&self) -> String {
        self.model.clone()
    }

    fn stream_chat(&self, messages: Vec<Message>, cancel: CancellationToken) -> ModelStream {
        if self.api_key.trim().is_empty() {
            return Box::pin(futures_util::stream::once(async {
                Err(ModelError::NotConfigured(
                    "API key is empty; set `api_key` for the provider in \
                     ~/.codest/providers.toml, pass --api-key, or export \
                     CST_API_KEY / OPENAI_API_KEY"
                        .to_string(),
                ))
            }));
        }

        let url = self.endpoint();
        let api_key = self.api_key.clone();
        let body = self.request_body(&messages);
        let http = self.http.clone();
        let headers = self.headers.clone();

        Box::pin(async_stream::try_stream! {
            let response = {
                let mut request = http.post(&url).bearer_auth(&api_key);
                for (key, value) in &headers {
                    request = request.header(key, value);
                }
                let send_result = tokio::select! {
                    _ = cancel.cancelled() => return,
                    result = request.json(&body).send() => result,
                };
                send_result.map_err(ModelError::network)?
            };

            let status = response.status();
            if status.is_success() {
                let mut bytes = response.bytes_stream();
                let mut decoder = SseDecoder::default();

                loop {
                    let next = tokio::select! {
                        _ = cancel.cancelled() => return,
                        next = bytes.next() => next,
                    };

                    match next {
                        None => break,
                        Some(Err(error)) => Err(ModelError::network(error))?,
                        Some(Ok(chunk)) => {
                            for frame in decoder.feed(&chunk) {
                                match parse_sse_frame(&frame)? {
                                    Parsed::Delta(item) => yield item,
                                    Parsed::Done => return,
                                    Parsed::Ignore => {}
                                }
                            }
                        }
                    }
                }

                // A well-behaved server ends with a blank line, but tolerate a
                // final frame that doesn't.
                if let Some(frame) = decoder.finish() {
                    match parse_sse_frame(&frame)? {
                        Parsed::Delta(item) => yield item,
                        Parsed::Done => {}
                        Parsed::Ignore => {}
                    }
                }
            } else {
                let body = response.text().await.unwrap_or_default();
                Err(ModelError::Http {
                    status: status.as_u16(),
                    body: truncate(&body, 600),
                })?;
            }
        })
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
enum Parsed {
    Delta(StreamItem),
    Done,
    Ignore,
}

fn parse_sse_frame(frame: &crate::core::sse::SseFrame) -> Result<Parsed, ModelError> {
    let data = frame.data.trim();
    if data.is_empty() {
        return Ok(Parsed::Ignore);
    }
    if data == "[DONE]" {
        return Ok(Parsed::Done);
    }

    let value: Value = serde_json::from_str(data).map_err(ModelError::decode)?;

    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| error.to_string());
        return Err(ModelError::decode(message));
    }

    if let Some(usage) = parse_usage(&value) {
        return Ok(Parsed::Delta(usage));
    }

    let delta = &value.pointer("/choices/0/delta");
    let content = delta
        .and_then(|delta| delta.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !content.is_empty() {
        return Ok(Parsed::Delta(StreamItem::Delta(content.to_string())));
    }

    let reasoning = delta
        .and_then(|delta| delta.get("reasoning_content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !reasoning.is_empty() {
        return Ok(Parsed::Delta(StreamItem::Thinking(reasoning.to_string())));
    }

    Ok(Parsed::Ignore)
}

/// A final chunk's usage report. Both OpenAI's
/// `prompt_tokens`/`completion_tokens` and the `input_tokens`/`output_tokens`
/// spelling are accepted; a usage object without either pair is ignored.
fn parse_usage(value: &Value) -> Option<StreamItem> {
    let usage = value.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    let input = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64);
    let output = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64);
    match (input, output) {
        (Some(input), Some(output)) => Some(StreamItem::Usage { input, output }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_accepts_base_or_full_url() {
        let client = OpenAiCompatClient::new("k", "https://example.test/v1/", "m").unwrap();
        assert_eq!(
            client.endpoint(),
            "https://example.test/v1/chat/completions"
        );

        let client =
            OpenAiCompatClient::new("k", "https://example.test/v1/chat/completions", "m").unwrap();
        assert_eq!(
            client.endpoint(),
            "https://example.test/v1/chat/completions"
        );
    }

    #[test]
    fn body_overrides_win_over_standard_fields() {
        let mut overrides = serde_json::Map::new();
        overrides.insert("temperature".to_string(), json!(0.5));
        overrides.insert("model".to_string(), json!("override"));

        let client = OpenAiCompatClient::new("k", "https://example.test/v1", "m")
            .unwrap()
            .with_body_overrides(overrides);
        let body = client.request_body(&[Message::user("hi")]);

        assert_eq!(body["model"], "override");
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[test]
    fn parse_extracts_content_errors_thinking_and_done() {
        let frame = |data: &str| crate::core::sse::SseFrame {
            event: None,
            data: data.to_string(),
        };
        let parsed = parse_sse_frame(&frame(r#"{"choices":[{"delta":{"content":"hello"}}]}"#))
            .unwrap();
        assert_eq!(
            parsed,
            Parsed::Delta(StreamItem::Delta("hello".to_string()))
        );

        assert_eq!(parse_sse_frame(&frame("")).unwrap(), Parsed::Ignore);
        assert_eq!(parse_sse_frame(&frame("[DONE]")).unwrap(), Parsed::Done);

        let error = parse_sse_frame(&frame(r#"{"error":{"message":"bad key"}}"#)).unwrap_err();
        assert!(error.to_string().contains("bad key"));

        let parsed = parse_sse_frame(&frame(
            r#"{"choices":[{"delta":{"reasoning_content":"thinking…"}}]}"#,
        ))
        .unwrap();
        assert_eq!(
            parsed,
            Parsed::Delta(StreamItem::Thinking("thinking…".to_string()))
        );

        // A delta with neither content nor reasoning is ignored.
        let parsed = parse_sse_frame(&frame(r#"{"choices":[{"delta":{}}]}"#)).unwrap();
        assert_eq!(parsed, Parsed::Ignore);
    }

    #[test]
    fn parse_reads_usage_in_both_spellings() {
        let frame = |data: &str| crate::core::sse::SseFrame {
            event: None,
            data: data.to_string(),
        };
        let parsed = parse_sse_frame(&frame(
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4}}"#,
        ))
        .unwrap();
        assert_eq!(parsed, Parsed::Delta(StreamItem::Usage { input: 10, output: 4 }));

        let parsed = parse_sse_frame(&frame(
            r#"{"usage":{"input_tokens":7,"output_tokens":2}}"#,
        ))
        .unwrap();
        assert_eq!(parsed, Parsed::Delta(StreamItem::Usage { input: 7, output: 2 }));

        // No usage pair at all: ignored, not an error.
        let parsed = parse_sse_frame(&frame(r#"{"usage":{}}"#)).unwrap();
        assert_eq!(parsed, Parsed::Ignore);
    }

    #[tokio::test]
    async fn empty_key_yields_not_configured() {
        let client = OpenAiCompatClient::new("  ", "https://example.test/v1", "m").unwrap();
        let mut stream = client.stream_chat(vec![], CancellationToken::new());
        let error = stream.next().await.expect("one item").unwrap_err();
        assert!(matches!(error, ModelError::NotConfigured(_)));
    }
}
