use soundcheck::lint::lint;
use soundcheck::suite::{self, Problem};
use std::path::Path;

fn problems(fixture: &str) -> Vec<Problem> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    lint(&suite::load(&root), "0.2.0")
}

#[test]
fn a_good_suite_has_no_problems() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/good");
    let suite = suite::load(&root);
    let ids: Vec<_> = suite.specs.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["annotations/001-draw", "annotations/002-back"]);
    assert_eq!(lint(&suite, "0.2.0"), vec![]);
}

#[test]
fn a_broken_suite_reports_every_rule() {
    let found = problems("broken");
    let report: Vec<String> = found
        .iter()
        .map(|p| format!("{}: {}", p.path.display(), p.message))
        .collect();
    let expect = [
        "suite.yaml: pinned to soundcheck 0.3",
        "suite.yaml: platforms.android: needs app-id",
        "suite.yaml: platforms.web: needs start",
        "suite.yaml: hooks.seed must contain {scenario}",
        "suite.yaml: seeds: list the seed scenarios",
        "suite.yaml: models.default: gpt",
        "stray.spec.md: specs belong in a feature folder",
        "Bad_Feature: feature folders are lower-case",
        "annotations/02-misnamed.spec.md: spec files are named NNN-name.spec.md",
        "annotations/004-tools.spec.md: seed: viewer isn't in suite.yaml's seeds",
        "annotations/004-tools.spec.md: platforms: ios isn't configured in suite.yaml",
        "annotations/004-tools.spec.md: tools: tap is a basic tool",
        "annotations/004-tools.spec.md: tools: network-drop on a device needs suite.yaml's network section",
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

#[test]
fn a_reports_folder_inside_the_suite_is_not_a_feature() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("soundcheck-reports-{nanos}"));
    let good = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/good");
    std::fs::create_dir_all(root.join("annotations")).unwrap();
    std::fs::create_dir_all(root.join("runs/20261020-235134")).unwrap();
    for file in ["annotations/_feature.md", "annotations/001-draw.spec.md"] {
        std::fs::copy(good.join(file), root.join(file)).unwrap();
    }
    let config = std::fs::read_to_string(good.join("suite.yaml")).unwrap() + "reports: runs\n";
    std::fs::write(root.join("suite.yaml"), config).unwrap();
    assert_eq!(lint(&suite::load(&root), "0.2.0"), vec![]);
}

#[test]
fn specs_can_live_in_their_own_folder() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("soundcheck-specs-{nanos}"));
    let good = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/good");
    std::fs::create_dir_all(root.join("suite/annotations")).unwrap();
    std::fs::copy(
        good.join("annotations/001-draw.spec.md"),
        root.join("suite/annotations/001-draw.spec.md"),
    )
    .unwrap();
    let config = std::fs::read_to_string(good.join("suite.yaml")).unwrap() + "specs: suite\n";
    std::fs::write(root.join("suite.yaml"), config).unwrap();
    let suite = suite::load(&root);
    assert_eq!(suite.specs.len(), 1);
    // The missing _feature.md is reported where it belongs.
    let paths: Vec<_> = lint(&suite, "0.2.0").into_iter().map(|p| p.path).collect();
    assert_eq!(paths, [Path::new("suite/annotations/_feature.md")]);
}

#[test]
fn a_network_section_is_checked() {
    let report: Vec<String> = problems("network")
        .iter()
        .map(|p| format!("{}: {}", p.path.display(), p.message))
        .collect();
    assert_eq!(
        report,
        [
            "suite.yaml: network.proxy.echo: listen and upstream must differ",
            "suite.yaml: network.proxy.echo: port 23000 listed twice",
            "suite.yaml: network: one proxy serves every platform, so platforms can't run in parallel; give each its own",
        ]
    );
}

#[test]
fn a_proxy_upstream_url_is_checked() {
    let report: Vec<String> = problems("network-url")
        .iter()
        .map(|p| format!("{}: {}", p.path.display(), p.message))
        .collect();
    assert_eq!(report.len(), 1, "{report:?}");
    assert!(
        report[0].starts_with("suite.yaml: network.proxy")
            && report[0].contains("\"https://api.example.com/v1\" has more than a host and port"),
        "{report:?}"
    );
}

#[test]
fn a_proxy_per_platform_is_checked() {
    let report: Vec<String> = problems("network-per-platform")
        .iter()
        .map(|p| format!("{}: {}", p.path.display(), p.message))
        .collect();
    assert_eq!(
        report,
        [
            "suite.yaml: network.proxy.android.bucket: device port 23000 listed twice",
            "suite.yaml: network.proxy.web.api: port 33000 listed twice",
            "suite.yaml: network.proxy.web.api: device is for Android's routes, which forward ports from the device",
            "suite.yaml: network.proxy: ios has no routes, so its app can't reach anything",
            "suite.yaml: hooks: stack-up and stack-down go together, so every stack started is removed",
            "suite.yaml: prepare: no file missing.md",
        ]
    );
}
