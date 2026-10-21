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
    async fn network_drop(&self, _seconds: u64) -> Result<()> {
        Err(self.unsupported("network-drop"))
    }
    async fn stress(&self, _load: u32, _seconds: u64) -> Result<()> {
        Err(self.unsupported("stress"))
    }

    fn unsupported(&self, tool: &'static str) -> DriverError {
        DriverError::Unsupported {
            tool,
            platform: self.platform(),
        }
    }
}
