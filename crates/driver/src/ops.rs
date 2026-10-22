use crate::compare::{Comparison, Image, PixelBox, changed_box, colour_box};
use crate::device::{Device, DriverError, Result};
use crate::element::Element;
use crate::element::Rect;
use crate::matcher::Matcher;
use std::path::Path;
use std::time::Duration;
use tokio::time::{Instant, sleep};

/// Milliseconds everywhere, and anything outside these is refused.
pub const WAIT_MAX_MS: u64 = 120_000;
pub const TAP_WAIT_MS: u64 = 5_000;
pub const GESTURE_MAX_MS: u64 = 10_000;
pub const NETWORK_DROP_MAX_S: u64 = 300;
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

/// What [`Driver::preview_compare`] places and how it compares.
#[derive(Debug, Clone)]
pub struct CompareRequest {
    pub from: (f64, f64),
    pub to: Option<(f64, f64)>,
    pub commit: Option<Matcher>,
    pub colour: Option<[u8; 3]>,
    pub region: Option<Rect>,
    /// Screenshot pixels an edge may differ by: antialiasing moves a pixel, a real offset more.
    pub tolerance: i64,
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
    /// The proxy the app reaches its services through, when the suite configures one; otherwise the network
    /// tools fall to the device's own switch (web).
    pub network: Option<crate::proxy::Network>,
}

impl<D: Device> Driver<D> {
    pub fn new(device: D) -> Self {
        Driver {
            device,
            network: None,
        }
    }

    pub fn with_network(self, network: Option<crate::proxy::Network>) -> Self {
        Driver { network, ..self }
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
        let e = self.target(target, last).await?;
        let (x, y) = e.tap_point();
        self.device.tap_at(x, y).await?;
        Ok(e)
    }

    pub async fn hold(&self, target: &Matcher, ms: u64) -> Result<Element> {
        within("hold", ms, 1, GESTURE_MAX_MS, "milliseconds")?;
        let e = self.target(target, false).await?;
        let (x, y) = e.tap_point();
        self.device.hold_at(x, y, ms).await?;
        Ok(e)
    }

    /// The element to touch: a point names itself, anything else is waited for up to 5 s.
    async fn target(&self, target: &Matcher, last: bool) -> Result<Element> {
        if let Matcher::Point(x, y) = *target {
            return Ok(Element {
                label: format!("at:{x},{y}"),
                frame: crate::element::Rect {
                    x,
                    y,
                    ..Default::default()
                },
                enabled: true,
                ..Default::default()
            });
        }
        self.wait_for(target, TAP_WAIT_MS, last).await
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
            // At the right end, so the cursor lands after the text: an on-screen keyboard only deletes backwards.
            let f = field.frame;
            let x = (f.x + f.width - 8.0).max(field.tap_point().0);
            self.device.tap_at(x, f.y + f.height / 2.0).await?;
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
                // Case aside: the system capitalises a field's words or sentences as a person types, too.
                .any(|e| {
                    e.value
                        .as_deref()
                        .is_some_and(|v| v.eq_ignore_ascii_case(text))
                        || e.label.eq_ignore_ascii_case(text)
                });
            if landed {
                return Ok(());
            }
        }
        Err(DriverError::Invalid(format!(
            "{text:?} didn't land in {target} after {FILL_TRIES} tries"
        )))
    }

    pub async fn double_tap(&self, target: &Matcher, last: bool) -> Result<Element> {
        let e = self.target(target, last).await?;
        let (x, y) = e.tap_point();
        self.device.double_tap(x, y).await?;
        Ok(e)
    }

    pub async fn network_drop(&self, seconds: u64) -> Result<()> {
        within("network-drop", seconds, 1, NETWORK_DROP_MAX_S, "seconds")?;
        match &self.network {
            Some(n) => n.drop_for(seconds).await,
            None => self.device.network_drop(seconds).await,
        }
    }

    /// Takes the app offline (connections refused, as in airplane mode) until called with `false`.
    pub async fn offline(&self, on: bool) -> Result<()> {
        match &self.network {
            Some(n) => n.offline(on).await,
            None => self.device.offline(on).await,
        }
    }

    /// Places with a held touch from `from` (to `to` for a line or stroke), screenshots the preview while held,
    /// releases, screenshots what was saved, and compares the two inside a region around the touch, so a magnifier
    /// above it stays out. With `commit`, nothing is held: the preview is what's on screen now, and tapping `commit`
    /// saves it (a text box's Done); `region` then bounds the comparison. With `colour`, only pixels near that colour
    /// count, for text on a tinted box. Screenshots are written to `dir` as `<name>-before|preview|saved.png`.
    pub async fn preview_compare(
        &self,
        req: &CompareRequest,
        dir: &Path,
        name: &str,
    ) -> Result<Comparison> {
        let file = |stage: &str| dir.join(format!("{name}-{stage}.png"));
        std::fs::create_dir_all(dir).map_err(|e| DriverError::Device(e.to_string()))?;
        self.device.screenshot(&file("before")).await?;
        let before = Image::load(&file("before"))?;
        let (units_w, _) = self.device.screen_size().await?;
        let scale = before.width as f64 / units_w;
        let pad = 40.0 * self.device.density().await;
        let to = req.to.unwrap_or(req.from);
        let region_units = match (&req.commit, req.region) {
            (_, Some(r)) => r,
            (None, None) => Rect {
                x: req.from.0.min(to.0) - pad,
                y: req.from.1.min(to.1) - pad,
                width: (req.from.0 - to.0).abs() + 2.0 * pad,
                height: (req.from.1 - to.1).abs() + 2.0 * pad,
            },
            (Some(_), None) => Rect {
                x: 0.0,
                y: 0.0,
                width: units_w,
                height: before.height as f64 / scale,
            },
        };
        match &req.commit {
            None => {
                self.device.touch_down(req.from.0, req.from.1).await?;
                sleep(Duration::from_millis(300)).await;
                let steps = (((to.0 - req.from.0).hypot(to.1 - req.from.1)) / 20.0)
                    .ceil()
                    .max(1.0) as u32;
                for i in 1..=steps {
                    let t = f64::from(i) / f64::from(steps);
                    self.device
                        .touch_move(
                            req.from.0 + (to.0 - req.from.0) * t,
                            req.from.1 + (to.1 - req.from.1) * t,
                        )
                        .await?;
                }
                sleep(Duration::from_millis(600)).await;
                self.device.screenshot(&file("preview")).await?;
                self.device.touch_up(to.0, to.1).await?;
            }
            Some(commit) => {
                self.device.screenshot(&file("preview")).await?;
                self.tap(commit, false).await?;
            }
        }
        sleep(Duration::from_millis(1500)).await;
        self.device.screenshot(&file("saved")).await?;
        let (preview, saved) = (Image::load(&file("preview"))?, Image::load(&file("saved"))?);
        let region = PixelBox {
            x: (region_units.x * scale).round() as i64,
            y: (region_units.y * scale).round() as i64,
            w: (region_units.width * scale).round() as i64,
            h: (region_units.height * scale).round() as i64,
        };
        let (p, s) = match req.colour {
            Some(c) => (
                colour_box(&preview, region, c),
                colour_box(&saved, region, c),
            ),
            None => (
                changed_box(&before, &preview, region),
                changed_box(&before, &saved, region),
            ),
        };
        Ok(Comparison::new(p, s, req.tolerance))
    }

    /// Looks in what a screen reader gets first, then in everything `see` shows with `all`: a view marked not
    /// important (the score's page) is still a target by its id.
    async fn find(&self, target: &Matcher, last: bool) -> Result<Option<Element>> {
        for all in [false, true] {
            let elements = self.device.read(all).await?;
            let mut matching = elements.into_iter().filter(|e| target.matches(e));
            let found = if last {
                matching.next_back()
            } else {
                matching.next()
            };
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
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
