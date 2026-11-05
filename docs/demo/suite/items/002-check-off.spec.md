---
title: Check items off
platforms: [android, ios]
prepare:
  - sh scripts/seed.sh kitchen
---

## Goal

Check off `Eggs` and `Oat milk`, then uncheck `Eggs`.

## Outcomes

- A checked item is struck through and sinks to the end of the list.
- An unchecked item returns to its place among the unchecked items.
