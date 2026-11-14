//! The `/usage` report: detailed context and token consumption, inline in
//! the transcript.
//!
//! The statistics mirror cordis-harness-tui's segmented context bar — the
//! window split per content type (`system` / `prompt` / `assistant` /
//! `thinking` / `tools` / `free`), a plain-text bar, cache hit rates, and the
//! per-turn history. Two number kinds stay strictly apart, as the reference
//! insists:
//!
//! * the **measured** occupancy (the backend's own usage) quotes every total
//!   and fixes the bar's used/free boundary;
//! * the per-type **estimates** ([`crate::core::usage`]) only divide that
//!   measured total, largest-remainder style, so the breakdown's numbers add
//!   up exactly to the readout beside them.
//!
//! The report is a [`ReportBlock`]: pure data, styled by the card renderer
//! like `/context`'s pre-session report.

use crate::core::format_cache_rate;
use crate::core::format_context_usage;
use crate::core::format_tokens;
use crate::core::rollout::UsageTurn;
use crate::core::tokens::cache_percent;
use crate::core::usage::ContextComposition;
use crate::core::usage::attribute_composition;
use crate::core::usage::estimate_tokens;
use crate::core::usage::render_usage_bar;
use crate::plugins::ReportBlock;
use crate::plugins::ReportEntry;
use crate::plugins::ReportGroup;
use crate::tui::chat::HistoryCell;

/// The usage bar's width in columns, inside a report entry.
const BAR_COLUMNS: usize = 30;
/// How many recent turns the report lists before folding the rest into a
/// summary line.
const TURN_ROWS: usize = 10;

/// Everything the report needs, snapshot-style: the active session's
/// measured usage, the transcript cells, and the assembled system prompt.
/// The caller (the app) already holds all of it.
pub struct UsageReportData<'a> {
    pub model: &'a str,
    pub context_window: Option<u64>,
    /// The last measured round: `(input, output)` — `input` doubling as the
    /// current context size, like the footer's readout.
    pub last_usage: Option<(u64, u64)>,
    /// The prompt-cache subset of `last_usage`'s input.
    pub last_cached: Option<u64>,
    pub total_input: u64,
    pub total_output: u64,
    pub total_cached: u64,
    /// Completed turns, oldest first, as the rollout records them.
    pub turns: &'a [UsageTurn],
    /// The assembled system prompt as labeled sections (`base` first).
    pub system_sections: &'a [(String, String)],
    /// The active session's transcript cells, the composition estimates'
    /// source.
    pub cells: &'a [HistoryCell],
}

/// What the context is made of, estimated from the transcript. Weights only —
/// the caller attributes them against the measured total.
fn estimate_cells(data: &UsageReportData<'_>) -> ContextComposition {
    let mut composition = ContextComposition::default();
    for (_, text) in data.system_sections {
        composition.system += estimate_tokens(text);
    }
    for cell in data.cells {
        match cell {
            HistoryCell::User(text) => composition.prompt += estimate_tokens(text),
            HistoryCell::Assistant(text) => composition.assistant += estimate_tokens(text),
            HistoryCell::Thinking { text, .. } => composition.thinking += estimate_tokens(text),
            HistoryCell::ToolCall {
                arguments,
                result,
                error,
                ..
            } => {
                composition.tools += estimate_tokens(arguments);
                composition.tools += estimate_tokens(error.as_deref().unwrap_or(result));
            }
            // Notices, errors, reports: display-only, never reach the model.
            _ => {}
        }
    }
    composition
}

/// A `(name, tokens)` pair of the breakdown legend, `system 1.2k`-style.
fn legend_entry(name: &str, tokens: u64) -> String {
    format!("{name} {}", format_tokens(tokens))
}

/// The `/usage` report: context (with the plain-text bar and the per-type
/// breakdown), the session's measured totals and cache rates, and the
/// per-turn history.
pub fn build_usage_report(data: &UsageReportData<'_>) -> ReportBlock {
    let mut groups: Vec<ReportGroup> = Vec::new();
    let composition = estimate_cells(data);

    // --- Context: the window split per content type ------------------------
    let used = data
        .last_usage
        .map(|(input, output)| input.saturating_add(output));
    let mut context_entries: Vec<ReportEntry> = Vec::new();

    let mut context_line = match used {
        Some(used) => {
            let mut line = format_context_usage(used, data.context_window);
            if data.context_window.is_none() {
                line.push_str(" · window unknown");
            }
            line
        }
        None => "no measured turn yet".to_string(),
    };
    if let Some((input, _)) = data.last_usage
        && let Some(rate) = format_cache_rate(data.last_cached, input)
    {
        context_line.push_str(&format!(" · {rate}"));
    }
    context_entries.push(ReportEntry::new("context", context_line));

    // The bar and the attributed breakdown need a measured occupancy; the
    // free remainder additionally needs a window. Without either, the raw
    // estimates stand in, clearly marked.
    match (used, data.context_window) {
        (Some(used), Some(window)) => {
            let attributed = attribute_composition(&composition, used);
            let names = ContextComposition::SEGMENT_NAMES;
            let mut legend: Vec<String> = names
                .iter()
                .zip([
                    attributed.system,
                    attributed.prompt,
                    attributed.assistant,
                    attributed.thinking,
                    attributed.tools,
                ])
                .filter(|(_, tokens)| *tokens > 0)
                .map(|(name, tokens)| legend_entry(name, tokens))
                .collect();
            // All-zero weights attribute nothing: the measured total stands
            // alone rather than inventing five equal slices.
            if legend.is_empty() {
                legend.push(legend_entry("used", used));
            }
            let free = window.saturating_sub(used);
            if free > 0 {
                legend.push(legend_entry("free", free));
            }
            context_entries.push(ReportEntry::new(
                "bar",
                format!(
                    "{} {}",
                    render_usage_bar(used, window, BAR_COLUMNS),
                    context_entries[0].text
                ),
            ));
            context_entries.push(ReportEntry::new("breakdown", legend.join(" · ")));
        }
        _ => {
            let names = ContextComposition::SEGMENT_NAMES;
            let legend: Vec<String> = names
                .iter()
                .zip([
                    composition.system,
                    composition.prompt,
                    composition.assistant,
                    composition.thinking,
                    composition.tools,
                ])
                .filter(|(_, tokens)| *tokens > 0)
                .map(|(name, tokens)| legend_entry(name, tokens))
                .collect();
            if !legend.is_empty() {
                context_entries.push(ReportEntry::new(
                    "breakdown",
                    format!("{} (estimated)", legend.join(" · ")),
                ));
            }
        }
    }
    groups.push(ReportGroup {
        heading: "Context".to_string(),
        entries: context_entries,
    });

    // --- Session: the measured sums ----------------------------------------
    let mut session_entries = vec![ReportEntry::new(
        "tokens",
        format!(
            "session in {} / out {}",
            format_tokens(data.total_input),
            format_tokens(data.total_output)
        ),
    )];
    // "0%" reads as a measured zero, but a zero sum can equally mean the
    // backend reports no cache accounting at all — silence is the honest
    // shape, matching the footer's omitted cache part.
    if data.total_input > 0 && data.total_cached > 0 {
        let rate = cache_percent(data.total_cached, data.total_input).unwrap_or(0);
        session_entries.push(ReportEntry::new(
            "cache",
            format!(
                "session cache {rate}% ({} cached)",
                format_tokens(data.total_cached)
            ),
        ));
    }
    if let Some((input, output)) = data.last_usage {
        let mut last = format!(
            "in {} / out {}",
            format_tokens(input),
            format_tokens(output)
        );
        if let Some(cached) = data.last_cached {
            last.push_str(&format!(
                " · cached {} ({}%)",
                format_tokens(cached),
                cache_percent(cached, input).unwrap_or(0),
            ));
        }
        session_entries.push(ReportEntry::new("last turn", last));
    }
    groups.push(ReportGroup {
        heading: "Session".to_string(),
        entries: session_entries,
    });

    // --- Turns: the per-turn history ---------------------------------------
    if !data.turns.is_empty() {
        let mut turn_entries: Vec<ReportEntry> = Vec::new();
        if data.turns.len() > TURN_ROWS {
            turn_entries.push(ReportEntry::new(
                "earlier",
                format!("{} more turns", data.turns.len() - TURN_ROWS),
            ));
        }
        for turn in data
            .turns
            .iter()
            .rev()
            .take(TURN_ROWS)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let mut text = format!(
                "in {} / out {}",
                format_tokens(turn.input),
                format_tokens(turn.output)
            );
            if turn.cached.is_some() {
                text.push_str(&format!(
                    " · cached {}",
                    format_tokens(turn.cached.unwrap_or(0))
                ));
            }
            turn_entries.push(ReportEntry::new(
                turn.model.as_deref().unwrap_or("turn"),
                text,
            ));
        }
        groups.push(ReportGroup {
            heading: "Turns".to_string(),
            entries: turn_entries,
        });
    }

    ReportBlock {
        title: "usage".to_string(),
        groups,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::rollout::UsageTurn;

    fn data<'a>(
        cells: &'a [HistoryCell],
        turns: &'a [UsageTurn],
        system_sections: &'a [(String, String)],
        last_usage: Option<(u64, u64)>,
        last_cached: Option<u64>,
        window: Option<u64>,
    ) -> UsageReportData<'a> {
        UsageReportData {
            model: "glm/glm-5.3",
            context_window: window,
            last_usage,
            last_cached,
            total_input: turns.iter().map(|turn| turn.input).sum(),
            total_output: turns.iter().map(|turn| turn.output).sum(),
            total_cached: turns.iter().filter_map(|turn| turn.cached).sum(),
            turns,
            system_sections,
            cells,
        }
    }

    #[test]
    fn breakdown_adds_up_to_the_measured_reading() {
        // 400 ASCII chars ≈ 100 estimated tokens per side, and a flat
        // 100_000 measured input attributes exactly 50_000 to each — whole
        // `k` units, so the parsed legend sums exactly.
        let cells = vec![
            HistoryCell::User("a".repeat(400)),
            HistoryCell::Assistant("b".repeat(400)),
        ];
        let report = build_usage_report(&data(
            &cells,
            &[],
            &[],
            Some((100_000, 0)),
            Some(80_000),
            Some(200_000),
        ));

        let context = &report.groups[0];
        // The bar quotes the same readout as the context entry.
        assert_eq!(context.entries.len(), 3, "{context:?}");
        assert!(
            context.entries[0].text.contains("100.0k/200.0k (50%)"),
            "{context:?}"
        );
        assert!(context.entries[0].text.contains("cache 80%"), "{context:?}");
        assert!(context.entries[1].text.starts_with("█████"), "{context:?}");
        // The legend sums exactly to the measured used, free fills the rest.
        let breakdown = &context.entries[2].text;
        assert!(breakdown.contains("free 100.0k"), "{breakdown:?}");
        let total: u64 = breakdown
            .split(" · ")
            .filter_map(|part| part.split(' ').next_back())
            .filter_map(parse_token_count)
            .sum();
        assert_eq!(total, 200_000);
    }

    /// Parse `842` / `12.3k` / `1.23M` back to tokens (test-only, coarse).
    fn parse_token_count(token: &str) -> Option<u64> {
        if let Some(k) = token.strip_suffix('k') {
            return (k.parse::<f64>().ok()).map(|value| (value * 1_000.0) as u64);
        }
        if let Some(m) = token.strip_suffix('M') {
            return (m.parse::<f64>().ok()).map(|value| (value * 1_000_000.0) as u64);
        }
        token.parse().ok()
    }

    #[test]
    fn without_a_measured_turn_the_estimates_stand_in() {
        let cells = vec![HistoryCell::User("hi".to_string())];
        let report = build_usage_report(&data(&cells, &[], &[], None, None, Some(200_000)));
        let context = &report.groups[0];
        assert_eq!(context.entries[0].text, "no measured turn yet");
        assert!(
            context.entries[1].text.contains("(estimated)"),
            "{context:?}"
        );
        // No bar without an occupancy to anchor it to.
        assert!(context.entries.len() == 2, "{context:?}");
    }

    #[test]
    fn turns_list_folds_older_entries() {
        let turns: Vec<UsageTurn> = (0..14)
            .map(|index| UsageTurn {
                model: Some(format!("m{index}")),
                input: 1_000 + index,
                output: 10,
                cached: Some(500),
            })
            .collect();
        let report = build_usage_report(&data(&[], &turns, &[], Some((1_013, 10)), None, None));
        let turns_group = report
            .groups
            .iter()
            .find(|group| group.heading == "Turns")
            .unwrap();
        assert_eq!(turns_group.entries.len(), TURN_ROWS + 1, "{turns_group:?}");
        assert_eq!(turns_group.entries[0].label, "earlier");
        assert_eq!(turns_group.entries[0].text, "4 more turns");
        // Newest last, matching rollout order.
        assert_eq!(turns_group.entries[1].label, "m4");
        assert_eq!(turns_group.entries[TURN_ROWS].label, "m13");
    }
}
