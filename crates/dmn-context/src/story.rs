//! Stories (#58, SPEC § Context and LLM, Sources): one file per story, with a tag line
//! (`tags: conflict, ownership`) from front matter or the first line. A behavioral question is
//! matched to stories by tag overlap with the question's themes first and BM25 second; the best
//! one fills the STAR block and Swap walks down the ranking.
//!
//! Themes are words a question uses for a tag: "disagreed" asks for a `conflict` story,
//! "mentored" for a `mentoring` one. A tag also matches when one of its own words is in the
//! question, so a story tagged `migration` answers "tell me about a migration". The table is
//! small and fixed, so matching is deterministic and needs no model call.

use std::path::Path;

use crate::bm25::{Bm25, terms};
use crate::chunk::approx_tokens;

/// A matched story reaches the prompt capped at this many approximate tokens: a story is a
/// paragraph or four, and the behavioral prompt must stay inside the 2 s budget.
pub const STORY_TOKENS: usize = 400;

/// One story, parsed from its stored text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Story {
    /// The absolute path its source was read from.
    pub path: String,
    /// `stories/<file>`: its folder and file name, as the STAR header shows it (SPEC: the
    /// matched story path in the header).
    pub label: String,
    /// From front matter, else the first `#` heading, else the file name.
    pub title: String,
    /// Lowercased, as written.
    pub tags: Vec<String>,
    /// The story without its front matter or tag line, capped at [`STORY_TOKENS`].
    pub text: String,
}

impl Story {
    /// Parse `raw` (the stored text of the file at `path`).
    pub fn parse(path: &str, raw: &str) -> Self {
        let (front, body) = split_front_matter(raw);
        let mut tags = Vec::new();
        let mut title = None;
        let body = match front {
            Some(front) => {
                let (t, ti) = parse_front_matter(front);
                tags = t;
                title = ti;
                body
            }
            None => match tag_line(body) {
                Some((t, rest)) => {
                    tags = t;
                    rest
                }
                None => body,
            },
        };
        let body = body.trim();
        let title = title
            .or_else(|| heading(body))
            .unwrap_or_else(|| stem(path));
        Self {
            path: path.to_owned(),
            label: label(path),
            title,
            tags,
            text: cap_tokens(body, STORY_TOKENS).to_owned(),
        }
    }
}

/// A story ranked for a question.
#[derive(Debug, Clone, PartialEq)]
pub struct StoryMatch<'a> {
    pub story: &'a Story,
    /// The story's tags the question asks for, in the story's order.
    pub tags: Vec<&'a str>,
    /// BM25 of the question against the story's text.
    pub score: f64,
}

/// The stories loaded for a session, with a BM25 index over their text.
#[derive(Debug, Clone, Default)]
pub struct Stories {
    stories: Vec<Story>,
    index: Bm25,
}

impl Stories {
    pub fn new(stories: Vec<Story>) -> Self {
        let index = Bm25::new(stories.iter().map(|s| s.text.as_str()));
        Self { stories, index }
    }

    pub fn len(&self) -> usize {
        self.stories.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stories.is_empty()
    }

    /// The story with `label` (or, failing that, `path`).
    pub fn get(&self, label: &str) -> Option<&Story> {
        self.stories
            .iter()
            .find(|s| s.label == label)
            .or_else(|| self.stories.iter().find(|s| s.path == label))
    }

    /// Every story that shares a theme or a term with `question`: most matching tags first,
    /// then the higher BM25 score, then load order. A story with neither is left out.
    pub fn rank(&self, question: &str) -> Vec<StoryMatch<'_>> {
        let asked = terms(question);
        let scores = self.index.search(question);
        let score_of = |i: usize| {
            scores
                .iter()
                .find(|(j, _)| *j == i)
                .map_or(0.0, |(_, s)| *s)
        };
        let mut out: Vec<(usize, StoryMatch<'_>)> = self
            .stories
            .iter()
            .enumerate()
            .map(|(i, story)| {
                let tags = story
                    .tags
                    .iter()
                    .filter(|t| tag_asked(t, &asked))
                    .map(String::as_str)
                    .collect();
                (
                    i,
                    StoryMatch {
                        story,
                        tags,
                        score: score_of(i),
                    },
                )
            })
            .filter(|(_, m)| !m.tags.is_empty() || m.score > 0.0)
            .collect();
        out.sort_by(|(ia, a), (ib, b)| {
            b.tags
                .len()
                .cmp(&a.tags.len())
                .then(b.score.total_cmp(&a.score))
                .then(ia.cmp(ib))
        });
        out.into_iter().map(|(_, m)| m).collect()
    }
}

/// Whether `question` asks for a story from the candidate's past ("tell me about a time…",
/// "describe a situation where…"). The story goes into the fast-phase prompt only then, so a
/// system-design or knowledge question's prompt is not made longer (and slower) by a story it
/// cannot use.
pub fn asks_for_a_story(question: &str) -> bool {
    let q: String = question
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let q = format!(" {} ", q.split_whitespace().collect::<Vec<_>>().join(" "));
    CUES.iter().any(|cue| q.contains(&format!(" {cue} ")))
}

/// Phrases that open a behavioral question (SPEC § Transcription: kind `behavioral`).
const CUES: &[&str] = &[
    "a time",
    "the time",
    "a situation",
    "a project",
    "an example",
    "a moment",
    "an occasion",
    "tell me about a",
    "have you ever",
    "how did you handle",
    "what did you do when",
];

/// Question words that ask for a tag, beyond the tag's own words. Keys are tags as a story
/// would write them; words are matched after the same folding BM25 uses (lowercase, plurals).
const THEMES: &[(&str, &[&str])] = &[
    (
        "conflict",
        &[
            "conflict",
            "disagree",
            "disagreed",
            "disagreement",
            "pushback",
            "argument",
            "argued",
            "tension",
            "clash",
        ],
    ),
    (
        "disagreement",
        &["disagree", "disagreed", "disagreement", "conflict"],
    ),
    (
        "failure",
        &[
            "fail", "failed", "failure", "mistake", "wrong", "setback", "lesson",
        ],
    ),
    (
        "lessons-learned",
        &["lesson", "learned", "learn", "failure", "mistake"],
    ),
    (
        "prioritization",
        &[
            "prioritize",
            "prioritise",
            "prioritization",
            "priority",
            "scope",
            "cut",
            "tradeoff",
        ],
    ),
    ("scope", &["scope", "cut"]),
    ("tradeoffs", &["tradeoff", "trade", "cut"]),
    ("deadline", &["deadline", "ship", "shipped", "date"]),
    (
        "mentoring",
        &[
            "mentor",
            "mentored",
            "mentoring",
            "coach",
            "coached",
            "junior",
            "teach",
            "taught",
        ],
    ),
    ("coaching", &["coach", "coached", "coaching", "mentor"]),
    ("growth", &["grow", "grew", "growth", "develop"]),
    (
        "ownership",
        &["ownership", "own", "owned", "responsibility", "responsible"],
    ),
    ("initiative", &["initiative", "broken", "stepped"]),
    (
        "communication",
        &[
            "news",
            "communicate",
            "communicated",
            "communication",
            "tell",
        ],
    ),
    (
        "stakeholder",
        &["stakeholder", "director", "executive", "customer", "client"],
    ),
    ("bad-news", &["news"]),
    (
        "influence",
        &["authority", "influence", "persuade", "convince", "report"],
    ),
    ("cross-team", &["authority", "report", "partner"]),
    (
        "leadership",
        &["lead", "led", "leader", "leadership", "manage"],
    ),
    ("technical-decision", &["technical", "decision", "design"]),
    (
        "migration",
        &["migration", "migrate", "migrated", "cutover"],
    ),
];

/// `tag` is asked for by a question whose terms are `asked`.
fn tag_asked(tag: &str, asked: &[String]) -> bool {
    let own = terms(tag);
    if own.iter().any(|t| asked.contains(t)) {
        return true;
    }
    THEMES
        .iter()
        .filter(|(key, _)| *key == tag)
        .flat_map(|(_, words)| words.iter())
        .flat_map(|w| terms(w))
        .any(|w| asked.contains(&w))
}

/// Front matter (between a first line `---` and the next `---`) and the rest.
fn split_front_matter(raw: &str) -> (Option<&str>, &str) {
    let text = raw.trim_start_matches('\u{feff}').trim_start();
    let Some(rest) = text.strip_prefix("---") else {
        return (None, raw);
    };
    let Some(rest) = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    else {
        return (None, raw);
    };
    // The closing fence is a line of its own.
    let mut at = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return (Some(&rest[..at]), &rest[at + line.len()..]);
        }
        at += line.len();
    }
    (None, raw)
}

/// `tags` (a `[a, b]` list, an `a, b` line, or `- a` items under `tags:`) and `title` from front
/// matter.
fn parse_front_matter(front: &str) -> (Vec<String>, Option<String>) {
    let mut tags = Vec::new();
    let mut title = None;
    let mut in_tags = false;
    for line in front.lines() {
        let trimmed = line.trim();
        if in_tags {
            if let Some(item) = trimmed.strip_prefix("- ") {
                tags.extend(split_tags(item));
                continue;
            }
            in_tags = false;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        match key.trim().to_lowercase().as_str() {
            "tags" => {
                if value.trim().is_empty() {
                    in_tags = true;
                } else {
                    tags.extend(split_tags(value));
                }
            }
            "title" => {
                let v = unquote(value.trim());
                if !v.is_empty() {
                    title = Some(v.to_owned());
                }
            }
            _ => {}
        }
    }
    (tags, title)
}

/// A first line `tags: a, b` (after blank lines) and the text after it.
fn tag_line(text: &str) -> Option<(Vec<String>, &str)> {
    let start = text.len() - text.trim_start().len();
    let rest = &text[start..];
    let line_end = rest.find('\n').unwrap_or(rest.len());
    let line = &rest[..line_end];
    let (key, value) = line.split_once(':')?;
    if !key.trim().eq_ignore_ascii_case("tags") {
        return None;
    }
    Some((split_tags(value), &rest[line_end..]))
}

fn split_tags(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|t| unquote(t.trim()).trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

fn unquote(s: &str) -> &str {
    s.trim_matches(|c| c == '"' || c == '\'')
}

/// The first Markdown `#` heading's text.
fn heading(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("# "))
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
}

fn stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .replace(['-', '_'], " ")
}

/// `<folder>/<file>`, or the file name alone when there is no folder.
fn label(path: &str) -> String {
    let p = Path::new(path);
    let file = p.file_name().and_then(|f| f.to_str()).unwrap_or(path);
    match p
        .parent()
        .and_then(|d| d.file_name())
        .and_then(|d| d.to_str())
    {
        Some(dir) => format!("{dir}/{file}"),
        None => file.to_owned(),
    }
}

/// `text` cut after the last whole word that keeps it within `max` approximate tokens.
fn cap_tokens(text: &str, max: usize) -> &str {
    let mut words = 0;
    let mut end = 0;
    let mut in_word = false;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            in_word = false;
        } else if !in_word {
            in_word = true;
            words += 1;
            if approx_tokens(words) > max {
                return text[..end].trim_end();
            }
        }
        if in_word {
            end = i + c.len_utf8();
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn story(path: &str, raw: &str) -> Story {
        Story::parse(path, raw)
    }

    #[test]
    fn tags_come_from_front_matter_in_every_list_form_or_from_the_first_line() {
        let flow = story(
            "/s/stories/a.md",
            "---\ntitle: \"Broker\"\ntags: [conflict, Disagreement]\n---\n\nBody text.",
        );
        assert_eq!(flow.tags, ["conflict", "disagreement"]);
        assert_eq!(flow.title, "Broker");
        assert_eq!(flow.text, "Body text.");
        assert_eq!(flow.label, "stories/a.md");

        let plain = story(
            "/s/x/b.md",
            "---\ntags: failure, lessons-learned\n---\n# Cutover\nIt went wrong.",
        );
        assert_eq!(plain.tags, ["failure", "lessons-learned"]);
        assert_eq!(
            plain.title, "Cutover",
            "from the heading when front matter has none"
        );

        let items = story(
            "/s/c.md",
            "---\ntags:\n  - ownership\n  - initiative\nx: y\n---\nBody",
        );
        assert_eq!(items.tags, ["ownership", "initiative"]);

        let line = story(
            "/s/stories/d.md",
            "\ntags: mentoring, growth\n\n# Draft PRs\nShe grew.",
        );
        assert_eq!(line.tags, ["mentoring", "growth"]);
        assert_eq!(line.title, "Draft PRs");
        assert!(
            !line.text.contains("tags:"),
            "the tag line is not story text"
        );

        let none = story(
            "/s/stories/kafka-replay.md",
            "We replayed six hours of events.",
        );
        assert!(none.tags.is_empty());
        assert_eq!(none.title, "kafka replay");
    }

    #[test]
    fn a_long_story_is_capped_at_a_word_boundary() {
        let raw = "word ".repeat(1_000);
        let s = story("/s/stories/long.md", &raw);
        let words = s.text.split_whitespace().count();
        assert!(approx_tokens(words) <= STORY_TOKENS);
        assert!(approx_tokens(words + 1) > STORY_TOKENS, "as much as fits");
        assert!(s.text.ends_with("word"));
    }

    #[test]
    fn tag_overlap_outranks_bm25_when_both_match() {
        let stories = Stories::new(vec![
            // Every word of the question, but no tag it asks for.
            story(
                "/s/stories/words.md",
                "tags: kafka\nI disagreed with a technical decision about how you handled it and \
                 disagreed again; disagreed, technical decision, handled.",
            ),
            // One tag the question asks for, few of its words.
            story(
                "/s/stories/tagged.md",
                "tags: conflict\nThe tech lead and I saw the broker choice differently.",
            ),
        ]);
        let q =
            "Tell me about a time you disagreed with a technical decision and how you handled it.";
        let ranked = stories.rank(q);
        assert_eq!(ranked[0].story.label, "stories/tagged.md", "{ranked:?}");
        assert_eq!(ranked[0].tags, ["conflict"]);
        assert!(
            ranked[1].score > ranked[0].score,
            "the untagged story has the higher BM25 and still comes second"
        );
    }

    #[test]
    fn with_no_tag_match_bm25_ranks_and_unrelated_stories_are_left_out() {
        let stories = Stories::new(vec![
            story(
                "/s/stories/mentor.md",
                "tags: mentoring\nShe opened draft pull requests.",
            ),
            story(
                "/s/stories/kafka.md",
                "We replayed six hours of Kafka events after a bug.",
            ),
        ]);
        let ranked = stories.rank("Tell me about a time you used Kafka replay in an incident.");
        assert_eq!(ranked.len(), 1, "{ranked:?}");
        assert_eq!(ranked[0].story.label, "stories/kafka.md");
        assert!(ranked[0].tags.is_empty());
        assert!(stories.rank("What is the capital of France?").is_empty());
    }

    #[test]
    fn behavioral_questions_ask_for_a_story_and_others_do_not() {
        for q in [
            "Tell me about a time you disagreed with a technical decision.",
            "Let's talk about failure. Describe a project that didn't go well.",
            "Give me an example of a time you had to cut scope to ship something.",
            "Describe a situation where something was broken that wasn't your responsibility.",
            "Walk me through a time you had to deliver bad news to a stakeholder.",
            "Have you ever had to fire someone?",
        ] {
            assert!(asks_for_a_story(q), "{q}");
        }
        for q in [
            "How would you design a rate limiter for a globally distributed API?",
            "What's the difference between a process and a thread?",
            "What would you have done if he'd said no after seeing the spike?",
            "What time is it there?",
        ] {
            assert!(!asks_for_a_story(q), "{q}");
        }
    }
}
