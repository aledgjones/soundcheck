# Suite migrations

What changes in a suite between soundcheck versions, so the setup skill can bring a pinned suite up to date. Each entry
names the version it migrates to, what to change, and how to check it. The newest is first.

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
