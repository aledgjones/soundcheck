// Renders the page to a string from a view the server made (sample-view.json, from crates/server/tests/report.rs
// with SOUNDCHECK_REPORT_SAMPLE set), so the components are checked without a browser.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import * as esbuild from "esbuild";

const built = await esbuild.build({
  entryPoints: [fileURLToPath(new URL("entry.tsx", import.meta.url))],
  bundle: true,
  format: "esm",
  platform: "node",
  jsx: "automatic",
  jsxImportSource: "preact",
  write: false,
});

const { renderView, applyPatch } = await import(
  `data:text/javascript;base64,${Buffer.from(built.outputFiles[0].text).toString("base64")}`
);
const sample = JSON.parse(readFileSync(new URL("sample-view.json", import.meta.url), "utf8"));

test("the page shows the verdict, the problems and each row's state", () => {
  const html = renderView(sample);
  for (const want of [
    "<h1>v2.16.2 · candidate</h1>",
    '<strong class="failing">Failing</strong>',
    '<li class="chip reason bad">1 failure</li>',
    "1 being checked",
    '<span class="chip blocks">Failure</span>',
    "Undo leaves the stroke on screen when zoomed in</h3>",
    '<span class="chip small finding">1 finding</span>',
    "The stroke stayed after Undo at 2.5× zoom.",
    '<span class="callline">The screenshot shows the stroke after Undo.</span>',
    'class="res review"',
    "checking <b>annotations/001-draw</b>",
    'class="nowbar"',
    '<a class="download" href="download"',
    "No screenshot of it; the detail above is all there is.",
  ]) {
    assert.ok(html.includes(want), `missing ${want}`);
  }

  assert.match(html, /<details class="check" id="spec-annotations-001-draw" data-attn(="")? open/);
});

test("an export has no live bar, download or runs menu", () => {
  const html = renderView({ ...sample, bar: undefined, export: true });
  assert.ok(!html.includes('class="nowbar"'));
  assert.ok(!html.includes('class="download"'));
  assert.ok(!html.includes('class="runs-menu"'));
});

test("a patch replaces what changed and leaves the rest as it was", () => {
  const patched = applyPatch(sample, {
    checks: { "annotations/002-back": { title: "Renamed" } },
    bar: null,
  });

  assert.equal(patched.checks["annotations/002-back"].title, "Renamed");
  assert.equal(patched.checks["annotations/001-draw"], sample.checks["annotations/001-draw"]);
  assert.equal(patched.summary, sample.summary);
  assert.ok(!("bar" in patched));
  assert.ok(renderView(patched).includes("Renamed"));
});
