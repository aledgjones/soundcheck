use serde::{Deserialize, Serialize};
use soundcheck_driver::proxy::Upstream;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;

/// A platform's name, as the suite defines it (`ios`, `iphone`, `web`). What it runs on is its block's `driver`.
///
/// Anything outside suite.yaml that names a platform (a spec, a line tag, a tool call, a command) goes through
/// [`SuiteConfig::platform`], so a name the suite doesn't define is refused there, with the names it does.
///
/// Names are interned, so a platform is `Copy` like the enum it replaced: a suite has a handful, kept once each for
/// the life of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Platform(&'static str);

impl Platform {
    pub fn as_str(&self) -> &'static str {
        self.0
    }

    /// A name fixed in code, for tests and examples (`const IOS: Platform = Platform::literal("ios")`). Names read
    /// from anywhere else are parsed, and resolved through [`SuiteConfig::platform`].
    pub const fn literal(name: &'static str) -> Platform {
        Platform(name)
    }
}

fn intern(name: &str) -> &'static str {
    static NAMES: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());
    let mut names = NAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    if let Some(known) = names.get(name) {
        return known;
    }

    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    names.insert(leaked);
    leaked
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// A well-formed name: lowercase letters, digits and hyphens, starting with a letter. Whether the suite defines it is
/// [`SuiteConfig::platform`]'s question.
impl std::str::FromStr for Platform {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut chars = s.chars();
        let well_formed = chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');

        if !well_formed {
            return Err(format!(
                "{s:?} isn't a platform name (lowercase letters, digits and hyphens, starting with a letter)"
            ));
        }

        Ok(Platform(intern(s)))
    }
}

/// A name in what soundcheck reports (a problem's platform); a tool's own arguments take a plain string, resolved
/// through [`SuiteConfig::platform`].
impl schemars::JsonSchema for Platform {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Platform".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        String::json_schema(generator)
    }
}

impl Serialize for Platform {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for Platform {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Platform, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// How soundcheck drives a platform (section 4 of DESIGN.md): the driver crate's own type, so suite.yaml's
/// `driver:` and the tools' lists of drivers are one name.
pub use soundcheck_driver::DriverKind;

/// `suite.yaml`: everything a project tells soundcheck about itself.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SuiteConfig {
    /// The soundcheck version this suite is written for, e.g. `0.3`.
    pub soundcheck: String,
    /// Each platform's whole map: nothing about one platform is shared from the top level.
    pub platforms: BTreeMap<Platform, PlatformConfig>,
    /// Seed scenario names the `seed` hook accepts.
    pub seeds: Vec<String>,
    /// The folder holding the feature folders, relative to the suite; defaults to the suite folder itself.
    pub specs: Option<PathBuf>,
    /// Files testers can `push` onto a device, relative to the suite; defaults to `fixtures/` inside it.
    pub fixtures: Option<PathBuf>,
    /// Where runs are written, relative to the suite; defaults to `reports/` beside the suite folder.
    pub reports: Option<PathBuf>,
    #[serde(default)]
    pub models: Models,
    #[serde(default)]
    pub parallel: bool,
    pub tickets: Option<Tickets>,
    /// Who may open the live page through `--remote`'s tunnel.
    pub remote: Option<Remote>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PlatformConfig {
    pub driver: DriverKind,
    /// What the app runs on: a simulator's name or UDID (`ios`), an AVD name or adb serial (`android`), `desktop`
    /// (`chrome`).
    pub device: String,
    /// `ios`, `android`: the app ID the driver launches.
    pub app: Option<String>,
    /// `chrome`: the window's size at every reset, `WIDTHxHEIGHT`, unless a spec sets its own.
    pub viewport: Option<String>,
    /// `chrome`: where a launch opens, and the site a spec's `start` path is on.
    pub url: Option<String>,
    /// `chrome`: a Chrome profile signed in to Google by hand (`soundcheck chrome-profile`); each run starts from a
    /// copy. An absolute path, or `~/` for the home folder.
    pub chrome_profile: Option<String>,
    pub hooks: Hooks,
    /// How the app reaches its services on this machine, so `offline` and `network-drop` can cut them.
    #[serde(default)]
    pub proxy: Vec<ProxyRoute>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    /// Google account emails let past the tunnel's sign-in.
    pub allow: Vec<String>,
}

/// The app's test build reaches `listen`; `upstream` really serves it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyRoute {
    pub name: String,
    pub listen: u16,
    /// Android only: the port the app uses on the device, forwarded to `listen` on this machine (default: the same
    /// port). It lets each platform's proxy listen on its own ports while every build uses the same ones.
    pub device: Option<u16>,
    #[serde(deserialize_with = "upstream")]
    pub upstream: Upstream,
}

impl ProxyRoute {
    pub fn device_port(&self) -> u16 {
        self.device.unwrap_or(self.listen)
    }
}

/// A port on this machine, or a remote service's URL.
fn upstream<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Upstream, D::Error> {
    struct Visit;
    impl serde::de::Visitor<'_> for Visit {
        type Value = Upstream;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a port or a URL like https://api.example.com")
        }
        fn visit_u64<E: serde::de::Error>(self, n: u64) -> Result<Upstream, E> {
            u16::try_from(n)
                .map(Upstream::Port)
                .map_err(|_| E::custom(format!("{n} isn't a port")))
        }
        fn visit_i64<E: serde::de::Error>(self, n: i64) -> Result<Upstream, E> {
            u64::try_from(n)
                .map_err(|_| E::custom(format!("{n} isn't a port")))
                .and_then(|n| self.visit_u64(n))
        }
        fn visit_str<E: serde::de::Error>(self, url: &str) -> Result<Upstream, E> {
            Upstream::parse(url).map_err(E::custom)
        }
    }
    d.deserialize_any(Visit)
}

impl SuiteConfig {
    /// Reads suite.yaml. 0.2's shape is refused key by key, each saying where it went, before the 0.3 shape is read.
    pub fn parse(text: &str) -> Result<SuiteConfig, Vec<String>> {
        let value: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(text).map_err(|e| vec![e.to_string()])?;
        let old = old_shape(&value);

        if !old.is_empty() {
            return Err(old);
        }

        // From the text again rather than the value, so an error says where it is.
        serde_yaml_ng::from_str(text).map_err(|e| vec![e.to_string()])
    }

    /// The platform this suite calls `name`, or why there's none: the names it defines, and the nearest one.
    pub fn platform(&self, name: &str) -> Result<Platform, String> {
        if let Some(platform) = self.platforms.keys().find(|p| p.as_str() == name).copied() {
            return Ok(platform);
        }

        let names: Vec<&str> = self.platforms.keys().map(Platform::as_str).collect();
        let nearest = names
            .iter()
            .map(|n| (strsim::levenshtein(name, n), *n))
            .filter(|(distance, _)| *distance <= 2)
            .min()
            .map(|(_, n)| format!("; did you mean {n}?"))
            .unwrap_or_default();
        Err(format!(
            "{name} isn't one of suite.yaml's platforms ({}){nearest}",
            names.join(", ")
        ))
    }

    /// The block for a platform the suite defines. A [`Platform`] comes from the suite, so it's there.
    pub fn of(&self, platform: &Platform) -> &PlatformConfig {
        self.platforms.get(platform).unwrap_or_else(|| {
            panic!("{platform} isn't in suite.yaml; names go through SuiteConfig::platform")
        })
    }

    pub fn driver(&self, platform: &Platform) -> DriverKind {
        self.of(platform).driver
    }

    /// The `chrome` platform's site: its `serve` hook and the `url` it serves at. One web app a run, while lint allows
    /// one platform per driver (SA-880 step 5 lifts that).
    pub fn served(&self) -> Option<Served> {
        self.platforms
            .iter()
            .filter(|(_, p)| p.driver == DriverKind::Chrome)
            .find_map(|(platform, p)| {
                Some(Served {
                    platform: *platform,
                    command: p.hooks.serve.clone()?,
                    url: p.url.clone()?,
                })
            })
    }
}

/// The site a `chrome` platform serves: its platform, `serve` hook and `url`.
#[derive(Debug, Clone)]
pub struct Served {
    pub platform: Platform,
    pub command: String,
    pub url: String,
}

/// 0.2 keys 0.3 refuses by name, and where each went (MIGRATIONS.md has the whole step).
const MOVED_TOP: &[(&str, &str)] = &[
    ("hooks", "each platform has its own `hooks`"),
    ("network", "each platform has its own `proxy` routes"),
    ("prepare", "gone; every spec starts from its own seed"),
];
const MOVED_PLATFORM: &[(&str, &str)] = &[
    ("prepare", "gone; every spec starts from its own seed"),
    ("ready", "gone; each spec names its `start`"),
    ("account", "gone; the platform's hooks name their account"),
    ("app-id", "now `app`"),
    ("base-url", "now `url`"),
    ("profile", "now `chrome-profile`"),
    ("start", "now the `serve` hook"),
];

fn old_shape(value: &serde_yaml_ng::Value) -> Vec<String> {
    let migrate = "see MIGRATIONS.md for 0.2 → 0.3";
    let mut problems = Vec::new();
    for (key, went) in MOVED_TOP {
        if value.get(key).is_some() {
            problems.push(format!("{key}: {went} ({migrate})"));
        }
    }

    let platforms = value
        .get("platforms")
        .and_then(serde_yaml_ng::Value::as_mapping);
    for (name, block) in platforms.into_iter().flatten() {
        let name = name.as_str().unwrap_or("?");
        for (key, went) in MOVED_PLATFORM {
            if block.get(key).is_some() {
                problems.push(format!("platforms.{name}.{key}: {went} ({migrate})"));
            }
        }

        for (key, what) in [
            ("driver", "ios, android or chrome"),
            ("device", "a simulator, emulator or `desktop`"),
        ] {
            if block.get(key).is_none() {
                problems.push(format!(
                    "platforms.{name}: needs `{key}`, {what}; 0.2 took it from the platform's name ({migrate})"
                ));
            }
        }

        let hooks = block
            .get("hooks")
            .and_then(serde_yaml_ng::Value::as_mapping);
        for (hook, command) in hooks.into_iter().flatten() {
            let command = command.as_str().unwrap_or_default();
            for placeholder in ["{platform}", "{account}"] {
                if command.contains(placeholder) {
                    problems.push(format!(
                        "platforms.{name}.hooks.{}: {placeholder} is gone; the command names it itself ({migrate})",
                        hook.as_str().unwrap_or("?")
                    ));
                }
            }
        }
    }

    problems
}

impl PlatformConfig {
    pub fn profile_dir(&self) -> Option<PathBuf> {
        let profile = self.chrome_profile.as_deref()?;
        Some(match profile.strip_prefix("~/") {
            Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
            None => PathBuf::from(profile),
        })
    }

    /// `chrome`'s window size; lint has checked it parses.
    pub fn viewport(&self) -> Option<(u32, u32)> {
        self.viewport.as_deref().and_then(parse_viewport)
    }
}

/// `1440x900` as a width and height, each at least 320.
pub fn parse_viewport(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.trim().split_once('x')?;
    let (width, height) = (width.parse().ok()?, height.parse().ok()?);
    if width < 320 || height < 320 {
        return None;
    }

    Some((width, height))
}

/// Shell commands: exit 0 for ok, JSON on stdout where data comes back. Each names its platform, stack and account
/// itself; the only placeholders are the call's own arguments.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Hooks {
    pub guard: String,
    /// Must contain `{scenario}`.
    pub seed: String,
    pub build_identity: String,
    /// Makes the build current; skipped when `build-identity` says it is.
    pub install: String,
    /// Read-only data checks testers can ask for: `check {name} [args]`.
    pub check: String,
    /// Keeps what the platform needs running (on `chrome`, the site at `url`); restarted after every install.
    pub serve: Option<String>,
    /// The other side acting while a tester watches (another user revokes access): `act {name} [args]`.
    pub act: Option<String>,
    /// Starts this platform's blank backend and returns once it answers. Runs at server start.
    pub stack_up: Option<String>,
    /// Removes that backend with its data; must succeed when it's already gone.
    pub stack_down: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Models {
    pub default: String,
}

impl Default for Models {
    fn default() -> Self {
        Models {
            default: "sonnet".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tickets {
    /// A link with `{id}` in place of the ticket ID.
    pub url: String,
}

/// A pin is compatible when it names this version's major and minor (before 1.0) or its major (from 1.0).
pub fn compatible(pin: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> { v.split('.').map(|p| p.parse().ok()).collect() };
    let (Some(pin), Some(current)) = (parse(pin), parse(current)) else {
        return false;
    };
    if pin.is_empty() || current.len() < 2 {
        return false;
    }

    let significant = if current[0] == 0 { 2 } else { 1 };
    pin.len() >= significant && pin[..significant] == current[..significant]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_match_on_minor_before_one_and_major_after() {
        assert!(compatible("0.1", "0.1.3"));
        assert!(compatible("0.1.0", "0.1.3"));
        assert!(!compatible("0.2", "0.1.3"));
        assert!(!compatible("0", "0.1.3"));
        assert!(compatible("1", "1.4.0"));
        assert!(compatible("1.2", "1.4.0"));
        assert!(!compatible("2", "1.4.0"));
        assert!(!compatible("latest", "0.1.0"));
    }

    const HOOKS: &str =
        "{ guard: g, seed: \"s {scenario}\", build-identity: b, install: i, check: c }";

    fn config(platforms: &str) -> Result<SuiteConfig, Vec<String>> {
        SuiteConfig::parse(&format!(
            "soundcheck: \"0.3\"\nseeds: [empty]\nplatforms:\n{platforms}"
        ))
    }

    #[test]
    fn platforms_are_the_suites_own_names() {
        let config = config(&format!(
            "  ipad: {{ driver: ios, device: iPad, app: a, hooks: {HOOKS} }}\n  \
             iphone: {{ driver: ios, device: iPhone, app: a, hooks: {HOOKS} }}\n"
        ))
        .unwrap();
        let ipad = config.platform("ipad").unwrap();
        assert_eq!(config.driver(&ipad), DriverKind::Ios);
        assert_eq!(
            config.platform("ipda").unwrap_err(),
            "ipda isn't one of suite.yaml's platforms (ipad, iphone); did you mean ipad?"
        );
        assert_eq!(
            config.platform("android").unwrap_err(),
            "android isn't one of suite.yaml's platforms (ipad, iphone)"
        );
    }

    #[test]
    fn a_platform_name_is_lowercase_words() {
        assert!("iphone-17".parse::<Platform>().is_ok());
        assert!("iPhone".parse::<Platform>().is_err());
        assert!("17".parse::<Platform>().is_err());
        assert!(
            config(&format!(
                "  Web: {{ driver: chrome, device: desktop, hooks: {HOOKS} }}\n"
            ))
            .is_err()
        );
    }

    #[test]
    fn the_old_shape_is_refused_by_name() {
        let problems = SuiteConfig::parse(
            "soundcheck: \"0.2\"\nseeds: [s]\nhooks: { guard: g }\nprepare: prepare.md\n\
             platforms:\n  web:\n    base-url: http://x\n    ready: [Library]\n    \
             hooks: { seed: \"s {platform}\" }\n",
        )
        .unwrap_err();
        let joined = problems.join("\n");
        for expected in [
            "hooks: each platform has its own `hooks`",
            "prepare: gone",
            "platforms.web.base-url: now `url`",
            "platforms.web.ready: gone",
            "platforms.web.hooks.seed: {platform} is gone",
            "MIGRATIONS.md",
        ] {
            assert!(joined.contains(expected), "{expected:?} in {joined}");
        }
    }

    #[test]
    fn every_platform_says_its_driver_and_device() {
        let problems = config(&format!("  ios: {{ app: a, hooks: {HOOKS} }}\n")).unwrap_err();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems[0].starts_with("platforms.ios: needs `driver`")
                && problems[0].contains("MIGRATIONS.md"),
            "{problems:?}"
        );
        assert!(
            problems[1].starts_with("platforms.ios: needs `device`"),
            "{problems:?}"
        );
    }
}
