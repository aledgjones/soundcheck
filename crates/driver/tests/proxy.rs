//! The network proxy on this machine alone: an echo server behind it, a client in front.

use soundcheck_driver::proxy::{self, Route, Upstream};
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
        upstream: Upstream::Port(upstream),
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

/// An HTTP service that answers each request on a kept-alive connection with 200 and `<path> <body bytes>`.
async fn http(port: u16) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    tokio::spawn(async move {
        while let Ok((c, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (r, mut w) = c.into_split();
                let mut r = tokio::io::BufReader::new(r);
                while let Some((path, body)) = read_message(&mut r).await {
                    if path == "/stream" {
                        let _ = w
                            .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n")
                            .await;
                        std::future::pending::<()>().await;
                    }

                    let text = format!("{path} {}", body.len());
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{text}",
                        text.len()
                    );
                    if w.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
}

/// One request or response: its first line's second word (the path or status) and its body, length or chunked.
async fn read_message<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> Option<(String, Vec<u8>)> {
    use tokio::io::AsyncBufReadExt;
    let mut first = String::new();
    if r.read_line(&mut first).await.ok()? == 0 {
        return None;
    }

    let word = first.split_whitespace().nth(1)?.to_string();
    let (mut length, mut chunked) = (0usize, false);
    loop {
        let mut line = String::new();
        r.read_line(&mut line).await.ok()?;
        let line = line.trim_end().to_ascii_lowercase();
        if line.is_empty() {
            break;
        }

        if let Some(n) = line.strip_prefix("content-length:") {
            length = n.trim().parse().ok()?;
        }

        if line == "transfer-encoding: chunked" {
            chunked = true;
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            r.read_line(&mut size).await.ok()?;
            let size = usize::from_str_radix(size.trim(), 16).ok()?;
            let mut chunk = vec![0u8; size + 2];
            if size == 0 {
                r.read_line(&mut String::new()).await.ok()?;
                break;
            }

            r.read_exact(&mut chunk).await.ok()?;
            body.extend_from_slice(&chunk[..size]);
        }
    } else {
        body.resize(length, 0);
        r.read_exact(&mut body).await.ok()?;
    }
    Some((word, body))
}

async fn start_http(listen: u16, upstream: u16) -> std::path::PathBuf {
    http(upstream).await;
    let routes = [Route {
        name: "api".into(),
        listen,
        upstream: Upstream::Port(upstream),
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

/// Sends one request on a kept-alive connection and returns the status and body of its response.
async fn request(
    conn: &mut tokio::io::BufReader<TcpStream>,
    method: &str,
    path: &str,
    body: &str,
    chunked: bool,
) -> Option<(String, String)> {
    let framing = if chunked {
        format!(
            "transfer-encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
            body.len()
        )
    } else {
        format!("content-length: {}\r\n\r\n{body}", body.len())
    };
    let req = format!("{method} {path} HTTP/1.1\r\nhost: localhost\r\n{framing}");
    conn.get_mut().write_all(req.as_bytes()).await.ok()?;
    let reply = tokio::time::timeout(Duration::from_secs(1), read_message(conn))
        .await
        .ok()??;
    Some((reply.0, String::from_utf8_lossy(&reply.1).into_owned()))
}

async fn connect(port: u16) -> tokio::io::BufReader<TcpStream> {
    tokio::io::BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap())
}

#[tokio::test]
async fn a_fault_answers_its_path_and_lets_the_rest_through() {
    let socket = start_http(19104, 19004).await;
    let mut kept = connect(19104).await;
    assert_eq!(
        request(&mut kept, "GET", "/health", "", false)
            .await
            .unwrap(),
        ("200".into(), "/health 0".into())
    );

    assert_eq!(
        proxy::command(&socket, "fault /files/*/upload-url 403")
            .await
            .unwrap(),
        "online; /files/*/upload-url 403"
    );
    assert_eq!(
        proxy::command(&socket, "fault /write 503").await.unwrap(),
        "online; /files/*/upload-url 403, /write 503"
    );
    let (status, body) = request(&mut kept, "POST", "/write?x=1", "{\"op\":1}", false)
        .await
        .unwrap();
    assert_eq!(
        status, "503",
        "a kept-alive connection's next request wasn't faulted"
    );
    assert!(body.contains("soundcheck fault"));
    assert!(
        request(&mut kept, "GET", "/health", "", false)
            .await
            .is_none(),
        "the connection stayed open after a fault"
    );

    let mut fresh = connect(19104).await;
    assert_eq!(
        request(&mut fresh, "GET", "/files/p1/download-url", "", false)
            .await
            .unwrap()
            .0,
        "200"
    );
    assert_eq!(
        request(
            &mut fresh,
            "PUT",
            "/files/p1/upload-url",
            "chunked body",
            true
        )
        .await
        .unwrap()
        .0,
        "403",
        "a chunked body wasn't read before the answer"
    );
    let mut deeper = connect(19104).await;
    assert_eq!(
        request(&mut deeper, "POST", "/files/p1/x/upload-url", "", false)
            .await
            .unwrap()
            .0,
        "200",
        "* matched more than one segment"
    );

    assert_eq!(
        proxy::command(&socket, "fault clear").await.unwrap(),
        "online"
    );
    let mut cleared = connect(19104).await;
    assert_eq!(
        request(&mut cleared, "POST", "/write", "{}", false)
            .await
            .unwrap()
            .0,
        "200"
    );
    assert_eq!(
        request(&mut cleared, "PUT", "/files/p1/upload-url", "abc", true)
            .await
            .unwrap(),
        ("200".into(), "/files/p1/upload-url 3".into()),
        "a chunked body didn't pass through whole"
    );
    proxy::command(&socket, "stop").await.unwrap();
}

#[tokio::test]
async fn reset_brings_the_proxy_online_with_no_faults() {
    let socket = start_http(19105, 19005).await;
    proxy::command(&socket, "fault /write 503").await.unwrap();
    proxy::command(&socket, "offline").await.unwrap();
    assert_eq!(proxy::command(&socket, "reset").await.unwrap(), "online");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut conn = connect(19105).await;
    assert_eq!(
        request(&mut conn, "POST", "/write", "", false)
            .await
            .unwrap()
            .0,
        "200"
    );

    for bad in [
        "fault /write 200",
        "fault /write soon",
        "fault write 503",
        "fault /write",
    ] {
        assert!(
            proxy::command(&socket, bad)
                .await
                .unwrap()
                .starts_with("error:"),
            "{bad} was accepted"
        );
    }
    assert!(
        proxy::command(&socket, "started")
            .await
            .unwrap()
            .parse::<u64>()
            .is_ok()
    );
    proxy::command(&socket, "stop").await.unwrap();
}

#[tokio::test]
async fn a_new_fault_ends_an_open_stream_to_its_path() {
    let socket = start_http(19106, 19006).await;
    let mut stream = connect(19106).await;
    stream
        .get_mut()
        .write_all(b"POST /stream HTTP/1.1\r\nhost: localhost\r\ncontent-length: 0\r\n\r\n")
        .await
        .unwrap();
    let mut seen = vec![0u8; 256];
    let n = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut seen))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&seen[..n]).contains("hello"));
    let mut other = connect(19106).await;
    assert_eq!(
        request(&mut other, "GET", "/health", "", false)
            .await
            .unwrap()
            .0,
        "200"
    );

    proxy::command(&socket, "fault /stream 503").await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut seen)).await;
    assert!(
        matches!(closed, Ok(Ok(0) | Err(_))),
        "the open stream outlived its fault"
    );
    assert_eq!(
        request(&mut other, "GET", "/health", "", false)
            .await
            .unwrap()
            .0,
        "200",
        "a connection to another path was dropped"
    );
    proxy::command(&socket, "stop").await.unwrap();
}
