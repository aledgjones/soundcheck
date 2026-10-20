//! One interface over Android, iOS and web devices. This crate knows devices only: never suites, rows or an app.

/// Driver tools beyond the basic reads and inputs, which a spec must name in its `tools` frontmatter.
pub const TOOLS: &[&str] = &["tap-burst", "network-drop", "stress"];
