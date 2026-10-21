//! `soundcheck drive`: one driver command from the terminal, outside a run, for exploring, reproducing a bug or
//! setting up screenshots. The readers stay up on the device between calls, so each call costs a connect, not a
//! reader start.

use crate::serve::connect;
use crate::serve::tools::{Step, line, step};
use crate::suite::config::Platform;
use anyhow::{Result, bail};
use clap::Subcommand;
use soundcheck_driver::{Device, Driver, Matcher};
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
    /// Press, hold, then drag between two points.
    Drag {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        #[arg(default_value_t = 700)]
        hold_ms: u64,
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
}

pub async fn drive(suite: &Path, platform: Platform, action: Action) -> Result<String> {
    if platform == Platform::Web {
        bail!("web has no browser outside a run to drive: each call would start a fresh one");
    }
    let loaded = crate::suite::load(suite);
    let config = loaded
        .config
        .as_ref()
        .and_then(|c| c.platforms.get(&platform));
    let driver = Driver::new(connect(platform, config, false).await?);
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
        Action::Drag {
            x1,
            y1,
            x2,
            y2,
            hold_ms,
        } => {
            d.drag((x1, y1), (x2, y2), hold_ms).await?;
            "dragged".into()
        }
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
    })
}
