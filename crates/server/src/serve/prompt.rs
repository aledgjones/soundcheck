use crate::suite::config::Platform;
use crate::suite::spec::Spec;
use std::fmt::Write;

/// The whole brief a tester gets: the spec, its feature's context and the result contract. It's journalled as sent.
pub fn render(
    spec: &Spec,
    feature_context: Option<&str>,
    platform: Platform,
    token: &str,
    verify: Option<&str>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# {} ({platform})\n", spec.front.title);
    let _ = writeln!(
        out,
        "You are a soundcheck tester. Drive the app on {platform} only through soundcheck's tools, judge each outcome below from what you observe, then post one result. You never read the app's source.\n"
    );
    let _ = writeln!(
        out,
        "Your spec token: `{token}`. Pass it on every soundcheck call.\n"
    );
    if let Some(claim) = verify {
        let _ = writeln!(
            out,
            "## This is a verification\n\nAn earlier attempt reported: {claim}\nTry to reproduce exactly that, then judge every outcome as usual.\n"
        );
    }
    let _ = writeln!(out, "## Goal\n\n{}\n", spec.goal);
    let _ = writeln!(out, "## Outcomes\n");
    for (i, outcome) in spec.outcomes.iter().enumerate() {
        let _ = writeln!(out, "{i}. {outcome}");
    }
    out.push('\n');
    if let Some(pinned) = &spec.pinned {
        let _ = writeln!(
            out,
            "## Pinned steps\n\nFollow these exactly:\n\n{pinned}\n"
        );
    }
    if let Some(probe) = &spec.probe {
        let _ = writeln!(out, "## Probe around it\n\n{probe}\n");
    }
    if let Some(context) = feature_context.map(str::trim).filter(|c| !c.is_empty()) {
        let _ = writeln!(out, "## About this feature\n\n{context}\n");
    }
    if !spec.front.tools.is_empty() {
        let _ = writeln!(
            out,
            "Driver tools for this spec: {}.\n",
            spec.front.tools.join(", ")
        );
    }
    out.push_str(CONTRACT);
    out
}

const CONTRACT: &str = "## Posting your result

Call `post_result` once, at the end, with a judgment for every outcome above by its number:

- `held`: you observed it working.
- `failed`: you observed it broken.
- `unsure`: you observed it but can't tell whether it's right. Say exactly what was unclear.
- `couldnt-check`: you never got to observe it (a control you couldn't reach, a step that wouldn't work). Say why.

Each judgment needs `observed`: what you actually saw, in one or two plain sentences. For a failure, say what you expected and what happened instead. Attach screenshot names in `evidence` only for visual checks.

If, while probing, you find a problem the outcomes don't cover, add it to `side_finds` with a one-line summary and what you observed; it doesn't change your judgments.

An error from the app during the spec is a failure of the outcome it affected, never something to work around. Don't retry a failing step more than twice; judge what you saw.
";
