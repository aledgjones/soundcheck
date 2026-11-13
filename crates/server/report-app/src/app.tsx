import { Component, type ComponentType, createContext, type FunctionComponent, h } from "preact";
import { useContext, useEffect, useRef, useState } from "preact/hooks";
import { follow } from "./follow";
import { applyPatch } from "./patch";
import { clock, formatTimes, span, stopwatch, useNow } from "./time";
import type * as V from "./view";

type Theme = "auto" | "light" | "dark";

function stored(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function store(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {}
}

/** Re-renders only when a prop changes by identity: a merge patch keeps every untouched part's identity. */
function memo<P extends object>(inner: FunctionComponent<P>): ComponentType<P> {
  return class Memo extends Component<P> {
    shouldComponentUpdate(next: P): boolean {
      const before = this.props as Record<string, unknown>;
      const after = next as Record<string, unknown>;
      const keys = new Set([...Object.keys(before), ...Object.keys(after)]);
      for (const key of keys) {
        if (before[key] !== after[key]) {
          return true;
        }
      }

      return false;
    }

    render() {
      return h(inner, this.props);
    }
  };
}

// Only what the viewer opened or closed keeps that state; everything else follows the view, so a new failure opens
// itself. Kept outside the components so a row that moves keeps it too.
const chosen = new Map<string, boolean>();

function useDetails(key: string, fallback: boolean): [boolean, (e: Event) => void] {
  const [, redraw] = useState(0);
  const open = chosen.get(key) ?? fallback;
  function onToggle(e: Event): void {
    const now = (e.currentTarget as HTMLDetailsElement).open;

    // Setting `open` from the view fires toggle too; only a change away from what was drawn is the viewer's.
    if (now !== open) {
      chosen.set(key, now);
      redraw((n) => n + 1);
    }
  }

  return [open, onToggle];
}

interface LightboxShots {
  shots: V.Shot[];
  at: number;
  opened: number;
}

const ShowShot = createContext<(shots: V.Shot[], at: number) => void>(() => {});

function Time({ at, date = false }: { at: number; date?: boolean }) {
  return <time dateTime={new Date(at).toISOString()}>{clock(at, date)}</time>;
}

function Ago({ o }: { o: V.Overdue }) {
  const now = useNow();
  const passed = now - o.since;
  const hidden = passed < o.warn_ms;
  return (
    <span class={o.warn_ms > 0 ? "ago warn" : "ago"} hidden={hidden}>
      {hidden ? "" : o.label.replace("{t}", span(passed))}
    </span>
  );
}

function Elapsed({ timer }: { timer: V.Timer }) {
  const now = useNow();
  // Counting on from the last entry, unless the run has gone quiet for longer than counts as working.
  const quiet = timer.since === undefined ? 0 : now - timer.since;
  const counting = quiet <= timer.cap_ms ? Math.max(0, quiet) : 0;
  return (
    <span class="nowbar-time elapsed" title="Time the run has been working, without the gaps between sessions">
      {stopwatch(timer.active_ms + counting)}
    </span>
  );
}

function LiveDot({ word }: { word: string }) {
  return (
    <span class="nowbar-live">
      <i class="live-dot" aria-hidden="true"></i>
      <b>{word}</b>
    </span>
  );
}

/** `connected` is false from the moment the feed drops until the server sends a whole view again. */
function NowBar({ bar, connected }: { bar: V.Bar; connected: boolean }) {
  let live;
  let doing;
  let timer = null;

  if (bar.kind === "starting") {
    if (bar.failed !== undefined) {
      live = <span class="nowbar-live warn">Couldn't start</span>;
      doing = <span class="start-failed">{bar.failed}</span>;
    } else if (bar.step !== undefined) {
      live = <LiveDot word="Starting" />;
      doing = (
        <span>
          <b>{bar.step.text}</b> <Ago o={bar.step.ago} />
        </span>
      );
    } else {
      live = <LiveDot word="Starting" />;
      doing = <span class="muted">Checking</span>;
    }
  } else {
    live = bar.paused ? <span class="nowbar-live warn">Paused</span> : <LiveDot word="Live" />;
    doing = (
      <>
        {bar.doing.length === 0 ? <span class="muted">Nothing running</span> : null}
        {bar.doing.map((d) =>
          d.checking === undefined ? (
            <span key={`${d.platform} ${d.spec}`}>
              {d.platform} · <b>{d.spec}</b>
            </span>
          ) : (
            <span key={`${d.platform} ${d.spec}`}>
              {d.platform} · checking <b>{d.spec}</b> <Ago o={d.checking} />
            </span>
          ),
        )}
        {bar.idle !== undefined ? <Ago o={bar.idle} /> : null}
      </>
    );

    if (bar.timer !== undefined) {
      timer = <Elapsed timer={bar.timer} />;
    }
  }

  if (!connected) {
    live = <span class="nowbar-live warn">Not connected · retrying</span>;
  }

  return (
    <div class="nowbar" role="status">
      <div class="nowbar-in">
        {live}
        <div class="nowbar-doing">{doing}</div>
        {timer}
      </div>
    </div>
  );
}

/** A remote viewer comes through the tunnel's own host and is read-only. */
function onThisMachine(): boolean {
  return typeof location !== "undefined" && (location.hostname === "127.0.0.1" || location.hostname === "localhost");
}

/** Deletes the run this page shows once the viewer confirms, then goes to the runs list (SA-984). */
async function deleteRun(): Promise<void> {
  if (!confirm("Delete this run and its screenshots? This can’t be undone.")) {
    return;
  }

  try {
    // Relative, so it names this page's own run.
    const response = await fetch(".", { method: "DELETE" });

    if (!response.ok) {
      throw new Error((await response.text()).trim() || `the server answered ${response.status}`);
    }

    location.assign("/");
  } catch (e) {
    alert(`Couldn’t delete the run: ${e instanceof Error ? e.message : String(e)}`);
  }
}

function Header({ view, onRuns, canDelete }: { view: V.View; onRuns: () => void; canDelete: boolean }) {
  return (
    <header class="head">
      <div class="head-text">
        <div class="head-top">
          {view.export ? null : (
            <button class="runs-menu" type="button" aria-label="All runs" aria-haspopup="dialog" onClick={onRuns}>
              <svg viewBox="0 0 20 20" aria-hidden="true">
                <path d="M3 5h14M3 10h14M3 15h14" />
              </svg>
            </button>
          )}
          <span class="eyebrow">Soundcheck · local regression</span>
        </div>
        <h1>{view.label}</h1>
        {view.trial === undefined ? null : (
          <p class="trial" role="note">
            {view.trial}
          </p>
        )}
        <div class="facts">
          {view.facts.map((f, i) => (
            <span key={i} title={f.title}>
              {f.text}
              {f.at === undefined ? null : " "}
              {f.at === undefined ? null : <Time at={f.at} />}
            </span>
          ))}
        </div>
        {/* Relative, so it works on the live page and on a past run's. */}
        {view.export ? null : (
          <div class="head-actions">
            <a class="download" href="download" download="">
              Download report
            </a>
            {canDelete ? (
              <button class="delete-run" type="button" onClick={() => void deleteRun()}>
                Delete run…
              </button>
            ) : null}
          </div>
        )}
      </div>
    </header>
  );
}

function Ring({ segments }: { segments: V.Segment[] }) {
  return (
    <svg class="ring" viewBox="0 0 120 120" aria-hidden="true">
      <circle cx="60" cy="60" r="44" class="ring-bg" />
      {segments.map((s) => (
        <circle
          key={s.class}
          cx="60"
          cy="60"
          r="44"
          class={`ring-seg ${s.class}`}
          stroke-dasharray={s.dash}
          stroke-dashoffset={s.offset}
        />
      ))}
    </svg>
  );
}

function GridCell({ cell }: { cell: V.Cell }) {
  if (cell.bars === undefined) {
    return (
      <span class="cell">
        <span class="cell-text">{cell.text}</span>
      </span>
    );
  }

  const [pass, fail, blocked, review, pending] = cell.bars;
  return (
    <span class={cell.class ? `cell ${cell.class}` : "cell"}>
      <span class="cell-text">
        <i></i>
        {cell.text}
      </span>
      <span class="mini">
        <i class="pass" style={`width:${pass}%`}></i>
        <i class="fail" style={`width:${fail}%`}></i>
        <i class="blocked" style={`width:${blocked}%`}></i>
        <i class="review" style={`width:${review}%`}></i>
        <i class="pending" style={`width:${pending}%`}></i>
      </span>
    </span>
  );
}

const Summary = memo(function Summary({ summary, platforms }: { summary: V.Summary; platforms: string[] }) {
  return (
    <section class="summary card" aria-label="Summary">
      <div class="summary-head">
        <div class="ringwrap">
          <Ring segments={summary.ring} />
          <div class="ringlabel">
            {/* Three digits don't fit the ring's hole at the two-digit size. */}
            <b class={summary.pct >= 100 ? "full" : undefined}>{summary.pct}%</b>
          </div>
        </div>
        <div class="summary-verdict">
          <strong class={summary.class || undefined}>{summary.word}</strong>
          {summary.reasons.length === 0 ? null : (
            <ul class="reasons" aria-label="What's in the way">
              {summary.reasons.map((r, i) => (
                <li key={i} class={r.bad ? "chip reason bad" : "chip reason"}>
                  {r.text}
                </li>
              ))}
            </ul>
          )}
        </div>
      </div>
      <div class="grid">
        <div class="grid-row grid-head">
          <span>Section</span>
          {platforms.map((p) => (
            <span key={p}>{p}</span>
          ))}
        </div>
        {summary.grid.map((row) => (
          <a key={row.feature} class="grid-row" href={`#section-${row.feature}`}>
            <span class="grid-name">{row.name}</span>
            {row.cells.map((c, i) => (
              <GridCell key={i} cell={c} />
            ))}
          </a>
        ))}
      </div>
    </section>
  );
});

function Thumbs({ shots }: { shots: V.Shot[] }) {
  const show = useContext(ShowShot);

  if (shots.length === 0) {
    return null;
  }

  const found = shots.filter((s) => s.src !== undefined);
  return (
    <div class="thumbs" aria-label="Screenshots" style="margin-top:12px">
      {shots.map((s, i) =>
        s.src === undefined ? (
          <div key={i} class="thumb">
            {s.name}
          </div>
        ) : (
          <a
            key={i}
            class="thumb"
            href={s.src}
            target="_blank"
            rel="noopener"
            onClick={(e) => {
              if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) {
                return;
              }

              e.preventDefault();
              show(found, found.indexOf(s));
            }}
          >
            <img src={s.src} alt={s.name} />
          </a>
        ),
      )}
    </div>
  );
}

function FailureBlock({ failure }: { failure: V.Failure }) {
  return (
    <>
      {failure.call === undefined ? null : <span class="callline">{failure.call}</span>}
      <Thumbs shots={failure.shots} />
    </>
  );
}

function Outcomes({ outcomes, failure }: { outcomes: V.Outcome[]; failure?: V.Failure }) {
  return (
    <ul class="outcomes">
      {outcomes.map((o, i) => (
        <li key={i} class={o.boxed ? "failed-outcome" : undefined}>
          <span class={`mark ${o.class}`} aria-label={o.label}>
            {o.mark}
          </span>
          <span>
            <span class="o">{o.text}</span>
            <span class="obs">{o.observed}</span>
            {o.settled === undefined ? null : <span class="settled">{o.settled}</span>}
            {o.failure && failure !== undefined ? <FailureBlock failure={failure} /> : null}
          </span>
        </li>
      ))}
    </ul>
  );
}

function EvidenceFold({ evidence }: { evidence: V.Evidence }) {
  return (
    <details class="fold">
      <summary>Evidence and tester's notes</summary>
      <Thumbs shots={evidence.shots} />
      {evidence.texts.map((t) =>
        t.text === undefined ? (
          <p key={t.name}>
            <a href={t.href} target="_blank" rel="noopener">
              {t.label}
            </a>
          </p>
        ) : (
          <details key={t.name} class="fold">
            <summary>{t.label}</summary>
            <pre class="evidence-text">{t.text}</pre>
          </details>
        ),
      )}
      {evidence.notes === undefined ? null : <p>{evidence.notes}</p>}
    </details>
  );
}

function BodyView({ body }: { body: V.Body }) {
  const label = body.label ? <span class="platform-label">{body.platform}</span> : null;

  if (body.kind === "pending") {
    return (
      <>
        {label}
        <p class="pending-state">{body.state}</p>
        <ul class="outcomes">
          {body.outcomes.map((o, i) =>
            o.judged === undefined ? (
              <li key={i}>
                <span class={body.running ? "mark pending running" : "mark pending"} aria-label={body.state}></span>
                <span>
                  <span class="o pending">{o.text}</span>
                </span>
              </li>
            ) : (
              <li key={i}>
                <span class={`mark ${o.judged.class}`} aria-label="judged so far">
                  {o.judged.mark}
                </span>
                <span>
                  <span class="o">{o.text}</span>
                  <span class="obs">{o.judged.observed}</span>
                </span>
              </li>
            ),
          )}
        </ul>
      </>
    );
  }

  const outcomes =
    body.outcomes.length === 0 ? null : <Outcomes outcomes={body.outcomes} failure={body.failure} />;
  let failed = null;

  if (body.failure !== undefined) {
    failed = (
      <>
        {body.failure.alone ? (
          <div class="failed-outcome">
            <FailureBlock failure={body.failure} />
          </div>
        ) : null}
        {outcomes}
      </>
    );
  }

  return (
    <>
      {label}
      {failed}
      {body.blocked === undefined ? null : (
        <div class="blocked-block">
          <span class="expect">{body.blocked.expect}</span>
          <span class="saw">{body.blocked.saw}</span>
        </div>
      )}
      {body.findings.map((f) => (
        <div key={f.id} class="finding-block">
          <span class="expect">{f.title}</span>
          <span class="saw">
            {f.detail} <a href={`#problem-${f.id}`}>In Problems</a>
          </span>
          <Thumbs shots={f.shots} />
        </div>
      ))}
      {body.failure === undefined ? outcomes : null}
      {body.evidence === undefined ? null : <EvidenceFold evidence={body.evidence} />}
    </>
  );
}

const CheckRow = memo(function CheckRow({ id, check }: { id: string; check: V.Check }) {
  const [open, onToggle] = useDetails(check.anchor, check.open);
  return (
    <details class="check" id={check.anchor} data-attn={check.attn ? "" : undefined} open={open} onToggle={onToggle}>
      <summary>
        <span class="cid" title={id}>
          {check.number}
        </span>
        <span class="ctitle">
          {check.title}
          {check.pill === undefined ? null : <span class={`chip small ${check.pill.class}`}>{check.pill.text}</span>}
        </span>
        {check.cells.map((c) => (
          // Named, for a narrow screen where the platform columns' headings are hidden.
          <span key={c.platform} data-p={c.platform} class={`res ${c.class}`} title={c.title}>
            {c.text}
          </span>
        ))}
      </summary>
      {check.bodies.length === 0 ? null : (
        <div class="check-body">
          {check.bodies.map((b) => (
            <BodyView key={b.platform} body={b} />
          ))}
        </div>
      )}
    </details>
  );
});

function Sections({ view, attnOnly, onFilter }: { view: V.View; attnOnly: boolean; onFilter: (attn: boolean) => void }) {
  return (
    <>
      <div class="toolbar">
        <h2>Checks by section</h2>
        <div class="segmented" role="group" aria-label="Show">
          <button class="filter" type="button" aria-pressed={!attnOnly} onClick={() => onFilter(false)}>
            All
          </button>
          <button class="filter" type="button" aria-pressed={attnOnly} onClick={() => onFilter(true)}>
            Needs attention
          </button>
        </div>
      </div>
      <div class={attnOnly ? "sections attn-only" : "sections"}>
        {view.sections.map((s) => (
          <section key={s.id} class="section card" id={`section-${s.id}`}>
            <header>
              <h3>{s.name}</h3>
            </header>
            <div class="checks-head">
              <span>ID</span>
              <span>Check</span>
              {view.platforms.map((p) => (
                <span key={p}>{p}</span>
              ))}
            </div>
            {s.specs.map((id) => (view.checks[id] === undefined ? null : <CheckRow key={id} id={id} check={view.checks[id]} />))}
          </section>
        ))}
      </div>
    </>
  );
}

const ProblemRow = memo(function ProblemRow({ id, problem }: { id: string; problem: V.Problem }) {
  const [open, onToggle] = useDetails(`problem-${id}`, false);
  return (
    <details class="problem" id={`problem-${id}`} open={open} onToggle={onToggle}>
      <summary class="p2">
        <span class="pticket">
          {problem.badge === undefined ? null : <span class={`chip ${problem.badge.class}`}>{problem.badge.text}</span>}
        </span>
        <h3>
          <span class="pid">{problem.number}</span>
          {problem.title}
        </h3>
        <span class="pwhere">{problem.place}</span>
      </summary>
      <div class="problem-body">
        <p>{problem.detail}</p>
        <Thumbs shots={problem.shots} />
        {problem.paras.map((p, i) => (
          <p key={i}>{p}</p>
        ))}
        <p>
          <a href={`#${problem.row}`}>Open its row</a>
        </p>
      </div>
    </details>
  );
});

function Problems({ view }: { view: V.View }) {
  if (view.groups.length === 0) {
    return null;
  }

  return (
    <section class="block" id="problems">
      <h2>Problems</h2>
      {view.groups.map((g) => [
        <h4 key={`${g.title} title`} class="pgroup">
          {g.title} · {g.ids.length}
        </h4>,
        <div key={g.title} class="problems">
          {g.ids.map((id) =>
            view.problems[id] === undefined ? null : <ProblemRow key={id} id={id} problem={view.problems[id]} />,
          )}
        </div>,
      ])}
    </section>
  );
}

const NotesBlock = memo(function NotesBlock({ notes }: { notes: V.Notes }) {
  const [open, onToggle] = useDetails("notes", false);
  return (
    <section class="block">
      <h2>Notes</h2>
      <div class="card notes">
        <details class="fold" id="notes" open={open} onToggle={onToggle}>
          <summary>Show all {notes.count} notes</summary>
          <dl>
            {notes.rows.map((r) => [
              <dt key={`${r.title} title`}>{r.title}</dt>,
              <dd key={r.title}>{r.lines.map((line, i) => (i === 0 ? line : [<br key={i} />, line]))}</dd>,
            ])}
          </dl>
        </details>
      </div>
    </section>
  );
});

const Timeline = memo(function Timeline({ timeline }: { timeline: Record<string, V.Line> }) {
  const [open, onToggle] = useDetails("timeline", false);
  const seqs = Object.keys(timeline).sort((a, b) => Number(b) - Number(a));

  if (seqs.length === 0) {
    return null;
  }

  return (
    <section class="block">
      <h2>Timeline</h2>
      <div class="card timeline">
        <details class="fold" id="timeline" open={open} onToggle={onToggle}>
          <summary>Show all {seqs.length} events</summary>
          <ol>
            {seqs.map((seq) => (
              <li key={seq}>
                <span>
                  <Time at={timeline[seq].at} />
                </span>
                {timeline[seq].text}
              </li>
            ))}
          </ol>
        </details>
      </div>
    </section>
  );
});

const THEMES: [Theme, string][] = [
  ["auto", "Auto"],
  ["light", "Light"],
  ["dark", "Dark"],
];

function Footer({ version, theme, onTheme }: { version: string; theme: Theme; onTheme: (theme: Theme) => void }) {
  return (
    <footer>
      <div class="segmented" role="group" aria-label="Appearance">
        {THEMES.map(([choice, word]) => (
          <button
            key={choice}
            class="theme-choice"
            type="button"
            aria-pressed={theme === choice}
            onClick={() => onTheme(choice)}
          >
            {word}
          </button>
        ))}
      </div>
      <p>
        Generated by soundcheck {version} from the run's journal. Local stack only; nothing here touched production.
      </p>
    </footer>
  );
}

function Lightbox({ shots, at: first, onClose }: { shots: V.Shot[]; at: number; onClose: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [at, setAt] = useState(first);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);

  function step(by: number): void {
    setAt((i) => (i + by + shots.length) % shots.length);
  }

  const shot = shots[at];
  return (
    <dialog
      ref={dialog}
      class="lightbox"
      aria-label="Screenshot"
      onClose={onClose}
      onClick={(e) => {
        if (e.target === e.currentTarget) {
          dialog.current?.close();
        }
      }}
      onKeyDown={(e) => {
        if (e.key === "ArrowLeft") {
          step(-1);
        } else if (e.key === "ArrowRight") {
          step(1);
        }
      }}
    >
      <img src={shot.src} alt={shot.name} />
      <p class="lightbox-count">{shots.length > 1 ? `${at + 1} of ${shots.length} · ${shot.name}` : shot.name}</p>
      <button class="lightbox-nav prev" aria-label="Previous screenshot" hidden={shots.length < 2} onClick={() => step(-1)}>
        ‹
      </button>
      <button class="lightbox-nav next" aria-label="Next screenshot" hidden={shots.length < 2} onClick={() => step(1)}>
        ›
      </button>
      <button class="lightbox-close" aria-label="Close" onClick={() => dialog.current?.close()}>
        ✕
      </button>
    </dialog>
  );
}

/** The runs drawer: an installed app has no address bar, so this is the way to the other runs. */
function RunsDrawer({ onClose }: { onClose: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [list, setList] = useState<string | undefined>(undefined);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    dialog.current?.showModal();
    async function load(): Promise<void> {
      try {
        const response = await fetch("/");
        const runs = new DOMParser().parseFromString(await response.text(), "text/html").querySelector(".runs");

        if (!response.ok || runs === null) {
          throw new Error(`the runs list answered ${response.status}`);
        }

        // The list's links are relative to the list's own page.
        for (const a of runs.querySelectorAll<HTMLAnchorElement>("a[href]")) {
          a.href = new URL(a.getAttribute("href") ?? "", response.url).href;

          if (a.pathname === location.pathname) {
            a.setAttribute("aria-current", "page");
          }
        }

        formatTimes(runs);
        setList(runs.innerHTML);
      } catch {
        setFailed(true);
      }
    }

    void load();
  }, []);

  return (
    <dialog
      ref={dialog}
      class="drawer"
      aria-label="Runs"
      onClose={onClose}
      onClick={(e) => {
        if (e.target === e.currentTarget) {
          dialog.current?.close();
        }
      }}
    >
      <div class="drawer-head">
        <a href="/">Runs</a>
        <button class="drawer-close" type="button" aria-label="Close" onClick={() => dialog.current?.close()}>
          ✕
        </button>
      </div>
      <div class="drawer-body">
        {list === undefined ? (
          <section class="card runs">
            <p class="empty">{failed ? "Couldn’t load the runs." : "Loading…"}</p>
          </section>
        ) : (
          <section class="card runs" dangerouslySetInnerHTML={{ __html: list }} />
        )}
      </div>
    </dialog>
  );
}

function applyTheme(theme: Theme): void {
  if (theme === "light" || theme === "dark") {
    document.documentElement.dataset.theme = theme;
  } else {
    delete document.documentElement.dataset.theme;
  }
}

/** The run's page. `live` follows the server's updates; without it (a past run, an export) the view stays as given. */
export function App({ initial, live }: { initial: V.View; live: boolean }) {
  const [view, setView] = useState(initial);
  const [attnOnly, setAttnOnly] = useState(() => stored("soundcheck-attn") === "1");
  const [theme, setTheme] = useState<Theme>(() => {
    const saved = stored("soundcheck-theme");
    return saved === "light" || saved === "dark" ? saved : "auto";
  });

  const [lightbox, setLightbox] = useState<LightboxShots | undefined>(undefined);
  const [runs, setRuns] = useState(false);
  const [connected, setConnected] = useState(true);

  useEffect(() => {
    if (!live) {
      return undefined;
    }

    return follow("updates", {
      onView: (next) => {
        setView(next);
      },
      onPatch: (patch) => {
        setView((before) => applyPatch(before, patch) as V.View);
      },
      onConnected: setConnected,
    });
  }, [live]);

  useEffect(() => {
    document.title = `${view.label} · soundcheck`;
  }, [view.label]);

  function filter(attn: boolean): void {
    setAttnOnly(attn);
    store("soundcheck-attn", attn ? "1" : "0");
  }

  function chooseTheme(choice: Theme): void {
    setTheme(choice);
    store("soundcheck-theme", choice);
    applyTheme(choice);
  }

  function showShot(shots: V.Shot[], at: number): void {
    setLightbox({ shots, at, opened: Date.now() });
  }

  return (
    <ShowShot.Provider value={showShot}>
      {view.bar === undefined ? null : <NowBar bar={view.bar} connected={connected} />}
      <main style={`--platforms:${Math.max(1, view.platforms.length)}`}>
        <Header view={view} onRuns={() => setRuns(true)} canDelete={!live && !view.export && onThisMachine()} />
        <Summary summary={view.summary} platforms={view.platforms} />
        <Sections view={view} attnOnly={attnOnly} onFilter={filter} />
        <Problems view={view} />
        {view.notes === undefined ? null : <NotesBlock notes={view.notes} />}
        <Timeline timeline={view.timeline} />
        <Footer version={view.version} theme={theme} onTheme={chooseTheme} />
      </main>
      {lightbox === undefined ? null : (
        <Lightbox key={lightbox.opened} shots={lightbox.shots} at={lightbox.at} onClose={() => setLightbox(undefined)} />
      )}
      {runs ? <RunsDrawer onClose={() => setRuns(false)} /> : null}
    </ShowShot.Provider>
  );
}
