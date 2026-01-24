/**
 * circuit-breaker.ts - Circuit breaker pattern for daemon IPC resilience
 *
 * The circuit breaker prevents cascading failures when the daemon becomes
 * unresponsive or overloaded. Instead of queueing up IPC calls that will
 * eventually timeout, it fails fast when the circuit is open.
 *
 * States:
 * - CLOSED: Normal operation, all calls pass through
 * - OPEN: Failure threshold exceeded, calls fail immediately
 * - HALF_OPEN: Recovery testing, limited calls allowed to probe daemon health
 *
 * @see health-monitor.ts for integration with daemon health checks
 */

import { Logger } from "./logger";

const logger = Logger.forComponent("circuit-breaker");

/**
 * Circuit breaker states
 */
export enum CircuitState {
  CLOSED = "closed",
  OPEN = "open",
  HALF_OPEN = "half-open",
}

/**
 * Configuration for circuit breaker behavior
 */
export interface CircuitBreakerConfig {
  /** Number of consecutive failures before opening circuit (default: 3) */
  failureThreshold: number;
  /** Time in ms before trying half-open state (default: 10000) */
  recoveryTimeout: number;
  /** Number of successes in half-open before closing circuit (default: 2) */
  successThreshold: number;
  /** Name for logging purposes */
  name: string;
}

/**
 * Snapshot of circuit breaker state for monitoring
 */
export interface CircuitBreakerSnapshot {
  state: CircuitState;
  failures: number;
  successes: number;
  lastFailureTime: number | null;
  lastSuccessTime: number | null;
  lastStateChange: number | null;
}

/**
 * Event types emitted by the circuit breaker
 */
export type CircuitBreakerEvent =
  | { type: "state-change"; from: CircuitState; to: CircuitState }
  | { type: "failure"; failures: number; threshold: number }
  | { type: "success"; state: CircuitState }
  | { type: "rejected"; state: CircuitState };

/**
 * Circuit breaker error thrown when circuit is open
 */
export class CircuitOpenError extends Error {
  constructor(
    public readonly circuitName: string,
    public readonly state: CircuitState
  ) {
    super(`Circuit breaker '${circuitName}' is ${state}, request rejected`);
    this.name = "CircuitOpenError";
  }
}

/**
 * CircuitBreaker - Implements the circuit breaker pattern for IPC resilience
 *
 * Usage:
 * ```typescript
 * const breaker = new CircuitBreaker({ name: 'daemon-ipc', ... });
 *
 * try {
 *   const result = await breaker.execute(() => invoke('some_command'));
 * } catch (error) {
 *   if (error instanceof CircuitOpenError) {
 *     // Circuit is open, fail fast
 *   } else {
 *     // Actual operation error
 *   }
 * }
 * ```
 */
export class CircuitBreaker {
  private state: CircuitState = CircuitState.CLOSED;
  private failures: number = 0;
  private successes: number = 0;
  private lastFailureTime: number | null = null;
  private lastSuccessTime: number | null = null;
  private lastStateChange: number | null = null;
  private recoveryTimer: number | null = null;
  private listeners: Set<(event: CircuitBreakerEvent) => void> = new Set();

  private readonly config: CircuitBreakerConfig;

  constructor(config: Partial<CircuitBreakerConfig> & { name: string }) {
    this.config = {
      failureThreshold: config.failureThreshold ?? 3,
      recoveryTimeout: config.recoveryTimeout ?? 10000,
      successThreshold: config.successThreshold ?? 2,
      name: config.name,
    };

    logger.info("Circuit breaker created", {
      name: this.config.name,
      failureThreshold: this.config.failureThreshold,
      recoveryTimeout: this.config.recoveryTimeout,
      successThreshold: this.config.successThreshold,
    });
  }

  /**
   * Execute an operation through the circuit breaker
   *
   * @param operation - Async operation to execute
   * @returns Promise resolving to operation result
   * @throws CircuitOpenError if circuit is open
   * @throws Original error if operation fails
   */
  async execute<T>(operation: () => Promise<T>): Promise<T> {
    if (!this.canAttempt()) {
      this.emitEvent({ type: "rejected", state: this.state });
      throw new CircuitOpenError(this.config.name, this.state);
    }

    try {
      const result = await operation();
      this.recordSuccess();
      return result;
    } catch (error) {
      this.recordFailure();
      throw error;
    }
  }

  /**
   * Check if an operation can be attempted
   *
   * @returns true if circuit allows the operation
   */
  canAttempt(): boolean {
    switch (this.state) {
      case CircuitState.CLOSED:
        return true;

      case CircuitState.OPEN:
        // Check if recovery timeout has elapsed
        if (this.lastFailureTime !== null) {
          const elapsed = Date.now() - this.lastFailureTime;
          if (elapsed >= this.config.recoveryTimeout) {
            this.transitionTo(CircuitState.HALF_OPEN);
            return true;
          }
        }
        return false;

      case CircuitState.HALF_OPEN:
        // Allow limited probing in half-open state
        return true;

      default:
        return false;
    }
  }

  /**
   * Record a successful operation
   */
  recordSuccess(): void {
    this.lastSuccessTime = Date.now();
    this.successes++;

    logger.info("Operation succeeded", {
      name: this.config.name,
      state: this.state,
      successes: this.successes,
    });

    this.emitEvent({ type: "success", state: this.state });

    switch (this.state) {
      case CircuitState.HALF_OPEN:
        // Check if we've had enough successes to close the circuit
        if (this.successes >= this.config.successThreshold) {
          this.transitionTo(CircuitState.CLOSED);
        }
        break;

      case CircuitState.CLOSED:
        // Reset failure count on success
        this.failures = 0;
        break;
    }
  }

  /**
   * Record a failed operation
   */
  recordFailure(): void {
    this.lastFailureTime = Date.now();
    this.failures++;

    logger.warn("Operation failed", {
      name: this.config.name,
      state: this.state,
      failures: this.failures,
      threshold: this.config.failureThreshold,
    });

    this.emitEvent({
      type: "failure",
      failures: this.failures,
      threshold: this.config.failureThreshold,
    });

    switch (this.state) {
      case CircuitState.CLOSED:
        // Check if failure threshold exceeded
        if (this.failures >= this.config.failureThreshold) {
          this.transitionTo(CircuitState.OPEN);
        }
        break;

      case CircuitState.HALF_OPEN:
        // Any failure in half-open immediately re-opens
        this.transitionTo(CircuitState.OPEN);
        break;
    }
  }

  /**
   * Get current circuit state
   */
  getState(): CircuitState {
    return this.state;
  }

  /**
   * Get a snapshot of circuit breaker state for monitoring
   */
  getSnapshot(): CircuitBreakerSnapshot {
    return {
      state: this.state,
      failures: this.failures,
      successes: this.successes,
      lastFailureTime: this.lastFailureTime,
      lastSuccessTime: this.lastSuccessTime,
      lastStateChange: this.lastStateChange,
    };
  }

  /**
   * Check if the circuit is allowing operations
   */
  isAvailable(): boolean {
    return this.state === CircuitState.CLOSED || this.state === CircuitState.HALF_OPEN;
  }

  /**
   * Force the circuit to a specific state (for testing/recovery)
   */
  forceState(state: CircuitState): void {
    logger.warn("Forcing circuit state", {
      name: this.config.name,
      from: this.state,
      to: state,
    });
    this.transitionTo(state);
  }

  /**
   * Reset the circuit breaker to initial closed state
   */
  reset(): void {
    logger.info("Resetting circuit breaker", { name: this.config.name });

    if (this.recoveryTimer !== null) {
      window.clearTimeout(this.recoveryTimer);
      this.recoveryTimer = null;
    }

    this.state = CircuitState.CLOSED;
    this.failures = 0;
    this.successes = 0;
    this.lastFailureTime = null;
    this.lastSuccessTime = null;
    this.lastStateChange = Date.now();
  }

  /**
   * Subscribe to circuit breaker events
   */
  onEvent(callback: (event: CircuitBreakerEvent) => void): () => void {
    this.listeners.add(callback);
    return () => this.listeners.delete(callback);
  }

  /**
   * Get the circuit breaker name
   */
  getName(): string {
    return this.config.name;
  }

  /**
   * Transition to a new state
   */
  private transitionTo(newState: CircuitState): void {
    if (this.state === newState) {
      return;
    }

    const oldState = this.state;
    this.state = newState;
    this.lastStateChange = Date.now();

    // Reset counters on state change
    if (newState === CircuitState.CLOSED) {
      this.failures = 0;
      this.successes = 0;
    } else if (newState === CircuitState.HALF_OPEN) {
      this.successes = 0;
    }

    logger.info("Circuit state changed", {
      name: this.config.name,
      from: oldState,
      to: newState,
    });

    this.emitEvent({ type: "state-change", from: oldState, to: newState });
  }

  /**
   * Emit an event to all listeners
   */
  private emitEvent(event: CircuitBreakerEvent): void {
    this.listeners.forEach((callback) => {
      try {
        callback(event);
      } catch (error) {
        logger.error("Error in circuit breaker event listener", error as Error);
      }
    });
  }
}

/**
 * Default configuration for daemon IPC circuit breaker
 */
export const DEFAULT_DAEMON_CIRCUIT_CONFIG: Omit<CircuitBreakerConfig, "name"> = {
  failureThreshold: 3, // Open after 3 consecutive failures
  recoveryTimeout: 10000, // Try half-open after 10 seconds
  successThreshold: 2, // Close after 2 successful probes
};

// Singleton circuit breaker for daemon IPC
let daemonCircuitBreaker: CircuitBreaker | null = null;

/**
 * Get the singleton daemon circuit breaker instance
 */
export function getDaemonCircuitBreaker(): CircuitBreaker {
  if (!daemonCircuitBreaker) {
    daemonCircuitBreaker = new CircuitBreaker({
      name: "daemon-ipc",
      ...DEFAULT_DAEMON_CIRCUIT_CONFIG,
    });
  }
  return daemonCircuitBreaker;
}

/**
 * Reset the daemon circuit breaker (for testing)
 */
export function resetDaemonCircuitBreaker(): void {
  if (daemonCircuitBreaker) {
    daemonCircuitBreaker.reset();
  }
}
