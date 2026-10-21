use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
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
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SuiteConfig {
    /// The soundcheck version this suite is written for, e.g. `0.1`.
    pub soundcheck: String,
    pub platforms: BTreeMap<Platform, PlatformConfig>,
    pub hooks: Hooks,
    /// Seed scenario names the `seed` hook accepts.
    pub seeds: Vec<String>,
    /// Files testers can `push` onto a device, relative to the suite; defaults to `fixtures/` inside it.
    pub fixtures: Option<PathBuf>,
    /// Where runs are written, relative to the suite; defaults to `reports/` beside the suite folder.
    pub reports: Option<PathBuf>,
    #[serde(default)]
    pub models: Models,
    #[serde(default)]
    pub parallel: bool,
    pub tickets: Option<Tickets>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PlatformConfig {
    pub app_id: Option<String>,
    /// The account this platform signs in with; each platform has its own so they can run in parallel.
    pub account: Option<String>,
    /// Web only.
    pub base_url: Option<String>,
    /// Web only: the command that serves the app under test.
    pub start: Option<String>,
}

/// Shell commands: exit 0 for ok, JSON on stdout where data comes back.
#[derive(Debug, Deserialize)]
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
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
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
