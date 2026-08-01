import { beforeEach, describe, expect, it } from "vitest";

import {
  SIGN_IN_PATH,
  SIGN_OUT_PATH,
  initialsFor,
  renderAccountMenu,
  wireAccountMenu,
} from "../src/accountMenu";

/** A stand-in for `window` carrying whatever the Worker injected. */
function scopeWith(value: unknown): typeof globalThis {
  return { __LOOM_FLEET__: value } as unknown as typeof globalThis;
}

function container(): HTMLElement {
  const node = document.createElement("div");
  document.body.appendChild(node);
  return node;
}

beforeEach(() => {
  document.body.replaceChildren();
});

describe("initialsFor", () => {
  it.each([
    ["robb.walters@example.com", "RW"],
    ["robb_walters@example.com", "RW"],
    ["robb-walters@example.com", "RW"],
    ["agent7@example.com", "A"],
    ["a@example.com", "A"],
  ])("derives initials from %s", (email, expected) => {
    expect(initialsFor(email)).toBe(expected);
  });

  // An authenticated token without an `email` claim is unusual but allowed by
  // the schema — render a neutral glyph rather than an empty circle.
  it.each([[undefined], [""], ["@example.com"]])("falls back to a glyph for %s", (email) => {
    expect(initialsFor(email as string | undefined)).toBe("●");
  });
});

describe("renderAccountMenu — anonymous", () => {
  it("shows a Sign in link pointing at the Access login app", () => {
    const node = container();
    renderAccountMenu(node, scopeWith({ authenticated: false }));

    const link = node.querySelector("a.account__signin");
    expect(link?.getAttribute("href")).toBe(SIGN_IN_PATH);
    expect(link?.textContent).toBe("Sign in");
  });

  it("renders no identity and no sign-out affordance", () => {
    const node = container();
    renderAccountMenu(node, scopeWith({ authenticated: false }));

    expect(node.querySelector('[data-testid="account-avatar"]')).toBeNull();
    expect(node.querySelector('[data-testid="sign-out"]')).toBeNull();
  });

  // Fail closed: every malformed shape must render Sign in, never a phantom
  // identity. A page that never received the injection is the common case
  // (e.g. the bundle loaded from somewhere the Worker did not stamp).
  it.each([
    ["flag absent", {}],
    ["authenticated:false", { authenticated: false }],
    ["truthy but not true", { authenticated: "yes" }],
    ["null", null],
    ["not an object", "authenticated"],
    ["undefined", undefined],
  ])("treats %s as anonymous", (_label, value) => {
    const node = container();
    renderAccountMenu(node, scopeWith(value));
    expect(node.querySelector("a.account__signin")).not.toBeNull();
  });
});

describe("renderAccountMenu — signed in", () => {
  const signedIn = { authenticated: true, email: "robb.walters@example.com" };

  it("shows an avatar with initials and no Sign in link", () => {
    const node = container();
    renderAccountMenu(node, scopeWith(signedIn));

    expect(node.querySelector('[data-testid="account-avatar"]')?.textContent).toBe("RW");
    expect(node.querySelector("a.account__signin")).toBeNull();
  });

  it("puts the email and a sign-out link in the menu", () => {
    const node = container();
    renderAccountMenu(node, scopeWith(signedIn));

    expect(node.querySelector('[data-testid="account-email"]')?.textContent).toBe("robb.walters@example.com");
    // Sign-out is Cloudflare Access's own endpoint — this app has no session
    // of its own to tear down.
    expect(node.querySelector('[data-testid="sign-out"]')?.getAttribute("href")).toBe(SIGN_OUT_PATH);
  });

  it("keeps the menu closed until the avatar is clicked, and toggles it", () => {
    const node = container();
    renderAccountMenu(node, scopeWith(signedIn));

    const avatar = node.querySelector('[data-testid="account-avatar"]') as HTMLElement;
    const menu = node.querySelector('[data-testid="account-menu"]') as HTMLElement;

    expect(menu.hasAttribute("hidden")).toBe(true);
    expect(avatar.getAttribute("aria-expanded")).toBe("false");

    avatar.click();
    expect(menu.hasAttribute("hidden")).toBe(false);
    expect(avatar.getAttribute("aria-expanded")).toBe("true");

    avatar.click();
    expect(menu.hasAttribute("hidden")).toBe(true);
    expect(avatar.getAttribute("aria-expanded")).toBe("false");
  });

  it("renders a signed-in viewer with no email claim without breaking", () => {
    const node = container();
    renderAccountMenu(node, scopeWith({ authenticated: true }));

    expect(node.querySelector('[data-testid="account-avatar"]')?.textContent).toBe("●");
    expect(node.querySelector('[data-testid="account-email"]')?.textContent).toBe("Signed in");
    expect(node.querySelector('[data-testid="sign-out"]')?.getAttribute("href")).toBe(SIGN_OUT_PATH);
  });

  // The email originates remotely (an IdP claim relayed through a JWT). Every
  // other view in this app pins the same property — see dom.ts's module doc.
  it("renders the email as text, never as markup", () => {
    const node = container();
    renderAccountMenu(node, scopeWith({ authenticated: true, email: "<img src=x onerror=alert(1)>@e.com" }));

    expect(node.querySelector("img")).toBeNull();
    expect(node.querySelector('[data-testid="account-email"]')?.textContent).toContain("<img");
  });

  it("replaces prior contents rather than appending on re-render", () => {
    const node = container();
    renderAccountMenu(node, scopeWith(signedIn));
    renderAccountMenu(node, scopeWith(signedIn));

    expect(node.querySelectorAll('[data-testid="account-avatar"]')).toHaveLength(1);
  });
});

describe("wireAccountMenu", () => {
  it("renders into #account when the page has one", () => {
    const host = container();
    host.id = "account";
    wireAccountMenu(document, scopeWith({ authenticated: true, email: "a.b@example.com" }));

    expect(host.querySelector('[data-testid="account-avatar"]')?.textContent).toBe("AB");
  });

  it("is a no-op on a page with no #account container", () => {
    expect(() => wireAccountMenu(document, scopeWith({ authenticated: false }))).not.toThrow();
  });
});
