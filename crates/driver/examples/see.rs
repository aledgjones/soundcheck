//! Prints what a driver reads on the emulator or simulator now, as `see` would.
//!   cargo run -p soundcheck-driver --example see -- [--ios] <app id> [--all] [--launch]

use soundcheck_driver::android::Android;
use soundcheck_driver::any::AnyDevice;
use soundcheck_driver::ios::Ios;
use soundcheck_driver::{Device, Driver};
use std::path::Path;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let app = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .expect("usage: see [--ios] <app id> [--all] [--launch]");
    let readers = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../readers");
    let device = if flag("--ios") {
        AnyDevice::Ios(
            Ios::connect(app, Some(readers.join("ios/build/axe-reader")))
                .await
                .expect("connect"),
        )
    } else {
        AnyDevice::Android(
            Android::connect(app, Some(readers.join("android/build/reader.apk")))
                .await
                .expect("connect"),
        )
    };
    let driver = Driver::new(device);
    if flag("--launch") {
        driver.device.launch().await.expect("launch");
    }
    for e in driver.see(None, flag("--all")).await.expect("read") {
        let (x, y) = e.tap_point();
        println!("{x:.0},{y:.0}\t{}", e.line());
    }
}
