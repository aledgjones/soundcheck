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
    /// The run this server is running, which the live page shows instead.
    pub current: Option<String>,
}

/// `/runs/{name}/`, its evidence, and its export if it has one.
pub fn router(reports: Reports) -> axum::Router {
    axum::Router::new()
        .route("/runs/{name}", get(to_folder))
        .route("/runs/{name}/", get(run_page))
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
        let href = if entry.path().join(saved::INFO_FILE).exists() {
            Some(format!("runs/{name}/"))
        } else if entry.path().join(EXPORT_FILE).exists() {
            // Runs from before soundcheck kept their suite can only show their export.
            Some(format!("runs/{name}/report.html"))
        } else {
            None
        };
        runs.push((name, summary, href));
    }
    runs.sort_by(|a, b| b.0.cmp(&a.0));
    runs
}

fn folder(reports: &Reports, name: &str) -> Option<PathBuf> {
    report::safe_name(name).then(|| reports.dir.join(name))
}

async fn to_folder(Path(name): Path<String>) -> Redirect {
    Redirect::permanent(&format!("{name}/"))
}

async fn run_page(State(reports): State<Arc<Reports>>, Path(name): Path<String>) -> Response {
    if reports.current.as_deref() == Some(name.as_str()) {
        return Redirect::to("/").into_response();
    }
    let Some(dir) = folder(&reports, &name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let rendered = tokio::task::spawn_blocking(move || {
        let (suite, run, label) = saved::load(&dir, now_ms())?;
        anyhow::Ok(report::document(&report::Page {
            suite: &suite,
            run: &run,
            label: &label,
            live: false,
            run_dir: Some(&dir),
            export: None,
        }))
    })
    .await;
    match rendered {
        Ok(Ok(html)) => Html(html).into_response(),
        Ok(Err(e)) => (StatusCode::NOT_FOUND, format!("{e:#}")).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
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
        let (suite, run, label) = saved::load(&dir, now_ms())?;
        let finished = run.state().finished;
        let html = report::document(&report::Page {
            suite: &suite,
            run: &run,
            label: &label,
            live: false,
            run_dir: Some(&dir),
            export: Some(report::Export {
                at: now_ms(),
                finished,
            }),
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
