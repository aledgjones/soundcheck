use crate::suite::config::{Platform, compatible};
use crate::suite::{CONFIG_FILE, FEATURE_FILE, ID_MAP_FILE, Problem, Suite};
use regex::Regex;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::LazyLock;

pub const MODELS: &[&str] = &["haiku", "sonnet", "opus", "fable"];
const TITLE_MAX: usize = 80;

/// A title is the user's goal in plain words: no tap paths, IDs or commands.
static TITLE_NOISE: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        (Regex::new(r"`").unwrap(), "no code formatting"),
        (Regex::new(r"\s>\s").unwrap(), "no tap paths (`A > B`)"),
        (
            Regex::new(r"\b[a-z0-9-]+/\d{3}-").unwrap(),
            "no references to other specs",
        ),
        (
            Regex::new(r"\b[A-Z]{2,5}-\d+\b").unwrap(),
            "no row or ticket IDs (put tickets in past-bugs)",
        ),
        (Regex::new(r"--[a-z]").unwrap(), "no command flags"),
    ]
});

/// Every rule `soundcheck lint` checks, on top of what loading the suite already reported.
pub fn lint(suite: &Suite, version: &str) -> Vec<Problem> {
    let mut problems = suite.problems.clone();
    let mut push = |path: PathBuf, message: String| problems.push(Problem::new(path, message));

    let Some(config) = &suite.config else {
        return problems;
    };
    if !compatible(&config.soundcheck, version) {
        push(
            CONFIG_FILE.into(),
            format!(
                "pinned to soundcheck {} but this is {version}; run the setup skill to migrate",
                config.soundcheck
            ),
        );
    }
    if config.platforms.is_empty() {
        push(CONFIG_FILE.into(), "platforms: name at least one".into());
    }
    for (platform, p) in &config.platforms {
        let mut need = |field: &str, value: &Option<String>| {
            if value.as_deref().is_none_or(str::is_empty) {
                push(
                    CONFIG_FILE.into(),
                    format!("platforms.{platform}: needs {field}"),
                );
            }
        };
        match platform {
            Platform::Web => {
                need("base-url", &p.base_url);
                need("start", &p.start);
            }
            _ => need("app-id", &p.app_id),
        }
    }
    if !config.hooks.seed.contains("{scenario}") {
        push(
            CONFIG_FILE.into(),
            "hooks.seed must contain {scenario}".into(),
        );
    }
    if config.seeds.is_empty() {
        push(CONFIG_FILE.into(), "seeds: list the seed scenarios".into());
    }
    if !MODELS.contains(&config.models.default.as_str()) {
        push(
            CONFIG_FILE.into(),
            format!(
                "models.default: {} isn't one of {}",
                config.models.default,
                MODELS.join(", ")
            ),
        );
    }
    if let Some(tickets) = &config.tickets
        && !tickets.url.contains("{id}")
    {
        push(CONFIG_FILE.into(), "tickets.url must contain {id}".into());
    }

    for feature in &suite.features {
        if feature.context.is_none() {
            push(
                PathBuf::from(&feature.name).join(FEATURE_FILE),
                "every feature needs a _feature.md: how to get there, label hints, platform differences".into(),
            );
        }
    }

    let ids: BTreeSet<&str> = suite.specs.iter().map(|s| s.id.as_str()).collect();
    for spec in &suite.specs {
        let path = PathBuf::from(format!("{}.spec.md", spec.id));
        let f = &spec.front;
        let mut push = |message: String| push(path.clone(), message);

        let title = f.title.trim();
        if title.is_empty() {
            push("title: empty".into());
        } else if title.chars().count() > TITLE_MAX {
            push(format!("title: keep it under {TITLE_MAX} characters"));
        }
        for (pattern, rule) in TITLE_NOISE.iter() {
            if pattern.is_match(title) {
                push(format!("title: {rule}"));
            }
        }
        if title.contains(&f.seed) && f.seed.len() > 3 {
            push("title: no seed names".into());
        }
        for tool in soundcheck_driver::TOOLS {
            if title.contains(tool) {
                push("title: no tool names".into());
            }
        }

        if !config.seeds.contains(&f.seed) {
            push(format!(
                "seed: {} isn't in suite.yaml's seeds ({})",
                f.seed,
                config.seeds.join(", ")
            ));
        }
        if f.starts_on.is_empty() {
            push("starts-on: name what must be on screen after the seed, so a bad reset is caught before the tester starts".into());
        }
        if f.platforms.is_empty() {
            push("platforms: name at least one".into());
        }
        let mut seen = BTreeSet::new();
        for platform in &f.platforms {
            if !seen.insert(platform) {
                push(format!("platforms: {platform} listed twice"));
                continue;
            }
            if !config.platforms.contains_key(platform) {
                push(format!(
                    "platforms: {platform} isn't configured in suite.yaml"
                ));
            }
        }
        if let Some(model) = &f.model
            && !MODELS.contains(&model.as_str())
        {
            push(format!("model: {model} isn't one of {}", MODELS.join(", ")));
        }
        for tool in &f.tools {
            if !soundcheck_driver::TOOLS.contains(&tool.as_str()) {
                push(format!(
                    "tools: {tool} isn't a driver tool ({})",
                    soundcheck_driver::TOOLS.join(", ")
                ));
            }
        }
    }

    for (old, new) in &suite.id_map {
        if ids.contains(old.as_str()) {
            push(
                ID_MAP_FILE.into(),
                format!("{old} is still a spec, so it can't be an old ID"),
            );
        }
        if !ids.contains(new.as_str()) {
            push(
                ID_MAP_FILE.into(),
                format!("{old} maps to {new}, which doesn't exist"),
            );
        }
    }
    problems
}
