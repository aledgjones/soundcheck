//! The network proxy on this machine alone: an echo server behind it, a client in front.

use soundcheck_driver::proxy::{self, Route};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn echo(port: u16) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = c.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
}

/// "ping" back through the proxy, or what went wrong, within a second.
async fn ping(stream: &mut TcpStream) -> Result<String, String> {
    stream.write_all(b"ping").await.map_err(|e| e.to_string())?;
    let mut buf = [0u8; 4];
    match tokio::time::timeout(Duration::from_secs(1), stream.read_exact(&mut buf)).await {
        Ok(Ok(_)) => Ok(String::from_utf8_lossy(&buf).into_owned()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("timed out".into()),
    }
}

async fn start(listen: u16, upstream: u16) -> std::path::PathBuf {
    echo(upstream).await;
    let routes = [Route {
        name: "echo".into(),
        listen,
        upstream,
    }];
    let socket = proxy::socket_path(&routes);
    tokio::spawn({
        let socket = socket.clone();
        async move { proxy::serve(&routes, &socket).await }
    });
    for _ in 0..50 {
        if proxy::command(&socket, "status").await.is_ok() {
            return socket;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the proxy didn't start");
}

#[tokio::test]
async fn offline_refuses_and_closes_connections_until_back_online() {
    let socket = start(19101, 19001).await;
    let mut open = TcpStream::connect(("127.0.0.1", 19101)).await.unwrap();
    assert_eq!(ping(&mut open).await.unwrap(), "ping");

    assert_eq!(proxy::command(&socket, "offline").await.unwrap(), "offline");
    assert!(
        ping(&mut open).await.is_err(),
        "an open connection stayed up"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        TcpStream::connect(("127.0.0.1", 19101)).await.is_err(),
        "a new connection wasn't refused"
    );

    assert_eq!(proxy::command(&socket, "online").await.unwrap(), "online");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut again = TcpStream::connect(("127.0.0.1", 19101)).await.unwrap();
    assert_eq!(ping(&mut again).await.unwrap(), "ping");
    proxy::command(&socket, "stop").await.unwrap();
}

#[tokio::test]
async fn a_drop_freezes_then_lets_new_connections_through() {
    let socket = start(19102, 19002).await;
    let mut before = TcpStream::connect(("127.0.0.1", 19102)).await.unwrap();
    assert_eq!(ping(&mut before).await.unwrap(), "ping");

    assert_eq!(proxy::command(&socket, "drop 3").await.unwrap(), "dropped");
    assert_eq!(
        ping(&mut before).await.unwrap_err(),
        "timed out",
        "a frozen connection passes nothing and doesn't close"
    );
    let mut during = TcpStream::connect(("127.0.0.1", 19102)).await.unwrap();
    assert_eq!(ping(&mut during).await.unwrap_err(), "timed out");

    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(proxy::command(&socket, "status").await.unwrap(), "online");
    let mut after = TcpStream::connect(("127.0.0.1", 19102)).await.unwrap();
    assert_eq!(ping(&mut after).await.unwrap(), "ping");
    proxy::command(&socket, "stop").await.unwrap();
}

#[tokio::test]
async fn a_later_command_outlasts_a_drop_timer() {
    let socket = start(19103, 19003).await;
    proxy::command(&socket, "drop 1").await.unwrap();
    proxy::command(&socket, "offline").await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        proxy::command(&socket, "status").await.unwrap(),
        "offline",
        "the drop's timer brought an offline proxy back"
    );
    assert!(
        proxy::command(&socket, "drop soon")
            .await
            .unwrap()
            .starts_with("error:")
    );
    proxy::command(&socket, "stop").await.unwrap();
}
