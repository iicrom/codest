//! `ask_user_question`: let the model ask the user instead of guessing.
//!
//! One tool, one interruption. The model sends up to four questions with
//! written-out options; the front-end shows them as a single dialog and hands
//! the choices back as text the model can act on. The tool itself is thin on
//! purpose — it validates and normalizes the arguments, asks the front-end
//! through [`PromptHost`](crate::plugins::PromptHost), and formats the answer
//! envelope — because a plugin may not draw (see `docs/plugins.md`).
//!
//! The shapes here follow the `ask_user_question` extension of pi
//! (`@juicesharp/rpiv-ask-user-question`): the same parameter limits, the same
//! reserved labels, the same envelope wording, so a model that learned the
//! tool elsewhere finds nothing new.

use std::sync::Arc;
use std::sync::Mutex;

use crate::core::tools::ToolCall;
use crate::core::tools::ToolSpec;
use crate::plugins::Plugin;
use crate::plugins::PluginError;
use crate::plugins::PromptAnswer;
use crate::plugins::PromptAnswerKind;
use crate::plugins::PromptForm;
use crate::plugins::PromptHost;
use crate::plugins::PromptOption;
use crate::plugins::PromptQuestion;
use crate::plugins::PromptResponse;

/// The canonical tool name.
pub const TOOL_NAME: &str = "ask_user_question";

/// At most this many questions per call.
pub const MAX_QUESTIONS: usize = 4;
/// At least this many options per question.
pub const MIN_OPTIONS: usize = 2;
/// At most this many options per question.
pub const MAX_OPTIONS: usize = 4;
/// Longest question header (the dialog's tab chip).
pub const MAX_HEADER_LENGTH: usize = 16;
/// Longest option label.
pub const MAX_LABEL_LENGTH: usize = 60;

/// The custom-answer row every question gets, appended by the front-end.
pub const CUSTOM_ROW: &str = "Type something.";
/// The commit row a multi-select question gets.
pub const NEXT_ROW: &str = "Next";

/// Labels the dialog supplies itself. Authoring one is rejected: the model is
/// conditioned to reach for `Other`, and the other two are the runtime rows.
pub const RESERVED_LABELS: [&str; 3] = ["Other", CUSTOM_ROW, NEXT_ROW];

/// What the model sees when the front-end has no dialog to show.
pub const NO_UI: &str = "UI not available: this run has no interactive front-end, so the user \
never saw the questions — do NOT treat this as a decline. Ask the questions as plain chat text \
instead, without using this tool.";

/// The envelope's decline sentence; also what an empty submission collapses to.
pub const DECLINE_MESSAGE: &str = "User declined to answer questions";
pub const ENVELOPE_PREFIX: &str = "User has answered your questions:";
pub const ENVELOPE_SUFFIX: &str = "You can now continue with the user's answers in mind.";
/// Stand-in for an answer text that is empty.
pub const NO_INPUT: &str = "(no input)";

static TOOL_DESCRIPTION: &str = "Ask the user one or more structured questions during execution. \
Use when you need to:\n\
1. Gather user preferences or requirements\n\
2. Clarify ambiguous instructions\n\
3. Get decisions on implementation choices as you work\n\
4. Offer choices to the user about what direction to take\n\
\n\
Usage notes:\n\
- Users can type a custom answer via the automatically appended \"Type something.\" row on \
every question, or press Esc to abandon the questionnaire. Do NOT author \"Other\" or \
\"Type something.\" labels yourself — reserved labels are rejected at runtime.\n\
- Use multiSelect: true when multiple answers are valid.\n\
- If you recommend a specific option, make that the first option in the list and add \
\"(Recommended)\" at the end of the label.\n\
- Provide an options[].preview markdown string when an option benefits from richer context \
(ASCII mockups, code snippets, diagrams, configuration examples) — single-select questions only. \
Previews are not a substitute for a good description: keep the label and description short \
enough to compare at a glance.";

static PROMPT: &str = "## Asking the user\n\
- `ask_user_question` puts up to 4 structured questions to the user in one interruption. Use it \
when the request is underspecified and you cannot proceed without a concrete decision.\n\
- Each question needs 2-4 options, and every option needs a short label (1-5 words) plus a \
description of what the choice means or costs.\n\
- Every question also offers a custom answer and a decline, so do not author \"Other\" or \
\"Type something.\" options — those labels are rejected.\n\
- Set `multiSelect` when several answers are valid. If you recommend an option, list it first \
and end its label with \"(Recommended)\".\n\
- Group every clarifying question into one call; never stack several calls back to back.";

static PARAMETERS: &str = r#"{
  "type": "object",
  "properties": {
    "questions": {
      "type": "array",
      "minItems": 1,
      "maxItems": 4,
      "description": "Questions to ask the user (1-4 questions)",
      "items": {
        "type": "object",
        "properties": {
          "question": {
            "type": "string",
            "description": "The complete question to ask the user. Should be clear, specific, and end with a question mark. If multiSelect is true, phrase it accordingly."
          },
          "header": {
            "type": "string",
            "maxLength": 16,
            "description": "MAX 16 CHARACTERS — very short chip/tag shown next to the question, e.g. \"Auth method\", \"Library\", \"Approach\"."
          },
          "options": {
            "type": "array",
            "minItems": 2,
            "maxItems": 4,
            "description": "The available choices: 2-4 distinct, mutually exclusive options (unless multiSelect is enabled). The \"Type something.\" row is appended automatically — do NOT author it.",
            "items": {
              "type": "object",
              "properties": {
                "label": {
                  "type": "string",
                  "maxLength": 60,
                  "description": "MAX 60 CHARACTERS — the display text the user picks. Concise (1-5 words) and clearly describing the choice."
                },
                "description": {
                  "type": "string",
                  "description": "What this option means or what happens if chosen — its trade-offs or implications."
                },
                "preview": {
                  "type": "string",
                  "description": "Optional markdown preview shown beside the options while this option is focused: mockups, code snippets, diagrams, configs. Single-select questions only."
                }
              },
              "required": ["label", "description"]
            }
          },
          "multiSelect": {
            "type": "boolean",
            "default": false,
            "description": "Set to true to let the user select several options instead of one. Use when the choices are not mutually exclusive."
          }
        },
        "required": ["question", "header", "options"]
      }
    }
  },
  "required": ["questions"]
}"#;

static TOOLS: [ToolSpec; 1] = [ToolSpec {
    name: TOOL_NAME,
    description: TOOL_DESCRIPTION,
    parameters_json: PARAMETERS,
    // Asking a question mutates nothing: a read-only profile may keep it, and
    // `/tools` marks it as an inspection aid rather than an executor.
    read_only: true,
}];

pub static ASK_USER: AskUser = AskUser::new();

/// The plugi itself. The prompt service arrives from the front-end once at
/// startup ([`Plugin::set_prompt_host`]) and stays behind a `Mutex` because a
/// tool runs with `&self`.
pub struct AskUser {
    host: Mutex<Option<Arc<dyn PromptHost>>>,
}

impl AskUser {
    pub const fn new() -> Self {
        Self {
            host: Mutex::new(None),
        }
    }

    /// The installed prompt service, when the front-end supplied one.
    fn host(&self) -> Option<Arc<dyn PromptHost>> {
        self.host.lock().expect("ask-user host lock").clone()
    }
}

impl Default for AskUser {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for AskUser {
    fn id(&self) -> &'static str {
        TOOL_NAME
    }

    fn system_prompt(&self) -> Option<&'static str> {
        Some(PROMPT)
    }

    fn tools(&self) -> &'static [ToolSpec] {
        &TOOLS
    }

    fn set_prompt_host(&self, host: Arc<dyn PromptHost>) {
        *self.host.lock().expect("ask-user host lock") = Some(host);
    }

    fn run_tool(&self, call: &ToolCall) -> Result<String, PluginError> {
        if call.name != TOOL_NAME {
            return Err(PluginError::new(format!(
                "plugin `{TOOL_NAME}` does not implement tool `{}`",
                call.name
            )));
        }
        // Validation and normalization run first, so every downstream consumer
        // — the dialog and the envelope — sees the same clean text.
        let form = parse_form(&call.arguments).map_err(PluginError::new)?;
        let Some(host) = self.host() else {
            return Err(PluginError::new(NO_UI));
        };
        match host.ask(form.clone()) {
            Some(response) => Ok(envelope(&form, &response)),
            // The front-end could not show the form at all. Say so plainly:
            // the user never saw the questions, which is not a decline.
            None => Err(PluginError::new(NO_UI)),
        }
    }
}

/// Parse, normalize, and validate the model's arguments.
fn parse_form(arguments: &str) -> Result<PromptForm, String> {
    let trimmed = arguments.trim();
    let raw: serde_json::Value = if trimmed.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(trimmed)
            .map_err(|error| format!("arguments are not valid JSON: {error}"))?
    };
    // Some parsers (GLM/Qwen-class models behind LiteLLM) double-encode nested
    // structures as JSON strings. Decode only where an array or object belongs,
    // so a malformed call still reports its real problem.
    let raw = decode_nested(raw);
    let questions = raw
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "At least one question is required".to_string())?;

    if questions.is_empty() {
        return Err("At least one question is required".to_string());
    }
    if questions.len() > MAX_QUESTIONS {
        return Err(format!(
            "At most {MAX_QUESTIONS} questions are allowed per invocation"
        ));
    }

    let mut form = PromptForm::new(Vec::with_capacity(questions.len()));
    for question in questions {
        let object = question
            .as_object()
            .ok_or_else(|| "Each question must be an object".to_string())?;
        let text = text_field(object, "question")
            .ok_or_else(|| "Each question requires a `question` string".to_string())?;
        let header = text_field(object, "header").unwrap_or_default();
        let options = object
            .get("options")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "Each question requires an `options` array".to_string())?;
        let mut parsed = Vec::with_capacity(options.len());
        for option in options {
            let option = option
                .as_object()
                .ok_or_else(|| "Each option must be an object".to_string())?;
            let label = text_field(option, "label")
                .ok_or_else(|| "Each option requires a `label` string".to_string())?;
            let description = text_field(option, "description").unwrap_or_default();
            let preview = text_field(option, "preview").filter(|text| !text.is_empty());
            let mut spec = PromptOption::new(label, description);
            if let Some(preview) = preview {
                spec = spec.with_preview(preview);
            }
            parsed.push(spec);
        }
        let multi_select = object
            .get("multiSelect")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let question = PromptQuestion {
            question: text,
            header,
            options: parsed,
            multi_select,
        };
        form.questions.push(question);
    }

    validate(&form)?;
    Ok(form)
}

/// One string field, with line terminators normalized. A bare `\r` is a cursor
/// control byte, not text: it would overwrite the row it lands on, so it is
/// deleted (never turned into a space or a newline). `\r\n` collapses to `\n`,
/// which keeps genuine multi-line content — previews — intact.
fn text_field(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(|text| text.replace("\r\n", "\n").replace('\r', ""))
}

/// Decode the nested structures a double-encoded tool call arrives as: a JSON
/// string where an array or object belongs is parsed back, one level down
/// (`questions`, each question, its `options`, each option).
fn decode_nested(value: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut map) = value else {
        return value;
    };
    let Some(slot) = map.remove("questions") else {
        return serde_json::Value::Object(map);
    };
    let decoded = match slot {
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(decode_question).collect())
        }
        serde_json::Value::String(text) => {
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(serde_json::Value::Array(items)) => {
                    serde_json::Value::Array(items.into_iter().map(decode_question).collect())
                }
                // Not the shape an array would take: leave it alone so
                // validation reports the real problem.
                _ => serde_json::Value::String(text),
            }
        }
        other => other,
    };
    map.insert("questions".to_string(), decoded);
    serde_json::Value::Object(map)
}

/// One question of a double-encoded call: the question object itself, and its
/// `options` array.
fn decode_question(value: serde_json::Value) -> serde_json::Value {
    let value = decode_inner(value);
    let serde_json::Value::Object(mut question) = value else {
        return value;
    };
    // Only a string-encoded array is decoded; a real array is left untouched —
    // `remove` would take it off the map, so it must go back on either way.
    if matches!(question.get("options"), Some(serde_json::Value::String(_)))
        && let Some(serde_json::Value::String(text)) = question.remove("options")
    {
        let decoded = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(options @ serde_json::Value::Array(_)) => options,
            _ => serde_json::Value::String(text),
        };
        question.insert("options".to_string(), decoded);
    }
    serde_json::Value::Object(question)
}

/// A JSON string holding an object → that object; anything else unchanged.
fn decode_inner(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => match serde_json::from_str(&text) {
            Ok(parsed @ serde_json::Value::Object(_)) => parsed,
            _ => serde_json::Value::String(text),
        },
        other => other,
    }
}

/// Reject what the dialog could not show honestly. The order is the reference
/// order: the reserved-label guard short-circuits before the duplicate-label
/// one, so a model that authored two `Other` rows hears the more useful error.
fn validate(form: &PromptForm) -> Result<(), String> {
    if form.questions.is_empty() {
        return Err("At least one question is required".to_string());
    }
    if form.questions.len() > MAX_QUESTIONS {
        return Err(format!(
            "At most {MAX_QUESTIONS} questions are allowed per invocation"
        ));
    }
    let mut seen = Vec::with_capacity(form.questions.len());
    for question in &form.questions {
        if seen.contains(&question.question.as_str()) {
            return Err("Question text must be unique within an invocation".to_string());
        }
        seen.push(question.question.as_str());
    }
    for question in &form.questions {
        if question.header.chars().count() > MAX_HEADER_LENGTH {
            return Err(format!(
                "Each question header allows at most {MAX_HEADER_LENGTH} characters"
            ));
        }
        if question.options.len() < MIN_OPTIONS {
            return Err(format!("Each question requires at least {MIN_OPTIONS} options"));
        }
        if question.options.len() > MAX_OPTIONS {
            return Err(format!("Each question allows at most {MAX_OPTIONS} options"));
        }
        let mut labels: Vec<&str> = Vec::with_capacity(question.options.len());
        for option in &question.options {
            if option.label.chars().count() > MAX_LABEL_LENGTH {
                return Err(format!(
                    "Each option label allows at most {MAX_LABEL_LENGTH} characters"
                ));
            }
            if RESERVED_LABELS.contains(&option.label.as_str()) {
                return Err(format!(
                    "Option label is reserved ({})",
                    RESERVED_LABELS.join(", ")
                ));
            }
            if labels.contains(&option.label.as_str()) {
                return Err("Option labels must be unique within a question".to_string());
            }
            labels.push(option.label.as_str());
        }
    }
    Ok(())
}

/// Format one answer's scalar text: the chosen label, the typed text, or a
/// multi-select answer's checked labels followed by any typed addition.
fn answer_scalar(answer: &PromptAnswer) -> String {
    match answer.kind {
        PromptAnswerKind::Multi => {
            let mut values: Vec<&str> = answer.selected.iter().map(String::as_str).collect();
            if let Some(text) = answer.answer.as_deref().filter(|text| !text.is_empty()) {
                values.push(text);
            }
            if values.is_empty() {
                NO_INPUT.to_string()
            } else {
                values.join(", ")
            }
        }
        PromptAnswerKind::Custom => answer
            .answer
            .as_deref()
            .filter(|text| !text.is_empty())
            .unwrap_or(NO_INPUT)
            .to_string(),
        PromptAnswerKind::Option => answer.answer.clone().unwrap_or_else(|| NO_INPUT.to_string()),
    }
}

/// One `"question"="answer"` segment, plus the chosen preview and the note.
fn answer_segment(answer: &PromptAnswer) -> String {
    let mut parts = vec![format!(
        "\"{}\"=\"{}\"",
        answer.question,
        answer_scalar(answer)
    )];
    if let Some(preview) = answer.preview.as_deref().filter(|text| !text.is_empty()) {
        parts.push(format!("selected preview: {preview}"));
    }
    if let Some(notes) = answer.notes.as_deref().filter(|text| !text.is_empty()) {
        parts.push(format!("user notes: {notes}"));
    }
    format!("{}.", parts.join(". "))
}

/// The model-facing text for one answered form.
///
/// A decline — and a submission that carries neither an answer nor a global
/// note — collapses to the single canonical sentence, so the model cannot
/// confuse "the user skipped everything" with "the user answered".
pub fn envelope(form: &PromptForm, response: &PromptResponse) -> String {
    if response.cancelled {
        return DECLINE_MESSAGE.to_string();
    }
    let mut segments: Vec<String> = Vec::new();
    let mut unanswered: Vec<usize> = Vec::new();
    for (index, _) in form.questions.iter().enumerate() {
        match response.answer(index) {
            Some(answer) => segments.push(answer_segment(answer)),
            None => unanswered.push(index),
        }
    }
    let global_note = response
        .global_note
        .as_deref()
        .filter(|note| !note.is_empty());
    if segments.is_empty() && global_note.is_none() {
        return DECLINE_MESSAGE.to_string();
    }
    if !unanswered.is_empty() {
        let named: Vec<String> = unanswered
            .iter()
            .map(|index| format!("\"{}\"", form.questions[*index].question))
            .collect();
        segments.push(format!("unanswered: {}.", named.join(", ")));
    }
    if let Some(note) = global_note {
        segments.push(format!("global note: {note}."));
    }
    format!(
        "{ENVELOPE_PREFIX} {} {ENVELOPE_SUFFIX}",
        segments.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn args(questions: serde_json::Value) -> String {
        serde_json::json!({ "questions": questions }).to_string()
    }

    fn question(text: &str, header: &str, labels: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "question": text,
            "header": header,
            "options": labels
                .iter()
                .map(|label| serde_json::json!({ "label": label, "description": "d" }))
                .collect::<Vec<_>>(),
        })
    }

    /// A host that answers with `response` without ever asking anyone.
    struct Stub {
        response: Option<PromptResponse>,
        seen: Mutex<Vec<PromptForm>>,
    }

    impl Stub {
        fn new(response: Option<PromptResponse>) -> Arc<Self> {
            Arc::new(Self {
                response,
                seen: Mutex::new(Vec::new()),
            })
        }
    }

    impl PromptHost for Stub {
        fn ask(&self, form: PromptForm) -> Option<PromptResponse> {
            self.seen.lock().expect("stub lock").push(form);
            self.response.clone()
        }
    }

    fn plugin_with(response: Option<PromptResponse>) -> (AskUser, Arc<Stub>) {
        let plugin = AskUser::new();
        let stub = Stub::new(response);
        plugin.set_prompt_host(stub.clone());
        (plugin, stub)
    }

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "c1".to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
        }
    }

    fn single_answer(text: &str, answer: &str) -> PromptResponse {
        PromptResponse {
            answers: vec![PromptAnswer {
                question_index: 0,
                question: text.to_string(),
                kind: PromptAnswerKind::Option,
                answer: Some(answer.to_string()),
                selected: Vec::new(),
                notes: None,
                preview: None,
            }],
            cancelled: false,
            global_note: None,
        }
    }

    #[test]
    fn the_spec_is_the_contract_the_model_sees() {
        let function = TOOLS[0].function_json();
        assert_eq!(function["function"]["name"], TOOL_NAME);
        let parameters = &function["function"]["parameters"];
        assert_eq!(parameters["properties"]["questions"]["maxItems"], 4);
        let item = &parameters["properties"]["questions"]["items"];
        assert_eq!(item["properties"]["header"]["maxLength"], 16);
        assert_eq!(item["properties"]["options"]["minItems"], 2);
        assert_eq!(item["properties"]["options"]["maxItems"], 4);
        assert_eq!(
            item["properties"]["options"]["items"]["properties"]["label"]["maxLength"],
            60
        );
        assert!(TOOLS[0].read_only, "asking a question mutates nothing");
        // The prompt section teaches the tool by name, so the model does not
        // have to infer its conventions from the schema alone.
        assert!(PROMPT.contains(TOOL_NAME), "{PROMPT}");
    }

    #[test]
    fn validation_keeps_the_reference_order_and_wording() {
        let cases: Vec<(serde_json::Value, &str)> = vec![
            (
                serde_json::json!([]),
                "At least one question is required",
            ),
            (
                serde_json::json!([
                    question("a?", "A", &["1", "2"]),
                    question("b?", "B", &["1", "2"]),
                    question("c?", "C", &["1", "2"]),
                    question("d?", "D", &["1", "2"]),
                    question("e?", "E", &["1", "2"]),
                ]),
                "At most 4 questions are allowed per invocation",
            ),
            (
                serde_json::json!([
                    question("same?", "A", &["1", "2"]),
                    question("same?", "B", &["1", "2"]),
                ]),
                "Question text must be unique within an invocation",
            ),
            (
                serde_json::json!([question("q?", "A", &["only"])]),
                "Each question requires at least 2 options",
            ),
            (
                serde_json::json!([question("q?", "A", &["1", "2", "3", "4", "5"])]),
                "Each question allows at most 4 options",
            ),
            (
                serde_json::json!([question("q?", "A", &["1", "Other"])]),
                "Option label is reserved (Other, Type something., Next)",
            ),
            (
                serde_json::json!([question("q?", "A", &["same", "same"])]),
                "Option labels must be unique within a question",
            ),
            (
                serde_json::json!([question("q?", "a header that is too long", &["1", "2"])]),
                "Each question header allows at most 16 characters",
            ),
        ];
        for (questions, expected) in cases {
            let error = parse_form(&args(questions)).expect_err(expected);
            assert_eq!(error, expected);
        }
        // A reserved label wins over a duplicate: the model hears the more
        // useful complaint first.
        let error = parse_form(&args(serde_json::json!([question(
            "q?",
            "A",
            &["Other", "Other"]
        )])))
        .unwrap_err();
        assert!(error.starts_with("Option label is reserved"), "{error}");
    }

    #[test]
    fn normalization_cleans_line_terminators_before_validation() {
        // A stray `\r` is deleted (it is a cursor-control byte), while a real
        // `\r\n` becomes a plain newline so previews keep their shape.
        let arguments = serde_json::json!({
            "questions": [{
                "question": "a\rb?",
                "header": "H\r",
                "options": [
                    { "label": "Other\r", "description": "d" },
                    { "label": "two", "description": "d", "preview": "x\r\ny" }
                ]
            }]
        })
        .to_string();
        // `Other\r` normalizes to the reserved label, so the guard sees it.
        let error = parse_form(&arguments).unwrap_err();
        assert!(error.starts_with("Option label is reserved"), "{error}");

        let arguments = serde_json::json!({
            "questions": [{
                "question": "a\rb?",
                "header": "H\r",
                "options": [
                    { "label": "one", "description": "d", "preview": "x\r\ny" },
                    { "label": "two", "description": "d" }
                ]
            }]
        })
        .to_string();
        let form = parse_form(&arguments).expect("valid");
        assert_eq!(form.questions[0].question, "ab?");
        assert_eq!(form.questions[0].header, "H");
        assert_eq!(
            form.questions[0].options[0].preview.as_deref(),
            Some("x\ny")
        );
    }

    #[test]
    fn double_encoded_arguments_are_decoded_back() {
        let questions = serde_json::json!([question("q?", "A", &["1", "2"])]).to_string();
        let arguments = serde_json::json!({ "questions": questions }).to_string();
        let form = parse_form(&arguments).expect("decoded");
        assert_eq!(form.questions.len(), 1);
        assert_eq!(form.questions[0].options.len(), 2);

        // One level deeper: the question object itself, and its options.
        let one = question("q?", "A", &["1", "2"]);
        let options = one["options"].to_string();
        let mut encoded = one.clone();
        encoded["options"] = serde_json::Value::String(options);
        let arguments = serde_json::json!({ "questions": [encoded.to_string()] }).to_string();
        let form = parse_form(&arguments).expect("decoded deeper");
        assert_eq!(form.questions[0].options.len(), 2);
        assert_eq!(form.questions[0].options[1].label, "2");
    }

    #[test]
    fn the_envelope_matches_the_reference_wording() {
        let form = parse_form(&args(serde_json::json!([
            {
                "question": "Which library?",
                "header": "Lib",
                "options": [
                    { "label": "chrono (Recommended)", "description": "d", "preview": "use chrono;" },
                    { "label": "time", "description": "d" }
                ]
            },
            {
                "question": "Which features?",
                "header": "Feat",
                "multiSelect": true,
                "options": [
                    { "label": "serde", "description": "d" },
                    { "label": "clap", "description": "d" }
                ]
            },
            question("Which lint level?", "Lint", &["warn", "deny"]),
        ])))
        .expect("valid");

        let response = PromptResponse {
            answers: vec![
                PromptAnswer {
                    question_index: 0,
                    question: "Which library?".to_string(),
                    kind: PromptAnswerKind::Option,
                    answer: Some("chrono (Recommended)".to_string()),
                    selected: Vec::new(),
                    notes: Some("we pin it anyway".to_string()),
                    preview: Some("use chrono;".to_string()),
                },
                PromptAnswer {
                    question_index: 1,
                    question: "Which features?".to_string(),
                    kind: PromptAnswerKind::Multi,
                    answer: Some("plus a custom one".to_string()),
                    selected: vec!["serde".to_string(), "clap".to_string()],
                    notes: None,
                    preview: None,
                },
            ],
            cancelled: false,
            global_note: Some("ship it".to_string()),
        };
        assert_eq!(
            envelope(&form, &response),
            "User has answered your questions: \
\"Which library?\"=\"chrono (Recommended)\". selected preview: use chrono;. \
user notes: we pin it anyway. \
\"Which features?\"=\"serde, clap, plus a custom one\". \
unanswered: \"Which lint level?\". global note: ship it. \
You can now continue with the user's answers in mind."
        );

        // A custom single-select answer, and a multi-select with nothing
        // checked, keep the reference placeholders.
        let form = parse_form(&args(serde_json::json!([
            question("q?", "A", &["1", "2"]),
            {
                "question": "m?",
                "header": "M",
                "multiSelect": true,
                "options": [
                    { "label": "x", "description": "d" },
                    { "label": "y", "description": "d" }
                ]
            }
        ])))
        .expect("valid");
        let response = PromptResponse {
            answers: vec![
                PromptAnswer {
                    question_index: 0,
                    question: "q?".to_string(),
                    kind: PromptAnswerKind::Custom,
                    answer: Some("something else".to_string()),
                    selected: Vec::new(),
                    notes: None,
                    preview: None,
                },
                PromptAnswer {
                    question_index: 1,
                    question: "m?".to_string(),
                    kind: PromptAnswerKind::Multi,
                    answer: None,
                    selected: Vec::new(),
                    notes: None,
                    preview: None,
                },
            ],
            cancelled: false,
            global_note: None,
        };
        assert_eq!(
            envelope(&form, &response),
            "User has answered your questions: \"q?\"=\"something else\". \
\"m?\"=\"(no input)\". You can now continue with the user's answers in mind."
        );
    }

    #[test]
    fn declines_and_empty_submissions_share_one_sentence() {
        let form = parse_form(&args(serde_json::json!([question("q?", "A", &["1", "2"])])))
            .expect("valid");
        assert_eq!(
            envelope(&form, &PromptResponse::declined()),
            DECLINE_MESSAGE
        );
        // Submitting a form without answering anything is a decline in words;
        // a global note alone still counts as an answer.
        assert_eq!(
            envelope(
                &form,
                &PromptResponse {
                    answers: Vec::new(),
                    cancelled: false,
                    global_note: None,
                }
            ),
            DECLINE_MESSAGE
        );
        assert_eq!(
            envelope(
                &form,
                &PromptResponse {
                    answers: Vec::new(),
                    cancelled: false,
                    global_note: Some("go with your best judgement".to_string()),
                }
            ),
            "User has answered your questions: unanswered: \"q?\". \
global note: go with your best judgement. \
You can now continue with the user's answers in mind."
        );
    }

    #[test]
    fn the_tool_asks_the_front_end_and_returns_the_envelope() {
        let (plugin, stub) = plugin_with(Some(single_answer("q?", "two")));
        let text = plugin
            .run_tool(&call(
                TOOL_NAME,
                &args(serde_json::json!([question("q?", "A", &["one", "two"])])),
            ))
            .expect("answered");
        assert_eq!(
            text,
            "User has answered your questions: \"q?\"=\"two\". \
You can now continue with the user's answers in mind."
        );
        // The form reached the front-end with the authored options, and without
        // the custom row: the dialog appends that itself.
        let seen = stub.seen.lock().expect("stub lock");
        assert_eq!(seen.len(), 1);
        let labels: Vec<&str> = seen[0].questions[0]
            .options
            .iter()
            .map(|option| option.label.as_str())
            .collect();
        assert_eq!(labels, vec!["one", "two"]);
    }

    #[test]
    fn an_invalid_call_and_a_missing_ui_are_tool_errors() {
        let (plugin, stub) = plugin_with(None);
        let error = plugin
            .run_tool(&call(TOOL_NAME, &args(serde_json::json!([]))))
            .expect_err("no questions");
        assert_eq!(error.message(), "At least one question is required");

        // A valid call whose front-end has no dialog: the message tells the
        // model the user never saw the questions, so it must not read a
        // decline into it.
        let error = plugin
            .run_tool(&call(
                TOOL_NAME,
                &args(serde_json::json!([question("q?", "A", &["1", "2"])])),
            ))
            .expect_err("no ui");
        assert!(error.message().starts_with("UI not available"), "{error}");
        assert_eq!(stub.seen.lock().expect("stub lock").len(), 1);

        // A plugin that was never handed a service behaves the same way.
        let bare = AskUser::new();
        let error = bare
            .run_tool(&call(
                TOOL_NAME,
                &args(serde_json::json!([question("q?", "A", &["1", "2"])])),
            ))
            .expect_err("no ui");
        assert!(error.message().starts_with("UI not available"), "{error}");

        // A foreign tool name is a wiring bug, not a user-visible condition.
        let (plugin, _) = plugin_with(None);
        assert!(plugin.run_tool(&call("read", "{}")).is_err());
    }

    /// The tool blocks on the front-end: `ask` runs on the tool's own thread,
    /// so a dialog that answers from another thread must unblock it. This is
    /// the shape the TUI relies on.
    #[test]
    fn a_blocking_host_unblocks_the_tool_thread() {
        struct ChannelHost {
            tx: Mutex<Option<mpsc::Sender<PromptResponse>>>,
        }

        impl PromptHost for ChannelHost {
            fn ask(&self, _form: PromptForm) -> Option<PromptResponse> {
                let (tx, rx) = mpsc::channel();
                *self.tx.lock().expect("send lock") = Some(tx);
                rx.recv().ok()
            }
        }

        let host = Arc::new(ChannelHost {
            tx: Mutex::new(None),
        });
        let plugin = Arc::new(AskUser::new());
        plugin.set_prompt_host(host.clone());
        let caller = plugin.clone();
        let handle = std::thread::spawn(move || {
            caller.run_tool(&call(
                TOOL_NAME,
                &args(serde_json::json!([question("q?", "A", &["1", "2"])])),
            ))
        });
        // Answer once the tool has parked on the channel. Bounded: a tool that
        // gave up early must fail the test rather than spin the suite.
        let mut answered = false;
        for _ in 0..10_000 {
            let sender = host.tx.lock().expect("send lock").clone();
            if let Some(sender) = sender {
                sender.send(single_answer("q?", "one")).expect("answer");
                answered = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(answered, "the tool never parked on the prompt host");
        let text = handle.join().expect("thread").expect("answered");
        assert!(text.contains("\"q?\"=\"one\""), "{text}");
    }
}
