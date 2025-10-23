import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";
import type { AppState } from "./state";

const logger = Logger.forComponent("parallel-terminal-creator");

/**
 * Configuration for a terminal to be created
 */
export interface TerminalConfig {
  id: string;
  name: string;
  role: string;
  workingDir: string;
  instanceNumber: number;
}

/**
 * Result of parallel terminal creation
 */
export interface ParallelCreationResult {
  succeeded: Array<{ configId: string; terminalId: string }>;
  failed: Array<{ configId: string; error: unknown }>;
}

/**
 * Create terminals in parallel with error aggregation
 *
 * This function creates all terminals concurrently using Promise.allSettled,
 * which allows failures to be isolated. If terminal 3 fails, terminals 4-7
 * still proceed normally.
 *
 * @param configs - Array of terminal configurations to create
 * @param workspacePath - The workspace directory path
 * @returns Promise resolving to succeeded and failed terminal creation results
 */
export async function createTerminalsInParallel(
  configs: TerminalConfig[],
  workspacePath: string
): Promise<ParallelCreationResult> {
  logger.info("Starting parallel terminal creation", {
    workspacePath,
    terminalCount: configs.length,
  });

  const startTime = Date.now();

  // Create all terminals concurrently
  const results = await Promise.allSettled(
    configs.map(async (config) => {
      try {
        logger.info("Creating terminal", {
          workspacePath,
          configId: config.id,
          terminalName: config.name,
          instanceNumber: config.instanceNumber,
          role: config.role,
        });

        const terminalId = await invoke<string>("create_terminal", {
          configId: config.id,
          name: config.name,
          workingDir: config.workingDir,
          role: config.role,
          instanceNumber: config.instanceNumber,
        });

        logger.info("Terminal created successfully", {
          workspacePath,
          configId: config.id,
          terminalId,
          terminalName: config.name,
        });

        return { configId: config.id, terminalId };
      } catch (error) {
        logger.error("Terminal creation failed", error as Error, {
          workspacePath,
          configId: config.id,
          terminalName: config.name,
        });
        throw { configId: config.id, error };
      }
    })
  );

  // Separate succeeded and failed results
  const succeeded = results
    .filter((r) => r.status === "fulfilled")
    .map((r) => r.value as { configId: string; terminalId: string });

  const failed = results
    .filter((r) => r.status === "rejected")
    .map((r) => r.reason as { configId: string; error: unknown });

  const elapsedMs = Date.now() - startTime;

  logger.info("Parallel terminal creation complete", {
    workspacePath,
    totalTerminals: configs.length,
    succeeded: succeeded.length,
    failed: failed.length,
    elapsedMs,
  });

  return { succeeded, failed };
}

/**
 * Retry failed terminal creations with exponential backoff
 *
 * This function attempts to recreate terminals that failed during initial
 * parallel creation. It uses exponential backoff (1s, 2s, 4s) to handle
 * transient failures like race conditions or resource contention.
 *
 * @param failedConfigs - Array of terminal configurations that failed
 * @param configs - Original array of all terminal configurations (for lookup)
 * @param workspacePath - The workspace directory path
 * @param maxRetries - Maximum number of retry attempts per terminal (default: 2)
 * @returns Promise resolving to final succeeded and failed results after retries
 */
export async function retryFailedTerminals(
  failedConfigs: Array<{ configId: string; error: unknown }>,
  configs: TerminalConfig[],
  workspacePath: string,
  maxRetries = 2
): Promise<ParallelCreationResult> {
  logger.info("Starting retry for failed terminals", {
    workspacePath,
    failedCount: failedConfigs.length,
    maxRetries,
  });

  const succeeded: Array<{ configId: string; terminalId: string }> = [];
  const failed: Array<{ configId: string; error: unknown }> = [];

  for (const failedConfig of failedConfigs) {
    const config = configs.find((c) => c.id === failedConfig.configId);
    if (!config) {
      logger.error(
        "Failed config not found in original configs",
        new Error("Config lookup failed"),
        {
          workspacePath,
          configId: failedConfig.configId,
        }
      );
      failed.push(failedConfig);
      continue;
    }

    let retrySucceeded = false;

    // Retry with exponential backoff: 1s, 2s, 4s
    for (let attempt = 1; attempt <= maxRetries; attempt++) {
      const backoffMs = 1000 * 2 ** (attempt - 1); // 1s, 2s, 4s

      logger.info("Retrying terminal creation", {
        workspacePath,
        configId: config.id,
        terminalName: config.name,
        attempt,
        maxRetries,
        backoffMs,
      });

      await new Promise((resolve) => setTimeout(resolve, backoffMs));

      try {
        const terminalId = await invoke<string>("create_terminal", {
          configId: config.id,
          name: config.name,
          workingDir: config.workingDir,
          role: config.role,
          instanceNumber: config.instanceNumber,
        });

        logger.info("Terminal creation retry succeeded", {
          workspacePath,
          configId: config.id,
          terminalId,
          attempt,
        });

        succeeded.push({ configId: config.id, terminalId });
        retrySucceeded = true;
        break;
      } catch (error) {
        logger.error("Terminal creation retry failed", error as Error, {
          workspacePath,
          configId: config.id,
          attempt,
          maxRetries,
        });

        if (attempt === maxRetries) {
          logger.error("All retry attempts exhausted", error as Error, {
            workspacePath,
            configId: config.id,
            terminalName: config.name,
          });
        }
      }
    }

    if (!retrySucceeded) {
      failed.push(failedConfig);
    }
  }

  logger.info("Retry complete", {
    workspacePath,
    originalFailures: failedConfigs.length,
    nowSucceeded: succeeded.length,
    stillFailed: failed.length,
  });

  return { succeeded, failed };
}

/**
 * Create terminals in parallel with automatic retry for failures
 *
 * This is a convenience function that combines parallel creation and retry logic.
 * It creates all terminals concurrently, then retries any failures with exponential backoff.
 *
 * @param configs - Array of terminal configurations to create
 * @param workspacePath - The workspace directory path
 * @param state - App state instance (for getting next terminal number)
 * @returns Promise resolving to arrays of succeeded and failed terminal IDs
 */
export async function createTerminalsWithRetry(
  configs: TerminalConfig[],
  workspacePath: string,
  state: AppState
): Promise<{
  succeeded: Array<{ configId: string; terminalId: string }>;
  failed: Array<{ configId: string; error: unknown }>;
}> {
  logger.info("Starting terminal creation with retry", {
    workspacePath,
    terminalCount: configs.length,
  });

  // Assign instance numbers to each config
  const configsWithNumbers = configs.map((config) => ({
    ...config,
    instanceNumber: state.getNextTerminalNumber(),
  }));

  // First attempt: create all terminals in parallel
  const firstResult = await createTerminalsInParallel(configsWithNumbers, workspacePath);

  // If all succeeded, we're done
  if (firstResult.failed.length === 0) {
    logger.info("All terminals created successfully on first attempt", {
      workspacePath,
      terminalCount: firstResult.succeeded.length,
    });
    return firstResult;
  }

  // Retry failures
  logger.info("Retrying failed terminals", {
    workspacePath,
    failedCount: firstResult.failed.length,
  });

  const retryResult = await retryFailedTerminals(
    firstResult.failed,
    configsWithNumbers,
    workspacePath
  );

  // Combine results
  const allSucceeded = [...firstResult.succeeded, ...retryResult.succeeded];
  const allFailed = retryResult.failed;

  logger.info("Terminal creation with retry complete", {
    workspacePath,
    totalTerminals: configs.length,
    succeeded: allSucceeded.length,
    failed: allFailed.length,
    successRate: `${Math.round((allSucceeded.length / configs.length) * 100)}%`,
  });

  return {
    succeeded: allSucceeded,
    failed: allFailed,
  };
}
