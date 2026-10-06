//! The echo guard (docs/SPEC.md § Audio capture, speaker attribution rule 2; issue #60).
//!
//! When the mic hears the call itself (a laptop without echo cancellation, or questions read
//! from a phone in the same room: live session 5, the issue's motivation), every interviewer
//! line comes back as a You line a moment later. This guard drops those copies: a mic (You)
//! final whose `[start, end]` interval meets an interviewer final's (the gap between the two is
//! at most [`WINDOW`] either way) and whose text is a near-duplicate of it is dropped and
//! counted, never published. "Near-duplicate" is the SPEC's ratio ≥ 0.8 ([`MIN_SIMILARITY`]),
//! measured as [`containment`] (the mic text found inside the meeting interviewer text) for a
//! mic final of [`MIN_CONTAINED_WORDS`] or more, and as the whole-line [`similarity`] for a
//! shorter one: the two lanes cut the same speech at different pauses, so a copy is usually a
//! piece of an interviewer line, not the whole of it.
//!
//! Interviewer segments publish at once and are never held. A mic final with no match yet is
//! held, waiting for its loopback original, for [`HOLD`]; while an interviewer utterance that
//! could still meet it is open (its partials have arrived but not its final) the hold stretches,
//! up to [`MAX_HOLD`]. The two lanes are transcribed by separate whisper workers, so an original
//! and its echo reach the guard in either order and up to about a second apart; that is wall
//! time, not audio time, so the caller passes a wall clock (at `--speed 8` a 300 ms audio-time
//! hold would be 37 ms of real waiting, far shorter than the skew between the lanes). You
//! segments leave the guard in the order they came in: a partial that arrives behind a held
//! final waits with it, so a transcript that keeps one partial per source never sees a newer
//! utterance's partial evicted by an older final. Only the You lane pays the hold; the
//! detector scores Interviewer lines, which are never delayed.
//!
//! Empty finals (dmn-stt's "this utterance produced no text" marker) and Unknown-source
//! segments are not compared, only kept in order.
//!
//! #109: while `t` plays the user's translated speech into the `dmn-voice` virtual microphone,
//! the guard also drops anything on either lane whose audio meets that playback window
//! ([`EchoGuard::speaking_from`] / [`EchoGuard::speaking_until`], on audio time). A virtual
//! source is neither the mic nor the call's output, so in practice nothing arrives; the window
//! is a belt-and-braces guard against a setup that routes it back into one, not new echo logic.
//!
//! Based on Kimi's unpushed draft for #60 (the matcher and most tests); the wall-clock hold,
//! the open-utterance stretch, ordered release and split-line matching are new here.

use std::collections::VecDeque;
use std::time::Duration;

use dmn_audio::Source;

use crate::Segment;

/// The SPEC's 300 ms: two finals "meet" when the gap between their `[start, end]` intervals is
/// at most this, in either direction. An echo typically trails its original by ~100 ms.
pub const WINDOW: Duration = Duration::from_millis(300);

/// A mic final at or above this [`similarity`] with a meeting interviewer final is a copy.
pub const MIN_SIMILARITY: f64 = 0.8;

/// How long, in wall time from its arrival, a mic final waits for an interviewer final that
/// might condemn it when no interviewer utterance is open. Covers the skew between the two
/// whisper lanes (each final pass takes ~0.3–1 s on CPU and they run independently).
pub const HOLD: Duration = Duration::from_millis(1500);

/// The longest a mic final waits while an interviewer utterance that could meet it is still
/// open. Bounds the You lane's added latency whatever the interviewer lane is doing.
pub const MAX_HOLD: Duration = Duration::from_millis(4000);

/// Interviewer finals kept for matching, at most. One lane's finals arrive in time order, so
/// old ones are pruned as mic finals move past them; this caps a session in which the candidate
/// never speaks.
const KEEP: usize = 64;

/// Playback windows ([`EchoGuard::speaking_from`]) kept, at most: only recent ones can still
/// meet a segment on its way through whisper.
const KEEP_WINDOWS: usize = 8;

/// What the guard decided, in the order the pipeline must act on it.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Give this segment to the session, the observer, the detector and the log as usual.
    Publish(Segment),
    /// A mic final that duplicates the loopback: count it and never show its text. The pipeline
    /// still closes its already-shown partial with an empty final of the same key.
    Drop(Segment),
}

/// The issue's "ratio", defined exactly. Both texts are normalised (lowercased, every
/// non-alphanumeric character a word boundary, whitespace collapsed; the normalisation
/// `eval/run.py` scores with) and then, over the two word lists:
///
/// ```text
/// similarity = 1 − (word-level Levenshtein distance / max(word count of the two))
/// ```
///
/// Two empty texts are identical (1.0); one empty and one not are nothing alike (0.0).
pub fn similarity(a: &str, b: &str) -> f64 {
    let (x, y) = (words(a), words(b));
    if x.is_empty() || y.is_empty() {
        return if x.is_empty() && y.is_empty() {
            1.0
        } else {
            0.0
        };
    }
    1.0 - word_distance(&x, &y) as f64 / x.len().max(y.len()) as f64
}

/// The words of one text: lowercase, punctuation and whitespace both mere boundaries.
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Word-level Levenshtein: the fewest word insertions, deletions and substitutions that turn
/// `a` into `b`. Two DP rows; a segment is a sentence or two, not a book.
fn word_distance(a: &[String], b: &[String]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, x) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let substitute = prev[j] + usize::from(x != y);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(substitute);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The gap between two segments' `[start, end]` intervals; zero when they overlap.
fn gap(a: &Segment, b: &Segment) -> Duration {
    if a.end < b.start {
        b.start - a.end
    } else if b.end < a.start {
        a.start - b.end
    } else {
        Duration::ZERO
    }
}

/// True when the two finals are close enough in time to be original and echo.
fn meets(a: &Segment, b: &Segment) -> bool {
    gap(a, b) <= WINDOW
}

/// How well `mic` appears somewhere inside `orig`: 1 − (the fewest word edits that turn `mic`
/// into some contiguous stretch of `orig`) / (words in `mic`). A semi-global alignment: skipping
/// `orig`'s words before and after the stretch is free.
///
/// The SPEC's ratio compares whole lines, but the two lanes cut the same speech at different
/// places: on the echo fixture the quieter copy on the mic splits at pauses the clean loopback
/// lane runs straight through, so a copy is usually one sentence of a longer interviewer final
/// ("How would you design a rate limiter…" inside "Let's start with something concrete. How
/// would you design…"), and its whole-line ratio is 0.3–0.7. Containment scores those 1.0.
pub fn containment(mic: &str, orig: &str) -> f64 {
    let (x, y) = (words(mic), words(orig));
    if x.is_empty() || y.is_empty() {
        return 0.0;
    }
    // Row 0 is all zeros: the stretch may start anywhere in `orig`.
    let mut prev = vec![0; y.len() + 1];
    let mut cur = vec![0; y.len() + 1];
    for (i, a) in x.iter().enumerate() {
        cur[0] = i + 1;
        for (j, b) in y.iter().enumerate() {
            let substitute = prev[j] + usize::from(a != b);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(substitute);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    // ...and end anywhere: the best cell of the last row.
    let best = prev.iter().copied().min().unwrap_or(x.len());
    1.0 - best as f64 / x.len() as f64
}

/// Below this many words a mic final must match a whole interviewer line ([`similarity`]), not
/// just appear inside one ([`containment`]): a candidate's "Right." or "Makes sense." said over
/// the interviewer's last words would otherwise be found inside almost any line.
pub const MIN_CONTAINED_WORDS: usize = 3;

/// Whether `mic` duplicates the loopback in `ivs`. Only interviewer finals that meet it in time
/// count. A mic final of [`MIN_CONTAINED_WORDS`] or more is a copy when it is contained in them
/// (joined in time order, so an echo spanning a 12 s forced split still matches); a shorter one
/// when it is a near copy of one whole line.
fn duplicates(mic: &Segment, ivs: &VecDeque<Segment>) -> bool {
    let mut meeting: Vec<&Segment> = ivs.iter().filter(|iv| meets(iv, mic)).collect();
    if meeting.is_empty() {
        return false;
    }
    if words(&mic.text).len() < MIN_CONTAINED_WORDS {
        return meeting
            .iter()
            .any(|iv| similarity(&iv.text, &mic.text) >= MIN_SIMILARITY);
    }
    meeting.sort_by_key(|iv| iv.start);
    let joined = meeting
        .iter()
        .map(|iv| iv.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    containment(&mic.text, &joined) >= MIN_SIMILARITY
}

fn comparable(seg: &Segment) -> bool {
    seg.is_final && !seg.text.trim().is_empty()
}

/// What a queued You segment is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// A mic final that may still turn out to be an echo.
    Waiting,
    /// A mic final an interviewer final has since condemned: goes out as a drop.
    Condemned,
    /// Anything else (a partial, an empty final, a mic final already cleared): goes out as is,
    /// in order.
    Pass,
}

/// A You segment in the guard's queue; `since` is the caller's wall time at arrival.
struct Queued {
    seg: Segment,
    since: Duration,
    state: State,
}

/// The guard itself. Build with [`EchoGuard::new`]; drive with [`EchoGuard::observe`] (one
/// segment at a time, as the bus delivers them), [`EchoGuard::expire`] (on a tick) and
/// [`EchoGuard::finish`] (when the audio ends). `now` is always the caller's wall clock.
pub struct EchoGuard {
    enabled: bool,
    /// You segments not yet released, in arrival order.
    queue: VecDeque<Queued>,
    /// Interviewer finals a mic final could still duplicate, in arrival order.
    interviewers: VecDeque<Segment>,
    /// The interviewer utterance whose partials have arrived but whose final has not:
    /// `(start, end so far)`.
    open: Option<(Duration, Duration)>,
    dropped: u64,
    /// #109: audio-time spans in which `t` played speech into `dmn-voice`, oldest first; the
    /// last one's end is `None` while it plays.
    speaking: VecDeque<(Duration, Option<Duration>)>,
}

impl EchoGuard {
    /// `enabled` is `[audio].echo_guard`. Disabled, the guard publishes everything at once and
    /// drops nothing.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            queue: VecDeque::new(),
            interviewers: VecDeque::new(),
            open: None,
            dropped: 0,
            speaking: VecDeque::new(),
        }
    }

    /// Mic finals dropped so far, as copies of the loopback or (#109) inside a playback window.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// #109: dmn started playing speech into `dmn-voice` at audio time `at`. Until
    /// [`EchoGuard::speaking_until`], a segment on either lane whose audio meets the window is
    /// dropped: a virtual source is not on the loopback and not the mic, so this is a
    /// time-window guard in case a setup routes it back into one, not new echo logic.
    pub fn speaking_from(&mut self, at: Duration) {
        if !self.enabled {
            return;
        }
        self.speaking.push_back((at, None));
        while self.speaking.len() > KEEP_WINDOWS {
            self.speaking.pop_front();
        }
    }

    /// #109: the playback that [`EchoGuard::speaking_from`] began ended at audio time `at`.
    /// Segments still in whisper whose audio falls inside it are dropped when they arrive.
    pub fn speaking_until(&mut self, at: Duration) {
        if let Some((from, end @ None)) = self.speaking.back_mut() {
            *end = Some(at.max(*from));
        }
    }

    /// Whether `seg`'s audio meets a playback window.
    fn during_playback(&self, seg: &Segment) -> bool {
        self.speaking
            .iter()
            .any(|(from, until)| seg.end >= *from && until.is_none_or(|u| seg.start <= u))
    }

    /// You segments currently held back.
    pub fn held(&self) -> usize {
        self.queue.len()
    }

    /// Feed one segment from the bus; act on the returned verdicts in order.
    pub fn observe(&mut self, seg: Segment, now: Duration) -> Vec<Verdict> {
        if !self.enabled {
            return vec![Verdict::Publish(seg)];
        }
        // #109: nothing heard while dmn itself was speaking is taken as said, on either lane. A
        // final counts as dropped; a partial is closed like a dropped final's, uncounted.
        if seg.source != Source::Unknown && self.during_playback(&seg) {
            if seg.is_final && !seg.text.trim().is_empty() {
                self.dropped += 1;
            }
            return vec![Verdict::Drop(seg)];
        }
        match seg.source {
            Source::Interviewer => self.interviewer(seg, now),
            Source::You => self.you(seg, now),
            // Single-stream fallback: there is no second lane to echo from.
            _ => vec![Verdict::Publish(seg)],
        }
    }

    /// Release held You segments whose wait is over at `now`.
    pub fn expire(&mut self, now: Duration) -> Vec<Verdict> {
        self.release(now)
    }

    /// Release everything still held: the audio has ended, so no interviewer final can condemn
    /// it any more (a copy already condemned still goes out as a drop).
    pub fn finish(&mut self) -> Vec<Verdict> {
        self.open = None;
        let mut out = Vec::with_capacity(self.queue.len());
        for q in std::mem::take(&mut self.queue) {
            out.push(self.verdict(q));
        }
        out
    }

    fn interviewer(&mut self, seg: Segment, now: Duration) -> Vec<Verdict> {
        if !seg.is_final {
            self.open = Some((seg.start, seg.end));
            return vec![Verdict::Publish(seg)];
        }
        if self.open.is_some_and(|(start, _)| start <= seg.start) {
            self.open = None;
        }
        if comparable(&seg) {
            self.interviewers.push_back(seg.clone());
            while self.interviewers.len() > KEEP {
                self.interviewers.pop_front();
            }
            // The original may condemn mic finals already waiting.
            for i in 0..self.queue.len() {
                if self.queue[i].state == State::Waiting
                    && duplicates(&self.queue[i].seg, &self.interviewers)
                {
                    self.queue[i].state = State::Condemned;
                }
            }
        }
        let mut out = vec![Verdict::Publish(seg)];
        out.extend(self.release(now));
        out
    }

    fn you(&mut self, seg: Segment, now: Duration) -> Vec<Verdict> {
        let state = if comparable(&seg) {
            // An interviewer final too old to meet this one can never meet a later one either:
            // the mic lane's finals arrive in time order.
            while self
                .interviewers
                .front()
                .is_some_and(|iv| iv.end + WINDOW < seg.start)
            {
                self.interviewers.pop_front();
            }
            if duplicates(&seg, &self.interviewers) {
                State::Condemned
            } else {
                State::Waiting
            }
        } else {
            State::Pass
        };
        if self.queue.is_empty() && state != State::Waiting {
            // Nothing to wait behind: decided at once.
            let q = Queued {
                seg,
                since: now,
                state,
            };
            return vec![self.verdict(q)];
        }
        self.queue.push_back(Queued {
            seg,
            since: now,
            state,
        });
        self.release(now)
    }

    /// Whether the queue's head can go out at `now`.
    fn ripe(&self, q: &Queued, now: Duration) -> bool {
        if q.state != State::Waiting {
            return true;
        }
        let waited = now.saturating_sub(q.since);
        if waited >= MAX_HOLD {
            return true;
        }
        let could_meet = self
            .open
            .is_some_and(|(start, _)| start <= q.seg.end + WINDOW);
        !could_meet && waited >= HOLD
    }

    fn verdict(&mut self, q: Queued) -> Verdict {
        if q.state == State::Condemned {
            self.dropped += 1;
            Verdict::Drop(q.seg)
        } else {
            Verdict::Publish(q.seg)
        }
    }

    /// Release the queue's head while it can go, in order.
    fn release(&mut self, now: Duration) -> Vec<Verdict> {
        let mut out = Vec::new();
        while self.queue.front().is_some_and(|head| self.ripe(head, now)) {
            let Some(q) = self.queue.pop_front() else {
                break;
            };
            out.push(self.verdict(q));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TranscriptLog;

    fn seg(source: Source, start_ms: u64, end_ms: u64, text: &str, is_final: bool) -> Segment {
        Segment {
            source,
            start: ms(start_ms),
            end: ms(end_ms),
            text: text.to_owned(),
            is_final,
            avg_logprob: -0.2,
            language: None,
        }
    }

    fn mic(start_ms: u64, end_ms: u64, text: &str) -> Segment {
        seg(Source::You, start_ms, end_ms, text, true)
    }

    fn interviewer(start_ms: u64, end_ms: u64, text: &str) -> Segment {
        seg(Source::Interviewer, start_ms, end_ms, text, true)
    }

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    /// Feed verdicts into a log, closing a dropped copy's partial the way the pipeline does.
    fn apply(log: &mut TranscriptLog, verdicts: &[Verdict]) {
        for v in verdicts {
            match v {
                Verdict::Publish(s) => {
                    log.apply(s.clone());
                }
                Verdict::Drop(s) => {
                    log.apply(Segment {
                        text: String::new(),
                        ..s.clone()
                    });
                }
            }
        }
    }

    fn published(log: &TranscriptLog) -> Vec<(Source, String)> {
        log.finals()
            .iter()
            .map(|s| (s.source, s.text.clone()))
            .collect()
    }

    const QUESTION: &str = "How would you design a rate limiter?";

    /// #109: while `t` plays speech into `dmn-voice` (audio 40–46 s here), a segment on either
    /// lane whose audio meets that window is dropped, counted when it is a final; one outside it
    /// is kept, including one whisper delivers after the window has closed.
    #[test]
    fn a_segment_inside_the_playback_window_is_dropped_and_one_outside_is_kept() {
        let mut guard = EchoGuard::new(true);
        let mut log = TranscriptLog::new();
        let now = ms(50_000);
        apply(
            &mut log,
            &guard.observe(mic(30_000, 33_000, "I'd shard by tenant."), now),
        );
        guard.speaking_from(ms(40_000));
        // While it plays: the mic, the loopback, a partial; all overlapping the open window.
        let during = [
            mic(41_000, 43_000, "Empezaría por clave."),
            interviewer(42_000, 44_000, "Mm-hm, go on."),
            seg(Source::Interviewer, 44_500, 45_000, "Sure", false),
        ];
        for s in during {
            let verdicts = guard.observe(s.clone(), now);
            assert_eq!(verdicts, [Verdict::Drop(s)]);
            apply(&mut log, &verdicts);
        }
        guard.speaking_until(ms(46_000));
        // Transcribed late, spoken inside the window: still dropped.
        let late = interviewer(45_500, 47_000, "Sounds good.");
        assert_eq!(guard.observe(late.clone(), now), [Verdict::Drop(late)]);
        // After the window, on both lanes: kept.
        let after = interviewer(46_500, 49_000, "And how would you test it?");
        assert_eq!(
            guard.observe(after.clone(), now),
            [Verdict::Publish(after.clone())]
        );
        apply(&mut log, &[Verdict::Publish(after)]);
        apply(
            &mut log,
            &guard.observe(mic(50_000, 52_000, "With a replay harness."), ms(60_000)),
        );
        apply(&mut log, &guard.finish());
        assert_eq!(
            guard.dropped(),
            3,
            "two finals in the window, one late; no partial"
        );
        assert_eq!(
            published(&log),
            [
                (Source::You, "I'd shard by tenant.".to_owned()),
                (Source::Interviewer, "And how would you test it?".to_owned()),
                (Source::You, "With a replay harness.".to_owned()),
            ]
        );
        assert!(
            log.open_partials().is_empty(),
            "the dropped partial is closed"
        );
    }

    #[test]
    fn with_the_guard_off_a_playback_window_drops_nothing() {
        let mut guard = EchoGuard::new(false);
        guard.speaking_from(ms(40_000));
        let s = mic(41_000, 43_000, "Empezaría por clave.");
        assert_eq!(guard.observe(s.clone(), ms(50_000)), [Verdict::Publish(s)]);
        assert_eq!(guard.dropped(), 0);
    }

    #[test]
    fn echo_similarity_is_exactly_as_defined() {
        assert_eq!(
            similarity(QUESTION, "how would you design a rate limiter"),
            1.0
        );
        assert_eq!(
            similarity("a b c d", "a b x d"),
            0.75,
            "one of four off: under the bar"
        );
        assert_eq!(
            similarity("a b c d e", "a b c d f"),
            0.8,
            "one of five off: at the bar"
        );
        assert_eq!(similarity("a b c d", "w x y z"), 0.0);
        assert_eq!(
            similarity("a b c", "a b"),
            1.0 - 1.0 / 3.0,
            "over the longer side"
        );
        assert_eq!(similarity("", ""), 1.0);
        assert_eq!(similarity("", "a"), 0.0);
        assert_eq!(similarity("?!", "..."), 1.0, "both normalise to no words");
    }

    #[test]
    fn echo_containment_finds_the_mic_text_inside_a_longer_line() {
        let line = "Let's start with something concrete. How would you design a rate limiter?";
        assert_eq!(
            containment(QUESTION, line),
            1.0,
            "one sentence of it, verbatim"
        );
        assert_eq!(
            containment(line, QUESTION),
            1.0 - 6.0 / 13.0,
            "not the other way round"
        );
        // One misheard word in seven: 1 − 1/7.
        assert_eq!(
            containment("How would you design a rate lifter", line),
            1.0 - 1.0 / 7.0
        );
        assert_eq!(
            containment("I would use a token bucket", line),
            1.0 - 5.0 / 6.0
        );
        assert_eq!(containment("", line), 0.0);
        assert_eq!(containment("anything", ""), 0.0);
    }

    #[test]
    fn echo_of_one_sentence_of_a_longer_interviewer_line_is_dropped() {
        // The echo fixture's pattern: the clean loopback runs two sentences together, the
        // quieter copy on the mic splits at the pause between them.
        let mut guard = EchoGuard::new(true);
        guard.observe(
            interviewer(
                20_000,
                29_800,
                "Let's start with something concrete. How would you design a rate limiter for \
                 a globally distributed API?",
            ),
            ms(0),
        );
        let first = mic(20_100, 21_800, "Let's start with something concrete.");
        let second = mic(
            22_400,
            26_100,
            "How would you design a rate limiter for a globally distributed API?",
        );
        assert_eq!(
            guard.observe(first.clone(), ms(100)),
            vec![Verdict::Drop(first)]
        );
        assert_eq!(
            guard.observe(second.clone(), ms(200)),
            vec![Verdict::Drop(second)]
        );
        assert_eq!(guard.dropped(), 2);
    }

    #[test]
    fn echo_guard_keeps_a_short_backchannel_said_over_the_interviewer() {
        // "Right." inside the interviewer's line would be a perfect containment match; under
        // MIN_CONTAINED_WORDS only a whole-line match counts, so the candidate's aside stays.
        let mut guard = EchoGuard::new(true);
        guard.observe(
            interviewer(
                10_000,
                14_000,
                "Right, so the cache sits in front. Makes sense?",
            ),
            ms(0),
        );
        let aside = mic(12_000, 12_400, "Right.");
        guard.observe(aside.clone(), ms(100));
        assert_eq!(guard.dropped(), 0);
        assert_eq!(guard.finish(), vec![Verdict::Publish(aside)]);
    }

    #[test]
    fn echo_arriving_after_its_original_is_dropped_at_once() {
        let mut guard = EchoGuard::new(true);
        let original = interviewer(10_000, 12_000, QUESTION);
        assert_eq!(
            guard.observe(original.clone(), ms(0)),
            vec![Verdict::Publish(original)],
            "interviewer lines are never held"
        );
        let echo = mic(10_100, 12_100, "How would you design a rate limiter");
        assert_eq!(
            guard.observe(echo.clone(), ms(200)),
            vec![Verdict::Drop(echo)]
        );
        assert_eq!((guard.dropped(), guard.held()), (1, 0));
    }

    #[test]
    fn echo_arriving_first_is_held_then_dropped_when_the_original_lands() {
        let mut guard = EchoGuard::new(true);
        let echo = mic(10_100, 12_100, QUESTION);
        assert_eq!(guard.observe(echo.clone(), ms(0)), vec![], "held");
        assert_eq!(guard.expire(ms(900)), vec![], "still inside the hold");
        let original = interviewer(10_000, 12_000, QUESTION);
        assert_eq!(
            guard.observe(original.clone(), ms(1_000)),
            vec![Verdict::Publish(original), Verdict::Drop(echo)],
            "a second later, the slower lane's original condemns it"
        );
        assert_eq!(guard.dropped(), 1);
    }

    #[test]
    fn echo_hold_is_wall_time_and_bounded() {
        let mut guard = EchoGuard::new(true);
        let line = mic(10_000, 11_000, "yes exactly");
        guard.observe(line.clone(), ms(5_000));
        assert_eq!(
            guard.expire(ms(5_000) + HOLD - ms(1)),
            vec![],
            "one ms early"
        );
        assert_eq!(
            guard.expire(ms(5_000) + HOLD),
            vec![Verdict::Publish(line)],
            "HOLD from arrival, on the caller's clock"
        );
        assert_eq!(guard.expire(ms(99_999)), vec![]);
    }

    #[test]
    fn echo_hold_stretches_for_an_open_interviewer_utterance_up_to_the_cap() {
        let mut guard = EchoGuard::new(true);
        // The interviewer lane has an utterance open (its partials are in) that overlaps the
        // mic final: its final may still condemn it, so the mic final waits past HOLD...
        guard.observe(
            seg(Source::Interviewer, 10_000, 11_000, "How would you", false),
            ms(0),
        );
        let line = mic(10_100, 12_100, QUESTION);
        guard.observe(line.clone(), ms(100));
        assert_eq!(
            guard.expire(ms(100) + HOLD),
            vec![],
            "stretched: an original may still land"
        );
        // ...but never past MAX_HOLD.
        assert_eq!(
            guard.expire(ms(100) + MAX_HOLD),
            vec![Verdict::Publish(line)]
        );
    }

    #[test]
    fn echo_hold_is_not_stretched_by_an_utterance_that_cannot_meet() {
        let mut guard = EchoGuard::new(true);
        // The interviewer started talking again well after the mic line ended.
        guard.observe(
            seg(Source::Interviewer, 20_000, 21_000, "Next", false),
            ms(0),
        );
        let line = mic(10_000, 12_000, "I would shard by tenant.");
        guard.observe(line.clone(), ms(100));
        assert_eq!(guard.expire(ms(100) + HOLD), vec![Verdict::Publish(line)]);
    }

    #[test]
    fn echo_guard_keeps_a_real_answer_overlapping_the_loopback() {
        let mut guard = EchoGuard::new(true);
        guard.observe(interviewer(10_000, 12_000, QUESTION), ms(0));
        // The candidate starts before the interviewer quite stops: overlapping, nothing alike.
        let answer = mic(
            11_500,
            14_000,
            "I would start with a token bucket per client.",
        );
        assert_eq!(guard.observe(answer.clone(), ms(100)), vec![]);
        assert_eq!(guard.expire(ms(100) + HOLD), vec![Verdict::Publish(answer)]);
        assert_eq!(guard.dropped(), 0);
    }

    #[test]
    fn echo_window_bound_is_inclusive_at_300_ms() {
        let mut guard = EchoGuard::new(true);
        guard.observe(interviewer(10_000, 12_000, QUESTION), ms(0));
        // 301 ms after the interviewer stopped: someone reading the line back, not an echo.
        guard.observe(mic(12_301, 14_500, QUESTION), ms(100));
        assert_eq!(guard.dropped(), 0);
        assert_eq!(
            guard.finish(),
            vec![Verdict::Publish(mic(12_301, 14_500, QUESTION))]
        );

        let mut guard = EchoGuard::new(true);
        guard.observe(interviewer(10_000, 12_000, QUESTION), ms(0));
        let at_bound = mic(12_300, 14_500, QUESTION);
        assert_eq!(
            guard.observe(at_bound.clone(), ms(100)),
            vec![Verdict::Drop(at_bound)]
        );
    }

    #[test]
    fn echo_guard_keeps_a_meeting_mic_final_under_the_similarity_bar() {
        let mut guard = EchoGuard::new(true);
        guard.observe(
            interviewer(
                10_000,
                12_000,
                "Tell me about a time you disagreed with a coworker.",
            ),
            ms(0),
        );
        // Same shape, different question: distance 3 over 11 words ≈ 0.73 < 0.8.
        guard.observe(
            mic(
                10_100,
                12_100,
                "Tell me about a time you disagreed with your manager today.",
            ),
            ms(100),
        );
        assert_eq!((guard.dropped(), guard.held()), (0, 1));
    }

    #[test]
    fn echo_of_a_line_split_at_12_s_matches_the_joined_halves() {
        let mut guard = EchoGuard::new(true);
        // The interviewer lane force-split one long question at 12 s; the echo lane's VAD
        // closed it once.
        guard.observe(
            interviewer(
                0,
                12_000,
                "Imagine you run a chat service with a million users and",
            ),
            ms(0),
        );
        guard.observe(
            interviewer(
                12_000,
                15_000,
                "every message must be searchable forever. How?",
            ),
            ms(50),
        );
        let echo = mic(
            100,
            15_100,
            "Imagine you run a chat service with a million users and every message must be \
             searchable forever, how?",
        );
        assert_eq!(
            guard.observe(echo.clone(), ms(100)),
            vec![Verdict::Drop(echo)]
        );
    }

    #[test]
    fn echo_guard_releases_you_segments_in_order_behind_a_held_final() {
        let mut guard = EchoGuard::new(true);
        let answer = mic(10_000, 12_000, "I would shard by tenant.");
        guard.observe(answer.clone(), ms(0));
        // The next utterance's partial arrives while the answer is still held: it must not
        // overtake it, or a one-partial-per-source transcript would lose it to the older final.
        let next = seg(Source::You, 13_000, 14_000, "And then", false);
        assert_eq!(guard.observe(next.clone(), ms(300)), vec![]);
        assert_eq!(
            guard.expire(HOLD),
            vec![Verdict::Publish(answer), Verdict::Publish(next)]
        );
        // With nothing held, partials pass straight through.
        let later = seg(Source::You, 20_000, 21_000, "So", false);
        assert_eq!(
            guard.observe(later.clone(), ms(5_000)),
            vec![Verdict::Publish(later)]
        );
    }

    #[test]
    fn echo_guard_never_compares_empty_finals_or_unknown_source() {
        let mut guard = EchoGuard::new(true);
        guard.observe(interviewer(10_000, 12_000, QUESTION), ms(0));
        let empty = mic(10_100, 12_100, "");
        assert_eq!(
            guard.observe(empty.clone(), ms(100)),
            vec![Verdict::Publish(empty)]
        );
        let unknown = seg(Source::Unknown, 10_100, 12_100, QUESTION, true);
        assert_eq!(
            guard.observe(unknown.clone(), ms(100)),
            vec![Verdict::Publish(unknown)]
        );
        assert_eq!((guard.dropped(), guard.held()), (0, 0));
    }

    #[test]
    fn echo_guard_disabled_publishes_everything_and_drops_nothing() {
        let mut guard = EchoGuard::new(false);
        guard.observe(interviewer(10_000, 12_000, QUESTION), ms(0));
        let echo = mic(10_100, 12_100, QUESTION);
        assert_eq!(
            guard.observe(echo.clone(), ms(100)),
            vec![Verdict::Publish(echo)]
        );
        assert_eq!(guard.dropped(), 0);
        assert_eq!(guard.expire(ms(99_000)), vec![]);
        assert_eq!(guard.finish(), vec![]);
    }

    #[test]
    fn echo_whose_original_never_comes_is_published_not_lost() {
        // The interviewer lane dropped the original as a hallucination; the mic copy is all
        // that survives, and it must not vanish too.
        let mut guard = EchoGuard::new(true);
        let line = mic(10_000, 12_000, QUESTION);
        guard.observe(line.clone(), ms(0));
        assert_eq!(guard.expire(HOLD), vec![Verdict::Publish(line)]);
        assert_eq!(guard.dropped(), 0);
    }

    #[test]
    fn echo_condemned_but_still_queued_at_the_end_goes_out_as_a_drop() {
        let mut guard = EchoGuard::new(true);
        let answer = mic(5_000, 8_000, "I would shard by tenant.");
        guard.observe(answer.clone(), ms(0));
        guard.observe(interviewer(10_000, 12_000, QUESTION), ms(100));
        let echo = mic(10_100, 12_100, QUESTION);
        assert_eq!(
            guard.observe(echo.clone(), ms(200)),
            vec![],
            "decided, but behind the answer"
        );
        assert_eq!(
            guard.finish(),
            vec![Verdict::Publish(answer), Verdict::Drop(echo)]
        );
        assert_eq!(guard.dropped(), 1);
    }

    /// The acceptance scenario at segment level: a two-lane stream built like the echo fixture
    /// (every interviewer question copied onto the mic ~100 ms late, plus real answers), with
    /// the lanes' finals reaching the guard in both orders.
    #[test]
    fn echo_fixture_scenario_drops_duplicates_and_keeps_the_answers() {
        let questions = [
            (
                1_000,
                4_500,
                "How would you design a rate limiter for a public API?",
            ),
            (
                20_000,
                24_000,
                "And what if Redis goes down? How does your design degrade?",
            ),
            (
                45_000,
                49_500,
                "Walk me through how you would test this end to end?",
            ),
        ];
        let answers = [
            (
                5_500,
                12_000,
                "I would start with a token bucket per API key, stored in Redis.",
            ),
            (
                25_000,
                33_000,
                "If Redis goes down I would fall back to in-process limiting and shed load.",
            ),
            (
                50_500,
                58_000,
                "I would write contract tests against the API, then load test the limiter.",
            ),
        ];
        let mut guard = EchoGuard::new(true);
        let mut log = TranscriptLog::new();
        let mut wall = 0;
        let mut feed =
            |guard: &mut EchoGuard, log: &mut TranscriptLog, s: Option<Segment>, step: u64| {
                wall += step;
                if let Some(s) = s {
                    apply(log, &guard.observe(s, ms(wall)));
                }
                apply(log, &guard.expire(ms(wall)));
            };
        for (i, (qs, qe, qt)) in questions.iter().enumerate() {
            let echo = mic(qs + 100, qe + 100, &qt.replace('?', ""));
            let original = interviewer(*qs, *qe, qt);
            // Alternate which lane's final lands first, up to 900 ms apart.
            if i % 2 == 0 {
                feed(&mut guard, &mut log, Some(original), 500);
                feed(&mut guard, &mut log, Some(echo), 300);
            } else {
                feed(&mut guard, &mut log, Some(echo), 500);
                feed(&mut guard, &mut log, Some(original), 900);
            }
            let (answer_start, answer_end, text) = answers[i];
            feed(
                &mut guard,
                &mut log,
                Some(mic(answer_start, answer_end, text)),
                2_000,
            );
            feed(&mut guard, &mut log, None, 2_000);
        }
        apply(&mut log, &guard.finish());

        let lines = published(&log);
        assert_eq!(guard.dropped(), 3, "every copied question: {lines:?}");
        for (_, _, qt) in &questions {
            assert!(
                lines.contains(&(Source::Interviewer, (*qt).to_owned())),
                "{qt}"
            );
            let copy = qt.replace('?', "");
            assert!(!lines.iter().any(|(_, t)| *t == copy), "copy gone: {copy}");
        }
        for (_, _, text) in &answers {
            assert!(
                lines.contains(&(Source::You, (*text).to_owned())),
                "answer kept: {text}"
            );
        }
        assert_eq!(lines.len(), 6, "{lines:?}");
    }
}
