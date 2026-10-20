//! The plugin's files name tools by hand; these keep them in step with the registry and the server.

use std::path::Path;

fn plugin_file(path: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugin");
    std::fs::read_to_string(root.join(path)).unwrap()
}

const PREFIX: &str = "mcp__plugin_soundcheck_soundcheck__";

#[test]
fn the_tester_agent_can_use_every_driver_tool() {
    let agent = plugin_file("agents/soundcheck-tester.md");
    for tool in soundcheck_driver::TOOLS {
        let name = format!("{PREFIX}{}", tool.mcp_name());
        assert!(
            agent.contains(&name),
            "soundcheck-tester.md doesn't list {name}"
        );
    }
    assert!(agent.contains(&format!("{PREFIX}post_result")));
    assert!(
        !agent.contains(&format!("{PREFIX}review")),
        "testers never review"
    );
}

#[test]
fn the_run_skill_pre_allows_every_coordinator_tool() {
    let skill = plugin_file("skills/run/SKILL.md");
    for tool in [
        "status",
        "dispatch",
        "review",
        "review_find",
        "install_build",
        "triage",
        "pause",
        "resume",
        "finish",
    ] {
        assert!(
            skill.contains(&format!("{PREFIX}{tool}")),
            "run/SKILL.md doesn't pre-allow {tool}"
        );
    }
}

#[test]
fn the_mcp_config_points_at_the_default_port() {
    let config: serde_json::Value = serde_json::from_str(&plugin_file(".mcp.json")).unwrap();
    assert_eq!(config["soundcheck"]["url"], "http://127.0.0.1:7357/mcp");
}
