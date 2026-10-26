/// In the platform's own units: pixels on Android, points on iOS, CSS pixels on web.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn centre(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }
}

/// One element as a screen reader gets it, the same shape on every platform.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Element {
    /// What's announced first: the accessibility label, else the text, else the id.
    pub label: String,
    pub value: Option<String>,
    pub id: Option<String>,
    /// Button, switch, text field, link…
    pub role: Option<String>,
    pub frame: Rect,
    pub enabled: bool,
    pub checked: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub focused: bool,
    /// The visible area of the scroll view that clips it out of view, when it's scrolled out: a tap scrolls it in.
    pub scroll: Option<Rect>,
}

impl Element {
    pub fn tap_point(&self) -> (f64, f64) {
        self.frame.centre()
    }

    /// How `see` prints it: `label = value` when the value differs, then its states.
    pub fn line(&self) -> String {
        let mut line = self.label.clone();
        if let Some(value) = self
            .value
            .as_deref()
            .filter(|v| *v != self.label && !v.is_empty())
        {
            line.push_str(" = ");
            line.push_str(value);
        }
        let mut states = Vec::new();
        if !self.enabled {
            states.push("disabled");
        }
        match self.checked {
            Some(true) => states.push("checked"),
            Some(false) => states.push("unchecked"),
            None => {}
        }
        if self.selected == Some(true) {
            states.push("selected");
        }
        match self.expanded {
            Some(true) => states.push("expanded"),
            Some(false) => states.push("collapsed"),
            None => {}
        }
        if self.focused {
            states.push("focused");
        }
        if self.scroll.is_some() {
            states.push("off view: a tap scrolls to it");
        }
        if !states.is_empty() {
            line.push_str(&format!(" ({})", states.join(", ")));
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_shows_value_and_states() {
        let e = Element {
            label: "Send diagnostic info".into(),
            value: Some("Send diagnostic info".into()),
            role: Some("switch".into()),
            enabled: true,
            checked: Some(true),
            ..Element::default()
        };
        assert_eq!(e.line(), "Send diagnostic info (checked)");
        let field = Element {
            label: "Title".into(),
            value: Some("Opener".into()),
            enabled: false,
            ..Element::default()
        };
        assert_eq!(field.line(), "Title = Opener (disabled)");
    }
}
