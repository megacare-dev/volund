//! Server-sent events, as far as Chat Completions streams use them: `data:` lines joined into one
//! payload per event, events ended by a blank line, `:` comment lines (keep-alives) skipped.

#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    data: String,
    has_data: bool,
}

impl Decoder {
    /// Feeds bytes as they arrive and returns the payloads of the events they complete.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Some(event) = self.line(&String::from_utf8_lossy(&line)) {
                events.push(event);
            }
        }
        events
    }

    /// The last event, when the stream ends without the blank line that closes it.
    pub fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.buf);
        if !rest.is_empty() {
            let text = String::from_utf8_lossy(&rest).into_owned();
            if let Some(event) = self.line(text.trim_end_matches('\r')) {
                return Some(event);
            }
        }
        self.dispatch()
    }

    fn line(&mut self, line: &str) -> Option<String> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            if self.has_data {
                self.data.push('\n');
            }
            self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
            self.has_data = true;
        }
        None
    }

    fn dispatch(&mut self) -> Option<String> {
        if !self.has_data {
            return None;
        }
        self.has_data = false;
        Some(std::mem::take(&mut self.data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_across_chunks_and_line_endings() {
        let mut d = Decoder::default();
        assert!(d.push(b"data: {\"a\"").is_empty());
        assert_eq!(
            d.push(b":1}\r\n\r\n: keep-alive\n\ndata: [DONE]\n"),
            vec!["{\"a\":1}"]
        );
        assert_eq!(d.push(b"\n"), vec!["[DONE]"]);
        assert_eq!(d.finish(), None);
    }

    #[test]
    fn multi_line_data_and_an_unclosed_last_event() {
        let mut d = Decoder::default();
        assert!(d.push(b"event: x\ndata: one\ndata:two\n").is_empty());
        assert_eq!(d.finish(), Some("one\ntwo".into()));

        let mut d = Decoder::default();
        assert!(d.push(b"data: tail").is_empty());
        assert_eq!(d.finish(), Some("tail".into()));
    }

    #[test]
    fn utf8_split_inside_a_character() {
        let text = "data: วิศวกร\n\n".as_bytes();
        let mut d = Decoder::default();
        let mut out = d.push(&text[..8]);
        out.extend(d.push(&text[8..]));
        assert_eq!(out, vec!["วิศวกร"]);
    }
}
