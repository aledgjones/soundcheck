# Signed in at every reset (approved 2026-11-04)

Replaces `prepare`. Every spec starts from its own seed, and the seed now includes who is signed in, so no state is carried between specs. Sign-in itself gets specs of its own. Facts from solo-scores are cited by file.

## Shape in soundcheck

Folded into DESIGN.md: 3.11 "Signed in at every reset" (the `session` shapes, why `prepare` went, the open `next` question), 5.3 (the seed hook's `session`) and 3.2 (where the reset applies it). `prepare`, its dispatch and its `done` checks go, and `soundcheck ready` keeps only its host-hold wait. What follows is Solo Scores' side, for SA-884 and its tickets.

Solo's session link: `{ "link": "sync://e2e/session?…" }`.

## Minting (solo-scores hooks, no backend change)

- Refresh tokens are 32 random bytes stored as a SHA-256 hash in `auth_refresh_tokens` (sync-backend/src/services/auth.ts:270-295). The seed hook already has the stack's database, so it inserts a fresh token for the account and returns it. The app refreshes on launch to get its 15-minute sync token through the normal `/auth/refresh`.
- The stack's signing key isn't needed, and no route is added: the production backend gains nothing.
- The account's user row already exists with a deterministic id (stack.mjs:171-195 `addAccount`).

## App entry (sync-frontend)

- The session lives in SecureStore `sync-session` + `sync-identity`, written only by `startSession` (lib/powersync/session.ts:184-217). On launch the stored identity alone decides "signed in" (services/auth.tsx:238-293).
- Signed out, no route mounts (auth.tsx:437-451), so the entry is `app/+native-intent.tsx` or a URL listener in `AuthProvider`, not a screen.
- It wipes local data first (`clearLocalData`, db.ts:86-91, as a sign-out does), then writes identity and session, then sets the user. A wipe on every spec is the Maestro `clearState` pattern: no replica survives from another spec or run. Cost: a first sync of a seeded account per spec, to be measured.

## Web entry (sync-site)

- The refresh token is the HttpOnly cookie `refresh_token` (`persistent.<token>`, path `/auth`, secure, sameSite strict; routes/auth.ts:70-83). JS can't set it, but DevTools can, and Chrome treats localhost as secure. The app then refreshes through `/auth/web/refresh` as after any reload.
- To check: whether the app needs `solo-scores.user` / `solo-scores.db-owner` in localStorage to try the refresh, which `storage` covers.
- If that works, web has no shortcut code at all.

## Never in a release

A shortcut in a release would let any link sign a user in to someone else's account. Layers, each enough on its own:

1. **Not in the bundle.** The entry is its own module, swapped for a stub by Metro's resolver unless the build is an e2e build. A release bundle has no code for it, not just a false flag.
2. **An e2e build is defined by where it points**, not by a flag that can leak. The resolver swaps the module in only when `EXPO_PUBLIC_API_URL` is a localhost URL. A production build or OTA update points at production, so it can't carry the module whatever the shell or `.env` holds. Today's guard (metro.config.js:4-11) only checks `EAS_BUILD`, so `eas update` bundled locally passes it.
3. **Checked after building.** A script greps the release bundle (and sync-site's dist) for the module's marker string and fails the build. It runs from `eas-build-on-success`, before `eas update`, and in sync-site's CI.
4. **Refused at runtime.** The entry also refuses unless the API URL is localhost.
5. **Nothing to mint against production.** No backend route exists; a session needs the stack's database.

Side find: layers 2 and 3 fix the same gap for `EXPO_PUBLIC_ALLOW_TEST_STORE_IN_RELEASE`.

## Sign-in specs (from today's prepare.md)

- `account/0xx-sign-in-google`: platforms ios, android, web. Seed with no session; sign in through Google's real screens (today's prepare steps), see the library, `Up to date`.
- `account/0xx-sign-in-apple`: the same through Apple. Needs an Apple ID signed in on the simulator, and Apple sign-in configured in the stack (to check); on Android and web it's Apple's web popup.
- `account/007-plan-survives-sign-out` keeps its real sign-out and switching, and no longer risks the specs after it.
- Specs never type into Google's or Apple's screens; anything beyond choosing the account fails the spec.
