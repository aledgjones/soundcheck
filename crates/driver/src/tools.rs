use crate::Platform;

const ALL: &[Platform] = &[Platform::Android, Platform::Ios, Platform::Web];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Every spec may use it without declaring it.
    Basic,
    /// A mechanic an agent can't perform one call at a time; a spec declares it in `tools`.
    Mechanic,
}

#[derive(Debug)]
pub struct Tool {
    pub name: &'static str,
    pub kind: Kind,
    pub platforms: &'static [Platform],
    pub summary: &'static str,
}

const fn basic(name: &'static str, platforms: &'static [Platform], summary: &'static str) -> Tool {
    Tool {
        name,
        kind: Kind::Basic,
        platforms,
        summary,
    }
}

const fn mechanic(
    name: &'static str,
    platforms: &'static [Platform],
    summary: &'static str,
) -> Tool {
    Tool {
        name,
        kind: Kind::Mechanic,
        platforms,
        summary,
    }
}

/// Every driver tool. The MCP tool list, lint and the tester's brief all read from here.
pub const TOOLS: &[Tool] = &[
    basic(
        "see",
        ALL,
        "list what's on screen, as a screen reader gets it",
    ),
    basic("expect", ALL, "fail unless an element is on screen"),
    basic("gone", ALL, "fail if an element is on screen"),
    basic("check", ALL, "several expect/gone assertions on one read"),
    basic("wait", ALL, "wait for an element to appear"),
    basic(
        "sleep",
        ALL,
        "do nothing for N ms (at most 30000), e.g. to let an animation or a sync settle",
    ),
    basic("shot", ALL, "save a screenshot"),
    basic("errors", ALL, "crashes and app errors since launch"),
    basic("tap", ALL, "tap an element"),
    basic("fill", ALL, "empty a field and type into it"),
    basic("type", ALL, "type into the focused field"),
    basic("clear", ALL, "empty the focused field"),
    basic("swipe", ALL, "swipe between two points"),
    basic("hold", ALL, "press and hold"),
    basic(
        "touch",
        ALL,
        "one finger along a path of stops, resting at each: drag, slide off and back, long-press then drag; `keep` holds it down",
    ),
    basic(
        "release",
        ALL,
        "lift the touch a kept `touch` left down, optionally back at its start",
    ),
    basic("keyboard", ALL, "whether the on-screen keyboard is shown"),
    basic(
        "press",
        ALL,
        "press a hardware key (ArrowRight, Enter, Escape, Home…), as a keyboard or pedal would",
    ),
    basic("launch", ALL, "restart the app"),
    basic(
        "push",
        &[Platform::Android, Platform::Ios],
        "copy a file from the suite's fixtures onto the device, under a name",
    ),
    basic(
        "batch",
        ALL,
        "run steps in order, stopping at the first failure",
    ),
    basic("hover", &[Platform::Web], "hover over an element"),
    basic(
        "goto",
        &[Platform::Web],
        "open a URL, or go back or forward",
    ),
    basic("viewport", &[Platform::Web], "resize the browser viewport"),
    mechanic(
        "double-tap",
        ALL,
        "double-tap an element, at a person's double-tap speed",
    ),
    mechanic(
        "preview-compare",
        ALL,
        "place with a held touch, screenshot the preview, release, and compare where it was saved",
    ),
    mechanic(
        "network-drop",
        ALL,
        "freeze the network for N seconds (nothing passes, nothing closes), then let new connections through",
    ),
    mechanic(
        "offline",
        ALL,
        "take the app offline (connections refused, as in airplane mode) until it is brought back online",
    ),
];

impl Tool {
    /// Its MCP tool name: the same words joined by underscores.
    pub fn mcp_name(&self) -> String {
        self.name.replace('-', "_")
    }
}

pub fn tool(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique() {
        for (i, t) in TOOLS.iter().enumerate() {
            assert!(
                TOOLS[i + 1..].iter().all(|o| o.name != t.name),
                "{} twice",
                t.name
            );
        }
    }
}
