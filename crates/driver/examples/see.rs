//! Prints what the Android driver reads on the emulator now, as `see` would.
//!   cargo run -p soundcheck-driver --example see -- <package> [--all] [--launch]

use soundcheck_driver::android::Android;
use soundcheck_driver::{Device, Driver};
use std::path::Path;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let package = args
        .first()
        .expect("usage: see <package> [--all] [--launch]");
    let apk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../readers/android/build/reader.apk");
    let driver = Driver::new(Android::connect(package, Some(apk)).await.expect("connect"));
    if args.iter().any(|a| a == "--launch") {
        driver.device.launch().await.expect("launch");
    }
    for e in driver
        .see(None, args.iter().any(|a| a == "--all"))
        .await
        .expect("read")
    {
        let (x, y) = e.tap_point();
        println!("{x:.0},{y:.0}\t{}", e.line());
    }
}
