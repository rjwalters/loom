// Re-export all UI functions for backward compatibility
// Consumers can still import from "./ui" and get all functions

export { renderHeader } from "./header";
export { getStatusColor } from "./helpers";
export { renderLoadingState } from "./loading";
export {
  type HealthCheckTiming,
  renderMissingSessionError,
  renderPrimaryTerminal,
} from "./terminal-primary";

// Placeholder exports for Phase 5 implementation
export function renderAnalyticsView(): void {
  // Analytics dashboard implementation (Phase 5)
  // For now, the placeholder HTML in index.html is sufficient
}

export function renderStatusBar(): void {
  // Status bar implementation (Phase 5)
  // For now, the placeholder HTML in index.html is sufficient
}
