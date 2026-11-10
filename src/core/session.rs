//! The agent worker: owns the conversation, consumes submissions, emits events.
//!
//! This is the minimal analogue of codex's `CodexConversation`: the front-end
//! gets a handle with `submit`/`next_event` and has no access to model code.
//! A `CancellationToken` is installed for the in-flight turn so `interrupt`
//! works without racing the submission queue.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::core::config::ProfileSet;
use crate::core::config::ModelInfo;
use crate::core::reasoning::normalize_effort;
use crate::core::conversation::Conversation;
use crate::core::conversation::DEFAULT_SYSTEM_PROMPT;
use crate::core::model::ModelClient;
use crate::core::model::ModelStream;
use crate::core::model::StreamItem;
use crate::core::model::TurnOptions;
use crate::core::protocol::Event;
use crate::core::protocol::EventMsg;
use crate::core::protocol::Message;
use crate::core::protocol::Op;
use crate::core::protocol::Submission;
use crate::core::providers::ModelPool;
use crate::core::providers::Selection;
use crate::core::rollout::LoadedSession;
use crate::core::rollout::RolloutFile;
use crate::core::rollout::RolloutItem;
use crate::core::rollout::RolloutWriter;
use crate::core::tools::MAX_TOOL_ROUNDS;
use crate::core::tools::ToolCall;
use crate::core::tools::ToolExecutor;
use crate::core::tools::ToolSpec;

/// Shared cancellation handle for the turn currently being streamed.
#[derive(Debug, Clone, Default)]
pub struct InterruptHandle {
    current: Arc<Mutex<Option<CancellationToken>>>,
}

impl InterruptHandle {
    fn install(&self, token: CancellationToken) {
        if let Ok(mut current) = self.current.lock() {
            *current = Some(token);
        }
    }

    fn clear(&self) {
        if let Ok(mut current) = self.current.lock() {
            *current = None;
        }
    }

    /// Cancel the active turn, if any. Safe to call at any time.
    pub fn interrupt(&self) {
        if let Ok(current) = self.current.lock()
            && let Some(token) = current.as_ref()
        {
            token.cancel();
        }
    }
}

/// Where a worker's model requests go.
///
/// [`ModelBackend::Fixed`] is the single-client shape tests and simple hosts
/// use; [`ModelBackend::Pooled`] is the multi-provider shape: a
/// [`ModelPool`] plus the currently selected `provider/model`, replaceable
/// between turns via `Op::SwitchModel`. [`ModelBackend::Empty`] is the
/// no-providers-configured shape: turns fail with guidance until a switch
/// migrates it to [`ModelBackend::Pooled`].
pub enum ModelBackend {
    Fixed(Arc<dyn ModelClient>),
    Pooled {
        pool: Arc<ModelPool>,
        current: Selection,
        client: Arc<dyn ModelClient>,
    },
    Empty { pool: Arc<ModelPool> },
}

impl ModelBackend {
    /// The client for the upcoming turn. Only meaningful for the backends
    /// that have one — callers guard `Empty` before reaching here.
    fn client(&self) -> &Arc<dyn ModelClient> {
        match self {
            ModelBackend::Fixed(client) => client,
            ModelBackend::Pooled { client, .. } => client,
            ModelBackend::Empty { .. } => unreachable!("Empty backend has no client; guard turns with has_client()"),
        }
    }

    /// The active selection, for rollout metadata; `None` on a fixed or
    /// empty backend.
    fn selection(&self) -> Option<&Selection> {
        match self {
            ModelBackend::Fixed(_) | ModelBackend::Empty { .. } => None,
            ModelBackend::Pooled { current, .. } => Some(current),
        }
    }

    /// The metadata of `selection` when the backend can look it up (the
    /// pooled backend merges declared and discovered models); `None` on a
    /// fixed or empty backend.
    fn model_info(&self, selection: &Selection) -> Option<ModelInfo> {
        match self {
            ModelBackend::Pooled { pool, .. } => pool.model_info(selection),
            ModelBackend::Fixed(_) | ModelBackend::Empty { .. } => None,
        }
    }

    /// Switch the pooled backend to another `provider/model`. The new client
    /// is built (or cache-hit) here, so a broken selection surfaces as this
    /// error instead of failing the next turn. A fixed backend cannot
    /// switch; an empty backend becomes pooled on the first successful one.
    fn switch(&mut self, selection: &str) -> Result<Selection, String> {
        match self {
            ModelBackend::Fixed(_) => Err("model switching is unavailable: \
                this session runs a fixed backend"
                .to_string()),
            ModelBackend::Pooled { pool, current, client } => {
                let parsed = Self::resolve(pool, selection)?;
                *current = parsed.clone();
                *client = Self::client_for(pool, &parsed)?;
                Ok(parsed)
            }
            ModelBackend::Empty { pool } => {
                let parsed = Self::resolve(pool, selection)?;
                let client = Self::client_for(pool, &parsed)?;
                *self = ModelBackend::Pooled {
                    pool: pool.clone(),
                    current: parsed.clone(),
                    client,
                };
                Ok(parsed)
            }
        }
    }

    fn resolve(pool: &ModelPool, selection: &str) -> Result<Selection, String> {
        pool.registry()
            .parse_selection(selection)
            .map_err(|error| error.to_string())
    }

    fn client_for(pool: &ModelPool, parsed: &Selection) -> Result<Arc<dyn ModelClient>, String> {
        pool.client(parsed).map_err(|error| error.to_string())
    }
}

/// Front-end handle to the agent worker.
pub struct AgentSession {
    tx: mpsc::UnboundedSender<Submission>,
    rx: mpsc::UnboundedReceiver<Event>,
    interrupt: InterruptHandle,
    next_id: AtomicU64,
}

impl AgentSession {
    /// Spawn the worker with the default system prompt.
    pub fn spawn(client: Arc<dyn ModelClient>) -> Self {
        Self::spawn_with_system_prompt(client, DEFAULT_SYSTEM_PROMPT)
    }

    pub fn spawn_with_system_prompt(
        client: Arc<dyn ModelClient>,
        system_prompt: impl Into<String>,
    ) -> Self {
        Self::spawn_with_bootstrap(client, system_prompt, SessionBootstrap::default())
    }

    /// Spawn the worker, optionally restoring saved sessions and persisting
    /// to a rollout directory. See [`SessionBootstrap`].
    pub fn spawn_with_bootstrap(
        client: Arc<dyn ModelClient>,
        system_prompt: impl Into<String>,
        bootstrap: SessionBootstrap,
    ) -> Self {
        Self::spawn_with_backend(
            ModelBackend::Fixed(client),
            system_prompt,
            bootstrap,
        )
    }

    /// Spawn the worker on a multi-provider pool, starting at `selection`
    /// (`"provider/model"`). `None` starts with no route at all (nothing
    /// configured): turns fail with guidance until a `/model` switch lands.
    /// Fails when a given selection cannot resolve to a client — callers
    /// should validate before offering it.
    pub fn spawn_with_pool(
        pool: Arc<ModelPool>,
        selection: Option<&Selection>,
        system_prompt: impl Into<String>,
        bootstrap: SessionBootstrap,
    ) -> Result<Self, String> {
        let backend = match selection {
            Some(selection) => ModelBackend::Pooled {
                client: pool.client(selection).map_err(|error| error.to_string())?,
                current: selection.clone(),
                pool,
            },
            None => ModelBackend::Empty { pool },
        };
        Ok(Self::spawn_with_backend(backend, system_prompt, bootstrap))
    }

    fn spawn_with_backend(
        backend: ModelBackend,
        system_prompt: impl Into<String>,
        bootstrap: SessionBootstrap,
    ) -> Self {
        let (tx, submission_rx) = mpsc::unbounded_channel();
        let (event_tx, rx) = mpsc::unbounded_channel();
        let interrupt = InterruptHandle::default();

        tokio::spawn(run_worker(
            backend,
            system_prompt.into(),
            submission_rx,
            event_tx,
            interrupt.clone(),
            bootstrap,
        ));

        Self {
            tx,
            rx,
            interrupt,
            next_id: AtomicU64::new(1),
        }
    }

    /// Queue a submission and return its id. The id is echoed on every event
    /// produced for that submission.
    pub fn submit(&self, op: Op) -> String {
        let id = format!("sub-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let _ = self.tx.send(Submission { id: id.clone(), op });
        id
    }

    pub fn submit_user_turn(&self, text: impl Into<String>) -> String {
        self.submit(Op::UserTurn { text: text.into() })
    }

    /// Drop the conversation history, keeping the system prompt.
    pub fn clear_conversation(&self) -> String {
        self.submit(Op::ClearConversation)
    }

    /// Cancel the in-flight turn. No-op when idle.
    pub fn interrupt(&self) {
        self.interrupt.interrupt();
    }

    /// Ask the worker to stop. `ShutdownComplete` is emitted once any active
    /// turn has been aborted.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Submission {
            id: "shutdown".to_string(),
            op: Op::Shutdown,
        });
    }

    /// Archive the active conversation and start a fresh session. The new
    /// session's index arrives as [`EventMsg::SessionStarted`].
    pub fn new_session(&self) -> String {
        self.submit(Op::NewSession)
    }

    /// Make a recorded session active again. `session` is the creation-order
    /// index echoed by [`EventMsg::SessionStarted`].
    pub fn switch_session(&self, session: usize) -> String {
        self.submit(Op::SwitchSession { session })
    }

    /// Name the session at `session` (see `/rename`). Recorded in the
    /// session's rollout so it survives restarts.
    pub fn rename_session(&self, session: usize, name: impl Into<String>) -> String {
        self.submit(Op::RenameSession {
            session,
            name: name.into(),
        })
    }

    /// Make `selection` (`"provider/model"`, see `/model`) the backend for
    /// the following turns. The result arrives as [`EventMsg::ModelSwitched`]
    /// or an error event; ops are FIFO, so a switch never lands mid-turn.
    pub fn switch_model(&self, selection: impl Into<String>) -> String {
        self.submit(Op::SwitchModel {
            selection: selection.into(),
        })
    }

    /// Apply the named profile's presets (`/profile <name>`): its declared
    /// selection and effort. The result arrives as
    /// [`EventMsg::ProfileSwitched`] (with the selection and effort changes
    /// riding along as their own events) or an error event; ops are FIFO, so
    /// a switch never lands mid-turn.
    pub fn switch_profile(&self, name: impl Into<String>) -> String {
        self.submit(Op::SwitchProfile { name: name.into() })
    }

    /// Set the reasoning effort sent with the following turns (`/effort`);
    /// `None` clears the override. The result arrives as
    /// [`EventMsg::ReasoningEffortSwitched`]; ops are FIFO, so it never lands
    /// mid-turn.
    pub fn set_reasoning_effort(&self, effort: Option<String>) -> String {
        self.submit(Op::SetReasoningEffort { effort })
    }

    /// Replace the active session's enabled-tool set (`/tools`): the whole
    /// set, not a delta. Enabled tools are declared to the model from the
    /// next turn on and are the only ones executed; unknown names are
    /// dropped. The result arrives as [`EventMsg::ToolsSwitched`]; ops are
    /// FIFO, so it never lands mid-turn.
    pub fn set_tools_enabled(&self, enabled: Vec<String>) -> String {
        self.submit(Op::SetToolsEnabled { enabled })
    }

    /// Read the active conversation (`/context` viewer). The result arrives
    /// as [`EventMsg::ConversationSnapshot`] — system prompt first, then the
    /// transcript.
    pub fn inspect_conversation(&self) -> String {
        self.submit(Op::InspectConversation)
    }

    /// Delete transcript messages (`/context` viewer). `indices` are
    /// transcript positions (the system prompt is not addressable); the
    /// caller expands tool-call rounds. The result arrives as a
    /// [`EventMsg::ConversationSnapshot`] or an error event.
    pub fn delete_messages(&self, indices: Vec<usize>) -> String {
        self.submit(Op::DeleteMessages { indices })
    }

    /// Replace a transcript message's content (`/context` viewer). The result
    /// arrives as a [`EventMsg::ConversationSnapshot`] or an error event.
    pub fn edit_message(&self, index: usize, content: impl Into<String>) -> String {
        self.submit(Op::EditMessage {
            index,
            content: content.into(),
        })
    }

    /// Undo the most recent `/context` delete or edit. The result arrives as
    /// a [`EventMsg::ConversationSnapshot`] or an error event.
    pub fn undo_context_edit(&self) -> String {
        self.submit(Op::UndoContextEdit)
    }

    /// Await the next event. `None` means the worker has stopped and the
    /// channel is closed.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}

struct ActiveTurn {
    id: String,
    stream: ModelStream,
    cancel: CancellationToken,
    assistant: String,
    /// Usage reported by the backend for this turn, surfaced with
    /// `TurnUsage` on completion.
    usage: Option<(u64, u64)>,
    /// The selection that serves this turn, recorded with the usage in the
    /// rollout. `None` on a fixed backend.
    model: Option<String>,
}

/// What the worker starts from: sessions restored from disk plus the rollout
/// directory to persist into. `rollout_dir: None` runs fully in memory (used
/// by tests); restore failures degrade to an empty list.
#[derive(Default)]
pub struct SessionBootstrap {
    /// Sessions restored from rollouts, oldest first.
    pub loaded: Vec<LoadedSession>,
    /// Directory for rollout files; `None` disables persistence.
    pub rollout_dir: Option<std::path::PathBuf>,
    /// Workspace recorded in new rollout metadata.
    pub cwd: String,
    /// Whether to also start one fresh session. Plain startup restores the
    /// saved sessions *and* opens a new one; the restore modes reuse the
    /// loaded sessions and only fall back to a fresh one when none exist.
    pub fresh: bool,
    /// Tools the plugins declared; offered to the model on every turn.
    pub tools: Vec<ToolSpec>,
    /// Executes tool calls; the plugin host in a normal run. `None` turns
    /// every call into an error result the model can react to.
    pub executor: Option<Arc<dyn ToolExecutor>>,
    /// The available profiles, for `Op::SwitchProfile` lookups. Empty means
    /// none are configured and every profile switch errors.
    pub profiles: Arc<ProfileSet>,
    /// The profile this run started with, recorded in new sessions' rollout
    /// metadata. Run-scoped: switching sessions does not restore it.
    pub profile: Option<String>,
    /// The startup effort preset (the active profile's `effort`), seeded
    /// before the first turn and over the model's `default_level`. `None`
    /// keeps the model-derived seeding.
    pub initial_effort: Option<String>,
}

impl SessionBootstrap {
    /// An empty profile table for hosts that have none (tests, simple runs).
    pub fn empty_profiles() -> Arc<ProfileSet> {
        Arc::new(ProfileSet::new())
    }
}

impl std::fmt::Debug for SessionBootstrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionBootstrap")
            .field("loaded", &self.loaded)
            .field("rollout_dir", &self.rollout_dir)
            .field("cwd", &self.cwd)
            .field("fresh", &self.fresh)
            .field("tools", &self.tools)
            .field("executor", &self.executor.is_some())
            .field("profiles", &self.profiles.len())
            .field("profile", &self.profile)
            .field("initial_effort", &self.initial_effort)
            .finish()
    }
}

/// Rollout state of one session. The append handle exists only once the
/// session has earned a file — a session that never received a user message
/// stays out of the rollout directory entirely, so idle `/new` sessions do
/// not litter it. Items recorded before the first user message (an early
/// `/rename`, say) are held back and flushed into the file in order.
struct RolloutSlot {
    file: Option<RolloutFile>,
    pending: Vec<RolloutItem>,
    /// Set when the rollout file cannot be created or reopened; the session
    /// then stays in memory for the rest of the run.
    disabled: bool,
}

impl RolloutSlot {
    fn inactive() -> Self {
        Self {
            file: None,
            pending: Vec::new(),
            disabled: false,
        }
    }

    fn orphaned() -> Self {
        Self {
            file: None,
            pending: Vec::new(),
            disabled: true,
        }
    }
}

/// All rollout slots plus the writer they lazily create files with. The
/// active model selection is recorded in a session's meta line, so a rollout
/// says which backend produced it.
struct Rollouts {
    writer: Option<RolloutWriter>,
    system_prompt: String,
    selection: Option<Selection>,
    /// The profile the run started with, recorded in new sessions' meta line.
    profile: Option<String>,
    slots: Vec<RolloutSlot>,
}

impl Rollouts {
    fn push_slot(&mut self) {
        self.slots.push(RolloutSlot::inactive());
    }

    fn append(&mut self, index: usize, item: RolloutItem) {
        let Some(writer) = self.writer.as_ref() else {
            return;
        };
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        if slot.disabled {
            return;
        }
        if slot.file.is_none() {
            if !matches!(item, RolloutItem::UserMessage { .. }) {
                slot.pending.push(item);
                return;
            }
            // First user input: the session earns its rollout file now, with
            // any buffered items flushed ahead of the message.
            let (provider, model) = match self.selection.as_ref() {
                Some(selection) => (
                    Some(selection.provider.as_str()),
                    Some(selection.model.as_str()),
                ),
                None => (None, None),
            };
            match writer.start_session(&self.system_prompt, provider, model, self.profile.as_deref()) {
                Ok(mut file) => {
                    for buffered in std::mem::take(&mut slot.pending) {
                        if let Err(error) = file.append(&buffered) {
                            eprintln!("codest: rollout append failed: {error}");
                        }
                    }
                    slot.file = Some(file);
                }
                Err(error) => {
                    eprintln!("codest: persistence disabled ({error}); session stays in memory");
                    slot.disabled = true;
                    slot.pending.clear();
                    return;
                }
            }
        }
        if let Some(file) = slot.file.as_mut()
            && let Err(error) = file.append(&item)
        {
            eprintln!("codest: rollout append failed: {error}");
        }
    }
}

/// Every declared tool enabled: the starting tool set of a session that has
/// never customized it with `/tools`.
fn default_enabled(tools: &[ToolSpec]) -> BTreeSet<String> {
    tools.iter().map(|spec| spec.name.to_string()).collect()
}

/// The tools actually declared to the model: the enabled subset of the
/// declared specs, in registration order. Disabled tools are withheld so the
/// model neither sees them nor (in the common case) calls them; the executor
/// gate is the backstop for a call the model hallucinated anyway.
fn declared_tools(tools: &[ToolSpec], enabled: &BTreeSet<String>) -> Vec<ToolSpec> {
    tools
        .iter()
        .filter(|spec| enabled.contains(spec.name))
        .copied()
        .collect()
}

async fn run_worker(
    backend: ModelBackend,
    system_prompt: String,
    mut rx: mpsc::UnboundedReceiver<Submission>,
    tx: mpsc::UnboundedSender<Event>,
    interrupt: InterruptHandle,
    mut bootstrap: SessionBootstrap,
) {
    // One worker owns every conversation and streams at most one turn at a
    // time. Sessions are append-only, so the creation-order index is stable
    // and front-ends can mirror it with their own per-session view state.
    let tools = std::mem::take(&mut bootstrap.tools);
    let executor = bootstrap.executor.take();
    let profiles = std::mem::take(&mut bootstrap.profiles);
    let startup_profile = bootstrap.profile.take();
    let writer = bootstrap
        .rollout_dir
        .map(|dir| RolloutWriter::new(dir, bootstrap.cwd));
    let mut backend = backend;
    let mut sessions = Vec::new();
    let mut slots = Vec::new();
    // The model each session last used, replayed from its rollout. Restored
    // when the session becomes active again (startup adoption happens in the
    // front-end; this covers switching between sessions mid-run).
    let mut session_models: Vec<Option<String>> = Vec::new();
    // The tool set each session last ran with (`/tools`). Per session, like
    // the model: switching restores the session's own set.
    let mut session_tools: Vec<BTreeSet<String>> = Vec::new();

    for loaded in &bootstrap.loaded {
        // The replay includes the `/context` checkpoint stack, so an undo
        // keeps working across a restart.
        let conversation = Conversation::replayed(
            loaded.meta.system_prompt.clone(),
            loaded.messages.clone(),
            loaded.undo.clone(),
        );
        let slot = match writer.as_ref() {
            Some(writer) => match writer.open_existing(&loaded.path) {
                Ok(file) => RolloutSlot {
                    file: Some(file),
                    pending: Vec::new(),
                    disabled: false,
                },
                Err(error) => {
                    eprintln!("codest: cannot reopen rollout: {error}");
                    RolloutSlot::orphaned()
                }
            },
            None => RolloutSlot::inactive(),
        };
        // A recorded session restores the tool set it last ran with; `None`
        // means it never customized one, so every declared tool starts
        // enabled. Names this run does not declare are dropped — a plugin
        // that is absent today cannot be called either way.
        let enabled = match &loaded.tools {
            Some(saved) => saved
                .iter()
                .filter(|name| tools.iter().any(|spec| spec.name == name.as_str()))
                .cloned()
                .collect(),
            None => default_enabled(&tools),
        };
        sessions.push(conversation);
        slots.push(slot);
        session_models.push(loaded.model.clone());
        session_tools.push(enabled);
    }

    // Plain startup opens one fresh session on top of the restored ones; the
    // restore modes only fall back to one when nothing was restored.
    if bootstrap.fresh || sessions.is_empty() {
        sessions.push(Conversation::new(system_prompt.clone()));
        slots.push(RolloutSlot::inactive());
        session_models.push(None);
        session_tools.push(default_enabled(&tools));
    }

    let mut rollouts = Rollouts {
        writer,
        system_prompt: system_prompt.clone(),
        selection: backend.selection().cloned(),
        profile: startup_profile,
        slots,
    };
    let mut current = sessions.len() - 1;
    let mut active: Option<ActiveTurn> = None;
    // Reasoning effort for the next turn (`Op::SetReasoningEffort`,
    // `/effort`). Worker-level rather than backend-level, so it also applies
    // to fixed/test backends (which simply ignore it). A startup preset (the
    // active profile's `effort`) wins outright — including an explicit
    // `off`, which clears the override; otherwise it is seeded from the
    // initial model's `default_level` when it declares one.
    let mut effort =
        match bootstrap.initial_effort.take().map(|value| normalize_effort(&value)) {
            Some(applied) => applied,
            None => {
                backend
                    .selection()
                    .cloned()
                    .and_then(|selection| backend.model_info(&selection))
                    .and_then(|model| model.default_level)
            }
        };

    loop {
        if let Some(mut turn) = active.take() {
            interrupt.install(turn.cancel.clone());
            let deps = TurnContext {
                client: backend.client().clone(),
                options: TurnOptions {
                    effort: effort.clone(),
                    tools: declared_tools(&tools, &session_tools[current]),
                },
                executor: executor.clone(),
                declared: tools.clone(),
                enabled: session_tools[current].clone(),
            };
            run_turn(
                &mut turn,
                &mut sessions[current],
                &mut rollouts,
                current,
                &tx,
                &interrupt,
                &deps,
            )
            .await;
            continue;
        }

        let Some(submission) = rx.recv().await else {
            break;
        };

        match submission.op {
            Op::UserTurn { text } => {
                // Two stops before any wire traffic: nothing selected at all,
                // or a half-specified endpoint (lone base_url/api_key). Both
                // answer with guidance instead of a doomed request — non-TUI
                // front-ends rely on this guard.
                let blocked = match &backend {
                    ModelBackend::Empty { .. } => {
                        Some(crate::core::providers::NO_MODEL_GUIDANCE.to_string())
                    }
                    ModelBackend::Pooled { pool, current, .. } => {
                        pool.missing_credential(&current.provider)
                    }
                    ModelBackend::Fixed(_) => None,
                };
                if let Some(message) = blocked {
                    let _ = tx.send(Event::new(submission.id, EventMsg::Error { message }));
                    continue;
                }
                sessions[current].push_user(text.clone());
                rollouts.append(current, RolloutItem::UserMessage { message: text });
                let cancel = CancellationToken::new();
                let stream = backend.client().stream_chat(
                    sessions[current].snapshot(),
                    TurnOptions {
                        effort: effort.clone(),
                        tools: declared_tools(&tools, &session_tools[current]),
                    },
                    cancel.clone(),
                );
                let _ = tx.send(Event::new(submission.id.clone(), EventMsg::TurnStarted));
                active = Some(ActiveTurn {
                    id: submission.id,
                    stream,
                    cancel,
                    assistant: String::new(),
                    usage: None,
                    model: backend
                        .selection()
                        .map(|selection| selection.as_str().to_string()),
                });
            }
            Op::Interrupt => {
                let _ = tx.send(Event::new(
                    submission.id,
                    EventMsg::TurnAborted {
                        reason: "nothing to interrupt".to_string(),
                    },
                ));
            }
            Op::ClearConversation => {
                sessions[current].clear();
                rollouts.append(current, RolloutItem::Cleared);
                let _ = tx.send(Event::new(submission.id, EventMsg::ConversationCleared));
            }
            Op::NewSession => {
                sessions.push(Conversation::new(system_prompt.clone()));
                let previous = current;
                current = sessions.len() - 1;
                rollouts.push_slot();
                session_tools.push(default_enabled(&tools));
                // A fresh session starts with every tool enabled. The
                // front-end's mirror still shows the previous session's set,
                // so resync it only when that differs — sent before the
                // confirmation so `SessionStarted` stays the op's last event.
                if session_tools[previous] != session_tools[current] {
                    let _ = tx.send(Event::new(
                        submission.id.clone(),
                        EventMsg::ToolsSwitched {
                            enabled: session_tools[current].iter().cloned().collect(),
                        },
                    ));
                }
                let _ = tx.send(Event::new(
                    submission.id,
                    EventMsg::SessionStarted { session: current },
                ));
            }
            Op::SwitchSession { session } => {
                if session < sessions.len() {
                    let previous = current;
                    current = session;
                    // A recorded session remembers the model it last used:
                    // becoming active again restores it, so the footer and
                    // the next turn pick up where the session left off.
                    // Best-effort — a selection that no longer resolves
                    // (provider removed, say) keeps the current model.
                    if let Some(saved) = session_models[session].clone()
                        && !backend
                            .selection()
                            .is_some_and(|active| active.as_str() == saved.as_str())
                        && let Ok(parsed) = apply_model_switch(
                            &mut backend,
                            &mut rollouts,
                            current,
                            &tx,
                            &mut effort,
                            &saved,
                            &submission.id,
                        )
                    {
                        session_models[current] = Some(parsed.as_str().to_string());
                    }
                    // The tool set is per session; the front-end's mirror
                    // still shows the session just left. Resync only when the
                    // set actually differs, and before the confirmation so
                    // `SessionSwitched` stays the op's last event.
                    if session_tools[previous] != session_tools[current] {
                        let _ = tx.send(Event::new(
                            submission.id.clone(),
                            EventMsg::ToolsSwitched {
                                enabled: session_tools[current].iter().cloned().collect(),
                            },
                        ));
                    }
                    let _ = tx.send(Event::new(
                        submission.id,
                        EventMsg::SessionSwitched { session },
                    ));
                } else {
                    let _ = tx.send(Event::new(
                        submission.id,
                        EventMsg::Error {
                            message: format!(
                                "no session {} ({} session{} recorded)",
                                session + 1,
                                sessions.len(),
                                if sessions.len() == 1 { "" } else { "s" }
                            ),
                        },
                    ));
                }
            }
            Op::RenameSession { session, name } => {
                if session < sessions.len() {
                    rollouts.append(session, RolloutItem::Renamed { name: name.clone() });
                    let _ = tx.send(Event::new(
                        submission.id,
                        EventMsg::SessionRenamed { session, name },
                    ));
                } else {
                    let _ = tx.send(Event::new(
                        submission.id,
                        EventMsg::Error {
                            message: format!("no session {}", session + 1),
                        },
                    ));
                }
            }
            Op::SwitchModel { selection } => {
                match apply_model_switch(
                    &mut backend,
                    &mut rollouts,
                    current,
                    &tx,
                    &mut effort,
                    &selection,
                    &submission.id,
                ) {
                    Ok(parsed) => {
                        session_models[current] = Some(parsed.as_str().to_string());
                    }
                    Err(message) => {
                        let _ = tx.send(Event::new(submission.id, EventMsg::Error { message }));
                    }
                }
            }
            Op::SetReasoningEffort { effort: new_effort } => {
                effort = new_effort.clone();
                rollouts.append(
                    current,
                    RolloutItem::ReasoningEffortSwitched { effort: new_effort.clone() },
                );
                let _ = tx.send(Event::new(
                    submission.id,
                    EventMsg::ReasoningEffortSwitched { effort: new_effort },
                ));
            }
            Op::SwitchProfile { name } => {
                let Some(profile) = profiles.get(&name) else {
                    let _ = tx.send(Event::new(
                        submission.id,
                        EventMsg::Error {
                            message: format!("no profile `{name}` is configured"),
                        },
                    ));
                    continue;
                };
                // The declared selection first (switching models reseeds the
                // effort from the model's own `default_level`), then the
                // declared effort, which gets the final say. An unresolvable
                // selection reports but does not abort the rest of the
                // preset — the profile still becomes active.
                if let Some(selection) = profile.default_selection() {
                    match apply_model_switch(
                        &mut backend,
                        &mut rollouts,
                        current,
                        &tx,
                        &mut effort,
                        selection,
                        &submission.id,
                    ) {
                        Ok(parsed) => {
                            session_models[current] = Some(parsed.as_str().to_string());
                        }
                        Err(message) => {
                            let _ = tx.send(Event::new(
                                submission.id.clone(),
                                EventMsg::Error { message },
                            ));
                        }
                    }
                }
                if let Some(level) = &profile.effort {
                    // The declared value behaves exactly like `/effort`: an
                    // `off` clears the override, anything else is the level.
                    let applied = normalize_effort(level);
                    effort = applied.clone();
                    rollouts.append(
                        current,
                        RolloutItem::ReasoningEffortSwitched { effort: applied.clone() },
                    );
                    let _ = tx.send(Event::new(
                        submission.id.clone(),
                        EventMsg::ReasoningEffortSwitched { effort: applied },
                    ));
                }
                rollouts.append(current, RolloutItem::ProfileSwitched { name: name.clone() });
                let _ = tx.send(Event::new(
                    submission.id,
                    EventMsg::ProfileSwitched { name },
                ));
            }
            Op::SetToolsEnabled { enabled: requested } => {
                // Only declared tools can be enabled: unknown names are
                // dropped, so a replayed set cannot resurrect tools this run
                // never declared. An empty set is legitimate (chat-only).
                let applied: BTreeSet<String> = requested
                    .into_iter()
                    .filter(|name| tools.iter().any(|spec| spec.name == name.as_str()))
                    .collect();
                session_tools[current] = applied;
                let enabled: Vec<String> = session_tools[current].iter().cloned().collect();
                rollouts.append(
                    current,
                    RolloutItem::ToolsSwitched {
                        enabled: enabled.clone(),
                    },
                );
                let _ = tx.send(Event::new(submission.id, EventMsg::ToolsSwitched { enabled }));
            }
            Op::InspectConversation => {
                let _ = tx.send(Event::new(
                    submission.id,
                    EventMsg::ConversationSnapshot {
                        messages: sessions[current].snapshot(),
                    },
                ));
            }
            Op::DeleteMessages { indices } => {
                let conversation = &mut sessions[current];
                match conversation.delete_at(&indices) {
                    Ok(removed) => {
                        // The `/context` viewer expands tool-call rounds
                        // before submitting, so a rejected shape means a
                        // stale or hand-built index set: undo the delete and
                        // answer with why.
                        if let Err(message) =
                            Conversation::validate_tool_structure(conversation.messages())
                        {
                            conversation.undo_last_edit();
                            let _ = tx.send(Event::new(
                                submission.id,
                                EventMsg::Error { message },
                            ));
                        } else {
                            let (indices, removed): (Vec<usize>, Vec<Message>) =
                                removed.into_iter().unzip();
                            rollouts.append(
                                current,
                                RolloutItem::ContextDeleted { indices, removed },
                            );
                            let _ = tx.send(Event::new(
                                submission.id,
                                EventMsg::ConversationSnapshot {
                                    messages: conversation.snapshot(),
                                },
                            ));
                        }
                    }
                    Err(message) => {
                        let _ = tx.send(Event::new(submission.id, EventMsg::Error { message }));
                    }
                }
            }
            Op::EditMessage { index, content } => {
                let conversation = &mut sessions[current];
                match conversation.edit_content(index, content.clone()) {
                    Ok(previous) => {
                        rollouts.append(
                            current,
                            RolloutItem::ContextEdited {
                                index,
                                content,
                                previous,
                            },
                        );
                        let _ = tx.send(Event::new(
                            submission.id,
                            EventMsg::ConversationSnapshot {
                                messages: conversation.snapshot(),
                            },
                        ));
                    }
                    Err(message) => {
                        let _ = tx.send(Event::new(submission.id, EventMsg::Error { message }));
                    }
                }
            }
            Op::UndoContextEdit => {
                let conversation = &mut sessions[current];
                if conversation.undo_last_edit() {
                    rollouts.append(current, RolloutItem::ContextRestored);
                    let _ = tx.send(Event::new(
                        submission.id,
                        EventMsg::ConversationSnapshot {
                            messages: conversation.snapshot(),
                        },
                    ));
                } else {
                    let _ = tx.send(Event::new(
                        submission.id,
                        EventMsg::Error {
                            message: "nothing left to restore".to_string(),
                        },
                    ));
                }
            }
            Op::Shutdown => {
                let _ = tx.send(Event::new(submission.id, EventMsg::ShutdownComplete));
                break;
            }
        }
    }
}

/// Everything a turn's rounds need beyond the stream itself: the client for
/// subsequent rounds, the per-turn options (effort + tools), the plugin
/// executor for tool calls, and the tools this turn enabled. Built fresh per
/// turn; the backend cannot switch mid-turn.
struct TurnContext {
    client: Arc<dyn ModelClient>,
    options: TurnOptions,
    executor: Option<Arc<dyn ToolExecutor>>,
    /// Every tool the plugins declared this run, whatever the session's
    /// `/tools` set says — the gate's dictionary.
    declared: Vec<ToolSpec>,
    /// The enabled subset at turn start. A call for anything outside it is
    /// rejected without executing: the model was not shown it, so reaching
    /// here means a hallucinated name (or a race with `/tools` mid-turn —
    /// the snapshot wins, ops are FIFO anyway).
    enabled: BTreeSet<String>,
}

async fn run_turn(
    turn: &mut ActiveTurn,
    conversation: &mut Conversation,
    rollouts: &mut Rollouts,
    current: usize,
    tx: &mpsc::UnboundedSender<Event>,
    interrupt: &InterruptHandle,
    deps: &TurnContext,
) {
    let mut round = 0usize;
    loop {
        round += 1;

        // One round: stream to completion, keeping any tool calls the model
        // asked for. Text and reasoning stream through exactly like a
        // tool-less turn.
        let mut requested: Vec<ToolCall> = Vec::new();
        loop {
            tokio::select! {
                biased;
                _ = turn.cancel.cancelled() => {
                    finalize_aborted(turn, conversation, rollouts, current, tx);
                    interrupt.clear();
                    return;
                }
                item = turn.stream.next() => {
                    match item {
                        Some(Ok(StreamItem::Delta(delta))) => {
                            turn.assistant.push_str(&delta);
                            let _ = tx.send(Event::new(
                                turn.id.clone(),
                                EventMsg::AgentMessageDelta { delta },
                            ));
                        }
                        Some(Ok(StreamItem::Thinking(delta))) => {
                            // Reasoning output stays out of the conversation and
                            // the rollout; the front-end renders it display-only.
                            let _ = tx.send(Event::new(
                                turn.id.clone(),
                                EventMsg::AgentThinkingDelta { delta },
                            ));
                        }
                        Some(Ok(StreamItem::ToolCalls(calls))) => {
                            requested = calls;
                        }
                        Some(Ok(StreamItem::Usage { input, output })) => {
                            turn.usage = Some((input, output));
                        }
                        // `StreamItem` is `#[non_exhaustive]` and lives in the
                        // contract crate, so a plugin protocol adding a variant
                        // cannot break this build: unknown items are ignored
                        // rather than ending the turn.
                        Some(Ok(_)) => {}
                        Some(Err(error)) => {
                            finalize_error(turn, conversation, rollouts, current, tx, &error.to_string());
                            interrupt.clear();
                            return;
                        }
                        None => break,
                    }
                }
            }
        }

        // No tool calls: the turn is over, exactly like the tool-less past.
        if requested.is_empty() {
            finalize_complete(turn, conversation, rollouts, current, tx);
            interrupt.clear();
            return;
        }

        // The model wants tools. Commit whatever text preceded the calls so
        // the transcript keeps its order (text → calls → results), record the
        // calls message, then run each call and feed the results back as
        // tool messages.
        commit_round_text(turn, conversation, rollouts, current, tx);
        conversation.push_message(Message::assistant_tool_calls(requested.clone()));
        rollouts.append(
            current,
            RolloutItem::AssistantToolCalls {
                calls: requested.clone(),
            },
        );
        for call in requested {
            if turn.cancel.is_cancelled() {
                finalize_aborted(turn, conversation, rollouts, current, tx);
                interrupt.clear();
                return;
            }
            // `false` = interrupted while the tool ran; its unrecorded result
            // is dropped and the turn ends aborted.
            if !execute_tool_call(turn, conversation, rollouts, current, tx, deps, call).await {
                finalize_aborted(turn, conversation, rollouts, current, tx);
                interrupt.clear();
                return;
            }
        }

        if round >= MAX_TOOL_ROUNDS {
            finalize_error(
                turn,
                conversation,
                rollouts,
                current,
                tx,
                &format!("tool round limit reached ({MAX_TOOL_ROUNDS} rounds)"),
            );
            interrupt.clear();
            return;
        }

        // The next round sees the tool results in the snapshot.
        turn.stream = deps.client.stream_chat(
            conversation.snapshot(),
            deps.options.clone(),
            turn.cancel.clone(),
        );
    }
}

/// Commit the text streamed so far this round (assistant message in the
/// conversation and rollout, `AgentMessage` to the front-end) and empty the
/// accumulator so the next round starts clean.
fn commit_round_text(
    turn: &mut ActiveTurn,
    conversation: &mut Conversation,
    rollouts: &mut Rollouts,
    current: usize,
    tx: &mpsc::UnboundedSender<Event>,
) {
    if turn.assistant.is_empty() {
        return;
    }
    let text = std::mem::take(&mut turn.assistant);
    conversation.push_assistant(text.clone());
    rollouts.append(current, RolloutItem::AssistantMessage { message: text.clone() });
    let _ = tx.send(Event::new(turn.id.clone(), EventMsg::AgentMessage { message: text }));
}

/// Run one tool call, record it everywhere, and tell the front-end. Failures
/// become the tool message's content so the model can react to them. Returns
/// `false` when the turn was interrupted while the call ran: nothing is
/// recorded for it (the caller finalizes the abort), and the abandoned call
/// keeps running on the blocking pool with its result dropped — a blocking
/// task cannot be cancelled, only ignored.
async fn execute_tool_call(
    turn: &mut ActiveTurn,
    conversation: &mut Conversation,
    rollouts: &mut Rollouts,
    current: usize,
    tx: &mpsc::UnboundedSender<Event>,
    deps: &TurnContext,
    call: ToolCall,
) -> bool {
    let outcome = if !deps.enabled.contains(&call.name) {
        // Two distinct cases, answered differently: a tool nobody declares
        // (a hallucinated name) versus one this session switched off.
        // Neither executes; the model gets something it can act on.
        if deps.declared.iter().any(|spec| spec.name == call.name) {
            Err(format!(
                "tool `{}` is disabled in this session (re-enable it with /tools)",
                call.name
            ))
        } else {
            Err(format!("unknown tool `{}`", call.name))
        }
    } else {
        match deps.executor.clone() {
            // The executor is synchronous plugin code: run it on the blocking
            // pool so a slow tool cannot stall the worker, and let an
            // interrupt win over a still-running call.
            Some(executor) => {
                let owned = call.clone();
                tokio::select! {
                    biased;
                    _ = turn.cancel.cancelled() => return false,
                    outcome = tokio::task::spawn_blocking(move || executor.execute(&owned)) => {
                        outcome
                            .unwrap_or_else(|error| Err(format!("tool `{}` task failed: {error}", call.name)))
                    }
                }
            }
            None => Err(format!(
                "tool `{}` is not available: no tool executor is configured",
                call.name
            )),
        }
    };
    let (result, error) = match outcome {
        Ok(output) => (output, None),
        Err(message) => (String::new(), Some(message)),
    };
    let content = match &error {
        Some(message) => format!("error: {message}"),
        None => result.clone(),
    };
    let _ = tx.send(Event::new(
        turn.id.clone(),
        EventMsg::AgentToolCall {
            name: call.name.clone(),
            arguments: call.arguments,
            result,
            error,
        },
    ));
    conversation.push_message(Message::tool_result(&call.id, content.clone()));
    rollouts.append(
        current,
        RolloutItem::ToolResult {
            tool_call_id: call.id,
            content,
        },
    );
    true
}

/// Switch the backend to `selection` and record it everywhere: the rollout
/// item, the `ModelSwitched` event, the model's `default_level` seeding the
/// effort, and the returned selection for the caller's bookkeeping. Shared
/// by `Op::SwitchModel` and the per-session model restore on
/// `Op::SwitchSession`.
fn apply_model_switch(
    backend: &mut ModelBackend,
    rollouts: &mut Rollouts,
    current: usize,
    tx: &mpsc::UnboundedSender<Event>,
    effort: &mut Option<String>,
    selection: &str,
    submission_id: &str,
) -> Result<Selection, String> {
    let parsed = backend.switch(selection)?;
    rollouts.selection = Some(parsed.clone());
    rollouts.append(
        current,
        RolloutItem::ModelSwitched {
            provider: parsed.provider.clone(),
            model: parsed.model.clone(),
        },
    );
    let _ = tx.send(Event::new(
        submission_id.to_string(),
        EventMsg::ModelSwitched {
            model: parsed.as_str(),
        },
    ));
    // Selecting a model that declares a `default_level` resets the effort to
    // it; one without keeps the current effort. The event keeps the
    // front-end footer in step.
    let default_effort = backend
        .model_info(&parsed)
        .and_then(|model| model.default_level);
    if default_effort.is_some() && default_effort != *effort {
        *effort = default_effort.clone();
        rollouts.append(
            current,
            RolloutItem::ReasoningEffortSwitched { effort: effort.clone() },
        );
        let _ = tx.send(Event::new(
            submission_id.to_string(),
            EventMsg::ReasoningEffortSwitched { effort: default_effort },
        ));
    }
    Ok(parsed)
}

fn finalize_complete(
    turn: &ActiveTurn,
    conversation: &mut Conversation,
    rollouts: &mut Rollouts,
    current: usize,
    tx: &mpsc::UnboundedSender<Event>,
) {
    conversation.push_assistant(turn.assistant.clone());
    rollouts.append(
        current,
        RolloutItem::AssistantMessage {
            message: turn.assistant.clone(),
        },
    );
    let _ = tx.send(Event::new(
        turn.id.clone(),
        EventMsg::AgentMessage {
            message: turn.assistant.clone(),
        },
    ));
    if let Some((input, output)) = turn.usage {
        rollouts.append(
            current,
            RolloutItem::TurnUsage {
                model: turn.model.clone(),
                input,
                output,
            },
        );
        let _ = tx.send(Event::new(turn.id.clone(), EventMsg::TurnUsage { input, output }));
    }
    let _ = tx.send(Event::new(turn.id.clone(), EventMsg::TurnComplete));
}

fn finalize_aborted(
    turn: &ActiveTurn,
    conversation: &mut Conversation,
    rollouts: &mut Rollouts,
    current: usize,
    tx: &mpsc::UnboundedSender<Event>,
) {
    if !turn.assistant.is_empty() {
        conversation.push_assistant(turn.assistant.clone());
        rollouts.append(
            current,
            RolloutItem::AssistantMessage {
                message: turn.assistant.clone(),
            },
        );
        let _ = tx.send(Event::new(
            turn.id.clone(),
            EventMsg::AgentMessage {
                message: turn.assistant.clone(),
            },
        ));
    }
    let _ = tx.send(Event::new(
        turn.id.clone(),
        EventMsg::TurnAborted {
            reason: "interrupted by user".to_string(),
        },
    ));
    let _ = tx.send(Event::new(turn.id.clone(), EventMsg::TurnComplete));
}

fn finalize_error(
    turn: &ActiveTurn,
    conversation: &mut Conversation,
    rollouts: &mut Rollouts,
    current: usize,
    tx: &mpsc::UnboundedSender<Event>,
    message: &str,
) {
    // Keep any partial text in the transcript so the next turn has context.
    if !turn.assistant.is_empty() {
        conversation.push_assistant(turn.assistant.clone());
        rollouts.append(
            current,
            RolloutItem::AssistantMessage {
                message: turn.assistant.clone(),
            },
        );
        let _ = tx.send(Event::new(
            turn.id.clone(),
            EventMsg::AgentMessage {
                message: turn.assistant.clone(),
            },
        ));
    }
    let _ = tx.send(Event::new(
        turn.id.clone(),
        EventMsg::Error {
            message: message.to_string(),
        },
    ));
    let _ = tx.send(Event::new(turn.id.clone(), EventMsg::TurnComplete));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::ModelInfo;
    use crate::core::config::ProviderInstance;
    use crate::core::echo_model::EchoModelClient;
    use crate::core::protocol::Message;
    use crate::core::protocol::Role;
    use std::collections::HashMap;
    use std::time::Duration;

    async fn collect_turn(session: &mut AgentSession) -> Vec<Event> {
        let mut events = Vec::new();
        while let Some(event) = session.next_event().await {
            let done = matches!(event.msg, EventMsg::TurnComplete);
            events.push(event);
            if done {
                break;
            }
        }
        events
    }

    #[tokio::test]
    async fn streams_a_turn_end_to_end() {
        let client = Arc::new(EchoModelClient::new(Duration::ZERO));
        let mut session = AgentSession::spawn(client);

        session.submit_user_turn("hello");
        let events = collect_turn(&mut session).await;

        assert!(matches!(events[0].msg, EventMsg::TurnStarted));

        let mut streamed = String::new();
        let mut final_message = None;
        for event in &events {
            match &event.msg {
                EventMsg::AgentMessageDelta { delta } => streamed.push_str(delta),
                EventMsg::AgentMessage { message } => final_message = Some(message.clone()),
                _ => {}
            }
        }

        let final_message = final_message.expect("a completed turn emits AgentMessage");
        assert_eq!(streamed, final_message);
        assert!(final_message.contains("You said: \"hello\""));
        assert!(matches!(
            events.last().map(|event| &event.msg),
            Some(EventMsg::TurnComplete)
        ));
    }

    /// Records the full snapshot and options passed to each model call.
    #[derive(Clone, Default)]
    struct RecordingClient {
        seen: Arc<Mutex<Vec<Vec<Message>>>>,
        options: Arc<Mutex<Vec<TurnOptions>>>,
    }

    impl RecordingClient {
        fn new() -> Self {
            Self::default()
        }

        fn snapshots(&self) -> Arc<Mutex<Vec<Vec<Message>>>> {
            self.seen.clone()
        }

        fn turn_options(&self) -> Arc<Mutex<Vec<TurnOptions>>> {
            self.options.clone()
        }
    }

    impl ModelClient for RecordingClient {
        fn label(&self) -> String {
            "recording".to_string()
        }

        fn stream_chat(
            &self,
            messages: Vec<Message>,
            options: TurnOptions,
            _cancel: CancellationToken,
        ) -> ModelStream {
            self.seen.lock().expect("lock").push(messages);
            self.options.lock().expect("lock").push(options);
            Box::pin(futures_util::stream::once(async {
                Ok::<_, crate::core::model::ModelError>(StreamItem::Delta("ok".to_string()))
            }))
        }
    }

    /// A client that plays back one scripted stream per `stream_chat` call —
    /// the shape a tool round-trip produces (tools first, then final text).
    /// Exhausted scripts fall back to plain text so a runaway loop still ends.
    struct ScriptedClient {
        scripts: Arc<Mutex<Vec<Vec<StreamItem>>>>,
        seen: Arc<Mutex<Vec<Vec<Message>>>>,
        options: Arc<Mutex<Vec<TurnOptions>>>,
    }

    impl ScriptedClient {
        fn new(scripts: Vec<Vec<StreamItem>>) -> Self {
            Self {
                scripts: Arc::new(Mutex::new(scripts)),
                seen: Arc::new(Mutex::new(Vec::new())),
                options: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn snapshots(&self) -> Arc<Mutex<Vec<Vec<Message>>>> {
            self.seen.clone()
        }

        fn turn_options(&self) -> Arc<Mutex<Vec<TurnOptions>>> {
            self.options.clone()
        }
    }

    impl ModelClient for ScriptedClient {
        fn label(&self) -> String {
            "scripted".to_string()
        }

        fn stream_chat(
            &self,
            messages: Vec<Message>,
            options: TurnOptions,
            _cancel: CancellationToken,
        ) -> ModelStream {
            self.seen.lock().expect("lock").push(messages);
            self.options.lock().expect("lock").push(options);
            let script = {
                let mut scripts = self.scripts.lock().expect("lock");
                if scripts.is_empty() {
                    vec![StreamItem::Delta("done".to_string())]
                } else {
                    scripts.remove(0)
                }
            };
            Box::pin(futures_util::stream::iter(
                script.into_iter().map(Ok::<_, crate::core::model::ModelError>),
            ))
        }
    }

    /// Executes `ping`, fails everything else — the minimal plugin stand-in.
    struct MapExecutor;

    impl ToolExecutor for MapExecutor {
        fn execute(&self, call: &ToolCall) -> Result<String, String> {
            match call.name.as_str() {
                "ping" => Ok(format!("pong {}", call.arguments)),
                other => Err(format!("nope: {other}")),
            }
        }
    }

    static PING_SPEC: ToolSpec = ToolSpec {
        name: "ping",
        description: "reply with pong",
        parameters_json: "",
        read_only: true,
    };

    fn tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: "{}".to_string(),
        }
    }

    #[tokio::test]
    async fn tool_calls_execute_and_continue_the_turn() {
        let client = ScriptedClient::new(vec![
            vec![
                StreamItem::Delta("checking".to_string()),
                StreamItem::ToolCalls(vec![tool_call("c1", "ping")]),
                StreamItem::Usage {
                    input: 10,
                    output: 2,
                },
            ],
            vec![
                StreamItem::Delta("all done".to_string()),
                StreamItem::Usage {
                    input: 20,
                    output: 4,
                },
            ],
        ]);
        let seen = client.snapshots();
        let options = client.turn_options();
        let mut session = AgentSession::spawn_with_bootstrap(
            Arc::new(client),
            "be brief",
            SessionBootstrap {
                tools: vec![PING_SPEC],
                executor: Some(Arc::new(MapExecutor)),
                ..SessionBootstrap::default()
            },
        );

        session.submit_user_turn("hi");
        let events = collect_turn(&mut session).await;

        // The pre-call text commits as its own message before the tool
        // activity, then the second round answers.
        let messages: Vec<&EventMsg> = events.iter().map(|event| &event.msg).collect();
        assert!(matches!(
            messages.as_slice(),
            [
                EventMsg::TurnStarted,
                EventMsg::AgentMessageDelta { .. },
                EventMsg::AgentMessage { message },
                EventMsg::AgentToolCall { name, result, error: None, .. },
                EventMsg::AgentMessageDelta { .. },
                EventMsg::AgentMessage { .. },
                EventMsg::TurnUsage { input: 20, output: 4 },
                EventMsg::TurnComplete,
            ] if message == "checking" && name == "ping" && result == "pong {}"
        ));

        // The second request carries the tool round on the wire: the text,
        // the calls message, and the tool result answering it.
        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].len(), 5);
        assert_eq!(seen[1][2].content, "checking");
        assert_eq!(seen[1][3].role, Role::Assistant);
        assert_eq!(
            seen[1][3].tool_calls.as_deref(),
            Some(&[tool_call("c1", "ping")][..])
        );
        assert_eq!(seen[1][4].role, Role::Tool);
        assert_eq!(seen[1][4].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(seen[1][4].content, "pong {}");

        // Both rounds offer the declared tools.
        let options = options.lock().expect("lock");
        assert_eq!(options[0].tools, vec![PING_SPEC]);
        assert_eq!(options[1].tools, vec![PING_SPEC]);
    }

    #[tokio::test]
    async fn tool_failures_are_fed_back_and_the_turn_continues() {
        let client = ScriptedClient::new(vec![
            vec![StreamItem::ToolCalls(vec![tool_call("c1", "ping")])],
            vec![StreamItem::Delta("recovered".to_string())],
        ]);
        let seen = client.snapshots();
        let mut session = AgentSession::spawn_with_bootstrap(
            Arc::new(client),
            "be brief",
            SessionBootstrap {
                tools: vec![PING_SPEC],
                executor: Some(Arc::new(FailExecutor)),
                ..SessionBootstrap::default()
            },
        );

        session.submit_user_turn("hi");
        let events = collect_turn(&mut session).await;

        // The tool is enabled and declared, so the failure comes from the
        // executor itself rather than the availability gate.
        assert!(events.iter().any(|event| matches!(
            &event.msg,
            EventMsg::AgentToolCall { name, error: Some(error), .. }
                if name == "ping" && error == "nope: ping"
        )));

        // The model sees the failure as the tool message content.
        let seen = seen.lock().expect("lock");
        assert_eq!(seen[1][3].content, "error: nope: ping");
    }

    /// Declares `ping` but always fails it — the executor-level failure the
    /// turn loop must feed back rather than abort on.
    struct FailExecutor;

    impl ToolExecutor for FailExecutor {
        fn execute(&self, call: &ToolCall) -> Result<String, String> {
            Err(format!("nope: {}", call.name))
        }
    }

    #[tokio::test]
    async fn set_tools_enabled_withholds_declarations_and_rejects_calls() {
        let client = ScriptedClient::new(vec![
            // The model calls `ping` anyway — it must come back as an error
            // result, not an execution.
            vec![StreamItem::ToolCalls(vec![tool_call("c1", "ping")])],
            vec![StreamItem::Delta("done".to_string())],
        ]);
        let seen = client.snapshots();
        let options = client.turn_options();
        let mut session = AgentSession::spawn_with_bootstrap(
            Arc::new(client),
            "be brief",
            SessionBootstrap {
                tools: vec![PING_SPEC],
                executor: Some(Arc::new(MapExecutor)),
                ..SessionBootstrap::default()
            },
        );

        // Unknown names are dropped; an empty set is legitimate (chat-only).
        session.set_tools_enabled(vec!["nonexistent".to_string()]);
        session.set_tools_enabled(Vec::new());
        session.submit_user_turn("hi");
        let events = collect_turn(&mut session).await;

        // The switch confirms with the applied (validated) set.
        let switches: Vec<Vec<String>> = events
            .iter()
            .filter_map(|event| match &event.msg {
                EventMsg::ToolsSwitched { enabled } => Some(enabled.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(switches, [Vec::<String>::new(), Vec::<String>::new()]);

        // The disabled tool is not declared to the model on the wire…
        let options = options.lock().expect("lock");
        assert!(options[0].tools.is_empty());
        drop(options);

        // …and a hallucinated call for it is rejected without executing.
        assert!(events.iter().any(|event| matches!(
            &event.msg,
            EventMsg::AgentToolCall { name, error: Some(error), .. }
                if name == "ping" && error.contains("disabled")
        )));
        let seen = seen.lock().expect("lock");
        assert!(seen[1][3].content.contains("disabled"));
    }

    /// Blocks inside `execute` until released, announcing that it entered —
    /// the stand-in for a slow tool.
    struct GateExecutor {
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        release: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ToolExecutor for GateExecutor {
        fn execute(&self, _call: &ToolCall) -> Result<String, String> {
            use std::sync::atomic::Ordering;
            let _ = self.entered.send(());
            while !self.release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok("released".to_string())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn interrupt_lands_during_a_slow_tool_call() {
        use std::sync::atomic::Ordering;

        let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let client = ScriptedClient::new(vec![vec![StreamItem::ToolCalls(vec![tool_call(
            "c1", "ping",
        )])]]);
        let seen = client.snapshots();
        let mut session = AgentSession::spawn_with_bootstrap(
            Arc::new(client),
            "be brief",
            SessionBootstrap {
                tools: vec![PING_SPEC],
                executor: Some(Arc::new(GateExecutor {
                    entered,
                    release: release.clone(),
                })),
                ..SessionBootstrap::default()
            },
        );

        session.submit_user_turn("hi");
        // Wait until the call is running on the blocking pool, then interrupt
        // mid-call and let the tool finish its wait.
        entered_rx.recv().await.expect("the tool started");
        session.interrupt();
        release.store(true, Ordering::SeqCst);

        let events = collect_turn(&mut session).await;
        // The abandoned call leaves no trace: no tool-call event, no result,
        // and no follow-up round against the model.
        let messages: Vec<&EventMsg> = events.iter().map(|event| &event.msg).collect();
        assert!(matches!(
            messages.as_slice(),
            [EventMsg::TurnStarted, EventMsg::TurnAborted { .. }, EventMsg::TurnComplete]
        ));
        assert_eq!(seen.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn tool_calls_without_an_executor_error_but_keep_the_turn_going() {
        let client = ScriptedClient::new(vec![
            vec![StreamItem::ToolCalls(vec![tool_call("c1", "ping")])],
            vec![StreamItem::Delta("gave up".to_string())],
        ]);
        let seen = client.snapshots();
        let mut session = AgentSession::spawn_with_bootstrap(
            Arc::new(client),
            "be brief",
            SessionBootstrap {
                tools: vec![PING_SPEC],
                ..SessionBootstrap::default()
            },
        );

        session.submit_user_turn("hi");
        let events = collect_turn(&mut session).await;

        assert!(events.iter().any(|event| matches!(
            &event.msg,
            EventMsg::AgentToolCall { error: Some(error), .. }
                if error.contains("no tool executor is configured")
        )));
        let seen = seen.lock().expect("lock");
        assert!(seen[1][3].content.starts_with("error: "));
    }

    #[tokio::test]
    async fn a_runaway_tool_loop_hits_the_round_limit() {
        let calls = || {
            vec![StreamItem::ToolCalls(vec![tool_call("c1", "ping")])]
        };
        let client = ScriptedClient::new((0..MAX_TOOL_ROUNDS).map(|_| calls()).collect());
        let seen = client.snapshots();
        let mut session = AgentSession::spawn_with_bootstrap(
            Arc::new(client),
            "be brief",
            SessionBootstrap {
                tools: vec![PING_SPEC],
                executor: Some(Arc::new(MapExecutor)),
                ..SessionBootstrap::default()
            },
        );

        session.submit_user_turn("hi");
        let events = collect_turn(&mut session).await;

        let tool_events = events
            .iter()
            .filter(|event| matches!(event.msg, EventMsg::AgentToolCall { .. }))
            .count();
        assert_eq!(tool_events, MAX_TOOL_ROUNDS);
        assert!(events.iter().any(|event| matches!(
            &event.msg,
            EventMsg::Error { message } if message.contains("tool round limit")
        )));
        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), MAX_TOOL_ROUNDS);
    }

    #[tokio::test]
    async fn tool_rounds_persist_and_replay() {
        let dir = std::env::temp_dir().join(format!(
            "codest-worker-test-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let client = ScriptedClient::new(vec![
            vec![StreamItem::ToolCalls(vec![tool_call("c1", "ping")])],
            vec![StreamItem::Delta("all done".to_string())],
        ]);
        let mut session = AgentSession::spawn_with_bootstrap(
            Arc::new(client),
            "be brief",
            SessionBootstrap {
                tools: vec![PING_SPEC],
                executor: Some(Arc::new(MapExecutor)),
                rollout_dir: Some(dir.clone()),
                cwd: "/tmp/demo".to_string(),
                fresh: true,
                ..SessionBootstrap::default()
            },
        );
        session.submit_user_turn("hi");
        collect_turn(&mut session).await;
        session.shutdown();
        while session.next_event().await.is_some() {}

        let loaded = crate::core::rollout::load_workspace_sessions(&dir).expect("load");
        assert_eq!(loaded.len(), 1);
        let messages = &loaded[0].messages;
        assert_eq!(messages.len(), 4); // user, calls, tool result, final text
        assert!(matches!(messages[1].tool_calls.as_deref(), Some([_])));
        assert_eq!(messages[2].role, Role::Tool);
        assert_eq!(messages[2].content, "pong {}");
        assert_eq!(messages[3].content, "all done");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn second_turn_sends_the_full_history() {        let client = RecordingClient::new();
        let seen = client.snapshots();
        let mut session = AgentSession::spawn(Arc::new(client));

        session.submit_user_turn("first");
        collect_turn(&mut session).await;
        session.submit_user_turn("second");
        collect_turn(&mut session).await;

        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].len(), 2); // system + first user
        assert_eq!(seen[1].len(), 4); // system + first user + assistant + second user
        assert_eq!(seen[1][0].role, Role::System);
        assert_eq!(seen[1][1].content, "first");
        assert_eq!(seen[1][2].content, "ok");
        assert_eq!(seen[1][3].content, "second");
    }

    #[tokio::test]
    async fn clear_conversation_drops_history_but_keeps_the_system_prompt() {
        let client = RecordingClient::new();
        let seen = client.snapshots();
        let mut session = AgentSession::spawn(Arc::new(client));

        session.submit_user_turn("first");
        collect_turn(&mut session).await;

        session.clear_conversation();
        let event = session.next_event().await.expect("clear event");
        assert!(matches!(event.msg, EventMsg::ConversationCleared));

        session.submit_user_turn("second");
        collect_turn(&mut session).await;

        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].len(), 2); // system + second user only
        assert_eq!(seen[1][0].role, Role::System);
        assert_eq!(seen[1][1].content, "second");
    }

    #[tokio::test]
    async fn interrupt_keeps_partial_output() {
        let client = Arc::new(EchoModelClient::new(Duration::from_millis(5)));
        let mut session = AgentSession::spawn(client);

        session.submit_user_turn("interrupt me");

        // Wait for at least one delta before interrupting.
        loop {
            let event = session.next_event().await.expect("event");
            if matches!(event.msg, EventMsg::AgentMessageDelta { .. }) {
                break;
            }
        }
        session.interrupt();

        let mut saw_aborted = false;
        loop {
            let event = session.next_event().await.expect("event");
            match event.msg {
                EventMsg::TurnAborted { .. } => saw_aborted = true,
                EventMsg::TurnComplete => break,
                _ => {}
            }
        }
        assert!(saw_aborted);
    }

    #[tokio::test]
    async fn new_session_starts_fresh_context_and_switch_restores_it() {
        let client = RecordingClient::new();
        let seen = client.snapshots();
        let mut session = AgentSession::spawn(Arc::new(client));

        session.submit_user_turn("first");
        collect_turn(&mut session).await;

        session.new_session();
        let event = session.next_event().await.expect("session started");
        assert!(matches!(event.msg, EventMsg::SessionStarted { session: 1 }));

        session.submit_user_turn("second");
        collect_turn(&mut session).await;

        session.switch_session(0);
        let event = session.next_event().await.expect("session switched");
        assert!(matches!(
            event.msg,
            EventMsg::SessionSwitched { session: 0 }
        ));

        session.submit_user_turn("third");
        collect_turn(&mut session).await;

        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].len(), 2); // system + first user
        assert_eq!(seen[1].len(), 2); // fresh session: system + second user only
        assert_eq!(seen[1][1].content, "second");
        // Restored session replays its archived history.
        assert_eq!(seen[2].len(), 4); // system + first user + assistant + third
        assert_eq!(seen[2][1].content, "first");
        assert_eq!(seen[2][2].content, "ok");
        assert_eq!(seen[2][3].content, "third");
    }

    #[tokio::test]
    async fn switching_to_an_unknown_session_reports_an_error() {
        let client = Arc::new(EchoModelClient::new(Duration::ZERO));
        let mut session = AgentSession::spawn(client);

        session.switch_session(3);
        let event = session.next_event().await.expect("error event");
        assert!(matches!(event.msg, EventMsg::Error { .. }));

        // The worker keeps serving turns after the failed switch.
        session.submit_user_turn("still there");
        let events = collect_turn(&mut session).await;
        assert!(matches!(
            events.last().map(|event| &event.msg),
            Some(EventMsg::TurnComplete)
        ));
    }

    #[tokio::test]
    async fn sessions_persist_to_rollouts_and_reload() {
        let dir = std::env::temp_dir().join(format!(
            "codest-worker-test-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let bootstrap = |loaded, fresh| SessionBootstrap {
            loaded,
            rollout_dir: Some(dir.clone()),
            cwd: "/tmp/demo".to_string(),
            fresh,
            ..SessionBootstrap::default()
        };

        // First run: one turn lands in a rollout file.
        let mut session = AgentSession::spawn_with_bootstrap(
            Arc::new(RecordingClient::new()),
            "be brief",
            bootstrap(Vec::new(), true),
        );
        session.submit_user_turn("first");
        collect_turn(&mut session).await;
        session.shutdown();
        while session.next_event().await.is_some() {}

        let loaded = crate::core::rollout::load_workspace_sessions(&dir).expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].messages.len(), 2); // user + assistant
        assert_eq!(loaded[0].messages[0].content, "first");
        assert_eq!(loaded[0].name, None);

        // Second run: the replayed history reaches the model. `fresh: false`
        // reactivates the restored session instead of opening a new one.
        let client = RecordingClient::new();
        let seen = client.snapshots();
        let mut session = AgentSession::spawn_with_bootstrap(
            Arc::new(client),
            "be brief",
            bootstrap(loaded, false),
        );
        session.submit_user_turn("second");
        collect_turn(&mut session).await;

        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].len(), 4); // system + first + "ok" + second
        assert_eq!(seen[0][1].content, "first");
        assert_eq!(seen[0][2].content, "ok");
        assert_eq!(seen[0][3].content, "second");
    }

    #[tokio::test]
    async fn renames_persist_to_rollouts() {
        let dir = std::env::temp_dir().join(format!(
            "codest-rename-test-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");

        // A session with no user input produces no rollout file at all, even
        // when it was renamed.
        let mut session = AgentSession::spawn_with_bootstrap(
            Arc::new(EchoModelClient::new(Duration::ZERO)),
            "be brief",
            SessionBootstrap {
                loaded: Vec::new(),
                rollout_dir: Some(dir.clone()),
                cwd: "/tmp/demo".to_string(),
                fresh: true,
                ..SessionBootstrap::default()
            },
        );
        session.rename_session(0, "my chat");
        session.shutdown();
        while session.next_event().await.is_some() {}
        assert!(
            crate::core::rollout::list_workspace_sessions(&dir)
                .expect("list")
                .is_empty()
        );

        // Once input arrives the file appears, with the rename replayed.
        let mut session = AgentSession::spawn_with_bootstrap(
            Arc::new(EchoModelClient::new(Duration::ZERO)),
            "be brief",
            SessionBootstrap {
                loaded: Vec::new(),
                rollout_dir: Some(dir.clone()),
                cwd: "/tmp/demo".to_string(),
                fresh: true,
                ..SessionBootstrap::default()
            },
        );
        session.rename_session(0, "my chat");
        session.submit_user_turn("hello");
        collect_turn(&mut session).await;
        session.shutdown();
        while session.next_event().await.is_some() {}

        let loaded = crate::core::rollout::load_workspace_sessions(&dir).expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name.as_deref(), Some("my chat"));
        // The user message plus the test model's reply.
        assert_eq!(loaded[0].messages.len(), 2);
        assert_eq!(loaded[0].messages[0].content, "hello");
    }

    #[tokio::test]
    async fn shutdown_stops_the_worker() {
        let client = Arc::new(EchoModelClient::new(Duration::ZERO));
        let mut session = AgentSession::spawn(client);
        session.shutdown();

        let event = session.next_event().await.expect("shutdown event");
        assert!(matches!(event.msg, EventMsg::ShutdownComplete));
        assert!(session.next_event().await.is_none());
    }

    // -----------------------------------------------------------------------
    // Multi-provider switching
    // -----------------------------------------------------------------------

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
        let registry =
            crate::core::providers::ProviderRegistry::build(&[], vec![instance])
                .expect("valid registry");
        Arc::new(ModelPool::new(registry))
    }

    /// Test protocol: hands out recording clients keyed by the instance
    /// name. `spawn_with_pool` builds clients through the pool — bypassing
    /// any client a test passes to a `spawn_with_*` helper — so pooled
    /// tests that need to observe the actual requests register their
    /// handles with [`recording_handles`] and use this factory.
    struct RecordingFactory;

    type RecordingHandles = (
        Arc<Mutex<Vec<Vec<Message>>>>,
        Arc<Mutex<Vec<TurnOptions>>>,
    );

    static RECORDINGS: std::sync::OnceLock<Mutex<HashMap<String, RecordingHandles>>> =
        std::sync::OnceLock::new();

    fn recordings() -> &'static Mutex<HashMap<String, RecordingHandles>> {
        RECORDINGS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Pre-install the handles for instance `name` and return them.
    fn recording_handles(name: &str) -> RecordingHandles {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let options = Arc::new(Mutex::new(Vec::new()));
        recordings()
            .lock()
            .expect("lock")
            .insert(name.to_string(), (seen.clone(), options.clone()));
        (seen, options)
    }

    impl crate::core::providers::ProtocolFactory for RecordingFactory {
        fn id(&self) -> &'static str {
            "recording"
        }

        fn display_name(&self) -> &'static str {
            "Recording (testing)"
        }

        fn create(
            &self,
            instance: &ProviderInstance,
            _model: &str,
        ) -> Result<Arc<dyn ModelClient>, crate::core::model::ModelError> {
            let mut map = recordings().lock().expect("lock");
            let (seen, options) = map
                .entry(instance.name.clone())
                .or_insert_with(|| {
                    (
                        Arc::new(Mutex::new(Vec::new())),
                        Arc::new(Mutex::new(Vec::new())),
                    )
                });
            Ok(Arc::new(RecordingClient {
                seen: seen.clone(),
                options: options.clone(),
            }))
        }
    }

    /// A one-instance pool over [`RecordingFactory`].
    fn recording_pool(name: &str, models: Vec<ModelInfo>) -> Arc<ModelPool> {
        let instance = ProviderInstance {
            name: name.to_string(),
            protocol: Some("recording".to_string()),
            base_url: None,
            api_key: None,
            models,
            fetch_models: Some(false),
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: false,
        };
        let registry = crate::core::providers::ProviderRegistry::build(
            &[&RecordingFactory],
            vec![instance],
        )
        .expect("valid registry");
        Arc::new(ModelPool::new(registry))
    }

    #[tokio::test]
    async fn switch_model_swaps_the_backend_between_turns() {
        let mut session = AgentSession::spawn_with_pool(
            test_pool(),
            Some(&Selection::new("test", "one")),
            "be brief",
            SessionBootstrap::default(),
        )
        .expect("initial selection resolves");

        session.submit_user_turn("first");
        collect_turn(&mut session).await;

        session.switch_model("test/two");
        let event = session.next_event().await.expect("switch event");
        assert!(matches!(
            event.msg,
            EventMsg::ModelSwitched { model } if model == "test/two"
        ));

        // Ops are FIFO: the switch applies before the next turn streams.
        session.submit_user_turn("second");
        let events = collect_turn(&mut session).await;
        assert!(matches!(
            events.last().map(|event| &event.msg),
            Some(EventMsg::TurnComplete)
        ));
    }

    #[tokio::test]
    async fn switching_to_an_unknown_provider_reports_an_error_and_keeps_serving() {
        let mut session = AgentSession::spawn_with_pool(
            test_pool(),
            Some(&Selection::new("test", "one")),
            "be brief",
            SessionBootstrap::default(),
        )
        .expect("initial selection resolves");

        session.switch_model("other/m");
        let event = session.next_event().await.expect("error event");
        assert!(matches!(event.msg, EventMsg::Error { .. }));

        // The failed switch left the active backend in place.
        session.submit_user_turn("still there");
        let events = collect_turn(&mut session).await;
        assert!(matches!(
            events.last().map(|event| &event.msg),
            Some(EventMsg::TurnComplete)
        ));
    }

    #[tokio::test]
    async fn a_fixed_backend_rejects_switching() {
        let mut session =
            AgentSession::spawn(Arc::new(EchoModelClient::new(Duration::ZERO)));
        session.switch_model("test/one");
        let event = session.next_event().await.expect("error event");
        match &event.msg {
            EventMsg::Error { message } => assert!(message.contains("fixed backend"), "{message}"),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_route_answers_turns_with_guidance() {
        let mut session = AgentSession::spawn_with_pool(
            test_pool(),
            None,
            "be brief",
            SessionBootstrap::default(),
        )
        .expect("empty route spawns");

        session.submit_user_turn("hello");
        let event = session.next_event().await.expect("guidance event");
        match &event.msg {
            EventMsg::Error { message } => {
                assert!(message.contains("no model is configured"), "{message}");
            }
            other => panic!("expected guidance, got {other:?}"),
        }

        // The worker keeps serving; another turn gets the same guidance.
        session.submit_user_turn("still nothing");
        let event = session.next_event().await.expect("second guidance");
        assert!(matches!(event.msg, EventMsg::Error { .. }));
    }

    #[tokio::test]
    async fn a_half_specified_provider_answers_turns_with_guidance() {
        // URL but no key: the route arms (the model id is declared), and the
        // first turn names the missing half instead of firing a doomed
        // request.
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
        let registry = crate::core::providers::ProviderRegistry::build(&[], vec![instance])
            .expect("valid registry");
        let mut session = AgentSession::spawn_with_pool(
            Arc::new(ModelPool::new(registry)),
            Some(&Selection::new("openai", "m1")),
            "be brief",
            SessionBootstrap::default(),
        )
        .expect("selection resolves");

        session.submit_user_turn("hello");
        let event = session.next_event().await.expect("guidance event");
        match &event.msg {
            EventMsg::Error { message } => {
                assert!(message.contains("missing `api_key`"), "{message}");
            }
            other => panic!("expected guidance, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn switching_from_an_empty_route_arms_the_backend() {
        let mut session = AgentSession::spawn_with_pool(
            test_pool(),
            None,
            "be brief",
            SessionBootstrap::default(),
        )
        .expect("empty route spawns");

        session.switch_model("test/two");
        let event = session.next_event().await.expect("switch event");
        assert!(matches!(
            event.msg,
            EventMsg::ModelSwitched { model } if model == "test/two"
        ));

        // The empty backend migrated to pooled: the next turn streams.
        session.submit_user_turn("now it works");
        let events = collect_turn(&mut session).await;
        assert!(matches!(
            events.last().map(|event| &event.msg),
            Some(EventMsg::TurnComplete)
        ));
    }

    #[tokio::test]
    async fn model_selection_is_recorded_in_the_rollout() {
        let dir = std::env::temp_dir().join(format!(
            "codest-model-rollout-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let mut session = AgentSession::spawn_with_pool(
            test_pool(),
            Some(&Selection::new("test", "one")),
            "be brief",
            SessionBootstrap {
                loaded: Vec::new(),
                rollout_dir: Some(dir.clone()),
                cwd: "/tmp/demo".to_string(),
                fresh: true,
                ..SessionBootstrap::default()
            },
        )
        .expect("initial selection resolves");
        session.submit_user_turn("hello");
        collect_turn(&mut session).await;
        session.switch_model("test/two");
        let _ = session.next_event().await;
        session.submit_user_turn("again");
        collect_turn(&mut session).await;
        session.shutdown();
        while session.next_event().await.is_some() {}

        let loaded = crate::core::rollout::load_workspace_sessions(&dir).expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].meta.provider.as_deref(), Some("test"));
        assert_eq!(loaded[0].meta.model.as_deref(), Some("one"));
        assert_eq!(loaded[0].messages.len(), 4); // 2 turns × (user + assistant)

        // Each completed turn records its usage with the selection that
        // served it (the echo client reports deterministic numbers); the
        // replay aggregates both into the loaded session's stats.
        let content = std::fs::read_to_string(&loaded[0].path).expect("read rollout");
        let mut models = Vec::new();
        for line in content.lines() {
            let Ok(parsed) = serde_json::from_str::<crate::core::rollout::RolloutLine>(line)
            else {
                continue;
            };
            if let crate::core::rollout::RolloutItem::TurnUsage { model, input, output } =
                parsed.item
            {
                assert!(input > 0, "echo usage input is positive");
                assert!(output > 0, "echo usage output is positive");
                models.push(model);
            }
        }
        assert_eq!(
            models,
            vec![Some("test/one".to_string()), Some("test/two".to_string())]
        );
        assert_eq!(
            loaded[0].last_usage.map(|(input, _)| input > 0),
            Some(true)
        );
        assert!(loaded[0].total_input > 0);
        assert!(loaded[0].total_output > 0);
    }

    #[tokio::test]
    async fn switching_to_a_saved_session_restores_its_model() {
        let mut two = ModelInfo::from_id("two");
        two.default_level = Some("high".to_string());
        let instance = ProviderInstance {
            name: "test".to_string(),
            protocol: Some("test".to_string()),
            base_url: None,
            api_key: None,
            models: vec![ModelInfo::from_id("one"), two],
            fetch_models: Some(false),
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: false,
        };
        let registry =
            crate::core::providers::ProviderRegistry::build(&[], vec![instance])
                .expect("registry");
        let pool = Arc::new(ModelPool::new(registry));

        // A saved session that last ran on test/two.
        let loaded = LoadedSession {
            meta: crate::core::rollout::SessionMeta {
                version: 1,
                id: "saved".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                cwd: "/tmp/demo".to_string(),
                system_prompt: "be brief".to_string(),
                name: None,
                provider: Some("test".to_string()),
                model: Some("two".to_string()),
                profile: None,
            },
            path: std::path::PathBuf::from("/tmp/saved.jsonl"),
            model: Some("test/two".to_string()),
            last_usage: None,
            total_input: 0,
            total_output: 0,
            messages: vec![Message::user("hello"), Message::assistant("hi")],
            undo: Vec::new(),
            name: None,
            profile: None,
            tools: None,
        };

        // Plain startup: the fresh session runs on the configured test/one.
        let mut session = AgentSession::spawn_with_pool(
            pool,
            Some(&Selection::new("test", "one")),
            "be brief",
            SessionBootstrap {
                loaded: vec![loaded],
                rollout_dir: None,
                cwd: "/tmp/demo".to_string(),
                fresh: true,
                ..SessionBootstrap::default()
            },
        )
        .expect("spawn");

        // Becoming active again restores the session's model (and the effort
        // its `default_level` seeds) before the switch is confirmed.
        session.switch_session(0);
        let mut events = Vec::new();
        while let Some(event) = session.next_event().await {
            let done = matches!(event.msg, EventMsg::SessionSwitched { session: 0 });
            events.push(event);
            if done {
                break;
            }
        }
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(
            matches!(&events[0].msg, EventMsg::ModelSwitched { model } if model == "test/two"),
            "{events:?}"
        );
        assert!(
            matches!(&events[1].msg, EventMsg::ReasoningEffortSwitched { effort } if effort.as_deref() == Some("high")),
            "{events:?}"
        );

        // Switching to the fresh session (no recorded model) keeps the
        // current model: only the switch confirmation arrives.
        session.switch_session(1);
        let mut events = Vec::new();
        while let Some(event) = session.next_event().await {
            let done = matches!(event.msg, EventMsg::SessionSwitched { session: 1 });
            events.push(event);
            if done {
                break;
            }
        }
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(&events[0].msg, EventMsg::SessionSwitched { session: 1 }));

        // And back again: the backend already runs the session's model, so
        // the switch is a no-op — only the confirmation arrives.
        session.switch_session(0);
        let mut events = Vec::new();
        while let Some(event) = session.next_event().await {
            let done = matches!(event.msg, EventMsg::SessionSwitched { session: 0 });
            events.push(event);
            if done {
                break;
            }
        }
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(&events[0].msg, EventMsg::SessionSwitched { session: 0 }));
    }

    #[tokio::test]
    async fn set_reasoning_effort_reaches_the_client_and_the_rollout() {
        let dir = std::env::temp_dir().join(format!(
            "codest-effort-rollout-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let client = RecordingClient::new();
        let options = client.turn_options();
        let mut session = AgentSession::spawn_with_bootstrap(
            Arc::new(client),
            "be brief",
            SessionBootstrap {
                loaded: Vec::new(),
                rollout_dir: Some(dir.clone()),
                cwd: "/tmp/demo".to_string(),
                fresh: true,
                ..SessionBootstrap::default()
            },
        );

        session.submit_user_turn("first");
        collect_turn(&mut session).await;

        session.set_reasoning_effort(Some("high".to_string()));
        let event = session.next_event().await.expect("effort event");
        assert!(matches!(
            &event.msg,
            EventMsg::ReasoningEffortSwitched { effort } if effort.as_deref() == Some("high")
        ));

        session.submit_user_turn("second");
        collect_turn(&mut session).await;

        // Clearing the override reaches the client as no effort at all.
        session.set_reasoning_effort(None);
        let event = session.next_event().await.expect("clear event");
        assert!(matches!(
            &event.msg,
            EventMsg::ReasoningEffortSwitched { effort } if effort.is_none()
        ));

        session.submit_user_turn("third");
        collect_turn(&mut session).await;

        {
            let seen = options.lock().expect("lock");
            assert_eq!(seen.len(), 3);
            assert_eq!(seen[0].effort, None);
            assert_eq!(seen[1].effort, Some("high".to_string()));
            assert_eq!(seen[2].effort, None);
        }

        session.shutdown();
        while session.next_event().await.is_some() {}

        // Both switches are recorded for attribution; the replayed
        // conversation is unaffected (three user/assistant pairs).
        let loaded = crate::core::rollout::load_workspace_sessions(&dir).expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].messages.len(), 6);
        let content = std::fs::read_to_string(&loaded[0].path).expect("read rollout");
        assert_eq!(content.matches("reasoning_effort_switched").count(), 2);
        // The recording client reports no usage, so no per-turn usage record
        // is written.
        assert_eq!(content.matches("turn_usage").count(), 0);
    }

    #[tokio::test]
    async fn the_initial_model_default_level_seeds_the_effort() {
        let mut one = ModelInfo::from_id("one");
        one.default_level = Some("high".to_string());
        let (seen, options) = recording_handles("rec-seed");
        let mut session = AgentSession::spawn_with_pool(
            recording_pool("rec-seed", vec![one, ModelInfo::from_id("two")]),
            Some(&Selection::new("rec-seed", "one")),
            "be brief",
            SessionBootstrap::default(),
        )
        .expect("initial selection resolves");

        session.submit_user_turn("hello");
        collect_turn(&mut session).await;

        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 1);
        let options = options.lock().expect("lock");
        assert_eq!(options[0].effort, Some("high".to_string()));
    }

    #[tokio::test]
    async fn switching_profile_applies_its_presets_and_records_them() {
        let mut two = ModelInfo::from_id("two");
        two.default_level = Some("high".to_string());
        let (seen, options) = recording_handles("rec-profile");
        let mut profiles = ProfileSet::new();
        profiles.insert(
            "deep".to_string(),
            crate::core::config::Profile {
                default: Some("rec-profile/two".to_string()),
                effort: Some("low".to_string()),
                ..Default::default()
            },
        );
        profiles.insert(
            "calm".to_string(),
            crate::core::config::Profile {
                effort: Some("off".to_string()),
                ..Default::default()
            },
        );
        let mut session = AgentSession::spawn_with_pool(
            recording_pool("rec-profile", vec![ModelInfo::from_id("one"), two]),
            Some(&Selection::new("rec-profile", "one")),
            "be brief",
            SessionBootstrap {
                profiles: Arc::new(profiles),
                ..SessionBootstrap::default()
            },
        )
        .expect("initial selection resolves");

        // `deep` moves the selection to rec-profile/two — whose default_level
        // seeds `high` first (the same reseed a `/model` switch does), then
        // the profile's own effort wins with `low` — and the confirmation
        // names the profile.
        session.switch_profile("deep");
        let mut events = Vec::new();
        while let Some(event) = session.next_event().await {
            let done = matches!(event.msg, EventMsg::ProfileSwitched { .. });
            events.push(event);
            if done {
                break;
            }
        }
        assert_eq!(events.len(), 4, "{events:?}");
        assert!(
            matches!(&events[0].msg, EventMsg::ModelSwitched { model } if model == "rec-profile/two"),
            "{events:?}"
        );
        assert!(
            matches!(&events[1].msg, EventMsg::ReasoningEffortSwitched { effort } if effort.as_deref() == Some("high")),
            "{events:?}"
        );
        assert!(
            matches!(&events[2].msg, EventMsg::ReasoningEffortSwitched { effort } if effort.as_deref() == Some("low")),
            "{events:?}"
        );
        assert!(
            matches!(&events[3].msg, EventMsg::ProfileSwitched { name } if name == "deep"),
            "{events:?}"
        );

        // A profile without a selection keeps the current model; `off`
        // clears the effort override exactly like `/effort off`.
        session.switch_profile("calm");
        let mut events = Vec::new();
        while let Some(event) = session.next_event().await {
            let done = matches!(event.msg, EventMsg::ProfileSwitched { .. });
            events.push(event);
            if done {
                break;
            }
        }
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(
            matches!(&events[0].msg, EventMsg::ReasoningEffortSwitched { effort } if effort.is_none()),
            "{events:?}"
        );
        assert!(
            matches!(&events[1].msg, EventMsg::ProfileSwitched { name } if name == "calm"),
            "{events:?}"
        );

        // An unknown name is an error, not a switch.
        session.switch_profile("nope");
        let event = session.next_event().await.expect("error event");
        assert!(matches!(&event.msg, EventMsg::Error { .. }), "{event:?}");

        // The presets actually reached the wire: the last turn options carry
        // the cleared effort (no `effort` field at all).
        session.submit_user_turn("hello");
        collect_turn(&mut session).await;
        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 1);
        let options = options.lock().expect("lock");
        assert_eq!(options[0].effort, None);
    }

    #[tokio::test]
    async fn switching_models_adopts_the_new_default_level() {
        let mut one = ModelInfo::from_id("one");
        one.default_level = Some("high".to_string());
        let (seen, options) = recording_handles("rec-switch");
        let mut session = AgentSession::spawn_with_pool(
            recording_pool("rec-switch", vec![one, ModelInfo::from_id("two")]),
            Some(&Selection::new("rec-switch", "two")), // declares no default
            "be brief",
            SessionBootstrap::default(),
        )
        .expect("initial selection resolves");

        // A user override on a model without a default.
        session.set_reasoning_effort(Some("low".to_string()));
        let _ = session.next_event().await;
        session.submit_user_turn("first");
        collect_turn(&mut session).await;

        // Switching to the model WITH a default adopts it: `ModelSwitched`
        // followed by `ReasoningEffortSwitched`.
        session.switch_model("rec-switch/one");
        let switch = session.next_event().await.expect("switch event");
        assert!(matches!(switch.msg, EventMsg::ModelSwitched { .. }));
        let effort = session.next_event().await.expect("effort event");
        assert!(matches!(
            &effort.msg,
            EventMsg::ReasoningEffortSwitched { effort } if effort.as_deref() == Some("high")
        ));

        session.submit_user_turn("second");
        collect_turn(&mut session).await;

        // Switching back to the default-less model keeps the current effort.
        session.switch_model("rec-switch/two");
        let switch = session.next_event().await.expect("switch event");
        assert!(matches!(switch.msg, EventMsg::ModelSwitched { .. }));
        session.submit_user_turn("third");
        collect_turn(&mut session).await;

        let options = options.lock().expect("lock");
        assert_eq!(options.len(), 3);
        assert_eq!(options[0].effort, Some("low".to_string()));
        assert_eq!(options[1].effort, Some("high".to_string()));
        assert_eq!(
            options[2].effort,
            Some("high".to_string()),
            "kept when the model declares no default"
        );
        drop(options);
        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 3);
    }

    /// Collect events until one satisfies `done`, inclusive — the op-level
    /// sibling of [`collect_turn`].
    async fn collect_until(
        session: &mut AgentSession,
        done: impl Fn(&EventMsg) -> bool,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        while let Some(event) = session.next_event().await {
            let finished = done(&event.msg);
            events.push(event);
            if finished {
                break;
            }
        }
        events
    }

    /// Message count of the active conversation, via `InspectConversation`.
    async fn snapshot_len(session: &mut AgentSession) -> usize {
        session.inspect_conversation();
        let events = collect_until(session, |msg| {
            matches!(msg, EventMsg::ConversationSnapshot { .. })
        })
        .await;
        let [event] = events.as_slice() else {
            panic!("expected one snapshot, got {events:?}");
        };
        match &event.msg {
            EventMsg::ConversationSnapshot { messages } => messages.len(),
            _ => unreachable!("filtered above"),
        }
    }

    /// One turn against the echo model leaves a two-message transcript:
    /// the user text and `You said: "…"`.
    async fn transcript_of_two() -> AgentSession {
        let mut session = AgentSession::spawn(Arc::new(EchoModelClient::new(Duration::ZERO)));
        session.submit_user_turn("hello");
        collect_turn(&mut session).await;
        session
    }

    #[tokio::test]
    async fn inspect_returns_the_whole_conversation() {
        let mut session = transcript_of_two().await;

        session.inspect_conversation();
        let events = collect_until(&mut session, |msg| {
            matches!(msg, EventMsg::ConversationSnapshot { .. })
        })
        .await;
        let [event] = events.as_slice() else {
            panic!("expected exactly one snapshot, got {events:?}");
        };
        let EventMsg::ConversationSnapshot { messages } = &event.msg else {
            unreachable!("filtered above");
        };
        // System prompt first, then the transcript — exactly what a model
        // request would carry.
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[1].content, "hello");
        assert_eq!(messages[2].role, Role::Assistant);
    }

    #[tokio::test]
    async fn delete_messages_confirms_with_a_snapshot_and_replays() {
        let dir = std::env::temp_dir().join(format!(
            "codest-context-delete-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("tempdir");
        let client = Arc::new(EchoModelClient::new(Duration::ZERO));
        let mut session = AgentSession::spawn_with_bootstrap(
            client,
            "be brief",
            SessionBootstrap {
                rollout_dir: Some(dir.clone()),
                cwd: "/tmp/demo".to_string(),
                ..SessionBootstrap::default()
            },
        );
        session.submit_user_turn("hello");
        collect_turn(&mut session).await;

        // Delete the assistant reply.
        session.delete_messages(vec![1]);
        let events = collect_until(&mut session, |msg| {
            matches!(
                msg,
                EventMsg::ConversationSnapshot { .. } | EventMsg::Error { .. }
            )
        })
        .await;
        let [event] = events.as_slice() else {
            panic!("expected one confirmation, got {events:?}");
        };
        let EventMsg::ConversationSnapshot { messages } = &event.msg else {
            panic!("expected a snapshot, got {:?}", event.msg);
        };
        assert_eq!(messages.len(), 2, "system + the surviving user message");
        assert_eq!(messages[1].content, "hello");

        // A second undo source: an out-of-range index answers with an error.
        session.delete_messages(vec![9]);
        let events = collect_until(&mut session, |msg| {
            matches!(msg, EventMsg::ConversationSnapshot { .. } | EventMsg::Error { .. })
        })
        .await;
        let [event] = events.as_slice() else {
            panic!("expected one reply, got {events:?}");
        };
        assert!(matches!(&event.msg, EventMsg::Error { .. }));

        session.shutdown();
        drop(session);
        // The rollout replays to the edited transcript, checkpoint intact.
        let mut paths = crate::core::rollout::list_workspace_sessions(&dir).expect("list");
        let loaded = crate::core::rollout::load_session(&paths.remove(0)).expect("load");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].content, "hello");
        assert_eq!(loaded.undo.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_message_replaces_content_and_replays() {
        let mut session = transcript_of_two().await;

        session.edit_message(0, "hello (fixed)");
        let events = collect_until(&mut session, |msg| {
            matches!(msg, EventMsg::ConversationSnapshot { .. } | EventMsg::Error { .. })
        })
        .await;
        let [event] = events.as_slice() else {
            panic!("expected one reply, got {events:?}");
        };
        let EventMsg::ConversationSnapshot { messages } = &event.msg else {
            panic!("expected a snapshot, got {:?}", event.msg);
        };
        assert_eq!(messages[1].content, "hello (fixed)");
        assert_eq!(messages[1].role, Role::User);

        session.undo_context_edit();
        let events = collect_until(&mut session, |msg| {
            matches!(msg, EventMsg::ConversationSnapshot { .. } | EventMsg::Error { .. })
        })
        .await;
        let [event] = events.as_slice() else {
            panic!("expected one reply, got {events:?}");
        };
        let EventMsg::ConversationSnapshot { messages } = &event.msg else {
            panic!("expected a snapshot, got {:?}", event.msg);
        };
        assert_eq!(messages[1].content, "hello");
    }

    #[tokio::test]
    async fn undo_without_checkpoints_errors_and_keeps_the_transcript() {
        let mut session = transcript_of_two().await;

        session.undo_context_edit();
        let events = collect_until(&mut session, |msg| {
            matches!(msg, EventMsg::ConversationSnapshot { .. } | EventMsg::Error { .. })
        })
        .await;
        let [event] = events.as_slice() else {
            panic!("expected one reply, got {events:?}");
        };
        let EventMsg::Error { message } = &event.msg else {
            panic!("expected an error, got {:?}", event.msg);
        };
        assert!(message.contains("nothing left"), "{message}");
    }

    /// Deleting only the assistant half of a tool round breaks the
    /// tool_calls↔tool wire shape: the op is rejected and the transcript is
    /// untouched. (The viewer expands the cascade, so this guards a stale
    /// index set.)
    #[tokio::test]
    async fn delete_rejects_a_shape_breaking_cascade() {
        let client = ScriptedClient::new(vec![
            vec![StreamItem::ToolCalls(vec![tool_call("c1", "ping")])],
            vec![StreamItem::Delta("done".to_string())],
        ]);
        let mut session = AgentSession::spawn_with_bootstrap(
            Arc::new(client),
            "be brief",
            SessionBootstrap {
                tools: vec![PING_SPEC],
                executor: Some(Arc::new(MapExecutor)),
                ..SessionBootstrap::default()
            },
        );
        session.submit_user_turn("hi");
        collect_turn(&mut session).await;
        // Transcript: user, assistant(tool_calls), tool result, assistant.
        assert_eq!(snapshot_len(&mut session).await, 5, "system + 4 messages");

        session.delete_messages(vec![1]);
        let events = collect_until(&mut session, |msg| {
            matches!(msg, EventMsg::ConversationSnapshot { .. } | EventMsg::Error { .. })
        })
        .await;
        let [event] = events.as_slice() else {
            panic!("expected one reply, got {events:?}");
        };
        let EventMsg::Error { message } = &event.msg else {
            panic!("expected a rejection, got {:?}", event.msg);
        };
        // The orphaned tool result is what the validator trips on first.
        assert!(message.contains("tool result"), "{message}");

        // The transcript is intact: the rejected delete rolled back.
        assert_eq!(snapshot_len(&mut session).await, 5);
    }
}
