import { beforeEach, describe, expect, it } from "vitest";

import { renderBuildInfo, wireBuildInfo } from "../src/buildInfo";

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

describe("renderBuildInfo", () => {
  it("renders a short commit stamp when one is injected", () => {
    const node = container();
    renderBuildInfo(node, scopeWith({ authenticated: false, commit: "abcdef1234567890" }));

    expect(node.textContent).toBe("build abcdef123456");
  });

  it("renders the same stamp for an authenticated viewer — commit is not identity", () => {
    const node = container();
    renderBuildInfo(node, scopeWith({ authenticated: true, email: "operator@2amlogic.com", commit: "abc1234" }));

    expect(node.textContent).toBe("build abc1234");
  });

  it("renders nothing when no commit was stamped (e.g. local wrangler dev)", () => {
    const node = container();
    renderBuildInfo(node, scopeWith({ authenticated: false }));

    expect(node.textContent).toBe("");
  });

  it("renders nothing when the global was never injected", () => {
    const node = container();
    renderBuildInfo(node, {} as unknown as typeof globalThis);

    expect(node.textContent).toBe("");
  });
});

describe("wireBuildInfo", () => {
  it("populates #build-info when the host page has one", () => {
    const footer = document.createElement("footer");
    footer.id = "build-info";
    document.body.appendChild(footer);

    wireBuildInfo(document, scopeWith({ authenticated: false, commit: "abc1234" }));

    expect(footer.textContent).toBe("build abc1234");
  });

  it("does nothing when the host page has no #build-info container", () => {
    // No throw, nothing to assert beyond "did not blow up" — mirrors
    // `wireAccountMenu`'s equivalent case.
    expect(() => wireBuildInfo(document, scopeWith({ authenticated: false, commit: "abc1234" }))).not.toThrow();
  });
});
