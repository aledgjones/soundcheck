//! Drives the booted iOS simulator with its own Settings app. Skips where no simulator is booted or the reader isn't built.

use soundcheck_driver::ios::Ios;
use soundcheck_driver::{Device, Driver, Matcher};
use std::collections::BTreeSet;
use std::path::Path;

fn m(target: &str) -> Matcher {
    Matcher::parse(target).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ios_driver_reads_and_drives_settings() {
    let reader = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../readers/ios/build/reader");
    if !reader.exists() {
        eprintln!("skipping: no iOS reader built (readers/ios/build.sh)");
        return;
    }
    let ios = match Ios::connect("com.apple.Preferences", Some(reader)).await {
        Ok(i) => i,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let driver = Driver::new(ios);
    driver.device.launch().await.unwrap();
    driver.wait(&m("General"), 15_000).await.unwrap();

    let reader: BTreeSet<String> = driver
        .see(None, true)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.label)
        .collect();
    let voiceover: BTreeSet<String> = driver
        .see(None, false)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.label)
        .collect();
    assert!(voiceover.is_subset(&reader) && voiceover.contains("General"));

    driver.tap(&m("General"), false).await.unwrap();
    driver.wait(&m("About"), 10_000).await.unwrap();
    driver.device.launch().await.unwrap();
    driver.wait(&m("General"), 15_000).await.unwrap();

    let health = driver.device.health().await.unwrap();
    for fact in ["Mac memory", "Mac CPU", "launch"] {
        assert!(
            health.facts.iter().any(|f| f.starts_with(fact)),
            "no {fact} in {:?}",
            health.facts
        );
    }

    let (w, h) = driver.device.screen_size().await.unwrap();
    assert!(w > 100.0 && h > 100.0);
    assert!(driver.device.errors().await.unwrap().is_empty());
    let shot = std::env::temp_dir().join("soundcheck-ios.png");
    driver.device.screenshot(&shot).await.unwrap();
    assert!(std::fs::metadata(&shot).unwrap().len() > 1000);
}

/// Reboots the simulator: run by hand with `--ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn restart_reboots_the_simulator_and_reads_again() {
    let reader = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../readers/ios/build/reader");
    let ios = Ios::connect("com.apple.Preferences", reader.exists().then_some(reader))
        .await
        .unwrap();
    ios.restart().await.unwrap();
    let driver = Driver::new(ios);
    driver.device.launch().await.unwrap();
    driver.wait(&m("General"), 30_000).await.unwrap();
}
