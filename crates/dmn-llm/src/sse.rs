//! Server-sent events, line-based: `data: <payload>` frames separated by blank lines, `event:` /
//! `id:` / `:` comment lines ignored. Both remote providers (#59) stream this way; the parser is
//! byte-oriented like [`NdjsonParser`], so a chunk boundary inside a multi-byte character is
//! harmless. A frame's JSON always arrives on one `data:` line from the APIs this covers (OpenAI
//! chat completions, Anthropic messages), so lines are not joined across an event boundary.

use crate::ndjson::NdjsonParser;

/// Accumulates bytes and yields the payloads of complete `data:` lines.
#[derive(Debug, Default)]
pub struct SseParser {
    lines: NdjsonParser,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes; return the data payload of every `data:` line they complete, in order.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.lines
            .feed(bytes)
            .into_iter()
            .filter_map(|line| data(&line))
            .collect()
    }

    /// The payload of a trailing unterminated `data:` line, if any, once the body has ended.
    pub fn finish(&mut self) -> Option<String> {
        self.lines.finish().and_then(|line| data(&line))
    }
}

/// The payload of a `data:` line: after the colon, at most one leading space is stripped
/// (RFC 4320/whatwg convention), the rest is verbatim.
fn data(line: &[u8]) -> Option<String> {
    let rest = line.strip_prefix(b"data:")?;
    let rest = rest.strip_prefix(b" ").unwrap_or(rest);
    Some(String::from_utf8_lossy(rest).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_lines_come_out_in_order_and_other_fields_are_ignored() {
        let mut p = SseParser::new();
        let out = p.feed(
            b"event: message_start\ndata: {\"a\":1}\n\n: keepalive\ndata:{\"b\":2}\nid: 7\ndata: [DONE]\n\n",
        );
        assert_eq!(out, ["{\"a\":1}", "{\"b\":2}", "[DONE]"]);
        assert!(p.finish().is_none());
    }

    #[test]
    fn a_frame_split_across_chunks_waits_for_its_newline() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: {\"a\"").is_empty());
        assert_eq!(p.feed(b":1}\ndata: two\n"), ["{\"a\":1}", "two"]);
        p.feed(b"data: tail");
        assert_eq!(p.finish().as_deref(), Some("tail"));
    }

    #[test]
    fn only_one_leading_space_is_stripped() {
        let mut p = SseParser::new();
        assert_eq!(p.feed(b"data:  spaced\n"), [" spaced"]);
        assert_eq!(p.feed(b"data:\n"), [""]);
    }
}
