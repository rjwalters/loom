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
  setupEventListeners();
}

// Initial render
render();

// Re-render on state changes
state.onChange(render);

// Event listeners
function setupEventListeners() {
  // Theme toggle
  document.getElementById('theme-toggle')?.addEventListener('click', () => {
    toggleTheme();
  });

  // Add terminal (placeholder for Issue #5)
  document.getElementById('add-terminal-btn')?.addEventListener('click', () => {
    const count = state.getTerminals().length + 1;
    state.addTerminal({
      id: String(Date.now()),
      name: `Terminal ${count}`,
      status: TerminalStatus.Idle,
      isPrimary: false
    });
  });

  // Switch terminal (event delegation)
  document.getElementById('mini-terminal-row')?.addEventListener('click', (e) => {
    const target = e.target as HTMLElement;
    const card = target.closest('[data-terminal-id]');

    if (card && !target.classList.contains('close-terminal-btn')) {
      const id = card.getAttribute('data-terminal-id');
      if (id) {
        state.setPrimary(id);
      }
    }
  });

  // Close terminal (event delegation)
  document.getElementById('mini-terminal-row')?.addEventListener('click', (e) => {
    const target = e.target as HTMLElement;

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
    }
  });
}

console.log('Loom initialized');
