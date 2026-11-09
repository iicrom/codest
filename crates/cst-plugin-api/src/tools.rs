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

/// The maximum number of model → tool → model rounds in one user turn. A
/// model that keeps calling tools past this limit ends the turn with an
/// error instead of looping forever.
pub const MAX_TOOL_ROUNDS: usize = 8;

/// A tool a plugin contributes to the model. `parameters_json` is the raw
/// JSON-schema object for the `function.parameters` field (empty text means
/// no parameters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters_json: &'static str,
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

/// Executes tool calls on behalf of the worker. Implemented by the plugin
/// host, which dispatches to the plugin that declared the tool.
///
/// The contract is synchronous (`&self`, plain return), but the turn runner
/// moves every call onto the blocking pool and watches the interrupt token
/// while it runs: implementations may block freely, and an interrupted turn
/// discards the outcome of a still-running call.
pub trait ToolExecutor: Send + Sync {
    fn execute(&self, call: &ToolCall) -> Result<String, String>;
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
        if calls.is_empty() {
            None
        } else {
            Some(calls)
        }
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
        };
        let function = spec.function_json()["function"].clone();
        assert_eq!(function["name"], "ping");
        assert_eq!(function["description"], "reply with pong");
        assert_eq!(function["parameters"]["properties"]["n"]["type"], "integer");

        let empty = ToolSpec { parameters_json: "", ..spec };
        assert_eq!(empty.function_json()["function"]["parameters"], serde_json::json!({ "type": "object" }));

        let malformed = ToolSpec { parameters_json: "not json", ..spec };
        assert_eq!(malformed.function_json()["function"]["parameters"], serde_json::json!({ "type": "object" }));
    }

    #[test]
    fn tool_call_survives_serde_roundtrip() {
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "ping".to_string(),
            arguments: r#"{ "n": 2 }"#.to_string(),
        };
        let text = serde_json::to_string(&call).expect("serialize");
        assert_eq!(serde_json::from_str::<ToolCall>(&text).expect("deserialize"), call);
    }
}
