//! The run's page: one renderer for the live page and the export, so what's watched is what's kept.

use crate::run::event::Event;
use crate::run::journal::Entry;
use crate::run::model::{Answer, Call, Final, Posted, ProblemKind, ProblemSource, Status};
use crate::run::usage;
use crate::run::verdict::{Verdict, VerdictStatus, verdict};
use crate::run::{Row, RowRecord, Run};
use crate::serve::events;
use crate::suite::Suite;
use crate::suite::config::Platform;
use crate::suite::spec::Spec;
use base64::Engine;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

const CSS: &str = include_str!("report.css");
const JS: &str = include_str!("report.js");
pub const ICON_180: &[u8] = include_bytes!("icon-180.png");
pub const ICON_512: &[u8] = include_bytes!("icon-512.png");
/// No service worker: a live report shown stale while offline is worse than one that says it can't connect.
pub const MANIFEST: &str = r##"{"name":"soundcheck","short_name":"soundcheck","start_url":"/","scope":"/","display":"standalone","background_color":"#f5f5f7","theme_color":"#f5f5f7","icons":[{"src":"/icon-180.png","sizes":"180x180","type":"image/png"},{"src":"/icon-512.png","sizes":"512x512","type":"image/png","purpose":"any maskable"}]}"##;
// The manifest is fetched without cookies unless asked, so behind --remote's sign-in it got the sign-in redirect.
const APP_HEAD: &str = r##"<link rel="manifest" href="/manifest.webmanifest" crossorigin="use-credentials"><link rel="apple-touch-icon" href="/icon-180.png"><meta name="mobile-web-app-capable" content="yes"><meta name="apple-mobile-web-app-capable" content="yes"><meta name="apple-mobile-web-app-title" content="soundcheck"><meta name="theme-color" content="#f5f5f7" media="(prefers-color-scheme: light)"><meta name="theme-color" content="#000000" media="(prefers-color-scheme: dark)">"##;
/// A check waiting this long for the coordinator turns the bar amber.
const CHECKING_WARN_MS: u64 = 5 * 60_000;
/// No journal event for this long while specs remain turns the bar amber.
const IDLE_WARN_MS: u64 = 15 * 60_000;

pub struct Page<'a> {
    pub suite: &'a Suite,
    pub run: &'a Run,
    /// The release under test, e.g. "v2.16.2 · candidate".
    pub label: &'a str,
    /// The live page follows the event stream; an export is a still copy.
    pub live: bool,
    /// Where the run keeps its evidence; screenshots are linked from it live and embedded in an export.
    pub run_dir: Option<&'a Path>,
    /// Set for an export: when it was made, and whether the run had finished.
    pub export: Option<Export>,
    /// Set while the run is starting: its checks are shown in place of what is running.
    pub starting: Option<&'a StartUp>,
}

/// A run that is starting, before any tester can go: the checks passed so far, the step under way and since when, or
/// why it stopped.
#[derive(Debug, Clone, Default)]
pub struct StartUp {
    pub passed: Vec<String>,
    pub doing: Option<(String, u64)>,
    pub failed: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Export {
    pub at: u64,
    pub finished: bool,
}

/// Screenshots and other evidence a tester or the server saved for a row.
pub const EVIDENCE_DIR: &str = "evidence";

/// A file name evidence can use: no folders, nothing hidden.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The prefix every evidence file of one attempt shares, e.g. `annotations-003-eraser-1-ios`.
pub fn evidence_base(spec: &str, attempt: u32, platform: Platform) -> String {
    format!("{}-{attempt}-{platform}", spec.replace('/', "-"))
}

/// Evidence is kept at what a tester's Read shows, which is detail enough for a person too, and as WebP: a full-size
/// PNG made a full run's report 340 MB.
const SHOT_MAX_SIDE: u32 = 2000;
const SHOT_QUALITY: f32 = 80.0;

/// A screenshot among evidence files: WebP, or PNG from older runs.
pub fn is_shot(name: &str) -> bool {
    name.ends_with(".webp") || name.ends_with(".png")
}

/// Stores the device screenshot at `png` as evidence at `to`, scaled down and as WebP. `png` stays.
pub fn store_shot(png: &Path, to: &Path) -> anyhow::Result<()> {
    let img = image::open(png)?.into_rgb8();
    let (w, h) = img.dimensions();
    let long = w.max(h);
    let img = if long > SHOT_MAX_SIDE {
        let scale = f64::from(SHOT_MAX_SIDE) / f64::from(long);
        let size = |v: u32| ((f64::from(v) * scale).round() as u32).max(1);
        image::imageops::resize(
            &img,
            size(w),
            size(h),
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    let (w, h) = img.dimensions();
    let encoded = webp::Encoder::from_rgb(&img, w, h).encode(SHOT_QUALITY);
    std::fs::write(to, &*encoded)?;
    Ok(())
}

/// Takes a screenshot through `capture` (which writes a PNG) and stores it as evidence at `to`.
pub async fn capture_shot<F, E>(to: &Path, capture: F) -> anyhow::Result<()>
where
    F: AsyncFnOnce(&Path) -> Result<(), E>,
    E: std::error::Error + Send + Sync + 'static,
{
    let png = to.with_extension("png");
    capture(&png).await?;
    let (from, dest) = (png.clone(), to.to_path_buf());
    let stored = tokio::task::spawn_blocking(move || store_shot(&from, &dest)).await?;
    let _ = std::fs::remove_file(&png);
    stored
}

/// The file a tester's screenshot `name` is saved as: under its attempt's prefix, ending `.webp`.
pub fn shot_file(base: &str, name: &str) -> String {
    let name = name.trim_end_matches(".webp").trim_end_matches(".png");
    let name = name.strip_prefix(&format!("{base}-")).unwrap_or(name);
    format!("{base}-{name}.webp")
}

/// The evidence file a tester's `name` refers to, if it was saved: its attempt's own file first, then the name as
/// given (or with `.webp` or `.png`), which covers the server's captures and older runs' unprefixed shots.
pub fn resolve_evidence(dir: &Path, base: &str, name: &str) -> Option<String> {
    let shot = shot_file(base, name);
    [
        shot.clone(),
        shot.replace(".webp", ".png"),
        name.to_string(),
        format!("{name}.webp"),
        format!("{name}.png"),
    ]
    .into_iter()
    .find(|f| safe_name(f) && dir.join(f).is_file())
}

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The whole document.
pub fn document(page: &Page) -> String {
    let seq = page.run.last_seq();
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1,viewport-fit=cover\">{APP_HEAD}<title>{} · soundcheck</title><style>{CSS}</style><script>try{{var t=localStorage.getItem(\"soundcheck-theme\");if(t===\"light\"||t===\"dark\")document.documentElement.dataset.theme=t}}catch(e){{}}</script></head><body><div id=\"live\" data-live=\"{}\" data-seq=\"{seq}\">{}</div><script>{JS}</script></body></html>\n",
        esc(page.label),
        page.live && !page.run.state().finished,
        content(page),
    )
}

/// What the live page swaps in after each event.
pub fn content(page: &Page) -> String {
    let mut out = String::new();
    if page.live && !page.run.state().finished {
        match page.starting {
            Some(starting) => out.push_str(&start_bar(starting)),
            None => out.push_str(&now_bar(page)),
        }
    }
    let _ = write!(
        out,
        "<main style=\"--platforms:{}\">",
        platforms(page).len().max(1)
    );
    out.push_str(&header(page));
    let v = verdict(page.run.state());
    out.push_str(&summary(page, &v));
    out.push_str(&sections(page));
    out.push_str(&problems(page));
    out.push_str(&notes(page));
    out.push_str(&timeline(page));
    let _ = write!(
        out,
        "<footer><div class=\"segmented\" role=\"group\" aria-label=\"Appearance\"><button class=\"theme-choice\" type=\"button\" data-theme-choice=\"auto\" aria-pressed=\"true\">Auto</button><button class=\"theme-choice\" type=\"button\" data-theme-choice=\"light\" aria-pressed=\"false\">Light</button><button class=\"theme-choice\" type=\"button\" data-theme-choice=\"dark\" aria-pressed=\"false\">Dark</button></div><p>Generated by soundcheck {} from the run's journal. Local stack only; nothing here touched production.</p></footer></main>",
        crate::VERSION
    );
    out
}

fn platforms(page: &Page) -> Vec<Platform> {
    let mut all: Vec<Platform> = page
        .run
        .plan()
        .specs
        .iter()
        .flat_map(|s| s.platforms.iter().copied())
        .collect();
    all.sort();
    all.dedup();
    all
}

fn platform_name(p: Platform) -> &'static str {
    match p {
        Platform::Android => "Android",
        Platform::Ios => "iOS",
        Platform::Web => "Web",
    }
}

fn feature_name(feature: &str) -> String {
    let words = feature.replace('-', " ");
    let mut chars = words.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn anchor(spec: &str) -> String {
    format!("spec-{}", spec.replace('/', "-"))
}

fn time(at: u64) -> String {
    // UTC as text for a page without script; the script shows it in the viewer's zone.
    let secs = at / 1000;
    format!(
        "<time data-at=\"{at}\">{:02}:{:02} UTC</time>",
        (secs / 3600) % 24,
        (secs / 60) % 60
    )
}

fn date_time(at: u64) -> String {
    let secs = at / 1000;
    format!(
        "<time data-at=\"{at}\" data-format=\"date\">{:02}:{:02} UTC</time>",
        (secs / 3600) % 24,
        (secs / 60) % 60
    )
}

/// A warning that stays hidden until `warn_ms` after `since`, then reads `label` with `{t}` as the time passed.
fn overdue(since: u64, warn_ms: u64, label: &str) -> String {
    format!(
        "<span class=\"ago warn\" data-since=\"{since}\" data-warn=\"{warn_ms}\" data-label=\"{}\" hidden></span>",
        esc(label)
    )
}

/// The longest quiet spell that still counts as working: a spec runs for minutes between journal entries, but a run
/// left overnight and rerun the next morning shouldn't count the night.
const ACTIVE_GAP_MS: u64 = 30 * 60_000;

/// How long the run has been working: the time between its journal entries, leaving out quiet spells longer than
/// [`ACTIVE_GAP_MS`].
pub fn active_ms(entries: &[Entry]) -> u64 {
    entries
        .windows(2)
        .map(|w| w[1].at.saturating_sub(w[0].at))
        .filter(|gap| *gap <= ACTIVE_GAP_MS)
        .sum()
}

/// 724k, 1.2M: a header fact, where the exact count is noise.
pub fn thousands(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

fn now_bar(page: &Page) -> String {
    let state = page.run.state();
    let entries = page.run.entries_after(0);
    let live = if state.paused {
        "<span class=\"nowbar-live warn\">Paused</span>"
    } else {
        "<span class=\"nowbar-live\"><i class=\"live-dot\" aria-hidden=\"true\"></i><b>Live</b></span>"
    };
    let mut doing = Vec::new();
    for ((spec, platform), r) in &state.rows {
        match &r.row {
            Row::Running { .. } => doing.push(format!(
                "<span>{} · <b>{}</b></span>",
                platform_name(*platform),
                esc(spec)
            )),
            Row::NeedsReview { since, .. } => doing.push(format!(
                "<span>{} · checking <b>{}</b> {}</span>",
                platform_name(*platform),
                esc(spec),
                overdue(*since, CHECKING_WARN_MS, "for {t}")
            )),
            _ => {}
        }
    }
    if doing.is_empty() {
        doing.push("<span class=\"muted\">Nothing running</span>".into());
    }
    if let Some(last) = entries.last() {
        doing.push(overdue(
            last.at,
            IDLE_WARN_MS,
            "idle {t}: coordinator not responding",
        ));
    }
    let timer = entries
        .last()
        .map(|last| {
            let counting = if state.finished || state.paused {
                String::new()
            } else {
                format!(" data-since=\"{}\" data-cap=\"{ACTIVE_GAP_MS}\"", last.at)
            };
            format!(
                "<span class=\"nowbar-time elapsed\" data-active=\"{}\"{counting} title=\"Time the run has been working, without the gaps between sessions\"></span>",
                active_ms(entries)
            )
        })
        .unwrap_or_default();
    format!(
        "<div class=\"nowbar\" role=\"status\"><div class=\"nowbar-in\">{live}<div class=\"nowbar-doing\">{}</div>{timer}</div></div>",
        doing.join("")
    )
}

fn start_bar(starting: &StartUp) -> String {
    let (live, doing) = match (&starting.failed, &starting.doing) {
        (Some(why), _) => (
            "<span class=\"nowbar-live warn\">Couldn't start</span>",
            format!("<span class=\"start-failed\">{}</span>", esc(why)),
        ),
        (None, Some((step, since))) => (
            "<span class=\"nowbar-live\"><i class=\"live-dot\" aria-hidden=\"true\"></i><b>Starting</b></span>",
            format!(
                "<span><b>{}</b> <span class=\"ago\" data-since=\"{since}\" data-warn=\"0\" data-label=\"· {{t}}\"></span></span>",
                esc(step)
            ),
        ),
        (None, None) => (
            "<span class=\"nowbar-live\"><i class=\"live-dot\" aria-hidden=\"true\"></i><b>Starting</b></span>",
            "<span class=\"muted\">Checking</span>".to_string(),
        ),
    };
    format!(
        "<div class=\"nowbar\" role=\"status\"><div class=\"nowbar-in\">{live}<div class=\"nowbar-doing\">{doing}</div></div></div>"
    )
}

fn header(page: &Page) -> String {
    let entries = page.run.entries_after(0);
    let mut facts: Vec<String> = platforms(page)
        .iter()
        .map(|p| format!("<span>{}</span>", platform_name(*p)))
        .collect();
    if let Some(first) = entries.first() {
        facts.push(format!("<span>Started {}</span>", time(first.at)));
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
            facts.push(format!(
                "<span title=\"Testers' working time on each platform\">Active {}</span>",
                each.join(" · ")
            ));
        } else if let Some((_, ms)) = per_platform.first() {
            facts.push(format!(
                "<span title=\"Testers' working time, added up across attempts\">Active {}</span>",
                took(*ms)
            ));
        }
    } else if active_ms(entries) >= 60_000 {
        // A run from before usage was recorded falls back to the time between its journal entries.
        facts.push(format!(
            "<span title=\"Time the run spent working, without the gaps between sessions\">Active {}</span>",
            took(active_ms(entries))
        ));
    }
    let tokens: u64 = attempts.iter().filter_map(|a| a.tokens).sum();
    if tokens > 0 {
        facts.push(format!(
            "<span title=\"Testers' tokens, added up across attempts\">Tokens {}</span>",
            thousands(tokens)
        ));
    }
    if let Some(export) = page.export {
        facts.push(format!(
            "<span>{} {}</span>",
            if export.finished {
                "Final report, exported"
            } else {
                "In progress, exported"
            },
            time(export.at)
        ));
    }
    // Relative, so it works on the live page (`/download`) and a past run's (`/runs/<name>/download`).
    let download = if page.export.is_none() {
        "<a class=\"download\" href=\"download\" download>Download report</a>"
    } else {
        ""
    };
    // An export has no server to list the runs.
    let menu = if page.export.is_none() {
        "<button class=\"runs-menu\" type=\"button\" aria-label=\"All runs\" aria-haspopup=\"dialog\"><svg viewBox=\"0 0 20 20\" aria-hidden=\"true\"><path d=\"M3 5h14M3 10h14M3 15h14\"/></svg></button>"
    } else {
        ""
    };
    format!(
        "<header class=\"head\"><div class=\"head-text\"><div class=\"head-top\">{menu}<span class=\"eyebrow\">Soundcheck · local regression</span></div><h1>{}</h1><div class=\"facts\">{}</div>{download}</div></header>",
        esc(page.label),
        facts.join("")
    )
}

#[derive(Default, Clone, Copy)]
struct Tally {
    total: u32,
    pass: u32,
    fail: u32,
    blocked: u32,
    checking: u32,
    running: u32,
}

impl Tally {
    fn add(&mut self, r: &RowRecord) {
        self.total += 1;
        match &r.row {
            Row::Filed { result, .. } => match result {
                Final::Pass => self.pass += 1,
                Final::Fail => self.fail += 1,
                Final::Blocked => self.blocked += 1,
            },
            Row::NeedsReview { .. } => self.checking += 1,
            Row::Running { .. } => self.running += 1,
            Row::NotRun | Row::Queued { .. } => {}
        }
    }
    fn pct(&self, n: u32) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            f64::from(n) * 100.0 / f64::from(self.total)
        }
    }
}

fn ring(t: &Tally) -> String {
    let c = 2.0 * std::f64::consts::PI * 44.0;
    let mut out = String::from(
        "<svg class=\"ring\" viewBox=\"0 0 120 120\" aria-hidden=\"true\"><circle cx=\"60\" cy=\"60\" r=\"44\" class=\"ring-bg\"/>",
    );
    let mut offset = 0.0;
    for (class, n) in [("pass", t.pass), ("fail", t.fail), ("blocked", t.blocked)] {
        if n == 0 {
            continue;
        }
        let len = c * t.pct(n) / 100.0;
        let _ = write!(
            out,
            "<circle cx=\"60\" cy=\"60\" r=\"44\" class=\"ring-seg {class}\" stroke-dasharray=\"{len:.1} {:.1}\" stroke-dashoffset=\"{:.1}\"/>",
            c - len,
            -offset
        );
        offset += len;
    }
    out.push_str("</svg>");
    out
}

fn summary(page: &Page, v: &Verdict) -> String {
    let state = page.run.state();
    let mut all = Tally::default();
    for r in state.rows.values() {
        all.add(r);
    }
    let (class, word) = match v.status {
        VerdictStatus::Failing => ("failing", "Failing"),
        VerdictStatus::NotReady => ("", "Not ready"),
        VerdictStatus::Ready => ("", "Ready"),
    };
    let reasons: String = v
        .reasons
        .iter()
        .map(|r| {
            let bad = if r.contains("failure") { " bad" } else { "" };
            format!("<li class=\"chip reason{bad}\">{}</li>", esc(r))
        })
        .collect();
    let mut grid =
        String::from("<div class=\"grid\"><div class=\"grid-row grid-head\"><span>Section</span>");
    let shown = platforms(page);
    for p in &shown {
        let _ = write!(grid, "<span>{}</span>", platform_name(*p));
    }
    grid.push_str("</div>");
    for feature in &page.suite.features {
        let _ = write!(
            grid,
            "<a class=\"grid-row\" href=\"#section-{}\"><span class=\"grid-name\">{}</span>",
            esc(&feature.name),
            esc(&feature_name(&feature.name))
        );
        for p in &shown {
            let mut t = Tally::default();
            for ((spec, platform), r) in &state.rows {
                if platform == p && spec.split('/').next() == Some(feature.name.as_str()) {
                    t.add(r);
                }
            }
            grid.push_str(&cell(&t));
        }
        grid.push_str("</a>");
    }
    grid.push_str("</div>");
    let pct = all.pct(all.pass);
    // Three digits don't fit the ring's hole at the two-digit size.
    let size = if pct.round() >= 100.0 {
        " class=\"full\""
    } else {
        ""
    };
    format!(
        "<section class=\"summary card\" aria-label=\"Summary\"><div class=\"summary-head\"><div class=\"ringwrap\">{}<div class=\"ringlabel\"><b{size}>{pct:.0}%</b></div></div><div class=\"summary-verdict\"><strong class=\"{class}\">{word}</strong>{}</div></div>{grid}</section>",
        ring(&all),
        if reasons.is_empty() {
            String::new()
        } else {
            format!("<ul class=\"reasons\" aria-label=\"What's in the way\">{reasons}</ul>")
        },
    )
}

fn cell(t: &Tally) -> String {
    if t.total == 0 {
        return "<span class=\"cell\"><span class=\"cell-text\">–</span></span>".into();
    }
    let done = t.pass;
    let (class, text) = if t.pass + t.fail + t.blocked + t.checking + t.running == 0 {
        ("", "Not run".to_string())
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
        ("pass", format!("{done}/{} passed", t.total))
    } else {
        ("pass", format!("{done} of {} passed", t.total))
    };
    format!(
        "<span class=\"cell {class}\"><span class=\"cell-text\"><i></i>{text}</span><span class=\"mini\"><i class=\"pass\" style=\"width:{:.0}%\"></i><i class=\"fail\" style=\"width:{:.0}%\"></i><i class=\"blocked\" style=\"width:{:.0}%\"></i><i class=\"review\" style=\"width:{:.0}%\"></i><i class=\"pending\" style=\"width:{:.0}%\"></i></span></span>",
        t.pct(t.pass),
        t.pct(t.fail),
        t.pct(t.blocked),
        t.pct(t.checking),
        t.pct(t.running)
    )
}

fn result_cell(r: Option<&RowRecord>) -> String {
    let Some(r) = r else {
        return "<span class=\"res na\">–</span>".into();
    };
    match &r.row {
        Row::Filed { result: Final::Pass, .. } => "<span class=\"res pass\">Passed</span>".into(),
        Row::Filed { result: Final::Fail, .. } => "<span class=\"res fail\">Failed</span>".into(),
        Row::Filed { result: Final::Blocked, .. } => "<span class=\"res blocked\">Blocked</span>".into(),
        Row::NeedsReview { .. } => "<span class=\"res review\" title=\"The tester reported a problem; the coordinator is checking it\">Checking</span>".into(),
        Row::Running { .. } => "<span class=\"res running\">Running</span>".into(),
        Row::Queued { .. } => "<span class=\"res not-run\">Queued</span>".into(),
        Row::NotRun => "<span class=\"res not-run\">Not run</span>".into(),
    }
}

fn sections(page: &Page) -> String {
    let shown = platforms(page);
    let mut out = String::from(
        "<div class=\"toolbar\"><h2>Checks by section</h2><div class=\"segmented\" role=\"group\" aria-label=\"Show\"><button class=\"filter\" type=\"button\" data-filter=\"all\" aria-pressed=\"true\">All</button><button class=\"filter\" type=\"button\" data-filter=\"attn\" aria-pressed=\"false\">Needs attention</button></div></div><div class=\"sections\">",
    );
    for feature in &page.suite.features {
        let specs: Vec<&Spec> = page
            .suite
            .specs
            .iter()
            .filter(|s| {
                s.feature == feature.name && page.run.plan().specs.iter().any(|p| p.id == s.id)
            })
            .collect();
        if specs.is_empty() {
            continue;
        }
        let _ = write!(
            out,
            "<section class=\"section card\" id=\"section-{}\"><header><h3>{}</h3></header><div class=\"checks-head\"><span>ID</span><span>Check</span>",
            esc(&feature.name),
            esc(&feature_name(&feature.name))
        );
        for p in &shown {
            let _ = write!(out, "<span>{}</span>", platform_name(*p));
        }
        out.push_str("</div>");
        for spec in specs {
            out.push_str(&check(page, spec, &shown));
        }
        out.push_str("</section>");
    }
    out.push_str("</div>");
    out
}

fn findings_for<'p>(
    page: &'p Page,
    spec: &str,
    platform: Platform,
) -> Vec<&'p crate::run::model::Problem> {
    page.run
        .state()
        .problems
        .iter()
        .filter(|p| p.spec == spec && p.platform == platform && p.kind == ProblemKind::Finding)
        .collect()
}

fn check(page: &Page, spec: &Spec, shown: &[Platform]) -> String {
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
        .unwrap_or("");
    let mut attention = false;
    // Findings belong to the check, not one platform's result: one pill by the title counts them all.
    let findings: Vec<_> = shown
        .iter()
        .flat_map(|p| findings_for(page, &spec.id, *p))
        .collect();
    let open_findings = findings.iter().filter(|f| f.status == Status::Open).count();
    let pill = match findings.len() {
        0 => String::new(),
        n => format!(
            "<span class=\"chip small{}\">{n} finding{}</span>",
            if open_findings > 0 {
                " finding"
            } else {
                " muted"
            },
            if n == 1 { "" } else { "s" }
        ),
    };
    let mut cells = String::new();
    for p in shown {
        let r = state.rows.get(&(spec.id.clone(), *p));
        let c = result_cell(r);
        if let Some(r) = r
            && matches!(
                r.row,
                Row::Filed {
                    result: Final::Fail | Final::Blocked,
                    ..
                } | Row::NeedsReview { .. }
            )
        {
            attention = true;
        }
        // Named, for a narrow screen where the platform columns' headings are hidden.
        cells.push_str(&c.replacen(
            "<span class=\"res",
            &format!("<span data-p=\"{}\" class=\"res", platform_name(*p)),
            1,
        ));
    }
    attention |= open_findings > 0;

    let with_detail: Vec<&(Platform, &RowRecord)> = records
        .iter()
        .filter(|(p, r)| {
            r.posted.is_some()
                || matches!(r.row, Row::Filed { .. })
                || !findings_for(page, &spec.id, *p).is_empty()
        })
        .collect();
    let label_platforms = records.len() > 1;
    let mut body = String::new();
    for (platform, r) in &records {
        // A rerun's earlier result gives way to the attempt under way.
        if waiting(r) {
            body.push_str(&pending(spec, *platform, r, label_platforms));
        } else if with_detail.iter().any(|(p, _)| p == platform) {
            body.push_str(&detail(page, spec, *platform, r, label_platforms));
        }
    }
    let open = records.iter().any(|(_, r)| {
        matches!(
            r.row,
            Row::Filed {
                result: Final::Fail | Final::Blocked,
                ..
            }
        )
    });
    format!(
        "<details class=\"check\" id=\"{}\"{}{}><summary><span class=\"cid\" title=\"{}\">{}</span><span class=\"ctitle\">{}{pill}</span>{cells}</summary>{}</details>",
        anchor(&spec.id),
        if attention { " data-attn" } else { "" },
        if open { " open" } else { "" },
        esc(&spec.id),
        esc(number),
        esc(&spec.front.title),
        if body.is_empty() {
            String::new()
        } else {
            format!("<div class=\"check-body\">{body}</div>")
        }
    )
}

/// Whether the row's current attempt has no result in yet.
fn waiting(r: &RowRecord) -> bool {
    matches!(
        r.row,
        Row::NotRun | Row::Queued { .. } | Row::Running { .. }
    )
}

/// The outcomes of a row with no result yet, so a reader can see what it will check while it waits or runs.
fn pending(spec: &Spec, platform: Platform, r: &RowRecord, label: bool) -> String {
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
    let mut out = String::new();
    if label {
        let _ = write!(
            out,
            "<span class=\"platform-label\">{}</span>",
            platform_name(platform)
        );
    }
    let state = esc(&state);
    let _ = write!(
        out,
        "<p class=\"pending-state\">{state}</p><ul class=\"outcomes\">"
    );
    let class = if running {
        "pending running"
    } else {
        "pending"
    };
    for (i, outcome) in spec.outcomes.iter().enumerate() {
        // Judged so far: held reads as held, anything else waits for the coordinator's call, like a claim in review.
        match r.provisional.iter().find(|j| j.outcome == i) {
            Some(j) if running => {
                let (mark_class, mark) = if j.answer == Answer::Held {
                    ("held", "✓")
                } else {
                    ("unsure", "?")
                };
                let _ = write!(
                    out,
                    "<li><span class=\"mark {mark_class}\" aria-label=\"judged so far\">{mark}</span><span><span class=\"o\">{}</span><span class=\"obs\">{}</span></span></li>",
                    esc(outcome),
                    esc(&j.observed)
                );
            }
            _ => {
                let _ = write!(
                    out,
                    "<li><span class=\"mark {class}\" aria-label=\"{state}\"></span><span><span class=\"o pending\">{}</span></span></li>",
                    esc(outcome)
                );
            }
        }
    }
    out.push_str("</ul>");
    out
}

/// An outcome's wording as the latest result judged it, so a spec edited since then doesn't reword the result.
fn judged_as<'a>(spec: &'a Spec, r: &'a RowRecord, i: usize) -> &'a str {
    r.judged_against
        .get(i)
        .or_else(|| spec.outcomes.get(i))
        .map_or("", String::as_str)
}

fn detail(page: &Page, spec: &Spec, platform: Platform, r: &RowRecord, label: bool) -> String {
    let mut out = String::new();
    let name = platform_name(platform);
    if label {
        let _ = write!(out, "<span class=\"platform-label\">{name}</span>");
    }
    let reason = r.review.as_ref().map(|(_, reason)| reason.as_str());
    let failed = matches!(
        r.row,
        Row::Filed {
            result: Final::Fail,
            ..
        }
    );

    match &r.row {
        Row::Filed {
            result: Final::Fail,
            reason: filed,
            ..
        } => {
            let call = reason.unwrap_or(filed);
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
                .map(|p| thumbs(page, &p.evidence))
                .unwrap_or_default();
            // Why it was filed and what it looked like, inside the failed outcome's red box. A judged Unsure already
            // carries the call in its note.
            let judged = matches!(r.review, Some((Call::Judge { .. }, _)));
            let evidence = if judged {
                shots
            } else {
                format!(
                    "<span class=\"callline\">{}{}</span>{shots}",
                    if r.verifying {
                        "Reproduced on verification. "
                    } else {
                        ""
                    },
                    esc(call),
                )
            };
            match &r.posted {
                Some(posted) => out.push_str(&outcomes(spec, posted, r, Some(&evidence))),
                None => {
                    let _ = write!(out, "<div class=\"failed-outcome\">{evidence}</div>");
                }
            }
        }
        Row::Filed {
            result: Final::Blocked,
            reason: filed,
            ..
        } => {
            let _ = write!(
                out,
                "<div class=\"blocked-block\"><span class=\"expect\">Couldn't be tested on {name}.</span><span class=\"saw\">{}. It counts as not tested until a rerun passes.</span></div>",
                esc(reason.unwrap_or(filed).trim_end_matches('.'))
            );
        }
        _ => {}
    }
    for f in findings_for(page, &spec.id, platform) {
        let _ = write!(
            out,
            "<div class=\"finding-block\"><span class=\"expect\">Finding {:03}: {}</span><span class=\"saw\">{} <a href=\"#problem-{}\">In Problems</a></span>{}</div>",
            f.id,
            esc(&f.title),
            esc(&f.detail),
            f.id,
            thumbs(page, &f.evidence)
        );
    }
    if let Some(posted) = &r.posted {
        if !failed {
            out.push_str(&outcomes(spec, posted, r, None));
        }
        out.push_str(&evidence(page, posted, &r.captured));
    }
    out
}

/// `skip_shown`: a failed row already shows its failed outcomes in the red block, so they aren't listed twice.
/// The outcomes as judged. `failure` is the filed failure's call and screenshots, which go inside its last failed
/// outcome, so the red box holds the outcome and its evidence together.
fn outcomes(spec: &Spec, posted: &Posted, r: &RowRecord, failure: Option<&str>) -> String {
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
    let listed: Vec<_> = posted.judgments.iter().collect();
    if listed.is_empty() {
        return String::new();
    }
    let last_failed = listed
        .iter()
        .rposition(|j| shown(j.answer) == Answer::Failed);
    let mut out = String::from("<ul class=\"outcomes\">");
    for (i, j) in listed.into_iter().enumerate() {
        let (class, mark, label) = match shown(j.answer) {
            Answer::Held => ("held", "✓", "held"),
            Answer::Failed => ("failed", "✕", "failed"),
            Answer::Unsure => ("unsure", "?", "unsure"),
            Answer::CouldntCheck => ("blocked", "–", "couldn't check"),
        };
        let note = match (j.answer, judged) {
            (Answer::Unsure, Some((result, reason))) => format!(
                "<span class=\"settled\">The tester was unsure; the coordinator judged it {}: {}</span>",
                if result == Final::Fail {
                    "failed"
                } else {
                    "passed"
                },
                esc(reason)
            ),
            _ => String::new(),
        };
        let (row, evidence) = match failure {
            Some(evidence) if Some(i) == last_failed => (" class=\"failed-outcome\"", evidence),
            _ if shown(j.answer) == Answer::Failed && failure.is_some() => {
                (" class=\"failed-outcome\"", "")
            }
            _ => ("", ""),
        };
        let _ = write!(
            out,
            "<li{row}><span class=\"mark {class}\" aria-label=\"{label}\">{mark}</span><span><span class=\"o\">{}</span><span class=\"obs\">{}</span>{note}{evidence}</span></li>",
            esc(judged_as(spec, r, j.outcome)),
            esc(&j.observed)
        );
    }
    out.push_str("</ul>");
    out
}

fn image(page: &Page, name: &str) -> Option<String> {
    let dir = page.run_dir?.join(EVIDENCE_DIR);
    // Older journals hold names as the tester typed them, sometimes without an extension.
    let name = [
        name.to_string(),
        format!("{name}.webp"),
        format!("{name}.png"),
    ]
    .into_iter()
    .find(|f| safe_name(f) && dir.join(f).is_file())?;
    let path = dir.join(&name);
    if page.export.is_some() {
        let bytes = std::fs::read(&path).ok()?;
        let kind = match path.extension().and_then(|e| e.to_str()) {
            Some("webp") => "image/webp",
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            _ => return None,
        };
        Some(format!(
            "data:{kind};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    } else {
        Some(format!("files/{EVIDENCE_DIR}/{name}"))
    }
}

/// A row of screenshot thumbnails; each opens in the page's lightbox.
fn thumbs<S: AsRef<str>>(page: &Page, shots: &[S]) -> String {
    if shots.is_empty() {
        return String::new();
    }
    let mut out =
        String::from("<div class=\"thumbs\" aria-label=\"Screenshots\" style=\"margin-top:12px\">");
    for s in shots.iter().map(AsRef::as_ref) {
        match image(page, s) {
            Some(src) => {
                let _ = write!(
                    out,
                    "<a class=\"thumb\" href=\"{src}\" target=\"_blank\" rel=\"noopener\"><img src=\"{src}\" alt=\"{}\"></a>",
                    esc(s)
                );
            }
            None => {
                let _ = write!(out, "<div class=\"thumb\">{}</div>", esc(s));
            }
        }
    }
    out.push_str("</div>");
    out
}

fn evidence(page: &Page, posted: &Posted, captured: &[String]) -> String {
    let mut shots: Vec<&String> = posted
        .judgments
        .iter()
        .flat_map(|j| j.evidence.iter())
        .collect();
    shots.extend(captured.iter().filter(|f| is_shot(f)));
    let texts: Vec<&String> = captured.iter().filter(|f| f.ends_with(".txt")).collect();
    if shots.is_empty() && texts.is_empty() && posted.notes.is_none() {
        return String::new();
    }
    let mut out =
        String::from("<details class=\"fold\"><summary>Evidence and tester's notes</summary>");
    out.push_str(&thumbs(page, &shots));
    for t in texts {
        let label = if t.ends_with("-seen.txt") {
            "Element tree the tester last read"
        } else if t.ends_with("-tree.txt") {
            "Element tree when the result was posted"
        } else if t.ends_with("-errors.txt") {
            "Errors"
        } else {
            t.as_str()
        };
        match (page.export.is_some(), page.run_dir) {
            // An export carries the text itself, so it stays one self-contained file.
            (true, Some(dir)) if safe_name(t) => {
                let text =
                    std::fs::read_to_string(dir.join(EVIDENCE_DIR).join(t)).unwrap_or_default();
                let text: String = text.chars().take(20_000).collect();
                let _ = write!(
                    out,
                    "<details class=\"fold\"><summary>{label}</summary><pre class=\"evidence-text\">{}</pre></details>",
                    esc(&text)
                );
            }
            (false, Some(_)) if safe_name(t) => {
                let _ = write!(
                    out,
                    "<p><a href=\"files/{EVIDENCE_DIR}/{}\" target=\"_blank\" rel=\"noopener\">{label}</a></p>",
                    esc(t)
                );
            }
            _ => {}
        }
    }
    if let Some(notes) = &posted.notes {
        let _ = write!(out, "<p>{}</p>", esc(notes));
    }
    out.push_str("</details>");
    out
}

fn problems(page: &Page) -> String {
    let state = page.run.state();
    let originals: Vec<&crate::run::model::Problem> = state
        .problems
        .iter()
        .filter(|p| !matches!(p.status, Status::Duplicate { .. }))
        .collect();
    if originals.is_empty() {
        return String::new();
    }
    let item = |p: &crate::run::model::Problem, badge: String| -> String {
        let also: Vec<String> = state
            .problems
            .iter()
            .filter(|d| d.status == (Status::Duplicate { of: p.id }))
            .map(|d| platform_name(d.platform).to_string())
            .collect();
        let feature = feature_name(p.spec.split('/').next().unwrap_or(""));
        let mut body = format!("<p>{}</p>{}", esc(&p.detail), thumbs(page, &p.evidence));
        if p.verified && p.source == ProblemSource::Row {
            body.push_str("<p>Verified: reproduced on another attempt from a fresh seed.</p>");
        }
        if p.source == ProblemSource::SideFind {
            body.push_str("<p>Found while probing, outside the check itself.</p>");
            if p.evidence.is_empty() {
                body.push_str("<p>No screenshot of it; the detail above is all there is.</p>");
            }
        }
        if !also.is_empty() {
            let _ = write!(body, "<p>Also seen on {}.</p>", also.join(", "));
        }
        match &p.status {
            Status::Fixed { reason, .. }
            | Status::Deferred { reason, .. }
            | Status::Dismissed { reason } => {
                let _ = write!(body, "<p>{}</p>", esc(reason));
            }
            _ => {}
        }
        let _ = write!(
            body,
            "<p><a href=\"#{}\">Open its row</a></p>",
            anchor(&p.spec)
        );
        format!(
            "<details class=\"problem\" id=\"problem-{}\"><summary class=\"p2\"><span class=\"pticket\">{badge}</span><h3><span class=\"pid\">{:03}</span>{}</h3><span class=\"pwhere\">{} · {}</span></summary><div class=\"problem-body\">{body}</div></details>",
            p.id,
            p.id,
            esc(&p.title),
            platform_name(p.platform),
            esc(&feature)
        )
    };
    let ticket = |t: &Option<String>| {
        t.as_deref().map_or(String::new(), |t| {
            format!("<span class=\"chip ticket\">{}</span>", esc(t))
        })
    };
    let mut out = String::from("<section class=\"block\" id=\"problems\"><h2>Problems</h2>");
    let mut group = |title: &str, items: Vec<String>| {
        if !items.is_empty() {
            let _ = write!(
                out,
                "<h4 class=\"pgroup\">{title} · {}</h4><div class=\"problems\">{}</div>",
                items.len(),
                items.join("")
            );
        }
    };
    let mut open: Vec<&&crate::run::model::Problem> = originals
        .iter()
        .filter(|p| p.status == Status::Open)
        .collect();
    open.sort_by_key(|p| (p.kind != ProblemKind::Failure, p.id));
    group(
        "Open",
        open.iter()
            .map(|p| {
                let badge = match p.kind {
                    ProblemKind::Failure => "<span class=\"chip blocks\">Failure</span>",
                    ProblemKind::Finding => "<span class=\"chip finding\">Finding</span>",
                };
                item(p, badge.into())
            })
            .collect(),
    );
    group(
        "Fixed",
        originals
            .iter()
            .filter_map(|p| match &p.status {
                Status::Fixed { ticket: t, .. } => Some(item(p, ticket(t))),
                _ => None,
            })
            .collect(),
    );
    group(
        "Deferred",
        originals
            .iter()
            .filter_map(|p| match &p.status {
                Status::Deferred { ticket: t, .. } => Some(item(p, ticket(t))),
                _ => None,
            })
            .collect(),
    );
    group(
        "Dismissed",
        originals
            .iter()
            .filter(|p| matches!(p.status, Status::Dismissed { .. }))
            .map(|p| item(p, "<span class=\"chip muted\">Dismissed</span>".into()))
            .collect(),
    );
    out.push_str("</section>");
    out
}

fn notes(page: &Page) -> String {
    let state = page.run.state();
    let mut rows: Vec<(String, String)> = Vec::new();
    if !state.builds.is_empty() {
        let builds: Vec<String> = state
            .builds
            .iter()
            .map(|(p, b)| format!("{}: {}", platform_name(*p), esc(&b.label())))
            .collect();
        rows.push(("Builds".into(), builds.join(". ")));
    }
    let prepared = preparation(page.run.entries_after(0));
    if !prepared.is_empty() {
        rows.push(("Preparation".into(), prepared.join(". ")));
    }
    let mut kinds: Vec<(&str, Vec<String>)> =
        vec![("Reruns", vec![]), ("Harness and environment", vec![])];
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
            esc(&n.spec),
            platform_name(n.platform),
            esc(&n.text)
        );
        if n.text.starts_with("rerun") || n.text.starts_with("queued again") {
            kinds[0].1.push(line);
        } else {
            kinds[1].1.push(line);
        }
    }
    for (title, lines) in kinds {
        if !lines.is_empty() {
            rows.push((title.into(), lines.join("<br>")));
        }
    }
    if !state.unclear_specs.is_empty() {
        let specs: Vec<String> = state.unclear_specs.iter().map(|s| esc(s)).collect();
        rows.push((
            "Specs flagged unclear".into(),
            format!("{}. Reword them with create-test.", specs.join(", ")),
        ));
    }
    if rows.is_empty() {
        return String::new();
    }
    let dl: String = rows
        .iter()
        .map(|(t, d)| format!("<dt>{t}</dt><dd>{d}</dd>"))
        .collect();
    let count = state.notes.len()
        + usize::from(!state.builds.is_empty())
        + usize::from(!prepared.is_empty());
    format!(
        "<section class=\"block\"><h2>Notes</h2><div class=\"card notes\"><details class=\"fold\" id=\"notes\"><summary>Show all {count} notes</summary><dl>{dl}</dl></details></div></section>"
    )
}

/// Each platform's latest preparation (suite.yaml `prepare`): how long it took, or why it couldn't be done.
fn preparation(entries: &[Entry]) -> Vec<String> {
    let mut started: BTreeMap<String, u64> = BTreeMap::new();
    let mut latest: BTreeMap<Platform, String> = BTreeMap::new();
    for e in entries {
        match &e.event {
            Event::PrepareDispatched {
                platform,
                attempt,
                token,
                ..
            } => {
                started.insert(token.clone(), e.at);
                latest.insert(*platform, format!("preparing (attempt {attempt})"));
            }
            Event::Prepared { platform, token } => {
                let took = started.get(token).map_or(0, |at| e.at.saturating_sub(*at)) / 1000;
                latest.insert(*platform, format!("prepared in {took} s"));
            }
            Event::PrepareFailed {
                platform,
                reason,
                last,
                ..
            } => {
                let what = if *last {
                    "couldn't be prepared"
                } else {
                    "not prepared, trying again"
                };
                latest.insert(*platform, format!("{what}: {}", esc(reason)));
            }
            _ => {}
        }
    }
    latest
        .into_iter()
        .map(|(p, line)| format!("{}: {line}", platform_name(p)))
        .collect()
}

fn timeline(page: &Page) -> String {
    let entries = page.run.entries_after(0);
    if entries.is_empty() {
        return String::new();
    }
    let lines: String = entries
        .iter()
        .rev()
        .map(|e| {
            format!(
                "<li><span>{}</span>{}</li>",
                time(e.at),
                esc(&events::summary(e))
            )
        })
        .collect();
    format!(
        "<section class=\"block\"><h2>Timeline</h2><div class=\"card timeline\"><details class=\"fold\" id=\"timeline\"><summary>Show all {} events</summary><ol>{lines}</ol></details></div></section>",
        entries.len()
    )
}

/// What the runs index shows for a run, rewritten after every event.
pub fn run_summary(run: &Run, label: &str) -> serde_json::Value {
    let state = run.state();
    let mut t = Tally::default();
    for r in state.rows.values() {
        t.add(r);
    }
    let entries = run.entries_after(0);
    serde_json::json!({
        "label": label,
        "started": entries.first().map(|e| e.at),
        "updated": entries.last().map(|e| e.at),
        "finished": state.finished,
        "verdict": verdict(state),
        "counts": { "total": t.total, "pass": t.pass, "fail": t.fail, "blocked": t.blocked },
    })
}

/// The list of runs: each opens its live page (the current run) or its export.
pub fn index(runs: &[(String, serde_json::Value, Option<String>)], current: &str) -> String {
    let mut rows = String::new();
    for (name, s, link) in runs {
        let count = |k: &str| s["counts"][k].as_u64().unwrap_or(0) as u32;
        let t = Tally {
            total: count("total"),
            pass: count("pass"),
            fail: count("fail"),
            blocked: count("blocked"),
            ..Tally::default()
        };
        let status = s["verdict"]["status"].as_str().unwrap_or("not-ready");
        let (class, word) = match status {
            "failing" => ("failing", "Failing"),
            "ready" => ("", "Ready"),
            _ => ("", "Not ready"),
        };
        let reasons = s["verdict"]["reasons"]
            .as_array()
            .map(|r| {
                r.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(" · ")
            })
            .unwrap_or_default();
        let href = link.as_deref().map(esc);
        let when = s["started"].as_u64().map(date_time).unwrap_or_default();
        let live = if name == current && !s["finished"].as_bool().unwrap_or(false) {
            " <span class=\"chip ticket\">Live</span>"
        } else {
            ""
        };
        let inner = format!(
            "<span class=\"run-ring\">{}</span><span class=\"run-text\"><span class=\"run-title\"><b>{}</b>{live}</span><span class=\"muted\">{when}</span></span><span class=\"run-verdict\"><strong class=\"{class}\">{word}</strong><span class=\"muted\">{}</span></span>",
            ring(&t),
            esc(s["label"].as_str().unwrap_or(name)),
            esc(&reasons)
        );
        match href {
            Some(href) => {
                let _ = write!(rows, "<a class=\"run-row\" href=\"{href}\">{inner}</a>");
            }
            None => {
                let _ = write!(rows, "<div class=\"run-row\">{inner}</div>");
            }
        }
    }
    if rows.is_empty() {
        rows = "<p class=\"empty\">No runs yet.</p>".into();
    }
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1,viewport-fit=cover\">{APP_HEAD}<title>Runs · soundcheck</title><style>{CSS}</style></head><body><div id=\"live\" data-live=\"false\"><main><header class=\"head\"><div class=\"head-text\"><span class=\"eyebrow\">Soundcheck · local regression</span><h1>Runs</h1></div></header><section class=\"card runs\">{rows}</section></main></div><script>{JS}</script></body></html>\n"
    )
}
