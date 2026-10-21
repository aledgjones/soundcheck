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
    /// Show web's Chrome in a window to watch the run, rather than headless. Look only: a click or resize in it can
    /// steer a test.
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
        Command::Serve {
            suite,
            port,
            open,
            no_open,
        } => serve_command(&suite, port, open, !no_open),
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
    // A resumed run keeps how it was started, unless told otherwise.
    let saved = resume
        .as_deref()
        .and_then(|dir| soundcheck::run::saved::read_info(dir).ok());
    let platforms: Vec<Platform> = match (platforms.is_empty(), &saved) {
        (false, _) => platforms,
        (true, Some(info)) => info.platforms.clone(),
        (true, None) => config.platforms.keys().copied().collect(),
    };
    let scope: Vec<String> = match (scope.is_empty(), &saved) {
        (true, Some(info)) => info.scope.clone(),
        _ => scope.to_vec(),
    };
    let label = label.or_else(|| saved.as_ref().map(|i| i.label.clone()));
    let plan = soundcheck::run::saved::plan_for(&suite, &platforms, &scope)?;
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
        const CLOSED: &str = "the event stream closed: the run's server stopped";
        while let Some(chunk) = response.chunk().await.context(CLOSED)? {
            pending.extend_from_slice(&chunk);
            while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                let entry: soundcheck::run::journal::Entry = serde_json::from_slice(&line)?;
                writeln!(out, "{}", serve::events::summary(&entry))?;
                out.flush()?;
            }
        }
        bail!(CLOSED)
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
