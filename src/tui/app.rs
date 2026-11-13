//! Top-level TUI app: terminal setup, event loop, and rendering.
//!
//! Two display modes are supported, mirroring `pi`'s `tuiMode` and cht's
//! `fullscreen` setting:
//!
//! * [`TuiMode::Fullscreen`] (default) owns the alternate screen, keeps the
//!   transcript in an application-scrolled viewport, and captures the mouse
//!   wheel.
//! * [`TuiMode::Inline`] leaves the terminal in place, streams finished
//!   transcript lines into the terminal's own scrollback, and keeps only the
//!   composer plus footer pinned to the bottom of the screen.

use std::io::Stdout;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use crossterm::cursor::MoveTo;
use crossterm::cursor::Show;
use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableMouseCapture;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::EnableMouseCapture;
use crossterm::event::Event;
use crossterm::event::EventStream;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use crossterm::execute;
use crossterm::style::Print;
use crossterm::terminal::Clear;
use crossterm::terminal::ClearType;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;
use futures_util::StreamExt;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::TerminalOptions;
use ratatui::Viewport;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Alignment;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use tokio::time::MissedTickBehavior;

use crate::core::AgentSession;
use crate::core::CommandCandidate;
use crate::core::CommandInput;
use crate::core::DEFAULT_SYSTEM_PROMPT;
use crate::core::EventMsg;
use crate::core::Invocation;
use crate::core::ModelPool;
use crate::core::NO_MODEL_GUIDANCE;
use crate::core::NO_MODEL_SELECTED;
use crate::core::Selection;
use crate::core::SessionBootstrap;
use crate::core::ToolSpec;
use crate::core::config::StatusBarConfig;
use crate::core::context_percent;
use crate::core::format_context_usage;
use crate::core::format_tokens;
use crate::core::openai_compat::endpoint_url;
use crate::core::reasoning;
use crate::core::rollout;
use crate::core::rollout::LoadedSession;
use crate::plugins::PluginEffect;
use crate::plugins::PluginHost;
use crate::plugins::PluginInfo;
use crate::plugins::PluginOutput;
use crate::plugins::PluginSettings;
use crate::plugins::PluginTask;
use crate::plugins::StatusTone;
use crate::tui::RestoreMode;
use crate::tui::TuiMode;
use crate::tui::chat;
use crate::tui::chat::HistoryCell;
use crate::tui::composer::Composer;
use crate::tui::session::SessionView;
use crate::tui::session::truncated;
use crate::tui::theme;
use crate::tui::wrap::display_width;

type Tui = Terminal<CrosstermBackend<Stdout>>;

const MAX_COMPOSER_ROWS: usize = 8;
/// Rows the inline pane devotes to the still-streaming message tail. Fixed
/// while a turn runs: a pane that grew row by row would scroll its own
/// already-drawn tail into the terminal's scrollback, which is exactly the
/// output [`take_inline_pending`] later commits for real — the duplicate
/// lines that produced.
const MAX_LIVE_TAIL_ROWS: usize = 4;
const MAX_COMMAND_MENU_ROWS: usize = 6;
const WHEEL_LINES: usize = 3;
const ALT_WHEEL_LINES: usize = 15;
/// How long an ephemeral footer message (Ctrl+T's "thinking expanded",
/// Ctrl+O's "tool output expanded") stays visible. Long enough to read at a
/// glance, gone before it can accumulate.
const STATUS_TTL: Duration = Duration::from_millis(2_000);

/// The profile side of startup: the whole `profiles.toml` table (for
/// `/profile` listing and `Op::SwitchProfile` lookups) plus the name this
/// run started with (`--profile`). The active profile's tool filter is
/// already applied to the plugin host; its prompt and effort presets are
/// applied in [`run`].
pub struct ProfileStartup {
    pub set: Arc<crate::core::config::ProfileSet>,
    pub active: Option<String>,
}

/// Run the TUI until the user quits or the agent worker stops.
///
/// `plugins` carries the command table (and the provider factories already
/// merged into `pool`'s registry); `selection` is the model this run starts
/// with, or `None` when nothing is configured — the app then opens with an
/// empty route and guides the user on the first send. `profiles` carries the
/// profile set and the name of the profile this run started with (its tool
/// filter is already applied to `plugins`). Providers with models discovery
/// enabled are refreshed in the background, best-effort: results arrive as
/// transcript notices. `max_tool_rounds` is `config.toml`'s optional cap on a
/// turn's tool rounds; `None` leaves the loop unbounded. `status_bar` is the
/// footer's `[status_bar]` table; `plugin_settings` the per-plugin
/// `[plugins."<id>"]` pockets, already validated to name existing plugins.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    plugins: Arc<PluginHost>,
    pool: Arc<ModelPool>,
    selection: Option<Selection>,
    system_prompt: Option<String>,
    mode: TuiMode,
    restore: RestoreMode,
    profiles: ProfileStartup,
    max_tool_rounds: Option<usize>,
    status_bar: StatusBarConfig,
    plugin_settings: Vec<(String, PluginSettings)>,
) -> Result<()> {
    // A settings pocket naming an unknown plugin id would silently configure
    // nothing — the same discipline as a typo anywhere else in these files.
    // The uninstall path is expected to clean the table up; the error says
    // exactly which table to drop.
    for (id, _) in &plugin_settings {
        if !plugins.plugin_ids().contains(&id.as_str()) {
            anyhow::bail!(
                "config.toml [plugins.\"{id}\"] names an unknown plugin \
                 (known: {}); remove the table or install the plugin",
                plugins.plugin_ids().join(", ")
            );
        }
    }

    // Created before setup so a partial setup still restores the terminal.
    let _guard = TerminalGuard { mode };
    let mut terminal = Some(setup_terminal(mode)?);

    // The active profile's prompt addition is the most specific fragment:
    // it gets the last word, after the plugin fragments, and `/context`
    // attributes it to `profile:<name>`.
    let base_prompt = system_prompt.unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
    let mut system_prompt = plugins.system_prompt(&base_prompt);
    let mut system_sections = plugins.system_prompt_sections(&base_prompt);
    let profile_append = profiles
        .active
        .as_deref()
        .and_then(|name| profiles.set.get(name))
        .and_then(|active| active.prompt.append.as_deref())
        .filter(|text| !text.trim().is_empty());
    if let Some(text) = profile_append {
        if !system_prompt.is_empty() {
            system_prompt.push_str("\n\n");
        }
        system_prompt.push_str(text.trim());
        let name = profiles.active.as_deref().unwrap_or_default();
        system_sections.push((
            format!("profile:{name}"),
            text.trim().to_string(),
        ));
    }

    // The workspace's saved sessions are always loaded so `/sessions` shows
    // the same list no matter how codest was started. Plain startup opens a
    // fresh session on top; the restore modes continue where the last run
    // left off. Everything from this run is persisted best-effort under
    // ~/.codest/sessions/<workspace-hash>/.
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let sessions_dir = rollout::workspace_sessions_dir(&workspace);
    let loaded = rollout::load_workspace_sessions(&sessions_dir).unwrap_or_else(|error| {
        eprintln!("codest: cannot load saved sessions: {error}");
        Vec::new()
    });
    let fresh = restore == RestoreMode::None;

    // A restored session remembers the model it last used: adopt it for this
    // run so the backend, the footer, and the effort seeding all pick up
    // where the session left off — before anything renders.
    let selection = restore_model_selection(&pool, selection, &loaded, fresh);
    // The profile is a fresh-start preset: a restored session restores its
    // own state instead, though the profile it last used is still shown as
    // the active one.
    //
    // Three states, mirroring the worker: `None` means "seed from the active
    // model's `default_level`", `Some(None)` is an explicit `off`, and
    // `Some(Some(level))` is that level. A restored session's own recorded
    // choice wins; failing that (or on a fresh start) the profile preset.
    let initial_effort: Option<Option<String>> = if fresh {
        profiles
            .active
            .as_deref()
            .and_then(|name| profiles.set.get(name))
            .and_then(|active| active.effort.as_deref())
            .map(reasoning::normalize_effort)
    } else {
        loaded.last().map(|session| session.effort.clone()).unwrap_or(None)
    };
    let active_profile = if fresh {
        profiles.active.clone()
    } else {
        loaded
            .last()
            .and_then(|session| session.profile.clone())
            .or_else(|| profiles.active.clone())
    };

    let session = AgentSession::spawn_with_pool(
        pool.clone(),
        selection.as_ref(),
        system_prompt.clone(),
        SessionBootstrap {
            cwd: workspace.to_string_lossy().into_owned(),
            rollout_dir: Some(sessions_dir),
            loaded: loaded.clone(),
            fresh,
            tools: plugins.tools(),
            executor: Some(plugins.clone()),
            profiles: profiles.set.clone(),
            profile: profiles.active.clone(),
            initial_effort: initial_effort.clone(),
            max_tool_rounds,
        },
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;

    // The channel plugin background tasks send effects through; the host
    // built it, this run drains it.
    let plugin_effects = plugins
        .take_effects_receiver()
        .expect("the plugin host always creates its effect channel");
    let (notice_tx, notice_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (provider_tx, provider_tasks) = tokio::sync::mpsc::unbounded_channel::<ProviderTask>();
    let model_label = selection
        .as_ref()
        .map(Selection::as_str)
        .unwrap_or_else(|| "no model".to_string());
    let (views, active_view) = build_views(&model_label, mode, &loaded, fresh);
    let mut app = App::new(
        session,
        pool,
        notice_tx.clone(),
        notice_rx,
        provider_tx,
        provider_tasks,
        plugin_effects,
        model_label,
        mode,
        plugins,
        system_sections,
        views,
        active_view,
        profiles.set,
        active_profile,
        initial_effort,
        status_bar,
        plugin_settings,
    );
    // The tool set is per session, and the worker restores the adopted
    // session's set silently (no op ran, so no event comes back). Mirror it:
    // a saved set names only what that session left enabled, and names this
    // run does not declare are dropped; a session without one starts with
    // everything enabled (already the case in the App).
    if !fresh
        && let Some(saved) = loaded.last().and_then(|session| session.tools.as_ref())
    {
        app.enabled_tools = saved
            .iter()
            .filter(|name| app.tools.iter().any(|spec| spec.name == name.as_str()))
            .cloned()
            .collect();
        app.enabled_tools_vec = app.enabled_tools.iter().cloned().collect();
    }
    if restore == RestoreMode::Picker {
        app.open_session_menu();
    }
    let output = app.plugins.startup(app.plugin_info());
    app.apply_output(output);
    // Give every event subscriber a picture of the conversation it just
    // joined: the snapshot arrives as an ordinary event, so monitors and
    // metrics plugins see the restored transcript without any special path.
    app.session.inspect_conversation();

    // Models discovery for every instance that asked for it. Failures are
    // notices, never errors: the declared models keep the menu usable. An
    // incomplete endpoint (lone base_url or api_key) is reported the same
    // way, naming the missing piece.
    for instance in app.pool.registry().instances() {
        if instance.should_fetch_models() && app.pool.can_discover(&instance.name) {
            app.spawn_discovery(&instance.name);
        }
    }

    let result = app.event_loop(&mut terminal).await;
    finish_terminal(&mut terminal, &app, mode);
    result
}

/// The selection this run starts with, after restore: a restored session
/// remembers the model it last used, and on the restore modes the active
/// (most recent) session's selection wins over the configured default.
/// Best-effort — a selection that no longer parses or builds a client (the
/// provider was removed from providers.toml, say) keeps the configured one.
/// A plain startup (`fresh`) never overrides.
fn restore_model_selection(
    pool: &ModelPool,
    selection: Option<Selection>,
    loaded: &[LoadedSession],
    fresh: bool,
) -> Option<Selection> {
    if fresh {
        return selection;
    }
    let saved = loaded.last().and_then(|session| session.model.clone())?;
    if selection
        .as_ref()
        .is_some_and(|active| active.as_str() == saved.as_str())
    {
        return selection;
    }
    match pool
        .registry()
        .parse_selection(&saved)
        .ok()
        .filter(|parsed| pool.client(parsed).is_ok())
    {
        Some(parsed) => Some(parsed),
        None => {
            eprintln!(
                "codest: note: saved session model `{saved}` is not configured; \
                 keeping the current selection"
            );
            selection
        }
    }
}

/// Starting views: one per restored session (oldest first) plus, on a plain
/// startup, the fresh session being typed into. The startup banner belongs
/// to this run rather than any session, so it goes into the active view.
fn build_views(
    model_label: &str,
    mode: TuiMode,
    loaded: &[LoadedSession],
    fresh: bool,
) -> (Vec<SessionView>, usize) {
    let mut views: Vec<SessionView> = loaded.iter().map(SessionView::restored).collect();
    if fresh || views.is_empty() {
        views.push(SessionView::initial(model_label, mode));
        let active = views.len() - 1;
        if active > 0 {
            views[active].history.push(HistoryCell::Notice(format!(
                "{} saved session{} available via /sessions",
                loaded.len(),
                if loaded.len() == 1 { "" } else { "s" }
            )));
        }
        return (views, active);
    }

    let active = views.len() - 1;
    let mut cells = SessionView::initial(model_label, mode).history;
    cells.push(HistoryCell::Notice(format!(
        "resumed {} saved session{}",
        loaded.len(),
        if loaded.len() == 1 { "" } else { "s" }
    )));
    views[active].history.splice(0..0, cells);
    (views, active)
}

fn setup_terminal(mode: TuiMode) -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    match mode {
        TuiMode::Fullscreen => {
            execute!(
                stdout,
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableMouseCapture
            )?;
            let backend = CrosstermBackend::new(stdout);
            let mut terminal = Terminal::new(backend)?;
            terminal.clear()?;
            Ok(terminal)
        }
        TuiMode::Inline => {
            execute!(stdout, EnableBracketedPaste)?;
            let (columns, rows) = crossterm::terminal::size()?;
            let height = inline_pane_height(rows, 1, 0, 0);
            // Reserve the bottom rows for the pane: scrolling once keeps the
            // shell's existing output intact above us.
            execute!(
                stdout,
                MoveTo(0, rows.saturating_sub(1)),
                Print("\n".repeat(height as usize))
            )?;
            let backend = CrosstermBackend::new(stdout);
            // A fixed viewport is used instead of `Viewport::Inline` because we
            // scroll and reposition the pane ourselves; `Viewport::Inline`
            // re-queries the cursor when its height changes, which races with
            // the event stream reading stdin.
            let terminal = Terminal::with_options(
                backend,
                TerminalOptions {
                    viewport: Viewport::Fixed(inline_rect(columns, rows, height)),
                },
            )?;
            Ok(terminal)
        }
    }
}

/// Restore the terminal even if the event loop returns an error.
struct TerminalGuard {
    mode: TuiMode,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = std::io::stdout();
        match self.mode {
            TuiMode::Fullscreen => {
                let _ = execute!(
                    stdout,
                    DisableMouseCapture,
                    DisableBracketedPaste,
                    LeaveAlternateScreen,
                    Show
                );
            }
            TuiMode::Inline => {
                let _ = execute!(stdout, DisableBracketedPaste, Show);
            }
        }
    }
}

/// Hand the bottom of the terminal back to the shell after an inline run.
fn finish_terminal(terminal: &mut Option<Tui>, app: &App, mode: TuiMode) {
    if let Some(terminal) = terminal.as_mut() {
        let _ = terminal.show_cursor();
    }
    if mode != TuiMode::Inline {
        return;
    }
    let Ok((_, rows)) = crossterm::terminal::size() else {
        return;
    };
    let top = app.inline_top.min(rows.saturating_sub(1));
    let mut stdout = std::io::stdout();
    let _ = execute!(
        stdout,
        MoveTo(0, top),
        Clear(ClearType::FromCursorDown),
        Show
    );
}

/// Height of the inline bottom pane: the live assistant tail, optional
/// command menu, composer block (top border + visible rows), and the footer.
///
/// `live_rows` is the number of rows the unsettled part of the streaming
/// message needs. Markdown blocks such as a partially streamed table or code
/// fence span several rows, so the tail cannot be pinned to one line.
fn inline_pane_height(rows: u16, composer_rows: usize, live_rows: usize, menu_rows: usize) -> u16 {
    let composer = composer_rows.clamp(1, MAX_COMPOSER_ROWS) as u16 + 1;
    let menu = menu_rows as u16;
    let live = live_rows as u16;
    (composer + live + menu + 1).min(rows.max(1))
}

/// How many rows of the streaming tail the inline pane may show, given the
/// terminal height and the fixed composer/menu/footer rows.
fn inline_live_rows(rows: u16, composer_rows: usize, menu_rows: usize) -> usize {
    let fixed = composer_rows.clamp(1, MAX_COMPOSER_ROWS) + 1 + menu_rows + 1;
    (rows as usize).saturating_sub(fixed).max(1)
}

/// A result from background `/provider` work, delivered to the event loop.
///
/// The wizard never blocks the render thread: a probe or a browser sign-in
/// runs on its own task and reports back through this channel, exactly like
/// `spawn_discovery` reports through `notice_tx`.
enum ProviderTask {
    /// Model discovery for a draft endpoint finished.
    Probed(Result<Vec<crate::core::config::ModelInfo>, String>),
    /// A sign-in flow reported progress (a device URL, a wait notice).
    SignInNotice {
        message: String,
        url: Option<String>,
    },
    /// A sign-in flow finished. `Ok(Some(account))` carries a label.
    SignedIn(Result<Option<String>, String>),
}

struct App {
    session: AgentSession,
    plugins: Arc<PluginHost>,
    /// The multi-provider registry behind `/model` and background discovery.
    pool: Arc<ModelPool>,
    /// Notices from background tasks (models discovery). The sender half is
    /// kept alive by the tasks and `notice_tx`; once all senders are gone the
    /// arm is disabled via `notices_closed`.
    notice_tx: tokio::sync::mpsc::UnboundedSender<String>,
    notices: tokio::sync::mpsc::UnboundedReceiver<String>,
    notices_closed: bool,
    /// Backend label shown in the footer and banner: the active
    /// `"provider/model"` selection, or `no model` before anything is
    /// configured.
    model_label: String,
    /// The active selection; `None` until a model is configured and chosen.
    /// Derived from `model_label` (which is always either a valid selection
    /// string or the literal `no model`).
    route: Option<Selection>,
    mode: TuiMode,
    /// One view per recorded session; `views[i]` mirrors core conversation i.
    /// Append-only: `/new` adds one and switches to it, nothing is removed.
    views: Vec<SessionView>,
    /// Creation-order index of the session being viewed and edited.
    active_view: usize,
    /// Open `/sessions` picker: the selected view index; `None` when closed.
    session_menu: Option<usize>,
    /// Open `/model` picker over the registry's selections.
    model_menu: Option<ModelMenu>,
    /// Open `/effort` picker over the reasoning effort levels.
    effort_menu: Option<EffortMenu>,
    /// Open `/tools` picker over the declared tools.
    tools_menu: Option<ToolsMenu>,
    /// Open `/context` viewer (`Some` while it owns the input): the whole
    /// frame shows the conversation as the model sees it, with delete /
    /// edit / undo. `None` in normal chat.
    context_view: Option<crate::tui::context_view::ContextView>,
    /// Open `/provider` wizard (`Some` while it owns the input).
    provider_wizard: Option<crate::tui::provider_wizard::Wizard>,
    /// Results from background wizard work (model probes, sign-ins).
    provider_tx: tokio::sync::mpsc::UnboundedSender<ProviderTask>,
    provider_tasks: tokio::sync::mpsc::UnboundedReceiver<ProviderTask>,
    provider_tasks_closed: bool,
    /// Cancels the wizard's in-flight background task on Esc.
    provider_cancel: Option<tokio_util::sync::CancellationToken>,
    /// The reasoning effort sent with every turn (`/effort`); `None` sends no
    /// effort field at all. Updated optimistically, confirmed by
    /// `ReasoningEffortSwitched`.
    effort: Option<String>,
    /// The available profiles (`~/.codest/profiles.toml`), for `/profile`.
    profiles: Arc<crate::core::config::ProfileSet>,
    /// The profile this run or restored session last activated; `None` when
    /// running without one.
    active_profile: Option<String>,
    /// The effective system prompt as labeled sections (`base` +
    /// `plugin:<id>`), kept in registration order for the `/context` report.
    system_sections: Vec<(String, String)>,
    /// Per-request wire facts (protocol, endpoint, `body_overrides`) derived
    /// from the active route; refreshed on every model switch.
    request_facts: RequestFacts,
    /// Tools the plugins declared; static for the whole run.
    tools: Vec<ToolSpec>,
    /// The active session's enabled tools (`/tools`), mirroring the worker.
    /// Updated optimistically on a toggle and authoritatively on
    /// `ToolsSwitched` (which also resyncs after session switches: the set
    /// is per session).
    enabled_tools: std::collections::BTreeSet<String>,
    /// [`Self::enabled_tools`] as a flat slice for [`PluginInfo`], rebuilt
    /// whenever the set changes (the struct is `Copy`, so it cannot borrow a
    /// lazily-built `Vec`).
    enabled_tools_vec: Vec<String>,
    /// Whether thinking cards render expanded by default. Global across
    /// sessions; Ctrl+T and `/thinking` both toggle it, and committed
    /// Thinking cells re-fold with the next frame. Independent of
    /// [`Self::show_tool_detail`]: the two card kinds fold separately.
    show_thinking: bool,
    /// Ctrl+O's tool-output switch: tool cards show their full output
    /// instead of the three-line preview while it is set. Says nothing about
    /// thinking cards — that is [`Self::show_thinking`].
    show_tool_detail: bool,
    /// A short-lived line for the footer, with the moment it expires. Toggles
    /// like Ctrl+T and Ctrl+O report here instead of pushing a transcript
    /// notice: the feedback is worth a glance, not a permanent row in the
    /// message stream that survives scrolling and session restores.
    status: Option<(String, Instant)>,
    /// When the current thinking stream began, for the settled card's
    /// duration chip. Set on the first visible delta of a stream.
    thinking_started: Option<Instant>,
    composer: Composer,
    /// Selected row in the slash-command menu.
    command_selection: usize,
    /// True after Esc dismissed the menu for the current input.
    command_menu_dismissed: bool,
    busy: bool,
    tick: usize,
    quit: bool,
    /// Plugin status segments (`plugin:key` → segment), in registration
    /// order. `[status_bar] order` decides display priority at render time.
    status_segments: Vec<(String, crate::plugins::StatusSegment)>,
    /// The footer's `[status_bar]` config: the master switch and the display
    /// priority for plugin segments.
    status_bar: StatusBarConfig,
    /// Per-plugin settings from `config.toml`, handed to every hook through
    /// [`PluginInfo::plugin_configs`].
    plugin_settings: Vec<(String, PluginSettings)>,
    /// Effects plugin background tasks sent through the host's bridge.
    plugin_effects: tokio::sync::mpsc::UnboundedReceiver<PluginEffect>,
    plugin_effects_closed: bool,
    /// Handles of the tasks plugins spawned via `PluginContext::spawn`;
    /// aborted on exit after the host's cancellation token fires.
    plugin_tasks: Vec<tokio::task::JoinHandle<()>>,
    prompts: Vec<String>,
    prompt_index: Option<usize>,
    draft: String,
    last_ctrl_c: Option<Instant>,
    /// Inline mode: current pane rectangle (always pinned to the bottom row).
    inline_top: u16,
    inline_height: u16,
    inline_rows: u16,
    inline_columns: u16,
}

/// State of the open `/model` picker.
struct ModelMenu {
    items: Vec<ModelMenuItem>,
    selected: usize,
}

struct ModelMenuItem {
    selection: Selection,
    /// Declared or discovered context window, shown in the picker row.
    context_window: Option<u64>,
    current: bool,
}

/// State of the open `/effort` picker.
struct EffortMenu {
    items: Vec<EffortMenuItem>,
    selected: usize,
}

/// One row of the `/effort` picker; `level: None` is the `off` entry that
/// clears the override.
struct EffortMenuItem {
    level: Option<String>,
    current: bool,
}

/// State of the open `/tools` picker.
struct ToolsMenu {
    items: Vec<ToolsMenuItem>,
    selected: usize,
}

/// One row of the `/tools` picker: a declared tool and its checkbox. Space
/// toggles the entry; Enter applies the whole set as one op.
struct ToolsMenuItem {
    name: String,
    /// The tool only inspects state (`read`) — advisory, shown as a marker
    /// and later used by read-only profiles.
    read_only: bool,
    enabled: bool,
}

/// Per-request wire facts for `/context`, derived from the active route's
/// provider instance. An empty route (nothing configured) yields empty
/// strings everywhere.
#[derive(Debug, Clone, Default)]
struct RequestFacts {
    protocol: String,
    base_url: String,
    endpoint: String,
    /// The instance's `body_overrides` as compact `key=value` strings, values
    /// truncated for display.
    overrides: Vec<(String, String)>,
}

/// One-line display form of a `body_overrides` JSON value (`0.2`,
/// `"high"`, `{"a":1}`), truncated to keep the report narrow.
fn compact_value(value: &serde_json::Value) -> String {
    let text = match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    crate::tui::session::truncated(&text, 120)
}

/// A plugin segment's tone mapped onto the shared palette: `Dim` is the
/// quiet default, `Accent` the highlight the state word gets while working,
/// `Warn` the warning color.
fn status_tone_style(tone: StatusTone) -> Style {
    match tone {
        StatusTone::Dim => Style::default().fg(theme::DIM),
        StatusTone::Accent => Style::default().fg(theme::ACCENT),
        StatusTone::Warn => Style::default().fg(theme::WARNING),
    }
}

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        session: AgentSession,
        pool: Arc<ModelPool>,
        notice_tx: tokio::sync::mpsc::UnboundedSender<String>,
        notices: tokio::sync::mpsc::UnboundedReceiver<String>,
        provider_tx: tokio::sync::mpsc::UnboundedSender<ProviderTask>,
        provider_tasks: tokio::sync::mpsc::UnboundedReceiver<ProviderTask>,
        plugin_effects: tokio::sync::mpsc::UnboundedReceiver<PluginEffect>,
        model_label: String,
        mode: TuiMode,
        plugins: Arc<PluginHost>,
        system_sections: Vec<(String, String)>,
        views: Vec<SessionView>,
        active_view: usize,
        profiles: Arc<crate::core::config::ProfileSet>,
        active_profile: Option<String>,
        initial_effort: Option<Option<String>>,
        status_bar: StatusBarConfig,
        plugin_settings: Vec<(String, PluginSettings)>,
    ) -> Self {
        let (inline_columns, inline_rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let inline_height = inline_pane_height(inline_rows, 1, 0, 0);
        let inline_top = inline_rows.saturating_sub(inline_height);

        let route = Selection::parse(&model_label).ok();
        // Mirrors the worker's seeding: a recorded or configured effort wins
        // outright — including an explicit `off` — and only an unset one is
        // seeded from the model's `default_level`. Footer and requests agree
        // until `/effort` overrides either side.
        let effort = initial_effort.unwrap_or_else(|| {
            route
                .as_ref()
                .and_then(|route| pool.model_info(route))
                .and_then(|model| model.default_level)
        });

        let tools = plugins.tools();
        let enabled_tools: std::collections::BTreeSet<String> = tools
            .iter()
            .map(|spec| spec.name.to_string())
            .collect();
        let enabled_tools_vec: Vec<String> = enabled_tools.iter().cloned().collect();
        let mut app = Self {
            session,
            plugins,
            pool,
            notice_tx,
            notices,
            notices_closed: false,
            route,
            model_label,
            mode,
            views,
            active_view,
            session_menu: None,
            model_menu: None,
            effort_menu: None,
            tools_menu: None,
            context_view: None,
            provider_wizard: None,
            provider_tx,
            provider_tasks,
            provider_tasks_closed: false,
            provider_cancel: None,
            effort,
            profiles,
            active_profile,
            system_sections,
            request_facts: RequestFacts::default(),
            tools,
            enabled_tools,
            enabled_tools_vec,
            show_thinking: false,
            show_tool_detail: false,
            status: None,
            thinking_started: None,
            composer: Composer::new(),
            command_selection: 0,
            command_menu_dismissed: false,
            busy: false,
            tick: 0,
            quit: false,
            status_segments: Vec::new(),
            status_bar,
            plugin_settings,
            plugin_effects,
            plugin_effects_closed: false,
            plugin_tasks: Vec::new(),
            prompts: Vec::new(),
            prompt_index: None,
            draft: String::new(),
            last_ctrl_c: None,
            inline_top,
            inline_height,
            inline_rows,
            inline_columns,
        };
        app.refresh_request_facts();
        app
    }

    /// Re-derive [`RequestFacts`] from the active route's provider instance.
    /// Called at startup and on every model switch, optimistic or confirmed.
    fn refresh_request_facts(&mut self) {
        // The registry is now swapped by `/provider`, so a snapshot is taken
        // first and the facts are cloned out of it.
        let registry = self.pool.registry();
        self.request_facts = match self
            .route
            .as_ref()
            .and_then(|route| registry.instance(&route.provider))
        {
            Some(instance) => RequestFacts {
                protocol: instance.protocol_id().to_string(),
                base_url: instance.base_url.clone().unwrap_or_default(),
                endpoint: instance
                    .base_url
                    .as_deref()
                    .map(endpoint_url)
                    .unwrap_or_default(),
                overrides: instance
                    .body_overrides
                    .iter()
                    .map(|(key, value)| {
                        (key.clone(), compact_value(value))
                    })
                    .collect(),
            },
            None => RequestFacts::default(),
        };
    }

    /// The view of the session being edited; only the active session is ever
    /// busy or streaming, because session commands wait for an idle agent.
    fn view(&self) -> &SessionView {
        &self.views[self.active_view]
    }

    fn view_mut(&mut self) -> &mut SessionView {
        &mut self.views[self.active_view]
    }

    async fn event_loop(&mut self, terminal: &mut Option<Tui>) -> Result<()> {
        let mut events = Box::pin(EventStream::new());
        let mut ticker = tokio::time::interval(Duration::from_millis(90));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            self.draw(terminal)?;
            if self.quit {
                break;
            }

            tokio::select! {
                maybe_event = events.next() => {
                    match maybe_event {
                        Some(Ok(event)) => self.handle_terminal_event(event),
                        Some(Err(error)) => return Err(error.into()),
                        None => break,
                    }
                }
                maybe_event = self.session.next_event() => {
                    match maybe_event {
                        Some(event) => self.handle_agent_event(event),
                        None => break,
                    }
                }
                maybe_task = self.provider_tasks.recv(), if !self.provider_tasks_closed => {
                    match maybe_task {
                        Some(task) => self.handle_provider_task(task),
                        None => self.provider_tasks_closed = true,
                    }
                }
                maybe_notice = self.notices.recv(), if !self.notices_closed => {
                    match maybe_notice {
                        Some(text) => {
                            self.view_mut().history.push(HistoryCell::Notice(text));
                        }
                        None => self.notices_closed = true,
                    }
                }
                maybe_effect = self.plugin_effects.recv(), if !self.plugin_effects_closed => {
                    // What plugin background tasks send lands here, one
                    // effect at a time, and applies through the same path as
                    // synchronous dispatch output.
                    match maybe_effect {
                        Some(effect) => self.apply_effects(vec![effect]),
                        None => self.plugin_effects_closed = true,
                    }
                }
                _ = ticker.tick() => self.on_tick(),
            }
        }

        self.session.shutdown();
        // A quit ends plugin work, not just the UI: give tasks the token,
        // then sweep whatever did not watch it.
        self.plugins.cancel_token().cancel();
        for handle in self.plugin_tasks.drain(..) {
            handle.abort();
        }
        Ok(())
    }

    fn draw(&mut self, terminal: &mut Option<Tui>) -> Result<()> {
        match self.mode {
            TuiMode::Fullscreen => {
                if let Some(terminal) = terminal.as_mut() {
                    terminal.draw(|frame| self.render_fullscreen(frame))?;
                }
                Ok(())
            }
            TuiMode::Inline => self.draw_inline(terminal),
        }
    }

    /// Inline mode: print newly finished transcript lines above the bottom pane,
    /// then redraw the pane itself.
    ///
    /// The pane is a fixed viewport pinned to the bottom row; transcript lines
    /// are written to the screen directly, scrolling the terminal so they end
    /// up in its native scrollback.
    fn draw_inline(&mut self, terminal: &mut Option<Tui>) -> Result<()> {
        let Some(tui) = terminal.as_mut() else {
            return Ok(());
        };
        let size = tui.size()?;

        // The `/context` viewer takes the whole pane while open; closing it
        // restores the normal pane, scrollback untouched.
        if self.context_view.is_some() {
            let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
            tui.draw(|frame| {
                let view = self.context_view.as_ref().expect("checked above");
                view.render(frame, area);
            })?;
            return Ok(());
        }
        let width = size.width as usize;
        let rows = size.height;

        let composer_width = width.saturating_sub(2).max(1);
        let composer_rows = self.composer.visual_rows(composer_width);
        let desired_rows = composer_rows.len().clamp(1, MAX_COMPOSER_ROWS);
        let menu = self.menu_lines(MAX_COMMAND_MENU_ROWS);
        let menu_rows = menu.len();
        // The unsettled tail of the streaming message, or — while the model is
        // still reasoning and no content has started — a thinking line. The
        // whole tail lives here because scrollback takes only settled lines.
        // Its height is capped so the pane keeps a stable size while a turn
        // streams; growth would scroll the tail into scrollback.
        let live_rows = if self.view().live.is_some()
            || self.view().running_tool.is_some()
            || self
                .view()
                .live_thinking
                .as_ref()
                .is_some_and(|text| !text.is_empty())
        {
            inline_live_rows(rows, desired_rows, menu_rows).min(MAX_LIVE_TAIL_ROWS)
        } else {
            0
        };
        let live = self.inline_live_tail(width, live_rows);
        let pane_height = inline_pane_height(rows, desired_rows, live_rows, menu_rows);
        let composer_height = desired_rows as u16 + 1;
        let menu_height = (menu_rows as u16).min(
            pane_height.saturating_sub(live_rows as u16 + composer_height + 1),
        );
        let new_top = rows.saturating_sub(pane_height);

        let resized = rows != self.inline_rows || size.width != self.inline_columns;
        let old_top = self.inline_top;
        let old_height = self.inline_height;

        // Flush finished transcript lines before moving the pane. On a terminal
        // resize, defer the flush one frame so wrapping uses the new width.
        let mut wrote_lines = false;
        if !resized {
            let history_rows = rows.saturating_sub(old_height) as usize;
            if history_rows > 0 {
                let pending = self.take_inline_pending(width);
                if !pending.is_empty() {
                    insert_inline_lines(&pending, rows, old_height)?;
                    wrote_lines = true;
                }
            }
        }

        // Growing the pane consumes transcript rows; push them to scrollback.
        if pane_height > old_height && !resized {
            scroll_screen_up(rows, (pane_height - old_height) as usize)?;
        }

        let geometry_changed = resized || pane_height != old_height;
        if geometry_changed {
            let clear_top = old_top.min(new_top);
            let mut stdout = std::io::stdout();
            execute!(
                stdout,
                MoveTo(0, clear_top),
                Clear(ClearType::FromCursorDown)
            )?;
        }

        let rect = Rect::new(0, new_top, size.width, pane_height);
        if geometry_changed || wrote_lines {
            // The screen scrolled (or the pane moved) underneath Ratatui's
            // retained buffers, so start the next frame from a blank canvas.
            tui.resize(rect)?;
            tui.current_buffer_mut().reset();
        }
        self.inline_top = new_top;
        self.inline_height = pane_height;
        self.inline_rows = rows;
        self.inline_columns = size.width;

        tui.draw(|frame| {
            self.render_inline_pane(frame, &live, &menu, menu_height, &composer_rows);
        })?;
        Ok(())
    }

    /// The rows of the streaming message that cannot go to scrollback yet.
    ///
    /// Whatever markdown has settled is printed by [`take_inline_pending`];
    /// only the mutable tail stays here, capped at `max_rows` so a long
    /// unfinished block cannot push the composer off screen. The tail is
    /// bottom-anchored: the newest text is what the reader is watching.
    fn inline_live_tail(&self, width: usize, max_rows: usize) -> Vec<Line<'static>> {
        let view = self.view();
        let thinking_expanded = self.show_thinking;
        // While reasoning with nothing visible yet, show the thinking tail.
        let thinking_only = view.live.as_ref().is_some_and(String::is_empty)
            && view
                .live_thinking
                .as_ref()
                .is_some_and(|text| !text.is_empty());
        let rows = if thinking_only {
            if thinking_expanded {
                let frame = chat::SPINNER[self.tick % chat::SPINNER.len()];
                view.live_thinking
                    .as_ref()
                    .map(|text| chat::thinking_live_lines(text, width, true, frame))
            } else {
                let frame = chat::SPINNER[self.tick % chat::SPINNER.len()];
                Some(chat::thinking_live_lines("", width, false, frame))
            }
        } else if let Some(running) = &view.running_tool {
            // A tool is executing: its live card fills the tail until the
            // completion event settles it into history.
            let elapsed = chat::format_duration(running.started_at.elapsed().as_millis() as u64);
            let frame = chat::SPINNER[self.tick % chat::SPINNER.len()];
            Some(chat::running_tool_lines(
                &running.name,
                &running.arguments,
                &elapsed,
                frame,
                width,
            ))
        } else {
            view.live.as_ref().map(|live| {
                let (lines, settled) = chat::assistant_lines_tracked(live, width, self.busy);
                lines[settled.min(lines.len())..].to_vec()
            })
        };
        let mut rows = rows.unwrap_or_default();
        let start = rows.len().saturating_sub(max_rows);
        rows.drain(..start);
        rows
    }

    /// Collect transcript lines that have not reached the terminal scrollback
    /// yet, advancing the bookkeeping that prevents reprinting them. Only the
    /// active session's view is flushed; a background view catches up when it
    /// becomes active again.
    fn take_inline_pending(&mut self, width: usize) -> Vec<Line<'static>> {
        // The fold flags freeze per frame: a card flushes once, folded or
        // expanded as the toggles stand right now — scrollback cannot be
        // rewritten to honor a later Ctrl+T or Ctrl+O.
        let display = chat::CardDisplay {
            thinking: self.show_thinking,
            tool_detail: self.show_tool_detail,
        };
        let view = self.view_mut();
        // Inline mode has no viewport to park: the history goes to the
        // terminal's own scrollback, where a report is readable in full.
        view.park_on_last_report = false;
        let mut pending: Vec<Line<'static>> = Vec::new();

        // Remaining lines of an assistant message that just finished.
        if let Some(text) = view.inline_final_text.take() {
            let lines = chat::assistant_lines(&text, width, false);
            let start = view.live_committed.min(lines.len());
            pending.extend(lines[start..].iter().cloned());
            pending.push(Line::from(""));
            view.live_committed = 0;
        }

        // Transcript cells that have not been written to scrollback yet.
        while view.printed_cells < view.history.len() {
            pending.extend(view.history[view.printed_cells].lines(width, &display));
            pending.push(Line::from(""));
            view.printed_cells += 1;
        }

        // Lines of the in-flight assistant message that markdown has already
        // settled. Scrollback cannot be rewritten, so only the final prefix
        // goes out; the mutable tail keeps re-rendering in the pane.
        if let Some(live) = &view.live {
            let (lines, settled) = chat::assistant_lines_tracked(live, width, false);
            if settled > view.live_committed {
                pending.extend(lines[view.live_committed..settled].iter().cloned());
                view.live_committed = settled;
            }
        }

        pending
    }

    fn handle_terminal_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key),
            Event::Paste(text) => {
                self.composer.insert_str(&text);
                self.command_selection = 0;
                self.command_menu_dismissed = false;
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            _ => {}
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.mode != TuiMode::Fullscreen {
            return;
        }
        let amount = if mouse.modifiers.contains(KeyModifiers::ALT) {
            ALT_WHEEL_LINES
        } else {
            WHEEL_LINES
        };
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_up(amount),
            MouseEventKind::ScrollDown => self.scroll_down(amount),
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let before = self.composer.text();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // The `/context` viewer owns the whole keyboard while it is open —
        // fully modal, like the provider wizard but stricter: nothing leaks
        // to the composer or the menus underneath.
        if self.context_view.is_some() {
            if let Some(action) = self
                .context_view
                .as_mut()
                .expect("checked above")
                .handle_key(key)
            {
                self.apply_context_action(action);
            }
            return;
        }

        // The `/provider` wizard owns the keyboard while it is open: its text
        // steps type into the composer, its pickers take ↑↓, and Enter/Esc mean
        // "next"/"back" rather than "submit"/"interrupt". It returns here so
        // the pickers below never see the same key.
        if self.provider_wizard.is_some() {
            if self.handle_provider_key(key) {
                return;
            }
            // A text step: typing and editing fall through to the composer.
            match key.code {
                KeyCode::Char(c) if !ctrl && !alt => {
                    self.composer.insert_char(c);
                    return;
                }
                KeyCode::Backspace => {
                    self.composer.backspace();
                    return;
                }
                KeyCode::Delete => {
                    self.composer.delete();
                    return;
                }
                KeyCode::Left => {
                    self.composer.move_left();
                    return;
                }
                KeyCode::Right => {
                    self.composer.move_right();
                    return;
                }
                KeyCode::Home => {
                    self.composer.home();
                    return;
                }
                KeyCode::End => {
                    self.composer.end();
                    return;
                }
                KeyCode::Char('a') if ctrl => {
                    self.composer.home();
                    return;
                }
                KeyCode::Char('e') if ctrl => {
                    self.composer.end();
                    return;
                }
                KeyCode::Char('u') if ctrl => {
                    self.composer.kill_to_start();
                    return;
                }
                KeyCode::Char('k') if ctrl => {
                    self.composer.kill_to_end();
                    return;
                }
                KeyCode::Char('w') if ctrl => {
                    self.composer.delete_word_backward();
                    return;
                }
                KeyCode::Enter if ctrl || alt || shift => {
                    self.composer.insert_char('\n');
                    return;
                }
                KeyCode::Char('c') if ctrl => {
                    // Ctrl+C always leaves the wizard rather than quitting.
                    self.close_provider_wizard();
                    return;
                }
                _ => return,
            }
        }

        // The `/sessions`, `/model`, and `/effort` pickers are modal for
        // navigation keys: ↑↓ move, plain Enter confirms, Esc closes.
        // Everything else (typing, Ctrl+C, ...) still reaches its normal
        // handler while a menu is open.
        if self.session_menu.is_some() {
            match key.code {
                KeyCode::Up => return self.move_session_selection(-1),
                KeyCode::Down => return self.move_session_selection(1),
                KeyCode::Enter if !ctrl && !alt && !shift => return self.confirm_session_switch(),
                KeyCode::Esc => {
                    self.session_menu = None;
                    return;
                }
                _ => {}
            }
        }
        if self.model_menu.is_some() {
            match key.code {
                KeyCode::Up => return self.move_model_selection(-1),
                KeyCode::Down => return self.move_model_selection(1),
                KeyCode::Enter if !ctrl && !alt && !shift => return self.confirm_model_switch(),
                KeyCode::Char('r') if !ctrl && !alt && !shift => return self.refresh_model(),
                KeyCode::Esc => {
                    self.model_menu = None;
                    return;
                }
                _ => {}
            }
        }
        if self.effort_menu.is_some() {
            match key.code {
                KeyCode::Up => return self.move_effort_selection(-1),
                KeyCode::Down => return self.move_effort_selection(1),
                KeyCode::Enter if !ctrl && !alt && !shift => return self.confirm_effort_selection(),
                KeyCode::Esc => {
                    self.effort_menu = None;
                    return;
                }
                _ => {}
            }
        }
        // The `/tools` picker also takes Space to flip the highlighted
        // checkbox; Enter applies the whole set as one op.
        if self.tools_menu.is_some() {
            match key.code {
                KeyCode::Up => return self.move_tools_selection(-1),
                KeyCode::Down => return self.move_tools_selection(1),
                KeyCode::Char(' ') if !ctrl && !alt && !shift => return self.toggle_tools_entry(),
                KeyCode::Enter if !ctrl && !alt && !shift => return self.confirm_tools_selection(),
                KeyCode::Esc => {
                    self.tools_menu = None;
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('c') if ctrl => self.on_ctrl_c(),
            // Thinking and tool cards fold independently. Ctrl+T is the
            // thinking switch (`/thinking` does the same, from the command
            // line); Ctrl+O stays the tool-output switch. Keeping them apart
            // means a reader can follow the reasoning without dumping a wall
            // of command output over it, or vice versa.
            //
            // Both report in the footer rather than the transcript: they are
            // view switches, and a stream notice for one would outlive the
            // keystroke in scrollback and in the restored session.
            KeyCode::Char('t') if ctrl => {
                self.show_thinking = !self.show_thinking;
                let notice = if self.show_thinking {
                    "thinking expanded"
                } else {
                    "thinking folded"
                };
                self.flash(notice);
            }
            KeyCode::Char('o') if ctrl => {
                self.show_tool_detail = !self.show_tool_detail;
                let notice = if self.show_tool_detail {
                    "tool output expanded"
                } else {
                    "tool output folded"
                };
                self.flash(notice);
            }
            KeyCode::Char('d') if ctrl => {
                if self.composer.is_empty() {
                    self.quit = true;
                }
            }
            KeyCode::Char('j') if ctrl => self.composer.insert_char('\n'),
            KeyCode::Char('a') if ctrl => self.composer.home(),
            KeyCode::Char('e') if ctrl => self.composer.end(),
            KeyCode::Char('u') if ctrl => self.composer.kill_to_start(),
            KeyCode::Char('k') if ctrl => self.composer.kill_to_end(),
            KeyCode::Char('w') if ctrl => self.composer.delete_word_backward(),
            KeyCode::Char('p') if ctrl => self.history_prev(),
            KeyCode::Char('n') if ctrl => self.history_next(),
            KeyCode::Char(c) if !ctrl && !alt => self.composer.insert_char(c),
            KeyCode::Enter if ctrl || alt || shift => self.composer.insert_char('\n'),
            KeyCode::Enter => self.on_enter(),
            KeyCode::Tab => {
                self.complete_command_selection();
            }
            KeyCode::Backspace if alt => self.composer.delete_word_backward(),
            KeyCode::Backspace => self.composer.backspace(),
            KeyCode::Delete => self.composer.delete(),
            KeyCode::Left => self.composer.move_left(),
            KeyCode::Right => self.composer.move_right(),
            KeyCode::Up => self.on_up(),
            KeyCode::Down => self.on_down(),
            KeyCode::Home => self.composer.home(),
            KeyCode::End => self.composer.end(),
            KeyCode::Esc => self.on_escape(),
            // Inline mode leaves paging to the terminal's native scrollback.
            KeyCode::PageUp if self.mode == TuiMode::Fullscreen => self.scroll_up(5),
            KeyCode::PageDown if self.mode == TuiMode::Fullscreen => self.scroll_down(5),
            _ => {}
        }

        // Any edit re-opens the menu and re-anchors the selection; navigation
        // and Esc leave the composer untouched and keep their state.
        if self.composer.text() != before {
            self.command_selection = 0;
            self.command_menu_dismissed = false;
        }
    }

    fn on_tick(&mut self) {
        if self.busy {
            self.tick = self.tick.wrapping_add(1);
        }
        // The footer message is time-boxed: the loop redraws on every tick, so
        // dropping it here is all it takes for the hint row to come back.
        if self
            .status
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() >= *until)
        {
            self.status = None;
        }
    }

    /// Show a short-lived footer message. Used for view toggles (Ctrl+T,
    /// Ctrl+O), where the feedback matters for a second and a transcript row
    /// would pollute the conversation for the rest of the session.
    fn flash(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), Instant::now() + STATUS_TTL));
    }

    fn on_ctrl_c(&mut self) {
        if self.busy {
            self.session.interrupt();
            self.last_ctrl_c = None;
            return;
        }
        if !self.composer.is_empty() {
            self.composer.clear();
            self.last_ctrl_c = None;
            return;
        }

        let now = Instant::now();
        if self
            .last_ctrl_c
            .is_some_and(|previous| now.duration_since(previous) < Duration::from_secs(2))
        {
            self.quit = true;
        } else {
            self.last_ctrl_c = Some(now);
        }
    }

    fn on_escape(&mut self) {
        if !self.command_matches().is_empty() {
            self.command_menu_dismissed = true;
            return;
        }
        if self.busy {
            self.session.interrupt();
        }
    }

    fn on_enter(&mut self) {
        // A visible menu is authoritative: Enter runs the highlighted command
        // even when only a prefix has been typed.
        self.complete_command_selection();
        self.try_submit();
    }

    /// The partial command name after the leading `/`, or `None` when the
    /// composer does not hold an unfinished slash command.
    fn command_query(&self) -> Option<String> {
        let text = self.composer.text();
        let rest = text.trim_start().strip_prefix('/')?;
        if rest.chars().any(char::is_whitespace) {
            return None;
        }
        Some(rest.to_ascii_lowercase())
    }

    fn command_matches(&self) -> Vec<CommandCandidate> {
        if self.busy
            || self.command_menu_dismissed
            || self.session_menu.is_some()
            || self.model_menu.is_some()
            || self.effort_menu.is_some()
            || self.tools_menu.is_some()
            || self.provider_wizard.is_some()
        {
            return Vec::new();
        }
        let Some(query) = self.command_query() else {
            return Vec::new();
        };
        self.plugins.suggestions(&query)
    }

    fn command_menu_open(&self) -> bool {
        !self.command_matches().is_empty()
    }

    /// Move the menu selection by one row, wrapping at both ends. Returns false
    /// when the menu is closed so the caller can fall back to other behavior.
    fn move_command_selection(&mut self, delta: isize) -> bool {
        let len = self.command_matches().len();
        if len == 0 {
            return false;
        }
        let current = self.command_selection.min(len - 1);
        self.command_selection = if delta < 0 {
            (current + len - 1) % len
        } else {
            (current + 1) % len
        };
        true
    }

    /// Replace the partial name with the selected command. Returns false when
    /// there is no menu open.
    fn complete_command_selection(&mut self) -> bool {
        let matches = self.command_matches();
        if matches.is_empty() {
            return false;
        }
        let index = self.command_selection.min(matches.len() - 1);
        self.composer.set_text(format!("/{}", matches[index].name));
        self.command_selection = 0;
        true
    }

    fn try_submit(&mut self) {
        if self.busy {
            return;
        }
        let text = self.composer.text();
        if text.trim().is_empty() {
            return;
        }

        match self.plugins.parse(&text) {
            CommandInput::Command(invocation) => {
                // Commands are local — none of them need a model — so they
                // keep working on an empty route.
                self.clear_for_submit(&text);
                self.run_command(invocation);
                return;
            }
            CommandInput::Invalid(error) => {
                self.clear_for_submit(&text);
                self.view_mut()
                    .history
                    .push(HistoryCell::Error(error.to_string()));
                return;
            }
            CommandInput::Message => {}
        }

        // A message needs a usable route: either a selected model on a
        // complete endpoint, or actionable guidance. The composer is
        // deliberately untouched in every guidance branch so the message
        // survives until the setup is fixed.
        if self.route.is_none() {
            let guidance = if self.pool.registry().instances().is_empty() {
                crate::core::providers::NO_MODEL_GUIDANCE.to_string()
            } else {
                crate::core::providers::NO_MODEL_SELECTED.to_string()
            };
            self.view_mut().history.push(HistoryCell::Error(guidance));
            return;
        }
        if let Some(prompt) = self.route_guidance() {
            self.view_mut().history.push(HistoryCell::Error(prompt));
            return;
        }

        self.clear_for_submit(&text);
        self.submit_user_turn(text);
    }

    /// Why the active route cannot serve a message right now, if it can't:
    /// the provider is only half specified (a lone `--base-url` or
    /// `--api-key`).
    fn route_guidance(&self) -> Option<String> {
        let route = self.route.as_ref()?;
        self.pool.missing_credential(&route.provider)
    }

    /// Reset the composer for an accepted submission: clear it, record the
    /// input in the prompt history, and reset the input navigation state.
    fn clear_for_submit(&mut self, text: &str) {
        self.composer.clear();
        self.command_selection = 0;
        self.command_menu_dismissed = false;
        if self.prompts.last().map(String::as_str) != Some(text) {
            self.prompts.push(text.to_string());
        }
        self.reset_input_navigation();
    }

    /// Dispatch a command. The built-in session commands are front-end
    /// behavior and never reach the plugin host; their specs live in the
    /// shared command table so parsing, the command menu, and `/help` agree.
    fn run_command(&mut self, invocation: Invocation) {
        match invocation.spec.name {
            "new" => return self.start_new_session(),
            "rename" => return self.rename_session(&invocation.args),
            "sessions" => return self.open_session_menu(),
            "model" => return self.open_model_menu(),
            "provider" => return self.open_provider_wizard(),
            // `/effort` with a level sets it directly; with no argument it
            // opens the picker (the argument policy is Optional).
            "effort" => {
                if invocation.args.is_empty() {
                    return self.open_effort_menu();
                }
                return self.set_effort(&invocation.args);
            }
            "thinking" => return self.toggle_thinking(),
            // `/profile` with a name applies the profile's presets at the
            // next turn boundary; with no argument it lists the profiles.
            "profile" => {
                if invocation.args.is_empty() {
                    return self.list_profiles();
                }
                return self.request_profile_switch(&invocation.args);
            }
            // `/tools` with no argument opens the checkbox picker; with a
            // tool name it flips that tool directly (`on`/`off` pin it).
            "tools" => {
                if invocation.args.is_empty() {
                    return self.open_tools_menu();
                }
                return self.set_tool_enabled(&invocation.args);
            }
            // `/context` has two faces: the classic prompt report while the
            // session has no messages, the context viewer once it does.
            "context" => return self.open_context_view(),
            _ => {}
        }
        let output = self
            .plugins
            .dispatch_command(&invocation, self.plugin_info());
        self.apply_output(output);
    }

    /// `/new`: archive the current session and start a fresh one. The view
    /// switches immediately; the core confirms with `SessionStarted`, and
    /// because ops are FIFO the next turn lands on the new conversation.
    fn start_new_session(&mut self) {
        self.session.new_session();
        let index = self.views.len();
        self.views.push(SessionView::started());
        self.active_view = index;
    }

    /// `/sessions` (alias `/resume`): open the picker over the recorded
    /// sessions, starting from the active one.
    fn open_session_menu(&mut self) {
        self.command_menu_dismissed = true;
        self.session_menu = Some(self.active_view);
    }

    /// `/rename <name>`: name the current session for the session menu. By
    /// default a session is named after its first user message; an explicit
    /// rename overrides that. The op is recorded in the session's rollout so
    /// the name survives restarts.
    fn rename_session(&mut self, args: &str) {
        // The menu renders one row per session, so keep the name single-line.
        let name = args.replace(['\r', '\n'], " ");
        self.session.rename_session(self.active_view, &name);
        let view = self.view_mut();
        view.name = Some(name.clone());
        view.history.push(HistoryCell::Notice(format!(
            "session renamed to \"{name}\""
        )));
    }

    fn move_session_selection(&mut self, delta: isize) {
        let Some(selection) = self.session_menu else {
            return;
        };
        let len = self.views.len();
        self.session_menu = Some(if delta < 0 {
            (selection + len - 1) % len
        } else {
            (selection + 1) % len
        });
    }

    /// Confirm the picker: switching to the already-active session just
    /// closes the menu.
    fn confirm_session_switch(&mut self) {
        let Some(selection) = self.session_menu.take() else {
            return;
        };
        if selection != self.active_view {
            self.switch_to_session(selection);
        }
    }

    /// Switch the front-end to a recorded session and tell the core. Ops are
    /// FIFO, so the next user turn is answered with the restored context.
    fn switch_to_session(&mut self, index: usize) {
        self.active_view = index;
        let view = self.view_mut();
        view.follow = true;
        view.scroll = view.max_scroll;
        view.history
            .push(HistoryCell::Notice("Session resumed".to_string()));
        self.session.switch_session(index);
    }

    /// `/model`: open the picker over the registry's selections, starting at
    /// the active one. The items are re-read from the pool every time, so a
    /// background models-discovery refresh is picked up without a restart.
    fn open_model_menu(&mut self) {
        let selections = self.pool.selections();
        if selections.is_empty() {
            self.command_menu_dismissed = true;
            // Nothing to list yet. If a complete endpoint can still discover
            // its models, kick the discovery off from here; otherwise tell
            // the user what is actually missing.
            let fetchable: Vec<String> = self
                .pool
                .registry()
                .instances()
                .iter()
                .filter(|instance| {
                    instance.should_fetch_models()
                        && self.pool.missing_credential(&instance.name).is_none()
                        && self.pool.can_discover(&instance.name)
                })
                .map(|instance| instance.name.clone())
                .collect();
            if !fetchable.is_empty() {
                for name in &fetchable {
                    self.spawn_discovery(name);
                }
                self.view_mut().history.push(HistoryCell::Notice(format!(
                    "discovering models from {} — run /model again in a moment",
                    fetchable.join(", ")
                )));
            } else if let Some(guidance) = self
                .pool
                .registry()
                .instances()
                .iter()
                .find_map(|instance| self.pool.missing_credential(&instance.name))
            {
                self.view_mut().history.push(HistoryCell::Error(guidance));
            } else if self.pool.registry().instances().is_empty() {
                self.view_mut()
                    .history
                    .push(HistoryCell::Error(NO_MODEL_GUIDANCE.to_string()));
            } else {
                self.view_mut()
                    .history
                    .push(HistoryCell::Error(NO_MODEL_SELECTED.to_string()));
            }
            return;
        }
        let items: Vec<ModelMenuItem> = selections
            .into_iter()
            .map(|selection| ModelMenuItem {
                current: selection.as_str() == self.model_label,
                context_window: self
                    .pool
                    .model_info(&selection)
                    .and_then(|model| model.context_window),
                selection,
            })
            .collect();
        let selected = items
            .iter()
            .position(|item| item.current)
            .unwrap_or(0);
        self.command_menu_dismissed = true;
        self.model_menu = Some(ModelMenu { items, selected });
    }

    fn move_model_selection(&mut self, delta: isize) {
        let Some(menu) = self.model_menu.as_mut() else {
            return;
        };
        let len = menu.items.len();
        menu.selected = if delta < 0 {
            (menu.selected + len - 1) % len
        } else {
            (menu.selected + 1) % len
        };
    }

    /// Confirm the picker: switching to the already-active model just closes
    /// the menu. The label updates optimistically; the core's `ModelSwitched`
    /// event confirms (a failure arrives as an error cell).
    fn confirm_model_switch(&mut self) {
        let Some(menu) = self.model_menu.take() else {
            return;
        };
        let Some(item) = menu.items.get(menu.selected) else {
            return;
        };
        let selection = item.selection.as_str();
        if selection == self.model_label {
            return;
        }
        self.model_label = selection.to_string();
        self.route = Some(item.selection.clone());
        self.refresh_request_facts();
        let view = self.view_mut();
        view.history
            .push(HistoryCell::Notice(format!("switched to {selection}")));
        self.session.switch_model(selection);
    }

    /// `r` in the picker: re-run models discovery for the selected item's
    /// provider in the background; the completion notice lands as a
    /// transcript notice and the menu shows the merged list on its next open.
    /// Run models discovery for one provider in the background; the outcome
    /// lands as a transcript notice via the notification channel.
    fn spawn_discovery(&self, provider: &str) {
        let pool = self.pool.clone();
        let tx = self.notice_tx.clone();
        let provider = provider.to_string();
        tokio::spawn(async move {
            match pool.refresh(&provider).await {
                Ok(count) => {
                    let _ = tx.send(format!(
                        "model discovery for {provider} found {count} model{}",
                        if count == 1 { "" } else { "s" }
                    ));
                }
                Err(error) => {
                    let _ = tx.send(format!("model discovery for {provider} failed: {error}"));
                }
            }
        });
    }

    fn refresh_model(&mut self) {
        let Some(menu) = self.model_menu.as_ref() else {
            return;
        };
        let Some(item) = menu.items.get(menu.selected) else {
            return;
        };
        self.spawn_discovery(&item.selection.provider);
    }

    // -----------------------------------------------------------------------
    // `/provider` wizard
    // -----------------------------------------------------------------------

    /// `/provider`: open the wizard. The sign-in targets are assembled from the
    /// plugins' entries crossed with the configured instances, so an instance
    /// whose protocol a plugin serves gets a sign-in row even though the plugin
    /// knows nothing about `providers.toml`.
    fn open_provider_wizard(&mut self) {
        self.command_menu_dismissed = true;
        let targets = self.sign_in_targets();
        let configured = self.configured_rows();
        let mut wizard = crate::tui::provider_wizard::Wizard::new(targets);
        wizard.configured = configured;
        self.provider_wizard = Some(wizard);
        self.composer.clear();
    }

    /// The read-only "what is configured" rows, with a sign-in badge for any
    /// protocol a plugin serves.
    fn configured_rows(&self) -> Vec<crate::tui::provider_wizard::ConfiguredRow> {
        use crate::tui::provider_wizard::ConfiguredRow;

        let registry = self.pool.registry();
        registry
            .instances()
            .iter()
            .map(|instance| {
                let protocol = instance.protocol_id();
                let status = self.plugins.auth_status(&instance.name, protocol);
                ConfiguredRow {
                    name: instance.name.clone(),
                    protocol: protocol.to_string(),
                    base_url: instance.base_url.clone().unwrap_or_default(),
                    models: self.pool.models_of(&instance.name).len(),
                    status: match status {
                        cst_plugin_api::auth::AuthStatus::Unsupported => None,
                        other => Some(other),
                    },
                }
            })
            .collect()
    }

    /// Every sign-in row the menu offers: one per configured instance whose
    /// protocol has a plugin entry, plus one per entry that no instance uses
    /// yet (selecting it signs in and then creates the instance).
    fn sign_in_targets(&self) -> Vec<crate::tui::provider_wizard::SignInTarget> {
        use crate::tui::provider_wizard::SignInTarget;

        let registry = self.pool.registry();
        let entries = self.plugins.auth_providers();
        let mut targets: Vec<SignInTarget> = Vec::new();

        for instance in registry.instances() {
            let protocol = instance.protocol_id();
            let Some((_, provider)) = entries
                .iter()
                .find(|(_, provider)| provider.protocol == protocol)
                .copied()
            else {
                continue;
            };
            let status = self.plugins.auth_status(&instance.name, protocol);
            targets.push(SignInTarget {
                instance: instance.name.clone(),
                configured: true,
                provider,
                status,
            });
        }

        // Entries whose protocol no instance uses yet: the wizard creates the
        // instance on success.
        let configured_protocols: Vec<&str> = targets
            .iter()
            .map(|target| target.provider.protocol)
            .collect();
        for (_, provider) in entries {
            if configured_protocols.contains(&provider.protocol) {
                continue;
            }
            if targets.iter().any(|target| target.instance == provider.id) {
                continue;
            }
            targets.push(SignInTarget {
                instance: provider.id.to_string(),
                configured: false,
                provider,
                status: self.plugins.auth_status(provider.id, provider.protocol),
            });
        }
        targets
    }

    /// Route a keystroke into the wizard. Returns `true` when the wizard
    /// consumed it, so the composer never sees it.
    fn handle_provider_key(&mut self, key: KeyEvent) -> bool {
        let Some(wizard) = self.provider_wizard.as_ref() else {
            return false;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // While background work runs, only cancellation is meaningful.
        if wizard.step.is_busy() {
            if key.code == KeyCode::Esc {
                self.cancel_provider_task();
            }
            return true;
        }

        match key.code {
            KeyCode::Up if wizard.step.is_picker() => {
                if let Some(wizard) = self.provider_wizard.as_mut() {
                    wizard.move_selection(-1);
                }
                true
            }
            KeyCode::Down if wizard.step.is_picker() => {
                if let Some(wizard) = self.provider_wizard.as_mut() {
                    wizard.move_selection(1);
                }
                true
            }
            KeyCode::Enter if !ctrl && !alt && !shift => {
                self.confirm_provider_step();
                true
            }
            KeyCode::Esc => {
                self.back_provider_step();
                true
            }
            _ => false,
        }
    }

    /// Enter inside the wizard: confirm the highlighted row, or submit the text
    /// field the current step collects.
    fn confirm_provider_step(&mut self) {
        let existing: Vec<String> = self
            .pool
            .registry()
            .instances()
            .iter()
            .map(|instance| instance.name.clone())
            .collect();
        let Some(wizard) = self.provider_wizard.as_mut() else {
            return;
        };
        let action = if let Some(field) = wizard.field() {
            let value = self.composer.text();
            match wizard.submit_field(&value, &existing) {
                Ok(action) => {
                    self.composer.clear();
                    if field.is_secret() {
                        // A secret must not survive in the input history.
                        self.prompts.pop();
                    }
                    action
                }
                Err(error) => {
                    wizard.status = Some(error.message());
                    return;
                }
            }
        } else {
            wizard.confirm()
        };
        self.run_provider_action(action);
    }

    /// Esc inside the wizard: one step back, or leave from the top.
    fn back_provider_step(&mut self) {
        let Some(wizard) = self.provider_wizard.as_mut() else {
            return;
        };
        let action = wizard.back();
        if action == crate::tui::provider_wizard::WizardAction::Close {
            self.close_provider_wizard();
        } else {
            self.composer.clear();
        }
    }

    fn close_provider_wizard(&mut self) {
        self.cancel_provider_task();
        self.provider_wizard = None;
        self.composer.clear();
    }

    /// Cancel whatever background work the wizard started.
    fn cancel_provider_task(&mut self) {
        if let Some(cancel) = self.provider_cancel.take() {
            cancel.cancel();
        }
        if let Some(wizard) = self.provider_wizard.as_mut() {
            wizard.status = Some("cancelled".to_string());
            if wizard.step.is_busy() {
                wizard.step = crate::tui::provider_wizard::Step::Action;
                wizard.selected = 0;
            }
        }
    }

    /// Perform one transition's side effect.
    fn run_provider_action(&mut self, action: crate::tui::provider_wizard::WizardAction) {
        use crate::tui::provider_wizard::WizardAction;

        match action {
            WizardAction::None => {}
            WizardAction::Close => self.close_provider_wizard(),
            WizardAction::ProbeModels(instance) => self.spawn_probe(instance),
            WizardAction::BeginSignIn {
                instance,
                method,
                secret,
            } => self.spawn_sign_in(&instance, method, secret),
            WizardAction::Persist(instance) | WizardAction::InstallDefaults(instance) => {
                self.persist_provider(instance);
            }
        }
    }

    /// Probe a draft endpoint's model list in the background. A probe is
    /// advisory: it fills the confirm screen with a count and never blocks the
    /// write.
    fn spawn_probe(&mut self, instance: crate::core::config::ProviderInstance) {
        let pool = self.pool.clone();
        let tx = self.provider_tx.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let token = cancel.clone();
        self.provider_cancel = Some(cancel);
        tokio::spawn(async move {
            // Register the draft provisionally so the pool can answer the
            // models call, then put the previous instance back: the probe must
            // not leave a half-configured provider selectable.
            let previous = pool.registry().instance(&instance.name).cloned();
            if pool.install_instance(instance.clone()).is_err() {
                let _ = tx.send(ProviderTask::Probed(Err(
                    "the draft endpoint is not a valid provider configuration".to_string(),
                )));
                return;
            }
            let models = pool.refresh(&instance.name).await;
            match previous {
                Some(previous) => {
                    let _ = pool.install_instance(previous);
                }
                None => {
                    let _ = pool.remove_instance(&instance.name);
                }
            }
            if token.is_cancelled() {
                return;
            }
            let result = match models {
                Ok(_) => Ok(pool.models_of(&instance.name)),
                Err(error) => Err(error.to_string()),
            };
            let _ = tx.send(ProviderTask::Probed(result));
        });
    }

    /// Run a plugin's sign-in flow in the background and forward its notices.
    fn spawn_sign_in(&mut self, instance: &str, method: &'static str, secret: Option<String>) {
        let protocol = self.protocol_of(instance).unwrap_or_default();
        let Some(plugin) = self.plugins.auth_plugin_for_protocol(&protocol) else {
            if let Some(wizard) = self.provider_wizard.as_mut() {
                wizard.status = Some(format!("no plugin signs in to `{instance}`"));
                wizard.step = crate::tui::provider_wizard::Step::Action;
            }
            return;
        };

        let cancel = tokio_util::sync::CancellationToken::new();
        let token = cancel.clone();
        self.provider_cancel = Some(cancel);

        let tx = self.provider_tx.clone();
        let notify_tx = tx.clone();
        let notifier = cst_plugin_api::auth::SignInNotifier::new(move |notice| {
            let _ = notify_tx.send(ProviderTask::SignInNotice {
                message: notice.message,
                url: notice.url,
            });
        });
        let instance_name = instance.to_string();
        let method = method.to_string();

        let Some(future) =
            plugin.begin_sign_in(&instance_name, &method, secret, notifier, token.clone())
        else {
            if let Some(wizard) = self.provider_wizard.as_mut() {
                wizard.status =
                    Some(format!("`{instance_name}` does not support that sign-in method"));
                wizard.step = crate::tui::provider_wizard::Step::Action;
            }
            return;
        };

        tokio::spawn(async move {
            let result = future.await;
            if token.is_cancelled() {
                return;
            }
            let _ = tx.send(ProviderTask::SignedIn(match result {
                Ok(outcome) => Ok(outcome.account),
                Err(error) => Err(error.to_string()),
            }));
        });
    }

    /// The protocol an instance (or a plugin's default route) uses.
    fn protocol_of(&self, instance: &str) -> Option<String> {
        if let Some(configured) = self.pool.registry().instance(instance) {
            return Some(configured.protocol_id().to_string());
        }
        self.plugins
            .auth_providers()
            .into_iter()
            .find(|(_, provider)| provider.id == instance)
            .map(|(_, provider)| provider.protocol.to_string())
    }

    /// Write one instance to `providers.toml` and make it live.
    ///
    /// The file write happens first: if it fails nothing else changes, so the
    /// in-memory pool never disagrees with what a restart would load. The file
    /// is rewritten from the pool's current instances plus the new one, which
    /// is why an instance the wizard did not touch keeps its values.
    fn persist_provider(&mut self, instance: crate::core::config::ProviderInstance) {
        let name = instance.name.clone();
        let mut instances: Vec<crate::core::config::ProviderInstance> = self
            .pool
            .registry()
            .instances()
            .iter()
            .filter(|existing| existing.name != name)
            .cloned()
            .collect();
        instances.push(instance.clone());

        if let Err(error) = crate::core::config::save_providers(
            &crate::core::config::providers_path(),
            &instances,
        ) {
            if let Some(wizard) = self.provider_wizard.as_mut() {
                wizard.status = Some(format!("cannot write providers.toml: {error}"));
            }
            return;
        }

        if let Err(error) = self.pool.install_instance(instance) {
            if let Some(wizard) = self.provider_wizard.as_mut() {
                wizard.status =
                    Some(format!("provider `{name}` was written but is unusable: {error}"));
            }
            return;
        }

        self.provider_wizard = None;
        self.composer.clear();
        self.refresh_request_facts();
        self.view_mut()
            .history
            .push(HistoryCell::Notice(format!("added provider `{name}`")));
        self.spawn_discovery(&name);
    }

    /// Apply a background result to the open wizard, or ignore it when the
    /// wizard was closed.
    fn handle_provider_task(&mut self, task: ProviderTask) {
        let Some(wizard) = self.provider_wizard.as_mut() else {
            return;
        };
        let action = match task {
            ProviderTask::Probed(result) => {
                wizard.probe_finished(result);
                None
            }
            ProviderTask::SignInNotice { message, url } => {
                wizard.sign_in_notice(message, url);
                None
            }
            ProviderTask::SignedIn(result) => Some(wizard.sign_in_finished(result)),
        };
        self.provider_cancel = None;
        if let Some(action) = action {
            self.run_provider_action(action);
        }
    }

    /// The effort levels the active model accepts: its declared `reasoning`
    /// list when present, else the shared defaults — with the model's own
    /// `default_level` always selectable even when the list omits it. Both
    /// `/effort` forms (argument validation and the picker) use this list.
    fn effort_levels(&self) -> Vec<String> {
        let info = self
            .route
            .as_ref()
            .and_then(|route| self.pool.model_info(route));
        let mut levels = match info.as_ref().and_then(|model| model.reasoning.as_deref()) {
            Some(levels) if !levels.is_empty() => levels.to_vec(),
            _ => reasoning::DEFAULT_EFFORT_LEVELS
                .iter()
                .map(|level| (*level).to_string())
                .collect(),
        };
        if let Some(default) = info.as_ref().and_then(|model| model.default_level.as_deref())
            && !levels.iter().any(|level| level.eq_ignore_ascii_case(default))
        {
            levels.push(default.to_string());
        }
        levels
    }

    /// The active model's declared context window (providers.toml or the
    /// models API), if known.
    fn context_window(&self) -> Option<u64> {
        self.route
            .as_ref()
            .and_then(|route| self.pool.model_info(route))
            .and_then(|model| model.context_window)
    }

    /// Tokens the next request will roughly carry: the last measured turn's
    /// input (the transcript as sent) plus its output. `None` before the
    /// first measured turn of this run.
    fn context_tokens(&self) -> Option<u64> {
        self.view()
            .last_usage
            .map(|(input, output)| input.saturating_add(output))
    }

    /// The footer's context readout, always present so the segment never
    /// appears and disappears as metadata arrives: `13.0k/200.0k (6%)` when
    /// both are known, `—/200.0k` with only a window, `13.0k tok` with only
    /// measured usage, and `—` when neither is known yet. The boolean flags a
    /// nearly full window (≥90%) for a warning color.
    fn context_segment(&self) -> (String, bool) {
        match (self.context_tokens(), self.context_window()) {
            (Some(used), window) => {
                let warn = window.is_some_and(|window| context_percent(used, window) >= 90);
                (format_context_usage(used, window), warn)
            }
            (None, Some(window)) => (format!("—/{}", format_tokens(window)), false),
            (None, None) => ("—".to_string(), false),
        }
    }

    /// `/effort <level|off>`: validate the argument against the active
    /// model's levels and apply it. A failed validation leaves the current
    /// effort untouched.
    fn set_effort(&mut self, args: &str) {
        let value = args.trim();
        if let Err(error) = reasoning::validate_effort(value, Some(&self.effort_levels())) {
            self.view_mut().history.push(HistoryCell::Error(error));
            return;
        }
        let effort = reasoning::normalize_effort(value);
        self.apply_effort(effort);
    }

    /// Apply an effort change: update the label optimistically, tell the
    /// core, and record a notice. The core's `ReasoningEffortSwitched` event
    /// is the authoritative confirmation (mirroring `/model`).
    fn apply_effort(&mut self, effort: Option<String>) {
        let view = self.view_mut();
        view.history.push(HistoryCell::Notice(match &effort {
            Some(level) => format!("reasoning effort set to {level}"),
            None => "reasoning effort unset".to_string(),
        }));
        self.effort = effort.clone();
        self.session.set_reasoning_effort(effort);
    }

    /// `/effort` with no argument: open the picker over the levels the active
    /// model accepts (the defaults when it declares none), plus an `off`
    /// entry that clears the override. Selection starts on the active level.
    fn open_effort_menu(&mut self) {
        let mut items: Vec<EffortMenuItem> = self
            .effort_levels()
            .into_iter()
            .map(|level| EffortMenuItem {
                current: self.effort.as_deref() == Some(level.as_str()),
                level: Some(level),
            })
            .collect();
        items.push(EffortMenuItem {
            level: None,
            current: self.effort.is_none(),
        });
        let selected = items
            .iter()
            .position(|item| item.current)
            .unwrap_or(items.len() - 1);
        self.command_menu_dismissed = true;
        self.effort_menu = Some(EffortMenu { items, selected });
    }

    fn move_effort_selection(&mut self, delta: isize) {
        let Some(menu) = self.effort_menu.as_mut() else {
            return;
        };
        let len = menu.items.len();
        menu.selected = if delta < 0 {
            (menu.selected + len - 1) % len
        } else {
            (menu.selected + 1) % len
        };
    }

    /// `/profile` with no argument: list the available profiles, marking the
    /// active one with each one's presets.
    fn list_profiles(&mut self) {
        if self.profiles.is_empty() {
            self.view_mut().history.push(HistoryCell::Notice(format!(
                "no profiles in {}",
                crate::core::config::profiles_path().display()
            )));
            return;
        }
        let lines = self
            .profiles
            .iter()
            .map(|(name, profile)| {
                let marker = if self.active_profile.as_deref() == Some(name.as_str()) {
                    "* "
                } else {
                    "  "
                };
                let mut line = format!("{marker}{name}");
                if let Some(selection) = profile.default_selection() {
                    line.push_str(&format!(" — {selection}"));
                }
                if let Some(effort) = &profile.effort {
                    line.push_str(&format!(", effort {effort}"));
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.view_mut().history.push(HistoryCell::Notice(lines));
    }

    /// `/profile <name>`: apply the profile's presets (declared selection and
    /// effort) at the next turn boundary. Ops are FIFO, so a switch never
    /// lands mid-turn; the worker confirms with `ProfileSwitched`, and an
    /// unresolvable selection arrives as an error cell. Assembly-time values
    /// (tools, system prompt) are startup-only and untouched.
    fn request_profile_switch(&mut self, args: &str) {
        let name = args.trim();
        let Some(_) = self.profiles.get(name) else {
            let available = if self.profiles.is_empty() {
                "none".to_string()
            } else {
                self.profiles
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            self.view_mut().history.push(HistoryCell::Error(format!(
                "no profile `{name}` is configured (available: {available})"
            )));
            return;
        };
        if self.active_profile.as_deref() == Some(name) {
            return;
        }
        self.active_profile = Some(name.to_string());
        let view = self.view_mut();
        view.history
            .push(HistoryCell::Notice(format!("switched to profile `{name}`")));
        self.session.switch_profile(name);
    }

    /// Confirm the picker: picking the already-active level just closes the
    /// menu.
    fn confirm_effort_selection(&mut self) {
        let Some(menu) = self.effort_menu.take() else {
            return;
        };
        let Some(item) = menu.items.get(menu.selected) else {
            return;
        };
        if item.current {
            return;
        }
        self.apply_effort(item.level.clone());
    }

    /// `/tools` with no argument: open the checkbox picker over the declared
    /// tools, checked for the ones currently enabled. Space toggles; Enter
    /// applies the whole set as one op.
    fn open_tools_menu(&mut self) {
        if self.tools.is_empty() {
            self.view_mut().history.push(HistoryCell::Notice(
                "no tools are declared (no loaded plugin contributes any)".to_string(),
            ));
            return;
        }
        let items = self
            .tools
            .iter()
            .map(|spec| ToolsMenuItem {
                name: spec.name.to_string(),
                read_only: spec.read_only,
                enabled: self.enabled_tools.contains(spec.name),
            })
            .collect();
        self.command_menu_dismissed = true;
        self.tools_menu = Some(ToolsMenu { items, selected: 0 });
    }

    /// `/context`: the classic prompt report while the conversation is
    /// empty, the interactive viewer once messages exist. The gate reads the
    /// view's transcript cells (an approximation of the worker's context —
    /// close enough to pick a face, and the viewer's own snapshot is the
    /// authority once open).
    fn open_context_view(&mut self) {
        let has_transcript = self.view().history.iter().any(|cell| {
            matches!(
                cell,
                HistoryCell::User(_) | HistoryCell::Assistant(_) | HistoryCell::ToolCall { .. }
            )
        });
        if !has_transcript {
            let block = crate::tui::context_view::build_context_report(&self.plugin_info());
            // A report is read from its top, like every plugin report.
            let view = self.view_mut();
            view.history.push(HistoryCell::Report(block));
            view.park_on_last_report = true;
            return;
        }
        self.command_menu_dismissed = true;
        self.context_view = Some(crate::tui::context_view::ContextView::new());
        // The snapshot comes back as a `ConversationSnapshot` event and the
        // viewer renders "loading…" until then.
        self.session.inspect_conversation();
    }

    /// Carry out one viewer action: every mutation is an op away — the
    /// worker is the only owner of the transcript.
    fn apply_context_action(&mut self, action: crate::tui::context_view::ContextAction) {
        use crate::tui::context_view::ContextAction as Action;
        match action {
            Action::Close => self.context_view = None,
            Action::Delete { indices } => {
                self.session.delete_messages(indices);
            }
            Action::Edit { index, content } => {
                self.session.edit_message(index, content);
            }
            Action::Undo => {
                self.session.undo_context_edit();
            }
        }
    }

    fn move_tools_selection(&mut self, delta: isize) {
        let Some(menu) = self.tools_menu.as_mut() else {
            return;
        };
        let len = menu.items.len();
        menu.selected = if delta < 0 {
            (menu.selected + len - 1) % len
        } else {
            (menu.selected + 1) % len
        };
    }

    /// Flip the highlighted checkbox. Nothing is sent until Enter applies
    /// the set, so Esc discards the fiddling for free.
    fn toggle_tools_entry(&mut self) {
        if let Some(menu) = self.tools_menu.as_mut()
            && let Some(item) = menu.items.get_mut(menu.selected)
        {
            item.enabled = !item.enabled;
        }
    }

    /// Apply the picker: the checked names become the session's enabled set.
    fn confirm_tools_selection(&mut self) {
        let Some(menu) = self.tools_menu.take() else {
            return;
        };
        let enabled: Vec<String> = menu
            .items
            .into_iter()
            .filter(|item| item.enabled)
            .map(|item| item.name)
            .collect();
        self.apply_tools(enabled);
    }

    /// `/tools <name> [on|off]`: flip one tool directly, or pin it with the
    /// optional direction argument. Unknown tools and directions error.
    fn set_tool_enabled(&mut self, args: &str) {
        let mut parts = args.split_whitespace();
        let Some(name) = parts.next() else {
            return;
        };
        let Some(spec) = self.tools.iter().find(|spec| spec.name == name) else {
            let available = if self.tools.is_empty() {
                "none".to_string()
            } else {
                self.tools
                    .iter()
                    .map(|spec| spec.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            self.view_mut().history.push(HistoryCell::Error(format!(
                "no tool `{name}` is declared (available: {available})"
            )));
            return;
        };
        let direction = parts.next();
        let enabled = match direction {
            None => !self.enabled_tools.contains(spec.name),
            Some("on") | Some("enable") => true,
            Some("off") | Some("disable") => false,
            Some(other) => {
                self.view_mut().history.push(HistoryCell::Error(format!(
                    "expected `on` or `off`, got `{other}` (try /tools {name} off)"
                )));
                return;
            }
        };
        let mut set = self.enabled_tools.clone();
        if enabled {
            if !set.insert(spec.name.to_string()) {
                return; // already enabled; nothing to apply
            }
        } else if !set.remove(spec.name) {
            return; // already disabled
        }
        self.apply_tools(set.into_iter().collect());
    }

    /// Apply an enabled-set change: update the mirror optimistically, tell
    /// the core, and record a notice. The core's `ToolsSwitched` event is
    /// the authoritative confirmation (mirroring `/effort`).
    fn apply_tools(&mut self, enabled: Vec<String>) {
        let notice = if enabled.is_empty() {
            "all tools disabled".to_string()
        } else {
            format!("tools enabled: {}", enabled.join(", "))
        };
        let view = self.view_mut();
        view.history.push(HistoryCell::Notice(notice));
        self.set_enabled_tools(enabled.clone());
        self.session.set_tools_enabled(enabled);
    }

    /// Store the enabled set and its flattened mirror together, so
    /// [`PluginInfo`] can never observe a stale slice.
    fn set_enabled_tools(&mut self, enabled: impl IntoIterator<Item = String>) {
        self.enabled_tools = enabled.into_iter().collect();
        self.enabled_tools_vec = self.enabled_tools.iter().cloned().collect();
    }

    /// `/thinking`: fold or unfold the model's thinking cards. The command
    /// spelling of Ctrl+T, kept for discoverability and for hosts without the
    /// key. A global display flag, so committed Thinking cells in every
    /// session re-render with it on the next frame. Tool cards are untouched:
    /// they fold on their own switch (Ctrl+O).
    fn toggle_thinking(&mut self) {
        self.show_thinking = !self.show_thinking;
        let state = if self.show_thinking {
            "expanded"
        } else {
            "folded"
        };
        let view = self.view_mut();
        view.history
            .push(HistoryCell::Notice(format!("thinking cards {state}")));
    }

    /// Commit streamed thinking as a history cell, keeping it above the
    /// assistant message it precedes. Called when visible content starts and
    /// when the turn finalizes; blank thinking produces no cell. The stream's
    /// wall-clock time rides along for the card's duration chip.
    fn commit_thinking(&mut self) {
        let started = self.thinking_started.take();
        if let Some(text) = self.view_mut().live_thinking.take()
            && !text.trim().is_empty()
        {
            let duration_ms = started.map(|at| at.elapsed().as_millis() as u64);
            self.view_mut()
                .history
                .push(HistoryCell::Thinking { text, duration_ms });
        }
    }

    /// Start a user turn. Hooks run before the turn is queued so they cannot
    /// race the model request; `busy` is already true, so nested submissions
    /// are ignored.
    fn submit_user_turn(&mut self, text: String) {
        let view = self.view_mut();
        view.history.push(HistoryCell::User(text.clone()));
        view.live = Some(String::new());
        view.follow = true;
        self.busy = true;

        let output = self.plugins.user_message(&text, self.plugin_info());
        self.apply_output(output);

        if !self.quit {
            self.session.submit_user_turn(text);
        }
    }

    fn plugin_info(&self) -> PluginInfo<'_> {
        let view = self.view();
        PluginInfo {
            model: &self.model_label,
            mode: self.mode.label(),
            transcript_len: view.history.len(),
            context_window: self.context_window(),
            last_usage: view.last_usage,
            total_input: view.total_input,
            total_output: view.total_output,
            plugin_ids: self.plugins.plugin_ids(),
            system_sections: &self.system_sections,
            effort: self.effort.as_deref(),
            request: crate::plugins::RequestFacts {
                protocol: &self.request_facts.protocol,
                base_url: &self.request_facts.base_url,
                endpoint: &self.request_facts.endpoint,
                overrides: &self.request_facts.overrides,
            },
            tools: &self.tools,
            enabled_tools: self.enabled_tools_vec.as_slice(),
            message_count: 1
                + view
                    .history
                    .iter()
                    .filter(|cell| {
                        matches!(
                            cell,
                            HistoryCell::User(_)
                                | HistoryCell::Assistant(_)
                                | HistoryCell::ToolCall { .. }
                        )
                    })
                    .count(),
            plugin_configs: &self.plugin_settings,
        }
    }

    /// Apply plugin effects in order. `SubmitUserTurn` is honored only while
    /// the agent is idle, which turns recursive submissions into a no-op.
    fn apply_effects(&mut self, effects: Vec<PluginEffect>) {
        for effect in effects {
            match effect {
                PluginEffect::Notice(text) => {
                    self.view_mut().history.push(HistoryCell::Notice(text));
                }
                PluginEffect::Error(text) => {
                    self.view_mut().history.push(HistoryCell::Error(text));
                }
                PluginEffect::Report(block) => {
                    // A report (the `/context` dump, typically) is read from
                    // its top. Mark it so the next render parks the viewport
                    // on its first line: the row count is only known once
                    // the cell renders at the current width, so it cannot be
                    // computed here.
                    self.view_mut().history.push(HistoryCell::Report(block));
                    self.view_mut().park_on_last_report = true;
                }
                PluginEffect::ClearConversation => {
                    self.clear_transcript();
                    self.session.clear_conversation();
                }
                PluginEffect::Quit => self.quit = true,
                PluginEffect::SetComposer(text) => {
                    self.composer.set_text(text);
                    self.command_selection = 0;
                    self.command_menu_dismissed = false;
                }
                PluginEffect::SubmitUserTurn(text) => {
                    if !self.busy && !text.trim().is_empty() && self.route.is_some() {
                        self.submit_user_turn(text);
                    }
                }
                PluginEffect::ContinueTurn => {
                    // `/continue`: drive the active transcript without a new
                    // user message — the recovery path for an interrupted
                    // turn. Like `SubmitUserTurn`, honored only while the
                    // agent is idle; refused with the same guidance a message
                    // gets when no usable route exists. Nothing is appended:
                    // the model picks up where the transcript stands.
                    if self.busy {
                        continue;
                    }
                    if self.route.is_none() {
                        let guidance = if self.pool.registry().instances().is_empty() {
                            crate::core::providers::NO_MODEL_GUIDANCE.to_string()
                        } else {
                            crate::core::providers::NO_MODEL_SELECTED.to_string()
                        };
                        self.view_mut().history.push(HistoryCell::Error(guidance));
                        continue;
                    }
                    if let Some(prompt) = self.route_guidance() {
                        self.view_mut().history.push(HistoryCell::Error(prompt));
                        continue;
                    }
                    let view = self.view_mut();
                    view.live = Some(String::new());
                    view.follow = true;
                    self.busy = true;
                    self.session.continue_turn();
                }
                PluginEffect::SetStatus(segment) => {
                    // Upsert by full `plugin:key`: a re-set segment replaces
                    // its text and tone in place, so ordering by registration
                    // is stable across updates.
                    match self
                        .status_segments
                        .iter_mut()
                        .find(|(key, _)| *key == segment.key)
                    {
                        Some(entry) => entry.1 = segment,
                        None => self.status_segments.push((segment.key.clone(), segment)),
                    }
                }
                PluginEffect::ClearStatus(key) => {
                    self.status_segments.retain(|(existing, _)| existing != &key);
                }
            }
        }
    }

    /// Apply one dispatch's output: start its background tasks, then apply
    /// its effects. Tasks spawn here — the host is synchronous by contract —
    /// and a task that panics surfaces as a transcript notice instead of
    /// taking the UI loop down.
    fn apply_output(&mut self, output: PluginOutput) {
        for task in output.tasks {
            self.spawn_plugin_task(task);
        }
        self.apply_effects(output.effects);
    }

    /// Start one plugin background task. The task's own cancellation is the
    /// host's token (fired on exit); this wrapper turns an unexpected panic
    /// into a transcript notice instead of a silently-dead task.
    fn spawn_plugin_task(&mut self, task: PluginTask) {
        let notices = self.notice_tx.clone();
        let task = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(task));
        let handle = tokio::spawn(async move {
            match task.await {
                Ok(()) => {}
                Err(panic) => {
                    let detail = panic
                        .downcast_ref::<&str>()
                        .map(|message| (*message).to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "panic".to_string());
                    let _ = notices.send(format!("plugin task failed: {detail}"));
                }
            }
        });
        self.plugin_tasks.push(handle);
    }

    fn reset_input_navigation(&mut self) {
        self.prompt_index = None;
        self.draft.clear();
        self.last_ctrl_c = None;
    }

    /// Reset the active view's transcript and the inline-mode printing
    /// bookkeeping. `/clear` intentionally keeps the composer's input history
    /// and only affects the current session.
    fn clear_transcript(&mut self) {
        let view = self.view_mut();
        view.history.clear();
        view.history
            .push(HistoryCell::Notice("conversation cleared".to_string()));
        view.live = None;
        view.live_thinking = None;
        view.inline_final_text = None;
        view.live_committed = 0;
        view.printed_cells = 0;
        view.follow = true;
        view.scroll = 0;
        view.max_scroll = 0;
        // The dropped turns' usage no longer describes the context; the
        // replayed rollout resets the same counters on `/clear`.
        view.last_usage = None;
        view.total_input = 0;
        view.total_output = 0;
    }

    fn handle_agent_event(&mut self, event: crate::core::Event) {
        // Subscribed plugins observe the event as it happened, before the
        // view state moves on: the payload is the event itself, and
        // `event.at` is the worker's emit stamp, so a meter sees the same
        // ordering the transcript does. Effects and spawned tasks apply like
        // any other dispatch.
        let output = self.plugins.dispatch_event(&event, self.plugin_info());
        self.apply_output(output);
        match event.msg {
            EventMsg::TurnStarted => {
                self.busy = true;
                if self.view().live.is_none() {
                    self.view_mut().live = Some(String::new());
                }
            }
            EventMsg::AgentMessageDelta { delta } => {
                // Thinking that streamed before the first visible delta
                // commits as a history cell now, so it renders above the
                // streaming answer.
                self.commit_thinking();
                self.view_mut()
                    .live
                    .get_or_insert_with(String::new)
                    .push_str(&delta);
            }
            EventMsg::AgentThinkingDelta { delta } => {
                // The duration chip is measured from the first visible delta.
                if self.view().live_thinking.is_none() {
                    self.thinking_started = Some(Instant::now());
                }
                let view = self.view_mut();
                view.live_thinking
                    .get_or_insert_with(String::new)
                    .push_str(&delta);
            }
            EventMsg::ContextUsage { input, output } => {
                // Display-only liveness: the footer follows the turn as it
                // streams. The session totals and the rollout record are the
                // completed-turn `TurnUsage`'s business.
                self.view_mut().last_usage = Some((input, output));
            }
            EventMsg::TurnUsage { input, output } => {
                // Record per session view: the last turn's usage doubles as
                // the current context size, the sums accumulate for /status.
                let view = self.view_mut();
                view.last_usage = Some((input, output));
                view.total_input = view.total_input.saturating_add(input);
                view.total_output = view.total_output.saturating_add(output);
            }
            EventMsg::ModelSwitched { model } => {
                // `/model` already updated the label optimistically; this is
                // the core's authoritative confirmation (also covering
                // non-TUI front-ends).
                self.route = Selection::parse(&model).ok();
                self.model_label = model;
                self.refresh_request_facts();
            }
            EventMsg::ReasoningEffortSwitched { effort } => {
                // `/effort` already updated optimistically; the core confirms
                // (also covering non-TUI front-ends).
                self.effort = effort;
            }
            EventMsg::ProfileSwitched { name } => {
                // `/profile` already updated optimistically; the core
                // confirms (also covering non-TUI front-ends). The preset's
                // selection and effort changes ride along as their own
                // events, handled above.
                self.active_profile = Some(name);
            }
            EventMsg::ToolsSwitched { enabled } => {
                // `/tools` already updated optimistically; the core confirms
                // with the validated set. Session switches and creations
                // send the same event with the session's own set, so the
                // mirror resyncs here too.
                self.set_enabled_tools(enabled);
            }
            EventMsg::AgentMessage { message } => {
                self.view_mut().live = Some(message);
            }
            EventMsg::AgentToolCallStarted { name, arguments } => {
                // Any text streamed before the calls is complete by now:
                // commit it so the running card renders below it. The card
                // lives in the transcript tail (spinner + elapsed) until the
                // completion event settles it into history.
                self.finalize_live();
                self.view_mut().running_tool = Some(crate::tui::session::RunningTool {
                    name,
                    arguments,
                    started_at: Instant::now(),
                });
            }
            EventMsg::AgentToolCall {
                name,
                arguments,
                result,
                error,
                duration_ms,
            } => {
                self.finalize_live();
                self.view_mut().running_tool = None;
                self.view_mut().history.push(HistoryCell::ToolCall {
                    name,
                    arguments,
                    result,
                    error,
                    duration_ms: Some(duration_ms),
                });
            }
            EventMsg::TurnAborted { .. } => {
                self.settle_running_tool("interrupted");
                self.finalize_live();
                self.view_mut().history.push(HistoryCell::Interrupted);
            }
            EventMsg::Error { message } => {
                // A rejected context op reports inside the viewer, where the
                // action that failed is still on screen.
                if let Some(view) = self.context_view.as_mut() {
                    view.set_error(message);
                    return;
                }
                self.settle_running_tool("interrupted");
                self.finalize_live();
                self.view_mut().history.push(HistoryCell::Error(message));
            }
            EventMsg::TurnComplete => {
                let message = self.view().live.clone().unwrap_or_default();
                self.finalize_live();
                let output = self.plugins.turn_complete(&message, self.plugin_info());
                self.busy = false;
                self.last_ctrl_c = None;
                self.apply_output(output);
            }
            EventMsg::ConversationCleared => {
                // The TUI already reset its transcript synchronously when
                // `/clear` ran; this confirms the core also dropped the
                // context and lets plugins observe the reset.
                self.context_view = None;
                let output = self.plugins.conversation_cleared(self.plugin_info());
                self.apply_output(output);
            }
            EventMsg::SessionStarted { .. } | EventMsg::SessionSwitched { .. } => {
                // `/new` and the session picker already applied their view
                // changes synchronously; these events confirm the core (and
                // the rollout file) did the same. A viewer open across the
                // switch would show the wrong session's context.
                self.context_view = None;
                // The event subscribers just saw a session boundary: re-send
                // the snapshot so monitors and metrics follow the new
                // conversation without replaying anything.
                self.session.inspect_conversation();
            }
            EventMsg::SessionRenamed { .. } => {
                // `/rename` already applied its view change synchronously;
                // this event confirms the core (and the rollout) did too.
            }
            EventMsg::ConversationSnapshot { messages } => {
                // The `/context` viewer's read path: the worker's own
                // transcript, whether from `InspectConversation` or as the
                // confirmation of a delete / edit / undo.
                if let Some(view) = self.context_view.as_mut() {
                    view.apply_snapshot(messages);
                }
            }
            EventMsg::ShutdownComplete => {
                self.quit = true;
            }
        }
    }

    /// The plugin segments to render, in display priority: `[status_bar]
    /// order` first, then everything else in registration order. Empty when
    /// `plugin_segments = false`.
    fn ordered_status_segments(&self) -> Vec<&crate::plugins::StatusSegment> {
        if !self.status_bar.plugin_segments {
            return Vec::new();
        }
        let mut remaining: Vec<&crate::plugins::StatusSegment> = self
            .status_segments
            .iter()
            .map(|(_, segment)| segment)
            .collect();
        let mut ordered = Vec::with_capacity(remaining.len());
        for key in &self.status_bar.order {
            if let Some(position) = remaining.iter().position(|segment| &segment.key == key) {
                ordered.push(remaining.remove(position));
            }
        }
        ordered.extend(remaining);
        ordered
    }

    /// Turn a still-running tool card into a settled error card so an
    /// interrupted turn keeps the record of what was executing. A no-op when
    /// no card is live.
    fn settle_running_tool(&mut self, reason: &str) {
        if let Some(running) = self.view_mut().running_tool.take() {
            let duration_ms = Some(running.started_at.elapsed().as_millis() as u64);
            self.view_mut().history.push(HistoryCell::ToolCall {
                name: running.name,
                arguments: running.arguments,
                result: String::new(),
                error: Some(reason.to_string()),
                duration_ms,
            });
        }
    }

    fn finalize_live(&mut self) {
        // Any thinking that never saw visible content (interrupted or failed
        // turn) still lands as its own cell, above the assistant text.
        self.commit_thinking();
        let Some(text) = self.view_mut().live.take() else {
            return;
        };
        if text.trim().is_empty() {
            return;
        }
        match self.mode {
            TuiMode::Fullscreen => self.view_mut().history.push(HistoryCell::Assistant(text)),
            // Inline mode already printed the stable lines. Queue the remainder
            // so it lands in scrollback before any following error/notice cell.
            TuiMode::Inline => self.view_mut().inline_final_text = Some(text),
        }
    }

    fn on_up(&mut self) {
        if self.move_command_selection(-1) {
            return;
        }
        if !self.composer.move_up() && self.composer.is_single_line() {
            self.history_prev();
        }
    }

    fn on_down(&mut self) {
        if self.move_command_selection(1) {
            return;
        }
        if self.prompt_index.is_some() {
            if !self.composer.move_down() {
                self.history_next();
            }
        } else {
            self.composer.move_down();
        }
    }

    fn history_prev(&mut self) {
        if self.prompts.is_empty() {
            return;
        }
        match self.prompt_index {
            None => {
                self.draft = self.composer.text();
                self.prompt_index = Some(self.prompts.len() - 1);
            }
            Some(0) => {}
            Some(index) => self.prompt_index = Some(index - 1),
        }
        if let Some(index) = self.prompt_index {
            self.composer.set_text(self.prompts[index].clone());
        }
    }

    fn history_next(&mut self) {
        let Some(index) = self.prompt_index else {
            return;
        };
        if index + 1 < self.prompts.len() {
            self.prompt_index = Some(index + 1);
            self.composer.set_text(self.prompts[index + 1].clone());
        } else {
            self.prompt_index = None;
            let draft = std::mem::take(&mut self.draft);
            self.composer.set_text(draft);
        }
    }

    fn scroll_up(&mut self, amount: usize) {
        let view = self.view_mut();
        view.follow = false;
        view.scroll = view.scroll.saturating_sub(amount).min(view.max_scroll);
    }

    fn scroll_down(&mut self, amount: usize) {
        let view = self.view_mut();
        view.scroll = (view.scroll + amount).min(view.max_scroll);
        if view.scroll >= view.max_scroll {
            view.follow = true;
        }
    }

    fn show_ctrl_c_hint(&self) -> bool {
        !self.busy
            && self.composer.is_empty()
            && self
                .last_ctrl_c
                .is_some_and(|previous| previous.elapsed() < Duration::from_secs(2))
    }

    fn render_fullscreen(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width == 0 || area.height == 0 {
            return;
        }

        // The `/context` viewer takes the whole frame while open.
        if let Some(view) = self.context_view.as_ref() {
            view.render(frame, area);
            return;
        }
        let width = area.width as usize;

        let composer_width = width.saturating_sub(2).max(1);
        let composer_rows = self.composer.visual_rows(composer_width);
        let desired_rows = composer_rows.len().clamp(1, MAX_COMPOSER_ROWS);
        let max_composer_height = area.height.saturating_sub(2).max(1);
        let composer_height = (desired_rows as u16 + 1).min(max_composer_height);
        let visible_composer_rows = composer_height.saturating_sub(1) as usize;

        let menu_height =
            (self.menu_row_count() as u16).min(area.height.saturating_sub(composer_height + 2));

        let chunks = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(menu_height),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(area);

        let menu = self.menu_lines(menu_height as usize);
        self.render_chat(frame, chunks[0], width);
        self.render_menu(frame, chunks[1], &menu);
        self.render_composer(
            frame,
            chunks[2],
            composer_width,
            visible_composer_rows,
            &composer_rows,
        );
        self.render_footer(frame, chunks[3]);
    }

    fn render_inline_pane(
        &self,
        frame: &mut Frame,
        live_tail: &[Line<'static>],
        menu: &[Line<'static>],
        menu_height: u16,
        composer_rows: &[String],
    ) {
        let area = frame.area();
        if area.width == 0 || area.height == 0 {
            return;
        }

        let composer_width = (area.width as usize).saturating_sub(2).max(1);
        let visible_composer_rows = composer_rows.len().clamp(1, MAX_COMPOSER_ROWS);
        let live_height = live_tail.len() as u16;
        let composer_height = visible_composer_rows as u16 + 1;
        let chunks = Layout::vertical([
            Constraint::Length(live_height),
            Constraint::Length(menu_height),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(area);

        if !live_tail.is_empty() {
            frame.render_widget(
                Paragraph::new(Text::from(live_tail.to_vec())),
                chunks[0],
            );
        }
        self.render_menu(frame, chunks[1], menu);
        self.render_composer(
            frame,
            chunks[2],
            composer_width,
            visible_composer_rows,
            composer_rows,
        );
        self.render_footer(frame, chunks[3]);
    }

    /// Rows the menu above the composer needs: a picker (`/sessions`,
    /// `/model`, `/effort`) when it is open, otherwise the slash-command
    /// candidates. Empty when neither is visible.
    fn menu_row_count(&self) -> usize {
        if let Some(wizard) = self.provider_wizard.as_ref() {
            // Pickers show their rows; a text step shows the prompt; a running
            // step shows the status line. Never more than the shared cap.
            return wizard_rows(wizard).min(MAX_COMMAND_MENU_ROWS);
        }
        if let Some(menu) = self.model_menu.as_ref() {
            menu.items.len().min(MAX_COMMAND_MENU_ROWS)
        } else if let Some(menu) = self.effort_menu.as_ref() {
            menu.items.len().min(MAX_COMMAND_MENU_ROWS)
        } else if let Some(menu) = self.tools_menu.as_ref() {
            menu.items.len().min(MAX_COMMAND_MENU_ROWS)
        } else if self.session_menu.is_some() {
            self.views.len().min(MAX_COMMAND_MENU_ROWS)
        } else {
            self.command_matches().len().min(MAX_COMMAND_MENU_ROWS)
        }
    }

    /// The menu lines, windowed around the selection so it stays visible
    /// within `height` rows.
    fn menu_lines(&self, height: usize) -> Vec<Line<'static>> {
        if let Some(wizard) = self.provider_wizard.as_ref() {
            return provider_menu_lines(wizard, &self.composer.text());
        }
        if let Some(menu) = self.model_menu.as_ref() {
            let window = command_menu_window(menu.items.len(), menu.selected, height);
            return window
                .map(|index| {
                    model_menu_line(
                        index + 1,
                        &menu.items[index],
                        index == menu.selected,
                    )
                })
                .collect();
        }
        if let Some(menu) = self.effort_menu.as_ref() {
            let window = command_menu_window(menu.items.len(), menu.selected, height);
            return window
                .map(|index| {
                    effort_menu_line(index + 1, &menu.items[index], index == menu.selected)
                })
                .collect();
        }
        if let Some(menu) = self.tools_menu.as_ref() {
            let window = command_menu_window(menu.items.len(), menu.selected, height);
            return window
                .map(|index| tools_menu_line(&menu.items[index], index == menu.selected))
                .collect();
        }
        if let Some(selected) = self.session_menu {
            let window = command_menu_window(self.views.len(), selected, height);
            return window
                .map(|index| {
                    session_menu_line(
                        index,
                        &self.views[index],
                        index == self.active_view,
                        index == selected,
                    )
                })
                .collect();
        }

        let matches = self.command_matches();
        if matches.is_empty() {
            return Vec::new();
        }
        let selected = self.command_selection.min(matches.len() - 1);
        let window = command_menu_window(matches.len(), selected, height);
        matches[window.clone()]
            .iter()
            .enumerate()
            .map(|(offset, candidate)| {
                command_menu_line(candidate, window.start + offset == selected)
            })
            .collect()
    }

    /// Draw the menu (session picker or command candidates) above the composer.
    fn render_menu(&self, frame: &mut Frame, area: Rect, lines: &[Line<'static>]) {
        if lines.is_empty() || area.width == 0 || area.height == 0 {
            return;
        }
        frame.render_widget(Paragraph::new(Text::from(lines.to_vec())), area);
    }

    fn render_chat(&mut self, frame: &mut Frame, area: Rect, width: usize) {
        if area.height == 0 {
            self.view_mut().max_scroll = 0;
            return;
        }

        let display = chat::CardDisplay {
            thinking: self.show_thinking,
            tool_detail: self.show_tool_detail,
        };
        let busy = self.busy;
        let spinner = chat::SPINNER[self.tick % chat::SPINNER.len()];
        let view = self.view_mut();

        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(""));

        // Where the last report cell starts, for the park-on-report scroll
        // below (the row count is only known here, at this width).
        let mut last_report_start = None;
        for (index, cell) in view.history.iter().enumerate() {
            if matches!(cell, HistoryCell::Report(_)) {
                last_report_start = Some(lines.len());
            }
            lines.extend(cell.lines(width, &display));
            let has_more = index + 1 < view.history.len() || view.live.is_some();
            if has_more {
                lines.push(Line::from(""));
            }
        }

        // The tool call currently executing, between the committed history
        // and whatever the next round streams.
        if let Some(running) = &view.running_tool {
            let elapsed = chat::format_duration(running.started_at.elapsed().as_millis() as u64);
            lines.extend(chat::running_tool_lines(
                &running.name,
                &running.arguments,
                &elapsed,
                spinner,
                width,
            ));
            lines.push(Line::from(""));
        }

        // The thinking block still streaming for this turn, above the
        // assistant text it precedes.
        if let Some(thinking) = &view.live_thinking
            && !thinking.is_empty()
        {
            lines.extend(chat::thinking_live_lines(
                thinking,
                width,
                display.thinking,
                spinner,
            ));
            lines.push(Line::from(""));
        }

        if let Some(live) = &view.live {
            lines.extend(chat::assistant_lines(live, width, busy));
        }

        let height = area.height as usize;
        view.max_scroll = lines.len().saturating_sub(height);

        if view.park_on_last_report {
            // A report was just appended: show its first line. Doing it here
            // (not in the effect handler) is what makes the row count and the
            // viewport height available.
            view.park_on_last_report = false;
            if let Some(start) = last_report_start {
                view.follow = false;
                view.scroll = start.min(view.max_scroll);
            }
        } else if view.follow {
            view.scroll = view.max_scroll;
        } else {
            view.scroll = view.scroll.min(view.max_scroll);
        }

        let start = view.scroll.min(lines.len());
        let end = (start + height).min(lines.len());
        let visible = lines[start..end].to_vec();

        frame.render_widget(Paragraph::new(Text::from(visible)), area);
    }

    fn render_composer(
        &self,
        frame: &mut Frame,
        area: Rect,
        composer_width: usize,
        visible_rows: usize,
        rows: &[String],
    ) {
        frame.render_widget(
            Block::new()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(theme::DIM)),
            area,
        );

        if area.height < 2 || area.width < 2 {
            return;
        }

        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(1),
        };

        // Prompt is drawn in its own two-column gutter so the wrapped text and
        // cursor share one origin.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "› ",
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ))),
            Rect {
                x: area.x,
                y: area.y + 1,
                width: area.width.min(2),
                height: 1,
            },
        );

        let (cursor_row, cursor_column) = self.composer.cursor_visual(composer_width);
        let offset = cursor_row
            .saturating_sub(visible_rows.saturating_sub(1))
            .min(rows.len().saturating_sub(visible_rows));
        let visible = rows
            .iter()
            .skip(offset)
            .take(visible_rows)
            .cloned()
            .collect::<Vec<_>>();

        if self.composer.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Ask anything…",
                    Style::default().fg(theme::DIM),
                ))),
                inner,
            );
        } else {
            let lines = visible
                .iter()
                .map(|row| Line::from(Span::raw(row.clone())))
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(Text::from(lines)), inner);
        }

        if inner.width > 0 && inner.height > 0 {
            let cursor_in_view = cursor_row.saturating_sub(offset) as u16;
            let column = u16::try_from(cursor_column).unwrap_or(u16::MAX);
            let x = inner
                .x
                .saturating_add(column)
                .min(inner.x + inner.width.saturating_sub(1));
            let y = inner
                .y
                .saturating_add(cursor_in_view)
                .min(inner.y + inner.height.saturating_sub(1));
            frame.set_cursor_position(Position::new(x, y));
        }
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let spinner = chat::SPINNER[self.tick % chat::SPINNER.len()];
        let state = if self.busy {
            format!("{spinner} working · Esc interrupt")
        } else {
            "ready".to_string()
        };
        // A flashing message takes the hint slot: it is the one line that is
        // always on screen, and it is transient, unlike a transcript notice.
        let flashing = self.status.as_ref().map(|(text, _)| text.as_str());
        let hints = match flashing {
            Some(text) => text,
            None => {
                if let Some(wizard) = self.provider_wizard.as_ref() {
                    wizard.hint()
                } else if self.session_menu.is_some() {
                    "↑↓ select · Enter switch · Esc close"
                } else if self.model_menu.is_some() {
                    "↑↓ select · Enter switch · r refresh · Esc close"
                } else if self.effort_menu.is_some() {
                    "↑↓ select · Enter switch · Esc close"
                } else if self.command_menu_open() {
                    "↑↓ select · Tab complete · Enter run · Esc close"
                } else if self.show_ctrl_c_hint() {
                    "Ctrl+C again to quit"
                } else if self.busy {
                    "streaming…"
                } else if self.mode == TuiMode::Fullscreen {
                    "Enter · / commands · Ctrl+J newline · Ctrl+T thinking · Ctrl+O tools · wheel scroll"
                } else {
                    "Enter · / commands · Ctrl+J newline · Ctrl+T thinking · Ctrl+O tools · PgUp/PgDn scroll"
                }
            }
        };

        let effort_label = self.effort.as_deref().unwrap_or(reasoning::EFFORT_OFF);
        let (context, context_warn) = self.context_segment();

        // Plugin segments sit between the context readout and the state
        // separator, in `[status_bar]` priority order. The existing priority
        // holds: the left side (label · state) wins on a narrow terminal,
        // and the right-hand hint is already the line that degrades — plugin
        // segments take what is left before the hint, then drop from their
        // own tail once they stop fitting.
        let base_plain = format!(
            " codest · {} · effort {effort_label} · {context}",
            self.model_label
        );
        let state_width = display_width(&format!("  │  {state}")) as u16 + 1;
        let mut segment_budget =
            area.width.saturating_sub(display_width(&base_plain) as u16 + state_width);
        let mut chosen: Vec<&crate::plugins::StatusSegment> = Vec::new();
        for segment in self.ordered_status_segments() {
            // " · " joins each plugin segment to the host's own readouts.
            let width = display_width(&segment.text) as u16 + 3;
            if width > segment_budget {
                break;
            }
            segment_budget -= width;
            chosen.push(segment);
        }
        let plugin_plain: String = chosen
            .iter()
            .map(|segment| format!(" · {}", segment.text))
            .collect();
        // The left side wins: on a narrow terminal the right
        // hint is what degrades, never the state word.
        let left_plain = format!("{base_plain}{plugin_plain}  │  {state}");
        let left_width = (display_width(&left_plain) as u16 + 1).min(area.width);
        let hint_width =
            (display_width(hints) as u16 + 1).min(area.width.saturating_sub(left_width));
        let parts =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(hint_width)]).split(area);

        let mut left = vec![
            Span::styled(
                " codest",
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {}", self.model_label),
                Style::default().fg(theme::DIM),
            ),
            Span::styled(
                format!(" · effort {effort_label}"),
                Style::default().fg(theme::DIM),
            ),
            // A nearly full window renders in the accent color as a warning.
            Span::styled(
                format!(" · {context}"),
                Style::default().fg(if context_warn {
                    theme::ACCENT
                } else {
                    theme::DIM
                }),
            ),
        ];
        for segment in &chosen {
            left.push(Span::styled(
                format!(" · {}", segment.text),
                status_tone_style(segment.tone),
            ));
        }
        left.push(Span::styled("  │  ", Style::default().fg(theme::DIM)));
        left.push(Span::styled(
            state,
            Style::default().fg(if self.busy { theme::ACCENT } else { theme::DIM }),
        ));
        frame.render_widget(Paragraph::new(Line::from(left)), parts[0]);
        // The flash reads as feedback, not as a keybinding hint, so it is not
        // dimmed like the hints it replaces.
        let hint_style = if flashing.is_some() {
            Style::default().fg(theme::ACCENT)
        } else {
            Style::default().fg(theme::DIM)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hints, hint_style)))
                .alignment(Alignment::Right),
            parts[1],
        );
    }
}

/// The slice of command candidates to render so the selected row stays
/// visible inside a window of `height` rows.
fn command_menu_window(len: usize, selected: usize, height: usize) -> std::ops::Range<usize> {
    if len == 0 || height == 0 {
        return 0..0;
    }
    let height = height.min(len);
    let selected = selected.min(len - 1);
    let start = selected.saturating_sub(height - 1).min(len - height);
    start..start + height
}

/// How many rows the wizard needs: one per picker row, plus a persistent
/// status line when one exists. Text steps keep a single prompt row so the
/// composer directly underneath reads as its input.
fn wizard_rows(wizard: &crate::tui::provider_wizard::Wizard) -> usize {
    use crate::tui::provider_wizard::Step;
    let rows = match wizard.step {
        Step::Action | Step::SignInPick | Step::SignInMethod => wizard.row_count(),
        Step::AddConfirm => 5,
        Step::Configured => wizard.row_count().max(1),
        _ => 1,
    };
    if wizard.status.is_some()
        && !matches!(
            wizard.step,
            Step::AddConfirm | Step::AddProbe | Step::SignInRunning
        )
    {
        rows + 1
    } else {
        rows
    }
}

/// Render the wizard as menu-strip lines above the composer — the same
/// non-overlay shape every other picker uses, so nothing has to be erased.
fn provider_menu_lines(
    wizard: &crate::tui::provider_wizard::Wizard,
    input: &str,
) -> Vec<Line<'static>> {
    use crate::tui::provider_wizard::Step;

    let mut lines = Vec::new();
    match wizard.step {
        Step::Action => {
            let actions = wizard.actions();
            for (index, action) in actions.iter().enumerate() {
                lines.push(provider_option_line(
                    index,
                    action_label(*action),
                    action_description(*action),
                    index == wizard.selected,
                ));
            }
        }
        Step::SignInPick => {
            if wizard.targets.is_empty() {
                lines.push(provider_note("no plugin offers a sign-in entry"));
            } else {
                for (index, target) in wizard.targets.iter().enumerate() {
                    let description = if target.configured {
                        target.provider.label.to_string()
                    } else {
                        format!("{} · creates the route", target.provider.label)
                    };
                    let mut line = provider_option_line(
                        index,
                        target.instance.clone(),
                        description,
                        index == wizard.selected,
                    );
                    let badge = target.status_label();
                    if !badge.is_empty() {
                        line.spans.push(Span::styled(
                            format!(" · {badge}"),
                            Style::default()
                                .fg(theme::DIM)
                                .add_modifier(Modifier::ITALIC),
                        ));
                    }
                    lines.push(line);
                }
            }
        }
        Step::SignInMethod => {
            let methods = wizard
                .chosen
                .as_ref()
                .map(|target| target.provider.methods)
                .unwrap_or_default();
            for (index, method) in methods.iter().enumerate() {
                lines.push(provider_option_line(
                    index,
                    method.label.to_string(),
                    if index == 0 {
                        "recommended".to_string()
                    } else {
                        String::new()
                    },
                    index == wizard.selected,
                ));
            }
        }
        Step::Configured => {
            if wizard.configured.is_empty() {
                lines.push(provider_note("no providers are configured yet"));
            } else {
                for (index, row) in wizard.configured.iter().enumerate() {
                    let mut line = provider_option_line(
                        index,
                        row.name.clone(),
                        format!("{} · {}", row.protocol, row.base_url),
                        false,
                    );
                    line.spans.push(Span::styled(
                        format!(" · {} model(s)", row.models),
                        Style::default().fg(theme::DIM),
                    ));
                    if let Some(badge) = row.status.as_ref().and_then(configured_badge) {
                        line.spans.push(Span::styled(
                            format!(" · {badge}"),
                            Style::default()
                                .fg(theme::DIM)
                                .add_modifier(Modifier::ITALIC),
                        ));
                    }
                    lines.push(line);
                }
            }
        }
        Step::AddConfirm => {
            let draft = &wizard.add;
            lines.push(provider_note(&format!(
                "write `{}` to {}",
                draft.name,
                crate::core::config::providers_path().display()
            )));
            lines.push(provider_note(&format!("endpoint {}", draft.base_url)));
            let key = if draft.api_key.trim().is_empty() {
                "no key"
            } else {
                "key set"
            };
            let models = match (draft.models.is_empty(), &draft.probed) {
                (true, Some(probed)) => format!("all {} discovered", probed.len()),
                (true, None) => "discover at startup".to_string(),
                (false, _) => format!("{} declared", draft.models.len()),
            };
            lines.push(provider_note(&format!("{key} · {models}")));
            if let Some(error) = &draft.probe_error {
                lines.push(Line::from(Span::styled(
                    format!("probe failed: {error}"),
                    Style::default().fg(theme::WARNING),
                )));
            }
            if let Some(status) = &wizard.status {
                lines.push(Line::from(Span::styled(
                    status.clone(),
                    Style::default().fg(theme::ERROR),
                )));
            }
        }
        _ => {
            // Text and busy steps: the prompt, plus whatever the wizard has to
            // say, with the typed value in the composer below.
            let field = wizard.field();
            let prompt = match field {
                Some(field) => field.prompt(wizard.chosen.as_ref().map(|target| &target.provider)),
                None => String::new(),
            };
            let mut spans = vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    prompt,
                    Style::default()
                        .fg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some(field) = field
                && field.is_secret()
            {
                // Echo length only: the secret must not appear on screen.
                let masked: String = std::iter::repeat_n('•', input.chars().count()).collect();
                spans.push(Span::styled(
                    format!("  {masked}"),
                    Style::default().fg(theme::DIM),
                ));
            }
            lines.push(Line::from(spans));
            if let Some(status) = &wizard.status {
                lines.push(Line::from(Span::styled(
                    format!("  {status}"),
                    Style::default().fg(theme::WARNING),
                )));
            }
        }
    }
    lines
}

fn configured_badge(status: &cst_plugin_api::auth::AuthStatus) -> Option<String> {
    use cst_plugin_api::auth::AuthStatus;
    match status {
        AuthStatus::Unsupported => None,
        AuthStatus::SignedOut => Some("not signed in".to_string()),
        AuthStatus::SignedIn { account } => Some(match account {
            Some(account) => format!("signed in ({account})"),
            None => "signed in".to_string(),
        }),
        AuthStatus::Expired => Some("sign-in expired".to_string()),
    }
}

fn action_label(action: crate::tui::provider_wizard::Action) -> String {
    use crate::tui::provider_wizard::Action;
    match action {
        Action::AddOpenAi => "Add an OpenAI-compatible provider".to_string(),
        Action::SignIn => "Sign in to a plugin provider".to_string(),
        Action::Configured => "Show configured providers".to_string(),
        Action::Close => "Close".to_string(),
    }
}

fn action_description(action: crate::tui::provider_wizard::Action) -> String {
    use crate::tui::provider_wizard::Action;
    match action {
        Action::AddOpenAi => "base URL + API key".to_string(),
        Action::SignIn => "credentials handled by the plugin".to_string(),
        Action::Configured => "read-only list".to_string(),
        Action::Close => String::new(),
    }
}

/// One wizard row in the same shape as the other pickers: marker, number,
/// label, description.
fn provider_option_line(
    index: usize,
    label: String,
    description: String,
    selected: bool,
) -> Line<'static> {
    let marker = if selected { "▸ " } else { "  " };
    let (marker_style, number_style, text_style) = if selected {
        (
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
            Style::default(),
        )
    } else {
        (
            Style::default().fg(theme::DIM),
            Style::default().fg(theme::DIM),
            Style::default().fg(theme::DIM),
        )
    };
    let mut spans = vec![
        Span::styled(marker, marker_style),
        Span::styled(format!("{} · ", index + 1), number_style),
        Span::styled(crate::tui::session::truncated(&label, 40), text_style),
    ];
    if !description.is_empty() {
        spans.push(Span::styled(
            format!("  {description}"),
            Style::default().fg(theme::DIM),
        ));
    }
    Line::from(spans)
}

fn provider_note(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(theme::DIM),
    ))
}

/// One command row: marker, aligned command name, summary, and aliases.
fn command_menu_line(candidate: &CommandCandidate, selected: bool) -> Line<'static> {
    let spec = candidate.spec;
    let marker = if selected { "▸ " } else { "  " };
    let (marker_style, name_style, text_style) = if selected {
        (
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
            Style::default(),
        )
    } else {
        (
            Style::default().fg(theme::DIM),
            Style::default().fg(theme::DIM),
            Style::default().fg(theme::DIM),
        )
    };

    let mut spans = vec![
        Span::styled(marker, marker_style),
        // Fixed wide name column (headroom for future longer commands) so the
        // summary always keeps its own column and a separating space.
        Span::styled(format!("/{:<24}", candidate.name), name_style),
        Span::styled(spec.summary, text_style),
    ];
    let hint = if candidate.is_alias() {
        format!(" (alias of /{})", spec.name)
    } else {
        spec.alias_hint()
    };
    if !hint.is_empty() {
        spans.push(Span::styled(
            hint,
            Style::default().fg(theme::DIM).add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

/// One row of the `/sessions` picker: marker, 1-based session number, a
/// first-message preview, the cell count, and a marker on the active session.
/// One row of the `/sessions` picker: marker, 1-based session number, the
/// session name (the `/rename` name, else its first user message), the cell
/// count, and a marker on the active session.
fn session_menu_line(
    index: usize,
    view: &SessionView,
    current: bool,
    selected: bool,
) -> Line<'static> {
    let marker = if selected { "▸ " } else { "  " };
    let (marker_style, number_style, text_style) = if selected {
        (
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
            Style::default(),
        )
    } else {
        (
            Style::default().fg(theme::DIM),
            Style::default().fg(theme::DIM),
            Style::default().fg(theme::DIM),
        )
    };

    let mut spans = vec![
        Span::styled(marker, marker_style),
        Span::styled(format!("{} · ", index + 1), number_style),
    ];
    let name = view
        .display_name()
        .map(str::to_string)
        .unwrap_or_else(|| format!("session {}", index + 1));
    spans.push(Span::styled(
        format!("{} · ", truncated(&name, 32)),
        text_style,
    ));
    let cells = view.history.len();
    spans.push(Span::styled(
        format!("{cells} cell{}", if cells == 1 { "" } else { "s" }),
        Style::default().fg(theme::DIM),
    ));
    if current {
        spans.push(Span::styled(
            " · current",
            Style::default()
                .fg(theme::DIM)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    Line::from(spans)
}

/// One row of the `/model` picker: marker, 1-based number, the
/// `provider/model` selection, its context window when known, and a marker
/// on the active model.
fn model_menu_line(number: usize, item: &ModelMenuItem, selected: bool) -> Line<'static> {
    let marker = if selected { "▸ " } else { "  " };
    let (marker_style, number_style, text_style) = if selected {
        (
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
            Style::default(),
        )
    } else {
        (
            Style::default().fg(theme::DIM),
            Style::default().fg(theme::DIM),
            Style::default().fg(theme::DIM),
        )
    };

    let mut spans = vec![
        Span::styled(marker, marker_style),
        Span::styled(format!("{number} · "), number_style),
        Span::styled(truncated(&item.selection.as_str(), 48), text_style),
    ];
    if let Some(window) = item.context_window {
        spans.push(Span::styled(
            format!(" · {} ctx", format_tokens(window)),
            Style::default().fg(theme::DIM),
        ));
    }
    if item.current {
        spans.push(Span::styled(
            " · current",
            Style::default()
                .fg(theme::DIM)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    Line::from(spans)
}

/// One row of the `/effort` picker: marker, 1-based number, the level (or the
/// `off` entry), and a marker on the active level.
fn effort_menu_line(number: usize, item: &EffortMenuItem, selected: bool) -> Line<'static> {
    let marker = if selected { "▸ " } else { "  " };
    let (marker_style, number_style, text_style) = if selected {
        (
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
            Style::default(),
        )
    } else {
        (
            Style::default().fg(theme::DIM),
            Style::default().fg(theme::DIM),
            Style::default().fg(theme::DIM),
        )
    };

    let mut spans = vec![
        Span::styled(marker, marker_style),
        Span::styled(format!("{number} · "), number_style),
        Span::styled(
            item.level
                .clone()
                .unwrap_or_else(|| reasoning::EFFORT_OFF.to_string()),
            text_style,
        ),
    ];
    if item.current {
        spans.push(Span::styled(
            " · current",
            Style::default()
                .fg(theme::DIM)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    Line::from(spans)
}

/// One row of the `/tools` picker: checkbox, tool name, and a read-only
/// marker on the tools that only inspect state.
fn tools_menu_line(item: &ToolsMenuItem, selected: bool) -> Line<'static> {
    let marker = if selected { "▸ " } else { "  " };
    let (marker_style, text_style) = if selected {
        (
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
            Style::default(),
        )
    } else {
        (
            Style::default().fg(theme::DIM),
            Style::default().fg(theme::DIM),
        )
    };

    let checkbox = if item.enabled { "[x] " } else { "[ ] " };
    let checkbox_style = if item.enabled {
        Style::default().fg(theme::ACCENT)
    } else {
        Style::default().fg(theme::DIM)
    };

    let mut spans = vec![
        Span::styled(marker, marker_style),
        Span::styled(checkbox, checkbox_style),
        Span::styled(item.name.clone(), text_style),
    ];
    if item.read_only {
        spans.push(Span::styled(
            " · read-only",
            Style::default()
                .fg(theme::DIM)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    Line::from(spans)
}

/// The inline pane always spans the terminal width and is pinned to the bottom.
fn inline_rect(columns: u16, rows: u16, height: u16) -> Rect {
    let height = height.min(rows.max(1));
    Rect::new(0, rows.saturating_sub(height), columns, height)
}

/// Scroll the whole screen up by `lines`, pushing the top lines into the
/// terminal's scrollback, then leave the cursor on the bottom row.
fn scroll_screen_up(rows: u16, lines: usize) -> Result<()> {
    if lines == 0 || rows == 0 {
        return Ok(());
    }
    let mut stdout = std::io::stdout();
    execute!(stdout, MoveTo(0, rows - 1), Print("\n".repeat(lines)))?;
    stdout.flush()?;
    Ok(())
}

/// Write transcript lines above a bottom-anchored pane of `pane_height` rows.
///
/// The screen scrolls once per chunk so every line lands in the terminal's own
/// scrollback, exactly like normal command output.
fn insert_inline_lines(lines: &[Line<'static>], rows: u16, pane_height: u16) -> Result<()> {
    let history_rows = rows.saturating_sub(pane_height) as usize;
    if history_rows == 0 || lines.is_empty() {
        return Ok(());
    }
    let mut stdout = std::io::stdout();
    let mut offset = 0;
    while offset < lines.len() {
        let chunk = (lines.len() - offset).min(history_rows);
        scroll_screen_up(rows, chunk)?;
        let first_row = (history_rows - chunk) as u16;
        for (index, line) in lines[offset..offset + chunk].iter().enumerate() {
            execute!(
                stdout,
                MoveTo(0, first_row + index as u16),
                Clear(ClearType::CurrentLine)
            )?;
            write_styled_line(&mut stdout, line)?;
        }
        stdout.flush()?;
        offset += chunk;
    }
    Ok(())
}

fn write_styled_line<W: Write>(out: &mut W, line: &Line<'_>) -> std::io::Result<()> {
    for span in &line.spans {
        let mut codes: Vec<String> = Vec::new();
        if let Some(color) = span.style.fg {
            codes.push(ansi_foreground(color));
        }
        if span.style.add_modifier.contains(Modifier::BOLD) {
            codes.push("1".to_string());
        }
        if span.style.add_modifier.contains(Modifier::DIM) {
            codes.push("2".to_string());
        }
        if span.style.add_modifier.contains(Modifier::ITALIC) {
            codes.push("3".to_string());
        }
        if span.style.add_modifier.contains(Modifier::UNDERLINED) {
            codes.push("4".to_string());
        }
        if span.style.add_modifier.contains(Modifier::REVERSED) {
            codes.push("7".to_string());
        }
        if span.style.add_modifier.contains(Modifier::CROSSED_OUT) {
            codes.push("9".to_string());
        }
        if !codes.is_empty() {
            write!(out, "\x1b[{}m", codes.join(";"))?;
        }
        write!(out, "{}", span.content)?;
        if !codes.is_empty() {
            write!(out, "\x1b[0m")?;
        }
    }
    Ok(())
}

fn ansi_foreground(color: Color) -> String {
    match color {
        Color::Reset => "39".to_string(),
        Color::Black => "30".to_string(),
        Color::Red => "31".to_string(),
        Color::Green => "32".to_string(),
        Color::Yellow => "33".to_string(),
        Color::Blue => "34".to_string(),
        Color::Magenta => "35".to_string(),
        Color::Cyan => "36".to_string(),
        Color::Gray => "37".to_string(),
        Color::DarkGray => "90".to_string(),
        Color::LightRed => "91".to_string(),
        Color::LightGreen => "92".to_string(),
        Color::LightYellow => "93".to_string(),
        Color::LightBlue => "94".to_string(),
        Color::LightMagenta => "95".to_string(),
        Color::LightCyan => "96".to_string(),
        Color::White => "97".to_string(),
        Color::Rgb(red, green, blue) => format!("38;2;{red};{green};{blue}"),
        Color::Indexed(index) => format!("38;5;{index}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::ModelInfo;
    use crate::core::config::ProviderInstance;
    use crate::core::providers::ProviderRegistry;
    use crate::plugins::Plugin;
    use ratatui::backend::TestBackend;

    /// A pool over the built-in `test` protocol with two selectable models.
    fn test_pool() -> Arc<ModelPool> {
        let instance = ProviderInstance {
            name: "test".to_string(),
            protocol: Some("test".to_string()),
            base_url: None,
            api_key: None,
            models: vec![ModelInfo::from_id("one"), ModelInfo::from_id("two")],
            fetch_models: Some(false),
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: false,
        };
        let registry = ProviderRegistry::build(&[], vec![instance]).expect("valid registry");
        Arc::new(ModelPool::new(registry))
    }

    fn test_app() -> App {
        let pool = test_pool();
        let session = AgentSession::spawn_with_pool(
            pool.clone(),
            Some(&Selection::new("test", "one")),
            DEFAULT_SYSTEM_PROMPT,
            SessionBootstrap::default(),
        )
        .expect("initial selection resolves");
        let (notice_tx, notice_rx) = tokio::sync::mpsc::unbounded_channel();
        let (provider_tx, provider_tasks) = tokio::sync::mpsc::unbounded_channel();
        let (_plugin_effect_tx, plugin_effects) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            session,
            pool,
            notice_tx,
            notice_rx,
            provider_tx,
            provider_tasks,

            plugin_effects,
            "test/one".to_string(),
            TuiMode::Fullscreen,
            Arc::new(PluginHost::shipped().expect("shipped plugins")),
            Vec::new(),
            vec![SessionView::initial("test/one", TuiMode::Fullscreen)],
            0,
            SessionBootstrap::empty_profiles(),
            None,
            None,
            StatusBarConfig::default(),
            Vec::new(),
        )
    }

    /// An app whose plugin host is `plugins` — used by the `/provider` tests,
    /// which need a plugin that contributes a sign-in entry.
    fn test_app_with_plugins(plugins: Arc<PluginHost>, pool: Arc<ModelPool>) -> App {
        let session = AgentSession::spawn_with_pool(
            pool.clone(),
            Some(&Selection::new("test", "one")),
            DEFAULT_SYSTEM_PROMPT,
            SessionBootstrap::default(),
        )
        .expect("initial selection resolves");
        let (notice_tx, notice_rx) = tokio::sync::mpsc::unbounded_channel();
        let (provider_tx, provider_tasks) = tokio::sync::mpsc::unbounded_channel();
        let (_plugin_effect_tx, plugin_effects) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            session,
            pool,
            notice_tx,
            notice_rx,
            provider_tx,
            provider_tasks,

            plugin_effects,
            "test/one".to_string(),
            TuiMode::Fullscreen,
            plugins,
            Vec::new(),
            vec![SessionView::initial("test/one", TuiMode::Fullscreen)],
            0,
            SessionBootstrap::empty_profiles(),
            None,
            None,
            StatusBarConfig::default(),
            Vec::new(),
        )
    }

    /// An app whose worker and `/profile` state share one profile table.
    fn test_app_with_profiles(profiles: crate::core::config::ProfileSet) -> App {
        let pool = test_pool();
        let session = AgentSession::spawn_with_pool(
            pool.clone(),
            Some(&Selection::new("test", "one")),
            DEFAULT_SYSTEM_PROMPT,
            SessionBootstrap {
                profiles: Arc::new(profiles.clone()),
                ..SessionBootstrap::default()
            },
        )
        .expect("initial selection resolves");
        let (notice_tx, notice_rx) = tokio::sync::mpsc::unbounded_channel();
        let (provider_tx, provider_tasks) = tokio::sync::mpsc::unbounded_channel();
        let (_plugin_effect_tx, plugin_effects) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            session,
            pool,
            notice_tx,
            notice_rx,
            provider_tx,
            provider_tasks,

            plugin_effects,
            "test/one".to_string(),
            TuiMode::Fullscreen,
            Arc::new(PluginHost::shipped().expect("shipped plugins")),
            Vec::new(),
            vec![SessionView::initial("test/one", TuiMode::Fullscreen)],
            0,
            Arc::new(profiles),
            None,
            None,
            StatusBarConfig::default(),
            Vec::new(),
        )
    }

    /// A pool whose openai-compat instance is half specified (url but no
    /// key) — the partial CLI synthesis shape.
    fn test_pool_partial() -> Arc<ModelPool> {
        let instance = ProviderInstance {
            name: "openai".to_string(),
            protocol: None,
            base_url: Some("http://127.0.0.1:1/v1".to_string()),
            api_key: None,
            models: vec![ModelInfo::from_id("m1")],
            fetch_models: Some(false),
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: true,
        };
        let registry = ProviderRegistry::build(&[], vec![instance]).expect("valid registry");
        Arc::new(ModelPool::new(registry))
    }

    fn test_app_partial() -> App {
        let pool = test_pool_partial();
        let session = AgentSession::spawn_with_pool(
            pool.clone(),
            Some(&Selection::new("openai", "m1")),
            DEFAULT_SYSTEM_PROMPT,
            SessionBootstrap::default(),
        )
        .expect("partial selection resolves (gated at send, not spawn)");
        let (notice_tx, notice_rx) = tokio::sync::mpsc::unbounded_channel();
        let (provider_tx, provider_tasks) = tokio::sync::mpsc::unbounded_channel();
        let (_plugin_effect_tx, plugin_effects) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            session,
            pool,
            notice_tx,
            notice_rx,
            provider_tx,
            provider_tasks,

            plugin_effects,            "openai/m1".to_string(),
            TuiMode::Fullscreen,
            Arc::new(PluginHost::shipped().expect("shipped plugins")),
            Vec::new(),
            vec![SessionView::initial("openai/m1", TuiMode::Fullscreen)],
            0,
            SessionBootstrap::empty_profiles(),
            None,
            None,
            StatusBarConfig::default(),
            Vec::new(),
        )
    }

    /// A pool whose `test/one` model declares reasoning metadata: a levels
    /// list that does NOT spell out its own `default_level`.
    fn meta_pool() -> Arc<ModelPool> {
        let mut one = ModelInfo::from_id("one");
        one.reasoning = Some(vec!["low".to_string(), "high".to_string()]);
        one.default_level = Some("thinking".to_string());
        let instance = ProviderInstance {
            name: "test".to_string(),
            protocol: Some("test".to_string()),
            base_url: None,
            api_key: None,
            models: vec![one, ModelInfo::from_id("two")],
            fetch_models: Some(false),
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: false,
        };
        let registry = ProviderRegistry::build(&[], vec![instance]).expect("valid registry");
        Arc::new(ModelPool::new(registry))
    }

    fn test_app_meta() -> App {
        let pool = meta_pool();
        let session = AgentSession::spawn_with_pool(
            pool.clone(),
            Some(&Selection::new("test", "one")),
            DEFAULT_SYSTEM_PROMPT,
            SessionBootstrap::default(),
        )
        .expect("initial selection resolves");
        let (notice_tx, notice_rx) = tokio::sync::mpsc::unbounded_channel();
        let (provider_tx, provider_tasks) = tokio::sync::mpsc::unbounded_channel();
        let (_plugin_effect_tx, plugin_effects) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            session,
            pool,
            notice_tx,
            notice_rx,
            provider_tx,
            provider_tasks,

            plugin_effects,            "test/one".to_string(),
            TuiMode::Fullscreen,
            Arc::new(PluginHost::shipped().expect("shipped plugins")),
            Vec::new(),
            vec![SessionView::initial("test/one", TuiMode::Fullscreen)],
            0,
            SessionBootstrap::empty_profiles(),
            None,
            None,
            StatusBarConfig::default(),
            Vec::new(),
        )
    }

    /// A pool whose `test/one` model declares a context window, for the
    /// context-usage footer and `/status` tests.
    fn context_pool() -> Arc<ModelPool> {
        let mut one = ModelInfo::from_id("one");
        one.context_window = Some(200_000);
        let instance = ProviderInstance {
            name: "test".to_string(),
            protocol: Some("test".to_string()),
            base_url: None,
            api_key: None,
            models: vec![one, ModelInfo::from_id("two")],
            fetch_models: Some(false),
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: false,
        };
        let registry = ProviderRegistry::build(&[], vec![instance]).expect("valid registry");
        Arc::new(ModelPool::new(registry))
    }

    fn test_app_context() -> App {
        let pool = context_pool();
        let session = AgentSession::spawn_with_pool(
            pool.clone(),
            Some(&Selection::new("test", "one")),
            DEFAULT_SYSTEM_PROMPT,
            SessionBootstrap::default(),
        )
        .expect("initial selection resolves");
        let (notice_tx, notice_rx) = tokio::sync::mpsc::unbounded_channel();
        let (provider_tx, provider_tasks) = tokio::sync::mpsc::unbounded_channel();
        let (_plugin_effect_tx, plugin_effects) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            session,
            pool,
            notice_tx,
            notice_rx,
            provider_tx,
            provider_tasks,

            plugin_effects,            "test/one".to_string(),
            TuiMode::Fullscreen,
            Arc::new(PluginHost::shipped().expect("shipped plugins")),
            Vec::new(),
            vec![SessionView::initial("test/one", TuiMode::Fullscreen)],
            0,
            SessionBootstrap::empty_profiles(),
            None,
            None,
            StatusBarConfig::default(),
            Vec::new(),
        )
    }

    #[tokio::test]
    async fn a_model_default_level_seeds_the_footer_effort() {
        let mut app = test_app_meta();
        assert_eq!(app.effort.as_deref(), Some("thinking"));
        let lines = render_lines(&mut app, 110, 20);
        assert!(lines.join("\n").contains("· effort thinking"), "{lines:?}");

        // The model's default stays selectable even though the declared
        // `reasoning` list does not spell it out…
        app.composer.set_text("/effort thinking".to_string());
        app.try_submit();
        assert_eq!(app.effort.as_deref(), Some("thinking"));

        // …the declared levels stay available…
        app.composer.set_text("/effort high".to_string());
        app.try_submit();
        assert_eq!(app.effort.as_deref(), Some("high"));

        // …and levels outside both are still rejected.
        app.composer.set_text("/effort medium".to_string());
        app.try_submit();
        assert_eq!(app.effort.as_deref(), Some("high"));
        assert!(
            matches!(app.view().history.last(), Some(HistoryCell::Error(text)) if text.contains("unknown effort level"))
        );
    }

    /// An app whose provider is a complete endpoint with no models yet:
    /// route empty until discovery fills the list.
    fn test_app_unselected() -> App {
        let instance = ProviderInstance {
            name: "openai".to_string(),
            protocol: None,
            base_url: Some("http://127.0.0.1:1/v1".to_string()),
            api_key: Some("k".to_string()),
            models: vec![],
            fetch_models: None,
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: true,
        };
        let registry = ProviderRegistry::build(&[], vec![instance]).expect("valid registry");
        let pool = Arc::new(ModelPool::new(registry));
        let session = AgentSession::spawn_with_pool(
            pool.clone(),
            None,
            DEFAULT_SYSTEM_PROMPT,
            SessionBootstrap::default(),
        )
        .expect("empty route spawns");
        let (notice_tx, notice_rx) = tokio::sync::mpsc::unbounded_channel();
        let (provider_tx, provider_tasks) = tokio::sync::mpsc::unbounded_channel();
        let (_plugin_effect_tx, plugin_effects) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            session,
            pool,
            notice_tx,
            notice_rx,
            provider_tx,
            provider_tasks,

            plugin_effects,            "no model".to_string(),
            TuiMode::Fullscreen,
            Arc::new(PluginHost::shipped().expect("shipped plugins")),
            Vec::new(),
            vec![SessionView::initial("no model", TuiMode::Fullscreen)],
            0,
            SessionBootstrap::empty_profiles(),
            None,
            None,
            StatusBarConfig::default(),
            Vec::new(),
        )
    }

    /// An app started with nothing configured: an empty registry (so even
    /// `/model` has nothing to list), an empty route, and a `no model` label.
    fn test_app_without_model() -> App {
        let registry = ProviderRegistry::build(&[], Vec::new()).expect("empty registry");
        let pool = Arc::new(ModelPool::new(registry));
        let session = AgentSession::spawn_with_pool(
            pool.clone(),
            None,
            DEFAULT_SYSTEM_PROMPT,
            SessionBootstrap::default(),
        )
        .expect("empty route spawns");
        let (notice_tx, notice_rx) = tokio::sync::mpsc::unbounded_channel();
        let (provider_tx, provider_tasks) = tokio::sync::mpsc::unbounded_channel();
        let (_plugin_effect_tx, plugin_effects) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            session,
            pool,
            notice_tx,
            notice_rx,
            provider_tx,
            provider_tasks,

            plugin_effects,            "no model".to_string(),
            TuiMode::Fullscreen,
            Arc::new(PluginHost::shipped().expect("shipped plugins")),
            Vec::new(),
            vec![SessionView::initial("no model", TuiMode::Fullscreen)],
            0,
            SessionBootstrap::empty_profiles(),
            None,
            None,
            StatusBarConfig::default(),
            Vec::new(),
        )
    }

    fn render_lines(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| app.render_fullscreen(frame))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                let line: String = (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>();
                line.trim_end().to_string()
            })
            .collect()
    }

    #[tokio::test]
    async fn profile_command_lists_switches_and_the_event_confirms() {
        let mut profiles = crate::core::config::ProfileSet::new();
        profiles.insert(
            "deep".to_string(),
            crate::core::config::Profile {
                default: Some("test/two".to_string()),
                effort: Some("low".to_string()),
                ..Default::default()
            },
        );
        let mut app = test_app_with_profiles(profiles);
        let event = |msg| crate::core::Event::new("sub-1", msg);

        // No argument: the profiles are listed with their presets; nothing is
        // active yet.
        app.composer.set_text("/profile".to_string());
        app.try_submit();
        assert!(
            matches!(app.view().history.last(),
                Some(HistoryCell::Notice(text)) if text.contains("deep") && text.contains("test/two")),
            "{:?}",
            app.view().history.last()
        );

        // An unknown name: an error cell, no switch.
        app.composer.set_text("/profile nope".to_string());
        app.try_submit();
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Error(_))
        ));
        assert_eq!(app.active_profile, None);

        // A known name: the optimistic notice goes out and the op follows.
        app.composer.set_text("/profile deep".to_string());
        app.try_submit();
        assert!(
            matches!(app.view().history.last(),
                Some(HistoryCell::Notice(text)) if text.contains("profile `deep`")),
            "{:?}",
            app.view().history.last()
        );
        assert_eq!(app.active_profile.as_deref(), Some("deep"));

        // The listing now marks the active profile.
        app.composer.set_text("/profile".to_string());
        app.try_submit();
        assert!(
            matches!(app.view().history.last(),
                Some(HistoryCell::Notice(text)) if text.contains("* deep")),
            "{:?}",
            app.view().history.last()
        );

        // The worker's confirmation updates the state silently; the preset's
        // selection and effort ride along through their own events.
        app.handle_agent_event(event(EventMsg::ProfileSwitched {
            name: "deep".to_string(),
        }));
        assert_eq!(app.active_profile.as_deref(), Some("deep"));
        app.handle_agent_event(event(EventMsg::ModelSwitched {
            model: "test/two".to_string(),
        }));
        assert_eq!(app.model_label, "test/two");
        app.handle_agent_event(event(EventMsg::ReasoningEffortSwitched {
            effort: Some("low".to_string()),
        }));
        assert_eq!(app.effort.as_deref(), Some("low"));
    }

    #[test]
    fn build_views_restores_sessions_and_banners_the_active_one() {
        use crate::core::Message;

        let loaded_session = |id: &str,
                              messages: Vec<Message>,
                              name: Option<String>,
                              last_usage: Option<(u64, u64)>|
         -> LoadedSession {
            LoadedSession {
                meta: crate::core::SessionMeta {
                    version: 1,
                    id: id.to_string(),
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    cwd: "/tmp/demo".to_string(),
                    system_prompt: "be brief".to_string(),
                    name: None,
                    provider: None,
                    model: None,
                    profile: None,
                },
                path: PathBuf::from(format!("/tmp/{id}.jsonl")),
                model: None,
                effort: None,
                last_usage,
                total_input: last_usage.map_or(0, |(input, _)| input),
                total_output: last_usage.map_or(0, |(_, output)| output),
                messages,
                thinking: Vec::new(),
                undo: Vec::new(),
                name,
                profile: None,
                tools: None,
            }
        };

        let loaded = vec![
            loaded_session(
                "one",
                vec![Message::user("hello"), Message::assistant("hi")],
                None,
                Some((1_000, 200)),
            ),
            loaded_session("named", vec![], Some("my chat".to_string()), None),
        ];
        let (views, active) = build_views("test-model", TuiMode::Fullscreen, &loaded, false);
        assert_eq!(views.len(), 2);
        assert_eq!(active, 1);
        // Messages replay into transcript cells, already counted as printed.
        assert_eq!(views[0].history.len(), 2);
        assert!(matches!(&views[0].history[0], HistoryCell::User(text) if text == "hello"));
        assert_eq!(views[0].printed_cells, 2);
        // The rollout's recorded usage seeds the view's context accounting.
        assert_eq!(views[0].last_usage, Some((1_000, 200)));
        assert_eq!(views[0].total_input, 1_000);
        assert_eq!(views[0].total_output, 200);
        assert_eq!(views[1].name.as_deref(), Some("my chat"));
        // The active view carries this run's banner plus the resumed notice.
        assert!(
            matches!(&views[1].history[0], HistoryCell::Notice(text) if text.contains("codest"))
        );
        assert!(matches!(
            &views[1].history.last(),
            Some(HistoryCell::Notice(text)) if text.contains("resumed 2 saved sessions")
        ));

        // A plain startup restores the saved sessions and opens a fresh one;
        // the fresh view points at the list instead of claiming a resume.
        let (views, active) = build_views("test-model", TuiMode::Fullscreen, &loaded, true);
        assert_eq!(views.len(), 3);
        assert_eq!(active, 2);
        assert!(matches!(
            &views[2].history.last(),
            Some(HistoryCell::Notice(text))
                if text.contains("2 saved sessions available via /sessions")
        ));

        // Nothing restored anywhere falls back to the fresh initial view.
        let (views, active) = build_views("test-model", TuiMode::Fullscreen, &[], true);
        assert_eq!(views.len(), 1);
        assert_eq!(active, 0);
        assert!(
            matches!(&views[0].history[0], HistoryCell::Notice(text) if text.contains("codest"))
        );
    }

    #[test]
    fn restore_model_selection_adopts_the_last_saved_model() {
        let pool = context_pool();
        let selection = Some(Selection::new("test", "one"));
        let saved_session = |model: Option<&str>| LoadedSession {
            meta: crate::core::SessionMeta {
                version: 1,
                id: "s".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                cwd: "/tmp/demo".to_string(),
                system_prompt: "be brief".to_string(),
                name: None,
                provider: None,
                model: None,
                profile: None,
            },
            path: PathBuf::from("/tmp/s.jsonl"),
            model: model.map(str::to_string),
            effort: None,
            last_usage: None,
            total_input: 0,
            total_output: 0,
            messages: Vec::new(),
            thinking: Vec::new(),
            undo: Vec::new(),
            name: None,
            profile: None,
            tools: None,
        };
        let saved = vec![saved_session(Some("test/two"))];

        // A plain startup keeps the configured selection.
        assert_eq!(
            restore_model_selection(&pool, selection.clone(), &saved, true),
            selection
        );
        // A restore adopts the session's last used model…
        assert_eq!(
            restore_model_selection(&pool, selection.clone(), &saved, false),
            Some(Selection::new("test", "two"))
        );
        // …keeps it when it already matches…
        let already = Some(Selection::new("test", "two"));
        assert_eq!(
            restore_model_selection(&pool, already, &saved, false),
            Some(Selection::new("test", "two"))
        );
        // …falls back when the saved provider is gone…
        let gone = vec![saved_session(Some("ghost/model"))];
        assert_eq!(
            restore_model_selection(&pool, selection.clone(), &gone, false),
            selection
        );
        // …and when the session never recorded a model.
        let unnamed = vec![saved_session(None)];
        assert_eq!(
            restore_model_selection(&pool, selection, &unnamed, false),
            None
        );
    }

    #[tokio::test]
    async fn command_menu_renders_above_the_composer() {
        let mut app = test_app();
        app.composer.set_text("/".to_string());

        // Wide enough that the alias hints are not clipped.
        let lines = render_lines(&mut app, 110, 20);
        let rendered = lines.join("\n");
        // Nineteen names (fifteen commands plus four aliases) share a
        // six-row window; the first page shows /help with its aliases and the
        // first commands, aliases labeled with the command they point at.
        assert!(rendered.contains("▸ /help"), "{rendered}");
        assert!(rendered.contains("/h "), "{rendered}");
        assert!(rendered.contains("/clear"), "{rendered}");
        assert!(rendered.contains("(alias of /help)"), "{rendered}");
        assert!(rendered.contains("↑↓ select"), "{rendered}");
        assert!(!rendered.contains("/reset"), "{rendered}");
        assert!(!rendered.contains("/sessions "), "{rendered}");

        let menu_row = lines
            .iter()
            .position(|line| line.contains("/help"))
            .expect("menu row");
        let composer_row = lines
            .iter()
            .position(|line| line.contains("›"))
            .expect("composer row");
        assert!(menu_row < composer_row, "{rendered}");

        // Moving the selection pages the window onto the tail; the longest
        // command name still gets a separating space before its summary.
        // (Twelve presses put the selection on the /resume alias; /tps sits
        // between /continue and the shipped commands.)
        for _ in 0..12 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let lines = render_lines(&mut app, 110, 20);
        let rendered = lines.join("\n");
        assert!(rendered.contains("▸ /resume"), "{rendered}");
        assert!(rendered.contains("/sessions "), "{rendered}");
        assert!(rendered.contains("(alias of /sessions)"), "{rendered}");
    }

    #[tokio::test]
    async fn slash_help_prints_a_notice_without_starting_a_turn() {
        let mut app = test_app();
        app.composer.set_text("/help".to_string());
        app.try_submit();

        assert!(!app.busy);
        assert!(app.composer.is_empty());
        match app.view().history.last() {
            Some(HistoryCell::Notice(text)) => {
                assert!(text.contains("/clear"));
                assert!(text.contains("/quit"));
            }
            other => panic!("expected a help notice, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_command_reports_an_error_without_starting_a_turn() {
        let mut app = test_app();
        app.composer.set_text("/nope".to_string());
        app.try_submit();

        assert!(!app.busy);
        match app.view().history.last() {
            Some(HistoryCell::Error(text)) => assert!(text.contains("unknown command")),
            other => panic!("expected an error cell, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn plain_text_still_starts_a_turn() {
        let mut app = test_app();
        app.composer.set_text("hello".to_string());
        app.try_submit();

        assert!(app.busy);
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::User(text)) if text == "hello"
        ));
    }

    /// `/continue` on a model-less run gets the same guidance a refused
    /// message gets, and once a model is active the effect starts a turn that
    /// appends no user message — the transcript is driven as it stands.
    #[tokio::test]
    async fn continue_with_no_model_shows_guidance_instead_of_submitting() {
        let mut app = test_app_without_model();
        app.apply_effects(vec![PluginEffect::ContinueTurn]);

        assert!(!app.busy);
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Error(text)) if text.contains("no model is configured"),
        ));

        app.handle_agent_event(crate::core::Event::new(
            "sub-1",
            EventMsg::ModelSwitched {
                model: "test/one".to_string(),
            },
        ));
        app.apply_effects(vec![PluginEffect::ContinueTurn]);
        assert!(app.busy);
        assert!(app.view().live.is_some());
        assert!(
            !app.view()
                .history
                .iter()
                .any(|cell| matches!(cell, HistoryCell::User(_))),
            "no user message is appended"
        );
    }

    #[tokio::test]
    async fn continue_while_busy_is_ignored() {
        let mut app = test_app();
        app.busy = true;
        let before = app.view().history.len();

        app.apply_effects(vec![PluginEffect::ContinueTurn]);

        assert!(app.busy);
        assert_eq!(app.view().history.len(), before);
    }

    #[tokio::test]
    async fn clear_resets_the_transcript_immediately() {
        let mut app = test_app();
        app.view_mut()
            .history
            .push(HistoryCell::Assistant("old reply".to_string()));

        app.composer.set_text("/clear".to_string());
        app.try_submit();

        assert_eq!(app.view().history.len(), 1);
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Notice(text)) if text == "conversation cleared"
        ));
        assert_eq!(app.view().printed_cells, 0);
        assert!(app.view().follow);
    }

    #[tokio::test]
    async fn plugin_effects_are_applied_in_order() {
        let mut app = test_app();
        app.apply_effects(vec![
            PluginEffect::SetComposer("/help".to_string()),
            PluginEffect::Notice("hello".to_string()),
            PluginEffect::Quit,
        ]);

        assert_eq!(app.composer.text(), "/help");
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Notice(text)) if text == "hello"
        ));
        assert!(app.quit);
    }

    #[tokio::test]
    async fn submit_effect_only_starts_a_turn_while_idle() {
        let mut app = test_app();
        app.apply_effects(vec![PluginEffect::SubmitUserTurn("hello".to_string())]);
        assert!(app.busy);
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::User(text)) if text == "hello"
        ));

        let mut app = test_app();
        app.busy = true;
        let before = app.view().history.len();
        app.apply_effects(vec![PluginEffect::SubmitUserTurn("ignored".to_string())]);
        assert_eq!(app.view().history.len(), before);
    }

    #[tokio::test]
    async fn status_command_reports_plugin_context() {
        let mut app = test_app();
        app.composer.set_text("/status".to_string());
        app.try_submit();

        assert!(!app.busy);
        match app.view().history.last() {
            Some(HistoryCell::Notice(text)) => {
                assert!(text.contains("backend: test/one"), "{text}");
                assert!(
                    text.contains("plugins: help, clear, quit, status"),
                    "{text}"
                );
            }
            other => panic!("expected a status notice, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_menu_lists_selections_and_switches() {
        let mut app = test_app();
        app.composer.set_text("/model".to_string());
        app.try_submit();

        // The picker lists every selection with the active one marked.
        assert!(app.model_menu.is_some());
        assert_eq!(app.model_menu.as_ref().unwrap().items.len(), 2);
        assert_eq!(app.model_menu.as_ref().unwrap().selected, 0);

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.model_menu.is_none());
        assert_eq!(app.model_label, "test/two");
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Notice(text)) if text == "switched to test/two"
        ));

        // Confirming on the already-active model only closes the menu.
        app.composer.set_text("/model".to_string());
        app.try_submit();
        assert_eq!(app.model_menu.as_ref().unwrap().selected, 1);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.model_menu.is_none());
        let notices = app
            .view()
            .history
            .iter()
            .filter(
                |cell| matches!(cell, HistoryCell::Notice(text) if text.contains("switched to")),
            )
            .count();
        assert_eq!(notices, 1);
    }

    #[tokio::test]
    async fn model_command_rejects_arguments_and_busy_state() {
        let mut app = test_app();
        app.composer.set_text("/model now".to_string());
        app.try_submit();
        assert!(app.model_menu.is_none());
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Error(text)) if text.contains("arguments")
        ));

        app.busy = true;
        app.composer.set_text("/model".to_string());
        app.try_submit();
        assert!(app.model_menu.is_none(), "no picker while streaming");
    }

    #[tokio::test]
    async fn model_menu_renders_selections_with_the_current_marker() {
        let mut app = test_app();
        app.composer.set_text("/model".to_string());
        app.try_submit();
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

        // Wide enough that the footer's always-on context segment does not
        // squeeze the menu's hint out of the render.
        let lines = render_lines(&mut app, 100, 20);
        let rendered = lines.join("\n");
        assert!(rendered.contains("▸ 2 · test/two"), "{rendered}");
        assert!(rendered.contains("test/one"), "{rendered}");
        assert!(rendered.contains("current"), "{rendered}");
        assert!(rendered.contains("r refresh"), "{rendered}");
    }

    #[tokio::test]
    async fn model_switched_event_updates_the_label() {
        let mut app = test_app();
        app.handle_agent_event(crate::core::Event::new(
            "sub-1",
            EventMsg::ModelSwitched {
                model: "test/two".to_string(),
            },
        ));
        assert_eq!(app.model_label, "test/two");
        assert_eq!(app.route, Some(Selection::new("test", "two")));
    }

    #[tokio::test]
    async fn reasoning_effort_switched_event_updates_effort() {
        let mut app = test_app();
        app.handle_agent_event(crate::core::Event::new(
            "sub-1",
            EventMsg::ReasoningEffortSwitched {
                effort: Some("medium".to_string()),
            },
        ));
        assert_eq!(app.effort.as_deref(), Some("medium"));

        app.handle_agent_event(crate::core::Event::new(
            "sub-1",
            EventMsg::ReasoningEffortSwitched { effort: None },
        ));
        assert_eq!(app.effort, None);
    }

    #[tokio::test]
    async fn turn_usage_accumulates_per_session_view() {
        let mut app = test_app();

        app.handle_agent_event(crate::core::Event::new(
            "sub-1",
            EventMsg::TurnUsage {
                input: 1_000,
                output: 200,
            },
        ));
        assert_eq!(app.view().last_usage, Some((1_000, 200)));
        assert_eq!(app.view().total_input, 1_000);
        assert_eq!(app.view().total_output, 200);

        // Each turn replaces `last_usage` and grows the session totals.
        app.handle_agent_event(crate::core::Event::new(
            "sub-2",
            EventMsg::TurnUsage {
                input: 1_300,
                output: 300,
            },
        ));
        assert_eq!(app.view().last_usage, Some((1_300, 300)));
        assert_eq!(app.view().total_input, 2_300);
        assert_eq!(app.view().total_output, 500);

        // A fresh session tracks its own usage from zero…
        app.start_new_session();
        assert_eq!(app.view().last_usage, None);
        app.handle_agent_event(crate::core::Event::new(
            "sub-3",
            EventMsg::TurnUsage {
                input: 500,
                output: 50,
            },
        ));
        assert_eq!(app.view().last_usage, Some((500, 50)));

        // …and switching back shows the first session's stats untouched.
        app.switch_to_session(0);
        assert_eq!(app.view().last_usage, Some((1_300, 300)));
        assert_eq!(app.view().total_input, 2_300);
        assert_eq!(app.view().total_output, 500);
    }

    #[tokio::test]
    async fn footer_shows_context_usage_against_the_window() {
        let mut app = test_app_context();

        // No measured turn yet: the window total shows behind an unmeasured
        // placeholder.
        let lines = render_lines(&mut app, 110, 20);
        assert!(lines.join("\n").contains("· —/200.0k"), "{lines:?}");

        app.handle_agent_event(crate::core::Event::new(
            "sub-1",
            EventMsg::TurnUsage {
                input: 12_340,
                output: 660,
            },
        ));
        let lines = render_lines(&mut app, 110, 20);
        assert!(
            lines.join("\n").contains("· 13.0k/200.0k (6%)"),
            "{lines:?}"
        );
        assert!(!app.context_segment().1);

        // Crossing 90% of the window flags the segment as a warning.
        app.handle_agent_event(crate::core::Event::new(
            "sub-2",
            EventMsg::TurnUsage {
                input: 180_000,
                output: 10_000,
            },
        ));
        let lines = render_lines(&mut app, 110, 20);
        assert!(
            lines.join("\n").contains("· 190.0k/200.0k (95%)"),
            "{lines:?}"
        );
        assert!(app.context_segment().1);
    }

    /// The context segment is always on screen: without a measured turn and
    /// without a known window it shows a placeholder rather than vanishing.
    #[tokio::test]
    async fn footer_keeps_the_context_segment_with_no_metadata() {
        let mut app = test_app();
        assert_eq!(app.context_window(), None);
        assert_eq!(app.context_tokens(), None);
        let lines = render_lines(&mut app, 110, 20);
        let rendered = lines.join("\n");
        assert!(rendered.contains("· —  │  "), "{rendered}");

        // A measured round fills it in place, and the turn-in-progress
        // event alone is enough to do so.
        app.handle_agent_event(crate::core::Event::new(
            "sub-1",
            EventMsg::ContextUsage {
                input: 4_000,
                output: 200,
            },
        ));
        let lines = render_lines(&mut app, 110, 20);
        assert!(lines.join("\n").contains("· 4.2k tok"), "{lines:?}");
    }

    /// Mid-turn usage is display-only: it moves the footer without touching
    /// the session totals or the completed-turn record.
    #[tokio::test]
    async fn context_usage_updates_the_footer_without_accumulating() {
        let mut app = test_app();
        app.handle_agent_event(crate::core::Event::new(
            "sub-1",
            EventMsg::ContextUsage {
                input: 900,
                output: 100,
            },
        ));
        assert_eq!(app.view().last_usage, Some((900, 100)));
        assert_eq!(app.view().total_input, 0, "no accumulation mid-turn");
        assert_eq!(app.view().total_output, 0);

        // The completed turn is still the one counted.
        app.handle_agent_event(crate::core::Event::new(
            "sub-1",
            EventMsg::TurnUsage {
                input: 1_000,
                output: 200,
            },
        ));
        assert_eq!(app.view().last_usage, Some((1_000, 200)));
        assert_eq!(app.view().total_input, 1_000);
        assert_eq!(app.view().total_output, 200);
    }

    #[tokio::test]
    async fn plugin_status_segments_render_between_context_and_state() {
        let mut app = test_app();
        let set = |key: &str, text: &str| {
            PluginEffect::SetStatus(crate::plugins::StatusSegment {
                key: key.to_string(),
                text: text.to_string(),
                tone: crate::plugins::StatusTone::Accent,
            })
        };
        app.apply_effects(vec![set("tps:rate", "42 t/s")]);
        let footer = render_lines(&mut app, 110, 20)
            .pop()
            .expect("footer line");
        assert!(footer.contains("42 t/s"), "{footer}");
        assert!(footer.contains("· 42 t/s  │  ready"), "{footer}");

        // A re-set key upserts in place rather than appending a duplicate.
        app.apply_effects(vec![set("tps:rate", "7.5 t/s")]);
        let footer = render_lines(&mut app, 110, 20).pop().expect("footer");
        assert!(footer.contains("· 7.5 t/s  │"), "{footer}");
        assert!(!footer.contains("42 t/s"), "{footer}");

        // ClearStatus removes the segment and its separator with it.
        app.apply_effects(vec![PluginEffect::ClearStatus("tps:rate".to_string())]);
        let footer = render_lines(&mut app, 110, 20).pop().expect("footer");
        assert!(!footer.contains("t/s"), "{footer}");
    }

    #[tokio::test]
    async fn status_bar_order_reorders_plugin_segments() {
        let mut app = test_app();
        let set = |key: &str, text: &str| {
            PluginEffect::SetStatus(crate::plugins::StatusSegment {
                key: key.to_string(),
                text: text.to_string(),
                tone: crate::plugins::StatusTone::Dim,
            })
        };
        app.apply_effects(vec![set("a:first", "AAA"), set("b:second", "BBB")]);
        // Registration order by default.
        let footer = render_lines(&mut app, 110, 20).pop().expect("footer");
        let (first, second) = (
            footer.find("AAA").expect("AAA"),
            footer.find("BBB").expect("BBB"),
        );
        assert!(first < second, "{footer}");

        // `[status_bar] order` flips the display priority.
        app.status_bar.order = vec!["b:second".to_string(), "a:first".to_string()];
        let footer = render_lines(&mut app, 110, 20).pop().expect("footer");
        let (first, second) = (
            footer.find("AAA").expect("AAA"),
            footer.find("BBB").expect("BBB"),
        );
        assert!(second < first, "{footer}");
    }

    #[tokio::test]
    async fn status_bar_plugin_segments_can_be_turned_off() {
        let mut app = test_app();
        app.apply_effects(vec![PluginEffect::SetStatus(crate::plugins::StatusSegment {
            key: "tps:rate".to_string(),
            text: "42 t/s".to_string(),
            tone: crate::plugins::StatusTone::Dim,
        })]);
        app.status_bar.plugin_segments = false;
        let footer = render_lines(&mut app, 110, 20).pop().expect("footer");
        assert!(!footer.contains("42 t/s"), "{footer}");
        assert!(footer.contains("│  ready"), "{footer}");
    }

    #[tokio::test]
    async fn footer_shows_bare_tokens_without_a_known_window() {
        let mut app = test_app();
        app.handle_agent_event(crate::core::Event::new(
            "sub-1",
            EventMsg::TurnUsage {
                input: 12_340,
                output: 660,
            },
        ));
        let lines = render_lines(&mut app, 110, 20);
        assert!(lines.join("\n").contains("· 13.0k tok"), "{lines:?}");
    }

    #[tokio::test]
    async fn model_menu_lists_the_context_window() {
        // `context_pool` declares a window only for `test/one`, so the menu
        // exercises both the annotated and the bare row.
        let mut app = test_app_context();
        app.composer.set_text("/model".to_string());
        app.try_submit();
        let lines = render_lines(&mut app, 80, 20);
        let rendered = lines.join("\n");
        assert!(
            rendered.contains("1 · test/one · 200.0k ctx · current"),
            "{rendered}"
        );
        assert!(rendered.contains("2 · test/two\n"), "{rendered}");

        // Without declared metadata the row simply omits the window.
        let mut plain = test_app();
        plain.composer.set_text("/model".to_string());
        plain.try_submit();
        let lines = render_lines(&mut plain, 80, 20);
        let rendered = lines.join("\n");
        assert!(rendered.contains("1 · test/one · current"), "{rendered}");
        assert!(!rendered.contains("ctx"), "{rendered}");
    }

    #[tokio::test]
    async fn status_command_reports_context_usage() {
        let mut app = test_app_context();
        app.handle_agent_event(crate::core::Event::new(
            "sub-1",
            EventMsg::TurnUsage {
                input: 12_340,
                output: 660,
            },
        ));

        app.composer.set_text("/status".to_string());
        app.try_submit();
        assert!(
            matches!(app.view().history.last(), Some(HistoryCell::Notice(text))
                if text.contains("context: 13.0k/200.0k (6%)")
                    && text.contains("tokens: session in 12.3k / out 660")),
            "{:?}",
            app.view().history.last()
        );
    }

    #[tokio::test]
    async fn effort_command_sets_validates_and_clears() {
        let mut app = test_app();

        // The footer always shows the effort: `off` until one is set.
        let lines = render_lines(&mut app, 110, 20);
        assert!(lines.join("\n").contains("· effort off"), "{lines:?}");

        app.composer.set_text("/effort high".to_string());
        app.try_submit();
        assert_eq!(app.effort.as_deref(), Some("high"));
        assert!(
            matches!(app.view().history.last(), Some(HistoryCell::Notice(text)) if text.contains("high"))
        );

        // Levels are case-insensitive and normalize to lowercase.
        app.composer.set_text("/effort HIGH".to_string());
        app.try_submit();
        assert_eq!(app.effort.as_deref(), Some("high"));

        // An unknown level is an error cell; the current effort stays.
        app.composer.set_text("/effort bogus".to_string());
        app.try_submit();
        assert_eq!(app.effort.as_deref(), Some("high"));
        assert!(
            matches!(app.view().history.last(), Some(HistoryCell::Error(text)) if text.contains("unknown effort level"))
        );

        // `off` clears the override.
        app.composer.set_text("/effort off".to_string());
        app.try_submit();
        assert_eq!(app.effort, None);
        assert!(
            matches!(app.view().history.last(), Some(HistoryCell::Notice(text)) if text.contains("unset"))
        );
        let lines = render_lines(&mut app, 110, 20);
        assert!(lines.join("\n").contains("· effort off"), "{lines:?}");

        // The footer shows the level while one is set.
        app.composer.set_text("/effort medium".to_string());
        app.try_submit();
        let lines = render_lines(&mut app, 110, 20);
        assert!(lines.join("\n").contains("· effort medium"), "{lines:?}");
    }

    #[tokio::test]
    async fn effort_menu_opens_navigates_and_switches() {
        let mut app = test_app();

        app.composer.set_text("/effort".to_string());
        app.try_submit();
        let menu = app.effort_menu.as_ref().expect("picker open");
        // Four default levels plus `off`; nothing is set yet, so the
        // selection starts on the `off` entry.
        assert_eq!(menu.items.len(), 5);
        assert_eq!(menu.selected, 4);
        assert!(
            menu.items
                .last()
                .is_some_and(|item| item.current && item.level.is_none())
        );

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.effort_menu.is_none());
        assert_eq!(app.effort.as_deref(), Some("high"));
        assert!(
            matches!(app.view().history.last(), Some(HistoryCell::Notice(text)) if text.contains("high"))
        );

        // Reopening starts on the now-active level; confirming it only
        // closes the menu.
        app.composer.set_text("/effort".to_string());
        app.try_submit();
        assert_eq!(app.effort_menu.as_ref().unwrap().selected, 3);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.effort_menu.is_none());
        let notices = app
            .view()
            .history
            .iter()
            .filter(|cell| matches!(cell, HistoryCell::Notice(text) if text.contains("set to")))
            .count();
        assert_eq!(notices, 1);

        // Esc closes the picker without changing anything.
        app.composer.set_text("/effort".to_string());
        app.try_submit();
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.effort_menu.is_none());
        assert_eq!(app.effort.as_deref(), Some("high"));
    }

    #[tokio::test]
    async fn effort_menu_renders_levels_with_the_current_marker() {
        let mut app = test_app();
        app.composer.set_text("/effort high".to_string());
        app.try_submit();
        app.composer.set_text("/effort".to_string());
        app.try_submit();
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

        let lines = render_lines(&mut app, 80, 20);
        let rendered = lines.join("\n");
        assert!(rendered.contains("▸ 5 · off"), "{rendered}");
        assert!(rendered.contains("4 · high · current"), "{rendered}");
        assert!(rendered.contains("Enter switch"), "{rendered}");
    }

    #[tokio::test]
    async fn thinking_command_toggles_the_fold_flag() {
        let mut app = test_app();
        // Cards fold when settled; /thinking makes them expand by default.
        assert!(!app.show_thinking);

        app.composer.set_text("/thinking".to_string());
        app.try_submit();
        assert!(app.show_thinking);
        assert!(
            matches!(app.view().history.last(), Some(HistoryCell::Notice(text)) if text.contains("expanded"))
        );

        app.composer.set_text("/thinking".to_string());
        app.try_submit();
        assert!(!app.show_thinking);
        assert!(
            matches!(app.view().history.last(), Some(HistoryCell::Notice(text)) if text.contains("folded"))
        );
    }

    #[tokio::test]
    async fn ctrl_o_toggles_tool_output_only() {
        let mut app = test_app();
        assert!(!app.show_tool_detail);

        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(app.show_tool_detail);
        // Thinking has its own switch: Ctrl+O must not move it.
        assert!(!app.show_thinking);
        // A view toggle reports in the footer, never as a transcript row: the
        // message stream must not grow a line per keystroke.
        let lines = render_lines(&mut app, 80, 20);
        let footer = lines.last().expect("footer row").clone();
        assert!(footer.contains("tool output expanded"), "{footer}");
        assert!(
            !app.view()
                .history
                .iter()
                .any(|cell| matches!(cell, HistoryCell::Notice(text) if text.contains("output")))
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(!app.show_tool_detail);
        let lines = render_lines(&mut app, 80, 20);
        let footer = lines.last().expect("footer row").clone();
        assert!(footer.contains("tool output folded"), "{footer}");
    }

    #[tokio::test]
    async fn ctrl_t_toggles_thinking_only() {
        let mut app = test_app();
        assert!(!app.show_thinking);

        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(app.show_thinking);
        // Tool output keeps its own switch: Ctrl+T must not move it.
        assert!(!app.show_tool_detail);
        let lines = render_lines(&mut app, 80, 20);
        let footer = lines.last().expect("footer row").clone();
        assert!(footer.contains("thinking expanded"), "{footer}");
        assert!(
            !app.view()
                .history
                .iter()
                .any(|cell| matches!(cell, HistoryCell::Notice(text) if text.contains("thinking")))
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(!app.show_thinking);
        let lines = render_lines(&mut app, 80, 20);
        let footer = lines.last().expect("footer row").clone();
        assert!(footer.contains("thinking folded"), "{footer}");
    }

    #[tokio::test]
    async fn the_ctrl_o_flash_expires_and_leaves_the_hint_row() {
        let mut app = test_app();
        let before = app.view().history.len();
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        let lines = render_lines(&mut app, 80, 20);
        assert!(
            lines
                .last()
                .is_some_and(|row| row.contains("tool output expanded"))
        );

        // Past the deadline the footer goes back to its hints, and the
        // transcript gained nothing along the way.
        app.status = app
            .status
            .take()
            .map(|(text, _)| (text, Instant::now() - Duration::from_millis(1)));
        app.on_tick();
        assert!(app.status.is_none());
        let lines = render_lines(&mut app, 120, 20);
        let footer = lines.last().expect("footer row").clone();
        assert!(!footer.contains("expanded"), "{footer}");
        assert!(footer.contains("Ctrl+J newline"), "{footer}");
        assert_eq!(app.view().history.len(), before);
    }

    #[tokio::test]
    async fn thinking_deltas_commit_above_the_answer() {
        let mut app = test_app();
        let event = |msg| crate::core::Event::new("sub-1", msg);

        app.handle_agent_event(event(EventMsg::TurnStarted));
        app.handle_agent_event(event(EventMsg::AgentThinkingDelta {
            delta: "pondering".to_string(),
        }));
        assert_eq!(app.view().live_thinking.as_deref(), Some("pondering"));
        assert!(app.thinking_started.is_some(), "the stream is timed");
        assert!(
            app.view()
                .history
                .iter()
                .all(|cell| !matches!(cell, HistoryCell::Thinking { .. })),
            "nothing commits while only thinking has streamed"
        );

        // The first visible delta commits the thinking cell above it.
        app.handle_agent_event(event(EventMsg::AgentMessageDelta {
            delta: "answer".to_string(),
        }));
        assert_eq!(app.view().live_thinking, None);
        assert!(app.thinking_started.is_none(), "the clock rode along");
        assert!(
            matches!(&app.view().history[3], HistoryCell::Thinking { text, .. } if text == "pondering"),
            "{:?}",
            app.view().history
        );
        assert_eq!(app.view().live.as_deref(), Some("answer"));

        app.handle_agent_event(event(EventMsg::AgentMessage {
            message: "answer".to_string(),
        }));
        app.handle_agent_event(event(EventMsg::TurnComplete));
        assert!(
            matches!(&app.view().history[4], HistoryCell::Assistant(text) if text == "answer"),
            "{:?}",
            app.view().history
        );
    }

    #[tokio::test]
    async fn tool_call_cards_run_and_settle() {
        let mut app = test_app();
        let event = |msg| crate::core::Event::new("sub-1", msg);

        app.handle_agent_event(event(EventMsg::TurnStarted));
        app.handle_agent_event(event(EventMsg::AgentToolCallStarted {
            name: "bash".to_string(),
            arguments: r#"{"command": "ls"}"#.to_string(),
        }));
        let running = app.view().running_tool.as_ref().expect("live card");
        assert_eq!(running.name, "bash");
        assert!(
            app.view()
                .history
                .iter()
                .all(|cell| !matches!(cell, HistoryCell::ToolCall { .. })),
            "the running card is not history yet"
        );

        // Completion settles the card into history with its measured time.
        app.handle_agent_event(event(EventMsg::AgentToolCall {
            name: "bash".to_string(),
            arguments: r#"{"command": "ls"}"#.to_string(),
            result: "file".to_string(),
            error: None,
            duration_ms: 4200,
        }));
        assert!(app.view().running_tool.is_none());
        assert!(
            matches!(
                app.view().history.last(),
                Some(HistoryCell::ToolCall { name, duration_ms: Some(4_200), .. }) if name == "bash"
            ),
            "{:?}",
            app.view().history.last()
        );

        app.handle_agent_event(event(EventMsg::TurnComplete));
        assert!(!app.busy);
    }

    #[tokio::test]
    async fn an_interrupted_tool_settles_as_an_error_card() {
        let mut app = test_app();
        let event = |msg| crate::core::Event::new("sub-1", msg);

        app.handle_agent_event(event(EventMsg::TurnStarted));
        app.handle_agent_event(event(EventMsg::AgentToolCallStarted {
            name: "bash".to_string(),
            arguments: r#"{"command": "sleep 99"}"#.to_string(),
        }));
        app.handle_agent_event(event(EventMsg::TurnAborted {
            reason: "esc".to_string(),
        }));
        assert!(app.view().running_tool.is_none());
        assert!(
            matches!(
                app.view().history.get(app.view().history.len() - 2),
                Some(HistoryCell::ToolCall { error: Some(reason), duration_ms: Some(_), .. }) if reason == "interrupted"
            ),
            "{:?}",
            app.view().history
        );
        assert!(
            matches!(app.view().history.last(), Some(HistoryCell::Interrupted)),
            "{:?}",
            app.view().history.last()
        );
    }

    /// An app whose view mirrors one completed user turn (the worker keeps
    /// running for real; its events just sit unread in the channel).
    fn app_with_transcript() -> App {
        let mut app = test_app();
        app.composer.set_text("hello".to_string());
        app.try_submit();
        app.handle_agent_event(crate::core::Event::new("sub-1", EventMsg::TurnComplete));
        app
    }

    /// Collect from the real worker until a `ConversationSnapshot` arrives —
    /// the confirmation every context op answers with.
    async fn next_snapshot(app: &mut App) -> EventMsg {
        loop {
            let event = app.session.next_event().await.expect("worker event");
            if let EventMsg::ConversationSnapshot { .. } = &event.msg {
                return event.msg;
            }
        }
    }

    #[tokio::test]
    async fn context_prints_the_report_before_the_first_message() {
        let mut app = test_app();
        app.composer.set_text("/context".to_string());
        app.try_submit();

        assert!(app.context_view.is_none(), "no viewer without messages");
        assert!(
            matches!(app.view().history.last(), Some(HistoryCell::Report(block)) if block.title == "context"),
            "{:?}",
            app.view().history.last()
        );
        assert!(app.view().park_on_last_report);
    }

    #[tokio::test]
    async fn context_opens_the_viewer_once_messages_exist() {
        let mut app = app_with_transcript();
        app.composer.set_text("/context".to_string());
        app.try_submit();

        assert!(app.context_view.is_some(), "messages exist, so the viewer");
        // The worker answers the inspect op with the real transcript.
        let snapshot = next_snapshot(&mut app).await;
        app.handle_agent_event(crate::core::Event::new("sub-c", snapshot));
        assert_eq!(
            app.context_view.as_ref().unwrap().entries_len(),
            Some(3),
            "system + user + the echo model's reply"
        );

        // Rendering shows the viewer, not the chat frame: the banner and the
        // normal footer hints are gone, replaced by the viewer's own.
        let lines = render_lines(&mut app, 80, 24);
        let rendered = lines.join("\n");
        assert!(rendered.contains("Context"), "{rendered}");
        assert!(rendered.contains("u undo"), "{rendered}");
        assert!(!rendered.contains("distilled from codex"), "{rendered}");
        assert!(!rendered.contains("Ctrl+C twice"), "{rendered}");
    }

    #[tokio::test]
    async fn delete_in_the_viewer_round_trips_through_the_worker() {
        let mut app = app_with_transcript();
        app.composer.set_text("/context".to_string());
        app.try_submit();
        let snapshot = next_snapshot(&mut app).await;
        app.handle_agent_event(crate::core::Event::new("sub-c", snapshot));

        // Select the user entry and confirm the delete.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

        let snapshot = next_snapshot(&mut app).await;
        let EventMsg::ConversationSnapshot { messages } = &snapshot else {
            panic!("expected a snapshot, got {snapshot:?}");
        };
        assert_eq!(messages.len(), 2, "system + the surviving assistant");
        app.handle_agent_event(crate::core::Event::new("sub-c", snapshot));
        assert_eq!(app.context_view.as_ref().unwrap().entries_len(), Some(2));
    }

    #[tokio::test]
    async fn edit_in_the_viewer_round_trips_through_the_worker() {
        let mut app = app_with_transcript();
        app.composer.set_text("/context".to_string());
        app.try_submit();
        let snapshot = next_snapshot(&mut app).await;
        app.handle_agent_event(crate::core::Event::new("sub-c", snapshot));

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let snapshot = next_snapshot(&mut app).await;
        let EventMsg::ConversationSnapshot { messages } = &snapshot else {
            panic!("expected a snapshot, got {snapshot:?}");
        };
        assert_eq!(messages[1].content, "hello!", "the edit landed");
        assert_eq!(messages[1].role, crate::core::Role::User);
    }

    #[tokio::test]
    async fn undo_in_the_viewer_restores_a_deleted_entry() {
        let mut app = app_with_transcript();
        app.composer.set_text("/context".to_string());
        app.try_submit();
        let snapshot = next_snapshot(&mut app).await;
        app.handle_agent_event(crate::core::Event::new("sub-c", snapshot));

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        let snapshot = next_snapshot(&mut app).await;
        app.handle_agent_event(crate::core::Event::new("sub-c", snapshot));

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
        let snapshot = next_snapshot(&mut app).await;
        let EventMsg::ConversationSnapshot { messages } = &snapshot else {
            panic!("expected a snapshot, got {snapshot:?}");
        };
        assert_eq!(messages.len(), 3, "the user message came back");
        assert_eq!(messages[1].content, "hello");
    }

    #[tokio::test]
    async fn a_rejected_op_reports_inside_the_viewer() {
        let mut app = app_with_transcript();
        app.composer.set_text("/context".to_string());
        app.try_submit();
        let snapshot = next_snapshot(&mut app).await;
        app.handle_agent_event(crate::core::Event::new("sub-c", snapshot));

        let cells_before = app.view().history.len();
        app.handle_agent_event(crate::core::Event::new(
            "sub-c",
            EventMsg::Error {
                message: "a tool result has no tool-call message before it".to_string(),
            },
        ));
        assert!(
            app.context_view.as_ref().unwrap().has_error(),
            "the viewer shows the rejection"
        );
        assert_eq!(
            app.view().history.len(),
            cells_before,
            "no chat error cell while the viewer is open"
        );
    }

    #[tokio::test]
    async fn esc_and_clear_close_the_viewer() {
        let mut app = app_with_transcript();
        app.composer.set_text("/context".to_string());
        app.try_submit();
        assert!(app.context_view.is_some());

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.context_view.is_none());

        // Reopen, then a `/clear` confirmation closes it too: the context it
        // showed is gone.
        app.composer.set_text("/context".to_string());
        app.try_submit();
        assert!(app.context_view.is_some());
        app.handle_agent_event(crate::core::Event::new(
            "sub-c",
            EventMsg::ConversationCleared,
        ));
        assert!(app.context_view.is_none());
    }

    #[tokio::test]
    async fn a_thinking_only_turn_still_commits_the_cell() {
        let mut app = test_app();
        let event = |msg| crate::core::Event::new("sub-1", msg);

        app.handle_agent_event(event(EventMsg::TurnStarted));
        app.handle_agent_event(event(EventMsg::AgentThinkingDelta {
            delta: "silent reasoning".to_string(),
        }));
        app.handle_agent_event(event(EventMsg::TurnAborted {
            reason: "interrupted by user".to_string(),
        }));

        assert!(
            matches!(&app.view().history[3], HistoryCell::Thinking { text, .. } if text == "silent reasoning"),
            "{:?}",
            app.view().history
        );
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Interrupted)
        ));
        assert_eq!(app.view().live_thinking, None);
    }

    #[tokio::test]
    async fn thinking_cells_render_folded_and_expanded() {
        let mut app = test_app();
        app.view_mut().history.push(HistoryCell::Thinking {
            text: "long thoughts".to_string(),
            duration_ms: Some(4_200),
        });

        // Expanded (/thinking on): header over the dim body.
        app.show_thinking = true;
        let lines = render_lines(&mut app, 60, 20);
        let rendered = lines.join("\n");
        // `⚓` is a double-width cell, so the framebuffer pads it; ask for the
        // label and the chip, and check the marker separately.
        assert!(rendered.contains("thinking · 4.2s"), "{rendered}");
        assert!(lines.iter().any(|line| line.starts_with('⚓')), "{rendered}");
        assert!(rendered.contains("long thoughts"), "{rendered}");

        // Folded (the default): a single marker line naming the expand key.
        app.show_thinking = false;
        let lines = render_lines(&mut app, 60, 20);
        let rendered = lines.join("\n");
        assert!(
            rendered.contains("thinking · 4.2s (ctrl+t to expand)"),
            "{rendered}"
        );
        assert!(!rendered.contains("long thoughts"), "{rendered}");
    }

    #[tokio::test]
    async fn tool_cards_render_a_preview_and_expand_with_ctrl_o() {
        let mut app = test_app();
        app.view_mut().history.push(HistoryCell::ToolCall {
            name: "bash".to_string(),
            arguments: r#"{"command": "cargo build"}"#.to_string(),
            result: "a\nb\nc\nd\ne".to_string(),
            error: None,
            duration_ms: Some(4_200),
        });

        // Folded (the default): the head preview plus the fold hint.
        let lines = render_lines(&mut app, 80, 20);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("● bash(cargo build) · 4.2s")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|line| line.contains("⎿  a")), "{lines:?}");
        assert!(
            !lines.iter().any(|line| line.trim_end() == "    e"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("… +2 lines (ctrl+o to expand)")),
            "{lines:?}"
        );

        // Ctrl+O: the whole output, hint gone.
        app.show_tool_detail = true;
        let lines = render_lines(&mut app, 80, 20);
        assert!(
            lines.iter().any(|line| line.trim_end() == "    e"),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("ctrl+o to expand")),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn a_running_tool_renders_a_live_card() {
        let mut app = test_app();
        app.view_mut().running_tool = Some(crate::tui::session::RunningTool {
            name: "bash".to_string(),
            arguments: r#"{"command": "cargo build"}"#.to_string(),
            started_at: Instant::now(),
        });
        app.busy = true;

        let lines = render_lines(&mut app, 80, 20);
        let rendered = lines.join("\n");
        assert!(rendered.contains("bash(cargo build)"), "{rendered}");
        assert!(rendered.contains("Running… ("), "{rendered}");
    }

    #[tokio::test]
    async fn inline_live_tail_shows_the_running_tool_card() {
        let mut app = test_app();
        app.view_mut().running_tool = Some(crate::tui::session::RunningTool {
            name: "bash".to_string(),
            arguments: r#"{"command": "cargo build"}"#.to_string(),
            started_at: Instant::now(),
        });
        app.busy = true;

        let rows = app.inline_live_tail(80, 10);
        let text: Vec<String> = rows
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert_eq!(text.len(), 2, "header + elapsed row, not bottom-cropped");
        assert!(text[0].contains("bash(cargo build)"), "{text:?}");
        assert!(text[1].contains("Running… ("), "{text:?}");
    }

    #[tokio::test]
    async fn inline_flush_prints_thinking_cards_folded_by_default() {
        let mut app = test_app();
        app.view_mut().history.push(HistoryCell::Thinking {
            text: "deep reasoning".to_string(),
            duration_ms: Some(4_200),
        });

        // Scrollback is immutable, so the flush bakes in the current fold
        // state: folded by default, compact and permanent.
        let pending = app.take_inline_pending(60);
        let text: Vec<String> = pending
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert!(
            text.iter()
                .any(|row| row.contains("⚓ thinking · 4.2s (ctrl+t to expand)")),
            "{text:?}"
        );
        assert!(
            !text.iter().any(|row| row.contains("deep reasoning")),
            "{text:?}"
        );
    }

    #[tokio::test]
    async fn empty_route_prompts_instead_of_submitting_and_keeps_the_input() {
        let mut app = test_app_without_model();
        app.composer.set_text("hello there".to_string());
        app.try_submit();

        // The message never reached the worker (no busy) and the composer
        // keeps its content; the transcript carries the guidance instead.
        assert!(!app.busy);
        assert_eq!(app.composer.text(), "hello there");
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Error(text)) if text.contains("no model is configured"),
        ));

        // The submit effect is gated the same way.
        app.apply_effects(vec![PluginEffect::SubmitUserTurn("hi".to_string())]);
        assert!(!app.busy);

        // Once a model is actually active (event arrives), sending works.
        app.handle_agent_event(crate::core::Event::new(
            "sub-1",
            EventMsg::ModelSwitched {
                model: "test/one".to_string(),
            },
        ));
        app.try_submit();
        assert!(app.busy);
        assert_eq!(app.composer.text(), "");
    }

    #[tokio::test]
    async fn commands_still_work_on_an_empty_route() {
        let mut app = test_app_without_model();

        // Local commands are unaffected by the missing model…
        app.composer.set_text("/help".to_string());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.busy);
        assert!(app.composer.is_empty());
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Notice(text)) if text.contains("/clear"),
        ));

        // …including `/status`, which reports the empty backend label…
        app.composer.set_text("/status".to_string());
        app.try_submit();
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Notice(text)) if text.contains("backend: no model"),
        ));

        // …and `/model`, whose empty menu carries the same guidance.
        app.composer.set_text("/model".to_string());
        app.try_submit();
        assert!(app.model_menu.is_none());
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Error(text)) if text.contains("no model is configured"),
        ));

        // Unknown commands still report themselves as unknown, not as the
        // missing-model guidance.
        app.composer.set_text("/nope".to_string());
        app.try_submit();
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Error(text)) if text.contains("unknown command"),
        ));
    }

    #[tokio::test]
    async fn partial_credentials_prompt_for_the_missing_half() {
        let mut app = test_app_partial();

        // The route is armed, but sending names the missing half instead of
        // firing a doomed request; the composer keeps its content.
        app.composer.set_text("hello".to_string());
        app.try_submit();
        assert!(!app.busy);
        assert_eq!(app.composer.text(), "hello");
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Error(text))
                if text.contains("missing `api_key`") && text.contains("openai"),
        ));

        // The model menu lists the declared model (listing needs no key);
        // discovery would be gated with the same prompt.
        app.composer.set_text("/model".to_string());
        app.try_submit();
        assert!(app.model_menu.is_some());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    }

    #[tokio::test]
    async fn providers_without_a_selected_model_prompt_for_the_picker() {
        let mut app = test_app_unselected();
        app.composer.set_text("hello".to_string());
        app.try_submit();

        assert!(!app.busy);
        assert_eq!(app.composer.text(), "hello");
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Error(text)) if text.contains("open /model"),
        ));

        // `/model` on a discovery-pending provider kicks discovery off and
        // says so, instead of failing.
        app.composer.set_text("/model".to_string());
        app.try_submit();
        assert!(app.model_menu.is_none());
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Notice(text)) if text.contains("discovering models from openai"),
        ));
    }

    #[tokio::test]
    async fn slash_opens_a_filtered_command_menu() {
        let mut app = test_app();

        app.composer.set_text("/".to_string());
        assert_eq!(
            app.command_matches().len(),
            20,
            "sixteen commands plus four aliases"
        );

        app.composer.set_text("/cl".to_string());
        let matches = app.command_matches();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "clear");

        app.composer.set_text("/se".to_string());
        let matches = app.command_matches();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "sessions");

        app.composer.set_text("/re".to_string());
        let matches = app.command_matches();
        assert_eq!(
            matches.len(),
            2,
            "`/re` offers the /rename and /resume aliases"
        );
        // Aliases are candidates in their own right and complete to themselves.
        assert_eq!(matches[0].name, "rename");
        assert_eq!(matches[1].name, "resume");
        assert_eq!(matches[1].spec.name, "sessions");

        app.composer.set_text("/help me".to_string());
        assert!(app.command_matches().is_empty());

        app.composer.set_text("/".to_string());
        app.busy = true;
        assert!(app.command_matches().is_empty(), "no menu while streaming");
    }

    #[tokio::test]
    async fn arrows_and_tab_navigate_and_complete_the_menu() {
        let mut app = test_app();
        app.composer.set_text("/".to_string());

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.command_selection, 1);
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        // The candidate under the selection is `/help`'s alias, and it
        // completes to the alias itself.
        assert_eq!(app.composer.text(), "/h");

        app.composer.set_text("/".to_string());
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.command_selection, 19);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.command_selection, 18);
    }

    #[tokio::test]
    async fn enter_runs_the_selected_command() {
        let mut app = test_app();
        app.composer.set_text("/he".to_string());

        app.on_enter();

        assert!(!app.busy);
        assert!(app.composer.is_empty());
        match app.view().history.last() {
            Some(HistoryCell::Notice(text)) => assert!(text.contains("/clear")),
            other => panic!("expected the help notice, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn escape_dismisses_the_menu_without_clearing_input() {
        let mut app = test_app();
        app.composer.set_text("/".to_string());
        assert!(app.command_menu_open());

        app.on_escape();

        assert!(!app.command_menu_open());
        assert_eq!(app.composer.text(), "/");
        assert!(!app.quit);
    }

    #[tokio::test]
    async fn new_session_archives_the_current_transcript() {
        let mut app = test_app();
        app.view_mut()
            .history
            .push(HistoryCell::User("earlier chat".to_string()));

        app.composer.set_text("/new".to_string());
        app.try_submit();

        assert_eq!(app.views.len(), 2);
        assert_eq!(app.active_view, 1);
        assert!(!app.busy);
        assert!(
            matches!(app.view().history.first(), Some(HistoryCell::Notice(text)) if text == "New session started"),
            "{:?}",
            app.view().history
        );
        assert!(matches!(
            app.views[0].history.last(),
            Some(HistoryCell::User(text)) if text == "earlier chat"
        ));

        // `/resume` opens the same picker under its alias.
        app.composer.set_text("/resume".to_string());
        app.try_submit();
        assert_eq!(app.session_menu, Some(1));
    }

    #[tokio::test]
    async fn session_menu_switches_back_and_forth() {
        let mut app = test_app();
        app.view_mut()
            .history
            .push(HistoryCell::User("old talk".to_string()));
        app.composer.set_text("/new".to_string());
        app.try_submit();

        app.composer.set_text("/sessions".to_string());
        app.try_submit();
        assert_eq!(app.session_menu, Some(1));

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.session_menu, Some(0));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.session_menu.is_none());
        assert_eq!(app.active_view, 0);
        assert!(matches!(
            app.view().history.last(),
            Some(HistoryCell::Notice(text)) if text == "Session resumed"
        ));

        // Confirming on the already-active session only closes the menu.
        app.composer.set_text("/sessions".to_string());
        app.try_submit();
        assert_eq!(app.session_menu, Some(0));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.session_menu.is_none());
        assert_eq!(app.active_view, 0);
        let resumed = app
            .view()
            .history
            .iter()
            .filter(|cell| matches!(cell, HistoryCell::Notice(text) if text.contains("resumed")))
            .count();
        assert_eq!(resumed, 1);
    }

    #[tokio::test]
    async fn session_commands_are_noops_while_busy() {
        let mut app = test_app();
        app.busy = true;

        app.composer.set_text("/new".to_string());
        app.try_submit();
        assert_eq!(app.views.len(), 1);

        app.composer.set_text("/sessions".to_string());
        app.try_submit();
        assert!(app.session_menu.is_none());
    }

    #[tokio::test]
    async fn session_menu_renders_rows_with_the_current_marker() {
        let mut app = test_app();
        app.view_mut()
            .history
            .push(HistoryCell::User("old talk".to_string()));
        app.composer.set_text("/new".to_string());
        app.try_submit();
        app.composer.set_text("/sessions".to_string());
        app.try_submit();
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        let lines = render_lines(&mut app, 80, 20);
        let rendered = lines.join("\n");
        assert!(rendered.contains("▸ 1"), "{rendered}");
        assert!(rendered.contains("old talk"), "{rendered}");
        assert!(rendered.contains("current"), "{rendered}");
    }

    #[tokio::test]
    async fn rename_names_the_session_in_the_menu() {
        let mut app = test_app();
        app.composer.set_text("/rename my chat".to_string());
        app.try_submit();

        assert_eq!(app.view().name.as_deref(), Some("my chat"));
        match app.view().history.last() {
            Some(HistoryCell::Notice(text)) => assert!(text.contains("my chat")),
            other => panic!("expected a rename notice, got {other:?}"),
        }

        // The menu shows the explicit name for session 1 and the index-based
        // fallback for a session without a name or user message.
        app.composer.set_text("/new".to_string());
        app.try_submit();
        app.composer.set_text("/sessions".to_string());
        app.try_submit();
        let lines = render_lines(&mut app, 80, 20);
        let rendered = lines.join("\n");
        assert!(rendered.contains("my chat"), "{rendered}");
        assert!(rendered.contains("session 2"), "{rendered}");

        // `/rename` needs a name; the failed attempt changes nothing.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.composer.set_text("/rename".to_string());
        app.try_submit();
        match app.view().history.last() {
            Some(HistoryCell::Error(text)) => assert!(text.contains("needs arguments")),
            other => panic!("expected an error cell, got {other:?}"),
        }
        assert_eq!(app.view().name.as_deref(), None);
    }

    #[test]
    fn command_menu_window_keeps_the_selection_visible() {
        assert_eq!(command_menu_window(3, 0, 6), 0..3);
        assert_eq!(command_menu_window(10, 9, 3), 7..10);
        assert_eq!(command_menu_window(0, 0, 3), 0..0);
    }

    #[test]
    fn inline_pane_height_counts_live_and_footer() {
        // No live rows -> composer (1 + border) + footer.
        assert_eq!(inline_pane_height(24, 1, 0, 0), 3);
        // The tail grows the pane by exactly its rows.
        assert_eq!(inline_pane_height(24, 1, 1, 0), 4);
        assert_eq!(inline_pane_height(24, 1, 4, 0), 7);
        assert_eq!(inline_pane_height(24, 3, 1, 0), 6);
        assert_eq!(inline_pane_height(24, 1, 0, 3), 6);
    }

    #[test]
    fn inline_live_rows_leaves_room_for_the_fixed_rows() {
        // 24 rows - (composer 1 + border 1 + footer 1) = 21 spare rows.
        assert_eq!(inline_live_rows(24, 1, 0), 21);
        // The menu takes from the same budget.
        assert_eq!(inline_live_rows(24, 1, 5), 16);
        // Never collapses to zero, even in a tiny terminal.
        assert_eq!(inline_live_rows(3, 1, 0), 1);
    }

    #[test]
    fn inline_pane_height_caps_composer_rows() {
        assert_eq!(
            inline_pane_height(40, 20, 0, 0),
            MAX_COMPOSER_ROWS as u16 + 2
        );
    }

    #[test]
    fn inline_pane_height_never_exceeds_terminal() {
        assert_eq!(inline_pane_height(2, 8, 4, 0), 2);
        assert_eq!(inline_pane_height(0, 1, 0, 0), 1);
        assert_eq!(inline_pane_height(3, 1, 0, 6), 3);
    }

    #[test]
    fn inline_rect_is_pinned_to_the_bottom() {
        assert_eq!(inline_rect(100, 24, 4), Rect::new(0, 20, 100, 4));
        assert_eq!(inline_rect(100, 3, 9), Rect::new(0, 0, 100, 3));
    }

    #[test]
    fn ansi_foreground_covers_basic_and_extended_colors() {
        assert_eq!(ansi_foreground(Color::Cyan), "36");
        assert_eq!(ansi_foreground(Color::DarkGray), "90");
        assert_eq!(ansi_foreground(Color::Indexed(123)), "38;5;123");
        assert_eq!(ansi_foreground(Color::Rgb(1, 2, 3)), "38;2;1;2;3");
    }

    // -----------------------------------------------------------------------
    // `/provider` wizard, wired through the app
    // -----------------------------------------------------------------------

    /// A plugin that contributes one sign-in entry and nothing else — the
    /// smallest thing the wizard can recognise as "a provider I can sign in to".
    struct SignInPlugin;

    const FAKE_PAT: cst_plugin_api::AuthMethod = cst_plugin_api::AuthMethod {
        id: "pat",
        label: "Paste a token",
        kind: cst_plugin_api::AuthMethodKind::SecretPrompt,
    };
    static FAKE_METHODS: [cst_plugin_api::AuthMethod; 1] = [FAKE_PAT];
    static FAKE_AUTHS: [cst_plugin_api::AuthProvider; 1] = [cst_plugin_api::AuthProvider {
        id: "fake",
        protocol: "test",
        label: "Fake",
        default_base_url: Some("https://fake.example/"),
        methods: &FAKE_METHODS,
    }];

    impl Plugin for SignInPlugin {
        fn id(&self) -> &'static str {
            "fake-sign-in"
        }

        fn auth_providers(&self) -> &'static [cst_plugin_api::AuthProvider] {
            &FAKE_AUTHS
        }

        fn auth_status(&self, _instance: &str) -> cst_plugin_api::AuthStatus {
            cst_plugin_api::AuthStatus::SignedOut
        }
    }

    static SIGN_IN_PLUGIN: SignInPlugin = SignInPlugin;

    static SIGN_IN_PLUGINS: &[&dyn Plugin] = &[&SIGN_IN_PLUGIN];

    fn test_host_with_sign_in() -> Arc<PluginHost> {
        Arc::new(PluginHost::new(SIGN_IN_PLUGINS).expect("plugin host"))
    }

    #[tokio::test]
    async fn provider_command_opens_the_wizard_and_hides_the_command_strip() {
        let mut app = test_app();
        app.composer.set_text("/provider".to_string());
        app.on_enter();

        let wizard = app.provider_wizard.as_ref().expect("wizard open");
        assert_eq!(wizard.step, crate::tui::provider_wizard::Step::Action);
        assert!(app.composer.is_empty());
        // The wizard owns the strip: the slash menu must not render over it.
        app.composer.set_text("/".to_string());
        assert!(app.command_matches().is_empty());
    }

    #[tokio::test]
    async fn provider_wizard_targets_pair_instances_with_plugin_protocols() {
        let app = test_app_with_plugins(test_host_with_sign_in(), test_pool());
        // The pool's `test` instance uses the protocol the fake plugin serves.
        let targets = app.sign_in_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].instance, "test");
        assert!(targets[0].configured);
        assert_eq!(targets[0].provider.label, "Fake");
        assert_eq!(targets[0].status, cst_plugin_api::AuthStatus::SignedOut);
    }

    #[tokio::test]
    async fn provider_wizard_offers_a_route_for_an_unconfigured_protocol() {
        // A plugin whose protocol no instance uses yet gets a create-on-login
        // row instead.
        struct Other;
        static AUTHS: [cst_plugin_api::AuthProvider; 1] = [cst_plugin_api::AuthProvider {
            id: "other",
            protocol: "not-configured",
            label: "Other",
            default_base_url: None,
            methods: &FAKE_METHODS,
        }];
        impl Plugin for Other {
            fn id(&self) -> &'static str {
                "other"
            }
            fn auth_providers(&self) -> &'static [cst_plugin_api::AuthProvider] {
                &AUTHS
            }
        }
        static OTHER: Other = Other;
        static OTHERS: &[&dyn Plugin] = &[&OTHER];
        let host = Arc::new(PluginHost::new(OTHERS).expect("host"));
        let app = test_app_with_plugins(host, test_pool());

        let targets = app.sign_in_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].instance, "other");
        assert!(!targets[0].configured, "success creates the instance");
    }

    #[tokio::test]
    async fn provider_wizard_typed_steps_advance_without_submitting_a_turn() {
        let mut app = test_app();
        app.composer.set_text("/provider".to_string());
        app.on_enter();

        // Row 0 = add an OpenAI-compatible provider.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.provider_wizard.as_ref().expect("open").step,
            crate::tui::provider_wizard::Step::AddName
        );

        // Typing goes to the composer, and Enter advances instead of sending.
        for ch in "gateway".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(app.composer.text(), "gateway");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.provider_wizard.as_ref().expect("open").step,
            crate::tui::provider_wizard::Step::AddBaseUrl
        );
        assert!(app.composer.is_empty(), "the field is consumed");
        assert!(!app.busy, "no turn was submitted");

        // Esc walks back rather than closing outright.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            app.provider_wizard.as_ref().expect("still open").step,
            crate::tui::provider_wizard::Step::AddName
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            app.provider_wizard.is_none(),
            "Esc from the first step closes"
        );
    }

    #[tokio::test]
    async fn provider_wizard_reports_a_duplicate_route_name() {
        let mut app = test_app();
        app.open_provider_wizard();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        for ch in "test".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let wizard = app.provider_wizard.as_ref().expect("open");
        assert_eq!(
            wizard.step,
            crate::tui::provider_wizard::Step::AddName,
            "the step stays put"
        );
        assert!(
            wizard
                .status
                .as_deref()
                .is_some_and(|status| status.contains("already configured")),
            "the duplicate is explained: {:?}",
            wizard.status
        );
    }

    /// The write-back path: a confirmed draft reaches both the file and the
    /// live pool, and the rewrite keeps the providers it did not touch.
    #[tokio::test]
    async fn provider_wizard_writes_and_installs_a_new_endpoint() {
        let dir = std::env::temp_dir().join(format!("codest-provider-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: this test owns CODEST_HOME for its duration; the provider
        // tests in other modules use their own temporary paths and never read
        // this variable.
        unsafe { std::env::set_var("CODEST_HOME", &dir) };

        let pool = test_pool();
        let session = AgentSession::spawn_with_pool(
            pool.clone(),
            Some(&Selection::new("test", "one")),
            DEFAULT_SYSTEM_PROMPT,
            SessionBootstrap::default(),
        )
        .expect("initial selection resolves");
        let (notice_tx, notice_rx) = tokio::sync::mpsc::unbounded_channel();
        let (provider_tx, provider_tasks) = tokio::sync::mpsc::unbounded_channel();
        let (_plugin_effect_tx, plugin_effects) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            session,
            pool,
            notice_tx,
            notice_rx,
            provider_tx,
            provider_tasks,
            plugin_effects,
            "test/one".to_string(),
            TuiMode::Fullscreen,
            Arc::new(PluginHost::shipped().expect("shipped plugins")),
            Vec::new(),
            vec![SessionView::initial("test/one", TuiMode::Fullscreen)],
            0,
            SessionBootstrap::empty_profiles(),
            None,
            None,
            StatusBarConfig::default(),
            Vec::new(),
        );
        app.provider_wizard = Some(crate::tui::provider_wizard::Wizard {
            step: crate::tui::provider_wizard::Step::AddConfirm,
            add: crate::tui::provider_wizard::AddDraft {
                name: "local".to_string(),
                base_url: "http://127.0.0.1:8080/v1".to_string(),
                ..Default::default()
            },
            ..crate::tui::provider_wizard::Wizard::new(Vec::new())
        });
        app.confirm_provider_step();

        assert!(
            app.provider_wizard.is_none(),
            "the wizard closes on success"
        );
        assert!(
            app.pool.registry().instance("local").is_some(),
            "the pool sees the new provider immediately"
        );
        let saved = crate::core::config::load_providers(&crate::core::config::providers_path())
            .expect("read back");
        assert!(saved.iter().any(|instance| instance.name == "local"));
        // The pre-existing provider survived the rewrite.
        assert!(saved.iter().any(|instance| instance.name == "test"));

        let _ = std::fs::remove_dir_all(&dir);
        unsafe { std::env::remove_var("CODEST_HOME") };
    }
}
