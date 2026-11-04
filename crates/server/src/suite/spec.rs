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
    pub seed: String,
    /// Where a platform starts when it isn't the app's home screen: a link opened after the seed (on web, a URL or a
    /// path on the base URL's site).
    #[serde(default)]
    pub start: BTreeMap<Platform, String>,
    /// Gone (every spec starts on the home screen or its `start` link), but read so a run's saved copy of an older
    /// suite still opens; lint refuses it in a live suite.
    #[serde(default)]
    pub starts_on: Option<serde_yaml_ng::Value>,
    pub platforms: Vec<Platform>,
    /// Web only: the page's size for this spec, `WIDTHxHEIGHT`, in place of the suite's (a layout only a wide or a
    /// narrow window has).
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
    let front: Frontmatter =
        serde_yaml_ng::from_str(&rest[..end]).map_err(|e| vec![format!("frontmatter: {e}")])?;
    let body = rest.get(end + 5..).unwrap_or("");

    let mut problems = Vec::new();
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
seed: viewer
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
        assert_eq!(spec.front.seed, "viewer");
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
    fn a_start_names_each_platforms_link() {
        let home = parse("annotations/001-draw", "annotations", GOOD).unwrap();
        assert!(home.front.start.is_empty());

        let linked = GOOD.replace(
            "platforms: [android, ios]",
            "platforms: [android, ios]\nstart:\n  ios: sync://collection/1",
        );
        let spec = parse("annotations/001-draw", "annotations", &linked).unwrap();
        assert_eq!(
            spec.front.start.get(&p("ios")).map(String::as_str),
            Some("sync://collection/1")
        );
    }

    #[test]
    fn reports_missing_sections_and_unknown_ones() {
        let text = "---\ntitle: T\nseed: s\nplatforms: [web]\n---\n## Goal\n\n## Steps\nx\n";
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
        let text = "---\ntitle: T\nseed: s\nplatforms: [web]\nchecks: [x]\n---\n";
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
seed: library
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
    fn a_saved_suite_from_before_start_links_still_reads() {
        let old = GOOD.replace(
            "platforms: [android, ios]",
            "starts-on: [Annotate]\nplatforms: [android, ios]",
        );
        let spec = parse("annotations/001-draw", "annotations", &old).unwrap();
        assert!(spec.front.starts_on.is_some());
    }
}
