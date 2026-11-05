use crate::suite::config::{DriverKind, Platform, SuiteConfig, compatible, parse_viewport};
use crate::suite::{CONFIG_FILE, FEATURE_FILE, Problem, Suite};
use regex::Regex;
use soundcheck_driver::Kind;
use soundcheck_driver::proxy::Upstream;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::LazyLock;

fn mechanics() -> impl Iterator<Item = &'static soundcheck_driver::Tool> {
    soundcheck_driver::TOOLS
        .iter()
        .filter(|t| t.kind == Kind::Mechanic)
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

    for message in platform_problems(config) {
        push(CONFIG_FILE.into(), message);
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
        if f.starts_on.is_some() {
            push(
                "starts-on: gone; a spec starts on the home screen, or give it a `start` link"
                    .into(),
            );
        }
        // Unknown platforms are reported below; nothing else is asked of them.
        let driver = |p: &Platform| config.platforms.get(p).map(|c| c.driver);
        for (platform, link) in &f.start {
            if !f.platforms.contains(platform) {
                push(format!("start: {platform} isn't in platforms"));
            } else if driver(platform) == Some(DriverKind::Chrome) {
                if !link.starts_with('/') && !link.starts_with("http") {
                    push(format!(
                        "start: {platform}'s link is a URL or a path from the site's root, not {link:?}"
                    ));
                }
            } else if driver(platform).is_some() && !link.contains("://") {
                push(format!(
                    "start: {platform}'s link is a deep link such as myapp://screen, not {link:?}"
                ));
            }
        }

        let on_chrome = f
            .platforms
            .iter()
            .any(|p| driver(p) == Some(DriverKind::Chrome));
        match &f.viewport {
            None => {}
            Some(_) if !on_chrome => {
                push(
                    "viewport: sizes a chrome platform's window, so the spec must run on one"
                        .into(),
                );
            }
            Some(viewport) if parse_viewport(viewport).is_none() => {
                push(format!(
                    "viewport: {viewport} isn't WIDTHxHEIGHT, such as 1440x900"
                ));
            }
            Some(_) => {}
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

            if let Err(unknown) = config.platform(platform.as_str()) {
                push(format!("platforms: {unknown}"));
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
                        if let Some(d) = driver(platform)
                            && !t.drivers.contains(&d)
                        {
                            push(format!("tools: {name} doesn't run on {platform} ({d})"));
                        }
                    }

                    let unrouted: Vec<String> = f
                        .platforms
                        .iter()
                        .filter(|p| driver(p).is_some())
                        .filter(|p| crate::network::routes(config, **p).is_none())
                        .map(ToString::to_string)
                        .collect();
                    if NETWORK_TOOLS.contains(&name.as_str()) && !unrouted.is_empty() {
                        push(format!(
                            "tools: {name} needs suite.yaml to give {} proxy routes (the proxy it goes through)",
                            unrouted.join(", ")
                        ));
                    }
                }
            }
        }
    }

    problems
}

/// suite.yaml's platforms: what each block needs for its driver, its hooks and proxy routes, and what two blocks
/// mustn't share. Every message starts with where in suite.yaml it is.
fn platform_problems(config: &SuiteConfig) -> Vec<String> {
    let mut problems = Vec::new();
    let mut listens: BTreeMap<u16, String> = BTreeMap::new();
    let mut devices: BTreeMap<(DriverKind, &str), Platform> = BTreeMap::new();
    let mut drivers: BTreeMap<DriverKind, Platform> = BTreeMap::new();
    for (&platform, p) in &config.platforms {
        let at = format!("platforms.{platform}");
        let mut need = |field: &str, value: &Option<String>| {
            if value.as_deref().is_none_or(str::is_empty) {
                problems.push(format!("{at}: needs {field} for the {} driver", p.driver));
            }
        };

        match p.driver {
            DriverKind::Chrome => {
                need("url", &p.url);
                need("viewport", &p.viewport);

                if p.device != "desktop" {
                    problems.push(format!(
                        "{at}.device: a chrome platform runs on `desktop`, not {:?}",
                        p.device
                    ));
                }

                if p.app.is_some() {
                    problems.push(format!(
                        "{at}.app: a chrome platform opens its `url`; app is for ios and android"
                    ));
                }
            }
            DriverKind::Android | DriverKind::Ios => need("app", &p.app),
        }

        if let Some(viewport) = &p.viewport {
            if p.driver != DriverKind::Chrome {
                problems.push(format!(
                    "{at}.viewport: only a chrome platform has a window size"
                ));
            } else if parse_viewport(viewport).is_none() {
                problems.push(format!(
                    "{at}.viewport: {viewport} isn't WIDTHxHEIGHT, such as 1440x900"
                ));
            }
        }

        for (field, value) in [("url", &p.url), ("chrome-profile", &p.chrome_profile)] {
            if value.is_some() && p.driver != DriverKind::Chrome {
                problems.push(format!("{at}.{field}: only a chrome platform has one"));
            }
        }

        if let Some(profile) = &p.chrome_profile
            && !(profile.starts_with('/') || profile.starts_with("~/"))
        {
            problems.push(format!(
                "{at}.chrome-profile: {profile} must be absolute or start with ~/"
            ));
        }

        if p.device.trim().is_empty() {
            problems.push(format!("{at}.device: name the device it runs on"));
        } else if let Some(other) = devices.insert((p.driver, p.device.as_str()), platform) {
            problems.push(format!(
                "{at}.device: {other} runs on {} too; give each platform its own",
                p.device
            ));
        }

        if let Some(other) = drivers.insert(p.driver, platform) {
            problems.push(format!(
                "{at}: {other} uses the {} driver too; two platforms on one driver need a device each, which lands with per-platform readers (SA-880)",
                p.driver
            ));
        }

        if !p.hooks.seed.contains("{scenario}") {
            problems.push(format!("{at}.hooks.seed must contain {{scenario}}"));
        }

        if p.hooks.stack_up.is_some() != p.hooks.stack_down.is_some() {
            problems.push(format!(
                "{at}.hooks: stack-up and stack-down go together, so every stack started is removed"
            ));
        }

        // Every proxy listens on this machine, so no two routes share a port, whichever platform they're for.
        // Device ports, and whether a route named one: two implicit ones are listen ports, already reported.
        let mut device_ports: BTreeMap<u16, bool> = BTreeMap::new();
        for route in &p.proxy {
            let at = format!("{at}.proxy.{}", route.name);

            if route.upstream == Upstream::Port(route.listen) {
                problems.push(format!("{at}: listen and upstream must differ"));
            }

            if let Some(other) = listens.insert(route.listen, at.clone()) {
                problems.push(format!("{at}: port {} is {other}'s too", route.listen));
            }

            if route.device.is_some() && p.driver != DriverKind::Android {
                problems.push(format!(
                    "{at}: device is for Android's routes, which forward ports from the device"
                ));
            }

            let explicit = route.device.is_some();

            if let Some(earlier) = device_ports.insert(route.device_port(), explicit)
                && (earlier || explicit)
            {
                problems.push(format!(
                    "{at}: device port {} listed twice",
                    route.device_port()
                ));
            }
        }
    }

    problems.extend(names_of_others(config));
    problems
}

/// Each block is written out in full, so a new one usually starts as a copy of another: a hook, `device` or `url`
/// that still names the platform it was copied from is the likely leftover.
fn names_of_others(config: &SuiteConfig) -> Vec<String> {
    let mut problems = Vec::new();
    for (&platform, p) in &config.platforms {
        let hooks = &p.hooks;
        let fields = [
            ("hooks.guard", Some(&hooks.guard)),
            ("hooks.seed", Some(&hooks.seed)),
            ("hooks.build-identity", Some(&hooks.build_identity)),
            ("hooks.install", Some(&hooks.install)),
            ("hooks.check", Some(&hooks.check)),
            ("hooks.serve", hooks.serve.as_ref()),
            ("hooks.act", hooks.act.as_ref()),
            ("hooks.stack-up", hooks.stack_up.as_ref()),
            ("hooks.stack-down", hooks.stack_down.as_ref()),
            ("device", Some(&p.device)),
            ("url", p.url.as_ref()),
        ];
        for (field, text) in fields {
            let Some(text) = text else {
                continue;
            };

            let named = words(text);
            for other in config.platforms.keys().filter(|o| **o != platform) {
                if named.contains(other.as_str()) {
                    problems.push(format!(
                        "platforms.{platform}.{field} names {other}, another platform: copied from its block?"
                    ));
                }
            }
        }
    }

    problems
}

/// The whole words in `text` a platform name could be, lowercased: runs of letters, digits and hyphens, without a
/// flag's leading hyphens. `--platform ios-tablet` holds `platform` and `ios-tablet` but not `ios`, and
/// `--skip-web-build` holds only `skip-web-build`; `hooks/web.mjs` and `iPad Pro` hold `web` and `ipad`.
fn words(text: &str) -> BTreeSet<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
        .map(|word| word.trim_matches('-'))
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}
