//! `/tps`: a live tokens-per-second meter for the footer.
//!
//! This is the reference plugin for the event stream and the status-bar
//! segments: it subscribes to deltas and usage, derives a throughput
//! readout from [`PluginEvent::at`] stamps, and publishes it as
//! `tps:rate` while streaming and a `tps:spark` sparkline of recent turn
//! throughputs while idle.
//!
//! The metering follows the shape proven by the reference TUIs' tps meters
//! (pi-tps-meter via cordis-harness-tui): a *decode-span* throughput, not a
//! wall-clock average — the span starts at the first streamed token and
//! pauses across tool execution, so a turn that spends a minute running
//! `bash` does not report a degraded t/s. While deltas stream, the readout
//! is a script-weighted estimate; when the backend's usage arrives, the
//! estimate is corrected with the real output count (summed over the turn's
//! rounds — the worker's completed-turn usage only carries the last one).
//!
//! Every finished turn leaves one sample in a small ring, which is what the
//! idle sparkline renders. The sparkline is a shape, not a number: a single
//! sample has nothing to compare against and stays hidden, and switching
//! sessions clears the meter — throughput describes the conversation on
//! screen, so carrying it across a switch would report someone else's turns.
//!
//! Off by default: `/tps` toggles it for the run, and
//!
//! ```toml
//! [plugins.tps]
//! enabled = true
//! ```
//!
//! starts it on. `/settings` edits that same key: the row writes
//! `[plugins.tps] enabled` through the host, the file is the record, and
//! [`Plugin::on_settings_changed`] flips the meter in place — no restart, and
//! the choice survives the next run. `/tps` stays the keyboard shortcut for
//! the same switch; it only changes this run (the file is not rewritten from
//! a slash command).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use cst_plugin_api::settings::SettingField;
use cst_plugin_api::settings::SettingsSection;

use crate::plugins::ArgsPolicy;
use crate::plugins::CommandSpec;
use crate::plugins::EventSubscription;
use crate::plugins::Invocation;
use crate::plugins::Plugin;
use crate::plugins::PluginContext;
use crate::plugins::PluginError;
use crate::plugins::PluginEvent;
use crate::plugins::StatusSegment;
use crate::plugins::StatusTone;

pub static TPS: Tps = Tps {
    state: Mutex::new(TpsState {
        enabled: false,
        turn: None,
        live: None,
        samples: VecDeque::new(),
    }),
};

/// A decode span shorter than this is noise: a couple of tokens, or a turn
/// that finished before the first delta settled. Below it the meter shows
/// nothing rather than a wildly unstable number.
const MIN_DECODE_MS: u128 = 500;
/// Throughput tones, ported from the reference meters: fast enough reads as
/// healthy, slow enough reads as a warning, in between is quiet.
const FAST_TPS: f64 = 50.0;
const SLOW_TPS: f64 = 20.0;
/// Idle sparkline: the last twelve per-turn samples.
const SPARK_SAMPLES: usize = 12;
/// The per-turn sample ring. The sparkline shows a dozen; the ring keeps a
/// little history for a future stats readout without growing unbounded.
const SAMPLE_CAP: usize = 500;
const SPARK_BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

static COMMANDS: [CommandSpec; 1] = [CommandSpec::new(
    "tps",
    &[],
    "/tps",
    "toggle the tokens-per-second status meter",
    ArgsPolicy::None,
)];

/// What `/settings` offers for this plugin: the one key it reads at startup
/// and applies live. The description says both, because the row is the only
/// place the user learns that `/tps` leaves the file alone.
static SETTINGS_FIELDS: [SettingField; 1] = [SettingField::toggle(
    "enabled",
    "Tokens-per-second meter",
    "Show the live t/s readout while streaming and the per-turn sparkline \
     while idle. Saved here, so it survives a restart; /tps flips the meter \
     for this run only.",
    false,
)];

static SETTINGS_SECTION: SettingsSection =
    SettingsSection::new("tps", "throughput meter", &SETTINGS_FIELDS);

/// One turn's metering: decode time from closed steps, the streamed-text
/// estimate, and the backend's real output once it arrives.
struct TurnMeter {
    /// Decode time of finished steps (each tool call closes one).
    decode_ms: u128,
    /// Start of the in-flight step, `None` while no deltas are streaming.
    step_started: Option<Instant>,
    /// Output the backend counted for rounds that already settled. The
    /// completed turn's `TurnUsage` only carries the *last* round, while the
    /// decode span covers every round — summing the per-round counts is what
    /// keeps a tool-heavy turn from reporting a fraction of its real speed.
    settled_output: u64,
    /// The script-weighted estimate for the round still streaming.
    live_estimate: f64,
    /// The completed turn's own count, the fallback for backends that report
    /// usage only once at the end.
    real_output: Option<u64>,
}

impl TurnMeter {
    fn new() -> Self {
        Self {
            decode_ms: 0,
            step_started: None,
            settled_output: 0,
            live_estimate: 0.0,
            real_output: None,
        }
    }

    /// Close the in-flight step at `at`, if one is open.
    fn close_step(&mut self, at: Instant) {
        if let Some(started) = self.step_started.take() {
            self.decode_ms += at.saturating_duration_since(started).as_millis();
        }
    }

    /// Total decode time at `at`, including the open step.
    fn decode_ms_at(&self, at: Instant) -> u128 {
        let open = self
            .step_started
            .map(|started| at.saturating_duration_since(started).as_millis())
            .unwrap_or(0);
        self.decode_ms + open
    }

    /// Output tokens this turn has produced so far: counted rounds plus the
    /// estimate for the round still streaming. Falls back to the turn's own
    /// count when the backend reported no per-round usage.
    fn tokens(&self) -> f64 {
        if self.settled_output > 0 {
            self.settled_output as f64 + self.live_estimate
        } else if let Some(real) = self.real_output {
            real as f64
        } else {
            self.live_estimate
        }
    }

    /// The throughput readout over the decode span, or `None` below the noise
    /// floor — or for a span that produced nothing.
    fn tps(&self, at: Instant) -> Option<f64> {
        let decode_ms = self.decode_ms_at(at);
        let tokens = self.tokens();
        if decode_ms < MIN_DECODE_MS || tokens <= 0.0 {
            return None;
        }
        Some(tokens / (decode_ms as f64 / 1000.0))
    }
}

struct TpsState {
    enabled: bool,
    turn: Option<TurnMeter>,
    live: Option<f64>,
    samples: VecDeque<f64>,
}

impl TpsState {
    fn new() -> Self {
        Self {
            enabled: false,
            turn: None,
            live: None,
            samples: VecDeque::new(),
        }
    }
}

pub struct Tps {
    state: Mutex<TpsState>,
}

impl Tps {
    /// Whether the meter is on for this run — the reading `/tps` flips and
    /// `/settings` persists.
    pub fn is_enabled(&self) -> bool {
        self.state.lock().expect("tps state").enabled
    }

    /// Return the shared [`TPS`] instance to its startup state (off, no
    /// metering). The static is process-wide, so a test that flips it has to
    /// put it back or it leaks into every other test sharing the shipped host.
    #[cfg(test)]
    pub fn reset_for_test(&self) {
        *self.state.lock().expect("tps state") = TpsState::new();
    }

    fn publish_rate(&self, state: &TpsState, ctx: &mut PluginContext<'_>) {
        match state.live {
            Some(tps) => {
                let tone = if tps >= FAST_TPS {
                    StatusTone::Accent
                } else if tps < SLOW_TPS {
                    StatusTone::Warn
                } else {
                    StatusTone::Dim
                };
                ctx.set_status(StatusSegment::with_tone("rate", format_tps(tps), tone));
            }
            None => ctx.clear_status("rate"),
        }
    }

    fn publish_spark(&self, state: &TpsState, ctx: &mut PluginContext<'_>) {
        // One sample is not a trend: min-max normalization would render it as
        // a mid-height block and the footer would carry a lone number that
        // never changes. The sparkline appears once there is something to
        // compare, and it stays a shape — the numeric readout is the live
        // rate's job, and that retires with its turn.
        if state.samples.len() < 2 {
            ctx.clear_status("spark");
            return;
        }
        ctx.set_status(StatusSegment::new("spark", sparkline(&state.samples)));
    }
}

impl Plugin for Tps {
    fn id(&self) -> &'static str {
        "tps"
    }

    fn commands(&self) -> &'static [CommandSpec] {
        &COMMANDS
    }

    fn settings(&self) -> Option<SettingsSection> {
        Some(SETTINGS_SECTION)
    }

    fn on_settings_changed(
        &self,
        settings: Option<&crate::plugins::PluginSettings>,
        ctx: &mut PluginContext<'_>,
    ) {
        // The file is the record; this is the live half. A pocket with no
        // `enabled` (the key was removed) means the plugin's default: off.
        let enabled = settings
            .and_then(|settings| settings.option_bool("enabled"))
            .unwrap_or(false);
        let mut state = self.state.lock().expect("tps state");
        if state.enabled == enabled {
            // Idempotent: a save that repeats the stored value says nothing.
            return;
        }
        state.enabled = enabled;
        if enabled {
            // Anything streamed before the switch was never metered, so the
            // turn starts from here rather than reporting a bogus span.
            state.turn = None;
            state.live = None;
            ctx.notice("tps meter on");
            self.publish_spark(&state, ctx);
        } else {
            state.turn = None;
            state.live = None;
            ctx.notice("tps meter off");
            ctx.clear_status("rate");
            ctx.clear_status("spark");
        }
    }

    fn subscriptions(&self) -> EventSubscription {
        EventSubscription::DELTAS
            .union(EventSubscription::USAGE)
            .union(EventSubscription::LIFECYCLE)
            .union(EventSubscription::SESSION)
    }

    fn on_startup(&self, ctx: &mut PluginContext<'_>) {
        let enabled = ctx
            .plugin_settings()
            .and_then(|settings| settings.option_bool("enabled"))
            .unwrap_or(false);
        self.state.lock().expect("tps state").enabled = enabled;
    }

    fn on_event(&self, event: &PluginEvent<'_>, ctx: &mut PluginContext<'_>) {
        let mut state = self.state.lock().expect("tps state");
        if !state.enabled {
            return;
        }
        let at = event.at;
        match event.kind {
            crate::plugins::PluginEventKind::TurnStarted => {
                state.turn = Some(TurnMeter::new());
                state.live = None;
            }
            crate::plugins::PluginEventKind::MessageDelta { text, .. } => {
                let turn = state.turn.get_or_insert_with(TurnMeter::new);
                if turn.step_started.is_none() {
                    turn.step_started = Some(at);
                }
                turn.live_estimate += estimate_tokens(text);
                state.live = turn.tps(at);
                self.publish_rate(&state, ctx);
            }
            crate::plugins::PluginEventKind::RoundUsage { output, .. } => {
                // A round's stream ended (a tool round, typically): bank its
                // counted output and close the step, so the estimate for that
                // round stops accumulating and tool execution stays out of
                // the decode span.
                if let Some(turn) = state.turn.as_mut() {
                    turn.settled_output = turn.settled_output.saturating_add(output);
                    turn.live_estimate = 0.0;
                    turn.close_step(at);
                }
            }
            crate::plugins::PluginEventKind::TurnUsage { output, .. } => {
                if let Some(turn) = state.turn.as_mut() {
                    turn.close_step(at);
                    // A backend that reports per round already banked them;
                    // one that speaks only at turn end gets its own count.
                    if turn.settled_output == 0 {
                        turn.real_output = Some(output);
                    }
                }
                state.live = state.turn.as_ref().and_then(|turn| turn.tps(at));
                self.publish_rate(&state, ctx);
            }
            crate::plugins::PluginEventKind::ToolCallStarted { .. } => {
                if let Some(turn) = state.turn.as_mut() {
                    turn.close_step(at);
                }
            }
            crate::plugins::PluginEventKind::TurnComplete
            | crate::plugins::PluginEventKind::TurnAborted { .. } => {
                if let Some(mut turn) = state.turn.take() {
                    turn.close_step(at);
                    if let Some(tps) = turn.tps(at) {
                        state.samples.push_back(tps);
                        while state.samples.len() > SAMPLE_CAP {
                            state.samples.pop_front();
                        }
                    }
                }
                state.live = None;
                self.publish_rate(&state, ctx);
                self.publish_spark(&state, ctx);
            }
            crate::plugins::PluginEventKind::ConversationCleared
            | crate::plugins::PluginEventKind::SessionStarted { .. }
            | crate::plugins::PluginEventKind::SessionSwitched { .. } => {
                // Both mean "the turns you metered are no longer on screen":
                // `/clear` dropped them, and a session switch swapped in
                // another conversation. Carrying the samples across would
                // report throughput for turns the user cannot see — and the
                // sparkline would follow you from session to session.
                let enabled = state.enabled;
                *state = TpsState::new();
                state.enabled = enabled;
                ctx.clear_status("rate");
                ctx.clear_status("spark");
            }
            _ => {}
        }
    }

    fn run_command(
        &self,
        _invocation: &Invocation,
        ctx: &mut PluginContext<'_>,
    ) -> Result<(), PluginError> {
        let mut state = self.state.lock().expect("tps state");
        state.enabled = !state.enabled;
        if state.enabled {
            ctx.notice("tps meter on");
            if state.live.is_some() {
                self.publish_rate(&state, ctx);
            }
            self.publish_spark(&state, ctx);
        } else {
            ctx.notice("tps meter off");
            ctx.clear_status("rate");
            ctx.clear_status("spark");
        }
        Ok(())
    }
}

/// A script-weighted token estimate for streamed text: CJK characters are
/// roughly one token each, other scripts roughly four characters per token.
/// A plain chars/4 rule under-counted CJK by about three times (the
/// reference meters' notes); the ratio only needs to be honest enough for a
/// live readout, because the usage event corrects it with the backend's
/// real count.
fn estimate_tokens(text: &str) -> f64 {
    let mut cjk = 0.0_f64;
    let mut other = 0.0_f64;
    for ch in text.chars() {
        if is_cjk_script(ch) {
            cjk += 1.0;
        } else {
            other += 1.0;
        }
    }
    cjk + other / 4.0
}

/// The wide scripts whose characters run about one token per character.
fn is_cjk_script(ch: char) -> bool {
    matches!(ch as u32,
        0x3000..=0x303F   // CJK punctuation
        | 0x3040..=0x30FF // kana
        | 0x3400..=0x4DBF // CJK extension A
        | 0x4E00..=0x9FFF // CJK unified ideographs
        | 0xAC00..=0xD7AF // hangul syllables
        | 0xF900..=0xFAFF // CJK compatibility ideographs
        | 0xFF00..=0xFFEF // fullwidth forms
    )
}

/// `47` above ten, `3.4` below — one decimal is only informative when the
/// number is small.
fn format_tps(tps: f64) -> String {
    if tps >= 10.0 {
        format!("{tps:.0} t/s")
    } else {
        format!("{tps:.1} t/s")
    }
}

/// The last [`SPARK_SAMPLES`] per-turn samples as a min-max normalized
/// block row. A flat window renders as a mid-height row, not a zero one.
fn sparkline(samples: &VecDeque<f64>) -> String {
    let window: Vec<f64> = samples
        .iter()
        .rev()
        .take(SPARK_SAMPLES)
        .rev()
        .copied()
        .collect();
    if window.is_empty() {
        return String::new();
    }
    let min = window.iter().cloned().fold(f64::MAX, f64::min);
    let max = window.iter().cloned().fold(f64::MIN, f64::max);
    window
        .iter()
        .map(|value| {
            if max <= min {
                return SPARK_BLOCKS[(SPARK_BLOCKS.len() - 1) / 2];
            }
            let scaled = ((value - min) / (max - min) * (SPARK_BLOCKS.len() - 1) as f64).round();
            SPARK_BLOCKS[scaled.clamp(0.0, (SPARK_BLOCKS.len() - 1) as f64) as usize]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::core::EventMsg;
    use crate::core::protocol::Event;
    use crate::plugins::DeltaKind;
    use crate::plugins::PluginBridge;
    use crate::plugins::PluginEffect;
    use crate::plugins::PluginEventKind;
    use crate::plugins::RequestFacts;
    use cst_plugin_api::commands::CommandTable;
    use cst_plugin_api::plugin::PluginInfo;
    use tokio_util::sync::CancellationToken;

    fn info() -> PluginInfo<'static> {
        static NO_TOOLS: [crate::core::ToolSpec; 0] = [];
        static NO_CONFIGS: [(String, cst_plugin_api::config::PluginSettings); 0] = [];
        PluginInfo {
            model: "test/model",
            mode: "fullscreen",
            transcript_len: 0,
            context_window: None,
            last_usage: None,
            last_cached: None,
            total_input: 0,
            total_output: 0,
            total_cached: 0,
            plugin_ids: &["tps"],
            system_sections: &[],
            effort: None,
            request: RequestFacts {
                protocol: "",
                base_url: "",
                endpoint: "",
                overrides: &[],
            },
            tools: &NO_TOOLS,
            enabled_tools: &[],
            message_count: 1,
            plugin_configs: &NO_CONFIGS,
        }
    }

    /// A context whose collected effects are returned after the dispatch.
    fn dispatch(plugin: &Tps, event: &Event) -> Vec<PluginEffect> {
        let table = CommandTable::new(std::iter::empty()).expect("empty table");
        let bridge = PluginBridge::new(plugin.id(), CancellationToken::new(), Arc::new(|_| {}));
        let mut ctx = PluginContext::new(&table, info(), plugin.id(), bridge);
        plugin.on_event(
            &PluginEvent {
                at: event.at,
                kind: plugin_event_kind_test(&event.msg),
            },
            &mut ctx,
        );
        ctx.into_effects()
    }

    fn plugin_event_kind_test(msg: &EventMsg) -> PluginEventKind<'_> {
        use crate::core::EventMsg;
        match msg {
            EventMsg::TurnStarted => PluginEventKind::TurnStarted,
            EventMsg::AgentMessageDelta { delta } => PluginEventKind::MessageDelta {
                kind: DeltaKind::Text,
                text: delta,
            },
            EventMsg::AgentThinkingDelta { delta } => PluginEventKind::MessageDelta {
                kind: DeltaKind::Thinking,
                text: delta,
            },
            EventMsg::TurnUsage { input, output, .. } => PluginEventKind::TurnUsage {
                input: *input,
                output: *output,
            },
            EventMsg::ContextUsage { input, output, .. } => PluginEventKind::RoundUsage {
                input: *input,
                output: *output,
            },
            EventMsg::AgentToolCallStarted { name, arguments } => {
                PluginEventKind::ToolCallStarted { name, arguments }
            }
            EventMsg::TurnComplete => PluginEventKind::TurnComplete,
            EventMsg::TurnAborted { reason } => PluginEventKind::TurnAborted { reason },
            EventMsg::ConversationCleared => PluginEventKind::ConversationCleared,
            EventMsg::SessionStarted { session } => {
                PluginEventKind::SessionStarted { session: *session }
            }
            EventMsg::SessionSwitched { session } => {
                PluginEventKind::SessionSwitched { session: *session }
            }
            _ => unreachable!("test events cover the metered kinds only"),
        }
    }

    fn event(msg: EventMsg, offset_ms: u64) -> Event {
        let base = Instant::now() - Duration::from_secs(3600);
        Event {
            id: String::new(),
            msg,
            at: base + Duration::from_millis(offset_ms),
        }
    }

    fn segment<'a>(effects: &'a [PluginEffect], key: &str) -> &'a StatusSegment {
        let full = format!("tps:{key}");
        let found = effects.iter().find_map(|effect| match effect {
            PluginEffect::SetStatus(segment) if segment.key == full => Some(segment),
            _ => None,
        });
        found.expect("expected a SetStatus effect for the key")
    }

    fn set_segment(effects: &[PluginEffect], key: &str) -> bool {
        effects.iter().any(|effect| {
            matches!(effect, PluginEffect::SetStatus(segment) if segment.key == format!("tps:{key}"))
        })
    }

    fn cleared(effects: &[PluginEffect], key: &str) -> bool {
        effects
            .iter()
            .any(|effect| matches!(effect, PluginEffect::ClearStatus(existing) if existing == &format!("tps:{key}")))
    }

    #[test]
    fn a_short_turn_below_the_noise_floor_shows_nothing() {
        let tps = fresh();
        // Enable, then feed a turn that produced a few tokens over 100 ms.
        let table = CommandTable::new(std::iter::empty()).expect("empty table");
        let bridge = PluginBridge::new(tps.id(), CancellationToken::new(), Arc::new(|_| {}));
        let mut ctx = PluginContext::new(&table, info(), tps.id(), bridge);
        tps.run_command(&toggle(), &mut ctx).expect("toggle");
        let effects = ctx.into_effects();
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, PluginEffect::Notice(text) if text.contains("on")))
        );

        let out = dispatch(&tps, &event(EventMsg::TurnStarted, 0));
        let out = {
            let mut combined = out;
            combined.extend(dispatch(
                &tps,
                &event(EventMsg::AgentMessageDelta { delta: "hi".into() }, 100),
            ));
            combined
        };
        assert!(
            !out.iter()
                .any(|effect| matches!(effect, PluginEffect::SetStatus(_))),
            "a 100 ms decode span is below the noise floor: {out:?}"
        );
    }

    fn toggle() -> Invocation {
        Invocation {
            spec: &COMMANDS[0],
            args: String::new(),
        }
    }

    /// A fresh meter: tests run in parallel and must not share the static's
    /// state.
    fn fresh() -> Tps {
        Tps {
            state: Mutex::new(TpsState::new()),
        }
    }

    /// A meter switched on through its own command, like `/tps`.
    fn enabled() -> Tps {
        let tps = fresh();
        let table = CommandTable::new(std::iter::empty()).expect("empty table");
        let bridge = PluginBridge::new(tps.id(), CancellationToken::new(), Arc::new(|_| {}));
        let mut ctx = PluginContext::new(&table, info(), tps.id(), bridge);
        tps.run_command(&toggle(), &mut ctx).expect("toggle");
        ctx.into_effects();
        tps
    }

    /// The meter's per-turn sample ring.
    fn samples(tps: &Tps) -> Vec<f64> {
        tps.state
            .lock()
            .expect("tps state")
            .samples
            .iter()
            .copied()
            .collect()
    }

    /// One streamed round: 48 ASCII chars (12 estimated tokens) over 600 ms.
    fn stream_a_round(tps: &Tps, start_ms: u64) {
        dispatch(
            tps,
            &event(
                EventMsg::AgentMessageDelta {
                    delta: "a".repeat(24),
                },
                start_ms,
            ),
        );
        dispatch(
            tps,
            &event(
                EventMsg::AgentMessageDelta {
                    delta: "a".repeat(24),
                },
                start_ms + 600,
            ),
        );
    }

    #[test]
    fn deltas_stream_a_live_estimate_and_usage_corrects_it() {
        let tps = enabled();

        dispatch(&tps, &event(EventMsg::TurnStarted, 0));
        stream_a_round(&tps, 100);
        let out = dispatch(
            &tps,
            &event(
                EventMsg::AgentMessageDelta {
                    delta: "a".repeat(4),
                },
                700,
            ),
        );
        let rate = segment(&out, "rate");
        assert_eq!(rate.text, "22 t/s");
        assert_eq!(rate.tone, StatusTone::Dim);

        // The backend's real count corrects the estimate: 60 output tokens
        // over the same 600 ms → 100 t/s, which reads as fast.
        let out = dispatch(
            &tps,
            &event(
                EventMsg::TurnUsage {
                    input: 1_000,
                    output: 60,
                    cached: None,
                    model: None,
                },
                700,
            ),
        );
        let rate = segment(&out, "rate");
        assert_eq!(rate.text, "100 t/s");
        assert_eq!(rate.tone, StatusTone::Accent);
    }

    #[test]
    fn a_finished_turn_clears_the_rate_and_a_lone_sample_stays_hidden() {
        let tps = enabled();

        dispatch(&tps, &event(EventMsg::TurnStarted, 0));
        stream_a_round(&tps, 100);
        let out = dispatch(&tps, &event(EventMsg::TurnComplete, 700));
        assert!(cleared(&out, "rate"), "the live rate retires with the turn");
        // One sample is not a trend: showing it as a sparkline would leave a
        // frozen block plus a lone number in the footer forever.
        assert!(!set_segment(&out, "spark"), "{out:?}");
        assert!(cleared(&out, "spark"), "{out:?}");
        assert_eq!(samples(&tps), vec![20.0]);
    }

    #[test]
    fn two_turns_render_a_sparkline_and_no_number() {
        let tps = enabled();

        dispatch(&tps, &event(EventMsg::TurnStarted, 0));
        stream_a_round(&tps, 100);
        let out = dispatch(&tps, &event(EventMsg::TurnComplete, 700));
        assert!(cleared(&out, "spark"));

        dispatch(&tps, &event(EventMsg::TurnStarted, 1_000));
        stream_a_round(&tps, 1_100);
        let out = dispatch(&tps, &event(EventMsg::TurnComplete, 1_700));
        let spark = segment(&out, "spark");
        // Two equal samples: a flat window, and nothing but blocks — the
        // numeric readout belongs to the live rate, not to the history.
        assert_eq!(spark.text, "▄▄");
        assert_eq!(samples(&tps), vec![20.0, 20.0]);
    }

    #[test]
    fn a_session_switch_drops_the_previous_sessions_samples() {
        let tps = enabled();

        for (turn, at) in [(0u64, 0u64), (1, 1_000)] {
            dispatch(&tps, &event(EventMsg::TurnStarted, at));
            stream_a_round(&tps, at + 100);
            dispatch(&tps, &event(EventMsg::TurnComplete, at + 700));
            assert_eq!(samples(&tps).len() as u64, turn + 1);
        }

        // Switching sessions swaps in another conversation: its throughput
        // must not report turns that session never ran.
        let out = dispatch(
            &tps,
            &event(EventMsg::SessionSwitched { session: 0 }, 2_000),
        );
        assert!(cleared(&out, "rate"), "{out:?}");
        assert!(cleared(&out, "spark"), "{out:?}");
        assert!(samples(&tps).is_empty());

        // A fresh session is the same boundary.
        dispatch(&tps, &event(EventMsg::TurnStarted, 3_000));
        stream_a_round(&tps, 3_100);
        dispatch(&tps, &event(EventMsg::TurnComplete, 3_700));
        let out = dispatch(&tps, &event(EventMsg::SessionStarted { session: 1 }, 4_000));
        assert!(cleared(&out, "spark"), "{out:?}");
        assert!(samples(&tps).is_empty());

        // And the meter stays on across the boundary.
        assert!(tps.state.lock().expect("tps state").enabled);
    }

    #[test]
    fn a_cleared_conversation_forgets_its_samples_but_stays_on() {
        let tps = enabled();
        dispatch(&tps, &event(EventMsg::TurnStarted, 0));
        stream_a_round(&tps, 100);
        dispatch(&tps, &event(EventMsg::TurnComplete, 700));

        let out = dispatch(&tps, &event(EventMsg::ConversationCleared, 1_000));
        assert!(cleared(&out, "rate"), "{out:?}");
        assert!(cleared(&out, "spark"), "{out:?}");
        assert!(samples(&tps).is_empty());
        assert!(tps.state.lock().expect("tps state").enabled);
    }

    #[test]
    fn a_tool_round_banks_its_own_output_instead_of_the_turns_last_round() {
        let tps = enabled();

        // Round one streams 12 tokens over 600 ms and reports 50 output
        // tokens; round two streams over 400 ms and reports 50 more. The
        // completed turn's `TurnUsage` carries only the last round (50), but
        // the decode span covers both — the meter must sum the rounds, or it
        // reports half the real speed.
        dispatch(&tps, &event(EventMsg::TurnStarted, 0));
        dispatch(
            &tps,
            &event(
                EventMsg::AgentMessageDelta {
                    delta: "a".repeat(24),
                },
                100,
            ),
        );
        dispatch(
            &tps,
            &event(
                EventMsg::AgentMessageDelta {
                    delta: "a".repeat(24),
                },
                700,
            ),
        );
        dispatch(
            &tps,
            &event(
                EventMsg::ContextUsage {
                    input: 10,
                    output: 50,
                    cached: None,
                },
                700,
            ),
        );
        dispatch(
            &tps,
            &event(
                EventMsg::AgentToolCallStarted {
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
                800,
            ),
        );
        // The tool runs for five seconds — never part of the decode span.
        dispatch(
            &tps,
            &event(
                EventMsg::AgentMessageDelta {
                    delta: "b".repeat(8),
                },
                5_800,
            ),
        );
        dispatch(
            &tps,
            &event(
                EventMsg::ContextUsage {
                    input: 20,
                    output: 50,
                    cached: None,
                },
                6_200,
            ),
        );
        dispatch(
            &tps,
            &event(
                EventMsg::TurnUsage {
                    input: 20,
                    output: 50,
                    cached: None,
                    model: None,
                },
                6_200,
            ),
        );
        let out = dispatch(&tps, &event(EventMsg::TurnComplete, 6_200));

        // 100 tokens over 1 s of decode (600 ms + 400 ms) → 100 t/s.
        assert_eq!(samples(&tps), vec![100.0]);
        assert!(cleared(&out, "rate"));
    }

    #[test]
    fn a_backend_without_per_round_usage_falls_back_to_the_turn_count() {
        let tps = enabled();

        dispatch(&tps, &event(EventMsg::TurnStarted, 0));
        stream_a_round(&tps, 100);
        // No `ContextUsage` at all this turn: `TurnUsage` is the only count.
        dispatch(
            &tps,
            &event(
                EventMsg::TurnUsage {
                    input: 1_000,
                    output: 60,
                    cached: None,
                    model: None,
                },
                700,
            ),
        );
        dispatch(&tps, &event(EventMsg::TurnComplete, 700));
        assert_eq!(samples(&tps), vec![100.0]);
    }

    #[test]
    fn tool_execution_time_stays_out_of_the_decode_span() {
        let tps = enabled();

        // 12 tokens streamed over 600 ms, then a 5-second tool call, then
        // the turn settles. The tool time never enters the span.
        dispatch(&tps, &event(EventMsg::TurnStarted, 0));
        stream_a_round(&tps, 100);
        dispatch(
            &tps,
            &event(
                EventMsg::AgentToolCallStarted {
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
                700,
            ),
        );
        dispatch(
            &tps,
            &event(
                EventMsg::TurnUsage {
                    input: 100,
                    output: 12,
                    cached: None,
                    model: None,
                },
                5_700,
            ),
        );
        dispatch(&tps, &event(EventMsg::TurnComplete, 5_700));
        assert_eq!(samples(&tps), vec![20.0]);
    }

    #[test]
    fn cjk_text_estimates_one_token_per_character() {
        assert_eq!(estimate_tokens("你好"), 2.0);
        assert!((estimate_tokens("abcd") - 1.0).abs() < f64::EPSILON);
        assert!((estimate_tokens("你好ab") - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn the_toggle_clears_segments_when_turned_off() {
        let tps = enabled();
        let table = CommandTable::new(std::iter::empty()).expect("empty table");
        let bridge = PluginBridge::new(tps.id(), CancellationToken::new(), Arc::new(|_| {}));

        dispatch(&tps, &event(EventMsg::TurnStarted, 0));
        stream_a_round(&tps, 100);
        assert!(set_segment(
            &dispatch(
                &tps,
                &event(
                    EventMsg::AgentMessageDelta {
                        delta: "b".repeat(4)
                    },
                    800
                )
            ),
            "rate"
        ));

        let mut ctx = PluginContext::new(&table, info(), tps.id(), bridge);
        tps.run_command(&toggle(), &mut ctx).expect("off");
        let out = ctx.into_effects();
        assert!(cleared(&out, "rate"));
        assert!(cleared(&out, "spark"));
        // And nothing further publishes.
        let out = dispatch(
            &tps,
            &event(
                EventMsg::AgentMessageDelta {
                    delta: "c".repeat(48),
                },
                800,
            ),
        );
        assert!(out.is_empty(), "{out:?}");
    }

    /// The `/settings` toggle and `/tps` are the same switch: the persisted
    /// hook applies live, and turning it off retires the segments it drew.
    #[test]
    fn the_settings_toggle_applies_live_and_clears_when_switched_off() {
        let tps = fresh();
        let table = CommandTable::new(std::iter::empty()).expect("empty table");
        let bridge = || PluginBridge::new(tps.id(), CancellationToken::new(), Arc::new(|_| {}));

        // Switching on is a notice plus the switch itself.
        let mut ctx = PluginContext::new(&table, info(), tps.id(), bridge());
        let on = cst_plugin_api::config::PluginSettings {
            options: serde_json::json!({ "enabled": true })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        };
        tps.on_settings_changed(Some(&on), &mut ctx);
        let effects = ctx.into_effects();
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, PluginEffect::Notice(text) if text.contains("on"))),
            "{effects:?}"
        );
        assert!(tps.is_enabled());

        // It meters from here on, exactly like `/tps` had been typed.
        dispatch(&tps, &event(EventMsg::TurnStarted, 0));
        stream_a_round(&tps, 100);
        assert!(
            set_segment(
                &dispatch(
                    &tps,
                    &event(
                        EventMsg::AgentMessageDelta {
                            delta: "b".repeat(4)
                        },
                        800
                    ),
                ),
                "rate",
            ),
            "the meter publishes once the settings toggle is on"
        );

        // Switching off through the same hook clears what it drew.
        let mut ctx = PluginContext::new(&table, info(), tps.id(), bridge());
        tps.on_settings_changed(None, &mut ctx);
        let effects = ctx.into_effects();
        assert!(cleared(&effects, "rate"), "{effects:?}");
        assert!(cleared(&effects, "spark"), "{effects:?}");
        assert!(!tps.is_enabled());

        // A repeat of the stored value is a no-op.
        let mut ctx = PluginContext::new(&table, info(), tps.id(), bridge());
        tps.on_settings_changed(None, &mut ctx);
        assert!(ctx.into_effects().is_empty());
    }
}
