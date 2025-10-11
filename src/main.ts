import './style.css';
import { initTheme, toggleTheme } from './lib/theme';
import { AppState, TerminalStatus } from './lib/state';
import { renderHeader, renderPrimaryTerminal, renderMiniTerminals } from './lib/ui';

// Initialize theme
initTheme();

// Initialize state with mock data
const state = new AppState();

// Add some example terminals
state.addTerminal({
  id: '1',
  name: 'Terminal 1',
  status: TerminalStatus.Idle,
  isPrimary: true
});

state.addTerminal({
  id: '2',
  name: 'Terminal 2',
  status: TerminalStatus.Busy,
  isPrimary: false
});

state.addTerminal({
  id: '3',
  name: 'Terminal 3',
  status: TerminalStatus.Idle,
  isPrimary: false
});

// Render function
function render() {
  renderHeader();
  renderPrimaryTerminal(state.getPrimary());
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
            alert('Cannot close the last terminal');
            return;
          }

          if (confirm('Close this terminal?')) {
            state.removeTerminal(id);
          }
        }
        return;
      }

      // Handle add terminal button
      if (target.id === 'add-terminal-btn' || target.closest('#add-terminal-btn')) {
        const count = state.getTerminals().length + 1;
        state.addTerminal({
          id: String(Date.now()),
          name: `Terminal ${count}`,
          status: TerminalStatus.Idle,
          isPrimary: false
        });
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
