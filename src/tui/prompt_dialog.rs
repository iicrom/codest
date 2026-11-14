//! The question dialog: the TUI's [`PromptHost`] front-end.
//!
//! `ask_user_question` runs on a blocking tool thread and calls
//! [`PromptHost::ask`]; this module is the receiving half. [`TuiPromptHost`]
//! forwards the form to the UI thread through the app's event channel and
//! parks the tool thread on a one-shot reply channel — the exact shape the
//! plugin's own tests rely on — and the dialog sends the structured
//! [`PromptResponse`] back, which unblocks the tool.
//!
//! The dialog is fully modal: a pending question is an active tool call, so
//! it owns the whole keyboard until it is answered or declined. One question
//! is in focus at a time (the tab strip), showing the authored options plus
//! the runtime rows the contract reserves: `Type something.` on every
//! question and `Next` on a multi-select. Opening the custom row starts a
//! free-text edit; ↑↓ and Tab leave the edit and walk back to the options —
//! only `Enter` (keep) and `Esc` (abandon) answer inside it. `Esc` declines
//! the whole form —
//! which is a decline, not "nobody asked": the tool can tell the difference.

use std::sync::mpsc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::plugins::builtin::{CUSTOM_ROW, NEXT_ROW};
use crate::plugins::{PromptAnswer, PromptAnswerKind, PromptForm, PromptHost, PromptResponse};
use crate::tui::inline_edit::{LineEditor, Outcome};
use crate::tui::theme;
use crate::tui::wrap::wrap_str;

/// One pending question: the parsed form plus the one-shot channel the
/// dialog sends the answer through. Dropping `respond` without sending is
/// how a dialog closed from the outside (an aborted turn) turns into "UI
/// not available" for the parked tool.
pub struct PromptRequest {
    pub form: PromptForm,
    pub respond: mpsc::Sender<PromptResponse>,
}

/// The front-end's [`PromptHost`]: forwards every form to the UI thread and
/// blocks the calling tool thread until the dialog answers. The app installs
/// it once at startup (`PluginHost::install_prompt_host`); a headless
/// front-end installs nothing, and the tool falls back to its guidance
/// message instead.
pub struct TuiPromptHost {
    requests: tokio::sync::mpsc::UnboundedSender<PromptRequest>,
}

impl TuiPromptHost {
    pub fn new(requests: tokio::sync::mpsc::UnboundedSender<PromptRequest>) -> Self {
        Self { requests }
    }
}

impl PromptHost for TuiPromptHost {
    fn ask(&self, form: PromptForm) -> Option<PromptResponse> {
        let (respond, reply) = mpsc::channel();
        self.requests.send(PromptRequest { form, respond }).ok()?;
        // Park until the dialog sends. The sender dying without a send — a
        // dialog dropped by an aborted turn — reads as "UI not available".
        reply.recv().ok()
    }
}

/// One focusable row of the question in focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// An authored option, by index.
    Option(usize),
    /// The custom-answer row the dialog appends itself.
    Custom,
    /// The commit row of a multi-select question.
    Commit,
}

/// The open question dialog: one interruption's state, keys, and rendering.
///
/// Answers are kept as per-question draft state and assembled only on
/// submit, so navigating between tabs never loses what was entered. The
/// form always has at least one question (`parse_form` rejects an empty
/// one), so `self.active` indexes validly everywhere below.
pub struct PromptDialog {
    form: PromptForm,
    respond: mpsc::Sender<PromptResponse>,
    /// The question in focus.
    active: usize,
    /// Focused row per question: a plain index into that question's rows —
    /// the authored options, then the custom row, then `Next` for a
    /// multi-select.
    focus: Vec<usize>,
    /// The picked option per single-select question.
    choice: Vec<Option<usize>>,
    /// The checked options per question (multi-select only).
    checked: Vec<Vec<bool>>,
    /// The typed custom text per question, kept while navigating. For a
    /// single-select question it is the whole answer (`Custom`); for a
    /// multi-select one it is the addition to the checked labels.
    typed: Vec<Option<String>>,
    /// Whether each multi-select question was committed with `Next`.
    committed: Vec<bool>,
    /// Whether each question was skipped with `Next` on a blank commit row.
    /// A skipped question submits as absent — the envelope reports it
    /// `unanswered` — and counts as settled for the auto-submit.
    skipped: Vec<bool>,
    /// The open free-text editor, for the focused question's custom row.
    editor: Option<LineEditor>,
}

impl PromptDialog {
    pub fn new(request: PromptRequest) -> Self {
        let form = request.form;
        let count = form.questions.len();
        Self {
            focus: vec![0; count],
            choice: vec![None; count],
            checked: form
                .questions
                .iter()
                .map(|question| vec![false; question.options.len()])
                .collect(),
            typed: vec![None; count],
            committed: vec![false; count],
            skipped: vec![false; count],
            form,
            respond: request.respond,
            active: 0,
            editor: None,
        }
    }

    /// Feed one key. Returns `true` when the dialog closed and the response
    /// — answer or decline — has been sent back to the parked tool.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // The free-text editor owns the keyboard while it is open; `Esc`
        // there abandons the edit, not the questionnaire. The keys a one-line
        // editor has no use for — ↑↓ and Tab — leave the edit instead of
        // being swallowed: that is the way back to the authored options.
        if let Some(mut editor) = self.editor.take() {
            let step = match key.code {
                KeyCode::Up if !ctrl && !alt => Some(-1),
                KeyCode::Down if !ctrl && !alt => Some(1),
                KeyCode::Tab if !ctrl && !alt => {
                    self.switch_question(1);
                    return false;
                }
                KeyCode::BackTab if !ctrl && !alt => {
                    self.switch_question(-1);
                    return false;
                }
                _ => None,
            };
            if let Some(step) = step {
                // Walking off the row abandons the unsaved text, like an edit
                // a tab switch leaves behind; the saved text stays seeded.
                self.move_focus(step);
                return false;
            }
            match editor.handle_key(key) {
                None => {
                    self.editor = Some(editor);
                    return false;
                }
                Some(Outcome::Cancelled) => return false,
                Some(Outcome::Confirmed(text)) => {
                    // Typing a custom answer replaces a picked option on a
                    // single-select question; on a multi-select one it is
                    // the addition and `Next` still commits.
                    self.typed[self.active] = Some(text);
                    self.choice[self.active] = None;
                    if self.form.questions[self.active].multi_select {
                        return false;
                    }
                    return self.advance();
                }
            }
        }

        match key.code {
            KeyCode::Esc => {
                self.decline();
                return true;
            }
            // Ctrl+C declines rather than quits: the questionnaire must be
            // finished or abandoned before anything else runs anyway.
            KeyCode::Char('c') if ctrl => {
                self.decline();
                return true;
            }
            KeyCode::Up if !ctrl && !alt => self.move_focus(-1),
            KeyCode::Down if !ctrl && !alt => self.move_focus(1),
            KeyCode::Left | KeyCode::BackTab if !ctrl && !alt => self.switch_question(-1),
            KeyCode::Right | KeyCode::Tab if !ctrl && !alt => self.switch_question(1),
            KeyCode::Char(' ') if !ctrl && !alt => self.toggle_checked(),
            KeyCode::Enter if !ctrl && !alt => return self.confirm(),
            _ => {}
        }
        false
    }

    /// Insert pasted text — only into the open editor; with the editor
    /// closed a paste would type into an invisible composer, so it is
    /// dropped, like the full-frame screens do.
    pub fn paste(&mut self, text: &str) {
        if let Some(editor) = self.editor.as_mut() {
            editor.insert_paste(text);
        }
    }

    /// The height the dialog wants at `width`, for the inline pane's sizing
    /// while the dialog owns it.
    pub fn preferred_height(&self, width: u16) -> u16 {
        let inner = width.saturating_sub(2).max(1) as usize;
        let (content, _) = self.content_lines(inner);
        content.len() as u16 + 1 + 2 // content + hint row + borders
    }

    /// Render the dialog over `area`: a centered popup when the area leaves
    /// room for one, filling the area when it is pane-sized.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if area.width < 12 || area.height < 4 {
            return;
        }
        let width = area
            .width
            .saturating_sub(4)
            .clamp(28, 90)
            .min(area.width.max(1));
        let inner_width = width.saturating_sub(2).max(1) as usize;
        let (content, focus_line) = self.content_lines(inner_width);
        let hint_height = 1u16;
        let height = (content.len() as u16 + hint_height + 2).min(area.height);
        let rect = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );

        frame.render_widget(Clear, rect);
        let block = Block::bordered().title(" Questions ").title_style(
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        );
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        // The hint row is pinned to the bottom; the content above it windows
        // so the focused row stays visible.
        let visible = inner.height.saturating_sub(hint_height) as usize;
        let scroll = if visible > 0 && focus_line >= visible {
            focus_line + 1 - visible
        } else {
            0
        };
        let content_area = Rect::new(inner.x, inner.y, inner.width, visible as u16);
        frame.render_widget(
            Paragraph::new(content).scroll((scroll as u16, 0)),
            content_area,
        );
        let hint_area = Rect::new(inner.x, inner.y + visible as u16, inner.width, 1);
        frame.render_widget(
            Paragraph::new(self.hint()).style(Style::default().fg(theme::DIM)),
            hint_area,
        );
    }

    /// The hint row for the current state: while the custom editor is open,
    /// the keys it describes are the editor's, not the questionnaire's.
    fn hint(&self) -> &'static str {
        if self.editor.is_some() {
            EDIT_HINT
        } else {
            HINT
        }
    }

    /// The full content, wrapped to `width`, plus the content row the focus
    /// is rendered on (for the follow window).
    fn content_lines(&self, width: usize) -> (Vec<Line<'static>>, usize) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut focus_line = 0usize;
        let count = self.form.questions.len();
        let question = &self.form.questions[self.active];

        // The tab strip, from two questions up: the active tab is accented,
        // an answered one carries a check. An empty header is numbered — the
        // contract leaves the chip text to the front-end then.
        if count > 1 {
            let mut spans = Vec::with_capacity(count * 2);
            for index in 0..count {
                let tab = &self.form.questions[index];
                let label = if tab.header.is_empty() {
                    (index + 1).to_string()
                } else {
                    tab.header.clone()
                };
                let mark = if self.is_answered(index) { " ✓" } else { "" };
                let style = if index == self.active {
                    Style::default()
                        .fg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else if self.is_answered(index) {
                    Style::default().fg(theme::ASSISTANT)
                } else {
                    Style::default().fg(theme::DIM)
                };
                spans.push(Span::styled(format!(" {label}{mark} "), style));
                spans.push(Span::raw(" "));
            }
            lines.push(Line::from(spans));
            lines.push(Line::from(""));
        }

        for text in wrap_str(&question.question, width) {
            lines.push(Line::from(Span::styled(
                text,
                Style::default().add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(""));

        for index in 0..question.options.len() {
            let option = &question.options[index];
            let focused = self.focus[self.active] == index;
            let (glyph, style) = if question.multi_select {
                let checked = self.checked[self.active][index];
                (
                    if checked { "[✓]" } else { "[ ]" },
                    if focused {
                        Style::default()
                            .fg(theme::ACCENT)
                            .add_modifier(Modifier::BOLD)
                    } else if checked {
                        Style::default().fg(theme::ASSISTANT)
                    } else {
                        Style::default()
                    },
                )
            } else {
                let picked = self.choice[self.active] == Some(index);
                (
                    if picked { "(●)" } else { "( )" },
                    if focused {
                        Style::default()
                            .fg(theme::ACCENT)
                            .add_modifier(Modifier::BOLD)
                    } else if picked {
                        Style::default().fg(theme::ASSISTANT)
                    } else {
                        Style::default()
                    },
                )
            };
            if focused {
                focus_line = lines.len();
            }
            lines.push(Line::from(Span::styled(
                format!("{glyph} {}", option.label),
                style,
            )));
            if !option.description.is_empty() {
                for text in wrap_str(&option.description, width.saturating_sub(4)) {
                    lines.push(Line::from(Span::styled(
                        format!("    {text}"),
                        Style::default().fg(theme::DIM),
                    )));
                }
            }
        }

        // The custom row, appended by the dialog itself. While the editor is
        // open it shows the editing line instead of the reserved label.
        let custom_focused = self.focus[self.active] == question.options.len();
        if custom_focused {
            focus_line = lines.len();
        }
        let custom_style = if custom_focused {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::DIM)
        };
        let custom_text = match (&self.editor, &self.typed[self.active]) {
            (Some(editor), _) => editor.display(),
            (None, Some(text)) if !text.is_empty() => text.clone(),
            _ => CUSTOM_ROW.to_string(),
        };
        lines.push(Line::from(Span::styled(
            format!("✎ {custom_text}"),
            custom_style,
        )));

        // The commit row of a multi-select question.
        if question.multi_select {
            let commit_focused = self.focus[self.active] == question.options.len() + 1;
            if commit_focused {
                focus_line = lines.len();
            }
            let style = if commit_focused {
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::DIM)
            };
            lines.push(Line::from(Span::styled(format!("▸ {NEXT_ROW}"), style)));
        }

        // The focused option's preview, under the rows it belongs to.
        if let Row::Option(index) = self.row_kind(self.active, self.focus[self.active])
            && let Some(preview) = question.options[index]
                .preview
                .as_deref()
                .filter(|text| !text.is_empty())
        {
            let rule = format!("── preview {}", "─".repeat(width.saturating_sub(12)));
            lines.push(Line::from(Span::styled(
                rule,
                Style::default().fg(theme::DIM),
            )));
            for text in wrap_str(preview, width) {
                lines.push(Line::from(Span::styled(
                    text,
                    Style::default().fg(theme::SUBTLE),
                )));
            }
        }

        (lines, focus_line)
    }

    /// What the focused row of `question` is.
    fn row_kind(&self, question: usize, row: usize) -> Row {
        let options = self.form.questions[question].options.len();
        if row < options {
            Row::Option(row)
        } else if row == options {
            Row::Custom
        } else {
            Row::Commit
        }
    }

    /// Whether `question` carries an answer to submit.
    fn is_answered(&self, question: usize) -> bool {
        // A skip settles the question too: it submits as absent.
        if self.skipped[question] {
            return true;
        }
        let typed = self.typed[question]
            .as_deref()
            .is_some_and(|text| !text.is_empty());
        if self.form.questions[question].multi_select {
            // A multi-select question answers only through its commit row.
            self.committed[question]
                && (self.checked[question].iter().any(|checked| *checked) || typed)
        } else {
            self.choice[question].is_some() || typed
        }
    }

    fn rows_of(&self, question: usize) -> usize {
        let base = self.form.questions[question].options.len() + 1;
        if self.form.questions[question].multi_select {
            base + 1
        } else {
            base
        }
    }

    fn move_focus(&mut self, step: isize) {
        let rows = self.rows_of(self.active) as isize;
        let next = (self.focus[self.active] as isize + step).rem_euclid(rows);
        self.focus[self.active] = next as usize;
    }

    /// Switch the focused tab; an unsaved edit is abandoned, the saved text
    /// stays.
    fn switch_question(&mut self, step: isize) {
        let count = self.form.questions.len() as isize;
        let next = (self.active as isize + step).rem_euclid(count);
        self.active = next as usize;
        self.editor = None;
    }

    fn toggle_checked(&mut self) {
        if let Row::Option(index) = self.row_kind(self.active, self.focus[self.active])
            && self.form.questions[self.active].multi_select
        {
            let checked = &mut self.checked[self.active];
            checked[index] = !checked[index];
        }
    }

    /// Enter on the focused row: pick an option, open the free-text editor,
    /// or commit a multi-select question. Returns `true` when this closed
    /// the dialog by completing the form.
    fn confirm(&mut self) -> bool {
        match self.row_kind(self.active, self.focus[self.active]) {
            Row::Option(index) => {
                if self.form.questions[self.active].multi_select {
                    // Multi-select options are toggled; the commit row
                    // submits the set.
                    let checked = &mut self.checked[self.active];
                    checked[index] = !checked[index];
                    false
                } else {
                    self.choice[self.active] = Some(index);
                    self.typed[self.active] = None;
                    self.advance()
                }
            }
            Row::Custom => {
                self.editor = Some(LineEditor::with_text(
                    self.typed[self.active].clone().unwrap_or_default(),
                ));
                false
            }
            Row::Commit => {
                let answered = self.checked[self.active].iter().any(|checked| *checked)
                    || self.typed[self.active]
                        .as_deref()
                        .is_some_and(|text| !text.is_empty());
                if answered {
                    self.committed[self.active] = true;
                    self.advance()
                } else {
                    // `Next` on a blank question skips it: the answer comes
                    // back as `unanswered`, not as an empty answer.
                    self.skipped[self.active] = true;
                    self.advance()
                }
            }
        }
    }

    /// The focused question was just answered: submit when everything is
    /// answered, otherwise move to the next blank one.
    fn advance(&mut self) -> bool {
        if (0..self.form.questions.len()).all(|index| self.is_answered(index)) {
            self.submit();
            return true;
        }
        self.move_to_next_blank();
        false
    }

    /// Move to the next question that has no answer yet, staying put when
    /// only the current one is still blank.
    fn move_to_next_blank(&mut self) {
        let count = self.form.questions.len();
        for step in 1..count {
            let index = (self.active + step) % count;
            if !self.is_answered(index) {
                self.active = index;
                self.editor = None;
                return;
            }
        }
    }

    /// The user closed the form without answering: a decline, which the
    /// envelope renders as one canonical sentence.
    fn decline(&mut self) {
        let _ = self.respond.send(PromptResponse::declined());
    }

    /// Assemble the draft state into the response and unblock the tool.
    fn submit(&mut self) {
        let mut answers = Vec::new();
        for index in 0..self.form.questions.len() {
            let question = &self.form.questions[index];
            let typed = self.typed[index]
                .as_deref()
                .filter(|text| !text.is_empty())
                .map(str::to_string);
            let answer = if question.multi_select {
                let selected: Vec<String> = question
                    .options
                    .iter()
                    .enumerate()
                    .filter(|(option, _)| self.checked[index][*option])
                    .map(|(_, option)| option.label.clone())
                    .collect();
                if !self.committed[index] || (selected.is_empty() && typed.is_none()) {
                    continue;
                }
                PromptAnswer {
                    question_index: index,
                    question: question.question.clone(),
                    kind: PromptAnswerKind::Multi,
                    answer: typed,
                    selected,
                    notes: None,
                    preview: None,
                }
            } else if let Some(option) = self.choice[index].map(|picked| &question.options[picked])
            {
                PromptAnswer {
                    question_index: index,
                    question: question.question.clone(),
                    kind: PromptAnswerKind::Option,
                    answer: Some(option.label.clone()),
                    selected: Vec::new(),
                    notes: None,
                    preview: option.preview.clone(),
                }
            } else if let Some(text) = typed {
                PromptAnswer {
                    question_index: index,
                    question: question.question.clone(),
                    kind: PromptAnswerKind::Custom,
                    answer: Some(text),
                    selected: Vec::new(),
                    notes: None,
                    preview: None,
                }
            } else {
                continue;
            };
            answers.push(answer);
        }
        let _ = self.respond.send(PromptResponse {
            answers,
            cancelled: false,
            global_note: None,
        });
    }
}

const HINT: &str = "↑↓ move · Enter select · Space toggle (multi) · ←→ question · Esc declines all";
/// Shown while the custom-answer editor is open: the idle keys have
/// different meanings now, and the way back to the options has to be
/// spelled out — the editor otherwise reads as a trap.
const EDIT_HINT: &str = "type · Enter keep text · ↑↓ back to options · ←→ cursor · Esc abandons";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::PromptOption;
    use crate::plugins::PromptQuestion;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    /// A one-question form with two plain options.
    fn single_form() -> PromptForm {
        PromptForm::new(vec![PromptQuestion::new(
            "Which lint level?",
            "Lint",
            vec![
                PromptOption::new("warn", "default"),
                PromptOption::new("deny", "strict"),
            ],
        )])
    }

    /// A two-question form: a single-select with a preview, then a
    /// multi-select.
    fn mixed_form() -> PromptForm {
        PromptForm::new(vec![
            PromptQuestion::new(
                "Which library?",
                "Lib",
                vec![
                    PromptOption::new("chrono", "full").with_preview("use chrono;"),
                    PromptOption::new("time", "lean"),
                ],
            ),
            PromptQuestion::new(
                "Which features?",
                "Feat",
                vec![
                    PromptOption::new("serde", "serialization"),
                    PromptOption::new("clap", "cli"),
                ],
            )
            .multi(),
        ])
    }

    /// Open a dialog over `form` and return it with the channel its answer
    /// arrives on.
    fn open(form: PromptForm) -> (PromptDialog, mpsc::Receiver<PromptResponse>) {
        let (respond, reply) = mpsc::channel();
        let dialog = PromptDialog::new(PromptRequest { form, respond });
        (dialog, reply)
    }

    fn typed_answer(question_index: usize, question: &str, text: &str) -> PromptResponse {
        PromptResponse {
            answers: vec![PromptAnswer {
                question_index,
                question: question.to_string(),
                kind: PromptAnswerKind::Custom,
                answer: Some(text.to_string()),
                selected: Vec::new(),
                notes: None,
                preview: None,
            }],
            cancelled: false,
            global_note: None,
        }
    }

    #[test]
    fn picking_the_last_option_of_the_last_question_submits() {
        let (mut dialog, reply) = open(single_form());
        dialog.handle_key(key(KeyCode::Down)); // focus "deny"
        assert!(dialog.handle_key(key(KeyCode::Enter)), "dialog closes");
        let response = reply.recv().expect("answered");
        assert!(!response.cancelled, "submitted, not declined");
        assert_eq!(response.answers.len(), 1);
        let answer = &response.answers[0];
        assert_eq!(answer.kind, PromptAnswerKind::Option);
        assert_eq!(answer.answer.as_deref(), Some("deny"));
        assert_eq!(answer.question, "Which lint level?");
    }

    #[test]
    fn the_chosen_option_echoes_its_preview() {
        let (mut dialog, reply) = open(mixed_form());
        // "chrono" is focused and carries a preview; picking it moves to the
        // multi-select question, it does not submit yet.
        assert!(
            !dialog.handle_key(key(KeyCode::Enter)),
            "moves on to the next question"
        );
        // The multi-select question: check both options, commit.
        dialog.handle_key(key(KeyCode::Enter)); // toggle serde
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Enter)); // toggle clap
        dialog.handle_key(key(KeyCode::Down)); // custom row
        dialog.handle_key(key(KeyCode::Down)); // commit row
        assert!(dialog.handle_key(key(KeyCode::Enter)), "dialog closes");
        let response = reply.recv().expect("answered");
        assert!(!response.cancelled, "submitted, not declined");
        assert_eq!(response.answers.len(), 2);
        assert_eq!(response.answers[0].preview.as_deref(), Some("use chrono;"));
        assert_eq!(
            response.answers[1].selected,
            vec!["serde".to_string(), "clap".to_string()]
        );
    }

    #[test]
    fn typing_a_custom_answer_replaces_a_picked_option() {
        let (mut dialog, reply) = open(mixed_form());
        dialog.handle_key(key(KeyCode::Enter)); // pick "chrono"
        dialog.handle_key(key(KeyCode::Left)); // back to the first question
        dialog.handle_key(key(KeyCode::Down)); // focus "time"
        dialog.handle_key(key(KeyCode::Down)); // focus the custom row
        dialog.handle_key(key(KeyCode::Enter)); // open the editor
        for c in "use time crate".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
        assert!(
            !dialog.handle_key(key(KeyCode::Enter)),
            "confirming the text moves to the blank multi-select"
        );
        // The multi-select question is still blank, so the dialog moved on
        // instead of submitting; answer it to close.
        dialog.handle_key(key(KeyCode::Enter)); // toggle serde
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Down)); // Next
        assert!(dialog.handle_key(key(KeyCode::Enter)), "dialog closes");
        let response = reply.recv().expect("answered");
        assert!(!response.cancelled, "submitted, not declined");
        assert_eq!(
            response.answers[0].kind,
            PromptAnswerKind::Custom,
            "the typed text replaced the picked option"
        );
        assert_eq!(
            response.answers[0].answer.as_deref(),
            Some("use time crate")
        );
    }

    #[test]
    fn the_custom_row_takes_typed_text() {
        let (mut dialog, reply) = open(single_form());
        dialog.handle_key(key(KeyCode::Down)); // focus "deny"
        dialog.handle_key(key(KeyCode::Down)); // focus custom row
        assert!(dialog.editor.is_none(), "editor not open yet");
        dialog.handle_key(key(KeyCode::Enter)); // open the editor
        assert!(dialog.editor.is_some(), "editor opens");
        for c in "something else".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
        assert!(dialog.handle_key(key(KeyCode::Enter)), "dialog closes");
        let response = reply.recv().expect("answered");
        assert!(!response.cancelled, "submitted, not declined");
        assert_eq!(
            response.answers[0].kind,
            PromptAnswerKind::Custom,
            "typed text is a custom answer"
        );
        assert_eq!(
            response.answers[0].answer.as_deref(),
            Some("something else")
        );
    }

    #[test]
    fn a_multi_select_needs_its_commit_row() {
        let (mut dialog, reply) = open(mixed_form());
        // Answer the single-select question first.
        dialog.handle_key(key(KeyCode::Enter)); // pick "chrono"
        // Check one option, leave the rest, commit with `Next`.
        dialog.handle_key(key(KeyCode::Enter)); // toggle serde
        dialog.handle_key(key(KeyCode::Down)); // clap
        dialog.handle_key(key(KeyCode::Down)); // custom
        dialog.handle_key(key(KeyCode::Down)); // Next
        assert!(dialog.handle_key(key(KeyCode::Enter)), "dialog closes");
        let response = reply.recv().expect("answered");
        assert!(!response.cancelled, "submitted, not declined");
        assert_eq!(response.answers.len(), 2);
        assert_eq!(
            response.answers[1].selected,
            vec!["serde".to_string()],
            "uncommitted questions still submit their checks once committed"
        );
    }

    #[test]
    fn next_on_a_blank_question_skips_it() {
        let (mut dialog, reply) = open(mixed_form());
        dialog.handle_key(key(KeyCode::Enter)); // pick "chrono"
        // Immediately commit the untouched multi-select: it is skipped.
        dialog.handle_key(key(KeyCode::Down)); // clap
        dialog.handle_key(key(KeyCode::Down)); // custom
        dialog.handle_key(key(KeyCode::Down)); // Next
        assert!(dialog.handle_key(key(KeyCode::Enter)), "dialog closes");
        let response = reply.recv().expect("answered");
        assert!(!response.cancelled, "submitted, not declined");
        assert_eq!(
            response.answers.len(),
            1,
            "a skipped question is absent, not empty"
        );
    }

    #[test]
    fn escape_declines_the_whole_form() {
        let (mut dialog, reply) = open(mixed_form());
        dialog.handle_key(key(KeyCode::Enter)); // answer one question
        assert!(dialog.handle_key(key(KeyCode::Esc)), "dialog closes");
        let response = reply.recv().expect("declined");
        assert!(response.cancelled);
        assert!(response.answers.is_empty());
    }

    #[test]
    fn ctrl_c_declines_instead_of_quitting() {
        let (mut dialog, reply) = open(single_form());
        assert!(dialog.handle_key(ctrl(KeyCode::Char('c'))), "dialog closes");
        assert!(reply.recv().expect("declined").cancelled);
    }

    #[test]
    fn navigating_back_replaces_the_answer() {
        let (mut dialog, reply) = open(mixed_form());
        dialog.handle_key(key(KeyCode::Enter)); // pick "chrono"
        dialog.handle_key(key(KeyCode::Left)); // back to the first question
        dialog.handle_key(key(KeyCode::Down)); // focus "time"
        assert!(
            !dialog.handle_key(key(KeyCode::Enter)),
            "re-answering moves on instead of submitting"
        );
        // Finish the multi-select question to close.
        dialog.handle_key(key(KeyCode::Enter)); // toggle serde
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Down)); // Next
        assert!(dialog.handle_key(key(KeyCode::Enter)), "dialog closes");
        let response = reply.recv().expect("answered");
        assert!(!response.cancelled, "submitted, not declined");
        assert_eq!(response.answers[0].answer.as_deref(), Some("time"));
    }

    #[test]
    fn the_editor_swallows_its_own_escape() {
        let (mut dialog, reply) = open(single_form());
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Enter)); // open the editor
        assert!(
            !dialog.handle_key(key(KeyCode::Esc)),
            "Esc closes the editor, not the dialog"
        );
        assert!(dialog.editor.is_none());
        // The questionnaire is still open: Esc now declines it.
        assert!(dialog.handle_key(key(KeyCode::Esc)), "dialog closes");
        assert!(reply.recv().expect("declined").cancelled);
    }

    #[test]
    fn the_arrow_keys_walk_out_of_the_editor_to_the_options() {
        let (mut dialog, reply) = open(single_form());
        dialog.handle_key(key(KeyCode::Down)); // focus "deny"
        dialog.handle_key(key(KeyCode::Down)); // focus the custom row
        dialog.handle_key(key(KeyCode::Enter)); // open the editor
        dialog.handle_key(key(KeyCode::Char('x')));
        // ↑ leaves the edit and lands back on the authored options instead
        // of being swallowed by the one-line editor.
        assert!(
            !dialog.handle_key(key(KeyCode::Up)),
            "the dialog stays open"
        );
        assert!(dialog.editor.is_none(), "the edit is closed");
        assert!(
            dialog.handle_key(key(KeyCode::Enter)),
            "focus is on `deny`, so Enter submits"
        );
        let response = reply.recv().expect("answered");
        assert_eq!(
            response.answers[0].kind,
            PromptAnswerKind::Option,
            "the unsaved draft is abandoned: the picked option submits"
        );
        assert_eq!(response.answers[0].answer.as_deref(), Some("deny"));
    }

    #[test]
    fn down_leaves_the_editor_onto_the_next_row() {
        let (mut dialog, reply) = open(mixed_form());
        dialog.handle_key(key(KeyCode::Enter)); // pick "chrono", next question
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Down)); // the multi-select custom row
        dialog.handle_key(key(KeyCode::Enter)); // open the editor
        dialog.handle_key(key(KeyCode::Char('x')));
        dialog.handle_key(key(KeyCode::Down)); // out of the edit, onto `Next`
        assert!(dialog.editor.is_none(), "the edit is closed");
        // The commit row is focused: Enter skips the blank question.
        assert!(dialog.handle_key(key(KeyCode::Enter)), "dialog closes");
        let response = reply.recv().expect("answered");
        assert_eq!(
            response.answers.len(),
            1,
            "the skipped multi-select is absent"
        );
    }

    #[test]
    fn tab_switches_questions_out_of_the_editor() {
        let (mut dialog, _reply) = open(mixed_form());
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Down)); // the custom row
        dialog.handle_key(key(KeyCode::Enter)); // open the editor
        dialog.handle_key(key(KeyCode::Tab));
        assert!(dialog.editor.is_none(), "the edit is closed");
        assert_eq!(dialog.active, 1, "Tab moved to the next question");
        dialog.handle_key(key(KeyCode::BackTab));
        assert_eq!(dialog.active, 0, "BackTab moved back");
    }

    #[test]
    fn the_hint_row_follows_the_editor() {
        let (mut dialog, _reply) = open(single_form());
        assert_eq!(dialog.hint(), HINT, "the idle keys are described");
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Enter)); // open the editor
        assert_eq!(dialog.hint(), EDIT_HINT, "the edit keys are described");
        dialog.handle_key(key(KeyCode::Up)); // leave the edit
        assert_eq!(dialog.hint(), HINT, "the idle keys are back");
    }

    #[test]
    fn paste_reaches_only_the_open_editor() {
        let (mut dialog, _reply) = open(single_form());
        dialog.paste("no editor yet");
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.paste("pasted\n multiline\taudio");
        let editor = dialog.editor.as_ref().expect("editor open");
        // Newlines and tabs flatten to spaces, like the settings editor's
        // paste; the space before the newline is kept.
        assert_eq!(editor.text(), "pasted  multiline audio");
    }

    #[test]
    fn the_tab_mark_follows_the_answers() {
        let (mut dialog, _reply) = open(mixed_form());
        let (lines, _) = dialog.content_lines(60);
        let tabs = lines[0]
            .spans
            .iter()
            .map(|span| span.content.clone())
            .collect::<Vec<_>>();
        assert!(tabs.join("").contains("Lib "), "{tabs:?}");
        assert!(!tabs.join("").contains("✓"), "nothing answered yet");

        dialog.handle_key(key(KeyCode::Enter)); // pick "chrono"
        let (lines, _) = dialog.content_lines(60);
        let tabs = lines[0]
            .spans
            .iter()
            .map(|span| span.content.clone())
            .collect::<Vec<_>>();
        assert!(tabs.join("").contains("Lib ✓"), "{tabs:?}");
    }

    #[test]
    fn an_option_description_renders_indented_and_dim() {
        let (dialog, _reply) = open(mixed_form());
        let (lines, _) = dialog.content_lines(60);
        let option = lines
            .iter()
            .position(|line| line.spans.iter().any(|span| span.content.contains("chrono")))
            .expect("the focused option renders");
        let description = &lines[option + 1];
        let text = description
            .spans
            .iter()
            .map(|span| span.content.clone())
            .collect::<String>();
        // The trade-off text sits indented under the label it belongs to, in
        // the dim style — it must not read as a second option.
        assert_eq!(text, "    full", "{text}");
        assert_eq!(description.spans[0].style.fg, Some(theme::DIM));
    }

    #[test]
    fn an_empty_header_is_numbered() {
        let form = PromptForm::new(vec![
            PromptQuestion::new(
                "First?",
                "",
                vec![PromptOption::new("a", "d"), PromptOption::new("b", "d")],
            ),
            PromptQuestion::new(
                "Second?",
                "",
                vec![PromptOption::new("a", "d"), PromptOption::new("b", "d")],
            ),
        ]);
        let (dialog, _reply) = open(form);
        let (lines, _) = dialog.content_lines(60);
        let tabs = lines[0]
            .spans
            .iter()
            .map(|span| span.content.clone())
            .collect::<Vec<_>>();
        let joined = tabs.join("");
        assert!(joined.contains(" 1 "), "{joined:?}");
        assert!(joined.contains(" 2 "), "{joined:?}");
    }

    #[test]
    fn the_host_blocks_until_the_dialog_answers() {
        let (requests, mut inbox) = tokio::sync::mpsc::unbounded_channel::<PromptRequest>();
        let host = TuiPromptHost::new(requests);

        let handle = std::thread::spawn(move || host.ask(single_form()));
        // Wait for the form to arrive, then answer from "the UI thread".
        let mut request = None;
        for _ in 0..10_000 {
            match inbox.try_recv() {
                Ok(pending) => {
                    request = Some(pending);
                    break;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => std::thread::yield_now(),
                Err(_) => break,
            }
        }
        let request = request.expect("the tool parked on the host");
        request
            .respond
            .send(typed_answer(0, "Which lint level?", "warn"))
            .expect("answer");
        let response = handle.join().expect("thread").expect("answered");
        assert_eq!(response.answers[0].answer.as_deref(), Some("warn"));
    }

    #[test]
    fn a_host_without_a_ui_reports_unavailable() {
        let (requests, inbox) = tokio::sync::mpsc::unbounded_channel::<PromptRequest>();
        drop(inbox);
        let host = TuiPromptHost::new(requests);
        assert!(host.ask(single_form()).is_none(), "no UI, no answer");
    }

    #[test]
    fn dropping_the_dialog_unblocks_the_tool_with_none() {
        let (requests, mut inbox) = tokio::sync::mpsc::unbounded_channel::<PromptRequest>();
        let host = TuiPromptHost::new(requests);
        let handle = std::thread::spawn(move || host.ask(single_form()));
        let mut request = None;
        for _ in 0..10_000 {
            match inbox.try_recv() {
                Ok(pending) => {
                    request = Some(pending);
                    break;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => std::thread::yield_now(),
                Err(_) => break,
            }
        }
        // The dialog is closed from the outside (an aborted turn): the
        // request — and with it the reply sender — is dropped.
        drop(request.expect("the tool parked on the host"));
        assert!(handle.join().expect("thread").is_none());
    }
}
