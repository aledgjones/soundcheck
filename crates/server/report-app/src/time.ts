import { useEffect, useState } from "preact/hooks";

function pad(n: number): string {
  return String(n).padStart(2, "0");
}

/** A clock time in the viewer's zone, with the day for `date`. */
export function clock(at: number, date = false): string {
  const d = new Date(at);
  const hm = `${pad(d.getHours())}:${pad(d.getMinutes())}`;

  if (date) {
    return `${d.toLocaleDateString(undefined, { day: "numeric", month: "short" })}, ${hm}`;
  }

  return hm;
}

/** 12 s, 4 min, 1 h 5 min. */
export function span(ms: number): string {
  const min = Math.floor(ms / 60000);

  if (min < 1) {
    return `${Math.max(0, Math.floor(ms / 1000))} s`;
  }

  if (min < 60) {
    return `${min} min`;
  }

  return `${Math.floor(min / 60)} h ${min % 60} min`;
}

/** 4:05, or 1:04:05 past an hour. */
export function stopwatch(ms: number): string {
  const s = Math.floor(ms / 1000);
  const h = Math.floor(s / 3600);
  const rest = `${pad(Math.floor(s / 60) % 60)}:${pad(s % 60)}`;
  return h > 0 ? `${h}:${rest}` : rest;
}

/** Fills server-rendered `<time data-at>` elements (the runs list) with the viewer's clock. */
export function formatTimes(root: ParentNode): void {
  for (const t of root.querySelectorAll<HTMLElement>("time[data-at]")) {
    t.textContent = clock(Number(t.dataset.at), t.dataset.format === "date");
  }
}

// One timer for the page; only what shows a running time listens, so the rest never re-renders on a tick.
const listeners = new Set<(now: number) => void>();
let ticker: ReturnType<typeof setInterval> | undefined;

export function useNow(): number {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    listeners.add(setNow);

    if (ticker === undefined) {
      ticker = setInterval(() => {
        const at = Date.now();
        for (const listener of listeners) {
          listener(at);
        }
      }, 1000);
    }

    return () => {
      listeners.delete(setNow);

      if (listeners.size === 0) {
        clearInterval(ticker);
        ticker = undefined;
      }
    };
  }, []);

  return now;
}
