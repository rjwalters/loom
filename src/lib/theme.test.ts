import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { getCurrentTheme, initTheme, toggleTheme } from "./theme";

describe("theme", () => {
  let localStorageGetItemSpy: any;
  let localStorageSetItemSpy: any;
  let matchMediaMock: any;

  beforeEach(() => {
    // Reset document state
    document.documentElement.classList.remove("dark");

    // Spy on localStorage
    localStorageGetItemSpy = vi.spyOn(Storage.prototype, "getItem");
    localStorageSetItemSpy = vi.spyOn(Storage.prototype, "setItem");

    // Mock matchMedia
    matchMediaMock = vi.fn();
    window.matchMedia = matchMediaMock;
  });

  afterEach(() => {
    // Clean up
    localStorageGetItemSpy.mockRestore();
    localStorageSetItemSpy.mockRestore();
    document.documentElement.classList.remove("dark");
  });

  describe("initTheme", () => {
    it("uses stored dark theme from localStorage", () => {
      localStorageGetItemSpy.mockReturnValue("dark");
      matchMediaMock.mockReturnValue({ matches: false });

      initTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(true);
    });

    it("uses stored light theme from localStorage", () => {
      localStorageGetItemSpy.mockReturnValue("light");
      matchMediaMock.mockReturnValue({ matches: true }); // Even if system prefers dark

      initTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(false);
    });

    it("uses system preference when no stored theme (prefers dark)", () => {
      localStorageGetItemSpy.mockReturnValue(null);
      matchMediaMock.mockReturnValue({ matches: true });

      initTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(true);
    });

    it("uses system preference when no stored theme (prefers light)", () => {
      localStorageGetItemSpy.mockReturnValue(null);
      matchMediaMock.mockReturnValue({ matches: false });

      initTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(false);
    });

    it("defaults to light theme when localStorage is empty string", () => {
      localStorageGetItemSpy.mockReturnValue("");
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
      expect(localStorageSetItemSpy).toHaveBeenCalledWith("theme", "dark");
    });

    it("toggles from dark to light", () => {
      document.documentElement.classList.add("dark");

      toggleTheme();

      expect(document.documentElement.classList.contains("dark")).toBe(false);
      expect(localStorageSetItemSpy).toHaveBeenCalledWith("theme", "light");
    });

    it("persists theme to localStorage", () => {
      toggleTheme();

      expect(localStorageSetItemSpy).toHaveBeenCalledOnce();
      expect(localStorageSetItemSpy).toHaveBeenCalledWith("theme", expect.any(String));
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
      localStorageGetItemSpy.mockReturnValue("dark");
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
      localStorageGetItemSpy.mockReturnValue("light");
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
