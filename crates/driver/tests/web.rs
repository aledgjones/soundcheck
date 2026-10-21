//! Drives a real headless Chrome against a small local page. Skips (with a note) where Chrome isn't installed.

use soundcheck_driver::web::Web;
use soundcheck_driver::{Device, Driver, Matcher};

const PAGE: &str = r#"<!doctype html><html><head><title>Fixture</title></head><body>
<main>
  <h1>Library</h1>
  <button onclick="document.getElementById('status').textContent='Saved'">Save</button>
  <p id="status">Not saved</p>
  <label><input type="checkbox" checked> Send diagnostic info</label>
  <label for="t">Title</label> <input id="t" value="Old title">
  <button disabled>Delete</button>
  <button aria-expanded="false" onclick="this.setAttribute('aria-expanded','true')">Menu</button>
  <button onmouseover="this.textContent='Hovered'">Hover me</button>
  <button onclick="throw new Error('boom')">Break</button>
  <button onclick="window.tapCount=(window.tapCount||0)+1;document.getElementById('taps').textContent='Taps '+window.tapCount">Tap</button>
  <p id="taps">Taps 0</p>
  <p id="net">online</p>
  <p id="width"></p>
  <p id="key">No key</p>
</main>
<script>
  addEventListener('keydown', (e) => { document.getElementById('key').textContent = 'Key ' + e.key; });
  setInterval(() => {
    document.getElementById('net').textContent = navigator.onLine ? 'online' : 'offline';
    document.getElementById('width').textContent = 'Width ' + innerWidth;
  }, 50);
</script>
</body></html>"#;

fn m(target: &str) -> Matcher {
    Matcher::parse(target).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_web_driver_reads_and_drives_a_real_page() {
    let dir = std::env::temp_dir().join(format!("soundcheck-web-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("fixture.html");
    std::fs::write(&file, PAGE).unwrap();
    let url = format!("file://{}", file.display());

    let web = match Web::launch(&url, 1024, 768).await {
        Ok(web) => web,
        Err(e) => {
            eprintln!("skipping: Chrome didn't start ({e})");
            return;
        }
    };
    let driver = Driver::new(web);

    // The tree, as a screen reader gets it, with states.
    let seen = driver.see(None, false).await.unwrap();
    let find = |label: &str| seen.iter().find(|e| e.label == label).cloned();
    assert!(find("Library").is_some(), "{seen:#?}");
    let checkbox = find("Send diagnostic info").expect("checkbox by its label");
    assert_eq!(checkbox.checked, Some(true));
    assert!(!find("Delete").unwrap().enabled);
    assert_eq!(find("Menu").unwrap().expanded, Some(false));
    assert_eq!(
        find("Title").map(|e| e.value),
        Some(Some("Old title".into()))
    );

    // Input.
    driver.tap(&m("Save"), false).await.unwrap();
    driver.wait(&m("Saved"), 3000).await.unwrap();
    driver.tap(&m("Menu"), false).await.unwrap();
    let menu = driver.expect(&m("Menu")).await.unwrap();
    assert_eq!(menu.expanded, Some(true));
    driver.fill(&m("Title"), "Opener").await.unwrap();
    assert_eq!(
        driver.expect(&m("Title")).await.unwrap().value.as_deref(),
        Some("Opener")
    );

    let hover = driver.expect(&m("Hover me")).await.unwrap();
    let (x, y) = hover.tap_point();
    driver.device.hover_at(x, y).await.unwrap();
    driver.wait(&m("Hovered"), 3000).await.unwrap();

    driver.tap(&m("Break"), false).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let errors = driver.device.errors().await.unwrap();
    assert!(errors.iter().any(|e| e.contains("boom")), "{errors:?}");

    driver.tap_burst(&m("Tap"), 5, 50).await.unwrap();
    driver.wait(&m("Taps 5"), 3000).await.unwrap();

    driver.device.press("ArrowRight").await.unwrap();
    driver.wait(&m("Key ArrowRight"), 3000).await.unwrap();

    driver.device.viewport(500, 700).await.unwrap();
    driver.wait(&m("Width 500"), 3000).await.unwrap();

    // Offline for a second, then back.
    let drop = driver.network_drop(1);
    let watch = async {
        driver.wait(&m("offline"), 3000).await.unwrap();
    };
    let (dropped, ()) = tokio::join!(drop, watch);
    dropped.unwrap();
    driver.wait(&m("online"), 3000).await.unwrap();

    let shot = dir.join("shot.png");
    driver.device.screenshot(&shot).await.unwrap();
    assert!(std::fs::metadata(&shot).unwrap().len() > 1000);

    driver.device.close().await;
}
