// Re-export all UI functions for backward compatibility
// Consumers can still import from "./ui" and get all functions

export { renderHeader } from "./header";
export { getStatusColor } from "./helpers";
export { renderLoadingState } from "./loading";
export { renderMiniTerminals } from "./terminal-grid";
export {
  type HealthCheckTiming,
  renderMissingSessionError,
  renderPrimaryTerminal,
} from "./terminal-primary";
