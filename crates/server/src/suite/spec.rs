use super::config::Platform;
use serde::Deserialize;
use std::collections::BTreeMap;

/// One test case: `<feature>/NNN-name.spec.md`. Its path is its identity and its order.
#[derive(Debug, Clone)]
pub struct Spec {
    /// `annotations/001-draw`
    pub id: String,
    pub feature: String,
    pub front: Frontmatter,
    pub goal: String,
    pub outcomes: Vec<String>,
    pub probe: Option<String>,
    /// Exact steps, only where they guard a past bug or reach a control an agent can't discover.
    pub pinned: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Frontmatter {
    pub title: String,
    pub platforms: Vec<Platform>,
    /// Commands run after the fresh relaunch, in order: the spec's data, who is signed in, where it starts
    /// (DESIGN.md 5.6). Empty, the spec starts on a fresh launch.
    #[serde(default)]
    pub prepare: Vec<Step>,
    /// `chrome` only: the page's size for this spec, `WIDTHxHEIGHT`, in place of the platform's (a layout only a wide
    /// or a narrow window has).
    pub viewport: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    /// `software` for a spec that needs the on-screen keyboard; otherwise a hardware keyboard is connected, so typing
    /// is fast and the on-screen one stays hidden.
    #[serde(default)]
    pub keyboard: Keyboard,
    #[serde(default)]
    pub past_bugs: Vec<String>,
    #[serde(default)]
    pub proven_failing_on: Vec<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Keyboard {
    #[default]
    Hardware,
    Software,
}

/// One `prepare` step: a command for every platform the spec lists, or one per platform.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "serde_yaml_ng::Value")]
pub enum Step {
    All(String),
    /// Names every platform the spec lists; `None` is `true`, nothing on that platform.
    Each(BTreeMap<Platform, Option<String>>),
}

impl Step {
    /// The command this step runs on `platform`, if any.
    pub fn on(&self, platform: Platform) -> Option<&str> {
        match self {
            Step::All(command) => Some(command),
            Step::Each(each) => each.get(&platform)?.as_deref(),
        }
    }
}

impl TryFrom<serde_yaml_ng::Value> for Step {
    type Error = String;

    fn try_from(value: serde_yaml_ng::Value) -> Result<Step, String> {
        use serde_yaml_ng::Value;

        let command = |value: &Value| match value {
            Value::String(s) if !s.trim().is_empty() => Ok(Some(s.clone())),
            Value::Bool(true) => Ok(None),
            _ => Err("a command, or `true` for nothing"),
        };
        match &value {
            Value::String(_) => match command(&value) {
                Ok(Some(c)) => Ok(Step::All(c)),
                _ => Err("a prepare step can't be empty".into()),
            },
            Value::Mapping(map) => map
                .iter()
                .map(|(platform, value)| {
                    let name = platform.as_str().unwrap_or_default();
                    // YAML reads an unquoted command holding `: ` as a map, so say so rather than only that the key
                    // isn't a platform.
                    let platform = name.parse::<Platform>().map_err(|e| {
                        format!(
                            "{e}; a command with `: ` in it needs quotes, or YAML reads it as a map"
                        )
                    })?;
                    let command = command(value).map_err(|e| format!("`{name}`: {e}"))?;
                    Ok((platform, command))
                })
                .collect::<Result<_, String>>()
                .map(Step::Each),
            _ => Err("a prepare step is a command, or a map of one per platform".into()),
        }
    }
}

/// 0.2 frontmatter 0.3 refuses by name, and where each went.
const MOVED: &[(&str, &str)] = &[
    ("seed", "now a script the spec's `prepare` runs"),
    (
        "start",
        "now a `soundcheck drive <platform> open` step in `prepare`",
    ),
    (
        "starts-on",
        "gone; `prepare` leaves the app where the spec starts",
    ),
];

pub const SECTIONS: &[&str] = &["Goal", "Outcomes", "Probe around it", "Pinned"];

/// Parses a spec file's text. Errors are lint problems, so they're plain messages.
pub fn parse(id: &str, feature: &str, text: &str) -> Result<Spec, Vec<String>> {
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| vec!["must start with `---` frontmatter".to_string()])?;
    let end = rest
        .find("\n---\n")
        .or_else(|| rest.strip_suffix("\n---").map(str::len))
        .ok_or_else(|| vec!["frontmatter has no closing `---`".to_string()])?;
    let raw: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&rest[..end]).map_err(|e| vec![format!("frontmatter: {e}")])?;
    let moved: Vec<String> = MOVED
        .iter()
        .filter(|(key, _)| raw.get(key).is_some())
        .map(|(key, went)| format!("`{key}`: {went} (see MIGRATIONS.md for 0.2 → 0.3)"))
        .collect();
    if !moved.is_empty() {
        return Err(moved);
    }

    let front: Frontmatter =
        serde_yaml_ng::from_value(raw).map_err(|e| vec![format!("frontmatter: {e}")])?;
    let body = rest.get(end + 5..).unwrap_or("");

    let mut problems = prepare_problems(&front);
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in body.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            sections.push((heading.trim().to_string(), String::new()));
        } else if let Some((_, content)) = sections.last_mut() {
            content.push_str(line);
            content.push('\n');
        } else if !line.trim().is_empty() {
            problems.push(format!("text before the first section: {:?}", line.trim()));
        }
    }
    let mut take = |name: &str| -> Option<String> {
        let found: Vec<_> = sections.iter().filter(|(h, _)| h == name).collect();
        if found.len() > 1 {
            problems.push(format!("`## {name}` appears {} times", found.len()));
        }
        found
            .first()
            .map(|(_, c)| c.trim().to_string())
            .filter(|c| !c.is_empty())
    };
    let goal = take("Goal");
    let outcomes_text = take("Outcomes");
    let probe = take("Probe around it");
    let pinned = take("Pinned");
    for (heading, _) in &sections {
        if !SECTIONS.contains(&heading.as_str()) {
            problems.push(format!(
                "unknown section `## {heading}` (sections: {})",
                SECTIONS.join(", ")
            ));
        }
    }
    if goal.is_none() {
        problems.push("needs a `## Goal` with text".into());
    }
    let outcomes = outcomes_text.as_deref().map(bullets).unwrap_or_default();
    if outcomes.is_empty() {
        problems.push("needs a `## Outcomes` list with at least one `- ` bullet".into());
    }
    // An outcome is judged whole, so a tag inside one would reach every platform as text.
    for line in outcomes_text.iter().flat_map(|t| t.lines()) {
        let nested = line.starts_with(' ').then(|| line.trim_start());
        if let Some(body) = nested.map(|b| b.strip_prefix("- ").unwrap_or(b))
            && tag(body).is_some()
        {
            problems.push(format!(
                "tag a whole outcome, not a line inside one: {:?}",
                line.trim()
            ));
        }
    }
    let spec = Spec {
        id: id.into(),
        feature: feature.into(),
        front,
        goal: goal.unwrap_or_default(),
        outcomes,
        probe,
        pinned,
    };
    if problems.is_empty() {
        problems.extend(spec.tag_problems());
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(spec)
}

/// A map step names every platform the spec lists and no other, and a `soundcheck drive` names the platform its
/// command runs on.
fn prepare_problems(front: &Frontmatter) -> Vec<String> {
    let mut problems = Vec::new();
    for (n, step) in front.prepare.iter().enumerate() {
        let at = format!("prepare step {}", n + 1);
        match step {
            Step::All(command) => {
                for named in drives(command).into_iter().filter(|p| *p != "$") {
                    if front.platforms.len() > 1
                        || front.platforms.iter().all(|p| p.as_str() != named)
                    {
                        problems.push(format!(
                            "{at} drives {named} on every platform the spec lists; give each platform its own \
                             command, or drive $SOUNDCHECK_PLATFORM"
                        ));
                    }
                }
            }
            Step::Each(each) => {
                for platform in &front.platforms {
                    if !each.contains_key(platform) {
                        problems.push(format!(
                            "{at} has nothing for {platform}; give it a command, or `true` for nothing"
                        ));
                    }
                }

                for (platform, command) in each {
                    if !front.platforms.contains(platform) {
                        problems.push(format!(
                            "{at} names {platform}, which the spec doesn't run on"
                        ));
                    }

                    let wrong = command
                        .iter()
                        .flat_map(|c| drives(c))
                        .filter(|named| *named != "$" && *named != platform.as_str());
                    for named in wrong {
                        problems.push(format!("{at} drives {named} in {platform}'s command"));
                    }
                }
            }
        }
    }

    problems
}

/// The platform each `soundcheck drive <platform>` in a command names, quotes and braces stripped.
fn drives(command: &str) -> Vec<&str> {
    let words: Vec<&str> = command.split_whitespace().collect();
    words
        .windows(3)
        .filter(|w| w[0].ends_with("soundcheck") && w[1] == "drive")
        .map(|w| {
            let named = w[2].trim_matches(['"', '\'']);
            if named.contains('$') { "$" } else { named }
        })
        .collect()
}

/// The relative paths a command runs (`node scripts/seed.js viewer` → `scripts/seed.js`), for lint to find on disk:
/// a word with a `/` and a file extension, not a URL, an option or a variable.
pub fn script_paths(command: &str) -> Vec<&str> {
    command
        .split_whitespace()
        .map(|w| w.trim_matches(['"', '\'']))
        .filter(|w| {
            w.contains('/')
                && !w.starts_with(['/', '-', '~'])
                && !w.contains([':', '$', '=', '{'])
                && w.rsplit('/').next().is_some_and(|file| file.contains('.'))
        })
        .collect()
}

impl Spec {
    /// The spec as a tester on `platform` reads it: lines tagged for other platforms gone, its own tags removed.
    pub fn for_platform(&self, platform: Platform) -> Spec {
        let keep = |text: &str| filter_lines(text, platform);
        Spec {
            goal: keep(&self.goal),
            outcomes: self
                .outcomes
                .iter()
                .filter_map(|o| match tag(o) {
                    Some((Ok(tagged), rest)) if tagged.contains(&platform) => {
                        Some(rest.to_string())
                    }
                    Some(_) => None,
                    None => Some(o.clone()),
                })
                .collect(),
            probe: self.probe.as_deref().map(keep).filter(|p| !p.is_empty()),
            pinned: self.pinned.as_deref().map(keep).filter(|p| !p.is_empty()),
            ..self.clone()
        }
    }

    fn tag_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let sections = [Some(&self.goal), self.probe.as_ref(), self.pinned.as_ref()];
        let lines = sections
            .into_iter()
            .flatten()
            .flat_map(|t| t.lines())
            .map(|l| l.trim_start().strip_prefix("- ").unwrap_or(l.trim_start()))
            .chain(self.outcomes.iter().map(String::as_str));
        for line in lines {
            let Some((tagged, _)) = tag(line) else {
                continue;
            };
            match tagged {
                Err(unknown) => problems.push(format!(
                    "`[{unknown}]` isn't a platform tag (a tag names some of the spec's platforms, separated by commas)"
                )),
                Ok(tagged) => {
                    if let Some(p) = tagged.iter().find(|p| !self.front.platforms.contains(p)) {
                        problems.push(format!(
                            "a line is tagged `{p}`, which the spec doesn't run on: {line:?}"
                        ));
                    } else if self.front.platforms.iter().all(|p| tagged.contains(p)) {
                        problems.push(format!(
                            "a line is tagged with every platform the spec runs on; leave the tag off: {line:?}"
                        ));
                    }
                }
            }
        }
        if problems.is_empty() {
            for &p in &self.front.platforms {
                let seen = self.for_platform(p);
                if seen.goal.is_empty() {
                    problems.push(format!("no goal is left for {p}"));
                }
                if seen.outcomes.is_empty() {
                    problems.push(format!("no outcome is left for {p}"));
                }
            }
        }
        problems
    }
}

/// A leading `[web]` or `[android, ios]`: the platforms (or the tag's text when one isn't a platform) and the rest.
/// A `[text](link)` isn't a tag.
fn tag(line: &str) -> Option<(Result<Vec<Platform>, String>, &str)> {
    let inner = line.strip_prefix('[')?;
    let (inside, rest) = inner.split_once(']')?;
    if rest.starts_with('(') {
        return None;
    }
    let platforms = inside
        .split(',')
        .map(|p| p.trim().parse::<Platform>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| inside.to_string());
    Some((platforms, rest.trim_start()))
}

/// Keeps a section's untagged lines and those tagged for `platform`. A dropped line takes the lines indented
/// deeper below it.
fn filter_lines(text: &str, platform: Platform) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut dropped_at: Option<usize> = None;
    for line in text.lines() {
        let indent = &line[..line.len() - line.trim_start().len()];
        if dropped_at.is_some_and(|depth| indent.len() > depth) && !line.trim().is_empty() {
            continue;
        }

        dropped_at = None;
        let body = line.trim_start();
        let (bullet, after) = match body.strip_prefix("- ") {
            Some(after) => ("- ", after),
            None => ("", body),
        };
        match tag(after) {
            Some((Ok(tagged), rest)) if tagged.contains(&platform) => {
                out.push(format!("{indent}{bullet}{rest}"));
            }
            Some(_) => dropped_at = Some(indent.len()),
            None => out.push(line.to_string()),
        }
    }
    out.join("\n").trim().to_string()
}

/// Top-level `- ` bullets; indented lines continue the bullet above.
fn bullets(text: &str) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(item) = line.strip_prefix("- ") {
            items.push(item.trim().to_string());
        } else if let Some(last) = items.last_mut().filter(|_| line.starts_with(' ')) {
            last.push(' ');
            last.push_str(line.trim());
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str) -> Platform {
        name.parse().unwrap()
    }

    const GOOD: &str = "---
title: Draw a pen stroke, undo and redo it
platforms: [android, ios]
---

## Goal

Draw a level stroke, undo it, redo it.

## Outcomes

- The stroke disappears on undo
  and returns on redo.
- Undo and Redo enable to match.

## Probe around it

Undo twice quickly.
";

    #[test]
    fn parses_a_spec() {
        let spec = parse("annotations/001-draw", "annotations", GOOD).unwrap();
        assert!(spec.front.prepare.is_empty());
        assert_eq!(spec.front.platforms, vec![p("android"), p("ios")]);
        assert_eq!(
            spec.outcomes,
            vec![
                "The stroke disappears on undo and returns on redo.",
                "Undo and Redo enable to match."
            ]
        );
        assert_eq!(spec.probe.as_deref(), Some("Undo twice quickly."));
    }

    #[test]
    fn prepare_steps_run_everywhere_or_per_platform() {
        let prepared = GOOD.replace(
            "platforms: [android, ios]",
            "platforms: [android, ios]\nprepare:\n  - node scripts/seed.js viewer\n  \
             - ios: soundcheck drive ios open \"myapp://viewer/$SCORE\"\n    android: true",
        );
        let spec = parse("annotations/001-draw", "annotations", &prepared).unwrap();
        let [seed, open] = &spec.front.prepare[..] else {
            panic!("two steps: {:?}", spec.front.prepare);
        };
        assert_eq!(seed.on(p("android")), Some("node scripts/seed.js viewer"));
        assert_eq!(
            open.on(p("ios")),
            Some("soundcheck drive ios open \"myapp://viewer/$SCORE\"")
        );
        assert_eq!(open.on(p("android")), None, "`true` does nothing");
    }

    #[test]
    fn a_prepare_map_covers_the_specs_platforms_and_drives_its_own() {
        let problems = |prepare: &str| {
            let text = GOOD.replace(
                "platforms: [android, ios]",
                &format!("platforms: [android, ios]\nprepare:\n{prepare}"),
            );
            parse("annotations/001-draw", "annotations", &text).unwrap_err()
        };

        assert_eq!(
            problems("  - ios: soundcheck drive ios relaunch\n"),
            vec![
                "prepare step 1 has nothing for android; give it a command, or `true` for nothing"
            ]
        );
        assert_eq!(
            problems("  - { ios: true, android: true, chrome: true }\n"),
            vec!["prepare step 1 names chrome, which the spec doesn't run on"]
        );
        assert_eq!(
            problems("  - { ios: soundcheck drive android clear, android: true }\n"),
            vec!["prepare step 1 drives android in ios's command"]
        );
        assert!(
            problems("  - soundcheck drive ios relaunch\n")[0]
                .contains("drive $SOUNDCHECK_PLATFORM")
        );
        assert!(problems("  - { ios: false, android: true }\n")[0].contains("`true` for nothing"));
        assert!(
            problems("  - node scripts/seed.js --label: viewer\n")[0].contains("needs quotes"),
            "an unquoted `: ` makes a map"
        );

        let shared = GOOD.replace(
            "platforms: [android, ios]",
            "platforms: [android, ios]\nprepare:\n  - soundcheck drive \"$SOUNDCHECK_PLATFORM\" relaunch",
        );
        assert!(parse("annotations/001-draw", "annotations", &shared).is_ok());
    }

    #[test]
    fn a_script_path_is_a_relative_file() {
        assert_eq!(
            script_paths("node scripts/seed.js --seed new-user && ./bin/sign-in.sh x/y"),
            vec!["scripts/seed.js", "./bin/sign-in.sh"]
        );
        assert!(
            script_paths("soundcheck drive chrome open /app/viewer/$SCORE https://x.io/a.js")
                .is_empty()
        );
    }

    #[test]
    fn reports_missing_sections_and_unknown_ones() {
        let text = "---\ntitle: T\nplatforms: [web]\n---\n## Goal\n\n## Steps\nx\n";
        let problems = parse("f/001-x", "f", text).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("unknown section `## Steps`"))
        );
        assert!(problems.iter().any(|p| p.contains("`## Goal`")));
        assert!(problems.iter().any(|p| p.contains("`## Outcomes`")));
    }

    #[test]
    fn rejects_unknown_frontmatter_keys() {
        let text = "---\ntitle: T\nplatforms: [web]\nchecks: [x]\n---\n";
        let problems = parse("f/001-x", "f", text).unwrap_err();
        assert!(
            problems[0].contains("unknown field `checks`"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_viewport_is_width_by_height() {
        use crate::suite::config::parse_viewport;
        assert_eq!(parse_viewport("1440x900"), Some((1440, 900)));
        assert_eq!(parse_viewport("wide"), None);
        assert_eq!(parse_viewport("100x900"), None, "narrower than any phone");

        let sized = GOOD.replace(
            "platforms: [android, ios]",
            "platforms: [web]\nviewport: 1440x900",
        );
        let spec = parse("annotations/001-draw", "annotations", &sized).unwrap();
        assert_eq!(spec.front.viewport.as_deref(), Some("1440x900"));
    }

    const TAGGED: &str = "---
title: Delete a score
platforms: [android, ios, web]
---

## Goal

Delete Beta while organising.
[web] Tick its box and use the selection's Delete.

## Outcomes

- [android, ios] While organising, a tick box tap ticks the row
  instead of opening it.
- [web] A tick box tap ticks the row.
- Beta is gone.

## Probe around it

- Cancel once.
- [web] Press Escape
  on the confirm.
";

    #[test]
    fn each_platform_reads_its_own_lines() {
        let spec = parse("scores/005-delete", "scores", TAGGED).unwrap();
        let ios = spec.for_platform(p("ios"));
        assert_eq!(ios.goal, "Delete Beta while organising.");
        assert_eq!(
            ios.outcomes,
            vec![
                "While organising, a tick box tap ticks the row instead of opening it.",
                "Beta is gone."
            ]
        );
        assert_eq!(ios.probe.as_deref(), Some("- Cancel once."));

        let web = spec.for_platform(p("web"));
        assert_eq!(
            web.goal,
            "Delete Beta while organising.\nTick its box and use the selection's Delete."
        );
        assert_eq!(
            web.outcomes,
            vec!["A tick box tap ticks the row.", "Beta is gone."]
        );
        assert_eq!(
            web.probe.as_deref(),
            Some("- Cancel once.\n- Press Escape\n  on the confirm.")
        );
    }

    #[test]
    fn a_dropped_line_takes_only_deeper_lines() {
        let nested = TAGGED.replace(
            "- Cancel once.\n",
            "- Drag a row\n  - [web] Hover first\n    over it.\n  - Then count the rows\n",
        );
        let spec = parse("scores/005-delete", "scores", &nested).unwrap();
        assert_eq!(
            spec.for_platform(p("ios")).probe.as_deref(),
            Some("- Drag a row\n  - Then count the rows")
        );
    }

    #[test]
    fn a_link_at_the_start_of_a_line_isnt_a_tag() {
        let linked = TAGGED.replace("- Beta is gone.", "- [Beta](https://x) is gone.");
        let spec = parse("scores/005-delete", "scores", &linked).unwrap();
        assert_eq!(spec.for_platform(p("ios")).outcomes.len(), 2);
    }

    #[test]
    fn lint_checks_the_tags() {
        let problems = |text: &str| parse("scores/005-delete", "scores", text).unwrap_err();

        let unknown = problems(&TAGGED.replace("[web] Tick", "[Desktop] Tick"));
        assert!(
            unknown[0].contains("`[Desktop]` isn't a platform tag"),
            "{unknown:?}"
        );

        let elsewhere = problems(&TAGGED.replace("[web] Tick", "[desktop] Tick"));
        assert!(
            elsewhere[0].contains("tagged `desktop`, which the spec doesn't run on"),
            "{elsewhere:?}"
        );

        let unlisted =
            problems(&TAGGED.replace("platforms: [android, ios, web]", "platforms: [ios, web]"));
        assert!(unlisted[0].contains("tagged `android`"), "{unlisted:?}");

        let everywhere =
            problems(&TAGGED.replace("- Beta is gone.", "- [android, ios, web] Beta is gone."));
        assert!(everywhere[0].contains("every platform"), "{everywhere:?}");

        let emptied = problems(
            &TAGGED
                .replace("- [web] A tick box tap ticks the row.\n", "")
                .replace("- Beta is gone.\n", "- [ios] Beta is gone.\n"),
        );
        assert_eq!(emptied, vec!["no outcome is left for web"]);

        let unspaced = parse(
            "scores/005-delete",
            "scores",
            &TAGGED.replace("[web] Tick", "[web]Tick"),
        )
        .unwrap();
        assert!(!unspaced.for_platform(p("ios")).goal.contains("Tick"));

        let inside = problems(&TAGGED.replace("  instead of opening it.", "  - [ios] instead."));
        assert!(inside[0].contains("tag a whole outcome"), "{inside:?}");

        let goalless = problems(
            &TAGGED
                .replace(
                    "Delete Beta while organising.\n",
                    "[android, ios] Delete Beta.\n",
                )
                .replace("[web] Tick", "[ios] Tick"),
        );
        assert_eq!(goalless, vec!["no goal is left for web"]);
    }

    #[test]
    fn the_0_2_frontmatter_is_refused_by_name() {
        let old = GOOD.replace(
            "platforms: [android, ios]",
            "seed: viewer\nstart: { ios: \"sync://x\" }\nstarts-on: [Annotate]\nplatforms: [android, ios]",
        );
        let problems = parse("annotations/001-draw", "annotations", &old).unwrap_err();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(
            problems[0].starts_with("`seed`: now a script"),
            "{problems:?}"
        );
        assert!(problems.iter().all(|p| p.contains("MIGRATIONS.md")));
    }
}
