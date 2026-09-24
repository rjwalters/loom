import { describe, expect, it } from "vitest";

import { branchUrl, forgeLink, issueUrl, repoUrl, sweepWorkTitle, sweepWorkUrl } from "../src/forgeLinks";

describe("forge URL builders", () => {
  it("resolves a well-formed owner/repo slug against github.com", () => {
    expect(repoUrl("rjwalters/loom")).toBe("https://github.com/rjwalters/loom");
    expect(repoUrl("2AMLogic/gf180-sar-adc")).toBe("https://github.com/2AMLogic/gf180-sar-adc");
    expect(issueUrl("rjwalters/loom", 4703)).toBe("https://github.com/rjwalters/loom/issues/4703");
    expect(branchUrl("rjwalters/loom", 4703)).toBe("https://github.com/rjwalters/loom/tree/feature/issue-4703");
  });

  it("refuses anything that is not a two-segment slug", () => {
    // The telemetry's `repo` falls back to the workspace-root *path* when no
    // GitHub remote resolves; that must never become a github.com link.
    expect(repoUrl("/Users/op/repos/loom")).toBeUndefined();
    expect(repoUrl("loom")).toBeUndefined();
    expect(repoUrl("a/b/c")).toBeUndefined();
    expect(repoUrl("owner/repo name")).toBeUndefined();
    // Redacted (private repo, public viewer) or simply absent.
    expect(repoUrl(undefined)).toBeUndefined();
    expect(repoUrl("")).toBeUndefined();
    expect(issueUrl(undefined, 4703)).toBeUndefined();
    expect(issueUrl("rjwalters/loom", undefined)).toBeUndefined();
    expect(branchUrl("rjwalters/loom", undefined)).toBeUndefined();
  });

  it("sends a sweep to its branch once Builder has run, and to its issue before that", () => {
    for (const phase of ["builder", "judge", "doctor", "merge"]) {
      expect(sweepWorkUrl("rjwalters/loom", 42, phase)).toBe("https://github.com/rjwalters/loom/tree/feature/issue-42");
      expect(sweepWorkTitle(42, phase)).toBe("Branch feature/issue-42 on the forge");
    }
    for (const phase of ["curator", undefined, "unheard-of-phase"]) {
      expect(sweepWorkUrl("rjwalters/loom", 42, phase)).toBe("https://github.com/rjwalters/loom/issues/42");
      expect(sweepWorkTitle(42, phase)).toBe("Issue #42 on the forge");
    }
    expect(sweepWorkUrl(undefined, 42, "builder")).toBeUndefined();
    expect(sweepWorkUrl("rjwalters/loom", undefined, "builder")).toBeUndefined();
    expect(sweepWorkTitle(undefined, "builder")).toBeUndefined();
  });
});

describe("forgeLink", () => {
  it("renders an off-site anchor that opens in a new tab", () => {
    const link = forgeLink("rjwalters/loom", "https://github.com/rjwalters/loom", "card__repo-label", "tip");
    expect(link.tagName).toBe("A");
    expect(link.getAttribute("href")).toBe("https://github.com/rjwalters/loom");
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toBe("noopener noreferrer");
    expect(link.getAttribute("title")).toBe("tip");
    expect(link.className).toBe("card__repo-label forge-link");
    expect(link.textContent).toBe("rjwalters/loom");
  });

  it("falls back to the plain span, same class and text, when nothing is linkable", () => {
    const span = forgeLink("#42", undefined, "card__sweep-label");
    expect(span.tagName).toBe("SPAN");
    expect(span.className).toBe("card__sweep-label");
    expect(span.textContent).toBe("#42");
    expect(span.hasAttribute("href")).toBe(false);
  });
});
