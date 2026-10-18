//! Area 3: the transcript, then the status dots and shortcuts. The transcript is the last two
//! lines under the Approach on a stacked screen, a right-hand column on a wide one (#88), or the
//! whole screen above the status line with assistance off (#79); the last two are wrapped.
//!
//! #114: the status line opens with the voice tag while `[speak]` is on: `voice: you (en)` in
//! pass-through mode (the You lane's colour), `voice: → de` in translate mode (teal, amber
//! while a turn is in flight). A transcript line that went out translated is labelled
//! `You → de`, its translation dim on the row under it, and a mode change is one dim marker
//! row (`— translate mode on: → de —`).

use dmn_session::{
    Health, Line as TranscriptLine, QuestionSource, Section, SpeakStage, Speaker, VoiceMode,
};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use super::sections::key_of;
use super::text::{truncate, truncate_front, width, wrap};
use super::{Mode, View};

/// Transcript rows (SPEC § Transcript & status: the last two segments, one line each).
const TRANSCRIPT_LINES: usize = 2;
/// Speaker labels are padded to one width so the text lines up.
const LABEL_CELLS: usize = 11;
/// #79: the share of the local LLM in system memory, in percent, from which the status line says
/// `llm partly on cpu`. The default `qwen3.6:35b-a3b` normally sits at 4 % on the dev 4090
/// (`ollama ps`, 2026-09-29), and the 2 s budget was measured like that; loaded after GPU
/// whisper it is far more.
const LLM_CPU_NOTICE: u8 = 10;
/// Trails the open utterance's partial text.
const CURSOR: &str = "▍";

/// Where the transcript is drawn, which decides its shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TranscriptArea {
    /// Under the Approach: the last two lines, one row each, cut at the front.
    Strip,
    /// A right-hand column beside the question and Approach (#88), wrapped, newest at the bottom.
    Column,
    /// The whole screen above the status line, assistance off (#79), wrapped the same way.
    Screen,
}

pub(super) fn render_transcript(
    view: &View<'_>,
    shape: TranscriptArea,
    area: Rect,
    buf: &mut Buffer,
) {
    // The column's left edge is the line between it and the question and Approach.
    let borders = match shape {
        TranscriptArea::Column => Borders::TOP | Borders::LEFT,
        TranscriptArea::Strip | TranscriptArea::Screen => Borders::TOP,
    };
    let block = Block::new().borders(borders).title(Span::styled(
        " TRANSCRIPT ",
        Style::new().fg(Color::DarkGray),
    ));
    let inner = block.inner(area);
    block.render(area, buf);
    let cells = usize::from(inner.width);

    // Finals and open partials together, in speaking order; the newest two are shown.
    let s = view.session;
    let mut all: Vec<(&TranscriptLine, bool)> = s
        .transcript
        .iter()
        .map(|l| (l, false))
        .chain(s.partials.iter().map(|l| (l, true)))
        .collect();
    all.sort_by(|a, b| a.0.start.total_cmp(&b.0.start));
    if shape != TranscriptArea::Strip {
        // Bottom-aligned, like a terminal: the newest line sits just above the status line
        // however much of the window there is.
        let rows = wrapped(&all, cells, usize::from(inner.height));
        let used = u16::try_from(rows.len()).unwrap_or(inner.height);
        let [_, bottom] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(used)]).areas(inner);
        Paragraph::new(rows).render(bottom, buf);
        return;
    }
    // The newest rows: a line that went out translated takes two (#114), so it may be the only
    // one shown.
    let mut rows: Vec<Line<'static>> = Vec::new();
    for (l, partial) in all.iter().rev() {
        if rows.len() >= TRANSCRIPT_LINES {
            break;
        }
        let mut lines = transcript_lines(l, *partial, cells);
        // A line's own row comes before its translation: when only one fits, the label
        // (`You → de`) is the mark to keep.
        lines.truncate(TRANSCRIPT_LINES - rows.len());
        lines.reverse();
        rows.extend(lines);
    }
    rows.truncate(TRANSCRIPT_LINES);
    rows.reverse();
    Paragraph::new(rows).render(inner, buf);
}

/// #79, the transcript-first screen: the newest lines that fit in `rows`, each wrapped under
/// its speaker label, in speaking order. The oldest line shown loses its first rows when only its
/// end fits, so the newest words are always on screen.
fn wrapped(all: &[(&TranscriptLine, bool)], cells: usize, rows: usize) -> Vec<Line<'static>> {
    let indent = " ".repeat(LABEL_CELLS + 1);
    let mut out: Vec<Line<'static>> = Vec::new();
    for (l, partial) in all.iter().rev() {
        if out.len() >= rows {
            break;
        }
        if l.note {
            // #114: a marker the session wrote, one dim row, no speaker.
            out.push(Line::from(Span::styled(
                truncate(&l.text, cells),
                Style::new().fg(Color::DarkGray),
            )));
            continue;
        }
        let (label, colour) = label_of(l);
        let room = cells.saturating_sub(LABEL_CELLS + 1 + if *partial { width(CURSOR) } else { 0 });
        // Wrapped in full, not to `rows`: `wrap` keeps the first lines and marks the cut with
        // `…`, and the newest words are the ones to keep (Copilot, PR #80). The rows past the
        // screen's top are dropped below, oldest first.
        let mut text = wrap(&l.text, room, usize::MAX);
        if text.is_empty() {
            text.push(String::new());
        }
        let last = text.len() - 1;
        let mut lines: Vec<Line<'static>> = text
            .into_iter()
            .enumerate()
            .map(|(i, t)| {
                let head = if i == 0 {
                    Span::styled(
                        format!("{label:<LABEL_CELLS$} "),
                        Style::new().fg(colour).add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::raw(indent.clone())
                };
                let mut spans = vec![head, Span::raw(t)];
                if *partial && i == last {
                    spans.push(Span::styled(CURSOR, Style::new().fg(Color::DarkGray)));
                }
                Line::from(spans)
            })
            .collect();
        // #114: what the call heard, dim, under the line it translates; wrapped the same way.
        if let Some(tr) = &l.translation {
            let dim = Style::new().fg(Color::DarkGray);
            lines.extend(
                wrap(&format!("{SPOKEN_MARK}{tr}"), room, usize::MAX)
                    .into_iter()
                    .map(|t| Line::from(vec![Span::raw(indent.clone()), Span::styled(t, dim)])),
            );
        }
        // Newest first while collecting; each line's rows go in bottom-up too.
        out.extend(lines.into_iter().rev());
    }
    out.truncate(rows);
    out.reverse();
    out
}

fn speaker(s: Speaker) -> (&'static str, Color) {
    match s {
        Speaker::Interviewer => ("Interviewer", Color::Cyan),
        Speaker::You => ("You", Color::Magenta),
        Speaker::Unknown => ("?", Color::DarkGray),
    }
}

/// A line's label: the speaker, or `You → de` for a line that went out translated (#114).
fn label_of(l: &TranscriptLine) -> (String, Color) {
    let (label, colour) = speaker(l.speaker);
    match &l.spoken_as {
        Some(target) => (format!("{label} → {target}"), colour),
        None => (label.to_owned(), colour),
    }
}

/// #114: leads the translation under a line that went out translated (the question block's
/// own mark for the one being spoken).
const SPOKEN_MARK: &str = "» ";

/// The strip's rows for one line: one, cut at the front, and (#114) the translation it went
/// out as, dim, on a second; a marker is one dim row.
fn transcript_lines(l: &TranscriptLine, partial: bool, cells: usize) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    if l.note {
        return vec![Line::from(Span::styled(truncate(&l.text, cells), dim))];
    }
    let (label, colour) = label_of(l);
    let room = cells.saturating_sub(LABEL_CELLS + 1 + if partial { width(CURSOR) } else { 0 });
    let mut spans = vec![
        Span::styled(
            format!("{label:<LABEL_CELLS$} "),
            Style::new().fg(colour).add_modifier(Modifier::BOLD),
        ),
        Span::raw(truncate_front(&l.text, room)),
    ];
    if partial {
        spans.push(Span::styled(CURSOR, dim));
    }
    let mut lines = vec![Line::from(spans)];
    if let Some(tr) = &l.translation {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(LABEL_CELLS + 1)),
            Span::styled(truncate(&format!("{SPOKEN_MARK}{tr}"), room), dim),
        ]));
    }
    lines
}

pub(super) fn render_status(view: &View<'_>, area: Rect, buf: &mut Buffer) {
    let st = &view.session.status;
    let mut spans = Vec::new();
    let mut detail = None;
    // #114: what `dmn-voice` carries, first and wide, so which voice the call hears is never
    // in doubt: your own (the You lane's colour), or the translation (teal; amber while a
    // turn is in flight). Assistance on or off.
    if let Some(v) = &view.session.voice {
        let (text, bg) = match v.mode {
            VoiceMode::Passthrough => {
                let own = v
                    .own
                    .as_ref()
                    .map(|l| format!(" ({l})"))
                    .unwrap_or_default();
                (format!(" {VOICE_YOU}{own} "), Color::Magenta)
            }
            VoiceMode::Translate => {
                let bg = if view.session.speech.is_some() {
                    Color::Yellow
                } else {
                    Color::Cyan
                };
                (format!(" {VOICE_TO} {} ", v.target), bg)
            }
            // #124: push-to-talk: red while the button is held (the call hears nothing yet),
            // then as translate mode.
            VoiceMode::Ptt => {
                if view.session.ptt == Some(true) {
                    (format!(" {VOICE_PTT_HELD} {} ", v.target), Color::Red)
                } else {
                    let bg = if view.session.speech.is_some() {
                        Color::Yellow
                    } else {
                        Color::Cyan
                    };
                    (format!(" {VOICE_PTT} {} ", v.target), bg)
                }
            }
        };
        spans.push(Span::styled(
            text,
            Style::new()
                .fg(Color::Black)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // #59: the model dot is named after the provider answering, once one has been announced.
    let model = st.provider.as_ref().map_or("model", |p| p.name.as_str());
    for (name, health) in [("mic", &st.mic), ("system", &st.system), (model, &st.model)] {
        let (dot, style, suffix) = match health {
            Health::Ok => ("●", Style::new().fg(Color::Green), ""),
            Health::Unknown => ("○", Style::new().fg(Color::DarkGray), ""),
            Health::Muted => ("○", Style::new().fg(Color::Yellow), " muted"),
            Health::AllAudio => ("●", Style::new().fg(Color::Yellow), ": all audio"),
            Health::Error(why) => {
                // The dots are the only place errors appear (SPEC § Transcript & status); the
                // first error's reason stands in for the window's tooltip.
                if detail.is_none() {
                    detail = Some(format!("{name}: {why}"));
                }
                ("●", Style::new().fg(Color::Red), "")
            }
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(dot, style));
        spans.push(Span::raw(format!(" {name}{suffix} ")));
    }
    // #102: beside the model dot, while the web lookup is on: a search query may leave the
    // machine. Nothing is asked while assistance is off, so nothing is said then.
    if st.web && !view.session.assist_off {
        spans.push(Span::styled(" web ", Style::new().fg(Color::DarkGray)));
    }
    // #16: ring-buffer overruns (audio dropped because a consumer stalled) show beside the dots,
    // and only once there are any.
    if st.overruns > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled("●", Style::new().fg(Color::Red)));
        spans.push(Span::raw(format!(" {} dropped ", st.overruns)));
    }
    // #61: which device transcribes. `gpu+cpu` is the usual split beside a local LLM: the
    // interviewer's lane on the GPU, yours on the CPU. Amber only when a GPU was asked for and
    // no lane got one (the reason is in the log and the replay header).
    if !st.stt.is_empty() {
        let on_gpu = st.stt.contains("GPU ");
        let on_cpu = st.stt.split("; ").any(|lane| lane.starts_with("CPU"));
        let device = match (on_gpu, on_cpu) {
            (true, true) => "gpu+cpu",
            (true, false) => "gpu",
            _ => "cpu",
        };
        let (text, style) = if st.stt_fallback && !on_gpu {
            (
                format!(" stt {device} (no gpu) "),
                Style::new().fg(Color::Yellow),
            )
        } else {
            (format!(" stt {device} "), Style::new().fg(Color::DarkGray))
        };
        spans.push(Span::styled(text, style));
    }
    // #77: what whisper heard, when it was not English (the transcript text is translated).
    if !st.language.is_empty() && st.language != "en" {
        spans.push(Span::styled(
            format!(" {}→en ", st.language),
            Style::new().fg(Color::DarkGray),
        ));
    }
    // #90: whose questions this run answers, when it is not the default. Nothing is shown while
    // assistance is off: nothing is detected then, whatever the source.
    if !view.session.assist_off && view.session.question_source != QuestionSource::Interviewer {
        spans.push(Span::styled(
            format!(" asks: {} ", view.session.question_source.as_str()),
            Style::new().fg(Color::DarkGray),
        ));
    }
    // #109: a `t` press in flight, in the You lane's colour (it is your voice), with the seconds
    // since the key: the TTS server takes 10 s and more on a CPU, so the wait is made legible.
    // Shown with assistance off too: `t` is a speech feature, not an answer feature.
    if let Some(sp) = &view.session.speech {
        let stage = match sp.stage {
            SpeakStage::Translating => TRANSLATING,
            SpeakStage::Speaking => SPEAKING,
        };
        spans.push(Span::styled(
            format!(" {stage} {}s ", sp.secs),
            Style::new().fg(Color::Magenta),
        ));
    }
    // #79: a local model that loaded partly into system memory (whisper took the GPU first) is
    // named in amber: answers come, but slower. (Assistance off is said on the right, with the
    // keys, where nothing cuts it.)
    if !view.session.assist_off && st.llm_cpu_percent >= LLM_CPU_NOTICE {
        spans.push(Span::styled(
            " llm partly on cpu ",
            Style::new().fg(Color::Yellow),
        ));
    }
    // An error's reason first (red); otherwise, when transcript text leaves the machine, the
    // SPEC § Providers notice in words (amber) for as long as it does.
    let note = detail.map(|d| (d, Color::Red)).or_else(|| {
        st.provider
            .as_ref()
            .filter(|p| p.remote)
            .map(|_| ("transcript leaves this machine".to_owned(), Color::Yellow))
    });
    let used = Line::from(spans.clone()).width();
    // #96: the keys as long as the note fits whole beside them, `⏎ ask` going first: the note
    // had those cells before the key did. `← earlier` (#94) is the next to go. A note too long
    // for any form is cut, as it always was, beside the full keys.
    // #114: the mode key (`l translate` / `l own voice`) is added only when the whole line
    // then fits: the voice tag at the front already says the mode, and a press's seconds or a
    // dot are worth more than a hint on a narrow pane.
    let Keys { lead, forms, voice } = shortcuts(view);
    let whole = |keys: &&String| {
        note.as_ref()
            .is_none_or(|(d, _)| used + 2 + width(d) + width(keys) <= usize::from(area.width))
    };
    let note_cells = note.as_ref().map_or(0, |(d, _)| 2 + width(d));
    let with_voice = forms
        .first()
        .zip(voice.as_ref())
        .map(|(first, key)| format!("{}{SEP}{key} ", first.trim_end()));
    let keys = with_voice
        .filter(|keys| used + note_cells + width(keys) <= usize::from(area.width))
        .or_else(|| forms.iter().find(whole).or(forms.first()).cloned())
        .unwrap_or_default();
    let keys_cells = u16::try_from(width(&keys)).unwrap_or(u16::MAX);
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(keys_cells)]).areas(area);
    if let Some((d, color)) = note {
        let room = usize::from(left.width).saturating_sub(used + 2);
        if room > 3 {
            spans.push(Span::styled(
                format!(" {}", truncate(&d, room)),
                Style::new().fg(color),
            ));
        }
    }
    Paragraph::new(Line::from(spans)).render(left, buf);
    // #79: the right-hand area is never cut (the left one is, on a narrow pane), so the one
    // state that changes what the screen is for leads it, in amber.
    // #96: the answer to an Enter the model declined leads it the same way, for its 5 s, and so
    // does a web lookup that came to nothing (#102).
    // #94: and so does the count of questions that arrived behind an earlier one.
    let rest = keys.strip_prefix(lead.as_str()).unwrap_or(&keys).to_owned();
    let right_spans = vec![
        Span::styled(lead, Style::new().fg(Color::Yellow)),
        Span::styled(rest, Style::new().fg(Color::DarkGray)),
    ];
    Paragraph::new(Line::from(right_spans)).render(right, buf);
}

/// The status line's right-hand side: the keys in every form they can take, longest first, and
/// the state that leads each of them in amber (empty when none does).
struct Keys {
    lead: String,
    forms: Vec<String>,
    /// #114: the mode key, appended to the longest form when the line has room for it.
    voice: Option<String>,
}

impl Keys {
    /// `lead`, then `parts`, each form one part shorter than the one before. The lead is the
    /// last to go; with none, the first part (the two keys for the state) always stays.
    fn new(lead: &[String], parts: &[String], voice: Option<String>) -> Self {
        let least = usize::from(lead.is_empty());
        let forms = (least..=parts.len())
            .rev()
            .map(|n| {
                let shown: Vec<&str> = lead.iter().chain(&parts[..n]).map(String::as_str).collect();
                format!("{} ", shown.join(SEP))
            })
            .collect();
        Self {
            lead: lead.join(SEP),
            forms,
            voice,
        }
    }
}

/// Between two keys, and between the state that leads them and the first.
const SEP: &str = " · ";

/// #79: how the status line names assistance off; it leads the keys.
const ASSIST_OFF: &str = "assist off";

/// #96: what the status line says when the model declined what Enter sent (`is_question: false`
/// or small talk); it leads the keys while [`dmn_session::Session::not_a_question`] is set.
const NOT_A_QUESTION: &str = "not a question";

/// #102: what the status line says for 5 s after a web lookup came to nothing; it leads the keys
/// while [`dmn_session::Session::lookup_failed`] is set.
const LOOKUP_FAILED: &str = "lookup failed";

/// #96: Enter asks about what the asking lane last said (the interviewer, by default; the hint
/// names no lane, so it reads the same under `asks: you` and `asks: both`). Offered whenever
/// assistance is on: the detector's miss it is for can come in any state of the screen.
const ASK: &str = "⏎ ask";

/// The two most useful keys for the state on screen (SPEC § Transcript & status: "Footer shows
/// the two most useful shortcuts for the current state"). While a coding question is on screen
/// the second one is `s` with the coding-solutions mode it would switch from (#50), so which
/// mode Deeper Answer follows is always visible.
///
/// Once the question on screen has section rows (#56), the first key is about them: `1–6 more`
/// while all are closed, the open one's own key to close it (`2 close`), or `↑↓ scroll` while
/// its body is taller than its rows (#94).
///
/// While assistance is on, `⏎ ask` follows the two (#96), and `not a question` leads them for
/// the 5 s after the model declined what Enter sent. While `[speak]` is on `t speak` comes last
/// (#109), and for 5 s after a press spoke nothing, why leads them. Once the session has an earlier question,
/// `← earlier` comes before `⏎ ask` (#94).
///
/// On an earlier question (#94) the two are the ways out of it, `← earlier` (not on the first)
/// and `→ later`, or `→ live` from the newest of them, followed by the sections' key and
/// `⏎ ask`; `p`, `r` and `w` act on the live question and are not offered. The count of
/// questions that arrived meanwhile (`2 new`) leads.
///
/// Returned longest first ([`Keys::new`]): [`render_status`] takes the first form that leaves
/// the status note whole.
fn shortcuts(view: &View<'_>) -> Keys {
    let coding = view.shown().is_some_and(|q| q.kind == CODING);
    let solutions = view.session.coding_solutions.label();
    let sections = view.shown().and_then(|q| {
        let visible = |s: &Section| q.sections.visible(*s);
        match q.open.filter(visible) {
            Some(_) if view.scrollable => Some(SCROLL.to_owned()),
            Some(open) => Some(format!("{} close", key_of(open))),
            None => Section::ALL
                .iter()
                .any(visible)
                .then(|| "1–6 more".to_owned()),
        }
    });
    let pin = if view.mode == Mode::Pinned {
        "p unpin"
    } else {
        "p pin"
    };
    // #58: a STAR block built from one of several matched stories offers Swap in place of the
    // pin or redo key.
    let swap = view
        .shown()
        .is_some_and(|q| q.star.is_some() && q.stories.len() > 1);
    // #109: why the last `t` spoke nothing leads the keys for its 5 s, assistance on or off.
    let speak_failed = view.session.speak_failed.as_ref().map(|n| n.reason.clone());
    let speak = view.session.speak.then(|| SPEAK.to_owned());
    // #114: the mode key, named for what it switches to; after `t speak`, and only when the
    // line has room for it (the tag at the front says the mode either way).
    let voice = view.session.voice.as_ref().map(|v| {
        match v.mode {
            VoiceMode::Passthrough => VOICE_TRANSLATE,
            // #124: `l` goes on to push-to-talk when a button is set up.
            VoiceMode::Translate if view.session.ptt.is_some() => VOICE_TO_PTT,
            VoiceMode::Translate | VoiceMode::Ptt => VOICE_OWN,
        }
        .to_owned()
    });
    if view.session.assist_off {
        // Ctrl+Shift+A in tmux's notation: short enough that an 80-column pane with every tag
        // (`system: all audio`, `es→en`) still shows the whole state. #109: `t` works with
        // assistance off, so its key follows, and goes first when the pane is narrow.
        let lead: Vec<String> = speak_failed
            .into_iter()
            .chain(std::iter::once(ASSIST_OFF.to_owned()))
            .collect();
        let lead = lead.join(SEP);
        let on = format!("{lead} · C-S-a on");
        let forms = match speak {
            Some(key) => vec![format!("{on} · {key} "), format!("{on} ")],
            None => vec![format!("{on} ")],
        };
        return Keys { lead, forms, voice };
    }
    // What leads the keys is the last part to go: the answer to Enter, and on an earlier
    // question the count of those that arrived behind it.
    let mut lead = Vec::new();
    if view.session.not_a_question.is_some() {
        lead.push(NOT_A_QUESTION.to_owned());
    }
    if view.session.lookup_failed.is_some() {
        lead.push(LOOKUP_FAILED.to_owned());
    }
    lead.extend(speak_failed);
    if let Some((at, of)) = view.session.place() {
        if view.session.unseen > 0 {
            lead.push(format!("{} new", view.session.unseen));
        }
        // `of` counts the live question, so the one before it is the newest earlier one.
        let forward = if at + 1 == of {
            "→ live"
        } else {
            "→ later"
        };
        let step = if at > 1 {
            format!("{EARLIER} · {forward}")
        } else {
            forward.to_owned()
        };
        let parts: Vec<String> = [Some(step), sections, Some(ASK.to_owned()), speak]
            .into_iter()
            .flatten()
            .collect();
        return Keys::new(&lead, &parts, voice);
    }
    let pair = match (view.mode, view.loading, coding, sections) {
        (Mode::Listening, ..) | (Mode::Live, true, false, _) => "m mute · q quit".to_owned(),
        (_, _, true, Some(sec)) => format!("{sec} · s: {solutions}"),
        (_, _, true, None) => format!("{pin} · s: {solutions}"),
        (_, _, false, Some(sec)) if swap => format!("{sec} · w swap"),
        (_, _, false, Some(sec)) => format!("{sec} · {pin}"),
        (_, _, false, None) if swap => format!("{pin} · w swap"),
        (_, _, false, None) => format!("{pin} · r redo"),
    };
    let earlier = (!view.session.history.is_empty()).then(|| EARLIER.to_owned());
    let parts: Vec<String> = [Some(pair), earlier, Some(ASK.to_owned()), speak]
        .into_iter()
        .flatten()
        .collect();
    Keys::new(&lead, &parts, voice)
}

/// #109: `t` translates what you last said into `[speak].target` and speaks it in your voice.
/// Offered while `[speak]` is on, after `⏎ ask`.
const SPEAK: &str = "t speak";

/// #114: `l` in pass-through mode: switch to translate mode. Offered last, and only when the
/// line has room: the voice tag at the front says the mode.
const VOICE_TRANSLATE: &str = "l translate";
/// #114: `l` in translate mode: back to your own voice.
const VOICE_OWN: &str = "l own voice";
/// #124: the mode key in translate mode when a push-to-talk button is set up.
const VOICE_TO_PTT: &str = "l push-to-talk";
/// #124: the voice tag in push-to-talk mode, and while the button is held.
pub(super) const VOICE_PTT: &str = "voice: PTT →";
pub(super) const VOICE_PTT_HELD: &str = "● talking →";
/// #114: the voice tag in pass-through mode (`voice: you (en)` when the language is known).
pub(super) const VOICE_YOU: &str = "voice: you";
/// #114: the voice tag in translate mode, followed by the target (`voice: → de`).
pub(super) const VOICE_TO: &str = "voice: →";

/// #109: the status line's tag while the model translates a `t` press.
pub(super) const TRANSLATING: &str = "translating…";
/// #109: the tag while the TTS server renders the translation and it plays into `dmn-voice`.
pub(super) const SPEAKING: &str = "speaking…";

/// #94: `←` shows the question before the one on screen. Offered once there is one.
const EARLIER: &str = "← earlier";

/// #94: the arrows (and PgUp / PgDn) scroll the open section. Offered in place of its close key
/// while its body is taller than its rows.
const SCROLL: &str = "↑↓ scroll";

/// The fast phase's `kind` for a coding question (SPEC § Transcription and question detection).
const CODING: &str = "coding";
