use clap::{Parser, Subcommand};
use soundcheck::{VERSION, lint, suite};
use std::path::PathBuf;
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
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Lint { suite: root } => {
            let suite = suite::load(&root);
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
    }
}
