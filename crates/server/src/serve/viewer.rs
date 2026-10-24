//! `soundcheck view`: a run's pages on this Mac's network, so a phone on the same Wi-Fi can follow it live. It passes
//! page requests through to the run's server, which stays local-only, and never its MCP endpoint: anyone who could
//! reach that could drive the devices. Opening it needs the key in the link it prints; a cookie keeps it after that.

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;

const COOKIE: &str = "soundcheck_view";

struct Viewer {
    key: String,
    server: String,
    client: reqwest::Client,
}

/// Serves the pages of the run on `server_port` at this Mac's network address on `port`, until stopped.
pub async fn view(server_port: u16, port: u16) -> Result<()> {
    let key = new_key()?;
    let address = lan_address().context("no network address: is this Mac on Wi-Fi?")?;
    let viewer = Arc::new(Viewer {
        key: key.clone(),
        server: format!("http://127.0.0.1:{server_port}"),
        client: reqwest::Client::new(),
    });
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("port {port} is taken"))?;
    println!("phone viewer at http://{address}:{port}/?k={key}");
    println!("read-only: pages and live updates only; it never reaches the run's MCP tools");
    let app = axum::Router::new().fallback(pass).with_state(viewer);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn pass(State(viewer): State<Arc<Viewer>>, request: Request) -> Response {
    let path = request.uri().path().to_string();
    if request.method() != Method::GET || path.starts_with("/mcp") {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    }
    let query = request.uri().query().unwrap_or_default();
    let in_link = query.split('&').any(|p| p == format!("k={}", viewer.key));
    let in_cookie = request
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .any(|c| c.trim() == format!("{COOKIE}={}", viewer.key));
    if !in_link && !in_cookie {
        return (
            StatusCode::FORBIDDEN,
            "open the link soundcheck view printed, with its key\n",
        )
            .into_response();
    }
    let rest: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty() && !p.starts_with("k="))
        .collect();
    let url = if rest.is_empty() {
        format!("{}{path}", viewer.server)
    } else {
        format!("{}{path}?{}", viewer.server, rest.join("&"))
    };
    let Ok(mut upstream) = viewer.client.get(&url).send().await else {
        return (
            StatusCode::BAD_GATEWAY,
            "no soundcheck run is serving on this Mac right now\n",
        )
            .into_response();
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = Response::builder().status(status);
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_DISPOSITION,
        header::CACHE_CONTROL,
    ] {
        if let Some(value) = upstream
            .headers()
            .get(name.as_str())
            .and_then(|v| HeaderValue::from_bytes(v.as_bytes()).ok())
        {
            response = response.header(name, value);
        }
    }
    if in_link {
        response = response.header(
            header::SET_COOKIE,
            format!("{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict", viewer.key),
        );
    }
    // Streamed, so the page's live event feed reaches the phone as it happens.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        while let Ok(Some(chunk)) = upstream.chunk().await {
            if tx.send(Ok(chunk)).await.is_err() {
                break;
            }
        }
    });
    response
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// A key nobody can guess, from the system's random source.
fn new_key() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 12];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// This Mac's address on its network: the one a route out would use. Connecting a UDP socket sends nothing.
fn lan_address() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}
