pub mod config;
pub mod spec;

use config::SuiteConfig;
use regex::Regex;
use spec::Spec;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// A problem with the suite, reported by `soundcheck lint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Relative to the suite folder.
    pub path: PathBuf,
    pub message: String,
}

impl Problem {
    pub fn new(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Problem {
            path: path.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug)]
pub struct Feature {
    pub name: String,
    /// `_feature.md`: how to get there, label hints, platform differences.
    pub context: Option<String>,
}

/// A suite as read from disk; whatever couldn't be read is in `problems`.
#[derive(Debug)]
pub struct Suite {
    pub root: PathBuf,
    /// The feature folders' folder, relative to `root` (empty when they sit in it).
    pub specs_dir: PathBuf,
    pub config: Option<SuiteConfig>,
    pub features: Vec<Feature>,
    /// In run order: by feature folder, then file name.
    pub specs: Vec<Spec>,
    /// `id-map.yaml`: old spec ID → current spec ID, so history survives renames.
    pub id_map: BTreeMap<String, String>,
    pub problems: Vec<Problem>,
}

pub const CONFIG_FILE: &str = "suite.yaml";
pub const ID_MAP_FILE: &str = "id-map.yaml";
pub const FEATURE_FILE: &str = "_feature.md";
/// Folders in the suite root that aren't features.
const NOT_FEATURES: &[&str] = &["hooks", "fixtures"];

static FEATURE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9]+(-[a-z0-9]+)*$").unwrap());
static SPEC_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{3}-[a-z0-9]+(-[a-z0-9]+)*)\.spec\.md$").unwrap());

pub fn load(root: &Path) -> Suite {
    let mut suite = Suite {
        root: root.to_path_buf(),
        specs_dir: PathBuf::new(),
        config: None,
        features: Vec::new(),
        specs: Vec::new(),
        id_map: BTreeMap::new(),
        problems: Vec::new(),
    };
    match fs::read_to_string(root.join(CONFIG_FILE)) {
        Ok(text) => match serde_yaml_ng::from_str(&text) {
            Ok(config) => suite.config = Some(config),
            Err(e) => suite
                .problems
                .push(Problem::new(CONFIG_FILE, e.to_string())),
        },
        Err(e) => suite
            .problems
            .push(Problem::new(CONFIG_FILE, format!("can't read it: {e}"))),
    }
    if let Ok(text) = fs::read_to_string(root.join(ID_MAP_FILE)) {
        match serde_yaml_ng::from_str(&text) {
            Ok(map) => suite.id_map = map,
            Err(e) => suite
                .problems
                .push(Problem::new(ID_MAP_FILE, e.to_string())),
        }
    }

    let prefix = suite
        .config
        .as_ref()
        .and_then(|c| c.specs.clone())
        .unwrap_or_default();
    suite.specs_dir = prefix.clone();
    let specs_dir = root.join(&prefix);
    if !specs_dir.is_dir() {
        suite.problems.push(Problem::new(
            &prefix,
            "the specs folder suite.yaml names isn't there",
        ));
    }
    for entry in sorted_entries(&specs_dir) {
        let name = name_of(&entry);
        if entry.is_file() {
            if name.ends_with(".spec.md") {
                suite.problems.push(Problem::new(
                    prefix.join(&name),
                    "specs belong in a feature folder: <feature>/NNN-name.spec.md",
                ));
            }
            continue;
        }
        if name.starts_with('.') || NOT_FEATURES.contains(&name.as_str()) {
            continue;
        }
        if suite
            .config
            .as_ref()
            .is_some_and(|c| reports_dir(root, c) == entry)
        {
            continue;
        }
        load_feature(&mut suite, &entry, &name, &prefix);
    }
    suite
}

fn load_feature(suite: &mut Suite, dir: &Path, name: &str, prefix: &Path) {
    if !FEATURE_NAME.is_match(name) {
        suite.problems.push(Problem::new(
            prefix.join(name),
            "feature folders are lower-case words joined by hyphens",
        ));
    }
    let context = fs::read_to_string(dir.join(FEATURE_FILE)).ok();
    for entry in sorted_entries(dir) {
        let file = name_of(&entry);
        let rel = prefix.join(name).join(&file);
        if entry.is_dir() {
            suite.problems.push(Problem::new(
                rel,
                "specs are one level deep: <feature>/NNN-name.spec.md",
            ));
            continue;
        }
        if file == FEATURE_FILE || !file.ends_with(".spec.md") {
            continue;
        }
        let Some(stem) = SPEC_FILE.captures(&file).map(|c| c[1].to_string()) else {
            suite.problems.push(Problem::new(
                rel,
                "spec files are named NNN-name.spec.md, e.g. 001-draw.spec.md",
            ));
            continue;
        };
        let text = match fs::read_to_string(&entry) {
            Ok(text) => text,
            Err(e) => {
                suite
                    .problems
                    .push(Problem::new(rel, format!("can't read it: {e}")));
                continue;
            }
        };
        match spec::parse(&format!("{name}/{stem}"), name, &text) {
            Ok(spec) => suite.specs.push(spec),
            Err(messages) => suite
                .problems
                .extend(messages.into_iter().map(|m| Problem::new(&rel, m))),
        }
    }
    suite.features.push(Feature {
        name: name.to_string(),
        context,
    });
}

/// Where runs are written: `reports` in suite.yaml (relative to the suite), or `reports/` beside the suite folder.
pub fn reports_dir(root: &Path, config: &SuiteConfig) -> PathBuf {
    match &config.reports {
        Some(dir) => root.join(dir),
        None => root.parent().unwrap_or(root).join("reports"),
    }
}

fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    entries.sort();
    entries
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}
