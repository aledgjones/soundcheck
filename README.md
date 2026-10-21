# Soundcheck

Regression testing for apps, with AI agents as the testers. One local server drives Android, iOS and web, records every result in a journal, and shows the run live in your browser. A coordinator agent judges each result as it lands, so a run goes from start to finish without anyone watching it; you review the report at the end.

The design, and the reasons behind it, are in [DESIGN.md](DESIGN.md).

## What's built

- **Suites:** specs as Markdown files, `suite.yaml`, and `soundcheck lint`.
- **The run:** a journal and its row states, review, attempts, stalls, fix builds mid-run, and the verdict and triage.
- **The server:** MCP over local HTTP, the event stream, the live page, the runs index, and exports.
- **Hooks:** guard, seed, build identity, and the web app's start command.
- **Drivers:** web (its own headless Chrome), Android (emulators, through a persistent reader APK, with adb for input) and iOS (simulators, through a persistent reader, with AXe for input), and evidence capture on failures.
- **The Claude Code plugin:** MCP config, the tester agent, and the `run`, `setup` and `create-test` skills.

## Install

You need a Rust toolchain, plus what each platform drives: Chrome for web; the Android SDK (adb, build tools) for Android; Xcode and [AXe](https://github.com/cameroncooke/AXe) (`brew install cameroncooke/axe/axe`) for iOS.

```sh
cargo install --path crates/server     # puts `soundcheck` on your PATH
readers/android/build.sh               # the Android reader APK
readers/ios/build.sh                   # the iOS reader
```

The server finds the readers in `readers/<platform>/build/`, or where `SOUNDCHECK_ANDROID_READER` and `SOUNDCHECK_IOS_READER` point. It drives the one running emulator (`ANDROID_SERIAL` picks one) and the one booted simulator (`SOUNDCHECK_IOS_UDID` picks one).

Then, in Claude Code:

```
/plugin marketplace add /path/to/soundcheck
/plugin install soundcheck@soundcheck
```

## Use

1. **Set a project up** with the plugin's `setup` skill. It writes `suite.yaml`, hook stubs, a draft regression skill for the project, and the permission rule that stops background testers prompting.
2. **Write specs** with `create-test`, then check them with `soundcheck lint <suite>`.
3. **Run:** the project's regression skill hands a scope to the plugin's `run` skill. That skill starts the server (`soundcheck run <suite> --label "v1.2.3 · candidate"`), which opens the live page and serves MCP at `http://127.0.0.1:7357/mcp`. If your Claude Code session started before the server, reconnect soundcheck with `/mcp`.

Outside a run, `soundcheck drive <android|ios> <command>` runs one driver command (`see`, `tap`, `wait`, `shot`…) from the suite folder, for exploring, reproducing a bug or setting up screenshots.

Runs are written beside the suite in `reports/<run>/`: the journal, a summary, evidence, and `report.html` once the run finishes. `/runs` on the server lists every run.

## Develop

```sh
cargo test                  # device tests skip where Chrome, an emulator or a booted simulator isn't there
cargo clippy --all-targets -- -D warnings
cargo fmt
```
