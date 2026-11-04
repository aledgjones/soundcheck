import type { View } from "./view";

const FIRST_RETRY_MS = 1000;
const LAST_RETRY_MS = 30_000;
/** The server pings every 15 s; this long without any event means the link is dead though nothing said so. */
export const QUIET_MS = 35_000;

export interface Feed {
  onView: (view: View) => void;
  onPatch: (patch: unknown) => void;
  /** True once connected, false on any error or silence; a resync from a whole view follows every reconnect. */
  onConnected: (connected: boolean) => void;
}

/**
 * Follows the server's `updates` Server-Sent Events: `view` on every connect, then `patch`es and `ping`s. EventSource
 * reconnects by itself after a drop, but gives up for good on a non-200 answer (an expired sign-in, another run on the
 * port), so then a new one is made after a wait that doubles each time. Returns a stop.
 */
export function follow(url: string, feed: Feed): () => void {
  let source: EventSource | undefined;
  let retry: ReturnType<typeof setTimeout> | undefined;
  let quiet: ReturnType<typeof setTimeout> | undefined;
  let wait = FIRST_RETRY_MS;

  function heard(): void {
    clearTimeout(quiet);
    quiet = setTimeout(() => {
      feed.onConnected(false);
      // A connection that stays silent may never error by itself, so start a fresh one.
      reopen(0);
    }, QUIET_MS);
  }

  function reopen(after: number): void {
    source?.close();
    clearTimeout(quiet);
    clearTimeout(retry);
    retry = setTimeout(open, after);
  }

  function open(): void {
    const next = new EventSource(url);
    source = next;
    next.onopen = () => {
      wait = FIRST_RETRY_MS;
      feed.onConnected(true);
      heard();
    };
    next.onerror = () => {
      feed.onConnected(false);

      if (next.readyState === EventSource.CLOSED) {
        reopen(wait);
        wait = Math.min(wait * 2, LAST_RETRY_MS);
      }
    };
    next.addEventListener("view", (e) => {
      feed.onView(JSON.parse((e as MessageEvent<string>).data) as View);
      feed.onConnected(true);
      heard();
    });
    next.addEventListener("patch", (e) => {
      feed.onPatch(JSON.parse((e as MessageEvent<string>).data));
      heard();
    });
    next.addEventListener("ping", () => {
      heard();
    });
  }

  open();
  return () => {
    source?.close();
    clearTimeout(quiet);
    clearTimeout(retry);
  };
}
