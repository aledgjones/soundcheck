---
title: Delete a list
platforms: [android, ios]
prepare:
  - sh scripts/seed.sh kitchen
---

## Goal

Delete `Weekly shop` from its menu.

## Outcomes

- Deleting asks for confirmation, naming the list.
- The list is gone from the side menu, and the app shows the empty-account prompt.
