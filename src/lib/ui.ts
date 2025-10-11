import { Terminal, TerminalStatus } from './state';

export function renderHeader(displayedWorkspacePath: string, hasWorkspace: boolean): void {
  const container = document.getElementById('workspace-name');
  if (!container) return;

  if (hasWorkspace) {
    // Show repo name in header (no "Loom" title)
    const repoName = extractRepoName(displayedWorkspacePath);
    container.innerHTML = `📂 ${escapeHtml(repoName)}`;
  } else {
    // Show "Loom" title when no workspace
    container.innerHTML = 'Loom';
  }
}

function extractRepoName(path: string): string {
  if (!path) return '';
  // Get the last component of the path
  const parts = path.split('/').filter(p => p.length > 0);
  return parts[parts.length - 1] || path;
}

export function renderPrimaryTerminal(terminal: Terminal | null, hasWorkspace: boolean, displayedWorkspacePath: string): void {
  const container = document.getElementById('primary-terminal');
  if (!container) return;

  if (!terminal) {
    if (hasWorkspace) {
      // Workspace set but no agents
      container.innerHTML = `
        <div class="h-full flex items-center justify-center text-gray-400">
          <p class="text-lg">No agents. Click + to add an agent.</p>
        </div>
      `;
    } else {
      // No workspace - show selector in center
      container.innerHTML = `
        <div class="h-full flex items-center justify-center">
          <div class="flex flex-col items-center gap-3">
            <p class="text-lg text-gray-400 mb-2">Open a git repository to begin</p>
            <div class="flex items-center gap-2">
              <input
                id="workspace-path"
                type="text"
                placeholder="Select or enter workspace path..."
                value="${escapeHtml(displayedWorkspacePath)}"
                class="px-3 py-2 text-sm bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-600 rounded focus:outline-none focus:ring-2 focus:ring-blue-500 w-96"
              />
              <button
                id="browse-workspace"
                class="p-2 hover:bg-gray-100 dark:hover:bg-gray-700 rounded border border-gray-300 dark:border-gray-600"
                title="Browse for folder"
              >
                📁
              </button>
            </div>
            <div id="workspace-error" class="text-sm text-red-500 dark:text-red-400 min-h-[20px]"></div>
          </div>
        </div>
      `;
    }
    return;
  }

  container.innerHTML = `
    <div class="h-full flex flex-col bg-white dark:bg-gray-800 rounded-lg border border-gray-200 dark:border-gray-700 overflow-hidden">
      <div class="flex items-center justify-between px-4 py-2 border-b border-gray-200 dark:border-gray-700 bg-gray-50 dark:bg-gray-700">
        <div class="flex items-center gap-2">
          <div class="w-2 h-2 rounded-full ${getStatusColor(terminal.status)}"></div>
          <span class="terminal-name font-medium text-sm" data-terminal-id="${terminal.id}">${escapeHtml(terminal.name)}</span>
        </div>
      </div>
      <div class="flex-1 p-4 overflow-auto" id="terminal-content-${terminal.id}">
        <div class="font-mono text-sm text-gray-600 dark:text-gray-400">
          ${escapeHtml(terminal.name)}<br>
          Status: ${terminal.status}<br>
          (Agent terminal display will be implemented in Issue #4)
        </div>
      </div>
    </div>
  `;
}

export function renderMiniTerminals(terminals: Terminal[], hasWorkspace: boolean): void {
  const container = document.getElementById('mini-terminal-row');
  if (!container) return;

  const terminalCards = terminals.map((t, index) => createMiniTerminalHTML(t, index)).join('');

  const addButtonClasses = hasWorkspace
    ? 'bg-gray-100 dark:bg-gray-700 hover:bg-gray-200 dark:hover:bg-gray-600 cursor-pointer'
    : 'bg-gray-50 dark:bg-gray-800 cursor-not-allowed opacity-50';

  const addButtonDisabled = hasWorkspace ? '' : 'disabled';
  const addButtonTitle = hasWorkspace ? 'Add agent' : 'Select a workspace first';

  container.innerHTML = `
    <div class="h-full flex items-center gap-2 px-4 py-2 overflow-x-auto overflow-y-visible">
      ${terminalCards}
      <button
        id="add-terminal-btn"
        class="flex-shrink-0 w-32 h-32 flex items-center justify-center ${addButtonClasses} rounded-lg border-2 border-dashed border-gray-300 dark:border-gray-600 transition-colors"
        title="${addButtonTitle}"
        ${addButtonDisabled}
      >
        <span class="text-3xl text-gray-400">+</span>
      </button>
    </div>
  `;
}

function createMiniTerminalHTML(terminal: Terminal, index: number): string {
  const activeClass = terminal.isPrimary
    ? 'border-2 border-blue-500'
    : 'border border-gray-200 dark:border-gray-700';

  return `
    <div class="p-1 flex-shrink-0">
      <div
        class="terminal-card group w-40 h-32 bg-white dark:bg-gray-800 hover:bg-gray-900/5 dark:hover:bg-white/5 rounded-lg ${activeClass} cursor-grab transition-all"
        data-terminal-id="${terminal.id}"
        draggable="true"
      >
      <div class="p-2 border-b border-gray-200 dark:border-gray-700 bg-gray-50 dark:bg-gray-700 group-hover:bg-gray-100 dark:group-hover:bg-gray-600 flex items-center justify-between transition-colors rounded-t-lg">
        <div class="flex items-center gap-2 flex-1 min-w-0">
          <div class="w-2 h-2 rounded-full flex-shrink-0 ${getStatusColor(terminal.status)}"></div>
          <span class="terminal-name text-xs font-medium truncate">${escapeHtml(terminal.name)}</span>
        </div>
        <button
          class="close-terminal-btn flex-shrink-0 text-gray-400 hover:text-red-500 dark:hover:text-red-400 font-bold transition-colors"
          data-terminal-id="${terminal.id}"
          title="Close agent"
        >
          ×
        </button>
      </div>
      <div class="p-2 text-xs text-gray-500 dark:text-gray-400 flex items-center justify-between">
        <span>${terminal.status}</span>
        <span class="font-mono font-bold text-blue-600 dark:text-blue-400">#${index}</span>
      </div>
      </div>
    </div>
  `;
}

export function getStatusColor(status: TerminalStatus): string {
  const colors = {
    [TerminalStatus.Idle]: 'bg-green-500',
    [TerminalStatus.Busy]: 'bg-blue-500',
    [TerminalStatus.NeedsInput]: 'bg-yellow-500',
    [TerminalStatus.Error]: 'bg-red-500',
    [TerminalStatus.Stopped]: 'bg-gray-400'
  };
  return colors[status];
}

function escapeHtml(text: string): string {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}
