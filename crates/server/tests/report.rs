use serde_json::{Value, json};
use soundcheck::report::{self, Page};
use soundcheck::run::model::*;
use soundcheck::run::{Plan, PlannedSpec, Run};
use soundcheck::suite::{self, config::Platform};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const ANDROID: Platform = Platform::literal("android");
const IOS: Platform = Platform::literal("ios");
const WEB: Platform = Platform::literal("web");

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
                outcomes: soundcheck::run::outcomes_by_platform(s, &s.front.platforms),
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
        .dispatch(t0, "annotations/001-draw", ANDROID, brief)
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
        ANDROID,
        Call::Uphold {
            title: Some("Undo leaves the stroke on screen when zoomed in".into()),
            cause: None,
        },
        "The screenshot shows the stroke after Undo.",
    )
    .unwrap();

    let t = run
        .dispatch(t0 + 100_000, "annotations/002-back", ANDROID, brief)
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
        .dispatch(t0 + 200_000, "annotations/001-draw", IOS, brief)
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

fn page<'a>(suite: &'a suite::Suite, run: &'a Run, label: &'a str, live: bool) -> Page<'a> {
    Page {
        suite,
        run,
        label,
        live,
        run_dir: None,
        export: None,
        starting: None,
    }
}

/// The view JSON embedded in a document, as the page reads it.
fn embedded(html: &str) -> Value {
    let start = html
        .find("<script type=\"application/json\" id=\"view\">")
        .unwrap();
    let json = &html[start..];
    let json = &json[json.find('>').unwrap() + 1..json.find("</script>").unwrap()];
    serde_json::from_str(json).unwrap()
}

#[test]
fn the_page_shows_failures_findings_and_checks() {
    let (suite, run) = sample();
    let view = report::view::json(&page(&suite, &run, "v2.16.2 · candidate", true));
    if let Ok(path) = std::env::var("SOUNDCHECK_REPORT_SAMPLE") {
        std::fs::write(path, serde_json::to_string_pretty(&view).unwrap()).unwrap();
    }

    assert_eq!(view["label"], "v2.16.2 · candidate");
    assert_eq!(view["summary"]["word"], "Failing");
    assert_eq!(view["summary"]["class"], "failing");
    let reasons = view["summary"]["reasons"].as_array().unwrap();
    assert!(reasons.contains(&json!({ "text": "1 failure", "bad": true })));
    for want in ["1 finding to decide", "1 being checked"] {
        assert!(
            reasons.iter().any(|r| r["text"] == want),
            "missing {want:?}"
        );
    }

    let draw = &view["checks"]["annotations/001-draw"];
    assert_eq!(draw["open"], true, "a failed row opens by itself");
    let cells: Vec<(&str, &str)> = draw["cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["platform"].as_str().unwrap(), c["text"].as_str().unwrap()))
        .collect();
    assert!(cells.contains(&("android", "Failed")));
    assert!(cells.contains(&("ios", "Checking")));
    let android = &draw["bodies"][0];
    assert_eq!(
        android["failure"]["call"],
        "The screenshot shows the stroke after Undo."
    );
    assert_eq!(
        android["outcomes"][0]["observed"],
        "The stroke stayed after Undo at 2.5× zoom."
    );
    assert_eq!(android["outcomes"][0]["failure"], true);

    let back = &view["checks"]["annotations/002-back"];
    assert_eq!(
        back["pill"],
        json!({ "class": "finding", "text": "1 finding" })
    );
    let finding = &back["bodies"][0]["findings"][0];
    let title = finding["title"].as_str().unwrap();
    assert!(
        title.starts_with("Finding 00")
            && title.ends_with(": The back arrow flickers on fast taps")
    );

    let open = &view["groups"][0];
    assert_eq!(open["title"], "Open");
    let problems: Vec<&Value> = open["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| &view["problems"][id.as_str().unwrap()])
        .collect();
    assert_eq!(
        problems[0]["badge"],
        json!({ "class": "blocks", "text": "Failure" })
    );
    assert_eq!(
        problems[0]["title"],
        "Undo leaves the stroke on screen when zoomed in"
    );
    assert!(problems[0]["number"].as_str().unwrap().starts_with("00"));
    assert_eq!(
        problems[1]["badge"],
        json!({ "class": "finding", "text": "Finding" })
    );
    assert!(problems[1]["paras"].as_array().unwrap().contains(&json!(
        "No screenshot of it; the detail above is all there is."
    )));

    assert_eq!(view["bar"]["kind"], "now");
    let doing = &view["bar"]["doing"][0];
    assert_eq!(doing["platform"], "ios");
    assert_eq!(doing["spec"], "annotations/001-draw");
    assert!(doing["checking"]["since"].is_u64());
    assert!(
        !view.to_string().contains("severity"),
        "no severity anywhere"
    );
    assert!(
        !view.to_string().contains("null"),
        "a null would delete its key in a patch"
    );
}

#[test]
fn an_export_has_no_live_bar() {
    let (suite, run) = sample();
    let html = report::document(&Page {
        export: Some(report::Export {
            at: 0,
            finished: false,
        }),
        ..page(&suite, &run, "v2.16.2", false)
    });
    let view = embedded(&html);
    assert!(view.get("bar").is_none());
    assert_eq!(view["export"], true);
    assert!(html.contains("data-live=\"false\""));
}

#[test]
fn text_from_testers_is_data_that_cant_end_its_script() {
    let (suite, mut run) = sample();
    let sly = "</script><script>alert(1)</script><!-- \"x\" & 'y'";
    let t = run
        .dispatch(1, "annotations/001-draw", WEB, |_, _| String::new())
        .unwrap();
    run.post(
        2,
        &t,
        judged(&[(Answer::Held, sly), (Answer::Held, "fine")]),
    )
    .unwrap();
    let html = report::document(&page(&suite, &run, "x", false));
    let script = &html[html.find("id=\"view\">").unwrap()..];
    let script = &script[..script.find("</script>").unwrap()];
    assert!(
        !script.contains('<'),
        "nothing in the view can end its script"
    );
    let view = embedded(&html);
    let web = view["checks"]["annotations/001-draw"]["bodies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["platform"] == "web")
        .unwrap();
    assert_eq!(
        web["outcomes"][0]["observed"], sly,
        "and it reads back unchanged"
    );
}

#[test]
fn the_runs_list_escapes_what_it_renders() {
    assert_eq!(
        report::esc("<b>\"x\" & 'y'</b>"),
        "&lt;b&gt;&quot;x&quot; &amp; &#39;y&#39;&lt;/b&gt;"
    );
}

#[test]
fn the_app_can_sit_in_a_script_element() {
    assert!(!report::APP.is_empty());
    assert!(!report::APP.to_lowercase().contains("</script"));
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
        .dispatch(1, "annotations/001-draw", WEB, |_, _| String::new())
        .unwrap();
    let mut posted = judged(&[(Answer::Held, "fine"), (Answer::Held, "fine")]);
    posted.judgments[0].evidence.push("back.png".into());
    run.post(2, &t, posted).unwrap();
    let page = |export: Option<report::Export>| Page {
        live: export.is_none(),
        run_dir: Some(&dir),
        export,
        ..page(&suite, &run, "x", true)
    };
    let shots = |view: &Value| -> Vec<Value> {
        view["checks"]["annotations/001-draw"]["bodies"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b.get("evidence"))
            .flat_map(|e| e["shots"].as_array().unwrap().clone())
            .collect()
    };
    let live = report::view::json(&page(None));
    assert!(
        shots(&live).contains(&json!({ "name": "back.png", "src": "files/evidence/back.png" }))
    );
    let exported = embedded(&report::document(&page(Some(report::Export {
        at: 3,
        finished: false,
    }))));
    assert!(
        shots(&exported)
            .contains(&json!({ "name": "back.png", "src": "data:image/png;base64,iVBORw==" }))
    );
    assert!(
        exported["facts"]
            .as_array()
            .unwrap()
            .contains(&json!({ "text": "In progress, exported", "at": 3 }))
    );
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
        .dispatch(1, "annotations/001-draw", WEB, |_, _| String::new())
        .unwrap();
    let mut posted = judged(&[(Answer::Held, "fine"), (Answer::Held, "fine")]);
    posted.judgments[0].evidence.push("erased".into());
    run.post(2, &t, posted).unwrap();
    let view = report::view::json(&Page {
        run_dir: Some(&dir),
        ..page(&suite, &run, "x", true)
    });
    assert!(
        view.to_string()
            .contains(r#"{"name":"erased","src":"files/evidence/erased.png"}"#),
        "{view}"
    );
}

#[test]
fn a_failure_with_no_failed_outcome_still_shows_why_it_failed() {
    let (suite, mut run) = sample();
    // A BLOCKED the app caused is filed as a FAIL, though no outcome reads as failed.
    let t = run
        .dispatch(300_000, "annotations/001-draw", WEB, |_, _| String::new())
        .unwrap();
    run.post(
        310_000,
        &t,
        judged(&[
            (Answer::CouldntCheck, "The score never opened."),
            (Answer::CouldntCheck, "The score never opened."),
        ]),
    )
    .unwrap();
    run.review(
        320_000,
        "annotations/001-draw",
        WEB,
        Call::Uphold {
            title: Some("The score never opens on web".into()),
            cause: Some(Cause::App),
        },
        "The app crashed opening the score.",
    )
    .unwrap();
    let view = report::view::json(&page(&suite, &run, "x", true));
    let bodies = view["checks"]["annotations/001-draw"]["bodies"]
        .as_array()
        .unwrap();
    let web = bodies.iter().find(|b| b["platform"] == "web").unwrap();
    assert_eq!(web["failure"]["alone"], true, "{web}");
    assert_eq!(web["failure"]["call"], "The app crashed opening the score.");
    assert!(
        web["outcomes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["failure"] == false)
    );

    let android = bodies.iter().find(|b| b["platform"] == "android").unwrap();
    assert_eq!(
        android["failure"]["alone"], false,
        "a failed outcome holds it"
    );
}

#[test]
fn an_unchanged_view_needs_no_patch() {
    let (suite, run) = sample();
    let before = report::view::json(&page(&suite, &run, "x", true));
    let after = report::view::json(&page(&suite, &run, "x", true));
    assert_eq!(report::view::merge_patch(&before, &after), None);
}

#[test]
fn a_row_that_changes_patches_only_its_own_key() {
    let (suite, mut run) = sample();
    let before = report::view::json(&page(&suite, &run, "x", true));
    run.dispatch(3, "annotations/001-draw", WEB, |_, _| String::new())
        .unwrap();
    let after = report::view::json(&page(&suite, &run, "x", true));
    let patch = report::view::merge_patch(&before, &after).unwrap();
    let keys = |v: &Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };
    assert_eq!(keys(&patch["checks"]), ["annotations/001-draw"]);
    assert_eq!(keys(&patch["timeline"]).len(), 1, "one new journal entry");
    assert!(patch.get("problems").is_none());
    assert!(patch.get("sections").is_none());
}

#[test]
fn a_merge_patch_removes_what_went_and_keeps_what_stayed() {
    let before = json!({ "a": 1, "b": { "c": 2, "d": [1, 2] }, "gone": { "x": 1 } });
    let after = json!({ "a": 1, "b": { "c": 3, "d": [1, 2] }, "new": "y" });
    assert_eq!(
        report::view::merge_patch(&before, &after),
        Some(json!({ "b": { "c": 3 }, "gone": null, "new": "y" }))
    );
}

#[test]
fn a_shot_is_named_under_its_attempts_prefix() {
    let base = report::evidence_base("annotations/003-eraser", 1, IOS);
    assert_eq!(base, "annotations-003-eraser-1-ios");
    for given in [
        "wide",
        "wide.png",
        "wide.webp",
        "annotations-003-eraser-1-ios-wide.webp",
    ] {
        assert_eq!(
            report::shot_file(&base, given),
            "annotations-003-eraser-1-ios-wide.webp",
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
