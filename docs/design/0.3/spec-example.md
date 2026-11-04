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

0.3:

```yaml
---
title: See the invites waiting for you
seed: invites+library
platforms: [android, ios, web]
start: launch
---
```

- `start` is required: `launch` (a fresh launch on every platform) or a link per platform the spec lists.
- Signed in or not is the seed's: `invites+library` returns a session for the account, so the spec starts signed in with nothing in its frontmatter. A signed-out spec uses a seed that returns none.
- `viewport` goes: web starts at the platform's `1440x900`. The two reference-image specs keep `viewport: 1440x1500`.

A spec that starts deeper names a link for each platform:

```yaml
start:
  ios: sync://collection/{E2E Seed Library Collection}
  android: sync://collection/{E2E Seed Library Collection}
  web: /app/collection/{E2E Seed Library Collection}
```
