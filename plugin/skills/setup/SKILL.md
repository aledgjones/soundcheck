---
name: setup
description: "Set a project up for soundcheck, or bring an existing suite up to a newer soundcheck version. Use when: adopting soundcheck in a project, creating a suite.yaml, writing a project's hook scripts (guard, seed, build identity, install, check), drafting the project's regression skill, or when lint says a suite is pinned to an older soundcheck."
---

# Set up soundcheck

A project brings three things: a **suite** (specs, `suite.yaml`, hook scripts), a **regression skill** of its own (when and how to run, accounts, device quirks), and a **permission rule** so unattended runs never stop at a prompt. Soundcheck brings everything else. This skill sets those three up, or migrates them.

## Is it a new project or a migration?

- No `suite.yaml` yet: set up (below).
- `soundcheck lint` says the suite is pinned to an older version: migrate (at the end).

## 1. Interview

Ask the person, one topic at a time, and don't guess. Write the answers down; they become the config and the project skill.

1. **Platforms:** Android, iOS, web? For each, the app or bundle ID; for web, the local URL and the command that serves it.
2. **Devices:** emulators, simulators and local browsers only. Which emulator or simulator model?
3. **Accounts:** the test account each platform signs in with. Each platform needs its own, so platforms could later run in parallel. **Check that each can sign in with nobody there:** no account picker, no device-policy prompt, no two-factor step. A sign-in that needs a human ends an overnight run.
4. **Safety:** how to tell the local stack from production, e.g. "every service URL is localhost". That becomes the guard hook, which runs before every seed.
5. **Test data:** how to wipe the test account and seed a named starting state locally. Which states do specs need (empty account, one score, storage almost full…)?
6. **Build identity:** how to tell which build is installed: commit, native fingerprint (if the platform has one), version.
7. **Installing a build** on each device, if soundcheck should do it.
8. **Data checks** testers may need, e.g. "what's in the database for this account".
9. **Network:** will specs take the app offline or drop its network? On a device that goes through soundcheck's proxy, so list each local service the app reaches (API, sync, file storage…) with its real port, and pick a port for the proxy to listen on. The project's test builds must point the app at the listen ports, including any URL the backend hands out (signed file URLs). The device's store or other internet services stay outside it. Skip it for a suite with no offline specs.
10. **Tickets:** the tracker and its link format, for triage.
11. **Where things live:** the suite folder (usually the project skill's own folder, with the specs in `suite/`) and where runs are written.

## 2. Write the suite

```
<project skill folder>/
  SKILL.md
  suite.yaml
  hooks/          # one self-contained script per concern; no reaching into other folders
  fixtures/
  suite/          # only the specs
    <feature>/
      _feature.md
      001-<name>.spec.md
```

`suite.yaml`:

```yaml
soundcheck: "0.1" # the version this suite is written for
platforms:
  android:
    app-id: com.example.app
    account: android-tester@example.com
    ready: [Home] # what shows once the app is signed in; the run checks it before starting
  ios:
    app-id: com.example.app
    account: ios-tester@example.com
hooks:
  guard: node hooks/guard.mjs
  seed: node hooks/seed.mjs {scenario}
  build-identity: node hooks/build-identity.mjs {platform}
  install: node hooks/install.mjs {platform}
  check: node hooks/check.mjs
seeds: [empty, one-score]
specs: suite
reports: ../../../reports # relative to suite.yaml; runs are written here
tickets:
  url: https://example.atlassian.net/browse/{id}
network: # only for offline specs: the test build reaches `listen`, `upstream` really serves it
  proxy:
    - { name: api, listen: 23000, upstream: 3000 }
```

### Hooks

Shell commands, run from the folder holding suite.yaml, in any language. Exit 0 for ok; anything else is a refusal, with the reason on stderr. Where data comes back, it's JSON on stdout.

| Hook                        | Called                                  | Prints                                                      |
| --------------------------- | --------------------------------------- | ----------------------------------------------------------- |
| `guard`                     | At run start, and before every seed     | nothing; exit 1 with a reason to refuse                     |
| `seed {scenario}`           | Before every spec: a full wipe and seed | optionally `{"context": {...}}` for the tester's brief      |
| `build-identity {platform}` | At run start and after each install     | `{"commit": "...", "fingerprint": "...", "version": "..."}` |
| `install {platform}`        | When the coordinator installs a build   | nothing                                                     |
| `check {name} [args]`       | When a tester asks for a data check     | the data, as JSON                                           |

- **The guard is the project's safety net.** Make it refuse unless everything points at the local stack, and never weaken it to make a run start.
- **A seed resets data only.** Soundcheck relaunches the app and checks the spec's `starts-on` labels itself.
- Write stubs first that exit 1 with "not written yet", then fill them in with the person, testing each on its own before the first run.

## 3. Draft the project's regression skill

It holds what's specific to this project, never soundcheck's generic steps:

- when the project runs a regression, and with what scope. A release process usually chooses the scope and calls this skill;
- the accounts and how each device is prepared (emulator model, simulator, sign-in);
- known device quirks and their workarounds;
- how to create tickets during triage;
- then: hand the suite folder, platforms and scope to `soundcheck:run`.

## 4. Allow soundcheck's tools

Add this rule to the project's `.claude/settings.json` under `permissions.allow`, so background testers never stop at a permission prompt:

```json
"mcp__plugin_soundcheck_soundcheck__*"
```

It's safe because the tester agent's only tools are soundcheck's own and Read. Show the person the change before writing it.

## 5. First spec and lint

Write one real spec with `create-test`, then run `soundcheck lint <suite>`. It must be clean before any run.

## Migrating a suite to a newer soundcheck

1. Read the suite's pin (`soundcheck:` in `suite.yaml`) and the installed version (`soundcheck --version`).
2. Read the plugin's `MIGRATIONS.md` and apply every entry newer than the pin, oldest first.
3. Bump the pin, run `soundcheck lint`, and show the person the whole diff for review. Never migrate mid-release: results must stay comparable within a release.
