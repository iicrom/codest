//! Display-width-aware text wrapping.
//!
//! Both the chat transcript and the composer need the same answer to "which
//! characters land on which visual row?". Codex has a much larger wrapping
//! module with grapheme and hyperlink support; this is the minimal version:
//! char-based, Unicode width aware, with word breaks preferred over mid-word
//! breaks.

use unicode_width::UnicodeWidthChar;

/// Width of a single character as rendered by a terminal.
pub fn char_width(c: char) -> usize {
    match c {
        '\t' => 4,
        c if c.is_control() => 0,
        c => UnicodeWidthChar::width(c).unwrap_or(0),
    }
}

pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn width_of(chars: &[char]) -> usize {
    chars.iter().copied().map(char_width).sum()
}

/// Split `chars` into visual rows as half-open `(start, end)` ranges.
///
/// Newlines always end a row. Overlong rows break at the last space in the
/// row when there is one; the break space itself is dropped from the display
/// (the indices remain usable for cursor mapping). If there is no space, the
/// row breaks at the character that would overflow.
pub fn wrap_chars(chars: &[char], width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut line_start = 0usize;
    let mut index = 0usize;
    let mut column = 0usize;
    let mut last_space: Option<usize> = None;

    while index < chars.len() {
        let c = chars[index];
        if c == '\n' {
            rows.push((line_start, index));
            index += 1;
            line_start = index;
            column = 0;
            last_space = None;
            continue;
        }

        let w = char_width(c);
        if column > 0 && column + w > width && index > line_start {
            // The current char may itself be the break space. Otherwise use
            // the latest space seen in this row. If the only space is at the
            // row start, drop it instead of emitting a leading space.
            let break_space = if c == ' ' {
                Some(index)
            } else {
                last_space.filter(|&space| space > line_start)
            };

            match break_space {
                Some(space) => {
                    if space > line_start {
                        rows.push((line_start, space));
                    }
                    line_start = space + 1;
                }
                None if last_space == Some(line_start) => {
                    line_start += 1;
                }
                None => {
                    rows.push((line_start, index));
                    line_start = index;
                }
            }

            column = if line_start <= index {
                width_of(&chars[line_start..index])
            } else {
                0
            };
            last_space = if line_start <= index {
                chars[line_start..index]
                    .iter()
                    .rposition(|&c| c == ' ')
                    .map(|offset| line_start + offset)
            } else {
                None
            };

            // A space that triggered the break is consumed by the break.
            if c == ' ' {
                index += 1;
                continue;
            }
        }

        column += w;
        if c == ' ' {
            last_space = Some(index);
        }
        index += 1;
    }

    rows.push((line_start, chars.len()));
    rows
}

/// Wrap a string, returning the visible text of each row.
pub fn wrap_str(text: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    wrap_chars(&chars, width)
        .into_iter()
        .map(|(start, end)| chars[start..end].iter().collect())
        .collect()
}

/// Map a cursor (a char index) to `(row, column)` within already wrapped rows.
pub fn cursor_position(chars: &[char], rows: &[(usize, usize)], cursor: usize) -> (usize, usize) {
    let cursor = cursor.min(chars.len());
    let Some(&(last_start, _)) = rows.last() else {
        return (0, 0);
    };

    for (index, &(start, end)) in rows.iter().enumerate() {
        if cursor < end {
            return (index, width_of(&chars[start..cursor]));
        }
        if cursor == end {
            if let Some(&(next_start, _)) = rows.get(index + 1)
                && next_start == cursor
            {
                return (index + 1, 0);
            }
            return (index, width_of(&chars[start..end]));
        }
    }

    (rows.len() - 1, width_of(&chars[last_start..chars.len()]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str, width: usize) -> Vec<String> {
        wrap_str(text, width)
    }

    #[test]
    fn preserves_newlines_and_empty_lines() {
        assert_eq!(rows("a\n\nb", 20), vec!["a", "", "b"]);
        assert_eq!(rows("", 20), vec![""]);
        assert_eq!(rows("a\n", 20), vec!["a", ""]);
    }

    #[test]
    fn wraps_at_spaces_without_leading_blanks() {
        assert_eq!(rows("aaaa bbbb", 4), vec!["aaaa", "bbbb"]);
        assert_eq!(
            rows("the quick brown fox", 9),
            vec!["the quick", "brown fox"]
        );
    }

    #[test]
    fn breaks_overlong_words() {
        assert_eq!(rows("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn accounts_for_wide_characters() {
        assert_eq!(rows("你好世界", 4), vec!["你好", "世界"]);
        assert_eq!(rows("hi 你好", 4), vec!["hi", "你好"]);
    }

    #[test]
    fn maps_cursor_to_visual_rows() {
        let chars: Vec<char> = "aaaa bbbb".chars().collect();
        let wrapped = wrap_chars(&chars, 4);
        assert_eq!(cursor_position(&chars, &wrapped, 0), (0, 0));
        assert_eq!(cursor_position(&chars, &wrapped, 4), (0, 4)); // before break
        assert_eq!(cursor_position(&chars, &wrapped, 5), (1, 0));
        assert_eq!(cursor_position(&chars, &wrapped, 9), (1, 4));
    }

    #[test]
    fn cursor_at_hard_break_moves_to_next_row() {
        let chars: Vec<char> = "abcdef".chars().collect();
        let wrapped = wrap_chars(&chars, 3);
        assert_eq!(wrapped, vec![(0, 3), (3, 6)]);
        assert_eq!(cursor_position(&chars, &wrapped, 3), (1, 0));
    }
}
