/**
 * Manages workspace path state and validation.
 * Handles both the validated workspace path and the displayed path (which may be invalid during user input).
 */
export class WorkspaceStateManager {
  private workspacePath: string | null = null;
  private displayedWorkspacePath: string = "";
  private listeners: Set<() => void> = new Set();

  /**
   * Sets the validated workspace path and persists it to localStorage.
   * Also updates the displayed workspace path to match.
   * Workspace path persists across HMR reloads.
   */
  setWorkspace(path: string): void {
    this.workspacePath = path;
    this.displayedWorkspacePath = path;
    // Persist workspace to localStorage to survive HMR reloads
    if (path) {
      localStorage.setItem("loom:workspace", path);
    } else {
      localStorage.removeItem("loom:workspace");
    }
    this.notify();
  }

  /**
   * Sets the displayed workspace path without validation.
   * Used to show user input in the workspace selector even if it's invalid.
   * This allows showing specific error messages while preserving user typing.
   */
  setDisplayedWorkspace(path: string): void {
    this.displayedWorkspacePath = path;
    this.notify();
  }

  /**
   * Gets the current validated workspace path.
   */
  getWorkspace(): string | null {
    return this.workspacePath;
  }

  /**
   * Check if a valid workspace is set.
   */
  hasWorkspace(): boolean {
    return this.workspacePath !== null && this.workspacePath !== "";
  }

  /**
   * Get workspace path or throw error if none exists.
   * Use this when workspace is required for an operation.
   */
  getWorkspaceOrThrow(): string {
    if (!this.workspacePath) {
      throw new Error("No workspace selected");
    }
    return this.workspacePath;
  }

  /**
   * Gets the displayed workspace path (which may be invalid).
   * This may differ from getWorkspace() when user has entered an invalid path.
   */
  getDisplayedWorkspace(): string {
    return this.displayedWorkspacePath;
  }

  /**
   * Restore workspace from localStorage (for HMR survival).
   * Workspace path is automatically persisted to survive hot module replacement during development.
   */
  restoreWorkspaceFromLocalStorage(): string | null {
    const stored = localStorage.getItem("loom:workspace");
    if (stored) {
      this.workspacePath = stored;
      this.displayedWorkspacePath = stored;
      this.notify();
      return stored;
    }
    return null;
  }

  /**
   * Clears the workspace state.
   */
  clearWorkspace(): void {
    this.workspacePath = null;
    this.displayedWorkspacePath = "";
    this.notify();
  }

  /**
   * Registers a callback to be notified of state changes.
   */
  onChange(callback: () => void): () => void {
    this.listeners.add(callback);
    return () => this.listeners.delete(callback);
  }

  private notify(): void {
    this.listeners.forEach((cb) => cb());
  }
}
