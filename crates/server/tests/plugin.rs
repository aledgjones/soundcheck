//! The plugin's files name tools by hand; these keep them in step with the registry and the server.

use std::path::Path;

fn plugin_file(path: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugin");
    std::fs::read_to_string(root.join(path)).unwrap()
}

const PREFIX: &str = "mcp__plugin_soundcheck_soundcheck__";

#[test]
fn the_tester_agents_list_their_driver_tools() {
    // The plain tester leaves out the mechanics, whose schemas would ride along on every turn of a spec that can't use them.
    for (file, with_mechanics) in [
        ("agents/soundcheck-tester.md", false),
        ("agents/soundcheck-tester-mechanics.md", true),
    ] {
        let agent = plugin_file(file);
        for tool in soundcheck_driver::TOOLS {
            let name = format!("{PREFIX}{}\n", tool.mcp_name());
            let wanted = with_mechanics || tool.kind == soundcheck_driver::Kind::Basic;
            assert_eq!(
                agent.contains(&name),
                wanted,
                "{file} {} {}",
                if wanted { "doesn't list" } else { "lists" },
                name.trim()
            );
        }
        assert!(agent.contains(&format!("{PREFIX}post_result")));
        assert!(agent.contains(&format!("{PREFIX}brief")));
        assert!(
            !agent.contains(&format!("{PREFIX}review")),
            "testers never review"
        );
    }
}

#[test]
fn the_tester_agents_share_their_instructions() {
    let body = |file| {
        let text = plugin_file(file);
        text.split("\n---\n").nth(1).unwrap().to_string()
    };
    assert_eq!(
        body("agents/soundcheck-tester.md"),
        body("agents/soundcheck-tester-mechanics.md"),
        "the two tester agents differ only in their tools"
    );
}

#[test]
fn dispatch_names_agents_the_plugin_has() {
    for agent in [
        soundcheck::serve::prompt::TESTER,
        soundcheck::serve::prompt::MECHANICS_TESTER,
    ] {
        let file = plugin_file(&format!("agents/{agent}.md"));
        assert!(file.contains(&format!("name: {agent}\n")));
    }
}

#[test]
fn the_run_skill_pre_allows_every_coordinator_tool() {
    let skill = plugin_file("skills/run/SKILL.md");
    for tool in [
        "status",
        "dispatch",
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
