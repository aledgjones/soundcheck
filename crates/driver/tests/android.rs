//! Drives the running Android emulator with Android's own Settings app. Runs only with `SOUNDCHECK_DEVICE_TESTS=1`, and skips where no emulator is running.

use soundcheck_driver::android::Android;
use soundcheck_driver::proxy::{self, Route, Upstream};
use soundcheck_driver::{Device, Driver, Matcher};
use std::collections::BTreeSet;
use std::path::Path;

/// One emulator: the tests take turns on it.
static EMULATOR: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// These drive whatever emulator is running, which may be a live run's: they run only when asked for.
fn device_tests() -> bool {
    let on = std::env::var_os("SOUNDCHECK_DEVICE_TESTS").is_some_and(|v| v == "1");
    if !on {
        eprintln!("skipping: set SOUNDCHECK_DEVICE_TESTS=1 to drive the emulator");
    }
    on
}

fn m(target: &str) -> Matcher {
    Matcher::parse(target).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_android_driver_reads_and_drives_settings() {
    if !device_tests() {
        return;
    }
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
        .read_uiautomator(false)
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
async fn the_device_reaches_the_proxy_through_its_forward_and_loses_it_offline() {
    if !device_tests() {
        return;
    }
    let _turn = EMULATOR.lock().await;
    let android = match Android::connect("com.android.settings", None).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let echo = tokio::net::TcpListener::bind(("127.0.0.1", 18997))
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
    let routes = [Route {
        name: "echo".into(),
        listen: 18999,
        upstream: Upstream::Port(18997),
    }];
    let socket = proxy::socket_path(&routes);
    tokio::spawn({
        let socket = socket.clone();
        async move { proxy::serve(&routes, &socket).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let _android = android.forward(&[(18999, 18999)]).await.unwrap();
    assert!(adb(&["reverse", "--list"]).contains("tcp:18999 tcp:18999"));
    assert_eq!(echo_from_device(), "ping");

    assert_eq!(proxy::command(&socket, "offline").await.unwrap(), "offline");
    assert_eq!(echo_from_device(), "", "an offline device got through");
    assert_eq!(proxy::command(&socket, "online").await.unwrap(), "online");
    assert_eq!(echo_from_device(), "ping", "it didn't come back online");

    proxy::command(&socket, "stop").await.unwrap();
    adb(&["reverse", "--remove", "tcp:18999"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_measures_the_emulator_and_puts_back_a_lost_forward() {
    if !device_tests() {
        return;
    }
    let _turn = EMULATOR.lock().await;
    adb(&["reverse", "tcp:18998", "tcp:18998"]);
    let android = match Android::connect("com.android.settings", None).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("skipping: {e}");
            adb(&["reverse", "--remove", "tcp:18998"]);
            return;
        }
    };
    android.launch().await.unwrap();
    adb(&["reverse", "--remove", "tcp:18998"]);
    let health = android.health().await.unwrap();
    for fact in [
        "memory",
        "CPU",
        "launch",
        "restored the lost forward for port 18998",
    ] {
        assert!(
            health.facts.iter().any(|f| f.starts_with(fact)),
            "no {fact} in {:?}",
            health.facts
        );
    }
    assert!(adb(&["reverse", "--list"]).contains("tcp:18998 tcp:18998"));
    adb(&["reverse", "--remove", "tcp:18998"]);
    eprintln!("{health:?}");
}

/// Reboots the emulator: run by hand with `--ignored` and `SOUNDCHECK_DEVICE_TESTS=1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn restart_reboots_the_emulator_and_keeps_its_forwards() {
    if !device_tests() {
        return;
    }
    let _turn = EMULATOR.lock().await;
    let before = adb(&["reverse", "--list"]);
    let android = Android::connect("com.android.settings", None)
        .await
        .unwrap();
    android.restart().await.unwrap();
    assert_eq!(adb(&["reverse", "--list"]), before);
    android.launch().await.unwrap();
    let driver = Driver::new(android);
    driver
        .wait(&m("prefix:Network & internet"), 30_000)
        .await
        .unwrap();
}
