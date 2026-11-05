---
title: Add items to a list
platforms: [android, ios]
prepare:
  - sh scripts/seed.sh kitchen
---

## Goal

Add `Lemons`, `Rice` and `Coffee` from the bar at the bottom of the list.

## Outcomes

- Each item appears at the end of the unchecked items as it's added.
- The bar stays focused, so the next item can be typed straight away.
