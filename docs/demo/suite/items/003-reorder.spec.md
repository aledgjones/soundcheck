---
title: Reorder items by dragging
platforms: [android, ios]
prepare:
  - sh scripts/seed.sh kitchen
---

## Goal

Drag `Bread` to the top of the list by its handle.

## Outcomes

- The item follows the finger and the others make room.
- The new order is kept after a relaunch.

## Probe around it

Drag an item down past the bottom of the screen.
