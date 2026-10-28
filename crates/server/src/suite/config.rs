use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use soundcheck_driver::proxy::Upstream;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Android,
    Ios,
    Web,
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Platform::Android => "android",
            Platform::Ios => "ios",
            Platform::Web => "web",
        })
    }
}

impl std::str::FromStr for Platform {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "android" => Ok(Platform::Android),
            "ios" => Ok(Platform::Ios),
            "web" => Ok(Platform::Web),
            other => Err(format!("{other} isn't a platform (android, ios, web)")),
        }
    }
}

/// `suite.yaml`: everything a project tells soundcheck about itself.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SuiteConfig {
    /// The soundcheck version this suite is written for, e.g. `0.1`.
    pub soundcheck: String,
    pub platforms: BTreeMap<Platform, PlatformConfig>,
    pub hooks: Hooks,
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
    /// How the app reaches its services on this machine, so `offline` and `network-drop` can cut them.
    pub network: Option<Network>,
    /// Who may open the live page through `--remote`'s tunnel.
    pub remote: Option<Remote>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    /// Google account emails let past the tunnel's sign-in.
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub proxy: Proxy,
}

/// One list of routes every platform shares, or a list per platform, so each platform's app reaches its own
/// backend through its own proxy, and taking one platform offline leaves the others alone.
#[derive(Debug, Clone)]
pub enum Proxy {
    Shared(Vec<ProxyRoute>),
    PerPlatform(BTreeMap<Platform, Vec<ProxyRoute>>),
}

// By hand rather than untagged, which would hide a route's own error (a bad upstream) behind "no variant matched".
impl<'de> Deserialize<'de> for Proxy {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Proxy, D::Error> {
        use serde::de::value::{MapAccessDeserializer, SeqAccessDeserializer};
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = Proxy;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a list of routes, or a list per platform")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, seq: A) -> Result<Proxy, A::Error> {
                Deserialize::deserialize(SeqAccessDeserializer::new(seq)).map(Proxy::Shared)
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Proxy, A::Error> {
                Deserialize::deserialize(MapAccessDeserializer::new(map)).map(Proxy::PerPlatform)
            }
        }
        d.deserialize_any(Visit)
    }
}

impl Proxy {
    /// The routes `platform`'s app goes through: none for a platform a per-platform proxy doesn't list.
    pub fn routes(&self, platform: Platform) -> &[ProxyRoute] {
        match self {
            Proxy::Shared(routes) => routes,
            Proxy::PerPlatform(by) => by.get(&platform).map_or(&[], Vec::as_slice),
        }
    }

    /// Every route, with the platform it belongs to (none when shared).
    pub fn all(&self) -> Vec<(Option<Platform>, &ProxyRoute)> {
        match self {
            Proxy::Shared(routes) => routes.iter().map(|r| (None, r)).collect(),
            Proxy::PerPlatform(by) => by
                .iter()
                .flat_map(|(p, routes)| routes.iter().map(move |r| (Some(*p), r)))
                .collect(),
        }
    }
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PlatformConfig {
    pub app_id: Option<String>,
    /// The account this platform signs in with; each platform has its own so they can run in parallel.
    pub account: Option<String>,
    /// Web only.
    pub base_url: Option<String>,
    /// Web only: the command that serves the app under test.
    pub start: Option<String>,
    /// Labels that prove the app is ready to test once launched (signed in, past onboarding). The run checks them
    /// before it starts, so a signed-out device stops it there rather than failing every spec.
    #[serde(default)]
    pub ready: Vec<String>,
}

/// Shell commands: exit 0 for ok, JSON on stdout where data comes back.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Hooks {
    pub guard: String,
    /// Must contain `{scenario}`.
    pub seed: String,
    pub build_identity: String,
    pub install: Option<String>,
    /// Read-only data checks testers can ask for: `check {name} [args]`.
    pub check: Option<String>,
    /// The other side acting while a tester watches (another user revokes access): `act {name} [args]`.
    pub act: Option<String>,
    /// Starts a blank backend for one platform (`{platform}`) and returns once it answers. Runs at server start.
    pub stack_up: Option<String>,
    /// Removes that platform's backend with its data; must succeed when it's already gone.
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
    use super::compatible;

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
}
