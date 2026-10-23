//! One type for whichever platform a spec is on, so the server can hold a driver per platform.

use crate::Platform;
use crate::android::Android;
use crate::device::{Device, Health, Result, Stop};
use crate::element::Element;
use crate::ios::Ios;
use crate::web::Web;
use std::path::Path;

pub enum AnyDevice {
    Android(Android),
    Ios(Ios),
    Web(Box<Web>),
}

macro_rules! each {
    ($self:ident, $d:ident => $e:expr) => {
        match $self {
            AnyDevice::Android($d) => $e,
            AnyDevice::Ios($d) => $e,
            AnyDevice::Web($d) => $e,
        }
    };
}

impl Device for AnyDevice {
    fn platform(&self) -> Platform {
        each!(self, d => d.platform())
    }
    async fn read(&self, all: bool) -> Result<Vec<Element>> {
        each!(self, d => d.read(all).await)
    }
    async fn tap_at(&self, x: f64, y: f64) -> Result<()> {
        each!(self, d => d.tap_at(x, y).await)
    }
    async fn hold_at(&self, x: f64, y: f64, ms: u64) -> Result<()> {
        each!(self, d => d.hold_at(x, y, ms).await)
    }
    async fn touch(&self, stops: &[Stop], keep: bool) -> Result<()> {
        each!(self, d => d.touch(stops, keep).await)
    }
    async fn release(&self, back: bool) -> Result<()> {
        each!(self, d => d.release(back).await)
    }
    async fn swipe(&self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        each!(self, d => d.swipe(from, to, ms).await)
    }
    async fn type_text(&self, text: &str) -> Result<()> {
        each!(self, d => d.type_text(text).await)
    }
    async fn clear(&self, chars: usize) -> Result<()> {
        each!(self, d => d.clear(chars).await)
    }
    async fn launch(&self) -> Result<()> {
        match self {
            AnyDevice::Android(d) => Device::launch(d).await,
            AnyDevice::Ios(d) => Device::launch(d).await,
            AnyDevice::Web(d) => Device::launch(d.as_ref()).await,
        }
    }
    async fn screenshot(&self, path: &Path) -> Result<()> {
        each!(self, d => d.screenshot(path).await)
    }
    async fn errors(&self) -> Result<Vec<String>> {
        each!(self, d => d.errors().await)
    }
    async fn push_file(&self, source: &Path, name: &str) -> Result<()> {
        each!(self, d => d.push_file(source, name).await)
    }
    async fn screen_size(&self) -> Result<(f64, f64)> {
        each!(self, d => d.screen_size().await)
    }
    async fn density(&self) -> f64 {
        each!(self, d => d.density().await)
    }
    async fn press(&self, key: &str) -> Result<()> {
        each!(self, d => d.press(key).await)
    }
    async fn keyboard_shown(&self) -> Result<bool> {
        each!(self, d => d.keyboard_shown().await)
    }
    async fn hover_at(&self, x: f64, y: f64) -> Result<()> {
        each!(self, d => d.hover_at(x, y).await)
    }
    async fn goto(&self, url: &str) -> Result<()> {
        each!(self, d => d.goto(url).await)
    }
    async fn viewport(&self, width: u32, height: u32) -> Result<()> {
        each!(self, d => d.viewport(width, height).await)
    }
    async fn double_tap(&self, x: f64, y: f64) -> Result<()> {
        each!(self, d => d.double_tap(x, y).await)
    }
    async fn offline(&self, on: bool) -> Result<()> {
        each!(self, d => d.offline(on).await)
    }
    async fn hardware_keyboard(&self, on: bool) -> Result<()> {
        each!(self, d => d.hardware_keyboard(on).await)
    }
    async fn network_drop(&self, seconds: u64) -> Result<()> {
        each!(self, d => d.network_drop(seconds).await)
    }
    async fn health(&self) -> Result<Health> {
        each!(self, d => d.health().await)
    }
    async fn restart(&self) -> Result<()> {
        each!(self, d => d.restart().await)
    }
}
