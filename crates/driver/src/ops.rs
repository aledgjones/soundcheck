use crate::device::{Device, DriverError, Result};
use crate::element::Element;
use crate::matcher::Matcher;
use std::time::Duration;
use tokio::time::{Instant, sleep};

/// Milliseconds everywhere, and anything outside these is refused.
pub const WAIT_MAX_MS: u64 = 120_000;
pub const TAP_WAIT_MS: u64 = 5_000;
pub const GESTURE_MAX_MS: u64 = 10_000;
pub const BURST_MAX: u32 = 50;
pub const BURST_INTERVAL_MS: (u64, u64) = (10, 5_000);
pub const NETWORK_DROP_MAX_S: u64 = 300;
pub const STRESS_MAX_S: u64 = 600;
const POLL_MS: u64 = 250;
const FILL_TRIES: u32 = 3;

fn within(what: &str, value: u64, min: u64, max: u64, unit: &str) -> Result<()> {
    if value < min || value > max {
        return Err(DriverError::Invalid(format!(
            "{what} takes {unit}: {value} is outside {min}–{max}"
        )));
    }
    Ok(())
}

/// One assertion's outcome from [`Driver::check`].
#[derive(Debug, Clone, PartialEq)]
pub struct CheckLine {
    /// As given: `A` must be on screen, `!A` must not.
    pub assertion: String,
    pub ok: bool,
}

/// The operations every platform shares, built on a [`Device`]'s raw reads and inputs.
pub struct Driver<D: Device> {
    pub device: D,
}

impl<D: Device> Driver<D> {
    pub fn new(device: D) -> Self {
        Driver { device }
    }

    pub async fn see(&self, filter: Option<&Matcher>, all: bool) -> Result<Vec<Element>> {
        let elements = self.device.read(all).await?;
        Ok(match filter {
            Some(m) => elements.into_iter().filter(|e| m.matches(e)).collect(),
            None => elements,
        })
    }

    pub async fn expect(&self, target: &Matcher) -> Result<Element> {
        self.find(target, false)
            .await?
            .ok_or_else(|| DriverError::NotFound(target.to_string()))
    }

    pub async fn gone(&self, target: &Matcher) -> Result<()> {
        match self.find(target, false).await? {
            Some(e) => Err(DriverError::Invalid(format!(
                "still on screen: {}",
                e.line()
            ))),
            None => Ok(()),
        }
    }

    /// Several assertions on one read: `A` must be on screen, `!A` must not.
    pub async fn check(&self, assertions: &[String]) -> Result<Vec<CheckLine>> {
        let parsed: Vec<(bool, Matcher, &String)> = assertions
            .iter()
            .map(|a| {
                let (absent, target) = a
                    .strip_prefix('!')
                    .map_or((false, a.as_str()), |t| (true, t));
                Matcher::parse(target)
                    .map(|m| (absent, m, a))
                    .map_err(|e| DriverError::Invalid(format!("{a}: {e}")))
            })
            .collect::<Result<_>>()?;
        let elements = self.device.read(false).await?;
        Ok(parsed
            .into_iter()
            .map(|(absent, m, a)| CheckLine {
                assertion: a.clone(),
                ok: elements.iter().any(|e| m.matches(e)) != absent,
            })
            .collect())
    }

    pub async fn wait(&self, target: &Matcher, timeout_ms: u64) -> Result<Element> {
        within("wait", timeout_ms, 0, WAIT_MAX_MS, "milliseconds")?;
        self.wait_for(target, timeout_ms, false).await
    }

    /// Waits up to 5 s for the element, then taps its centre. `last` takes the last match: a system alert's button
    /// can share its label with the row that opened it.
    pub async fn tap(&self, target: &Matcher, last: bool) -> Result<Element> {
        let e = self.wait_for(target, TAP_WAIT_MS, last).await?;
        let (x, y) = e.tap_point();
        self.device.tap_at(x, y).await?;
        Ok(e)
    }

    pub async fn hold(&self, target: &Matcher, ms: u64) -> Result<Element> {
        within("hold", ms, 1, GESTURE_MAX_MS, "milliseconds")?;
        let e = self.wait_for(target, TAP_WAIT_MS, false).await?;
        let (x, y) = e.tap_point();
        self.device.hold_at(x, y, ms).await?;
        Ok(e)
    }

    pub async fn drag(&self, from: (f64, f64), to: (f64, f64), hold_ms: u64) -> Result<()> {
        within("drag's hold", hold_ms, 0, GESTURE_MAX_MS, "milliseconds")?;
        self.device.drag(from, to, hold_ms).await
    }

    pub async fn swipe(&self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        within("swipe", ms, 1, GESTURE_MAX_MS, "milliseconds")?;
        self.device.swipe(from, to, ms).await
    }

    /// Taps the field, empties it, types `text` and checks it landed; typing can drop characters, so it tries 3 times.
    pub async fn fill(&self, target: &Matcher, text: &str) -> Result<()> {
        for _ in 0..FILL_TRIES {
            let field = self.wait_for(target, TAP_WAIT_MS, false).await?;
            let (x, y) = field.tap_point();
            self.device.tap_at(x, y).await?;
            let held = field.value.as_deref().map_or(0, |v| v.chars().count());
            self.device
                .clear(held.max(text.chars().count()) + 5)
                .await?;
            self.device.type_text(text).await?;
            let landed = self
                .device
                .read(false)
                .await?
                .iter()
                .any(|e| e.value.as_deref() == Some(text) || e.label == text);
            if landed {
                return Ok(());
            }
        }
        Err(DriverError::Invalid(format!(
            "{text:?} didn't land in {target} after {FILL_TRIES} tries"
        )))
    }

    pub async fn tap_burst(
        &self,
        target: &Matcher,
        count: u32,
        interval_ms: u64,
    ) -> Result<Element> {
        within(
            "tap-burst's count",
            count.into(),
            1,
            BURST_MAX.into(),
            "a count",
        )?;
        within(
            "tap-burst's interval",
            interval_ms,
            BURST_INTERVAL_MS.0,
            BURST_INTERVAL_MS.1,
            "milliseconds",
        )?;
        let e = self.wait_for(target, TAP_WAIT_MS, false).await?;
        let (x, y) = e.tap_point();
        self.device.tap_burst(x, y, count, interval_ms).await?;
        Ok(e)
    }

    pub async fn network_drop(&self, seconds: u64) -> Result<()> {
        within("network-drop", seconds, 1, NETWORK_DROP_MAX_S, "seconds")?;
        self.device.network_drop(seconds).await
    }

    pub async fn stress(&self, load: u32, seconds: u64) -> Result<()> {
        within("stress's load", load.into(), 1, 100, "a percentage")?;
        within("stress", seconds, 1, STRESS_MAX_S, "seconds")?;
        self.device.stress(load, seconds).await
    }

    async fn find(&self, target: &Matcher, last: bool) -> Result<Option<Element>> {
        let elements = self.device.read(false).await?;
        let mut matching = elements.into_iter().filter(|e| target.matches(e));
        Ok(if last {
            matching.next_back()
        } else {
            matching.next()
        })
    }

    async fn wait_for(&self, target: &Matcher, timeout_ms: u64, last: bool) -> Result<Element> {
        let end = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            // A read can fail for a moment while an animation runs; keep trying until the deadline.
            if let Ok(Some(e)) = self.find(target, last).await {
                return Ok(e);
            }
            if Instant::now() >= end {
                return Err(DriverError::NotFound(target.to_string()));
            }
            sleep(Duration::from_millis(POLL_MS)).await;
        }
    }
}
