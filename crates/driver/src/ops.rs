use crate::compare::{Comparison, Image, PixelBox, changed_box, colour_box};
use crate::device::{Device, DriverError, Result, Stop};
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
pub const TOUCH_MAX_STOPS: u64 = 20;
pub const NETWORK_DROP_MAX_S: u64 = 300;
const POLL_MS: u64 = 250;
/// Bringing an element scrolled out of view into it: at most this many swipes, each a person's page turn.
const SCROLL_PAGES_MAX: u32 = 5;
const SCROLL_SWIPE_MS: u64 = 300;
const SCROLL_SETTLE_READS: u32 = 12;
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

    /// [`Driver::expect`], read again until it holds or `timeout_ms` runs out.
    pub async fn expect_within(&self, target: &Matcher, timeout_ms: u64) -> Result<Element> {
        within("expect", timeout_ms, 0, WAIT_MAX_MS, "milliseconds")?;
        if timeout_ms == 0 {
            return self.expect(target).await;
        }
        self.wait_for(target, timeout_ms, false).await
    }

    /// [`Driver::gone`], read again until it holds or `timeout_ms` runs out.
    pub async fn gone_within(&self, target: &Matcher, timeout_ms: u64) -> Result<()> {
        within("gone", timeout_ms, 0, WAIT_MAX_MS, "milliseconds")?;
        let end = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match self.gone(target).await {
                Ok(()) => return Ok(()),
                Err(e) if matches!(e, DriverError::Restarted(_)) || Instant::now() >= end => {
                    return Err(e);
                }
                Err(_) => {}
            }
            sleep(Duration::from_millis(POLL_MS)).await;
        }
    }

    /// [`Driver::check`], read again until every assertion holds or `timeout_ms` runs out; returns the last read's.
    pub async fn check_within(
        &self,
        assertions: &[String],
        timeout_ms: u64,
    ) -> Result<Vec<CheckLine>> {
        within("check", timeout_ms, 0, WAIT_MAX_MS, "milliseconds")?;
        let end = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let lines = match self.check(assertions).await {
                Ok(lines) => Some(lines),
                Err(e @ (DriverError::Restarted(_) | DriverError::Invalid(_))) => return Err(e),
                Err(e) if Instant::now() >= end => return Err(e),
                Err(_) => None,
            };
            if let Some(lines) = lines
                && (lines.iter().all(|l| l.ok) || Instant::now() >= end)
            {
                return Ok(lines);
            }
            sleep(Duration::from_millis(POLL_MS)).await;
        }
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
        let mut e = self.wait_for(target, TAP_WAIT_MS, last).await?;
        // Scrolled out of view: swipe its scroll view toward it a page at a time, as VoiceOver does, then touch it.
        for _ in 0..SCROLL_PAGES_MAX {
            let Some(area) = e.scroll else {
                return Ok(e);
            };
            let (ex, ey) = e.frame.centre();
            let (ax, ay) = area.centre();
            let (near, far) = (0.2, 0.8);
            let (from, to) = if (ex - ax).abs() >= (ey - ay).abs() {
                let (a, b) = (area.x + area.width * near, area.x + area.width * far);
                if ex > ax {
                    ((b, ay), (a, ay))
                } else {
                    ((a, ay), (b, ay))
                }
            } else {
                let (a, b) = (area.y + area.height * near, area.y + area.height * far);
                if ey > ay {
                    ((ax, b), (ax, a))
                } else {
                    ((ax, a), (ax, b))
                }
            };
            self.device.swipe(from, to, SCROLL_SWIPE_MS).await?;
            // A tap on a list still gliding only stops it: wait until the element holds still.
            e = self.wait_for(target, TAP_WAIT_MS, last).await?;
            for _ in 0..SCROLL_SETTLE_READS {
                sleep(Duration::from_millis(POLL_MS)).await;
                let now = self.wait_for(target, TAP_WAIT_MS, last).await?;
                let still = now.frame == e.frame;
                e = now;
                if still {
                    break;
                }
            }
        }
        if e.scroll.is_some() {
            return Err(DriverError::Invalid(format!(
                "{target} stayed out of view after {SCROLL_PAGES_MAX} swipes of its scroll view"
            )));
        }
        Ok(e)
    }

    /// One finger through `path`, each stop an element or a point with how long to rest there: down at the first,
    /// moving at a finger's pace, lifted at the last, or with `keep` held there until `release`. Every element is
    /// found before the touch goes down, so nothing is read mid-gesture.
    pub async fn touch(&self, path: &[(Matcher, u64)], keep: bool) -> Result<Vec<Element>> {
        within(
            "touch's path",
            path.len() as u64,
            1,
            TOUCH_MAX_STOPS,
            "stops",
        )?;
        let mut elements = Vec::new();
        let mut stops = Vec::new();
        for (target, pause_ms) in path {
            within(
                "a stop's pause",
                *pause_ms,
                0,
                GESTURE_MAX_MS,
                "milliseconds",
            )?;
            let e = self.target(target, false).await?;
            let (x, y) = e.tap_point();
            stops.push(crate::device::Stop {
                x,
                y,
                pause_ms: *pause_ms,
            });
            elements.push(e);
        }
        self.device.touch(&stops, keep).await?;
        Ok(elements)
    }

    pub async fn release(&self, back: bool) -> Result<()> {
        self.device.release(back).await
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
            // Under load the deletes are still landing when typing starts, and the last few eat the first letters
            // typed: wait until the field holds still.
            let mut last = None;
            for _ in 0..10 {
                let now = self.find(target, false).await?.and_then(|e| e.value);
                if last.as_ref() == Some(&now) {
                    break;
                }
                last = Some(now);
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
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

    /// Types into the focused field and checks it landed: keys can drop under load, and a person would see it. A
    /// field whose value changed but doesn't hold the text is reported, not passed over.
    pub async fn type_checked(&self, text: &str) -> Result<()> {
        let values = |elements: Vec<Element>| -> Vec<(String, Option<String>)> {
            elements.into_iter().map(|e| (e.label, e.value)).collect()
        };
        let before = values(self.device.read(false).await?);
        self.device.type_text(text).await?;
        let typed = text.to_lowercase();
        let mut changed = None;
        for _ in 0..4 {
            let after = values(self.device.read(false).await?);
            if after.iter().any(|(label, value)| {
                value
                    .as_deref()
                    .is_some_and(|v| v.to_lowercase().contains(&typed))
                    || label.to_lowercase().contains(&typed)
            }) {
                return Ok(());
            }
            changed = after
                .into_iter()
                .find(|field| field.1.is_some() && !before.contains(field));
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        match changed {
            Some((label, Some(value))) => Err(DriverError::Invalid(format!(
                "typed {text:?} but {label} shows {value:?}: keys were dropped. `fill` sets a field's whole value and checks it"
            ))),
            // Nothing readable changed (a secure field, or no value exposed), so there's nothing to check against.
            _ => Ok(()),
        }
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

    /// Has the suite's proxy answer HTTP requests to `path` (`*` for any one segment) with `status` instead of
    /// passing them on, until cleared; returns every fault now set. Only the proxy can, so web refuses it.
    pub async fn fault(&self, path: &str, status: u16) -> Result<String> {
        match &self.network {
            Some(n) => n.fault(path, status).await,
            None => Err(self.device.unsupported("fault")),
        }
    }

    pub async fn fault_clear(&self) -> Result<()> {
        match &self.network {
            Some(n) => n.fault_clear().await,
            None => Err(self.device.unsupported("fault")),
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
                let stops = [
                    Stop {
                        x: req.from.0,
                        y: req.from.1,
                        pause_ms: 300,
                    },
                    Stop {
                        x: to.0,
                        y: to.1,
                        pause_ms: 0,
                    },
                ];
                self.device.touch(&stops, true).await?;
                sleep(Duration::from_millis(600)).await;
                self.device.screenshot(&file("preview")).await?;
                self.device.release(false).await?;
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
            // A read can fail for a moment while an animation runs; keep trying until the deadline. A restart can't
            // be waited out: the app's state went with it.
            match self.find(target, last).await {
                Ok(Some(e)) => return Ok(e),
                Err(e @ DriverError::Restarted(_)) => return Err(e),
                _ => {}
            }
            if Instant::now() >= end {
                return Err(DriverError::NotFound(target.to_string()));
            }
            sleep(Duration::from_millis(POLL_MS)).await;
        }
    }
}
