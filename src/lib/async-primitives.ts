/**
 * Async coordination primitives for preventing concurrent operations.
 *
 * This module provides utilities to manage async operations and prevent
 * race conditions in the autonomous agent system.
 */

import { Logger } from "./logger";

const logger = Logger.forComponent("async-primitives");

/**
 * Provides mutual exclusion for async operations.
 * Only one operation can hold the lock at a time.
 *
 * @example
 * ```ts
 * const lock = new AsyncLock();
 *
 * async function criticalSection() {
 *   const release = await lock.acquire();
 *   try {
 *     // Only one operation can be here at a time
 *     await doSomething();
 *   } finally {
 *     release();
 *   }
 * }
 * ```
 */
export class AsyncLock {
  private locked = false;
  private waitQueue: Array<() => void> = [];

  /**
   * Acquire the lock. If the lock is held, waits until it becomes available.
   *
   * @returns A release function that must be called to release the lock
   */
  async acquire(): Promise<() => void> {
    while (this.locked) {
      await new Promise<void>((resolve) => this.waitQueue.push(resolve));
    }
    this.locked = true;

    // Return release function
    return () => {
      this.locked = false;
      const next = this.waitQueue.shift();
      if (next) next();
    };
  }

  /**
   * Check if the lock is currently held.
   */
  isLocked(): boolean {
    return this.locked;
  }

  /**
   * Get the number of operations waiting for the lock.
   */
  get queueLength(): number {
    return this.waitQueue.length;
  }
}

/**
 * Deduplicates in-flight async operations by key.
 * If an operation is already running for a key, returns undefined instead
 * of starting a duplicate.
 *
 * This implements the "skip duplicates" strategy for concurrent operations,
 * preventing resource waste and potential race conditions.
 *
 * @example
 * ```ts
 * const dedup = new PromiseDeduplicator<void>();
 *
 * async function sendPrompt(terminalId: string) {
 *   const result = await dedup.execute(terminalId, async () => {
 *     await actualSendPrompt(terminalId);
 *   });
 *
 *   if (result === undefined) {
 *     console.log('Skipped duplicate prompt');
 *   }
 * }
 * ```
 */
export class PromiseDeduplicator<T> {
  private inFlight = new Map<string, Promise<T>>();

  /**
   * Execute an operation, skipping if one is already in flight for the key.
   *
   * @param key - Unique identifier for the operation (e.g., terminal ID)
   * @param operation - The async operation to execute
   * @returns The operation result, or undefined if skipped due to duplicate
   */
  async execute(key: string, operation: () => Promise<T>): Promise<T | undefined> {
    // Skip if already in flight
    if (this.inFlight.has(key)) {
      logger.info("Skipping duplicate operation", { key });
      return undefined;
    }

    const promise = operation();
    this.inFlight.set(key, promise);

    try {
      const result = await promise;
      return result;
    } finally {
      this.inFlight.delete(key);
    }
  }

  /**
   * Check if an operation is currently in flight for the given key.
   */
  isInFlight(key: string): boolean {
    return this.inFlight.has(key);
  }

  /**
   * Get all keys with in-flight operations.
   */
  getInFlightKeys(): string[] {
    return Array.from(this.inFlight.keys());
  }

  /**
   * Get the count of in-flight operations.
   */
  get inFlightCount(): number {
    return this.inFlight.size;
  }
}
