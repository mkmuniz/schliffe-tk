//! Server-Sent Events parser (WHATWG `text/event-stream`), used by the
//! Streamable HTTP transport (specs.md §6.3). MCP carries one JSON-RPC
//! message per SSE event `data` field; a single event may span several
//! `data:` lines, which the spec joins with `\n`. The naive
//! `strip_prefix("data:")`-per-line approach this replaces split such a
//! message into fragments, each of which then failed to parse as JSON.
//!
//! This is a push parser: feed it one line at a time (newline already
//! stripped). It returns a completed event only when a blank line dispatches
//! it, matching the spec's event-boundary rule.

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The event type (`event:` field); defaults to "message" when absent.
    pub event: String,
    /// The joined `data:` field(s) — one JSON-RPC message for MCP.
    pub data: String,
}

#[derive(Default)]
pub struct SseParser {
    event: String,
    data: String,
    saw_data: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one line (without its trailing newline). Returns a completed
    /// event when this line is the blank dispatch line and data was buffered.
    pub fn feed_line(&mut self, line: &str) -> Option<SseEvent> {
        // A trailing CR survives when the stream uses CRLF and the caller
        // split on LF only.
        let line = line.strip_suffix('\r').unwrap_or(line);

        if line.is_empty() {
            return self.dispatch();
        }
        // Comment line (spec: a line starting with ':' is ignored).
        if line.starts_with(':') {
            return None;
        }

        let (field, value) = match line.split_once(':') {
            // Spec: strip a single leading space from the value.
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            // A line with no colon is a field name with an empty value.
            None => (line, ""),
        };

        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                if self.saw_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.saw_data = true;
            }
            // `id`, `retry` and unknown fields are ignored (id tracking is
            // added with reconnect support).
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = std::mem::take(&mut self.event);
        let data = std::mem::take(&mut self.data);
        let saw_data = std::mem::replace(&mut self.saw_data, false);
        if !saw_data {
            return None;
        }
        Some(SseEvent {
            event: if event.is_empty() {
                "message".to_string()
            } else {
                event
            },
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(input: &str) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        let mut out = Vec::new();
        for line in input.split('\n') {
            if let Some(event) = parser.feed_line(line) {
                out.push(event);
            }
        }
        out
    }

    #[test]
    fn single_data_line() {
        let events = collect("data: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
        assert_eq!(events[0].event, "message");
    }

    #[test]
    fn multiline_data_is_joined_with_newline() {
        let events = collect("data: {\ndata:   \"a\": 1\ndata: }\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\n  \"a\": 1\n}");
    }

    #[test]
    fn multiple_events_separated_by_blank_lines() {
        let events = collect("data: first\n\ndata: second\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "first");
        assert_eq!(events[1].data, "second");
    }

    #[test]
    fn event_type_is_captured() {
        let events = collect("event: ping\ndata: x\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "ping");
    }

    #[test]
    fn comment_and_blank_without_data_dispatch_nothing() {
        assert!(collect(": keep-alive\n\n").is_empty());
        assert!(collect("\n\n\n").is_empty());
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        let events = collect("data: hi\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hi");
    }

    #[test]
    fn no_space_after_colon_is_accepted() {
        let events = collect("data:hi\n\n");
        assert_eq!(events[0].data, "hi");
    }
}
