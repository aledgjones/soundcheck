//! One interface over Android, iOS and web devices. This crate knows devices only: never suites, rows or an app.

pub mod android;
pub mod any;
pub mod compare;
pub mod device;
pub mod element;
pub mod host;
pub mod ios;
pub mod matcher;
pub mod ops;
pub mod parity;
pub mod proxy;
pub mod reference;
pub mod tools;
pub mod web;

pub use device::{Device, DriverError, Health, Stop};
pub use element::{Element, Rect};
pub use matcher::Matcher;
pub use ops::Driver;
pub use tools::{Kind, TOOLS, Tool, tool};

/// How soundcheck drives a device: a suite's platform names one (`driver: chrome`). A new kind of device or browser
/// is a new variant here, with its own implementation of each tool it supports.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum DriverKind {
    Android,
    Ios,
    Chrome,
}

impl std::fmt::Display for DriverKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DriverKind::Android => "android",
            DriverKind::Ios => "ios",
            DriverKind::Chrome => "chrome",
        })
    }
}
