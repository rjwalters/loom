export function initTheme(): void {
  const stored = localStorage.getItem("theme");
  const prefersDark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  const theme = stored || (prefersDark ? "dark" : "light");

  document.documentElement.classList.toggle("dark", theme === "dark");
}

export function toggleTheme(): void {
  const wasDark = document.documentElement.classList.contains("dark");
  const isDark = document.documentElement.classList.toggle("dark");
  const newTheme = isDark ? "dark" : "light";
  localStorage.setItem("theme", newTheme);

  // Update theme icon
  const icon = document.getElementById("theme-icon");
  if (icon) {
    icon.textContent = isDark ? "☀️" : "🌙";
  }

  // Update xterm terminal themes
  import("./terminal-manager")
    .then(({ getTerminalManager }) => {
      const manager = getTerminalManager();
      manager.updateAllThemes(isDark);
    })
    .catch(() => {
      // Silently fail - terminal themes are not critical
    });
}
