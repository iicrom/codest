//! Shared output truncation for the built-in tools, distilled from pi's
//! `truncate.js` (2000 lines / 50 KB with an actionable notice) and codex's
//! collection cap (1 MiB per stream, so a runaway command cannot eat memory
//! before the model-facing cap applies).
//!
//! Two directions, matched to how each tool is consumed:
//!
//! * [`truncate_tail`] keeps the *last* lines of shell output — recent
//!   output is usually what matters after a long build. The notice says how
//!   much was dropped and suggests narrowing the command.
//! * [`truncate_head`] keeps the *first* lines of a file read and reports
//!   where it stopped, so the caller can tell the model to continue with an
//!   `offset`.

/// Lines a tool result may carry before truncation kicks in.
pub(crate) const MAX_LINES: usize = 2000;
/// Bytes a tool result may carry before truncation kicks in.
pub(crate) const MAX_BYTES: usize = 50 * 1024;
/// Characters a single line may carry before it is cut (very long lines are
/// usually minified code or base64 blobs, not signal).
pub(crate) const MAX_LINE_CHARS: usize = 2000;
/// Bytes collected from one pipe of a running command before the rest is
/// discarded (codex's `UNIFIED_EXEC_OUTPUT_MAX_BYTES`).
pub(crate) const COLLECT_MAX_BYTES: usize = 1024 * 1024;

/// Cut one line to [`MAX_LINE_CHARS`], marking the cut.
pub(crate) fn clip_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let clipped: String = line.chars().take(MAX_LINE_CHARS).collect();
    format!("{clipped} …[line cut at {MAX_LINE_CHARS} chars]")
}

/// Keep the tail of `text` within both caps; report what was dropped in a
/// leading notice. Lines are clipped to [`MAX_LINE_CHARS`] either way.
pub(crate) fn truncate_tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= MAX_LINES && text.len() <= MAX_BYTES {
        return lines.iter().map(|line| clip_line(line)).collect::<Vec<_>>().join("\n");
    }
    // Walk backwards until either cap would be exceeded.
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for line in lines.iter().rev() {
        if kept.len() >= MAX_LINES || bytes + line.len() > MAX_BYTES {
            break;
        }
        bytes += line.len() + 1; // the newline that will rejoin it
        kept.push(line);
    }
    kept.reverse();
    let dropped = lines.len() - kept.len();
    let mut out = format!(
        "[... output truncated: {dropped} of {} lines dropped; showing the last {} — narrow the command (grep/tail) for the rest ...]\n",
        lines.len(),
        kept.len()
    );
    out.push_str(&kept.iter().map(|line| clip_line(line)).collect::<Vec<_>>().join("\n"));
    out
}

/// Keep the head of `lines` within both caps. Returns the kept lines
/// (unclipped caller formatting aside) and how many lines were dropped, so
/// the caller can point at the next unread line.
pub(crate) fn truncate_head(lines: &[String], start_line: usize) -> (Vec<String>, usize) {
    let mut kept: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    for line in lines.iter() {
        if kept.len() >= MAX_LINES || bytes + line.len() > MAX_BYTES {
            break;
        }
        bytes += line.len() + 1;
        kept.push(line.clone());
    }
    let dropped = lines.len() - kept.len();
    if dropped == 0 {
        return (kept, 0);
    }
    let next_line = start_line + kept.len();
    let mut out = kept;
    out.push(format!(
        "[... truncated: {dropped} more lines; continue with offset={next_line} ...]"
    ));
    (out, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_passes_through() {
        assert_eq!(truncate_tail("a\nb\nc"), "a\nb\nc");
    }

    #[test]
    fn tail_keeps_the_last_lines_under_the_line_cap() {
        let text: Vec<String> = (0..MAX_LINES + 10).map(|n| format!("line {n}")).collect();
        let out = truncate_tail(&text.join("\n"));
        assert!(out.starts_with("[... output truncated: 10 of "), "{out}");
        assert!(out.contains(&format!("line {}", MAX_LINES + 9)));
        assert!(!out.contains("line 3\n"));
    }

    #[test]
    fn tail_keeps_the_last_lines_under_the_byte_cap() {
        let line = "x".repeat(1000);
        let text: Vec<String> = (0..100).map(|_| line.clone()).collect();
        let out = truncate_tail(&text.join("\n"));
        assert!(out.contains("output truncated"), "{out}");
        // 50KB cap keeps roughly 51 lines, never more than 2000.
        let kept = out.lines().count() - 1; // minus the notice
        assert!(kept <= MAX_LINES);
        assert!(kept < 100);
    }

    #[test]
    fn long_lines_are_clipped() {
        let out = truncate_tail(&"y".repeat(MAX_LINE_CHARS + 100));
        assert!(out.contains("…[line cut at 2000 chars]"));
    }

    #[test]
    fn head_keeps_the_first_lines_and_reports_the_offset() {
        let lines: Vec<String> = (1..=MAX_LINES + 5).map(|n| format!("line {n}")).collect();
        let (kept, dropped) = truncate_head(&lines, 1);
        assert_eq!(dropped, 5);
        assert_eq!(kept.len(), MAX_LINES + 1); // kept + the notice
        assert!(kept.last().unwrap().contains("offset=2001"));
        assert!(kept[0].starts_with("line 1"));
    }

    #[test]
    fn head_within_the_caps_is_untouched() {
        let lines: Vec<String> = (1..=10).map(|n| format!("line {n}")).collect();
        let (kept, dropped) = truncate_head(&lines, 1);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 10);
        assert!(kept.last().unwrap().starts_with("line 10"));
    }
}
