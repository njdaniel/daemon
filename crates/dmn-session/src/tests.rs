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
