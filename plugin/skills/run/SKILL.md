---
name: run
description: "Coordinate a soundcheck regression run: start the server, which dispatches the testers and has their claims reviewed by itself; step in only when it asks, pause or resume on request, finish, then triage the findings with the person. Use when a project's regression skill hands over a run (a suite folder, platforms and a scope), or when asked to run, resume or triage a soundcheck run."
allowed-tools:
  - Bash(soundcheck run:*)
  - Bash(soundcheck attention:*)
  - Bash(soundcheck events:*)
  - Bash(soundcheck remote:*)
  - Bash(soundcheck lint:*)
  - Bash(soundcheck drive:*)
  - Read
  - mcp__plugin_soundcheck_soundcheck__status
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

You are the coordinator. The server runs the suite by itself: it starts each spec on each platform, launches its tester, files clean passes, restarts a device that needs it, and launches a reviewer for every claim and side find. You start the run, keep it on track when it asks for you, answer the person's questions, and go through the findings with them at the end. The server holds all state, so nothing lives only in your memory and a restarted session carries on from the journal.

You never drive a device during a run, edit a result, start a tester or message one: everything goes through the server, so it's all in the journal.

## Inputs

The project's regression skill gives you:

- the **suite folder** (the one with `suite.yaml`);
- the **platforms** to run;
- the **scope**: every spec, or a list of spec IDs;
- **what the run is testing**, in a sentence or two (the release's changes, the fix being checked), for the reviewers;
- with a tracker (`tickets` in suite.yaml), how to search and create its tickets, for triage.

## 1. Start

1. `soundcheck lint <suite>`: it must be clean. If it isn't, stop and report; never edit specs mid-run.
2. Start the server in the background: `soundcheck run <suite> --platforms <list> --scope <features or spec IDs> --title "<release under test>" --about "<what the run is testing>"` (leave out `--scope` for every spec), or `--open <run id>` to carry on an earlier run (its id, a unique start of it, or its folder). It prints the run's id and the live page's address at once (and, with `--remote`, its tunnel): give the person the link and the id straight away, since the page shows the step under way, a build included, and the id is how the run is opened again. If they ask for a remote link once the run is going, `soundcheck remote <run folder> on` opens one (same Google sign-in and allow list) and prints it; `off` closes it. Never restart a run for it. Wait for its `MCP at http://127.0.0.1:7357/mcp` line, which comes once the checks pass: from then on the server is dispatching. Opening a run never changes its rows, so it's safe to open one just to triage it. An earlier run's server that has finished and sat unused for 5 minutes makes way by itself; if the new run refuses because another run holds the port, tell the person what it said rather than stopping that run yourself. Only specs in scope become rows, one per platform the spec runs on, so the verdict covers exactly the scope. A platform whose installed build can't be identified is built and installed first with the suite's install hook (several minutes; tell the person), and `--install` rebuilds every platform from the current tree. A start that fails says why at once and keeps its run: fix what it names, then start it again with `--open <run id>`; its page keeps showing why for 10 minutes or until the next run takes the port. Add `--headed` when the person wants to watch the run. On web, Chrome opens in a window instead of headless. On iOS, Simulator.app opens on the device being driven, and again after a restart. On Android, the emulator's window shows only when soundcheck restarts the emulator; one already running stays as it is, without a window. On every platform it's for looking only: tell them a click or resize in the window can steer a test. Add `--uncommitted` when the person wants to try specs on code they haven't committed: every project hook then gets `SOUNDCHECK_UNCOMMITTED=1`, so the project's hooks may accept a build no commit identifies. It's a trial, not a regression result: the page says so, the run keeps it when opened again, and it files no tickets (see Triage).
3. If the `mcp__plugin_soundcheck_soundcheck__*` tools aren't available, the plugin's bridge (`soundcheck mcp`) didn't start: check `soundcheck` is on the PATH and ask the person to reconnect soundcheck with `/mcp`.
4. Run `soundcheck attention <run folder>` in the background (a background Bash command, not a Monitor) and end your turn. It waits, however long, until something needs you, prints it and exits, which wakes you. Deal with what it printed, then run it again: it keeps its own place in the run, so nothing that happened meanwhile is missed, and it never needs a filter or a cursor from you.

## 2. While it runs

The server needs nothing from you for a spec that passes, a claim a reviewer settles, a stall (the spec is queued again), a degraded device (it's restarted) or a busy Mac (it waits). Say nothing to the person between wake-ups: the live page shows every dispatch, judgment and review as it happens. `attention` wakes you only for these:

| It prints                                    | What to do                                                                                                                                                                                                                                                                                             |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `paused: soundcheck paused the run: …`       | The line says why: a tester couldn't run (Claude's usage limit, Claude missing, a model it doesn't have), or the installed build can't be identified (a commit landed in the app's repo mid-run, say). Nothing was lost, and the spec is still queued. Tell the person why. When it's fixed (the limit has reset, the build reads again or `install_build` has installed one), `resume`                                                                                       |
| `review unsettled: <spec> on <platform> …`   | Two reviewers ended without settling that row. Review it yourself: section 3                                                                                                                                                                                                                              |
| `build … installed on <platform>`            | An `install_build` you started has finished; its reruns are queued and the server dispatches them. Nothing to do unless the person is waiting to hear                                                                                                                                                     |
| `install failed on <platform>: …`            | The platform is held until an install succeeds there. Tell the person why it failed; `install_build` again once it's fixed                                                                                                                                                                              |
| `<platform> restart failed: …`               | The server tries again every minute and the platform's specs wait. If it keeps failing, tell the person: the device needs them                                                                                                                                                                          |
| `every row is filed: verdict …`              | The work is done: go on to triage (section 7)                                                                                                                                                                                                                                                            |
| `the run's server stopped`                   | If nobody stopped it, start it again with `--open <run id>`: it keeps the run's platforms, scope and title, and puts any spec that was running back first without using an attempt                                                                                                                       |

- **Questions.** When the person asks how the run is going, `status` answers: the verdict, counts, what's running, what waits for review, the problems so far. `soundcheck usage <run folder>` lists what each attempt and each reviewer cost, and the run's total; with `--compare <other run folder>` it compares two runs. `soundcheck events <run folder> --once` prints the run's story so far.
- **A note for one spec, for this run only** (the person knows something its tester should: a control that moved, an account to use): `queue` the spec with the `note` and a `reason`. The note goes into that spec's next brief and is then dropped. A spec that hasn't run yet takes a note the same way.
- **To run a spec now, or again:** `queue` it (section 6). Queued rows go first.
- A spec reworded mid-run (a vague outcome, a probe that asked for the impossible) takes effect when you `queue` it again: the server rereads it from disk, and the report notes the change beside its results.
- A tester or reviewer's whole conversation is kept in the run folder's `agents/` as a `.jsonl` stream, for when the person asks why one did what it did.

## 3. Review, when no reviewer settled it

Reviews are made by reviewers the server launches, one per row, each with only that row's evidence; you'll usually make none. For a row `attention` says is unsettled, or when the person disputes a reviewer's call (`queue` the spec again, or record their decision at triage), call `status` with the row's `spec` and `platform` for its judgments, then decide, with one plain line of reason:

| Call             | When                                                                                                                                                                                                                                                                                                                                                                                                                             |
| ---------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **uphold**       | The evidence is conclusive. A FAIL goes into Problems as a **failure**: give it a `title` in plain words. A BLOCKED claim needs its `cause`: `app` if the app stopped the test (a crash, a screen that won't load): it's filed FAIL, with a failure's `title`; `harness` if the environment or harness did (a missing label, a tool that can't reach a control, a stuck device): it's filed BLOCKED, which only means not tested |
| **verify**       | Plausible but not conclusive: seen once, on a degraded device, or hard to believe. Say exactly what to reproduce                                                                                                                                                                                                                                                                                                                 |
| **send-back**    | The tester erred: a wrong tap, a misread screen, a step from the wrong place                                                                                                                                                                                                                                                                                                                                                     |
| **judge**        | An Unsure you can settle from the evidence: PASS, or FAIL with the problem's `title`                                                                                                                                                                                                                                                                                                                                             |
| **spec-unclear** | An Unsure still unclear after verifying: the outcome is worded too vaguely. The spec is flagged for rewording                                                                                                                                                                                                                                                                                                                    |
| **harness**      | The harness, the environment or the spec's wording stopped the check, whatever the claim (a tool that can't reach a control, a hint that's wrong, a state an earlier attempt left). Without `rerun` it's filed BLOCKED, not tested, with a harness-gap note. Once the gap is fixed, give `rerun` a note for the tester instead: a fresh attempt is queued with it, and a pass files nothing                                      |

- **Titles are what the person reads first.** Write each as the problem in plain words ("Undo leaves the stroke on screen when zoomed in"), never as the outcome or the tester's wording.
- **Failure or finding is never your judgement call.** Upheld failures are failures; side finds and things that didn't reproduce are findings. If a failure doesn't seem worth blocking the release, that's for triage with the person, not a reason to file it as something milder.
- **A degraded device** (`degraded` on the row in `status`): the server samples the device's health after every result and restarts a degraded one before the next spec. A failure seen on a degraded device can't be upheld until it reproduces: call **verify**, and the rerun runs on the restarted device. A BLOCKED with cause `harness` can still be upheld.
- **Look at the evidence, not just the claim.** Open a screenshot only when the judgments don't settle it, which keeps your context small for a long run.
- **Side finds** (`side find(s) need review: id <n>: <summary>`; `status` lists the waiting ones under `side_finds_waiting`, with `spec` for just that spec's): `review_find` with that `id` and a `title` if it's worth the person's attention (it becomes a finding), or none if it isn't a problem, which keeps no screenshots, since it never reaches the page. For a finding, open each of its screenshots (in `evidence_dir`) and pass in `keep` only those on its topic: the control or screen the find is about, whether they confirm it or contradict it. Drop the rest (a slider finding doesn't keep shots of pen marks); when none is on topic, keep none and the page says so.
- **A pass with untried probes** (`PASS without trying its probes`, and `probes_untried` in `status`) waits as an Unsure: the outcomes held, but the tester said nothing in `probed`. Send it back to probe, or **judge** it PASS with the reason when the probes don't apply.
- **An outcome the platform can't show** (an Unsure saying the platform lacks what it describes: an on-screen keyboard on chrome, say) isn't the app's fault or the tester's. Call **harness** without `rerun`, noting what the platform lacks, so it's filed not tested. At triage, propose taking the platform off the spec, or rewording the outcome so it holds on every platform the spec lists.
- **Specs new to a platform** (a run that brings existing specs to a platform for the first time): once the rows are filed, review every pass too, not only claims (`status` with the `spec`). Read each judgment against what the spec is for. A pass that couldn't have failed there (an outcome about a control the platform doesn't have, a spec that guards a bug only another platform had) is hollow. Report each pass as genuine or hollow at the end, and propose the platforms to take off.
- **A person's pace.** A claim or side find that only happens faster than a person acts or looks isn't a problem: a tap a few milliseconds into a drawer's close, a second row tapped while the list is still shifting, a gesture straight after picking a tool in back-to-back calls, or a screenshot taken the instant after an action that shows the frame before the redraw. Treat it as a pace artefact: `review_find` it with no title, or send the claim back. When a spec's own probe asked for it, say so to the person at triage so the probe can be reworded.
- A spec gets at most 3 attempts. After that, file what's known.

## 4. Pause, resume, stop

When the person asks you to stop or pause (a big bug they want fixed first), call `pause` with their reason. Running testers finish, and nothing new starts. `resume` carries on from the next spec. There's no other special case for serious bugs.

## 5. A fix build mid-run

When the person has a fix committed (or built and installed it themselves) on a platform, the run carries on rather than starting again:

1. Read the fix's diff and propose which rows it could affect, each with a one-line reason. Include rows with open problems the fix addresses, since Fixed needs a pass on the build with the fix. If the build's fingerprint changed, say so and why (a `patches/` change counts as native even when the patch is JS), and let the person decide whether that means everything. Show the list to the person and adjust it to what they say: nothing reruns that they haven't agreed.
2. Call `install_build` with the agreed reruns, at any time: there's no need to pause or to wait for a gap. If a spec on that platform is running or waiting for review, the reply says the install is queued: nothing new starts on that platform, the other platforms carry on, and the install starts by itself once the running spec has posted and been reviewed. Don't call again while it's queued or installing; a second call is refused. The suite's install hook then builds and installs the current tree (several minutes). The server runs it and the call returns at once; `soundcheck attention` reports its end, `build ... installed on <platform>` or `install failed on <platform>: <why>`. If the server was stopped while the install was still queued, it's forgotten: ask again after reopening the run. Only the listed rows rerun; everything else keeps its result, and the log says which build it ran on. soundcheck builds and installs what it tests, so every mid-run install goes through `install_build`.
3. The reruns are queued and the server runs them first, once the install has ended.

## 6. Finish

The run stays open until the person says it's done: filing the last row doesn't close it. Once every row is filed, tell the person and go on to triage; call `finish` when they say the run is done (or want to stop with rows never run: `pause` first, so nothing new starts). It refuses, and names what's in the way, if anything is still running or waiting for review. Rows never run leave the verdict Not ready.

Finishing writes the final report (`report.html` in the run folder, screenshots embedded, so it opens anywhere offline) for sharing. Call `export` again after triage, so the shared copy shows the decisions; `export` also works mid-run, stamped in progress. Nobody needs an export just to look: every run, finished or not, opens from its folder (`/` on a run's server lists them, each at `/{run id}/`, or `soundcheck serve <suite>`).

If the server dies, start it again with `soundcheck run <suite> --open <run id>`: it keeps the run's platforms, scope and title, and puts any spec that was running back first without using an attempt.

To add a platform to a paused or finished run (the person wants both platforms in one report), open it with `--platforms <list>`: platforms the run doesn't have yet get rows for its saved scope after their preflight, and a finished run opens again, with the server running the new rows. Platforms already in the run are kept either way.

To run specs again or add new ones, at any point, call `queue` with spec IDs or features. A spec not in the run joins it (one the person wants covered too, or a spec written since the run started). A spec already in the run runs again whatever state its rows are in (a harness gap that blocked it is fixed, the person wants it checked again, a tester went wrong mid-attempt): give the `reason`, which the report shows beside the result, `platforms` to limit it, and a `note` for the tester when the attempt should know something. A running attempt is abandoned and a claim waiting for review set aside, so review it first if its evidence matters. Each queued row gets a fresh attempt budget and a finished run opens again: the server runs them, and `attention` says when they're all filed. Resuming with `--scope <list>` adds specs the same way. When that changes what the run is for (a check extended into the release regression), `relabel` it with the new label and why, so the page and the export say what it now covers; `--title` on `--open` does the same.

## 7. Triage, with the person

Go through the open problems with the person, usually at the end, one at a time: what failed (the outcome, expected versus observed), how sure you are (verified or a single attempt), and your suggestion. They decide; you record it with `triage`:

- **dismiss:** not a bug, or not reproduced;
- **fix:** it can only be recorded once the row has passed again on a build with the fix;
- **defer:** it's accepted for this release;
- **duplicate:** of an earlier problem, which it then follows.

How fix and defer are recorded follows the suite. With a tracker (`tickets` in suite.yaml), each needs a ticket, as below. Without one, the report is the record: leave `ticket` out, and make the reason say what was decided and what happens next, since nobody will look anywhere else.

**An uncommitted run files no tickets** (`status` says `uncommitted: true`): its results come from a build no commit identifies. Don't search the tracker, offer a ticket or create one; record fix and defer without `ticket` (`triage` refuses one), with a reason the report can stand on. A problem worth a ticket is found again in a run on the committed code.

**With a tracker, existing tickets are matched with the person, never automatically.** Before proposing a new ticket, search the tracker for an open one that covers the problem. If you find one, suggest it with its key and summary, and wait. The person decides:

- **the same bug:** record `fix` or `defer` with that ticket;
- **similar but its own bug:** create a new ticket and link it to the existing one ("relates to"), so the two stay together.

A finding that matches a known bug is still a finding; never dismiss it, or tell testers to skip it, because it's already ticketed.

**Evidence on request.** When the person asks for a screenshot of a problem, reproduce it yourself, never during a run's specs: `soundcheck drive <platform> relaunch`, run the spec's `prepare` steps in order from the suite folder with `SOUNDCHECK_PLATFORM=<platform>` set, drive the rest of the way with `soundcheck drive <platform> …`, and `soundcheck drive <platform> shot <file>.png` once it's on screen (one quick retry; if it doesn't come back, say so). `add_evidence` the shot of the screen where the problem should be, whether it confirms the claim or contradicts it: a contradiction (the claim says 102%, the screen 100%) is what the person needs to see. Say which in the reason and tell them; attach it to the problem's ticket too when there is one.

The verdict (`status`) is Ready only when nothing is open. Export once triage is done. Report it to the person in one line, with its reasons if it isn't Ready.
