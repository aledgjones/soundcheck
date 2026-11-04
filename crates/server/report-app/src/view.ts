// The page's view as the server sends it (crates/server/src/report/view.rs). Optional fields are left out, never
// null, since a null in a merge patch deletes its key.

export interface View {
  label: string;
  version: string;
  platforms: string[];
  bar?: Bar;
  facts: Fact[];
  export: boolean;
  summary: Summary;
  sections: Section[];
  checks: Record<string, Check>;
  problems: Record<string, Problem>;
  groups: Group[];
  notes?: Notes;
  /** By journal seq. */
  timeline: Record<string, Line>;
}

export type Bar =
  | { kind: "now"; paused: boolean; doing: Doing[]; idle?: Overdue; timer?: Timer }
  | { kind: "starting"; step?: Step; failed?: string };

export interface Doing {
  platform: string;
  spec: string;
  checking?: Overdue;
}

/** Shown only from `warn_ms` after `since`, as `label` with `{t}` replaced by the time passed. */
export interface Overdue {
  since: number;
  warn_ms: number;
  label: string;
}

export interface Timer {
  active_ms: number;
  since?: number;
  cap_ms: number;
}

export interface Step {
  text: string;
  ago: Overdue;
}

export interface Fact {
  text: string;
  title?: string;
  at?: number;
}

export interface Summary {
  pct: number;
  ring: Segment[];
  class: string;
  word: string;
  reasons: { text: string; bad: boolean }[];
  grid: GridRow[];
}

export interface Segment {
  class: string;
  dash: string;
  offset: string;
}

export interface GridRow {
  feature: string;
  name: string;
  cells: Cell[];
}

export interface Cell {
  class: string;
  text: string;
  /** Percent widths of pass, fail, blocked, checking and running. */
  bars?: [number, number, number, number, number];
}

export interface Section {
  id: string;
  name: string;
  specs: string[];
}

export interface Check {
  anchor: string;
  number: string;
  title: string;
  attn: boolean;
  open: boolean;
  pill?: Chip;
  cells: ResultCell[];
  bodies: Body[];
}

export interface Chip {
  class: string;
  text: string;
}

export interface ResultCell {
  platform: string;
  class: string;
  text: string;
  title?: string;
}

export type Body =
  | {
      kind: "pending";
      platform: string;
      label: boolean;
      state: string;
      running: boolean;
      outcomes: PendingOutcome[];
    }
  | {
      kind: "detail";
      platform: string;
      label: boolean;
      failure?: Failure;
      blocked?: { expect: string; saw: string };
      findings: FindingBlock[];
      outcomes: Outcome[];
      evidence?: Evidence;
    };

export interface PendingOutcome {
  text: string;
  judged?: { class: string; mark: string; observed: string };
}

export interface Failure {
  call?: string;
  shots: Shot[];
  /** No outcome reads as failed to hold it, so it gets its own red box. */
  alone: boolean;
}

export interface FindingBlock {
  id: number;
  title: string;
  detail: string;
  shots: Shot[];
}

export interface Outcome {
  class: string;
  mark: string;
  label: string;
  text: string;
  observed: string;
  settled?: string;
  boxed: boolean;
  failure: boolean;
}

export interface Shot {
  name: string;
  src?: string;
}

export interface Evidence {
  shots: Shot[];
  texts: { name: string; label: string; href?: string; text?: string }[];
  notes?: string;
}

export interface Problem {
  number: string;
  badge?: Chip;
  title: string;
  place: string;
  detail: string;
  shots: Shot[];
  paras: string[];
  row: string;
}

export interface Group {
  title: string;
  ids: string[];
}

export interface Notes {
  count: number;
  rows: { title: string; lines: string[] }[];
}

export interface Line {
  at: number;
  text: string;
}
