import { Logger } from "./logger";
import * as fs from "fs/promises";
import * as path from "path";
import { Command } from "@tauri-apps/plugin-shell";

const logger = Logger.forComponent("terminal-worktree-manager");

/**
 * Configuration for terminal worktree creation
 */
export interface TerminalWorktreeConfig {
  terminalId: string;
  terminalName: string;
  roleFile: string;
  workspacePath: string;
}

/**
 * Result of terminal worktree creation
 */
export interface TerminalWorktreeResult {
  terminalId: string;
  worktreePath: string;
  claudeMdPath: string;
}

/**
 * Create a git worktree for a terminal with role-specific CLAUDE.md
 *
 * This function:
 * 1. Creates .loom/worktrees/terminal-N directory
 * 2. Creates git worktree with branch worktree/terminal-N
 * 3. Reads role content from .loom/roles/<roleFile>
 * 4. Writes role content to CLAUDE.md in the worktree
 *
 * @param config - Terminal worktree configuration
 * @returns Promise resolving to worktree path and CLAUDE.md path
 */
export async function createTerminalWorktree(
  config: TerminalWorktreeConfig
): Promise<TerminalWorktreeResult> {
  const { terminalId, terminalName, roleFile, workspacePath } = config;

  logger.info("Creating terminal worktree", {
    terminalId,
    terminalName,
    roleFile,
    workspacePath,
  });

  // Construct paths
  const worktreePath = path.join(workspacePath, ".loom", "worktrees", terminalId);
  const branchName = `worktree/${terminalId}`;
  const claudeMdPath = path.join(worktreePath, "CLAUDE.md");
  const roleFilePath = path.join(workspacePath, ".loom", "roles", roleFile);

  try {
    // Ensure .loom/worktrees directory exists
    const worktreesDir = path.join(workspacePath, ".loom", "worktrees");
    await fs.mkdir(worktreesDir, { recursive: true });
    logger.info("Created worktrees directory", {
      terminalId,
      worktreesDir,
    });

    // Check if worktree already exists
    try {
      await fs.access(worktreePath);
      logger.warn("Worktree already exists, removing it first", {
        terminalId,
        worktreePath,
      });

      // Try to remove existing worktree via git
      try {
        const removeCommand = Command.create("git", [
          "worktree",
          "remove",
          worktreePath,
          "--force",
        ], { cwd: workspacePath });
        await removeCommand.execute();
      } catch (gitError) {
        // If git worktree remove fails, remove directory manually
        logger.warn("git worktree remove failed, removing directory manually", {
          terminalId,
          worktreePath,
        });
        await fs.rm(worktreePath, { recursive: true, force: true });
      }

      // Remove the branch if it exists
      try {
        const branchCommand = Command.create("git", ["branch", "-D", branchName], {
          cwd: workspacePath,
        });
        await branchCommand.execute();
      } catch (branchError) {
        // Branch might not exist, that's okay
        logger.info("Branch doesn't exist or couldn't be deleted", {
          terminalId,
          branchName,
        });
      }
    } catch (accessError) {
      // Worktree doesn't exist, that's fine
      logger.info("Worktree doesn't exist yet", {
        terminalId,
        worktreePath,
      });
    }

    // Create git worktree with dedicated branch
    logger.info("Creating git worktree", {
      terminalId,
      branchName,
      worktreePath,
    });

    const addCommand = Command.create(
      "git",
      ["worktree", "add", "-b", branchName, worktreePath, "HEAD"],
      { cwd: workspacePath }
    );
    const addResult = await addCommand.execute();

    if (addResult.code !== 0) {
      throw new Error(
        `Failed to create git worktree: ${addResult.stderr || addResult.stdout}`
      );
    }

    logger.info("Git worktree created successfully", {
      terminalId,
      worktreePath,
    });

    // Read role file content
    logger.info("Reading role file", {
      terminalId,
      roleFilePath,
    });

    let roleContent: string;
    try {
      roleContent = await fs.readFile(roleFilePath, "utf-8");
      logger.info("Role file read successfully", {
        terminalId,
        roleFilePath,
        contentLength: roleContent.length,
      });
    } catch (roleError) {
      logger.error("Failed to read role file", roleError as Error, {
        terminalId,
        roleFilePath,
      });
      throw new Error(
        `Failed to read role file ${roleFile}: ${roleError}`
      );
    }

    // Write CLAUDE.md to worktree
    logger.info("Writing CLAUDE.md to worktree", {
      terminalId,
      claudeMdPath,
    });

    await fs.writeFile(claudeMdPath, roleContent, "utf-8");

    logger.info("CLAUDE.md written successfully", {
      terminalId,
      claudeMdPath,
    });

    // Verify CLAUDE.md exists
    const stats = await fs.stat(claudeMdPath);
    logger.info("Terminal worktree creation complete", {
      terminalId,
      worktreePath,
      claudeMdSize: stats.size,
    });

    return {
      terminalId,
      worktreePath,
      claudeMdPath,
    };
  } catch (error) {
    logger.error("Failed to create terminal worktree", error as Error, {
      terminalId,
      terminalName,
      roleFile,
      workspacePath,
      worktreePath,
    });
    throw error;
  }
}

/**
 * Create multiple terminal worktrees in parallel
 *
 * @param configs - Array of terminal worktree configurations
 * @returns Promise resolving to array of terminal worktree results
 */
export async function createTerminalWorktreesInParallel(
  configs: TerminalWorktreeConfig[]
): Promise<{
  succeeded: TerminalWorktreeResult[];
  failed: Array<{ terminalId: string; error: unknown }>;
}> {
  logger.info("Creating terminal worktrees in parallel", {
    count: configs.length,
  });

  const results = await Promise.allSettled(
    configs.map((config) => createTerminalWorktree(config))
  );

  const succeeded = results
    .filter((r) => r.status === "fulfilled")
    .map((r) => r.value as TerminalWorktreeResult);

  const failed = results
    .filter((r) => r.status === "rejected")
    .map((r, index) => ({
      terminalId: configs[index].terminalId,
      error: r.reason,
    }));

  logger.info("Terminal worktree creation complete", {
    totalCount: configs.length,
    succeeded: succeeded.length,
    failed: failed.length,
  });

  return { succeeded, failed };
}

/**
 * Clean up a terminal worktree
 *
 * This removes the worktree directory and associated git branch.
 *
 * @param terminalId - Terminal ID to clean up
 * @param workspacePath - Workspace path
 */
export async function cleanupTerminalWorktree(
  terminalId: string,
  workspacePath: string
): Promise<void> {
  const worktreePath = path.join(workspacePath, ".loom", "worktrees", terminalId);
  const branchName = `worktree/${terminalId}`;

  logger.info("Cleaning up terminal worktree", {
    terminalId,
    worktreePath,
  });

  try {
    // Remove git worktree
    try {
      const removeCommand = Command.create(
        "git",
        ["worktree", "remove", worktreePath, "--force"],
        { cwd: workspacePath }
      );
      await removeCommand.execute();
      logger.info("Git worktree removed", {
        terminalId,
        worktreePath,
      });
    } catch (gitError) {
      // If git worktree remove fails, try manual removal
      logger.warn("git worktree remove failed, trying manual removal", {
        terminalId,
        worktreePath,
      });
      await fs.rm(worktreePath, { recursive: true, force: true });
    }

    // Remove the branch
    try {
      const branchCommand = Command.create("git", ["branch", "-D", branchName], {
        cwd: workspacePath,
      });
      await branchCommand.execute();
      logger.info("Git branch removed", {
        terminalId,
        branchName,
      });
    } catch (branchError) {
      // Branch might not exist, that's okay
      logger.info("Branch doesn't exist or couldn't be deleted", {
        terminalId,
        branchName,
      });
    }

    logger.info("Terminal worktree cleanup complete", {
      terminalId,
    });
  } catch (error) {
    logger.error("Failed to clean up terminal worktree", error as Error, {
      terminalId,
      worktreePath,
    });
    // Don't throw - cleanup failures are non-critical
  }
}

/**
 * Clean up all terminal worktrees
 *
 * @param workspacePath - Workspace path
 */
export async function cleanupAllTerminalWorktrees(workspacePath: string): Promise<void> {
  logger.info("Cleaning up all terminal worktrees", {
    workspacePath,
  });

  const worktreesDir = path.join(workspacePath, ".loom", "worktrees");

  try {
    // Check if worktrees directory exists
    try {
      await fs.access(worktreesDir);
    } catch {
      logger.info("No worktrees directory to clean up", {
        workspacePath,
      });
      return;
    }

    // Read all entries in worktrees directory
    const entries = await fs.readdir(worktreesDir, { withFileTypes: true });

    // Filter for terminal worktrees (terminal-N pattern)
    const terminalWorktrees = entries
      .filter((entry) => entry.isDirectory() && entry.name.startsWith("terminal-"))
      .map((entry) => entry.name);

    logger.info("Found terminal worktrees to clean up", {
      workspacePath,
      count: terminalWorktrees.length,
      worktrees: terminalWorktrees,
    });

    // Clean up each terminal worktree
    await Promise.all(
      terminalWorktrees.map((terminalId) =>
        cleanupTerminalWorktree(terminalId, workspacePath)
      )
    );

    logger.info("All terminal worktrees cleaned up", {
      workspacePath,
      count: terminalWorktrees.length,
    });
  } catch (error) {
    logger.error("Failed to clean up terminal worktrees", error as Error, {
      workspacePath,
    });
    // Don't throw - cleanup failures are non-critical
  }
}
