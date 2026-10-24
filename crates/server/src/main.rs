use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use soundcheck::run::Run;
use soundcheck::suite::config::Platform;
use soundcheck::{VERSION, lint, serve, suite};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "soundcheck", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check a suite's config and specs; exits 1 if anything is wrong.
    Lint {
        /// The suite folder (the one holding suite.yaml).
        suite: PathBuf,
    },
    /// Start a run: lint, guard, then serve the MCP tools and event stream until stopped.
    Run(RunArgs),
    /// Run one driver command on the emulator or simulator, outside a run: `soundcheck drive ios see`.
    Drive {
        platform: Platform,
        /// The suite folder, for the app's id (default: the current folder).
        #[arg(long, default_value = ".")]
        suite: PathBuf,
        #[command(subcommand)]
        action: soundcheck::drive::Action,
    },
    /// Show the suite's runs in the browser, finished or not, without a run going: `--open latest` opens the newest.
    Serve {
        /// The suite folder (the one holding suite.yaml).
        suite: PathBuf,
        #[arg(long, default_value_t = 7357)]
        port: u16,
        /// A run to open (its folder name), or `latest`; without it, the list of runs opens.
        #[arg(long)]
        open: Option<String>,
        /// Print the address without opening the browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Show the live run on this Mac's network, read-only, for a phone on the same Wi-Fi: prints a link with a key.
    View {
        /// The port the run's server is on.
        #[arg(long, default_value_t = 7357)]
        server: u16,
        /// The port to serve on, at this Mac's network address.
        #[arg(long, default_value_t = 7358)]
        port: u16,
    },
    /// Print one line per run event, replaying from a cursor: run it under a Monitor.
    Events {
        /// The run's folder (holding server.json).
        run: PathBuf,
        /// Only events after this sequence number.
        #[arg(long, default_value_t = 0)]
        after: u64,
        /// Print what the journal holds now and exit, instead of following the live stream.
        #[arg(long)]
        once: bool,
    },
    /// The plugin's MCP server over stdio: forwards each tool call to the run serving on the port.
    Mcp {
        #[arg(long, default_value_t = 7357)]
        port: u16,
    },
    /// The suite's network proxy (suite.yaml `network`): runs and drive commands start it when it isn't up.
    Proxy {
        #[arg(value_parser = ["serve", "status", "stop"])]
        action: String,
        /// The suite folder (default: the current folder).
        #[arg(long, default_value = ".")]
        suite: PathBuf,
    },
}

#[derive(clap::Args)]
struct RunArgs {
    suite: PathBuf,
    /// Platforms to run (default: every platform in suite.yaml).
    #[arg(long, value_delimiter = ',')]
    platforms: Vec<Platform>,
    /// Features or spec IDs to run, e.g. `help,scores/003-add-part` (default: every spec). Only these become
    /// rows, so the verdict covers exactly the scope.
    #[arg(long, value_delimiter = ',')]
    scope: Vec<String>,
    /// Carry on a run from its folder instead of starting a new one.
    #[arg(long)]
    resume: Option<PathBuf>,
    #[arg(long, default_value_t = 7357)]
    port: u16,
    /// The release under test, shown as the page's title, e.g. "v2.16.2 · candidate".
    #[arg(long)]
    label: Option<String>,
    /// Don't open the live page in the browser when the run starts.
    #[arg(long)]
    no_open: bool,
    /// Tester silence between tool calls before its spec counts as stalled.
    #[arg(long, default_value_t = 120)]
    stall_seconds: u64,
    /// How long a dispatched tester has to make its first tool call before its spec counts as stalled.
    #[arg(long, default_value_t = 600)]
    first_call_seconds: u64,
    /// Show the devices to watch the run (Chrome, Simulator.app, and the window of an emulator soundcheck restarts),
    /// rather than headless. Look only: a click or resize in one can steer a test.
    #[arg(long)]
    headed: bool,
    /// Build and install the current tree on every platform before starting, with the suite's install hook. Without
    /// it, only a platform whose installed build can't be identified is built.
    #[arg(long)]
    install: bool,
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Lint { suite } => return lint_command(&suite),
        Command::Run(args) => run_command(args),
        Command::Events { run, after, once } => events_command(&run, after, once),
        Command::Proxy { action, suite } => tokio::runtime::Runtime::new()
            .map_err(anyhow::Error::from)
            .and_then(|rt| rt.block_on(proxy_command(&action, &suite)))
            .map(|out| println!("{out}")),
        Command::Mcp { port } => tokio::runtime::Runtime::new()
            .map_err(anyhow::Error::from)
            .and_then(|rt| rt.block_on(soundcheck::bridge::bridge(port))),
        Command::Serve {
            suite,
            port,
            open,
            no_open,
        } => serve_command(&suite, port, open, !no_open),
        Command::View { server, port } => view_command(server, port),
        Command::Drive {
            platform,
            suite,
            action,
        } => tokio::runtime::Runtime::new()
            .map_err(anyhow::Error::from)
            .and_then(|rt| rt.block_on(soundcheck::drive::drive(&suite, platform, action)))
            .map(|out| println!("{out}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("soundcheck: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn lint_command(root: &Path) -> ExitCode {
    let suite = suite::load(root);
    let problems = lint::lint(&suite, VERSION);
    for p in &problems {
        eprintln!("{}: {}", root.join(&p.path).display(), p.message);
    }
    if problems.is_empty() {
        println!(
            "{} specs in {} features: ok",
            suite.specs.len(),
            suite.features.len()
        );
        ExitCode::SUCCESS
    } else {
        eprintln!("{} problems", problems.len());
        ExitCode::FAILURE
    }
}

fn run_command(args: RunArgs) -> Result<()> {
    let RunArgs {
        suite: root,
        platforms,
        scope,
        resume,
        port,
        label,
        no_open,
        stall_seconds,
        first_call_seconds,
        headed,
        install,
    } = args;
    let (root, scope, open_page) = (root.as_path(), scope.as_slice(), !no_open);
    let suite = suite::load(root);
    let problems = lint::lint(&suite, VERSION);
    if !problems.is_empty() {
        bail!(
            "the suite doesn't lint clean ({} problems); run `soundcheck lint {}`",
            problems.len(),
            root.display()
        );
    }
    let config = suite.config.as_ref().context("suite.yaml")?;
    let guard = std::process::Command::new("sh")
        .args(["-c", &config.hooks.guard])
        .current_dir(root)
        .output()
        .context("running the guard hook")?;
    if !guard.status.success() {
        bail!(
            "the guard hook refused: {}",
            String::from_utf8_lossy(&guard.stderr).trim()
        );
    }
    // Preflight: every check passes before anything runs, or the run refuses with the reason.
    let mut checks = Vec::new();
    pass(
        &mut checks,
        format!("the suite lints clean ({} specs)", suite.specs.len()),
    );
    let said = String::from_utf8_lossy(&guard.stdout).trim().to_string();
    pass(
        &mut checks,
        if said.is_empty() {
            "the guard passed".into()
        } else {
            format!("the guard passed: {said}")
        },
    );
    // A resumed run keeps how it was started, unless told otherwise.
    let saved = resume
        .as_deref()
        .and_then(|dir| soundcheck::run::saved::read_info(dir).ok());
    // On resume, `--platforms` only adds: dropping a platform would orphan its rows in the journal.
    let platforms: Vec<Platform> = match (platforms.is_empty(), &saved) {
        (false, Some(info)) => {
            let mut all = info.platforms.clone();
            for p in platforms {
                if !all.contains(&p) {
                    all.push(p);
                }
            }
            all
        }
        (false, None) => platforms,
        (true, Some(info)) => info.platforms.clone(),
        (true, None) => config.platforms.keys().copied().collect(),
    };
    let added: Vec<Platform> = saved.as_ref().map_or_else(Vec::new, |info| {
        platforms
            .iter()
            .copied()
            .filter(|p| !info.platforms.contains(p))
            .collect()
    });
    // Likewise `--scope` only adds, so no row in the journal is orphaned: the run opens with its saved scope and
    // new specs join through the journal.
    let (plan, added_specs, scope) = match &saved {
        Some(info) => soundcheck::run::saved::resume_plan(&suite, &platforms, &info.scope, scope)?,
        None => (
            soundcheck::run::saved::plan_for(&suite, &platforms, scope)?,
            Vec::new(),
            scope.to_vec(),
        ),
    };
    let label = label.or_else(|| saved.as_ref().map(|i| i.label.clone()));
    let resuming = resume.is_some();
    let run_dir = match resume {
        Some(dir) => dir,
        None => {
            let reports = suite::reports_dir(root, config);
            reports.join(run_name())
        }
    };
    std::fs::create_dir_all(&run_dir)?;
    let run_dir = std::fs::canonicalize(&run_dir)?;
    let mut run = Run::open(
        plan,
        &run_dir.join(soundcheck::run::saved::JOURNAL_FILE),
        serve::now_ms(),
    )?;
    if resuming {
        let interrupted = run.interrupt_running(serve::now_ms())?;
        if interrupted > 0 {
            println!("{interrupted} spec(s) were running when the server stopped: queued again");
        }
        // Specs first: a platform added with them may have rows only among the new specs.
        if !added_specs.is_empty() {
            let ids: Vec<String> = added_specs.iter().map(|s| s.id.clone()).collect();
            soundcheck::run::saved::snapshot_specs(&run_dir, &suite, &ids)?;
            run.add_specs(serve::now_ms(), added_specs)?;
            println!("added {} to the run: their rows join it", ids.join(", "));
        }
        if !added.is_empty() {
            run.add_platforms(serve::now_ms(), added.clone())?;
            println!(
                "added {} to the run: its specs in scope join it",
                added
                    .iter()
                    .map(Platform::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if let Some(info) = saved
            .as_ref()
            .filter(|i| i.scope != scope || !added.is_empty())
        {
            soundcheck::run::saved::write_info(
                &run_dir,
                &soundcheck::run::saved::RunInfo {
                    platforms: platforms.clone(),
                    scope: scope.clone(),
                    ..info.clone()
                },
            )?;
        }
    }
    let stall_ms = stall_seconds * 1000;

    let label = label.unwrap_or_else(|| {
        root.file_name()
            .map_or_else(|| "regression".into(), |n| n.to_string_lossy().into_owned())
    });
    if !resuming || saved.is_none() {
        soundcheck::run::saved::save(
            &run_dir,
            &soundcheck::run::saved::RunInfo {
                suite: std::fs::canonicalize(root)?,
                label: label.clone(),
                platforms: platforms.clone(),
                scope: scope.clone(),
            },
            &suite,
        )?;
    }
    // Listed among the runs from the start, even if the server dies before anything happens.
    std::fs::write(
        run_dir.join(serve::SUMMARY_FILE),
        soundcheck::report::run_summary(&run, &label).to_string(),
    )?;
    let config_has_install = config.hooks.install.is_some();
    let ready: std::collections::HashMap<Platform, Vec<String>> = config
        .platforms
        .iter()
        .map(|(p, c)| (*p, c.ready.clone()))
        .collect();
    let config_web_start = config
        .platforms
        .get(&Platform::Web)
        .and_then(|w| Some((w.start.clone()?, w.base_url.clone()?)));
    let root_dir = root.to_path_buf();
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        // The web app under test, if the suite starts one: it runs as long as the run does.
        let _web_app = match config_web_start {
            Some((start, base_url)) if platforms.contains(&Platform::Web) => {
                println!("starting the web app: {start}");
                Some(
                    serve::hooks::Service::start(
                        &root_dir,
                        &start,
                        &base_url,
                        serve::hooks::START_TIMEOUT,
                    )
                    .await?,
                )
            }
            _ => None,
        };
        let mut hub = serve::Hub::new(suite, run, label, run_dir.clone(), stall_ms);
        if let Some(only) = std::sync::Arc::get_mut(&mut hub) {
            only.headed = headed;
            only.start_ms = first_call_seconds * 1000;
        }
        // Results count only for an identified build, so a platform without one is built before anything runs.
        let has_install_hook = config_has_install;
        for &platform in &platforms {
            let unknown = hub.read_build(platform).await.err();
            if !(install || unknown.is_some() && has_install_hook) {
                continue;
            }
            match &unknown {
                Some(why) if !install => {
                    println!("{platform}: the installed build can't be identified ({why:#}); building and installing it now, which takes several minutes");
                }
                _ => println!("{platform}: building and installing the current tree, which takes several minutes"),
            }
            hub.install(platform).await?;
        }
        let problems = hub.read_builds(&platforms).await;
        if has_install_hook && !problems.is_empty() {
            bail!("the build can't be identified on {}", problems.join("; "));
        }
        for problem in problems {
            eprintln!("soundcheck: build identity unknown for {problem}");
        }
        for &platform in &platforms {
            if let Some(build) =
                hub.with_run(|run, _| Ok(run.state().builds.get(&platform).cloned()))?
            {
                pass(&mut checks, format!("{platform} has build {}", build.label()));
            }
        }
        // Devices and the app: connected, launched, and showing what proves it's ready (signed in, say).
        for &platform in &platforms {
            let labels = ready.get(&platform).cloned().unwrap_or_default();
            hub.on_device(platform, async |d| {
                soundcheck_driver::Device::launch(&d.device).await?;
                for label in &labels {
                    let m = soundcheck_driver::Matcher::parse(label)
                        .map_err(|e| soundcheck_driver::DriverError::Invalid(e.to_string()))?;
                    d.wait(&m, 30_000).await.map_err(|_| {
                        soundcheck_driver::DriverError::NotFound(format!(
                            "{label:?} didn't show after launching: is the app signed in and past onboarding?"
                        ))
                    })?;
                }
                Ok(())
            })
            .await
            .with_context(|| format!("{platform} isn't ready to test"))?;
            pass(&mut checks, if labels.is_empty() {
                format!("{platform}: the device answers and the app launched")
            } else {
                format!(
                    "{platform}: the device answers, the app launched and shows {}",
                    labels.join(", ")
                )
            });
        }
        hub.with_run(|run, now| run.preflight(now, checks.clone()))?;
        let server = serve::start(hub, port).await?;
        let url = format!("http://{}", server.addr);
        let info = serde_json::json!({
            "url": url,
            "mcp": format!("{url}/mcp"),
            "run": run_dir,
        });
        std::fs::write(
            run_dir.join("server.json"),
            serde_json::to_string_pretty(&info)?,
        )?;
        println!("soundcheck {VERSION}: run in {}", run_dir.display());
        println!("MCP at {url}/mcp; the live page is at {url}/");
        if open_page {
            let _ = std::process::Command::new("open")
                .arg(format!("{url}/"))
                .spawn();
        }
        tokio::signal::ctrl_c().await?;
        server.shutdown.cancel();
        let _ = server.handle.await;
        Ok(())
    })
}

fn serve_command(root: &Path, port: u16, open: Option<String>, open_page: bool) -> Result<()> {
    let suite = suite::load(root);
    let config = suite.config.as_ref().context("no suite.yaml")?;
    let dir = suite::reports_dir(root, config);
    let reports = serve::reports::Reports {
        dir: dir.clone(),
        current: None,
    };
    let path = match open.as_deref() {
        None => "/".to_string(),
        Some(name) => {
            let runs = serve::reports::list(&reports);
            let found = if name == "latest" {
                runs.first()
            } else {
                runs.iter().find(|(n, _, _)| n == name)
            };
            let (_, _, link) =
                found.with_context(|| format!("no run {name} in {}", dir.display()))?;
            format!(
                "/{}",
                link.as_deref()
                    .with_context(|| format!("{name} has nothing to show"))?
            )
        }
    };
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let server = serve::start_reports(dir, port).await?;
        let url = format!("http://{}{path}", server.addr);
        println!(
            "soundcheck {VERSION}: runs at http://{}/; showing {url}",
            server.addr
        );
        if open_page {
            let _ = std::process::Command::new("open").arg(&url).spawn();
        }
        tokio::signal::ctrl_c().await?;
        server.shutdown.cancel();
        let _ = server.handle.await;
        Ok(())
    })
}

fn events_command(run_dir: &Path, after: u64, once: bool) -> Result<()> {
    if once {
        let journal = std::fs::read_to_string(run_dir.join("journal.jsonl"))
            .context("no journal.jsonl: is this a run folder?")?;
        let mut out = std::io::stdout();
        for line in journal.lines().filter(|l| !l.trim().is_empty()) {
            let entry: soundcheck::run::journal::Entry = serde_json::from_str(line)?;
            if entry.seq > after {
                writeln!(out, "{}", serve::events::summary(&entry))?;
            }
        }
        return Ok(());
    }
    let info: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(run_dir.join("server.json"))
            .context("no server.json: is the run started?")?,
    )?;
    let url = info["url"].as_str().context("server.json: url")?;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let mut response = reqwest::Client::new()
            .get(format!("{url}/events?after={after}"))
            .send()
            .await?
            .error_for_status()?;
        let mut pending = Vec::new();
        let mut out = std::io::stdout();
        // A stopped server ends the stream mid-chunk, which is how a run ends, not an error.
        while let Ok(Some(chunk)) = response.chunk().await {
            pending.extend_from_slice(&chunk);
            while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                let entry: soundcheck::run::journal::Entry = serde_json::from_slice(&line)?;
                writeln!(out, "{}", serve::events::summary(&entry))?;
                out.flush()?;
            }
        }
        writeln!(out, "the run's server stopped")?;
        Ok(())
    })
}

fn run_name() -> String {
    let secs = serve::now_ms() / 1000;
    let output = std::process::Command::new("date")
        .args(["-r", &secs.to_string(), "+%Y%m%d-%H%M%S"])
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => secs.to_string(),
    }
}

/// A preflight check that passed: shown as it happens, and kept for the journal.
fn pass(checks: &mut Vec<String>, check: String) {
    println!("✓ {check}");
    checks.push(check);
}

async fn proxy_command(action: &str, root: &Path) -> Result<String> {
    use soundcheck_driver::proxy;
    let loaded = suite::load(root);
    let config = loaded.config.as_ref().context("no suite.yaml")?;
    let routes =
        soundcheck::network::routes(config).context("suite.yaml has no network section")?;
    let socket = proxy::socket_path(&routes);
    match action {
        "serve" => {
            if proxy::command(&socket, "status").await.is_ok() {
                bail!("the proxy is already running ({})", socket.display());
            }
            proxy::serve(&routes, &socket).await?;
            Ok("stopped".into())
        }
        "status" => Ok(proxy::command(&socket, "status")
            .await
            .unwrap_or_else(|_| "not running".into())),
        _ => Ok(proxy::command(&socket, "stop")
            .await
            .unwrap_or_else(|_| "not running".into())),
    }
}

fn view_command(server: u16, port: u16) -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(serve::viewer::view(server, port))
}
