use soundcheck::run::event::RowRef;
use soundcheck::run::model::*;
use soundcheck::run::{MAX_ATTEMPTS, Plan, PlannedSpec, Row, Run};
use soundcheck::suite::config::Platform::{self, Android, Ios};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const DRAW: &str = "annotations/001-draw";
const BACK: &str = "annotations/002-back";

fn bug() -> Call {
    Call::Uphold {
        title: Some("Undo leaves the stroke on screen".into()),
        cause: None,
    }
}

fn brief(_token: &str, _verify: Option<&str>) -> String {
    "brief".into()
}

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
        notes: None,
        probed: None,
    }
}

fn result(run: &Run, spec: &str, platform: Platform) -> Option<Final> {
    match run.row(spec, platform).unwrap().row {
        Row::Filed { result, .. } => Some(result),
        _ => None,
    }
}

#[test]
fn a_tester_gets_its_brief_only_while_its_spec_runs() {
    let (mut run, _) = open("brief");
    let token = run
        .dispatch(1, DRAW, Android, |t, _| format!("brief for {t}"))
        .unwrap();
    assert_eq!(run.brief(&token).unwrap(), format!("brief for {token}"));
    run.post(2, &token, judged(&[Answer::Held, Answer::Held]))
        .unwrap();
    assert!(
        run.brief(&token)
            .unwrap_err()
            .to_string()
            .contains("isn't a running spec")
    );
    assert!(run.brief("annotations/001-draw#9@android").is_err());
}

#[test]
fn a_pass_files_at_once() {
    let (mut run, _) = open("pass");
    let token = run.dispatch(1, DRAW, Android, brief).unwrap();
    let claim = run
        .post(2, &token, judged(&[Answer::Held, Answer::Held]))
        .unwrap();
    assert_eq!(claim, Claim::Pass);
    assert_eq!(result(&run, DRAW, Android), Some(Final::Pass));
}

#[test]
fn a_fail_waits_for_review_and_an_upheld_one_becomes_an_open_problem() {
    let (mut run, _) = open("fail");
    let token = run.dispatch(1, DRAW, Android, brief).unwrap();
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
        .review(
            3,
            DRAW,
            Android,
            Call::Uphold {
                title: None,
                cause: None,
            },
            "real",
        )
        .unwrap_err();
    assert!(err.to_string().contains("give it a title"));
    assert!(run.review(3, DRAW, Android, bug(), "").is_err());

    run.review(3, DRAW, Android, bug(), "screenshot shows the stroke")
        .unwrap();
    assert_eq!(result(&run, DRAW, Android), Some(Final::Fail));
    let problem = &run.state().problems[0];
    assert_eq!(problem.kind, ProblemKind::Failure);
    assert_eq!(problem.title, "Undo leaves the stroke on screen");
    assert_eq!(problem.detail, "Undo removes the stroke: saw outcome 0");
    assert!(!problem.verified);
}

#[test]
fn review_never_blocks_the_next_dispatch_but_blocks_finishing() {
    let (mut run, _) = open("finish");
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
    run.post(2, &t, judged(&[Answer::Failed, Answer::Held]))
        .unwrap();
    let next = run.dispatch(3, BACK, Android, brief).unwrap();
    run.post(4, &next, judged(&[Answer::Held])).unwrap();

    let err = run.finish(5).unwrap_err().to_string();
    assert!(
        err.contains("annotations/001-draw on android needs review"),
        "{err}"
    );

    run.review(6, DRAW, Android, bug(), "real").unwrap();
    let counts = run.finish(7).unwrap();
    assert_eq!((counts.pass, counts.fail, counts.not_run), (1, 1, 1));
    assert!(
        run.dispatch(8, DRAW, Ios, brief)
            .unwrap_err()
            .to_string()
            .contains("finished")
    );
}

#[test]
fn a_verification_that_passes_files_a_not_reproduced_finding() {
    let (mut run, _) = open("verify");
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
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

    let t2 = run.dispatch(4, DRAW, Android, brief).unwrap();
    assert_eq!(t2, "annotations/001-draw#2@android");
    run.post(5, &t2, judged(&[Answer::Held, Answer::Held]))
        .unwrap();
    assert_eq!(result(&run, DRAW, Android), Some(Final::Pass));
    let problem = &run.state().problems[0];
    assert_eq!(problem.source, ProblemSource::NotReproduced);
    assert_eq!(problem.kind, ProblemKind::Finding);
    assert_eq!(
        problem.title,
        "Not reproduced (1 of 2): the stroke survived undo"
    );
}

#[test]
fn a_verified_unsure_that_passes_files_only_a_pass() {
    let (mut run, _) = open("verify-unsure");
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
    run.post(2, &t, judged(&[Answer::Held, Answer::Unsure]))
        .unwrap();
    run.review(
        3,
        DRAW,
        Android,
        Call::Verify {
            claim: "redo may not restore the stroke's colour".into(),
        },
        "can't tell from the tree",
    )
    .unwrap();
    let t2 = run.dispatch(4, DRAW, Android, brief).unwrap();
    run.post(5, &t2, judged(&[Answer::Held, Answer::Held]))
        .unwrap();
    assert_eq!(result(&run, DRAW, Android), Some(Final::Pass));
    assert!(
        run.state().problems.is_empty(),
        "an Unsure isn't a claim of a bug"
    );
}

#[test]
fn a_harness_call_files_any_claim_blocked_with_a_note() {
    let (mut run, _) = open("harness");
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
    run.post(2, &t, judged(&[Answer::Held, Answer::Unsure]))
        .unwrap();
    run.review(
        3,
        DRAW,
        Android,
        Call::Harness { rerun: None },
        "no software keyboard",
    )
    .unwrap();
    assert_eq!(result(&run, DRAW, Android), Some(Final::Blocked));
    assert!(run.state().problems.is_empty());
    assert!(
        run.state().notes[0]
            .text
            .contains("harness gap: no software keyboard")
    );
}

#[test]
fn a_harness_rerun_tells_the_next_attempt_and_files_nothing_when_it_passes() {
    let (mut run, _) = open("harness-rerun");
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
    run.post(2, &t, judged(&[Answer::CouldntCheck, Answer::Held]))
        .unwrap();
    let call = Call::Harness {
        rerun: Some("Tap the page with at:516,700.".into()),
    };
    run.review(3, DRAW, Android, call, "double-tap took no point")
        .unwrap();
    assert_eq!(
        run.row(DRAW, Android).unwrap().rerun_note.as_deref(),
        Some("Tap the page with at:516,700.")
    );
    let t2 = run.dispatch(4, DRAW, Android, brief).unwrap();
    assert_eq!(run.row(DRAW, Android).unwrap().rerun_note, None);
    run.post(5, &t2, judged(&[Answer::Held, Answer::Held]))
        .unwrap();
    assert_eq!(result(&run, DRAW, Android), Some(Final::Pass));
    assert!(run.state().problems.is_empty());
}

#[test]
fn a_reproduced_verification_is_upheld_as_verified() {
    let (mut run, _) = open("verified");
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
    run.post(2, &t, judged(&[Answer::Failed, Answer::Held]))
        .unwrap();
    run.review(3, DRAW, Android, Call::Verify { claim: "c".into() }, "r")
        .unwrap();
    let t2 = run.dispatch(4, DRAW, Android, brief).unwrap();
    run.post(5, &t2, judged(&[Answer::Failed, Answer::Held]))
        .unwrap();
    run.review(6, DRAW, Android, bug(), "reproduced").unwrap();
    assert!(run.state().problems[0].verified);
}

#[test]
fn an_unsure_must_be_judged_verified_or_marked_unclear() {
    let (mut run, _) = open("unsure");
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
    assert_eq!(
        run.post(2, &t, judged(&[Answer::Unsure, Answer::Held]))
            .unwrap(),
        Claim::Unsure
    );
    assert!(run.review(3, DRAW, Android, bug(), "r").is_err());
    assert!(
        run.review(
            3,
            DRAW,
            Android,
            Call::Judge {
                result: Final::Fail,
                title: None
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
            title: None,
        },
        "screenshot is fine",
    )
    .unwrap();
    assert_eq!(result(&run, DRAW, Android), Some(Final::Pass));
    assert!(run.state().problems.is_empty());

    let t = run.dispatch(4, DRAW, Ios, brief).unwrap();
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
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
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
                title: None
            },
            "r"
        )
        .is_err()
    );
}

#[test]
fn a_blocked_the_harness_caused_is_a_note_not_a_problem() {
    let (mut run, _) = open("blocked");
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
    assert_eq!(
        run.post(2, &t, judged(&[Answer::CouldntCheck, Answer::Held]))
            .unwrap(),
        Claim::Blocked
    );
    run.review(
        3,
        DRAW,
        Android,
        Call::Uphold {
            title: None,
            cause: Some(Cause::Harness),
        },
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
            .dispatch(attempt as u64 * 10, DRAW, Android, brief)
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
    run.review(41, DRAW, Android, bug(), "seen 3 times")
        .unwrap();
    assert_eq!(run.row(DRAW, Android).unwrap().sightings, 3);
}

#[test]
fn only_silence_between_calls_counts_as_a_stall() {
    let (mut run, _) = open("stall");
    let t = run.dispatch(0, DRAW, Android, brief).unwrap();
    run.call_started(10_000, &t);
    assert!(
        run.stalled(500_000, 120_000, 600_000).is_empty(),
        "a long seed or wait isn't a stall"
    );
    run.call_finished(500_000, &t);
    assert!(run.stalled(600_000, 120_000, 600_000).is_empty());
    assert_eq!(run.stalled(620_000, 120_000, 600_000), vec![t.clone()]);

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
fn a_tester_that_starts_slowly_gets_the_longer_start_limit() {
    let (mut run, _) = open("slow-start");
    let t = run.dispatch(0, DRAW, Android, brief).unwrap();
    assert!(
        run.stalled(300_000, 120_000, 600_000).is_empty(),
        "a slow first call isn't a stall"
    );
    assert!(!run.started(&t));
    run.call_started(300_000, &t);
    run.call_finished(301_000, &t);
    assert!(run.started(&t));
    assert!(run.stalled(420_000, 120_000, 600_000).is_empty());
    assert_eq!(run.stalled(421_000, 120_000, 600_000), vec![t.clone()]);

    run.post(422_000, &t, judged(&[Answer::Held, Answer::Held]))
        .unwrap();
    let silent = run.dispatch(500_000, BACK, Android, brief).unwrap();
    assert!(run.stalled(1_099_000, 120_000, 600_000).is_empty());
    assert_eq!(run.stalled(1_100_000, 120_000, 600_000), vec![silent]);
}

#[test]
fn stalling_on_the_last_attempt_files_blocked() {
    let (mut run, _) = open("stall-last");
    for attempt in 1..=MAX_ATTEMPTS {
        let t = run.dispatch(attempt as u64, DRAW, Android, brief).unwrap();
        run.stall(attempt as u64, &t, "silent").unwrap();
    }
    assert_eq!(result(&run, DRAW, Android), Some(Final::Blocked));
}

#[test]
fn one_spec_runs_at_a_time_unless_parallel() {
    let (mut run, _) = open("serial");
    run.dispatch(1, DRAW, Android, brief).unwrap();
    let err = run.dispatch(2, DRAW, Ios, brief).unwrap_err().to_string();
    assert!(err.contains("still running on android"), "{err}");

    let mut parallel = Run::open(plan(true), &journal("parallel"), 0).unwrap();
    parallel.dispatch(1, DRAW, Android, brief).unwrap();
    parallel.dispatch(2, DRAW, Ios, brief).unwrap();
    assert!(
        parallel.dispatch(3, BACK, Android, brief).is_err(),
        "still one per platform"
    );
}

#[test]
fn a_result_must_judge_every_outcome_with_an_observation() {
    let (mut run, _) = open("validate");
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
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
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
    let mut posted = judged(&[Answer::Held, Answer::Held]);
    posted.side_finds.push(SideFind {
        summary: "Eraser width resets".into(),
        observed: "width went back to 4 after reopening".into(),
        evidence: vec!["before.png".into(), "after.png".into()],
    });
    run.post(2, &t, posted).unwrap();
    assert_eq!(
        result(&run, DRAW, Android),
        Some(Final::Pass),
        "the row keeps its own result"
    );
    let line = soundcheck::serve::events::summary(run.entries_after(0).last().unwrap());
    assert!(
        line.ends_with("1 side find(s) need review: id 1: Eraser width resets"),
        "review_find needs the id: {line}"
    );
    assert!(
        run.finish(3)
            .unwrap_err()
            .to_string()
            .contains("side find 1")
    );

    let reopened = Some("Eraser width resets after reopening".to_string());
    let err = run
        .review_find(4, 1, reopened.clone(), vec!["end.png".into()], "shot")
        .unwrap_err();
    assert!(err.to_string().contains("before.png, after.png"), "{err}");
    assert!(
        run.review_find(4, 1, None, vec!["after.png".into()], "no")
            .is_err(),
        "a dismissed find keeps nothing"
    );
    run.review_find(
        4,
        1,
        reopened,
        vec!["after.png".into()],
        "the after shot shows width 4",
    )
    .unwrap();
    let problem = &run.state().problems[0];
    assert_eq!(problem.source, ProblemSource::SideFind);
    assert_eq!(problem.kind, ProblemKind::Finding);
    assert_eq!(
        problem.detail,
        "Eraser width resets: width went back to 4 after reopening"
    );
    assert_eq!(problem.evidence, ["after.png"], "only the kept shot");
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
        run.dispatch(2, DRAW, Android, brief)
            .unwrap_err()
            .to_string()
            .contains("paused")
    );
    run.resume(3).unwrap();
    run.dispatch(4, DRAW, Android, brief).unwrap();
}

#[test]
fn a_platform_added_on_resume_reopens_a_finished_run() {
    let android_only = || {
        let mut p = plan(false);
        for spec in &mut p.specs {
            spec.platforms.retain(|p| *p == Android);
        }
        p
    };
    let path = journal("add-platform");
    let mut run = Run::open(android_only(), &path, 0).unwrap();
    for (at, spec) in [(1, DRAW), (3, BACK)] {
        let t = run.dispatch(at, spec, Android, brief).unwrap();
        let held = vec![
            Answer::Held;
            run.plan()
                .specs
                .iter()
                .find(|s| s.id == spec)
                .unwrap()
                .outcomes
                .len()
        ];
        run.post(at + 1, &t, judged(&held)).unwrap();
    }
    run.finish(5).unwrap();
    drop(run);

    let mut run = Run::open(plan(false), &path, 0).unwrap();
    assert!(run.state().finished);
    assert!(
        run.add_platforms(6, vec![Platform::Web])
            .unwrap_err()
            .to_string()
            .contains("no spec in the run's scope runs on web")
    );
    run.add_platforms(6, vec![Ios]).unwrap();
    assert!(!run.state().finished, "its new rows haven't run");
    assert_eq!(result(&run, DRAW, Android), Some(Final::Pass));
    assert_eq!(run.row(DRAW, Ios).unwrap().row, Row::NotRun);
    let t = run.dispatch(7, DRAW, Ios, brief).unwrap();
    run.post(8, &t, judged(&[Answer::Held, Answer::Held]))
        .unwrap();
    drop(run);

    let mut run = Run::open(plan(false), &path, 0).unwrap();
    assert!(!run.state().finished, "the reopening replays");
    let counts = run.finish(9).unwrap();
    assert_eq!((counts.pass, counts.not_run), (3, 0));
}

#[test]
fn specs_added_keep_the_old_rows_and_reopen_a_finished_run() {
    let draw_only = || {
        let mut p = plan(false);
        p.specs.retain(|s| s.id == DRAW);
        p
    };
    let path = journal("add-specs");
    let mut run = Run::open(draw_only(), &path, 0).unwrap();
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
    run.post(2, &t, judged(&[Answer::Held, Answer::Held]))
        .unwrap();
    run.finish(3).unwrap();

    let back = plan(false)
        .specs
        .into_iter()
        .find(|s| s.id == BACK)
        .unwrap();
    assert!(
        run.add_specs(4, vec![plan(false).specs[0].clone()])
            .unwrap_err()
            .to_string()
            .contains("already in the run")
    );
    let added = run.add_specs(4, vec![back]).unwrap();
    assert_eq!(added, vec![(BACK.to_string(), Android)]);
    assert!(!run.state().finished, "its new rows haven't run");
    assert_eq!(result(&run, DRAW, Android), Some(Final::Pass));
    assert_eq!(run.row(BACK, Android).unwrap().row, Row::NotRun);
    drop(run);

    let lines = std::fs::read_to_string(&path).unwrap();
    assert!(lines.contains(r#""event":"specs-added","specs":["annotations/002-back"]"#));
    // On resume the plan comes from the widened scope, so it already has the rows.
    let mut run = Run::open(plan(false), &path, 0).unwrap();
    assert!(!run.state().finished, "the reopening replays");
    assert_eq!(result(&run, DRAW, Android), Some(Final::Pass));
    let t = run.dispatch(5, BACK, Android, brief).unwrap();
    run.post(6, &t, judged(&[Answer::Held])).unwrap();
    let counts = run.finish(7).unwrap();
    assert_eq!(counts.pass, 2);
}

#[test]
fn resuming_with_a_scope_adds_to_the_saved_one() {
    use soundcheck::run::saved::{merge_scope, resume_plan};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/good");
    let suite = soundcheck::suite::load(&root);
    let saved = vec![DRAW.to_string()];

    let (before, added, scope) =
        resume_plan(&suite, &[Android, Ios], &saved, &[BACK.into(), DRAW.into()]).unwrap();
    assert_eq!(
        before
            .specs
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        vec![DRAW],
        "the run opens as it was saved"
    );
    assert_eq!(
        added
            .iter()
            .map(|s| (s.id.as_str(), s.platforms.clone()))
            .collect::<Vec<_>>(),
        vec![(BACK, vec![Android])],
        "only specs new to the run, on the run's platforms"
    );
    assert_eq!(scope, vec![DRAW.to_string(), BACK.to_string()]);

    let (_, added, scope) = resume_plan(&suite, &[Android], &saved, &[]).unwrap();
    assert!(added.is_empty(), "no --scope changes nothing");
    assert_eq!(scope, saved);

    let refused = resume_plan(&suite, &[Android], &saved, &["annotations/404-nope".into()])
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("no feature or spec by that name"),
        "{refused}"
    );

    assert!(
        merge_scope(&[], &[BACK.into()]).is_empty(),
        "an empty scope already has every spec"
    );
}

#[test]
fn a_reopened_journal_carries_on_where_it_stopped() {
    let (mut run, path) = open("replay");
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
    run.post(2, &t, judged(&[Answer::Failed, Answer::Held]))
        .unwrap();
    let running = run.dispatch(3, BACK, Android, brief).unwrap();
    drop(run);

    let mut run = Run::open(plan(false), &path, 1_000).unwrap();
    assert!(matches!(
        run.row(DRAW, Android).unwrap().row,
        Row::NeedsReview { .. }
    ));
    assert!(
        run.stalled(1_000 + 119_000, 120_000, 600_000).is_empty(),
        "the clock restarts on reopen"
    );
    run.review(4, DRAW, Android, bug(), "real").unwrap();
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
    let t = run.dispatch(at, spec, platform, brief).unwrap();
    let answers: Vec<Answer> = if spec == DRAW {
        vec![Answer::Failed, Answer::Held]
    } else {
        vec![Answer::Failed]
    };
    run.post(at + 1, &t, judged(&answers)).unwrap();
    run.review(at + 2, spec, platform, bug(), "real").unwrap();
}

fn pass(run: &mut Run, at: u64, spec: &str, platform: Platform) {
    let t = run.dispatch(at, spec, platform, brief).unwrap();
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
    assert_eq!(v.reasons, ["1 not run yet"]);
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
    assert_eq!(v.status, VerdictStatus::Failing);
    assert_eq!(v.reasons, ["1 failure"]);

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
    let t = run.dispatch(10, BACK, Android, brief).unwrap();
    let mut posted = judged(&[Answer::Failed]);
    posted.side_finds.push(SideFind {
        summary: "Menu flickers".into(),
        observed: "twice".into(),
        evidence: vec![],
    });
    run.post(11, &t, posted).unwrap();
    run.review(12, BACK, Android, Call::SendBack, "wrong tap")
        .unwrap();
    run.review_find(13, 2, Some("Menu flickers".into()), vec![], "seen twice")
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
    assert_eq!(run.verdict().reasons[0], "1 failure");
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
    let t = run.dispatch(1, BACK, Android, brief).unwrap();
    run.post(2, &t, judged(&[Answer::CouldntCheck])).unwrap();
    run.review(
        3,
        BACK,
        Android,
        Call::Uphold {
            title: None,
            cause: Some(Cause::Harness),
        },
        "no label",
    )
    .unwrap();
    pass(&mut run, 10, DRAW, Android);
    pass(&mut run, 20, DRAW, Ios);
    let v = run.verdict();
    assert_eq!(v.status, VerdictStatus::NotReady);
    assert_eq!(v.reasons, ["1 blocked"]);
}

#[test]
fn a_not_reproduced_finding_must_be_triaged() {
    let (mut run, _) = open("not-repro-verdict");
    let t = run.dispatch(1, BACK, Android, brief).unwrap();
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
    assert_eq!(run.verdict().reasons, ["1 finding to decide"]);
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

fn build(commit: &str, fingerprint: &str) -> Build {
    Build {
        commit: commit.into(),
        fingerprint: Some(fingerprint.into()),
        version: Some("2.16.1".into()),
    }
}

#[test]
fn a_js_fix_reruns_open_problems_and_listed_rows_only() {
    let (mut run, _) = open("fix-js");
    run.install_build(0, Android, build("aaaaaaa1", "fp1"), vec![])
        .unwrap();
    fail_and_uphold(&mut run, 1, DRAW, Android);
    pass(&mut run, 10, BACK, Android);
    pass(&mut run, 20, DRAW, Ios);

    let reruns = run
        .install_build(30, Android, build("bbbbbbb2", "fp1"), vec![])
        .unwrap();
    assert_eq!(
        reruns,
        [Rerun {
            spec: DRAW.into(),
            reason: "open problem 1".into()
        }]
    );
    assert_eq!(
        result(&run, BACK, Android),
        Some(Final::Pass),
        "untouched rows keep their result"
    );
    assert_eq!(
        run.row(BACK, Android)
            .unwrap()
            .build
            .as_ref()
            .unwrap()
            .commit,
        "aaaaaaa1"
    );
    assert_eq!(
        result(&run, DRAW, Ios),
        Some(Final::Pass),
        "another platform isn't touched"
    );

    // The fix: the rerun passes on the new build, which is what lets triage call it Fixed.
    pass(&mut run, 40, DRAW, Android);
    assert_eq!(
        run.row(DRAW, Android)
            .unwrap()
            .build
            .as_ref()
            .unwrap()
            .commit,
        "bbbbbbb2"
    );
    run.triage(
        50,
        1,
        Decision::Fix {
            ticket: "SA-704".into(),
        },
        "undo fixed",
    )
    .unwrap();
    assert!(
        run.state()
            .notes
            .iter()
            .any(|n| n.text == "rerun on 2.16.1 (bbbbbbb): open problem 1")
    );
}

#[test]
fn a_listed_rerun_needs_a_reason() {
    let (mut run, _) = open("fix-listed");
    run.install_build(0, Android, build("a1", "fp1"), vec![])
        .unwrap();
    pass(&mut run, 1, BACK, Android);
    let listed = |reason: &str| {
        vec![Rerun {
            spec: BACK.into(),
            reason: reason.into(),
        }]
    };
    assert!(
        run.install_build(2, Android, build("b2", "fp1"), listed(" "))
            .is_err()
    );
    let reruns = run
        .install_build(
            2,
            Android,
            build("b2", "fp1"),
            listed("the fix touches the back stack"),
        )
        .unwrap();
    assert_eq!(reruns[0].reason, "the fix touches the back stack");
    assert_eq!(
        run.row(BACK, Android).unwrap().row,
        Row::Queued { verify: None }
    );
}

#[test]
fn a_native_change_reruns_the_whole_platform() {
    let (mut run, _) = open("fix-native");
    run.install_build(0, Android, build("a1", "fp1"), vec![])
        .unwrap();
    pass(&mut run, 1, DRAW, Android);
    pass(&mut run, 10, BACK, Android);
    let reruns = run
        .install_build(20, Android, build("b2", "fp2"), vec![])
        .unwrap();
    assert_eq!(reruns.len(), 2);
    assert!(reruns.iter().all(|r| r.reason == "native change"));
}

#[test]
fn a_build_installs_only_between_specs() {
    let (mut run, _) = open("fix-between");
    run.dispatch(1, DRAW, Android, brief).unwrap();
    let err = run
        .install_build(2, Android, build("b2", "fp1"), vec![])
        .unwrap_err()
        .to_string();
    assert!(err.contains("still running or waiting for review"), "{err}");
    run.install_build(2, Ios, build("b2", "fp1"), vec![])
        .unwrap();
}

#[test]
fn a_rerun_after_finishing_opens_the_run_again() {
    let path = journal("fix-after-finish");
    let mut run = Run::open(plan(false), &path, 0).unwrap();
    pass(&mut run, 1, BACK, Android);
    run.finish(3).unwrap();
    let none = run
        .install_build(4, Android, build("a1", "fp1"), vec![])
        .unwrap();
    assert!(none.is_empty() && run.state().finished);
    let rerun = vec![Rerun {
        spec: BACK.into(),
        reason: "the harness gap that blocked it is fixed".into(),
    }];
    run.install_build(5, Android, build("a1", "fp1"), rerun)
        .unwrap();
    assert!(!run.state().finished);
    run.dispatch(6, BACK, Android, brief).unwrap();
    drop(run);
    assert!(!Run::open(plan(false), &path, 0).unwrap().state().finished);
}

fn row(spec: &str, platform: Platform) -> RowRef {
    RowRef {
        spec: spec.into(),
        platform,
    }
}

#[test]
fn any_row_can_be_queued_again_whatever_its_state() {
    let path = journal("requeue");
    let mut run = Run::open(plan(false), &path, 0).unwrap();
    // Filed.
    pass(&mut run, 1, BACK, Android);
    // Waiting for review.
    let t = run.dispatch(3, DRAW, Android, brief).unwrap();
    run.post(4, &t, judged(&[Answer::Failed, Answer::Held]))
        .unwrap();
    // Running.
    let running = run.dispatch(5, DRAW, Ios, brief).unwrap();
    assert!(run.requeue(6, vec![row(BACK, Android)], " ", None).is_err());
    let was = run
        .requeue(
            6,
            vec![row(BACK, Android), row(DRAW, Android), row(DRAW, Ios)],
            "the harness fix needs proving",
            Some("use `touch` for outcome 1".into()),
        )
        .unwrap();
    let was: Vec<&str> = was.iter().map(|(_, w)| *w).collect();
    assert_eq!(was, ["already filed", "waiting for review", "running"]);
    for (spec, platform) in [(BACK, Android), (DRAW, Android), (DRAW, Ios)] {
        let r = run.row(spec, platform).unwrap();
        assert!(matches!(r.row, Row::Queued { verify: None }), "{spec}");
        assert_eq!(r.budget, 0);
        assert_eq!(r.rerun_note.as_deref(), Some("use `touch` for outcome 1"));
    }
    // The abandoned attempt's tester can't post any more.
    assert!(
        run.post(7, &running, judged(&[Answer::Held, Answer::Held]))
            .is_err()
    );
    drop(run);
    let run = Run::open(plan(false), &path, 0).unwrap();
    assert!(matches!(
        run.row(DRAW, Ios).unwrap().row,
        Row::Queued { .. }
    ));
}

#[test]
fn a_pass_that_skips_its_probes_waits_for_review() {
    let path = journal("unprobed");
    let mut run = Run::open(plan(false), &path, 0).unwrap();
    let t = run.dispatch(1, BACK, Android, brief).unwrap();
    let claim = run
        .post_probed(2, &t, judged(&[Answer::Held]), true)
        .unwrap();
    assert_eq!(claim, Claim::Unsure);
    assert!(matches!(
        run.row(BACK, Android).unwrap().row,
        Row::NeedsReview { .. }
    ));
    run.review(
        3,
        BACK,
        Android,
        Call::Judge {
            result: Final::Pass,
            title: None,
        },
        "the outcomes held; the probes don't apply to this build",
    )
    .unwrap();
    assert_eq!(result(&run, BACK, Android), Some(Final::Pass));

    // Saying what was probed files the pass straight away, and a replay keeps both calls.
    let t = run.dispatch(4, DRAW, Android, brief).unwrap();
    let mut probed = judged(&[Answer::Held, Answer::Held]);
    probed.probed = Some("undid twice quickly: both strokes went".into());
    assert_eq!(run.post_probed(5, &t, probed, true).unwrap(), Claim::Pass);
    drop(run);
    let run = Run::open(plan(false), &path, 0).unwrap();
    assert_eq!(result(&run, BACK, Android), Some(Final::Pass));
    assert_eq!(result(&run, DRAW, Android), Some(Final::Pass));
}

#[test]
fn a_problem_keeps_the_wording_it_was_judged_against() {
    let path = journal("wording");
    let mut run = Run::open(plan(false), &path, 0).unwrap();
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
    run.post(2, &t, judged(&[Answer::Failed, Answer::Held]))
        .unwrap();
    run.review(3, DRAW, Android, bug(), "real").unwrap();
    drop(run);
    // The spec is reworded, and the run resumed with the new text.
    let mut reworded = plan(false);
    reworded.specs[0].outcomes[0] = "Undo takes the stroke away".into();
    let run = Run::open(reworded, &path, 0).unwrap();
    let detail = &run.state().problems[0].detail;
    assert!(detail.starts_with("Undo removes the stroke: "), "{detail}");
    assert_eq!(
        run.row(DRAW, Android).unwrap().judged_against[0],
        "Undo removes the stroke"
    );
}

#[test]
fn queueing_a_finished_run_opens_it_again() {
    let mut run = open("requeue-finished").0;
    pass(&mut run, 1, BACK, Android);
    run.finish(3).unwrap();
    run.requeue(4, vec![row(BACK, Android)], "check it again", None)
        .unwrap();
    assert!(!run.state().finished);
    run.dispatch(5, BACK, Android, brief).unwrap();
}

#[test]
fn each_build_gets_a_fresh_attempt_budget() {
    let (mut run, path) = open("fix-budget");
    for at in [1u64, 10, 20] {
        let t = run.dispatch(at, BACK, Android, brief).unwrap();
        run.post(at + 1, &t, judged(&[Answer::Failed])).unwrap();
        if at < 20 {
            run.review(at + 2, BACK, Android, Call::SendBack, "misread")
                .unwrap();
        }
    }
    run.review(30, BACK, Android, bug(), "seen 3 times")
        .unwrap();
    run.install_build(40, Android, build("b2", "fp1"), vec![])
        .unwrap();
    let t = run.dispatch(41, BACK, Android, brief).unwrap();
    assert_eq!(
        t, "annotations/002-back#4@android",
        "attempt numbers keep counting"
    );
    drop(run);
    let run = Run::open(plan(false), &path, 0).unwrap();
    assert_eq!(
        run.row(BACK, Android).unwrap().budget,
        1,
        "the replay agrees"
    );
}

#[test]
fn a_blocked_needs_its_cause_and_an_app_cause_is_a_failure() {
    let (mut run, _) = open("blocked-cause");
    let t = run.dispatch(1, BACK, Android, brief).unwrap();
    run.post(2, &t, judged(&[Answer::CouldntCheck])).unwrap();
    let call = |title: Option<&str>, cause| Call::Uphold {
        title: title.map(str::to_string),
        cause,
    };
    let err = run
        .review(3, BACK, Android, call(None, None), "r")
        .unwrap_err();
    assert!(err.to_string().contains("app or harness"), "{err}");
    let err = run
        .review(3, BACK, Android, call(None, Some(Cause::App)), "crashed")
        .unwrap_err();
    assert!(
        err.to_string().contains("give the failure a title"),
        "{err}"
    );
    run.review(
        3,
        BACK,
        Android,
        call(Some("The app crashes going back"), Some(Cause::App)),
        "crashed",
    )
    .unwrap();
    assert_eq!(run.state().problems[0].kind, ProblemKind::Failure);
    assert_eq!(
        result(&run, BACK, Android),
        Some(Final::Fail),
        "the app stopped it, so it failed"
    );
    assert_eq!(run.verdict().reasons[0], "1 failure");
}

#[test]
fn an_unsure_is_judged_pass_or_fail_only() {
    let (mut run, _) = open("judge-blocked");
    let t = run.dispatch(1, BACK, Android, brief).unwrap();
    run.post(2, &t, judged(&[Answer::Unsure])).unwrap();
    let judge = Call::Judge {
        result: Final::Blocked,
        title: None,
    };
    assert!(
        run.review(3, BACK, Android, judge, "r")
            .unwrap_err()
            .to_string()
            .contains("spec unclear")
    );
}

fn degraded() -> soundcheck_driver::Health {
    soundcheck_driver::Health {
        degraded: vec!["memory 4% free".into()],
        facts: vec!["memory 4% free".into()],
        ..Default::default()
    }
}

#[test]
fn a_failure_on_a_degraded_device_must_reproduce_on_a_healthy_one() {
    let (mut run, path) = open("degraded");
    let t = run.dispatch(1, DRAW, Android, brief).unwrap();
    run.post(2, &t, judged(&[Answer::Failed, Answer::Held]))
        .unwrap();
    run.device_health(3, DRAW, Android, 1, degraded()).unwrap();
    let err = run.review(4, DRAW, Android, bug(), "real").unwrap_err();
    assert!(
        err.to_string().contains("verify it on a healthy device"),
        "{err}"
    );
    // A harness BLOCKED, or a verification, still goes through.
    run.review(
        4,
        DRAW,
        Android,
        Call::Verify { claim: "c".into() },
        "device was low on memory",
    )
    .unwrap();
    let t2 = run.dispatch(5, DRAW, Android, brief).unwrap();
    run.post(6, &t2, judged(&[Answer::Failed, Answer::Held]))
        .unwrap();
    run.review(7, DRAW, Android, bug(), "reproduced").unwrap();
    assert!(run.state().problems[0].verified);
    // The degradation is a note in the record, and survives a replay of the journal.
    let notes: Vec<String> = run.state().notes.iter().map(|n| n.text.clone()).collect();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("device degraded during attempt 1: memory 4% free"))
    );
    drop(run);
    let replayed = Run::open(plan(false), &path, 8).unwrap();
    assert_eq!(replayed.state().notes.len(), notes.len());
}

#[test]
fn a_spec_running_when_the_server_stopped_goes_first_without_using_an_attempt() {
    let (mut run, path) = open("interrupted");
    run.dispatch(1, DRAW, Android, brief).unwrap();
    drop(run);
    // The server died; a resume replays the journal and puts the running spec back.
    let mut resumed = Run::open(plan(false), &path, 2).unwrap();
    assert_eq!(resumed.interrupt_running(3).unwrap(), 1);
    let row = resumed.row(DRAW, Android).unwrap();
    assert!(matches!(row.row, Row::Queued { .. }), "{:?}", row.row);
    assert_eq!(row.budget, 0);
    // Attempt numbers still only go up, so a later pass can't be mistaken for the interrupted one.
    resumed.dispatch(4, DRAW, Android, brief).unwrap();
    assert_eq!(resumed.row(DRAW, Android).unwrap().attempts, 2);
    drop(resumed);
    let replayed = Run::open(plan(false), &path, 5).unwrap();
    assert_eq!(replayed.row(DRAW, Android).unwrap().budget, 1);
}

#[test]
fn evidence_added_after_review_shows_on_the_problem_and_survives_a_reopen() {
    let (mut run, path) = open("add-evidence");
    fail_and_uphold(&mut run, 1, DRAW, Android);
    let id = run.state().problems[0].id;
    let shot = "annotations-001-draw-1-android-slider.png";
    assert!(
        run.add_evidence(2, id, shot, " ").is_err(),
        "needs a reason"
    );
    assert!(run.add_evidence(2, 99, shot, "drive shot").is_err());
    run.add_evidence(2, id, shot, "reproduced with drive")
        .unwrap();
    assert!(
        run.add_evidence(3, id, shot, "again").is_err(),
        "no doubles"
    );
    let line = soundcheck::serve::events::summary(run.entries_after(0).last().unwrap());
    assert!(
        line.ends_with(&format!(
            "problem {id} shows {shot} (reproduced with drive)"
        )),
        "{line}"
    );
    drop(run);
    let run = Run::open(plan(false), &path, 4).unwrap();
    assert!(
        run.state()
            .problem(id)
            .unwrap()
            .evidence
            .ends_with(&[shot.to_string()])
    );
}
