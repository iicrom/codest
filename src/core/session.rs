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
use crate::core::protocol::Event;
use crate::core::protocol::EventMsg;
use crate::core::protocol::Op;
use crate::core::protocol::Submission;

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
        let (tx, submission_rx) = mpsc::unbounded_channel();
        let (event_tx, rx) = mpsc::unbounded_channel();
        let interrupt = InterruptHandle::default();

        tokio::spawn(run_worker(
            client,
            system_prompt.into(),
            submission_rx,
            event_tx,
            interrupt.clone(),
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
}

async fn run_worker(
    client: Arc<dyn ModelClient>,
    system_prompt: String,
    mut rx: mpsc::UnboundedReceiver<Submission>,
    tx: mpsc::UnboundedSender<Event>,
    interrupt: InterruptHandle,
) {
    let mut conversation = Conversation::new(system_prompt);
    let mut active: Option<ActiveTurn> = None;

    loop {
        if let Some(mut turn) = active.take() {
            interrupt.install(turn.cancel.clone());
            run_turn(&mut turn, &mut conversation, &tx, &interrupt).await;
            continue;
        }

        let Some(submission) = rx.recv().await else {
            break;
        };

        match submission.op {
            Op::UserTurn { text } => {
                conversation.push_user(text);
                let cancel = CancellationToken::new();
                let stream = client.stream_chat(conversation.snapshot(), cancel.clone());
                let _ = tx.send(Event::new(submission.id.clone(), EventMsg::TurnStarted));
                active = Some(ActiveTurn {
                    id: submission.id,
                    stream,
                    cancel,
                    assistant: String::new(),
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
    tx: &mpsc::UnboundedSender<Event>,
    interrupt: &InterruptHandle,
) {
    loop {
        tokio::select! {
            biased;
            _ = turn.cancel.cancelled() => {
                finalize_aborted(turn, conversation, tx);
                interrupt.clear();
                return;
            }
            item = turn.stream.next() => {
                match item {
                    Some(Ok(delta)) => {
                        turn.assistant.push_str(&delta);
                        let _ = tx.send(Event::new(
                            turn.id.clone(),
                            EventMsg::AgentMessageDelta { delta },
                        ));
                    }
                    Some(Err(error)) => {
                        finalize_error(turn, conversation, tx, &error.to_string());
                        interrupt.clear();
                        return;
                    }
                    None => {
                        finalize_complete(turn, conversation, tx);
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
    tx: &mpsc::UnboundedSender<Event>,
) {
    conversation.push_assistant(turn.assistant.clone());
    let _ = tx.send(Event::new(
        turn.id.clone(),
        EventMsg::AgentMessage {
            message: turn.assistant.clone(),
        },
    ));
    let _ = tx.send(Event::new(turn.id.clone(), EventMsg::TurnComplete));
}

fn finalize_aborted(
    turn: &ActiveTurn,
    conversation: &mut Conversation,
    tx: &mpsc::UnboundedSender<Event>,
) {
    if !turn.assistant.is_empty() {
        conversation.push_assistant(turn.assistant.clone());
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
    tx: &mpsc::UnboundedSender<Event>,
    message: &str,
) {
    // Keep any partial text in the transcript so the next turn has context.
    if !turn.assistant.is_empty() {
        conversation.push_assistant(turn.assistant.clone());
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
    use crate::core::mock::MockModelClient;
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
        let client = Arc::new(MockModelClient::new(Duration::ZERO));
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

    #[tokio::test]
    async fn second_turn_sends_the_full_history() {
        struct RecordingClient {
            seen: Arc<Mutex<Vec<Vec<Message>>>>,
        }

        impl ModelClient for RecordingClient {
            fn label(&self) -> String {
                "recording".to_string()
            }

            fn stream_chat(
                &self,
                messages: Vec<Message>,
                _cancel: CancellationToken,
            ) -> ModelStream {
                self.seen.lock().expect("lock").push(messages);
                Box::pin(futures_util::stream::once(async {
                    Ok::<String, crate::core::model::ModelError>("ok".to_string())
                }))
            }
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let client = Arc::new(RecordingClient { seen: seen.clone() });
        let mut session = AgentSession::spawn(client);

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
    async fn interrupt_keeps_partial_output() {
        let client = Arc::new(MockModelClient::new(Duration::from_millis(5)));
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
    async fn shutdown_stops_the_worker() {
        let client = Arc::new(MockModelClient::new(Duration::ZERO));
        let mut session = AgentSession::spawn(client);
        session.shutdown();

        let event = session.next_event().await.expect("shutdown event");
        assert!(matches!(event.msg, EventMsg::ShutdownComplete));
        assert!(session.next_event().await.is_none());
    }
}
