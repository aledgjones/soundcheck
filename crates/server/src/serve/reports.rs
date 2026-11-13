//! Every run in a reports folder, shown from its own folder: the journal replayed against the suite snapshot it
//! kept. Nothing needs exporting to be looked at; an export is only for sharing.

use crate::report;
use crate::run::saved;
use crate::serve::{EXPORT_FILE, SUMMARY_FILE, now_ms};
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct Reports {
    pub dir: PathBuf,
    /// The page key (3.9) of the run this server is running, whose live page is served instead.
    pub current: Option<String>,
}

/// `/{key}/` for each run (its id, or its folder's name before ids), its evidence, and its export if it has one.
/// `/runs/{name}/` is the old address, kept so earlier links still open.
pub fn router(reports: Reports) -> axum::Router {
    axum::Router::new()
        .route("/{name}", get(to_folder))
        .route("/{name}/", get(run_page).delete(delete_run))
        .route("/{name}/files/evidence/{file}", get(evidence))
        .route("/{name}/report.html", get(exported))
        .route("/{name}/download", get(download))
        .route("/runs/{name}", get(to_page))
        .route("/runs/{name}/", get(to_page))
        .route("/runs/{name}/files/evidence/{file}", get(evidence))
        .route("/runs/{name}/report.html", get(exported))
        .route("/runs/{name}/download", get(download))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icon-180.png", get(|| png(report::ICON_180)))
        .route("/icon-512.png", get(|| png(report::ICON_512)))
        .with_state(Arc::new(reports))
}

async fn manifest() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        report::MANIFEST,
    )
}

async fn png(bytes: &'static [u8]) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/png")], bytes)
}

/// The runs in the folder, newest first, as (name, summary, link to its page if it has one).
pub fn list(reports: &Reports) -> Vec<(String, serde_json::Value, Option<String>)> {
    let mut runs = Vec::new();
    for entry in std::fs::read_dir(&reports.dir)
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(text) = std::fs::read_to_string(entry.path().join(SUMMARY_FILE)) else {
            continue;
        };
        let Ok(summary) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let key = saved::page_key(&entry.path());
        // A start that failed before its first entry tested nothing; only the run starting now is listed without one.
        if summary["started"].is_null() && reports.current.as_deref() != Some(key.as_str()) {
            continue;
        }

        let href = if entry.path().join(saved::INFO_FILE).exists() {
            Some(format!("{key}/"))
        } else if entry.path().join(EXPORT_FILE).exists() {
            // Runs from before soundcheck kept their suite can only show their export.
            Some(format!("{key}/report.html"))
        } else {
            None
        };
        runs.push((name, summary, href));
    }
    newest_first(&mut runs);
    runs
}

/// Newest first by start time, since a run's folder is named by its id; a run with no entries yet is the newest.
pub fn newest_first(runs: &mut [(String, serde_json::Value, Option<String>)]) {
    runs.sort_by(|a, b| {
        let started = |s: &serde_json::Value| s["started"].as_u64().unwrap_or(u64::MAX);
        started(&b.1)
            .cmp(&started(&a.1))
            .then_with(|| b.0.cmp(&a.0))
    });
}

/// The run folder a page key names: a run's id, or a folder's name.
fn folder(reports: &Reports, name: &str) -> Option<PathBuf> {
    if !report::safe_name(name) {
        return None;
    }

    let by_name = reports.dir.join(name);
    if by_name.join(saved::INFO_FILE).is_file() || by_name.join(EXPORT_FILE).is_file() {
        return Some(by_name);
    }

    saved::find_run(&reports.dir, std::path::Path::new(name)).ok()
}

async fn to_folder(Path(name): Path<String>) -> Redirect {
    Redirect::permanent(&format!("{name}/"))
}

/// An old `/runs/{name}/` link: to the run's page under its key.
async fn to_page(State(reports): State<Arc<Reports>>, Path(name): Path<String>) -> Response {
    match folder(&reports, &name) {
        Some(dir) => Redirect::permanent(&format!("/{}/", saved::page_key(&dir))).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn run_page(State(reports): State<Arc<Reports>>, Path(name): Path<String>) -> Response {
    let Some(dir) = folder(&reports, &name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let rendered = tokio::task::spawn_blocking(move || {
        let (suite, run, info) = saved::load(&dir, now_ms())?;
        anyhow::Ok(report::document(&report::Page {
            suite: &suite,
            run: &run,
            label: &info.label,
            uncommitted: info.uncommitted,
            live: false,
            run_dir: Some(&dir),
            export: None,
            starting: None,
        }))
    })
    .await;
    match rendered {
        Ok(Ok(html)) => Html(html).into_response(),
        Ok(Err(e)) => (StatusCode::NOT_FOUND, format!("{e:#}")).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Deletes a past run's whole folder, screenshots included (SA-984). The run this server is running stays.
async fn delete_run(State(reports): State<Arc<Reports>>, Path(name): Path<String>) -> Response {
    let Some(dir) = folder(&reports, &name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let key = saved::page_key(&dir);
    // Only a run's full key, and only a folder directly in the reports folder: never an id prefix's best guess.
    let exact = key == name || dir.file_name().is_some_and(|n| n == name.as_str());

    if !exact || dir.parent() != Some(reports.dir.as_path()) {
        return StatusCode::NOT_FOUND.into_response();
    }

    if reports.current.as_deref() == Some(key.as_str()) {
        return (StatusCode::CONFLICT, "this run is live on the server\n").into_response();
    }

    match tokio::fs::remove_dir_all(&dir).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

async fn evidence(
    State(reports): State<Arc<Reports>>,
    Path((name, file)): Path<(String, String)>,
) -> Response {
    let (Some(dir), true) = (folder(&reports, &name), report::safe_name(&file)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = dir.join(report::EVIDENCE_DIR).join(&file);
    let kind = match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("json" | "jsonl") => "application/json",
        _ => "text/plain; charset=utf-8",
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, kind)], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The export as a file to save, named after its run folder.
pub fn attachment(run_dir: &std::path::Path, html: String) -> Response {
    let name = run_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "run".into());
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"soundcheck-{name}.html\""),
            ),
        ],
        html,
    )
        .into_response()
}

async fn download(State(reports): State<Arc<Reports>>, Path(name): Path<String>) -> Response {
    let Some(dir) = folder(&reports, &name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let rendered = tokio::task::spawn_blocking(move || {
        let (suite, run, info) = saved::load(&dir, now_ms())?;
        let finished = run.state().finished;
        let html = report::document(&report::Page {
            suite: &suite,
            run: &run,
            label: &info.label,
            uncommitted: info.uncommitted,
            live: false,
            run_dir: Some(&dir),
            export: Some(report::Export {
                at: now_ms(),
                finished,
            }),
            starting: None,
        });
        anyhow::Ok((dir, html))
    })
    .await;
    match rendered {
        Ok(Ok((dir, html))) => attachment(&dir, html),
        Ok(Err(e)) => (StatusCode::NOT_FOUND, format!("{e:#}")).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn exported(State(reports): State<Arc<Reports>>, Path(name): Path<String>) -> Response {
    let Some(dir) = folder(&reports, &name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read_to_string(dir.join(EXPORT_FILE)).await {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_list_by_start_time_with_a_run_not_yet_started_first() {
        let run = |name: &str, started: Option<u64>| {
            (
                name.to_string(),
                serde_json::json!({ "started": started }),
                None,
            )
        };
        let mut runs = vec![
            run("ffff-older", Some(100)),
            run("0000-newer", Some(200)),
            run("aaaa-just-opened", None),
        ];
        newest_first(&mut runs);
        let names: Vec<&str> = runs.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(names, ["aaaa-just-opened", "0000-newer", "ffff-older"]);
    }

    #[test]
    fn leaves_out_runs_that_never_started_except_the_one_starting_now() {
        let dir = std::env::temp_dir().join(format!("soundcheck-reports-{}", now_ms()));
        for (name, started) in [
            ("tested", Some(100)),
            ("failed-start", None),
            ("starting", None),
        ] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(
                dir.join(name).join(SUMMARY_FILE),
                serde_json::json!({ "started": started }).to_string(),
            )
            .unwrap();
        }

        let runs = list(&Reports {
            dir: dir.clone(),
            current: Some("starting".into()),
        });
        let _ = std::fs::remove_dir_all(&dir);

        let names: Vec<&str> = runs.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(names, ["starting", "tested"]);
    }

    #[tokio::test]
    async fn deletes_a_past_run_by_its_full_key_but_never_the_live_one() {
        let dir = std::env::temp_dir().join(format!("soundcheck-delete-{}", now_ms()));
        for (folder, id) in [("20261101-past", "aaaa1111"), ("20261102-live", "bbbb2222")] {
            std::fs::create_dir_all(dir.join(folder).join(report::EVIDENCE_DIR)).unwrap();
            std::fs::write(
                dir.join(folder).join(saved::INFO_FILE),
                serde_json::json!({ "id": id, "suite": "/s", "label": "x", "platforms": [], "scope": [] })
                    .to_string(),
            )
            .unwrap();
        }
        let reports = Arc::new(Reports {
            dir: dir.clone(),
            current: Some("bbbb2222".into()),
        });
        let delete = |name: &str| delete_run(State(reports.clone()), Path(name.to_string()));

        let prefix = delete("aaaa").await.status();
        let live = delete("bbbb2222").await.status();
        let past = delete("aaaa1111").await.status();
        let past_gone = !dir.join("20261101-past").exists();
        let live_kept = dir.join("20261102-live").exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(prefix, StatusCode::NOT_FOUND);
        assert_eq!(live, StatusCode::CONFLICT);
        assert_eq!(past, StatusCode::NO_CONTENT);
        assert!(past_gone && live_kept);
    }
}
