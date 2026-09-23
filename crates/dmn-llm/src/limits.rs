//! Field limits, enforced in Rust after parsing and never left to the prompt (CLAUDE.md rules,
//! SPEC § Prompt contract): approach 3–5 items of ≤ 90 characters, `focus` ≤ 120 characters,
//! every list ≤ 5 items, `deeper` ≤ 4 entries of ≤ 240 characters. Text over its limit is cut at
//! a sentence boundary (or, failing that, a word boundary with `…`) and flagged in the debug log;
//! it is never shown as a wall. Lengths are counted in `char`s.

use crate::schema::{Kind, Star, Suggestion};

pub const APPROACH_MIN: usize = 3;
pub const APPROACH_MAX: usize = 5;
pub const APPROACH_LINE_CHARS: usize = 90;
pub const FOCUS_CHARS: usize = 120;
pub const LIST_MAX: usize = 5;
pub const DEEPER_MAX: usize = 4;
pub const DEEPER_CHARS: usize = 240;
/// Not stated in the SPEC; STAR cards get the same per-entry cap as `deeper` so none is a wall.
pub const STAR_FIELD_CHARS: usize = 240;

/// One limit that model output broke. `field` names the place, like `approach[2]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    pub field: String,
    pub kind: FlagKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagKind {
    /// Text cut from `from` to `to` characters.
    Truncated { from: usize, to: usize },
    /// List cut from `from` to `to` items.
    Dropped { from: usize, to: usize },
    /// Fewer items than the minimum; nothing is invented, the flag is all.
    TooFew { items: usize, min: usize },
}

impl Flag {
    pub(crate) fn new(field: impl Into<String>, kind: FlagKind) -> Self {
        let flag = Self {
            field: field.into(),
            kind,
        };
        tracing::debug!(field = %flag.field, kind = ?flag.kind, "llm output over limit");
        flag
    }
}

/// `text` cut to at most `max` chars, or `None` when it already fits. Prefers the last sentence
/// end (`.`, `!`, `?` followed by whitespace or the end) that keeps at least a third of the
/// budget; otherwise cuts at the last whitespace and appends `…`; otherwise hard-cuts with `…`.
pub fn truncate(text: &str, max: usize) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return None;
    }
    if max == 0 {
        return Some(String::new());
    }
    let window = &chars[..max];
    let sentence_end = (0..max).rev().find(|&i| {
        matches!(window[i], '.' | '!' | '?') && chars.get(i + 1).is_none_or(|c| c.is_whitespace())
    });
    if let Some(i) = sentence_end.filter(|&i| i + 1 >= max / 3) {
        return Some(window[..=i].iter().collect());
    }
    // Leave room for the ellipsis.
    let room = &chars[..max - 1];
    let cut = room
        .iter()
        .rposition(|c| c.is_whitespace())
        .filter(|&i| i > 0)
        .unwrap_or(max - 1);
    let mut out: String = room[..cut].iter().collect();
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    out.push('…');
    Some(out)
}

/// Apply [`truncate`] in place and record a flag when it cut.
pub fn limit_text(field: &str, text: &mut String, max: usize, flags: &mut Vec<Flag>) {
    if let Some(cut) = truncate(text, max) {
        let from = text.chars().count();
        let to = cut.chars().count();
        *text = cut;
        flags.push(Flag::new(field, FlagKind::Truncated { from, to }));
    }
}

/// Keep at most `max` items and record a flag when any were dropped.
pub fn limit_list<T>(field: &str, list: &mut Vec<T>, max: usize, flags: &mut Vec<Flag>) {
    if list.len() > max {
        let from = list.len();
        list.truncate(max);
        flags.push(Flag::new(field, FlagKind::Dropped { from, to: max }));
    }
}

/// One approach line as the UI shows it: trimmed, the model's own numbering or bullet removed
/// (the UI numbers lines itself), and cut to [`APPROACH_LINE_CHARS`].
pub fn approach_line(index: usize, raw: &str, flags: &mut Vec<Flag>) -> String {
    let mut line = strip_marker(raw.trim()).to_owned();
    limit_text(
        &format!("approach[{index}]"),
        &mut line,
        APPROACH_LINE_CHARS,
        flags,
    );
    line
}

/// Flag an Approach with fewer than [`APPROACH_MIN`] lines for a kind that shows one. Nothing
/// is invented to fill the gap.
pub fn check_approach_min(kind: Option<Kind>, lines: usize, flags: &mut Vec<Flag>) {
    if kind.is_some_and(Kind::shows_approach) && lines < APPROACH_MIN {
        flags.push(Flag::new(
            "approach",
            FlagKind::TooFew {
                items: lines,
                min: APPROACH_MIN,
            },
        ));
    }
}

/// Cut the STAR fields to [`STAR_FIELD_CHARS`].
pub fn limit_star(star: &mut Star, flags: &mut Vec<Flag>) {
    limit_text(
        "star.situation",
        &mut star.situation,
        STAR_FIELD_CHARS,
        flags,
    );
    limit_text("star.task", &mut star.task, STAR_FIELD_CHARS, flags);
    limit_text("star.action", &mut star.action, STAR_FIELD_CHARS, flags);
    limit_text("star.result", &mut star.result, STAR_FIELD_CHARS, flags);
}

/// Enforce every limit on `s` in place; returns what was cut. Also drops empty approach lines.
pub fn enforce(s: &mut Suggestion) -> Vec<Flag> {
    let mut flags = Vec::new();

    let focus = s.focus.trim().to_owned();
    s.focus = focus;
    limit_text("focus", &mut s.focus, FOCUS_CHARS, &mut flags);

    s.approach.retain(|l| !strip_marker(l.trim()).is_empty());
    limit_list("approach", &mut s.approach, APPROACH_MAX, &mut flags);
    let raw = std::mem::take(&mut s.approach);
    s.approach = raw
        .iter()
        .enumerate()
        .map(|(i, l)| approach_line(i, l, &mut flags))
        .collect();
    check_approach_min(s.kind, s.approach.len(), &mut flags);

    limit_list("deeper", &mut s.deeper, DEEPER_MAX, &mut flags);
    for (i, d) in s.deeper.iter_mut().enumerate() {
        limit_text(
            &format!("deeper[{i}]"),
            &mut d.text,
            DEEPER_CHARS,
            &mut flags,
        );
    }
    limit_list("followups", &mut s.followups, LIST_MAX, &mut flags);
    limit_list("tradeoffs", &mut s.tradeoffs, LIST_MAX, &mut flags);
    limit_list("mention", &mut s.mention, LIST_MAX, &mut flags);
    limit_list(
        "resume_context",
        &mut s.resume_context,
        LIST_MAX,
        &mut flags,
    );
    if let Some(star) = s.star.as_mut() {
        limit_star(star, &mut flags);
    }
    flags
}

/// Remove a leading `1.`, `2)`, `-`, `*` or `•` marker.
fn strip_marker(line: &str) -> &str {
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    let rest = &line[digits..];
    if digits > 0
        && let Some(r) = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))
        && r.starts_with(char::is_whitespace)
    {
        return r.trim_start();
    }
    for m in ['-', '*', '•'] {
        if let Some(r) = line.strip_prefix(m)
            && r.starts_with(char::is_whitespace)
        {
            return r.trim_start();
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Deeper;

    #[test]
    fn short_text_is_untouched() {
        assert_eq!(truncate("Fits.", 90), None);
        assert_eq!(truncate(&"x".repeat(90), 90), None);
    }

    #[test]
    fn long_text_cuts_at_the_last_sentence_that_fits() {
        let text = "Use a token bucket per key. Store counters in Redis with TTLs. Fail open when Redis is down.";
        let cut = truncate(text, 70).unwrap();
        assert_eq!(
            cut,
            "Use a token bucket per key. Store counters in Redis with TTLs."
        );
        assert!(cut.chars().count() <= 70);
    }

    #[test]
    fn no_sentence_end_cuts_at_a_word_with_ellipsis() {
        let text = "Clarify the read and write ratio and the latency target before choosing any storage engine at all";
        let cut = truncate(text, 40).unwrap();
        assert!(cut.ends_with('…'), "{cut}");
        assert!(cut.chars().count() <= 40, "{cut}");
        assert!(!cut.contains("  "));
        assert!(text.starts_with(cut.trim_end_matches('…')));
    }

    #[test]
    fn decimal_point_is_not_a_sentence_end() {
        let text =
            "Target p99 of 2.5 seconds across every region and every tier of the service mesh";
        let cut = truncate(text, 30).unwrap();
        assert!(cut.ends_with('…'), "{cut}");
    }

    #[test]
    fn multibyte_text_counts_chars_not_bytes() {
        let text = "é".repeat(100);
        let cut = truncate(&text, 90).unwrap();
        assert_eq!(cut.chars().count(), 90);
    }

    #[test]
    fn approach_line_strips_numbering_and_bullets() {
        let mut f = Vec::new();
        assert_eq!(
            approach_line(0, " 1. Clarify scope ", &mut f),
            "Clarify scope"
        );
        assert_eq!(approach_line(0, "2) Pick storage", &mut f), "Pick storage");
        assert_eq!(approach_line(0, "- Shard by key", &mut f), "Shard by key");
        assert_eq!(
            approach_line(0, "3x replication first", &mut f),
            "3x replication first"
        );
        assert!(f.is_empty());
    }

    #[test]
    fn enforce_cuts_every_field_and_flags_each_cut() {
        let long = "word ".repeat(80);
        let mut s = Suggestion {
            question: "q".into(),
            kind: Some(Kind::SystemDesign),
            focus: long.clone(),
            approach: (0..7).map(|i| format!("{i}. {long}")).collect(),
            deeper: (0..6)
                .map(|_| Deeper {
                    label: "L".into(),
                    text: long.clone(),
                })
                .collect(),
            followups: vec!["f".into(); 9],
            mention: vec!["m".into(); 6],
            ..Suggestion::default()
        };
        let flags = enforce(&mut s);
        assert!(s.focus.chars().count() <= FOCUS_CHARS);
        assert_eq!(s.approach.len(), APPROACH_MAX);
        assert!(
            s.approach
                .iter()
                .all(|l| l.chars().count() <= APPROACH_LINE_CHARS)
        );
        assert_eq!(s.deeper.len(), DEEPER_MAX);
        assert!(
            s.deeper
                .iter()
                .all(|d| d.text.chars().count() <= DEEPER_CHARS)
        );
        assert_eq!(s.followups.len(), LIST_MAX);
        assert_eq!(s.mention.len(), LIST_MAX);
        let fields: Vec<&str> = flags.iter().map(|f| f.field.as_str()).collect();
        for want in [
            "focus",
            "approach",
            "approach[0]",
            "approach[4]",
            "deeper",
            "deeper[3]",
            "followups",
            "mention",
        ] {
            assert!(fields.contains(&want), "missing flag {want}: {fields:?}");
        }
    }

    #[test]
    fn too_few_approach_lines_are_flagged_not_invented() {
        let mut s = Suggestion {
            kind: Some(Kind::Knowledge),
            approach: vec!["one".into(), "   ".into()],
            ..Suggestion::default()
        };
        let flags = enforce(&mut s);
        assert_eq!(s.approach, vec!["one".to_owned()]);
        assert!(
            flags
                .iter()
                .any(|f| f.kind == FlagKind::TooFew { items: 1, min: 3 })
        );
    }

    #[test]
    fn within_limits_produces_no_flags() {
        let mut s = Suggestion {
            kind: Some(Kind::Coding),
            focus: "hash maps, complexity".into(),
            approach: vec!["a".into(), "b".into(), "c".into()],
            ..Suggestion::default()
        };
        let before = s.clone();
        assert!(enforce(&mut s).is_empty());
        assert_eq!(s, before);
    }
}
