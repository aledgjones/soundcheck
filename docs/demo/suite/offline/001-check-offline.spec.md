---
title: Check items off with no connection
platforms: [android, ios]
prepare:
  - sh scripts/seed.sh kitchen
tools: [offline]
---

## Goal

Take the app offline, check off `Eggs` and `Oat milk`, then bring it back online.

## Outcomes

- Offline, the items check off as usual.
- Back online, they stay checked, and a second device sees them checked.
