//! TCP proxies between the app and the services it reaches, so a run can take the network away the way a phone loses
//! it, or have a service refuse one kind of request. The project lists the routes; this module knows ports, hosts and
//! HTTP paths only.

use crate::DriverError;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::watch;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto};

/// The app reaches `listen`; `upstream` really serves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub name: String,
    pub listen: u16,
    pub upstream: Upstream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upstream {
    /// A service on this machine.
    Port(u16),
    /// A service elsewhere, such as a store's API. The app still talks plain HTTP to `listen`; the proxy opens TLS
    /// when `tls` is set and gives every request this host, as a client calling it directly would.
    Remote { tls: bool, host: String, port: u16 },
}

impl Upstream {
    /// `https://api.example.com`, `http://host:8080` and the like: a scheme and a host, an optional port, no path.
    pub fn parse(url: &str) -> Result<Upstream, String> {
        let wrong =
            |why: &str| format!("{url:?} {why}; use a port or a URL like https://api.example.com");
        let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(wrong("isn't http:// or https://"));
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.contains(['/', '?', '#', '@', '[', ']']) {
            return Err(wrong("has more than a host and port"));
        }

        let (host, port) = match rest.rsplit_once(':') {
            Some((host, port)) => match port.parse::<u16>() {
                Ok(port) if port > 0 => (host, port),
                _ => return Err(wrong("has a bad port")),
            },
            None => (rest, if tls { 443 } else { 80 }),
        };
        let label =
            |l: &str| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !host.split('.').all(label) {
            return Err(wrong("has a bad host"));
        }

        Ok(Upstream::Remote {
            tls,
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    /// The `Host` a request to a remote service carries; the port only when it isn't the scheme's own.
    fn host_header(&self) -> Option<String> {
        match self {
            Upstream::Port(_) => None,
            Upstream::Remote { tls, host, port } => Some(if *port == if *tls { 443 } else { 80 } {
                host.clone()
            } else {
                format!("{host}:{port}")
            }),
        }
    }

    /// Opens a connection to the service, TLS included.
    async fn connect(&self, tls: &TlsConnector) -> std::io::Result<(Upstreamed, Downstreamed)> {
        let (host, port) = match self {
            Upstream::Port(port) => ("127.0.0.1", *port),
            Upstream::Remote { host, port, .. } => (host.as_str(), *port),
        };
        let tcp = TcpStream::connect((host, port)).await?;
        if !matches!(self, Upstream::Remote { tls: true, .. }) {
            let (read, write) = tcp.into_split();
            return Ok((Box::new(write), Box::new(read)));
        }

        let name = ServerName::try_from(host.to_string()).map_err(std::io::Error::other)?;
        let (read, write) = tokio::io::split(tls.connect(name, tcp).await?);
        Ok((Box::new(write), Box::new(read)))
    }
}

/// Toward the service, and back from it.
type Upstreamed = Box<dyn AsyncWrite + Unpin + Send>;
type Downstreamed = Box<dyn AsyncRead + Unpin + Send>;

/// Certificates checked against Mozilla's roots, plus any a test trusts for its own service.
fn tls_connector(extra: &[Vec<u8>]) -> std::io::Result<TlsConnector> {
    let mut roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for der in extra {
        roots
            .add(CertificateDer::from(der.clone()))
            .map_err(std::io::Error::other)?;
    }
    let config = ClientConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(std::io::Error::other)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Online,
    /// Nothing passes and nothing closes, as when a phone loses the network in the background. Connections open
    /// during a drop stay frozen; new ones get through after it.
    Dropped,
    /// Connections are refused and open ones closed, as in airplane mode.
    Offline,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Online => "online",
            Mode::Dropped => "dropped",
            Mode::Offline => "offline",
        }
    }
}

/// HTTP requests to `path` get `status` from the proxy itself, and the service never sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fault {
    path: String,
    status: u16,
}

impl Fault {
    /// Segment by segment, with `*` for any one segment; the query string doesn't count.
    fn matches(&self, target: &str) -> bool {
        let path = target.split(['?', '#']).next().unwrap_or("");
        let want = self.path.trim_end_matches('/').split('/');
        let have = path.trim_end_matches('/').split('/');

        want.clone().count() == have.clone().count()
            && want.zip(have).all(|(w, h)| w == "*" || w == h)
    }
}

/// Connections watch it, so a new fault ends the requests it matches that are still open.
type Faults = Arc<watch::Sender<Vec<Fault>>>;

/// Where the proxy for these routes takes commands. Under /tmp, not the temp dir: a sandboxed shell has its own.
pub fn socket_path(routes: &[Route]) -> PathBuf {
    let port = routes.iter().map(|r| r.listen).min().unwrap_or(0);
    PathBuf::from(format!("/tmp/soundcheck-proxy-{port}.sock"))
}

/// Runs the proxies and their command socket until a `stop` command.
pub async fn serve(routes: &[Route], socket: &Path) -> std::io::Result<()> {
    serve_trusting(routes, socket, &[]).await
}

/// As `serve`, also trusting these certificates (DER) from remote services, as a test's own one needs.
pub async fn serve_trusting(
    routes: &[Route],
    socket: &Path,
    certificates: &[Vec<u8>],
) -> std::io::Result<()> {
    let tls = tls_connector(certificates)?;
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (mode, _) = watch::channel(Mode::Online);
    let mode = Arc::new(mode);
    let faults: Faults = Arc::new(watch::channel(Vec::new()).0);
    for route in routes {
        // Retried, since a proxy being replaced may not have let go of its ports yet.
        let listener = bind(route.listen).await.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("port {} is taken", route.listen),
            )
        })?;
        tokio::spawn(accept_loop(
            route.clone(),
            listener,
            mode.clone(),
            faults.clone(),
            tls.clone(),
        ));
    }
    let _ = std::fs::remove_file(socket);
    let control = UnixListener::bind(socket)?;
    let generation = Arc::new(AtomicU64::new(0));
    loop {
        let (stream, _) = control.accept().await?;
        if command_line(stream, &mode, &faults, &generation, started).await {
            let _ = std::fs::remove_file(socket);
            return Ok(());
        }
    }
}

/// Answers one command with the state it leaves; true when told to stop.
async fn command_line(
    stream: UnixStream,
    mode: &Arc<watch::Sender<Mode>>,
    faults: &Faults,
    generation: &Arc<AtomicU64>,
    started: u64,
) -> bool {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    if BufReader::new(read).read_line(&mut line).await.is_err() {
        return false;
    }

    let words: Vec<&str> = line.split_whitespace().collect();
    let reply = match words.as_slice() {
        ["status"] => Ok(()),
        ["stop"] => {
            let _ = write.write_all(b"stopped\n").await;
            return true;
        }
        ["started"] => {
            let _ = write.write_all(format!("{started}\n").as_bytes()).await;
            return false;
        }
        ["online"] => {
            generation.fetch_add(1, Ordering::SeqCst);
            mode.send_replace(Mode::Online);
            Ok(())
        }
        ["offline"] => {
            generation.fetch_add(1, Ordering::SeqCst);
            mode.send_replace(Mode::Offline);
            Ok(())
        }
        ["reset"] => {
            generation.fetch_add(1, Ordering::SeqCst);
            mode.send_replace(Mode::Online);
            faults.send_replace(Vec::new());
            Ok(())
        }
        ["drop", seconds] => match seconds.parse::<u64>() {
            Ok(seconds) => {
                // Back on a timer, so a cancelled call never leaves the app offline; a later command wins.
                let mine = generation.fetch_add(1, Ordering::SeqCst) + 1;
                mode.send_replace(Mode::Dropped);
                let (mode, generation) = (mode.clone(), generation.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                    if generation.load(Ordering::SeqCst) == mine {
                        mode.send_replace(Mode::Online);
                    }
                });
                Ok(())
            }
            Err(_) => Err(format!("drop needs whole seconds, not {seconds:?}")),
        },
        ["fault", "clear"] => {
            faults.send_replace(Vec::new());
            Ok(())
        }
        ["fault", path, status] => match status.parse::<u16>() {
            Ok(status) if (400..=599).contains(&status) && path.starts_with('/') => {
                faults.send_modify(|faults| {
                    faults.retain(|f| f.path != *path);
                    faults.push(Fault {
                        path: path.to_string(),
                        status,
                    });
                });
                Ok(())
            }
            Ok(_) if !path.starts_with('/') => {
                Err(format!("fault needs a path starting with /, not {path:?}"))
            }
            _ => Err(format!(
                "fault needs a status from 400 to 599, not {status:?}"
            )),
        },
        _ => Err(format!("unknown command {:?}", line.trim())),
    };
    let text = match reply {
        Ok(()) => describe(*mode.borrow(), &faults.borrow()),
        Err(e) => format!("error: {e}"),
    };
    let _ = write.write_all(format!("{text}\n").as_bytes()).await;
    false
}

/// The mode, then each fault: `online; /write 503, /files/*/upload-url 403`.
fn describe(mode: Mode, faults: &[Fault]) -> String {
    if faults.is_empty() {
        return mode.name().to_string();
    }

    let faults: Vec<String> = faults
        .iter()
        .map(|f| format!("{} {}", f.path, f.status))
        .collect();
    format!("{}; {}", mode.name(), faults.join(", "))
}

/// Accepts while online or dropped; offline, it stops listening, so connections are refused.
async fn accept_loop(
    route: Route,
    first: TcpListener,
    mode: Arc<watch::Sender<Mode>>,
    faults: Faults,
    tls: TlsConnector,
) {
    let upstream = Arc::new(route.upstream);
    let mut listener = Some(first);
    let mut rx = mode.subscribe();
    loop {
        let Some(open) = &listener else {
            if rx.wait_for(|m| *m != Mode::Offline).await.is_err() {
                return;
            }
            listener = bind(route.listen).await;
            continue;
        };
        let offline = tokio::select! {
            accepted = open.accept() => {
                if let Ok((client, _)) = accepted {
                    tokio::spawn(connection(client, upstream.clone(), tls.clone(), mode.subscribe(), faults.clone()));
                }
                false
            }
            changed = rx.wait_for(|m| *m == Mode::Offline) => {
                if changed.is_err() {
                    return;
                }
                true
            }
        };
        if offline {
            listener = None;
        }
    }
}

async fn bind(port: u16) -> Option<TcpListener> {
    for _ in 0..50 {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)).await {
            return Some(l);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

async fn connection(
    client: TcpStream,
    upstream: Arc<Upstream>,
    tls: TlsConnector,
    mut rx: watch::Receiver<Mode>,
    faults: Faults,
) {
    let frozen = Arc::new(AtomicBool::new(*rx.borrow() == Mode::Dropped));
    let Ok((server_write, server_read)) = upstream.connect(&tls).await else {
        return;
    };
    let host = upstream.host_header();
    let (client_read, mut client_write) = client.into_split();
    let target = Arc::new(Mutex::new(String::new()));
    let mut faulted = faults.subscribe();
    let pipes = async {
        let answer = {
            let mut responses = Box::pin(pipe(server_read, &mut client_write, frozen.clone()));
            let requests = requests(
                BufReader::new(client_read),
                server_write,
                frozen.clone(),
                faults,
                target.clone(),
                host.as_deref(),
            );
            tokio::select! {
                ended = requests => match ended {
                    Ended::Closed => {
                        responses.await;
                        None
                    }
                    Ended::Fault(answer) => Some(answer),
                },
                () = &mut responses => None,
            }
        };
        // Clients don't pipeline, so nothing from the service is on its way when a faulted request is answered.
        if let Some(answer) = answer {
            let _ = client_write.write_all(&answer).await;
            let _ = client_write.shutdown().await;
        }
    };
    let watch = async {
        loop {
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        break;
                    }

                    match *rx.borrow() {
                        Mode::Dropped => frozen.store(true, Ordering::SeqCst),
                        Mode::Offline => return,
                        Mode::Online => {}
                    }
                }
                changed = faulted.changed() => {
                    if changed.is_err() {
                        break;
                    }

                    // A failing service drops what it was in the middle of, such as a long-lived stream.
                    let target = target.lock().unwrap().clone();
                    if faulted.borrow().iter().any(|f| f.matches(&target)) {
                        return;
                    }
                }
            }
        }
        std::future::pending::<()>().await;
    };
    // Going offline, or a fault on the request in flight, drops both halves, which closes the sockets.
    tokio::select! {
        () = pipes => {}
        () = watch => {}
    }
}

enum Ended {
    Closed,
    /// The response the proxy gives instead of the service; the connection ends after it.
    Fault(Vec<u8>),
}

/// How a request's body is framed, so the next request on a kept-alive connection is found.
enum Body {
    None,
    Length(u64),
    Chunked,
}

struct Head {
    bytes: Vec<u8>,
    target: String,
    body: Body,
    /// After an `Upgrade` (PowerSync's websocket) the connection is no longer HTTP.
    upgrade: bool,
    expects_continue: bool,
}

enum Read {
    Request(Head),
    /// Not HTTP: these bytes go through as they are, and so does the rest.
    Other(Vec<u8>),
    /// The client closed after these bytes.
    Eof(Vec<u8>),
}

const HEAD_MAX: usize = 64 * 1024;

/// Passes the client's requests to the service one at a time, answering any whose path has a fault itself.
async fn requests(
    mut from: BufReader<OwnedReadHalf>,
    mut to: Upstreamed,
    frozen: Arc<AtomicBool>,
    faults: Faults,
    target: Arc<Mutex<String>>,
    host: Option<&str>,
) -> Ended {
    loop {
        let mut head = match read_head(&mut from).await {
            Read::Request(head) => head,
            Read::Other(bytes) => {
                if send(&mut to, &bytes, &frozen).await.is_ok() {
                    pipe(from, to, frozen).await;
                }
                return Ended::Closed;
            }
            Read::Eof(bytes) => {
                if send(&mut to, &bytes, &frozen).await.is_ok() {
                    finish(to, &frozen).await;
                }
                return Ended::Closed;
            }
        };

        target.lock().unwrap().clone_from(&head.target);
        let status = faults
            .borrow()
            .iter()
            .find(|f| f.matches(&head.target))
            .map(|f| f.status);
        if let Some(status) = status.filter(|_| !frozen.load(Ordering::SeqCst)) {
            // Read to the end first: closing with a body unread resets the connection, and the client loses the
            // answer. A client waiting on 100 Continue sends no body once it has a final status.
            if !head.expects_continue {
                let _ = body(&mut from, None, &head.body, &frozen).await;
            }

            return Ended::Fault(answer(status));
        }

        if let Some(host) = host {
            head.bytes = with_host(&head.bytes, host);
        }

        if send(&mut to, &head.bytes, &frozen).await.is_err() {
            return Ended::Closed;
        }

        if head.upgrade {
            pipe(from, to, frozen).await;
            return Ended::Closed;
        }

        if body(&mut from, Some(&mut to), &head.body, &frozen)
            .await
            .is_err()
        {
            return Ended::Closed;
        }
    }
}

/// Reads up to the blank line ending a request's head, or as far as shows it isn't HTTP.
async fn read_head(from: &mut BufReader<OwnedReadHalf>) -> Read {
    let mut bytes = Vec::new();
    // A method is capital letters then a space; anything else is some other protocol.
    loop {
        let Ok(buf) = from.fill_buf().await else {
            return Read::Eof(bytes);
        };
        if buf.is_empty() {
            return Read::Eof(bytes);
        }

        let token = buf.iter().take_while(|b| b.is_ascii_uppercase()).count();
        bytes.extend_from_slice(&buf[..token]);
        let next = buf.get(token).copied();
        from.consume(token);
        match next {
            Some(b' ') if !bytes.is_empty() && bytes.len() <= 16 => break,
            Some(_) => return Read::Other(bytes),
            None if bytes.len() > 16 => return Read::Other(bytes),
            None => {}
        }
    }

    let start = bytes.len();
    loop {
        let before = bytes.len();
        match from.read_until(b'\n', &mut bytes).await {
            Ok(0) | Err(_) => return Read::Eof(bytes),
            Ok(_) => {}
        }

        let line = &bytes[before..];
        if line == b"\r\n" || line == b"\n" {
            break;
        }

        if bytes.len() > HEAD_MAX {
            return Read::Other(bytes);
        }
    }

    let text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    let mut lines = text.lines();
    let target = lines
        .next()
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or("")
        .to_string();
    let mut head = Head {
        bytes,
        target,
        body: Body::None,
        upgrade: false,
        expects_continue: false,
    };
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_ascii_lowercase();
        match name.trim().to_ascii_lowercase().as_str() {
            "transfer-encoding" if value.contains("chunked") => head.body = Body::Chunked,
            "content-length" if !matches!(head.body, Body::Chunked) => {
                head.body = value.parse().map_or(Body::None, Body::Length);
            }
            "upgrade" => head.upgrade = true,
            "expect" if value == "100-continue" => head.expects_continue = true,
            _ => {}
        }
    }
    Read::Request(head)
}

/// The head with `host` as its only `Host`, which a remote service picks its site by; the app named `listen`.
fn with_host(head: &[u8], host: &str) -> Vec<u8> {
    let mut lines = head.split_inclusive(|&b| b == b'\n');
    let mut out = Vec::with_capacity(head.len() + host.len());
    out.extend_from_slice(lines.next().unwrap_or_default());
    out.extend_from_slice(format!("Host: {host}\r\n").as_bytes());
    for line in lines {
        if !line
            .get(..5)
            .is_some_and(|l| l.eq_ignore_ascii_case(b"host:"))
        {
            out.extend_from_slice(line);
        }
    }
    out
}

/// Passes a request's body on, or reads it to the end when there's nowhere to send it.
async fn body(
    from: &mut BufReader<OwnedReadHalf>,
    mut to: Option<&mut Upstreamed>,
    framing: &Body,
    frozen: &AtomicBool,
) -> std::io::Result<()> {
    match framing {
        Body::None => Ok(()),
        Body::Length(n) => copy_exact(from, to, *n, frozen).await,
        Body::Chunked => loop {
            let size = line(from, to.as_deref_mut(), frozen).await?;
            let size = size.split(';').next().unwrap_or("").trim();
            let size = u64::from_str_radix(size, 16)
                .map_err(|_| std::io::Error::other("a bad chunk size"))?;
            if size == 0 {
                // Trailers, then the blank line that ends the body.
                while !line(from, to.as_deref_mut(), frozen).await?.is_empty() {}
                return Ok(());
            }

            copy_exact(from, to.as_deref_mut(), size, frozen).await?;
            line(from, to.as_deref_mut(), frozen).await?;
        },
    }
}

/// One CRLF-ended line of a chunked body, passed on; returns it without the line ending.
async fn line(
    from: &mut BufReader<OwnedReadHalf>,
    to: Option<&mut Upstreamed>,
    frozen: &AtomicBool,
) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    if from.read_until(b'\n', &mut bytes).await? == 0 {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }

    if let Some(to) = to {
        send(to, &bytes, frozen).await?;
    }

    Ok(String::from_utf8_lossy(&bytes).trim_end().to_string())
}

async fn copy_exact(
    from: &mut BufReader<OwnedReadHalf>,
    mut to: Option<&mut Upstreamed>,
    mut left: u64,
    frozen: &AtomicBool,
) -> std::io::Result<()> {
    while left > 0 {
        let buf = from.fill_buf().await?;
        if buf.is_empty() {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }

        let n = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        if let Some(to) = to.as_deref_mut() {
            send(to, &buf[..n], frozen).await?;
        }

        from.consume(n);
        left -= n as u64;
    }
    Ok(())
}

/// Writes unless the connection is frozen, when nothing passes.
async fn send(to: &mut Upstreamed, bytes: &[u8], frozen: &AtomicBool) -> std::io::Result<()> {
    if frozen.load(Ordering::SeqCst) {
        return Ok(());
    }

    to.write_all(bytes).await
}

/// The client closed: tell the service, unless the connection is frozen, which never closes.
async fn finish(mut to: Upstreamed, frozen: &AtomicBool) {
    if frozen.load(Ordering::SeqCst) {
        std::future::pending::<()>().await;
    }

    let _ = to.shutdown().await;
}

/// A response shaped like the service's own errors (Fastify's), so the app treats it as one.
fn answer(status: u16) -> Vec<u8> {
    let reason = match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    let body =
        format!(r#"{{"statusCode":{status},"error":"{reason}","message":"soundcheck fault"}}"#);
    format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Copies one direction of a connection until it closes, passing nothing while frozen.
async fn pipe(
    mut from: impl AsyncRead + Unpin,
    mut to: impl AsyncWrite + Unpin,
    frozen: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match from.read(&mut buf).await {
            Ok(0) | Err(_) => {
                // A frozen connection never closes, so the far side doesn't learn either.
                if frozen.load(Ordering::SeqCst) {
                    std::future::pending::<()>().await;
                }
                let _ = to.shutdown().await;
                return;
            }
            Ok(n) => {
                if !frozen.load(Ordering::SeqCst) && to.write_all(&buf[..n]).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Sends one command to a running proxy and returns its reply.
pub async fn command(socket: &Path, command: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket).await?;
    stream.write_all(format!("{command}\n").as_bytes()).await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    Ok(line.trim().to_string())
}

/// A driver's handle on the network proxy its app goes through.
#[derive(Debug, Clone)]
pub struct Network {
    pub socket: PathBuf,
}

impl Network {
    async fn send(&self, line: &str) -> Result<String, DriverError> {
        let reply = command(&self.socket, line).await.map_err(|e| {
            DriverError::Device(format!(
                "the network proxy isn't running ({}): {e}",
                self.socket.display()
            ))
        })?;
        match reply.strip_prefix("error: ") {
            Some(e) => Err(DriverError::Invalid(e.into())),
            None => Ok(reply),
        }
    }

    pub async fn offline(&self, on: bool) -> Result<(), DriverError> {
        self.send(if on { "offline" } else { "online" })
            .await
            .map(|_| ())
    }

    pub async fn drop_for(&self, seconds: u64) -> Result<(), DriverError> {
        self.send(&format!("drop {seconds}")).await.map(|_| ())
    }

    /// Requests to `path` (`*` for any one segment) get `status` until cleared; returns every fault now set.
    pub async fn fault(&self, path: &str, status: u16) -> Result<String, DriverError> {
        self.send(&format!("fault {path} {status}")).await
    }

    pub async fn fault_clear(&self) -> Result<(), DriverError> {
        self.send("fault clear").await.map(|_| ())
    }

    /// Online with no faults, whatever the last spec left.
    pub async fn reset(&self) -> Result<(), DriverError> {
        self.send("reset").await.map(|_| ())
    }
}
