import { invoke } from "@tauri-apps/api/core";
import { showToast } from "./toast";

/**
 * git-menu.ts - Git menu UI and branch management
 *
 * Provides UI for managing git branches and worktrees using the correct
 * one-worktree-per-branch pattern instead of one-worktree-per-terminal.
 */

export interface BranchInfo {
  name: string;
  is_current: boolean;
  has_worktree: boolean;
  worktree_path: string | null;
  last_commit: string;
  last_commit_message: string;
}

/**
 * Open the branch creation dialog
 *
 * Allows user to create a new branch with a worktree at .loom/worktrees/<branch>
 */
export async function openCreateBranchDialog(workspacePath: string): Promise<void> {
  // Get list of branches to populate base branch dropdown
  const branches = await listBranches(workspacePath);

  const modal = document.createElement("div");
  modal.className = "fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50";
  modal.innerHTML = `
    <div class="bg-white dark:bg-gray-800 rounded-lg shadow-xl p-6 w-96">
      <h2 class="text-xl font-bold mb-4 text-gray-900 dark:text-gray-100">Create New Branch</h2>

      <div class="space-y-4">
        <div>
          <label class="block text-sm font-medium mb-1 text-gray-700 dark:text-gray-300">
            Branch Name
          </label>
          <input
            type="text"
            id="branch-name-input"
            class="w-full px-3 py-2 border border-gray-300 dark:border-gray-600 rounded-md bg-white dark:bg-gray-700 text-gray-900 dark:text-gray-100 focus:outline-none focus:ring-2 focus:ring-blue-500"
            placeholder="e.g., feature/my-feature"
          />
          <p class="text-xs text-gray-500 dark:text-gray-400 mt-1">
            Use slashes for organization: feature/, bugfix/, hotfix/
          </p>
        </div>

        <div>
          <label class="block text-sm font-medium mb-1 text-gray-700 dark:text-gray-300">
            Base Branch
          </label>
          <select
            id="base-branch-select"
            class="w-full px-3 py-2 border border-gray-300 dark:border-gray-600 rounded-md bg-white dark:bg-gray-700 text-gray-900 dark:text-gray-100 focus:outline-none focus:ring-2 focus:ring-blue-500"
          >
            ${branches
              .map(
                (b) => `
              <option value="${escapeHtml(b.name)}" ${b.is_current ? "selected" : ""}>
                ${escapeHtml(b.name)}${b.is_current ? " (current)" : ""}
              </option>
            `
              )
              .join("")}
          </select>
        </div>

        <div class="bg-blue-50 dark:bg-blue-900/20 border border-blue-200 dark:border-blue-800 rounded-md p-3">
          <p class="text-sm text-blue-800 dark:text-blue-200">
            <strong>Note:</strong> This will create a worktree at <code class="bg-blue-100 dark:bg-blue-900/40 px-1 rounded">.loom/worktrees/&lt;branch&gt;</code>
          </p>
        </div>
      </div>

      <div class="flex justify-end space-x-3 mt-6">
        <button
          id="cancel-create-branch"
          class="px-4 py-2 text-gray-700 dark:text-gray-300 hover:bg-gray-100 dark:hover:bg-gray-700 rounded-md transition-colors"
        >
          Cancel
        </button>
        <button
          id="confirm-create-branch"
          class="px-4 py-2 bg-blue-600 text-white hover:bg-blue-700 rounded-md transition-colors"
        >
          Create Branch
        </button>
      </div>

      <div id="branch-error" class="mt-4 hidden">
        <div class="bg-red-50 dark:bg-red-900/20 border border-red-200 dark:border-red-800 rounded-md p-3">
          <p class="text-sm text-red-800 dark:text-red-200" id="branch-error-message"></p>
        </div>
      </div>
    </div>
  `;

  document.body.appendChild(modal);

  // Focus the branch name input
  const input = document.getElementById("branch-name-input") as HTMLInputElement;
  input?.focus();

  // Handle cancel
  const cancelBtn = document.getElementById("cancel-create-branch");
  cancelBtn?.addEventListener("click", () => {
    document.body.removeChild(modal);
  });

  // Handle create
  const confirmBtn = document.getElementById("confirm-create-branch") as HTMLButtonElement;
  confirmBtn?.addEventListener("click", async () => {
    const branchName = (
      document.getElementById("branch-name-input") as HTMLInputElement
    )?.value.trim();
    const baseBranch = (document.getElementById("base-branch-select") as HTMLSelectElement)?.value;

    if (!branchName) {
      showBranchError("Branch name is required");
      return;
    }

    // Validate branch name (no spaces, no special chars except / and -)
    if (!/^[a-zA-Z0-9/_-]+$/.test(branchName)) {
      showBranchError("Branch name can only contain letters, numbers, /, -, and _");
      return;
    }

    try {
      // Disable button during creation
      confirmBtn.disabled = true;
      confirmBtn.textContent = "Creating...";

      const worktreePath = await createBranch(workspacePath, branchName, baseBranch);

      // Success! Close modal
      document.body.removeChild(modal);

      // Emit event so app can update UI
      window.dispatchEvent(
        new CustomEvent("branch-created", {
          detail: { branchName, worktreePath, baseBranch },
        })
      );
    } catch (error) {
      showBranchError(String(error));
      confirmBtn.disabled = false;
      confirmBtn.textContent = "Create Branch";
    }
  });

  // Handle Enter key
  input?.addEventListener("keypress", (e) => {
    if (e.key === "Enter") {
      confirmBtn?.click();
    }
  });

  // Handle Escape key
  modal.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      cancelBtn?.click();
    }
  });

  function showBranchError(message: string): void {
    const errorDiv = document.getElementById("branch-error");
    const errorMessage = document.getElementById("branch-error-message");
    if (errorDiv && errorMessage) {
      errorDiv.classList.remove("hidden");
      errorMessage.textContent = message;
    }
  }
}

/**
 * Open the branch list/switcher dialog
 *
 * Shows all branches with worktree status and allows switching
 */
export async function openBranchListDialog(workspacePath: string): Promise<void> {
  const branches = await listBranches(workspacePath);

  const modal = document.createElement("div");
  modal.className = "fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50";
  modal.innerHTML = `
    <div class="bg-white dark:bg-gray-800 rounded-lg shadow-xl p-6 w-[600px] max-h-[80vh] flex flex-col">
      <div class="flex justify-between items-center mb-4">
        <h2 class="text-xl font-bold text-gray-900 dark:text-gray-100">Branches & Worktrees</h2>
        <button
          id="close-branch-list"
          class="text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"
        >
          ✕
        </button>
      </div>

      <div class="flex-1 overflow-y-auto">
        <div class="space-y-2">
          ${branches
            .map(
              (branch) => `
            <div class="border border-gray-200 dark:border-gray-700 rounded-md p-3 ${branch.is_current ? "bg-blue-50 dark:bg-blue-900/20 border-blue-200 dark:border-blue-800" : "hover:bg-gray-50 dark:hover:bg-gray-700/50"}">
              <div class="flex items-start justify-between">
                <div class="flex-1">
                  <div class="flex items-center space-x-2">
                    <span class="font-mono text-sm font-medium ${branch.is_current ? "text-blue-700 dark:text-blue-300" : "text-gray-900 dark:text-gray-100"}">
                      ${escapeHtml(branch.name)}
                    </span>
                    ${branch.is_current ? '<span class="text-xs bg-blue-600 text-white px-2 py-0.5 rounded">CURRENT</span>' : ""}
                    ${branch.has_worktree ? '<span class="text-xs bg-green-600 text-white px-2 py-0.5 rounded">WORKTREE</span>' : ""}
                  </div>
                  <div class="text-xs text-gray-600 dark:text-gray-400 mt-1">
                    ${escapeHtml(branch.last_commit_message || "No commits yet")}
                  </div>
                  ${
                    branch.worktree_path
                      ? `<div class="text-xs text-gray-500 dark:text-gray-400 mt-1 font-mono">
                      📁 ${escapeHtml(branch.worktree_path)}
                    </div>`
                      : ""
                  }
                </div>
                <div class="flex space-x-2 ml-4">
                  ${
                    !branch.is_current
                      ? `<button
                        class="text-xs px-2 py-1 bg-blue-600 text-white hover:bg-blue-700 rounded transition-colors"
                        onclick="window.dispatchEvent(new CustomEvent('switch-branch', { detail: { branchName: '${escapeHtml(branch.name)}' } }))"
                      >
                        Switch
                      </button>`
                      : ""
                  }
                  ${
                    branch.has_worktree && !branch.is_current
                      ? `<button
                        class="text-xs px-2 py-1 bg-red-600 text-white hover:bg-red-700 rounded transition-colors"
                        onclick="window.dispatchEvent(new CustomEvent('delete-branch', { detail: { branchName: '${escapeHtml(branch.name)}' } }))"
                      >
                        Delete
                      </button>`
                      : ""
                  }
                </div>
              </div>
            </div>
          `
            )
            .join("")}
        </div>
      </div>

      <div class="mt-4 pt-4 border-t border-gray-200 dark:border-gray-700">
        <button
          id="create-new-branch-from-list"
          class="w-full px-4 py-2 bg-green-600 text-white hover:bg-green-700 rounded-md transition-colors"
        >
          + Create New Branch
        </button>
      </div>
    </div>
  `;

  document.body.appendChild(modal);

  // Handle close
  const closeBtn = document.getElementById("close-branch-list");
  closeBtn?.addEventListener("click", () => {
    document.body.removeChild(modal);
  });

  // Handle create new branch
  const createBtn = document.getElementById("create-new-branch-from-list");
  createBtn?.addEventListener("click", () => {
    document.body.removeChild(modal);
    openCreateBranchDialog(workspacePath);
  });

  // Handle Escape key
  modal.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      closeBtn?.click();
    }
  });

  // Listen for switch-branch event
  window.addEventListener("switch-branch", async (e: Event) => {
    const customEvent = e as CustomEvent;
    const { branchName } = customEvent.detail;

    try {
      // Check if worktree exists for this branch
      const worktreePath = await getWorktreeForBranch(workspacePath, branchName);

      if (!worktreePath) {
        // Create worktree for this branch
        await createBranch(workspacePath, branchName, branchName);
      }

      // Close modal
      document.body.removeChild(modal);

      // Emit event so terminals can switch
      window.dispatchEvent(
        new CustomEvent("branch-switch-requested", {
          detail: { branchName, worktreePath },
        })
      );
    } catch (error) {
      showToast(`Failed to switch branch: ${error}`, "error");
    }
  });

  // Listen for delete-branch event
  window.addEventListener("delete-branch", async (e: Event) => {
    const customEvent = e as CustomEvent;
    const { branchName } = customEvent.detail;

    if (
      !confirm(
        `Are you sure you want to delete branch "${branchName}" and its worktree?\n\nThis action cannot be undone.`
      )
    ) {
      return;
    }

    try {
      await deleteBranch(workspacePath, branchName, false);

      // Refresh the list
      document.body.removeChild(modal);
      openBranchListDialog(workspacePath);
    } catch (error) {
      showToast(`Failed to delete branch: ${error}`, "error");
    }
  });
}

/**
 * Create a new branch with worktree
 */
async function createBranch(
  workspacePath: string,
  branchName: string,
  baseBranch: string
): Promise<string> {
  return await invoke<string>("create_branch_worktree", {
    workspace: workspacePath,
    branchName,
    baseBranch,
  });
}

/**
 * List all branches with worktree status
 */
async function listBranches(workspacePath: string): Promise<BranchInfo[]> {
  return await invoke<BranchInfo[]>("list_branch_worktrees", {
    workspace: workspacePath,
  });
}

/**
 * Delete a branch and its worktree
 */
async function deleteBranch(
  workspacePath: string,
  branchName: string,
  force: boolean
): Promise<void> {
  await invoke<void>("delete_branch_worktree", {
    workspace: workspacePath,
    branchName,
    force,
  });
}

/**
 * Get the worktree path for a branch
 */
async function getWorktreeForBranch(
  workspacePath: string,
  branchName: string
): Promise<string | null> {
  return await invoke<string | null>("get_worktree_for_branch", {
    workspace: workspacePath,
    branchName,
  });
}

/**
 * Escape HTML to prevent XSS
 */
function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}
