//! Deterministic in-process echo model.
//!
//! Serves two roles: the fake backend for offline unit tests, and the
//! implementation behind the built-in `test` protocol (pickable through
//! `providers.toml` with `protocol = "test"`), so a no-network demo does not
//! need the Python test server.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::core::model::{ModelClient, ModelError, ModelStream, StreamItem, TurnOptions};
use crate::core::protocol::{Message, Role};

/// A deterministic echo model. It streams word-sized chunks so session tests
/// exercise the same incremental path as a real backend.
#[derive(Debug, Clone)]
pub struct EchoModelClient {
    delay: Duration,
}

impl EchoModelClient {
    pub fn new(delay: Duration) -> Self {
        Self { delay }
    }
}

impl ModelClient for EchoModelClient {
    fn label(&self) -> String {
        "echo".to_string()
    }

    fn stream_chat(
        &self,
        messages: Vec<Message>,
        _options: TurnOptions,
        cancel: CancellationToken,
    ) -> ModelStream {
        let reply = reply_for(&messages);
        // A deterministic stand-in for a real usage report: the transcript
        // the request carried (system prompt included) and the reply it
        // produced, counted in chars. Varies across turns, so the context
        // display has something honest to show offline.
        let input: u64 = messages
            .iter()
            .map(|message| message.content.chars().count() as u64)
            .sum();
        let output = reply.chars().count() as u64;
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
                yield Ok::<_, ModelError>(StreamItem::Delta(token));
            }
            if cancel.is_cancelled() {
                return;
            }
            yield Ok(StreamItem::Usage { input, output });
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
    async fn streams_the_full_reply_then_usage() {
        let client = EchoModelClient::new(Duration::ZERO);
        let mut stream = client.stream_chat(
            vec![Message::system("be brief"), Message::user("ping")],
            TurnOptions::default(),
            CancellationToken::new(),
        );

        let mut out = String::new();
        let mut usage = None;
        while let Some(item) = stream.next().await {
            match item.expect("test stream never fails") {
                StreamItem::Delta(delta) => out.push_str(&delta),
                StreamItem::Usage { input, output } => usage = Some((input, output)),
                other => panic!("echo model streams deltas and usage, got {other:?}"),
            }
        }

        assert!(out.contains("You said: \"ping\""));
        assert!(out.contains("in-process test model"));
        // The request carried "be brief" (8) + "ping" (4) chars, the reply
        // is counted in chars too.
        assert_eq!(usage, Some((12, out.chars().count() as u64)));
    }

    #[tokio::test]
    async fn honours_cancellation() {
        let client = EchoModelClient::new(Duration::from_millis(5));
        let cancel = CancellationToken::new();
        let mut stream = client.stream_chat(
            vec![Message::user("a much longer message to stream")],
            TurnOptions::default(),
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
