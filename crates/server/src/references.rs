//! A suite's reference images for `match`: `<suite>/references/<name>.png`, approved by a person, and beside it
//! `<name>.yaml` with each platform's bar, the most of the ink that may differ, and any areas left out of the score.
//! A run keeps each platform's capture
//! losslessly at `<run>/references/<name>-<platform>.png` with its score, so a person can adopt one as the reference
//! and set the bars from what the run saw.

use crate::suite::config::Platform;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A run's own captures, inside its folder.
pub const RUN_DIR: &str = "references";
/// Added to a run's score when it sets a bar, so a capture that scored exactly that passes again despite noise.
pub const ALLOWANCE: f64 = 0.05;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Bars {
    #[serde(default)]
    pub bars: BTreeMap<String, f64>,
    /// Areas left out of the score on every platform, e.g. buttons floating over the page: `[x, y, width, height]`
    /// as fractions of the compared area. A person writes them; adopting a new reference keeps them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<[f64; 4]>,
}

/// What a run's capture scored against the reference it had, if any.
#[derive(Debug, Serialize, Deserialize)]
pub struct Captured {
    pub differs: Option<f64>,
    pub shift: Option<(i32, i32)>,
    /// Which reference it was scored against ([`reference_id`]), so bars are only set from scores against this one.
    #[serde(default)]
    pub reference: Option<String>,
}

/// A stable fingerprint of a reference image's bytes (FNV-1a), so a score can name the reference it was measured
/// against.
pub fn reference_id(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{hash:016x}")
}

pub fn valid_name(name: &str) -> bool {
    crate::report::safe_name(name) && !name.contains('.')
}

pub fn reference_png(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.png"))
}

fn bars_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.yaml"))
}

pub fn load_bars(dir: &Path, name: &str) -> Result<Bars> {
    let path = bars_path(dir, name);
    if !path.exists() {
        return Ok(Bars::default());
    }

    let text = std::fs::read_to_string(&path)?;
    serde_yaml_ng::from_str(&text).with_context(|| path.display().to_string())
}

pub fn save_bars(dir: &Path, name: &str, bars: &Bars) -> Result<()> {
    std::fs::write(bars_path(dir, name), serde_yaml_ng::to_string(bars)?)?;
    Ok(())
}

pub fn capture_png(run: &Path, name: &str, platform: Platform) -> PathBuf {
    run.join(RUN_DIR).join(format!("{name}-{platform}.png"))
}

pub fn captured_json(run: &Path, name: &str, platform: Platform) -> PathBuf {
    run.join(RUN_DIR).join(format!("{name}-{platform}.json"))
}

/// Makes `platform`'s capture in `run` the reference, and clears the bars: they were measured against the old one.
pub fn adopt(dir: &Path, run: &Path, name: &str, platform: Platform) -> Result<()> {
    let from = capture_png(run, name, platform);
    if !from.exists() {
        bail!("{} has no {platform} capture of {name}", run.display());
    }

    std::fs::create_dir_all(dir)?;
    std::fs::copy(&from, reference_png(dir, name))?;
    let mut bars = load_bars(dir, name)?;
    bars.bars.clear();
    save_bars(dir, name, &bars)
}

/// Sets the bar of each of `platforms` that captured `name` in `run`, from its score. A bar only tightens unless
/// `loosen`; returns what changed, one line per platform.
pub fn set_bars(
    dir: &Path,
    run: &Path,
    name: &str,
    platforms: &[Platform],
    loosen: bool,
) -> Result<Vec<String>> {
    let png = reference_png(dir, name);
    if !png.exists() {
        bail!("{name} has no reference yet: adopt one first");
    }

    let current = reference_id(&std::fs::read(&png)?);
    let mut bars = load_bars(dir, name)?;
    let mut changed = Vec::new();
    for &platform in platforms {
        let path = captured_json(run, name, platform);
        if !path.exists() {
            continue;
        }

        let captured: Captured = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        let Some(differs) = captured.differs else {
            changed.push(format!(
                "{platform}: captured with no reference, so no score"
            ));
            continue;
        };
        if captured.reference.as_deref() != Some(current.as_str()) {
            changed.push(format!(
                "{platform}: scored against an earlier reference, so no bar; set them from a run against this one"
            ));
            continue;
        }

        let bar = ((differs + ALLOWANCE) * 10.0).ceil() / 10.0;
        let key = platform.to_string();
        match bars.bars.get(&key) {
            Some(&old) if bar > old && !loosen => {
                changed.push(format!(
                    "{platform}: kept {old}% (scored {differs:.2}%; --loosen to raise it)"
                ));
            }
            old => {
                changed.push(match old {
                    Some(old) => format!("{platform}: {old}% → {bar}%"),
                    None => format!("{platform}: {bar}%"),
                });
                bars.bars.insert(key, bar);
            }
        }
    }
    save_bars(dir, name, &bars)?;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web() -> Platform {
        "web".parse().unwrap()
    }

    #[test]
    fn bars_only_tighten_unless_loosened() {
        let tmp = std::env::temp_dir().join(format!("sc-refs-{}", std::process::id()));
        let (dir, run) = (tmp.join("suite"), tmp.join("run"));
        std::fs::create_dir_all(run.join(RUN_DIR)).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(reference_png(&dir, "page"), b"png").unwrap();
        let score = |v: f64| {
            let c = Captured {
                differs: Some(v),
                shift: Some((0, 0)),
                reference: Some(reference_id(b"png")),
            };
            std::fs::write(
                captured_json(&run, "page", web()),
                serde_json::to_string(&c).unwrap(),
            )
            .unwrap();
        };

        score(2.0);
        set_bars(&dir, &run, "page", &[web()], false).unwrap();
        assert_eq!(load_bars(&dir, "page").unwrap().bars["web"], 2.1);
        score(4.0);
        set_bars(&dir, &run, "page", &[web()], false).unwrap();
        assert_eq!(load_bars(&dir, "page").unwrap().bars["web"], 2.1);
        score(1.0);
        set_bars(&dir, &run, "page", &[web()], false).unwrap();
        assert_eq!(load_bars(&dir, "page").unwrap().bars["web"], 1.1);
        score(4.0);
        set_bars(&dir, &run, "page", &[web()], true).unwrap();
        assert_eq!(load_bars(&dir, "page").unwrap().bars["web"], 4.1);

        let mut kept = load_bars(&dir, "page").unwrap();
        kept.exclude = vec![[0.0, 0.0, 1.0, 0.08]];
        save_bars(&dir, "page", &kept).unwrap();
        std::fs::write(capture_png(&run, "page", web()), b"new png").unwrap();
        adopt(&dir, &run, "page", web()).unwrap();
        set_bars(&dir, &run, "page", &[web()], false).unwrap();
        assert!(
            load_bars(&dir, "page").unwrap().bars.is_empty(),
            "a score against the old reference sets no bar"
        );
        let adopted = load_bars(&dir, "page").unwrap();
        assert!(
            adopted.bars.is_empty(),
            "bars were measured against the old reference"
        );
        assert_eq!(
            adopted.exclude, kept.exclude,
            "adopting keeps the excluded areas"
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
