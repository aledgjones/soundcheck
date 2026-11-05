# Scripted prepare (folded into DESIGN.md 5.6, 2026-11-05)

The design now lives in DESIGN.md: 5.6 (prepare), 5.4 (the spec format), 3.2 (the reset), 3.11 (signing in). This file keeps how it was reached.

1. **Seed + `start` + the seed's `session`** (2026-11-04): three fixed slots, each guessing at Solo's shape. Dropped: "it smells like the wrong abstraction" (Aled).
2. **Recipes** (named per-platform step lists in suite.yaml, specs referring to them by name): dropped as mysterious aliases. "The abstraction is the script, not the spec definition."
3. **Scripted prepare** (agreed): a spec lists runnable commands; a map per platform where they differ, `true` for nothing; soundcheck runs them and is the compatibility layer through `soundcheck drive <platform> <tool>`, always the long form. Seeds are the project's concern, not soundcheck's.

Settled with it (Aled, 2026-11-05):

- Timeout: `soundcheck run --prepare-timeout <seconds>`, default 60, one for the whole run; no per-step override.
- soundcheck relaunches once, before the first step; a step that needs another runs `relaunch` itself.
- Values a step prints become environment variables for later steps, filled by the shell; no `{name}` templating, no `SOUNDCHECK_VALUES` blob. The journal records commands as written.
- Browser-only tools (`cookies`, `storage`) fail on other drivers with why; a Firefox driver would implement them too.
- The browser driver is `chrome` everywhere in soundcheck (`DriverKind::Chrome`); Solo renames its `web` platform to `chrome` (SA-884).
