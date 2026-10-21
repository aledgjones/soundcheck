//! Drives the running Android emulator with Android's own Settings app. Skips where no emulator is running.

use soundcheck_driver::android::Android;
use soundcheck_driver::{Device, Driver, Matcher};
use std::collections::BTreeSet;
use std::path::Path;

fn m(target: &str) -> Matcher {
    Matcher::parse(target).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_android_driver_reads_and_drives_settings() {
    let apk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../readers/android/build/reader.apk");
    let android = match Android::connect("com.android.settings", Some(apk)).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let driver = Driver::new(android);
    driver.device.launch().await.unwrap();
    driver.wait(&m("Network & internet"), 10_000).await.unwrap();

    // The reader against uiautomator on the same screen: every element uiautomator lists, the reader lists too.
    let reader: BTreeSet<String> = driver
        .see(None, true)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.label)
        .collect();
    let uiautomator: BTreeSet<String> = driver
        .device
        .read_uiautomator()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.label)
        .collect();
    let missing: Vec<_> = uiautomator.difference(&reader).collect();
    assert!(missing.is_empty(), "the reader misses {missing:?}");
    // The screen-reader view is a subset.
    let talkback: BTreeSet<String> = driver
        .see(None, false)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.label)
        .collect();
    assert!(talkback.is_subset(&reader) && talkback.contains("Network & internet"));

    driver.tap(&m("Network & internet"), false).await.unwrap();
    driver.wait(&m("Internet"), 10_000).await.unwrap();
    driver.device.press("Back").await.unwrap();
    driver.wait(&m("Network & internet"), 10_000).await.unwrap();

    let (w, h) = driver.device.screen_size().await.unwrap();
    assert!(w > 100.0 && h > 100.0);
    assert!(driver.device.density().await >= 1.0);
    assert!(!driver.device.keyboard_shown().await.unwrap());
    let shot = std::env::temp_dir().join("soundcheck-android.png");
    driver.device.screenshot(&shot).await.unwrap();
    assert!(std::fs::metadata(&shot).unwrap().len() > 1000);
}
