//! Interactive prompts: a form put to the user, and the answer that comes back.
//!
//! A plugin cannot render anything — the contract carries no terminal code — so
//! a tool that needs a decision from the human asks the *front-end* through
//! [`PromptHost`]: the form is shown, the calling [`Plugin::run_tool`] blocks
//! until the user answers or declines, and the structured [`PromptResponse`]
//! comes back. The host installs the service once at startup
//! ([`Plugin::set_prompt_host`]), which keeps a prompt-capable tool a plugin
//! like any other: no terminal dependency, and no host API per idea.
//!
//! ```text
//! run_tool ──ask(form)──▶ PromptHost ──form──▶ front-end dialog
//!    ▲                       │                      │
//!    └──── PromptResponse ───┴────── answer ────────┘
//! ```
//!
//! "Prompt" here is the structured-question sense, not the system prompt. A
//! [`PromptForm`] is **one interruption**: several questions shown together,
//! each with authored options and one free-text row, which is what keeps a
//! model from asking four things in four separate turns.
//!
//! Declining is distinct from nobody being asked: [`PromptResponse::cancelled`]
//! means the user closed the form, while [`PromptHost::ask`] returning `None`
//! means this front-end could not show it at all. A tool must be able to tell
//! "the user said no" from "the user never saw the question".
//!
//! [`Plugin::set_prompt_host`]: crate::plugin::Plugin::set_prompt_host
//! [`Plugin::run_tool`]: crate::plugin::Plugin::run_tool

/// One authored choice of a [`PromptQuestion`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromptOption {
    /// The display text the user picks; short (a few words).
    pub label: String,
    /// What the choice means or costs — one line under the label.
    #[serde(default)]
    pub description: String,
    /// Markdown shown beside the option list while this option is focused
    /// (a mockup, a snippet, a diagram). Absent for plain preference choices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

impl PromptOption {
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
            preview: None,
        }
    }

    /// Attach the option's preview markdown.
    pub fn with_preview(mut self, preview: impl Into<String>) -> Self {
        self.preview = Some(preview.into());
        self
    }
}

/// One question of a [`PromptForm`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromptQuestion {
    /// The complete question, shown above the options.
    pub question: String,
    /// A short chip naming the question — the dialog's tab label. May be empty,
    /// in which case the front-end numbers the tab instead.
    #[serde(default)]
    pub header: String,
    /// The authored choices, in the order they should be shown. The free-text
    /// row every question gets is *not* an option: the front-end appends it.
    pub options: Vec<PromptOption>,
    /// Several answers are valid: the user checks options instead of picking
    /// one, and the custom row becomes an addition rather than a replacement.
    #[serde(default)]
    pub multi_select: bool,
}

impl PromptQuestion {
    pub fn new(
        question: impl Into<String>,
        header: impl Into<String>,
        options: Vec<PromptOption>,
    ) -> Self {
        Self {
            question: question.into(),
            header: header.into(),
            options,
            multi_select: false,
        }
    }

    /// A question that accepts several checked options.
    pub fn multi(mut self) -> Self {
        self.multi_select = true;
        self
    }
}

/// A form: the questions one interruption puts to the user.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromptForm {
    pub questions: Vec<PromptQuestion>,
}

impl PromptForm {
    pub fn new(questions: Vec<PromptQuestion>) -> Self {
        Self { questions }
    }
}

/// How one answer was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAnswerKind {
    /// The user picked one of the authored options.
    Option,
    /// The user typed free text on a single-select question.
    Custom,
    /// The user committed a multi-select question: checked labels, an optional
    /// typed addition, or both.
    Multi,
}

/// One answered question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptAnswer {
    /// Index into [`PromptForm::questions`].
    pub question_index: usize,
    /// The question text, echoed so an answer stands on its own.
    pub question: String,
    pub kind: PromptAnswerKind,
    /// The chosen label ([`PromptAnswerKind::Option`]), the typed text
    /// ([`PromptAnswerKind::Custom`]), or the typed addition to a multi-select
    /// answer (may be `None`).
    pub answer: Option<String>,
    /// The checked labels of a multi-select answer, in option order.
    pub selected: Vec<String>,
    /// The note the user attached to this answer, if any.
    pub notes: Option<String>,
    /// The chosen option's preview markdown, echoed when it carried one.
    pub preview: Option<String>,
}

/// What the user did with a form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptResponse {
    /// The answers, in question order. A question the user left blank is
    /// simply absent — a partial answer is not a decline.
    pub answers: Vec<PromptAnswer>,
    /// The user declined the whole form.
    pub cancelled: bool,
    /// The note attached to the form as a whole (the dialog's Submit tab).
    pub global_note: Option<String>,
}

impl PromptResponse {
    /// A decline: the user closed the form without answering.
    pub fn declined() -> Self {
        Self {
            answers: Vec::new(),
            cancelled: true,
            global_note: None,
        }
    }

    /// The answer to `question_index`, when the user gave one.
    pub fn answer(&self, question_index: usize) -> Option<&PromptAnswer> {
        self.answers
            .iter()
            .find(|answer| answer.question_index == question_index)
    }
}

/// The front-end's prompt service.
///
/// Implemented by the front-end, installed once through
/// [`Plugin::set_prompt_host`], and called from a tool on a blocking thread —
/// so [`PromptHost::ask`] may block for as long as the user takes, and must
/// return promptly once the dialog closes.
///
/// [`Plugin::set_prompt_host`]: crate::plugin::Plugin::set_prompt_host
pub trait PromptHost: Send + Sync {
    /// Show `form` and block until it is answered, declined, or abandoned.
    ///
    /// `None` means this front-end could not put the question to the user at
    /// all (a headless run, a shut-down UI) — never "the user declined", which
    /// is [`PromptResponse::cancelled`].
    fn ask(&self, form: PromptForm) -> Option<PromptResponse>;
}
