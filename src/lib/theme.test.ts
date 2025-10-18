import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { getCurrentTheme, initTheme, toggleTheme } from "./theme";

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
      expect(getCurrentTheme()).toBe("dark");

      toggleTheme(); // dark -> light
      expect(getCurrentTheme()).toBe("light");

      toggleTheme(); // light -> dark
      expect(getCurrentTheme()).toBe("dark");
    });
  });

  describe("getCurrentTheme", () => {
    it("returns 'dark' when dark class is present", () => {
      document.documentElement.classList.add("dark");

      expect(getCurrentTheme()).toBe("dark");
    });

    it("returns 'light' when dark class is absent", () => {
      document.documentElement.classList.remove("dark");

      expect(getCurrentTheme()).toBe("light");
    });
  });

  describe("integration", () => {
    it("initTheme and getCurrentTheme work together", () => {
      localStorage.setItem("theme", "dark");
      matchMediaMock.mockReturnValue({ matches: false });

      initTheme();

      expect(getCurrentTheme()).toBe("dark");
    });

    it("toggleTheme and getCurrentTheme work together", () => {
      document.documentElement.classList.remove("dark");

      expect(getCurrentTheme()).toBe("light");

      toggleTheme();

      expect(getCurrentTheme()).toBe("dark");

      toggleTheme();

      expect(getCurrentTheme()).toBe("light");
    });

    it("full workflow: init -> toggle -> getCurrentTheme", () => {
      localStorage.setItem("theme", "light");
      matchMediaMock.mockReturnValue({ matches: false });

      initTheme();
      expect(getCurrentTheme()).toBe("light");

      toggleTheme();
      expect(getCurrentTheme()).toBe("dark");

      toggleTheme();
      expect(getCurrentTheme()).toBe("light");
    });
  });
});
