//! Incremental parsing of one streamed JSON object, so fields can be published as they complete
//! instead of when the whole object has arrived (#12: on the dev 4090 the question is ready at
//! 1.3 s, the whole object only at 3.0 s).
//!
//! Feed text deltas in order; [`IncrementalObject::feed`] returns a [`JsonEvent`] for every
//! top-level field whose value just completed, and for every element of a top-level array as
//! soon as that element completes. Each completed value is parsed with `serde_json`, so escapes
//! and nesting follow the real grammar; the scanner only tracks where values start and end.
//! Structural characters are ASCII, so scanning bytes is safe with multi-byte text in strings.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum JsonEvent {
    /// A top-level field's value completed.
    Field { key: String, value: Value },
    /// Element `index` of the top-level array field `key` completed (before the array closes).
    Item {
        key: String,
        index: usize,
        value: Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Object,
    Array,
}

/// What the top-level object expects next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Key,
    Colon,
    Value,
    CommaOrEnd,
}

#[derive(Debug)]
pub struct IncrementalObject {
    buf: String,
    /// Bytes of `buf` already scanned.
    pos: usize,
    stack: Vec<Container>,
    in_string: bool,
    escape: bool,
    expect: Expect,
    key: Option<String>,
    key_start: Option<usize>,
    field_start: Option<usize>,
    item_start: Option<usize>,
    item_index: usize,
    /// Start of an unquoted scalar (number, `true`, `false`, `null`).
    scalar_start: Option<usize>,
    closed: bool,
}

impl Default for IncrementalObject {
    fn default() -> Self {
        Self::new()
    }
}

impl IncrementalObject {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            pos: 0,
            stack: Vec::new(),
            in_string: false,
            escape: false,
            expect: Expect::Key,
            key: None,
            key_start: None,
            field_start: None,
            item_start: None,
            item_index: 0,
            scalar_start: None,
            closed: false,
        }
    }

    /// Everything fed so far.
    pub fn text(&self) -> &str {
        &self.buf
    }

    /// Whether the top-level object has closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Append a delta and return the values it completed, in stream order.
    pub fn feed(&mut self, delta: &str) -> Vec<JsonEvent> {
        self.buf.push_str(delta);
        let mut out = Vec::new();
        while self.pos < self.buf.len() && !self.closed {
            let i = self.pos;
            let b = self.buf.as_bytes()[i];
            self.pos += 1;
            self.step(i, b, &mut out);
        }
        out
    }

    fn step(&mut self, i: usize, b: u8, out: &mut Vec<JsonEvent>) {
        if self.in_string {
            if self.escape {
                self.escape = false;
            } else if b == b'\\' {
                self.escape = true;
            } else if b == b'"' {
                self.in_string = false;
                self.string_closed(i, out);
            }
            return;
        }
        if self.scalar_start.is_some()
            && matches!(b, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
        {
            self.scalar_start = None;
            self.value_end(i, out);
        }
        match b {
            b'"' => {
                self.in_string = true;
                if self.stack.len() == 1 && self.expect == Expect::Key {
                    self.key_start = Some(i);
                } else {
                    self.value_begin(i);
                }
            }
            b'{' | b'[' => {
                self.value_begin(i);
                self.stack.push(if b == b'{' {
                    Container::Object
                } else {
                    Container::Array
                });
                if self.stack.len() == 2 && b == b'[' {
                    self.item_index = 0;
                }
            }
            b'}' | b']' => {
                self.stack.pop();
                if self.stack.is_empty() {
                    self.closed = true;
                    return;
                }
                self.value_end(i + 1, out);
            }
            b':' if self.stack.len() == 1 => self.expect = Expect::Value,
            b',' if self.stack.len() == 1 => self.expect = Expect::Key,
            b',' | b':' | b' ' | b'\t' | b'\n' | b'\r' => {}
            _ => {
                if self.scalar_start.is_none() && !self.stack.is_empty() {
                    self.scalar_start = Some(i);
                    self.value_begin(i);
                }
            }
        }
    }

    fn in_top_array(&self) -> bool {
        self.stack.len() == 2 && self.stack[1] == Container::Array
    }

    fn value_begin(&mut self, i: usize) {
        if self.stack.len() == 1 && self.expect == Expect::Value {
            self.field_start = Some(i);
        } else if self.in_top_array() {
            self.item_start = Some(i);
        }
    }

    fn string_closed(&mut self, i: usize, out: &mut Vec<JsonEvent>) {
        if let Some(s) = self.key_start.take() {
            self.key = serde_json::from_str(&self.buf[s..=i]).ok();
            self.expect = Expect::Colon;
            return;
        }
        self.value_end(i + 1, out);
    }

    /// A value ending at byte `end` (exclusive) just closed at the current nesting level.
    fn value_end(&mut self, end: usize, out: &mut Vec<JsonEvent>) {
        if self.stack.len() == 1 {
            if let Some(s) = self.field_start.take() {
                self.expect = Expect::CommaOrEnd;
                if let (Some(key), Ok(value)) =
                    (self.key.clone(), serde_json::from_str(&self.buf[s..end]))
                {
                    out.push(JsonEvent::Field { key, value });
                }
            }
        } else if self.in_top_array()
            && let Some(s) = self.item_start.take()
        {
            let index = self.item_index;
            self.item_index += 1;
            if let (Some(key), Ok(value)) =
                (self.key.clone(), serde_json::from_str(&self.buf[s..end]))
            {
                out.push(JsonEvent::Item { key, index, value });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn feed_in_chunks(text: &str, n: usize) -> Vec<JsonEvent> {
        let mut p = IncrementalObject::new();
        let chars: Vec<char> = text.chars().collect();
        let mut out = Vec::new();
        for c in chars.chunks(n) {
            out.extend(p.feed(&c.iter().collect::<String>()));
        }
        assert!(p.is_closed());
        out
    }

    #[test]
    fn fields_and_items_arrive_in_stream_order_for_every_chunk_size() {
        let value = json!({
            "is_question": true,
            "kind": "system_design",
            "question": "Design a \"rate\" limiter, é \u{1F3A4}?",
            "focus": "",
            "approach": ["one", "two, with comma", "three ]}"],
            "star": {"situation": "s", "task": "t"},
            "n": -1.5e3,
            "z": null
        });
        let text = serde_json::to_string_pretty(&value).unwrap();
        for n in [1, 2, 3, 7, 1000] {
            let events = feed_in_chunks(&text, n);
            let expected = vec![
                JsonEvent::Field {
                    key: "is_question".into(),
                    value: json!(true),
                },
                JsonEvent::Field {
                    key: "kind".into(),
                    value: json!("system_design"),
                },
                JsonEvent::Field {
                    key: "question".into(),
                    value: value["question"].clone(),
                },
                JsonEvent::Field {
                    key: "focus".into(),
                    value: json!(""),
                },
                JsonEvent::Item {
                    key: "approach".into(),
                    index: 0,
                    value: json!("one"),
                },
                JsonEvent::Item {
                    key: "approach".into(),
                    index: 1,
                    value: json!("two, with comma"),
                },
                JsonEvent::Item {
                    key: "approach".into(),
                    index: 2,
                    value: json!("three ]}"),
                },
                JsonEvent::Field {
                    key: "approach".into(),
                    value: value["approach"].clone(),
                },
                JsonEvent::Field {
                    key: "star".into(),
                    value: value["star"].clone(),
                },
                JsonEvent::Field {
                    key: "n".into(),
                    value: json!(-1500.0),
                },
                JsonEvent::Field {
                    key: "z".into(),
                    value: json!(null),
                },
            ];
            assert_eq!(events, expected, "chunk size {n}");
        }
    }

    #[test]
    fn question_is_available_before_the_approach_array_closes() {
        let mut p = IncrementalObject::new();
        let events = p.feed(r#"{"is_question":true,"kind":"coding","question":"Reverse a list?","approach":["Clarify"#);
        assert!(events.contains(&JsonEvent::Field {
            key: "question".into(),
            value: json!("Reverse a list?")
        }));
        assert!(!events.iter().any(|e| matches!(e, JsonEvent::Item { .. })));
        let events = p.feed(r#" inputs","Two"#);
        assert_eq!(
            events,
            vec![JsonEvent::Item {
                key: "approach".into(),
                index: 0,
                value: json!("Clarify inputs")
            }]
        );
        assert!(!p.is_closed());
    }

    #[test]
    fn scalars_at_the_end_of_the_object_complete() {
        let mut p = IncrementalObject::new();
        let events = p.feed(r#"{"a":1,"b":false}"#);
        assert_eq!(
            events,
            vec![
                JsonEvent::Field {
                    key: "a".into(),
                    value: json!(1)
                },
                JsonEvent::Field {
                    key: "b".into(),
                    value: json!(false)
                },
            ]
        );
    }

    #[test]
    fn arrays_of_objects_yield_whole_items() {
        let events = feed_in_chunks(
            r#"{"deeper":[{"label":"A","text":"x"},{"label":"B","text":"y"}]}"#,
            5,
        );
        assert_eq!(
            events[..2],
            [
                JsonEvent::Item {
                    key: "deeper".into(),
                    index: 0,
                    value: json!({"label":"A","text":"x"})
                },
                JsonEvent::Item {
                    key: "deeper".into(),
                    index: 1,
                    value: json!({"label":"B","text":"y"})
                },
            ]
        );
    }
}
