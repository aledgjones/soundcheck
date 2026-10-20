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
