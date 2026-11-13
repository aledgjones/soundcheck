//! Drives a real headless Chrome against a small local page. Skips (with a note) where Chrome isn't installed.

use soundcheck_driver::web::Chrome;
use soundcheck_driver::{Device, Driver, Matcher, Stop};
use std::time::Duration;

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
  <p id="width"></p>
  <p id="key">No key</p>
</main>
<script>
  addEventListener('keydown', (e) => { document.getElementById('key').textContent = 'Key ' + e.key; });
  setInterval(() => {
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

    let web = match Chrome::launch(&url, 1024, 768, false).await {
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

    driver.double_tap(&m("Tap"), false).await.unwrap();
    driver.wait(&m("Taps 2"), 3000).await.unwrap();

    driver.device.press("ArrowRight").await.unwrap();
    driver.wait(&m("Key ArrowRight"), 3000).await.unwrap();

    driver.device.viewport(500, 700).await.unwrap();
    driver.wait(&m("Width 500"), 3000).await.unwrap();

    let shot = dir.join("shot.png");
    driver.device.screenshot(&shot).await.unwrap();
    assert!(std::fs::metadata(&shot).unwrap().len() > 1000);

    driver.device.close().await;
}

/// A drawing page: dragging shows a red preview line on an overlay; releasing saves it, `offset` pixels lower.
fn drawing(offset: u32) -> String {
    format!(
        r#"<!doctype html><body style="margin:0;background:#fff">
<canvas id="page" width="600" height="400" style="position:absolute;left:0;top:0"></canvas>
<canvas id="preview" width="600" height="400" style="position:absolute;left:0;top:0"></canvas>
<script>
  const page = document.getElementById('page').getContext('2d');
  const preview = document.getElementById('preview').getContext('2d');
  let start = null, end = null;
  const line = (ctx, a, b, dy) => {{ ctx.strokeStyle = '#000'; ctx.lineWidth = 4; ctx.beginPath();
    ctx.moveTo(a[0], a[1] + dy); ctx.lineTo(b[0], b[1] + dy); ctx.stroke(); }};
  addEventListener('mousedown', (e) => {{ start = end = [e.clientX, e.clientY]; }});
  addEventListener('mousemove', (e) => {{ if (!start) return; end = [e.clientX, e.clientY];
    preview.clearRect(0, 0, 600, 400); line(preview, start, end, 0); }});
  addEventListener('mouseup', () => {{ preview.clearRect(0, 0, 600, 400); line(page, start, end, {offset}); start = null; }});
</script></body>"#
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preview_compare_catches_a_save_that_moves() {
    use soundcheck_driver::ops::CompareRequest;
    let dir = std::env::temp_dir().join(format!("soundcheck-compare-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (offset, should_pass) in [(0, true), (10, false)] {
        let file = dir.join(format!("draw-{offset}.html"));
        std::fs::write(&file, drawing(offset)).unwrap();
        let web = match Chrome::launch(&format!("file://{}", file.display()), 800, 600, false).await
        {
            Ok(web) => web,
            Err(e) => {
                eprintln!("skipping: Chrome didn't start ({e})");
                return;
            }
        };
        let driver = Driver::new(web);
        let request = CompareRequest {
            from: (100.0, 200.0),
            to: Some((300.0, 200.0)),
            commit: None,
            colour: None,
            region: None,
            tolerance: 2,
        };
        let result = driver
            .preview_compare(&request, &dir, &format!("line-{offset}"))
            .await
            .unwrap();
        assert_eq!(
            result.passed,
            should_pass,
            "offset {offset}: {}",
            result.summary()
        );
        if !should_pass {
            assert_eq!(
                result.offsets.map(|o| o[1]),
                Some(10),
                "{}",
                result.summary()
            );
        }
        driver.device.close().await;
    }
}

/// Opens a visible Chrome window for a moment: run by hand with `--ignored`.
#[tokio::test]
#[ignore]
async fn headed_chrome_runs_without_headless() {
    let web = Chrome::launch("about:blank", 800, 600, true).await.unwrap();
    let ps = std::process::Command::new("ps")
        .args(["-ax", "-o", "args"])
        .output()
        .unwrap();
    let ours: Vec<String> = String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|l| {
            l.contains("Chrome")
                && l.contains("--user-data-dir=")
                && l.contains("soundcheck-chrome-")
        })
        .filter(|l| !l.contains(" -c "))
        .map(str::to_string)
        .collect();
    assert!(!ours.is_empty(), "no Chrome with soundcheck's profile");
    assert!(
        ours.iter().all(|l| !l.contains("--headless")),
        "{} of our Chrome processes run headless",
        ours.iter().filter(|l| l.contains("--headless")).count()
    );
    drop(web);
}

/// Chrome's date and time inputs are segments that ignore inserted text: `fill` takes the field's own ISO value and
/// types each segment, and the app's save on blur gets the whole date, as Solo Scores' Playlist Settings does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fill_types_a_date_into_a_native_date_field() {
    let page = "data:text/html,<input type=date aria-label=\"Performance date\" value=2026-01-02 \
onblur=\"saved.textContent = 'Saved ' + this.value\"><input type=time aria-label=Start>\
<input type=datetime-local aria-label=Doors><input type=month aria-label=Season><input type=week aria-label=Rota>\
<button>Done</button><p id=saved>-</p>";
    let web = match Chrome::launch(page, 1200, 600, false).await {
        Ok(web) => web,
        Err(e) => {
            eprintln!("skipping: Chrome didn't start ({e})");
            return;
        }
    };
    let driver = Driver::new(web);

    driver
        .fill(&m("Performance date"), "2026-12-25")
        .await
        .unwrap();
    driver.tap(&m("Done"), false).await.unwrap();
    driver.wait(&m("Saved 2026-12-25"), 3000).await.unwrap();

    for (field, value) in [
        ("Start", "21:07"),
        ("Doors", "2027-11-30T09:59"),
        ("Season", "2027-10"),
        ("Rota", "2027-W51"),
    ] {
        driver.fill(&m(field), value).await.unwrap();
        assert_eq!(
            driver.expect(&m(field)).await.unwrap().value.as_deref(),
            Some(value)
        );
    }

    // Digits that aren't a whole value are typed into the segment that has the cursor, as real key presses.
    driver.tap(&m("Performance date"), false).await.unwrap();
    driver.device.press("ArrowLeft").await.unwrap();
    driver.device.press("ArrowLeft").await.unwrap();
    driver.device.press("ArrowLeft").await.unwrap();
    driver.device.type_text("07").await.unwrap();
    let date = driver.expect(&m("Performance date")).await.unwrap().value;
    assert!(
        date.as_deref().is_some_and(|d| d.contains("-07")),
        "{date:?}"
    );
    driver.device.close().await;
}

/// Google's sign-in opens a popup: the driver reads and taps there while it's open, then the app's page again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_popup_takes_reads_and_taps_until_it_closes() {
    let dir = std::env::temp_dir().join(format!("soundcheck-popup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("popup.html"),
        r#"<body><button onclick="opener.postMessage('chosen', '*'); window.close()">Choose account</button></body>"#,
    )
    .unwrap();
    let main = dir.join("main.html");
    std::fs::write(
        &main,
        r#"<body><button onclick="open('popup.html', 'signin', 'width=400,height=500')">Sign in</button>
<button onclick="this.textContent = document.title">Show title</button>
<script>addEventListener('message', (e) => { document.title = e.data; });</script></body>"#,
    )
    .unwrap();
    let web = match Chrome::launch(&format!("file://{}", main.display()), 800, 600, false).await {
        Ok(web) => web,
        Err(e) => {
            eprintln!("skipping: Chrome didn't start ({e})");
            return;
        }
    };
    let driver = Driver::new(web);
    driver.tap(&m("Sign in"), false).await.unwrap();
    let mut seen = Vec::new();
    for _ in 0..20 {
        seen = driver.see(None, false).await.unwrap();
        if seen.iter().any(|e| e.label == "Choose account") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        seen.iter().any(|e| e.label == "Choose account"),
        "{seen:#?}"
    );
    driver.tap(&m("Choose account"), false).await.unwrap();
    for _ in 0..30 {
        let seen = driver.see(None, false).await.unwrap();
        eprintln!(
            "after: {:?}",
            seen.iter().map(|e| e.label.clone()).collect::<Vec<_>>()
        );
        if seen.iter().any(|e| e.label == "Show title") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    driver.tap(&m("Show title"), false).await.unwrap();
    let seen = driver.see(None, false).await.unwrap();
    assert!(seen.iter().any(|e| e.label == "chosen"), "{seen:#?}");
    driver.device.close().await;
}

/// PostHog returns no feature flags to a "HeadlessChrome" user agent, so the page sees an ordinary Chrome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_reads_as_ordinary_chrome() {
    let page = "data:text/html,<button id=b></button><script>b.textContent = navigator.userAgent.includes('HeadlessChrome') ? 'headless' : 'chrome'</script>";
    let web = match Chrome::launch(page, 800, 600, false).await {
        Ok(web) => web,
        Err(e) => {
            eprintln!("skipping: Chrome didn't start ({e})");
            return;
        }
    };
    let driver = Driver::new(web);
    let seen = driver.see(None, false).await.unwrap();
    assert!(seen.iter().any(|e| e.label == "chrome"), "{seen:#?}");
    driver.device.close().await;
}

/// Sign Out asks with `window.confirm`: a dialog blocks the page, so the driver shows it and answers it instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_dialogs_are_seen_and_answered() {
    let page = "data:text/html,<button onclick=\"r.textContent = confirm('Sign out?') ? 'yes' : 'no'\">Sign out</button>\
<button onclick=\"r.textContent = prompt('Name?') ?? 'none'\">Name</button>\
<button onclick=\"alert('Saved'); r.textContent = 'alerted'\">Save</button>\
<button onclick=\"setTimeout(() => r.textContent = confirm('Later?') ? 'yes' : 'no', 300)\">Later</button><p id=r>-</p>";
    let web = match Chrome::launch(page, 800, 600, false).await {
        Ok(web) => web,
        Err(e) => {
            eprintln!("skipping: Chrome didn't start ({e})");
            return;
        }
    };
    let driver = Driver::new(web);
    let labels = async || {
        driver
            .see(None, false)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.label)
            .collect::<Vec<_>>()
    };

    driver.tap(&m("Sign out"), false).await.unwrap();
    assert_eq!(labels().await, ["Sign out?", "OK", "Cancel"]);
    assert!(
        driver.tap(&m("Name"), false).await.is_err(),
        "the page is blocked"
    );
    driver.tap(&m("Cancel"), false).await.unwrap();
    assert!(labels().await.contains(&"no".to_string()));

    driver.tap(&m("Name"), false).await.unwrap();
    driver.device.type_text("Viola").await.unwrap();
    driver.tap(&m("OK"), false).await.unwrap();
    assert!(labels().await.contains(&"Viola".to_string()));

    driver.tap(&m("Save"), false).await.unwrap();
    assert_eq!(labels().await, ["Saved", "OK"]);
    driver.tap(&m("OK"), false).await.unwrap();
    assert!(labels().await.contains(&"alerted".to_string()));

    // The app awaits its unsynced counts before confirming, so the tap returns first and the dialog opens mid-read.
    driver.tap(&m("Later"), false).await.unwrap();
    let seen = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let labels = labels().await;
            if labels.contains(&"Later?".to_string()) {
                return labels;
            }
        }
    })
    .await
    .expect("a read that a dialog interrupts still answers");
    assert_eq!(seen, ["Later?", "OK", "Cancel"]);
    driver.device.close().await;
}

#[tokio::test]
async fn a_swipe_scrolls_and_close_shuts_a_tab_the_page_opened() {
    let dir = std::env::temp_dir().join(format!("soundcheck-tab-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("terms.html"), "<body><h1>Terms</h1></body>").unwrap();
    let main = dir.join("main.html");
    std::fs::write(
        &main,
        r#"<body style="height:3000px"><button onclick="open('terms.html', '_blank', 'noopener')">Terms</button>
<button onclick="this.textContent = 'scrolled ' + scrollY">Where</button>
<img src="data:image/gif;base64,R0lGODlhAQABAAAAACw=" width=400 height=400></body>"#,
    )
    .unwrap();
    let web = match Chrome::launch(&format!("file://{}", main.display()), 800, 600, false).await {
        Ok(web) => web,
        Err(e) => {
            eprintln!("skipping: Chrome didn't start ({e})");
            return;
        }
    };
    let driver = Driver::new(web);

    // Starting on the image, as a drag would have pulled it out of the page.
    driver
        .device
        .swipe((200.0, 300.0), (200.0, 100.0), 300)
        .await
        .unwrap();
    driver
        .device
        .goto("close")
        .await
        .expect_err("the app's own tab stays");
    driver
        .device
        .swipe((200.0, 300.0), (200.0, 500.0), 300)
        .await
        .unwrap();
    driver
        .device
        .swipe((200.0, 300.0), (200.0, 100.0), 300)
        .await
        .unwrap();
    driver.tap(&m("Where"), false).await.unwrap();
    assert!(
        driver
            .see(None, false)
            .await
            .unwrap()
            .iter()
            .any(|e| e.label == "scrolled 200"),
        "a swipe up by 200 scrolls down by 200"
    );

    driver.tap(&m("Terms"), false).await.unwrap();
    driver.wait(&m("Terms"), 5_000).await.unwrap();
    driver.device.goto("close").await.unwrap();
    driver.wait(&m("scrolled 200"), 5_000).await.unwrap();
    driver.device.close().await;
}

#[tokio::test]
async fn a_touch_drags_what_the_page_lets_you_drag_and_drops_it() {
    let dir = std::env::temp_dir().join(format!("soundcheck-drag-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let main = dir.join("drag.html");
    std::fs::write(
        &main,
        r#"<body>
<div draggable="true" ondragstart="event.dataTransfer.setData('text/x-score', 'Bach')"
  style="position:absolute;left:20px;top:20px;width:120px;height:40px">Bach</div>
<div ondragover="event.preventDefault()"
  ondrop="event.preventDefault(); window.drops = (window.drops || 0) + 1; this.textContent = 'dropped ' + event.dataTransfer.getData('text/x-score') + ' ' + drops"
  style="position:absolute;left:300px;top:300px;width:200px;height:100px">Playlist</div>
</body>"#,
    )
    .unwrap();
    let web = match Chrome::launch(&format!("file://{}", main.display()), 800, 600, false).await {
        Ok(web) => web,
        Err(e) => {
            eprintln!("skipping: Chrome didn't start ({e})");
            return;
        }
    };
    let driver = Driver::new(web);
    let stop = |x, y| Stop {
        x,
        y,
        pause_ms: 100,
    };

    driver
        .device
        .touch(&[stop(80.0, 40.0), stop(400.0, 350.0)], false)
        .await
        .unwrap();
    driver.wait(&m("dropped Bach 1"), 5_000).await.unwrap();

    // Taken back before letting go, nothing lands.
    driver
        .device
        .touch(&[stop(80.0, 40.0), stop(400.0, 350.0)], true)
        .await
        .unwrap();
    driver.device.release(true).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        driver
            .see(None, false)
            .await
            .unwrap()
            .iter()
            .any(|e| e.label == "dropped Bach 1"),
        "a cancelled drag drops nothing"
    );
    driver.device.close().await;
}

/// The import's rows (SA-1009): the drag's last step crosses into a row's label, and the row takes the drop only
/// from `dragover`, which Chrome sends on the update after the one that entered the label.
#[tokio::test]
async fn a_drag_ending_as_it_enters_a_rows_label_still_drops() {
    let dir = std::env::temp_dir().join(format!("soundcheck-drag-label-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let main = dir.join("drag.html");
    // The path to 400,350 steps from 390,340 straight into the label.
    std::fs::write(
        &main,
        r#"<body>
<div draggable="true" ondragstart="event.dataTransfer.setData('text/x-part', 'Viola')"
  style="position:absolute;left:20px;top:20px;width:120px;height:40px">Viola</div>
<div ondragover="event.preventDefault()"
  ondrop="event.preventDefault(); document.getElementById('said').textContent = 'dropped ' + event.dataTransfer.getData('text/x-part')"
  style="position:absolute;left:300px;top:300px;width:200px;height:100px">
  <span style="position:absolute;left:95px;top:45px;width:20px;height:10px">Q</span>
</div>
<p id="said" style="position:absolute;top:500px">nothing dropped</p>
</body>"#,
    )
    .unwrap();
    let web = match Chrome::launch(&format!("file://{}", main.display()), 800, 600, false).await {
        Ok(web) => web,
        Err(e) => {
            eprintln!("skipping: Chrome didn't start ({e})");
            return;
        }
    };
    let driver = Driver::new(web);
    let stop = |x, y, pause_ms| Stop { x, y, pause_ms };

    driver
        .device
        .touch(&[stop(80.0, 40.0, 700), stop(400.0, 350.0, 0)], false)
        .await
        .unwrap();
    driver.wait(&m("dropped Viola"), 5_000).await.unwrap();
    driver.device.close().await;
}

#[tokio::test]
async fn a_file_picker_offers_what_was_pushed_and_downloads_are_seen() {
    let dir = std::env::temp_dir().join(format!("soundcheck-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let main = dir.join("files.html");
    std::fs::write(
        &main,
        r#"<body>
<label>One part <input type="file" accept="application/pdf"
  onchange="document.getElementById('one').textContent = 'Picked ' + [...this.files].map(f => f.name + ' ' + f.size).join(', ')"></label>
<p id="one">Picked nothing</p>
<label>Several parts <input type="file" multiple
  onchange="document.getElementById('several').textContent = 'Picked ' + [...this.files].map(f => f.name).join(', ')"></label>
<p id="several">Picked nothing</p>
<button onclick="const a = document.createElement('a'); a.href = window.URL.createObjectURL(new Blob(['%PDF-1.7 running order'], {type: 'application/pdf'})); a.download = 'gala-running-order.pdf'; a.click()">Save running order</button>
</body>"#,
    )
    .unwrap();
    let pdf = dir.join("score.pdf");
    std::fs::write(&pdf, b"%PDF-1.7 a score").unwrap();
    let web = match Chrome::launch(&format!("file://{}", main.display()), 800, 600, false).await {
        Ok(web) => web,
        Err(e) => {
            eprintln!("skipping: Chrome didn't start ({e})");
            return;
        }
    };
    let driver = Driver::new(web);
    driver.device.push_file(&pdf, "viola.pdf").await.unwrap();
    driver.device.push_file(&pdf, "cello.pdf").await.unwrap();

    // Cancelled, the page gets nothing; picked, it gets the file under its pushed name.
    driver.tap(&m("One part"), false).await.unwrap();
    driver.wait(&m("File picker"), 3_000).await.unwrap();
    assert!(
        driver.tap(&m("Save running order"), false).await.is_err(),
        "the picker covers the page until it's answered"
    );
    driver.tap(&m("Cancel"), false).await.unwrap();
    driver.wait(&m("Picked nothing"), 3_000).await.unwrap();
    driver.tap(&m("One part"), false).await.unwrap();
    driver.wait(&m("File picker"), 3_000).await.unwrap();
    driver.tap(&m("viola.pdf"), false).await.unwrap();
    driver.wait(&m("Picked viola.pdf 16"), 3_000).await.unwrap();

    // A picker that takes several ticks each, then gives them all.
    driver.tap(&m("Several parts"), false).await.unwrap();
    driver.wait(&m("File picker"), 3_000).await.unwrap();
    driver.tap(&m("viola.pdf"), false).await.unwrap();
    driver.tap(&m("cello.pdf"), false).await.unwrap();
    assert_eq!(
        driver.expect(&m("cello.pdf")).await.unwrap().checked,
        Some(true)
    );
    driver.tap(&m("Choose"), false).await.unwrap();
    driver
        .wait(&m("Picked viola.pdf, cello.pdf"), 3_000)
        .await
        .unwrap();

    driver.tap(&m("Save running order"), false).await.unwrap();
    let mut seen = None;
    for _ in 0..50 {
        seen = driver
            .see(None, false)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.label == "Downloaded gala-running-order.pdf");
        if seen.as_ref().and_then(|e| e.value.as_deref()) == Some("PDF, 22 bytes") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        seen.and_then(|e| e.value).as_deref(),
        Some("PDF, 22 bytes"),
        "a finished download shows what it is"
    );
    driver.device.close().await;
}

/// A page whose service worker answers every navigation itself once it controls the page.
async fn worker_app() -> (u16, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = [0u8; 2048];
            let n = stream.read(&mut request).await.unwrap_or(0);
            let (kind, body) = if String::from_utf8_lossy(&request[..n]).starts_with("GET /sw.js") {
                (
                    "text/javascript",
                    "addEventListener('install', () => skipWaiting()); addEventListener('activate', (e) => e.waitUntil(clients.claim())); addEventListener('fetch', (e) => e.request.mode === 'navigate' && e.respondWith(new Response('<p>served by the worker</p>', { headers: { 'Content-Type': 'text/html' } })));",
                )
            } else {
                (
                    "text/html",
                    "<p id=s>uncontrolled</p><script>navigator.serviceWorker.addEventListener('controllerchange', () => s.textContent = 'controlled'); navigator.serviceWorker.register('/sw.js');</script>",
                )
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    (port, server)
}

/// Launches Chrome on the worker app and waits for the worker to control the page; `None` when Chrome won't start.
async fn controlled(port: u16) -> Option<Driver<Chrome>> {
    let web = match Chrome::launch(&format!("http://localhost:{port}/"), 800, 600, false).await {
        Ok(web) => web,
        Err(e) => {
            eprintln!("skipping: Chrome didn't start ({e})");
            return None;
        }
    };
    let driver = Driver::new(web);
    let mut seen = Vec::new();
    for _ in 0..50 {
        seen = driver.see(None, false).await.unwrap();
        if seen.iter().any(|e| e.label == "controlled") {
            break;
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(seen.iter().any(|e| e.label == "controlled"), "{seen:#?}");
    Some(driver)
}

/// The web app works offline through its service worker, so the worker has to run in soundcheck's Chrome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_service_worker_starts_and_controls_the_page() {
    let (port, server) = worker_app().await;
    let Some(driver) = controlled(port).await else {
        return;
    };
    // A relaunch navigates a page the worker now controls.
    driver.device.launch().await.unwrap();
    // With the server gone, only the worker can answer.
    server.abort();
    let _ = server.await;
    driver.device.launch().await.unwrap();
    let seen = driver.see(None, false).await.unwrap();
    assert!(
        seen.iter().any(|e| e.label == "served by the worker"),
        "{seen:#?}"
    );
    driver.device.close().await;
}

/// A new build installed mid-run: the old build's worker would keep answering, so the next launch reaches the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_build_drops_the_old_builds_worker() {
    let (port, server) = worker_app().await;
    let Some(driver) = controlled(port).await else {
        return;
    };
    driver.device.new_build().await.unwrap();
    driver.device.launch().await.unwrap();
    let seen = driver.see(None, false).await.unwrap();
    assert!(
        !seen.iter().any(|e| e.label == "served by the worker"),
        "{seen:#?}"
    );
    server.abort();
    driver.device.close().await;
}
