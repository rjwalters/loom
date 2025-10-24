import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { ColorTheme } from "./state";
import { getTheme, getThemeForRole, getThemeStyles, isDarkMode, TERMINAL_THEMES } from "./themes";

describe("themes", () => {
  describe("TERMINAL_THEMES", () => {
    it("should have all expected themes", () => {
      expect(TERMINAL_THEMES.default).toBeDefined();
      expect(TERMINAL_THEMES.ocean).toBeDefined();
      expect(TERMINAL_THEMES.forest).toBeDefined();
      expect(TERMINAL_THEMES.sunset).toBeDefined();
      expect(TERMINAL_THEMES.lavender).toBeDefined();
      expect(TERMINAL_THEMES.rose).toBeDefined();
      expect(TERMINAL_THEMES.crimson).toBeDefined();
      expect(TERMINAL_THEMES.slate).toBeDefined();
    });

    it("should have required properties for each theme", () => {
      for (const [themeId, theme] of Object.entries(TERMINAL_THEMES)) {
        expect(theme.name, `${themeId} should have name`).toBeDefined();
        expect(theme.primary, `${themeId} should have primary`).toBeDefined();
        expect(theme.border, `${themeId} should have border`).toBeDefined();
      }
    });
  });

  describe("getTheme", () => {
    it("should return theme by ID", () => {
      const theme = getTheme("ocean");
      expect(theme.name).toBe("Ocean");
      expect(theme.primary).toBe("#06b6d4");
    });

    it("should return default theme for invalid ID", () => {
      const theme = getTheme("nonexistent");
      expect(theme).toEqual(TERMINAL_THEMES.default);
    });

    it("should return default theme when no ID provided", () => {
      const theme = getTheme();
      expect(theme).toEqual(TERMINAL_THEMES.default);
    });

    it("should return custom theme when themeId is 'custom'", () => {
      const customTheme: ColorTheme = {
        name: "Custom",
        primary: "#ff0000",
        border: "#ff0000",
      };

      const theme = getTheme("custom", customTheme);
      expect(theme).toEqual(customTheme);
    });

    it("should return default when themeId is 'custom' but no customTheme provided", () => {
      const theme = getTheme("custom");
      expect(theme).toEqual(TERMINAL_THEMES.default);
    });
  });

  describe("getThemeStyles", () => {
    it("should generate styles for light mode", () => {
      const theme = TERMINAL_THEMES.ocean;
      const styles = getThemeStyles(theme, false);

      expect(styles.borderColor).toBe(theme.border);
      expect(styles.backgroundColor).toBe(theme.background);
      expect(styles.activeColor).toBe(theme.primary);
      expect(styles.hoverColor).toBeDefined();
      expect(styles.hoverColor).not.toBe(theme.primary);
    });

    it("should generate styles for dark mode", () => {
      const theme = TERMINAL_THEMES.ocean;
      const styles = getThemeStyles(theme, true);

      expect(styles.borderColor).toBe(theme.border);
      expect(styles.backgroundColor).toBe(theme.background);
      expect(styles.activeColor).toBe(theme.primary);
      expect(styles.hoverColor).toBeDefined();
    });

    it("should use transparent background for themes without background", () => {
      const theme = TERMINAL_THEMES.default;
      const stylesLight = getThemeStyles(theme, false);
      const stylesDark = getThemeStyles(theme, true);

      expect(stylesLight.backgroundColor).toBe("transparent");
      expect(stylesDark.backgroundColor).toBe("transparent");
    });

    it("should generate different hover colors for light and dark modes", () => {
      const theme = TERMINAL_THEMES.ocean;
      const lightStyles = getThemeStyles(theme, false);
      const darkStyles = getThemeStyles(theme, true);

      expect(lightStyles.hoverColor).not.toBe(darkStyles.hoverColor);
    });
  });

  describe("isDarkMode", () => {
    beforeEach(() => {
      // Reset dark mode class
      document.documentElement.classList.remove("dark");
    });

    afterEach(() => {
      // Cleanup
      document.documentElement.classList.remove("dark");
    });

    it("should return false when dark mode is not active", () => {
      expect(isDarkMode()).toBe(false);
    });

    it("should return true when dark mode is active", () => {
      document.documentElement.classList.add("dark");
      expect(isDarkMode()).toBe(true);
    });
  });

  describe("getThemeForRole", () => {
    // Analysis/strategic roles → lavender (purple)
    it("should map architect role to lavender theme", () => {
      expect(getThemeForRole("architect.md")).toBe("lavender");
    });

    it("should map judge role to lavender theme", () => {
      expect(getThemeForRole("judge.md")).toBe("lavender");
    });

    // Organization/curation roles → ocean (cyan)
    it("should map curator role to ocean theme", () => {
      expect(getThemeForRole("curator.md")).toBe("ocean");
    });

    it("should map guide role to ocean theme", () => {
      expect(getThemeForRole("guide.md")).toBe("ocean");
    });

    // Building/implementation roles → forest (green)
    it("should map builder role to forest theme", () => {
      expect(getThemeForRole("builder.md")).toBe("forest");
    });

    it("should map hermit role to forest theme", () => {
      expect(getThemeForRole("hermit.md")).toBe("forest");
    });

    // Fixing/treating role → rose (pink)
    it("should map doctor role to rose theme", () => {
      expect(getThemeForRole("doctor.md")).toBe("rose");
    });

    // Leadership/visibility role → sunset (orange)
    it("should map champion role to sunset theme", () => {
      expect(getThemeForRole("champion.md")).toBe("sunset");
    });

    // General-purpose role → slate (gray)
    it("should map driver role to slate theme", () => {
      expect(getThemeForRole("driver.md")).toBe("slate");
    });

    it("should return default for plain shell (undefined role)", () => {
      expect(getThemeForRole()).toBe("default");
    });

    it("should return default for unmapped roles", () => {
      expect(getThemeForRole("custom-role.md")).toBe("default");
    });
  });
});
