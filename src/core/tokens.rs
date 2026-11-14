//! Token-count formatting shared by the TUI footer and plugin output.

/// Format a token count for compact display: `842`, `12.3k`, `1.23M`.
pub fn format_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{:.2}M", tokens as f64 / 1_000_000.0)
    }
}

/// Occupancy of a context window in whole percent, capped at 100. A zero
/// window counts as fully used.
pub fn context_percent(used: u64, window: u64) -> u8 {
    used.saturating_mul(100)
        .checked_div(window)
        .unwrap_or(100)
        .min(100) as u8
}

/// The compact context readout: `12.3k/200.0k (6%)` with a known window,
/// `12.3k tok` when only the usage is measured.
pub fn format_context_usage(used: u64, window: Option<u64>) -> String {
    match window {
        Some(window) => format!(
            "{}/{} ({}%)",
            format_tokens(used),
            format_tokens(window),
            context_percent(used, window)
        ),
        None => format!("{} tok", format_tokens(used)),
    }
}

/// Occupancy of the prompt cache in whole percent — `cached / input`,
/// capped at 100. `None` when the round reports no cache accounting or
/// carries no input at all (a hit rate against nothing is meaningless).
pub fn cache_percent(cached: u64, input: u64) -> Option<u8> {
    (input > 0).then(|| {
        cached
            .saturating_mul(100)
            .checked_div(input)
            .unwrap_or(0)
            .min(100) as u8
    })
}

/// The compact cache readout for a usage pair: `cache 80%`. `None` when
/// there is nothing to rate — no reported cached tokens, or a zero input.
/// Paired with [`format_context_usage`], it extends the footer's context
/// segment and the `/usage` report alike.
pub fn format_cache_rate(cached: Option<u64>, input: u64) -> Option<String> {
    cached
        .and_then(|cached| cache_percent(cached, input))
        .map(|pct| format!("cache {pct}%"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tokens_scales_units() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(842), "842");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(12_340), "12.3k");
        assert_eq!(format_tokens(200_000), "200.0k");
        assert_eq!(format_tokens(999_999), "1000.0k");
        assert_eq!(format_tokens(1_000_000), "1.00M");
        assert_eq!(format_tokens(12_345_678), "12.35M");
    }

    #[test]
    fn context_percent_caps_at_100() {
        assert_eq!(context_percent(0, 200_000), 0);
        assert_eq!(context_percent(20_000, 200_000), 10);
        assert_eq!(context_percent(190_000, 200_000), 95);
        assert_eq!(context_percent(300_000, 200_000), 100);
        assert_eq!(context_percent(1, 0), 100);
    }

    #[test]
    fn format_context_usage_adapts_to_a_known_window() {
        assert_eq!(
            format_context_usage(12_340, Some(200_000)),
            "12.3k/200.0k (6%)"
        );
        assert_eq!(
            format_context_usage(190_000, Some(200_000)),
            "190.0k/200.0k (95%)"
        );
        assert_eq!(format_context_usage(12_340, None), "12.3k tok");
    }

    #[test]
    fn cache_percent_rates_against_input() {
        assert_eq!(cache_percent(80, 100), Some(80));
        assert_eq!(cache_percent(0, 100), Some(0));
        assert_eq!(cache_percent(100, 100), Some(100));
        // A misbehaving backend reporting more cached than input stays capped.
        assert_eq!(cache_percent(200, 100), Some(100));
        // No input to rate against: not a 0% hit rate, just unknown.
        assert_eq!(cache_percent(0, 0), None);
    }

    #[test]
    fn format_cache_rate_needs_both_sides() {
        assert_eq!(
            format_cache_rate(Some(80), 100).as_deref(),
            Some("cache 80%")
        );
        assert_eq!(format_cache_rate(None, 100), None);
        assert_eq!(format_cache_rate(Some(80), 0), None);
    }
}
