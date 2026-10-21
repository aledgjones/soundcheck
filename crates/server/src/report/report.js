// Keeps the page current: times in the viewer's zone, ages that tick, the attention filter, and (on the live page)
// a fresh render after every journal event, keeping whatever rows the viewer opened or closed.
(() => {
  const root = document.getElementById("live");
  const pad = (n) => String(n).padStart(2, "0");

  function tick() {
    for (const t of document.querySelectorAll("time[data-at]")) {
      const d = new Date(Number(t.dataset.at));
      t.textContent = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
    }
    const now = Date.now();
    for (const a of document.querySelectorAll(".ago[data-since]")) {
      const ms = now - Number(a.dataset.since);
      const min = Math.floor(ms / 60000);
      a.textContent = min < 1 ? `${Math.max(0, Math.floor(ms / 1000))} s` : `${min} min`;
      const warn = Number(a.dataset.warn || 0);
      a.classList.toggle("warn", warn > 0 && ms >= warn);
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

  tick();
  applyFilter();
  setInterval(tick, 10000);
  if (!root || root.dataset.live !== "true") return;

  let refreshing = false;
  let again = false;
  async function refresh() {
    if (refreshing) {
      again = true;
      return;
    }
    refreshing = true;
    const state = new Map([...document.querySelectorAll("details[id]")].map((d) => [d.id, d.open]));
    try {
      const response = await fetch("fragment");
      if (response.ok) {
        root.innerHTML = await response.text();
        for (const [id, open] of state) {
          const d = document.getElementById(id);
          if (d) d.open = open;
        }
        tick();
        applyFilter();
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
