// Drives follow() with a stand-in EventSource and mocked timers, so reconnects and the quiet timeout are checked
// without a server.
import assert from "node:assert/strict";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import * as esbuild from "esbuild";

const built = await esbuild.build({
  entryPoints: [fileURLToPath(new URL("../src/follow.ts", import.meta.url))],
  bundle: true,
  format: "esm",
  platform: "node",
  write: false,
});

const { follow, QUIET_MS } = await import(
  `data:text/javascript;base64,${Buffer.from(built.outputFiles[0].text).toString("base64")}`
);

class FakeSource {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSED = 2;
  static made = [];

  constructor(url) {
    this.url = url;
    this.readyState = FakeSource.CONNECTING;
    this.listeners = {};
    FakeSource.made.push(this);
  }

  addEventListener(name, listener) {
    this.listeners[name] = listener;
  }

  close() {
    this.readyState = FakeSource.CLOSED;
  }

  open() {
    this.readyState = FakeSource.OPEN;
    this.onopen();
  }

  send(name, data) {
    this.listeners[name]({ data: JSON.stringify(data) });
  }

  fail(closed) {
    this.readyState = closed ? FakeSource.CLOSED : FakeSource.CONNECTING;
    this.onerror();
  }
}

function setUp(t) {
  FakeSource.made = [];
  globalThis.EventSource = FakeSource;
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const got = { views: [], patches: [], connected: [] };
  const stop = follow("updates", {
    onView: (view) => {
      got.views.push(view);
    },
    onPatch: (patch) => {
      got.patches.push(patch);
    },
    onConnected: (connected) => {
      got.connected.push(connected);
    },
  });
  return { got, stop };
}

test("a view then patches arrive, and the badge follows open and error", (t) => {
  const { got, stop } = setUp(t);
  const source = FakeSource.made[0];
  assert.equal(source.url, "updates");
  source.open();
  source.send("view", { label: "a" });
  source.send("patch", { label: "b" });
  source.fail(false);
  assert.deepEqual(got.views, [{ label: "a" }]);
  assert.deepEqual(got.patches, [{ label: "b" }]);
  assert.deepEqual(got.connected, [true, true, false]);
  // The browser reconnects by itself while CONNECTING, so no new source is made.
  t.mock.timers.tick(20_000);
  assert.equal(FakeSource.made.length, 1);
  stop();
});

test("a closed source is replaced after a wait that doubles to 30 s", (t) => {
  const { stop } = setUp(t);
  const waits = [];
  for (let i = 0; i < 7; i += 1) {
    FakeSource.made.at(-1).fail(true);
    const before = FakeSource.made.length;
    let waited = 0;
    while (FakeSource.made.length === before) {
      t.mock.timers.tick(500);
      waited += 500;
    }
    waits.push(waited);
  }
  assert.deepEqual(waits, [1000, 2000, 4000, 8000, 16000, 30000, 30000]);
  FakeSource.made.at(-1).open();
  FakeSource.made.at(-1).fail(true);
  t.mock.timers.tick(1000);
  assert.equal(FakeSource.made.at(-2).readyState, FakeSource.CLOSED);
  assert.equal(FakeSource.made.at(-1).readyState, FakeSource.CONNECTING);
  stop();
});

test("silence past the heartbeat shows not connected and opens a fresh source", (t) => {
  const { got, stop } = setUp(t);
  const first = FakeSource.made[0];
  first.open();
  first.send("view", { label: "a" });
  t.mock.timers.tick(QUIET_MS - 1000);
  first.send("ping", {});
  t.mock.timers.tick(QUIET_MS - 1000);
  assert.equal(FakeSource.made.length, 1);
  t.mock.timers.tick(1000);
  assert.deepEqual(got.connected, [true, true, false]);
  assert.equal(first.readyState, FakeSource.CLOSED);
  assert.equal(FakeSource.made.length, 2);
  stop();
  assert.equal(FakeSource.made[1].readyState, FakeSource.CLOSED);

});
