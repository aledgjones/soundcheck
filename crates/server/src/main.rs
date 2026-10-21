use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use soundcheck::run::{Plan, PlannedSpec, Run};
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
    Run {
        suite: PathBuf,
        /// Platforms to run (default: every platform in suite.yaml).
        #[arg(long, value_delimiter = ',')]
        platforms: Vec<Platform>,
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
    },
    /// Print one line per run event, replaying from a cursor: run it under a Monitor.
    Events {
        /// The run's folder (holding server.json).
        run: PathBuf,
        /// Only events after this sequence number.
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Lint { suite } => return lint_command(&suite),
        Command::Run {
            suite,
            platforms,
            resume,
            port,
            label,
            no_open,
            stall_seconds,
        } => run_command(
            &suite,
            platforms,
            resume,
            port,
            label,
            !no_open,
            stall_seconds,
        ),
        Command::Events { run, after } => events_command(&run, after),
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

fn run_command(
    root: &Path,
    platforms: Vec<Platform>,
    resume: Option<PathBuf>,
    port: u16,
    label: Option<String>,
    open_page: bool,
    stall_seconds: u64,
) -> Result<()> {
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
    let platforms: Vec<Platform> = if platforms.is_empty() {
        config.platforms.keys().copied().collect()
    } else {
        platforms
    };
    let plan = Plan {
        specs: suite
            .specs
            .iter()
            .map(|s| PlannedSpec {
                id: s.id.clone(),
                platforms: s
                    .front
                    .platforms
                    .iter()
                    .copied()
                    .filter(|p| platforms.contains(p))
                    .collect(),
                outcomes: s.outcomes.clone(),
            })
            .filter(|s| !s.platforms.is_empty())
            .collect(),
        parallel: config.parallel,
    };
    let run_dir = match resume {
        Some(dir) => dir,
        None => {
            let reports = suite::reports_dir(root, config);
            reports.join(run_name())
        }
    };
    std::fs::create_dir_all(&run_dir)?;
    let run = Run::open(plan, &run_dir.join("journal.jsonl"), serve::now_ms())?;
    let stall_ms = stall_seconds * 1000;

    let label = label.unwrap_or_else(|| {
        root.file_name()
            .map_or_else(|| "regression".into(), |n| n.to_string_lossy().into_owned())
    });
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let hub = serve::Hub::new(suite, run, label, stall_ms);
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

fn events_command(run_dir: &Path, after: u64) -> Result<()> {
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
