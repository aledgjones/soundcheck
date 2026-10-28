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
    rerun: Option<&str>,
    seeded: Option<&str>,
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
    if let Some(note) = rerun {
        let _ = writeln!(
            out,
            "## Before you start\n\nAn earlier attempt was stopped by the harness, not the app; that's been fixed. {note}\n"
        );
    }
    if let Some(seeded) = seeded.map(str::trim).filter(|s| !s.is_empty()) {
        let _ = writeln!(
            out,
            "## Starting state\n\nThe spec starts from the `{}` seed:\n\n{seeded}\n",
            spec.front.seed
        );
    }
    let _ = writeln!(out, "## Goal\n\n{}\n", spec.goal);
    let _ = writeln!(out, "## Outcomes\n");
    for (i, outcome) in spec.outcomes.iter().enumerate() {
        let _ = writeln!(out, "{i}. {outcome}");
    }
    let _ = writeln!(
        out,
        "\n**As you go:** the moment you've seen an outcome hold or fail, record it with `judge`, then carry on. Don't save them for the end: the person watching the run sees your progress only through `judge`.\n"
    );
    if let Some(pinned) = &spec.pinned {
        let _ = writeln!(
            out,
            "## Pinned steps\n\nFollow these exactly:\n\n{pinned}\n"
        );
    }
    if let Some(probe) = &spec.probe {
        let _ = writeln!(
            out,
            "## Probe around it\n\n{probe}\n\nAfter the outcomes, try these at a person's pace, and say in `probed` what you tried and what happened. A pass that leaves `probed` out waits for review.\n"
        );
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

/// The tester agent type for a spec. Only specs that list driver tools get the one with the mechanics: their schemas
/// are sent on every turn, and most specs never use them.
pub fn agent(spec: &Spec) -> &'static str {
    if spec.front.tools.is_empty() {
        TESTER
    } else {
        MECHANICS_TESTER
    }
}

pub const TESTER: &str = "soundcheck-tester";
pub const MECHANICS_TESTER: &str = "soundcheck-tester-mechanics";

/// The brief for readying a platform (suite.yaml `prepare`). It isn't a spec: there's one thing to say, whether it
/// worked, and soundcheck checks the platform's `ready` labels itself afterwards.
pub fn prepare(steps: &str, platform: Platform, token: &str) -> String {
    format!(
        "# Prepare {platform} for the run\n\nYou are a soundcheck tester, readying {platform} before any spec runs there. Drive the app only through soundcheck's tools and follow the steps below. This isn't a test: don't look for problems or post side finds.\n\nYour token: `{token}`. Pass it on every soundcheck call.\n\n## Steps\n\n{}\n\n## When you're done\n\nCall `post_result` once, with one judgment for outcome 0: `held` when the steps are done and the app is ready, saying what shows; `failed` saying what stopped you. Soundcheck then checks the app is ready itself.\n",
        steps.trim()
    )
}

/// What the coordinator passes to the tester agent: the brief itself travels by `brief`, outside the coordinator.
pub fn handoff(token: &str) -> String {
    format!(
        "Run soundcheck spec `{token}`: call `brief` with this token first, and follow it. When you've posted, reply with one line only: the token, the claim `post_result` returned and how many side finds you posted."
    )
}

const CONTRACT: &str = "## Posting your result

As soon as you've settled an outcome, record it with `judge`: the run shows your progress as you go. Judging the same outcome again replaces it, if a later step changes your mind.

Call `post_result` once, at the end. It takes up everything you've judged; add a judgment for any outcome you haven't, by its number:

- `held`: you observed it working.
- `failed`: you observed it broken.
- `unsure`: you observed it but can't tell whether it's right. Say exactly what was unclear.
- `couldnt-check`: you never got to observe it (a control you couldn't reach, a step that wouldn't work). Say why.

Each judgment needs `observed`: what you actually saw, in one or two plain sentences. For a failure, say what you expected and what happened instead. Attach screenshot names in `evidence` only for visual checks.

The spec starts from wherever the app opens after the reset, which can be a screen whose data the reset just replaced (a collection that's no longer there, say): get to where the spec needs from there. That opening screen isn't a side find, unless the spec is about where the app opens.

If, while probing, you find a problem the outcomes don't cover, add it to `side_finds` with a one-line summary and what you observed; it doesn't change your judgments. Its `evidence` is only screenshots on its topic: the control or screen the problem is about, such as a before and after around the step. Take the shot the moment you see it, and if it's gone first, retry the step once. When the problem itself isn't visible (a spoken value, a timing), still shoot the control it's about, and say in `observed` what you got instead: the `see` line or a call's output.

A reply that starts `harness:` means soundcheck restarted the device under you: the app's state went with it, and it isn't the app leaving the foreground or crashing. Judge what you hadn't checked yet as couldn't check, say why, and post. An error from the app during the spec is a failure of the outcome it affected, never something to work around. Don't retry a failing step more than twice; judge what you saw.

## Pace

Act and judge at a person's pace. A person never acts the instant a screen changes, and never judges from the first frame after an action.

- **Acting:** let the app respond to each action before the next one: a drawer or menu finishing its close, a list settling, and also any change of state, such as picking a tool, switching a mode or saving a setting. Picking a pen and drawing straight away, in back-to-back calls, is faster than any hand. A tap that lands mid-animation and is lost isn't a finding.
- **Judging:** let the screen settle before you read or screenshot a change. To wait for it, give `expect`, `gone` or `check` a `timeout_ms` (or use `wait`): they read again until it holds and return as soon as it does, so UI that comes late needs no pause first. A screenshot taken the instant after Undo or Clear can show the frame before the redraw. Before you call anything wrong, look again after it has settled; if it's right then, it isn't a finding.
- **`sleep` only when elapsed time is what's under test:** a network drop's length, a session expiring. Everywhere else, wait for what should change, as above.
- Probing \"quickly\" means as fast as a person realistically could, never faster. `batch` keeps inputs at least 500 ms apart for you.
- **Waiting a long time** (a spec that waits minutes for something to expire): send one `sleep` of up to 30 s per call, as many as it takes. Never pause for long inside a `batch`: a call that runs for minutes loses its reply, and the attempt with it.

## Fewer calls

Every call re-reads this whole conversation, so fewer calls make a faster, cheaper run.

- **Every input returns the screen once it holds still** (`tap`, `fill`, `type`, `clear`, `swipe`, `touch`, `release`, `hold`, `press`, `launch`, `double_tap` and `batch`), so don't `see` after one: read its reply. `see` is for a look before you've acted, or with a `filter` or `all`.
- **When you already know the next few steps** (open a menu, choose an item, confirm), send them as one `batch`. It stops at the first step that fails, says which and what ran before it, and returns the screen as it is then, so you lose nothing over single calls.
- **Keep the checks you judge an outcome on as their own calls,** after the batch, so what you saw is plain.
- `tap` waits up to 5 s for its target, so a tap needs no `see` or `wait` before it when you know the label.
";
