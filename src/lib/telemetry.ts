/**
 * Telemetry Module for Loom
 *
 * Provides performance monitoring, error tracking, and usage analytics.
 * All data is stored locally by default (privacy-first approach).
 *
 * @example
 * import { trackPerformance, captureError, trackEvent } from "./telemetry";
 *
 * // Track performance of async operations
 * const result = await trackPerformance("createTerminal", "ipc", async () => {
 *   return await invoke("create_terminal", { ... });
 * });
 *
 * // Track errors
 * captureError(error, { component: "terminal-manager" });
 *
 * // Track usage events
 * trackEvent("terminal_created", "feature", { role: "builder" });
 */

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";

const logger = Logger.forComponent("telemetry");

// ============================================================================
// Types
// ============================================================================

/**
 * Categories for performance metrics
 */
export type PerformanceCategory = "ui" | "ipc" | "agent" | "git" | "file" | "system";

/**
 * Performance metric entry
 */
export interface PerformanceMetric {
  name: string;
  duration_ms: number;
  timestamp: string;
  category: PerformanceCategory;
  success: boolean;
  metadata?: Record<string, unknown>;
}

/**
 * Error report entry
 */
export interface ErrorReport {
  message: string;
  stack?: string;
  timestamp: string;
  component: string;
  context?: Record<string, unknown>;
}

/**
 * Categories for usage events
 */
export type UsageCategory = "feature" | "interaction" | "workflow";

/**
 * Usage event entry
 */
export interface UsageEvent {
  event_name: string;
  category: UsageCategory;
  timestamp: string;
  properties?: Record<string, unknown>;
}

/**
 * Telemetry settings stored in localStorage
 */
interface TelemetrySettings {
  performanceEnabled: boolean;
  errorTrackingEnabled: boolean;
  usageAnalyticsEnabled: boolean;
}

// ============================================================================
// Settings Management
// ============================================================================

const SETTINGS_KEY = "loom:telemetry:settings";

/**
 * Default telemetry settings (all enabled by default, local-only)
 */
const DEFAULT_SETTINGS: TelemetrySettings = {
  performanceEnabled: true,
  errorTrackingEnabled: true,
  usageAnalyticsEnabled: true,
};

/**
 * Get current telemetry settings
 */
export function getTelemetrySettings(): TelemetrySettings {
  try {
    const stored = localStorage.getItem(SETTINGS_KEY);
    if (stored) {
      return { ...DEFAULT_SETTINGS, ...JSON.parse(stored) };
    }
  } catch {
    // Ignore parse errors
  }
  return DEFAULT_SETTINGS;
}

/**
 * Update telemetry settings
 */
export function setTelemetrySettings(settings: Partial<TelemetrySettings>): void {
  const current = getTelemetrySettings();
  const updated = { ...current, ...settings };
  localStorage.setItem(SETTINGS_KEY, JSON.stringify(updated));
  logger.info("Telemetry settings updated", updated);
}

/**
 * Check if performance tracking is enabled
 */
export function isPerformanceEnabled(): boolean {
  return getTelemetrySettings().performanceEnabled;
}

/**
 * Check if error tracking is enabled
 */
export function isErrorTrackingEnabled(): boolean {
  return getTelemetrySettings().errorTrackingEnabled;
}

/**
 * Check if usage analytics is enabled
 */
export function isUsageAnalyticsEnabled(): boolean {
  return getTelemetrySettings().usageAnalyticsEnabled;
}

// ============================================================================
// Performance Tracking
// ============================================================================

/** Threshold in ms above which we log a warning for slow operations */
const SLOW_OPERATION_THRESHOLD_MS = 1000;

/**
 * Track performance of an async operation
 *
 * Wraps an async function and measures its execution time. The metric is
 * stored locally and a warning is logged if the operation exceeds the
 * slow operation threshold.
 *
 * @param name - Name of the operation being tracked
 * @param category - Category of the operation (ui, ipc, agent, git, file, system)
 * @param operation - The async function to execute and measure
 * @param metadata - Optional additional context to store with the metric
 * @returns The result of the operation
 *
 * @example
 * const terminal = await trackPerformance("createTerminal", "ipc", async () => {
 *   return await invoke("create_terminal", { configId, name, workingDir });
 * }, { terminalCount: 5 });
 */
export async function trackPerformance<T>(
  name: string,
  category: PerformanceCategory,
  operation: () => Promise<T>,
  metadata?: Record<string, unknown>
): Promise<T> {
  if (!isPerformanceEnabled()) {
    return operation();
  }

  const startTime = performance.now();
  let success = true;

  try {
    const result = await operation();
    return result;
  } catch (error) {
    success = false;
    throw error;
  } finally {
    const duration = performance.now() - startTime;

    const metric: PerformanceMetric = {
      name,
      duration_ms: duration,
      timestamp: new Date().toISOString(),
      category,
      success,
      metadata,
    };

    // Log performance metric (non-blocking)
    logPerformanceMetric(metric);

    // Warn if operation is slow
    if (duration > SLOW_OPERATION_THRESHOLD_MS) {
      logger.warn("Slow operation detected", {
        name,
        duration_ms: Math.round(duration),
        category,
        success,
      });
    }
  }
}

/**
 * Log a performance metric to the backend database
 */
async function logPerformanceMetric(metric: PerformanceMetric): Promise<void> {
  try {
    await invoke("log_performance_metric", { metric });
  } catch (error) {
    // Non-blocking - log error but don't throw
    logger.error("Failed to log performance metric", error as Error, {
      metricName: metric.name,
    });
  }
}

/**
 * Get performance metrics from the database
 *
 * @param category - Optional category filter
 * @param since - Optional ISO timestamp to filter metrics after
 * @param limit - Maximum number of metrics to return (default 100)
 */
export async function getPerformanceMetrics(
  category?: PerformanceCategory,
  since?: string,
  limit?: number
): Promise<PerformanceMetric[]> {
  try {
    return await invoke<PerformanceMetric[]>("get_performance_metrics", {
      category,
      since,
      limit: limit ?? 100,
    });
  } catch (error) {
    logger.error("Failed to get performance metrics", error as Error);
    return [];
  }
}

/**
 * Performance statistics for a category or overall
 */
export interface PerformanceStats {
  category: string;
  count: number;
  avg_duration_ms: number;
  max_duration_ms: number;
  min_duration_ms: number;
  success_rate: number;
}

/**
 * Get performance statistics grouped by category
 *
 * @param since - Optional ISO timestamp to filter metrics after
 */
export async function getPerformanceStats(since?: string): Promise<PerformanceStats[]> {
  try {
    return await invoke<PerformanceStats[]>("get_performance_stats", { since });
  } catch (error) {
    logger.error("Failed to get performance stats", error as Error);
    return [];
  }
}

// ============================================================================
// Error Tracking
// ============================================================================

/**
 * Capture and log an error
 *
 * @param error - The error to capture
 * @param context - Additional context about where/why the error occurred
 */
export async function captureError(
  error: Error | unknown,
  context?: { component?: string; [key: string]: unknown }
): Promise<void> {
  if (!isErrorTrackingEnabled()) {
    return;
  }

  const errorReport: ErrorReport = {
    message: error instanceof Error ? error.message : String(error),
    stack: error instanceof Error ? error.stack : undefined,
    timestamp: new Date().toISOString(),
    component: context?.component ?? "unknown",
    context,
  };

  // Log to console as well
  logger.error("Error captured", error as Error, context);

  try {
    await invoke("log_error_report", { error: errorReport });
  } catch {
    // Non-blocking - already logged to console via logger above
  }
}

/**
 * Get error reports from the database
 *
 * @param since - Optional ISO timestamp to filter errors after
 * @param limit - Maximum number of errors to return (default 50)
 */
export async function getErrorReports(since?: string, limit?: number): Promise<ErrorReport[]> {
  try {
    return await invoke<ErrorReport[]>("get_error_reports", {
      since,
      limit: limit ?? 50,
    });
  } catch (error) {
    logger.error("Failed to get error reports", error as Error);
    return [];
  }
}

/**
 * Initialize global error handlers for uncaught errors and promise rejections
 *
 * Call this once during app initialization to catch all unhandled errors.
 */
export function initializeErrorTracking(): void {
  // Catch uncaught errors
  window.addEventListener("error", (event) => {
    captureError(event.error ?? new Error(event.message), {
      component: "global-error-handler",
      filename: event.filename,
      lineno: event.lineno,
      colno: event.colno,
    });
  });

  // Catch unhandled promise rejections
  window.addEventListener("unhandledrejection", (event) => {
    captureError(event.reason ?? new Error("Unhandled Promise Rejection"), {
      component: "unhandled-rejection",
      reason: String(event.reason),
    });
  });

  logger.info("Error tracking initialized");
}

// ============================================================================
// Usage Analytics
// ============================================================================

/**
 * Track a usage event
 *
 * @param eventName - Name of the event (e.g., "terminal_created")
 * @param category - Category of the event (feature, interaction, workflow)
 * @param properties - Optional properties to attach to the event
 */
export async function trackEvent(
  eventName: string,
  category: UsageCategory,
  properties?: Record<string, unknown>
): Promise<void> {
  if (!isUsageAnalyticsEnabled()) {
    return;
  }

  const event: UsageEvent = {
    event_name: eventName,
    category,
    timestamp: new Date().toISOString(),
    properties,
  };

  try {
    await invoke("log_usage_event", { event });
  } catch (error) {
    // Non-blocking - log error but don't throw
    logger.error("Failed to log usage event", error as Error, { eventName });
  }
}

/**
 * Get usage events from the database
 *
 * @param category - Optional category filter
 * @param since - Optional ISO timestamp to filter events after
 * @param limit - Maximum number of events to return (default 100)
 */
export async function getUsageEvents(
  category?: UsageCategory,
  since?: string,
  limit?: number
): Promise<UsageEvent[]> {
  try {
    return await invoke<UsageEvent[]>("get_usage_events", {
      category,
      since,
      limit: limit ?? 100,
    });
  } catch (error) {
    logger.error("Failed to get usage events", error as Error);
    return [];
  }
}

/**
 * Usage statistics for events
 */
export interface UsageStats {
  event_name: string;
  category: string;
  count: number;
  last_occurrence: string;
}

/**
 * Get usage statistics grouped by event name
 *
 * @param since - Optional ISO timestamp to filter events after
 */
export async function getUsageStats(since?: string): Promise<UsageStats[]> {
  try {
    return await invoke<UsageStats[]>("get_usage_stats", { since });
  } catch (error) {
    logger.error("Failed to get usage stats", error as Error);
    return [];
  }
}

// ============================================================================
// Data Management
// ============================================================================

/**
 * Export all telemetry data as JSON
 */
export async function exportTelemetryData(): Promise<{
  performanceMetrics: PerformanceMetric[];
  errorReports: ErrorReport[];
  usageEvents: UsageEvent[];
  exportedAt: string;
}> {
  const [performanceMetrics, errorReports, usageEvents] = await Promise.all([
    getPerformanceMetrics(undefined, undefined, 10000),
    getErrorReports(undefined, 10000),
    getUsageEvents(undefined, undefined, 10000),
  ]);

  return {
    performanceMetrics,
    errorReports,
    usageEvents,
    exportedAt: new Date().toISOString(),
  };
}

/**
 * Delete all telemetry data
 */
export async function deleteTelemetryData(): Promise<void> {
  try {
    await invoke("delete_telemetry_data");
    logger.info("Telemetry data deleted");
  } catch (error) {
    logger.error("Failed to delete telemetry data", error as Error);
    throw error;
  }
}
