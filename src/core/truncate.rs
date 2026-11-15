//! Tool-output truncation for the conversation the model sees.
//!
//! codest keeps two copies of every tool result, like codex: the rollout
//! file stores the full payload, while the history that reaches the model is
//! truncated at record time. This module is the policy for the second copy —
//! a per-output budget in estimated tokens ([`crate::core::usage`]'s
//! script-weighted estimate, the same accounting `/usage` renders) applied
//! where a result enters the conversation: live at execution time, and again
//! when a rollout replays, so a restored session carries the same truncated
//! shape a live one would have.
//!
//! The cut keeps the head and the tail, split evenly over the budget, and
//! names the dropped middle. Beginnings carry the command echo and its
//! setup; endings carry the error, the total, or the summary — the middle of
//! a long listing is the part a model almost never needs.

use crate::core::usage::estimate_char_fraction;

/// The default per-output budget, in estimated tokens. Generous enough that
/// ordinary reads never feel it, tight enough that a runaway dump (a binary
/// piped through a log, a whole tree `cat`ed) cannot swallow the window.
/// `config.toml`'s `tool_output_token_limit` overrides it; `0` disables.
pub const DEFAULT_TOOL_OUTPUT_TOKEN_LIMIT: usize = 4_000;

const TRUNCATED_MARKER: &str = " tokens truncated …";

/// The budget set aside for the notice itself, in estimated tokens — codex
/// reserves a serialization allowance for the same reason. Without it the
/// marker's own cost would push a trimmed output just over the limit, and
/// trimming it again would shave the halves every time. The longest
/// conceivable notice (`… 999999999 tokens truncated …` between newlines) is
/// ~45 ASCII chars ≈ 11 tokens; 32 is a generous ceiling.
const MARKER_RESERVE_TOKENS: f64 = 32.0;

/// One tool result as the model sees it: unchanged within the budget, else
/// head + marker + tail. The marker's count is the estimated tokens of the
/// dropped middle, so the notice reads in the same unit as the budget; the
/// halves are sized so the whole trimmed result — marker included — stays
/// within the budget.
pub fn truncate_tool_output(content: &str, limit_tokens: usize) -> String {
    if limit_tokens == 0 || content.is_empty() {
        return content.to_string();
    }
    let total = estimate_tokens_of(content);
    if total.ceil() as usize <= limit_tokens {
        return content.to_string();
    }
    let half = ((limit_tokens as f64 - MARKER_RESERVE_TOKENS) / 2.0).max(1.0);
    let head_end = head_boundary(content, half);
    let tail_start = tail_boundary(content, half).max(head_end);
    let head = &content[..head_end];
    let tail = &content[tail_start..];
    let dropped = (total - estimate_tokens_of(head) - estimate_tokens_of(tail))
        .ceil()
        .max(0.0) as u64;
    format!("{head}\n… {dropped}{TRUNCATED_MARKER}\n{tail}")
}

/// The whole-string fractional estimate, kept local so the boundaries and
/// the dropped count are computed from the same additive unit
/// ([`estimate_char_fraction`]) the per-char walks use.
fn estimate_tokens_of(text: &str) -> f64 {
    crate::core::usage::estimate_tokens_fraction(text)
}

/// The byte offset ending the head: the longest prefix whose estimate stays
/// within `budget`, always on a char boundary.
fn head_boundary(content: &str, budget: f64) -> usize {
    let mut spent = 0.0;
    for (offset, character) in content.char_indices() {
        let next = spent + estimate_char_fraction(character);
        if next > budget {
            return offset;
        }
        spent = next;
    }
    content.len()
}

/// The byte offset starting the tail: the longest suffix (walked from the
/// end) whose estimate stays within `budget`, always on a char boundary.
fn tail_boundary(content: &str, budget: f64) -> usize {
    let mut spent = 0.0;
    let mut boundary = content.len();
    for (offset, character) in content.char_indices().rev() {
        let next = spent + estimate_char_fraction(character);
        if next > budget {
            return boundary;
        }
        spent = next;
        boundary = offset;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_result(content: &str) -> String {
        truncate_tool_output(content, DEFAULT_TOOL_OUTPUT_TOKEN_LIMIT)
    }

    /// ASCII is 4 chars per estimated token, so a 110k-char dump busts the
    /// 4k default and comes back head+tail only. The cut is char-precise,
    /// not line-precise: the head starts at the first line, the tail ends
    /// inside one.
    #[test]
    fn a_oversized_output_is_cut_to_head_and_tail() {
        let line = "0123456789\n".repeat(10_000); // 110k chars ≈ 27.5k tokens
        let truncated = tool_result(&line);
        assert!(truncated.len() < line.len());
        assert!(truncated.starts_with("0123456789"));
        // The tail is a suffix of the output, so it ends with the output's
        // own last line — not with a marker or a partial character.
        assert!(truncated.ends_with("0123456789\n"));
        assert!(truncated.contains("tokens truncated …"));
        // The kept halves fit the budget: ~2k tokens each ≈ 8k chars, plus
        // the marker.
        assert!(truncated.len() < 20_000, "kept {} bytes", truncated.len());
    }

    /// Nothing under the budget changes, byte for byte.
    #[test]
    fn an_output_within_the_budget_is_untouched() {
        let content = "short output\n".repeat(100);
        assert_eq!(tool_result(&content), content);
    }

    /// The head and tail keep whole lines here — the boundary lands inside
    /// the repeated line and the marker carries the dropped middle's count.
    #[test]
    fn the_marker_names_the_dropped_middle() {
        let content = "x".repeat(40_000); // 10k tokens over a 4k budget
        let truncated = tool_result(&content);
        let marker = truncated
            .lines()
            .find(|line| line.contains("tokens truncated"))
            .expect("a marker line");
        let dropped: u64 = marker
            .trim_start_matches("… ")
            .trim_end_matches(TRUNCATED_MARKER)
            .trim()
            .parse()
            .expect("a token count");
        assert!(dropped > 5_000, "expected most of 10k dropped, got {dropped}");
    }

    /// CJK counts at roughly one token per 1.4 chars, so a Chinese dump
    /// busts the budget far sooner — the split must land on char boundaries.
    #[test]
    fn a_dense_script_output_is_cut_on_char_boundaries() {
        let content = "编译错误".repeat(4_000); // 16k chars ≈ 11.4k tokens
        let truncated = tool_result(&content);
        assert!(truncated.contains("tokens truncated …"));
        // Head and tail are whole repeats — no partial characters.
        assert!(truncated.starts_with("编译错误"));
        assert!(truncated.ends_with("编译错误"));
        assert!(truncated.chars().all(|c| c == '编' || c == '译' || c == '错' || c == '误' || !c.is_alphabetic() || c.is_ascii()));
    }

    /// A zero limit (or an empty string) is a no-op: `0` is the documented
    /// off switch.
    #[test]
    fn a_zero_limit_disables_truncation() {
        let content = "x".repeat(100_000);
        assert_eq!(truncate_tool_output(&content, 0), content);
        assert_eq!(truncate_tool_output("", 4_000), "");
    }

    /// A tiny limit still produces usable head+tail rather than only a
    /// marker: the halves never overlap, and nothing is lost mid-char.
    #[test]
    fn a_tiny_limit_keeps_both_halves() {
        let content = "abcdefgh".repeat(100); // 800 chars ≈ 200 tokens
        let truncated = truncate_tool_output(&content, 2);
        assert!(truncated.contains("tokens truncated …"));
        assert!(truncated.starts_with("ab"));
        assert!(truncated.ends_with("gh"));
    }

    /// The error content path is what the budget is for: a failed call that
    /// dumped a huge log keeps its beginning (the error) and its end (the
    /// summary), which is what a model can act on.
    #[test]
    fn truncation_is_idempotent() {
        let content = "y".repeat(40_000);
        let once = tool_result(&content);
        let twice = truncate_tool_output(&once, DEFAULT_TOOL_OUTPUT_TOKEN_LIMIT);
        assert_eq!(once, twice, "re-truncating a truncated output is a no-op");
    }
}
