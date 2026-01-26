/**
 * Modal Builder Utility
 *
 * Provides a shared infrastructure for creating consistent, accessible modals.
 * Consolidates common patterns: backdrop, show/hide, escape key, background click.
 *
 * @example
 * ```typescript
 * const modal = new ModalBuilder({
 *   title: "Settings",
 *   width: "600px",
 *   onClose: () => saveSettings(),
 * });
 *
 * modal.setContent("<div>Modal content here</div>");
 * modal.addFooterButton("Cancel", () => modal.close());
 * modal.addFooterButton("Save", handleSave, "primary");
 * modal.show();
 * ```
 */

export interface ModalOptions {
  /** Modal title displayed in header */
  title: string;
  /** Modal width (e.g., "600px", "800px", "max-w-md") */
  width?: string;
  /** Maximum height (default: "90vh") */
  maxHeight?: string;
  /** Close when clicking backdrop (default: true) */
  closeOnBackdrop?: boolean;
  /** Close when pressing Escape key (default: true) */
  closeOnEscape?: boolean;
  /** Callback when modal closes */
  onClose?: () => void;
  /** Custom modal ID for CSS/JS targeting */
  id?: string;
  /** Whether to show the default header with title (default: true) */
  showHeader?: boolean;
  /** Custom header content (replaces default header if provided) */
  customHeader?: string | HTMLElement;
}

export interface FooterButton {
  text: string;
  onClick: () => void;
  style: "primary" | "secondary" | "danger";
}

export class ModalBuilder {
  private backdrop: HTMLElement;
  private dialog: HTMLElement;
  private contentContainer: HTMLElement;
  private footerContainer: HTMLElement | null = null;
  private headerContainer: HTMLElement | null = null;
  private escapeHandler?: (e: KeyboardEvent) => void;
  private options: Required<Omit<ModalOptions, "customHeader" | "onClose" | "id">> & {
    customHeader?: string | HTMLElement;
    onClose?: () => void;
    id?: string;
  };
  private footerButtons: FooterButton[] = [];
  private isShown = false;

  constructor(options: ModalOptions) {
    this.options = {
      title: options.title,
      width: options.width ?? "600px",
      maxHeight: options.maxHeight ?? "90vh",
      closeOnBackdrop: options.closeOnBackdrop ?? true,
      closeOnEscape: options.closeOnEscape ?? true,
      showHeader: options.showHeader ?? true,
      onClose: options.onClose,
      id: options.id,
      customHeader: options.customHeader,
    };

    this.backdrop = this.createBackdrop();
    this.dialog = this.createDialog();
    this.contentContainer = this.createContentContainer();

    // Build the modal structure
    if (this.options.showHeader || this.options.customHeader) {
      this.headerContainer = this.createHeader();
      this.dialog.appendChild(this.headerContainer);
    }
    this.dialog.appendChild(this.contentContainer);
    this.backdrop.appendChild(this.dialog);
  }

  /**
   * Create the backdrop element with standard styling
   */
  private createBackdrop(): HTMLElement {
    const backdrop = document.createElement("div");
    if (this.options.id) {
      backdrop.id = this.options.id;
    }
    backdrop.className =
      "fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50 hidden";
    backdrop.setAttribute("role", "presentation");
    return backdrop;
  }

  /**
   * Create the dialog container with proper ARIA attributes
   */
  private createDialog(): HTMLElement {
    const dialog = document.createElement("div");

    // Handle width - check if it's a Tailwind class or a CSS value
    const widthClass = this.options.width.startsWith("max-w-") ? this.options.width : "";
    const widthStyle = widthClass ? "" : `width: ${this.options.width};`;

    dialog.className =
      `bg-white dark:bg-gray-800 rounded-lg flex flex-col border border-gray-200 dark:border-gray-700 ${widthClass}`.trim();
    dialog.style.cssText = `max-height: ${this.options.maxHeight}; ${widthStyle}`.trim();
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");
    if (this.options.title) {
      dialog.setAttribute("aria-labelledby", "modal-title");
    }

    return dialog;
  }

  /**
   * Create the header with title and close button
   */
  private createHeader(): HTMLElement {
    const header = document.createElement("div");
    header.className =
      "flex items-center justify-between p-4 border-b border-gray-200 dark:border-gray-700";

    if (this.options.customHeader) {
      if (typeof this.options.customHeader === "string") {
        header.innerHTML = this.options.customHeader;
      } else {
        header.appendChild(this.options.customHeader);
      }
    } else {
      header.innerHTML = `
        <h2 id="modal-title" class="text-xl font-bold text-gray-900 dark:text-gray-100">
          ${escapeHtml(this.options.title)}
        </h2>
        <button
          class="modal-close-btn text-gray-400 hover:text-gray-600 dark:hover:text-gray-300 font-bold text-2xl transition-colors"
          aria-label="Close modal"
        >
          &times;
        </button>
      `;

      // Wire up close button
      const closeBtn = header.querySelector(".modal-close-btn");
      closeBtn?.addEventListener("click", () => this.close());
    }

    return header;
  }

  /**
   * Create the content container
   */
  private createContentContainer(): HTMLElement {
    const content = document.createElement("div");
    content.className = "flex-1 overflow-y-auto p-4";
    return content;
  }

  /**
   * Create the footer container
   */
  private createFooter(): HTMLElement {
    const footer = document.createElement("div");
    footer.className = "flex justify-end gap-2 p-4 border-t border-gray-200 dark:border-gray-700";
    return footer;
  }

  /**
   * Set the modal content
   * @param content - HTML string or HTMLElement
   */
  setContent(content: string | HTMLElement): this {
    if (typeof content === "string") {
      this.contentContainer.innerHTML = content;
    } else {
      this.contentContainer.innerHTML = "";
      this.contentContainer.appendChild(content);
    }
    return this;
  }

  /**
   * Get the content container element for direct manipulation
   */
  getContentContainer(): HTMLElement {
    return this.contentContainer;
  }

  /**
   * Add a button to the modal footer
   * @param text - Button text
   * @param onClick - Click handler
   * @param style - Button style: "primary", "secondary", or "danger"
   */
  addFooterButton(
    text: string,
    onClick: () => void,
    style: "primary" | "secondary" | "danger" = "secondary"
  ): this {
    this.footerButtons.push({ text, onClick, style });
    return this;
  }

  /**
   * Clear all footer buttons
   */
  clearFooterButtons(): this {
    this.footerButtons = [];
    return this;
  }

  /**
   * Build and append footer with buttons
   */
  private buildFooter(): void {
    if (this.footerButtons.length === 0) return;

    this.footerContainer = this.createFooter();

    for (const btn of this.footerButtons) {
      const button = document.createElement("button");
      button.textContent = btn.text;
      button.addEventListener("click", btn.onClick);

      switch (btn.style) {
        case "primary":
          button.className =
            "px-4 py-2 bg-blue-600 hover:bg-blue-500 rounded text-white font-medium";
          break;
        case "danger":
          button.className = "px-4 py-2 bg-red-600 hover:bg-red-500 rounded text-white font-medium";
          break;
        default:
          button.className =
            "px-4 py-2 bg-gray-200 dark:bg-gray-700 hover:bg-gray-300 dark:hover:bg-gray-600 rounded text-gray-900 dark:text-gray-100";
          break;
      }

      this.footerContainer.appendChild(button);
    }

    this.dialog.appendChild(this.footerContainer);
  }

  /**
   * Set up event listeners for closing the modal
   */
  private setupCloseHandlers(): void {
    // Close on backdrop click
    if (this.options.closeOnBackdrop) {
      this.backdrop.addEventListener("click", (e) => {
        if (e.target === this.backdrop) {
          this.close();
        }
      });
    }

    // Close on Escape key
    if (this.options.closeOnEscape) {
      this.escapeHandler = (e: KeyboardEvent) => {
        if (e.key === "Escape") {
          this.close();
        }
      };
      document.addEventListener("keydown", this.escapeHandler);
    }
  }

  /**
   * Show the modal
   */
  show(): this {
    if (this.isShown) return this;

    this.buildFooter();
    this.setupCloseHandlers();

    document.body.appendChild(this.backdrop);
    this.backdrop.classList.remove("hidden");
    this.isShown = true;

    return this;
  }

  /**
   * Close and remove the modal
   */
  close(): void {
    if (!this.isShown) return;

    // Remove escape handler
    if (this.escapeHandler) {
      document.removeEventListener("keydown", this.escapeHandler);
      this.escapeHandler = undefined;
    }

    // Call onClose callback
    this.options.onClose?.();

    // Remove from DOM
    this.backdrop.remove();
    this.isShown = false;
  }

  /**
   * Get the backdrop element for custom event handling
   */
  getBackdrop(): HTMLElement {
    return this.backdrop;
  }

  /**
   * Get the dialog element for custom manipulation
   */
  getDialog(): HTMLElement {
    return this.dialog;
  }

  /**
   * Check if the modal is currently shown
   */
  isVisible(): boolean {
    return this.isShown;
  }

  /**
   * Update the modal title
   */
  setTitle(title: string): this {
    const titleEl = this.dialog.querySelector("#modal-title");
    if (titleEl) {
      titleEl.textContent = title;
    }
    return this;
  }

  /**
   * Query a selector within the modal
   */
  querySelector<T extends Element>(selector: string): T | null {
    return this.backdrop.querySelector<T>(selector);
  }

  /**
   * Query all elements matching a selector within the modal
   */
  querySelectorAll<T extends Element>(selectors: string): NodeListOf<T> {
    return this.backdrop.querySelectorAll<T>(selectors);
  }
}

/**
 * Escape HTML to prevent XSS
 */
function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}

/**
 * Create and show a simple confirmation modal
 */
export function showConfirmModal(options: {
  title: string;
  message: string;
  confirmText?: string;
  cancelText?: string;
  onConfirm: () => void;
  onCancel?: () => void;
  confirmStyle?: "primary" | "danger";
}): ModalBuilder {
  const modal = new ModalBuilder({
    title: options.title,
    width: "400px",
  });

  modal.setContent(`
    <p class="text-gray-700 dark:text-gray-300">${escapeHtml(options.message)}</p>
  `);

  modal.addFooterButton(options.cancelText ?? "Cancel", () => {
    modal.close();
    options.onCancel?.();
  });

  modal.addFooterButton(
    options.confirmText ?? "Confirm",
    () => {
      modal.close();
      options.onConfirm();
    },
    options.confirmStyle ?? "primary"
  );

  return modal.show();
}

/**
 * Create and show a simple alert modal
 */
export function showAlertModal(options: {
  title: string;
  message: string;
  buttonText?: string;
  onClose?: () => void;
}): ModalBuilder {
  const modal = new ModalBuilder({
    title: options.title,
    width: "400px",
    onClose: options.onClose,
  });

  modal.setContent(`
    <p class="text-gray-700 dark:text-gray-300">${escapeHtml(options.message)}</p>
  `);

  modal.addFooterButton(options.buttonText ?? "OK", () => modal.close(), "primary");

  return modal.show();
}
