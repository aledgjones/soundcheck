//! What a run keeps beside its journal, so any run can be shown again from its folder alone: how it was started
//! (`run.json`) and the suite as it was (`suite/`, a snapshot of `suite.yaml` and the specs).

use super::{Plan, PlannedSpec, Run};
use crate::suite::{self, Suite, config::Platform};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const INFO_FILE: &str = "run.json";
pub const SNAPSHOT_DIR: &str = "suite";
pub const JOURNAL_FILE: &str = "journal.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInfo {
    /// The suite the run tested, where hooks run from on resume.
    pub suite: PathBuf,
    /// The release under test, shown as the page's title.
    pub label: String,
    pub platforms: Vec<Platform>,
    /// Features or spec IDs; empty for every spec.
    pub scope: Vec<String>,
}

/// The rows a run holds: the specs in scope, each on the platforms it runs on that the run covers.
pub fn plan_for(suite: &Suite, platforms: &[Platform], scope: &[String]) -> Result<Plan> {
    let config = suite.config.as_ref().context("no suite.yaml")?;
    let in_scope =
        |id: &str, feature: &str| scope.is_empty() || scope.iter().any(|i| i == id || i == feature);
    for item in scope {
        if !suite
            .specs
            .iter()
            .any(|s| s.id == *item || s.feature == *item)
        {
            bail!("--scope {item}: no feature or spec by that name");
        }
    }
    Ok(Plan {
        specs: suite
            .specs
            .iter()
            .filter(|s| in_scope(&s.id, &s.feature))
            .map(|s| PlannedSpec {
                id: s.id.clone(),
                platforms: s
                    .front
                    .platforms
                    .iter()
                    .copied()
                    .filter(|p| platforms.contains(p))
                    .collect(),
                outcomes: s.outcomes.clone(),
            })
            .filter(|s| !s.platforms.is_empty())
            .collect(),
        parallel: config.parallel,
    })
}

/// Writes `run.json` and snapshots the suite's config and specs into the run folder.
pub fn save(run_dir: &Path, info: &RunInfo, suite: &Suite) -> Result<()> {
    std::fs::write(run_dir.join(INFO_FILE), serde_json::to_string_pretty(info)?)?;
    let snapshot = run_dir.join(SNAPSHOT_DIR);
    std::fs::create_dir_all(&snapshot)?;
    std::fs::copy(
        suite.root.join(suite::CONFIG_FILE),
        snapshot.join(suite::CONFIG_FILE),
    )?;
    let specs = suite.root.join(&suite.specs_dir);
    for feature in &suite.features {
        let from = specs.join(&feature.name);
        let to = snapshot.join(&suite.specs_dir).join(&feature.name);
        std::fs::create_dir_all(&to)?;
        for entry in std::fs::read_dir(&from)?.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".md") {
                std::fs::copy(entry.path(), to.join(name))?;
            }
        }
    }
    Ok(())
}

pub fn read_info(run_dir: &Path) -> Result<RunInfo> {
    let text = std::fs::read_to_string(run_dir.join(INFO_FILE))
        .with_context(|| format!("{} has no {INFO_FILE}", run_dir.display()))?;
    Ok(serde_json::from_str(&text)?)
}

/// A run rebuilt from its folder: the snapshot suite, the replayed journal and its label. Read only.
pub fn load(run_dir: &Path, now: u64) -> Result<(Suite, Run, String)> {
    let info = read_info(run_dir)?;
    let suite = suite::load(&run_dir.join(SNAPSHOT_DIR));
    let plan = plan_for(&suite, &info.platforms, &info.scope)?;
    let run = Run::open(plan, &run_dir.join(JOURNAL_FILE), now)?;
    Ok((suite, run, info.label))
}
