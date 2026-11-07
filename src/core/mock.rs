//! Offline model backend used for local demos and tests.
//!
//! Codex has a fake/OSS provider story; this is the smaller equivalent: a
//! deterministic stream with no network access, so the TUI can be exercised
//! end-to-end without credentials.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::core::model::{ModelClient, ModelError, ModelStream};
use crate::core::protocol::{Message, Role};

/// A deterministic echo model. It streams word-sized chunks so the TUI's
/// incremental rendering path is exercised exactly like a real backend.
#[derive(Debug, Clone)]
pub struct MockModelClient {
    delay: Duration,
}

impl Default for MockModelClient {
    fn default() -> Self {
        Self {
            delay: Duration::from_millis(18),
        }
    }
}

impl MockModelClient {
    pub fn new(delay: Duration) -> Self {
        Self { delay }
    }
}

impl ModelClient for MockModelClient {
    fn label(&self) -> String {
        "mock".to_string()
    }

    fn stream_chat(&self, messages: Vec<Message>, cancel: CancellationToken) -> ModelStream {
        let reply = reply_for(&messages);
        let delay = self.delay;

        Box::pin(async_stream::stream! {
            for token in tokenize(&reply) {
                if cancel.is_cancelled() {
                    return;
                }
                if !delay.is_zero() {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                yield Ok::<String, ModelError>(token);
            }
        })
    }
}

fn reply_for(messages: &[Message]) -> String {
    let last_user = messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(|message| message.content.trim())
        .filter(|text| !text.is_empty())
        .unwrap_or("(empty message)");

    format!(
        "You said: \"{last_user}\"\n\n\
         I'm the offline mock model, so no network request was made. \
         This is the cst TUI running on top of the minimal core: streaming \
         deltas arrive over the event queue and are rendered as they land.\n\n\
         Set CST_API_KEY (or pass --api-key) to talk to a real \
         OpenAI-compatible endpoint."
    )
}

/// Split text into stream-sized chunks, keeping whitespace attached to the
/// preceding word. The result is lossless: concatenating it reproduces `text`.
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for piece in text.split_inclusive([' ', '\n']) {
        if !piece.is_empty() {
            tokens.push(piece.to_string());
        }
    }
    if tokens.is_empty() {
        tokens.push(String::new());
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn tokenize_is_lossless() {
        let text = "hello  world\n\nsecond line";
        assert_eq!(tokenize(text).concat(), text);
    }

    #[tokio::test]
    async fn mock_streams_the_full_reply() {
        let client = MockModelClient::new(Duration::ZERO);
        let mut stream = client.stream_chat(vec![Message::user("ping")], CancellationToken::new());

        let mut out = String::new();
        while let Some(item) = stream.next().await {
            out.push_str(&item.expect("mock stream never fails"));
        }

        assert!(out.contains("You said: \"ping\""));
        assert!(out.contains("offline mock model"));
    }

    #[tokio::test]
    async fn mock_honours_cancellation() {
        let client = MockModelClient::new(Duration::from_millis(5));
        let cancel = CancellationToken::new();
        let mut stream = client.stream_chat(
            vec![Message::user("a much longer message to stream")],
            cancel.clone(),
        );

        // Consume a single delta, then cancel.
        let _ = stream.next().await;
        cancel.cancel();

        let mut remaining = 0;
        while let Some(item) = stream.next().await {
            item.expect("no errors after cancel");
            remaining += 1;
            assert!(remaining < 10, "stream kept yielding after cancellation");
        }
    }
}
