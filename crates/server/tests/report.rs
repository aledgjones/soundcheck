use soundcheck::report::{self, Page};
use soundcheck::run::model::*;
use soundcheck::run::{Plan, PlannedSpec, Run};
use soundcheck::suite::{self, config::Platform::*};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn judged(answers: &[(Answer, &str)]) -> Posted {
    Posted {
        judgments: answers
            .iter()
            .enumerate()
            .map(|(i, (answer, observed))| Judgment {
                outcome: i,
                answer: *answer,
                observed: (*observed).into(),
                evidence: vec![],
            })
            .collect(),
        side_finds: vec![],
        notes: None,
        probed: None,
    }
}

/// A run in the middle of things: a failure, a check in progress and a pass with a side find.
fn sample() -> (suite::Suite, Run) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/good");
    let suite = suite::load(&root);
    let plan = Plan {
        specs: suite
            .specs
            .iter()
            .map(|s| PlannedSpec {
                id: s.id.clone(),
                platforms: s.front.platforms.clone(),
                outcomes: s.outcomes.clone(),
            })
            .collect(),
        parallel: false,
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("soundcheck-report-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut run = Run::open(plan, &dir.join("journal.jsonl"), 0).unwrap();
    let brief = |_: &str, _: Option<&str>| String::from("brief");
    let t0 = 1_792_540_000_000u64;

    let t = run
        .dispatch(t0, "annotations/001-draw", Android, brief)
        .unwrap();
    run.post(
        t0 + 60_000,
        &t,
        judged(&[
            (Answer::Failed, "The stroke stayed after Undo at 2.5× zoom."),
            (Answer::Held, "Undo greyed out after the undo."),
        ]),
    )
    .unwrap();
    run.review(
        t0 + 90_000,
        "annotations/001-draw",
        Android,
        Call::Uphold {
            title: Some("Undo leaves the stroke on screen when zoomed in".into()),
            cause: None,
        },
        "The screenshot shows the stroke after Undo.",
    )
    .unwrap();

    let t = run
        .dispatch(t0 + 100_000, "annotations/002-back", Android, brief)
        .unwrap();
    let mut posted = judged(&[(Answer::Held, "The collection showed every time.")]);
    posted.side_finds.push(SideFind {
        summary: "Back arrow flickers".into(),
        observed: "It flashed white on the third tap.".into(),
        evidence: vec![],
    });
    run.post(t0 + 160_000, &t, posted).unwrap();
    run.review_find(
        t0 + 170_000,
        2,
        Some("The back arrow flickers on fast taps".into()),
        vec![],
        "only the observation shows it",
    )
    .unwrap();

    let t = run
        .dispatch(t0 + 200_000, "annotations/001-draw", Ios, brief)
        .unwrap();
    run.post(
        t0 + 260_000,
        &t,
        judged(&[
            (Answer::Unsure, "The stroke faded but a ghost line stayed."),
            (Answer::Held, "Fine."),
        ]),
    )
    .unwrap();
    (suite, run)
}

#[test]
fn the_page_shows_failures_findings_and_checks() {
    let (suite, run) = sample();
    let html = report::document(&Page {
        suite: &suite,
        run: &run,
        label: "v2.16.2 · candidate",
        live: true,
        run_dir: None,
        export: None,
    });
    if let Ok(path) = std::env::var("SOUNDCHECK_REPORT_SAMPLE") {
        std::fs::write(path, &html).unwrap();
    }
    for want in [
        "<h1>v2.16.2 · candidate</h1>",
        "<strong class=\"failing\">Failing</strong>",
        "<li class=\"chip reason bad\">1 failure</li>",
        "1 finding to decide",
        "1 being checked",
        "<span class=\"chip blocks\">Failure</span><",
        "</span>Undo leaves the stroke on screen when zoomed in</h3>",
        "<span class=\"chip finding\">Finding</span>",
        "The stroke stayed after Undo at 2.5× zoom.",
        "The screenshot shows the stroke after Undo.",
        "Checking</span>",
        "<span class=\"chip small finding\">1 finding</span>",
        "Finding 00",
        ": The back arrow flickers on fast taps",
        "No screenshot of it; the detail above is all there is.",
        "<span class=\"pid\">00",
        "class=\"nowbar\"",
        "checking <b>annotations/001-draw</b>",
    ] {
        assert!(html.contains(want), "missing {want:?}");
    }
    assert!(!html.contains("severity"), "no severity anywhere");
}

#[test]
fn an_export_has_no_live_bar() {
    let (suite, run) = sample();
    let html = report::document(&Page {
        suite: &suite,
        run: &run,
        label: "v2.16.2",
        live: false,
        run_dir: None,
        export: Some(report::Export {
            at: 0,
            finished: false,
        }),
    });
    assert!(!html.contains("class=\"nowbar\""));
    assert!(html.contains("data-live=\"false\""));
}

#[test]
fn text_from_testers_is_escaped() {
    assert_eq!(
        report::esc("<b>\"x\" & 'y'</b>"),
        "&lt;b&gt;&quot;x&quot; &amp; &#39;y&#39;&lt;/b&gt;"
    );
}

#[test]
fn an_export_embeds_its_screenshots() {
    let (suite, mut run) = sample();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soundcheck-evidence-{nanos}"));
    std::fs::create_dir_all(dir.join("evidence")).unwrap();
    std::fs::write(dir.join("evidence/back.png"), [0x89, b'P', b'N', b'G']).unwrap();
    let t = run
        .dispatch(1, "annotations/001-draw", Web, |_, _| String::new())
        .unwrap();
    let mut posted = judged(&[(Answer::Held, "fine"), (Answer::Held, "fine")]);
    posted.judgments[0].evidence.push("back.png".into());
    run.post(2, &t, posted).unwrap();
    let page = |export: Option<report::Export>| Page {
        suite: &suite,
        run: &run,
        label: "x",
        live: export.is_none(),
        run_dir: Some(&dir),
        export,
    };
    let live = report::document(&page(None));
    assert!(live.contains("src=\"files/evidence/back.png\""));
    let exported = report::document(&page(Some(report::Export {
        at: 3,
        finished: false,
    })));
    assert!(exported.contains("src=\"data:image/png;base64,iVBORw==\""));
    assert!(exported.contains("In progress, exported"));
}

#[test]
fn a_bare_evidence_name_from_an_older_journal_shows_its_png() {
    let (suite, mut run) = sample();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soundcheck-bare-evidence-{nanos}"));
    std::fs::create_dir_all(dir.join("evidence")).unwrap();
    std::fs::write(dir.join("evidence/erased.png"), [0x89, b'P', b'N', b'G']).unwrap();
    let t = run
        .dispatch(1, "annotations/001-draw", Web, |_, _| String::new())
        .unwrap();
    let mut posted = judged(&[(Answer::Held, "fine"), (Answer::Held, "fine")]);
    posted.judgments[0].evidence.push("erased".into());
    run.post(2, &t, posted).unwrap();
    let page = Page {
        suite: &suite,
        run: &run,
        label: "x",
        live: true,
        run_dir: Some(&dir),
        export: None,
    };
    let live = report::document(&page);
    assert!(live.contains("src=\"files/evidence/erased.png\""), "{live}");
}

#[test]
fn a_shot_is_named_under_its_attempts_prefix() {
    let base = report::evidence_base("annotations/003-eraser", 1, Ios);
    assert_eq!(base, "annotations-003-eraser-1-ios");
    for given in ["wide", "wide.png", "annotations-003-eraser-1-ios-wide.png"] {
        assert_eq!(
            report::shot_file(&base, given),
            "annotations-003-eraser-1-ios-wide.png",
            "{given}"
        );
    }
}

#[test]
fn the_timer_counts_working_time_not_a_night_between_sessions() {
    use soundcheck::run::event::Event;
    use soundcheck::run::journal::Entry;
    let at = |seq: u64, minutes: u64| Entry {
        seq,
        at: minutes * 60_000,
        event: Event::Resumed,
    };
    // Ten minutes of work, a night off, then five more.
    let entries = [at(1, 0), at(2, 4), at(3, 10), at(4, 10 + 600), at(5, 615)];
    assert_eq!(report::active_ms(&entries), 15 * 60_000);
}
