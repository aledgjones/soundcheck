#!/usr/bin/env node
// Switches a project's soundcheck plugin between the release its .claude/settings.json pins and this checkout.
// Dev mode lives only in the project's .claude/settings.local.json. Start a new Claude Code session after switching.
// Usage, from the project: node /path/to/soundcheck/scripts/dev-plugin.mjs on|off|status
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const checkout = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const name = JSON.parse(
  readFileSync(join(checkout, ".claude-plugin", "marketplace.json"), "utf8"),
).name;
const project = process.cwd();
const projectPath = join(project, ".claude", "settings.json");
const localPath = join(project, ".claude", "settings.local.json");

function claude(...args) {
  execFileSync("claude", ["plugin", ...args], {
    cwd: project,
    stdio: "inherit",
  });
}

function readJson(path) {
  return existsSync(path) ? JSON.parse(readFileSync(path, "utf8")) : {};
}

function writeLocal(settings) {
  writeFileSync(localPath, `${JSON.stringify(settings, null, 2)}\n`);
}

function on() {
  claude("marketplace", "add", checkout, "--scope", "local");
  const local = readJson(localPath);
  local.env = { ...local.env, SOUNDCHECK_DEV: "1" };
  writeLocal(local);
  console.log(
    `\nDev mode on: the plugin reads ${checkout}/plugin and runs the soundcheck on PATH.`,
  );
  console.log(
    `Install your build first: cargo install --path ${checkout}/crates/server`,
  );
}

function off() {
  const committed = existsSync(projectPath)
    ? readFileSync(projectPath, "utf8")
    : null;
  const source =
    committed && JSON.parse(committed).extraKnownMarketplaces?.[name]?.source;
  claude("marketplace", "remove", name, "--scope", "local");

  if (source?.source === "github") {
    // Removing the local source doesn't fall back to the project's, so the pinned release is added again.
    claude(
      "marketplace",
      "add",
      source.ref ? `${source.repo}@${source.ref}` : source.repo,
      "--scope",
      "project",
    );
    // The CLI rewrites the committed file in its own key order; the content is unchanged.
    writeFileSync(projectPath, committed);
  } else {
    console.log(
      `\n.claude/settings.json pins no GitHub source for ${name}: add the marketplace you want again.`,
    );
  }

  const local = readJson(localPath);

  if (local.env) {
    delete local.env.SOUNDCHECK_DEV;

    if (Object.keys(local.env).length === 0) {
      delete local.env;
    }
  }

  writeLocal(local);
  console.log(`\nDev mode off: the plugin runs the release the project pins.`);
}

const command = process.argv[2];

if (command === "on") {
  on();
} else if (command === "off") {
  off();
} else if (command === "status") {
  claude("marketplace", "list");
} else {
  console.error(
    "usage: node scripts/dev-plugin.mjs on|off|status (run from the project)",
  );
  process.exit(1);
}
