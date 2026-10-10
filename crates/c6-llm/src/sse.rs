//! Minimal server-sent-events decoder: yields the `data` of each event.

use bytes::BytesMut;

#[derive(Default)]
pub struct SseDecoder {
    buf: BytesMut,
    data: String,
}

impl SseDecoder {
    /// Feeds bytes; returns the data payloads of every completed event.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line = self.buf.split_to(pos + 1);
            line.truncate(line.len() - 1);
            if line.last() == Some(&b'\r') {
                line.truncate(line.len() - 1);
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                if !self.data.is_empty() {
                    out.push(std::mem::take(&mut self.data));
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
            // `event:`, `id:`, `retry:` and comments are not needed.
        }
        out
    }

    /// Flushes a trailing event that was not terminated by a blank line.
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = if self.buf.is_empty() { Vec::new() } else { self.push(b"\n") };
        out.extend(self.push(b"\n"));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_across_chunks() {
        let mut d = SseDecoder::default();
        assert!(d.push(b"data: {\"a\":").is_empty());
        assert_eq!(d.push(b"1}\r\n\r\n: comment\n\ndata: x\ndata: y\n\n"), vec!["{\"a\":1}", "x\ny"]);
        assert_eq!(d.push(b"data: [DONE]"), Vec::<String>::new());
        assert_eq!(d.finish(), vec!["[DONE]"]);
    }
}
