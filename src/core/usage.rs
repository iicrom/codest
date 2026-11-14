//! `/usage`: detailed context and token consumption, distilled from
//! cordis-harness-tui's segmented context bar (its `pi-nano-context`
//! extension).
//!
//! The statistics come in two kinds that must never be conflated:
//!
//! * **Measured** usage — what the backend reported per round
//!   ([`crate::core::rollout::UsageTurn`], the footer's context readout).
//!   Authoritative, and the only thing that ever totals up.
//! * **Estimated** composition — what the context is *made of*: system
//!   prompt, user prompts, assistant text, thinking, tool activity. No
//!   tokenizer is consulted; a script-weighted character heuristic
//!   ([`estimate_tokens`]) produces weights only. The composition never
//!   sums into the measured total — [`attribute_composition`] divides the
//!   measured total itself, largest-remainder style, so the breakdown's
//!   numbers add up exactly to the readout beside them (the reference's
//!   `attributeBarTokens` contract).
//!
//! The command spec below gives every front-end a shared, parseable shape;
//! the report itself is a front-end concern (this crate's TUI renders it
//! from [`ContextComposition`] plus the session's measured usage).

use crate::core::commands::ArgsPolicy;
use crate::core::commands::CommandSpec;

pub static USAGE_COMMAND: CommandSpec = CommandSpec::new(
    "usage",
    &[],
    "/usage",
    "show detailed context consumption: composition, cache hit rates, per-turn history",
    ArgsPolicy::None,
);

/// Usage commands merged into the shipped command table; no plugin owns
/// them, so front-ends dispatch them before asking the plugin host.
pub static USAGE_COMMANDS: &[CommandSpec] = &[USAGE_COMMAND];

// ---------------------------------------------------------------------------
// Script-weighted token estimation (cordis `channel/usage.ts`, ported)
// ---------------------------------------------------------------------------

/// ASCII, including control bytes: BPE vocabularies pack roughly four ASCII
/// characters into one token.
const CHARS_PER_TOKEN_ASCII: f64 = 4.0;
/// CJK ideographs, kana, Hangul, Yi, CJK punctuation and full-width forms:
/// ~1.4 characters per token — the midpoint of the measured 1–1.5 band. The
/// flat English rate under-counts CJK sessions ~3×, which squeezed the whole
/// composition flat against the measured reading (the reference's issue
/// #1170).
const CHARS_PER_TOKEN_CJK: f64 = 1.4;
/// Everything else — accented Latin, Cyrillic, emoji — deliberately
/// conservative at two characters per token: denser scripts still come out
/// low, while lighter ones are over-counted rather than under-counted. Since
/// these numbers only weight the fills, the bias lands on segment widths,
/// never on the quoted total.
const CHARS_PER_TOKEN_OTHER: f64 = 2.0;

/// Whether one code point belongs to a script BPE vocabularies pack at
/// roughly one token per one or two characters. Emoji are deliberately
/// excluded (they land in the [`CHARS_PER_TOKEN_OTHER`] rate): a ZWJ cluster
/// is several code points the tokenizer may charge several tokens for.
fn is_dense_script(code_point: u32) -> bool {
    matches!(code_point,
        0x1100..=0x11ff          // Hangul Jamo
        | 0x2e80..=0x303f        // CJK radicals … CJK symbols and punctuation
        | 0x3040..=0x33ff        // kana, Bopomofo, Hangul compat jamo, enclosed/compat CJK
        | 0x3400..=0x4dbf        // CJK unified ideographs extension A
        | 0x4e00..=0x9fff        // CJK unified ideographs
        | 0xa000..=0xa4cf        // Yi syllables and radicals
        | 0xa960..=0xa97f        // Hangul Jamo Extended-A
        | 0xac00..=0xd7af        // Hangul syllables
        | 0xd7b0..=0xd7ff        // Hangul Jamo Extended-B
        | 0xf900..=0xfaff        // CJK compatibility ideographs
        | 0xfe30..=0xfe4f        // CJK compatibility forms
        | 0xfe50..=0xfe6f        // small form variants (CJK punctuation variants)
        | 0xff00..=0xff60        // full-width forms
        | 0xff61..=0xffdc        // half-width kana, half-width Hangul, half-width CJK punctuation
        | 0xffe0..=0xffe6        // full-width signs
        | 0x1f200..=0x1f2ff      // enclosed ideographic supplement
        | 0x20000..=0x3fffd      // CJK extensions B–G
    )
}

/// The same script-weighted estimate as [`estimate_tokens`], without the
/// per-call rounding: a fractional count that stays additive over fragments,
/// so summing per-message estimates differs from rounding the whole at once
/// by at most one token per call.
pub fn estimate_tokens_fraction(text: &str) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    // Fast path: pure ASCII is exactly the legacy rate, and the scan keeps
    // the overwhelmingly common English case cheaper than a code-point walk.
    if text.is_ascii() {
        return text.chars().count() as f64 / CHARS_PER_TOKEN_ASCII;
    }
    let (mut ascii, mut dense, mut other) = (0u64, 0u64, 0u64);
    for character in text.chars() {
        let code_point = character as u32;
        if code_point < 0x80 {
            ascii += 1;
        } else if is_dense_script(code_point) {
            dense += 1;
        } else {
            other += 1;
        }
    }
    ascii as f64 / CHARS_PER_TOKEN_ASCII
        + dense as f64 / CHARS_PER_TOKEN_CJK
        + other as f64 / CHARS_PER_TOKEN_OTHER
}

/// Context-composition token estimate for model-visible text: a per-code-point
/// weighted count by script class, rounded up.
///
/// Pure and deterministic: non-negative, zero only for the empty string, and
/// monotonic — appending characters never lowers the result. This is an
/// ESTIMATE, not metering: no tokenizer is consulted, and the guess cannot
/// see the provider vocabulary, the request envelope's role markers, or a
/// cached prefix. The authoritative occupancy is the backend's own usage;
/// these numbers only describe what the breakdown is made of.
pub fn estimate_tokens(text: &str) -> u64 {
    estimate_tokens_fraction(text).ceil() as u64
}

// ---------------------------------------------------------------------------
// Composition weights and attribution
// ---------------------------------------------------------------------------

/// What the context window is made of, as local estimate weights per content
/// type — the five used segments of the reference's context bar, in bar
/// order. Weights only: never summed into the measured total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextComposition {
    /// The system prompt: base prompt plus every plugin fragment.
    pub system: u64,
    /// User prompts.
    pub prompt: u64,
    /// Assistant text.
    pub assistant: u64,
    /// Reasoning content (display metadata; it reaches the model only if the
    /// provider's transcript keeps it — the estimate weights it regardless,
    /// like the reference's thinking segment).
    pub thinking: u64,
    /// Tool call arguments and tool results (failures included).
    pub tools: u64,
}

impl ContextComposition {
    /// The weights as one array, in bar order.
    fn weights(&self) -> [u64; 5] {
        [
            self.system,
            self.prompt,
            self.assistant,
            self.thinking,
            self.tools,
        ]
    }

    /// Build from a weights array in bar order.
    fn from_weights(weights: [u64; 5]) -> Self {
        Self {
            system: weights[0],
            prompt: weights[1],
            assistant: weights[2],
            thinking: weights[3],
            tools: weights[4],
        }
    }

    /// Segment display names, in bar order — the breakdown legend's long
    /// forms (the reference's `USED_SEGMENTS` labels).
    pub const SEGMENT_NAMES: [&'static str; 5] =
        ["system", "prompt", "assistant", "thinking", "tools"];
}

/// Attribute the composition's weights to the one authoritative token total:
/// largest remainders, integers that sum EXACTLY to it, so the breakdown's
/// numbers add up to the measured reading instead of reporting a second,
/// unrelated total (the reference's `attributeBarTokens`).
///
/// All-zero weights — nothing estimated at all — attribute nothing: the
/// caller shows the measured total unattributed rather than inventing five
/// equal slices. A zero `measured_total` likewise attributes nothing.
pub fn attribute_composition(
    weights: &ContextComposition,
    measured_total: u64,
) -> ContextComposition {
    let values = weights.weights();
    let sum: u64 = values.iter().sum();
    if sum == 0 || measured_total == 0 {
        return ContextComposition::default();
    }
    let raw: [f64; 5] =
        std::array::from_fn(|index| values[index] as f64 / sum as f64 * measured_total as f64);
    let mut allocated: [u64; 5] = std::array::from_fn(|index| raw[index].floor() as u64);
    // Float drift could push the floors' sum past the budget; saturate so the
    // remainder never wraps.
    let mut remaining = measured_total.saturating_sub(allocated.iter().sum());
    // Hand the leftover columns to the largest fractional remainders.
    let mut order: [usize; 5] = [0, 1, 2, 3, 4];
    order.sort_by(|left, right| {
        raw[*right]
            .fract()
            .partial_cmp(&raw[*left].fract())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for index in order {
        if remaining == 0 {
            break;
        }
        allocated[index] += 1;
        remaining -= 1;
    }
    ContextComposition::from_weights(allocated)
}

/// The segmented context bar as one plain-text line: `█` per used column,
/// `░` per free column. The occupancy fixes the used/free boundary — a
/// measured total beyond the window fills the whole bar, like the reference's
/// occupancy ratio capped at 1.
///
/// Plain characters on purpose: a report entry carries text, not styled
/// fills, and the legend beside it names the numbers.
pub fn render_usage_bar(used: u64, window: u64, columns: usize) -> String {
    let columns = columns.max(1);
    let ratio = match window {
        0 => 1.0,
        window => (used.min(window) as f64 / window as f64).clamp(0.0, 1.0),
    };
    let used_columns = ((ratio * columns as f64).round() as usize).min(columns);
    format!(
        "{}{}",
        "█".repeat(used_columns),
        "░".repeat(columns - used_columns)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_is_the_legacy_rate_on_ascii() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("hello world!"), 3); // 12 chars, ceil(3.0)
        assert_eq!(estimate_tokens("hello world x"), 4); // 13 chars, ceil(3.25)
    }

    #[test]
    fn estimate_tokens_weights_dense_scripts() {
        // One CJK ideograph ≈ 0.71 tokens, but the per-call ceil keeps whole
        // tokens: two ideographs ≈ 1.43 → 2, six ≈ 4.29 → 5.
        assert_eq!(estimate_tokens("你好"), 2);
        assert_eq!(estimate_tokens("你好世界再见"), 5);
        // Mixed: 4 ASCII (1 token) + 1 CJK (0.71) = 1.71 → 2.
        assert_eq!(estimate_tokens("abcd你"), 2);
        // Emoji land in the conservative "other" rate.
        assert_eq!(estimate_tokens("👍"), 1);
    }

    #[test]
    fn estimate_tokens_fraction_is_additive() {
        let (left, right) = ("hello ", "世界");
        let combined = estimate_tokens_fraction(&format!("{left}{right}"));
        let summed = estimate_tokens_fraction(left) + estimate_tokens_fraction(right);
        assert!((combined - summed).abs() < 1e-9);
    }

    #[test]
    fn attribute_sums_exactly_to_the_measured_total() {
        let weights = ContextComposition {
            system: 1_200,
            prompt: 3_400,
            assistant: 5_000,
            thinking: 2_000,
            tools: 4_100,
        };
        let total = 12_345;
        let attributed = attribute_composition(&weights, total);
        let sum: u64 = [
            attributed.system,
            attributed.prompt,
            attributed.assistant,
            attributed.thinking,
            attributed.tools,
        ]
        .into_iter()
        .sum();
        assert_eq!(sum, total);
        // Proportions hold within a token of the exact share.
        let sum_weights: u64 = 15_700;
        for (weight, attributed) in [
            (weights.system, attributed.system),
            (weights.prompt, attributed.prompt),
            (weights.assistant, attributed.assistant),
            (weights.thinking, attributed.thinking),
            (weights.tools, attributed.tools),
        ] {
            let expected = weight as f64 / sum_weights as f64 * total as f64;
            assert!(
                (attributed as f64 - expected).abs() <= 1.0,
                "{attributed} vs {expected}"
            );
        }
    }

    #[test]
    fn attribute_nothing_when_there_is_nothing_to_weight() {
        assert_eq!(
            attribute_composition(&ContextComposition::default(), 500),
            ContextComposition::default()
        );
        let weights = ContextComposition {
            system: 10,
            ..Default::default()
        };
        assert_eq!(
            attribute_composition(&weights, 0),
            ContextComposition::default()
        );
    }

    #[test]
    fn usage_bar_spans_the_occupancy() {
        assert_eq!(render_usage_bar(5_000, 10_000, 10), "█████░░░░░");
        assert_eq!(render_usage_bar(0, 10_000, 4), "░░░░");
        // Over-occupancy fills the whole bar, never overflows it.
        assert_eq!(render_usage_bar(20_000, 10_000, 4), "████");
        assert_eq!(render_usage_bar(1_000, 0, 4), "████");
    }

    #[test]
    fn usage_command_parses_without_arguments() {
        match crate::core::commands::CommandTable::new(USAGE_COMMANDS.iter())
            .expect("valid")
            .parse("/usage")
        {
            crate::core::commands::CommandInput::Command(invocation) => {
                assert_eq!(invocation.spec.name, "usage");
                assert!(invocation.args.is_empty());
            }
            other => panic!("expected a command, got {other:?}"),
        }
    }
}
