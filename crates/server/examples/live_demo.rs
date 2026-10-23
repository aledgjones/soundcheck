//! A live walk through a run's states on the real page, with no devices: `cargo run --example live_demo`, then watch
//! http://127.0.0.1:7401/. Each step is what a tester or the coordinator would do, a few seconds apart.

use anyhow::Result;
use soundcheck::run::model::{Answer, Call, Cause, Final, Judgment, Posted};
use soundcheck::run::{Run, saved};
use soundcheck::serve::{self, Hub};
use soundcheck::suite::{self, config::Platform};
use std::path::Path;
use std::time::Duration;

const PORT: u16 = 7401;

#[tokio::main]
async fn main() -> Result<()> {
    let demo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/demo");
    let suite = suite::load(&demo.join("suite"));
    let run_dir = demo.join("reports/live-demo");
    let _ = std::fs::remove_dir_all(&run_dir);
    std::fs::create_dir_all(&run_dir)?;
    let platforms = vec![Platform::Android, Platform::Ios];
    let label = "v3.5.0 · live demo";
    let info = saved::RunInfo {
        suite: demo.join("suite"),
        label: label.into(),
        platforms: platforms.clone(),
        scope: vec!["items".into(), "sharing".into()],
    };
    saved::save(&run_dir, &info, &suite)?;
    let plan = saved::plan_for(&suite, &platforms, &info.scope)?;
    let run = Run::open(plan, &run_dir.join("journal.jsonl"), serve::now_ms())?;
    let hub = Hub::without_devices(suite, run, label.into(), run_dir, 600_000, platforms);
    let server = serve::start(hub.clone(), PORT).await?;
    println!("live page at http://{}/", server.addr);
    let _ = std::process::Command::new("open")
        .arg(format!("http://{}/", server.addr))
        .status();

    let say = |text: &str| println!("{text}");
    let wait = |seconds: u64| tokio::time::sleep(Duration::from_secs(seconds));
    wait(6).await;

    say("1. A spec starts: its cell turns blue, Running.");
    let t = dispatch(&hub, "items/001-add-items", Platform::Android)?;
    wait(5).await;
    say("2. The tester settles the first outcome: it fills in with a ✓.");
    judge(
        &hub,
        &t,
        0,
        Answer::Held,
        "Lemons, Rice and Coffee appeared in turn above the checked items.",
    )?;
    wait(5).await;
    say("3. And the second.");
    judge(
        &hub,
        &t,
        1,
        Answer::Held,
        "The keyboard stayed up and the bar kept focus after each item.",
    )?;
    wait(4).await;
    say("4. It posts: a pass files at once.");
    post(&hub, &t)?;
    wait(5).await;

    say(
        "5. Next spec. One outcome holds, then one doesn't: an orange ?, since nothing is decided yet.",
    );
    let t = dispatch(&hub, "items/003-reorder", Platform::Android)?;
    wait(4).await;
    judge(
        &hub,
        &t,
        0,
        Answer::Held,
        "Bread followed the finger to the top while the rows below shifted down.",
    )?;
    wait(5).await;
    judge(
        &hub,
        &t,
        1,
        Answer::Failed,
        "After relaunching, Bread was back in third place.",
    )?;
    wait(5).await;
    say(
        "6. It posts a FAIL: the cell turns to an orange ?, Checking, until the coordinator's call.",
    );
    post(&hub, &t)?;
    wait(7).await;
    say("7. The coordinator wants it reproduced: Queued, with what it's reproducing.");
    review(
        &hub,
        "items/003-reorder",
        Platform::Android,
        Call::Verify {
            claim: "The new order is lost after a relaunch".into(),
        },
        "Seen once: reproduce it on a fresh attempt",
    )?;
    wait(7).await;
    say("8. The second attempt runs and sees the same.");
    let t = dispatch(&hub, "items/003-reorder", Platform::Android)?;
    wait(4).await;
    judge(
        &hub,
        &t,
        0,
        Answer::Held,
        "Bread followed the finger to the top again.",
    )?;
    wait(3).await;
    judge(
        &hub,
        &t,
        1,
        Answer::Failed,
        "After relaunching, Bread was back in third place again.",
    )?;
    wait(3).await;
    post(&hub, &t)?;
    wait(5).await;
    say("9. The coordinator upholds it: now it's red, Failed, with the call in its red box.");
    review(
        &hub,
        "items/003-reorder",
        Platform::Android,
        Call::Uphold {
            title: Some("A reordered list goes back to its old order after a relaunch".into()),
            cause: None,
        },
        "Reproduced on a second attempt",
    )?;
    wait(7).await;

    say("10. An outcome the tester can't observe: an orange ? while it's checked.");
    let t = dispatch(&hub, "sharing/002-leave", Platform::Android)?;
    wait(4).await;
    judge(
        &hub,
        &t,
        0,
        Answer::CouldntCheck,
        "Signing in as Sam asked for a code sent to a phone the test account doesn't have.",
    )?;
    wait(3).await;
    judge(
        &hub,
        &t,
        1,
        Answer::CouldntCheck,
        "Never got past Sam's sign-in.",
    )?;
    wait(3).await;
    post(&hub, &t)?;
    wait(6).await;
    say("11. The coordinator puts it down to the harness: purple, Blocked.");
    review(
        &hub,
        "sharing/002-leave",
        Platform::Android,
        Call::Uphold {
            title: None,
            cause: Some(Cause::Harness),
        },
        "The second test account needs two-step sign-in turned off; not the app",
    )?;
    wait(7).await;

    say("12. An Unsure: the tester can't tell if what it saw is right.");
    let t = dispatch(&hub, "items/004-quantities", Platform::Ios)?;
    wait(4).await;
    judge(&hub, &t, 0, Answer::Held, "The row read Eggs × 12.")?;
    wait(3).await;
    judge(
        &hub,
        &t,
        1,
        Answer::Unsure,
        "At 1 the minus button stayed enabled, but tapping it left the count at 1.",
    )?;
    wait(3).await;
    post(&hub, &t)?;
    wait(6).await;
    say("13. The coordinator judges it passed: a ✓, with how it was settled.");
    review(
        &hub,
        "items/004-quantities",
        Platform::Ios,
        Call::Judge {
            result: Final::Pass,
            title: None,
        },
        "The count never goes below 1, which is the outcome",
    )?;
    say("Done. The page stays up for ten minutes; Ctrl-C ends it.");
    wait(600).await;
    server.shutdown.cancel();
    Ok(())
}

fn dispatch(hub: &Hub, spec: &str, platform: Platform) -> Result<String> {
    hub.with_run(|run, now| run.dispatch(now, spec, platform, |_, _| "brief".into()))
}

fn judge(hub: &Hub, token: &str, outcome: usize, answer: Answer, observed: &str) -> Result<()> {
    let judgment = Judgment {
        outcome,
        answer,
        observed: observed.into(),
        evidence: vec![],
    };
    hub.with_run(|run, now| run.judge(now, token, judgment).map(|_| ()))
}

fn post(hub: &Hub, token: &str) -> Result<()> {
    let rest = Posted {
        judgments: vec![],
        side_finds: vec![],
        notes: None,
        probed: None,
    };
    hub.with_run(|run, now| run.post(now, token, rest).map(|_| ()))
}

fn review(hub: &Hub, spec: &str, platform: Platform, call: Call, reason: &str) -> Result<()> {
    hub.with_run(|run, now| run.review(now, spec, platform, call, reason))
}
