//! The one process a run needs: MCP tools for testers and the coordinator, and the event stream, on one local port.

pub mod events;
pub mod prompt;
pub mod tools;

use crate::report;
use crate::run::Run;
use crate::run::journal::Entry;
use crate::suite::Suite;
use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub struct Hub {
    pub suite: Suite,
    /// The release under test, shown as the page's title.
    pub label: String,
    run: Mutex<Run>,
    events: broadcast::Sender<Entry>,
    pub stall_ms: u64,
}

impl Hub {
    pub fn new(suite: Suite, run: Run, label: String, stall_ms: u64) -> Arc<Hub> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Hub {
            suite,
            label,
            run: Mutex::new(run),
            events,
            stall_ms,
        })
    }

    /// Runs a command against the run, then pushes whatever it journalled to the event stream.
    pub fn with_run<T>(&self, f: impl FnOnce(&mut Run, u64) -> Result<T>) -> Result<T> {
        let mut run = self.run.lock().unwrap_or_else(|e| e.into_inner());
        let before = run.last_seq();
        let result = f(&mut run, now_ms());
        for entry in run.entries_after(before) {
            let _ = self.events.send(entry.clone());
        }
        result
    }

    fn stall_silent_testers(&self) {
        let silence = self.stall_ms;
        let _ = self.with_run(|run, now| {
            for token in run.stalled(now, silence) {
                run.stall(
                    now,
                    &token,
                    &format!("no tool call for {} s", silence / 1000),
                )?;
            }
            Ok(())
        });
    }
}

pub struct Server {
    pub addr: SocketAddr,
    pub shutdown: CancellationToken,
    pub handle: tokio::task::JoinHandle<()>,
}

/// Binds to 127.0.0.1 only; port 0 picks a free one.
pub async fn start(hub: Arc<Hub>, port: u16) -> Result<Server> {
    let shutdown = CancellationToken::new();
    let mcp: StreamableHttpService<tools::Tools, LocalSessionManager> = StreamableHttpService::new(
        {
            let hub = hub.clone();
            move || Ok(tools::Tools::new(hub.clone()))
        },
        Default::default(),
        StreamableHttpServerConfig::default().with_cancellation_token(shutdown.child_token()),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    let app = axum::Router::new()
        .nest_service("/mcp", mcp)
        .route("/events", get(events_stream))
        .route("/", get(page))
        .route("/fragment", get(fragment))
        .layer(middleware::from_fn_with_state(addr.port(), local_only))
        .with_state(hub.clone());

    let ticker = {
        let hub = hub.clone();
        let shutdown = shutdown.clone();
        async move {
            let mut every = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = every.tick() => hub.stall_silent_testers(),
                }
            }
        }
    };
    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            tokio::spawn(ticker);
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
                .await;
        }
    });
    Ok(Server {
        addr,
        shutdown,
        handle,
    })
}

/// Only this machine's own callers: agents and the CLI send no Origin, and the live page sends this server's own.
/// Checking Host as well stops a page whose domain was rebound to 127.0.0.1.
async fn local_only(State(port): State<u16>, request: Request, next: Next) -> Response {
    if is_local(port, request.headers()) {
        next.run(request).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "soundcheck only answers this machine's own callers\n",
        )
            .into_response()
    }
}

fn is_local(port: u16, headers: &axum::http::HeaderMap) -> bool {
    let ours = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let host_ok = header(header::HOST).is_some_and(|h| ours.iter().any(|o| o == h));
    let origin_ok = header(header::ORIGIN)
        .is_none_or(|origin| ours.iter().any(|o| origin == format!("http://{o}")));
    host_ok && origin_ok
}

async fn page(State(hub): State<Arc<Hub>>) -> Response {
    let html = hub
        .with_run(|run, _| {
            Ok(report::document(&report::Page {
                suite: &hub.suite,
                run,
                label: &hub.label,
                live: true,
            }))
        })
        .unwrap_or_default();
    axum::response::Html(html).into_response()
}

async fn fragment(State(hub): State<Arc<Hub>>) -> Response {
    let html = hub
        .with_run(|run, _| {
            Ok(report::content(&report::Page {
                suite: &hub.suite,
                run,
                label: &hub.label,
                live: true,
            }))
        })
        .unwrap_or_default();
    axum::response::Html(html).into_response()
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    after: u64,
}

/// Newline-delimited journal entries after `after`, then each new one as it's written. Replaying from a cursor
/// means a coordinator whose Monitor lapsed misses nothing.
async fn events_stream(State(hub): State<Arc<Hub>>, Query(q): Query<EventsQuery>) -> Response {
    // Subscribe before reading history, so nothing written in between is lost.
    let mut live = hub.events.subscribe();
    let history: Vec<Entry> = hub
        .with_run(|run, _| Ok(run.entries_after(q.after).to_vec()))
        .unwrap_or_default();
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    tokio::spawn(async move {
        let mut last = q.after;
        let line = |e: &Entry| {
            Bytes::from(format!(
                "{}\n",
                serde_json::to_string(e).unwrap_or_default()
            ))
        };
        for entry in &history {
            last = entry.seq;
            if tx.send(Ok(line(entry))).await.is_err() {
                return;
            }
        }
        loop {
            match live.recv().await {
                Ok(entry) if entry.seq > last => {
                    last = entry.seq;
                    if tx.send(Ok(line(&entry))).await.is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let missed: Vec<Entry> = hub
                        .with_run(|run, _| Ok(run.entries_after(last).to_vec()))
                        .unwrap_or_default();
                    for entry in &missed {
                        last = entry.seq;
                        if tx.send(Ok(line(entry))).await.is_err() {
                            return;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}
