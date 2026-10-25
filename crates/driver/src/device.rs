use crate::Platform;
use crate::element::Element;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    /// Refused clearly, so a spec never silently skips a step.
    #[error("{tool} isn't available on {platform}")]
    Unsupported {
        tool: &'static str,
        platform: Platform,
    },
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("device: {0}")]
    Device(String),
}

pub type Result<T> = std::result::Result<T, DriverError>;

/// From one tap's start to the next in a double-tap: a person's rhythm, inside Android's 300 ms window. Nothing
/// taps faster, since an 80 ms double Undo once left a state no finger could reach (SA-673).
pub const DOUBLE_TAP_MS: u64 = 150;
/// How long each touch of a double-tap is held.
pub const DOUBLE_TAP_HOLD_MS: u64 = 40;

/// A reader input script for a double-tap at one point, in the readers' shared step format.
pub fn double_tap_script(x: f64, y: f64) -> String {
    let tap = format!("d {x:.0} {y:.0};w {DOUBLE_TAP_HOLD_MS};u {x:.0} {y:.0}");
    format!("{tap};w {};{tap}", DOUBLE_TAP_MS - DOUBLE_TAP_HOLD_MS)
}

/// Where a touch passes on its path, and how long it rests there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stop {
    pub x: f64,
    pub y: f64,
    pub pause_ms: u64,
}

/// How far a moving touch goes between input events, in the platform's units.
const TOUCH_STEP: f64 = 10.0;
/// The rest before a moved touch lifts, so its end isn't read as a fling.
pub const SETTLE_MS: u64 = 300;

/// The points a touch moves through from `from` to `to`, at most [`TOUCH_STEP`] apart, ending at `to`.
pub fn touch_path(from: (f64, f64), to: (f64, f64)) -> Vec<(f64, f64)> {
    let steps = (((to.0 - from.0).abs().max((to.1 - from.1).abs())) / TOUCH_STEP)
        .ceil()
        .max(1.0) as u32;
    (1..=steps)
        .map(|i| {
            let t = f64::from(i) / f64::from(steps);
            (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
        })
        .collect()
}

/// A reader input script for one finger through `stops`: down at the first, moving a step every `step_ms` and resting
/// at each stop for its pause, then settling and lifting at the last. With `keep` it ends with `h` instead, and the
/// reader holds the touch there until `release`.
pub fn touch_script(stops: &[Stop], step_ms: u64, keep: bool) -> String {
    let mut steps = Vec::new();
    let mut at: Option<(f64, f64)> = None;
    for stop in stops {
        let to = (stop.x, stop.y);
        match at {
            None => steps.push(format!("d {:.0} {:.0}", to.0, to.1)),
            Some(from) => {
                for (x, y) in touch_path(from, to) {
                    steps.push(format!("d {x:.0} {y:.0}"));
                    steps.push(format!("w {step_ms}"));
                }
            }
        }
        if stop.pause_ms > 0 {
            steps.push(format!("w {}", stop.pause_ms));
        }
        at = Some(to);
    }
    let Some((x, y)) = at else {
        return String::new();
    };
    if keep {
        steps.push("h".into());
    } else {
        if stops.len() > 1 {
            steps.push(format!("w {SETTLE_MS}"));
        }
        steps.push(format!("u {x:.0} {y:.0}"));
    }
    steps.join(";")
}

/// A reader input script for a swipe from `from` to `to` over about `ms`, lifted while still moving so it can fling.
pub fn swipe_script(from: (f64, f64), to: (f64, f64), ms: u64) -> String {
    let points = touch_path(from, to);
    let step = ms / (points.len() as u64 + 1);
    let mut steps = vec![
        format!("d {:.0} {:.0}", from.0, from.1),
        format!("w {step}"),
    ];
    for (x, y) in points {
        steps.push(format!("d {x:.0} {y:.0}"));
        steps.push(format!("w {step}"));
    }
    steps.push(format!("u {:.0} {:.0}", to.0, to.1));
    steps.join(";")
}

/// How the device was doing, sampled after a result: part of the evidence. A slowly degrading emulator once produced
/// one-off failures that looked like app bugs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Health {
    /// Why the device counts as degraded; empty when it's healthy. A restart can fix these.
    pub degraded: Vec<String>,
    /// Why the machine running the device is busy (its load, its memory). A restart can't fix these and makes them
    /// worse, since a booting simulator adds load, so the server waits them out instead.
    pub busy: Vec<String>,
    /// What was measured, for the record ("memory 41% free", "launch 1.2 s").
    pub facts: Vec<String>,
}

impl Health {
    /// Records a measurement of the device, and a degradation when it's past its limit.
    pub fn measure(&mut self, fact: String, degraded: bool) {
        if degraded {
            self.degraded.push(fact.clone());
        }
        self.facts.push(fact);
    }

    /// Records a measurement of the host, and that it's busy when it's past its limit.
    pub fn host(&mut self, fact: String, busy: bool) {
        if busy {
            self.busy.push(fact.clone());
        }
        self.facts.push(fact);
    }
}

/// What each platform implements: raw reads and inputs. Everything built on them lives in [`crate::Driver`], once.
/// Coordinates are in the platform's own units, as [`Element::frame`] reports them.
#[allow(async_fn_in_trait)]
pub trait Device {
    fn platform(&self) -> Platform;

    /// The screen as a screen reader gets it; `all` includes views marked not important for accessibility.
    async fn read(&self, all: bool) -> Result<Vec<Element>>;
    async fn tap_at(&self, x: f64, y: f64) -> Result<()>;
    async fn hold_at(&self, x: f64, y: f64, ms: u64) -> Result<()>;
    /// One finger along `stops`, as [`touch_script`] describes; with `keep` it stays down at the last until `release`.
    /// A touch still held goes back to where it began and lifts first.
    async fn touch(&self, stops: &[Stop], keep: bool) -> Result<()>;
    /// Lifts the touch a kept `touch` left down: where it is, or `back` along a line to where it began.
    async fn release(&self, back: bool) -> Result<()>;
    async fn swipe(&self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()>;
    /// Appends to the focused field.
    async fn type_text(&self, text: &str) -> Result<()>;
    /// Empties the focused field, which holds at most `chars` characters.
    async fn clear(&self, chars: usize) -> Result<()>;
    async fn launch(&self) -> Result<()>;
    async fn screenshot(&self, path: &Path) -> Result<()>;
    /// Crashes and app errors since the last launch.
    async fn errors(&self) -> Result<Vec<String>>;

    /// Puts a file where the system file picker finds it (Android Downloads, iOS On My iPad), named `name`.
    async fn push_file(&self, _source: &Path, _name: &str) -> Result<()> {
        Err(self.unsupported("push"))
    }

    /// The screen's size in the units coordinates use, so a screenshot's pixels can be mapped to them.
    async fn screen_size(&self) -> Result<(f64, f64)> {
        Err(self.unsupported("preview-compare"))
    }
    /// Coordinate units per density-independent point: Android's input is in pixels, iOS and web in points.
    async fn density(&self) -> f64 {
        1.0
    }

    /// Presses a hardware key by its web name (`ArrowRight`, `Enter`, `Escape`, `Home`…), as a keyboard or pedal would.
    async fn press(&self, _key: &str) -> Result<()> {
        Err(self.unsupported("press"))
    }

    async fn keyboard_shown(&self) -> Result<bool> {
        Err(self.unsupported("keyboard"))
    }
    async fn hover_at(&self, _x: f64, _y: f64) -> Result<()> {
        Err(self.unsupported("hover"))
    }
    async fn goto(&self, _url: &str) -> Result<()> {
        Err(self.unsupported("goto"))
    }
    async fn viewport(&self, _width: u32, _height: u32) -> Result<()> {
        Err(self.unsupported("viewport"))
    }
    /// Two taps a person's double-tap apart, timed on the device; the default loop can't hold the gap across round
    /// trips, so readers override it.
    async fn double_tap(&self, x: f64, y: f64) -> Result<()> {
        self.tap_at(x, y).await?;
        tokio::time::sleep(std::time::Duration::from_millis(DOUBLE_TAP_MS)).await;
        self.tap_at(x, y).await
    }
    /// Connects or disconnects the device's hardware keyboard: connected, typing is fast; disconnected, the on-screen
    /// keyboard shows. Called before the app launches; a platform without one accepts and does nothing.
    async fn hardware_keyboard(&self, _on: bool) -> Result<()> {
        Ok(())
    }
    /// The device's own network switch, where it has one (web); Android and iOS go through the network proxy.
    async fn network_drop(&self, _seconds: u64) -> Result<()> {
        Err(self.unsupported("network-drop"))
    }
    async fn offline(&self, _on: bool) -> Result<()> {
        Err(self.unsupported("offline"))
    }

    /// Samples the device's health, and repairs what it safely can on the spot (lost port forwards).
    async fn health(&self) -> Result<Health> {
        Ok(Health::default())
    }
    /// Restarts the emulator or simulator and puts back what the run needs (forwards, the reader).
    async fn restart(&self) -> Result<()> {
        Err(self.unsupported("restart"))
    }

    fn unsupported(&self, tool: &'static str) -> DriverError {
        DriverError::Unsupported {
            tool,
            platform: self.platform(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop(x: f64, y: f64, pause_ms: u64) -> Stop {
        Stop { x, y, pause_ms }
    }

    #[test]
    fn a_drag_holds_then_moves_and_settles_before_lifting() {
        assert_eq!(
            touch_script(&[stop(0.0, 0.0, 500), stop(10.0, 0.0, 0)], 16, false),
            "d 0 0;w 500;d 10 0;w 16;w 300;u 10 0"
        );
    }

    #[test]
    fn a_touch_can_go_out_and_back_resting_on_the_way() {
        let script = touch_script(
            &[stop(0.0, 0.0, 0), stop(20.0, 0.0, 200), stop(0.0, 0.0, 0)],
            30,
            false,
        );
        assert_eq!(
            script,
            "d 0 0;d 10 0;w 30;d 20 0;w 30;w 200;d 10 0;w 30;d 0 0;w 30;w 300;u 0 0"
        );
    }

    #[test]
    fn a_kept_touch_ends_held_down() {
        let script = touch_script(&[stop(0.0, 0.0, 100), stop(10.0, 0.0, 0)], 16, true);
        assert_eq!(script, "d 0 0;w 100;d 10 0;w 16;h");
    }

    #[test]
    fn a_still_touch_is_a_hold() {
        assert_eq!(
            touch_script(&[stop(5.0, 6.0, 800)], 16, false),
            "d 5 6;w 800;u 5 6"
        );
    }

    #[test]
    fn a_path_ends_where_it_was_aimed() {
        let p = touch_path((0.0, 0.0), (25.0, 5.0));
        assert_eq!(p.len(), 3);
        assert_eq!(p.last(), Some(&(25.0, 5.0)));
    }

    #[test]
    fn a_swipe_lifts_while_moving() {
        assert_eq!(
            swipe_script((0.0, 0.0), (20.0, 0.0), 300),
            "d 0 0;w 100;d 10 0;w 100;d 20 0;w 100;u 20 0"
        );
    }

    #[test]
    fn a_busy_host_isnt_a_degraded_device() {
        let mut health = Health::default();
        health.host("Mac CPU 95% busy".into(), true);
        health.measure("launch 1.2 s".into(), false);
        assert!(health.degraded.is_empty());
        assert_eq!(health.busy, ["Mac CPU 95% busy"]);
        assert_eq!(health.facts.len(), 2);
    }
}
