# Suite migrations

What changes in a suite between soundcheck versions, so the setup skill can bring a pinned suite up to date. Each entry
names the version it migrates to, what to change, and how to check it. The newest is first.

## 0.3

Platforms describe themselves, and each spec readies itself with `prepare` steps. soundcheck reads no 0.2 shape:
`soundcheck lint` names every old key, so work through its list. DESIGN.md 5.2–5.6 has the whole model.

Before, 0.2:

```yaml
# suite.yaml
soundcheck: "0.2"
platforms:
  ios:
    app-id: com.example.app
    account: ios-tester@example.com
    ready: [Library]
hooks:
  guard: node hooks/guard.mjs --platform {platform}
  seed: node hooks/seed.mjs {scenario}
  build-identity: node hooks/build.mjs identity {platform}
  install: node hooks/build.mjs install {platform}
  check: node hooks/check.mjs
seeds: [library]

# a spec's frontmatter
seed: library
platforms: [ios]
start:
  ios: myapp://collection/{Library Collection}
```

After, 0.3:

```yaml
# suite.yaml
soundcheck: "0.3"
platforms:
  ios:
    driver: ios
    device: iPad Pro 13-inch (M5)
    app: com.example.app
    hooks:
      guard: node hooks/guard.mjs --platform ios
      build-identity: node hooks/build.mjs identity ios
      install: node hooks/build.mjs install ios
      check: node hooks/check.mjs

# a spec's frontmatter
platforms: [ios]
prepare:
  - node scripts/seed.js library # was the seed hook; prints {"values": {"COLLECTION": "…"}}
  - node scripts/sign-in.js --account ios-tester@example.com # what `account` and `ready` stood for
  - soundcheck drive ios open "myapp://collection/$COLLECTION"
```

- **Each platform is a whole block.** Nothing is shared from the top level, and no hook is filled in by a placeholder:
  - add `driver` (`ios`, `android` or `chrome`) and `device`: a simulator's name (or UDID, where two runtimes share a
    name), an AVD name, or `desktop` for chrome. Platform names are the project's own: `ios`, `iphone`, `chrome`…
    A `web` platform can keep its name, but naming it `chrome` reads better beside the driver;
  - rename `app-id` to `app`, `base-url` to `url`, `profile` to `chrome-profile`, and the platform's `start` hook to
    `serve`;
  - move the top-level `hooks` into every platform's block, writing out what `{platform}` and `{account}` stood for in
    each command (`node hooks/data.mjs guard --platform ios`);
  - move `network.proxy` into each platform as its `proxy` list, same routes;
  - delete `account`, `ready`, `prepare` (top-level and per platform, and its `prepare.md`), `seeds` and the `seed`
    hook. Signing in and seeding become scripts a spec runs (below).
- **Specs:** `seed`, `start` and `starts-on` go; a spec lists `prepare` steps, commands run from the suite folder after
  a fresh relaunch, in order (DESIGN.md 5.6):
  - the seed hook becomes a script (`node scripts/seed.js <scenario>`). Where it printed `{"ids": {...}}` for a start
    link, it prints `{"values": {"SCORE": "…"}}`: each value is an environment variable for the steps after it.
    `context` works as before;
  - a `start` link becomes a step per platform, `soundcheck drive <platform> open <link>` (a chrome path is on the
    platform's `url`), with `$SCORE` where the link had `{name}`;
  - signing in, if a spec needs it, is a script step too; where `ready` meant "signed in", the spec now says so in
    `prepare`;
  - a map step names every platform the spec lists, `true` where it does nothing; a plain string runs on all of them,
    so it drives `$SOUNDCHECK_PLATFORM`, never a platform's name.
- **Tools:** `launch` is now `relaunch`, in specs' `## Pinned` steps, `_feature.md` notes and the project's skills.
  `soundcheck drive` adds `open`, `clear-data`, and on chrome `cookies set` and `storage set`.
- **The environment:** `SOUNDCHECK_IOS_UDID` and `ANDROID_SERIAL` are gone; `device` picks the device, and one that
  isn't running is booted. A slow seed raises `soundcheck run --prepare-timeout` (60 s a step by default).
- **The project's skills:** drop instructions for preparing a device or signing in by hand before a run, now `prepare`
  steps, and point any run that drives devices outside soundcheck at the same `device` names.
- **Check:** `soundcheck lint` is clean, and `grep -rnE '^(seed|start|starts-on):|\blaunch\b|\{scenario\}'` in the
  suite finds nothing old.

## 0.2

`drag` is retired for `touch`, one finger along a path of stops, and the coordinator's `add_specs` becomes `queue`.

- **Specs and `_feature.md` notes:** a step that says to `drag` becomes a `touch` with two stops. A hold before moving
  is a `pause_ms` on the first stop (a reorder handle: about 700 ms). In `soundcheck drive`, `drag x1 y1 x2 y2 700`
  becomes `touch at:x1,y1 +700 at:x2,y2`, and `--keep` and `release` work as before, on Android and iOS.
- **The project's skills:** any mention of `add_specs` becomes `queue`, which also reruns specs already in the run
  (with a `reason`).
- **Probes:** a probe that asks for a step faster than a person acts ("tap it while the drawer is closing") is reworded
  or dropped: testers now keep a person's pace, and `batch` leaves 500 ms between inputs.
- **Check:** `grep -rnw drag` in the suite and the project's skills finds nothing that names the tool, and
  `soundcheck lint` is clean.
