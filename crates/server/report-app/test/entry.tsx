import { renderToString } from "preact-render-to-string";
import { App } from "../src/app";
import type { View } from "../src/view";

export { applyPatch } from "../src/patch";

export function renderView(view: View, live = false): string {
  return renderToString(<App initial={view} live={live} />);
}
