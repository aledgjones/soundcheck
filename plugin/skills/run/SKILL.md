---
name: run
description: "Coordinate a soundcheck regression run: start the server, dispatch one background tester per spec, review results as they arrive, pause or resume on request, finish, then triage the findings with the person. Use when a project's regression skill hands over a run (a suite folder, platforms and a scope), or when asked to run, resume or triage a soundcheck run."
allowed-tools:
  - Bash(soundcheck run:*)
  - Bash(soundcheck events:*)
  - Bash(soundcheck lint:*)
  - Monitor
  - Agent
  - Read
  - mcp__plugin_soundcheck_soundcheck__status
  - mcp__plugin_soundcheck_soundcheck__dispatch
  - mcp__plugin_soundcheck_soundcheck__review
  - mcp__plugin_soundcheck_soundcheck__review_find
  - mcp__plugin_soundcheck_soundcheck__triage
  - mcp__plugin_soundcheck_soundcheck__install_build
  - mcp__plugin_soundcheck_soundcheck__pause
  - mcp__plugin_soundcheck_soundcheck__resume
  - mcp__plugin_soundcheck_soundcheck__finish
  - mcp__plugin_soundcheck_soundcheck__export
---

# Run a suite

You are the coordinator. The run should go from start to finish without the person: you dispatch testers, judge each result as it lands, and present the findings at the end. The server holds all state, so nothing lives only in your memory and a restarted session carries on from the journal.

You never drive a device, edit a result or message a tester directly: everything goes through the server, so it's all in the journal.

## Inputs

The project's regression skill gives you:

- the **suite folder** (the one with `suite.yaml`);
- the **platforms** to run;
- the **scope**: every spec, or a list of spec IDs;
- how to create tickets in the project's tracker, for triage.

## 1. Start

1. `soundcheck lint <suite>`: it must be clean. If it isn't, stop and report; never edit specs mid-run.
2. Start the server in the background: `soundcheck run <suite> --platforms <list> --scope <features or spec IDs> --label "<release under test>"` (leave out `--scope` for every spec), or `--resume <run folder>` to carry on an earlier run. Wait for its `MCP at http://127.0.0.1:7357/mcp` line; it also prints the run folder. Only specs in scope become rows, one per platform the spec runs on, so the verdict covers exactly the scope.
3. If the `mcp__plugin_soundcheck_soundcheck__*` tools aren't available, this session connected before the server was up: ask the person to reconnect soundcheck with `/mcp`, then carry on.
4. Run `soundcheck events <run folder>` under a Monitor. It follows the run until the server stops; each line is one event, and each wakes you. A Monitor lasts at most 30 minutes: re-arm it with `--after <last sequence number you saw>`, so nothing is missed. `--once` prints what has happened so far and exits.

## 2. Dispatch

- Call `dispatch` with the next spec in scope and its platform. It returns the tester's `brief`, a `token` and the spec's `model`.
- Start a **background** Agent with `subagent_type: soundcheck-tester`, the `model` if one is given, and the `brief` as its prompt, **unchanged**, and the token as its description.
- One spec runs at a time unless the suite says `parallel: true`. The server refuses a second dispatch and says why; that's your cue to wait for the next event.
- When a tester posts (`posted … filed` or `needs review`) or stalls (`stalled … queued again`), dispatch the next spec straight away, **before** reviewing anything. Reviews never hold up testing.
- Queued rows (send-backs, verifications, stalls) go before untested specs.

## 3. Review

A `needs review` event means a tester claimed FAIL, BLOCKED or Unsure. Call `status` for the row's judgments, then decide, with one plain line of reason:

| Call             | When                                                                                                                                                                                                                                                                                                                                                                                                                             |
| ---------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **uphold**       | The evidence is conclusive. A FAIL goes into Problems as a **failure**: give it a `title` in plain words. A BLOCKED claim needs its `cause`: `app` if the app stopped the test (a crash, a screen that won't load): it's filed FAIL, with a failure's `title`; `harness` if the environment or harness did (a missing label, a tool that can't reach a control, a stuck device): it's filed BLOCKED, which only means not tested |
| **verify**       | Plausible but not conclusive: seen once, on a degraded device, or hard to believe. Say exactly what to reproduce                                                                                                                                                                                                                                                                                                                 |
| **send-back**    | The tester erred: a wrong tap, a misread screen, a step from the wrong place                                                                                                                                                                                                                                                                                                                                                     |
| **judge**        | An Unsure you can settle from the evidence: PASS, or FAIL with the problem's `title`                                                                                                                                                                                                                                                                                                                                             |
| **spec-unclear** | An Unsure still unclear after verifying: the outcome is worded too vaguely. The spec is flagged for rewording                                                                                                                                                                                                                                                                                                                    |

- **Titles are what the person reads first.** Write each as the problem in plain words ("Undo leaves the stroke on screen when zoomed in"), never as the outcome or the tester's wording.
- **Failure or finding is never your judgement call.** Upheld failures are failures; side finds and things that didn't reproduce are findings. If a failure doesn't seem worth blocking the release, that's for triage with the person, not a reason to file it as something milder.
- **A degraded device** (`degraded` on the row in `status`, and a `DEGRADED` event): the server samples the device's health after every result and restarts a degraded one before the next spec. A failure seen on a degraded device can't be upheld until it reproduces: call **verify**, and the rerun runs on the restarted device. A BLOCKED with cause `harness` can still be upheld.
- **Look at the evidence, not just the claim.** Open a screenshot only when the judgments don't settle it, which keeps your context small for a long run.
- **Side finds** (`side find(s) need review`): `review_find` with a `title` if it's worth the person's attention (it becomes a finding), or none if it isn't a problem.
- A spec gets at most 3 attempts. After that, file what's known.

## 4. Pause, resume, stop

When the person asks you to stop or pause (a big bug they want fixed first), call `pause` with their reason. Running testers finish, and nothing new starts. `resume` carries on from the next spec. There's no other special case for serious bugs.

## 5. A fix build mid-run

When the person has a fix built and installed on a platform, the run carries on rather than starting again:

1. Read the fix's diff and propose which rows it could affect, each with a one-line reason. Show the list to the person and adjust it to what they say.
2. Call `install_build` between specs (it refuses while anything on that platform is running or waiting for review) with the build's commit, fingerprint and version, and the agreed reruns. A new fingerprint reruns everything on the platform, and rows with open problems always rerun, so a fix is proven on the build that has it.
3. Carry on dispatching: the reruns are queued and go first.

## 6. Finish

When every row in scope has a result and nothing is running, queued or waiting for review, call `finish`. It refuses, and names what's in the way, if anything is still open. Rows never dispatched don't block it, but they leave the verdict Not ready.

Finishing writes the final report (`report.html` in the run folder, screenshots embedded, so it opens anywhere offline). Call `export` again after triage, so the kept copy shows the decisions; `export` also works mid-run, stamped in progress.

## 7. Triage, with the person

Go through the open problems with the person, usually at the end, one at a time: what failed (the outcome, expected versus observed), how sure you are (verified or a single attempt), and your suggestion. They decide; you record it with `triage`:

- **dismiss:** not a bug, or not reproduced;
- **fix:** create a ticket in the project's tracker. It can only be recorded once the row has passed again on a build with the fix;
- **defer:** create a ticket; it's accepted for this release;
- **duplicate:** of an earlier problem, which it then follows.

The verdict (`status`) is Ready only when nothing is open. Export once triage is done. Report it to the person in one line, with its reasons if it isn't Ready.
