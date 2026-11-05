# Spec frontmatter, 0.2 → 0.3

`sharing/001-see-invites` (starts on the library, and its first outcome is a dot on the drawer button).

0.2:

```yaml
---
title: See the invites waiting for you
seed: invites+library
platforms: [android, ios, web]
viewport: 1000x800
---
```

0.3 (DESIGN.md 5.4, 5.6):

```yaml
---
title: See the invites waiting for you
platforms: [android, ios, chrome]
prepare:
  - node scripts/seed.js invites+library
  - node scripts/sign-in.js
---
```

- `seed` and `start` go: the data, who is signed in and where it starts are `prepare` steps, project scripts soundcheck runs after a fresh relaunch. With no `open` step the spec starts wherever the signed-in app lands after launch.
- A signed-out spec leaves out `sign-in.js`.
- `viewport` goes: chrome starts at the platform's `1440x900`. The two reference-image specs keep `viewport: 1440x1500`.
- Solo's `web` platform is now `chrome` (SA-884), in `platforms:` and line tags.

A spec that starts deeper opens a link per platform, with a value the seed script printed (`{"values": {"COLLECTION": "…"}}`):

```yaml
prepare:
  - node scripts/seed.js invites+library
  - node scripts/sign-in.js
  - ios: soundcheck drive ios open "sync://collection/$COLLECTION"
    android: soundcheck drive android open "sync://collection/$COLLECTION"
    chrome: soundcheck drive chrome open "/app/collection/$COLLECTION"
```
