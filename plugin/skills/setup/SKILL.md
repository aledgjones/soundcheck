---
name: setup
description: "Set a project up for soundcheck, or bring an existing suite up to a newer soundcheck version. Use when: adopting soundcheck in a project, creating a suite.yaml, writing a project's hook scripts (guard, build identity, install, check) and prepare scripts (seeding, signing in), drafting the project's regression skill, or when lint says a suite is pinned to an older soundcheck."
---

# Set up soundcheck

A project brings three things: a **suite** (specs, `suite.yaml`, hook and prepare scripts), a **regression skill** of its own (when and how to run, accounts, device quirks), and a **permission rule** so unattended runs never stop at a prompt. Soundcheck brings everything else. This skill sets those three up, or migrates them.

**Only touch soundcheck's own files:** the suite folder, the project's regression skill, and the permission rules in `.claude/settings.json`. Reading the app's code to learn its screens is fine; changing it isn't. "Lint" in these skills means `soundcheck lint`: never run the project's own linters or formatters across the repo, and if the project formats files, format only the ones you wrote.

## Is it a new project or a migration?

- No `suite.yaml` yet: set up (below).
- `soundcheck lint` says the suite is pinned to an older version: migrate (at the end).

## 1. Interview

Ask the person, one topic at a time, and don't guess. Write the answers down; they become the config and the project skill.

1. **Platforms:** which ones, and what to call each (`ios`, `iphone`, `android`, `chrome`…)? Specs use these names. For each, its driver (`ios`, `android` or `chrome`), the app or bundle ID; for chrome, the local URL, the command that serves it and the window size.
2. **Devices:** emulators, simulators and local browsers only. Which simulator (its name in Xcode) or AVD for each platform? Two platforms on one driver need a device each; one that isn't running is booted.
3. **Sign-in:** does the app sign in at all, and how (email and password, a one-time code, a social or platform account, a dev shortcut such as a deep link with a token)? Which test account does each platform use? Each needs its own, so platforms can run in parallel. Every spec starts on a fresh relaunch, so signing in is a script a spec's `prepare` runs: ask how a script could put a session in place with nobody there (a deep link with a token minted against the local stack, a cookie on chrome). **Check that it works with nobody there:** no verification code or link sent to a person, no captcha, no account picker or device-policy prompt. A sign-in that needs a human ends an overnight run. A shortcut like that must never ship in a release: ask how the project keeps it out.
4. **Safety:** how to tell the local stack from production, e.g. "every service URL is localhost". That becomes the guard hook, which runs before every reset.
5. **Starting state:** what a spec needs in place before it starts: backend data (wipe the test account's data and load a scenario), device state (permissions, files, settings), who is signed in and which screen it opens on. Each is a script a spec's `prepare` runs, so ask which states specs need (an empty account, one item, storage almost full…) and how a script would make each. Some projects can't touch their data: then the scripts prepare only the device, and specs leave the data as they found it.
6. **Build identity:** how to tell which build is installed: commit, native fingerprint (if the platform has one), version.
7. **Installing a build** on each device, if soundcheck should do it.
8. **Data checks** testers may need, e.g. "what's in the database for this account".
9. **Network:** will specs take the app offline or drop its network? On a device that goes through soundcheck's proxy, so list each local service the app reaches (API, sync, file storage…) with its real port, and pick a port for the proxy to listen on. The project's test builds must point the app at the listen ports, including any URL the backend hands out (signed file URLs). The device's store or other internet services stay outside it. Skip it for a suite with no offline specs.
10. **Findings:** "How do you want to track findings?" In a tracker (Jira, Linear, GitHub Issues…): its link format goes in `tickets.url`, and how to search and create tickets goes in the regression skill. In the report only: leave `tickets` out, and each triage decision's reason is the record.
11. **Where things live:** the suite folder (usually the project skill's own folder, with the specs in `suite/`) and where runs are written.

## 2. Write the suite

```
<project skill folder>/
  SKILL.md
  suite.yaml
  hooks/          # one self-contained script per concern; no reaching into other folders
  scripts/        # what specs' prepare steps run: seeding, signing in
  fixtures/
  suite/          # only the specs
    <feature>/
      _feature.md
      001-<name>.spec.md
```

`suite.yaml`:

Each platform is a whole block: nothing is shared from the top level, so two platforms repeat what they have in common, and each command says its platform itself.

```yaml
soundcheck: "0.3" # the version this suite is written for
platforms:
  android:
    driver: android
    device: Pixel_Tablet_API_35 # the AVD
    app: com.example.app
    hooks:
      guard: node hooks/guard.mjs --platform android
      build-identity: node hooks/build.mjs identity android
      install: node hooks/build.mjs install android
      check: node hooks/check.mjs --platform android
    proxy: # only for offline specs: the test build reaches `listen`, `upstream` really serves it
      - { name: api, listen: 33000, device: 23000, upstream: 3000 } # `device`: the port the build uses on Android
  ios:
    driver: ios
    device: iPad Pro 13-inch (M5) # the simulator's name, or its UDID
    app: com.example.app
    hooks:
      guard: node hooks/guard.mjs --platform ios
      build-identity: node hooks/build.mjs identity ios
      install: node hooks/build.mjs install ios
      check: node hooks/check.mjs --platform ios
    proxy:
      - { name: api, listen: 23000, upstream: 3000 }
      - { name: store, listen: 28100, upstream: https://api.revenuecat.com } # a remote service, over TLS
  chrome:
    driver: chrome
    device: desktop
    url: http://localhost:5173/app
    viewport: 1440x900
    hooks:
      guard: node hooks/guard.mjs --platform chrome
      build-identity: node hooks/build.mjs identity chrome
      install: node hooks/build.mjs install chrome
      serve: node hooks/build.mjs serve chrome # keeps the site at `url` up for the run
      check: node hooks/check.mjs --platform chrome
specs: suite
reports: ../../../reports # relative to suite.yaml; runs are written here
tickets: # only with a tracker; fixed and deferred problems then need its ticket
  url: https://example.atlassian.net/browse/{id}
```

A spec readies itself in its frontmatter, with commands run from this folder after a fresh relaunch (DESIGN.md 5.6):

```yaml
prepare:
  - node scripts/seed.js one-score # prints {"values": {"SCORE": "…"}, "context": {...}}
  - node scripts/sign-in.js
  - ios: soundcheck drive ios open "myapp://score/$SCORE"
    android: soundcheck drive android open "myapp://score/$SCORE"
    chrome: soundcheck drive chrome open "/app/score/$SCORE"
```

### Hooks

Shell commands, run from the folder holding suite.yaml, in any language. Exit 0 for ok; anything else is a refusal, with the reason on stderr. Where data comes back, it's JSON on stdout.

| Hook                  | Called                                                    | Prints                                                      |
| --------------------- | --------------------------------------------------------- | ----------------------------------------------------------- |
| `guard`               | At run start, and before every reset                      | nothing; exit 1 with a reason to refuse                     |
| `build-identity`      | At run start and after each install; within 60 s          | `{"commit": "...", "fingerprint": "...", "version": "..."}` |
| `install`             | When the coordinator installs a build                     | nothing                                                     |
| `serve`               | Optional: run start and after every install (a web build) | nothing; keeps running until the server stops               |
| `check {name} [args]` | When a tester asks for a data check                       | the data, as JSON                                           |
| `stack-up`            | Optional, with `stack-down`: run start, before the guard  | nothing; exit once the platform's blank backend answers     |
| `stack-down`          | When the run's server stops, or after one that died       | nothing; exit 0 when there's nothing to remove too          |

- **Every hook is its platform's own.** There are no placeholders but a check's name and args: each command names its platform, stack and account itself. Hook scripts can still be one shared file that takes the platform as an argument.
- **Stacks make platforms independent.** With `stack-up` and `stack-down`, each platform gets its own blank backend per run (containers on their own ports, under a project name per platform) and shares no account or data with the others. Point each platform's `guard`, `check` and `proxy` at its own stack. Make `up` start from nothing every time and `down` remove everything, data included, and give the containers no restart policy, so nothing comes back after a crash.
- **The guard is the project's safety net.** Make it refuse unless everything points at the local stack, and never weaken it to make a run start.

### Prepare scripts

Hooks answer "how does the app get running?"; a spec's `prepare` answers "how is this spec readied?". soundcheck runs each spec's steps after the guard and a fresh relaunch, in order, from the suite folder. Seeding, signing in and where to start are the project's scripts; soundcheck has no seed, session or start link of its own.

- **A step gets** `SOUNDCHECK_PLATFORM`, `SOUNDCHECK_DRIVER`, `SOUNDCHECK_DEVICE`, `SOUNDCHECK_SERVER` (so `soundcheck drive` acts on the run's device) and every value earlier steps printed.
- **A step may print** `{"values": {"NAME": "…"}, "context": {...}}`: each value becomes an environment variable for the steps after it, and `context` goes into the tester's brief. The journal records the commands as written, so a token passed as a value never reaches a report.
- **Steps reach the device through `soundcheck drive <platform> <tool>`**: `open <link>`, `relaunch`, `clear-data`, and on chrome `cookies set` and `storage set`. Each has the same effect on every driver; a browser-only tool fails elsewhere, saying why.
- **A step that fails or passes `--prepare-timeout`** (60 s by default) fails the reset, which is retried twice and then filed BLOCKED. A seed that's slower raises the flag for the run, not per step.
- **Keep scripts readable from the spec.** A pattern that repeats becomes a script, not an alias: every step a spec takes stays visible in its frontmatter.

Write stubs first that exit 1 with "not written yet", then fill them in with the person, testing each on its own (`soundcheck drive` for device steps) before the first run.

## 3. Draft the project's regression skill

It holds what's specific to this project, never soundcheck's generic steps:

- when the project runs a regression, and with what scope. A release process usually chooses the scope and calls this skill;
- the accounts, and anything about the devices no script or `device:` covers;
- known device quirks and their workarounds;
- with a tracker, how to search and create tickets during triage;
- then: hand the suite folder, platforms and scope to `soundcheck:run`.

## 4. Allow soundcheck's tools

Add these rules to the project's `.claude/settings.json` under `permissions.allow`, so background testers never stop at a permission prompt and auto mode doesn't block the coordinator's commands as self-modification:

```json
"mcp__plugin_soundcheck_soundcheck__*",
"Bash(soundcheck lint:*)",
"Bash(soundcheck run:*)",
"Bash(soundcheck events:*)",
"Bash(soundcheck ready:*)",
"Bash(soundcheck serve:*)",
"Bash(soundcheck drive:*)"
```

They're safe because the tester agent's only tools are soundcheck's own and Read, and each command only reads the suite, drives the test devices or writes the suite's reports. Keep any rules already there, and show the person the change before writing it.

## 5. First spec and lint

Write one real spec with `create-test`, then run `soundcheck lint <suite>`. It must be clean before any run.

## Migrating a suite to a newer soundcheck

1. Read the suite's pin (`soundcheck:` in `suite.yaml`) and the installed version (`soundcheck --version`).
2. Read the plugin's `MIGRATIONS.md` and apply every entry newer than the pin, oldest first.
3. Whatever the pin, check that the project's `.claude/settings.json` has every rule in section 4, and add any missing.
4. Bump the pin, run `soundcheck lint`, and show the person the whole diff for review. Never migrate mid-release: results must stay comparable within a release.
