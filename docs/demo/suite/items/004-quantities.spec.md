---
title: Set an item's quantity
platforms: [android, ios]
prepare:
  - sh scripts/seed.sh kitchen
---

## Goal

Open `Eggs` and set its quantity to 12 with the stepper.

## Outcomes

- The list row shows `Eggs × 12`.
- The stepper can't go below 1.
