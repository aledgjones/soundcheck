import { render } from "preact";
import { App } from "./app";
import { formatTimes } from "./time";
import type { View } from "./view";

const data = document.getElementById("view");
const root = document.getElementById("live");

if (data === null || root === null) {
  // The runs list is rendered by the server; only its times need the viewer's zone.
  formatTimes(document);
} else {
  const view = JSON.parse(data.textContent ?? "") as View;
  render(<App initial={view} live={root.dataset.live === "true"} />, root);
}
