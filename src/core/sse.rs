//! Shared SSE (`text/event-stream`) framing.
//!
//! Extracted from the OpenAI-compatible client so every protocol that streams
//! over SSE — the built-in one and any plugin protocol — shares one decoder
//! instead of reimplementing (and re-bugging) chunk-boundary handling.
//!
//! The decoder follows the event-stream grammar: lines accumulate into a frame
//! that is dispatched on the blank line, `data:` lines of one frame are joined
//! with `\n`, and `event:` names the frame. Chunks may split lines, frames,
//! and multi-byte UTF-8 characters at arbitrary byte boundaries.

/// One dispatched event: its optional `event:` name and its `data:` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    /// Raw bytes, not a `String`: a network chunk may end in the middle of a
    /// multi-byte UTF-8 character.
    buffer: Vec<u8>,
    /// The frame being accumulated, carried across chunk boundaries.
    pending: PendingFrame,
}

#[derive(Debug, Default)]
struct PendingFrame {
    event: Option<String>,
    data_lines: Vec<String>,
}

impl PendingFrame {
    fn is_empty(&self) -> bool {
        self.event.is_none() && self.data_lines.is_empty()
    }

    fn take(&mut self) -> SseFrame {
        SseFrame {
            event: self.event.take(),
            data: std::mem::take(&mut self.data_lines).join("\n"),
        }
    }
}

impl SseDecoder {
    /// Append a chunk and return all complete frames. Frames are dispatched on
    /// the blank line that closes them; a chunk may end mid-frame.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();

        while let Some(index) = self.buffer.iter().position(|&byte| byte == b'\n') {
            let line_bytes: Vec<u8> = self.buffer.drain(..=index).collect();
            let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]);
            self.process_line(line.trim_end_matches('\r'), &mut out);
        }

        out
    }

    /// Flush a frame that was complete except for its terminating blank line —
    /// the "well-behaved server ends with a newline" tolerance, generalized:
    /// buffered tail lines are parsed, then a pending frame is dispatched.
    pub fn finish(&mut self) -> Option<SseFrame> {
        if !self.buffer.is_empty() {
            let bytes = std::mem::take(&mut self.buffer);
            let text = String::from_utf8_lossy(&bytes);
            let mut out = Vec::new();
            for line in text.split('\n') {
                self.process_line(line.trim_end_matches('\r'), &mut out);
            }
            // Frames dispatched inside the tail are already complete; the
            // caller of `feed` never saw them, but none are pending either.
        }
        if self.pending.is_empty() {
            None
        } else {
            Some(self.pending.take())
        }
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<SseFrame>) {
        if line.is_empty() {
            if !self.pending.is_empty() {
                out.push(self.pending.take());
            }
            return;
        }
        if let Some(data) = line.strip_prefix("data:") {
            self.pending
                .data_lines
                .push(strip_field_space(data).to_string());
        } else if let Some(event) = line.strip_prefix("event:") {
            self.pending.event = Some(strip_field_space(event).to_string());
        }
        // `id:`, `retry:`, and comment lines carry nothing this agent needs.
    }
}

/// SSE field syntax: `field: value` — one optional leading space is part of
/// the separator, not the value.
fn strip_field_space(value: &str) -> &str {
    value.strip_prefix(' ').unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(frame: &SseFrame) -> &str {
        &frame.data
    }

    #[test]
    fn dispatches_on_the_blank_line_and_joins_data_lines() {
        let mut decoder = SseDecoder::default();
        let frames = decoder.feed(b"data: first\ndata: second\nevent: add\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(data(&frames[0]), "first\nsecond");
        assert_eq!(frames[0].event.as_deref(), Some("add"));

        // A multi-line payload is not dispatched line by line.
        let frames = decoder.feed(b"data: {\n\ndata: done\n\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(data(&frames[0]), "{");
        assert_eq!(data(&frames[1]), "done");
    }

    #[test]
    fn handles_split_frames() {
        let mut decoder = SseDecoder::default();
        assert!(decoder
            .feed(b"data: {\"choices\":[{\"delta\"")
            .is_empty());
        assert!(decoder.feed(b":{\"content\":\"he\"}}]}\n").is_empty());
        // The frame is only complete after its blank line.
        let frames = decoder.feed(b"\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(data(&frames[0]), "{\"choices\":[{\"delta\":{\"content\":\"he\"}}]}");

        let frames = decoder.feed(b"data: [DONE]\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(data(&frames[0]), "[DONE]");
    }

    #[test]
    fn survives_utf8_split_across_chunks() {
        let mut decoder = SseDecoder::default();
        let frame = "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n".as_bytes();
        let split = frame.iter().position(|&byte| byte >= 0x80).expect("CJK") + 1;

        // The first chunk ends inside the three-byte character; no frame can
        // be emitted yet and no replacement characters are introduced.
        assert!(decoder.feed(&frame[..split]).is_empty());
        let frames = decoder.feed(&frame[split..]);
        assert_eq!(frames.len(), 1);
        assert_eq!(data(&frames[0]), "{\"choices\":[{\"delta\":{\"content\":\"你\"}}]}");
    }

    #[test]
    fn ignores_non_field_lines_and_handles_crlf() {
        let mut decoder = SseDecoder::default();
        let frames = decoder.feed(
            b": comment\r\nevent: message\r\ndata: {\"x\":1}\r\n\r\n",
        );
        assert_eq!(frames.len(), 1);
        assert_eq!(data(&frames[0]), "{\"x\":1}");
        assert_eq!(frames[0].event.as_deref(), Some("message"));
    }

    #[test]
    fn strips_one_leading_space_only() {
        let mut decoder = SseDecoder::default();
        // Two `data:` lines in one frame join with `\n`; each loses exactly
        // one leading space.
        let frames = decoder.feed(b"data:  two spaces\ndata:none\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(data(&frames[0]), " two spaces\nnone");
    }

    #[test]
    fn finish_flushes_a_frame_without_its_final_newline() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.feed(b"data: {\"x\":1}").is_empty());
        let frame = decoder.finish().expect("pending frame");
        assert_eq!(data(&frame), "{\"x\":1}");

        // Whitespace-only remains are not a frame.
        let mut decoder = SseDecoder::default();
        decoder.feed(b"data: a\n\n");
        assert!(decoder.finish().is_none());
    }
}
