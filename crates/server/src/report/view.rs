//! The page as data: everything it shows, worked out from the journal here, so the page only lays it out and a live
//! page can be sent just what changed.

use super::*;
use crate::run::Install;
use serde::Serialize;
use serde_json::{Map, Value};

// Options are left out rather than sent as null: a null in a merge patch deletes the key.

#[derive(Serialize)]
pub struct View {
    label: String,
    /// Set for a trial on uncommitted code, saying what its results aren't.
    #[serde(skip_serializing_if = "Option::is_none")]
    trial: Option<&'static str>,
    version: &'static str,
    platforms: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bar: Option<Bar>,
    facts: Vec<Fact>,
    /// An export has no server to download from or list runs.
    export: bool,
    summary: Summary,
    sections: Vec<Section>,
    checks: BTreeMap<String, Check>,
    problems: BTreeMap<String, ProblemView>,
    groups: Vec<Group>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<Notes>,
    /// By journal `seq`.
    timeline: BTreeMap<String, Line>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum Bar {
    Now {
        paused: bool,
        doing: Vec<Doing>,
        #[serde(skip_serializing_if = "Option::is_none")]
        idle: Option<Overdue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timer: Option<Timer>,
    },
    Starting {
        #[serde(skip_serializing_if = "Option::is_none")]
        step: Option<Step>,
        #[serde(skip_serializing_if = "Option::is_none")]
        failed: Option<String>,
    },
}

#[derive(Serialize)]
struct Doing {
    platform: &'static str,
    spec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    checking: Option<Overdue>,
}

/// A time shown only from `warn_ms` after `since`, as `label` with `{t}` replaced by the time passed.
#[derive(Serialize)]
struct Overdue {
    since: u64,
    warn_ms: u64,
    label: &'static str,
}

#[derive(Serialize)]
struct Timer {
    active_ms: u64,
    /// Counting on from here while the run works, up to `cap_ms` of quiet.
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<u64>,
    cap_ms: u64,
}

#[derive(Serialize)]
struct Step {
    text: String,
    ago: Overdue,
}

#[derive(Serialize)]
struct Fact {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    at: Option<u64>,
}

#[derive(Serialize)]
struct Summary {
    pct: u32,
    ring: Vec<Segment>,
    class: &'static str,
    word: &'static str,
    reasons: Vec<Reason>,
    grid: Vec<GridRow>,
}

#[derive(Serialize)]
pub(super) struct Segment {
    pub class: &'static str,
    pub dash: String,
    pub offset: String,
}

#[derive(Serialize)]
struct Reason {
    text: String,
    bad: bool,
}

#[derive(Serialize)]
struct GridRow {
    feature: String,
    name: String,
    cells: Vec<Cell>,
}

#[derive(Serialize)]
struct Cell {
    class: &'static str,
    text: String,
    /// Widths in percent of pass, fail, blocked, checking and running; none for a section with nothing planned.
    #[serde(skip_serializing_if = "Option::is_none")]
    bars: Option<[u32; 5]>,
}

#[derive(Serialize)]
struct Section {
    id: String,
    name: String,
    specs: Vec<String>,
}

#[derive(Serialize)]
struct Check {
    anchor: String,
    number: String,
    title: String,
    attn: bool,
    /// Open until the viewer chooses otherwise: a row with a failed or blocked result.
    open: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pill: Option<Chip>,
    cells: Vec<ResultCell>,
    bodies: Vec<Body>,
}

#[derive(Serialize)]
struct Chip {
    class: &'static str,
    text: String,
}

#[derive(Serialize)]
struct ResultCell {
    platform: &'static str,
    class: &'static str,
    text: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'static str>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum Body {
    Pending {
        platform: &'static str,
        label: bool,
        state: String,
        running: bool,
        outcomes: Vec<PendingOutcome>,
    },
    Detail {
        platform: &'static str,
        label: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        failure: Option<Failure>,
        #[serde(skip_serializing_if = "Option::is_none")]
        blocked: Option<Block>,
        findings: Vec<FindingBlock>,
        outcomes: Vec<Outcome>,
        #[serde(skip_serializing_if = "Option::is_none")]
        evidence: Option<Evidence>,
    },
}

#[derive(Serialize)]
struct PendingOutcome {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    judged: Option<Judged>,
}

#[derive(Serialize)]
struct Judged {
    class: &'static str,
    mark: &'static str,
    observed: String,
}

/// A filed failure's call and screenshots, shown inside its last failed outcome.
#[derive(Serialize)]
struct Failure {
    #[serde(skip_serializing_if = "Option::is_none")]
    call: Option<String>,
    shots: Vec<Shot>,
    /// No outcome reads as failed to hold it (an upheld Unsure, say), so it gets its own red box.
    alone: bool,
}

#[derive(Serialize)]
struct Block {
    expect: String,
    saw: String,
}

#[derive(Serialize)]
struct FindingBlock {
    id: u32,
    title: String,
    detail: String,
    shots: Vec<Shot>,
}

#[derive(Serialize)]
struct Outcome {
    class: &'static str,
    mark: &'static str,
    label: &'static str,
    text: String,
    observed: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    settled: Option<String>,
    /// Drawn as a failed outcome's red box.
    boxed: bool,
    /// Holds the row's [`Failure`].
    failure: bool,
}

#[derive(Serialize)]
struct Shot {
    name: String,
    /// A link to the file on the live page, or the image itself in an export; none when it can't be found.
    #[serde(skip_serializing_if = "Option::is_none")]
    src: Option<String>,
}

#[derive(Serialize)]
struct Evidence {
    shots: Vec<Shot>,
    texts: Vec<TextFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
}

/// A text file the server saved: linked on the live page, carried whole in an export.
#[derive(Serialize)]
struct TextFile {
    name: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    href: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

#[derive(Serialize)]
struct ProblemView {
    number: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    badge: Option<Chip>,
    title: String,
    place: String,
    detail: String,
    shots: Vec<Shot>,
    paras: Vec<String>,
    row: String,
}

#[derive(Serialize)]
struct Group {
    title: &'static str,
    ids: Vec<String>,
}

#[derive(Serialize)]
struct Notes {
    count: usize,
    rows: Vec<NoteRow>,
}

#[derive(Serialize)]
struct NoteRow {
    title: &'static str,
    lines: Vec<String>,
}

#[derive(Serialize)]
struct Line {
    at: u64,
    text: String,
}

/// What an uncommitted run's page says at the top.
pub const TRIAL: &str = "A trial on uncommitted code, not a regression result: no commit identifies the build, so it files no tickets.";

/// Where a fixed or deferred problem's ticket would be on an uncommitted run.
pub const NO_TICKET: &str = "No ticket: uncommitted";

pub fn build(page: &Page) -> View {
    let state = page.run.state();
    let shown = platforms(page);
    let bar = (page.live && !state.finished).then(|| match page.starting {
        Some(starting) => start_bar(starting),
        None => now_bar(page),
    });
    let (problems, groups) = problems(page);
    View {
        label: page.label.to_string(),
        trial: page.uncommitted.then_some(TRIAL),
        version: crate::VERSION,
        platforms: shown.iter().map(|p| platform_name(*p)).collect(),
        bar,
        facts: facts(page),
        export: page.export.is_some(),
        summary: summary(page),
        sections: sections(page),
        checks: checks(page, &shown),
        problems,
        groups,
        notes: notes(page),
        timeline: timeline(page),
    }
}

pub fn json(page: &Page) -> Value {
    serde_json::to_value(build(page)).unwrap_or_default()
}

/// The RFC 7386 merge patch that turns `old` into `new`, or none when they're the same.
pub fn merge_patch(old: &Value, new: &Value) -> Option<Value> {
    match (old, new) {
        (Value::Object(old), Value::Object(new)) => {
            let mut patch = Map::new();
            for key in old.keys() {
                if !new.contains_key(key) {
                    patch.insert(key.clone(), Value::Null);
                }
            }
            for (key, value) in new {
                match old.get(key) {
                    Some(before) => {
                        if let Some(changed) = merge_patch(before, value) {
                            patch.insert(key.clone(), changed);
                        }
                    }
                    None => {
                        patch.insert(key.clone(), value.clone());
                    }
                }
            }
            (!patch.is_empty()).then_some(Value::Object(patch))
        }
        _ if old == new => None,
        _ => Some(new.clone()),
    }
}

/// JSON to put inside a `<script>` element: no `<` can end it or open a comment.
pub fn script_json(view: &Value) -> String {
    view.to_string().replace('<', "\\u003c")
}

fn now_bar(page: &Page) -> Bar {
    let state = page.run.state();
    let entries = page.run.entries_after(0);
    let doing = state
        .rows
        .iter()
        .filter_map(|((spec, platform), r)| match &r.row {
            Row::Running { .. } => Some(Doing {
                platform: platform_name(*platform),
                spec: spec.clone(),
                checking: None,
            }),
            Row::NeedsReview { since, .. } => Some(Doing {
                platform: platform_name(*platform),
                spec: spec.clone(),
                checking: Some(Overdue {
                    since: *since,
                    warn_ms: CHECKING_WARN_MS,
                    label: "for {t}",
                }),
            }),
            _ => None,
        })
        .collect();
    Bar::Now {
        paused: state.paused,
        doing,
        idle: entries.last().map(|last| Overdue {
            since: last.at,
            warn_ms: IDLE_WARN_MS,
            label: "idle {t}: coordinator not responding",
        }),
        timer: entries.last().map(|last| Timer {
            active_ms: active_ms(entries),
            since: (!state.finished && !state.paused).then_some(last.at),
            cap_ms: ACTIVE_GAP_MS,
        }),
    }
}

fn start_bar(starting: &StartUp) -> Bar {
    Bar::Starting {
        step: match (&starting.failed, &starting.doing) {
            (None, Some((step, since))) => Some(Step {
                text: step.clone(),
                ago: Overdue {
                    since: *since,
                    warn_ms: 0,
                    label: "· {t}",
                },
            }),
            _ => None,
        },
        failed: starting.failed.clone(),
    }
}

fn facts(page: &Page) -> Vec<Fact> {
    let fact = |text: String, title: Option<&'static str>| Fact {
        text,
        title,
        at: None,
    };
    let entries = page.run.entries_after(0);
    let mut facts: Vec<Fact> = platforms(page)
        .iter()
        .map(|p| fact(platform_name(*p).into(), None))
        .collect();
    if let Some(first) = entries.first() {
        facts.push(Fact {
            text: "Started".into(),
            title: None,
            at: Some(first.at),
        });
    }

    let attempts = usage::attempts(entries);
    let took = |ms: u64| {
        let minutes = ms / 60_000;
        if minutes < 60 {
            format!("{minutes} min")
        } else {
            format!("{} h {} min", minutes / 60, minutes % 60)
        }
    };
    // Per platform, since platforms in parallel would add up to more than the clock.
    let per_platform: Vec<(Platform, u64)> = platforms(page)
        .iter()
        .map(|p| {
            let suffix = format!("@{p}");
            let ms = attempts
                .iter()
                .filter(|a| a.token.ends_with(&suffix))
                .filter_map(|a| a.duration_ms)
                .sum();
            (*p, ms)
        })
        .filter(|(_, ms)| *ms >= 60_000)
        .collect();
    if attempts.iter().any(|a| a.duration_ms.is_some()) {
        if per_platform.len() > 1 {
            let each: Vec<String> = per_platform
                .iter()
                .map(|(p, ms)| format!("{} {}", platform_name(*p), took(*ms)))
                .collect();
            facts.push(fact(
                format!("Active {}", each.join(" · ")),
                Some("Testers' working time on each platform"),
            ));
        } else if let Some((_, ms)) = per_platform.first() {
            facts.push(fact(
                format!("Active {}", took(*ms)),
                Some("Testers' working time, added up across attempts"),
            ));
        }
    } else if active_ms(entries) >= 60_000 {
        // A run from before usage was recorded falls back to the time between its journal entries.
        facts.push(fact(
            format!("Active {}", took(active_ms(entries))),
            Some("Time the run spent working, without the gaps between sessions"),
        ));
    }

    let tokens: u64 = attempts.iter().filter_map(|a| a.tokens).sum();
    if tokens > 0 {
        facts.push(fact(
            format!("Tokens {}", thousands(tokens)),
            Some("Testers' tokens, added up across attempts"),
        ));
    }

    if let Some(export) = page.export {
        let text = if export.finished {
            "Final report, exported"
        } else {
            "In progress, exported"
        };
        facts.push(Fact {
            text: text.into(),
            title: None,
            at: Some(export.at),
        });
    }
    facts
}

/// The ring's coloured arcs, as SVG dash settings on a circle of radius 44.
pub(super) fn ring_segments(t: &Tally) -> Vec<Segment> {
    let c = 2.0 * std::f64::consts::PI * 44.0;
    let mut offset = 0.0;
    let mut segments = Vec::new();
    for (class, n) in [("pass", t.pass), ("fail", t.fail), ("blocked", t.blocked)] {
        if n == 0 {
            continue;
        }

        let len = c * t.pct(n) / 100.0;
        segments.push(Segment {
            class,
            dash: format!("{len:.1} {:.1}", c - len),
            offset: format!("{:.1}", -offset),
        });
        offset += len;
    }
    segments
}

fn summary(page: &Page) -> Summary {
    let state = page.run.state();
    let v = verdict(state);
    let mut all = Tally::default();
    for r in state.rows.values() {
        all.add(r);
    }
    let (class, word) = match v.status {
        VerdictStatus::Failing => ("failing", "Failing"),
        VerdictStatus::NotReady => ("", "Not ready"),
        VerdictStatus::Ready => ("", "Ready"),
    };
    let reasons = v
        .reasons
        .iter()
        .map(|r| Reason {
            text: r.clone(),
            bad: r.contains("failure"),
        })
        .collect();
    let shown = platforms(page);
    let grid = page
        .suite
        .features
        .iter()
        .map(|feature| GridRow {
            feature: feature.name.clone(),
            name: feature_name(&feature.name),
            cells: shown
                .iter()
                .map(|p| {
                    let mut t = Tally::default();
                    for ((spec, platform), r) in &state.rows {
                        if platform == p && spec.split('/').next() == Some(feature.name.as_str()) {
                            t.add(r);
                        }
                    }
                    cell(&t)
                })
                .collect(),
        })
        .collect();
    Summary {
        pct: all.pct(all.pass).round() as u32,
        ring: ring_segments(&all),
        class,
        word,
        reasons,
        grid,
    }
}

fn cell(t: &Tally) -> Cell {
    if t.total == 0 {
        return Cell {
            class: "",
            text: "–".into(),
            bars: None,
        };
    }

    let done = t.pass;
    let (class, text) = if t.pass + t.fail + t.blocked + t.checking + t.running == 0 {
        ("", format!("Not run · 0/{}", t.total))
    } else if t.fail > 0 {
        ("fail", format!("{} failed · {done}/{}", t.fail, t.total))
    } else if t.running > 0 {
        ("live", format!("Running · {done}/{}", t.total))
    } else if t.checking > 0 {
        (
            "review",
            format!("{} checking · {done}/{}", t.checking, t.total),
        )
    } else if t.blocked > 0 {
        (
            "blocked",
            format!("{} blocked · {done}/{}", t.blocked, t.total),
        )
    } else if done == t.total {
        ("pass", format!("Passed · {done}/{}", t.total))
    } else {
        ("pass", format!("Waiting · {done}/{}", t.total))
    };
    let width = |n: u32| t.pct(n).round() as u32;
    Cell {
        class,
        text,
        bars: Some([
            width(t.pass),
            width(t.fail),
            width(t.blocked),
            width(t.checking),
            width(t.running),
        ]),
    }
}

fn result_cell(platform: Platform, r: Option<&RowRecord>) -> ResultCell {
    let (class, text, title) = match r.map(|r| &r.row) {
        None => ("na", "–", None),
        Some(Row::Filed {
            result: Final::Pass,
            ..
        }) => ("pass", "Passed", None),
        Some(Row::Filed {
            result: Final::Fail,
            ..
        }) => ("fail", "Failed", None),
        Some(Row::Filed {
            result: Final::Blocked,
            ..
        }) => ("blocked", "Blocked", None),
        Some(Row::NeedsReview { .. }) => (
            "review",
            "Checking",
            Some("The tester reported a problem; the coordinator is checking it"),
        ),
        Some(Row::Running { .. }) => ("running", "Running", None),
        Some(Row::Queued { .. }) => ("not-run", "Queued", None),
        Some(Row::NotRun) => ("not-run", "Not run", None),
    };
    ResultCell {
        platform: platform_name(platform),
        class,
        text,
        title,
    }
}

/// Each feature's planned specs, in suite order.
fn planned<'p>(page: &'p Page, feature: &str) -> Vec<&'p Spec> {
    page.suite
        .specs
        .iter()
        .filter(|s| s.feature == feature && page.run.plan().specs.iter().any(|p| p.id == s.id))
        .collect()
}

fn sections(page: &Page) -> Vec<Section> {
    page.suite
        .features
        .iter()
        .filter_map(|feature| {
            let specs = planned(page, &feature.name);
            (!specs.is_empty()).then(|| Section {
                id: feature.name.clone(),
                name: feature_name(&feature.name),
                specs: specs.iter().map(|s| s.id.clone()).collect(),
            })
        })
        .collect()
}

fn checks(page: &Page, shown: &[Platform]) -> BTreeMap<String, Check> {
    page.suite
        .features
        .iter()
        .flat_map(|feature| planned(page, &feature.name))
        .map(|spec| (spec.id.clone(), check(page, spec, shown)))
        .collect()
}

fn check(page: &Page, spec: &Spec, shown: &[Platform]) -> Check {
    let state = page.run.state();
    let records: Vec<(Platform, &RowRecord)> = shown
        .iter()
        .filter_map(|p| state.rows.get(&(spec.id.clone(), *p)).map(|r| (*p, r)))
        .collect();
    let number = spec
        .id
        .rsplit('/')
        .next()
        .unwrap_or("")
        .split('-')
        .next()
        .unwrap_or("")
        .to_string();
    // Findings belong to the check, not one platform's result: one pill by the title counts them all.
    let findings: Vec<_> = shown
        .iter()
        .flat_map(|p| findings_for(page, &spec.id, *p))
        .collect();
    let open_findings = findings.iter().filter(|f| f.status == Status::Open).count();
    let pill = match findings.len() {
        0 => None,
        n => Some(Chip {
            class: if open_findings > 0 {
                "finding"
            } else {
                "muted"
            },
            text: format!("{n} finding{}", if n == 1 { "" } else { "s" }),
        }),
    };
    let needs_look = |r: &RowRecord| {
        matches!(
            r.row,
            Row::Filed {
                result: Final::Fail | Final::Blocked,
                ..
            } | Row::NeedsReview { .. }
        )
    };
    let attn = records.iter().any(|(_, r)| needs_look(r)) || open_findings > 0;
    let cells = shown
        .iter()
        .map(|p| result_cell(*p, state.rows.get(&(spec.id.clone(), *p))))
        .collect();

    let label = records.len() > 1;
    let bodies = records
        .iter()
        .filter_map(|(platform, r)| {
            // A rerun's earlier result gives way to the attempt under way.
            if waiting(r) {
                Some(pending(spec, *platform, r, label))
            } else if r.posted.is_some()
                || matches!(r.row, Row::Filed { .. })
                || !findings_for(page, &spec.id, *platform).is_empty()
            {
                Some(detail(page, spec, *platform, r, label))
            } else {
                None
            }
        })
        .collect();
    let open = records.iter().any(|(_, r)| {
        matches!(
            r.row,
            Row::Filed {
                result: Final::Fail | Final::Blocked,
                ..
            }
        )
    });
    Check {
        anchor: anchor(&spec.id),
        number,
        title: spec.front.title.clone(),
        attn,
        open,
        pill,
        cells,
        bodies,
    }
}

/// The outcomes of a row with no result yet, so a reader can see what it will check while it waits or runs.
fn pending(spec: &Spec, platform: Platform, r: &RowRecord, label: bool) -> Body {
    let spec = &spec.for_platform(platform);
    let so_far = if r.provisional.is_empty() {
        String::new()
    } else {
        format!(
            " · {} of {} judged",
            r.provisional.len(),
            spec.outcomes.len()
        )
    };
    let (state, running) = match &r.row {
        Row::Running { attempt, .. } if *attempt > 1 => {
            (format!("Running now, attempt {attempt}{so_far}"), true)
        }
        Row::Running { .. } => (format!("Running now{so_far}"), true),
        Row::Queued {
            verify: Some(claim),
        } => (format!("Queued to reproduce: {claim}"), false),
        Row::Queued { .. } => ("Queued for another attempt".to_string(), false),
        _ => ("Not run yet".to_string(), false),
    };
    let outcomes = spec
        .outcomes
        .iter()
        .enumerate()
        .map(|(i, outcome)| {
            // Judged so far: held reads as held, anything else waits for the coordinator's call, like a claim in review.
            let judged = r
                .provisional
                .iter()
                .find(|j| j.outcome == i)
                .filter(|_| running)
                .map(|j| {
                    let (class, mark) = if j.answer == Answer::Held {
                        ("held", "✓")
                    } else {
                        ("unsure", "?")
                    };
                    Judged {
                        class,
                        mark,
                        observed: j.observed.clone(),
                    }
                });
            PendingOutcome {
                text: outcome.clone(),
                judged,
            }
        })
        .collect();
    Body::Pending {
        platform: platform_name(platform),
        label,
        state,
        running,
        outcomes,
    }
}

fn detail(page: &Page, spec: &Spec, platform: Platform, r: &RowRecord, label: bool) -> Body {
    let spec = &spec.for_platform(platform);
    let name = platform_name(platform);
    let reason = r.review.as_ref().map(|(_, reason)| reason.as_str());
    let mut failure = None;
    let mut blocked = None;
    match &r.row {
        Row::Filed {
            result: Final::Fail,
            reason: filed,
            ..
        } => {
            // The failure's screenshots sit in the red block, under its last failed outcome.
            let shots = page
                .run
                .state()
                .problems
                .iter()
                .rev()
                .find(|p| {
                    p.spec == spec.id && p.platform == platform && p.kind == ProblemKind::Failure
                })
                .map(|p| shots(page, &p.evidence))
                .unwrap_or_default();
            // A judged Unsure already carries the call in its note.
            let judged = matches!(r.review, Some((Call::Judge { .. }, _)));
            let call = (!judged).then(|| {
                format!(
                    "{}{}",
                    if r.verifying {
                        "Reproduced on verification. "
                    } else {
                        ""
                    },
                    reason.unwrap_or(filed)
                )
            });
            failure = Some(Failure {
                call,
                shots,
                alone: false,
            });
        }
        Row::Filed {
            result: Final::Blocked,
            reason: filed,
            ..
        } => {
            blocked = Some(Block {
                expect: format!("Couldn't be tested on {name}."),
                saw: format!(
                    "{}. It counts as not tested until a rerun passes.",
                    reason.unwrap_or(filed).trim_end_matches('.')
                ),
            });
        }
        _ => {}
    }

    let findings = findings_for(page, &spec.id, platform)
        .into_iter()
        .map(|f| FindingBlock {
            id: f.id,
            title: format!("Finding {:03}: {}", f.id, f.title),
            detail: f.detail.clone(),
            shots: shots(page, &f.evidence),
        })
        .collect();
    let outcomes = r
        .posted
        .as_ref()
        .map(|posted| outcomes(spec, posted, r, failure.is_some()))
        .unwrap_or_default();
    if let Some(failure) = &mut failure {
        failure.alone = !outcomes.iter().any(|o| o.failure);
    }

    let evidence = r
        .posted
        .as_ref()
        .and_then(|posted| evidence(page, posted, &r.captured));
    Body::Detail {
        platform: name,
        label,
        failure,
        blocked,
        findings,
        outcomes,
        evidence,
    }
}

/// The outcomes as judged. On a filed failure the failed ones are red boxes, and the last holds the failure's call
/// and screenshots, so the outcome and its evidence sit together.
fn outcomes(spec: &Spec, posted: &Posted, r: &RowRecord, failed: bool) -> Vec<Outcome> {
    let judged = match &r.review {
        Some((Call::Judge { result, .. }, reason)) => Some((*result, reason.as_str())),
        _ => None,
    };
    // An Unsure the coordinator judged reads as its verdict, since that's what the row was filed on; until a claim is
    // reviewed, anything that didn't hold reads as a question.
    let awaiting = matches!(r.row, Row::NeedsReview { .. });
    let shown = |answer: Answer| match (answer, judged) {
        (Answer::Held, _) => Answer::Held,
        _ if awaiting => Answer::Unsure,
        (Answer::Unsure, Some((Final::Fail, _))) => Answer::Failed,
        (Answer::Unsure, Some((Final::Pass, _))) => Answer::Held,
        _ => answer,
    };
    let last_failed = posted
        .judgments
        .iter()
        .rposition(|j| shown(j.answer) == Answer::Failed);
    posted
        .judgments
        .iter()
        .enumerate()
        .map(|(i, j)| {
            let (class, mark, label) = match shown(j.answer) {
                Answer::Held => ("held", "✓", "held"),
                Answer::Failed => ("failed", "✕", "failed"),
                Answer::Unsure => ("unsure", "?", "unsure"),
                Answer::CouldntCheck => ("blocked", "–", "couldn't check"),
            };
            let settled = match (j.answer, judged) {
                (Answer::Unsure, Some((result, reason))) => Some(format!(
                    "The tester was unsure; the coordinator judged it {}: {reason}",
                    if result == Final::Fail {
                        "failed"
                    } else {
                        "passed"
                    }
                )),
                _ => None,
            };
            Outcome {
                class,
                mark,
                label,
                text: judged_as(spec, r, j.outcome).to_string(),
                observed: j.observed.clone(),
                settled,
                boxed: failed && shown(j.answer) == Answer::Failed,
                failure: failed && Some(i) == last_failed,
            }
        })
        .collect()
}

fn shots<S: AsRef<str>>(page: &Page, names: &[S]) -> Vec<Shot> {
    names
        .iter()
        .map(|name| Shot {
            name: name.as_ref().to_string(),
            src: image(page, name.as_ref()),
        })
        .collect()
}

fn evidence(page: &Page, posted: &Posted, captured: &[String]) -> Option<Evidence> {
    let mut names: Vec<&String> = posted
        .judgments
        .iter()
        .flat_map(|j| j.evidence.iter())
        .collect();
    names.extend(captured.iter().filter(|f| is_shot(f)));
    let texts: Vec<&String> = captured.iter().filter(|f| f.ends_with(".txt")).collect();
    if names.is_empty() && texts.is_empty() && posted.notes.is_none() {
        return None;
    }

    let texts = texts
        .into_iter()
        .filter(|t| safe_name(t))
        .filter_map(|t| {
            let label = if t.ends_with("-seen.txt") {
                "Element tree the tester last read"
            } else if t.ends_with("-tree.txt") {
                "Element tree when the result was posted"
            } else if t.ends_with("-errors.txt") {
                "Errors"
            } else {
                t.as_str()
            };
            let dir = page.run_dir?;
            // An export carries the text itself, so it stays one self-contained file.
            let (href, text) = if page.export.is_some() {
                let text = std::fs::read_to_string(dir.join(EVIDENCE_DIR).join(t))
                    .unwrap_or_default()
                    .chars()
                    .take(20_000)
                    .collect();
                (None, Some(text))
            } else {
                (Some(format!("files/{EVIDENCE_DIR}/{t}")), None)
            };
            Some(TextFile {
                name: t.to_string(),
                label: label.to_string(),
                href,
                text,
            })
        })
        .collect();
    Some(Evidence {
        shots: shots(page, &names),
        texts,
        notes: posted.notes.clone(),
    })
}

fn problems(page: &Page) -> (BTreeMap<String, ProblemView>, Vec<Group>) {
    let state = page.run.state();
    let originals: Vec<&crate::run::model::Problem> = state
        .problems
        .iter()
        .filter(|p| !matches!(p.status, Status::Duplicate { .. }))
        .collect();
    let ticket = |t: &Option<String>| match t {
        Some(t) => Some(Chip {
            class: "ticket",
            text: t.clone(),
        }),
        None if page.uncommitted => Some(Chip {
            class: "muted",
            text: NO_TICKET.into(),
        }),
        None => None,
    };
    let mut items = BTreeMap::new();
    for p in &originals {
        let badge = match &p.status {
            Status::Open => Some(match p.kind {
                ProblemKind::Failure => Chip {
                    class: "blocks",
                    text: "Failure".into(),
                },
                ProblemKind::Finding => Chip {
                    class: "finding",
                    text: "Finding".into(),
                },
            }),
            Status::Fixed { ticket: t, .. } | Status::Deferred { ticket: t, .. } => ticket(t),
            Status::Dismissed { .. } => Some(Chip {
                class: "muted",
                text: "Dismissed".into(),
            }),
            Status::Duplicate { .. } => None,
        };
        let also: Vec<&str> = state
            .problems
            .iter()
            .filter(|d| d.status == (Status::Duplicate { of: p.id }))
            .map(|d| platform_name(d.platform))
            .collect();
        let mut paras = Vec::new();
        if p.verified && p.source == ProblemSource::Row {
            paras.push("Verified: reproduced on another attempt from a fresh seed.".to_string());
        }
        if p.source == ProblemSource::SideFind {
            paras.push("Found while probing, outside the check itself.".to_string());
            if p.evidence.is_empty() {
                paras.push("No screenshot of it; the detail above is all there is.".to_string());
            }
        }
        if !also.is_empty() {
            paras.push(format!("Also seen on {}.", also.join(", ")));
        }
        match &p.status {
            Status::Fixed { reason, .. }
            | Status::Deferred { reason, .. }
            | Status::Dismissed { reason } => paras.push(reason.clone()),
            _ => {}
        }
        items.insert(
            p.id.to_string(),
            ProblemView {
                number: format!("{:03}", p.id),
                badge,
                title: p.title.clone(),
                place: format!(
                    "{} · {}",
                    platform_name(p.platform),
                    feature_name(p.spec.split('/').next().unwrap_or(""))
                ),
                detail: p.detail.clone(),
                shots: shots(page, &p.evidence),
                paras,
                row: anchor(&p.spec),
            },
        );
    }

    let ids = |chosen: Vec<&&crate::run::model::Problem>| -> Vec<String> {
        chosen.iter().map(|p| p.id.to_string()).collect()
    };
    let mut open: Vec<&&crate::run::model::Problem> = originals
        .iter()
        .filter(|p| p.status == Status::Open)
        .collect();
    open.sort_by_key(|p| (p.kind != ProblemKind::Failure, p.id));
    let groups = [
        ("Open", ids(open)),
        (
            "Fixed",
            ids(originals
                .iter()
                .filter(|p| matches!(p.status, Status::Fixed { .. }))
                .collect()),
        ),
        (
            "Deferred",
            ids(originals
                .iter()
                .filter(|p| matches!(p.status, Status::Deferred { .. }))
                .collect()),
        ),
        (
            "Dismissed",
            ids(originals
                .iter()
                .filter(|p| matches!(p.status, Status::Dismissed { .. }))
                .collect()),
        ),
    ]
    .into_iter()
    .filter(|(_, ids)| !ids.is_empty())
    .map(|(title, ids)| Group { title, ids })
    .collect();
    (items, groups)
}

fn notes(page: &Page) -> Option<Notes> {
    let state = page.run.state();
    let mut rows = Vec::new();
    if !state.builds.is_empty() {
        let builds: Vec<String> = state
            .builds
            .iter()
            .map(|(p, b)| format!("{}: {}", platform_name(*p), b.label()))
            .collect();
        rows.push(NoteRow {
            title: "Builds",
            lines: vec![builds.join(". ")],
        });
    }

    if !state.installs.is_empty() {
        let held: Vec<String> = state
            .installs
            .iter()
            .map(|(p, install)| match install {
                Install::Started => format!("{}: {}", platform_name(*p), install.describe()),
                Install::Failed(_) => format!(
                    "{}: {}. Nothing runs there until a build installs.",
                    platform_name(*p),
                    install.describe()
                ),
            })
            .collect();
        rows.push(NoteRow {
            title: "Installs",
            lines: held,
        });
    }

    let mut reruns = Vec::new();
    let mut harness = Vec::new();
    for n in &state.notes {
        // A harness gap filed the row BLOCKED; once a later attempt files something else, it's history.
        let still_blocked = state
            .rows
            .get(&(n.spec.clone(), n.platform))
            .is_some_and(|r| {
                matches!(
                    r.row,
                    Row::Filed {
                        result: Final::Blocked,
                        ..
                    }
                )
            });
        let since = if n.text.starts_with("harness gap") && !still_blocked {
            " (an earlier attempt; it has run again since)"
        } else {
            ""
        };
        let line = format!(
            "{} on {}: {}{since}",
            n.spec,
            platform_name(n.platform),
            n.text
        );
        if n.text.starts_with("rerun") || n.text.starts_with("queued again") {
            reruns.push(line);
        } else {
            harness.push(line);
        }
    }
    for (title, lines) in [("Reruns", reruns), ("Harness and environment", harness)] {
        if !lines.is_empty() {
            rows.push(NoteRow { title, lines });
        }
    }

    if !state.unclear_specs.is_empty() {
        let specs: Vec<&str> = state.unclear_specs.iter().map(String::as_str).collect();
        rows.push(NoteRow {
            title: "Specs flagged unclear",
            lines: vec![format!(
                "{}. Reword them with create-test.",
                specs.join(", ")
            )],
        });
    }
    if rows.is_empty() {
        return None;
    }

    Some(Notes {
        count: state.notes.len() + usize::from(!state.builds.is_empty()) + state.installs.len(),
        rows,
    })
}

fn timeline(page: &Page) -> BTreeMap<String, Line> {
    page.run
        .entries_after(0)
        .iter()
        .map(|e| {
            (
                e.seq.to_string(),
                Line {
                    at: e.at,
                    text: events::summary(e),
                },
            )
        })
        .collect()
}
