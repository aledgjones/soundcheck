---
title: Rename a list
platforms: [android, ios]
prepare:
  - sh scripts/seed.sh kitchen
---

## Goal

Rename `Weekly shop` to `Saturday shop` from the list's menu.

## Outcomes

- The list's title and the side menu both show `Saturday shop`.
- After a relaunch the new name is kept.
