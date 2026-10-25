---
name: create-test
description: "Add or change soundcheck specs: the test cases in a project's suite (suite.yaml, <feature>/NNN-name.spec.md, _feature.md). Use when: writing a new test, turning a found bug into a regression spec, rewording a spec flagged 'spec unclear', adding a seed scenario a spec needs, renaming or moving a spec, or fixing `soundcheck lint` problems."
---

# Create or change a test

A spec is one thing a user does, written so an AI tester can drive the app, judge each outcome on its own, and probe around it. A soundcheck tester only sees the spec, its feature's `_feature.md` and the screen; it never reads the app's source. Write for that reader.

## Before writing

1. **Find the suite:** the folder with `suite.yaml`. The project's own regression skill says where it is.
2. **Read the feature's `_feature.md`** (how to get there, label hints, platform differences) and the specs already in that folder.
3. **Search for duplicates.** Grep the suite's titles and outcomes for the behaviour you're about to test. If a spec already covers the goal, extend its outcomes or its probe instead of adding a new spec: nothing is checked twice.

## The file

`<feature>/NNN-name.spec.md`, where `NNN` is the next free number in that feature folder, in run order, and `name` is a few lower-case words joined by hyphens. The path is the spec's ID (`annotations/004-eraser`), so never reuse a number.

```markdown
---
title: Erase part of a stroke
seed: viewer
starts-on: [Annotate]
platforms: [android, ios, web]
past-bugs: [SA-614]
---

## Goal

Draw a stroke, then erase its middle with the eraser.

## Outcomes

- Only the erased part disappears; both ends stay.
- Undo brings the erased part back.

## Probe around it

Erase across two strokes at once; change the eraser width first.
```

### Frontmatter

| Key                 | Rule                                                                                                                                       |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `title`             | The user's goal in plain words, under 80 characters. No tap paths (`A > B`), code formatting, spec or ticket IDs, flags or tool names      |
| `seed`              | A scenario from `suite.yaml`'s `seeds`. Every spec starts from its own full seed                                                           |
| `starts-on`         | Labels that are on screen only once the seed and launch worked. A miss is a reset error, retried by the server, never the tester's problem |
| `platforms`         | Every platform the goal applies to, `web` included. Differences go in `_feature.md` hints, not separate specs                              |
| `model`             | Optional. Ask for `opus` only when judging needs it, e.g. visual checks; the suite's default is cheaper and faster                         |
| `tools`             | Optional driver tools beyond the basics: `double-tap` (a quick double tap), `network-drop` (the network dies quietly, then comes back), `offline` (airplane mode until brought back), `fault` (a service answers one request path with an error status until cleared) |
| `keyboard`          | `software` when an outcome is about the on-screen keyboard; otherwise a hardware keyboard is connected (iOS), so typing is fast            |
| `past-bugs`         | Tickets this spec guards                                                                                                                   |
| `proven-failing-on` | Builds where this spec caught a real bug. Only record what actually happened; never fill it in to look trustworthy                         |

### Sections

- **`## Goal`** (required): what the user sets out to do, in a sentence or two.
- **`## Outcomes`** (required): a `- ` list of what a user would notice. The tester judges each one separately as held, failed, unsure or couldn't check, so each must be **observable on its own**:
  - one thing per bullet;
  - concrete enough that two testers would agree ("Undo brings the erased part back", not "Undo works properly");
  - exact copy only for destructive or blocking messages (delete confirmations, storage full);
  - no layout details a design tweak would break: header text, dividers, button order.
- **`## Probe around it`** (optional): variations to try near the goal. Agents find new bugs next to old ones, so every past bug gets one. Each must be something a person could realistically do: "tap it while the drawer is closing" asks for a tap no finger lands, and only produces findings that aren't bugs.
- **`## Pinned`** (optional): exact steps, only where they guard a past bug or reach a control an agent can't discover.

## Isolation

- A spec never relies on state another spec left behind, and never mentions another spec. A flow that builds on itself (create, rename, delete) is one spec.
- If no seed scenario gives the starting state, add one: extend the project's seed hook, add the name to `suite.yaml`'s `seeds`, and pick `starts-on` labels unique to it. Boundaries (storage exactly full, one byte over) are seed scenarios too.
- A seed can only reset what the project lets it: backend data, device state or both. Where it can't reset the data, the spec leaves the data as it found it: it only reads, or it undoes what it changed, and its outcomes say so.

## Controls without a label

Testers tap by what `see` reads. An app control with no label is an app bug: give it one in the app and name it in the hints. Only a control the app can't label (a system sheet's button, a backdrop strip) gets a point, written as a target the tester can use: `at:90,69` (points on iOS, pixels on Android), with what's there so a screenshot confirms it.

## Turning a found bug into a regression spec

Most specs start this way.

1. Search for the spec that covers the feature. If its goal is the same, add an outcome and a probe there. Otherwise, write a new spec placed right after the one it belongs with.
2. Make it more specific than a general spec: say in the Goal or Pinned how the bug was triggered, and in the Probe what to try around it.
3. Add the ticket to `past-bugs`.
4. If the trigger is mechanical (a double-tap, which one tap call at a time is too slow to make, or a dropped network), name the driver tool in `tools` and say how to use it ("double-tap the page with `double-tap`"). Taps keep a person's timing; a race that needs a faster or shorter touch is outside what a regression suite catches. Another quick gesture gets its own named tool, never a burst with a count. The tool's output is evidence; the tester's judgement of the whole spec is the result.
5. Record `proven-failing-on` only after the spec has failed on the bug build.

## Offline and network specs

`offline` and `network-drop` cut the app's own services through the suite's network proxy (`network` in `suite.yaml`); lint refuses them on a device without it. They don't cut what the device reaches on the internet itself, such as a store: a spec about the store being unreachable needs the device's own airplane mode, which only Android has, so it's Android-only. In the spec, say when to go offline and when to come back; the run brings the network back after the spec anyway. Pick `offline` for "no connection" (the app should say so at once) and `network-drop` for a connection that dies quietly in the background.

`fault <path> <status>` has one of those services refuse one kind of request while everything else works, as when the server fails or turns a request down: `fault /write 503`, or `fault /files/*/upload-url 403` (`*` is any one path segment; the query string doesn't count). It stays until the tester clears it, and the run clears it after the spec. Name the request by its path in the spec's steps, which is the one place a spec mentions one: the path is the project's, and the tester can't find it any other way. Faults need the proxy too, so they're Android and iOS only.

## Rewording a spec flagged "spec unclear"

The run's notes name the spec and the outcome that two testers couldn't judge. Rewrite that outcome so it's observable: name what's on screen when it holds, or split it into two outcomes. Don't loosen it until it can't fail.

## Renaming or moving

Give the spec its new path; the path is its ID, so a renamed spec starts a fresh history.

## Finish

Run `soundcheck lint <suite folder>` and fix every problem it reports. Lint checks the config, file names, frontmatter, sections, seeds, platforms, tools, and title rules. A run refuses to start on a suite that doesn't lint clean.

Change only the suite (and the seed hook, for a new scenario). That lint is soundcheck's, not the project's: never run the project's linters or formatters across the repo, and if the project formats files, format only the ones you wrote.
