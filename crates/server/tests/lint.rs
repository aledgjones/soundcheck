use soundcheck::lint::lint;
use soundcheck::suite::{self, Problem};
use std::path::Path;

fn problems(fixture: &str) -> Vec<Problem> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    lint(&suite::load(&root), "0.1.0")
}

#[test]
fn a_good_suite_has_no_problems() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/good");
    let suite = suite::load(&root);
    let ids: Vec<_> = suite.specs.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["annotations/001-draw", "annotations/002-back"]);
    assert_eq!(lint(&suite, "0.1.0"), vec![]);
}

#[test]
fn a_broken_suite_reports_every_rule() {
    let found = problems("broken");
    let report: Vec<String> = found
        .iter()
        .map(|p| format!("{}: {}", p.path.display(), p.message))
        .collect();
    let expect = [
        "suite.yaml: pinned to soundcheck 0.2",
        "suite.yaml: platforms.android: needs app-id",
        "suite.yaml: platforms.web: needs start",
        "suite.yaml: hooks.seed must contain {scenario}",
        "suite.yaml: seeds: list the seed scenarios",
        "suite.yaml: models.default: gpt",
        "stray.spec.md: specs belong in a feature folder",
        "Bad_Feature: feature folders are lower-case",
        "annotations/02-misnamed.spec.md: spec files are named NNN-name.spec.md",
        "annotations/003-empty.spec.md: needs a `## Goal` with text",
        "annotations/003-empty.spec.md: needs a `## Outcomes` list",
        "annotations/_feature.md: every feature needs a _feature.md",
        "Bad_Feature/_feature.md: every feature needs a _feature.md",
        "annotations/001-noisy.spec.md: title: keep it under 80",
        "annotations/001-noisy.spec.md: title: no code formatting",
        "annotations/001-noisy.spec.md: title: no tap paths",
        "annotations/001-noisy.spec.md: title: no references to other specs",
        "annotations/001-noisy.spec.md: title: no row or ticket IDs",
        "annotations/001-noisy.spec.md: title: no command flags",
        "annotations/001-noisy.spec.md: title: no tool names",
        "annotations/001-noisy.spec.md: seed: viewer isn't in suite.yaml's seeds",
        "annotations/001-noisy.spec.md: starts-on: name what must be on screen",
        "annotations/001-noisy.spec.md: platforms: ios listed twice",
        "annotations/001-noisy.spec.md: platforms: ios isn't configured",
        "annotations/001-noisy.spec.md: model: gpt",
        "annotations/001-noisy.spec.md: tools: back-burst isn't a driver tool",
        "id-map.yaml: annotations/001-noisy is still a spec",
        "id-map.yaml: OLD-1 maps to annotations/099-gone, which doesn't exist",
    ];
    for want in expect {
        assert!(
            report.iter().any(|line| line.starts_with(want)),
            "missing {want:?} in:\n{}",
            report.join("\n")
        );
    }
    assert_eq!(
        report.len(),
        expect.len(),
        "unexpected extras:\n{}",
        report.join("\n")
    );
}
