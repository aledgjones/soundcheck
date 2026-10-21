# Soundcheck

Regression testing for apps, with AI agents as the testers. One local server drives Android, iOS and web, records every result in a journal, and shows the run live in your browser. A coordinator agent judges each result as it lands, so a run goes from start to finish without anyone watching it; you review the report at the end.

The design, and the reasons behind it, are in [DESIGN.md](DESIGN.md).

## What's built

- **Suites:** specs as Markdown files, `suite.yaml`, and `soundcheck lint`.
- **The run:** a journal and its row states, review, attempts, stalls, fix builds mid-run, and the verdict and triage.
- **The server:** MCP over local HTTP, the event stream, the live page, the runs index, and exports.
- **Hooks:** guard, seed, build identity, and the web app's start command.
- **Drivers:** the web driver (its own headless Chrome) with every tool, and evidence capture on failures. The Android and iOS drivers are still to come; until then their device tools say so.
- **The Claude Code plugin:** MCP config, the tester agent, and the `run`, `setup` and `create-test` skills.

## Install

You need a Rust toolchain, and Chrome for web suites.

```sh
cargo install --path crates/server     # puts `soundcheck` on your PATH
```

Then, in Claude Code:

```
/plugin marketplace add /path/to/soundcheck
/plugin install soundcheck@soundcheck
```

## Use

1. **Set a project up** with the plugin's `setup` skill. It writes `suite.yaml`, hook stubs, a draft regression skill for the project, and the permission rule that stops background testers prompting.
2. **Write specs** with `create-test`, then check them with `soundcheck lint <suite>`.
3. **Run:** the project's regression skill hands a scope to the plugin's `run` skill. That skill starts the server (`soundcheck run <suite> --label "v1.2.3 · candidate"`), which opens the live page and serves MCP at `http://127.0.0.1:7357/mcp`. If your Claude Code session started before the server, reconnect soundcheck with `/mcp`.

Runs are written beside the suite in `reports/<run>/`: the journal, a summary, evidence, and `report.html` once the run finishes. `/runs` on the server lists every run.

## Develop

```sh
cargo test                  # the web tests skip where Chrome isn't installed
cargo clippy --all-targets -- -D warnings
cargo fmt
```
