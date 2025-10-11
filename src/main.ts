import './style.css';
import { initTheme, toggleTheme } from './lib/theme';

// Initialize theme
initTheme();

// Theme toggle button
document.getElementById('theme-toggle')?.addEventListener('click', () => {
  toggleTheme();
});

console.log('Loom initialized');
