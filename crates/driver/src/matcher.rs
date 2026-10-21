use crate::element::Element;
use regex::Regex;

/// How a target names an element. Exact unless asked otherwise: an implicit partial match once let `gone "Shuffle
/// Order"` pass while `Shuffle Order…` was on screen.
#[derive(Debug, Clone)]
pub enum Matcher {
    Exact(String),
    Prefix(String),
    Regex(Regex),
}

impl Matcher {
    /// `re:<pattern>` for a regex, `prefix:<text>` for a prefix, anything else exact.
    pub fn parse(target: &str) -> Result<Matcher, regex::Error> {
        if let Some(pattern) = target.strip_prefix("re:") {
            Ok(Matcher::Regex(Regex::new(pattern)?))
        } else if let Some(text) = target.strip_prefix("prefix:") {
            Ok(Matcher::Prefix(text.to_string()))
        } else {
            Ok(Matcher::Exact(target.to_string()))
        }
    }

    fn test(&self, s: &str) -> bool {
        match self {
            Matcher::Exact(t) => s == t,
            Matcher::Prefix(t) => s.starts_with(t.as_str()),
            Matcher::Regex(r) => r.is_match(s),
        }
    }

    /// Matches the label, value or id, or the whole `see` line, so a line copied from `see` names the same element.
    pub fn matches(&self, e: &Element) -> bool {
        self.test(&e.label)
            || e.value.as_deref().is_some_and(|v| self.test(v))
            || e.id.as_deref().is_some_and(|v| self.test(v))
            || self.test(&e.line())
            || self.placeholder_label(e)
    }

    /// iOS appends a text field's placeholder to its label ("Feedback message Message"), so a field's own label
    /// matches exactly at the start of it.
    fn placeholder_label(&self, e: &Element) -> bool {
        let Matcher::Exact(t) = self else {
            return false;
        };
        matches!(
            e.role.as_deref(),
            Some("TextField" | "TextArea" | "SecureTextField")
        ) && e
            .label
            .strip_prefix(t.as_str())
            .is_some_and(|rest| rest.starts_with(' '))
    }
}

impl std::fmt::Display for Matcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Matcher::Exact(t) => write!(f, "{t:?}"),
            Matcher::Prefix(t) => write!(f, "prefix {t:?}"),
            Matcher::Regex(r) => write!(f, "regex /{}/", r.as_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(label: &str) -> Element {
        Element {
            label: label.into(),
            enabled: true,
            ..Element::default()
        }
    }

    #[test]
    fn exact_is_the_default() {
        let m = Matcher::parse("Shuffle Order").unwrap();
        assert!(!m.matches(&el("Shuffle Order…")));
        assert!(m.matches(&el("Shuffle Order")));
        assert!(
            Matcher::parse("prefix:Shuffle Order")
                .unwrap()
                .matches(&el("Shuffle Order…"))
        );
        assert!(
            Matcher::parse("re:^Shuffle")
                .unwrap()
                .matches(&el("Shuffle Order…"))
        );
    }

    #[test]
    fn a_see_line_names_its_element() {
        let mut e = el("Artist");
        e.value = Some("Bach".into());
        assert!(Matcher::parse("Artist = Bach").unwrap().matches(&e));
        assert!(Matcher::parse("Bach").unwrap().matches(&e));
    }
}
