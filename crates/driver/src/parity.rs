//! Parity: does a reader see a screen the way the platform's own tool does (`uiautomator`, `axe`)?

use crate::element::{Element, Rect};

/// Frames closer than this, in the platform's units, are the same frame: tools round differently.
const FRAME_SLACK: f64 = 2.0;

#[derive(Debug, Default)]
pub struct Parity {
    /// Elements only the reader saw.
    pub only_reader: Vec<Element>,
    /// Elements only the platform's tool saw.
    pub only_tool: Vec<Element>,
    /// Elements both saw, as `label: what differs`.
    pub differs: Vec<String>,
    pub matched: usize,
    /// Elements only the tool saw, under a region the reader rightly leaves out (the on-screen keyboard).
    pub covered: Vec<Element>,
}

impl Parity {
    /// Sets aside what only the tool saw inside `region`: a screen reader doesn't get it either.
    pub fn set_aside(&mut self, region: Rect) {
        let inside = |e: &Element| {
            let (x, y) = e.frame.centre();
            x >= region.x
                && x <= region.x + region.width
                && y >= region.y
                && y <= region.y + region.height
        };
        let (covered, rest) = std::mem::take(&mut self.only_tool)
            .into_iter()
            .partition(inside);
        self.covered = covered;
        self.only_tool = rest;
    }

    pub fn clean(&self) -> bool {
        self.only_reader.is_empty() && self.only_tool.is_empty() && self.differs.is_empty()
    }

    pub fn render(&self, tool: &str) -> String {
        let mut out = vec![format!(
            "{}: {} matched, {} only in the reader, {} only in {tool}, {} differ",
            if self.clean() { "parity" } else { "NO PARITY" },
            self.matched,
            self.only_reader.len(),
            self.only_tool.len(),
            self.differs.len()
        )];
        let at = |e: &Element| {
            format!(
                "{} @ {:.0},{:.0} {:.0}x{:.0}",
                e.line(),
                e.frame.x,
                e.frame.y,
                e.frame.width,
                e.frame.height
            )
        };
        out.extend(
            self.only_reader
                .iter()
                .map(|e| format!("  only reader: {}", at(e))),
        );
        out.extend(
            self.only_tool
                .iter()
                .map(|e| format!("  only {tool}: {}", at(e))),
        );
        out.extend(self.differs.iter().map(|d| format!("  differs: {d}")));
        if !self.covered.is_empty() {
            out.push(format!(
                "  set aside, under the keyboard: {}",
                self.covered
                    .iter()
                    .map(|e| e.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out.join("\n")
    }
}

/// Pairs elements by label, in tree order, then compares what a tester relies on: frame, value and states.
pub fn compare(reader: &[Element], tool: &[Element]) -> Parity {
    let mut parity = Parity::default();
    let mut unmatched: Vec<Option<&Element>> = tool.iter().map(Some).collect();
    for r in reader {
        // Same label, nearest frame: a list can repeat a label, and tree order can differ between tools.
        let best = unmatched
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                t.filter(|t| t.label == r.label)
                    .map(|t| (i, distance(r, t)))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));
        let Some((i, _)) = best else {
            parity.only_reader.push(r.clone());
            continue;
        };
        let t = unmatched[i].take().unwrap();
        parity.matched += 1;
        let mut diffs = Vec::new();
        if distance(r, t) > FRAME_SLACK {
            diffs.push(format!(
                "frame {:.0},{:.0} {:.0}x{:.0} vs {:.0},{:.0} {:.0}x{:.0}",
                r.frame.x,
                r.frame.y,
                r.frame.width,
                r.frame.height,
                t.frame.x,
                t.frame.y,
                t.frame.width,
                t.frame.height
            ));
        }
        if r.value != t.value {
            diffs.push(format!("value {:?} vs {:?}", r.value, t.value));
        }
        for (name, a, b) in [
            ("enabled", Some(r.enabled), Some(t.enabled)),
            ("checked", r.checked, t.checked),
            ("selected", r.selected, t.selected),
            ("expanded", r.expanded, t.expanded),
            ("focused", Some(r.focused), Some(t.focused)),
        ] {
            if a != b {
                diffs.push(format!("{name} {a:?} vs {b:?}"));
            }
        }
        if !diffs.is_empty() {
            parity
                .differs
                .push(format!("{}: {}", r.label, diffs.join("; ")));
        }
    }
    parity.only_tool = unmatched.into_iter().flatten().cloned().collect();
    parity
}

fn distance(a: &Element, b: &Element) -> f64 {
    [
        a.frame.x - b.frame.x,
        a.frame.y - b.frame.y,
        a.frame.width - b.frame.width,
        a.frame.height - b.frame.height,
    ]
    .into_iter()
    .map(f64::abs)
    .fold(0.0, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(label: &str, x: f64) -> Element {
        Element {
            label: label.into(),
            frame: Rect {
                x,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
            enabled: true,
            ..Element::default()
        }
    }

    #[test]
    fn identical_trees_have_parity() {
        let tree = [el("Undo", 0.0), el("Redo", 20.0)];
        let p = compare(&tree, &tree);
        assert!(p.clean());
        assert_eq!(p.matched, 2);
    }

    #[test]
    fn missing_elements_show_on_their_side() {
        let p = compare(&[el("Undo", 0.0)], &[el("Undo", 0.0), el("Done", 50.0)]);
        assert!(!p.clean());
        assert_eq!(p.only_tool[0].label, "Done");
        assert!(
            p.render("uiautomator")
                .contains("only uiautomator: Done @ 50,0")
        );
    }

    #[test]
    fn repeated_labels_pair_by_nearest_frame() {
        let reader = [el("Score", 100.0), el("Score", 0.0)];
        let tool = [el("Score", 1.0), el("Score", 99.0)];
        assert!(compare(&reader, &tool).clean());
    }

    #[test]
    fn what_the_keyboard_covers_is_set_aside() {
        let mut p = compare(&[], &[el("Eraser", 0.0), el("Done", 50.0)]);
        p.set_aside(Rect {
            x: 0.0,
            y: 0.0,
            width: 20.0,
            height: 20.0,
        });
        assert_eq!(p.only_tool[0].label, "Done");
        assert_eq!(p.covered[0].label, "Eraser");
        assert!(
            p.render("uiautomator")
                .contains("set aside, under the keyboard: Eraser")
        );
    }

    #[test]
    fn a_state_or_frame_difference_is_named() {
        let mut off = el("Redo", 30.0);
        off.enabled = false;
        let p = compare(&[el("Redo", 20.0)], &[off]);
        assert_eq!(p.differs.len(), 1);
        assert!(p.differs[0].contains("frame 20,0"));
        assert!(p.differs[0].contains("enabled Some(true) vs Some(false)"));
    }
}
