use crate::suite::config::{Platform, Proxy, compatible};
use crate::suite::{CONFIG_FILE, FEATURE_FILE, Problem, Suite};
use regex::Regex;
use soundcheck_driver::Kind;
use soundcheck_driver::proxy::Upstream;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::LazyLock;

fn mechanics() -> impl Iterator<Item = &'static soundcheck_driver::Tool> {
    soundcheck_driver::TOOLS
        .iter()
        .filter(|t| t.kind == Kind::Mechanic)
}

fn driver_platform(platform: Platform) -> soundcheck_driver::Platform {
    match platform {
        Platform::Android => soundcheck_driver::Platform::Android,
        Platform::Ios => soundcheck_driver::Platform::Ios,
        Platform::Web => soundcheck_driver::Platform::Web,
    }
}

pub const MODELS: &[&str] = &["haiku", "sonnet", "opus", "fable"];
const TITLE_MAX: usize = 80;
const NETWORK_TOOLS: &[&str] = &["network-drop", "offline", "fault"];

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
    if let Some(network) = &config.network {
        let routes = network.proxy.all();
        if routes.is_empty() {
            push(
                CONFIG_FILE.into(),
                "network.proxy: list at least one route".into(),
            );
        }
        // Every proxy listens on this machine, so no two routes share a port, whichever platform they're for.
        let mut listens = BTreeSet::new();
        // Device ports, and whether a route named one: two implicit ones are the listen ports, already reported.
        let mut devices = std::collections::BTreeMap::new();
        for (platform, route) in &routes {
            let at = match platform {
                Some(p) => format!("network.proxy.{p}.{}", route.name),
                None => format!("network.proxy.{}", route.name),
            };
            if route.upstream == Upstream::Port(route.listen) {
                push(
                    CONFIG_FILE.into(),
                    format!("{at}: listen and upstream must differ"),
                );
            }
            if !listens.insert(route.listen) {
                push(
                    CONFIG_FILE.into(),
                    format!("{at}: port {} listed twice", route.listen),
                );
            }
            let explicit = route.device.is_some();
            if let Some(earlier) = devices.insert((*platform, route.device_port()), explicit)
                && (earlier || explicit)
            {
                push(
                    CONFIG_FILE.into(),
                    format!("{at}: device port {} listed twice", route.device_port()),
                );
            }
            if route.device.is_some() && *platform != Some(Platform::Android) {
                push(
                    CONFIG_FILE.into(),
                    format!(
                        "{at}: device is for Android's routes, which forward ports from the device"
                    ),
                );
            }
        }
        match &network.proxy {
            Proxy::Shared(_) if config.parallel => push(
                CONFIG_FILE.into(),
                "network: one proxy serves every platform, so platforms can't run in parallel; give each its own"
                    .into(),
            ),
            Proxy::PerPlatform(by) => {
                for platform in config.platforms.keys().filter(|p| **p != Platform::Web) {
                    if !by.contains_key(platform) {
                        push(
                            CONFIG_FILE.into(),
                            format!("network.proxy: {platform} has no routes, so its app can't reach anything"),
                        );
                    }
                }
            }
            Proxy::Shared(_) => {}
        }
    }
    if !config.hooks.seed.contains("{scenario}") {
        push(
            CONFIG_FILE.into(),
            "hooks.seed must contain {scenario}".into(),
        );
    }
    match (&config.hooks.stack_up, &config.hooks.stack_down) {
        (Some(up), Some(down)) => {
            for (name, command) in [("stack-up", up), ("stack-down", down)] {
                if !command.contains("{platform}") {
                    push(
                        CONFIG_FILE.into(),
                        format!("hooks.{name} must contain {{platform}}"),
                    );
                }
            }
        }
        (None, None) => {}
        _ => push(
            CONFIG_FILE.into(),
            "hooks: stack-up and stack-down go together, so every stack started is removed".into(),
        ),
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
    if let Some(prepare) = &config.prepare
        && !suite.root.join(prepare).is_file()
    {
        push(
            CONFIG_FILE.into(),
            format!("prepare: no file {}", prepare.display()),
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
                suite.specs_dir.join(&feature.name).join(FEATURE_FILE),
                "every feature needs a _feature.md: how to get there, label hints, platform differences".into(),
            );
        }
    }

    for spec in &suite.specs {
        let path = suite.specs_dir.join(format!("{}.spec.md", spec.id));
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
        // A double-tap is something the user does, so it may name their goal.
        if mechanics().any(|t| t.name != "double-tap" && title.contains(t.name)) {
            push("title: no tool names".into());
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
        for name in &f.tools {
            match soundcheck_driver::tool(name) {
                None => push(format!(
                    "tools: {name} isn't a driver tool ({})",
                    mechanics().map(|t| t.name).collect::<Vec<_>>().join(", ")
                )),
                Some(t) if t.kind == Kind::Basic => push(format!(
                    "tools: {name} is a basic tool; list only the others"
                )),
                Some(t) => {
                    for platform in &f.platforms {
                        if !t.platforms.contains(&driver_platform(*platform)) {
                            push(format!("tools: {name} doesn't run on {platform}"));
                        }
                    }
                    // Web has the browser's own switch; a device's network goes through the suite's proxy.
                    if NETWORK_TOOLS.contains(&name.as_str())
                        && config.network.is_none()
                        && f.platforms.iter().any(|p| *p != Platform::Web)
                    {
                        push(format!(
                            "tools: {name} on a device needs suite.yaml's network section (the proxy it goes through)"
                        ));
                    }
                }
            }
        }
    }

    problems
}
