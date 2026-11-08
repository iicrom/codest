//! The agent worker: owns the conversation, consumes submissions, emits events.
//!
//! This is the minimal analogue of codex's `CodexConversation`: the front-end
//! gets a handle with `submit`/`next_event` and has no access to model code.
//! A `CancellationToken` is installed for the in-flight turn so `interrupt`
//! works without racing the submission queue.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::core::conversation::Conversation;
use crate::core::conversation::DEFAULT_SYSTEM_PROMPT;
use crate::core::model::ModelClient;
use crate::core::model::ModelStream;
use crate::core::model::StreamItem;
use crate::core::protocol::Event;
use crate::core::protocol::EventMsg;
use crate::core::protocol::Op;
use crate::core::protocol::Submission;
use crate::core::providers::ModelPool;
use crate::core::providers::Selection;
use crate::core::rollout::LoadedSession;
use crate::core::rollout::RolloutFile;
use crate::core::rollout::RolloutItem;
use crate::core::rollout::RolloutWriter;

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
}

/// What the worker starts from: sessions restored from disk plus the rollout
/// directory to persist into. `rollout_dir: None` runs fully in memory (used
/// by tests); restore failures degrade to an empty list.
#[derive(Debug, Default)]
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
            match writer.start_session(&self.system_prompt, provider, model) {
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

async fn run_worker(
    backend: ModelBackend,
    system_prompt: String,
    mut rx: mpsc::UnboundedReceiver<Submission>,
    tx: mpsc::UnboundedSender<Event>,
    interrupt: InterruptHandle,
    bootstrap: SessionBootstrap,
) {
    // One worker owns every conversation and streams at most one turn at a
    // time. Sessions are append-only, so the creation-order index is stable
    // and front-ends can mirror it with their own per-session view state.
    let writer = bootstrap
        .rollout_dir
        .map(|dir| RolloutWriter::new(dir, bootstrap.cwd));
    let mut backend = backend;
    let mut sessions = Vec::new();
    let mut slots = Vec::new();

    for loaded in &bootstrap.loaded {
        let mut conversation = Conversation::new(loaded.meta.system_prompt.clone());
        for message in &loaded.messages {
            conversation.push_message(message.clone());
        }
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
        sessions.push(conversation);
        slots.push(slot);
    }

    // Plain startup opens one fresh session on top of the restored ones; the
    // restore modes only fall back to one when nothing was restored.
    if bootstrap.fresh || sessions.is_empty() {
        sessions.push(Conversation::new(system_prompt.clone()));
        slots.push(RolloutSlot::inactive());
    }

    let mut rollouts = Rollouts {
        writer,
        system_prompt: system_prompt.clone(),
        selection: backend.selection().cloned(),
        slots,
    };
    let mut current = sessions.len() - 1;
    let mut active: Option<ActiveTurn> = None;

    loop {
        if let Some(mut turn) = active.take() {
            interrupt.install(turn.cancel.clone());
            run_turn(
                &mut turn,
                &mut sessions[current],
                &mut rollouts,
                current,
                &tx,
                &interrupt,
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
                let stream =
                    backend
                        .client()
                        .stream_chat(sessions[current].snapshot(), cancel.clone());
                let _ = tx.send(Event::new(submission.id.clone(), EventMsg::TurnStarted));
                active = Some(ActiveTurn {
                    id: submission.id,
                    stream,
                    cancel,
                    assistant: String::new(),
                    usage: None,
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
                current = sessions.len() - 1;
                rollouts.push_slot();
                let _ = tx.send(Event::new(
                    submission.id,
                    EventMsg::SessionStarted { session: current },
                ));
            }
            Op::SwitchSession { session } => {
                if session < sessions.len() {
                    current = session;
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
            Op::SwitchModel { selection } => match backend.switch(&selection) {
                Ok(parsed) => {
                    rollouts.selection = Some(parsed.clone());
                    rollouts.append(
                        current,
                        RolloutItem::ModelSwitched {
                            provider: parsed.provider.clone(),
                            model: parsed.model.clone(),
                        },
                    );
                    let _ = tx.send(Event::new(
                        submission.id,
                        EventMsg::ModelSwitched {
                            model: parsed.as_str(),
                        },
                    ));
                }
                Err(message) => {
                    let _ = tx.send(Event::new(submission.id, EventMsg::Error { message }));
                }
            },
            Op::Shutdown => {
                let _ = tx.send(Event::new(submission.id, EventMsg::ShutdownComplete));
                break;
            }
        }
    }
}

async fn run_turn(
    turn: &mut ActiveTurn,
    conversation: &mut Conversation,
    rollouts: &mut Rollouts,
    current: usize,
    tx: &mpsc::UnboundedSender<Event>,
    interrupt: &InterruptHandle,
) {
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
                        // The minimal front-end ignores reasoning output; the
                        // event keeps the bus honest about what was produced.
                        let _ = tx.send(Event::new(
                            turn.id.clone(),
                            EventMsg::AgentThinkingDelta { delta },
                        ));
                    }
                    Some(Ok(StreamItem::Usage { input, output })) => {
                        turn.usage = Some((input, output));
                    }
                    Some(Err(error)) => {
                        finalize_error(turn, conversation, rollouts, current, tx, &error.to_string());
                        interrupt.clear();
                        return;
                    }
                    None => {
                        finalize_complete(turn, conversation, rollouts, current, tx);
                        interrupt.clear();
                        return;
                    }
                }
            }
        }
    }
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

    /// Records the full snapshot passed to each model call.
    #[derive(Clone, Default)]
    struct RecordingClient {
        seen: Arc<Mutex<Vec<Vec<Message>>>>,
    }

    impl RecordingClient {
        fn new() -> Self {
            Self::default()
        }

        fn snapshots(&self) -> Arc<Mutex<Vec<Vec<Message>>>> {
            self.seen.clone()
        }
    }

    impl ModelClient for RecordingClient {
        fn label(&self) -> String {
            "recording".to_string()
        }

        fn stream_chat(&self, messages: Vec<Message>, _cancel: CancellationToken) -> ModelStream {
            self.seen.lock().expect("lock").push(messages);
            Box::pin(futures_util::stream::once(async {
                Ok::<_, crate::core::model::ModelError>(StreamItem::Delta("ok".to_string()))
            }))
        }
    }

    #[tokio::test]
    async fn second_turn_sends_the_full_history() {
        let client = RecordingClient::new();
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
            requires_api_key: false,
        };
        let registry =
            crate::core::providers::ProviderRegistry::build(&[], vec![instance])
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
    }
}
