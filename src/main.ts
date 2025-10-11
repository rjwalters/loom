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

// Set up event listeners (only once, since parent elements are static)
function setupEventListeners() {
  // Theme toggle
  document.getElementById('theme-toggle')?.addEventListener('click', () => {
    toggleTheme();
  });

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

    // Drag and drop event handlers
    miniRow.addEventListener('dragstart', (e) => {
      const target = e.target as HTMLElement;
      const card = target.closest('.terminal-card') as HTMLElement;

      if (card) {
        draggedTerminalId = card.getAttribute('data-terminal-id');
        card.classList.add('dragging');

        if (e.dataTransfer) {
          e.dataTransfer.effectAllowed = 'move';
          e.dataTransfer.setData('text/html', card.innerHTML);
        }
      }
    });

    miniRow.addEventListener('dragend', (e) => {
      const target = e.target as HTMLElement;
      const card = target.closest('.terminal-card');

      if (card) {
        card.classList.remove('dragging');
      }

      draggedTerminalId = null;

      // Remove any insertion indicators
      document.querySelectorAll('.drop-indicator').forEach(el => el.remove());
    });

    miniRow.addEventListener('dragover', (e) => {
      e.preventDefault(); // Allow drop

      if (!draggedTerminalId) return;

      const target = e.target as HTMLElement;
      const card = target.closest('.terminal-card') as HTMLElement;

      if (card && card.getAttribute('data-terminal-id') !== draggedTerminalId) {
        // Remove old indicators
        document.querySelectorAll('.drop-indicator').forEach(el => el.remove());

        // Calculate if we should insert before or after
        const rect = card.getBoundingClientRect();
        const midpoint = rect.left + rect.width / 2;
        const insertBefore = e.clientX < midpoint;

        // Create and position insertion indicator
        const indicator = document.createElement('div');
        indicator.className = 'drop-indicator';
        card.parentElement?.insertBefore(indicator, insertBefore ? card : card.nextSibling);
      }
    });

    miniRow.addEventListener('drop', (e) => {
      e.preventDefault();

      if (!draggedTerminalId) return;

      const target = e.target as HTMLElement;
      const card = target.closest('.terminal-card') as HTMLElement;

      if (card) {
        const targetId = card.getAttribute('data-terminal-id');

        if (targetId && targetId !== draggedTerminalId) {
          // Calculate if we should insert before or after
          const rect = card.getBoundingClientRect();
          const midpoint = rect.left + rect.width / 2;
          const insertBefore = e.clientX < midpoint;

          state.reorderTerminal(draggedTerminalId, targetId, insertBefore);
        }
      }

      // Cleanup
      document.querySelectorAll('.drop-indicator').forEach(el => el.remove());
      draggedTerminalId = null;
    });
  }
}

// Set up all event listeners once
setupEventListeners();

console.log('Loom initialized');
