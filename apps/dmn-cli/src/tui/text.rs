//! Fitting text into terminal cells: display width, one-line truncation, and word wrap with an
//! ellipsis on the last line. The TUI never lets a field grow past its area: what does not fit is
//! cut with `…`, so nothing pushes the layout around (SPEC density rules).

use ratatui::text::Span;

/// Display width in terminal cells (wide characters count 2).
pub fn width(s: &str) -> usize {
    Span::raw(s).width()
}

fn char_width(c: char) -> usize {
    let mut b = [0u8; 4];
    width(c.encode_utf8(&mut b))
}

/// `s` cut to at most `max` cells, ending in `…` when anything was dropped.
pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > max - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    out.push('…');
    out
}

/// The end of `s` in at most `max` cells, starting with `…` when the front was dropped. For the
/// transcript, where the newest words matter.
pub fn truncate_front(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let w = char_width(c);
        if used + w > max - 1 {
            break;
        }
        kept.push(c);
        used += w;
    }
    let tail: String = kept.into_iter().rev().collect();
    format!("…{}", tail.trim_start())
}

/// `s` ending in `…`, cut first if the mark would not fit in `max` cells.
fn with_ellipsis(s: &str, max: usize) -> String {
    if width(s) < max {
        return format!("{s}…");
    }
    // `s` fills the line: dropping one cell more than truncate would makes room for the mark.
    truncate(&format!("{s}…"), max)
}

/// Word-wrap `s` into at most `max_lines` lines of at most `max` cells. When the text needs more
/// lines, the last one ends in `…`. A word longer than a whole line is broken inside.
pub fn wrap(s: &str, max: usize, max_lines: usize) -> Vec<String> {
    if max == 0 || max_lines == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let sep = usize::from(!cur.is_empty());
        if width(&cur) + sep + width(word) <= max {
            if sep == 1 {
                cur.push(' ');
            }
            cur.push_str(word);
            continue;
        }
        if !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        // A word wider than the line: break it across lines.
        let mut rest = word;
        while width(rest) > max {
            let mut cut = 0;
            let mut used = 0;
            for (i, c) in rest.char_indices() {
                let w = char_width(c);
                if used + w > max {
                    break;
                }
                used += w;
                cut = i + c.len_utf8();
            }
            lines.push(rest[..cut].to_owned());
            rest = &rest[cut..];
        }
        cur.push_str(rest);
        // Anything left once the lines are full is overflow; stop reading.
        if lines.len() >= max_lines {
            break;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            *last = with_ellipsis(last, max);
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_text_and_marks_a_cut() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a longer line of text", 10), "a longer…");
        assert_eq!(width(&truncate("a longer line of text", 10)), 9);
        assert_eq!(truncate("anything", 0), "");
    }

    #[test]
    fn truncate_front_keeps_the_newest_words() {
        assert_eq!(
            truncate_front("so the failure mode was retries", 12),
            "…was retries"
        );
        assert!(width(&truncate_front("so the failure mode was retries", 12)) <= 12);
    }

    #[test]
    fn wrap_breaks_at_words_and_ellipsises_overflow() {
        assert_eq!(
            wrap("one two three four", 9, 3),
            ["one two", "three", "four"]
        );
        let w = wrap("one two three four five six seven", 9, 2);
        assert_eq!(w.len(), 2);
        assert!(w[1].ends_with('…'), "{w:?}");
        assert!(w.iter().all(|l| width(l) <= 9), "{w:?}");
    }

    #[test]
    fn wrap_breaks_a_word_wider_than_the_line() {
        let w = wrap("abcdefghij", 4, 5);
        assert_eq!(w, ["abcd", "efgh", "ij"]);
    }
}
