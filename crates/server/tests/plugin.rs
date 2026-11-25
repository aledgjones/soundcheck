//! The plugin's files name tools by hand; these keep them in step with the registry and the server.

use std::path::Path;

fn plugin_file(path: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugin");
    std::fs::read_to_string(root.join(path)).unwrap()
}

const PREFIX: &str = "mcp__plugin_soundcheck_soundcheck__";

#[test]
fn the_plugin_ships_no_agents() {
    // The server launches its testers and reviewers itself, with their definitions (SA-1049).
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugin");
    assert!(!root.join("agents").exists());
}

#[test]
fn the_run_skill_pre_allows_every_coordinator_tool() {
    let skill = plugin_file("skills/run/SKILL.md");
    for tool in [
        "status",
        "queue",
        "review",
        "review_find",
        "install_build",
        "triage",
        "add_evidence",
        "pause",
        "resume",
        "finish",
        "export",
    ] {
        assert!(
            skill.contains(&format!("{PREFIX}{tool}")),
            "run/SKILL.md doesn't pre-allow {tool}"
        );
    }
}

#[test]
fn the_mcp_config_runs_the_stdio_bridge() {
    let config: serde_json::Value = serde_json::from_str(&plugin_file(".mcp.json")).unwrap();
    assert_eq!(
        config["soundcheck"]["command"],
        "${CLAUDE_PLUGIN_ROOT}/bin/soundcheck"
    );
    assert_eq!(config["soundcheck"]["args"], serde_json::json!(["mcp"]));
}

#[test]
fn each_hook_runs_its_soundcheck_hook_command() {
    let config: serde_json::Value = serde_json::from_str(&plugin_file("hooks/hooks.json")).unwrap();
    assert_eq!(
        config["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "\"${CLAUDE_PLUGIN_ROOT}/bin/soundcheck\" hook session-start"
    );
    // The hooks that made a coordinator dispatch went with SA-1049: the server dispatches.
    let events: Vec<&String> = config["hooks"].as_object().unwrap().keys().collect();
    assert_eq!(events, ["SessionStart"]);
}

#[test]
fn the_plugin_bin_runs_the_release_for_its_version() {
    let shim = plugin_file("bin/soundcheck");
    assert!(shim.contains(".claude-plugin/plugin.json"));
    assert!(shim.contains("releases/download/v$version/soundcheck-$version-macos-arm64.tar.gz"));
    let release = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/release.sh"),
    )
    .unwrap();
    assert!(release.contains(r#"NAME="soundcheck-$VERSION-macos-arm64""#));
}

#[test]
fn setup_allows_exactly_soundchecks_tools() {
    let skill = plugin_file("skills/setup/SKILL.md");
    assert!(
        skill.contains(&format!("\"{PREFIX}*\"")),
        "setup's allowlist rule must match the tool prefix"
    );
}

#[test]
fn the_plugin_and_the_binary_share_a_version() {
    let manifest = plugin_file(".claude-plugin/plugin.json");
    let version = format!("\"version\": \"{}\"", env!("CARGO_PKG_VERSION"));
    assert!(
        manifest.contains(&version),
        "plugin.json's version must be the crate's ({})",
        env!("CARGO_PKG_VERSION")
    );
}
