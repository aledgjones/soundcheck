//! One interface over Android, iOS and web devices. This crate knows devices only: never suites, rows or an app.

pub mod android;
pub mod any;
pub mod compare;
pub mod device;
pub mod element;
pub mod ios;
pub mod matcher;
pub mod ops;
pub mod parity;
pub mod proxy;
pub mod tools;
pub mod web;

pub use device::{Device, DriverError, Health};
pub use element::{Element, Rect};
pub use matcher::Matcher;
pub use ops::Driver;
pub use tools::{Kind, TOOLS, Tool, tool};

/// The platforms a driver serves. The suite has its own type; this one keeps the driver free of suite concepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Android,
    Ios,
    Web,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Platform::Android => "android",
            Platform::Ios => "ios",
            Platform::Web => "web",
        })
    }
}
