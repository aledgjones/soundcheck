# Soundcheck: design

Soundcheck runs an app's regression suite on Android, iOS and web with AI agents as the testers, and reports results you can trust while the run is still going. It's one generic product: a project plugs in its test cases and a little config, and soundcheck does the rest.

This document is the design agreed on 2026-10-20. It's for whoever builds soundcheck, human or agent. Nothing is built yet. Its first user is Solo Scores, which will prove the concept; its migration notes live in that project, not here.

---

## 1. Principles

- **A run needs no babysitter.** The suite runs start to finish on its own. The coordinator judges each result as it lands, verifying where it needs to, and the person reviews the report at the end and decides what to do with the findings. The person can stop a run at any time by asking the coordinator; there's no special case for major bugs. See 3.10.
- **Every check is something a user does.** A spec is a user's goal in plain words. Nothing is checked twice, and a pass means what it says.
- **Android, iOS and web are equal from the first version.** The interface, element model, spec format and config are designed for all three at once.
- **Results and judgments show in real time,** presented exactly as the final report will show them. The person may look in at any moment to decide whether to stop, so every result and every coordinator call reaches the page within seconds.
- **Machinery, not discipline.** Anything that went wrong because someone forgot a step becomes a state the server tracks and enforces.
- **Trust comes from:**
  - evidence filed against the exact build installed;
  - no unconfirmed result ever shown as a result;
  - parity tests for every reader against the platform's own tool;
  - verdict rules with no special exemptions.
- **Pieces, not the picture.** Driver tools are generic and parameterised, so they make sense unchanged in an unrelated app. No app's labels, flows or names enter soundcheck.
- **Emulators, simulators and local browsers only.** Physical devices are refused: they may run builds that point at production.

---

## 2. The pieces

| Piece             | What it is                                                                        | Knows about                                                                            | Lives in                             |
| ----------------- | --------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- | ------------------------------------ |
| **Server**        | One long-running process per run: drivers, row states, journal, live page, report | Suites, results, builds; the project via config                                        | `soundcheck` (Rust, `crates/server`) |
| **Drivers**       | One module per platform behind one interface                                      | Devices only; never tests, rows or the app                                             | `soundcheck` (`crates/driver`)       |
| **Plugin**        | The Claude Code plugin: binary, MCP config, skills, the tester agent type         | How to set up, run and author a suite                                                  | `soundcheck` (`plugin/`)             |
| **Project skill** | The project's own regression skill                                                | How to run its suite: accounts, device setup and quirks; takes a scope from its caller | The project                          |
| **Suite**         | Specs, `suite.yaml`, hook scripts                                                 | The app and its users                                                                  | Inside the project skill             |

The driver is a separate crate, so the compiler enforces that it never depends on the server.

### Who does what at run time

| Part            | Is                                                            | Does                                                                                                                                             | Never does                                                     |
| --------------- | ------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------- |
| **Server**      | The `soundcheck` process                                      | Owns devices, device health, build identity, row states, the journal, evidence capture, the live page and report; seeds before each spec         | Judges a non-pass, or decides to stop a run                    |
| **Coordinator** | The Claude session the person talks to (later: headless code) | Starts the run, dispatches one background agent per spec, reviews non-passes while the next spec runs, triages the findings list with the person | Drives a device, or holds results in its own memory            |
| **Testers**     | One `soundcheck-tester` agent per spec                        | Drive the device through the server's tools, post the spec's result with evidence                                                                | Read app source, run shell, settle results, talk to the person |

How they talk:

- **Tester → server:** MCP tool calls, starting with `brief` for its spec.
- **Server → tester:** only in the reply to the tester's next call ("device degraded, stop"). MCP notifications don't wake a Claude agent, so nothing relies on them.
- **Server → coordinator:** an event stream. The coordinator runs `soundcheck events` under a Monitor, which prints one line per event (spec finished, result needs review, device degraded) and replays anything missed since its cursor, so re-arming the Monitor every 30 minutes loses nothing.
- **Coordinator → server:** MCP tools or the CLI (start, dispatch, queue, uphold, verify, send back, pause, resume). It reaches testers only through the server, so every instruction is in the journal.
- **Server → live page:** WebSocket on the same port.

---

## 3. Server

### 3.1 Transport

- One process listens on localhost over HTTP: MCP (streamable HTTP, `rmcp`), the live page, its WebSocket and the event stream share one port. The plugin's MCP config runs `soundcheck mcp`, a stdio bridge that forwards every agent's tool calls to that URL.
- **Why not stdio:** Claude Code starts a stdio MCP server once per agent, which would mean several processes fighting over the devices and the journal.
- **Local callers only, with no secret.** It binds to 127.0.0.1, requires a `Host` of `127.0.0.1:<port>` or `localhost:<port>` (which stops a page whose domain was rebound to 127.0.0.1), and refuses any `Origin` but its own (agents and the CLI send none; the live page sends the server's). A token or fixed secret was dropped: any local process could read it and drive the emulator over adb anyway, and it would stop the plugin's MCP config being a plain fixed URL.

### 3.2 A run

1. **`soundcheck run <suite>`** (the coordinator starts it) runs a **preflight**: a fixed checklist, printed as each step passes, journalled for the timeline, and refusing to start at the first failure with what to fix. Deterministic on purpose: preparation is never an agent's judgement call.
   - lint the suite and check its pinned version;
   - run the project's `guard` hook (the project decides what "safe and up" means, e.g. local only, backend answering);
   - read each platform's build identity; build and install with the `install` hook where it's unknown (every platform with `--install`), and refuse if it's still unknown;
   - connect each device, launch the app and wait for the platform's `ready` labels from `suite.yaml` (signed in, past onboarding);
   - then open the live page.

   The project supplies every specific (hooks, `ready` labels); soundcheck supplies the sequence and the generic checks, so it stays project-agnostic.
2. **Dispatch, one spec at a time per platform:** the coordinator calls `dispatch(spec, platform)`.
   - The server runs the project's `seed` hook, relaunches the app, then checks the spec's `starts-on` elements are on screen. A failure there is a **reset error**: retried automatically (twice), never counted as an attempt and never blamed on the tester or the app; if it persists, the spec is filed BLOCKED "reset failed" with the server's capture.
   - It renders the tester's brief (the spec, its `_feature.md`, the platform hints, the result contract and a spec token) and records it in the journal.
   - It returns the token, the spec's model (the suite's `models.default` when the spec names none) and a short prompt naming the token. The coordinator passes the prompt unchanged to a **background** `soundcheck-tester` agent with the spec's model.
   - **The brief never passes through the coordinator** (2026-10-23): the tester fetches it with `brief`, which answers only while the token's spec is running. A brief is 3–8k tokens, and returning it from `dispatch` put each one in the coordinator's context twice, which ran a long run's coordinator out of context.
3. **The tester works** through the driver tools and posts the spec's result once, with evidence. Its final reply to the coordinator is one line (token, claim, side-find count): everything else is on the server.
4. **When a spec finishes,** the event wakes the coordinator. It dispatches the next spec straight away and reviews any non-pass in parallel.
5. **Finish:** the server refuses to finish the run (stamp the verdict, allow the final export) while any result is unreviewed or any spec is still running, and names what blocks it. Sections have no closing step: each grid tile always shows its live state.

Platforms run serially by default (`parallel: false`). All state is kept per platform (a dispatch lane, a device, an account, a seed), so turning parallel on later is a config change, not a refactor. Parallel runs have caused problems before, so it isn't a priority.

### 3.3 Row states

Each spec on each platform moves through `not run → running → needs review → filed`, and the server owns every transition. A non-pass then goes through two separate decisions: **review** (is the claim real?) during the run, and **triage** (what do we do about it?) with the person.

- **A PASS** files at once, provided it has evidence for every outcome.
- **The tester judges each outcome,** not the spec: **held**, **failed**, **unsure** (observed, but it can't tell whether it's right) or **couldn't check** (never got to observe it). The server derives the spec's claim: any failed is FAIL, else any couldn't check is BLOCKED, else any unsure is **Unsure**, else PASS.
- **Final results are only PASS, FAIL or BLOCKED**, each resting on specific outcome lines. Unsure is never final.
- **A FAIL, BLOCKED or Unsure** becomes `needs review`. On the page the row reads **"Checking"** (the tester reported a problem and the coordinator is checking it; it's the coordinator's job, not the person's), with no age on the row. A check that stalls shows in the sticky bar instead ("checking annotations/003 for 6 min", in amber). It should last only minutes.
- **Review** is the coordinator alone, judging the tester's evidence and the server's capture. It does one of three things:
  - **uphold:** the evidence is conclusive. A FAIL is filed, and goes into Problems as a **failure**. For a BLOCKED claim (the tester couldn't check), the coordinator says what stopped it: the **app** (it crashed, a screen wouldn't load), so the row is filed **FAIL** with a failure; or the **environment or harness** (a missing label, a tool that can't reach a control), so it's filed **BLOCKED** with a note.
  - **verify:** the evidence isn't conclusive. A verification attempt is queued: a new agent on a fresh seed, **told the claim** ("the first attempt saw the stroke survive undo; reproduce it"). Reproduced: upheld. Not reproduced: filed as a finding marked "not reproduced (1 of 2)", never silently dropped, since flaky problems matter at triage.
  - **resolve an Unsure:** judge it from the evidence (PASS or FAIL, with a reason) or verify it, telling the new agent exactly what was unclear. A verified Unsure that passes files PASS and nothing else: only a FAIL that doesn't reproduce becomes a "not reproduced" finding. Still unclear after verifying: filed BLOCKED with "spec unclear", and the spec is flagged for rewording with `create-test`, since ambiguity almost always means an outcome is worded too vaguely.
  - **send back:** the tester erred (a misread screen, a wrong tap, a device glitch), so a fresh attempt is queued without the claim.
  - Every call carries one line of reasoning ("upheld: the screenshot shows the stroke after undo"), shown on the row with whether it was verified or a single attempt, so the person's review at the end is an audit of the coordinator's calls.
  - All attempts stay in the journal.
- **Triage** happens in chat: the person with the coordinator, who records each decision through the server and creates the tickets, so the page (read-only) updates live. Matching a problem to an existing ticket is collaborative, never automatic (Aled, 2026-10-21): the coordinator searches the tracker and suggests a ticket; the person says whether it's the same bug (recorded under that ticket) or similar but its own (a new ticket, linked to the existing one). It's usually at the end of the run but possible at any time (a serious problem mid-run may need a fix decision straight away). Each open problem is:
  - **dismissed**: not a bug, or not reproduced;
  - **fixed**: a ticket is created and the fix lands;
  - **deferred**: a ticket is created and it's accepted for this release.
- **Side finds:** a tester that finds a problem outside the spec's outcomes while probing posts it as its own finding, linked to the row. The row keeps its own result, and the finding is reviewed like any non-pass. Its id (for `review_find`) is on the posted event's line and in `status`, beside its summary. Probing near old bugs is where new ones turn up.
  - **A finding's screenshots are on its topic** (Aled, 2026-10-21). They're there to help the person understand the problem, not to prove it: a finding about the eraser slider showed pen marks, which told him nothing. A side find never inherits the server's end-of-spec capture; that stays with failed rows. The tester attaches only shots of the control or screen the problem is about, including when the problem itself isn't visible (a spoken value): the screen may contradict the claim, and that's worth seeing. `review_find` keeps only the on-topic shots, whether they confirm the claim or not, and a finding with none says so on the page.
  - **Evidence on request** (Aled, 2026-10-21). When the person asks in triage for a screenshot of a problem, the coordinator reproduces it on the fly (the seed hook, then `soundcheck drive`) and `add_evidence` puts the shot on the problem, filed under its original attempt. It goes on whether it confirms the claim or contradicts it: a finding said the eraser slider read 102%, and a shot reading 100% is what starts the conversation that the text alone couldn't. No targeted-rerun command: the evidence is what matters, not a new result.
- **A spec whose tester stops** without posting goes back to `not run` with the reason, counted as an attempt, and is never left spinning. Stalled means no tool call for 2 minutes (configurable), counting only the time between calls: time inside a server call (a seed, a `wait`, a `sleep`, a `network-drop`) doesn't count. The clock starts at the tester's first tool call: before it, the limit is 10 minutes from dispatch (also configurable), since a slow network once delayed first calls past 2 minutes and every later call was refused.
- **At most 3 attempts per spec** (the original plus send-backs and verifications). After the third, the coordinator files what's known: BLOCKED with the reason, or the finding marked "not reproduced (1 of 3)". A flaky spec can't loop all night.
- **Not blocking:** a review never holds up the next dispatch. Unreviewed results block finishing the run; open problems block a Ready verdict, not finishing.

### 3.4 Journal

- Every event is appended to `journal.jsonl` before it's acknowledged: run start, dispatch with its prompt, result, review, device health, build installs and the reruns they triggered. A crash loses nothing.
- **A run folder is self-contained:** the journal, `run.json` (the suite, label, platforms and scope it started with), a snapshot of the suite's config and specs as they were, `summary.json`, and the evidence. Any run can be shown again from its folder alone, faithful to what it tested.
- **Resume** (`soundcheck run <suite> --resume <run folder>`) replays the journal with the saved platforms, scope and label. A spec that was running when the server stopped goes back to the front of the queue without using an attempt: the harness stopped it, not the app. `--platforms` on resume only adds: a platform the run lacks gets rows for its saved scope after its own preflight, `run.json` lists it and the journal records the addition, so one report covers both platforms. `--scope` on resume only adds too: specs new to the run get their rows, `run.json`'s scope and the snapshot gain them, and the journal records a `specs-added` event. The coordinator's `queue` does the same live, without a restart, reading new specs from the suite as it is now (they must lint clean). A finished run that gains rows opens again and is finished once more when they're done.
- **Any spec can run again, whatever state its rows are in** (Aled, 2026-10-23, SA-716). After a harness fix, playlists/015's BLOCKED rows had no way back in once the run had finished. `queue` takes spec IDs or features: one not in the run joins it, one in it runs again with a reason the report shows. A filed result or a claim waiting for review is set aside, a running attempt is abandoned (its token stops working), and each row gets a fresh attempt budget; a finished run opens again. Adding and rerunning are one tool because they're one intent: run these. A note left by an attempt that a later one replaced says so in the report.
- **The server is the only writer of results.** The report and live page render from the journal.
- **Location:** configurable in `suite.yaml` (`reports: <dir>`), one folder per run holding the journal, evidence and exports.

### 3.5 Evidence

- A tester attaches what it observed per outcome: tool output, plus screenshots for visual checks only.
- **On every non-pass, the server captures its own bundle** before replying: a screenshot, the element tree and recent errors/logs. The next spec's seed wipes the device state, so review must never depend on it.
- The server refuses a result that lacks evidence for any of the spec's outcomes.
- One evidence folder serves the whole run, so a tester's screenshot is saved as `<spec>-<attempt>-<platform>-<name>.png`, the server's captures' prefix. `post_result` resolves each attached name (bare, `.png` or full) to its saved file and refuses names that were never saved; the report also tries a bare name with `.png`, for older journals.

### 3.6 Build identity

- Read once at run start and again after each install, never on every filing (a device under memory pressure made per-filing queries take 40 s).
- **The driver reads generic facts:** app installed, version, install time. The project's `build-identity` hook turns them into commit plus fingerprint (mobile) or commit plus bundle hash (web).
- Results file against the installed build. The release name in the run's config is only a label, so a stale install can't pass as the version under test.

### 3.7 Device health

- **Sampled after every result,** and journalled with that attempt as evidence:
  - Android: free memory (degraded under 10%), busy CPU over two seconds while the app sits idle (over 80%; not the load average, which post-boot disk work keeps near 20 with the CPU idle), ANRs in other apps since launch, and the app's launch time (`am start -W`, over 15 s);
  - iOS: the app's launch time (over 20 s). The simulator shares the Mac, so the Mac's free memory (under 10%) and its one-minute load (over 2 per core) are sampled too, as host signals.
  - Lost `adb reverse` forwards are put back on the spot: a reboot drops them, and the app then silently shows stale data.
- **When the host is busy** (a host signal past its limit), and while a degraded device restarts: dispatch refuses with `held:` rather than blocking (a tool call times out long before a boot settles), and the server watches the host every 15 s for up to 10 minutes, then journals the platform's `ready` event, the coordinator's cue to dispatch again. A host that never settles isn't held for again for 15 minutes. The wait is a note in the record. A restart never answers a busy host: a booting simulator adds load, and on the first run (2026-10-21) restarts for the Mac's five-minute load fed a boot storm (`amsengagementd` and News at 100% each, load 140) that marked the next spec degraded too.
- **When health is degraded:**
  - the attempt is marked, and a failure seen on it can't be upheld until it reproduces: the coordinator verifies it, and the rerun runs on the restarted device. A harness BLOCKED can still be upheld;
  - the server restarts the emulator or simulator before the next spec on it, waits for it to settle, restores its forwards and reconnects its reader. Both are notes in the record. At most one restart per platform in 15 minutes, so a device that stays degraded is noted rather than rebooted in a loop.
- Health is part of the evidence. A slowly degrading emulator produced one-off failures that looked like app bugs (2026-10-20: 0.4 of 4 GB free, the system busy while idle, ANRs in Google's own apps, a 59 s cold launch).

### 3.8 Fix builds

A run continues across builds. When an urgent fix lands mid-run, the run carries on where it got to rather than starting again. This is rare, and the person is involved anyway (they asked for the fix), so it rests on judgement, not machinery.

- **Every row is stamped with the build it ran on.** That stamp is internal tracking: rows don't show it. The report's notes list the builds in the run and which rows ran on which.
- **Installs happen between specs.** The server refuses to install on a platform while a spec runs there, so no row straddles two builds. Platforms are independent: one still on the old build carries on there.
- **The coordinator proposes the reruns.** It reads the diff and lists the rows to rerun, each with a one-line reason, in chat. The person adjusts the list if needed, and the coordinator sends it to the server before the run resumes.
- **Two mechanical rules the server applies on top:**
  - **a native change** (new fingerprint) reruns every row on that platform;
  - **rows with open problems** always rerun, so a problem can only be triaged as Fixed after its row passes on a build containing the fix.
- **Everything else keeps its result.** The notes record which results came from an earlier build and the reasons for each rerun, never silently.
- **The run then continues** on the new build: the reruns and the remaining specs.

### 3.9 Report, live page, export

- **One UI.** The live page, the final report and the export are the same pages rendered from the same journal, so what the person watches during a run is exactly what they review afterwards.
- **Keep today's look.** The layout and styles of Solo Scores' current report (overview grid, section bars, overall ring, row and problem styling) carry over, adapted to the new workflow and data. Any design change beyond that is discussed with the person before it's made.
- **What needs attention must be obvious at a glance.** Today's rows can bury it in a wall of text. Every row's detail follows one order (Aled, 2026-10-21):
  1. **anything that needs attention, as one coloured block:** red for a failure (the outcome that didn't hold, expected versus observed, and the coordinator's call with its reason and whether it was verified); amber for a finding (what it is in one line, that it's outside the check, and a link to it in Problems);
  2. **the outcomes,** each with what was seen. Where the coordinator settled an outcome (an Unsure judged from a screenshot), its note sits under that outcome, never in a separate box;
  3. **the evidence and the tester's notes,** collapsed.

  The collapsed row hints at a finding ("Passed · 1 finding"). The server enforces the failure block's input: a non-pass must name the outcomes it failed, with observed text for each.

- **The page header says what's happening now:** how long the run has been going and the spec running on each platform, plus "idle 20 min: coordinator not responding" once it's true. That's the only alarm an unattended run needs.
- **Page decisions** (Aled, 2026-10-21):
  - **Header:** the release label, devices, run start and last update. Commits stay out: the notes say which build each row ran on.
  - **A slim sticky bar while the run is live** says what's happening now ("Run · 1 h 12 min · Android · annotations/003"), and goes once the run finishes. One timer, for the whole run: per-spec timers and "last event N ago" were noise (Aled, 2026-10-21). A check or an idle coordinator shows its time only once it's overdue, in amber.
  - **Rows show the spec's number within its feature card** ("001"); the full path appears on hover and in exports.
  - **Every row expands,** passes too, closed by default: a pass shows each outcome with what was seen, so you can check what it rested on.
  - **Problem titles are written by the coordinator** in plain words when it upholds a claim or a side find; the outcome-versus-observed lines sit beneath as the detail.
  - **One badge per problem, on the left:** an open problem shows **Failure** (red) or **Finding** (amber); a fixed or deferred one shows its ticket; a dismissed one a muted "Dismissed". The right side says only where it happened. "Verified" and "side find" go in the detail. The approved mockup: `design/report-mockup.html` (published at https://claude.ai/artifact/2vwanZhfugrneZJQqJq49Q)
  - **A collapsed timeline** at the bottom tells the run's story from the journal, one readable line per event. It replaces today's run log.
  - **Every run can be looked at, finished or not, without exporting:** a run's server lists all runs at `/runs` and renders each from its folder, and `soundcheck serve <suite> [--open latest]` does the same with no run going ("show me the previous report"). Exports stay single files, for sharing.
- **The report sections:** verdict rules (section 7), open problems, then triaged ones grouped as Fixed, Deferred or Dismissed, duplicates folded into their original, and time per spec and per run (plus agent cost where the coordinator can report it) so expensive specs stand out; nothing stops on budget; the notes hold harness gaps, device health and which build each row ran on. Ported from Solo Scores' `report.mjs`.
- **Light enough to sit open for hours** beside an emulator: animate only opacity and transform, and leave no spinner on a row nobody is working on.
- **Export:** a self-contained HTML file with screenshots embedded, which opens offline indefinitely.
  - Available at any time, from the page's **Download report** button (live or past run) or the coordinator's `export`. A mid-run export is stamped "in progress" with the verdict so far.
  - The final export is stamped with the verdict and build identities and doesn't change.

### 3.10 Unattended runs

Nothing in a run may wait for a human:

- **Permissions:** a Claude Code permission prompt must never be what stops an overnight run. The `run` skill pre-allows the coordinator's tools (`allowed-tools`); `setup` adds `mcp__plugin_soundcheck_soundcheck__*` to the project's permission allowlist so background testers never prompt. That's safe because the tester agent's only tools are soundcheck's own and Read. The plugin's MCP tools are named `mcp__plugin_soundcheck_soundcheck__<tool>`, with the registry's hyphens as underscores; a test keeps the plugin's files in step with the registry.
- **Accounts:** each platform's account is set up so seeding and sign-in never need a person (no account pickers, no device-policy prompts). `setup` checks this.
- **Stuck devices:** the server restarts a degraded or wedged device (3.7). If a spec still can't run, the coordinator files it BLOCKED with the reason and moves on.
- **Where the coordinator runs:** in v1, an ordinary Claude Code session on the test machine, left open. Headless dispatch stays a later step.
- **A coordinator that lasts hours:** all state lives in the server, so a compacted or restarted coordinator reconnects and carries on from the journal. The coordinator reads short evidence summaries and opens a screenshot only when the summary isn't enough, and never handles a tester's brief or a long hand-back (3.2), keeping its context small.
- **Stop is pause.** When the person asks the coordinator to stop, the run pauses with everything in the journal; `resume` continues from the next spec. That covers "big bug: fix it, then carry on" with no special case.

---

## 4. Drivers

### 4.1 Interface

- **One registry of every tool** (`soundcheck_driver::TOOLS`): each entry has its name, its kind (basic, or a mechanic) and the platforms it runs on, and later its parameters, which are also its MCP schema. The MCP tool list, lint and the tester's brief all read from it, so no tool is defined twice. Basic tools are open to every spec; a spec lists mechanics in `tools`, and lint refuses one on a platform it doesn't run on.
- **Reading:**
  - `see [filter]`: the element tree;
  - `expect` and `gone`;
  - `check "A" "B" "!C"`: several assertions on one read;
  - `wait`;
  - `sleep <ms>`: a plain timed pause, at most 30 s (the coordinator's `pause` pauses the run, hence the name);
  - `shot`;
  - `errors`.
- **Input:** `tap`, `fill`, `type`, `clear`, `swipe`, `hold`, `touch`, `release`, `keyboard`, `launch`.
- **`touch`: one finger along a path** (SA-715). Each stop is an element or a point, with a rest there; the finger moves between stops at a fixed finger pace, settles and lifts at the last. It covers any gesture a finger makes without lifting: slide off a control and back, long-press then drag, stop a slider partway. Every element is found before the finger goes down, so nothing is read mid-gesture. With `keep` the touch stays down at the last stop, so a tester can `see` or `shot` mid-gesture, until `release` (where it is, or `back` to its start so nothing drops), the next input or 30 s, which both go back first. It's the one finger-path primitive: a drag is two stops (`drag` was retired for it, 2026-10-23), `hold` is the one-stop case kept by name since a long press is its own everyday intent, and `preview-compare` holds its placement with a kept `touch`. Only `swipe` stays apart, since it lifts while moving to fling. The pace can't be set, for the same reason as `double-tap`'s rhythm. On web a kept touch is the mouse button held, lifted only by `release` or the next `touch`.
- **Web-only:** `hover`, `goto` (URL, back, forward), `viewport`, key shortcuts. They're in the interface from day one, and a platform without one refuses it clearly, so a spec never silently skips a step.
- **`batch`:** a sequence of steps that stops at its first failure.
- **`soundcheck drive <platform> <command>`** runs the same commands from a terminal outside a run (Android and iOS; web has no browser to keep between calls), for exploring, reproducing a bug or setting up screenshots. Two more are for the harness itself: `parity` (4.3) and `keyboard hardware|software` (iOS).
- **Tools for mechanics an agent can't do one call at a time:**
  - `double-tap <target>`, which one tap call at a time is too slow to make: two taps 150 ms apart, a person's double-tap. A quick gesture gets its own named tool like this one, never a burst with a count or an interval a spec could set faster than a finger;
  - `network-drop <seconds>`, with the app in the background: nothing passes and nothing closes, as when a phone loses the network in the background. Connections open during the drop stay frozen; new ones get through after it;
  - `offline` on and off: connections are refused and open ones closed, as in airplane mode, until the tester brings the network back. The run brings it back before every spec and after every attempt;
  - on Android and iOS both go through the suite's network proxy (5.2 `network`); web uses Chrome's own offline switch. A suite without a `network` section can't use them on a device, and lint says so (Aled, 2026-10-22: iOS had no way offline, and Android's airplane mode never cut the local stack, since `adb reverse` goes around it);
  - timed `hold` and `swipe`, and `touch` with its rests.
- **`judge`: progress as it happens** (Aled, 2026-10-23). A tester records each outcome with `judge` as soon as it's settled, so a running row fills in one outcome at a time instead of all at once on the post. Judgments are provisional: judging the same outcome again replaces it, and `post_result` takes them all up and files the result, so review and every rule after it are unchanged. Until the coordinator's call, the page shows anything that didn't hold as an orange ?, the same as the Checking cell; red and purple appear only once a result is filed.
- **A person's pace between actions** (Aled, 2026-10-23). Every interaction is one a person could physically and realistically make: the screen responds before the next action. The 2026-10-22 run filed findings for taps a few milliseconds into a drawer's close and for a second row tapped while the list shifted; the app was right each time, and no user does that. So the brief tells testers to act at a person's pace and that a tap lost mid-animation isn't a finding, `batch` starts each input at least 500 ms after the last one ended, and a spec's probes never ask for the impossible. Single gestures keep their own fixed rhythms (`tap`, `double-tap`, `touch`'s pace).
- **Units:** milliseconds everywhere, and absurd values are rejected (an agent once passed ms to a seconds argument and waited 16 hours).

### 4.2 Element model

- Each element has label, value, id, role, frame, tap point, enabled, and checked/selected/expanded/focused.
- **It matches what a screen reader gets.** On Android the reader asks for the tree a screen reader is given: without the flag that includes views marked not important, so Android itself decides importance, as `uiautomator dump --compressed` does. Labels that are only a resource or test id are left out too, since a screen reader never speaks them. `see --all` gives everything. A hidden duplicate label once took a tap meant for the real switch.
  - Filtering on each node's own importance flag doesn't work: on React Native every node reports "not important" (2026-10-21, Solo Scores read empty). Android's own tree is the reliable source.
  - The Android reader also asks for interactive windows, as TalkBack does, so Android counts a node the on-screen keyboard covers as not visible: a tap on it would land on the keyboard. uiautomator doesn't ask, and lists those nodes.
  - Both trees, everything and the screen reader's, leave out nodes that aren't visible to the user, as uiautomator does. That includes a control drawn outside its parent's bounds, which Android reports invisible with an empty frame (Solo Scores' Android text box Done, 2026-10-21): an app bug, since TalkBack can't reach it either.
  - Proven on the parity screen set (2026-10-21), and the default.
  - Expect a few label fixes in the app.
- **Label matching is explicit:** exact by default, with prefix or regex only when asked. An implicit match let `gone "Shuffle Order"` pass while `Shuffle Order…` was on screen.

### 4.3 Platforms

- **Android:** a persistent reader, an instrumentation APK that holds one `UiAutomation` connection and serves the tree over `adb forward`, plus adb for input. `uiautomator dump` is the fallback. Reads take 10–140 ms against about 3.5 s for `uiautomator dump`.
- **iOS:** a persistent Swift reader built on AXe's code, which keeps one simulator accessibility session and one HID session open, for reads and for all input. The HID session is built directly on the DTU transport: `connectToHID()`'s goes dead without an error after any `xcodebuild` that loads a workspace, a build or even `-list`, so taps land nowhere while reads still work (Aled, 2026-10-22, SA-688). There's no fallback for reads either: a reader gap is a bug to fix. The reader links idb's FBControlCore and FBSimulatorControl, which `readers/ios/build.sh` builds from source at the idb fork AXe pins (XcodeGen, about a minute, once per pin), so neither AXe nor its Homebrew tap is needed (2026-10-22). About 0.25 s per read, which is the simulator's own floor, and 0.1 s per tap. It also connects and disconnects the simulator's hardware keyboard (CoreSimulator's `setHardwareKeyboardEnabled`): connected for every spec unless it says `keyboard: software`.
- **Web:** `chromiumoxide` over the Chrome DevTools protocol, with Chrome's accessibility tree as the element model. It's chosen to keep one binary; we write our own waiting and snapshot code. Chrome runs headless; `soundcheck run --headed` shows it in a window to watch (look only: a click or resize can steer a test).
- **Headless devices** (SA-683): drawing device windows loads the Mac (WindowServer was its busiest process on 2026-10-21), so the emulator and simulator run without windows by default too. An emulator soundcheck restarts boots with `-no-window` (still the host GPU), and a simulator needs only `simctl boot`; `--headed` shows the emulator's window and opens Simulator.app on the device. A device already running when the run starts is used as it is. Reads, input, screenshots and the keyboard switch all work headless.
- **Readers** are built by the soundcheck build (their `build.sh` scripts) and embedded in the binary. The server installs, starts and supervises them:
  - a timeout on every call;
  - a wedged reader is restarted;
  - after a device reboot, the forwards are restored;
  - **the iOS reader says who it is:** `/ping` answers `ok <udid> <build id> <pid> <started, unix seconds>`. `build.sh` stamps the build id (a hash of `Reader.swift` and the idb pin) into the binary and writes it to `reader.build-id` beside it. The driver replaces a reader whose build id differs from the one on disk, or that started before the simulator's current boot (its input session answers but lands nowhere). It stops a reader by the pid it reports, or the one listening on its port, and only then by its path: on 2026-10-23 a reader a `cargo test` had started (`crates/driver/../../readers/…`) took the server's taps, which did nothing, because the server looked for its own spelling of the path (`crates/server/../../readers/…`);
  - **a lost channel restarts the simulator** (2026-10-23, five times in one iPad run): a read failing with `Channel disconnected`, or a fresh reader that can't read within a minute (just after a boot it can't for 10–20 s), is mended only by a reboot. The driver stops the reader, reboots the simulator, starts a fresh reader and waits for it to read, at most once in two minutes. The app's state goes with the reboot, so the call that hit it fails with a harness error saying the simulator was restarted and the spec needs a rerun; a spec's reset that hits it relaunches the app instead. The next spec starts on the working reader, and the device's health event says `restarted the simulator: the reader lost its channel (…)`, as it says `restarted the reader: …` for a stale reader.
- **Long-lived at both ends.** The readers keep one accessibility connection open on the device, and the server keeps one connection to each reader, so a step never starts a process. Today's speed-up came from exactly this for reads: each `ui.mjs` call still started Node, ran the guard and set up adb.
- **Input goes through the readers too.** Today every action starts a process: an Android tap is `adb shell input tap` (0.7–0.8 s, and `fill` 2.0 s, measured 2026-10-20), and each iOS touch is a new `axe` process. The readers take input over the same connection instead:
  - **Android (done 2026-10-21):** `UiAutomation.injectInputEvent` inside the reader (`/input` scripts in the iOS reader's step format, `/type` through the device's key map), so taps, drags and holds cost milliseconds, and a `double-tap`'s gap is timed on the device rather than across adb round trips. No `adb shell input` fallback; an older reader on the device is replaced;
  - **iOS (done 2026-10-21):** the reader keeps the HID connection open for touches and typing, as it does for reads: a tap fell from about 1.3 s through `axe` to 0.1 s, and a `fill` from 15–58 s to 3–5 s. There's no `axe` fallback for input: a gap is a bug to fix.
  - **Taps keep a person's timing, and sub-frame races are out of scope** (Aled, 2026-10-21, SA-673). On the SA-437 bug build (a back tap blanking the app when press-in and release land in one frame), raw `adb input tap`s and injected taps held 0 or 10 ms broke it within three rounds, while taps held 20 ms or more (the reader's `tap` holds 80) passed 100 rounds; on the fixed build, 0 ms taps passed 60. So injected input reproduces such a race only with an unnaturally short touch. A regression suite isn't expected to catch a bug that edge, so taps stay natural rather than chasing it, and `tap-burst` became `double-tap` with a fixed 150 ms rhythm, since an 80 ms double Undo once left a state no finger could reach.
- **Parity tests:** every reader against the platform's own tool (`uiautomator`, Chrome's snapshot; iOS has none since AXe went, and AXe read through the same idb code anyway), on a fixed screen set that includes overlays, sheets, dialogs, menus and screens with the keyboard up. A reader change ships only after it passes.
  - `soundcheck drive android parity` compares the current screen, both the full tree and the screen reader's (`uiautomator dump` and `--compressed`): elements pair by label and nearest frame, then frame (within 2 units), value and states must agree. On Android, what only uiautomator lists under the keyboard is set aside (above).
  - **The screen set** (Solo Scores, 2026-10-21): a collection's home, the app drawer, a header menu, a settings sheet, the same sheet with the keyboard up on a field, a destructive confirmation dialog, and markup mode with a text box open and the keyboard up. Both platforms had parity on every screen.
  - It found two bugs: Android's reader showed invisible nodes in its full tree (a closed drawer's full-screen "Close drawer"), and the Android parser lost every value uiautomator single-quotes (any text holding a `"`).
  - **A reader on the device is always this build's:** connecting compares the installed APK's hash with the built one and reinstalls on a difference, so a reader change can't be shadowed by an older one still installed.

### 4.4 Ruled out

- **Appium:** temperamental, and it would replace the evidence layer for little gain. Our Android reader is the same idea Appium uses underneath, without WebDriver.
- **Maestro:** it waits for the UI to settle, which hid a real race (SA-437 passed 20 of 20 under Maestro on the bug build, while raw taps broke it). That race turned out to need a near-instant touch, which soundcheck doesn't chase either (4.3), but settling still hides ordinary timing a person produces.
- **Detox:** it needs test hooks compiled into the app, so it can't test the shipped build.
- **`idb_companion`:** breaks with new Xcode releases; AXe already worked.
- **Playwright for web:** mature, but a Node sidecar, and its auto-waiting is the settling behaviour that hid the race above.
- **A `stress` tool for device load** (removed 2026-10-21): a hammered device makes any app sluggish, and a test can only confirm that, with no bug behind it to guard. Busy loops on an emulator aren't a slow phone either (memory, GPU, storage, heat). If a load-dependent bug ever turns up: start the loops with `setsid nohup timeout <s> sh -c 'while :; do :; done' &` (plain `&` dies with the adb shell, which is why the first version never loaded anything), return at once so the spec acts under the load, and end leftovers before the health sample so they don't read as a degraded device.

---

## 5. Project integration

### 5.1 Layout

```
<project skill>/
  SKILL.md                     # accounts, device setup and quirks; takes a scope, delegates the run to soundcheck:run
  suite.yaml                   # with `specs: suite`
  hooks/                       # the project's hook scripts, each self-contained
  fixtures/                    # files testers can push onto a device
  suite/                       # only the specs
    <feature>/
      _feature.md              # shared context: how to get there, label hints, platform differences
      001-<name>.spec.md
```

Scaffolding sits beside `suite.yaml`; the specs folder holds nothing but features, so it reads as the description of what's tested.

### 5.2 `suite.yaml`

- `soundcheck: <version>`: the pin (section 8).
- `platforms`:
  - app IDs per platform;
  - for web: base URL, a start hook, browsers and viewports;
  - the account each platform signs in with;
  - `ready`: labels that prove the launched app is ready to test (signed in, past onboarding), checked by the preflight.
- `hooks`: the commands in 5.3.
- `specs`: the folder holding the feature folders (default: the suite folder itself).
- `seeds`: the seed scenario names the `seed` hook accepts, so lint can check every spec's seed.
- `reports`: where runs are written.
- `models`: the default tester model (Sonnet).
- `parallel`: false by default.
- `tickets`: how to link a ticket ID.
- `network`: optional. `proxy` lists routes, each `{ name, listen, upstream }`: the app's test build reaches `listen`, and `upstream` really serves it. Soundcheck knows ports only; which services exist and why is the project's business. `soundcheck proxy serve` runs the proxies (a run or `soundcheck drive` starts it, detached, when it isn't up, and it keeps running between runs so the test build always reaches its services); `status` and `stop` manage it. Android forwards each `listen` port with `adb reverse` to the same port; the iOS simulator shares the Mac's ports. One proxy serves every platform, so a suite with `network` can't run platforms in parallel.

### 5.3 Hooks

Hooks are shell commands, so a project writes them in anything. The contract is exit code 0 for ok, plus JSON on stdout where data comes back. The project resets data; the driver launches the app, because launching is generic.

| Hook                  | Called                                                              | Returns                                                |
| --------------------- | ------------------------------------------------------------------- | ------------------------------------------------------ |
| `guard`               | Run start, and before every seed                                    | ok, or a refusal reason on stderr                      |
| `seed {scenario}`     | Before every spec (always full reset)                               | optional `{ "context": {...} }` passed into the prompt |
| `build-identity`      | Run start and after installs (gets the driver's app facts on stdin) | `{ "commit", "fingerprint", "version" }` per platform  |
| `install {platform}`  | Run start, for a platform whose build can't be identified (every platform with `--install`); a coordinator's `install_build` with `install: true` | ok                                                     |
| `check {name} [args]` | When a tester calls `check-data`                                    | JSON evidence (e.g. a database check)                  |

Every spec starts from its own full seed: no skipping and no snapshots. A spec never relies on state another spec left behind, so a rerun is independent and results can't depend on run order.

### 5.4 Spec format

```markdown
---
title: Draw a pen stroke, undo and redo it
seed: viewer
starts-on: ["Annotate"] # checked after the seed; a failure is a reset error, not a result
platforms: [android, ios, web]
model: sonnet # optional; ask for a stronger model for visual judgement
tools: [double-tap] # driver tools beyond the basics, if any
past-bugs: [SA-611]
proven-failing-on: []
---

## Goal

Draw a level stroke with the pen, undo it, redo it.

## Outcomes

- The stroke disappears on undo and returns in the same place on redo.
- Undo and Redo enable and disable to match.

## Probe around it

Undo straight after the first stroke; undo twice quickly.
```

- **Every spec is isolated:** its own seed, its `starts-on` check, and no reliance on another spec, so it can run alone, in any order, or again. Lint requires both.
- **The path is the identity and the order** (`annotations/001-draw`). A renamed spec starts a fresh history.
- **Sections:** `## Goal` and `## Outcomes` (a `- ` list) are required; `## Probe around it` is optional, and so is `## Pinned`: exact steps, only where they guard a past bug or reach a control an agent can't discover.
- **Each outcome is judged on its own** (held, failed, unsure, couldn't check), with evidence; the spec's result follows from them (3.3).
- **A seen bug gets a regression spec:** more specific instructions, plus a probe around the old failure. Mechanics an agent can't do are driver tools the spec names; boundaries are seed scenarios.
- **The title is the user's goal.** No tap paths or tool commands.
- **Outcomes are what a user would notice.** Exact copy only for destructive or blocking messages, and no layout details a design tweak would break.
- **Platform differences go in `_feature.md` hints,** not separate specs.

### 5.5 Lint

`soundcheck lint <suite>` checks:

- frontmatter is valid;
- the seed scenario exists;
- IDs are unique;
- platforms are known;
- the tools exist in the driver;
- titles follow the rules;
- every spec has a Goal, Outcomes, a seed and a `starts-on` check.

`create-test` runs it after every edit, and `run` refuses an invalid suite.

---

## 6. Plugin

The soundcheck repo ships as a Claude Code plugin:

- **The `soundcheck` binary,** built on install.
- **MCP config** for the local server.
- **The `soundcheck-tester` agent type,** whose only tools are soundcheck's MCP tools and Read (for its own screenshots). It can't read app source, run shell or edit files.
- **Skills:**
  - **`setup`:** interviews the person, then scaffolds `suite.yaml`, hook stubs and a draft project skill. On a suite pinned to an older version, it migrates the suite and shows the diff for review.
  - **`run`:** the generic coordinator procedure: start, dispatch background testers, review while the next spec runs, close, export. A project skill passes the scope it was given (platforms, specs or features) and delegates to it; choosing the scope belongs to whatever calls it, such as a release process.
  - **`create-test`:** add or change specs. Search for duplicates first, turn a found bug into a regression spec, add a seed scenario when needed, and finish with lint.
  - **`drive`:** use a device by hand outside a run (look at a screen, reproduce a bug, check a change, set up a screenshot) through `soundcheck drive`. It adds nothing `drive` can't already do: it's there so a session asked to look at the app finds the commands without loading a regression skill and treating a look as a test run (Aled, 2026-10-23).

---

## 7. Verdict rules

- **Any open FAIL blocks the release,** whichever spec it's on. There are no special gate tests.
- **A BLOCKED** goes through review and triage like a FAIL. One blocked by a harness gap, not the app, goes in the run's notes, and its spec counts as not run until a rerun passes.
- **Verdict:**
  - **Failing:** any open failure. (Not "Blocked", which a row means when the environment or harness stopped its test.)
  - **Not ready:** an open (untriaged) problem, specs not run, or results still under review.
  - **Ready:** otherwise.
- **BLOCKED means the environment or harness stopped the test, never the app** (Aled, 2026-10-21). It isn't a result about the app: the row shows purple with one line saying why, counts as not tested until a rerun passes, and keeps the verdict at Not ready. Reset failures, stuck devices, missing labels and tool gaps are all BLOCKED. If the app stopped the test, it's a FAIL.
- **Problems are app bugs and findings only.** Harness gaps and device glitches go in the run's notes.
- **Triage outcomes** (3.3):
  - **Dismissed:** not a bug, or not reproduced. A problem that only passed on a rerun is Dismissed, never Fixed.
  - **Fixed:** a fix landed. Needs a ticket.
  - **Deferred:** accepted for this release. Needs a ticket.
- **Failure or finding, never a severity** (Aled, 2026-10-21). A **failure** blocks the release until it's triaged: every upheld FAIL, including a test the app stopped. A **finding** doesn't block but still needs a decision: side finds, "not reproduced" ones, UX or accessibility observations. "Not exactly the test case, but something to think about." The kind follows from where the problem came from, so the coordinator never grades it, and a confirmed FAIL can't be labelled anything milder.
- **Duplicates fold into the original** (`duplicateOf`).

---

## 8. Versioning

- The suite pins a version in `suite.yaml`. The server refuses a suite pinned to an incompatible version, and `setup` offers the migration.
- The plugin, binary and skills share one version, so a skill and the server never drift apart.
- A soundcheck upgrade never lands mid-release.

---

## 9. Build order

Built against Solo Scores, which proves the concept; there's no separate demo app. The harness's own tests are unit tests against a fake driver, plus recorded device trees for the parsers and parity checks.

1. **Server core (device-free):**
   - Cargo workspace;
   - spec loader, `suite.yaml` schema, lint;
   - journal and row states with their rules;
   - verdict logic;
   - unit tests against a fake driver.
2. **Suite migration:** convert Solo Scores' guides into specs with `create-test` and lint, applying its cleanup backlog in the same pass.
3. **MCP and live page:** the HTTP server, testers' and coordinator's tools, the event stream, the live page. The device tools call Solo Scores' existing `ui.mjs` underneath, so a full run works end to end.
4. **Rust drivers:** Android and iOS behind the interface, with embedded readers, the element model, parity tests, `check`, `batch` and the mechanic tools. They replace `ui.mjs` behind the same MCP tools.
5. **Report port and export.**
6. **Web driver** with its parity test. Web is also the natural first target for CI.
7. **First real run** when ready. It's Solo Scores' next focus.
8. **Later:** parallel platforms, headless dispatch through the Agent SDK, phone notifications off the event stream, a second app.

---

## 10. Open questions

- **Licence:** MIT (decided 2026-10-21, `LICENSE`); the iOS reader follows AXe's MIT code.
- **Port:** 7357 by default. The plugin's MCP server is `soundcheck mcp`, a stdio bridge that lists the tools from the start and forwards each call to the run serving on the port, so a session opened before `soundcheck run` needs no reconnect (decided 2026-10-21, after the first run).
- **The tester's silence timeout** before a running spec is treated as stopped.
- **Web start hook:** a local dev server or a preview build, and how its build identity is read.
- **Android overlay gap** (answered 2026-10-21): neither timing nor the long-lived connection. The controls are drawn outside their parent's bounds, so Android reports them invisible with an empty frame to uiautomator as well; an app fix (4.2).
- **The parity screen set** (answered 2026-10-21, 4.3): reached by hand with `drive`, since how to reach screens is the app's knowledge; a project could script it from its specs later.
