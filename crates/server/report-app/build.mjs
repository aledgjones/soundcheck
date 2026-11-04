// Bundles the page into ../src/report/app.js, which soundcheck includes. `--check` fails if that file isn't what a
// fresh build makes, so the checked-in bundle can't drift from its source.
import * as esbuild from "esbuild";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));
const out = fileURLToPath(new URL("../src/report/app.js", import.meta.url));
const check = process.argv.includes("--check");

const result = await esbuild.build({
  absWorkingDir: here,
  entryPoints: ["src/main.tsx"],
  bundle: true,
  minify: true,
  format: "iife",
  target: "es2022",
  jsx: "automatic",
  jsxImportSource: "preact",
  legalComments: "none",
  write: false,
});

const code = result.outputFiles[0].text;

// The bundle is inlined in a <script> element: these would end it early or switch the parser into an escaped state.
for (const unsafe of ["</script", "<script", "<!--"]) {
  if (code.toLowerCase().includes(unsafe)) {
    console.error(`the bundle contains ${unsafe}`);
    process.exit(1);
  }
}

if (check) {
  let current = "";
  try {
    current = readFileSync(out, "utf8");
  } catch {}

  if (current !== code) {
    console.error("src/report/app.js is out of date: run `npm run build` and commit it");
    process.exit(1);
  }

  console.log("src/report/app.js matches a fresh build");
} else {
  writeFileSync(out, code);
  console.log(`wrote src/report/app.js (${code.length} bytes)`);
}
