//! Reducer and hub tests. The action logs here are built by hand in the shapes `dmn-core` maps
//! pipeline values into; `dmn-core`'s tests drive the same thing from the real detector and a
//! `MockProvider` fast phase.

use crate::*;

fn seg(speaker: Speaker, start: f64, end: f64, text: &str, is_final: bool) -> Action {
    Action::Segment {
        speaker,
        start,
        end,
        text: text.into(),
        is_final,
        avg_logprob: -0.2,
        language: None,
    }
}

fn candidate(id: u64, start: f64, text: &str) -> Action {
    Action::Candidate {
        id,
        speaker: Speaker::Interviewer,
        start,
        end: start + 4.0,
        text: text.into(),
        score: 5,
    }
}

fn shown(id: u64, question: &str, lines: &[&str]) -> Vec<Action> {
    let mut out = vec![
        Action::Question {
            candidate: id,
            text: question.into(),
            kind: "system_design".into(),
        },
        Action::Focus {
            candidate: id,
            text: "scale, failure".into(),
        },
    ];
    for (index, line) in lines.iter().enumerate() {
        out.push(Action::ApproachLine {
            candidate: id,
            index,
            text: (*line).into(),
        });
    }
    out.push(Action::Answered {
        candidate: id,
        clarification: false,
        payload: format!("{{\"question\":{question:?}}}"),
        at: 1_700_000_100 + id as i64,
    });
    out
}

/// A whole interview in miniature: chatter, a question, a clarification, a pin, a queued
/// question, an ignored candidate, a failure, a status change, unpin.
fn interview() -> Vec<Action> {
    let mut log = vec![
        Action::Started {
            label: "fixture".into(),
            at: 1_700_000_000,
        },
        Action::Status {
            dot: Dot::Model,
            health: Health::Ok,
        },
        seg(Speaker::Interviewer, 1.0, 3.0, "Hi, thanks for", false),
        seg(
            Speaker::Interviewer,
            1.0,
            5.0,
            "Hi, thanks for joining.",
            true,
        ),
        seg(Speaker::You, 5.5, 6.5, "Happy to be here.", true),
        seg(
            Speaker::Interviewer,
            8.0,
            12.0,
            "How would you design a rate limiter?",
            true,
        ),
        candidate(1, 8.0, "How would you design a rate limiter?"),
    ];
    log.extend(shown(
        1,
        "How would you design a rate limiter?",
        &["Clarify scope.", "Token bucket per key.", "Redis counters."],
    ));
    log.extend([
        seg(
            Speaker::Interviewer,
            30.0,
            33.0,
            "And what if Redis goes down?",
            true,
        ),
        candidate(2, 30.0, "And what if Redis goes down?"),
        Action::Clarification {
            candidate: 2,
            text: "What if Redis goes down?".into(),
        },
        Action::ApproachLine {
            candidate: 2,
            index: 0,
            text: "Fail open with local buckets.".into(),
        },
        Action::Answered {
            candidate: 2,
            clarification: true,
            payload: "{}".into(),
            at: 1_700_000_140,
        },
        Action::Pin,
        seg(
            Speaker::Interviewer,
            50.0,
            54.0,
            "Next: design a URL shortener.",
            true,
        ),
        candidate(3, 50.0, "Next: design a URL shortener."),
    ]);
    log.extend(shown(3, "Design a URL shortener.", &["Clarify QPS."]));
    log.extend([
        seg(Speaker::Interviewer, 70.0, 72.0, "Nice weather, huh?", true),
        candidate(4, 70.0, "Nice weather, huh?"),
        Action::Ignored {
            candidate: 4,
            reason: IgnoreReason::SmallTalk,
            score: 3,
        },
        seg(
            Speaker::Interviewer,
            90.0,
            94.0,
            "How do you test it?",
            true,
        ),
        candidate(5, 90.0, "How do you test it?"),
        Action::Failed {
            candidate: 5,
            error: "timeout after 4 s".into(),
        },
        Action::Status {
            dot: Dot::Mic,
            health: Health::Muted,
        },
        Action::Unpin,
        Action::Ended { at: 1_700_000_600 },
    ]);
    log
}

#[test]
fn a_question_fills_the_live_area_line_by_line() {
    let mut s = Session::default();
    let mut log = vec![
        seg(
            Speaker::Interviewer,
            8.0,
            12.0,
            "How would you design it?",
            true,
        ),
        candidate(1, 8.0, "How would you design it?"),
    ];
    for a in &log {
        reduce(&mut s, a);
    }
    assert_eq!(s.pending, Some(1), "loading: the previous Approach dims");
    assert!(s.live.is_none());

    log = shown(1, "How would you design it?", &["one", "two"]);
    reduce(&mut s, &log[0]);
    assert_eq!(s.live.as_ref().unwrap().text, "How would you design it?");
    assert!(s.live.as_ref().unwrap().approach.is_empty());
    reduce(&mut s, &log[1]);
    reduce(&mut s, &log[2]);
    assert_eq!(s.live.as_ref().unwrap().approach, ["one"]);
    for a in &log[3..] {
        reduce(&mut s, a);
    }
    let live = s.live.unwrap();
    assert_eq!(live.approach, ["one", "two"]);
    assert_eq!(live.focus, "scale, failure");
    assert_eq!(live.progress, Progress::Done);
    assert_eq!(s.pending, None);
}

#[test]
fn a_clarification_attaches_under_the_question_and_keeps_its_approach() {
    let s = Session::replay(&interview()[..20]);
    let live = s.live.unwrap();
    assert_eq!(live.candidate, 1);
    assert_eq!(
        live.approach,
        ["Clarify scope.", "Token bucket per key.", "Redis counters."],
        "the parent's Approach is not replaced"
    );
    assert_eq!(live.clarifications.len(), 1);
    assert_eq!(live.clarifications[0].text, "What if Redis goes down?");
    assert_eq!(
        live.clarifications[0].approach,
        ["Fail open with local buckets."]
    );
}

#[test]
fn a_clarification_with_nothing_on_screen_becomes_the_question() {
    let s = Session::replay(&[
        candidate(7, 1.0, "and if it's cold?"),
        Action::Clarification {
            candidate: 7,
            text: "What if the cache is cold?".into(),
        },
        Action::ApproachLine {
            candidate: 7,
            index: 0,
            text: "Warm it from the replica.".into(),
        },
    ]);
    let live = s.live.unwrap();
    assert_eq!(live.kind, "clarification");
    assert_eq!(live.approach, ["Warm it from the replica."]);
}

/// #63: the first half of a split question, shown, then its continuation.
fn split_question(pin: bool) -> Vec<Action> {
    let mut log = vec![candidate(1, 20.0, "How would you design a rate limiter?")];
    log.extend(shown(
        1,
        "How would you design a rate limiter?",
        &["Token bucket."],
    ));
    if pin {
        log.push(Action::Pin);
    }
    log.push(candidate(
        2,
        20.0,
        "How would you design a rate limiter? And if Redis dies?",
    ));
    log.push(Action::Continues {
        candidate: 2,
        continued: 1,
    });
    log.extend(shown(
        2,
        "How would you design a rate limiter, and what if Redis dies?",
        &["Token bucket.", "Fail open."],
    ));
    log
}

#[test]
fn a_continuation_replaces_its_first_half_in_place() {
    let s = Session::replay(&split_question(false));
    let live = s.live.as_ref().unwrap();
    assert_eq!(live.candidate, 2);
    assert_eq!(live.approach, ["Token bucket.", "Fail open."]);
    assert!(s.queued.is_empty(), "one question on screen, not two");
    assert_eq!(s.continuing, None, "the link is used once");
}

#[test]
fn a_continuation_of_the_pinned_question_replaces_it_instead_of_queueing() {
    let s = Session::replay(&split_question(true));
    assert!(s.pinned);
    assert_eq!(
        s.live.as_ref().unwrap().candidate,
        2,
        "the pinned question, refined"
    );
    assert!(
        s.queued.is_empty(),
        "no \"1 new\" badge for the rest of the same question"
    );
}

#[test]
fn a_continuation_whose_first_half_is_gone_is_a_new_question() {
    let log = vec![
        candidate(7, 20.0, "And if Redis dies?"),
        Action::Continues {
            candidate: 7,
            continued: 6,
        },
        Action::Question {
            candidate: 7,
            text: "What if Redis dies?".into(),
            kind: "system_design".into(),
        },
    ];
    let s = Session::replay(&log);
    assert_eq!(s.live.as_ref().unwrap().candidate, 7);
}

#[test]
fn pinned_questions_queue_and_unpin_shows_the_newest() {
    let log = interview();
    let at_pin_end = log
        .iter()
        .position(|a| matches!(a, Action::Answered { candidate: 3, .. }))
        .unwrap();
    let s = Session::replay(&log[..=at_pin_end]);
    assert!(s.pinned);
    assert_eq!(s.live.as_ref().unwrap().candidate, 1, "pinned stays");
    assert_eq!(s.queued.len(), 1, "the badge says 1 new");
    assert_eq!(
        s.queued[0].approach,
        ["Clarify QPS."],
        "a queued question still fills"
    );

    let end = Session::replay(&log);
    assert!(!end.pinned);
    assert!(end.queued.is_empty());
    assert_eq!(end.live.unwrap().candidate, 3, "unpin shows the newest");
}

#[test]
fn ignored_and_failed_candidates_change_nothing_on_screen() {
    let log = interview();
    let before_ignored = log
        .iter()
        .position(|a| matches!(a, Action::Candidate { id: 4, .. }))
        .unwrap();
    let before = Session::replay(&log[..before_ignored]);
    let after = Session::replay(&log[..before_ignored + 4]);
    assert_eq!(before.live, after.live);
    assert_eq!(before.queued, after.queued);
    assert_eq!(after.counts.ignored, 1);
    assert_eq!(after.pending, Some(5), "the next candidate is loading");

    let end = Session::replay(&log);
    assert_eq!(end.counts.failed, 1);
    assert_eq!(end.pending, None);
    assert_eq!(
        end.counts,
        Counts {
            candidates: 5,
            shown: 2,
            clarifications: 1,
            ignored: 1,
            failed: 1,
        }
    );
}

#[test]
fn partials_are_replaced_by_their_final_and_the_window_is_60_s() {
    let mut s = Session::default();
    reduce(&mut s, &seg(Speaker::You, 0.0, 1.0, "so I", false));
    reduce(&mut s, &seg(Speaker::You, 0.0, 2.0, "so I would", false));
    assert_eq!(s.partials.len(), 1);
    assert_eq!(s.partials[0].text, "so I would");
    reduce(
        &mut s,
        &seg(Speaker::You, 0.0, 3.0, "So I would shard it.", true),
    );
    assert!(s.partials.is_empty());
    assert_eq!(s.transcript.len(), 1);

    reduce(
        &mut s,
        &seg(Speaker::Interviewer, 80.0, 82.0, "Okay.", true),
    );
    assert_eq!(
        s.transcript.len(),
        1,
        "the line that ended at 3 s is out of the window"
    );
    assert_eq!(s.last_heard.as_deref(), Some("Okay."));

    reduce(&mut s, &seg(Speaker::You, 83.0, 84.0, "", true));
    assert_eq!(s.transcript.len(), 1, "an empty final adds nothing");
}

#[test]
fn the_listening_line_follows_the_question_source() {
    // The default: the interviewer's lane alone, as before #90.
    let mut s = Session::default();
    assert_eq!(s.question_source, QuestionSource::Interviewer);
    reduce(&mut s, &seg(Speaker::You, 1.0, 2.0, "Sure.", true));
    assert_eq!(s.last_heard, None);
    reduce(&mut s, &seg(Speaker::Interviewer, 2.0, 3.0, "Okay.", true));
    assert_eq!(s.last_heard.as_deref(), Some("Okay."));
    // At its default the source is left out of the log, so a pre-#90 session reads the same.
    let json = serde_json::to_string(&s).unwrap();
    assert!(!json.contains("question_source"), "{json}");
    assert!(json.contains("\"last_interviewer\""), "the wire name holds");

    let mut you = Session {
        question_source: QuestionSource::You,
        ..Session::default()
    };
    reduce(
        &mut you,
        &seg(Speaker::Interviewer, 1.0, 2.0, "Okay.", true),
    );
    assert_eq!(
        you.last_heard, None,
        "the lane that cannot ask is not shown"
    );
    reduce(
        &mut you,
        &seg(Speaker::You, 2.0, 3.0, "What's the SLA?", true),
    );
    assert_eq!(you.last_heard.as_deref(), Some("What's the SLA?"));
    // Labels are untouched: both lines are in the transcript under their own speaker.
    assert_eq!(you.transcript.len(), 2);
    assert_eq!(you.transcript[0].speaker, Speaker::Interviewer);
    assert_eq!(you.transcript[1].speaker, Speaker::You);

    let mut both = Session::default();
    reduce(
        &mut both,
        &Action::QuestionSource {
            source: QuestionSource::Both,
        },
    );
    reduce(&mut both, &seg(Speaker::You, 1.0, 2.0, "Mine.", true));
    assert_eq!(both.last_heard.as_deref(), Some("Mine."));
    reduce(
        &mut both,
        &seg(Speaker::Interviewer, 2.0, 3.0, "Theirs.", true),
    );
    assert_eq!(both.last_heard.as_deref(), Some("Theirs."));
    let json = serde_json::to_string(&both).unwrap();
    assert!(json.contains("\"question_source\":\"both\""), "{json}");
    assert_eq!(
        serde_json::from_str::<Session>(&json)
            .unwrap()
            .question_source,
        QuestionSource::Both
    );
}

#[test]
fn a_recorded_action_log_replays_to_an_identical_session() {
    let hub = SessionHub::default();
    let mut rx = hub.subscribe();
    let mut watch = hub.watch();
    for a in interview() {
        hub.dispatch(a);
    }
    let mut recorded = Vec::new();
    while let Ok(a) = rx.try_recv() {
        recorded.push(a);
    }
    assert_eq!(
        recorded,
        interview(),
        "the broadcast carries every action in order"
    );

    // Through JSON Lines and back, as a log file on disk would be.
    let jsonl: String = recorded
        .iter()
        .map(|a| serde_json::to_string(a).unwrap() + "\n")
        .collect();
    let parsed: Vec<Action> = jsonl
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let live = hub.snapshot();
    assert_eq!(Session::replay(&parsed), live);
    assert_eq!(Session::replay(&parsed), Session::replay(&parsed));
    assert!(watch.has_changed().unwrap());
    assert_eq!(*watch.borrow_and_update(), live, "watch mirrors the state");
    assert_eq!(live.ended_at, Some(1_700_000_600));
}

#[test]
fn started_resets_everything() {
    let mut s = Session::replay(&interview());
    reduce(
        &mut s,
        &Action::Started {
            label: "second".into(),
            at: 5,
        },
    );
    assert_eq!(
        s,
        Session {
            label: "second".into(),
            started_at: Some(5),
            ..Session::default()
        }
    );
}

#[test]
fn regenerate_dims_then_replaces_the_question_in_place() {
    let mut log = vec![candidate(1, 20.0, "How would you design a rate limiter?")];
    log.extend(shown(
        1,
        "Design a rate limiter",
        &["token bucket", "redis"],
    ));
    let before = Session::replay(&log);

    log.push(Action::Regenerate { candidate: 1 });
    let waiting = Session::replay(&log);
    let live = waiting.live.as_ref().expect("still live");
    assert!(live.regenerating, "the old Approach dims");
    assert_eq!(live.approach, ["token bucket", "redis"], "and stays");
    assert_eq!(waiting.pending, Some(1));

    log.extend(shown(
        1,
        "Design a rate limiter, differently",
        &["sliding window"],
    ));
    let after = Session::replay(&log);
    let live = after.live.as_ref().expect("still live");
    assert_eq!(live.candidate, 1);
    assert_eq!(live.text, "Design a rate limiter, differently");
    assert_eq!(live.approach, ["sliding window"]);
    assert!(!live.regenerating);
    assert_eq!(after.pending, None);
    assert_eq!(after.counts.shown, before.counts.shown + 1);
}

#[test]
fn regenerating_a_pinned_question_keeps_it_pinned_and_live() {
    let mut log = vec![candidate(1, 20.0, "q1")];
    log.extend(shown(1, "First", &["a"]));
    log.push(Action::Pin);
    log.push(candidate(2, 60.0, "q2"));
    log.extend(shown(2, "Second", &["b"]));
    log.push(Action::Regenerate { candidate: 1 });
    log.extend(shown(1, "First again", &["c"]));
    let s = Session::replay(&log);
    assert!(s.pinned);
    let live = s.live.as_ref().expect("live");
    assert_eq!((live.candidate, live.text.as_str()), (1, "First again"));
    assert_eq!(s.queued.len(), 1, "the queued question is untouched");
    assert_eq!(s.queued[0].text, "Second");
}

#[test]
fn a_declined_or_failed_regenerate_leaves_the_question_as_it_was() {
    let mut log = vec![candidate(1, 20.0, "q")];
    log.extend(shown(1, "Q", &["a", "b"]));
    for outcome in [
        Action::Ignored {
            candidate: 1,
            reason: IgnoreReason::NotAQuestion,
            score: 3,
        },
        Action::Failed {
            candidate: 1,
            error: "timeout".into(),
        },
    ] {
        let mut l = log.clone();
        l.push(Action::Regenerate { candidate: 1 });
        l.push(outcome);
        let s = Session::replay(&l);
        let live = s.live.as_ref().expect("live");
        assert!(!live.regenerating);
        assert_eq!(live.approach, ["a", "b"]);
        assert_eq!(live.progress, Progress::Done, "the earlier answer stands");
        assert_eq!(s.pending, None);
    }
}

#[test]
fn a_regenerate_that_fails_after_its_new_reply_started_is_marked_failed() {
    let mut log = vec![candidate(1, 20.0, "q")];
    log.extend(shown(1, "Q", &["a", "b"]));
    log.push(Action::Regenerate { candidate: 1 });
    log.push(Action::Question {
        candidate: 1,
        text: "Q, again".into(),
        kind: "system_design".into(),
    });
    log.push(Action::Failed {
        candidate: 1,
        error: "timeout".into(),
    });
    let s = Session::replay(&log);
    let live = s.live.as_ref().expect("live");
    assert_eq!(live.text, "Q, again", "what was shown stays");
    assert_eq!(live.progress, Progress::Failed("timeout".into()));
}

fn lookup(candidate: u64, query: &str) -> Action {
    Action::Lookup {
        candidate,
        query: query.into(),
    }
}

#[test]
fn a_lookup_marks_its_question_and_follows_the_newest_reply() {
    let go = "What is the latest stable major version of Go?";
    let mut log = vec![candidate(1, 20.0, go)];
    let mut first = shown(
        1,
        go,
        &[
            "Say 1.24 is the newest you know of.",
            "Say it may be out of date.",
        ],
    );
    // As the fast phase sends it: after the focus, before the Approach lines.
    first.insert(2, lookup(1, "latest stable Go release"));
    log.extend(first);
    let s = Session::replay(&log);
    assert_eq!(s.live.as_ref().unwrap().lookup, "latest stable Go release");

    // `r`: until the new reply starts, the question on screen (and its tag) is the old one.
    log.push(Action::Regenerate { candidate: 1 });
    assert_eq!(
        Session::replay(&log).live.unwrap().lookup,
        "latest stable Go release"
    );
    // The new reply names nothing to look up: the tag goes with the old reply.
    log.extend(shown(1, go, &["a", "b", "c"]));
    assert_eq!(Session::replay(&log).live.unwrap().lookup, "");
    // And a later one that does brings it back.
    log.push(Action::Regenerate { candidate: 1 });
    let mut again = shown(1, go, &["a", "b", "c"]);
    again.insert(2, lookup(1, "Go release history"));
    log.extend(again);
    assert_eq!(
        Session::replay(&log).live.unwrap().lookup,
        "Go release history"
    );

    // A declined retry leaves the question, tag and all, as it was.
    log.push(Action::Regenerate { candidate: 1 });
    log.push(Action::Ignored {
        candidate: 1,
        reason: IgnoreReason::NotAQuestion,
        score: 3,
    });
    assert_eq!(
        Session::replay(&log).live.unwrap().lookup,
        "Go release history"
    );

    // The next question starts without one; the earlier one keeps its tag in the history.
    log.push(candidate(2, 60.0, "How does a B-tree split a full node?"));
    log.extend(shown(
        2,
        "How does a B-tree split a full node?",
        &["a", "b", "c"],
    ));
    let s = Session::replay(&log);
    assert_eq!(s.live.as_ref().unwrap().lookup, "");
    assert_eq!(s.history[0].lookup, "Go release history");
}

#[test]
fn a_lookup_reaches_a_queued_question_and_a_session_without_one_serialises_as_before() {
    let mut log = vec![candidate(1, 20.0, "q1")];
    log.extend(shown(1, "First", &["a"]));
    log.push(Action::Pin);
    log.push(candidate(2, 60.0, "Which Kubernetes version is current?"));
    log.extend(shown(2, "Which Kubernetes version is current?", &["b"]));
    log.push(lookup(2, "current Kubernetes release"));
    let s = Session::replay(&log);
    assert_eq!(s.live.as_ref().unwrap().lookup, "");
    assert_eq!(s.queued[0].lookup, "current Kubernetes release");

    let plain = Session::replay(&interview());
    let json = serde_json::to_string(&plain).unwrap();
    assert!(!json.contains("lookup"), "{json}");
    let text = serde_json::to_string(&s).unwrap();
    assert!(text.contains(r#""lookup":"current Kubernetes release""#));
    assert_eq!(serde_json::from_str::<Session>(&text).unwrap(), s);
}

#[test]
fn a_lookup_that_came_back_tags_the_reply_built_on_it_until_the_question_starts_afresh() {
    // #102: the first reply flagged, then the second request's reply, in place, then `checked`.
    let go = "What is the latest stable major version of Go?";
    let mut log = vec![Action::WebLookup { enabled: true }, candidate(1, 20.0, go)];
    let mut first = shown(1, go, &["Say 1.24 is the newest you know of."]);
    first.insert(2, lookup(1, "latest stable Go release"));
    log.extend(first);
    log.push(Action::Regenerate { candidate: 1 });
    log.extend(shown(1, go, &["Say Go 1.25 is the latest, per go.dev."]));
    log.push(Action::Checked {
        candidate: 1,
        domain: "go.dev".into(),
    });
    let s = Session::replay(&log);
    assert!(s.status.web);
    let live = s.live.as_ref().unwrap();
    assert_eq!(live.candidate, 1);
    assert_eq!(live.checked, "go.dev");
    assert_eq!(live.approach, ["Say Go 1.25 is the latest, per go.dev."]);
    assert_eq!(s.counts.shown, 2, "one question, answered twice");
    assert!(
        s.history.is_empty(),
        "replaced in place, not a second question"
    );
    let text = serde_json::to_string(&s).unwrap();
    assert!(text.contains(r#""checked":"go.dev""#) && text.contains(r#""web":true"#));
    assert_eq!(serde_json::from_str::<Session>(&text).unwrap(), s);

    // `r` starts the question afresh: the new reply was not checked.
    log.push(Action::Regenerate { candidate: 1 });
    log.extend(shown(1, go, &["a", "b", "c"]));
    assert_eq!(Session::replay(&log).live.unwrap().checked, "");
}

fn checked(candidate: u64) -> Action {
    Action::Checked {
        candidate,
        domain: "go.dev".into(),
    }
}

/// Review on PR #104: every path that puts a new Approach in place of a checked one leaves it
/// untagged, because the new reply's `Question` rebuilds the question; only a reply that never
/// started (or was declined) leaves the checked Approach, and its tag, on screen.
#[test]
fn checked_never_outlives_the_approach_it_was_earned_by() {
    // Swap (#58): a behavioral question, checked, then built from the next story.
    let mut log = vec![candidate(1, 20.0, "Tell me about a time you disagreed")];
    log.extend(behavioral(1, "stories/a.md"));
    log.push(Action::Stories {
        candidate: 1,
        ranked: vec!["stories/a.md".into(), "stories/b.md".into()],
    });
    log.push(checked(1));
    assert_eq!(Session::replay(&log).live.unwrap().checked, "go.dev");
    log.push(Action::SwapStory { candidate: 1 });
    assert_eq!(
        Session::replay(&log).live.unwrap().checked,
        "go.dev",
        "until the new cards start, the checked ones are on screen"
    );
    log.extend(behavioral(1, "stories/b.md"));
    assert_eq!(Session::replay(&log).live.unwrap().checked, "");

    // A continuation (#63) replaces its checked first half.
    let mut log = split_question(false);
    log.insert(log.len() - 6, checked(1));
    let s = Session::replay(&log);
    assert_eq!(s.live.as_ref().unwrap().candidate, 2);
    assert_eq!(s.live.unwrap().checked, "");
    let mut pinned = split_question(true);
    pinned.insert(pinned.len() - 7, checked(1));
    assert_eq!(Session::replay(&pinned).live.unwrap().checked, "");

    // `r` on a checked answer whose reply starts and then fails: the half-built Approach is
    // not the checked one, and carries no tag.
    let go = "What is the latest stable major version of Go?";
    let mut log = vec![candidate(1, 20.0, go)];
    log.extend(shown(1, go, &["Say Go 1.25 is the latest."]));
    log.push(checked(1));
    log.push(Action::Regenerate { candidate: 1 });
    log.push(Action::Question {
        candidate: 1,
        text: go.into(),
        kind: "knowledge".into(),
    });
    log.push(Action::Failed {
        candidate: 1,
        error: "timeout".into(),
    });
    assert_eq!(Session::replay(&log).live.unwrap().checked, "");

    // The lookup's own second request failing midway: `Checked` is sent only after its
    // `Answered`, so the partial reply is untagged too.
    let mut log = vec![candidate(1, 20.0, go)];
    let mut first = shown(1, go, &["Say 1.24 is the newest you know of."]);
    first.insert(2, lookup(1, "latest stable Go release"));
    log.extend(first);
    log.push(Action::Regenerate { candidate: 1 });
    log.push(Action::Question {
        candidate: 1,
        text: go.into(),
        kind: "knowledge".into(),
    });
    log.push(Action::Failed {
        candidate: 1,
        error: "superseded by a newer question".into(),
    });
    let live = Session::replay(&log).live.unwrap();
    assert_eq!((live.checked.as_str(), live.lookup.as_str()), ("", ""));
}

#[test]
fn a_failed_lookup_says_so_until_its_notice_expires_and_changes_nothing_else() {
    let go = "What is the latest stable major version of Go?";
    let mut log = vec![candidate(1, 20.0, go)];
    let mut first = shown(1, go, &["Say 1.24 is the newest you know of."]);
    first.insert(2, lookup(1, "latest stable Go release"));
    log.extend(first);
    let before = Session::replay(&log);
    log.push(Action::LookupFailed { candidate: 1 });
    let s = Session::replay(&log);
    assert_eq!(s.lookup_failed, Some(1));
    assert_eq!(s.live, before.live, "the first answer and its tag stay");
    // Another candidate's notice ending leaves this one up.
    log.push(Action::NoticeExpired { candidate: 7 });
    assert_eq!(Session::replay(&log).lookup_failed, Some(1));
    log.push(Action::NoticeExpired { candidate: 1 });
    let s = Session::replay(&log);
    assert_eq!(s.lookup_failed, None);
    assert_eq!(s, before, "nothing else moved");
    // A session that never looked anything up serialises as before.
    let plain = serde_json::to_string(&Session::replay(&interview())).unwrap();
    for key in ["checked", "lookup_failed", "\"web\""] {
        assert!(!plain.contains(key), "{key}: {plain}");
    }
}

/// A behavioral question with STAR cards built from `story`.
fn behavioral(id: u64, story: &str) -> Vec<Action> {
    vec![
        Action::Question {
            candidate: id,
            text: "Tell me about a time you disagreed".into(),
            kind: "behavioral".into(),
        },
        Action::Star {
            candidate: id,
            cards: StarCards {
                situation: format!("from {story}"),
                task: "t".into(),
                action: "a".into(),
                result: "r".into(),
                story: Some(story.into()),
            },
        },
        Action::Answered {
            candidate: id,
            clarification: false,
            payload: "{}".into(),
            at: 1_700_000_100,
        },
    ]
}

#[test]
fn swap_cycles_through_the_ranked_stories_and_back_to_the_first() {
    let ranked = ["stories/a.md", "stories/b.md", "stories/c.md"];
    let mut log = vec![candidate(1, 20.0, "Tell me about a time you disagreed")];
    log.extend(behavioral(1, ranked[0]));
    log.push(Action::Stories {
        candidate: 1,
        ranked: ranked.iter().map(|s| (*s).into()).collect(),
    });
    let s = Session::replay(&log);
    assert_eq!(s.live.as_ref().unwrap().story_label(), Some("stories/a.md"));

    for want in ["stories/b.md", "stories/c.md", "stories/a.md"] {
        log.push(Action::SwapStory { candidate: 1 });
        let waiting = Session::replay(&log);
        let q = waiting.live.as_ref().unwrap();
        assert_eq!(q.story_label(), Some(want));
        assert!(q.regenerating, "the old cards dim until the new ones come");
        assert_eq!(waiting.pending, Some(1));
        // The reply for the swapped-to story replaces the question in place.
        log.extend(behavioral(1, want));
        let after = Session::replay(&log);
        let q = after.live.as_ref().unwrap();
        assert_eq!(q.story_label(), Some(want), "the reply keeps the choice");
        assert_eq!(q.stories.len(), 3, "and the ranking");
        assert_eq!(q.star.as_ref().unwrap().story.as_deref(), Some(want));
        assert!(!q.regenerating);
    }
    // Deterministic: the same log, the same session.
    assert_eq!(Session::replay(&log), Session::replay(&log.clone()));
}

#[test]
fn swap_with_one_story_or_none_says_so_and_asks_nothing() {
    let mut log = vec![candidate(1, 20.0, "Tell me about a time you disagreed")];
    log.extend(behavioral(1, "stories/only.md"));
    log.push(Action::Stories {
        candidate: 1,
        ranked: vec!["stories/only.md".into()],
    });
    log.push(Action::SwapStory { candidate: 1 });
    let s = Session::replay(&log);
    let q = s.live.as_ref().unwrap();
    assert!(q.no_other_story);
    assert!(!q.regenerating, "nothing is being asked");
    assert_eq!(q.story_label(), Some("stories/only.md"));
    assert_eq!(s.pending, None);

    let mut none = vec![candidate(2, 20.0, "Tell me about a time you failed")];
    none.extend(behavioral(2, "x"));
    none.push(Action::SwapStory { candidate: 2 });
    assert!(Session::replay(&none).live.unwrap().no_other_story);
}

#[test]
fn regenerate_of_an_unknown_candidate_changes_nothing() {
    let mut log = vec![candidate(1, 20.0, "q")];
    log.extend(shown(1, "Q", &["a"]));
    let before = Session::replay(&log);
    log.push(Action::Regenerate { candidate: 9 });
    assert_eq!(Session::replay(&log), before);
}

#[test]
fn measurements_change_nothing_on_screen_and_overruns_show_in_the_status() {
    let mut log = vec![candidate(1, 20.0, "q")];
    log.extend(shown(1, "Q", &["a"]));
    let before = Session::replay(&log);
    log.push(Action::Timing {
        candidate: 1,
        stage: Stage::Approach,
        at: 25.3,
    });
    log.push(Action::Cpu {
        at: 25.0,
        wall: 25.0,
        cpu: 3.1,
        whisper: 1.2,
    });
    assert_eq!(Session::replay(&log), before, "timing and CPU are log-only");

    log.push(Action::Overruns { total: 3 });
    assert_eq!(Session::replay(&log).status.overruns, 3);

    // A log of the new actions survives JSON Lines like every other action.
    let text: Vec<String> = log
        .iter()
        .map(|a| serde_json::to_string(a).unwrap())
        .collect();
    let back: Vec<Action> = text
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(back, log);
}

#[test]
fn a_status_saved_before_overruns_existed_still_loads() {
    let old = r#"{"mic":{"state":"ok"},"system":{"state":"ok"},"model":{"state":"unknown"}}"#;
    let status: Status = serde_json::from_str(old).unwrap();
    assert_eq!(status.overruns, 0);
}

#[test]
fn coding_solutions_default_to_hints_and_follow_each_action() {
    let mut log = vec![Action::Started {
        label: "coding".into(),
        at: 1,
    }];
    assert_eq!(
        Session::replay(&log).coding_solutions,
        CodingSolutions::Hints
    );

    log.push(Action::CodingSolutions {
        mode: CodingSolutions::Full,
    });
    assert_eq!(
        Session::replay(&log).coding_solutions,
        CodingSolutions::Full
    );

    // `s` toggles: each press is one action, so the log replays to the same mode.
    let mode = Session::replay(&log).coding_solutions.toggled();
    log.push(Action::CodingSolutions { mode });
    let s = Session::replay(&log);
    assert_eq!(s.coding_solutions, CodingSolutions::Hints);
    assert_eq!(Session::replay(&log), s, "deterministic");

    // Through JSON Lines like every other action.
    let back: Vec<Action> = log
        .iter()
        .map(|a| serde_json::from_str(&serde_json::to_string(a).unwrap()).unwrap())
        .collect();
    assert_eq!(back, log);
    assert_eq!(
        serde_json::to_string(&log[1]).unwrap(),
        r#"{"type":"coding_solutions","mode":"full"}"#
    );
}

#[test]
fn coding_solutions_labels_round_trip() {
    for m in [CodingSolutions::Hints, CodingSolutions::Full] {
        assert_eq!(CodingSolutions::from_label(m.label()), Some(m));
        assert_eq!(m.toggled().toggled(), m);
    }
    assert_eq!(CodingSolutions::from_label("solutions"), None);
}

#[test]
fn a_session_saved_before_coding_solutions_existed_still_loads() {
    let mut v = serde_json::to_value(Session::default()).unwrap();
    v.as_object_mut().unwrap().remove("coding_solutions");
    let s: Session = serde_json::from_value(v).unwrap();
    assert_eq!(s.coding_solutions, CodingSolutions::Hints);
}

// --- Slow phase (#55) --------------------------------------------------------------------------

fn slow(id: u64, field: SlowField) -> Action {
    Action::Slow {
        candidate: id,
        field,
    }
}

fn slow_ended(id: u64, end: SlowEnd) -> Action {
    Action::SlowEnded {
        candidate: id,
        end,
        payload: "{}".into(),
        at: 1_700_000_200,
    }
}

fn deeper(text: &str) -> SlowField {
    SlowField::Deeper {
        items: vec![DeeperItem {
            label: "Core".into(),
            text: text.into(),
        }],
    }
}

/// A question shown, its slow phase started, three sections filled, then `end`.
fn sections_log(end: SlowEnd) -> Vec<Action> {
    let mut log = vec![candidate(1, 10.0, "Design a cache?")];
    log.extend(shown(1, "How would you design a cache?", &["a", "b", "c"]));
    log.push(Action::SlowStarted { candidate: 1 });
    log.push(slow(1, deeper("Evict with LRU.")));
    log.push(slow(
        1,
        SlowField::Followups {
            items: vec!["What about TTLs?".into()],
        },
    ));
    log.push(slow(1, SlowField::Tradeoffs { items: vec![] }));
    log.push(slow_ended(1, end));
    log
}

#[test]
fn a_new_question_starts_with_every_section_loading() {
    let mut log = vec![candidate(1, 10.0, "Design a cache?")];
    log.extend(shown(1, "How would you design a cache?", &["a", "b", "c"]));
    let s = Session::replay(&log);
    let sections = &s.live.unwrap().sections;
    assert!(sections.is_untouched());
    for section in Section::ALL {
        assert_eq!(
            *sections.state(section),
            SectionState::Loading,
            "{section:?}"
        );
    }
}

#[test]
fn slow_fields_fill_their_sections_and_the_end_settles_the_rest() {
    let s = Session::replay(&sections_log(SlowEnd::Done));
    let q = s.live.unwrap();
    assert_eq!(q.sections.deeper[0].text, "Evict with LRU.");
    assert_eq!(q.sections.followups, ["What about TTLs?"]);
    assert_eq!(*q.sections.state(Section::Deeper), SectionState::Done);
    assert_eq!(
        *q.sections.state(Section::Tradeoffs),
        SectionState::Empty,
        "an empty list hides the row"
    );
    assert_eq!(
        *q.sections.state(Section::Example),
        SectionState::Empty,
        "a field that never came is empty once the phase is done"
    );
    assert_eq!(
        q.progress,
        Progress::Done,
        "the fast phase's state is untouched"
    );
}

#[test]
fn a_slow_timeout_keeps_what_arrived_and_marks_the_rest_slow() {
    let s = Session::replay(&sections_log(SlowEnd::TimedOut));
    let q = s.live.unwrap();
    assert_eq!(*q.sections.state(Section::Deeper), SectionState::Done);
    assert_eq!(*q.sections.state(Section::Example), SectionState::Slow);
    assert_eq!(
        *q.sections.state(Section::ResumeContext),
        SectionState::Slow
    );

    let failed = Session::replay(&sections_log(SlowEnd::Failed("boom".into())));
    assert_eq!(
        *failed.live.unwrap().sections.state(Section::Mention),
        SectionState::Failed("boom".into())
    );
}

#[test]
fn a_restarted_or_regenerated_question_keeps_its_sections_until_new_ones_arrive() {
    let mut log = sections_log(SlowEnd::Done);
    // `r`: the new fast reply replaces the question in place; its sections stay.
    log.push(Action::Regenerate { candidate: 1 });
    log.extend(shown(
        1,
        "How would you design a cache, again?",
        &["x", "y", "z"],
    ));
    let s = Session::replay(&log);
    let q = s.live.as_ref().unwrap();
    assert_eq!(q.text, "How would you design a cache, again?");
    assert_eq!(q.sections.deeper[0].text, "Evict with LRU.", "kept");

    // The new slow phase: loading again, old content still shown until replaced.
    log.push(Action::SlowStarted { candidate: 1 });
    let q = Session::replay(&log).live.unwrap();
    assert_eq!(*q.sections.state(Section::Deeper), SectionState::Loading);
    assert_eq!(q.sections.deeper[0].text, "Evict with LRU.");

    // It times out before Deeper Answer: the old content stays, marked slow.
    log.push(slow(
        1,
        SlowField::Followups {
            items: vec!["New?".into()],
        },
    ));
    log.push(slow_ended(1, SlowEnd::TimedOut));
    let q = Session::replay(&log).live.unwrap();
    assert_eq!(q.sections.deeper[0].text, "Evict with LRU.");
    assert_eq!(*q.sections.state(Section::Deeper), SectionState::Slow);
    assert_eq!(q.sections.followups, ["New?"]);
    assert_eq!(*q.sections.state(Section::Followups), SectionState::Done);

    // A stopped run leaves every section as it was.
    let before = Session::replay(&log);
    log.push(Action::SlowStarted { candidate: 1 });
    log.push(slow_ended(1, SlowEnd::Stopped));
    let after = Session::replay(&log).live.unwrap();
    assert_eq!(
        *after.sections.state(Section::Followups),
        SectionState::Loading
    );
    assert_eq!(
        after.sections.followups,
        before.live.unwrap().sections.followups
    );
}

#[test]
fn a_solution_belongs_to_deeper_answer() {
    let mut log = vec![candidate(1, 10.0, "Two sum?")];
    log.extend(shown(1, "Two sum?", &["a", "b", "c"]));
    log.push(Action::SlowStarted { candidate: 1 });
    log.push(slow(1, SlowField::Deeper { items: vec![] }));
    let q = Session::replay(&log).live.unwrap();
    assert_eq!(*q.sections.state(Section::Deeper), SectionState::Empty);
    log.push(slow(
        1,
        SlowField::Solution {
            code: CodeBlock {
                language: "python".into(),
                code: "def f(): pass".into(),
            },
        },
    ));
    let q = Session::replay(&log).live.unwrap();
    assert!(q.sections.solution.is_some());
    assert_eq!(*q.sections.state(Section::Deeper), SectionState::Done);

    // `s` to hints, then `r`: the new slow phase sends a Deeper Answer and no solution. The old
    // code must not stay on screen beside the new hints.
    log.push(Action::Regenerate { candidate: 1 });
    log.extend(shown(1, "Two sum?", &["x", "y", "z"]));
    log.push(Action::SlowStarted { candidate: 1 });
    let q = Session::replay(&log).live.unwrap();
    assert!(
        q.sections.solution.is_some(),
        "kept while the new phase loads"
    );
    log.push(slow(1, deeper("Use a hash map.")));
    log.push(slow_ended(1, SlowEnd::Done));
    let q = Session::replay(&log).live.unwrap();
    assert_eq!(
        q.sections.solution, None,
        "the new Deeper Answer replaced it"
    );
    assert_eq!(q.sections.deeper[0].text, "Use a hash map.");
}

#[test]
fn slow_actions_for_a_queued_question_fill_it_behind_the_pin() {
    let mut log = vec![candidate(1, 10.0, "Q1")];
    log.extend(shown(1, "Q1", &["a", "b", "c"]));
    log.push(Action::Pin);
    log.push(candidate(2, 30.0, "Q2"));
    log.extend(shown(2, "Q2", &["a", "b", "c"]));
    log.push(Action::SlowStarted { candidate: 2 });
    log.push(slow(2, deeper("For Q2.")));
    let s = Session::replay(&log);
    assert!(s.live.as_ref().unwrap().sections.deeper.is_empty());
    assert_eq!(s.queued[0].sections.deeper[0].text, "For Q2.");
}

#[test]
fn slow_actions_replay_identically_through_json_lines() {
    let mut log = sections_log(SlowEnd::TimedOut);
    log.push(slow(
        1,
        SlowField::ResumeContext {
            items: vec![Citation {
                source: "resume.md".into(),
                text: "Built a cache.".into(),
            }],
        },
    ));
    log.push(slow(
        1,
        SlowField::Example {
            code: Some(CodeBlock {
                language: "text".into(),
                code: "1 GB, 90% hit rate".into(),
            }),
        },
    ));
    let jsonl: Vec<String> = log
        .iter()
        .map(|a| serde_json::to_string(a).unwrap())
        .collect();
    assert!(
        jsonl
            .iter()
            .any(|l| l.contains(r#""type":"slow""#) && l.contains(r#""name":"resume_context""#))
    );
    assert!(jsonl.iter().any(|l| l.contains(r#""end":"timed_out""#)));
    let parsed: Vec<Action> = jsonl
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(parsed, log);
    assert_eq!(Session::replay(&parsed), Session::replay(&log));
    let q = Session::replay(&parsed).live.unwrap();
    assert_eq!(q.sections.resume_context[0].source, "resume.md");
}

#[test]
fn a_session_saved_before_sections_existed_still_loads() {
    let mut log = vec![candidate(1, 10.0, "Design a cache?")];
    log.extend(shown(1, "How would you design a cache?", &["a", "b", "c"]));
    let s = Session::replay(&log);
    let v = serde_json::to_value(&s).unwrap();
    assert!(
        v["live"].get("sections").is_none(),
        "untouched sections stay out of the JSON, so M1 fixtures are unchanged"
    );
    let back: Session = serde_json::from_value(v).unwrap();
    assert_eq!(back, s);
}

// --- Sections on screen (#56) ------------------------------------------------------------------

fn toggle(section: Section) -> Action {
    Action::ToggleSection { section }
}

fn open_section(log: &[Action]) -> Option<Section> {
    Session::replay(log).live.and_then(|q| q.open)
}

#[test]
fn rows_show_only_once_a_slow_phase_has_started() {
    let mut log = vec![candidate(1, 10.0, "Design a cache?")];
    log.extend(shown(1, "How would you design a cache?", &["a", "b", "c"]));
    let q = Session::replay(&log).live.unwrap();
    assert!(
        Section::ALL.iter().all(|s| !q.sections.visible(*s)),
        "no slow phase (off, or an M1 log): no rows, not six \"…\" hints for ever"
    );
    log.push(toggle(Section::Deeper));
    assert_eq!(
        open_section(&log),
        None,
        "a row not on screen does not open"
    );

    log.push(Action::SlowStarted { candidate: 1 });
    let q = Session::replay(&log).live.unwrap();
    assert!(
        Section::ALL.iter().all(|s| q.sections.visible(*s)),
        "loading rows show"
    );
}

#[test]
fn settled_rows_hide_when_empty_and_keep_a_slow_or_filled_row() {
    let q = Session::replay(&sections_log(SlowEnd::TimedOut))
        .live
        .unwrap();
    assert!(q.sections.visible(Section::Deeper), "filled");
    assert!(q.sections.visible(Section::Followups), "filled");
    assert!(!q.sections.visible(Section::Tradeoffs), "empty: hidden");
    assert!(q.sections.visible(Section::Example), "timed out: slow tag");

    let q = Session::replay(&sections_log(SlowEnd::Failed("boom".into())))
        .live
        .unwrap();
    assert!(
        q.sections.visible(Section::Deeper),
        "failed but has content"
    );
    assert!(
        !q.sections.visible(Section::Example),
        "failed with nothing: hidden, the error shows only as a dot"
    );
}

#[test]
fn opening_a_second_section_closes_the_first_and_its_own_key_closes_it() {
    let mut log = sections_log(SlowEnd::Done);
    log.push(toggle(Section::Deeper));
    assert_eq!(open_section(&log), Some(Section::Deeper));
    log.push(toggle(Section::Followups));
    assert_eq!(
        open_section(&log),
        Some(Section::Followups),
        "one open at a time"
    );
    log.push(toggle(Section::Followups));
    assert_eq!(open_section(&log), None, "its own key closes it");
    log.push(toggle(Section::Tradeoffs));
    assert_eq!(
        open_section(&log),
        None,
        "an empty, hidden row does not open"
    );
}

#[test]
fn slow_content_arriving_never_changes_which_section_is_open() {
    // Nothing open: every field of a slow phase arrives and nothing expands.
    let log = sections_log(SlowEnd::Done);
    assert_eq!(open_section(&log), None);

    // One open while loading: the other fields arriving leave it as it is.
    let mut log = vec![candidate(1, 10.0, "Design a cache?")];
    log.extend(shown(1, "How would you design a cache?", &["a", "b", "c"]));
    log.push(Action::SlowStarted { candidate: 1 });
    log.push(toggle(Section::Followups));
    log.push(slow(1, deeper("Evict with LRU.")));
    log.push(slow(
        1,
        SlowField::Followups {
            items: vec!["What about TTLs?".into()],
        },
    ));
    log.push(slow_ended(1, SlowEnd::Done));
    assert_eq!(open_section(&log), Some(Section::Followups));
}

#[test]
fn an_open_row_that_settles_empty_closes() {
    let mut log = vec![candidate(1, 10.0, "Design a cache?")];
    log.extend(shown(1, "How would you design a cache?", &["a", "b", "c"]));
    log.push(Action::SlowStarted { candidate: 1 });
    log.push(toggle(Section::Tradeoffs));
    assert_eq!(
        open_section(&log),
        Some(Section::Tradeoffs),
        "a loading row opens"
    );
    log.push(slow(1, SlowField::Tradeoffs { items: vec![] }));
    assert_eq!(
        open_section(&log),
        None,
        "it had nothing, so its row went away"
    );
}

#[test]
fn a_new_question_starts_collapsed_and_a_regenerate_keeps_the_open_row() {
    let mut log = sections_log(SlowEnd::Done);
    log.push(toggle(Section::Deeper));
    // `r`: the same question is answered again in place; the row the user opened stays open.
    log.push(Action::Regenerate { candidate: 1 });
    log.extend(shown(
        1,
        "How would you design a cache, again?",
        &["x", "y", "z"],
    ));
    assert_eq!(open_section(&log), Some(Section::Deeper));

    // A new question replaces it: collapsed, whatever was open before.
    log.push(candidate(2, 40.0, "Design a queue?"));
    log.extend(shown(2, "How would you design a queue?", &["a", "b", "c"]));
    log.push(Action::SlowStarted { candidate: 2 });
    log.push(slow(2, deeper("Use a log.")));
    assert_eq!(open_section(&log), None);
}

#[test]
fn section_toggles_replay_identically_through_json_lines() {
    let mut log = sections_log(SlowEnd::Done);
    log.extend([toggle(Section::Deeper), toggle(Section::Followups)]);
    let jsonl: Vec<String> = log
        .iter()
        .map(|a| serde_json::to_string(a).unwrap())
        .collect();
    assert!(
        jsonl
            .iter()
            .any(|l| l.contains(r#""type":"toggle_section""#) && l.contains(r#""followups""#))
    );
    let parsed: Vec<Action> = jsonl
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(Session::replay(&parsed), Session::replay(&log));
}

// --- Session memory (#57) ----------------------------------------------------------------------

fn summary(candidate: u64, line: &str, promise: Option<&str>) -> Action {
    Action::Summary {
        candidate,
        line: line.into(),
        promise: promise.map(str::to_owned),
        at: 1_700_000_000 + candidate as i64,
    }
}

fn mention(candidate: u64, items: &[&str]) -> Action {
    Action::Slow {
        candidate,
        field: SlowField::Mention {
            items: items.iter().map(|i| (*i).to_owned()).collect(),
        },
    }
}

#[test]
fn every_summary_line_is_kept_and_prompts_get_the_last_eight() {
    let mut log = Vec::new();
    for id in 1..=10u64 {
        log.push(candidate(id, id as f64 * 60.0, "Q?"));
        log.extend(shown(id, &format!("Q{id}"), &["a", "b", "c"]));
    }
    // Lines arrive out of order (a fallback for an earlier question at the end): stored in
    // question order.
    for id in (1..=10u64).rev() {
        log.push(summary(id, &format!("{id:02}:00 Q{id} — covered it"), None));
    }
    let s = Session::replay(&log);
    assert_eq!(s.summary.len(), 10);
    assert_eq!(s.summary[0].candidate, 1);
    let prompt = s.summary_for_prompt();
    assert_eq!(prompt.len(), memory::PROMPT_LINES);
    assert_eq!(prompt[0], "03:00 Q3 — covered it");
    assert_eq!(prompt[7], "10:00 Q10 — covered it");

    // The same question summarised again replaces its line.
    let mut again = log.clone();
    again.push(summary(4, "04:00 Q4 — covered more", None));
    let s = Session::replay(&again);
    assert_eq!(s.summary.len(), 10);
    assert_eq!(s.summary[3].line, "04:00 Q4 — covered more");
}

#[test]
fn a_promise_shows_on_later_questions_until_the_candidate_covers_it() {
    let mut log = vec![candidate(1, 10.0, "Design a rate limiter.")];
    log.extend(shown(1, "Design a rate limiter.", &["a", "b", "c"]));
    log.push(seg(
        Speaker::You,
        15.0,
        30.0,
        "Token bucket per key. I'll come back to clock skew.",
        true,
    ));
    log.push(candidate(2, 40.0, "How do you shard it?"));
    log.extend(shown(2, "How do you shard it?", &["a", "b", "c"]));
    log.push(Action::SlowStarted { candidate: 2 });
    log.push(mention(2, &["Name the SLO."]));
    // Question 2's slow phase summarises question 1's answer, promise included.
    log.push(summary(
        1,
        "00:10 rate limiter — covered token bucket; promised to return to clock skew",
        Some("clock skew"),
    ));
    let s = Session::replay(&log);
    let item = "Come back to clock skew: you said you would";
    let live = s.live.as_ref().unwrap();
    assert_eq!(live.sections.mention, [item, "Name the SLO."]);
    assert_eq!(*live.sections.state(Section::Mention), SectionState::Done);
    assert!(!s.promises[0].mentioned);

    // Question 3 arrives while it is still open: it shows there too, before any slow phase.
    let mut log3 = log.clone();
    log3.push(candidate(3, 90.0, "What about hot keys?"));
    log3.extend(shown(3, "What about hot keys?", &["a", "b", "c"]));
    let s3 = Session::replay(&log3);
    assert_eq!(s3.live.as_ref().unwrap().sections.mention, [item]);
    // Its own Mention field later keeps the promise first.
    log3.push(mention(3, &["Mention replication."]));
    let s3 = Session::replay(&log3);
    assert_eq!(
        s3.live.as_ref().unwrap().sections.mention,
        [item, "Mention replication."]
    );

    // Talking about something else keeps it, and so do the interviewer's words; the candidate
    // covering it closes it.
    log.push(seg(Speaker::You, 50.0, 55.0, "We hash the key.", true));
    log.push(seg(
        Speaker::Interviewer,
        56.0,
        58.0,
        "What about clock skew?",
        true,
    ));
    assert!(!Session::replay(&log).promises[0].mentioned);
    log.push(seg(
        Speaker::You,
        60.0,
        70.0,
        "Back to clock skew: we bound it with NTP.",
        true,
    ));
    let s = Session::replay(&log);
    assert!(s.promises[0].mentioned);
    assert_eq!(s.live.as_ref().unwrap().sections.mention, ["Name the SLO."]);

    // A covered promise is not added to later questions.
    log.push(candidate(4, 90.0, "Next?"));
    log.extend(shown(4, "Next?", &["a", "b", "c"]));
    assert!(
        Session::replay(&log)
            .live
            .unwrap()
            .sections
            .mention
            .is_empty()
    );
}

#[test]
fn a_mention_row_holding_only_a_covered_promise_is_hidden_again() {
    let mut log = vec![candidate(1, 1.0, "Q1?")];
    log.extend(shown(1, "Q1?", &["a", "b", "c"]));
    log.push(candidate(2, 20.0, "Q2?"));
    log.extend(shown(2, "Q2?", &["a", "b", "c"]));
    log.push(Action::SlowStarted { candidate: 2 });
    log.push(mention(2, &[]));
    log.push(Action::SlowEnded {
        candidate: 2,
        end: SlowEnd::Done,
        payload: "{}".into(),
        at: 1,
    });
    let state = |log: &[Action]| {
        Session::replay(log)
            .live
            .unwrap()
            .sections
            .state(Section::Mention)
            .clone()
    };
    assert_eq!(state(&log), SectionState::Empty);
    log.push(summary(1, "00:01 Q1 — covered", Some("consistent hashing")));
    assert_eq!(
        state(&log),
        SectionState::Done,
        "a promise gives the hidden row something to show"
    );
    log.push(seg(
        Speaker::You,
        30.0,
        35.0,
        "Consistent hashing, then.",
        true,
    ));
    assert_eq!(state(&log), SectionState::Empty);
}

#[test]
fn an_open_mention_row_closes_when_its_only_promise_is_covered() {
    let mut log = vec![candidate(1, 1.0, "Q1?")];
    log.extend(shown(1, "Q1?", &["a", "b", "c"]));
    log.push(candidate(2, 20.0, "Q2?"));
    log.extend(shown(2, "Q2?", &["a", "b", "c"]));
    log.push(Action::SlowStarted { candidate: 2 });
    log.push(mention(2, &[]));
    log.push(Action::SlowEnded {
        candidate: 2,
        end: SlowEnd::Done,
        payload: "{}".into(),
        at: 1,
    });
    log.push(summary(1, "00:01 Q1 — covered", Some("consistent hashing")));
    log.push(toggle(Section::Mention));
    assert_eq!(open_section(&log), Some(Section::Mention));
    log.push(seg(
        Speaker::You,
        30.0,
        35.0,
        "Consistent hashing, then.",
        true,
    ));
    assert_eq!(
        open_section(&log),
        None,
        "the row left the screen, so it is no longer open"
    );
}

#[test]
fn summary_lines_and_promises_survive_the_action_log_and_json() {
    let mut log = vec![candidate(1, 1.0, "Q1?")];
    log.extend(shown(1, "Q1?", &["a", "b", "c"]));
    log.push(summary(1, "00:01 Q1 — covered", Some("clock skew")));
    let text: String = log
        .iter()
        .map(|a| serde_json::to_string(a).unwrap() + "\n")
        .collect();
    let back: Vec<Action> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let s = Session::replay(&back);
    assert_eq!(s, Session::replay(&log));
    assert_eq!(s.summary[0].line, "00:01 Q1 — covered");
    let again: Session = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(again, s);
    // A session with no memory serialises as it did before #57.
    let bare = Session::replay(&log[..log.len() - 1]);
    let json = serde_json::to_string(&bare).unwrap();
    assert!(
        !json.contains("\"summary\"") && !json.contains("\"promises\""),
        "{json}"
    );
}

// --- The ask key (#96) -------------------------------------------------------------------------

fn declined(candidate: u64) -> Action {
    Action::Ignored {
        candidate,
        reason: IgnoreReason::NotAQuestion,
        score: 0,
    }
}

/// Question 1 on screen, then candidate 2 (a statement), sent by Enter when `asked`.
fn second_candidate(asked: bool) -> Vec<Action> {
    let mut log = vec![candidate(1, 20.0, "q")];
    log.extend(shown(1, "Q", &["a", "b"]));
    log.push(candidate(2, 40.0, "Okay, that makes sense."));
    if asked {
        log.push(Action::Asked { candidate: 2 });
    }
    log
}

#[test]
fn a_key_press_the_model_declines_says_not_a_question_until_the_notice_expires() {
    for reason in [IgnoreReason::NotAQuestion, IgnoreReason::SmallTalk] {
        let mut log = second_candidate(true);
        let before = Session::replay(&log);
        assert_eq!(before.asked, [2], "waiting for its reply");
        assert_eq!(before.not_a_question, None);

        log.push(Action::Ignored {
            candidate: 2,
            reason,
            score: 0,
        });
        let said = Session::replay(&log);
        assert_eq!(said.not_a_question, Some(2), "{reason:?}");
        assert!(said.asked.is_empty());
        assert_eq!(said.live, before.live, "the screen keeps what it had");
        assert_eq!((said.pending, said.counts.ignored), (None, 1));

        // Another candidate's expiry is not this notice's.
        log.push(Action::NoticeExpired { candidate: 1 });
        assert_eq!(Session::replay(&log).not_a_question, Some(2));
        log.push(Action::NoticeExpired { candidate: 2 });
        assert_eq!(Session::replay(&log).not_a_question, None);
    }
}

#[test]
fn a_detector_candidate_the_model_declines_stays_silent() {
    let mut log = second_candidate(false);
    log.push(declined(2));
    let s = Session::replay(&log);
    assert_eq!(s.not_a_question, None);
    assert_eq!(s.counts.ignored, 1);
}

fn timed_out(candidate: u64) -> Action {
    Action::Failed {
        candidate,
        error: "timeout after 4 s".into(),
    }
}

#[test]
fn a_key_press_that_is_answered_or_fails_leaves_no_notice() {
    let mut answered = second_candidate(true);
    answered.extend(shown(2, "Q2", &["c"]));
    let mut failed = second_candidate(true);
    failed.push(timed_out(2));
    for log in [answered, failed] {
        let s = Session::replay(&log);
        assert!(s.asked.is_empty(), "its reply ended");
        assert_eq!(s.not_a_question, None);
    }
}

#[test]
fn a_key_press_whose_request_failed_is_remembered_until_the_next_press() {
    // Only a failure: the pipeline lets Enter send that turn again.
    let mut failed = second_candidate(true);
    failed.push(timed_out(2));
    assert_eq!(Session::replay(&failed).ask_failed, Some(2));
    let mut answered = second_candidate(true);
    answered.extend(shown(2, "Q2", &["c"]));
    assert_eq!(Session::replay(&answered).ask_failed, None);
    let mut refused = second_candidate(true);
    refused.push(declined(2));
    assert_eq!(Session::replay(&refused).ask_failed, None);
    // A detector candidate that fails is not the key's.
    let mut detected = second_candidate(false);
    detected.push(timed_out(2));
    assert_eq!(Session::replay(&detected).ask_failed, None);

    // The retry is a new press: it starts clean, and its own outcome stands.
    failed.push(candidate(3, 40.0, "Okay, that makes sense."));
    failed.push(Action::Asked { candidate: 3 });
    assert_eq!(Session::replay(&failed).ask_failed, None);
    failed.extend(shown(3, "Q2", &["c"]));
    let s = Session::replay(&failed);
    assert_eq!(s.ask_failed, None);
    assert_eq!(s.live.as_ref().map(|q| q.candidate), Some(3));
}

#[test]
fn the_next_key_press_replaces_the_answer_to_the_last_one() {
    let mut log = second_candidate(true);
    log.push(declined(2));
    log.push(candidate(3, 60.0, "And how would you test it?"));
    log.push(Action::Asked { candidate: 3 });
    let s = Session::replay(&log);
    assert_eq!(s.not_a_question, None);
    assert_eq!(s.asked, [3]);

    // Behind a pin both replies come: each declined press is answered, the latest standing.
    let mut pinned = second_candidate(true);
    pinned.insert(pinned.len() - 2, Action::Pin);
    pinned.push(candidate(3, 60.0, "Right."));
    pinned.push(Action::Asked { candidate: 3 });
    assert_eq!(Session::replay(&pinned).asked, [2, 3]);
    pinned.extend([declined(2), declined(3)]);
    assert_eq!(Session::replay(&pinned).not_a_question, Some(3));
}

#[test]
fn the_ask_key_survives_the_action_log_and_a_session_from_before_it_still_loads() {
    let mut log = second_candidate(true);
    log.push(declined(2));
    let text: String = log
        .iter()
        .map(|a| serde_json::to_string(a).unwrap() + "\n")
        .collect();
    assert!(text.contains(r#"{"type":"asked","candidate":2}"#), "{text}");
    let back: Vec<Action> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let s = Session::replay(&back);
    assert_eq!(s, Session::replay(&log));
    let again: Session = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(again, s);
    // A session no key was pressed in serialises as it did before #96, and loads the same.
    let bare = Session::replay(&second_candidate(false));
    let json = serde_json::to_string(&bare).unwrap();
    assert!(
        !json.contains("\"asked\"")
            && !json.contains("\"not_a_question\"")
            && !json.contains("\"ask_failed\""),
        "{json}"
    );
    assert_eq!(serde_json::from_str::<Session>(&json).unwrap(), bare);
}

// --- Earlier questions and scrolling (#94) ------------------------------------------------------

/// Three questions shown one after the other, the first with a slow phase the second cut short:
/// Deeper Answer had arrived, the other sections were still loading.
fn three_questions() -> Vec<Action> {
    let mut log = vec![candidate(1, 10.0, "Design a cache?")];
    log.extend(shown(
        1,
        "How would you design a cache?",
        &["LRU.", "TTLs."],
    ));
    log.push(Action::Focus {
        candidate: 1,
        text: "eviction, invalidation".into(),
    });
    log.push(Action::SlowStarted { candidate: 1 });
    log.push(slow(1, deeper("Evict with LRU.")));
    log.push(candidate(2, 60.0, "And how would you warm it?"));
    log.extend(shown(2, "How would you warm it?", &["From the replica."]));
    log.push(candidate(3, 120.0, "What breaks at ten times the load?"));
    log.extend(shown(
        3,
        "What breaks at ten times the load?",
        &["The hot keys.", "The eviction storm."],
    ));
    log
}

/// The question on screen, by its candidate id.
fn on_screen(s: &Session) -> u64 {
    s.shown().unwrap().candidate
}

fn kept(s: &Session) -> Vec<u64> {
    s.history.iter().map(|q| q.candidate).collect()
}

#[test]
fn every_question_shown_is_kept_in_order_as_it_stood_when_it_was_replaced() {
    let s = Session::replay(&three_questions());
    assert_eq!(s.live.as_ref().unwrap().candidate, 3);
    assert_eq!(kept(&s), [1, 2]);
    let first = &s.history[0];
    assert_eq!(first.text, "How would you design a cache?");
    assert_eq!(first.focus, "eviction, invalidation");
    assert_eq!(first.approach, ["LRU.", "TTLs."]);
    assert_eq!(first.sections.deeper[0].text, "Evict with LRU.");
    assert_eq!(s.browsing, None, "nothing steps back on its own");
    assert_eq!(on_screen(&s), 3);
    assert_eq!(s.place(), None);
}

#[test]
fn earlier_twice_shows_the_first_question_and_later_twice_returns_to_live() {
    let mut log = three_questions();
    log.push(Action::Earlier);
    let s = Session::replay(&log);
    assert_eq!((on_screen(&s), s.place()), (2, Some((2, 3))));

    log.push(Action::Earlier);
    let s = Session::replay(&log);
    assert_eq!((on_screen(&s), s.place()), (1, Some((1, 3))));
    let first = s.shown().unwrap();
    assert_eq!(first.focus, "eviction, invalidation", "its own focus");
    assert_eq!(first.approach, ["LRU.", "TTLs."], "its own Approach");
    assert_eq!(first.sections.deeper[0].text, "Evict with LRU.");
    assert_eq!(s.live.as_ref().unwrap().candidate, 3, "live is untouched");

    // The first question of the session: nothing is before it.
    log.push(Action::Earlier);
    assert_eq!(Session::replay(&log), s);

    log.push(Action::Later);
    assert_eq!(on_screen(&Session::replay(&log)), 2);
    log.push(Action::Later);
    let back = Session::replay(&log);
    assert_eq!((on_screen(&back), back.browsing), (3, None));
    assert_eq!(
        back,
        Session::replay(&three_questions()),
        "as if never left"
    );

    // The live question: nothing is after it.
    log.push(Action::Later);
    assert_eq!(Session::replay(&log), back);
}

#[test]
fn the_first_question_of_a_session_has_nothing_before_it() {
    let mut log = vec![Action::Earlier, Action::Later];
    assert_eq!(Session::replay(&log), Session::default());
    log.extend(shown(1, "How would you design a cache?", &["LRU."]));
    let one = Session::replay(&log);
    log.push(Action::Earlier);
    assert_eq!(Session::replay(&log), one);
}

#[test]
fn a_question_that_arrives_behind_an_earlier_one_is_answered_and_counted() {
    let mut log = three_questions();
    log.push(Action::Earlier);
    log.push(candidate(4, 180.0, "How would you test it?"));
    log.extend(shown(4, "How would you test it?", &["Replay traffic."]));
    let s = Session::replay(&log);
    assert_eq!(on_screen(&s), 2, "the screen keeps what it had");
    assert_eq!(s.place(), Some((2, 4)));
    assert_eq!(s.unseen, 1, "the status line says 1 new");
    let live = s.live.as_ref().unwrap();
    assert_eq!(live.candidate, 4);
    assert_eq!(live.approach, ["Replay traffic."], "answered behind it");
    assert_eq!(live.progress, Progress::Done);

    // On the way back the question it replaced is one more step; then live, and the count goes.
    log.push(Action::Later);
    let s = Session::replay(&log);
    assert_eq!((on_screen(&s), s.unseen), (3, 1));
    log.push(Action::Later);
    let s = Session::replay(&log);
    assert_eq!((on_screen(&s), s.browsing, s.unseen), (4, None, 0));
}

#[test]
fn a_clarification_behind_an_earlier_question_goes_to_the_live_one_uncounted() {
    let mut log = three_questions();
    log.push(Action::Earlier);
    log.push(Action::Clarification {
        candidate: 4,
        text: "Read-heavy, you said?".into(),
    });
    let s = Session::replay(&log);
    assert_eq!(on_screen(&s), 2);
    assert!(s.shown().unwrap().clarifications.is_empty());
    assert_eq!(s.live.as_ref().unwrap().clarifications.len(), 1);
    assert_eq!(s.unseen, 0, "it refines a question, it is not a new one");
}

#[test]
fn a_split_question_is_one_entry_of_the_history_and_one_new_question() {
    // #63: the second half replaces the first where it is, so the history never holds a half.
    let mut log = split_question(false);
    log.extend(shown(3, "How would you test it?", &["Replay traffic."]));
    assert_eq!(
        kept(&Session::replay(&log)),
        [2],
        "the whole question, once"
    );

    // Both halves arriving behind an earlier question count as the one question they are.
    let mut log = three_questions();
    log.push(Action::Earlier);
    log.push(candidate(4, 180.0, "And if the cache"));
    log.extend(shown(4, "What if the cache?", &["Half an answer."]));
    log.push(candidate(5, 180.0, "And if the cache is cold, what then?"));
    log.push(Action::Continues {
        candidate: 5,
        continued: 4,
    });
    log.extend(shown(5, "What if the cache is cold?", &["Warm it."]));
    let s = Session::replay(&log);
    assert_eq!(s.unseen, 1);
    assert_eq!(kept(&s), [1, 2, 3]);
    assert_eq!(s.live.as_ref().unwrap().candidate, 5);
}

#[test]
fn browsing_with_a_pinned_question_leaves_the_pin_and_its_queue_as_they_were() {
    let mut log = vec![candidate(1, 10.0, "Design a cache?")];
    log.extend(shown(1, "How would you design a cache?", &["LRU."]));
    log.extend(shown(2, "How would you warm it?", &["From the replica."]));
    log.push(Action::Pin);
    log.push(Action::Earlier);
    log.extend(shown(
        3,
        "What breaks at ten times the load?",
        &["Hot keys."],
    ));
    log.extend(shown(4, "How would you test it?", &["Replay traffic."]));
    let s = Session::replay(&log);
    assert_eq!(on_screen(&s), 1);
    assert_eq!(s.unseen, 2, "2 new on the status line");
    assert!(s.pinned);
    assert_eq!(s.live.as_ref().unwrap().candidate, 2, "the pin holds");
    assert_eq!(s.place(), Some((1, 2)), "queued questions were not shown");

    // Back on live it is the pinned question with its queue, as if the user had never left.
    log.push(Action::Later);
    let back = Session::replay(&log);
    assert_eq!((on_screen(&back), back.browsing, back.unseen), (2, None, 0));
    assert!(back.pinned);
    let queued: Vec<u64> = back.queued.iter().map(|q| q.candidate).collect();
    assert_eq!(queued, [3, 4], "the badge says 2 new");
    let never_left: Vec<Action> = log
        .iter()
        .filter(|a| !matches!(a, Action::Earlier | Action::Later))
        .cloned()
        .collect();
    assert_eq!(back, Session::replay(&never_left));

    // Unpin shows the newest, as today; the question that was pinned is now an earlier one.
    log.push(Action::Unpin);
    let end = Session::replay(&log);
    assert_eq!(end.live.as_ref().unwrap().candidate, 4);
    assert!(end.queued.is_empty());
    assert_eq!(kept(&end), [1, 2]);
}

#[test]
fn an_earlier_question_keeps_what_had_arrived_and_nothing_fills_it_afterwards() {
    let mut log = three_questions();
    let before = Session::replay(&log);
    let first = &before.history[0];
    // The second question cancelled the first's slow phase with five sections still loading:
    // the one that had arrived is done, and the rest are not waited for.
    assert_eq!(first.sections.states.deeper, SectionState::Done);
    assert!(first.sections.visible(Section::Deeper));
    for section in [Section::Followups, Section::Tradeoffs, Section::Example] {
        assert_eq!(*first.sections.state(section), SectionState::Empty);
        assert!(!first.sections.visible(section), "{section:?} has no row");
    }
    // A late field for it, a late end, a regenerate, a swap: none of them reach the history.
    log.push(slow(
        1,
        SlowField::Followups {
            items: vec!["Too late.".into()],
        },
    ));
    log.push(slow_ended(1, SlowEnd::Done));
    log.push(Action::Regenerate { candidate: 1 });
    log.push(Action::SwapStory { candidate: 1 });
    assert_eq!(Session::replay(&log), before);
}

#[test]
fn a_question_replaced_before_its_retry_started_is_kept_as_the_reply_it_showed() {
    let mut log = vec![candidate(1, 10.0, "Design a cache?")];
    log.extend(shown(1, "How would you design a cache?", &["LRU."]));
    log.push(Action::Regenerate { candidate: 1 });
    log.extend(shown(2, "How would you warm it?", &["From the replica."]));
    let s = Session::replay(&log);
    let first = &s.history[0];
    assert!(!first.regenerating, "nothing dims an earlier question");
    assert_eq!(first.progress, Progress::Done);
    assert_eq!(first.approach, ["LRU."]);
}

#[test]
fn the_section_keys_open_the_rows_of_the_question_on_screen() {
    let mut log = three_questions();
    log.push(Action::SlowStarted { candidate: 3 });
    log.push(slow(3, deeper("Shard the hot keys.")));
    log.push(Action::Earlier);
    log.push(Action::Earlier);
    log.push(toggle(Section::Deeper));
    let s = Session::replay(&log);
    assert_eq!(s.shown().unwrap().open, Some(Section::Deeper));
    assert_eq!(s.live.as_ref().unwrap().open, None, "not the live one's");

    // A row the earlier question does not have stays shut, as on the live one.
    log.push(toggle(Section::Followups));
    assert_eq!(
        Session::replay(&log).shown().unwrap().open,
        Some(Section::Deeper)
    );

    // What was open on it is still open when the user comes back to it.
    log.extend([
        Action::Later,
        Action::Later,
        Action::Earlier,
        Action::Earlier,
    ]);
    let s = Session::replay(&log);
    assert_eq!(on_screen(&s), 1);
    assert_eq!(s.shown().unwrap().open, Some(Section::Deeper));
}

/// A question with Deeper Answer filled and open, scrolled `offset` rows.
fn scrolled(offset: u16) -> Vec<Action> {
    let mut log = sections_log(SlowEnd::Done);
    log.push(toggle(Section::Deeper));
    log.push(Action::Scroll { offset });
    log
}

#[test]
fn the_scroll_offset_is_kept_for_the_open_section_only() {
    assert_eq!(Session::replay(&scrolled(4)).scroll, 4);
    let mut up = scrolled(4);
    up.push(Action::Scroll { offset: 0 });
    assert_eq!(Session::replay(&up).scroll, 0);

    // Nothing open, nothing to scroll.
    let mut closed = sections_log(SlowEnd::Done);
    closed.push(Action::Scroll { offset: 4 });
    assert_eq!(Session::replay(&closed).scroll, 0);
    assert_eq!(
        Session::replay(&[Action::Scroll { offset: 4 }]),
        Session::default()
    );
}

#[test]
fn the_scroll_offset_resets_when_the_section_closes_or_the_question_changes() {
    let after = |more: Vec<Action>| {
        let mut log = scrolled(4);
        log.extend(more);
        Session::replay(&log).scroll
    };
    assert_eq!(after(vec![toggle(Section::Deeper)]), 0, "closed");
    assert_eq!(after(vec![toggle(Section::Followups)]), 0, "another opened");
    assert_eq!(
        after(shown(2, "How would you warm it?", &["From the replica."])),
        0,
        "a new question took the screen"
    );
    assert_eq!(
        after(vec![slow(1, SlowField::Deeper { items: vec![] })]),
        0,
        "the open row settled empty and closed"
    );
    assert_eq!(
        after(vec![Action::Started {
            label: "next".into(),
            at: 1_700_000_900,
        }]),
        0
    );
    // What does not change the section on screen leaves it where the user put it.
    assert_eq!(after(vec![slow(1, deeper("Evict with LFU."))]), 4);
    assert_eq!(after(vec![Action::Regenerate { candidate: 1 }]), 4);
    assert_eq!(after(vec![Action::Pin]), 4);
    assert_eq!(
        after(vec![seg(Speaker::You, 30.0, 32.0, "So, LRU.", true)]),
        4
    );
}

#[test]
fn the_scroll_offset_resets_on_a_step_and_holds_behind_an_earlier_question() {
    let mut log = scrolled(4);
    log.extend(shown(2, "How would you warm it?", &["From the replica."]));
    log.push(Action::Earlier);
    // The first question is on screen again with Deeper Answer open, from its top.
    let s = Session::replay(&log);
    assert_eq!(s.shown().unwrap().open, Some(Section::Deeper));
    assert_eq!(s.scroll, 0);

    log.push(Action::Scroll { offset: 2 });
    // A question arriving behind it does not move what the user is reading.
    log.extend(shown(3, "How would you test it?", &["Replay traffic."]));
    let s = Session::replay(&log);
    assert_eq!((on_screen(&s), s.scroll, s.unseen), (1, 2, 1));

    log.push(Action::Later);
    assert_eq!(
        Session::replay(&log).scroll,
        0,
        "another question on screen"
    );
}

#[test]
fn unpin_resets_the_scroll_when_it_changes_the_question_on_screen() {
    let mut log = scrolled(4);
    log.push(Action::Pin);
    log.extend(shown(2, "How would you warm it?", &["From the replica."]));
    assert_eq!(Session::replay(&log).scroll, 4, "queued behind the pin");
    log.push(Action::Unpin);
    let s = Session::replay(&log);
    assert_eq!((on_screen(&s), s.scroll), (2, 0));
    assert_eq!(s.history[0].open, Some(Section::Deeper), "kept as it stood");
}

#[test]
fn browsing_and_scrolling_survive_the_action_log_and_older_sessions_still_load() {
    let mut log = three_questions();
    log.extend([Action::Earlier, Action::Earlier, toggle(Section::Deeper)]);
    log.push(Action::Scroll { offset: 3 });
    log.extend(shown(4, "How would you test it?", &["Replay traffic."]));
    let text: String = log
        .iter()
        .map(|a| serde_json::to_string(a).unwrap() + "\n")
        .collect();
    for line in [r#"{"type":"earlier"}"#, r#"{"type":"scroll","offset":3}"#] {
        assert!(text.contains(line), "{line} in\n{text}");
    }
    assert_eq!(
        serde_json::to_string(&Action::Later).unwrap(),
        r#"{"type":"later"}"#
    );
    let back: Vec<Action> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let s = Session::replay(&back);
    assert_eq!(s, Session::replay(&log));
    assert_eq!((s.browsing, s.unseen, s.scroll), (Some(0), 1, 3));
    let again: Session = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(again, s);

    // A session with one question serialises as it did before #94, and loads the same.
    let mut one = vec![candidate(1, 10.0, "Design a cache?")];
    one.extend(shown(1, "How would you design a cache?", &["LRU."]));
    let bare = Session::replay(&one);
    let json = serde_json::to_string(&bare).unwrap();
    for key in ["\"history\"", "\"browsing\"", "\"unseen\"", "\"scroll\""] {
        assert!(!json.contains(key), "{key} in {json}");
    }
    assert_eq!(serde_json::from_str::<Session>(&json).unwrap(), bare);
}

// --- #109: `t` speaks the user's last turn in the interview's language ------------------------

fn speak_on() -> Vec<Action> {
    vec![
        Action::Started {
            label: "live".into(),
            at: 1_790_200_000,
        },
        Action::SpeakEnabled { enabled: true },
    ]
}

#[test]
fn a_speak_press_goes_translating_then_speaking_with_its_seconds_then_clears() {
    let mut log = speak_on();
    log.push(Action::SpeakStarted { id: 1 });
    let s = Session::replay(&log);
    assert!(s.speak);
    let sp = s.speech.clone().unwrap();
    assert_eq!((sp.id, sp.stage, sp.secs), (1, SpeakStage::Translating, 0));
    assert!(sp.text.is_empty(), "no translation yet");

    log.push(Action::SpeakElapsed { id: 1, secs: 1 });
    log.push(Action::SpeakTranslated {
        id: 1,
        text: "Empezaría con un token bucket.".into(),
    });
    log.push(Action::SpeakElapsed { id: 1, secs: 9 });
    let sp = Session::replay(&log).speech.unwrap();
    assert_eq!(sp.stage, SpeakStage::Speaking);
    assert_eq!(sp.text, "Empezaría con un token bucket.");
    assert_eq!(sp.secs, 9);

    // A stale press's actions change nothing.
    log.push(Action::SpeakElapsed { id: 7, secs: 99 });
    log.push(Action::SpeakDone { id: 7 });
    assert_eq!(Session::replay(&log).speech.unwrap().secs, 9);

    log.push(Action::SpeakDone { id: 1 });
    let s = Session::replay(&log);
    assert_eq!(s.speech, None, "the tag and the line go");
    assert_eq!(s.speak_failed, None);
}

#[test]
fn a_press_that_speaks_nothing_shows_why_until_its_notice_expires() {
    let mut log = speak_on();
    // Nothing said yet: the pipeline answers the press with a failure and no start.
    log.push(Action::SpeakFailed {
        id: 1,
        reason: "nothing to speak".into(),
    });
    let s = Session::replay(&log);
    assert_eq!(s.speech, None);
    assert_eq!(
        s.speak_failed,
        Some(SpeakNotice {
            id: 1,
            reason: "nothing to speak".into()
        })
    );
    // The next press clears it at once.
    log.push(Action::SpeakStarted { id: 2 });
    assert_eq!(Session::replay(&log).speak_failed, None);
    // A press that fails midway ends and says so.
    log.push(Action::SpeakFailed {
        id: 2,
        reason: "translate failed: timed out after 4 s".into(),
    });
    let s = Session::replay(&log);
    assert_eq!(s.speech, None);
    assert_eq!(s.speak_failed.as_ref().unwrap().id, 2);
    // An older notice's expiry leaves the newer one; its own takes it down.
    log.push(Action::SpeakNoticeExpired { id: 1 });
    assert!(Session::replay(&log).speak_failed.is_some());
    log.push(Action::SpeakNoticeExpired { id: 2 });
    assert_eq!(Session::replay(&log).speak_failed, None);
}

#[test]
fn speak_state_survives_the_action_log_and_older_sessions_still_load() {
    let mut log = speak_on();
    log.extend([
        Action::SpeakStarted { id: 3 },
        Action::SpeakTranslated {
            id: 3,
            text: "Hola.".into(),
        },
        Action::SpeakElapsed { id: 3, secs: 4 },
    ]);
    let text: String = log
        .iter()
        .map(|a| serde_json::to_string(a).unwrap() + "\n")
        .collect();
    for line in [
        r#"{"type":"speak_enabled","enabled":true}"#,
        r#"{"type":"speak_started","id":3}"#,
        r#"{"type":"speak_translated","id":3,"text":"Hola."}"#,
        r#"{"type":"speak_elapsed","id":3,"secs":4}"#,
    ] {
        assert!(text.contains(line), "{line} in\n{text}");
    }
    let back: Vec<Action> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let s = Session::replay(&back);
    assert_eq!(s, Session::replay(&log));
    let again: Session = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(again, s);

    // A session that never spoke serialises as it did before #109.
    let bare = Session::replay(&[Action::Started {
        label: "x".into(),
        at: 1,
    }]);
    let json = serde_json::to_string(&bare).unwrap();
    for key in ["\"speak\"", "\"speech\"", "\"speak_failed\""] {
        assert!(!json.contains(key), "{key} in {json}");
    }
    assert_eq!(serde_json::from_str::<Session>(&json).unwrap(), bare);
}

// --- #114: pass-through by default, translate mode by key ------------------------------------

fn voice(mode: VoiceMode) -> Action {
    Action::Voice {
        mode,
        target: "de".into(),
        own: Some("en".into()),
    }
}

#[test]
fn the_voice_mode_is_session_state_and_a_change_writes_one_marker_line() {
    let mut log = speak_on();
    log.push(voice(VoiceMode::Passthrough));
    log.push(seg(Speaker::You, 5.0, 8.0, "Hello there.", true));
    let s = Session::replay(&log);
    assert_eq!(
        s.voice,
        Some(Voice {
            mode: VoiceMode::Passthrough,
            target: "de".into(),
            own: Some("en".into()),
        })
    );
    assert_eq!(s.transcript.len(), 1, "the start-up mode is no line");

    // `l`: translate mode on, said once in the transcript after what was said so far.
    log.push(voice(VoiceMode::Translate));
    let s = Session::replay(&log);
    assert_eq!(s.voice.as_ref().unwrap().mode, VoiceMode::Translate);
    let marker = s.transcript.last().unwrap();
    assert!(marker.note);
    assert_eq!(marker.text, "— translate mode on: → de —");
    assert_eq!(
        (marker.start, marker.end),
        (8.0, 8.0),
        "after the last line"
    );
    assert_eq!(voice_marker(VoiceMode::Translate, "de"), marker.text);
    // The same mode again writes nothing; back writes the other marker.
    log.push(voice(VoiceMode::Translate));
    assert_eq!(Session::replay(&log).transcript.len(), 2);
    log.push(voice(VoiceMode::Passthrough));
    let s = Session::replay(&log);
    assert_eq!(s.transcript.len(), 3);
    assert_eq!(
        s.transcript[2].text,
        "— translate mode off: your own voice —"
    );
    assert_eq!(VoiceMode::Passthrough.toggled(), VoiceMode::Translate);
    assert_eq!(VoiceMode::Translate.toggled(), VoiceMode::Passthrough);
    // #124: with a push-to-talk button, translate goes on to push-to-talk, then own voice.
    assert_eq!(VoiceMode::Translate.next(true), VoiceMode::Ptt);
    assert_eq!(VoiceMode::Ptt.next(true), VoiceMode::Passthrough);
    assert_eq!(VoiceMode::Ptt.next(false), VoiceMode::Passthrough);
    assert!(VoiceMode::Ptt.translates() && !VoiceMode::Passthrough.translates());
    assert_eq!(
        voice_marker(VoiceMode::Ptt, "zh"),
        "— push-to-talk on: → zh —"
    );
    assert_eq!(VoiceMode::default(), VoiceMode::Passthrough);
    assert_eq!(
        (
            VoiceMode::Passthrough.as_str(),
            VoiceMode::Translate.as_str()
        ),
        ("passthrough", "translate")
    );
}

#[test]
fn a_press_that_played_marks_the_lines_it_spoke_with_the_translation_under_the_last() {
    let mut log = speak_on();
    log.push(voice(VoiceMode::Translate));
    log.extend([
        seg(Speaker::Interviewer, 0.0, 4.0, "Go on.", true),
        seg(
            Speaker::You,
            6.0,
            9.0,
            "I'd start with a token bucket.",
            true,
        ),
        seg(
            Speaker::You,
            9.4,
            12.0,
            "And keep the counters in Redis.",
            true,
        ),
        Action::SpeakStarted { id: 1 },
        Action::SpeakTurn {
            id: 1,
            from: 6.0,
            to: 12.0,
        },
    ]);
    let s = Session::replay(&log);
    assert_eq!(s.speech.as_ref().unwrap().span, Some((6.0, 12.0)));
    assert!(
        s.transcript.iter().all(|l| l.spoken_as.is_none()),
        "nothing is marked until it has played"
    );
    log.push(Action::SpeakTranslated {
        id: 1,
        text: "Ich würde mit einem Token-Bucket anfangen. Und die Zähler in Redis halten.".into(),
    });
    log.push(Action::SpeakDone { id: 1 });
    let s = Session::replay(&log);
    assert_eq!(s.speech, None);
    let marks: Vec<(Option<&str>, Option<&str>)> = s
        .transcript
        .iter()
        .map(|l| (l.spoken_as.as_deref(), l.translation.as_deref()))
        .collect();
    assert_eq!(
        marks,
        [
            (None, None),
            (Some("de"), None),
            (
                Some("de"),
                Some("Ich würde mit einem Token-Bucket anfangen. Und die Zähler in Redis halten.")
            ),
        ]
    );

    // A later line is not marked; a press that failed marks nothing; a press with no turn
    // (an older pipeline's) marks nothing either.
    log.push(seg(Speaker::You, 14.0, 15.0, "Also, sharding.", true));
    log.push(Action::SpeakStarted { id: 2 });
    log.push(Action::SpeakTurn {
        id: 2,
        from: 14.0,
        to: 15.0,
    });
    log.push(Action::SpeakFailed {
        id: 2,
        reason: "tts failed: server down".into(),
    });
    log.push(Action::SpeakStarted { id: 3 });
    log.push(Action::SpeakTranslated {
        id: 3,
        text: "Auch Sharding.".into(),
    });
    log.push(Action::SpeakDone { id: 3 });
    let s = Session::replay(&log);
    assert_eq!(s.transcript[3].spoken_as, None);
    assert_eq!(s.transcript[3].translation, None);
    assert_eq!(
        s.transcript[1].spoken_as.as_deref(),
        Some("de"),
        "the first turn keeps its mark"
    );
}

#[test]
fn voice_state_survives_the_action_log_and_older_sessions_still_load() {
    let mut log = speak_on();
    log.extend([
        voice(VoiceMode::Passthrough),
        voice(VoiceMode::Translate),
        seg(Speaker::You, 6.0, 9.0, "Hello.", true),
        Action::SpeakStarted { id: 1 },
        Action::SpeakTurn {
            id: 1,
            from: 6.0,
            to: 9.0,
        },
        Action::SpeakTranslated {
            id: 1,
            text: "Hallo.".into(),
        },
        Action::SpeakDone { id: 1 },
    ]);
    let text: String = log
        .iter()
        .map(|a| serde_json::to_string(a).unwrap() + "\n")
        .collect();
    for line in [
        r#"{"type":"voice","mode":"passthrough","target":"de","own":"en"}"#,
        r#"{"type":"voice","mode":"translate","target":"de","own":"en"}"#,
        r#"{"type":"speak_turn","id":1,"from":6.0,"to":9.0}"#,
    ] {
        assert!(text.contains(line), "{line} in\n{text}");
    }
    let back: Vec<Action> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let s = Session::replay(&back);
    assert_eq!(s, Session::replay(&log));
    let json = serde_json::to_string(&s).unwrap();
    assert!(
        json.contains(r#""spoken_as":"de""#) && json.contains(r#""note":true"#),
        "{json}"
    );
    let again: Session = serde_json::from_str(&json).unwrap();
    assert_eq!(again, s);

    // A session from before #114 has none of it: the keys are left out, and a line without
    // them reads as plain speech.
    let bare = Session::replay(&[
        Action::Started {
            label: "x".into(),
            at: 1,
        },
        seg(Speaker::You, 1.0, 2.0, "Hi.", true),
    ]);
    let json = serde_json::to_string(&bare).unwrap();
    for key in [
        "\"voice\"",
        "\"spoken_as\"",
        "\"translation\"",
        "\"note\"",
        "\"span\"",
    ] {
        assert!(!json.contains(key), "{key} in {json}");
    }
    assert_eq!(serde_json::from_str::<Session>(&json).unwrap(), bare);
    let old: Line =
        serde_json::from_str(r#"{"speaker":"you","start":1.0,"end":2.0,"text":"Hi."}"#).unwrap();
    assert_eq!(old, Line::spoken(Speaker::You, 1.0, 2.0, "Hi.".into()));
}

// --- #115: the language picker ----------------------------------------------------------------

fn lang(code: &str) -> SpeakLanguage {
    SpeakLanguage {
        code: code.into(),
        voice: format!("voice-{code}"),
    }
}

fn three_languages() -> Vec<Action> {
    let mut log = speak_on();
    log.push(voice(VoiceMode::Translate));
    log.push(Action::SpeakLanguages {
        languages: vec![lang("de"), lang("es"), lang("ru")],
    });
    log
}

fn pick(op: PickerOp) -> Action {
    Action::Picker { op }
}

#[test]
fn the_picker_opens_on_the_current_language_moves_within_the_list_and_closes() {
    let mut log = three_languages();
    let s = Session::replay(&log);
    assert_eq!(s.languages, [lang("de"), lang("es"), lang("ru")]);
    assert_eq!(s.picker, None, "closed until the key");
    assert_eq!(s.speak_target(), Some("de"));
    // Up and Down with the picker closed do nothing (they scroll a section then).
    log.push(pick(PickerOp::Down));
    assert_eq!(Session::replay(&log).picker, None);

    log.push(pick(PickerOp::Open));
    let s = Session::replay(&log);
    assert_eq!(
        s.picker,
        Some(Picker { cursor: 0 }),
        "on the current language"
    );
    assert_eq!(s.picked(), Some(&lang("de")));
    // A second Open leaves the cursor where it is.
    log.push(pick(PickerOp::Down));
    log.push(pick(PickerOp::Open));
    assert_eq!(Session::replay(&log).picker, Some(Picker { cursor: 1 }));
    // The cursor stops at both ends.
    log.extend([
        pick(PickerOp::Down),
        pick(PickerOp::Down),
        pick(PickerOp::Down),
    ]);
    let s = Session::replay(&log);
    assert_eq!(s.picker, Some(Picker { cursor: 2 }));
    assert_eq!(s.picked(), Some(&lang("ru")));
    log.extend([
        pick(PickerOp::Up),
        pick(PickerOp::Up),
        pick(PickerOp::Up),
        pick(PickerOp::Up),
    ]);
    assert_eq!(Session::replay(&log).picker, Some(Picker { cursor: 0 }));
    // Esc closes it with nothing chosen.
    log.push(pick(PickerOp::Close));
    let s = Session::replay(&log);
    assert_eq!(s.picker, None);
    assert_eq!(s.speak_target(), Some("de"));
    assert!(s.transcript.is_empty(), "nothing chosen, no marker");

    // The cursor starts on the current language, wherever it is in the list.
    log.push(Action::SpeakTarget {
        target: "ru".into(),
    });
    log.push(pick(PickerOp::Open));
    assert_eq!(Session::replay(&log).picker, Some(Picker { cursor: 2 }));

    // With no languages listed, or with `[speak]` off, the key does nothing.
    let mut off = speak_on();
    off.push(voice(VoiceMode::Translate));
    off.push(pick(PickerOp::Open));
    assert_eq!(Session::replay(&off).picker, None);
    let mut disabled = three_languages();
    disabled.push(Action::SpeakEnabled { enabled: false });
    disabled.push(pick(PickerOp::Open));
    assert_eq!(Session::replay(&disabled).picker, None);
    // A new list closes the picker and starts it afresh.
    let mut relisted = three_languages();
    relisted.push(pick(PickerOp::Open));
    relisted.push(Action::SpeakLanguages {
        languages: vec![lang("es")],
    });
    let s = Session::replay(&relisted);
    assert_eq!((s.picker, s.languages.len()), (None, 1));
}

#[test]
fn choosing_a_language_changes_the_target_closes_the_picker_and_writes_one_marker() {
    let mut log = three_languages();
    log.push(seg(Speaker::You, 5.0, 8.0, "Hello there.", true));
    log.push(pick(PickerOp::Open));
    log.push(pick(PickerOp::Down));
    log.push(Action::SpeakTarget {
        target: "es".into(),
    });
    let s = Session::replay(&log);
    assert_eq!(s.picker, None);
    assert_eq!(s.speak_target(), Some("es"));
    assert_eq!(
        s.voice,
        Some(Voice {
            mode: VoiceMode::Translate,
            target: "es".into(),
            own: Some("en".into()),
        })
    );
    let marker = s.transcript.last().unwrap();
    assert!(marker.note);
    assert_eq!(marker.text, "— language: → es —");
    assert_eq!(marker.text, language_marker("es"));
    assert_eq!(
        (marker.start, marker.end),
        (8.0, 8.0),
        "after the last line"
    );
    // The same language again writes nothing.
    log.push(Action::SpeakTarget {
        target: "es".into(),
    });
    assert_eq!(Session::replay(&log).transcript.len(), 2);
    // A code the list does not have is ignored (the picker still closes).
    log.push(pick(PickerOp::Open));
    log.push(Action::SpeakTarget {
        target: "xx".into(),
    });
    let s = Session::replay(&log);
    assert_eq!((s.picker, s.speak_target()), (None, Some("es")));
    assert_eq!(s.transcript.len(), 2);
    // `l` afterwards announces the new target with the mode.
    log.push(Action::Voice {
        mode: VoiceMode::Passthrough,
        target: "es".into(),
        own: Some("en".into()),
    });
    let s = Session::replay(&log);
    assert_eq!(
        s.transcript.last().unwrap().text,
        "— translate mode off: your own voice —"
    );
    assert_eq!(s.speak_target(), Some("es"));
}

/// The issue's criterion: a press in flight finishes in the language it started with, and the
/// lines it spoke are marked with that one; the next press speaks the chosen one.
#[test]
fn a_press_in_flight_keeps_its_language_and_the_next_one_takes_the_chosen_one() {
    let mut log = three_languages();
    log.extend([
        seg(
            Speaker::You,
            6.0,
            9.0,
            "I'd start with a token bucket.",
            true,
        ),
        Action::SpeakStarted { id: 1 },
        Action::SpeakTurn {
            id: 1,
            from: 6.0,
            to: 9.0,
        },
    ]);
    let s = Session::replay(&log);
    assert_eq!(s.speech.as_ref().unwrap().target.as_deref(), Some("de"));
    // The user picks Spanish while the German press is in flight.
    log.push(pick(PickerOp::Open));
    log.push(pick(PickerOp::Down));
    log.push(Action::SpeakTarget {
        target: "es".into(),
    });
    log.push(Action::SpeakTranslated {
        id: 1,
        text: "Ich würde mit einem Token-Bucket anfangen.".into(),
    });
    log.push(Action::SpeakDone { id: 1 });
    let s = Session::replay(&log);
    assert_eq!(s.speak_target(), Some("es"));
    let said = &s.transcript[0];
    assert_eq!(said.spoken_as.as_deref(), Some("de"), "spoken in German");
    assert_eq!(
        said.translation.as_deref(),
        Some("Ich würde mit einem Token-Bucket anfangen.")
    );
    assert_eq!(s.transcript[1].text, "— language: → es —");
    // The next press is Spanish.
    log.extend([
        seg(Speaker::You, 12.0, 14.0, "Then sharding.", true),
        Action::SpeakStarted { id: 2 },
        Action::SpeakTurn {
            id: 2,
            from: 12.0,
            to: 14.0,
        },
        Action::SpeakTranslated {
            id: 2,
            text: "Luego el sharding.".into(),
        },
        Action::SpeakDone { id: 2 },
    ]);
    let s = Session::replay(&log);
    assert_eq!(s.transcript[2].spoken_as.as_deref(), Some("es"));
    assert_eq!(s.transcript[0].spoken_as.as_deref(), Some("de"), "kept");

    // A log from before #115 (no `target` on the press) marks with the voice's target.
    let old: Speech =
        serde_json::from_str(r#"{"id":1,"stage":"speaking","text":"Hallo.","span":[6.0,9.0]}"#)
            .unwrap();
    assert_eq!(old.target, None);
    let json = serde_json::to_string(&s).unwrap();
    assert!(
        json.contains(r#""languages":[{"code":"de","voice":"voice-de"}"#),
        "{json}"
    );
    assert!(!json.contains("\"picker\""), "closed: left out");
    assert_eq!(serde_json::from_str::<Session>(&json).unwrap(), s);
    let bare = Session::replay(&speak_on());
    let json = serde_json::to_string(&bare).unwrap();
    assert!(
        !json.contains("\"languages\"") && !json.contains("\"picker\""),
        "{json}"
    );
    for line in [
        r#"{"type":"speak_languages","languages":[{"code":"de","voice":"voice-de"}]}"#,
        r#"{"type":"picker","op":"open"}"#,
        r#"{"type":"speak_target","target":"es"}"#,
    ] {
        let a: Action = serde_json::from_str(line).unwrap();
        assert_eq!(serde_json::to_string(&a).unwrap(), line);
    }
}
