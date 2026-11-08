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
        assert_eq!(format_context_usage(12_340, Some(200_000)), "12.3k/200.0k (6%)");
        assert_eq!(format_context_usage(190_000, Some(200_000)), "190.0k/200.0k (95%)");
        assert_eq!(format_context_usage(12_340, None), "12.3k tok");
    }
}
