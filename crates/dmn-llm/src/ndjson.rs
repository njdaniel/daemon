//! Incremental newline-delimited JSON: feed bytes as they arrive, take complete lines out.
//! A line is complete at `\n`; a trailing partial line waits for more bytes or for
//! [`NdjsonParser::finish`]. Byte-oriented, so a chunk boundary inside a multi-byte character is
//! harmless.

/// Accumulates bytes and yields complete lines. Empty lines are skipped.
#[derive(Debug, Default)]
pub struct NdjsonParser {
    buf: Vec<u8>,
}

impl NdjsonParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes and return every complete line they close, in order.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop(); // the '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if !line.is_empty() {
                lines.push(line);
            }
        }
        lines
    }

    /// The trailing partial line, if any, once the input has ended.
    pub fn finish(&mut self) -> Option<Vec<u8>> {
        let mut rest = std::mem::take(&mut self.buf);
        if rest.last() == Some(&b'\r') {
            rest.pop();
        }
        (!rest.is_empty()).then_some(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[Vec<u8>]) -> Vec<String> {
        v.iter()
            .map(|l| String::from_utf8(l.clone()).unwrap())
            .collect()
    }

    #[test]
    fn whole_lines_come_out_in_order() {
        let mut p = NdjsonParser::new();
        assert_eq!(
            s(&p.feed(b"{\"a\":1}\n{\"a\":2}\n")),
            ["{\"a\":1}", "{\"a\":2}"]
        );
        assert!(p.finish().is_none());
    }

    #[test]
    fn a_line_split_across_chunks_waits_for_its_newline() {
        let mut p = NdjsonParser::new();
        assert!(p.feed(b"{\"a\":").is_empty());
        assert!(p.feed(b"1").is_empty());
        assert_eq!(s(&p.feed(b"}\n{\"b\"")), ["{\"a\":1}"]);
        assert_eq!(s(&p.feed(b":2}\n")), ["{\"b\":2}"]);
    }

    #[test]
    fn multibyte_utf8_split_mid_char_survives() {
        let mut p = NdjsonParser::new();
        let text = "{\"t\":\"caf\u{e9} \u{1F3A4}\"}\n".as_bytes();
        // Split inside the 4-byte microphone character.
        let cut = text.len() - 4;
        assert!(p.feed(&text[..cut]).is_empty());
        let out = p.feed(&text[cut..]);
        assert_eq!(s(&out), ["{\"t\":\"caf\u{e9} \u{1F3A4}\"}"]);
    }

    #[test]
    fn blank_lines_and_crlf_are_tolerated_and_finish_returns_the_tail() {
        let mut p = NdjsonParser::new();
        assert_eq!(s(&p.feed(b"\r\n{\"a\":1}\r\n\n{\"a\":2}")), ["{\"a\":1}"]);
        assert_eq!(p.finish().unwrap(), b"{\"a\":2}");
        assert!(p.finish().is_none());
    }
}
