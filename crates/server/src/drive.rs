//! `soundcheck drive`: one driver command from the terminal, outside a run, for exploring, reproducing a bug or
//! setting up screenshots. The readers stay up on the device between calls, so each call costs a connect, not a
//! reader start.

use crate::serve::tools::{Step, TouchStop, line, step};
use crate::suite::config::Platform;
use anyhow::{Result, bail};
use clap::Subcommand;
use soundcheck_driver::any::AnyDevice;
use soundcheck_driver::parity::compare;
use soundcheck_driver::{Device, Matcher};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum Action {
    /// What's on screen as a screen reader gets it, optionally only what matches.
    See {
        target: Option<String>,
        /// Include elements labelled only by an id.
        #[arg(long)]
        all: bool,
    },
    /// Tap an element (exact label, `prefix:…` or `re:…`).
    Tap {
        target: String,
        /// The last match, e.g. an alert's button that shares its row's label.
        #[arg(long)]
        last: bool,
    },
    /// Tap a point, in the platform's units (pixels on Android, points on iOS).
    TapAt { x: f64, y: f64 },
    /// Wait until an element shows.
    Wait {
        target: String,
        #[arg(default_value_t = 10_000)]
        timeout_ms: u64,
    },
    /// Fail unless an element is on screen.
    Expect { target: String },
    /// Fail unless an element is gone.
    Gone { target: String },
    /// Empty a field and type into it.
    Fill { target: String, text: String },
    /// Type into the focused field.
    Type { text: String },
    /// Empty the focused field.
    Clear { chars: Option<usize> },
    /// Press and hold an element.
    Hold { target: String, ms: u64 },
    /// Swipe between two points.
    Swipe {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        #[arg(default_value_t = 300)]
        ms: u64,
    },
    /// One finger along a path: stops are elements or `at:x,y` points, and `+ms` after a stop rests there, e.g.
    /// `touch Thumb +500 at:900,400 Thumb` presses Thumb for 500 ms, slides away and back, and lifts. A drag is two
    /// stops: `touch at:980,640 +700 at:980,420`.
    Touch {
        #[arg(required = true)]
        path: Vec<String>,
        /// Stay down at the last stop until `release`, other input, or 30 s.
        #[arg(long)]
        keep: bool,
    },
    /// Lift the touch `touch --keep` left down.
    Release {
        /// Move back to where the touch began first, so nothing is dropped.
        #[arg(long)]
        back: bool,
    },
    /// Press a hardware key (ArrowRight, Enter, Escape…).
    Press { key: String },
    /// Restart the app.
    Launch,
    /// Save a screenshot.
    Shot { file: PathBuf },
    /// Crashes and app errors since the last launch.
    Errors,
    /// Copy a file onto the device (Downloads, or On My iPad).
    Push { file: PathBuf, name: Option<String> },
    /// Connect the simulator's hardware keyboard, or disconnect it so fields raise the on-screen one (iOS).
    Keyboard {
        #[arg(value_parser = ["hardware", "software"])]
        kind: String,
    },
    /// Take the app offline (connections refused, as in airplane mode), or bring it back, through the suite's proxy.
    Network {
        #[arg(value_parser = ["offline", "online"])]
        state: String,
    },
    /// Freeze the network for N seconds (nothing passes, nothing closes), then let new connections through.
    NetworkDrop { seconds: u64 },
    /// Have a service answer requests to a path (`*` for any one segment) with an error status, through the suite's
    /// proxy, until `fault clear`.
    Fault {
        /// The request path, or `clear` to clear every fault.
        path: String,
        /// The HTTP status, 400–599.
        status: Option<u16>,
    },
    /// Compare Android's reader with uiautomator on this screen, both the full tree and the screen reader's.
    Parity,
}

pub async fn drive(suite: &Path, platform: Platform, action: Action) -> Result<String> {
    if platform == Platform::Web {
        bail!("web has no browser outside a run to drive: each call would start a fresh one");
    }
    if !suite.join(crate::suite::CONFIG_FILE).exists() {
        bail!(
            "no {} in {}: run from the suite folder, or pass --suite",
            crate::suite::CONFIG_FILE,
            suite.canonicalize().unwrap_or(suite.into()).display()
        );
    }
    let loaded = crate::suite::load(suite);
    let driver = crate::serve::driver(platform, &loaded, false).await?;
    let d = &driver;
    let matcher = |t: &str| Matcher::parse(t).map_err(|e| anyhow::anyhow!("{t}: {e}"));
    let as_step = |s: Step| async move { Ok::<_, anyhow::Error>(step(d, &s).await?) };
    Ok(match action {
        Action::See { target, all } => {
            let filter = target.as_deref().map(matcher).transpose()?;
            let seen = d.see(filter.as_ref(), all).await?;
            seen.iter().map(line).collect::<Vec<_>>().join("\n")
        }
        Action::Tap { target, last } => as_step(Step::Tap { target, last }).await?,
        Action::TapAt { x, y } => {
            d.device.tap_at(x, y).await?;
            format!("tapped {x:.0},{y:.0}")
        }
        Action::Wait { target, timeout_ms } => as_step(Step::Wait { target, timeout_ms }).await?,
        Action::Expect { target } => as_step(Step::Expect { target }).await?,
        Action::Gone { target } => as_step(Step::Gone { target }).await?,
        Action::Fill { target, text } => as_step(Step::Fill { target, text }).await?,
        Action::Type { text } => as_step(Step::Type { text }).await?,
        Action::Clear { chars } => as_step(Step::Clear { chars }).await?,
        Action::Hold { target, ms } => as_step(Step::Hold { target, ms }).await?,
        Action::Swipe { x1, y1, x2, y2, ms } => {
            as_step(Step::Swipe {
                from: [x1, y1],
                to: [x2, y2],
                ms,
            })
            .await?
        }
        Action::Touch { path, keep } => {
            as_step(Step::Touch {
                path: touch_path(&path)?,
                keep,
            })
            .await?
        }
        Action::Release { back } => as_step(Step::Release { back }).await?,
        Action::Press { key } => as_step(Step::Press { key }).await?,
        Action::Launch => {
            d.device.launch().await?;
            "launched".into()
        }
        Action::Shot { file } => {
            d.device.screenshot(&file).await?;
            format!("saved {}", file.display())
        }
        Action::Errors => {
            let errors = d.device.errors().await?;
            if errors.is_empty() {
                "No errors since launch.".into()
            } else {
                errors.join("\n")
            }
        }
        Action::Push { file, name } => {
            let name = name.unwrap_or_else(|| {
                file.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
            d.device.push_file(&file, &name).await?;
            format!("pushed {name}")
        }
        Action::Keyboard { kind } => {
            d.device.hardware_keyboard(kind == "hardware").await?;
            format!("{kind} keyboard")
        }
        Action::Network { state } => {
            d.offline(state == "offline").await?;
            state
        }
        Action::NetworkDrop { seconds } => {
            d.network_drop(seconds).await?;
            format!("network down for {seconds} s; it comes back on its own")
        }
        Action::Fault { path, status } => match (path.as_str(), status) {
            ("clear", None) => {
                d.fault_clear().await?;
                "no faults".to_string()
            }
            (path, Some(status)) => d.fault(path, status).await?,
            (_, None) => bail!("fault takes a path and a status, or `clear`"),
        },
        Action::Parity => parity(&d.device).await?,
    })
}

/// `touch`'s arguments as stops: each element or point, with the `+ms` after it as its rest.
fn touch_path(args: &[String]) -> Result<Vec<TouchStop>> {
    let mut stops: Vec<TouchStop> = Vec::new();
    for arg in args {
        if let Some(ms) = arg.strip_prefix('+') {
            let Some(stop) = stops.last_mut() else {
                bail!("{arg} rests at a stop, so it comes after one");
            };
            stop.pause_ms = ms
                .parse()
                .map_err(|_| anyhow::anyhow!("{arg}: a rest is `+<ms>`, e.g. `+500`"))?;
        } else {
            stops.push(TouchStop {
                at: arg.clone(),
                pause_ms: 0,
            });
        }
    }
    Ok(stops)
}

async fn parity(device: &AnyDevice) -> Result<String> {
    // Read the reader first: on Android, uiautomator takes the UiAutomation connection and stops the reader.
    let full = device.read(true).await?;
    let spoken = device.read(false).await?;
    let (tool, full_tool, spoken_tool, keyboard) = match device {
        AnyDevice::Android(a) => (
            "uiautomator",
            a.read_uiautomator(false).await?,
            a.read_uiautomator(true).await?,
            a.keyboard_region().await?,
        ),
        AnyDevice::Ios(_) => bail!("iOS has no second tool to compare: the reader is the only one"),
        AnyDevice::Web(_) => bail!("web parity runs in a run, against Chrome's snapshot"),
    };
    let mut full = compare(&full, &full_tool);
    let mut spoken = compare(&spoken, &spoken_tool);
    if let Some(region) = keyboard {
        full.set_aside(region);
        spoken.set_aside(region);
    }
    Ok(format!(
        "full tree {}\nscreen reader {}",
        full.render(tool),
        spoken.render(tool)
    ))
}
