# Soundcheck

Regression testing for apps, with AI agents as the testers. One local server drives Android, iOS and web, records every result in a journal, and shows the run live in your browser. A coordinator agent judges each result as it lands, so a run goes from start to finish without anyone watching it; you review the report at the end.

The design, and the reasons behind it, are in [DESIGN.md](DESIGN.md).

## What's built

- **Suites:** specs as Markdown files, `suite.yaml`, and `soundcheck lint`.
- **The run:** a journal and its row states, review, attempts, stalls, fix builds mid-run, and the verdict and triage.
- **The server:** MCP over local HTTP, the event stream, the live page, the runs index, and exports.
- **Hooks:** guard, seed, build identity, and the web app's start command.
- **Drivers:** web (its own headless Chrome), Android (emulators, through a persistent reader APK, with adb for input) and iOS (simulators, through a persistent reader for reads and input), and evidence capture on failures.
- **The Claude Code plugin:** MCP config, the tester agent, and the `run`, `setup` and `create-test` skills.

## Install

You need what each platform drives: Chrome for web, the Android SDK (adb, an emulator) for Android, Xcode for iOS.

### Prebuilt (Apple Silicon Macs)

```sh
gh release download --repo aledgjones/soundcheck --pattern '*.tar.gz'
mkdir -p ~/soundcheck && tar -xzf soundcheck-*-macos-arm64.tar.gz -C ~/soundcheck
echo 'export PATH="$HOME/soundcheck/bin:$PATH"' >> ~/.zshrc   # then open a new terminal
```

A tarball downloaded through a browser is quarantined: `xattr -dr com.apple.quarantine ~/soundcheck` first.

### From source

Also needs a Rust toolchain, JDK 17+ and Android build-tools 35 for the Android reader, and [XcodeGen](https://github.com/yonaskolb/XcodeGen) (`brew install xcodegen`) for the iOS reader, whose build compiles idb's simulator frameworks from source (about a minute, once).

```sh
cargo install --path crates/server     # puts `soundcheck` on your PATH
readers/android/build.sh               # the Android reader APK
readers/ios/build.sh                   # the iOS reader
```

`scripts/release.sh` packages all of it into `target/package/`, and `scripts/release.sh --publish` uploads that as the GitHub release for the version in `Cargo.toml`.

### Then

The server finds the readers beside a prebuilt `bin/soundcheck`, in a checkout's `readers/<platform>/build/`, or where `SOUNDCHECK_ANDROID_READER` and `SOUNDCHECK_IOS_READER` point. It drives the one running emulator (`ANDROID_SERIAL` picks one) and the one booted simulator (`SOUNDCHECK_IOS_UDID` picks one).

In Claude Code, with the folder you unpacked or cloned:

```
/plugin marketplace add ~/soundcheck
/plugin install soundcheck@soundcheck
```

## Use

1. **Set a project up** with the plugin's `setup` skill. It writes `suite.yaml`, hook stubs, a draft regression skill for the project, and the permission rule that stops background testers prompting.
2. **Write specs** with `create-test`, then check them with `soundcheck lint <suite>`.
3. **Run:** the project's regression skill hands a scope to the plugin's `run` skill. That skill starts the server (`soundcheck run <suite> --scope <features or specs> --label "v1.2.3 · candidate"`), which opens the live page and serves MCP at `http://127.0.0.1:7357/mcp`. The plugin reaches it through `soundcheck mcp`, a stdio bridge, so a Claude Code session started before the server needs no reconnect.

To look at runs without one going, `soundcheck serve <suite> --open latest` opens the newest run's page, and the list of every run is at its root; nothing needs exporting first. An export (`report.html`, screenshots embedded) is for sharing.

Outside a run, `soundcheck drive <android|ios> <command>` runs one driver command (`see`, `tap`, `wait`, `shot`…) from the suite folder, for exploring, reproducing a bug or setting up screenshots. `soundcheck drive <platform> network offline|online` and `network-drop <seconds>` take the app's network away through the suite's proxy (`network` in `suite.yaml`), which runs detached once started; `soundcheck proxy status|stop` manage it.

Runs are written beside the suite in `reports/<run>/`: the journal, a summary, evidence, and `report.html` once the run finishes. `/runs` on the server lists every run.

## Develop

```sh
cargo test                  # device tests skip where Chrome, an emulator or a booted simulator isn't there
cargo clippy --all-targets -- -D warnings
cargo fmt
```
