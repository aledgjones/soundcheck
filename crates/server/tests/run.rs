use soundcheck::run::model::*;
use soundcheck::run::{MAX_ATTEMPTS, Plan, PlannedSpec, Row, Run};
use soundcheck::suite::config::Platform::{self, Android, Ios};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const DRAW: &str = "annotations/001-draw";
const BACK: &str = "annotations/002-back";

fn plan(parallel: bool) -> Plan {
    Plan {
        specs: vec![
            PlannedSpec {
                id: DRAW.into(),
                platforms: vec![Android, Ios],
                outcomes: vec!["Undo removes the stroke".into(), "Redo restores it".into()],
            },
            PlannedSpec {
                id: BACK.into(),
                platforms: vec![Android],
                outcomes: vec!["The collection shows".into()],
            },
        ],
        parallel,
    }
}

fn journal(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "soundcheck-test-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("journal.jsonl")
}

fn open(name: &str) -> (Run, PathBuf) {
    let path = journal(name);
    (Run::open(plan(false), &path, 0).unwrap(), path)
}

fn judged(answers: &[Answer]) -> Posted {
    Posted {
        judgments: answers
            .iter()
            .enumerate()
            .map(|(i, &answer)| Judgment {
                outcome: i,
                answer,
                observed: format!("saw outcome {i}"),
                evidence: vec![],
            })
            .collect(),
        side_finds: vec![],
        proposed_severity: None,
        notes: None,
    }
}

fn result(run: &Run, spec: &str, platform: Platform) -> Option<Final> {
    match run.row(spec, platform).unwrap().row {
        Row::Filed { result, .. } => Some(result),
        _ => None,
    }
}

#[test]
fn a_pass_files_at_once() {
    let (mut run, _) = open("pass");
    let token = run.dispatch(1, DRAW, Android, "prompt").unwrap();
    let claim = run
        .post(2, &token, judged(&[Answer::Held, Answer::Held]))
        .unwrap();
    assert_eq!(claim, Claim::Pass);
    assert_eq!(result(&run, DRAW, Android), Some(Final::Pass));
}

#[test]
fn a_fail_waits_for_review_and_an_upheld_one_becomes_an_open_problem() {
    let (mut run, _) = open("fail");
    let token = run.dispatch(1, DRAW, Android, "p").unwrap();
    assert_eq!(
        run.post(2, &token, judged(&[Answer::Failed, Answer::Held]))
            .unwrap(),
        Claim::Fail
    );
    assert!(matches!(
        run.row(DRAW, Android).unwrap().row,
        Row::NeedsReview { since: 2, .. }
    ));

    let err = run
        .review(3, DRAW, Android, Call::Uphold { severity: None }, "real")
        .unwrap_err();
    assert!(err.to_string().contains("needs a severity"));
    assert!(
        run.review(
            3,
            DRAW,
            Android,
            Call::Uphold {
                severity: Some(Severity::Bug)
            },
            ""
        )
        .is_err()
    );

    run.review(
        3,
        DRAW,
        Android,
        Call::Uphold {
            severity: Some(Severity::Bug),
        },
        "screenshot shows the stroke",
    )
    .unwrap();
    assert_eq!(result(&run, DRAW, Android), Some(Final::Fail));
    let problem = &run.state().problems[0];
    assert_eq!(problem.severity, Severity::Bug);
    assert_eq!(problem.summary, "Undo removes the stroke: saw outcome 0");
    assert!(!problem.verified);
}

#[test]
fn review_never_blocks_the_next_dispatch_but_blocks_finishing() {
    let (mut run, _) = open("finish");
    let t = run.dispatch(1, DRAW, Android, "p").unwrap();
    run.post(2, &t, judged(&[Answer::Failed, Answer::Held]))
        .unwrap();
    let next = run.dispatch(3, BACK, Android, "p").unwrap();
    run.post(4, &next, judged(&[Answer::Held])).unwrap();

    let err = run.finish(5).unwrap_err().to_string();
    assert!(
        err.contains("annotations/001-draw on android needs review"),
        "{err}"
    );

    run.review(
        6,
        DRAW,
        Android,
        Call::Uphold {
            severity: Some(Severity::Bug),
        },
        "real",
    )
    .unwrap();
    let counts = run.finish(7).unwrap();
    assert_eq!((counts.pass, counts.fail, counts.not_run), (1, 1, 1));
    assert!(
        run.dispatch(8, DRAW, Ios, "p")
            .unwrap_err()
            .to_string()
            .contains("finished")
    );
}

#[test]
fn a_verification_that_passes_files_a_not_reproduced_finding() {
    let (mut run, _) = open("verify");
    let t = run.dispatch(1, DRAW, Android, "p").unwrap();
    run.post(2, &t, judged(&[Answer::Failed, Answer::Held]))
        .unwrap();
    run.review(
        3,
        DRAW,
        Android,
        Call::Verify {
            claim: "the stroke survived undo".into(),
        },
        "one-off?",
    )
    .unwrap();
    assert_eq!(
        run.row(DRAW, Android).unwrap().row,
        Row::Queued {
            verify: Some("the stroke survived undo".into())
        }
    );

    let t2 = run.dispatch(4, DRAW, Android, "p").unwrap();
    assert_eq!(t2, "annotations/001-draw#2@android");
    run.post(5, &t2, judged(&[Answer::Held, Answer::Held]))
        .unwrap();
    assert_eq!(result(&run, DRAW, Android), Some(Final::Pass));
    let problem = &run.state().problems[0];
    assert_eq!(problem.source, ProblemSource::NotReproduced);
    assert_eq!(problem.severity, Severity::Finding);
    assert_eq!(
        problem.summary,
        "Not reproduced (1 of 2): the stroke survived undo"
    );
}

#[test]
fn a_reproduced_verification_is_upheld_as_verified() {
    let (mut run, _) = open("verified");
    let t = run.dispatch(1, DRAW, Android, "p").unwrap();
    run.post(2, &t, judged(&[Answer::Failed, Answer::Held]))
        .unwrap();
    run.review(3, DRAW, Android, Call::Verify { claim: "c".into() }, "r")
        .unwrap();
    let t2 = run.dispatch(4, DRAW, Android, "p").unwrap();
    run.post(5, &t2, judged(&[Answer::Failed, Answer::Held]))
        .unwrap();
    run.review(
        6,
        DRAW,
        Android,
        Call::Uphold {
            severity: Some(Severity::Bug),
        },
        "reproduced",
    )
    .unwrap();
    assert!(run.state().problems[0].verified);
}

#[test]
fn an_unsure_must_be_judged_verified_or_marked_unclear() {
    let (mut run, _) = open("unsure");
    let t = run.dispatch(1, DRAW, Android, "p").unwrap();
    assert_eq!(
        run.post(2, &t, judged(&[Answer::Unsure, Answer::Held]))
            .unwrap(),
        Claim::Unsure
    );
    assert!(
        run.review(
            3,
            DRAW,
            Android,
            Call::Uphold {
                severity: Some(Severity::Bug)
            },
            "r"
        )
        .is_err()
    );
    assert!(
        run.review(
            3,
            DRAW,
            Android,
            Call::Judge {
                result: Final::Fail,
                severity: None
            },
            "r"
        )
        .is_err()
    );
    run.review(
        3,
        DRAW,
        Android,
        Call::Judge {
            result: Final::Pass,
            severity: None,
        },
        "screenshot is fine",
    )
    .unwrap();
    assert_eq!(result(&run, DRAW, Android), Some(Final::Pass));
    assert!(run.state().problems.is_empty());

    let t = run.dispatch(4, DRAW, Ios, "p").unwrap();
    run.post(5, &t, judged(&[Answer::Unsure, Answer::Held]))
        .unwrap();
    run.review(
        6,
        DRAW,
        Ios,
        Call::SpecUnclear,
        "can't tell what level means",
    )
    .unwrap();
    assert_eq!(result(&run, DRAW, Ios), Some(Final::Blocked));
    assert!(run.state().unclear_specs.contains(DRAW));
}

#[test]
fn judge_and_unclear_are_only_for_an_unsure() {
    let (mut run, _) = open("judge-only-unsure");
    let t = run.dispatch(1, DRAW, Android, "p").unwrap();
    run.post(2, &t, judged(&[Answer::Failed, Answer::Held]))
        .unwrap();
    assert!(
        run.review(3, DRAW, Android, Call::SpecUnclear, "r")
            .is_err()
    );
    assert!(
        run.review(
            3,
            DRAW,
            Android,
            Call::Judge {
                result: Final::Pass,
                severity: None
            },
            "r"
        )
        .is_err()
    );
}

#[test]
fn a_blocked_upheld_without_severity_is_a_harness_gap_note() {
    let (mut run, _) = open("blocked");
    let t = run.dispatch(1, DRAW, Android, "p").unwrap();
    assert_eq!(
        run.post(2, &t, judged(&[Answer::CouldntCheck, Answer::Held]))
            .unwrap(),
        Claim::Blocked
    );
    run.review(
        3,
        DRAW,
        Android,
        Call::Uphold { severity: None },
        "no label on the undo button",
    )
    .unwrap();
    assert_eq!(result(&run, DRAW, Android), Some(Final::Blocked));
    assert!(run.state().problems.is_empty());
    assert_eq!(
        run.state().notes[0].text,
        "harness gap: no label on the undo button"
    );
}

#[test]
fn three_attempts_at_most() {
    let (mut run, _) = open("attempts");
    for attempt in 1..=MAX_ATTEMPTS {
        let t = run
            .dispatch(attempt as u64 * 10, DRAW, Android, "p")
            .unwrap();
        run.post(
            attempt as u64 * 10 + 1,
            &t,
            judged(&[Answer::Failed, Answer::Held]),
        )
        .unwrap();
        if attempt < MAX_ATTEMPTS {
            run.review(
                attempt as u64 * 10 + 2,
                DRAW,
                Android,
                Call::SendBack,
                "misread",
            )
            .unwrap();
        }
    }
    let err = run
        .review(40, DRAW, Android, Call::SendBack, "again")
        .unwrap_err()
        .to_string();
    assert!(err.contains("all 3 attempts"), "{err}");
    run.review(
        41,
        DRAW,
        Android,
        Call::Uphold {
            severity: Some(Severity::Finding),
        },
        "seen 3 times",
    )
    .unwrap();
    assert_eq!(run.row(DRAW, Android).unwrap().sightings, 3);
}

#[test]
fn only_silence_between_calls_counts_as_a_stall() {
    let (mut run, _) = open("stall");
    let t = run.dispatch(0, DRAW, Android, "p").unwrap();
    run.call_started(10_000, &t);
    assert!(
        run.stalled(500_000, 120_000).is_empty(),
        "a long seed or wait isn't a stall"
    );
    run.call_finished(500_000, &t);
    assert!(run.stalled(600_000, 120_000).is_empty());
    assert_eq!(run.stalled(620_000, 120_000), vec![t.clone()]);

    run.stall(620_000, &t, "no tool call for 2 min").unwrap();
    assert_eq!(
        run.row(DRAW, Android).unwrap().row,
        Row::Queued { verify: None }
    );
    assert!(
        run.post(620_001, &t, judged(&[Answer::Held, Answer::Held]))
            .is_err(),
        "a stalled token can't post"
    );
}

#[test]
fn stalling_on_the_last_attempt_files_blocked() {
    let (mut run, _) = open("stall-last");
    for attempt in 1..=MAX_ATTEMPTS {
        let t = run.dispatch(attempt as u64, DRAW, Android, "p").unwrap();
        run.stall(attempt as u64, &t, "silent").unwrap();
    }
    assert_eq!(result(&run, DRAW, Android), Some(Final::Blocked));
}

#[test]
fn one_spec_runs_at_a_time_unless_parallel() {
    let (mut run, _) = open("serial");
    run.dispatch(1, DRAW, Android, "p").unwrap();
    let err = run.dispatch(2, DRAW, Ios, "p").unwrap_err().to_string();
    assert!(err.contains("still running on android"), "{err}");

    let mut parallel = Run::open(plan(true), &journal("parallel"), 0).unwrap();
    parallel.dispatch(1, DRAW, Android, "p").unwrap();
    parallel.dispatch(2, DRAW, Ios, "p").unwrap();
    assert!(
        parallel.dispatch(3, BACK, Android, "p").is_err(),
        "still one per platform"
    );
}

#[test]
fn a_result_must_judge_every_outcome_with_an_observation() {
    let (mut run, _) = open("validate");
    let t = run.dispatch(1, DRAW, Android, "p").unwrap();
    let err = run
        .post(2, &t, judged(&[Answer::Held]))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("outcome 1 (\"Redo restores it\") isn't judged"),
        "{err}"
    );

    let mut twice = judged(&[Answer::Held, Answer::Held]);
    twice.judgments[1].outcome = 0;
    assert!(
        run.post(2, &t, twice)
            .unwrap_err()
            .to_string()
            .contains("judged twice")
    );

    let mut blank = judged(&[Answer::Held, Answer::Held]);
    blank.judgments[0].observed = " ".into();
    assert!(
        run.post(2, &t, blank)
            .unwrap_err()
            .to_string()
            .contains("what you observed")
    );
    assert!(matches!(
        run.row(DRAW, Android).unwrap().row,
        Row::Running { .. }
    ));
}

#[test]
fn a_side_find_is_reviewed_on_its_own() {
    let (mut run, _) = open("side-find");
    let t = run.dispatch(1, DRAW, Android, "p").unwrap();
    let mut posted = judged(&[Answer::Held, Answer::Held]);
    posted.side_finds.push(SideFind {
        summary: "Eraser width resets".into(),
        observed: "width went back to 4 after reopening".into(),
        evidence: vec![],
    });
    run.post(2, &t, posted).unwrap();
    assert_eq!(
        result(&run, DRAW, Android),
        Some(Final::Pass),
        "the row keeps its own result"
    );
    assert!(
        run.finish(3)
            .unwrap_err()
            .to_string()
            .contains("side find 1")
    );

    run.review_find(
        4,
        1,
        Some(Severity::Bug),
        "reproducible from the screenshot",
    )
    .unwrap();
    let problem = &run.state().problems[0];
    assert_eq!(problem.source, ProblemSource::SideFind);
    assert_eq!(
        problem.summary,
        "Eraser width resets: width went back to 4 after reopening"
    );
}

#[test]
fn a_reset_failure_is_blocked_without_using_an_attempt() {
    let (mut run, _) = open("reset");
    run.reset_failed(1, DRAW, Android, "starts-on: Annotate not found")
        .unwrap();
    assert_eq!(result(&run, DRAW, Android), Some(Final::Blocked));
    assert_eq!(run.row(DRAW, Android).unwrap().attempts, 0);
    assert_eq!(
        run.state().notes[0].text,
        "reset failed: starts-on: Annotate not found"
    );
}

#[test]
fn pause_stops_dispatch_until_resumed() {
    let (mut run, _) = open("pause");
    run.pause(1, "Aled: fix the undo bug first").unwrap();
    assert!(
        run.dispatch(2, DRAW, Android, "p")
            .unwrap_err()
            .to_string()
            .contains("paused")
    );
    run.resume(3).unwrap();
    run.dispatch(4, DRAW, Android, "p").unwrap();
}

#[test]
fn a_reopened_journal_carries_on_where_it_stopped() {
    let (mut run, path) = open("replay");
    let t = run.dispatch(1, DRAW, Android, "p").unwrap();
    run.post(2, &t, judged(&[Answer::Failed, Answer::Held]))
        .unwrap();
    let running = run.dispatch(3, BACK, Android, "p").unwrap();
    drop(run);

    let mut run = Run::open(plan(false), &path, 1_000).unwrap();
    assert!(matches!(
        run.row(DRAW, Android).unwrap().row,
        Row::NeedsReview { .. }
    ));
    assert!(
        run.stalled(1_000 + 119_000, 120_000).is_empty(),
        "the clock restarts on reopen"
    );
    run.review(
        4,
        DRAW,
        Android,
        Call::Uphold {
            severity: Some(Severity::Bug),
        },
        "real",
    )
    .unwrap();
    run.post(5, &running, judged(&[Answer::Held])).unwrap();
    assert_eq!(result(&run, BACK, Android), Some(Final::Pass));

    let lines = std::fs::read_to_string(&path).unwrap();
    assert_eq!(lines.lines().count(), 5);
    assert!(
        lines
            .lines()
            .nth(3)
            .unwrap()
            .contains(r#""event":"reviewed""#)
    );
    assert!(lines.lines().nth(3).unwrap().contains(r#""call":"uphold""#));
}

use soundcheck::run::verdict::VerdictStatus;

fn fail_and_uphold(run: &mut Run, at: u64, spec: &str, platform: Platform) {
    let t = run.dispatch(at, spec, platform, "p").unwrap();
    let answers: Vec<Answer> = if spec == DRAW {
        vec![Answer::Failed, Answer::Held]
    } else {
        vec![Answer::Failed]
    };
    run.post(at + 1, &t, judged(&answers)).unwrap();
    run.review(
        at + 2,
        spec,
        platform,
        Call::Uphold {
            severity: Some(Severity::Bug),
        },
        "real",
    )
    .unwrap();
}

fn pass(run: &mut Run, at: u64, spec: &str, platform: Platform) {
    let t = run.dispatch(at, spec, platform, "p").unwrap();
    let answers: Vec<Answer> = if spec == DRAW {
        vec![Answer::Held, Answer::Held]
    } else {
        vec![Answer::Held]
    };
    run.post(at + 1, &t, judged(&answers)).unwrap();
}

#[test]
fn every_spec_passing_is_ready() {
    let (mut run, _) = open("ready");
    pass(&mut run, 1, DRAW, Android);
    pass(&mut run, 10, DRAW, Ios);
    let v = run.verdict();
    assert_eq!(v.status, VerdictStatus::NotReady);
    assert_eq!(v.reasons, ["1 spec not run"]);
    pass(&mut run, 20, BACK, Android);
    assert_eq!(run.verdict().status, VerdictStatus::Ready);
}

#[test]
fn an_open_fail_blocks_until_it_is_deferred_or_dismissed() {
    let (mut run, _) = open("blocked-verdict");
    fail_and_uphold(&mut run, 1, DRAW, Android);
    pass(&mut run, 10, DRAW, Ios);
    pass(&mut run, 20, BACK, Android);
    let v = run.verdict();
    assert_eq!(v.status, VerdictStatus::Blocked);
    assert_eq!(v.reasons, ["1 open FAIL", "1 problem not triaged"]);

    run.finish(30).unwrap();
    let err = run
        .triage(31, 1, Decision::Defer { ticket: " ".into() }, "known")
        .unwrap_err();
    assert!(err.to_string().contains("need a ticket"));
    run.triage(
        31,
        1,
        Decision::Defer {
            ticket: "SA-700".into(),
        },
        "rare, ships next release",
    )
    .unwrap();
    assert_eq!(
        run.verdict().status,
        VerdictStatus::Ready,
        "triage works after the run finished"
    );
}

#[test]
fn fixed_needs_a_later_pass_of_its_row() {
    let (mut run, _) = open("fixed");
    fail_and_uphold(&mut run, 1, DRAW, Android);
    let err = run
        .triage(
            5,
            1,
            Decision::Fix {
                ticket: "SA-701".into(),
            },
            "fix landed",
        )
        .unwrap_err();
    assert!(err.to_string().contains("passes on android again"), "{err}");

    // A side find from an attempt that was sent back, whose row then passed on attempt 2.
    let t = run.dispatch(10, BACK, Android, "p").unwrap();
    let mut posted = judged(&[Answer::Failed]);
    posted.side_finds.push(SideFind {
        summary: "Menu flickers".into(),
        observed: "twice".into(),
        evidence: vec![],
    });
    run.post(11, &t, posted).unwrap();
    run.review(12, BACK, Android, Call::SendBack, "wrong tap")
        .unwrap();
    run.review_find(13, 2, Some(Severity::Finding), "seen in the shot")
        .unwrap();
    pass(&mut run, 14, BACK, Android);
    run.triage(
        20,
        2,
        Decision::Fix {
            ticket: "SA-702".into(),
        },
        "flicker fixed",
    )
    .unwrap();
    assert!(matches!(
        run.state().problem(2).unwrap().status,
        Status::Fixed { .. }
    ));
}

#[test]
fn a_duplicate_follows_its_original() {
    let (mut run, _) = open("duplicate");
    fail_and_uphold(&mut run, 1, DRAW, Android);
    fail_and_uphold(&mut run, 10, DRAW, Ios);
    pass(&mut run, 20, BACK, Android);
    assert!(
        run.triage(30, 2, Decision::Duplicate { of: 2 }, "same")
            .is_err()
    );
    run.triage(30, 2, Decision::Duplicate { of: 1 }, "same undo bug on iOS")
        .unwrap();
    assert_eq!(
        run.verdict().reasons,
        ["2 open FAILs", "1 problem not triaged"]
    );
    assert!(
        run.triage(31, 1, Decision::Duplicate { of: 2 }, "loop")
            .is_err()
    );

    run.triage(32, 1, Decision::Dismiss, "it's the intended behaviour")
        .unwrap();
    assert_eq!(
        run.verdict().status,
        VerdictStatus::Ready,
        "the duplicate follows the dismissal"
    );
}

#[test]
fn a_harness_gap_blocked_stays_not_ready() {
    let (mut run, _) = open("gap-verdict");
    let t = run.dispatch(1, BACK, Android, "p").unwrap();
    run.post(2, &t, judged(&[Answer::CouldntCheck])).unwrap();
    run.review(
        3,
        BACK,
        Android,
        Call::Uphold { severity: None },
        "no label",
    )
    .unwrap();
    pass(&mut run, 10, DRAW, Android);
    pass(&mut run, 20, DRAW, Ios);
    let v = run.verdict();
    assert_eq!(v.status, VerdictStatus::NotReady);
    assert_eq!(v.reasons, ["1 spec BLOCKED"]);
}

#[test]
fn a_not_reproduced_finding_must_be_triaged() {
    let (mut run, _) = open("not-repro-verdict");
    let t = run.dispatch(1, BACK, Android, "p").unwrap();
    run.post(2, &t, judged(&[Answer::Failed])).unwrap();
    run.review(
        3,
        BACK,
        Android,
        Call::Verify {
            claim: "blank screen".into(),
        },
        "once",
    )
    .unwrap();
    pass(&mut run, 4, BACK, Android);
    pass(&mut run, 10, DRAW, Android);
    pass(&mut run, 20, DRAW, Ios);
    assert_eq!(run.verdict().reasons, ["1 problem not triaged"]);
    run.triage(
        30,
        1,
        Decision::Dismiss,
        "not reproduced on a healthy device",
    )
    .unwrap();
    assert_eq!(run.verdict().status, VerdictStatus::Ready);
}

#[test]
fn triage_survives_a_reopen() {
    let (mut run, path) = open("triage-replay");
    fail_and_uphold(&mut run, 1, DRAW, Android);
    run.triage(
        5,
        1,
        Decision::Defer {
            ticket: "SA-703".into(),
        },
        "later",
    )
    .unwrap();
    drop(run);
    let run = Run::open(plan(false), &path, 0).unwrap();
    assert!(
        matches!(&run.state().problem(1).unwrap().status, Status::Deferred { ticket, .. } if ticket == "SA-703")
    );
}
