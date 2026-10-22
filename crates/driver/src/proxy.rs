//! TCP proxies between the app and the services it reaches on this machine, so a run can take the network away the
//! way a phone loses it. The project lists the routes; this module knows ports only.

use crate::DriverError;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::watch;

/// The app reaches `listen`; `upstream` really serves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub name: String,
    pub listen: u16,
    pub upstream: u16,
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

/// Where the proxy for these routes takes commands. Under /tmp, not the temp dir: a sandboxed shell has its own.
pub fn socket_path(routes: &[Route]) -> PathBuf {
    let port = routes.iter().map(|r| r.listen).min().unwrap_or(0);
    PathBuf::from(format!("/tmp/soundcheck-proxy-{port}.sock"))
}

/// Runs the proxies and their command socket until a `stop` command.
pub async fn serve(routes: &[Route], socket: &Path) -> std::io::Result<()> {
    let (mode, _) = watch::channel(Mode::Online);
    let mode = Arc::new(mode);
    for route in routes {
        let listener = TcpListener::bind(("127.0.0.1", route.listen)).await?;
        tokio::spawn(accept_loop(route.clone(), listener, mode.clone()));
    }
    let _ = std::fs::remove_file(socket);
    let control = UnixListener::bind(socket)?;
    let generation = Arc::new(AtomicU64::new(0));
    loop {
        let (stream, _) = control.accept().await?;
        if command_line(stream, &mode, &generation).await {
            let _ = std::fs::remove_file(socket);
            return Ok(());
        }
    }
}

/// Answers one command with the mode it leaves; true when told to stop.
async fn command_line(
    stream: UnixStream,
    mode: &Arc<watch::Sender<Mode>>,
    generation: &Arc<AtomicU64>,
) -> bool {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    if BufReader::new(read).read_line(&mut line).await.is_err() {
        return false;
    }
    let mut words = line.split_whitespace();
    let reply = match (words.next(), words.next()) {
        (Some("status"), None) => Ok(()),
        (Some("stop"), None) => {
            let _ = write.write_all(b"stopped\n").await;
            return true;
        }
        (Some("online"), None) => {
            generation.fetch_add(1, Ordering::SeqCst);
            mode.send_replace(Mode::Online);
            Ok(())
        }
        (Some("offline"), None) => {
            generation.fetch_add(1, Ordering::SeqCst);
            mode.send_replace(Mode::Offline);
            Ok(())
        }
        (Some("drop"), Some(seconds)) => match seconds.parse::<u64>() {
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
        _ => Err(format!("unknown command {:?}", line.trim())),
    };
    let text = match reply {
        Ok(()) => mode.borrow().name().to_string(),
        Err(e) => format!("error: {e}"),
    };
    let _ = write.write_all(format!("{text}\n").as_bytes()).await;
    false
}

/// Accepts while online or dropped; offline, it stops listening, so connections are refused.
async fn accept_loop(route: Route, first: TcpListener, mode: Arc<watch::Sender<Mode>>) {
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
                    tokio::spawn(connection(client, route.upstream, mode.subscribe()));
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

async fn connection(client: TcpStream, upstream: u16, mut rx: watch::Receiver<Mode>) {
    let frozen = Arc::new(AtomicBool::new(*rx.borrow() == Mode::Dropped));
    let Ok(server) = TcpStream::connect(("127.0.0.1", upstream)).await else {
        return;
    };
    let (client_read, client_write) = client.into_split();
    let (server_read, server_write) = server.into_split();
    let pipes = async {
        tokio::join!(
            pipe(client_read, server_write, frozen.clone()),
            pipe(server_read, client_write, frozen.clone()),
        )
    };
    let watch = async {
        while rx.changed().await.is_ok() {
            match *rx.borrow() {
                Mode::Dropped => frozen.store(true, Ordering::SeqCst),
                Mode::Offline => return,
                Mode::Online => {}
            }
        }
        std::future::pending::<()>().await;
    };
    // Going offline drops both halves, which closes the sockets.
    tokio::select! {
        _ = pipes => {}
        () = watch => {}
    }
}

/// Copies one direction of a connection until it closes, passing nothing while frozen.
async fn pipe(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
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
}
