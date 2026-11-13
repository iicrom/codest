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
    /// Logical rows the row budget hid (`0` → the body is complete). Counted
    /// while the body is built, so a huge `write` never materializes rows the
    /// card cannot show — this runs for every cell on every frame.
    pub hidden: usize,
}

/// The structured body for `name`, or `None` when this tool's output is just
/// text and the card should show the result string.
///
/// `arguments` is the raw JSON the model sent; a body is only built when the
/// arguments actually carry what the body needs (`content`, `old_string` /
/// `new_string`), so a malformed call still falls back to the plain card.
///
/// `max` bounds how many rows are materialized; everything past it is counted
/// into [`Body::hidden`]. The stat row and the numbered `read` rows both count
/// against it, the way cordis caps a whole diff body at
/// [`chat::DIFF_BODY_MAX_LINES`](crate::tui::chat).
pub fn build(name: &str, arguments: &str, result: &str, max: usize) -> Option<Body> {
    let parsed: serde_json::Value = serde_json::from_str(arguments.trim()).ok()?;
    let lang = parsed
        .get("path")
        .and_then(serde_json::Value::as_str)
        .and_then(language_of);
    match name {
        "write" => {
            let content = parsed.get("content").and_then(serde_json::Value::as_str)?;
            Some(diff(&[], content, lang.as_deref(), caption(result), max))
        }
        "edit" => {
            let old = parsed
                .get("old_string")
                .and_then(serde_json::Value::as_str)?;
            let new = parsed
                .get("new_string")
                .and_then(serde_json::Value::as_str)?;
            Some(diff(
                &lines_of(old),
                new,
                lang.as_deref(),
                caption(result),
                max,
            ))
        }
        "read" => Some(read(result, lang.as_deref(), max)),
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
/// The stat row is `(+N -M)`, followed by the tool's own one-line verdict when
/// `caption` supplies one (`created`, `overwrote`, the replacement count): the
/// result string still carries facts a diff cannot, so the card keeps them
/// instead of discarding the whole line.
fn diff(
    removed: &[String],
    added: &str,
    lang: Option<&str>,
    caption: Option<String>,
    max: usize,
) -> Body {
    // The stat states the WHOLE change, so it counts every line even when the
    // rows below it are capped.
    let added_total = count_lines(added);
    let mut rows = Vec::with_capacity(max.min(removed.len() + added_total + 1));
    let mut hidden = removed.len() + added_total;
    if max == 0 {
        return Body {
            rows,
            code: true,
            hidden,
        };
    }
    let mut stat = Span::styled(
        format!("(+{added_total} -{})", removed.len()),
        Style::default().fg(theme::DIM),
    );
    if let Some(caption) = caption {
        stat.content = format!("{} {caption}", stat.content).into();
    }
    rows.push(Row {
        tone: Tone::Plain,
        lead: MARK.to_string(),
        spans: vec![stat],
    });
    let mut budget = max - 1;
    let removed_rows = removed.len().min(budget);
    hidden -= removed_rows;
    budget -= removed_rows;
    push_side(&mut rows, &removed[..removed_rows], Tone::Del, '-', lang);

    let kept = lines_head(added, budget);
    hidden -= kept.len();
    push_side(&mut rows, &kept, Tone::Add, '+', lang);
    Body {
        rows,
        code: true,
        hidden,
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
fn read(result: &str, lang: Option<&str>, max: usize) -> Body {
    let mut numbered: Vec<(String, &str)> = Vec::new();
    let mut notes: Vec<&str> = Vec::new();
    let mut rows_seen = 0usize;
    let mut hidden = 0usize;
    for line in result.lines() {
        let row = match split_numbered(line) {
            Some((number, text)) => Some(Ok((number.to_string(), text))),
            None if !line.trim().is_empty() => Some(Err(line.trim_end())),
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
            Ok(numbered_row) => numbered.push(numbered_row),
            Err(note) => notes.push(note),
        }
    }

    let width = numbered
        .iter()
        .map(|(number, _)| number.len())
        .max()
        .unwrap_or(0);
    let segments = lang.filter(|_| !numbered.is_empty()).map(|lang| {
        let refs: Vec<&str> = numbered.iter().map(|(_, text)| *text).collect();
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

/// A text blob's lines: the trailing newline is a terminator, not an extra
/// blank line, while interior blanks survive. CRLF line ends are folded so the
/// rows do not carry a stray carriage return into the terminal.
fn lines_of(text: &str) -> Vec<String> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect()
}

/// How many lines [`lines_of`] would return, without building them.
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
    fn write_renders_its_content_as_added_lines() {
        let body = build(
            "write",
            r#"{"path": "a/b.py", "content": "one\ntwo\n"}"#,
            "created a/b.py (8 bytes)",
            ALL,
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
            ALL,
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
            ALL,
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
        let body = build("write", &arguments.to_string(), "created a.txt", 8).expect("body");
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
        let body = build("edit", &arguments.to_string(), "replaced", 4).expect("body");
        assert_eq!(
            text(&body),
            vec!["  (+1 -10) replaced", "- gone 1", "- gone 2", "- gone 3"]
        );
        // Seven removed rows and the one added row stayed out of the body.
        assert_eq!(body.hidden, 8);

        // A budget too small for the stat row materializes nothing at all: all
        // twelve content rows stay hidden and the stat is not shown either.
        let write_args = serde_json::json!({ "path": "a.txt", "content": content.join("\n") });
        let body = build("write", &write_args.to_string(), "created", 0).expect("body");
        assert!(body.rows.is_empty());
        assert_eq!(body.hidden, 12);
    }

    #[test]
    fn read_renumbers_its_window_and_keeps_the_caption() {
        let result = "     1\tfn main() {}\n     2\t\n[2 lines total, showing 1-2]";
        let body = build("read", r#"{"path": "a.rs"}"#, result, ALL).expect("body");
        assert_eq!(
            text(&body),
            vec!["1 fn main() {}", "2 ", "[2 lines total, showing 1-2]"]
        );
        // The number column is right-aligned to the widest number.
        let wider = build(
            "read",
            r#"{"path": "a.rs"}"#,
            "     9\tlast\n    10\tend\n[2 lines total, showing 9-10]",
            ALL,
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
        let body = build("read", r#"{"path": "a.rs"}"#, "a.rs is empty", ALL).expect("body");
        assert_eq!(text(&body), vec!["a.rs is empty"]);
        assert_eq!(body.rows[0].tone, Tone::Plain);
    }

    #[test]
    fn unknown_tools_and_missing_arguments_have_no_structured_body() {
        assert!(build("bash", r#"{"command": "ls"}"#, "a\nb", ALL).is_none());
        assert!(build("write", r#"{"path": "a.py"}"#, "created", ALL).is_none());
        assert!(build("edit", "{ not json", "boom", ALL).is_none());
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
