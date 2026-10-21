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
    async fn drag(&self, from: (f64, f64), to: (f64, f64), hold_ms: u64) -> Result<()>;
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

    /// A touch that stays down until `touch_up`, for gestures that must be watched mid-way.
    async fn touch_down(&self, _x: f64, _y: f64) -> Result<()> {
        Err(self.unsupported("preview-compare"))
    }
    async fn touch_move(&self, _x: f64, _y: f64) -> Result<()> {
        Err(self.unsupported("preview-compare"))
    }
    async fn touch_up(&self, _x: f64, _y: f64) -> Result<()> {
        Err(self.unsupported("preview-compare"))
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
    /// Taps timed on the device; the default loop can't hold short intervals across round trips, so readers override it.
    async fn tap_burst(&self, x: f64, y: f64, count: u32, interval_ms: u64) -> Result<()> {
        for i in 0..count {
            if i > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
            }
            self.tap_at(x, y).await?;
        }
        Ok(())
    }
    /// Puts (or takes) the device's connections through a proxy `network_drop` can freeze. Called before the app
    /// launches, so its connections go through it; a platform that doesn't need it accepts and does nothing.
    async fn intercept_network(&self, _on: bool) -> Result<()> {
        Ok(())
    }
    async fn network_drop(&self, _seconds: u64) -> Result<()> {
        Err(self.unsupported("network-drop"))
    }
    async fn stress(&self, _load: u32, _seconds: u64) -> Result<()> {
        Err(self.unsupported("stress"))
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

    #[test]
    fn a_busy_host_isnt_a_degraded_device() {
        let mut health = Health::default();
        health.host("Mac load 46.2 on 10 cores".into(), true);
        health.measure("launch 1.2 s".into(), false);
        assert!(health.degraded.is_empty());
        assert_eq!(health.busy, ["Mac load 46.2 on 10 cores"]);
        assert_eq!(health.facts.len(), 2);
    }
}
