/**
 * Provider identity — the names and marks the dashboard uses for the
 * pools (`tokens.snapshot`'s `provider`) and the agents (`sweep.started`'s
 * `runtime`) it renders.
 *
 * The iconography deliberately matches the 2amlogic.com homepage fleet feed
 * (`marketing/website/src/components/ModelLabels.tsx`): Claude gets its
 * mark (vendored from Simple Icons, CC0, at `/icons/claude.svg`); every other
 * provider is a plain text label. One vocabulary, so a reader who has seen
 * one surface recognizes the other.
 *
 * Both vocabularies are open: an unrecognized provider or runtime name still
 * renders, as itself, rather than being dropped or rendered as "unknown" —
 * a new adapter (z.ai, …) shows up on the dashboard the day the daemon
 * first names it, with no UI change required.
 */

import { el } from "./dom";

/** Display names for the provider/runtime identifiers the daemon emits. */
const DISPLAY_NAME: Readonly<Record<string, string>> = {
  claude: "Claude",
  codex: "Codex",
  openai: "OpenAI",
  opencode: "OpenCode",
  aider: "Aider",
  pi: "Pi",
  zai: "z.ai",
  "z.ai": "z.ai",
};

export function providerDisplayName(provider: string): string {
  return DISPLAY_NAME[provider.toLowerCase()] ?? provider;
}

/** The provider a sweep's `runtime` adapter draws its accounts from. Today
 * every adapter name is also its provider name (`claude` → Claude's pool,
 * `codex` → Codex's), so this is the identity map; it exists so the one
 * place a runtime→provider divergence would land is named. */
export function runtimeProvider(runtime: string): string {
  return runtime.toLowerCase();
}

/**
 * A provider's mark: the Claude icon for Claude, a short text label for any
 * other provider. `withName` appends the display name after the mark (the
 * icon alone is enough in a dense sweep row; the token-pool label wants
 * both).
 */
export function providerMark(provider: string, withName = false): HTMLElement {
  const key = provider.toLowerCase();
  const name = providerDisplayName(provider);
  const mark = el("span", {
    class: `provider-mark provider-mark--${cssToken(key)}`,
    title: name,
    data: { testid: "provider-mark", provider: key },
  });
  if (key === "claude") {
    const icon = document.createElement("img");
    icon.src = "/icons/claude.svg";
    icon.alt = withName ? "" : name;
    icon.width = 14;
    icon.height = 14;
    icon.className = "provider-mark__icon";
    mark.appendChild(icon);
    if (withName) mark.appendChild(el("span", { class: "provider-mark__name" }, name));
  } else {
    mark.appendChild(el("span", { class: "provider-mark__name" }, name));
  }
  return mark;
}

/** A provider key as a safe CSS class fragment (`z.ai` → `z-ai`). */
function cssToken(key: string): string {
  return key.replace(/[^a-z0-9]+/g, "-");
}
