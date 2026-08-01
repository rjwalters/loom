/**
 * The topbar account control — the standard app-shell pattern: an avatar
 * button at top right that opens a menu with the signed-in identity and a
 * sign-out action, or a plain Sign in button when nobody is signed in.
 *
 * ## Why this exists
 *
 * Under the single-URL layout (issue #4795) `/` serves the same bundle to
 * everyone and the Worker stamps the viewer's identity into the page
 * (`../../src/index.ts`'s `handleRoot` → `api.ts`'s `readAuthState`). Before
 * this module the SPA had no sign-in affordance at all: the server-rendered
 * fallback page had one, but that page only renders when no UI build is
 * uploaded, so the link vanished the moment the SPA became what `/` served.
 * An anonymous visitor's only route in was typing `/login` by hand.
 *
 * ## Sign-out is Cloudflare's, not ours
 *
 * `/cdn-cgi/access/logout` is an Access-provided endpoint that clears the
 * `CF_Authorization` cookie. There is no app-side session to tear down — the
 * cookie *is* the session — so signing out is a plain link to it, and this
 * app deliberately implements no logout logic of its own.
 *
 * ## No identity is inferred here
 *
 * The email is rendered, never trusted: it comes from a signature-verified
 * JWT the Worker already validated, and nothing in this module grants access
 * to anything. Editing it in devtools changes a label and nothing else — the
 * data split is enforced server-side by `/api/*` vs `/public/*`.
 */

import { el } from "./dom";
import { readAuthState } from "./api";

/** Access's own logout endpoint; clears the `CF_Authorization` cookie. */
export const SIGN_OUT_PATH = "/cdn-cgi/access/logout";

/** The Access application that mints the session cookie. */
export const SIGN_IN_PATH = "/login";

/**
 * Up to two initials for the avatar, derived from the email's local part.
 *
 * `robb.walters@example.com` → `RW`; `agent7@example.com` → `A`. Falls back
 * to a neutral glyph rather than throwing or rendering an empty circle when
 * there is no usable email — an authenticated token without an `email` claim
 * is unusual but permitted by the schema.
 */
export function initialsFor(email: string | undefined): string {
  const local = (email ?? "").split("@")[0] ?? "";
  const parts = local.split(/[._-]+/).filter(Boolean);
  if (parts.length === 0) return "●";
  if (parts.length === 1) return (parts[0]![0] ?? "●").toUpperCase();
  return `${parts[0]![0]}${parts[1]![0]}`.toUpperCase();
}

function signInButton(): HTMLElement {
  return el("a", { class: "button account__signin", href: SIGN_IN_PATH }, "Sign in");
}

function signedInMenu(email: string | undefined): HTMLElement {
  const label = email ?? "Signed in";

  const avatar = el(
    "button",
    {
      class: "account__avatar",
      type: "button",
      data: { testid: "account-avatar" },
      aria: {
        // The avatar renders initials, which a screen reader would otherwise
        // announce as two stray letters.
        label: `Account: ${label}`,
        expanded: "false",
        haspopup: "menu",
      },
    },
    initialsFor(email),
  );

  const menu = el(
    "div",
    { class: "account__menu", role: "menu", data: { testid: "account-menu" } },
    el("p", { class: "account__email", data: { testid: "account-email" } }, label),
    el(
      "a",
      { class: "account__signout", role: "menuitem", href: SIGN_OUT_PATH, data: { testid: "sign-out" } },
      "Sign out",
    ),
  );
  // `hidden` is a boolean attribute with no `Attrs` field; set directly
  // rather than widening the shared builder for one caller.
  menu.setAttribute("hidden", "");

  avatar.addEventListener("click", () => {
    const open = avatar.getAttribute("aria-expanded") === "true";
    avatar.setAttribute("aria-expanded", String(!open));
    if (open) menu.setAttribute("hidden", "");
    else menu.removeAttribute("hidden");
  });

  return el("div", { class: "account" }, avatar, menu);
}

/**
 * Render the account control into `container`, replacing its contents.
 *
 * Reads the injected auth state by default; `scope` is injected in tests.
 * Anonymous is the fail-closed default (see `readAuthState`), so a page that
 * never received the injection shows Sign in rather than a phantom identity.
 */
export function renderAccountMenu(container: HTMLElement, scope: typeof globalThis = globalThis): void {
  const state = readAuthState(scope);
  container.replaceChildren(state.authenticated ? signedInMenu(state.email) : signInButton());
}

/** Render into `#account` if the host page has one. A page without the
 * container simply has no account control — not an error. */
export function wireAccountMenu(doc: Document, scope: typeof globalThis = globalThis): void {
  const container = doc.getElementById("account");
  if (container instanceof HTMLElement) renderAccountMenu(container, scope);
}
