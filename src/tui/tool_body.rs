//! Structured tool-card bodies: diff excerpts and highlighted file content.
//!
//! Most tool cards show the tool's output text, and that is the right thing to
//! show. The file tools know more than their result string does: `write` and
//! `edit` know exactly which lines they added and removed, and `read` returns a
//! numbered window of a source file. Rendering those as a dump of the result
//! string buries the one fact the card exists to communicate — what changed.
//!
//! So they render the way cordis-harness-tui renders them: a diff excerpt with
//! `-`/`+` rows over a dim `(+N -M)` stat, or the file's own lines under a dim
//! number column, both syntax-highlighted by the file's extension. The `+`/`-`
//! tint rides on the marker and on whatever the syntax palette leaves plain,
//! so an added line reads green at a glance while its keywords and strings keep
//! their own colors.
//!
//! This module builds rows; [`chat`](crate::tui::chat) owns the card gutter, the
//! fold cap, and wrapping, so a body line is laid out exactly like any other
//! card row.

use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Span;

use crate::tui::highlight;
use crate::tui::theme;

/// The marker column a diff row reserves (`+ ` / `- `), so the code of every
/// row starts at the same column whether the row carries a marker or not.
const MARK: &str = "  ";

/// How a body row is toned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// A `+` row: a line this call added.
    Add,
    /// A `-` row: a line this call removed.
    Del,
    /// Unchanged content, a line number column, or a caption.
    Plain,
}

impl Tone {
    fn color(self) -> Option<Color> {
        match self {
            Tone::Add => Some(theme::DIFF_ADD),
            Tone::Del => Some(theme::DIFF_DEL),
            Tone::Plain => None,
        }
    }

    /// The style of the row's marker and of the tokens the syntax palette
    /// leaves uncolored.
    pub fn style(self) -> Style {
        match self.color() {
            Some(color) => Style::default().fg(color),
            None => Style::default().fg(theme::DIM),
        }
    }
}

/// One logical body row: its tone, its own leading column, and its text.
pub struct Row {
    pub tone: Tone,
    /// The fixed column before the text — `+ `, `- `, or a padded line number.
    /// Empty rows start at the card gutter.
    pub lead: String,
    pub spans: Vec<Span<'static>>,
}

/// A card body: rows plus the wrapping rule they need.
pub struct Body {
    pub rows: Vec<Row>,
    /// Code: wrap at the column budget instead of reflowing at spaces, so a
    /// wrapped row still re-joins into the line it came from.
    pub code: bool,
    /// Logical rows the row budget hid (`0` → the body is complete, marker
    /// rows included). Counted while the body is built, so a huge `write`
    /// never materializes rows the card cannot show.
    pub hidden: usize,
}

/// How many rows an expanded card shows before and after its omitted marker.
/// Unfolding every card with Ctrl+O used to materialize every row of every
/// body — a 2,000-line `read` window, or a 40,000-line `write` — per
/// rebuild. Head and tail keep the shape of the change visible at both ends
/// while bounding the work; codex truncates its live command output the same
/// way (`exec_cell/live_output.rs`, head+tail 50).
pub const EXPANDED_HEAD_ROWS: usize = 200;
pub const EXPANDED_TAIL_ROWS: usize = 50;

/// The row budget a card body is built under.
pub enum Budget {
    /// Folded card: at most `usize` rows from the top of the body; the rest
    /// are counted into [`Body::hidden`] and surface as the fold hint.
    Head(usize),
    /// Expanded card: head + tail rows around a dim `… +N lines omitted`
    /// marker, so both ends of the output stay visible. [`Body::hidden`] is
    /// zero — the marker row carries the count itself.
    HeadTail {
        head: usize,
        tail: usize,
    },
}

/// The structured body for `name`, or `None` when this tool's output is just
/// text and the card should show the result string.
///
/// `arguments` is the raw JSON the model sent; a body is only built when the
/// arguments actually carry what the body needs (`content`, `old_string` /
/// `new_string`), so a malformed call still falls back to the plain card.
///
/// `budget` bounds how many rows are materialized; see [`Budget`]. The stat
/// row and the numbered `read` rows both count against a [`Budget::Head`],
/// the way cordis caps a whole diff body at
/// [`chat::DIFF_BODY_MAX_LINES`](crate::tui::chat).
pub fn build(name: &str, arguments: &str, result: &str, budget: Budget) -> Option<Body> {
    let parsed: serde_json::Value = serde_json::from_str(arguments.trim()).ok()?;
    build_parsed(name, Some(&parsed), result, budget)
}

/// [`build`] over an arguments value the caller already parsed — the card
/// shares one parse between its header summary and this body, so a huge
/// `write` payload is only deserialized once per rebuild.
pub fn build_parsed(
    name: &str,
    parsed: Option<&serde_json::Value>,
    result: &str,
    budget: Budget,
) -> Option<Body> {
    let parsed = parsed?;
    let lang = parsed
        .get("path")
        .and_then(serde_json::Value::as_str)
        .and_then(language_of);
    match name {
        "write" => {
            let content = parsed.get("content").and_then(serde_json::Value::as_str)?;
            Some(diff("", content, lang.as_deref(), caption(result), budget))
        }
        "edit" => {
            let old = parsed
                .get("old_string")
                .and_then(serde_json::Value::as_str)?;
            let new = parsed
                .get("new_string")
                .and_then(serde_json::Value::as_str)?;
            Some(diff(
                old,
                new,
                lang.as_deref(),
                caption(result),
                budget,
            ))
        }
        "read" => Some(read(result, lang.as_deref(), budget)),
        _ => None,
    }
}

/// The tool's one-line verdict, as the diff's stat row carries it.
///
/// The result string repeats what the card already says — the path is in the
/// header, and `(N bytes)` says nothing about the change — so those are cut
/// away, leaving the part that is news: `created`, `overwrote`,
/// `replaced 2 occurrences`. `None` when nothing is left.
fn caption(result: &str) -> Option<String> {
    let first = result.lines().next()?.trim();
    if first.is_empty() {
        return None;
    }
    // `created a/b.rs (12 bytes)` loses the parenthetical; `replaced 1
    // occurrence in /abs/a.rs` loses everything from ` in ` on.
    let head = first.split_once(" (").map_or(first, |(head, _)| head);
    let head = head.split_once(" in ").map_or(head, |(head, _)| head);
    let mut words = head.split_whitespace();
    let mut caption = words.next()?.to_string();
    // Keep a following count word (`1`, `2`) and its noun, but never the path:
    // the header shows that, and it is the longest thing in the line.
    for word in words.take(2) {
        if looks_like_path(word) {
            break;
        }
        caption.push(' ');
        caption.push_str(word);
    }
    Some(caption)
}

/// Whether a result-string word is a path rather than a count or a verdict.
fn looks_like_path(word: &str) -> bool {
    word.contains(['/', '\\']) || (word.contains('.') && !word.starts_with('.'))
}

/// A `-`/`+` diff excerpt: the removed lines, then the added ones, under a dim
/// stat row. `write` passes no removed lines — it does not know the old
/// content, and inventing one would be a lie about what changed.
///
/// `removed` is the raw old text: its lines are materialized lazily (counted
/// first, only the shown prefix built), so a giant replacement never
/// allocates every line it hides.
///
/// The stat row is `(+N -M)`, followed by the tool's own one-line verdict when
/// `caption` supplies one (`created`, `overwrote`, the replacement count): the
/// result string still carries facts a diff cannot, so the card keeps them
/// instead of discarding the whole line.
fn diff(
    removed: &str,
    added: &str,
    lang: Option<&str>,
    caption: Option<String>,
    budget: Budget,
) -> Body {
    // The stat states the WHOLE change, so it counts every line even when the
    // rows below it are capped.
    let removed_total = count_lines(removed);
    let added_total = count_lines(added);
    match budget {
        Budget::Head(max) => {
            let mut rows = Vec::with_capacity(max.min(removed_total + added_total + 1));
            let mut hidden = removed_total + added_total;
            if max == 0 {
                return Body {
                    rows,
                    code: true,
                    hidden,
                };
            }
            let stat = stat_span(added_total, removed_total, caption);
            rows.push(Row {
                tone: Tone::Plain,
                lead: MARK.to_string(),
                spans: vec![stat],
            });
            let mut left = max - 1;
            let removed_rows = removed_total.min(left);
            hidden -= removed_rows;
            left -= removed_rows;
            push_side(&mut rows, &lines_head(removed, removed_rows), Tone::Del, '-', lang);

            let kept = lines_head(added, left);
            hidden -= kept.len();
            push_side(&mut rows, &kept, Tone::Add, '+', lang);
            Body {
                rows,
                code: true,
                hidden,
            }
        }
        Budget::HeadTail { head, tail } => {
            // The stat row spends one slot of the head budget; the removed
            // side spends the rest before the added side starts. A removed
            // side longer than that is capped without a marker — the stat's
            // `(-N)` total stays complete, and the head rows read as the
            // start of the delete.
            let head_budget = head.saturating_sub(1).max(1);
            let removed_rows = removed_total.min(head_budget);
            let mut rows = Vec::with_capacity(head + tail + 2);
            let stat = stat_span(added_total, removed_total, caption);
            rows.push(Row {
                tone: Tone::Plain,
                lead: MARK.to_string(),
                spans: vec![stat],
            });
            push_side(&mut rows, &lines_head(removed, removed_rows), Tone::Del, '-', lang);

            let added_head = head_budget - removed_rows;
            let kept_head = lines_head(added, added_head);
            push_side(&mut rows, &kept_head, Tone::Add, '+', lang);
            let remaining = added_total - kept_head.len();
            if remaining > tail {
                // Real omission: the marker stands for the middle, and the
                // tail rows are the true end of the change.
                rows.push(omitted_row(MARK, remaining - tail));
                push_side(&mut rows, &lines_tail(added, tail), Tone::Add, '+', lang);
            } else if remaining > 0 {
                // Everything left fits the tail budget: no marker, just the
                // rows after the head.
                push_side(
                    &mut rows,
                    &lines_tail(added, remaining),
                    Tone::Add,
                    '+',
                    lang,
                );
            }
            Body {
                rows,
                code: true,
                hidden: 0,
            }
        }
    }
}

/// The diff's stat span, `(+N -M)` plus the tool's verdict when it has one.
fn stat_span(added: usize, removed: usize, caption: Option<String>) -> Span<'static> {
    let mut stat = Span::styled(
        format!("(+{added} -{removed})"),
        Style::default().fg(theme::DIM),
    );
    if let Some(caption) = caption {
        stat.content = format!("{} {caption}", stat.content).into();
    }
    stat
}

/// The dim marker standing for rows a head+tail budget hid: the count, no
/// key hint — an expanded card is already expanded.
fn omitted_row(lead: &str, omitted: usize) -> Row {
    Row {
        tone: Tone::Plain,
        lead: lead.to_string(),
        spans: vec![Span::styled(
            format!("… +{omitted} lines omitted"),
            Style::default().fg(theme::DIM),
        )],
    }
}

/// One side of a diff: `lines` under their marker, highlighted as one block so
/// a multi-line string or comment on that side parses across rows.
fn push_side(rows: &mut Vec<Row>, lines: &[String], tone: Tone, mark: char, lang: Option<&str>) {
    let segments = lang.filter(|_| !lines.is_empty()).map(|lang| {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        highlight::block(lang, &refs)
    });
    for (index, line) in lines.iter().enumerate() {
        let segment = segments.as_ref().and_then(|block| block.get(index));
        rows.push(Row {
            tone,
            lead: format!("{mark} "),
            spans: styled(line, segment.map(Vec::as_slice), tone),
        });
    }
}

/// A `read` result as a file body: the numbered window under a dim number
/// column, with any non-numbered note (the `[N lines total]` caption, a
/// truncation notice) kept below it as dim text.
fn read(result: &str, lang: Option<&str>, budget: Budget) -> Body {
    match budget {
        Budget::Head(max) => read_window(result, lang, Some(max)),
        // Head+tail needs the total row count to place the marker, so the
        // window walk runs twice: once counting, once materializing each end.
        Budget::HeadTail { head, tail } => {
            let total = count_numbered_rows(result);
            if total <= head + tail {
                return read_window(result, lang, None);
            }
            let head_rows = read_rows(result, lang, 0..head);
            let tail_rows = read_rows(result, lang, total.saturating_sub(tail)..total);
            let mut rows = head_rows;
            rows.push(omitted_row("", total - head - tail));
            rows.extend(tail_rows);
            Body {
                rows,
                code: true,
                hidden: 0,
            }
        }
    }
}

/// Content rows of a `read` result: numbered file lines and notes, blanks
/// excluded.
enum ReadRow {
    Numbered(String, String),
    Note(String),
}

/// How many rows [`read_rows`] would render for `range`'s window — every
/// numbered line and every non-blank note in `result`.
fn count_numbered_rows(result: &str) -> usize {
    result
        .lines()
        .filter(|line| {
            split_numbered(line).is_some() || !line.trim().is_empty()
        })
        .count()
}

/// The whole-window walk: at most `max` rows from the top (`None` = all of
/// them), the rest counted into [`Body::hidden`] instead of materialized.
fn read_window(result: &str, lang: Option<&str>, max: Option<usize>) -> Body {
    let max = max.unwrap_or(usize::MAX);
    let mut numbered: Vec<(String, String)> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut rows_seen = 0usize;
    let mut hidden = 0usize;
    for line in result.lines() {
        let row = match split_numbered(line) {
            Some((number, text)) => Some(ReadRow::Numbered(number.to_string(), text.to_string())),
            None if !line.trim().is_empty() => Some(ReadRow::Note(line.trim_end().to_string())),
            // A blank row is layout, not content: it never becomes a body row.
            None => None,
        };
        let Some(row) = row else { continue };
        if rows_seen >= max {
            hidden += 1;
            continue;
        }
        rows_seen += 1;
        match row {
            ReadRow::Numbered(number, text) => numbered.push((number, text)),
            ReadRow::Note(note) => notes.push(note),
        }
    }

    let width = numbered
        .iter()
        .map(|(number, _)| number.len())
        .max()
        .unwrap_or(0);
    let segments = lang.filter(|_| !numbered.is_empty()).map(|lang| {
        let refs: Vec<&str> = numbered.iter().map(|(_, text)| text.as_str()).collect();
        highlight::block(lang, &refs)
    });

    let mut rows = Vec::with_capacity(numbered.len() + notes.len());
    for (index, (number, text)) in numbered.iter().enumerate() {
        let segment = segments.as_ref().and_then(|block| block.get(index));
        rows.push(Row {
            tone: Tone::Plain,
            lead: format!("{number:>width$} "),
            spans: styled(text, segment.map(Vec::as_slice), Tone::Plain),
        });
    }
    for note in notes {
        rows.push(Row {
            tone: Tone::Plain,
            // A caption carries no column of its own: it pads to the body's
            // number column and starts where the code does.
            lead: String::new(),
            spans: vec![Span::styled(
                note.to_string(),
                Style::default().fg(theme::DIM),
            )],
        });
    }
    Body {
        rows,
        code: true,
        hidden,
    }
}

/// The numbered rows and notes of `result`'s `range` window, laid out under
/// the number column, in the body's order: numbered rows first, then notes.
fn read_rows(result: &str, lang: Option<&str>, range: std::ops::Range<usize>) -> Vec<Row> {
    let mut numbered: Vec<(String, String)> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut seen = 0usize;
    for line in result.lines() {
        // The classification allocates, so rows before the window are only
        // counted: a tail window used to materialize every line it skipped.
        let parsed = split_numbered(line);
        let is_content = parsed.is_some() || !line.trim().is_empty();
        if !is_content {
            continue;
        }
        if seen >= range.start {
            match parsed {
                Some((number, text)) => {
                    numbered.push((number.to_string(), text.to_string()))
                }
                None => notes.push(line.trim_end().to_string()),
            }
        }
        seen += 1;
        if seen >= range.end {
            break;
        }
    }

    let width = numbered
        .iter()
        .map(|(number, _)| number.len())
        .max()
        .unwrap_or(0);
    let segments = lang.filter(|_| !numbered.is_empty()).map(|lang| {
        let refs: Vec<&str> = numbered.iter().map(|(_, text)| text.as_str()).collect();
        highlight::block(lang, &refs)
    });

    let mut rows = Vec::with_capacity(numbered.len() + notes.len());
    for (index, (number, text)) in numbered.iter().enumerate() {
        let segment = segments.as_ref().and_then(|block| block.get(index));
        rows.push(Row {
            tone: Tone::Plain,
            lead: format!("{number:>width$} "),
            spans: styled(text, segment.map(Vec::as_slice), Tone::Plain),
        });
    }
    for note in notes {
        rows.push(Row {
            tone: Tone::Plain,
            // A caption carries no column of its own: it pads to the body's
            // number column and starts where the code does.
            lead: String::new(),
            spans: vec![Span::styled(
                note.to_string(),
                Style::default().fg(theme::DIM),
            )],
        });
    }
    rows
}

/// Style one line from its highlight segments, substituting the row's tone for
/// every token the syntax palette leaves plain.
fn styled(line: &str, segments: Option<&[highlight::Segment]>, tone: Tone) -> Vec<Span<'static>> {
    let Some(segments) = segments.filter(|segments| !segments.is_empty()) else {
        return vec![Span::styled(line.to_string(), tone.style())];
    };
    segments
        .iter()
        .filter_map(|segment| {
            let text = line.get(segment.start..segment.end)?;
            let style = match segment.kind {
                highlight::TokenKind::Plain => tone.style(),
                kind => highlight::style(kind),
            };
            Some(Span::styled(text.to_string(), style))
        })
        .collect()
}

/// `read` writes each line as a right-aligned number, a tab, then the text.
fn split_numbered(line: &str) -> Option<(&str, &str)> {
    let (number, text) = line.split_once('\t')?;
    let number = number.trim();
    // A very long line the tool clipped still starts the same way; only a line
    // that really is `NNNN<TAB>text` counts, so a stray tab in prose cannot be
    // mistaken for a number column.
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((number, text))
}

/// How many lines a text blob would split into: the trailing newline is a
/// terminator, not an extra blank line.
fn count_lines(text: &str) -> usize {
    let text = text.strip_suffix('\n').unwrap_or(text);
    if text.is_empty() {
        0
    } else {
        text.matches('\n').count() + 1
    }
}

/// The first `max` of [`lines_of`]'s rows.
fn lines_head(text: &str, max: usize) -> Vec<String> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    if text.is_empty() || max == 0 {
        return Vec::new();
    }
    text.split('\n')
        .take(max)
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect()
}

/// The last `max` of [`lines_of`]'s rows.
fn lines_tail(text: &str, max: usize) -> Vec<String> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    if text.is_empty() || max == 0 {
        return Vec::new();
    }
    let skip = count_lines(text).saturating_sub(max);
    text.split('\n')
        .skip(skip)
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect()
}

/// The file extension as a highlighting token (`rs`, `py`, `ts`). An unknown
/// one simply renders plain — [`highlight::block`] has the same fallback.
fn language_of(path: &str) -> Option<String> {
    let extension = std::path::Path::new(path).extension()?.to_str()?;
    let extension = extension.to_ascii_lowercase();
    if extension.is_empty() {
        None
    } else {
        Some(extension)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::wrap::display_width;

    fn plain(row: &Row) -> String {
        row.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    /// A generous budget for the tests that are not about folding.
    const ALL: usize = 64;

    fn text(body: &Body) -> Vec<String> {
        body.rows
            .iter()
            .map(|row| format!("{}{}", row.lead, plain(row)))
            .collect()
    }

    #[test]
    fn an_expanded_read_shows_head_and_tail_around_a_marker() {
        let result: String = (1..=300)
            .map(|n| format!("{n:6}\tline {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let body = build(
            "read",
            r#"{"path": "a.txt"}"#,
            &result,
            Budget::HeadTail {
                head: 200,
                tail: 50,
            },
        )
        .expect("body");
        // The marker carries the count, so `hidden` stays zero and no fold
        // hint is added on top of it.
        assert_eq!(body.hidden, 0);
        assert_eq!(body.rows.len(), 251, "200 head + marker + 50 tail");
        let row = |index: usize| format!("{}{}", body.rows[index].lead, plain(&body.rows[index]));
        assert_eq!(row(0), "  1 line 1");
        assert_eq!(row(199), "200 line 200");
        assert_eq!(row(200), "… +50 lines omitted");
        assert_eq!(row(201), "251 line 251");
        assert_eq!(row(250), "300 line 300");
    }

    #[test]
    fn an_expanded_read_within_the_budget_shows_everything() {
        let result: String = (1..=60)
            .map(|n| format!("{n:6}\tline {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let body = build(
            "read",
            r#"{"path": "a.txt"}"#,
            &result,
            Budget::HeadTail {
                head: 200,
                tail: 50,
            },
        )
        .expect("body");
        assert_eq!(body.rows.len(), 60, "no marker under the budget");
        assert_eq!(text(&body)[59], "60 line 60");
    }

    #[test]
    fn an_expanded_write_shows_head_and_tail_of_the_change() {
        let content: String = (1..=400)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let arguments = serde_json::json!({ "path": "a.txt", "content": content });
        let body = build(
            "write",
            &arguments.to_string(),
            "created a.txt",
            Budget::HeadTail {
                head: 200,
                tail: 50,
            },
        )
        .expect("body");
        let lines = text(&body);
        // The stat row spends one slot of the head budget.
        assert_eq!(lines[0], "  (+400 -0) created");
        assert_eq!(lines[1], "+ line 1");
        assert_eq!(lines[199], "+ line 199");
        assert_eq!(lines[200], "  … +151 lines omitted");
        assert_eq!(lines[201], "+ line 351");
        assert_eq!(lines[250], "+ line 400");
        assert_eq!(body.rows.len(), 251);
        assert_eq!(body.hidden, 0);
    }

    #[test]
    fn write_renders_its_content_as_added_lines() {
        let body = build(
            "write",
            r#"{"path": "a/b.py", "content": "one\ntwo\n"}"#,
            "created a/b.py (8 bytes)",
            Budget::Head(ALL),
        )
        .expect("body");
        assert_eq!(body.rows.len(), 3, "stat row plus two content rows");
        assert_eq!(text(&body), vec!["  (+2 -0) created", "+ one", "+ two"]);
        assert_eq!(body.rows[1].tone, Tone::Add);
        assert!(body.code);
        assert_eq!(body.hidden, 0);
    }

    #[test]
    fn edit_removes_then_adds() {
        let body = build(
            "edit",
            r#"{"path": "a.py", "old_string": "old", "new_string": "new\nlines"}"#,
            "replaced 1 occurrence in a.py",
            Budget::Head(ALL),
        )
        .expect("body");
        assert_eq!(
            text(&body),
            vec![
                "  (+2 -1) replaced 1 occurrence",
                "- old",
                "+ new",
                "+ lines"
            ]
        );
        assert_eq!(body.rows[1].tone, Tone::Del);
    }

    #[test]
    fn edit_diffs_are_toned_and_highlighted() {
        let body = build(
            "edit",
            r#"{"path": "a.rs", "old_string": "let x = 1;", "new_string": "let x = 2;"}"#,
            "replaced 1 occurrence in a.rs",
            Budget::Head(ALL),
        )
        .expect("body");
        let removed = &body.rows[1];
        let added = &body.rows[2];
        let fg = |row: &Row, text: &str| {
            row.spans
                .iter()
                .find(|span| span.content.contains(text))
                .map(|span| span.style.fg)
        };
        // The marker column carries the tone, and so does the code the Rust
        // palette leaves plain (`= 1` / `= 2`, and `x`, whichever run the
        // grammar hands back)…
        assert_eq!(removed.lead, "- ");
        assert_eq!(fg(removed, "="), Some(Some(theme::DIFF_DEL)));
        assert_eq!(fg(added, "="), Some(Some(theme::DIFF_ADD)));
        // …while keywords keep their own colors, so the excerpt still reads as
        // source rather than as a painted block.
        assert_eq!(fg(removed, "let"), Some(Some(theme::CODE_KEYWORD)));
        assert_eq!(fg(added, "let"), Some(Some(theme::CODE_KEYWORD)));
    }

    #[test]
    fn a_body_caps_its_rows_and_counts_the_rest() {
        let content: Vec<String> = (1..=12).map(|n| format!("line {n}")).collect();
        let arguments = serde_json::json!({ "path": "a.txt", "content": content.join("\n") });
        // The stat row is row one of the budget, so seven content rows follow.
        let body = build("write", &arguments.to_string(), "created a.txt", Budget::Head(8)).expect("body");
        assert_eq!(body.rows.len(), 8);
        assert_eq!(body.hidden, 5);
        assert_eq!(
            text(&body)[0],
            "  (+12 -0) created",
            "the stat counts everything"
        );
        assert_eq!(text(&body)[7], "+ line 7");

        // Removed lines spend the budget before added ones, so a long delete
        // never hides the fact that it was a delete.
        let old: Vec<String> = (1..=10).map(|n| format!("gone {n}")).collect();
        let arguments = serde_json::json!({
            "path": "a.txt",
            "old_string": old.join("\n"),
            "new_string": "kept",
        });
        let body = build("edit", &arguments.to_string(), "replaced", Budget::Head(4)).expect("body");
        assert_eq!(
            text(&body),
            vec!["  (+1 -10) replaced", "- gone 1", "- gone 2", "- gone 3"]
        );
        // Seven removed rows and the one added row stayed out of the body.
        assert_eq!(body.hidden, 8);

        // A budget too small for the stat row materializes nothing at all: all
        // twelve content rows stay hidden and the stat is not shown either.
        let write_args = serde_json::json!({ "path": "a.txt", "content": content.join("\n") });
        let body = build("write", &write_args.to_string(), "created", Budget::Head(0)).expect("body");
        assert!(body.rows.is_empty());
        assert_eq!(body.hidden, 12);
    }

    #[test]
    fn read_renumbers_its_window_and_keeps_the_caption() {
        let result = "     1\tfn main() {}\n     2\t\n[2 lines total, showing 1-2]";
        let body = build("read", r#"{"path": "a.rs"}"#, result, Budget::Head(ALL)).expect("body");
        assert_eq!(
            text(&body),
            vec!["1 fn main() {}", "2 ", "[2 lines total, showing 1-2]"]
        );
        // The number column is right-aligned to the widest number.
        let wider = build(
            "read",
            r#"{"path": "a.rs"}"#,
            "     9\tlast\n    10\tend\n[2 lines total, showing 9-10]",
            Budget::Head(ALL),
        )
        .expect("body");
        assert_eq!(
            text(&wider),
            vec![" 9 last", "10 end", "[2 lines total, showing 9-10]"]
        );
        assert_eq!(display_width(&wider.rows[1].lead), 3);
    }

    #[test]
    fn read_keeps_unstructured_output_as_plain_rows() {
        // The tool answers `is empty` for an empty file: no number column, so
        // the row is a caption rather than a misparsed line.
        let body = build("read", r#"{"path": "a.rs"}"#, "a.rs is empty", Budget::Head(ALL)).expect("body");
        assert_eq!(text(&body), vec!["a.rs is empty"]);
        assert_eq!(body.rows[0].tone, Tone::Plain);
    }

    #[test]
    fn unknown_tools_and_missing_arguments_have_no_structured_body() {
        assert!(build("bash", r#"{"command": "ls"}"#, "a\nb", Budget::Head(ALL)).is_none());
        assert!(build("write", r#"{"path": "a.py"}"#, "created", Budget::Head(ALL)).is_none());
        assert!(build("edit", "{ not json", "boom", Budget::Head(ALL)).is_none());
    }

    #[test]
    fn the_stat_row_keeps_the_tools_verdict_but_drops_the_path() {
        // The header already shows the path, and `(N bytes)` says nothing about
        // the change; `created` / the replacement count still does.
        assert_eq!(
            caption("created a/b.rs (120 bytes)").as_deref(),
            Some("created")
        );
        assert_eq!(
            caption("overwrote /abs/docs/notes.md (74 bytes)").as_deref(),
            Some("overwrote")
        );
        assert_eq!(
            caption("replaced 1 occurrence in /abs/src/a.rs").as_deref(),
            Some("replaced 1 occurrence")
        );
        assert_eq!(
            caption("replaced 3 occurrences in a.rs").as_deref(),
            Some("replaced 3 occurrences")
        );
        assert_eq!(caption("   "), None);
    }

    #[test]
    fn extensions_become_highlighting_tokens() {
        assert_eq!(language_of("src/main.rs").as_deref(), Some("rs"));
        assert_eq!(language_of("A/B.PY").as_deref(), Some("py"));
        assert_eq!(language_of("Makefile"), None);
    }
}
