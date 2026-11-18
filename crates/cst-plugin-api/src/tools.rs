//! Tool-calling vocabulary shared by the core and the plugin layer.
//!
//! Plugins *declare* tools ([`ToolSpec`], via `Plugin::tools`) and *execute*
//! calls ([`ToolExecutor`], via `Plugin::run_tool`). The core owns the wire
//! shape ([`ToolCall`] as it travels inside a `Message`) and the turn loop
//! that drives model → tool → model rounds; neither side knows about the
//! other directly, which keeps `core` free of plugin dependencies.
//!
//! Tool schemas are raw JSON-schema text (`parameters_json`) rather than
//! `serde_json::Value` so a plugin can build its specs in a `const` context —
//! plugins are `&'static dyn Plugin` and cannot construct `Value` at
//! compile time. The text is parsed when the request body is serialized.
//!
//! [`ToolCallAccumulator`] is the other half every streaming protocol needs:
//! providers stream tool calls as per-index fragments (`id`, `name`, and
//! successive pieces of `arguments`), and reassembling them is identical work
//! for `openai-compat` and for any plugin protocol. It lives here so both
//! share one implementation instead of re-bugging fragment order twice.
//!
//! A tool result is text ([`ToolOutput::text`]) plus, for tools that read or
//! render media, attached images ([`ToolImage`]). The text-only
//! [`Plugin::run_tool`](crate::plugin::Plugin::run_tool) contract stays the
//! simple path: it is what most tools return, and the image-carrying path has
//! a default that wraps it.

use serde::Deserialize;
use serde::Serialize;

/// The default cap on model → tool → model rounds in one user turn: none.
///
/// The turn loop is deliberately unbounded, following codex: it ends when the
/// model stops calling tools, the user interrupts, or the request fails, so a
/// long read → edit → test cycle is never cut short by a loop counter. Esc
/// remains the way out of a run that will not settle. `config.toml`'s
/// `max_tool_rounds` re-enables a guard for hosts that want a ceiling.
pub const DEFAULT_MAX_TOOL_ROUNDS: Option<usize> = None;

/// A tool a plugin contributes to the model. `parameters_json` is the raw
/// JSON-schema object for the `function.parameters` field (empty text means
/// no parameters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters_json: &'static str,
    /// The call only inspects state and never mutates it (`read`, `ls`,
    /// `grep`). A purely advisory classification: profiles and the `/tools`
    /// switch use it to decide what a read-only mode may enable, and the UI
    /// shows it as a marker. Nothing enforces it — a tool that lies about
    /// `read_only` simply gets enabled by read-only profiles.
    pub read_only: bool,
}

impl ToolSpec {
    /// The `function` object of the chat-completions `tools` entry. A
    /// malformed `parameters_json` degrades to an empty schema rather than
    /// failing the turn.
    pub fn function_json(&self) -> serde_json::Value {
        let parameters = serde_json::from_str(self.parameters_json)
            .unwrap_or_else(|_| serde_json::json!({ "type": "object" }));
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": parameters,
            },
        })
    }
}

/// One tool invocation the model asked for, as it appears inside an
/// assistant message (`tool_calls[].function`). `arguments` is the raw JSON
/// text the model streamed; it is passed to the executor verbatim.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON object text (`"{}"` when the tool takes no arguments).
    pub arguments: String,
}

/// One image attached to a tool result.
///
/// Carried inline as a `data:` URL (base64), like codex's `view_image`: a
/// chat-completions request has no uploaded-file handle, so the bytes travel
/// with the message. `mime` names the encoding (`image/png`, `image/jpeg`, …)
/// and is separate from the URL so a front-end can label a card without
/// decoding it. `detail` is the hint requested from the model backend —
/// `"high"` for the downscaled default, `"original"` when the tool kept the
/// source resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolImage {
    /// `data:<mime>;base64,<payload>`.
    pub data_url: String,
    pub mime: String,
    /// `"high"` or `"original"`: the budget the tool prepared the image at.
    /// `original` is this contract's label, not a wire value — a client maps
    /// it to whatever its protocol accepts (chat-completions knows only
    /// `low`/`high`/`auto`).
    pub detail: String,
}

impl ToolImage {
    /// The image's own byte count, decoded — what the file cost before
    /// base64 and the `data:` prefix inflated it (`payload_bytes` is 4/3 of
    /// it). This is the number a status line or an inspector should show: a
    /// 412 KB PNG reads as `412 KB`, not as the size of its encoding.
    pub fn decoded_bytes(&self) -> usize {
        // base64 pads to a multiple of four; undo the padding to recover the
        // exact source length rather than the rounded-up encoding length.
        let encoded = self.payload_bytes();
        let padding = self
            .data_url
            .as_bytes()
            .iter()
            .rev()
            .take_while(|byte| **byte == b'=')
            .count();
        encoded / 4 * 3 - padding.min(2)
    }

    /// The encoded length after the data-URL prefix — the bytes actually on
    /// the wire.
    pub fn payload_bytes(&self) -> usize {
        self.data_url
            .rfind(',')
            .map(|comma| self.data_url.len() - comma - 1)
            .unwrap_or(0)
    }
}

/// The result of one tool call: its text and any images it attached.
///
/// Text-only tools never construct this — [`ToolOutput::text`] wraps a
/// `String`. A tool that returns an image states what it did in `text`
/// ("read image … (2304x864 → 2048x768, image/png)") and attaches the
/// prepared bytes; the model sees the text, the following request carries
/// the images.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolOutput {
    pub text: String,
    pub images: Vec<ToolImage>,
}

impl ToolOutput {
    /// A text-only result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }

    /// A result whose text reports on an attached image.
    pub fn with_images(text: impl Into<String>, images: Vec<ToolImage>) -> Self {
        Self {
            text: text.into(),
            images,
        }
    }
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

impl From<&str> for ToolOutput {
    fn from(text: &str) -> Self {
        Self::text(text.to_string())
    }
}

/// Executes tool calls on behalf of the worker. Implemented by the plugin
/// host, which dispatches to the plugin that declared the tool.
///
/// The contract is synchronous (`&self`, plain return), but the turn runner
/// moves every call onto the blocking pool and watches the interrupt token
/// while it runs: implementations may block freely, and an interrupted turn
/// discards the outcome of a still-running call.
pub trait ToolExecutor: Send + Sync {
    /// Execute one call, text only. Required: this is the method every
    /// existing executor already implements, and it must stay the one a
    /// text-only executor writes — a pair of mutually defaulted methods would
    /// recurse forever on a type that implements neither.
    fn execute(&self, call: &ToolCall) -> Result<String, String>;

    /// Execute one call, images included. The default wraps [`Self::execute`]
    /// with no images, so an executor that overrides only [`Self::execute`]
    /// compiles and behaves exactly as before.
    fn execute_output(&self, call: &ToolCall) -> Result<ToolOutput, String> {
        self.execute(call).map(ToolOutput::text)
    }
}

/// Reassembles streamed `delta.tool_calls` fragments into complete calls.
/// Providers stream one entry per call index per chunk, each carrying (across
/// successive chunks) the call's `id`, its function `name`, and successive
/// text fragments of `function.arguments`.
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    parts: std::collections::BTreeMap<u64, PartialToolCall>,
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
    pub fn feed(&mut self, delta: Option<&serde_json::Value>) {
        let Some(calls) = delta
            .and_then(|delta| delta.get("tool_calls"))
            .and_then(serde_json::Value::as_array)
        else {
            return;
        };
        for call in calls {
            let index = call
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let part = self.parts.entry(index).or_default();
            if let Some(id) = call.get("id").and_then(serde_json::Value::as_str)
                && !id.is_empty()
            {
                part.id = id.to_string();
            }
            if let Some(name) = call
                .pointer("/function/name")
                .and_then(serde_json::Value::as_str)
                && !name.is_empty()
            {
                part.name = name.to_string();
            }
            if let Some(arguments) = call
                .pointer("/function/arguments")
                .and_then(serde_json::Value::as_str)
            {
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
        if calls.is_empty() { None } else { Some(calls) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_json_parses_parameters_and_defaults_to_object_schema() {
        let spec = ToolSpec {
            name: "ping",
            description: "reply with pong",
            parameters_json: r#"{ "type": "object", "properties": { "n": { "type": "integer" } } }"#,
            read_only: true,
        };
        let function = spec.function_json()["function"].clone();
        assert_eq!(function["name"], "ping");
        assert_eq!(function["description"], "reply with pong");
        assert_eq!(function["parameters"]["properties"]["n"]["type"], "integer");

        let empty = ToolSpec {
            parameters_json: "",
            ..spec
        };
        assert_eq!(
            empty.function_json()["function"]["parameters"],
            serde_json::json!({ "type": "object" })
        );

        let malformed = ToolSpec {
            parameters_json: "not json",
            ..spec
        };
        assert_eq!(
            malformed.function_json()["function"]["parameters"],
            serde_json::json!({ "type": "object" })
        );
    }

    #[test]
    fn tool_call_survives_serde_roundtrip() {
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "ping".to_string(),
            arguments: r#"{ "n": 2 }"#.to_string(),
        };
        let text = serde_json::to_string(&call).expect("serialize");
        assert_eq!(
            serde_json::from_str::<ToolCall>(&text).expect("deserialize"),
            call
        );
    }
}
