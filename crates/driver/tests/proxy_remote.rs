//! The network proxy fronting a remote HTTPS service, played by a local one whose certificate only these tests trust.

use soundcheck_driver::proxy::{self, Route, Upstream};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{ServerConfig, crypto};

const CA: &[u8] = include_bytes!("fixtures/tls/ca.der");
const CERT: &[u8] = include_bytes!("fixtures/tls/localhost.der");
const KEY: &[u8] = include_bytes!("fixtures/tls/localhost.key.der");

/// An HTTPS service on `localhost` that answers each request on a kept-alive connection with `<path> <host>`.
async fn https(port: u16) {
    let config = ServerConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(CERT.to_vec())],
            PrivateKeyDer::try_from(KEY.to_vec()).unwrap(),
        )
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    tokio::spawn(async move {
        while let Ok((c, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(c).await else {
                    return;
                };
                let (r, mut w) = tokio::io::split(tls);
                let mut r = BufReader::new(r);
                while let Some((path, host)) = read_request(&mut r).await {
                    let text = format!("{path} {host}");
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

/// A request's path and every `Host` it carried, its body read and dropped.
async fn read_request<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> Option<(String, String)> {
    let mut first = String::new();
    if r.read_line(&mut first).await.ok()? == 0 {
        return None;
    }

    let path = first.split_whitespace().nth(1)?.to_string();
    let (mut hosts, mut length) = (Vec::new(), 0usize);
    loop {
        let mut line = String::new();
        r.read_line(&mut line).await.ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }

        let (name, value) = line.split_once(':')?;
        match name.to_ascii_lowercase().as_str() {
            "host" => hosts.push(value.trim().to_string()),
            "content-length" => length = value.trim().parse().ok()?,
            _ => {}
        }
    }
    r.read_exact(&mut vec![0u8; length]).await.ok()?;
    Some((path, hosts.join(",")))
}

async fn start(listen: u16, upstream: u16, trust: bool) -> std::path::PathBuf {
    https(upstream).await;
    let routes = [Route {
        name: "store".into(),
        listen,
        upstream: Upstream::parse(&format!("https://localhost:{upstream}")).unwrap(),
    }];
    let socket = proxy::socket_path(&routes);
    let trusted = if trust { vec![CA.to_vec()] } else { Vec::new() };
    tokio::spawn({
        let socket = socket.clone();
        async move { proxy::serve_trusting(&routes, &socket, &trusted).await }
    });
    for _ in 0..50 {
        if proxy::command(&socket, "status").await.is_ok() {
            return socket;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the proxy didn't start");
}

async fn connect(port: u16) -> BufReader<TcpStream> {
    BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap())
}

/// One plain-HTTP request on a kept-alive connection, as the app sends it: its status and body, or none within a second.
async fn get(conn: &mut BufReader<TcpStream>, path: &str, listen: u16) -> Option<(String, String)> {
    let req = format!("GET {path} HTTP/1.1\r\nhost: localhost:{listen}\r\n\r\n");
    conn.get_mut().write_all(req.as_bytes()).await.ok()?;
    tokio::time::timeout(Duration::from_secs(1), async {
        let mut first = String::new();
        if conn.read_line(&mut first).await.ok()? == 0 {
            return None;
        }

        let status = first.split_whitespace().nth(1)?.to_string();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            conn.read_line(&mut line).await.ok()?;
            let line = line.trim_end().to_ascii_lowercase();
            if line.is_empty() {
                break;
            }

            if let Some(n) = line.strip_prefix("content-length:") {
                length = n.trim().parse().ok()?;
            }
        }
        let mut body = vec![0u8; length];
        conn.read_exact(&mut body).await.ok()?;
        Some((status, String::from_utf8_lossy(&body).into_owned()))
    })
    .await
    .ok()?
}

#[test]
fn upstream_urls_parse() {
    let remote = |tls, host: &str, port| Upstream::Remote {
        tls,
        host: host.into(),
        port,
    };
    assert_eq!(
        Upstream::parse("https://api.revenuecat.com"),
        Ok(remote(true, "api.revenuecat.com", 443))
    );
    assert_eq!(
        Upstream::parse("https://API.example.com:8443/"),
        Ok(remote(true, "api.example.com", 8443))
    );
    assert_eq!(
        Upstream::parse("http://localhost:3000"),
        Ok(remote(false, "localhost", 3000))
    );
    for bad in [
        "api.example.com",
        "ftp://api.example.com",
        "https://",
        "https://api.example.com/v1",
        "https://api.example.com?x=1",
        "https://user@api.example.com",
        "https://api.example.com:0",
        "https://api.example.com:99999",
        "https://[::1]:443",
        "https://api..example.com",
    ] {
        assert!(Upstream::parse(bad).is_err(), "{bad} parsed");
    }
}

#[tokio::test]
async fn a_remote_service_gets_tls_and_its_own_host_on_every_request() {
    start(19121, 19221, true).await;
    let mut kept = connect(19121).await;
    for path in ["/v1/subscribers/a", "/v1/offerings"] {
        assert_eq!(
            get(&mut kept, path, 19121).await.unwrap(),
            ("200".into(), format!("{path} localhost:19221")),
            "the kept-alive connection's request didn't reach the service as its own host"
        );
    }
}

#[tokio::test]
async fn a_remote_service_goes_offline_drops_and_faults_like_a_local_one() {
    let socket = start(19122, 19222, true).await;
    let mut kept = connect(19122).await;
    assert_eq!(
        get(&mut kept, "/v1/offerings", 19122).await.unwrap().0,
        "200"
    );

    proxy::command(&socket, "fault /v1/receipts 503")
        .await
        .unwrap();
    let (status, body) = get(&mut kept, "/v1/receipts", 19122).await.unwrap();
    assert_eq!(status, "503");
    assert!(body.contains("soundcheck fault"));
    proxy::command(&socket, "fault clear").await.unwrap();

    let mut open = connect(19122).await;
    assert_eq!(
        get(&mut open, "/v1/offerings", 19122).await.unwrap().0,
        "200"
    );
    proxy::command(&socket, "drop 1").await.unwrap();
    assert!(
        get(&mut open, "/v1/offerings", 19122).await.is_none(),
        "a request went through a drop"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut after = connect(19122).await;
    assert_eq!(
        get(&mut after, "/v1/offerings", 19122).await.unwrap().0,
        "200"
    );

    proxy::command(&socket, "offline").await.unwrap();
    assert!(
        get(&mut after, "/v1/offerings", 19122).await.is_none(),
        "an open connection stayed up offline"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        TcpStream::connect(("127.0.0.1", 19122)).await.is_err(),
        "a new connection wasn't refused offline"
    );

    proxy::command(&socket, "online").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut back = connect(19122).await;
    assert_eq!(
        get(&mut back, "/v1/offerings", 19122).await.unwrap().0,
        "200"
    );
    proxy::command(&socket, "stop").await.unwrap();
}

#[tokio::test]
async fn a_certificate_nobody_vouches_for_is_refused() {
    let socket = start(19123, 19223, false).await;
    let mut conn = connect(19123).await;
    assert!(
        get(&mut conn, "/v1/offerings", 19123).await.is_none(),
        "the proxy talked to a service it can't verify"
    );
    proxy::command(&socket, "stop").await.unwrap();
}
