//! Drives the running Android emulator with Android's own Settings app. Skips where no emulator is running.

use soundcheck_driver::android::Android;
use soundcheck_driver::{Device, Driver, Matcher};
use std::collections::BTreeSet;
use std::path::Path;

/// One emulator: the tests take turns on it.
static EMULATOR: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn m(target: &str) -> Matcher {
    Matcher::parse(target).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_android_driver_reads_and_drives_settings() {
    let _turn = EMULATOR.lock().await;
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
    if driver
        .wait(&m("prefix:Network & internet"), 10_000)
        .await
        .is_err()
    {
        let seen: Vec<String> = driver
            .see(None, true)
            .await
            .unwrap()
            .iter()
            .map(|e| e.line())
            .collect();
        panic!("Settings didn't open on its home screen: {seen:?}");
    }

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

    driver
        .tap(&m("prefix:Network & internet"), false)
        .await
        .unwrap();
    driver.wait(&m("Internet"), 10_000).await.unwrap();
    driver.device.press("Back").await.unwrap();
    driver
        .wait(&m("prefix:Network & internet"), 10_000)
        .await
        .unwrap();

    let (w, h) = driver.device.screen_size().await.unwrap();
    assert!(w > 100.0 && h > 100.0);
    assert!(driver.device.density().await >= 1.0);
    assert!(!driver.device.keyboard_shown().await.unwrap());
    let shot = std::env::temp_dir().join("soundcheck-android.png");
    driver.device.screenshot(&shot).await.unwrap();
    assert!(std::fs::metadata(&shot).unwrap().len() > 1000);
}

fn adb(args: &[&str]) -> String {
    let out = std::process::Command::new("adb")
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Sends a line from the device to the host echo server and returns what comes back.
fn echo_from_device() -> String {
    adb(&["shell", "(echo ping; sleep 1) | nc -w 2 127.0.0.1 18999"])
        .trim()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn network_drop_freezes_the_device_connections_then_lets_new_ones_through() {
    let _turn = EMULATOR.lock().await;
    let android = match Android::connect("com.android.settings", None).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let echo = tokio::net::TcpListener::bind(("127.0.0.1", 18999))
        .await
        .unwrap();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = echo.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = c.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    adb(&["reverse", "tcp:18999", "tcp:18999"]);

    android.intercept_network(true).await.unwrap();
    assert!(adb(&["reverse", "--list"]).contains("tcp:18999 tcp:38999"));
    assert_eq!(echo_from_device(), "ping");

    let dropping = android.network_drop(5);
    let during = async {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        tokio::task::spawn_blocking(echo_from_device).await.unwrap()
    };
    let ((), during) = tokio::join!(async { dropping.await.unwrap() }, during);
    assert_eq!(during, "", "a connection made while dropped got through");
    assert_eq!(
        echo_from_device(),
        "ping",
        "new connections didn't come back"
    );

    android.intercept_network(false).await.unwrap();
    assert!(adb(&["reverse", "--list"]).contains("tcp:18999 tcp:18999"));
    adb(&["reverse", "--remove", "tcp:18999"]);
}
