/**
 * Entry-point regression test.
 *
 * `src/test/utils.tsx` wraps test subjects in `<Toaster>{children}</Toaster>`,
 * so every page test passes even when `src/main.tsx` mounts a childless
 * `<Toaster />` as a *sibling* of `<App />`. That shape puts every routed page
 * outside `ToastContext`, so `useToast()` throws on render and the real app
 * crashes on any page that calls it (`LoginPage`). The test harness and the
 * production mount had drifted apart with nothing to catch it.
 *
 * This test closes that gap by importing the real `src/main.tsx` module rather
 * than a hand-written copy of its provider tree, so it exercises whatever
 * shape the production entry point actually has.
 */
import { screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mockAuthFetch } from "@/test/mock-auth-fetch";

describe("main.tsx entry point", () => {
  beforeEach(() => {
    vi.resetModules();
    mockAuthFetch();

    const root = document.createElement("div");
    root.id = "root";
    document.body.appendChild(root);
  });

  afterEach(() => {
    document.getElementById("root")?.remove();
    vi.unstubAllGlobals();
  });

  it("renders /login — a routed page that calls useToast() — without a missing-provider crash", async () => {
    window.history.pushState({}, "", "/login");

    await import("@/main");

    await waitFor(() => {
      expect(screen.getByRole("heading", { name: /welcome back/i })).toBeInTheDocument();
    });
  });

  it("mounts every routed page inside the ToastContext provider", async () => {
    window.history.pushState({}, "", "/login");

    await import("@/main");

    // A rendered login form is only reachable if `useToast()` resolved a
    // provider; the hook throws otherwise and nothing below it renders.
    await waitFor(() => {
      expect(screen.getByLabelText(/email/i)).toBeInTheDocument();
    });
    expect(screen.getByRole("button", { name: /sign in/i })).toBeInTheDocument();
  });
});
