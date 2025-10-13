import type { ColorTheme } from "./state";

export const TERMINAL_THEMES: Record<string, ColorTheme> = {
  default: {
    name: "Default",
    primary: "#3b82f6", // Blue
    background: undefined,
    border: "#3b82f6",
  },
  ocean: {
    name: "Ocean",
    primary: "#06b6d4", // Cyan
    background: "rgba(6, 182, 212, 0.05)",
    border: "#06b6d4",
  },
  forest: {
    name: "Forest",
    primary: "#10b981", // Green
    background: "rgba(16, 185, 129, 0.05)",
    border: "#10b981",
  },
  sunset: {
    name: "Sunset",
    primary: "#f59e0b", // Orange
    background: "rgba(245, 158, 11, 0.05)",
    border: "#f59e0b",
  },
  lavender: {
    name: "Lavender",
    primary: "#8b5cf6", // Purple
    background: "rgba(139, 92, 246, 0.05)",
    border: "#8b5cf6",
  },
  rose: {
    name: "Rose",
    primary: "#ec4899", // Pink
    background: "rgba(236, 72, 153, 0.05)",
    border: "#ec4899",
  },
  crimson: {
    name: "Crimson",
    primary: "#ef4444", // Red
    background: "rgba(239, 68, 68, 0.05)",
    border: "#ef4444",
  },
  slate: {
    name: "Slate",
    primary: "#64748b", // Gray
    background: "rgba(100, 116, 139, 0.05)",
    border: "#64748b",
  },
};

/**
 * Get theme by ID, falling back to custom theme or default
 */
export function getTheme(themeId?: string, customTheme?: ColorTheme): ColorTheme {
  if (themeId === "custom" && customTheme) {
    return customTheme;
  }
  if (themeId && TERMINAL_THEMES[themeId]) {
    return TERMINAL_THEMES[themeId];
  }
  return TERMINAL_THEMES.default;
}

export interface ThemeStyles {
  borderColor: string;
  backgroundColor: string;
  hoverColor: string;
  activeColor: string;
}

/**
 * Calculate derived styles from a color theme
 */
export function getThemeStyles(theme: ColorTheme, isDark: boolean): ThemeStyles {
  const borderColor = theme.border;
  const backgroundColor = theme.background || (isDark ? "transparent" : "transparent");

  // For hover, brighten the primary color slightly
  const hoverColor = adjustColorBrightness(theme.primary, isDark ? 20 : -10);

  // For active, use primary color at full intensity
  const activeColor = theme.primary;

  return {
    borderColor,
    backgroundColor,
    hoverColor,
    activeColor,
  };
}

/**
 * Adjust color brightness (simple implementation)
 * Percent can be positive (brighten) or negative (darken)
 */
function adjustColorBrightness(color: string, percent: number): string {
  // Parse hex color
  const hex = color.replace("#", "");
  const r = Number.parseInt(hex.substring(0, 2), 16);
  const g = Number.parseInt(hex.substring(2, 4), 16);
  const b = Number.parseInt(hex.substring(4, 6), 16);

  // Adjust brightness
  const adjust = (value: number) => {
    const adjusted = value + (value * percent) / 100;
    return Math.max(0, Math.min(255, Math.round(adjusted)));
  };

  const newR = adjust(r);
  const newG = adjust(g);
  const newB = adjust(b);

  // Convert back to hex
  return `#${newR.toString(16).padStart(2, "0")}${newG.toString(16).padStart(2, "0")}${newB.toString(16).padStart(2, "0")}`;
}

/**
 * Check if current app is in dark mode
 */
export function isDarkMode(): boolean {
  return document.documentElement.classList.contains("dark");
}
