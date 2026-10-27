use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use soundcheck::run::Run;
use soundcheck::run::event::Event;
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
    /// What each tester attempt in a run cost: tokens, calls by tool and working time, with totals.
    Usage {
        /// The run's folder.
        run: PathBuf,
        /// Another run's folder: pairs each spec on each platform with its last attempt there, and shows the change.
        #[arg(long)]
        compare: Option<PathBuf>,
        /// Add usage from a TSV (token, tokens, calls, duration_ms) to a run recorded before soundcheck kept it.
        #[arg(long, conflicts_with = "compare")]
        import: Option<PathBuf>,
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
    /// Whether a platform can take a spec: exits 0 when it can, else 1 with why it's held (a restart, a busy Mac).
    Ready {
        /// The run's folder (holding server.json).
        run: PathBuf,
        platform: Platform,
        /// Ask every 15 s until the platform is free, then exit; for running in the background after a held dispatch.
        #[arg(long)]
        wait: bool,
    },
    /// The plugin's MCP server over stdio: forwards each tool call to the run serving on the port.
    Mcp {
        #[arg(long, default_value_t = 7357)]
        port: u16,
    },
    /// The plugin's hooks, which Claude Code runs with the hook's input on stdin.
    #[command(hide = true)]
    Hook {
        #[arg(value_parser = ["subagent-stop", "stop", "session-start", "pre-tool-use"])]
        event: String,
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
        Command::Ready {
            run,
            platform,
            wait,
        } => ready_command(&run, platform, wait),
        Command::Proxy { action, suite } => tokio::runtime::Runtime::new()
            .map_err(anyhow::Error::from)
            .and_then(|rt| rt.block_on(proxy_command(&action, &suite)))
            .map(|out| println!("{out}")),
        Command::Hook { event, port } => return hook_command(&event, port),
        Command::Mcp { port } => tokio::runtime::Runtime::new()
            .map_err(anyhow::Error::from)
            .and_then(|rt| rt.block_on(soundcheck::bridge::bridge(port))),
        Command::Usage {
            run,
            compare,
            import,
        } => match import {
            Some(tsv) => import_usage(&run, &tsv),
            None => usage_command(&run, compare.as_deref()),
        },
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
    if let Some(note) = take_port(port)? {
        pass(&mut checks, note);
    }
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
    // A label given on resume is a relabel, journalled once the run is open, so it outlasts this server.
    let relabel_to = label.clone().filter(|_| resume.is_some());
    let saved_label = saved.as_ref().map(|i| i.label.clone());
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
        if let Some(new) = relabel_to {
            hub.with_run(|run, now| {
                let current = run.state().label.clone().or(saved_label);
                if current.as_deref() == Some(new.as_str()) {
                    return Ok(());
                }
                run.relabel(now, &new, "given on resume")
            })?;
        }
        // Every device answers before anything is built, so a missing emulator or one too old shows in seconds, not
        // after a build. What needs the app (its ready labels) waits for the install.
        for &platform in &platforms {
            hub.on_device(platform, async |_| Ok(()))
                .await
                .with_context(|| format!("{platform} isn't ready to test"))?;
            pass(
                &mut checks,
                format!("{platform}: the device is there and soundcheck can drive it"),
            );
        }
        let settled = serve::wait_for_calm_host().await;
        // Results count only for an identified build, so a platform without one is built before anything runs.
        let has_install_hook = config_has_install;
        for &platform in &platforms {
            let unknown = hub.read_build(platform).await.err();
            if let Some(why) = &unknown
                && why.chain().any(|e| e.is::<serve::hooks::TimedOut>())
            {
                if settled {
                    bail!(
                        "{platform}: {why:#} on a settled Mac, so the hook itself is stuck; fix it rather than rebuilding"
                    );
                }
                bail!(
                    "{platform}: {why:#} while the Mac was still busy; start the run again once it has settled"
                );
            }
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
                            "{label:?} didn't show after launching; run `soundcheck drive {platform} see` from the suite folder to see what the screen shows"
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
        tokio::select! {
            stopped = tokio::signal::ctrl_c() => stopped?,
            _ = server.shutdown.cancelled() => println!("stepped down for a new run on the port"),
        }
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
        tokio::select! {
            stopped = tokio::signal::ctrl_c() => stopped?,
            _ = server.shutdown.cancelled() => println!("stepped down for a new run on the port"),
        }
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
    let url = server_url(run_dir)?;
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

/// Frees the port before anything slow happens: a finished run nobody is using, or a reports server, steps down;
/// anything else refuses the run with what holds the port.
fn take_port(port: u16) -> Result<Option<String>> {
    let taken = |port| std::net::TcpStream::connect(("127.0.0.1", port)).is_ok();
    if !taken(port) {
        return Ok(None);
    }

    let response = tokio::runtime::Runtime::new()?.block_on(async {
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/step-down"))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?;
        let status = response.status();
        anyhow::Ok((status, response.text().await?.trim().to_string()))
    });
    let (status, text) = match response {
        Ok(answer) => answer,
        Err(_) => bail!(
            "port {port} is in use by something other than soundcheck; stop it or pass --port"
        ),
    };
    if status == reqwest::StatusCode::CONFLICT {
        bail!("port {port} is held by another run: {text}");
    }

    if !status.is_success() {
        bail!(
            "port {port} is in use by something other than this soundcheck (an older version's server?); stop it or pass --port"
        );
    }

    for _ in 0..50 {
        if !taken(port) {
            return Ok(Some(text));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    bail!("{text}, but port {port} is still in use after 10 s")
}

fn read_attempts(run_dir: &Path) -> Result<Vec<soundcheck::run::usage::AttemptUsage>> {
    let path = run_dir.join(soundcheck::run::saved::JOURNAL_FILE);
    let journal = std::fs::read_to_string(&path)
        .with_context(|| format!("no {}: is this a run folder?", path.display()))?;
    let mut entries = Vec::new();
    for line in journal.lines().filter(|l| !l.trim().is_empty()) {
        entries.push(serde_json::from_str(line)?);
    }

    Ok(soundcheck::run::usage::attempts(&entries))
}

fn duration(ms: Option<u64>) -> String {
    match ms {
        Some(ms) => format!("{}m{:02}s", ms / 60_000, ms / 1000 % 60),
        None => "-".into(),
    }
}

fn opt(n: Option<u64>) -> String {
    n.map_or("-".into(), |n| n.to_string())
}

/// Always succeeds: a hook that fails would show in the person's session, and a missed hook costs nothing the run
/// can't recover. SubagentStop's message goes to stderr; the others print their JSON reply, if any, on stdout.
fn hook_command(event: &str, port: u16) -> ExitCode {
    use soundcheck::{plugin_hooks, tester_usage};
    let mut stdin = String::new();
    let result = std::io::Read::read_to_string(&mut std::io::stdin(), &mut stdin)
        .map_err(anyhow::Error::from)
        .and_then(|_| tokio::runtime::Runtime::new().map_err(anyhow::Error::from))
        .and_then(|rt| {
            rt.block_on(async {
                match event {
                    "subagent-stop" => tester_usage::subagent_stop(&stdin, port).await,
                    "stop" => plugin_hooks::stop(&stdin, port).await,
                    "session-start" => plugin_hooks::session_start(port).await,
                    _ => plugin_hooks::pre_tool_use(&stdin, port).await,
                }
            })
        });
    match (event, result) {
        ("subagent-stop", Ok(Some(line))) => eprintln!("soundcheck: {line}"),
        (_, Ok(Some(reply))) => println!("{reply}"),
        (_, Ok(None)) => {}
        (_, Err(e)) => eprintln!("soundcheck: {event} hook: {e:#}"),
    }

    ExitCode::SUCCESS
}

fn usage_command(run_dir: &Path, compare: Option<&Path>) -> Result<()> {
    let attempts = read_attempts(run_dir)?;
    if attempts.is_empty() {
        bail!("{} has no usage recorded", run_dir.display());
    }

    match compare {
        None => print_usage(&attempts),
        Some(other) => print_comparison(&read_attempts(other)?, &attempts),
    }

    Ok(())
}

fn print_usage(attempts: &[soundcheck::run::usage::AttemptUsage]) {
    let width = attempts.iter().map(|a| a.token.len()).max().unwrap_or(0);
    println!(
        "{:width$}  {:>7}  {:>10}  {:>5}  {:>6}  top tools",
        "attempt", "tokens", "cache read", "calls", "time"
    );
    for a in attempts {
        let mut tools: Vec<(&String, &u32)> = a.calls.iter().collect();
        tools.sort_by(|x, y| y.1.cmp(x.1).then(x.0.cmp(y.0)));
        let top: Vec<String> = tools
            .iter()
            .take(4)
            .map(|(t, n)| format!("{t} {n}"))
            .collect();
        println!(
            "{:width$}  {:>7}  {:>10}  {:>5}  {:>6}  {}",
            a.token,
            opt(a.tokens),
            opt(a.spent.map(|s| s.cache_read)),
            a.total_calls(),
            duration(a.duration_ms),
            top.join(", ")
        );
    }

    let n = attempts.len() as u64;
    let tokens: u64 = attempts.iter().filter_map(|a| a.tokens).sum();
    let calls: u64 = attempts.iter().map(|a| u64::from(a.total_calls())).sum();
    let ms: u64 = attempts.iter().filter_map(|a| a.duration_ms).sum();
    println!(
        "{} attempts: {tokens} tokens, {calls} calls, {} (average {} tokens, {} calls, {})",
        n,
        duration(Some(ms)),
        tokens / n,
        calls / n,
        duration(Some(ms / n))
    );
}

/// `before` and `after` paired by spec and platform, each side's last attempt; a row in only one run is left out.
fn print_comparison(
    before: &[soundcheck::run::usage::AttemptUsage],
    after: &[soundcheck::run::usage::AttemptUsage],
) {
    let last = |attempts: &[soundcheck::run::usage::AttemptUsage]| {
        attempts
            .iter()
            .map(|a| (a.row(), a.clone()))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let (before, after) = (last(before), last(after));
    let pairs: Vec<_> = after
        .iter()
        .filter_map(|(row, a)| before.get(row).map(|b| (row, b, a)))
        .collect();
    if pairs.is_empty() {
        println!("no spec ran on the same platform in both runs");
        return;
    }

    let change = |b: Option<u64>, a: Option<u64>| match (b, a) {
        (Some(b), Some(a)) if b > 0 => {
            format!(
                "{b} → {a} ({:+}%)",
                (a as f64 / b as f64 * 100.0 - 100.0).round()
            )
        }
        _ => format!("{} → {}", opt(b), opt(a)),
    };
    let width = pairs.iter().map(|(r, ..)| r.len()).max().unwrap_or(0);
    println!("{:width$}  tokens / calls / time", "spec");
    let (mut tb, mut ta, mut cb, mut ca, mut db, mut da) = (0, 0, 0, 0, 0, 0);
    for (row, b, a) in &pairs {
        println!(
            "{row:width$}  {} / {} / {} → {}",
            change(b.tokens, a.tokens),
            change(Some(b.total_calls().into()), Some(a.total_calls().into())),
            duration(b.duration_ms),
            duration(a.duration_ms)
        );
        tb += b.tokens.unwrap_or(0);
        ta += a.tokens.unwrap_or(0);
        cb += u64::from(b.total_calls());
        ca += u64::from(a.total_calls());
        db += b.duration_ms.unwrap_or(0);
        da += a.duration_ms.unwrap_or(0);
    }

    println!(
        "{} pairs: {} tokens / {} calls / {} → {}",
        pairs.len(),
        change(Some(tb), Some(ta)),
        change(Some(cb), Some(ca)),
        duration(Some(db)),
        duration(Some(da))
    );
}

/// Usage for a run from before soundcheck recorded it. The TSV has calls only as a total, so they go under `?`.
fn import_usage(run_dir: &Path, tsv: &Path) -> Result<()> {
    if let Ok(url) = server_url(run_dir)
        && tokio::runtime::Runtime::new()?
            .block_on(reqwest::get(&url))
            .is_ok()
    {
        bail!(
            "the run is being served at {url}: stop it first, since the server writes the journal"
        );
    }

    let path = run_dir.join(soundcheck::run::saved::JOURNAL_FILE);
    let (mut journal, entries) = soundcheck::run::journal::Journal::open(&path)?;
    let has_usage = soundcheck::run::usage::attempts(&entries);
    let dispatched: std::collections::BTreeSet<String> = entries
        .iter()
        .filter_map(|e| match &e.event {
            Event::Dispatched { token, .. } => Some(token.clone()),
            _ => None,
        })
        .collect();
    let text =
        std::fs::read_to_string(tsv).with_context(|| format!("can't read {}", tsv.display()))?;
    let mut events = Vec::new();
    for (n, line) in text.lines().enumerate().skip(1) {
        if line.trim().is_empty() {
            continue;
        }

        let cols: Vec<&str> = line.split('\t').collect();
        let [token, tokens, calls, duration_ms] = cols[..] else {
            bail!("{}:{}: expected 4 columns", tsv.display(), n + 1);
        };
        if !dispatched.contains(token) {
            bail!(
                "{}:{}: no attempt {token} in this run",
                tsv.display(),
                n + 1
            );
        }

        if has_usage.iter().any(|a| a.token == token) {
            bail!("{token} already has usage recorded");
        }

        let parse = |s: &str| {
            s.trim()
                .parse::<u64>()
                .with_context(|| format!("{}:{}: not a number: {s}", tsv.display(), n + 1))
        };
        events.push(Event::Usage {
            token: token.into(),
            calls: [("?".to_string(), parse(calls)? as u32)].into(),
            duration_ms: parse(duration_ms)?,
        });
        events.push(Event::Tokens {
            token: token.into(),
            tokens: parse(tokens)?,
            spent: None,
        });
    }

    let now = serve::now_ms();
    let count = events.len() / 2;
    for event in events {
        journal.append(now, event)?;
    }

    println!("added usage for {count} attempts");
    Ok(())
}

fn server_url(run_dir: &Path) -> Result<String> {
    let info: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(run_dir.join("server.json"))
            .context("no server.json: is the run started?")?,
    )?;
    Ok(info["url"]
        .as_str()
        .context("server.json: url")?
        .to_string())
}

fn ready_command(run_dir: &Path, platform: Platform, wait: bool) -> Result<()> {
    let url = format!("{}/ready/{platform}", server_url(run_dir)?);
    let started = std::time::Instant::now();
    let mut held = None;
    tokio::runtime::Runtime::new()?.block_on(async {
        let client = reqwest::Client::new();
        loop {
            // A stopped server is an error, so a background wait never outlives its run.
            let response = client.get(&url).send().await?;
            let status = response.status();
            let text = response.text().await?.trim().to_string();
            if status.is_success() {
                match held {
                    Some(why) => println!(
                        "{text} after {} s (held: {why})",
                        started.elapsed().as_secs()
                    ),
                    None => println!("{text}"),
                }
                return Ok(());
            }

            if status != reqwest::StatusCode::CONFLICT {
                bail!("{url}: {status} {text}");
            }

            if !wait {
                bail!("{platform} held: {text}");
            }

            held = Some(text);
            tokio::time::sleep(std::time::Duration::from_millis(serve::READY_POLL_MS)).await;
        }
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
