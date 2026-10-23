---
name: drive
description: "Drive the app on the Android emulator or iOS simulator by hand, outside a run: look at a screen, tap through a flow, reproduce a bug, check a change works on a device, read a screen's accessibility labels, or set up a screenshot. Use when asked to look at, try, open, check or screenshot something in the app on a device. Not for running specs: that's the project's regression skill and the run skill."
allowed-tools:
  - Bash(soundcheck drive:*)
  - Read
---

# Drive a device

`soundcheck drive <android|ios> <command>` runs one driver command against the emulator or simulator, the same commands testers use in a run. Each call is quick: the screen readers stay up on the device between calls.

## Before you start

- **Run from the suite folder** (the one with `suite.yaml`), or pass `--suite <folder>`: it names the app.
- **Never drive a device while a run is going.** A run's testers are using it, and your taps would steer their results. Check with the person, or look for a `soundcheck run` process.
- **Starting data** comes from the project's seed hook (`hooks.seed` in `suite.yaml`). The project's regression skill says how to run it by hand, and which scenarios exist.

## The loop

1. `see` first: what's on screen, as a screen reader gets it, one element a line with its tap point. `see --all` adds elements labelled only by an id.
2. Act on an element by its label exactly as `see` prints it (`prefix:` and `re:` match part of it). Use `at:x,y` only when there's no label, and say so to the person: a control with no label is worth knowing about.
3. `see` again to check what happened. After anything odd, `errors` lists crashes and app errors since the last launch.

## Commands

| Job | Commands |
|---|---|
| Look | `see [target] [--all]`, `wait <target> [ms]`, `expect <target>`, `gone <target>`, `shot <file>.png`, `errors` |
| Tap and type | `tap <target> [--last]`, `tap-at <x> <y>`, `fill <target> <text>` (empties the field, types, checks it landed), `type <text>` (appends to the focused field, and says if keys were dropped), `clear` |
| Gestures | `hold <target> <ms>` (a long press), `touch <stops…>` (one finger along a path: `touch Bread +700 at:980,420` is a drag; `--keep` holds it down, `release --back` lifts it where it began), `swipe <x1> <y1> <x2> <y2> [ms]` (lifts while moving, for scrolling and flings), `press <key>` (`Enter`, `Escape`, `Home`…) |
| App | `launch` (restarts it and checks it's in front), `push <file> [name]` (onto the device's Downloads or On My iPad, for file pickers) |
| Environment | `network offline\|online`, `network-drop <seconds>` (both through the suite's proxy), `keyboard hardware\|software` (iOS: whether fields raise the on-screen keyboard) |

## Rules

- **A person's pace.** Let the screen respond before the next action, the way a person would. A tap that lands mid-animation and is lost isn't a bug.
- **Report what you saw**, with the `see` lines or a `shot`, not what you expected to see.
- **Web isn't driven this way:** there's no browser to keep between calls. Use a browser tool, or a run.
- **iOS reads only the suite's app**, never a system app such as Settings.
