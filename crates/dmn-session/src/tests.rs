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
    assert_eq!(s.last_interviewer.as_deref(), Some("Okay."));

    reduce(&mut s, &seg(Speaker::You, 83.0, 84.0, "", true));
    assert_eq!(s.transcript.len(), 1, "an empty final adds nothing");
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
