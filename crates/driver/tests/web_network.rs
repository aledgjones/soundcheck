//! A real headless Chrome reaching a local page through the network proxy, as a suite's web build does. Skips (with a
//! note) where Chrome isn't installed.

use soundcheck_driver::proxy::{self, Network, Route, Upstream};
use soundcheck_driver::web::Web;
use soundcheck_driver::{Device, Driver};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

/// Fetches `/ping` over and over with no deadline, as an upload goes, and counts the answers.
const PAGE: &str = r#"<!doctype html><html><head><title>Fixture</title></head><body>
<p id="count">Fetched 0</p>
<script>
  let n = 0;
  async function loop() {
    try {
      const r = await fetch('/ping?' + Date.now());
      await r.text();
      n++;
      document.getElementById('count').textContent = 'Fetched ' + n;
    } catch (e) {}
    setTimeout(loop, 100);
  }
  loop();
</script>
</body></html>"#;

/// Serves the page at `/` and a short answer anywhere else, on kept-alive connections.
async fn site(port: u16) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    tokio::spawn(async move {
        while let Ok((c, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (r, mut w) = c.into_split();
                let mut r = tokio::io::BufReader::new(r);
                loop {
                    let mut first = String::new();
                    if r.read_line(&mut first).await.unwrap_or(0) == 0 {
                        return;
                    }

                    let mut length = 0usize;
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }

                        let line = line.trim_end().to_ascii_lowercase();
                        if line.is_empty() {
                            break;
                        }

                        if let Some(n) = line.strip_prefix("content-length:") {
                            length = n.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; length];
                    if r.read_exact(&mut body).await.is_err() {
                        return;
                    }

                    let path = first.split_whitespace().nth(1).unwrap_or("/");
                    let (kind, text) = if path == "/" {
                        ("text/html", PAGE)
                    } else {
                        ("text/plain", "pong")
                    };
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\ncache-control: no-store\r\ncontent-length: {}\r\n\r\n{text}",
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

async fn start(listen: u16, upstream: u16) -> Network {
    site(upstream).await;
    let routes = [Route {
        name: "app".into(),
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
            return Network { socket };
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the proxy didn't start");
}

/// The page's answer count, read from the page: 0 while it hasn't loaded.
async fn fetched(driver: &Driver<Web>) -> u64 {
    let seen = driver.see(None, false).await.unwrap_or_default();
    seen.iter()
        .find_map(|e| e.label.strip_prefix("Fetched ")?.parse().ok())
        .unwrap_or(0)
}

/// Whether the page gets answers again within `within` of now.
async fn recovers(driver: &Driver<Web>, within: Duration) -> bool {
    let from = fetched(driver).await;
    let until = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < until {
        if fetched(driver).await > from + 2 {
            return true;
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_through_the_proxy_gets_its_network_back_after_a_drop() {
    let network = start(19201, 19211).await;
    let web = match Web::launch("http://127.0.0.1:19201/", 800, 600, false).await {
        Ok(web) => web,
        Err(e) => {
            eprintln!("skipping: Chrome didn't start ({e})");
            proxy::command(&network.socket, "stop").await.unwrap();
            return;
        }
    };
    let driver = Driver::new(web).with_network(Some(network.clone()));
    assert!(
        recovers(&driver, Duration::from_secs(5)).await,
        "never fetched"
    );

    driver.network_drop(2).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let during = fetched(&driver).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(fetched(&driver).await, during, "fetched while dropped");

    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(
        recovers(&driver, Duration::from_secs(10)).await,
        "no answers within 10 s of the drop ending"
    );

    driver.device.close().await;
    proxy::command(&network.socket, "stop").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_reloaded_mid_drop_gets_its_network_back() {
    let network = start(19202, 19212).await;
    let web = match Web::launch("http://127.0.0.1:19202/", 800, 600, false).await {
        Ok(web) => web,
        Err(e) => {
            eprintln!("skipping: Chrome didn't start ({e})");
            proxy::command(&network.socket, "stop").await.unwrap();
            return;
        }
    };
    let driver = Driver::new(web).with_network(Some(network.clone()));
    assert!(
        recovers(&driver, Duration::from_secs(5)).await,
        "never fetched"
    );

    driver.network_drop(3).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The reload hangs while dropped, as it would with no network, so it isn't awaited.
    let reload = tokio::time::timeout(
        Duration::from_secs(1),
        driver.device.goto("http://127.0.0.1:19202/"),
    );
    let _ = reload.await;

    tokio::time::sleep(Duration::from_secs(3)).await;
    let reloaded = tokio::time::timeout(
        Duration::from_secs(5),
        driver.device.goto("http://127.0.0.1:19202/"),
    )
    .await;
    assert!(
        matches!(reloaded, Ok(Ok(()))),
        "reloading after the drop didn't load within 5 s: {reloaded:?}"
    );
    assert!(
        recovers(&driver, Duration::from_secs(10)).await,
        "no answers within 10 s of reloading after the drop"
    );

    driver.device.close().await;
    proxy::command(&network.socket, "stop").await.unwrap();
}
