// Re-export all UI functions for backward compatibility
// Consumers can still import from "./ui" and get all functions

// Phase 5: Analytics dashboard and status bar
export { renderDashboardView, stopAutoRefresh } from "./dashboard-view";
export { renderHeader } from "./header";
export { getStatusColor } from "./helpers";
export { renderLoadingState } from "./loading";
export { renderStatusBar, stopStatusRefresh } from "./status-bar";
export {
  type HealthCheckTiming,
  renderMissingSessionError,
  renderPrimaryTerminal,
} from "./terminal-primary";
