//! Writes a made-up run of `docs/demo/suite` (Larder, a shopping list app) for the README's screenshots, through the
//! same run API a real coordinator drives. `cargo run --example demo`, then `soundcheck serve docs/demo/suite`.

use anyhow::Result;
use soundcheck::report::EVIDENCE_DIR;
use soundcheck::run::model::{
    Answer, Build, Call, Cause, Decision, Final, Judgment, Posted, SideFind,
};
use soundcheck::run::{Run, saved};
use soundcheck::serve::SUMMARY_FILE;
use soundcheck::suite::{self, config::Platform};
use soundcheck_driver::Health;
use std::path::Path;

const LABEL: &str = "v3.4.0 · candidate";
/// 2026-10-21 08:30 UTC.
const START_MS: u64 = 1_792_571_400_000;
const MIN: u64 = 60_000;

fn main() -> Result<()> {
    let demo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/demo");
    let suite = suite::load(&demo.join("suite"));
    let run_dir = demo.join("reports/20261021-083000");
    let _ = std::fs::remove_dir_all(&run_dir);
    std::fs::create_dir_all(run_dir.join(EVIDENCE_DIR))?;
    let platforms = vec![Platform::Android, Platform::Ios];
    let info = saved::RunInfo {
        suite: demo.join("suite"),
        label: LABEL.into(),
        platforms: platforms.clone(),
        scope: vec![],
    };
    saved::save(&run_dir, &info, &suite)?;
    let plan = saved::plan_for(&suite, &platforms, &[])?;
    let mut run = Run::open(plan, &run_dir.join("journal.jsonl"), START_MS)?;
    let mut t = Clock(START_MS);

    for (platform, commit, fingerprint) in [
        (Platform::Android, "4e1c2a9", "9f2b61d0"),
        (Platform::Ios, "4e1c2a9", "c07e3a55"),
    ] {
        let build = Build {
            commit: commit.into(),
            fingerprint: Some(fingerprint.into()),
            version: Some("3.4.0".into()),
        };
        run.install_build(t.tick(0), platform, build, vec![])?;
    }

    let shot = |spec: &str, attempt: u32, platform: Platform, name: &str| -> Result<String> {
        let file = format!("{}-{attempt}-{platform}-{name}.png", spec.replace('/', "-"));
        std::fs::copy(
            demo.join("evidence").join(format!("{name}.png")),
            run_dir.join(EVIDENCE_DIR).join(&file),
        )?;
        Ok(file)
    };

    let passes: &[(&str, [&str; 2])] = &[
        (
            "lists/001-first-list",
            [
                "The empty account showed \"Create your first list\" with a button that opened the name field.",
                "Weekly shop opened at once with \"No items yet\".",
            ],
        ),
        (
            "lists/002-rename",
            [
                "The title and the side menu both read Saturday shop.",
                "After relaunching, the side menu still listed Saturday shop.",
            ],
        ),
        (
            "lists/003-delete",
            [
                "The alert read \"Delete Weekly shop?\" with Cancel and Delete.",
                "The side menu was empty and the first-list prompt showed.",
            ],
        ),
        (
            "items/001-add-items",
            [
                "Lemons, Rice and Coffee appeared in turn above the checked items.",
                "The keyboard stayed up and the bar kept focus after each item.",
            ],
        ),
        (
            "items/002-check-off",
            [
                "Eggs and Oat milk were struck through and moved below Rice.",
                "Eggs went back between Bread and Lemons when unchecked.",
            ],
        ),
        (
            "sharing/001-invite",
            [
                "Members showed sam@example.com · Invited.",
                "Sam's account showed the invite to Weekly shop.",
            ],
        ),
        (
            "offline/002-add-offline",
            [
                "Basil appeared with a small cloud mark while offline.",
                "The cloud mark cleared about two seconds after going back online.",
            ],
        ),
    ];

    for platform in [Platform::Android, Platform::Ios] {
        for (spec, observed) in passes {
            post(&mut run, &mut t, spec, platform, held(observed), 3)?;
        }
    }

    // Reorder: passes, with a side find on iOS that becomes a finding, deferred at triage.
    post(
        &mut run,
        &mut t,
        "items/003-reorder",
        Platform::Android,
        held(&[
            "Bread followed the finger to the top while the rows below shifted down.",
            "After relaunching, Bread was still first.",
        ]),
        4,
    )?;
    let mut reorder = held(&[
        "Bread followed the finger to the top while the rows below shifted down.",
        "After relaunching, Bread was still first.",
    ]);
    reorder.side_finds.push(SideFind {
        summary: "Dragging an item past the bottom edge doesn't scroll the list".into(),
        observed: "Held Coffee and moved it below the last visible row: the list stayed put, so Coffee couldn't go further down.".into(),
        evidence: vec![],
    });
    post(
        &mut run,
        &mut t,
        "items/003-reorder",
        Platform::Ios,
        reorder,
        4,
    )?;
    let find = run
        .state()
        .pending_finds
        .first()
        .map(|f| f.id)
        .expect("the reorder side find");
    run.review_find(
        t.tick(1),
        find,
        Some("Dragging an item below the screen's edge doesn't scroll the list".into()),
        vec![],
        "A long list can't be reordered past what's on screen",
    )?;

    // Quantities: an Unsure on Android, judged from the evidence.
    post(
        &mut run,
        &mut t,
        "items/004-quantities",
        Platform::Ios,
        held(&[
            "The row read Eggs × 12.",
            "The minus button greyed out at 1.",
        ]),
        3,
    )?;
    let unsure = Posted {
        judgments: vec![
            judgment(0, Answer::Held, "The row read Eggs × 12.", vec![]),
            judgment(
                1,
                Answer::Unsure,
                "At 1 the minus button stayed enabled, but tapping it left the count at 1. I can't tell if it should grey out.",
                vec![],
            ),
        ],
        side_finds: vec![],
        notes: None,
        probed: None,
    };
    post(
        &mut run,
        &mut t,
        "items/004-quantities",
        Platform::Android,
        unsure,
        3,
    )?;
    run.review(
        t.tick(1),
        "items/004-quantities",
        Platform::Android,
        Call::Judge {
            result: Final::Pass,
            title: None,
        },
        "The count never goes below 1, which is the outcome; Android keeps the button enabled by design",
    )?;

    // Leaving a share: the harness can't sign the second account in on Android.
    post(
        &mut run,
        &mut t,
        "sharing/002-leave",
        Platform::Ios,
        held(&[
            "After accepting, Sam's side menu listed Weekly shop with its six items.",
            "After leaving, it was gone for Sam and the owner still had it.",
        ]),
        5,
    )?;
    let blocked = Posted {
        judgments: vec![
            judgment(
                0,
                Answer::CouldntCheck,
                "Signing in as Sam asked for a code sent to a phone the test account doesn't have.",
                vec![],
            ),
            judgment(
                1,
                Answer::CouldntCheck,
                "Never got past Sam's sign-in.",
                vec![],
            ),
        ],
        side_finds: vec![],
        notes: None,
        probed: None,
    };
    post(
        &mut run,
        &mut t,
        "sharing/002-leave",
        Platform::Android,
        blocked,
        4,
    )?;
    run.review(
        t.tick(1),
        "sharing/002-leave",
        Platform::Android,
        Call::Uphold {
            title: None,
            cause: Some(Cause::Harness),
        },
        "The second test account needs two-step sign-in turned off; not the app",
    )?;

    // Checking off offline: fails, is verified, and is upheld as a failure.
    post(
        &mut run,
        &mut t,
        "offline/001-check-offline",
        Platform::Ios,
        held(&[
            "Eggs and Oat milk checked off and sank while the banner said No connection.",
            "Back online they stayed checked, and the second device showed them checked.",
        ]),
        4,
    )?;
    for attempt in [1, 2] {
        let evidence = vec![
            shot(
                "offline/001-check-offline",
                attempt,
                Platform::Android,
                "offline",
            )?,
            shot(
                "offline/001-check-offline",
                attempt,
                Platform::Android,
                "online",
            )?,
        ];
        let failed = Posted {
            judgments: vec![
                judgment(
                    0,
                    Answer::Held,
                    "Eggs and Oat milk checked off and sank while the banner said No connection.",
                    vec![evidence[0].clone()],
                ),
                judgment(
                    1,
                    Answer::Failed,
                    "Expected them to stay checked. Within a second of \"Back online\", both came back unchecked and moved up to their old places.",
                    evidence,
                ),
            ],
            side_finds: vec![],
            notes: None,
            probed: None,
        };
        post(
            &mut run,
            &mut t,
            "offline/001-check-offline",
            Platform::Android,
            failed,
            5,
        )?;
        let call = if attempt == 1 {
            Call::Verify {
                claim: "Items checked off offline come back unchecked on reconnecting".into(),
            }
        } else {
            Call::Uphold {
                title: Some(
                    "Items checked off offline come back unchecked when the phone reconnects"
                        .into(),
                ),
                cause: None,
            }
        };
        let reason = if attempt == 1 {
            "Plausible, but seen once: reproduce on a fresh attempt"
        } else {
            "Reproduced on a second attempt, with the same screenshots"
        };
        run.review(
            t.tick(1),
            "offline/001-check-offline",
            Platform::Android,
            call,
            reason,
        )?;
    }

    // Triage: the reorder finding is accepted for this release.
    let finding = run
        .state()
        .problems
        .iter()
        .find(|p| p.spec == "items/003-reorder")
        .map(|p| p.id)
        .expect("the reorder finding");
    run.triage(
        t.tick(8),
        finding,
        Decision::Defer {
            ticket: "LAR-41".into(),
        },
        "Real, but long lists are rare; accepted for 3.4.0",
    )?;
    run.finish(t.tick(1))?;

    std::fs::write(
        run_dir.join(SUMMARY_FILE),
        soundcheck::report::run_summary(&run, LABEL).to_string(),
    )?;
    println!("wrote {}", run_dir.display());
    in_progress(&demo, &suite)
}

/// A second run, stopped part way: some results in, one spec running, one queued to verify, the rest not run.
fn in_progress(demo: &Path, suite: &suite::Suite) -> Result<()> {
    let run_dir = demo.join("reports/20261022-091500");
    let _ = std::fs::remove_dir_all(&run_dir);
    std::fs::create_dir_all(run_dir.join(EVIDENCE_DIR))?;
    let platforms = vec![Platform::Android, Platform::Ios];
    let label = "v3.4.1 · candidate";
    let info = saved::RunInfo {
        suite: demo.join("suite"),
        label: label.into(),
        platforms: platforms.clone(),
        scope: vec![],
    };
    saved::save(&run_dir, &info, suite)?;
    let plan = saved::plan_for(suite, &platforms, &[])?;
    let start = START_MS + 24 * 60 * MIN + 45 * MIN;
    let mut run = Run::open(plan, &run_dir.join("journal.jsonl"), start)?;
    let mut t = Clock(start);
    for (spec, observed) in [
        (
            "items/001-add-items",
            [
                "Lemons, Rice and Coffee appeared in turn above the checked items.",
                "The keyboard stayed up and the bar kept focus after each item.",
            ],
        ),
        (
            "items/002-check-off",
            [
                "Eggs and Oat milk were struck through and moved below Rice.",
                "Eggs went back between Bread and Lemons when unchecked.",
            ],
        ),
    ] {
        for platform in [Platform::Android, Platform::Ios] {
            post(&mut run, &mut t, spec, platform, held(&observed), 3)?;
        }
    }
    // A failure the coordinator wants reproduced: queued for its next attempt.
    let failed = Posted {
        judgments: vec![
            judgment(
                0,
                Answer::Failed,
                "Dragging Bread's handle scrolled the list instead of moving the row.",
                vec![],
            ),
            judgment(1, Answer::CouldntCheck, "Bread never moved.", vec![]),
        ],
        side_finds: vec![],
        notes: None,
        probed: None,
    };
    post(
        &mut run,
        &mut t,
        "items/003-reorder",
        Platform::Android,
        failed,
        4,
    )?;
    run.review(
        t.tick(1),
        "items/003-reorder",
        Platform::Android,
        Call::Verify {
            claim: "Dragging a row's handle scrolls the list instead".into(),
        },
        "Seen once: reproduce it on a fresh attempt",
    )?;
    // And one running now.
    run.dispatch(t.tick(0), "items/003-reorder", Platform::Ios, |_, _| {
        "brief".into()
    })?;
    std::fs::write(
        run_dir.join(SUMMARY_FILE),
        soundcheck::report::run_summary(&run, label).to_string(),
    )?;
    println!("wrote {}", run_dir.display());
    Ok(())
}

struct Clock(u64);

impl Clock {
    fn tick(&mut self, minutes: u64) -> u64 {
        self.0 += minutes * MIN + 17_000;
        self.0
    }
}

fn judgment(outcome: usize, answer: Answer, observed: &str, evidence: Vec<String>) -> Judgment {
    Judgment {
        outcome,
        answer,
        observed: observed.into(),
        evidence,
    }
}

fn held(observed: &[&str]) -> Posted {
    Posted {
        judgments: observed
            .iter()
            .enumerate()
            .map(|(i, o)| judgment(i, Answer::Held, o, vec![]))
            .collect(),
        side_finds: vec![],
        notes: None,
        probed: None,
    }
}

/// One attempt: dispatched, posted `minutes` later, then the device's health.
fn post(
    run: &mut Run,
    t: &mut Clock,
    spec: &str,
    platform: Platform,
    posted: Posted,
    minutes: u64,
) -> Result<()> {
    let token = run.dispatch(t.tick(0), spec, platform, |_, _| "brief".into())?;
    let attempt = run.row(spec, platform).map_or(1, |r| r.attempts);
    run.post(t.tick(minutes), &token, posted)?;
    let mut health = Health::default();
    health.measure("memory 58% free".into(), false);
    health.measure("launch 1.1 s".into(), false);
    run.device_health(t.tick(0), spec, platform, attempt, health)?;
    Ok(())
}
