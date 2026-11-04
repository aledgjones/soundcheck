// RFC 7386 merge patch. Whatever the patch doesn't touch keeps its identity, so only changed parts re-render.

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function applyPatch(target: unknown, patch: unknown): unknown {
  if (!isObject(patch)) {
    return patch;
  }

  const result: Record<string, unknown> = isObject(target) ? { ...target } : {};
  for (const [key, value] of Object.entries(patch)) {
    if (value === null) {
      delete result[key];
    } else {
      result[key] = applyPatch(result[key], value);
    }
  }

  return result;
}
