//! OpenAI-compatible streaming client (`POST /chat/completions`, SSE).
//!
//! Codex's real client speaks the Responses API and does a lot more (auth,
//! retries, proxies, rate limits, telemetry). For a minimal chat agent the
//! useful extraction is the transport shape: build a JSON request, stream the
//! response, decode `data:` frames, and yield text deltas.
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
use crate::core::protocol::Message;

#[derive(Debug, Clone)]
pub struct OpenAiCompatClient {
    http: Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl OpenAiCompatClient {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, ModelError> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(ModelError::network)?;

        Ok(Self {
            http,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
        })
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
}

impl ModelClient for OpenAiCompatClient {
    fn label(&self) -> String {
        self.model.clone()
    }

    fn stream_chat(&self, messages: Vec<Message>, cancel: CancellationToken) -> ModelStream {
        if self.api_key.trim().is_empty() {
            return Box::pin(futures_util::stream::once(async {
                Err(ModelError::NotConfigured(
                    "API key is empty; set CST_API_KEY or use --mock".to_string(),
                ))
            }));
        }

        let url = self.endpoint();
        let api_key = self.api_key.clone();
        let model = self.model.clone();
        let body = json!({
            "model": model,
            "stream": true,
            "messages": messages.iter().map(|message| {
                json!({
                    "role": message.role.as_str(),
                    "content": message.content,
                })
            }).collect::<Vec<_>>(),
        });
        let http = self.http.clone();

        Box::pin(async_stream::try_stream! {
            let response = {
                let send_result = tokio::select! {
                    _ = cancel.cancelled() => return,
                    result = http.post(&url).bearer_auth(&api_key).json(&body).send() => result,
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
                            for data in decoder.feed(&chunk) {
                                match parse_sse_data(&data)? {
                                    SseEvent::Delta(delta) => yield delta,
                                    SseEvent::Done => return,
                                    SseEvent::Ignore => {}
                                }
                            }
                        }
                    }
                }

                // A well-behaved server ends with a newline, but tolerate a
                // final frame that doesn't.
                if let Some(data) = decoder.finish() {
                    match parse_sse_data(&data)? {
                        SseEvent::Delta(delta) => yield delta,
                        SseEvent::Done => {}
                        SseEvent::Ignore => {}
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

#[derive(Debug, Default)]
struct SseDecoder {
    /// Raw bytes, not a `String`: a network chunk may end in the middle of a
    /// multi-byte UTF-8 character.
    buffer: Vec<u8>,
}

impl SseDecoder {
    /// Append a chunk and return all complete `data:` payloads. Chunks may
    /// split frames (and UTF-8 characters) at arbitrary byte boundaries.
    fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();

        while let Some(index) = self.buffer.iter().position(|&byte| byte == b'\n') {
            let line_bytes: Vec<u8> = self.buffer.drain(..=index).collect();
            let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]);
            let line = line.trim_end_matches('\r');
            if let Some(data) = line.strip_prefix("data:") {
                out.push(data.trim_start().to_string());
            }
        }

        out
    }

    fn finish(&mut self) -> Option<String> {
        if self.buffer.iter().all(u8::is_ascii_whitespace) {
            self.buffer.clear();
            return None;
        }
        let bytes = std::mem::take(&mut self.buffer);
        let line = String::from_utf8_lossy(&bytes);
        line.trim_end_matches('\r')
            .strip_prefix("data:")
            .map(|data| data.trim_start().to_string())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SseEvent {
    Delta(String),
    Done,
    Ignore,
}

fn parse_sse_data(data: &str) -> Result<SseEvent, ModelError> {
    let data = data.trim();
    if data.is_empty() {
        return Ok(SseEvent::Ignore);
    }
    if data == "[DONE]" {
        return Ok(SseEvent::Done);
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

    let delta = value
        .pointer("/choices/0/delta/content")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if delta.is_empty() {
        Ok(SseEvent::Ignore)
    } else {
        Ok(SseEvent::Delta(delta.to_string()))
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
    fn decoder_handles_split_frames() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.feed(b"data: {\"choices\":[{\"delta\"").is_empty());
        assert!(!decoder.feed(b":{\"content\":\"he\"}}]}\n").is_empty());

        let frames = decoder.feed(b"data: [DONE]\n");
        assert_eq!(frames, vec!["[DONE]".to_string()]);
    }

    #[test]
    fn decoder_survives_utf8_split_across_chunks() {
        let mut decoder = SseDecoder::default();
        let frame = "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n".as_bytes();
        let split = frame.iter().position(|&byte| byte >= 0x80).expect("CJK") + 1;

        // The first chunk ends inside the three-byte character; no frame can
        // be emitted yet and no replacement characters are introduced.
        assert!(decoder.feed(&frame[..split]).is_empty());
        let frames = decoder.feed(&frame[split..]);
        assert_eq!(frames.len(), 1);
        assert_eq!(
            parse_sse_data(&frames[0]).unwrap(),
            SseEvent::Delta("你".to_string())
        );
    }

    #[test]
    fn decoder_ignores_non_data_lines_and_crlf() {
        let mut decoder = SseDecoder::default();
        let frames = decoder.feed(
            b"event: message\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\r\n\r\n",
        );
        assert_eq!(frames.len(), 1);
        assert_eq!(
            parse_sse_data(&frames[0]).unwrap(),
            SseEvent::Delta("x".to_string())
        );
    }

    #[test]
    fn parse_extracts_content_and_errors() {
        let frame = r#"{"choices":[{"delta":{"content":"hello"}}]}"#;
        assert_eq!(
            parse_sse_data(frame).unwrap(),
            SseEvent::Delta("hello".to_string())
        );

        assert_eq!(parse_sse_data("").unwrap(), SseEvent::Ignore);
        assert_eq!(parse_sse_data("[DONE]").unwrap(), SseEvent::Done);

        let error = parse_sse_data(r#"{"error":{"message":"bad key"}}"#).unwrap_err();
        assert!(error.to_string().contains("bad key"));
    }
}
