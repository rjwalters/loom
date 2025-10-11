import './style.css';
import { initTheme, toggleTheme } from './lib/theme';
import { AppState, TerminalStatus } from './lib/state';
import { renderHeader, renderPrimaryTerminal, renderMiniTerminals } from './lib/ui';
import { open } from '@tauri-apps/api/dialog';
import { invoke } from '@tauri-apps/api/tauri';
import { homeDir } from '@tauri-apps/api/path';
import { loadConfig, saveConfig, setConfigWorkspace } from './lib/config';

// Initialize theme
initTheme();

// Initialize state (no agents until workspace is selected)
const state = new AppState();

// Render function
function render() {
  renderHeader(state.getDisplayedWorkspace());
  const hasWorkspace = state.getWorkspace() !== null && state.getWorkspace() !== '';
  renderPrimaryTerminal(state.getPrimary(), hasWorkspace);
  renderMiniTerminals(state.getTerminals());
}

// Initial render
render();

// Re-render on state changes
state.onChange(render);

// Drag and drop state
let draggedTerminalId: string | null = null;
let dropTargetId: string | null = null;
let dropInsertBefore: boolean = false;
let isDragging: boolean = false;

// Save current state to config
async function saveCurrentConfig() {
  const workspace = state.getWorkspace();
  if (!workspace) {
    console.log('⚙️  No workspace, skipping config save');
    return;
  }

  const config = {
    nextAgentNumber: state.getCurrentAgentNumber(),
    agents: state.getTerminals()
  };

  await saveConfig(config);
  console.log('⚙️  Saved config with', config.agents.length, 'agents');
}

// Expand tilde (~) to home directory
async function expandTildePath(path: string): Promise<string> {
  if (path.startsWith('~')) {
    try {
      const home = await homeDir();
      return path.replace(/^~/, home);
    } catch (error) {
      console.error('Failed to get home directory:', error);
      return path;
    }
  }
  return path;
}

// Workspace error UI helpers
function showWorkspaceError(message: string) {
  const input = document.getElementById('workspace-path') as HTMLInputElement;
  const errorDiv = document.getElementById('workspace-error');

  if (input) {
    input.classList.remove('border-gray-300', 'dark:border-gray-600');
    input.classList.add('border-red-500', 'dark:border-red-500');
  }

  if (errorDiv) {
    errorDiv.textContent = message;
    errorDiv.classList.remove('hidden');
  }
}

function clearWorkspaceError() {
  const input = document.getElementById('workspace-path') as HTMLInputElement;
  const errorDiv = document.getElementById('workspace-error');

  if (input) {
    input.classList.remove('border-red-500', 'dark:border-red-500');
    input.classList.add('border-gray-300', 'dark:border-gray-600');
  }

  if (errorDiv) {
    errorDiv.textContent = '';
    errorDiv.classList.add('hidden');
  }
}

// Validate workspace path
async function validateWorkspacePath(path: string): Promise<boolean> {
  console.log('🔍 validateWorkspacePath called with:', path);

  if (!path || path.trim() === '') {
    console.log('🔍 Path is empty');
    clearWorkspaceError();
    return false;
  }

  try {
    console.log('🔍 Calling Rust command to validate...');
    const isValid = await invoke<boolean>('validate_git_repo', { path });
    console.log('🔍 Rust command returned:', isValid);
    console.log('✅ Validation passed');
    clearWorkspaceError();
    return true;
  } catch (error) {
    console.error('❌ Validation failed:', error);
    // Extract error message from Tauri error object
    const errorMessage = typeof error === 'string' ? error : (error as any)?.message || 'Invalid workspace path';
    showWorkspaceError(errorMessage);
    return false;
  }
}

// Browse for workspace folder
async function browseWorkspace() {
  console.log('📂 browseWorkspace called');
  try {
    console.log('📂 Opening dialog...');
    const selected = await open({
      directory: true,
      multiple: false,
      title: 'Select workspace folder'
    });

    console.log('📂 Dialog closed. Selected:', selected);

    if (selected && typeof selected === 'string') {
      console.log('📂 Setting displayed workspace path:', selected);
      // Update the displayed workspace path first (before validation)
      state.setDisplayedWorkspace(selected);

      // Then validate the path
      console.log('📂 Validating path...');
      await handleWorkspacePathInput(selected);
    } else {
      console.log('📂 No folder selected or canceled');
    }
  } catch (error) {
    console.error('❌ Error selecting workspace:', error);
    alert('Failed to select workspace. Please try again.');
  }
}

// Initialize Loom in workspace
async function initializeLoomWorkspace(workspacePath: string): Promise<boolean> {
  try {
    console.log('🔧 Initializing Loom workspace...');

    // In dev mode, use relative path from cwd (project root)
    // TODO: For production, bundle defaults as a resource and use resourceDir()
    const defaultsPath = 'defaults';
    console.log('🔧 Using defaults path:', defaultsPath);

    await invoke('initialize_loom_workspace', {
      path: workspacePath,
      defaultsPath: defaultsPath
    });

    console.log('✅ Workspace initialized successfully');
    return true;
  } catch (error) {
    console.error('❌ Failed to initialize workspace:', error);
    alert(`Failed to initialize workspace: ${error}`);
    return false;
  }
}

// Handle manual workspace path entry
async function handleWorkspacePathInput(path: string) {
  console.log('⌨️  handleWorkspacePathInput called with:', path);

  // Expand tilde if present
  const expandedPath = await expandTildePath(path);
  console.log('⌨️  Expanded path:', expandedPath);

  // Update displayed workspace with expanded path
  if (expandedPath !== path) {
    state.setDisplayedWorkspace(expandedPath);
  }

  const isValid = await validateWorkspacePath(expandedPath);
  if (!isValid) {
    console.log('⌨️  Path is invalid, keeping in input but not setting in state');
    state.setWorkspace('');
    return;
  }

  console.log('⌨️  Path is valid git repository');

  // Check if Loom is initialized in this workspace
  try {
    const isInitialized = await invoke<boolean>('check_loom_initialized', { path: expandedPath });
    console.log('⌨️  Workspace initialized:', isInitialized);

    if (!isInitialized) {
      // Ask user to confirm initialization
      const confirmed = confirm(
        `Initialize Loom in this workspace?\n\n` +
        `This will:\n` +
        `• Create .loom/ directory with default configuration\n` +
        `• Add .loom/ to .gitignore\n` +
        `• Set up 3 default agents\n\n` +
        `Continue?`
      );

      if (!confirmed) {
        console.log('⌨️  User canceled initialization');
        state.setWorkspace('');
        return;
      }

      // Initialize workspace
      const initialized = await initializeLoomWorkspace(expandedPath);
      if (!initialized) {
        state.setWorkspace('');
        return;
      }
    }

    // Now load config from workspace
    console.log('⌨️  Setting workspace');
    state.setWorkspace(expandedPath);

    setConfigWorkspace(expandedPath);
    const config = await loadConfig();
    state.setNextAgentNumber(config.nextAgentNumber);
    console.log('⌨️  Loaded config:', config);

    // Load agents from config
    if (config.agents && config.agents.length > 0) {
      state.loadAgents(config.agents);
      console.log('⌨️  Loaded', config.agents.length, 'agents from config');
    }
  } catch (error) {
    console.error('❌ Error handling workspace:', error);
    alert(`Error: ${error}`);
    state.setWorkspace('');
  }
}

// Helper function to start renaming a terminal
function startRename(terminalId: string, nameElement: HTMLElement) {
  const terminal = state.getTerminals().find(t => t.id === terminalId);
  if (!terminal) return;

  const currentName = terminal.name;
  const input = document.createElement('input');
  input.type = 'text';
  input.value = currentName;

  // Match the font size of the original element
  const fontSize = nameElement.classList.contains('text-sm') ? 'text-sm' : 'text-xs';
  input.className = `px-1 bg-white dark:bg-gray-900 border border-blue-500 rounded ${fontSize} font-medium w-full`;

  // Replace the name element with input
  const parent = nameElement.parentElement;
  if (!parent) return;

  parent.replaceChild(input, nameElement);
  input.focus();
  input.select();

  const commit = () => {
    const newName = input.value.trim();
    if (newName && newName !== currentName) {
      state.renameTerminal(terminalId, newName);
      saveCurrentConfig();
    } else {
      // Just re-render to restore original state
      render();
    }
  };

  const cancel = () => {
    render();
  };

  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      commit();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      cancel();
    }
  });

  input.addEventListener('blur', () => {
    commit();
  });
}

// Set up event listeners (only once, since parent elements are static)
function setupEventListeners() {
  // Theme toggle
  document.getElementById('theme-toggle')?.addEventListener('click', () => {
    toggleTheme();
  });

  // Workspace path input - validate on Enter or blur
  const workspaceInput = document.getElementById('workspace-path') as HTMLInputElement;
  if (workspaceInput) {
    workspaceInput.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        handleWorkspacePathInput(workspaceInput.value);
        workspaceInput.blur();
      }
    });

    workspaceInput.addEventListener('blur', () => {
      if (workspaceInput.value !== state.getWorkspace()) {
        handleWorkspacePathInput(workspaceInput.value);
      }
    });
  }

  // Browse workspace button
  document.getElementById('browse-workspace')?.addEventListener('click', () => {
    console.log('🖱️  Browse button clicked');
    browseWorkspace();
  });

  // Primary terminal - double-click to rename
  const primaryTerminal = document.getElementById('primary-terminal');
  if (primaryTerminal) {
    primaryTerminal.addEventListener('dblclick', (e) => {
      const target = e.target as HTMLElement;

      if (target.classList.contains('terminal-name')) {
        e.stopPropagation();
        const id = target.getAttribute('data-terminal-id');
        if (id) {
          startRename(id, target);
        }
      }
    });
  }

  // Mini terminal row - event delegation for dynamic children
  const miniRow = document.getElementById('mini-terminal-row');
  if (miniRow) {
    miniRow.addEventListener('click', (e) => {
      const target = e.target as HTMLElement;

      // Handle close button clicks
      if (target.classList.contains('close-terminal-btn')) {
        e.stopPropagation();
        const id = target.getAttribute('data-terminal-id');

        if (id) {
          if (state.getTerminals().length <= 1) {
            alert('Cannot close the last agent');
            return;
          }

          if (confirm('Close this agent?')) {
            state.removeTerminal(id);
            saveCurrentConfig();
          }
        }
        return;
      }

      // Handle add terminal button
      if (target.id === 'add-terminal-btn' || target.closest('#add-terminal-btn')) {
        const agentNumber = state.getNextAgentNumber();
        state.addTerminal({
          id: String(Date.now()),
          name: `Agent ${agentNumber}`,
          status: TerminalStatus.Idle,
          isPrimary: false
        });

        // Save updated state to config
        saveCurrentConfig();
        return;
      }

      // Handle terminal card clicks (switch primary)
      const card = target.closest('[data-terminal-id]');
      if (card) {
        const id = card.getAttribute('data-terminal-id');
        if (id) {
          state.setPrimary(id);
        }
      }
    });

    // Handle mousedown to show immediate visual feedback
    miniRow.addEventListener('mousedown', (e) => {
      const target = e.target as HTMLElement;

      // Don't handle if clicking close button
      if (target.classList.contains('close-terminal-btn')) {
        return;
      }

      const card = target.closest('.terminal-card');
      if (card) {
        // Remove selection from all cards and restore default border
        document.querySelectorAll('.terminal-card').forEach(c => {
          c.classList.remove('border-2', 'border-blue-500');
          c.classList.add('border', 'border-gray-200', 'dark:border-gray-700');
        });

        // Add selection to clicked card immediately
        card.classList.remove('border', 'border-gray-200', 'dark:border-gray-700');
        card.classList.add('border-2', 'border-blue-500');
      }
    });

    // Handle double-click to rename terminals
    miniRow.addEventListener('dblclick', (e) => {
      const target = e.target as HTMLElement;

      // Check if double-clicking on the terminal name in mini cards
      if (target.classList.contains('terminal-name')) {
        e.stopPropagation();
        const card = target.closest('[data-terminal-id]');
        const id = card?.getAttribute('data-terminal-id');
        if (id) {
          startRename(id, target);
        }
      }
    });

    // HTML5 drag events for visual feedback
    miniRow.addEventListener('dragstart', (e) => {
      const target = e.target as HTMLElement;
      const card = target.closest('.terminal-card') as HTMLElement;

      if (card) {
        isDragging = true;
        draggedTerminalId = card.getAttribute('data-terminal-id');
        card.classList.add('dragging');

        if (e.dataTransfer) {
          e.dataTransfer.effectAllowed = 'move';
          e.dataTransfer.setData('text/html', card.innerHTML);
        }
      }
    });

    miniRow.addEventListener('dragend', (e) => {
      // Perform reorder if valid
      if (draggedTerminalId && dropTargetId && dropTargetId !== draggedTerminalId) {
        state.reorderTerminal(draggedTerminalId, dropTargetId, dropInsertBefore);
        saveCurrentConfig();
      }

      // Select the terminal that was dragged
      if (draggedTerminalId) {
        state.setPrimary(draggedTerminalId);
      }

      // Cleanup
      const target = e.target as HTMLElement;
      const card = target.closest('.terminal-card');
      if (card) {
        card.classList.remove('dragging');
      }

      document.querySelectorAll('.drop-indicator').forEach(el => el.remove());
      draggedTerminalId = null;
      dropTargetId = null;
      dropInsertBefore = false;
      isDragging = false;
    });

    // dragover for tracking position and showing indicator
    miniRow.addEventListener('dragover', (e) => {
      e.preventDefault();
      if (e.dataTransfer) {
        e.dataTransfer.dropEffect = 'move';
      }

      if (!isDragging || !draggedTerminalId) return;

      const target = e.target as HTMLElement;
      const card = target.closest('.terminal-card') as HTMLElement;

      if (card && card.getAttribute('data-terminal-id') !== draggedTerminalId) {
        const targetId = card.getAttribute('data-terminal-id');

        // Remove old indicators
        document.querySelectorAll('.drop-indicator').forEach(el => el.remove());

        // Calculate if we should insert before or after
        const rect = card.getBoundingClientRect();
        const midpoint = rect.left + rect.width / 2;
        const insertBefore = e.clientX < midpoint;

        // Store drop target info
        dropTargetId = targetId;
        dropInsertBefore = insertBefore;

        // Create and position insertion indicator - insert at wrapper level
        const wrapper = card.parentElement;
        const indicator = document.createElement('div');
        indicator.className = 'drop-indicator';
        wrapper?.parentElement?.insertBefore(indicator, insertBefore ? wrapper : wrapper.nextSibling);
      } else if (!card) {
        // In empty space - find all cards and determine position
        const allCards = Array.from(miniRow.querySelectorAll('.terminal-card')) as HTMLElement[];
        const lastCard = allCards[allCards.length - 1];

        if (lastCard && !lastCard.classList.contains('dragging')) {
          const lastId = lastCard.getAttribute('data-terminal-id');
          if (lastId && lastId !== draggedTerminalId) {
            // Remove old indicators
            document.querySelectorAll('.drop-indicator').forEach(el => el.remove());

            // Drop after the last card
            dropTargetId = lastId;
            dropInsertBefore = false;

            // Create and position insertion indicator after last card - insert at wrapper level
            const wrapper = lastCard.parentElement;
            const indicator = document.createElement('div');
            indicator.className = 'drop-indicator';
            wrapper?.parentElement?.insertBefore(indicator, wrapper?.nextSibling || null);
          }
        }
      }
    });
  }
}

// Set up all event listeners once
setupEventListeners();
