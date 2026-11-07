//! Deterministic in-process model used by unit tests.
//!
//! This module is compiled only for `cargo test`. The shipped binary has no
//! offline backend: it always talks to an OpenAI-compatible endpoint. For
//! manual end-to-end testing, run `scripts/openai_test_server.py` and point
//! `codest` at it.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::core::model::{ModelClient, ModelError, ModelStream};
use crate::core::protocol::{Message, Role};

/// A deterministic echo model. It streams word-sized chunks so session tests
/// exercise the same incremental path as a real backend.
#[derive(Debug, Clone)]
pub(crate) struct FakeModelClient {
    delay: Duration,
}

impl FakeModelClient {
    pub(crate) fn new(delay: Duration) -> Self {
        Self { delay }
    }
}

impl ModelClient for FakeModelClient {
    fn label(&self) -> String {
        "test-model".to_string()
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

    format!("You said: \"{last_user}\"\n\nThis is the in-process test model.")
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
    async fn streams_the_full_reply() {
        let client = FakeModelClient::new(Duration::ZERO);
        let mut stream = client.stream_chat(vec![Message::user("ping")], CancellationToken::new());

        let mut out = String::new();
        while let Some(item) = stream.next().await {
            out.push_str(&item.expect("test stream never fails"));
        }

        assert!(out.contains("You said: \"ping\""));
        assert!(out.contains("in-process test model"));
    }

    #[tokio::test]
    async fn honours_cancellation() {
        let client = FakeModelClient::new(Duration::from_millis(5));
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
