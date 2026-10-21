use super::config::Platform;
use serde::Deserialize;

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
    /// Elements that must be on screen after the seed; a miss is a reset error, not a result.
    #[serde(default)]
    pub starts_on: Vec<String>,
    pub platforms: Vec<Platform>,
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
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(Spec {
        id: id.into(),
        feature: feature.into(),
        front,
        goal: goal.unwrap_or_default(),
        outcomes,
        probe,
        pinned,
    })
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

    const GOOD: &str = "---
title: Draw a pen stroke, undo and redo it
seed: viewer
starts-on: [Annotate]
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
        assert_eq!(spec.front.platforms, vec![Platform::Android, Platform::Ios]);
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
}
