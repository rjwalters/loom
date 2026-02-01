/**
 * Resize handle functionality for the terminal/analytics split view.
 *
 * Provides drag-to-resize with:
 * - Mouse drag support
 * - Minimum width constraints (300px per panel)
 * - localStorage persistence of split position
 * - Smooth visual feedback during drag
 */

const STORAGE_KEY = "loom:split-position";
const MIN_WIDTH = 300;
const DEFAULT_RATIO = 0.5;

let isDragging = false;
let startX = 0;
let startLeftWidth = 0;

/**
 * Get the stored split ratio or default to 50/50
 */
function getStoredRatio(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (stored) {
      const ratio = parseFloat(stored);
      if (!Number.isNaN(ratio) && ratio >= 0 && ratio <= 1) {
        return ratio;
      }
    }
  } catch {
    // localStorage might be unavailable
  }
  return DEFAULT_RATIO;
}

/**
 * Store the split ratio for persistence
 */
function storeRatio(ratio: number): void {
  try {
    localStorage.setItem(STORAGE_KEY, ratio.toFixed(4));
  } catch {
    // localStorage might be unavailable
  }
}

/**
 * Apply the split ratio to the panels
 */
function applySplitRatio(ratio: number): void {
  const mainContent = document.getElementById("main-content");
  const terminalView = document.getElementById("terminal-view");
  const analyticsView = document.getElementById("analytics-view");

  if (!mainContent || !terminalView || !analyticsView) return;

  const totalWidth = mainContent.clientWidth - 4; // Account for resize handle width
  const leftWidth = Math.max(MIN_WIDTH, Math.min(totalWidth - MIN_WIDTH, totalWidth * ratio));
  const rightWidth = totalWidth - leftWidth;

  // Use flex-basis to set widths
  terminalView.style.flexBasis = `${leftWidth}px`;
  terminalView.style.flexGrow = "0";
  terminalView.style.flexShrink = "0";

  analyticsView.style.flexBasis = `${rightWidth}px`;
  analyticsView.style.flexGrow = "0";
  analyticsView.style.flexShrink = "0";
}

/**
 * Handle mouse down on resize handle
 */
function handleMouseDown(e: MouseEvent): void {
  const terminalView = document.getElementById("terminal-view");
  const resizeHandle = document.getElementById("resize-handle");

  if (!terminalView || !resizeHandle) return;

  isDragging = true;
  startX = e.clientX;
  startLeftWidth = terminalView.getBoundingClientRect().width;

  // Add dragging class for visual feedback
  resizeHandle.classList.add("dragging");
  terminalView.classList.add("resizing");
  document.getElementById("analytics-view")?.classList.add("resizing");

  // Prevent text selection during drag
  document.body.style.cursor = "col-resize";
  document.body.style.userSelect = "none";

  e.preventDefault();
}

/**
 * Handle mouse move during drag
 */
function handleMouseMove(e: MouseEvent): void {
  if (!isDragging) return;

  const mainContent = document.getElementById("main-content");
  const terminalView = document.getElementById("terminal-view");
  const analyticsView = document.getElementById("analytics-view");

  if (!mainContent || !terminalView || !analyticsView) return;

  const deltaX = e.clientX - startX;
  const totalWidth = mainContent.clientWidth - 4; // Account for resize handle
  let newLeftWidth = startLeftWidth + deltaX;

  // Apply constraints
  newLeftWidth = Math.max(MIN_WIDTH, Math.min(totalWidth - MIN_WIDTH, newLeftWidth));
  const newRightWidth = totalWidth - newLeftWidth;

  // Apply widths
  terminalView.style.flexBasis = `${newLeftWidth}px`;
  analyticsView.style.flexBasis = `${newRightWidth}px`;
}

/**
 * Handle mouse up to end drag
 */
function handleMouseUp(): void {
  if (!isDragging) return;

  isDragging = false;

  const mainContent = document.getElementById("main-content");
  const terminalView = document.getElementById("terminal-view");
  const resizeHandle = document.getElementById("resize-handle");
  const analyticsView = document.getElementById("analytics-view");

  // Remove dragging classes
  resizeHandle?.classList.remove("dragging");
  terminalView?.classList.remove("resizing");
  analyticsView?.classList.remove("resizing");

  // Restore cursor
  document.body.style.cursor = "";
  document.body.style.userSelect = "";

  // Calculate and store the ratio
  if (mainContent && terminalView) {
    const totalWidth = mainContent.clientWidth - 4;
    const leftWidth = terminalView.getBoundingClientRect().width;
    const ratio = leftWidth / totalWidth;
    storeRatio(ratio);
  }
}

/**
 * Handle window resize to maintain split ratio
 */
function handleWindowResize(): void {
  if (isDragging) return;

  const ratio = getStoredRatio();
  applySplitRatio(ratio);
}

let initialized = false;

/**
 * Initialize the resize handle functionality.
 * Should be called once when the app starts.
 */
export function initializeResizeHandle(): void {
  if (initialized) return;
  initialized = true;

  const resizeHandle = document.getElementById("resize-handle");
  if (!resizeHandle) return;

  // Apply initial split ratio from storage
  const storedRatio = getStoredRatio();
  applySplitRatio(storedRatio);

  // Add event listeners
  resizeHandle.addEventListener("mousedown", handleMouseDown);
  document.addEventListener("mousemove", handleMouseMove);
  document.addEventListener("mouseup", handleMouseUp);

  // Handle window resize
  window.addEventListener("resize", handleWindowResize);
}
