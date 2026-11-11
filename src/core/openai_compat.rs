//! OpenAI-compatible streaming client (`POST /chat/completions`, SSE).
//!
//! Codex's real client speaks the Responses API and does a lot more (auth,
//! retries, proxies, rate limits, telemetry). For a minimal chat agent the
//! useful extraction is the transport shape: build a JSON request, stream the
//! response, decode `data:` frames, and yield text deltas.
//!
//! The client is protocol-`openai-compat`'s implementation behind the
//! provider registry: an instance's base URL, key, static headers,
//! `body_overrides`, and wire quirks are baked in at construction. Besides
//! `delta.content` it forwards the configured reasoning delta fields
//! (default `delta.reasoning_content`) as [`StreamItem::Thinking`] and honors
//! usage chunks.
//!
//! This module deliberately knows nothing about terminals or conversations.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::Client;
use serde_json::Value;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::core::config::EFFORT_FIELD_NONE;
use crate::core::config::WireQuirks;
use crate::core::model::ModelClient;
use crate::core::model::ModelError;
use crate::core::model::ModelStream;
use crate::core::model::StreamItem;
use crate::core::model::TurnOptions;
use crate::core::protocol::Message;
use crate::core::sse::SseDecoder;
use crate::core::tools::ToolCall;
use crate::core::tools::ToolSpec;

#[derive(Debug, Clone)]
pub struct OpenAiCompatClient {
    http: Client,
    base_url: String,
    api_key: String,
    model: String,
    headers: Vec<(String, String)>,
    body_overrides: serde_json::Map<String, Value>,
    quirks: WireQuirks,
    /// The header session ids are sent under, resolved from the instance's
    /// `wire.session_header` and the model's own override. `None` sends none.
    session_header: Option<String>,
}

/// The process-wide HTTP client. Clients are rebuilt per turn (the session id
/// changes with the active session, and it must not be baked into a cached
/// client), so the connection pool has to outlive them — otherwise every turn
/// would pay for a fresh TCP/TLS handshake.
static HTTP: std::sync::OnceLock<Client> = std::sync::OnceLock::new();

fn shared_http() -> Result<Client, ModelError> {
    if let Some(client) = HTTP.get() {
        return Ok(client.clone());
    }
    let built = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(ModelError::network)?;
    // A racing first caller may have won; either client is equivalent.
    let _ = HTTP.set(built);
    Ok(HTTP.get().expect("HTTP was just set").clone())
}

impl OpenAiCompatClient {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, ModelError> {
        Ok(Self {
            http: shared_http()?,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            headers: Vec::new(),
            body_overrides: serde_json::Map::new(),
            quirks: WireQuirks::default(),
            session_header: None,
        })
    }

    /// The header session ids go under (`session-id` by default), or `None`
    /// to send none. Resolved once from provider configuration: a model's own
    /// setting beats the instance's, and `"none"` at either level suppresses
    /// the header.
    pub fn with_session_header(mut self, header: Option<String>) -> Self {
        self.session_header = header;
        self
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

    /// Wire quirks baked in at construction. The default reproduces the
    /// built-in field spellings; an instance's `[providers.wire]` table
    /// overrides them without code.
    pub fn with_quirks(mut self, quirks: WireQuirks) -> Self {
        self.quirks = quirks;
        self
    }

    /// The wire quirks in effect, as baked in at construction. Test-only:
    /// outside tests the quirks are an implementation detail of
    /// `request_body` and `parse_sse_frame`.
    #[cfg(test)]
    pub(crate) fn quirks(&self) -> &WireQuirks {
        &self.quirks
    }

    /// Accepts either a base URL (`https://host/v1`) or the full endpoint.
    pub(crate) fn endpoint(&self) -> String {
        endpoint_url(&self.base_url)
    }

    /// The per-turn headers: the configured session header, when both a name
    /// and a session id are present. Kept separate from [`Self::headers`] so
    /// tests can assert the wire shape without a server, and so the ordering
    /// below stays explicit.
    pub(crate) fn session_headers<'a>(
        &'a self,
        options: &'a TurnOptions,
    ) -> Vec<(&'a str, &'a str)> {
        match (self.session_header.as_deref(), options.session_id.as_deref()) {
            (Some(header), Some(session_id)) => vec![(header, session_id)],
            _ => Vec::new(),
        }
    }

    /// The request body: the standard fields with `body_overrides` merged on
    /// top (overrides win), per-turn `tools` included when the plugins
    /// declared any, and a per-turn effort merged last under the instance's
    /// `effort_field` so an interactive `/effort` beats a static override for
    /// that one field. `effort_field = "none"` never sends the field.
    fn request_body(&self, messages: &[Message], options: &TurnOptions) -> Value {
        let mut body = json!({
            "model": self.model,
            "stream": true,
            "messages": messages.iter().map(message_json).collect::<Vec<_>>(),
        });
        let object = body
            .as_object_mut()
            .expect("the standard request body is an object");
        if !options.tools.is_empty() {
            object.insert(
                "tools".to_string(),
                Value::Array(
                    options
                        .tools
                        .iter()
                        .map(ToolSpec::function_json)
                        .collect(),
                ),
            );
        }
        for (key, value) in &self.body_overrides {
            object.insert(key.clone(), value.clone());
        }
        if let Some(effort) = &options.effort
            && self.quirks.effort_field != EFFORT_FIELD_NONE
        {
            object.insert(self.quirks.effort_field.clone(), json!(effort));
        }
        body
    }
}

/// The full chat-completions endpoint for a configured `base_url`: accepts
/// either a base URL (`https://host/v1`) or a URL that already is the
/// endpoint. Shared with `/context`'s request display so both show exactly
/// where requests go.
pub fn endpoint_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    }
}

/// One conversation message on the wire: role and content, plus the
/// tool-calling fields when present.
fn message_json(message: &Message) -> Value {
    let mut object = json!({
        "role": message.role.as_str(),
        "content": message.content,
    });
    let map = object
        .as_object_mut()
        .expect("the message envelope is an object");
    if let Some(calls) = &message.tool_calls {
        map.insert(
            "tool_calls".to_string(),
            Value::Array(
                calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": call.arguments,
                            },
                        })
                    })
                    .collect(),
            ),
        );
    }
    if let Some(id) = &message.tool_call_id {
        map.insert("tool_call_id".to_string(), json!(id));
    }
    object
}

impl ModelClient for OpenAiCompatClient {
    fn label(&self) -> String {
        self.model.clone()
    }

    fn stream_chat(
        &self,
        messages: Vec<Message>,
        options: TurnOptions,
        cancel: CancellationToken,
    ) -> ModelStream {
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
        let body = self.request_body(&messages, &options);
        let http = self.http.clone();
        let headers = self.headers.clone();
        // Session headers go on after the static ones so a live session id
        // wins over a hand-written `headers = { "session-id" = ... }` in
        // providers.toml — the same precedence `/effort` has over
        // `body_overrides`.
        let session_headers: Vec<(String, String)> = self
            .session_headers(&options)
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        let quirks = self.quirks.clone();

        Box::pin(async_stream::try_stream! {
            let response = {
                let mut request = http.post(&url).bearer_auth(&api_key);
                for (key, value) in &headers {
                    request = request.header(key, value);
                }
                for (key, value) in &session_headers {
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
                let mut tool_calls = ToolCallAccumulator::default();

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
                                match parse_sse_frame(&frame, &mut tool_calls, &quirks)? {
                                    Parsed::Delta(item) => yield item,
                                    Parsed::Done => {
                                        if let Some(calls) = tool_calls.finish() {
                                            yield StreamItem::ToolCalls(calls);
                                        }
                                        return;
                                    }
                                    Parsed::Ignore => {}
                                }
                            }
                        }
                    }
                }

                // A well-behaved server ends with a blank line, but tolerate a
                // final frame that doesn't.
                if let Some(frame) = decoder.finish() {
                    match parse_sse_frame(&frame, &mut tool_calls, &quirks)? {
                        Parsed::Delta(item) => yield item,
                        Parsed::Done => {}
                        Parsed::Ignore => {}
                    }
                }

                // Tool calls are only complete once the stream is: flush what
                // the accumulator reassembled even without an explicit
                // `[DONE]` frame.
                if let Some(calls) = tool_calls.finish() {
                    yield StreamItem::ToolCalls(calls);
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

/// Reassembles streamed `delta.tool_calls` fragments into complete calls.
/// OpenAI-compatible providers stream one entry per call index per chunk,
/// each carrying (across successive chunks) the call's `id`, its function
/// `name`, and successive text fragments of `function.arguments`.
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    parts: BTreeMap<u64, PartialToolCall>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    /// Feed one frame's `delta` object; frames without `tool_calls` are
    /// ignored.
    pub fn feed(&mut self, delta: Option<&Value>) {
        let Some(calls) = delta
            .and_then(|delta| delta.get("tool_calls"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for call in calls {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
            let part = self.parts.entry(index).or_default();
            if let Some(id) = call.get("id").and_then(Value::as_str)
                && !id.is_empty()
            {
                part.id = id.to_string();
            }
            if let Some(name) = call.pointer("/function/name").and_then(Value::as_str)
                && !name.is_empty()
            {
                part.name = name.to_string();
            }
            if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                part.arguments.push_str(arguments);
            }
        }
    }

    /// Drain the reassembled calls in index order; `None` when the stream
    /// asked for no tools. A call the provider never named gets a fallback
    /// id; one with no streamed arguments gets `{}`.
    pub fn finish(&mut self) -> Option<Vec<ToolCall>> {
        if self.parts.is_empty() {
            return None;
        }
        let parts = std::mem::take(&mut self.parts);
        let calls = parts
            .into_iter()
            .filter(|(_, part)| !part.name.is_empty())
            .map(|(index, part)| ToolCall {
                id: if part.id.is_empty() {
                    format!("call_{index}")
                } else {
                    part.id
                },
                name: part.name,
                arguments: if part.arguments.is_empty() {
                    "{}".to_string()
                } else {
                    part.arguments
                },
            })
            .collect::<Vec<_>>();
        if calls.is_empty() {
            None
        } else {
            Some(calls)
        }
    }
}

fn parse_sse_frame(
    frame: &crate::core::sse::SseFrame,
    tool_calls: &mut ToolCallAccumulator,
    quirks: &WireQuirks,
) -> Result<Parsed, ModelError> {
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

    let delta = value.pointer("/choices/0/delta");
    tool_calls.feed(delta);
    let content = delta
        .and_then(|delta| delta.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !content.is_empty() {
        return Ok(Parsed::Delta(StreamItem::Delta(content.to_string())));
    }

    // The reasoning delta under the instance's spellings, first hit wins.
    // An empty `thinking_fields` list opts the instance out entirely.
    let reasoning = quirks
        .thinking_fields
        .iter()
        .find_map(|field| {
            delta
                .and_then(|delta| delta.get(field.as_str()))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        });
    if let Some(reasoning) = reasoning {
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
        let body = client.request_body(&[Message::user("hi")], &TurnOptions::default());

        assert_eq!(body["model"], "override");
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[test]
    fn effort_is_sent_last_and_only_when_set() {
        let client = OpenAiCompatClient::new("k", "https://example.test/v1", "m").unwrap();
        let options = |effort: Option<&str>| TurnOptions {
            effort: effort.map(str::to_string),
            tools: Vec::new(),
            session_id: None,
        };

        // No effort: the field is absent entirely, not null.
        let body = client.request_body(&[Message::user("hi")], &options(None));
        assert!(body.get("reasoning_effort").is_none());

        // Some effort: sent as the OpenAI-standard top-level field…
        let body = client.request_body(&[Message::user("hi")], &options(Some("high")));
        assert_eq!(body["reasoning_effort"], "high");

        // …and a per-turn effort beats a static `body_overrides` value.
        let mut overrides = serde_json::Map::new();
        overrides.insert("reasoning_effort".to_string(), json!("low"));
        let client = client.with_body_overrides(overrides);
        let body = client.request_body(&[Message::user("hi")], &options(Some("high")));
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn tools_are_sent_as_function_schemas_only_when_declared() {
        let client = OpenAiCompatClient::new("k", "https://example.test/v1", "m").unwrap();

        let body = client.request_body(&[Message::user("hi")], &TurnOptions::default());
        assert!(body.get("tools").is_none());

        let options = TurnOptions {
            effort: None,
            tools: vec![crate::core::tools::ToolSpec {
                name: "ping",
                description: "reply with pong",
                parameters_json: r#"{ "type": "object" }"#,
                read_only: true,
            }],
            session_id: None,
        };
        let body = client.request_body(&[Message::user("hi")], &options);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "ping");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn tool_messages_serialize_with_call_references() {
        let call = crate::core::tools::ToolCall {
            id: "call_1".to_string(),
            name: "ping".to_string(),
            arguments: r#"{ "n": 2 }"#.to_string(),
        };
        let assistant = Message::assistant_tool_calls(vec![call.clone()]);
        let result = Message::tool_result("call_1", "pong");
        let client = OpenAiCompatClient::new("k", "https://example.test/v1", "m").unwrap();
        let body = client.request_body(&[assistant, result], &TurnOptions::default());

        assert_eq!(body["messages"][0]["role"], "assistant");
        assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(body["messages"][0]["tool_calls"][0]["type"], "function");
        assert_eq!(body["messages"][0]["tool_calls"][0]["function"]["name"], "ping");
        assert_eq!(body["messages"][0]["tool_calls"][0]["function"]["arguments"], r#"{ "n": 2 }"#);

        assert_eq!(body["messages"][1]["role"], "tool");
        assert_eq!(body["messages"][1]["tool_call_id"], "call_1");
        assert_eq!(body["messages"][1]["content"], "pong");
    }

    #[test]
    fn parse_extracts_content_errors_thinking_and_done() {
        let frame = |data: &str| crate::core::sse::SseFrame {
            event: None,
            data: data.to_string(),
        };
        let parsed = parse_sse_frame(
            &frame(r#"{"choices":[{"delta":{"content":"hello"}}]}"#),
            &mut ToolCallAccumulator::default(),
            &WireQuirks::default(),
        )
        .unwrap();
        assert_eq!(
            parsed,
            Parsed::Delta(StreamItem::Delta("hello".to_string()))
        );

        assert_eq!(
            parse_sse_frame(&frame(""), &mut ToolCallAccumulator::default(), &WireQuirks::default())
                .unwrap(),
            Parsed::Ignore
        );
        assert_eq!(
            parse_sse_frame(
                &frame("[DONE]"),
                &mut ToolCallAccumulator::default(),
                &WireQuirks::default()
            )
            .unwrap(),
            Parsed::Done
        );

        let error = parse_sse_frame(
            &frame(r#"{"error":{"message":"bad key"}}"#),
            &mut ToolCallAccumulator::default(),
            &WireQuirks::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("bad key"));

        let parsed = parse_sse_frame(
            &frame(r#"{"choices":[{"delta":{"reasoning_content":"thinking…"}}]}"#),
            &mut ToolCallAccumulator::default(),
            &WireQuirks::default(),
        )
        .unwrap();
        assert_eq!(
            parsed,
            Parsed::Delta(StreamItem::Thinking("thinking…".to_string()))
        );

        // A delta with neither content nor reasoning is ignored.
        let parsed = parse_sse_frame(
            &frame(r#"{"choices":[{"delta":{}}]}"#),
            &mut ToolCallAccumulator::default(),
            &WireQuirks::default(),
        )
        .unwrap();
        assert_eq!(parsed, Parsed::Ignore);
    }

    #[test]
    fn parse_reads_usage_in_both_spellings() {
        let frame = |data: &str| crate::core::sse::SseFrame {
            event: None,
            data: data.to_string(),
        };
        let parsed = parse_sse_frame(
            &frame(r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4}}"#),
            &mut ToolCallAccumulator::default(),
            &WireQuirks::default(),
        )
        .unwrap();
        assert_eq!(parsed, Parsed::Delta(StreamItem::Usage { input: 10, output: 4 }));

        let parsed = parse_sse_frame(
            &frame(r#"{"usage":{"input_tokens":7,"output_tokens":2}}"#),
            &mut ToolCallAccumulator::default(),
            &WireQuirks::default(),
        )
        .unwrap();
        assert_eq!(parsed, Parsed::Delta(StreamItem::Usage { input: 7, output: 2 }));

        // No usage pair at all: ignored, not an error.
        let parsed = parse_sse_frame(
            &frame(r#"{"usage":{}}"#),
            &mut ToolCallAccumulator::default(),
            &WireQuirks::default(),
        )
        .unwrap();
        assert_eq!(parsed, Parsed::Ignore);
    }

    #[test]
    fn thinking_fields_follow_the_instance_quirks() {
        let frame = |data: &str| crate::core::sse::SseFrame {
            event: None,
            data: data.to_string(),
        };

        // Alternative spellings parse as Thinking, first hit wins.
        let quirks = WireQuirks {
            thinking_fields: vec!["reasoning".to_string(), "thinking".to_string()],
            ..Default::default()
        };
        for field in ["reasoning", "thinking"] {
            let data = format!(r#"{{"choices":[{{"delta":{{"{field}":"thinking…"}}}}]}}"#);
            let parsed = parse_sse_frame(
                &frame(&data),
                &mut ToolCallAccumulator::default(),
                &quirks,
            )
            .unwrap();
            assert_eq!(
                parsed,
                Parsed::Delta(StreamItem::Thinking("thinking…".to_string()))
            );
        }

        // An empty list opts the instance out of thinking parsing entirely.
        let quirks = WireQuirks {
            thinking_fields: Vec::new(),
            ..Default::default()
        };
        let parsed = parse_sse_frame(
            &frame(r#"{"choices":[{"delta":{"reasoning_content":"thinking…"}}]}"#),
            &mut ToolCallAccumulator::default(),
            &quirks,
        )
        .unwrap();
        assert_eq!(parsed, Parsed::Ignore);
    }

    #[test]
    fn effort_field_follows_the_instance_quirks() {
        let options = |effort: Option<&str>| TurnOptions {
            effort: effort.map(str::to_string),
            tools: Vec::new(),
            session_id: None,
        };

        // An alternative spelling renames the request field.
        let quirks = WireQuirks {
            effort_field: "reasoning".to_string(),
            ..Default::default()
        };
        let client = OpenAiCompatClient::new("k", "https://example.test/v1", "m")
            .unwrap()
            .with_quirks(quirks);
        let body = client.request_body(&[Message::user("hi")], &options(Some("high")));
        assert_eq!(body["reasoning"], "high");
        assert!(body.get("reasoning_effort").is_none());

        // `none` never sends the field, even with an effort selected.
        let quirks = WireQuirks {
            effort_field: EFFORT_FIELD_NONE.to_string(),
            ..Default::default()
        };
        let client = client.with_quirks(quirks);
        let body = client.request_body(&[Message::user("hi")], &options(Some("high")));
        assert!(body.get("reasoning").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn the_session_header_carries_the_turn_session_id() {
        let options = |session_id: Option<&str>| TurnOptions {
            effort: None,
            tools: Vec::new(),
            session_id: session_id.map(str::to_string),
        };
        let session = "0192f0c4-6a3b-7c1d-8e2f-3a4b5c6d7e8f";

        // Default: the instance sends codex's `session-id` spelling.
        let client = OpenAiCompatClient::new("k", "https://example.test/v1", "m")
            .unwrap()
            .with_session_header(Some("session-id".to_string()));
        assert_eq!(
            client.session_headers(&options(Some(session))),
            vec![("session-id", session)]
        );

        // No session id this turn: no header at all, not an empty one.
        assert!(client.session_headers(&options(None)).is_empty());

        // A model that renamed the header sends the new spelling.
        let client = client.with_session_header(Some("x-session-id".to_string()));
        assert_eq!(
            client.session_headers(&options(Some(session))),
            vec![("x-session-id", session)]
        );

        // An opted-out instance sends nothing even with a session id.
        let client = client.with_session_header(None);
        assert!(client.session_headers(&options(Some(session))).is_empty());
    }

    #[test]
    fn the_session_header_value_follows_the_active_session() {
        // The point of rebuilding clients per turn: the same client config
        // must announce whichever session the turn belongs to. Reusing one
        // client across sessions would pin the first session's id.
        let client = OpenAiCompatClient::new("k", "https://example.test/v1", "m")
            .unwrap()
            .with_session_header(Some("session-id".to_string()));
        let turn = |session_id: &str| TurnOptions {
            effort: None,
            tools: Vec::new(),
            session_id: Some(session_id.to_string()),
        };

        let first = "0192f0c4-6a3b-7c1d-8e2f-000000000001";
        let second = "0192f0c4-6a3b-7c1d-8e2f-000000000002";
        assert_eq!(
            client.session_headers(&turn(first)),
            vec![("session-id", first)]
        );
        assert_eq!(
            client.session_headers(&turn(second)),
            vec![("session-id", second)]
        );
    }

    #[test]
    fn accumulator_reassembles_fragmented_and_parallel_tool_calls() {
        let frame = |data: &str| crate::core::sse::SseFrame {
            event: None,
            data: data.to_string(),
        };
        let feed = |accumulator: &mut ToolCallAccumulator, data: &str| {
            parse_sse_frame(&frame(data), accumulator, &WireQuirks::default()).unwrap()
        };

        let mut accumulator = ToolCallAccumulator::default();
        // Two calls streamed side by side; the second one's arguments arrive
        // in fragments, and its id/name repeat on later chunks.
        feed(
            &mut accumulator,
            r#"{"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_a","type":"function","function":{"name":"ping","arguments":""}},
                {"index":1,"id":"call_b","type":"function","function":{"name":"echo","arguments":"{\"x\":"}}
            ]}}]}"#,
        );
        feed(
            &mut accumulator,
            r#"{"choices":[{"delta":{"tool_calls":[
                {"index":1,"function":{"arguments":"1}"}}
            ]}}]}"#,
        );
        assert_eq!(
            feed(&mut accumulator, r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#),
            Parsed::Ignore
        );

        let calls = accumulator.finish().expect("two complete calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].name, "ping");
        assert_eq!(calls[0].arguments, "{}");
        assert_eq!(calls[1].id, "call_b");
        assert_eq!(calls[1].name, "echo");
        assert_eq!(calls[1].arguments, r#"{"x":1}"#);

        // Draining twice is safe: the second flush is empty.
        assert_eq!(accumulator.finish(), None);
    }

    #[tokio::test]
    async fn empty_key_yields_not_configured() {
        let client = OpenAiCompatClient::new("  ", "https://example.test/v1", "m").unwrap();
        let mut stream = client.stream_chat(vec![], TurnOptions::default(), CancellationToken::new());
        let error = stream.next().await.expect("one item").unwrap_err();
        assert!(matches!(error, ModelError::NotConfigured(_)));
    }
}
