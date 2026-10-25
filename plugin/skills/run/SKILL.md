---
name: run
description: "Coordinate a soundcheck regression run: start the server, dispatch one background tester per spec, review results as they arrive, pause or resume on request, finish, then triage the findings with the person. Use when a project's regression skill hands over a run (a suite folder, platforms and a scope), or when asked to run, resume or triage a soundcheck run."
allowed-tools:
  - Bash(soundcheck run:*)
  - Bash(soundcheck events:*)
  - Bash(soundcheck lint:*)
  - Bash(soundcheck drive:*)
  - Monitor
  - Agent
  - Read
  - mcp__plugin_soundcheck_soundcheck__status
  - mcp__plugin_soundcheck_soundcheck__dispatch
  - mcp__plugin_soundcheck_soundcheck__queue
  - mcp__plugin_soundcheck_soundcheck__review
  - mcp__plugin_soundcheck_soundcheck__review_find
  - mcp__plugin_soundcheck_soundcheck__triage
  - mcp__plugin_soundcheck_soundcheck__add_evidence
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
2. Start the server in the background: `soundcheck run <suite> --platforms <list> --scope <features or spec IDs> --label "<release under test>"` (leave out `--scope` for every spec), or `--resume <run folder>` to carry on an earlier run. Wait for its `MCP at http://127.0.0.1:7357/mcp` line; it also prints the run folder. Only specs in scope become rows, one per platform the spec runs on, so the verdict covers exactly the scope. A platform whose installed build can't be identified is built and installed first with the suite's install hook (several minutes; tell the person), and `--install` rebuilds every platform from the current tree. The server refuses to start if a build still can't be identified. Add `--headed` when the person wants to watch the run. On web, Chrome opens in a window instead of headless. On iOS, Simulator.app opens on the device being driven, and again after a restart. On Android, the emulator's window shows only when soundcheck restarts the emulator; one already running stays as it is, without a window. On every platform it's for looking only: tell them a click or resize in the window can steer a test.
3. If the `mcp__plugin_soundcheck_soundcheck__*` tools aren't available, the plugin's bridge (`soundcheck mcp`) didn't start: check `soundcheck` is on the PATH and ask the person to reconnect soundcheck with `/mcp`.
4. Run `soundcheck events <run folder> | grep --line-buffered -v ' judged outcome '` under a Monitor. It follows the run until the server stops; each line is one event, and each wakes you. Leave out the `judged outcome` lines: they're a tester's progress, for the page, and you review a result only once it's posted. A Monitor lasts at most 30 minutes: re-arm it with `--after <last sequence number you saw>`, so nothing is missed. `--once` prints what has happened so far and exits.

## 2. Dispatch

- Call `dispatch` with the next spec in scope and its platform. It returns a `token`, the spec's `model` and a short `prompt`. The brief itself isn't in it: the tester fetches it from the server with `brief`, so it never passes through your context.
- Start a **background** Agent with `subagent_type: soundcheck-tester`, the `model` if one is given, and the `prompt` as its prompt, **unchanged**, and the token as its description.
- A tester's final reply is one line (token, claim, side finds). Its judgments and evidence are on the server: read them with `status` when you review, not from the reply.
- One spec runs at a time unless the suite says `parallel: true`. The server refuses a second dispatch and says why; that's your cue to wait for the next event.
- A dispatch refused with `held:` means the platform is restarting or the Mac is busy. Wait for its `<platform> ready` event, then dispatch again.
- When a tester posts (`posted … filed` or `needs review`) or stalls (`stalled … queued again`), dispatch the next spec straight away, **before** reviewing anything. Reviews never hold up testing.
- Queued rows (send-backs, verifications, stalls) go before untested specs.
- A tester that never starts (the agent failed to launch, or its first call never comes) stalls after 10 minutes on its own. When you know it's gone, `queue` its spec with the reason instead of waiting: the attempt is abandoned and the row is free.
- A spec reworded mid-run (a vague outcome, a probe that asked for the impossible) takes effect when you `queue` it again: the server rereads it from disk, and the report notes the change beside its results.

## 3. Review

A `needs review` event means a tester claimed FAIL, BLOCKED or Unsure. Call `status` for the row's judgments, then decide, with one plain line of reason:

| Call             | When                                                                                                                                                                                                                                                                                                                                                                                                                             |
| ---------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **uphold**       | The evidence is conclusive. A FAIL goes into Problems as a **failure**: give it a `title` in plain words. A BLOCKED claim needs its `cause`: `app` if the app stopped the test (a crash, a screen that won't load): it's filed FAIL, with a failure's `title`; `harness` if the environment or harness did (a missing label, a tool that can't reach a control, a stuck device): it's filed BLOCKED, which only means not tested |
| **verify**       | Plausible but not conclusive: seen once, on a degraded device, or hard to believe. Say exactly what to reproduce                                                                                                                                                                                                                                                                                                                 |
| **send-back**    | The tester erred: a wrong tap, a misread screen, a step from the wrong place                                                                                                                                                                                                                                                                                                                                                     |
| **judge**        | An Unsure you can settle from the evidence: PASS, or FAIL with the problem's `title`                                                                                                                                                                                                                                                                                                                                             |
| **spec-unclear** | An Unsure still unclear after verifying: the outcome is worded too vaguely. The spec is flagged for rewording                                                                                                                                                                                                                                                                                                                    |
| **harness**      | The harness, the environment or the spec's wording stopped the check, whatever the claim (a tool that can't reach a control, a hint that's wrong, a state an earlier attempt left). Without `rerun` it's filed BLOCKED, not tested, with a harness-gap note. Once the gap is fixed, give `rerun` a note for the tester instead: a fresh attempt is queued with it, and a pass files nothing                                         |

- **Titles are what the person reads first.** Write each as the problem in plain words ("Undo leaves the stroke on screen when zoomed in"), never as the outcome or the tester's wording.
- **Failure or finding is never your judgement call.** Upheld failures are failures; side finds and things that didn't reproduce are findings. If a failure doesn't seem worth blocking the release, that's for triage with the person, not a reason to file it as something milder.
- **A degraded device** (`degraded` on the row in `status`, and a `DEGRADED` event): the server samples the device's health after every result and restarts a degraded one before the next spec. A failure seen on a degraded device can't be upheld until it reproduces: call **verify**, and the rerun runs on the restarted device. A BLOCKED with cause `harness` can still be upheld.
- **Look at the evidence, not just the claim.** Open a screenshot only when the judgments don't settle it, which keeps your context small for a long run.
- **Side finds** (`side find(s) need review: id <n>: <summary>`; `status` lists the waiting ones under `side_finds_waiting`): `review_find` with that `id` and a `title` if it's worth the person's attention (it becomes a finding), or none if it isn't a problem, which keeps no screenshots, since it never reaches the page. For a finding, open each of its screenshots (in `evidence_dir`) and pass in `keep` only those on its topic: the control or screen the find is about, whether they confirm it or contradict it. Drop the rest (a slider finding doesn't keep shots of pen marks); when none is on topic, keep none and the page says so.
- **A pass with untried probes** (`PASS without trying its probes`, and `probes_untried` in `status`) waits as an Unsure: the outcomes held, but the tester said nothing in `probed`. Send it back to probe, or **judge** it PASS with the reason when the probes don't apply.
- **A person's pace.** A claim or side find that only happens faster than a person acts or looks isn't a problem: a tap a few milliseconds into a drawer's close, a second row tapped while the list is still shifting, a gesture straight after picking a tool in back-to-back calls, or a screenshot taken the instant after an action that shows the frame before the redraw. Treat it as a pace artefact: `review_find` it with no title, or send the claim back. When a spec's own probe asked for it, say so to the person at triage so the probe can be reworded.
- A spec gets at most 3 attempts. After that, file what's known.

## 4. Pause, resume, stop

When the person asks you to stop or pause (a big bug they want fixed first), call `pause` with their reason. Running testers finish, and nothing new starts. `resume` carries on from the next spec. There's no other special case for serious bugs.

## 5. A fix build mid-run

When the person has a fix committed (or built and installed it themselves) on a platform, the run carries on rather than starting again:

1. Read the fix's diff and propose which rows it could affect, each with a one-line reason. Show the list to the person and adjust it to what they say.
2. Call `install_build` between specs (it refuses while anything on that platform is running or waiting for review) with the agreed reruns, and `install: true` to have the suite's install hook build and install it (several minutes); leave the build out and the hook identifies it. A new fingerprint reruns everything on the platform, and rows with open problems always rerun, so a fix is proven on the build that has it.
3. Carry on dispatching: the reruns are queued and go first.

## 6. Finish

When every row in scope has a result and nothing is running, queued or waiting for review, call `finish`. It refuses, and names what's in the way, if anything is still open. Rows never dispatched don't block it, but they leave the verdict Not ready.

Finishing writes the final report (`report.html` in the run folder, screenshots embedded, so it opens anywhere offline) for sharing. Call `export` again after triage, so the shared copy shows the decisions; `export` also works mid-run, stamped in progress. Nobody needs an export just to look: every run, finished or not, opens from its folder (`/runs` on a run's server, or `soundcheck serve <suite>`).

If the server dies, start it again with `soundcheck run <suite> --resume <run folder>`: it keeps the run's platforms, scope and label, and puts any spec that was running back first without using an attempt.

To add a platform to a paused or finished run (the person wants both platforms in one report), resume it with `--platforms <list>`: platforms the run doesn't have yet get rows for its saved scope after their preflight, and a finished run opens again, so dispatch the new rows and `finish` once more. Platforms already in the run are kept either way.

To run specs again or add new ones, at any point, call `queue` with spec IDs or features. A spec not in the run joins it (one the person wants covered too, or a spec written since the run started). A spec already in the run runs again whatever state its rows are in (a harness gap that blocked it is fixed, the person wants it checked again, a tester went wrong mid-attempt): give the `reason`, which the report shows beside the result, `platforms` to limit it, and a `note` for the tester when the attempt should know something. A running attempt is abandoned and a claim waiting for review set aside, so review it first if its evidence matters. Each queued row gets a fresh attempt budget and a finished run opens again: dispatch them and `finish` once more. Resuming with `--scope <list>` adds specs the same way.

## 7. Triage, with the person

Go through the open problems with the person, usually at the end, one at a time: what failed (the outcome, expected versus observed), how sure you are (verified or a single attempt), and your suggestion. They decide; you record it with `triage`:

- **dismiss:** not a bug, or not reproduced;
- **fix:** create a ticket in the project's tracker. It can only be recorded once the row has passed again on a build with the fix;
- **defer:** create a ticket; it's accepted for this release;
- **duplicate:** of an earlier problem, which it then follows.

**Existing tickets are matched with the person, never automatically.** Before proposing a new ticket, search the tracker for an open one that covers the problem. If you find one, suggest it with its key and summary, and wait. The person decides:

- **the same bug:** record `fix` or `defer` with that ticket;
- **similar but its own bug:** create a new ticket and link it to the existing one ("relates to"), so the two stay together.

A finding that matches a known bug is still a finding; never dismiss it, or tell testers to skip it, because it's already ticketed.

**Evidence on request.** When the person asks for a screenshot of a problem, reproduce it yourself, never during a run's specs: run the suite's `seed` hook (suite.yaml) with the spec's scenario, drive there with `soundcheck drive <platform> …`, and `soundcheck drive <platform> shot <file>.png` once it's on screen (one quick retry; if it doesn't come back, say so). `add_evidence` the shot of the screen where the problem should be, whether it confirms the claim or contradicts it: a contradiction (the claim says 102%, the screen 100%) is what the person needs to see. Say which in the reason and tell them; attach it to the problem's ticket too when there is one.

The verdict (`status`) is Ready only when nothing is open. Export once triage is done. Report it to the person in one line, with its reasons if it isn't Ready.
