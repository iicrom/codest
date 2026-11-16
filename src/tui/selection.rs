//! Mouse text selection over the transcript viewport.
//!
//! A selection lives in **absolute transcript coordinates** — a display
//! column within an absolute row, where absolute rows are the same numbering
//! `render_chat` uses to lay the session out (history cells, their blank
//! separators, then the live tail). That is the whole reason this module is
//! small: the transcript is a virtual row buffer, so scrolling the viewport
//! cannot lose or shift a selection the way it would in a screen-buffer UI —
//! the selection stays pinned to the text under it, and the highlight simply
//! follows the rows into view.
//!
//! Rendering is an overlay: after `render_chat` materializes the visible
//! rows, [`apply_overlay`] restyles the spans that fall inside the range,
//! trading the span's background for [`theme::SELECTION_BG`] while keeping
//! its foreground — a solid highlight that reads like a native terminal's.
//! Extraction ([`slice_line`]) walks the same spans, so what is copied is
//! exactly what is highlighted, wide characters included whole.
//!
//! Copied text is *not* the raw screen text: the transcript's own chrome is
//! decoration, not content. A card's `┌ │ └` spine, the two-column indent a
//! message's wrapped rows hang under, and the `• ` / `▎ ` markers markdown
//! puts in front are all there to make the screen readable, and pasting them
//! into an editor leaves a stripe of box-drawing characters down the left.
//! [`join_rows`] removes the *common* indent of a copied block, so a
//! multi-row copy comes out as the text the user selected, with the relative
//! indentation that carries meaning — code, nested lists — intact.

use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::tui::theme;

/// One end of a selection: a display column within an absolute transcript
/// row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub col: usize,
    pub row: usize,
}

impl Point {
    /// The two ends ordered reading order: earlier rows first, then columns.
    pub fn ordered(a: Point, b: Point) -> (Point, Point) {
        if (a.row, a.col) <= (b.row, b.col) {
            (a, b)
        } else {
            (b, a)
        }
    }
}

/// What the gesture selected: a free drag, a double-click word, or a
/// triple-click whole line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Chars,
    Word,
    Line,
}

/// Anchor plus focus, in absolute transcript coordinates. `focus` stays
/// `None` until the pointer moves, so a bare click never highlights — and a
/// click that never moved clears rather than copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionState {
    pub anchor: Point,
    pub focus: Option<Point>,
    pub kind: SelectionKind,
}

impl SelectionState {
    pub fn new(anchor: Point, kind: SelectionKind) -> Self {
        Self {
            anchor,
            focus: None,
            kind,
        }
    }

    /// The selection a double-click starts: the word at the point.
    pub fn word_selection(point: Point, row_text: &str) -> Self {
        let (lo, hi) = word_bounds(row_text, point.col);
        Self {
            anchor: Point {
                col: lo,
                row: point.row,
            },
            focus: Some(Point {
                col: hi,
                row: point.row,
            }),
            kind: SelectionKind::Word,
        }
    }

    /// The selection a triple-click starts: the whole line.
    pub fn line_selection(point: Point) -> Self {
        Self {
            anchor: Point {
                col: 0,
                row: point.row,
            },
            focus: Some(Point {
                col: usize::MAX,
                row: point.row,
            }),
            kind: SelectionKind::Line,
        }
    }

    /// Drag the focus end. Word kind snaps the focus to the boundary of the
    /// pointed-at word, in the direction of the drag; `row_text` is the
    /// pointed-at row's plain text. Line kind only follows the row.
    pub fn update(&mut self, point: Point, row_text: &str) {
        self.focus = Some(match self.kind {
            SelectionKind::Word => {
                let (lo, hi) = word_bounds(row_text, point.col);
                let forward = (point.row, point.col) >= (self.anchor.row, self.anchor.col);
                Point {
                    row: point.row,
                    col: if forward { hi } else { lo },
                }
            }
            _ => point,
        });
    }

    /// The normalized `(start, end)` range, once a focus exists. Line
    /// selections cover their rows whole, whichever direction the drag went.
    pub fn range(&self) -> Option<(Point, Point)> {
        let focus = self.focus?;
        Some(match self.kind {
            SelectionKind::Line => {
                let lo = self.anchor.row.min(focus.row);
                let hi = self.anchor.row.max(focus.row);
                (
                    Point { col: 0, row: lo },
                    Point {
                        col: usize::MAX,
                        row: hi,
                    },
                )
            }
            _ => Point::ordered(self.anchor, focus),
        })
    }

    /// Whether the selection covers nothing: no focus yet (a bare press), or
    /// a focus on the very cell the anchor sits on — a click that never
    /// moved a column. (A drag across columns that lands past the text
    /// copies nothing; `App::selected_text` handles that emptiness.)
    pub fn is_empty(&self) -> bool {
        match self.range() {
            None => true,
            Some((start, end)) => start == end,
        }
    }
}

/// The overlay style: a solid selection background that preserves whatever
/// foreground the span carries.
fn selection_style() -> Style {
    Style::new().bg(theme::SELECTION_BG)
}

/// Apply the selection's background to the visible rows. `top` is the
/// absolute transcript row of `lines[0]`.
pub fn apply_overlay(lines: &mut [Line<'static>], top: usize, selection: &SelectionState) {
    let Some((start, end)) = selection.range() else {
        return;
    };
    let base = selection_style();
    for (offset, line) in lines.iter_mut().enumerate() {
        let row = top + offset;
        if row < start.row || row > end.row {
            continue;
        }
        let (from, to) = if start.row == end.row {
            (start.col, end.col)
        } else if row == start.row {
            (start.col, usize::MAX)
        } else if row == end.row {
            (0, end.col)
        } else {
            (0, usize::MAX)
        };
        restyle_line(line, from, to, base);
    }
}

/// Restyle the parts of `line` between display columns `from` and `to`.
/// Wide characters are covered whole: a cut that would land inside one moves
/// to include the character, so a two-column glyph never renders as two
/// broken halves.
fn restyle_line(line: &mut Line<'static>, from: usize, to: usize, base: Style) {
    if to == 0 || from >= to {
        return;
    }
    let spans = std::mem::take(&mut line.spans);
    let mut rebuilt: Vec<Span<'static>> = Vec::with_capacity(spans.len());
    let mut col = 0usize;
    for span in spans {
        let width = UnicodeWidthStr::width(span.content.as_ref());
        // Fast path: the span lies wholly outside the range.
        if col >= to || col + width <= from {
            col += width;
            rebuilt.push(span);
            continue;
        }
        let base_col = col;
        let mut pre = String::new();
        let mut mid = String::new();
        let mut post = String::new();
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            let (start, end) = (col, col + w);
            if end <= from {
                pre.push(c);
            } else if start >= to {
                post.push(c);
            } else {
                // Straddling a boundary counts as covered.
                mid.push(c);
            }
            col += w;
        }
        debug_assert_eq!(col, base_col + width);
        if !pre.is_empty() {
            rebuilt.push(Span::styled(pre, span.style));
        }
        if !mid.is_empty() {
            rebuilt.push(Span::styled(mid, span.style.patch(base)));
        }
        if !post.is_empty() {
            rebuilt.push(Span::styled(post, span.style));
        }
    }
    line.spans = rebuilt;
}

/// The text of `line` between display columns `from` and `to`. Columns past
/// the line's end contribute nothing; wide characters are taken whole when
/// the range touches either half.
pub fn slice_line(line: &Line<'_>, from: usize, to: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for span in &line.spans {
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if col + w > from && col < to {
                out.push(c);
            }
            col += w;
        }
        if col >= to {
            break;
        }
    }
    out
}

/// The glyphs the transcript draws as chrome instead of content: the card
/// spine (`┌ │ └`), markdown's block markers (`▎ │ • `), and the gutter glyph
/// of each cell kind (`› ! ■ ▪ ● ✗`).
const CHROME: &str = "│┌└┏┓┗┛▎┃•›■▪●✗";

/// Whether `c` is decoration: one of [`CHROME`], or a blank — a marker never
/// sits flush against the text it introduces, and a nested list item steps in
/// by blanking its parent's marker, so blanks carry no content either.
fn is_decoration(c: char) -> bool {
    c.is_whitespace() || CHROME.contains(c)
}

/// How many display columns of chrome and blanks `row` starts with, and where
/// that run ends in the string.
fn decoration_end(row: &str) -> (usize, usize) {
    let mut width = 0usize;
    for (index, c) in row.char_indices() {
        if !is_decoration(c) {
            return (width, index);
        }
        width += crate::tui::wrap::char_width(c);
    }
    (width, row.len())
}

/// The text a copy of `rows` produces.
///
/// The transcript's left chrome is decoration, not content: copying a card's
/// body or a message's wrapped rows verbatim would paste a column of `│` and
/// the indent hanging under it into the user's editor. So the block's
/// **common** indent comes off — every row loses the narrowest leading
/// decoration run in the selection — while whatever a row carries beyond that
/// stays: code keeps its relative indentation, a diff row its `+`/`-` sign,
/// and a nested list its nesting. A single row therefore loses its whole
/// gutter, and a selection that starts inside the text (its first row already
/// cut at content) loses nothing at all, because the common run is then zero.
pub fn join_rows(rows: &[String]) -> String {
    // Per row: the chrome's width, where it ends, and whether anything is left
    // after it. Blank rows (and rows that are pure chrome, like a card's
    // closing ` └ `) have no content to align with, so they never shrink the
    // common indent.
    let decorated: Vec<(usize, usize, bool)> = rows
        .iter()
        .map(|row| {
            let (width, at) = decoration_end(row);
            (width, at, at < row.trim_end().len())
        })
        .collect();
    let common = decorated
        .iter()
        .filter(|(_, _, content)| *content)
        .map(|(width, _, _)| *width)
        .min()
        .unwrap_or(0);
    let mut out = String::new();
    for (index, (row, &(width, at, content))) in rows.iter().zip(&decorated).enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if !content {
            // A blank or chrome-only row copies as the blank line it reads as.
            continue;
        }
        // Trailing blanks are the padding the layout drew the row with.
        let text = row[at..].trim_end();
        if width > common {
            out.push_str(&" ".repeat(width - common));
        }
        out.push_str(text);
    }
    // The blank separator rows a selection can begin or end on are not part of
    // what was selected; interior blanks stay.
    out.trim_matches('\n').to_string()
}

/// The word-class run covering display column `col` in `text`, as
/// `(start, end)` display columns (end exclusive). Word characters cluster
/// with their class — alphanumerics plus the path/punctuation companions a
/// click on a filename or URL wants — and runs of anything else (whitespace
/// included) cluster together, so a double-click on a space selects the
/// spaces.
pub fn word_bounds(text: &str, col: usize) -> (usize, usize) {
    // Per-char display columns; zero-width marks ride with their base char.
    let mut chars: Vec<(char, usize, usize)> = Vec::new();
    let mut cursor = 0usize;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if w > 0 {
            chars.push((c, cursor, w));
            cursor += w;
        }
    }
    if chars.is_empty() {
        return (0, 0);
    }
    // The char under (or just before) the pointed-at column.
    let mut index = chars.partition_point(|&(_, start, _)| start <= col);
    index = index.saturating_sub(1).min(chars.len() - 1);
    let class = word_class(chars[index].0);
    let mut lo = index;
    while lo > 0 && word_class(chars[lo - 1].0) == class {
        lo -= 1;
    }
    let mut hi = index;
    while hi + 1 < chars.len() && word_class(chars[hi + 1].0) == class {
        hi += 1;
    }
    (chars[lo].1, chars[hi].1 + chars[hi].2)
}

/// Word characters: alphanumerics plus the companions filenames, URLs, and
/// identifiers are built from. Everything else shares one "other" class.
fn word_class(c: char) -> u8 {
    if c.is_alphanumeric()
        || matches!(
            c,
            '_' | '-' | '.' | '/' | '@' | '$' | '~' | '\\' | '+' | ':' | '#'
        )
    {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn line(text: &str) -> Line<'static> {
        Line::from(text.to_string())
    }

    #[test]
    fn slicing_cuts_at_display_columns() {
        let l = line("hello world");
        assert_eq!(slice_line(&l, 0, 5), "hello");
        assert_eq!(slice_line(&l, 6, 11), "world");
        assert_eq!(slice_line(&l, 6, 100), "world");
        assert_eq!(slice_line(&l, 0, 0), "");
        // Columns past the end contribute nothing.
        assert_eq!(slice_line(&l, 20, 30), "");
    }

    #[test]
    fn slicing_takes_wide_characters_whole() {
        // CJK chars are 2 columns each: 你(0-2) 界(2-4).
        let l = line("你界");
        assert_eq!(slice_line(&l, 0, 2), "你");
        // A cut through the second half of a wide char takes the whole char.
        assert_eq!(slice_line(&l, 1, 2), "你");
        assert_eq!(slice_line(&l, 1, 3), "你界");
        assert_eq!(slice_line(&l, 3, 4), "界");
    }

    #[test]
    fn overlay_trades_background_and_keeps_foreground() {
        let mut lines = [Line::from(vec![
            Span::styled("ab".to_string(), Style::new().fg(Color::Green)),
            Span::from("cd"),
        ])];
        let selection = SelectionState {
            anchor: Point { col: 1, row: 0 },
            focus: Some(Point { col: 3, row: 0 }),
            kind: SelectionKind::Chars,
        };
        apply_overlay(&mut lines, 0, &selection);
        let spans: Vec<_> = lines[0].spans.iter().collect();
        // Each source span splits at the range boundary: `ab` into a|b, `cd`
        // into c|d, with the covered parts carrying the selection bg.
        assert_eq!(spans.len(), 4, "the covered spans split: {spans:?}");
        assert_eq!(spans[0].content, "a");
        assert_eq!(spans[0].style.fg, Some(Color::Green));
        assert_eq!(spans[0].style.bg, None);
        assert_eq!(spans[1].content, "b");
        assert_eq!(spans[1].style.bg, Some(theme::SELECTION_BG));
        assert_eq!(spans[1].style.fg, Some(Color::Green));
        assert_eq!(spans[2].content, "c");
        assert_eq!(spans[2].style.bg, Some(theme::SELECTION_BG));
        assert_eq!(spans[3].content, "d");
        assert_eq!(spans[3].style.bg, None);
    }

    #[test]
    fn overlay_skips_rows_outside_the_range() {
        let mut lines = [line("middle")];
        let selection = SelectionState {
            anchor: Point { col: 0, row: 5 },
            focus: Some(Point { col: 2, row: 7 }),
            kind: SelectionKind::Chars,
        };
        apply_overlay(&mut lines, 1, &selection);
        assert!(lines[0].spans.iter().all(|span| span.style.bg.is_none()));
    }

    #[test]
    fn joining_rows_strips_the_common_chrome() {
        // A card body: the `┌ │ └` spine plus the code under it. Every row
        // loses the spine, and the code's own indentation survives.
        let rows = vec![
            " ┌  fn main() {".to_string(),
            " │      let x = 1;".to_string(),
            " └  }".to_string(),
        ];
        assert_eq!(join_rows(&rows), "fn main() {\n    let x = 1;\n}");
    }

    #[test]
    fn joining_rows_keeps_relative_indentation_and_markers() {
        // A markdown list under a message's two-column indent: one row loses
        // the whole gutter, the nested one keeps its extra step.
        let rows = vec![
            "• item one".to_string(),
            "  • nested".to_string(),
            "  continuing".to_string(),
        ];
        assert_eq!(join_rows(&rows), "item one\n  nested\ncontinuing");
        // A diff body keeps its number column and its sign column: nothing of
        // either is decoration, so the common run stops at the lead.
        let rows = vec!["    3 +added".to_string(), "    4 -removed".to_string()];
        assert_eq!(join_rows(&rows), "3 +added\n4 -removed");
    }

    #[test]
    fn joining_rows_leaves_a_content_started_selection_alone() {
        // The first row was cut inside the text, so the common run is zero:
        // no row is shifted sideways. A row that is only chrome still loses
        // the glyph, and keeps the column it occupied as blanks — alignment
        // survives without the decoration.
        let rows = vec![
            "e rest of the sentence".to_string(),
            "  │  second row".to_string(),
        ];
        assert_eq!(
            join_rows(&rows),
            "e rest of the sentence\n     second row",
            "no common indent, and no chrome glyph"
        );
        // A single row loses its whole gutter.
        assert_eq!(join_rows(&["  │  hello  ".to_string()]), "hello");
    }

    #[test]
    fn joining_rows_ignores_blank_and_chrome_only_rows() {
        // The blank separator and the closing corner carry no content, so they
        // must not pull the common indent down to the corner's own width.
        let rows = vec![
            " │  first".to_string(),
            String::new(),
            " │  second".to_string(),
            " └  ".to_string(),
        ];
        assert_eq!(join_rows(&rows), "first\n\nsecond");
    }

    #[test]
    fn joining_rows_leaves_a_single_line_copied_mid_text_alone() {
        // Nothing decorative in front: a word copy from the middle of a row.
        assert_eq!(join_rows(&["hello world".to_string()]), "hello world");
    }

    #[test]
    fn word_bounds_cluster_by_class() {
        assert_eq!(word_bounds("hello world", 2), (0, 5));
        assert_eq!(word_bounds("hello world", 4), (0, 5));
        // The space is its own run.
        assert_eq!(word_bounds("hello world", 5), (5, 6));
        assert_eq!(word_bounds("hello world", 7), (6, 11));
        // Clicking far past the text clamps to the last char.
        assert_eq!(word_bounds("hello world", 99), (6, 11));
        assert_eq!(word_bounds("", 0), (0, 0));
    }

    #[test]
    fn word_bounds_select_paths_and_punctuation_companions() {
        assert_eq!(word_bounds("src/tui/app.rs", 8), (0, 14));
        assert_eq!(word_bounds("cargo test --lib", 6), (6, 10));
        // A run of other characters clusters together.
        assert_eq!(word_bounds("a --- b", 3), (2, 5));
    }

    #[test]
    fn word_bounds_handles_wide_characters() {
        assert_eq!(word_bounds("你界世", 3), (0, 6));
    }

    #[test]
    fn word_and_line_selections_start_complete() {
        let selection = SelectionState::word_selection(Point { col: 7, row: 2 }, "hello world");
        assert_eq!(selection.kind, SelectionKind::Word);
        assert_eq!(
            selection.range(),
            Some((
                Point { col: 6, row: 2 },
                Point { col: 11, row: 2 }
            ))
        );
        let selection = SelectionState::line_selection(Point { col: 4, row: 3 });
        assert_eq!(selection.kind, SelectionKind::Line);
        assert_eq!(
            selection.range(),
            Some((
                Point { col: 0, row: 3 },
                Point { col: usize::MAX, row: 3 }
            ))
        );
    }

    #[test]
    fn word_drag_snaps_to_the_boundary_in_the_drag_direction() {
        let mut selection =
            SelectionState::word_selection(Point { col: 7, row: 3 }, "alpha beta");
        // "beta" is anchored; dragging right extends to its end…
        selection.update(Point { col: 9, row: 3 }, "alpha beta");
        assert_eq!(
            selection.range(),
            Some((Point { col: 6, row: 3 }, Point { col: 10, row: 3 }))
        );
        // …and dragging left of the anchor flips to the pointed-at word's
        // start: the range now runs from "alpha" through "beta".
        selection.update(Point { col: 2, row: 3 }, "alpha beta");
        assert_eq!(
            selection.range(),
            Some((Point { col: 0, row: 3 }, Point { col: 6, row: 3 }))
        );
    }

    #[test]
    fn line_drag_extends_to_whole_rows() {
        let mut selection = SelectionState::line_selection(Point { col: 4, row: 3 });
        selection.update(Point { col: 2, row: 5 }, "");
        assert_eq!(
            selection.range(),
            Some((
                Point { col: 0, row: 3 },
                Point { col: usize::MAX, row: 5 }
            ))
        );
        selection.update(Point { col: 2, row: 1 }, "");
        assert_eq!(
            selection.range(),
            Some((
                Point { col: 0, row: 1 },
                Point { col: usize::MAX, row: 3 }
            ))
        );
    }

    #[test]
    fn empty_selection_when_the_pointer_never_moved() {
        let mut selection = SelectionState::new(Point { col: 3, row: 1 }, SelectionKind::Chars);
        assert!(selection.is_empty());
        selection.update(Point { col: 3, row: 1 }, "");
        assert!(selection.is_empty(), "same cell: nothing selected");
        selection.update(Point { col: 6, row: 1 }, "");
        assert!(!selection.is_empty());
    }
}
