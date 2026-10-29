// Keeps the page current: times in the viewer's zone, ages that tick, the attention filter, and (on the live page)
// a fresh render after every journal event, keeping whatever rows the viewer opened or closed.
(() => {
  const root = document.getElementById("live");
  const pad = (n) => String(n).padStart(2, "0");

  function tick() {
    for (const t of document.querySelectorAll("time[data-at]")) {
      const d = new Date(Number(t.dataset.at));
      const hm = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
      t.textContent =
        t.dataset.format === "date"
          ? `${d.toLocaleDateString(undefined, { day: "numeric", month: "short" })}, ${hm}`
          : hm;
    }
    const now = Date.now();
    const span = (ms) => {
      const min = Math.floor(ms / 60000);
      if (min < 1) {
        return `${Math.max(0, Math.floor(ms / 1000))} s`;
      }
      return min < 60 ? `${min} min` : `${Math.floor(min / 60)} h ${min % 60} min`;
    };
    for (const e of document.querySelectorAll(".elapsed[data-active]")) {
      // Counting on from the last entry, unless the run has gone quiet for longer than counts as working.
      const quiet = e.dataset.since ? now - Number(e.dataset.since) : 0;
      const live = quiet <= Number(e.dataset.cap || 0) ? Math.max(0, quiet) : 0;
      const s = Math.floor((Number(e.dataset.active) + live) / 1000);
      const h = Math.floor(s / 3600);
      const ms = `${pad(Math.floor(s / 60) % 60)}:${pad(s % 60)}`;
      e.textContent = h > 0 ? `${h}:${ms}` : ms;
    }
    for (const a of document.querySelectorAll(".ago[data-since]")) {
      const ms = now - Number(a.dataset.since);
      a.hidden = ms < Number(a.dataset.warn || 0);
      a.textContent = a.hidden ? "" : a.dataset.label.replace("{t}", span(ms));
    }
  }

  let attnOnly = false;
  try {
    attnOnly = localStorage.getItem("soundcheck-attn") === "1";
  } catch {}
  function applyFilter() {
    const sections = document.querySelector(".sections");
    if (sections) sections.classList.toggle("attn-only", attnOnly);
    for (const b of document.querySelectorAll(".filter")) {
      b.setAttribute("aria-pressed", String((b.dataset.filter === "attn") === attnOnly));
    }
  }
  document.addEventListener("click", (e) => {
    const b = e.target.closest(".filter");
    if (!b) return;
    attnOnly = b.dataset.filter === "attn";
    try {
      localStorage.setItem("soundcheck-attn", attnOnly ? "1" : "0");
    } catch {}
    applyFilter();
  });

  // Appearance: Auto follows the system; Light or Dark is remembered in this browser.
  let theme = "auto";
  try {
    theme = localStorage.getItem("soundcheck-theme") || "auto";
  } catch {}
  function applyTheme() {
    if (theme === "light" || theme === "dark") {
      document.documentElement.dataset.theme = theme;
    } else {
      delete document.documentElement.dataset.theme;
    }
    for (const b of document.querySelectorAll(".theme-choice")) {
      b.setAttribute("aria-pressed", String(b.dataset.themeChoice === theme));
    }
  }
  document.addEventListener("click", (e) => {
    const b = e.target.closest(".theme-choice");
    if (!b) return;
    theme = b.dataset.themeChoice;
    try {
      localStorage.setItem("soundcheck-theme", theme);
    } catch {}
    applyTheme();
  });

  // Screenshots open in a lightbox; the arrows step through the row's shots and Esc closes it.
  let box = null;
  let shots = [];
  let at = 0;
  function show(i) {
    at = (i + shots.length) % shots.length;
    const img = box.querySelector("img");
    img.src = shots[at].href;
    img.alt = shots[at].querySelector("img").alt;
    box.querySelector(".lightbox-count").textContent =
      shots.length > 1 ? `${at + 1} of ${shots.length} · ${img.alt}` : img.alt;
    for (const b of box.querySelectorAll(".lightbox-nav")) b.hidden = shots.length < 2;
  }
  function lightbox() {
    if (box) return box;
    box = document.createElement("dialog");
    box.className = "lightbox";
    box.setAttribute("aria-label", "Screenshot");
    box.innerHTML =
      '<img alt=""><p class="lightbox-count"></p>' +
      '<button class="lightbox-nav prev" aria-label="Previous screenshot">‹</button>' +
      '<button class="lightbox-nav next" aria-label="Next screenshot">›</button>' +
      '<button class="lightbox-close" aria-label="Close">✕</button>';
    box.addEventListener("click", (e) => {
      if (e.target.closest(".prev")) show(at - 1);
      else if (e.target.closest(".next")) show(at + 1);
      else if (e.target.closest(".lightbox-close") || e.target === box) box.close();
    });
    box.addEventListener("keydown", (e) => {
      if (e.key === "ArrowLeft") show(at - 1);
      else if (e.key === "ArrowRight") show(at + 1);
    });
    document.body.append(box);
    return box;
  }
  document.addEventListener("click", (e) => {
    const a = e.target.closest("a.thumb");
    if (!a || e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
    e.preventDefault();
    shots = [...a.closest(".thumbs").querySelectorAll("a.thumb")];
    lightbox().showModal();
    show(shots.indexOf(a));
  });

  // The runs drawer: an installed app has no address bar, so this is the way to the other runs.
  let drawer = null;
  async function openRuns() {
    if (!drawer) {
      drawer = document.createElement("dialog");
      drawer.className = "drawer";
      drawer.setAttribute("aria-label", "Runs");
      drawer.innerHTML =
        '<div class="drawer-head"><a href="/">Runs</a>' +
        '<button class="drawer-close" type="button" aria-label="Close">✕</button></div>' +
        '<section class="card runs"><p class="empty">Loading…</p></section>';
      drawer.addEventListener("click", (e) => {
        if (e.target.closest(".drawer-close") || e.target === drawer) drawer.close();
      });
      document.body.append(drawer);
    }
    drawer.showModal();
    try {
      const response = await fetch("/");
      const list = new DOMParser()
        .parseFromString(await response.text(), "text/html")
        .querySelector(".runs");
      if (!response.ok || !list) throw new Error();
      // The list's links are relative to the list's own page.
      for (const a of list.querySelectorAll("a[href]")) {
        a.href = new URL(a.getAttribute("href"), response.url).href;
        if (a.pathname === location.pathname) a.setAttribute("aria-current", "page");
      }
      drawer.querySelector(".runs").replaceWith(document.adoptNode(list));
      tick();
    } catch {
      drawer.querySelector(".runs").innerHTML = '<p class="empty">Couldn’t load the runs.</p>';
    }
  }
  document.addEventListener("click", (e) => {
    if (e.target.closest(".runs-menu")) openRuns();
  });

  tick();
  applyFilter();
  applyTheme();
  setInterval(tick, 1000);
  if (!root || root.dataset.live !== "true") return;

  // Only rows the viewer opened or closed keep that state; the rest follow the page (a new failure opens itself).
  const chosen = new Map();
  let restoring = false;
  root.addEventListener(
    "toggle",
    (e) => {
      if (!restoring && e.target.id) chosen.set(e.target.id, e.target.open);
    },
    true,
  );

  let refreshing = false;
  let again = false;
  async function refresh() {
    if (refreshing) {
      again = true;
      return;
    }
    refreshing = true;
    try {
      const response = await fetch("fragment");
      if (response.ok) {
        restoring = true;
        root.innerHTML = await response.text();
        for (const [id, open] of chosen) {
          const d = document.getElementById(id);
          if (d) d.open = open;
        }
        // toggle events fire after this task; let them pass before listening again.
        setTimeout(() => (restoring = false), 0);
        tick();
        applyFilter();
        applyTheme();
      }
    } catch {}
    refreshing = false;
    if (again) {
      again = false;
      refresh();
    }
  }

  let after = Number(root.dataset.seq || 0);
  async function follow() {
    try {
      const response = await fetch(`events?after=${after}`);
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffered = "";
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buffered += decoder.decode(value, { stream: true });
        let end;
        let changed = false;
        while ((end = buffered.indexOf("\n")) >= 0) {
          const line = buffered.slice(0, end);
          buffered = buffered.slice(end + 1);
          try {
            after = JSON.parse(line).seq;
            changed = true;
          } catch {}
        }
        if (changed) refresh();
      }
    } catch {}
    // The server stopped or restarted: try again shortly, replaying from the last event seen.
    setTimeout(follow, 3000);
  }
  follow();
})();
