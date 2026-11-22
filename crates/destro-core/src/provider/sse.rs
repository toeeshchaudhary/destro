//! Incremental Server-Sent Events parsing.
//!
//! [`SseParser`] is a pure state machine: feed it raw bytes in any grouping and
//! it returns the events completed by those bytes. It has no clock, no I/O, and
//! no knowledge of OpenAI, which makes the tricky boundary cases (a line split
//! across TCP chunks, a multi-byte UTF-8 character split across chunks, a
//! comment line, the `[DONE]` sentinel) testable without a network.

/// One complete Server-Sent Event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SseEvent {
    /// A `data:` payload with the prefix stripped. Multiple `data:` lines in
    /// one event are joined with `\n`, as the SSE specification requires.
    Data(String),
    /// The OpenAI end-of-stream sentinel, `data: [DONE]`.
    Done,
}

/// A streaming SSE parser.
///
/// Feed received bytes to [`SseParser::push`]; call [`SseParser::finish`] once
/// the transport ends to flush a trailing event that lacked its final blank
/// line. Bytes are buffered until a complete line is available, so arbitrary
/// chunk boundaries are safe. Parsing never panics: invalid UTF-8 is replaced
/// lossily.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Bytes of the not-yet-terminated line.
    buffer: Vec<u8>,
    /// Data lines of the event currently being assembled.
    pending: String,
    /// Set once `[DONE]` was seen; later bytes are ignored.
    done: bool,
}

impl SseParser {
    /// Create an empty parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the `[DONE]` sentinel has been consumed.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Feed bytes received from the transport and return every event completed
    /// by them, in order.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        self.buffer.extend_from_slice(bytes);

        let mut start = 0usize;
        while let Some(offset) = self.buffer[start..].iter().position(|b| *b == b'\n') {
            let end = start + offset;
            let line = String::from_utf8_lossy(&self.buffer[start..end]).into_owned();
            start = end + 1;
            self.process_line(&line, &mut out);
            if self.done {
                start = self.buffer.len();
                break;
            }
        }
        if start > 0 {
            self.buffer.drain(..start);
        }
        out
    }

    /// Flush whatever is buffered at end of stream: a final line without a
    /// trailing newline and an event without its final blank line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        if !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&self.buffer).into_owned();
            self.buffer.clear();
            self.process_line(&line, &mut out);
        }
        self.flush_pending(&mut out);
        out
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        // A trailing `\r` from CRLF framing is not part of the payload.
        let line = line.strip_suffix('\r').unwrap_or(line);

        // A blank line dispatches the event being assembled.
        if line.is_empty() {
            self.flush_pending(out);
            return;
        }
        // `:` starts a comment; `event:`, `id:`, and `retry:` fields carry
        // nothing the OpenAI stream needs.
        let Some(value) = line.strip_prefix("data:") else {
            return;
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        if !self.pending.is_empty() {
            self.pending.push('\n');
        }
        self.pending.push_str(value);
    }

    fn flush_pending(&mut self, out: &mut Vec<SseEvent>) {
        if self.pending.is_empty() {
            return;
        }
        let data = std::mem::take(&mut self.pending);
        if data.trim() == "[DONE]" {
            self.done = true;
            out.push(SseEvent::Done);
        } else {
            out.push(SseEvent::Data(data));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_event_in_one_push() {
        let mut parser = SseParser::new();
        let events = parser.push(b"data: {\"a\":1}\n\n");
        assert_eq!(events, vec![SseEvent::Data("{\"a\":1}".into())]);
    }

    #[test]
    fn event_split_across_three_pushes() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"da").is_empty());
        assert!(parser.push(b"ta: {\"text\":\"hel").is_empty());
        let events = parser.push(b"lo\"}\n\n");
        assert_eq!(events, vec![SseEvent::Data("{\"text\":\"hello\"}".into())]);
    }

    #[test]
    fn comment_and_blank_lines_are_ignored() {
        let mut parser = SseParser::new();
        assert!(parser.push(b": keep-alive\n\n").is_empty());
        assert!(parser.push(b"\n\n").is_empty());
        assert!(parser.push(b": another comment\n").is_empty());
        let events = parser.push(b"data: ok\n\n");
        assert_eq!(events, vec![SseEvent::Data("ok".into())]);
    }

    #[test]
    fn done_stops_the_stream() {
        let mut parser = SseParser::new();
        assert_eq!(parser.push(b"data: [DONE]\n\n"), vec![SseEvent::Done]);
        assert!(parser.is_done());
        assert!(parser.push(b"data: ignored\n\n").is_empty());
        assert!(parser.finish().is_empty());
    }

    #[test]
    fn several_events_in_one_push() {
        let mut parser = SseParser::new();
        let events = parser.push(b"data: one\n\ndata: two\n\n");
        assert_eq!(
            events,
            vec![SseEvent::Data("one".into()), SseEvent::Data("two".into())]
        );
    }

    #[test]
    fn multiple_data_lines_join_with_newlines() {
        let mut parser = SseParser::new();
        let events = parser.push(b"data: a\ndata: b\n\n");
        assert_eq!(events, vec![SseEvent::Data("a\nb".into())]);
    }

    #[test]
    fn crlf_framing_is_accepted() {
        let mut parser = SseParser::new();
        let events = parser.push(b"data: ok\r\n\r\n");
        assert_eq!(events, vec![SseEvent::Data("ok".into())]);
    }

    #[test]
    fn utf8_split_across_chunks() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: \xe2").is_empty());
        assert!(parser.push(b"\x9c").is_empty());
        let events = parser.push(b"\x85\n\n");
        assert_eq!(events, vec![SseEvent::Data("\u{2705}".into())]);
    }

    #[test]
    fn invalid_utf8_does_not_panic() {
        let mut parser = SseParser::new();
        let events = parser.push(b"data: \xff\n\n");
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn finish_flushes_an_unterminated_event() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: tail").is_empty());
        let events = parser.finish();
        assert_eq!(events, vec![SseEvent::Data("tail".into())]);
    }
}
