//! The one process a run needs: MCP tools for testers and the coordinator, and the event stream, on one local port.

pub mod events;
pub mod prompt;
pub mod tools;

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
    run: Mutex<Run>,
    events: broadcast::Sender<Entry>,
    /// Every request carries it, so nothing else on the machine can drive the run.
    pub token: String,
    pub stall_ms: u64,
}

impl Hub {
    pub fn new(suite: Suite, run: Run, token: String, stall_ms: u64) -> Arc<Hub> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Hub {
            suite,
            run: Mutex::new(run),
            events,
            token,
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
    let app = axum::Router::new()
        .nest_service("/mcp", mcp)
        .route("/events", get(events_stream))
        .layer(middleware::from_fn_with_state(hub.clone(), require_token))
        .with_state(hub.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;

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

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

async fn require_token(
    State(hub): State<Arc<Hub>>,
    Query(query): Query<TokenQuery>,
    request: Request,
    next: Next,
) -> Response {
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if bearer == Some(hub.token.as_str()) || query.token.as_deref() == Some(hub.token.as_str()) {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, "missing or wrong run token\n").into_response()
    }
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
