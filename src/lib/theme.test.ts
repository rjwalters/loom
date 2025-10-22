import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { initTheme, toggleTheme } from "./theme";

describe("theme", () => {
  let matchMediaMock: any;

  beforeEach(() => {
    // Reset document state
    document.documentElement.classList.remove("dark");

    // Clear localStorage
    localStorage.clear();

    // Mock matchMedia
    matchMediaMock = vi.fn();
    window.matchMedia = matchMediaMock;
  });

  afterEach(() => {
    // Clean up
    localStorage.clear();
    document.documentElement.classList.remove("dark");
  });

  describe("initTheme", () => {
    it("uses stored dark theme from localStorage", () => {
      localStorage.setItem("theme", "dark");
      matchMediaMock.mockReturnValue({ matches: false });

      initTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(true);
    });

    it("uses stored light theme from localStorage", () => {
      localStorage.setItem("theme", "light");
      matchMediaMock.mockReturnValue({ matches: true }); // Even if system prefers dark

      initTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(false);
    });

    it("uses system preference when no stored theme (prefers dark)", () => {
      // localStorage is already clear from beforeEach
      matchMediaMock.mockReturnValue({ matches: true });

      initTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(true);
    });

    it("uses system preference when no stored theme (prefers light)", () => {
      // localStorage is already clear from beforeEach
      matchMediaMock.mockReturnValue({ matches: false });

      initTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(false);
    });

    it("defaults to light theme when localStorage is empty string", () => {
      localStorage.setItem("theme", "");
      matchMediaMock.mockReturnValue({ matches: false });

      initTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(false);
    });
  });

  describe("toggleTheme", () => {
    it("toggles from light to dark", () => {
      document.documentElement.classList.remove("dark");

      toggleTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(true);
      expect(localStorage.getItem("theme")).toBe("dark");
    });

    it("toggles from dark to light", () => {
      document.documentElement.classList.add("dark");

      toggleTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(false);
      expect(localStorage.getItem("theme")).toBe("light");
    });

    it("persists theme to localStorage", () => {
      toggleTheme();

      const theme = localStorage.getItem("theme");
      expect(theme).toBeTruthy();
      expect(["dark", "light"]).toContain(theme);
    });

    it("toggles multiple times correctly", () => {
      document.documentElement.classList.remove("dark");

      toggleTheme(); // light -> dark
      expect(document.documentElement.classList.contains("dark")).toBe(true);

      toggleTheme(); // dark -> light
      expect(document.documentElement.classList.contains("dark")).toBe(false);

      toggleTheme(); // light -> dark
      expect(document.documentElement.classList.contains("dark")).toBe(true);
    });
  });

  describe("integration", () => {
    it("initTheme sets dark class correctly", () => {
      localStorage.setItem("theme", "dark");
      matchMediaMock.mockReturnValue({ matches: false });

      initTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(true);
    });

    it("toggleTheme updates dark class correctly", () => {
      document.documentElement.classList.remove("dark");

      expect(document.documentElement.classList.contains("dark")).toBe(false);

      toggleTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(true);

      toggleTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(false);
    });

    it("full workflow: init -> toggle -> check dark class", () => {
      localStorage.setItem("theme", "light");
      matchMediaMock.mockReturnValue({ matches: false });

      initTheme();
      expect(document.documentElement.classList.contains("dark")).toBe(false);

      toggleTheme();
      expect(document.documentElement.classList.contains("dark")).toBe(true);

      toggleTheme();
      expect(document.documentElement.classList.contains("dark")).toBe(false);
    });
  });
});
