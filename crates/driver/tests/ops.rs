use soundcheck_driver::device::Result;
use soundcheck_driver::{Device, Driver, DriverError, Element, Matcher, Platform, Rect};
use std::path::Path;
use std::sync::Mutex;

/// Serves screens in order (the last one stays) and records every input.
struct Fake {
    platform: Platform,
    screens: Mutex<Vec<Vec<Element>>>,
    actions: Mutex<Vec<String>>,
    /// What typing puts in the focused field; `None` drops it, like a lost keystroke.
    typed: Mutex<Vec<Option<String>>>,
}

impl Fake {
    fn new(screens: Vec<Vec<Element>>) -> Self {
        Fake {
            platform: Platform::Android,
            screens: Mutex::new(screens),
            actions: Mutex::new(Vec::new()),
            typed: Mutex::new(Vec::new()),
        }
    }
    fn log(&self, action: String) {
        self.actions.lock().unwrap().push(action);
    }
}

impl Device for Fake {
    fn platform(&self) -> Platform {
        self.platform
    }
    async fn read(&self, _all: bool) -> Result<Vec<Element>> {
        let mut screens = self.screens.lock().unwrap();
        Ok(if screens.len() > 1 {
            screens.remove(0)
        } else {
            screens[0].clone()
        })
    }
    async fn tap_at(&self, x: f64, y: f64) -> Result<()> {
        self.log(format!("tap {x},{y}"));
        Ok(())
    }
    async fn hold_at(&self, x: f64, y: f64, ms: u64) -> Result<()> {
        self.log(format!("hold {x},{y} {ms}"));
        Ok(())
    }
    async fn drag(&self, from: (f64, f64), to: (f64, f64), hold_ms: u64) -> Result<()> {
        self.log(format!("drag {from:?} {to:?} {hold_ms}"));
        Ok(())
    }
    async fn swipe(&self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        self.log(format!("swipe {from:?} {to:?} {ms}"));
        Ok(())
    }
    async fn type_text(&self, text: &str) -> Result<()> {
        self.log(format!("type {text}"));
        let landed = self
            .typed
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(Some(text.into()));
        let mut screens = self.screens.lock().unwrap();
        let screen = screens.last_mut().unwrap();
        if let Some(field) = screen
            .iter_mut()
            .find(|e| e.role.as_deref() == Some("field"))
        {
            field.value = landed;
        }
        Ok(())
    }
    async fn clear(&self, chars: usize) -> Result<()> {
        self.log(format!("clear {chars}"));
        Ok(())
    }
    async fn launch(&self) -> Result<()> {
        self.log("launch".into());
        Ok(())
    }
    async fn screenshot(&self, _path: &Path) -> Result<()> {
        Ok(())
    }
    async fn errors(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }
}

fn el(label: &str, x: f64) -> Element {
    Element {
        label: label.into(),
        frame: Rect {
            x,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        },
        enabled: true,
        ..Element::default()
    }
}

fn m(target: &str) -> Matcher {
    Matcher::parse(target).unwrap()
}

#[tokio::test(start_paused = true)]
async fn tap_at_a_point_taps_it_without_reading_the_screen() {
    let driver = Driver::new(Fake::new(vec![]));
    let tapped = driver.tap(&m("at:90,69"), false).await.unwrap();
    assert_eq!(tapped.label, "at:90,69");
    assert_eq!(*driver.device.actions.lock().unwrap(), ["tap 90,69"]);
}

#[test]
fn a_point_needs_two_numbers() {
    assert!(Matcher::parse("at:90").is_err());
    assert!(Matcher::parse("at:x,69").is_err());
    assert!(Matcher::parse("at: 90, 69.5").is_ok());
}

#[tokio::test(start_paused = true)]
async fn tap_waits_for_the_element_then_taps_its_centre() {
    let driver = Driver::new(Fake::new(vec![vec![], vec![], vec![el("Save", 100.0)]]));
    let tapped = driver.tap(&m("Save"), false).await.unwrap();
    assert_eq!(tapped.label, "Save");
    assert_eq!(*driver.device.actions.lock().unwrap(), ["tap 105,5"]);
}

#[tokio::test(start_paused = true)]
async fn last_takes_the_last_match() {
    let driver = Driver::new(Fake::new(vec![vec![
        el("Revoke", 0.0),
        el("Revoke", 200.0),
    ]]));
    driver.tap(&m("Revoke"), true).await.unwrap();
    assert_eq!(*driver.device.actions.lock().unwrap(), ["tap 205,5"]);
}

#[tokio::test(start_paused = true)]
async fn a_missing_element_times_out_without_tapping() {
    let driver = Driver::new(Fake::new(vec![vec![el("Cancel", 0.0)]]));
    let err = driver.tap(&m("Save"), false).await.unwrap_err();
    assert!(matches!(err, DriverError::NotFound(_)));
    assert!(driver.device.actions.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn check_reads_once_for_every_assertion() {
    let driver = Driver::new(Fake::new(vec![
        vec![el("Undo", 0.0)],
        vec![el("Redo", 0.0)],
    ]));
    let lines = driver
        .check(&["Undo".into(), "!Redo".into(), "Redo".into()])
        .await
        .unwrap();
    let oks: Vec<bool> = lines.iter().map(|l| l.ok).collect();
    assert_eq!(
        oks,
        [true, true, false],
        "all three judged on the first screen"
    );
}

#[tokio::test(start_paused = true)]
async fn gone_is_exact_by_default() {
    let driver = Driver::new(Fake::new(vec![vec![el("Shuffle Order…", 0.0)]]));
    driver.gone(&m("Shuffle Order")).await.unwrap();
    assert!(driver.gone(&m("prefix:Shuffle Order")).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn fill_retries_when_typing_drops_characters() {
    let mut field = el("Title", 0.0);
    field.role = Some("field".into());
    field.value = Some("Old title".into());
    let driver = Driver::new(Fake::new(vec![vec![field]]));
    // The first attempt loses keystrokes; the second lands.
    driver
        .device
        .typed
        .lock()
        .unwrap()
        .extend([Some("Opener".into()), Some("Opnr".into())]);
    driver.fill(&m("Title"), "Opener").await.unwrap();
    let actions = driver.device.actions.lock().unwrap();
    assert_eq!(actions.iter().filter(|a| a.starts_with("type")).count(), 2);
    assert!(
        actions.contains(&"clear 14".to_string()),
        "clears enough for the old value: {actions:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn absurd_values_are_refused_before_touching_the_device() {
    let driver = Driver::new(Fake::new(vec![vec![el("Back", 0.0)]]));
    let err = driver
        .wait(&m("Back"), 60_000_000)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("wait takes milliseconds"), "{err}");
    assert!(driver.tap_burst(&m("Back"), 500, 100).await.is_err());
    assert!(driver.tap_burst(&m("Back"), 5, 1).await.is_err());
    assert!(driver.network_drop(0).await.is_err());
    assert!(driver.device.actions.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn tap_burst_taps_count_times() {
    let driver = Driver::new(Fake::new(vec![vec![el("Back", 0.0)]]));
    driver.tap_burst(&m("Back"), 5, 100).await.unwrap();
    assert_eq!(driver.device.actions.lock().unwrap().len(), 5);
}

#[tokio::test(start_paused = true)]
async fn tap_burst_takes_a_point() {
    let driver = Driver::new(Fake::new(vec![]));
    driver.tap_burst(&m("at:516,700"), 2, 80).await.unwrap();
    assert_eq!(
        *driver.device.actions.lock().unwrap(),
        ["tap 516,700", "tap 516,700"]
    );
}

#[tokio::test(start_paused = true)]
async fn a_platform_without_a_tool_refuses_it_clearly() {
    let driver = Driver::new(Fake::new(vec![vec![]]));
    let err = driver.network_drop(30).await.unwrap_err().to_string();
    assert_eq!(err, "network-drop isn't available on android");
    let err = driver
        .device
        .goto("http://localhost")
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(err, "goto isn't available on android");
}
