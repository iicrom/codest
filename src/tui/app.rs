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
use crate::core::rollout;
use crate::core::rollout::LoadedSession;
use crate::plugins::PluginEffect;
use crate::plugins::PluginHost;
use crate::plugins::PluginInfo;
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
const MAX_COMMAND_MENU_ROWS: usize = 6;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const WHEEL_LINES: usize = 3;
const ALT_WHEEL_LINES: usize = 15;

/// Run the TUI until the user quits or the agent worker stops.
///
/// `plugins` carries the command table (and the provider factories already
/// merged into `pool`'s registry); `selection` is the model this run starts
/// with, or `None` when nothing is configured — the app then opens with an
/// empty route and guides the user on the first send. Providers with models
/// discovery enabled are refreshed in the background, best-effort: results
/// arrive as transcript notices.
pub async fn run(
    plugins: PluginHost,
    pool: Arc<ModelPool>,
    selection: Option<Selection>,
    system_prompt: Option<String>,
    mode: TuiMode,
    restore: RestoreMode,
) -> Result<()> {
    // Created before setup so a partial setup still restores the terminal.
    let _guard = TerminalGuard { mode };
    let mut terminal = Some(setup_terminal(mode)?);

    let base_prompt = system_prompt.unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
    let system_prompt = plugins.system_prompt(&base_prompt);

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

    let session = AgentSession::spawn_with_pool(
        pool.clone(),
        selection.as_ref(),
        system_prompt.clone(),
        SessionBootstrap {
            cwd: workspace.to_string_lossy().into_owned(),
            rollout_dir: Some(sessions_dir),
            loaded: loaded.clone(),
            fresh,
        },
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;

    let (notice_tx, notice_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
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
        model_label,
        mode,
        plugins,
        views,
        active_view,
    );
    if restore == RestoreMode::Picker {
        app.open_session_menu();
    }
    let effects = app.plugins.startup(app.plugin_info());
    app.apply_effects(effects);

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
            let height = inline_pane_height(rows, 1, false, 0);
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

/// Height of the inline bottom pane: optional live assistant tail, optional
/// command menu, composer block (top border + visible rows), and the footer.
fn inline_pane_height(rows: u16, composer_rows: usize, has_live: bool, menu_rows: usize) -> u16 {
    let composer = composer_rows.clamp(1, MAX_COMPOSER_ROWS) as u16 + 1;
    let live = u16::from(has_live);
    let menu = menu_rows as u16;
    (composer + live + menu + 1).min(rows.max(1))
}

struct App {
    session: AgentSession,
    plugins: PluginHost,
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
    composer: Composer,
    /// Selected row in the slash-command menu.
    command_selection: usize,
    /// True after Esc dismissed the menu for the current input.
    command_menu_dismissed: bool,
    busy: bool,
    tick: usize,
    quit: bool,
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
    current: bool,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        session: AgentSession,
        pool: Arc<ModelPool>,
        notice_tx: tokio::sync::mpsc::UnboundedSender<String>,
        notices: tokio::sync::mpsc::UnboundedReceiver<String>,
        model_label: String,
        mode: TuiMode,
        plugins: PluginHost,
        views: Vec<SessionView>,
        active_view: usize,
    ) -> Self {
        let (inline_columns, inline_rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let inline_height = inline_pane_height(inline_rows, 1, false, 0);
        let inline_top = inline_rows.saturating_sub(inline_height);

        Self {
            session,
            plugins,
            pool,
            notice_tx,
            notices,
            notices_closed: false,
            route: Selection::parse(&model_label).ok(),
            model_label,
            mode,
            views,
            active_view,
            session_menu: None,
            model_menu: None,
            composer: Composer::new(),
            command_selection: 0,
            command_menu_dismissed: false,
            busy: false,
            tick: 0,
            quit: false,
            prompts: Vec::new(),
            prompt_index: None,
            draft: String::new(),
            last_ctrl_c: None,
            inline_top,
            inline_height,
            inline_rows,
            inline_columns,
        }
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
                maybe_notice = self.notices.recv(), if !self.notices_closed => {
                    match maybe_notice {
                        Some(text) => {
                            self.view_mut().history.push(HistoryCell::Notice(text));
                        }
                        None => self.notices_closed = true,
                    }
                }
                _ = ticker.tick() => self.on_tick(),
            }
        }

        self.session.shutdown();
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
        let width = size.width as usize;
        let rows = size.height;

        let composer_width = width.saturating_sub(2).max(1);
        let composer_rows = self.composer.visual_rows(composer_width);
        let desired_rows = composer_rows.len().clamp(1, MAX_COMPOSER_ROWS);
        let has_live = self.view().live.is_some();
        let menu = self.menu_lines(MAX_COMMAND_MENU_ROWS);
        let menu_rows = menu.len();
        let pane_height = inline_pane_height(rows, desired_rows, has_live, menu_rows);
        let live_height = u16::from(has_live);
        let composer_height = desired_rows as u16 + 1;
        let menu_height =
            (menu_rows as u16).min(pane_height.saturating_sub(live_height + composer_height + 1));
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

        let live_tail = self.view().live.as_ref().map(|text| {
            chat::assistant_lines(text, width, self.busy)
                .pop()
                .unwrap_or_else(|| Line::from(""))
        });

        tui.draw(|frame| {
            self.render_inline_pane(frame, live_tail, &menu, menu_height, &composer_rows);
        })?;
        Ok(())
    }

    /// Collect transcript lines that have not reached the terminal scrollback
    /// yet, advancing the bookkeeping that prevents reprinting them. Only the
    /// active session's view is flushed; a background view catches up when it
    /// becomes active again.
    fn take_inline_pending(&mut self, width: usize) -> Vec<Line<'static>> {
        let view = self.view_mut();
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
            pending.extend(view.history[view.printed_cells].lines(width));
            pending.push(Line::from(""));
            view.printed_cells += 1;
        }

        // Stable lines of the in-flight assistant message. The last wrapped
        // line stays in the pane so it can keep growing without reprinting.
        if let Some(live) = &view.live {
            let lines = chat::assistant_lines(live, width, false);
            let stable = lines.len().saturating_sub(1);
            if stable > view.live_committed {
                pending.extend(lines[view.live_committed..stable].iter().cloned());
                view.live_committed = stable;
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

        // The `/sessions` and `/model` pickers are modal for navigation keys:
        // ↑↓ move, plain Enter confirms, Esc closes. Everything else (typing,
        // Ctrl+C, ...) still reaches its normal handler while a menu is open.
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

        match key.code {
            KeyCode::Char('c') if ctrl => self.on_ctrl_c(),
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
            _ => {}
        }
        let effects = self
            .plugins
            .dispatch_command(&invocation, self.plugin_info());
        self.apply_effects(effects);
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

    /// Start a user turn. Hooks run before the turn is queued so they cannot
    /// race the model request; `busy` is already true, so nested submissions
    /// are ignored.
    fn submit_user_turn(&mut self, text: String) {
        let view = self.view_mut();
        view.history.push(HistoryCell::User(text.clone()));
        view.live = Some(String::new());
        view.follow = true;
        self.busy = true;

        let effects = self.plugins.user_message(&text, self.plugin_info());
        self.apply_effects(effects);

        if !self.quit {
            self.session.submit_user_turn(text);
        }
    }

    fn plugin_info(&self) -> PluginInfo<'_> {
        PluginInfo {
            model: &self.model_label,
            mode: self.mode.label(),
            transcript_len: self.view().history.len(),
            plugin_ids: self.plugins.plugin_ids(),
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
            }
        }
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
        view.inline_final_text = None;
        view.live_committed = 0;
        view.printed_cells = 0;
        view.follow = true;
        view.scroll = 0;
        view.max_scroll = 0;
    }

    fn handle_agent_event(&mut self, event: crate::core::Event) {
        match event.msg {
            EventMsg::TurnStarted => {
                self.busy = true;
                if self.view().live.is_none() {
                    self.view_mut().live = Some(String::new());
                }
            }
            EventMsg::AgentMessageDelta { delta } => {
                self.view_mut()
                    .live
                    .get_or_insert_with(String::new)
                    .push_str(&delta);
            }
            EventMsg::AgentThinkingDelta { .. } => {
                // Reasoning output is not rendered by the minimal front-end;
                // the worker already kept the transcript clean of it.
            }
            EventMsg::TurnUsage { .. } => {
                // Surfaced for future status/context work; nothing to render.
            }
            EventMsg::ModelSwitched { model } => {
                // `/model` already updated the label optimistically; this is
                // the core's authoritative confirmation (also covering
                // non-TUI front-ends).
                self.route = Selection::parse(&model).ok();
                self.model_label = model;
            }
            EventMsg::AgentMessage { message } => {
                self.view_mut().live = Some(message);
            }
            EventMsg::TurnAborted { .. } => {
                self.finalize_live();
                self.view_mut().history.push(HistoryCell::Interrupted);
            }
            EventMsg::Error { message } => {
                self.finalize_live();
                self.view_mut().history.push(HistoryCell::Error(message));
            }
            EventMsg::TurnComplete => {
                let message = self.view().live.clone().unwrap_or_default();
                self.finalize_live();
                let effects = self.plugins.turn_complete(&message, self.plugin_info());
                self.busy = false;
                self.last_ctrl_c = None;
                self.apply_effects(effects);
            }
            EventMsg::ConversationCleared => {
                // The TUI already reset its transcript synchronously when
                // `/clear` ran; this confirms the core also dropped the
                // context and lets plugins observe the reset.
                let effects = self.plugins.conversation_cleared(self.plugin_info());
                self.apply_effects(effects);
            }
            EventMsg::SessionStarted { .. }
            | EventMsg::SessionSwitched { .. }
            | EventMsg::SessionRenamed { .. } => {
                // `/new`, the session picker, and `/rename` already applied
                // their view changes synchronously; these events confirm the
                // core (and the rollout file) did the same.
            }
            EventMsg::ShutdownComplete => {
                self.quit = true;
            }
        }
    }

    fn finalize_live(&mut self) {
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
        live_tail: Option<Line<'static>>,
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
        let live_height = u16::from(live_tail.is_some());
        let composer_height = visible_composer_rows as u16 + 1;
        let chunks = Layout::vertical([
            Constraint::Length(live_height),
            Constraint::Length(menu_height),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(area);

        if let Some(line) = live_tail {
            frame.render_widget(Paragraph::new(Text::from(vec![line])), chunks[0]);
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
    /// `/model`) when it is open, otherwise the slash-command candidates.
    /// Empty when neither is visible.
    fn menu_row_count(&self) -> usize {
        if let Some(menu) = self.model_menu.as_ref() {
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

        let busy = self.busy;
        let view = self.view_mut();

        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(""));

        for (index, cell) in view.history.iter().enumerate() {
            lines.extend(cell.lines(width));
            let has_more = index + 1 < view.history.len() || view.live.is_some();
            if has_more {
                lines.push(Line::from(""));
            }
        }

        if let Some(live) = &view.live {
            lines.extend(chat::assistant_lines(live, width, busy));
        }

        let height = area.height as usize;
        view.max_scroll = lines.len().saturating_sub(height);

        if view.follow {
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

        let spinner = SPINNER[self.tick % SPINNER.len()];
        let state = if self.busy {
            format!("{spinner} working · Esc interrupt")
        } else {
            "ready".to_string()
        };
        let hints = if self.session_menu.is_some() {
            "↑↓ select · Enter switch · Esc close"
        } else if self.model_menu.is_some() {
            "↑↓ select · Enter switch · r refresh · Esc close"
        } else if self.command_menu_open() {
            "↑↓ select · Tab complete · Enter run · Esc close"
        } else if self.show_ctrl_c_hint() {
            "Ctrl+C again to quit"
        } else if self.busy {
            "streaming…"
        } else if self.mode == TuiMode::Fullscreen {
            "Enter · / commands · Ctrl+J newline · wheel scroll"
        } else {
            "Enter · / commands · Ctrl+J newline · PgUp/PgDn scroll"
        };

        let hint_width = (display_width(hints) as u16 + 1).min(area.width);
        let parts =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(hint_width)]).split(area);

        let left = Line::from(vec![
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
            Span::styled("  │  ", Style::default().fg(theme::DIM)),
            Span::styled(
                state,
                Style::default().fg(if self.busy { theme::ACCENT } else { theme::DIM }),
            ),
        ]);
        frame.render_widget(Paragraph::new(left), parts[0]);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hints,
                Style::default().fg(theme::DIM),
            )))
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
/// `provider/model` selection, and a marker on the active model.
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
        Span::styled(
            truncated(&item.selection.as_str(), 48),
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
        App::new(
            session,
            pool,
            notice_tx,
            notice_rx,
            "test/one".to_string(),
            TuiMode::Fullscreen,
            PluginHost::builtin().expect("builtin plugins"),
            vec![SessionView::initial("test/one", TuiMode::Fullscreen)],
            0,
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
        App::new(
            session,
            pool,
            notice_tx,
            notice_rx,
            "openai/m1".to_string(),
            TuiMode::Fullscreen,
            PluginHost::builtin().expect("builtin plugins"),
            vec![SessionView::initial("openai/m1", TuiMode::Fullscreen)],
            0,
        )
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
        App::new(
            session,
            pool,
            notice_tx,
            notice_rx,
            "no model".to_string(),
            TuiMode::Fullscreen,
            PluginHost::builtin().expect("builtin plugins"),
            vec![SessionView::initial("no model", TuiMode::Fullscreen)],
            0,
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
        App::new(
            session,
            pool,
            notice_tx,
            notice_rx,
            "no model".to_string(),
            TuiMode::Fullscreen,
            PluginHost::builtin().expect("builtin plugins"),
            vec![SessionView::initial("no model", TuiMode::Fullscreen)],
            0,
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

    #[test]
    fn build_views_restores_sessions_and_banners_the_active_one() {
        use crate::core::Message;

        let loaded_session =
            |id: &str, messages: Vec<Message>, name: Option<String>| LoadedSession {
                meta: crate::core::SessionMeta {
                    version: 1,
                    id: id.to_string(),
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    cwd: "/tmp/demo".to_string(),
                    system_prompt: "be brief".to_string(),
                    name: None,
                    provider: None,
                    model: None,
                },
                path: PathBuf::from(format!("/tmp/{id}.jsonl")),
                messages,
                name,
            };

        let loaded = vec![
            loaded_session(
                "one",
                vec![Message::user("hello"), Message::assistant("hi")],
                None,
            ),
            loaded_session("named", vec![], Some("my chat".to_string())),
        ];
        let (views, active) = build_views("test-model", TuiMode::Fullscreen, &loaded, false);
        assert_eq!(views.len(), 2);
        assert_eq!(active, 1);
        // Messages replay into transcript cells, already counted as printed.
        assert_eq!(views[0].history.len(), 2);
        assert!(matches!(&views[0].history[0], HistoryCell::User(text) if text == "hello"));
        assert_eq!(views[0].printed_cells, 2);
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

    #[tokio::test]
    async fn command_menu_renders_above_the_composer() {
        let mut app = test_app();
        app.composer.set_text("/".to_string());

        // Wide enough that the alias hints are not clipped.
        let lines = render_lines(&mut app, 110, 20);
        let rendered = lines.join("\n");
        // Twelve names (eight commands plus four aliases) share a six-row
        // window; the first page shows /help with its aliases and the first
        // commands, aliases labeled with the command they point at.
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
        for _ in 0..10 {
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
            .filter(|cell| matches!(cell, HistoryCell::Notice(text) if text.contains("switched to")))
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

        let lines = render_lines(&mut app, 80, 20);
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
            12,
            "eight commands plus four aliases"
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
        assert_eq!(app.command_selection, 11);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.command_selection, 10);
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
        assert_eq!(inline_pane_height(24, 1, false, 0), 3);
        assert_eq!(inline_pane_height(24, 1, true, 0), 4);
        assert_eq!(inline_pane_height(24, 3, true, 0), 6);
        assert_eq!(inline_pane_height(24, 1, false, 3), 6);
    }

    #[test]
    fn inline_pane_height_caps_composer_rows() {
        assert_eq!(
            inline_pane_height(40, 20, false, 0),
            MAX_COMPOSER_ROWS as u16 + 2
        );
    }

    #[test]
    fn inline_pane_height_never_exceeds_terminal() {
        assert_eq!(inline_pane_height(2, 8, true, 0), 2);
        assert_eq!(inline_pane_height(0, 1, false, 0), 1);
        assert_eq!(inline_pane_height(3, 1, false, 6), 3);
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
}

