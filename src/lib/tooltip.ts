interface TooltipOptions {
  text: string;
  position?: "top" | "bottom" | "left" | "right" | "auto";
  delay?: number; // milliseconds before showing (default 500)
}

class TooltipManager {
  private currentTooltip: HTMLElement | null = null;
  private showTimer: number | null = null;
  private attachedElements: WeakMap<HTMLElement, TooltipOptions> = new WeakMap();

  /**
   * Attach tooltip to an element
   */
  attach(element: HTMLElement, options: TooltipOptions): void {
    // Store options for this element
    this.attachedElements.set(element, options);

    // Add event listeners
    element.addEventListener("mouseenter", this.handleMouseEnter.bind(this, element));
    element.addEventListener("mouseleave", this.handleMouseLeave.bind(this));
    element.addEventListener("focus", this.handleFocus.bind(this, element));
    element.addEventListener("blur", this.handleBlur.bind(this));

    // Add aria-label for accessibility
    if (!element.getAttribute("aria-label")) {
      element.setAttribute("aria-label", options.text);
    }
  }

  /**
   * Remove tooltip from an element
   */
  detach(element: HTMLElement): void {
    this.attachedElements.delete(element);
    element.removeEventListener("mouseenter", this.handleMouseEnter.bind(this, element));
    element.removeEventListener("mouseleave", this.handleMouseLeave.bind(this));
    element.removeEventListener("focus", this.handleFocus.bind(this, element));
    element.removeEventListener("blur", this.handleBlur.bind(this));
  }

  /**
   * Show tooltip immediately (for programmatic display)
   */
  show(element: HTMLElement, options: TooltipOptions): void {
    // Hide any existing tooltip
    this.hide();

    // Create tooltip element
    const tooltip = document.createElement("div");
    tooltip.className = "tooltip";
    tooltip.textContent = options.text;
    tooltip.setAttribute("role", "tooltip");

    // Add to DOM (hidden initially for measurement)
    document.body.appendChild(tooltip);

    // Calculate position
    const position = this.calculatePosition(element, tooltip, options.position || "auto");
    tooltip.style.left = `${position.x}px`;
    tooltip.style.top = `${position.y}px`;

    // Add position class for arrow direction
    if (position.placement) {
      tooltip.classList.add(position.placement);
    }

    // Show with animation
    requestAnimationFrame(() => {
      tooltip.classList.add("show");
    });

    // Store reference
    this.currentTooltip = tooltip;

    // Connect tooltip to element for accessibility
    const tooltipId = `tooltip-${Date.now()}`;
    tooltip.id = tooltipId;
    element.setAttribute("aria-describedby", tooltipId);
  }

  /**
   * Hide current tooltip
   */
  hide(): void {
    // Clear any pending show timer
    if (this.showTimer !== null) {
      window.clearTimeout(this.showTimer);
      this.showTimer = null;
    }

    // Remove current tooltip
    if (this.currentTooltip) {
      // Get the element that was described by this tooltip
      const describedElement = document.querySelector(
        `[aria-describedby="${this.currentTooltip.id}"]`
      );
      if (describedElement) {
        describedElement.removeAttribute("aria-describedby");
      }

      // Fade out then remove
      this.currentTooltip.classList.remove("show");
      const tooltipToRemove = this.currentTooltip;
      setTimeout(() => {
        tooltipToRemove.remove();
      }, 150); // Match CSS transition duration

      this.currentTooltip = null;
    }
  }

  /**
   * Update existing tooltip text
   */
  update(element: HTMLElement, text: string): void {
    const options = this.attachedElements.get(element);
    if (options) {
      options.text = text;
      // Update aria-label
      element.setAttribute("aria-label", text);
      // If tooltip is currently shown for this element, update it
      if (this.currentTooltip) {
        this.currentTooltip.textContent = text;
      }
    }
  }

  /**
   * Calculate optimal tooltip position
   */
  private calculatePosition(
    element: HTMLElement,
    tooltip: HTMLElement,
    preferredPosition: "top" | "bottom" | "left" | "right" | "auto"
  ): { x: number; y: number; placement: string } {
    const elementRect = element.getBoundingClientRect();
    const tooltipRect = tooltip.getBoundingClientRect();
    const gap = 8; // pixels between element and tooltip

    const viewportWidth = window.innerWidth;
    const viewportHeight = window.innerHeight;

    // Try preferred position first
    const position = { x: 0, y: 0, placement: preferredPosition };

    switch (preferredPosition) {
      case "top":
        position.x = elementRect.left + elementRect.width / 2 - tooltipRect.width / 2;
        position.y = elementRect.top - tooltipRect.height - gap;
        break;
      case "bottom":
        position.x = elementRect.left + elementRect.width / 2 - tooltipRect.width / 2;
        position.y = elementRect.bottom + gap;
        break;
      case "left":
        position.x = elementRect.left - tooltipRect.width - gap;
        position.y = elementRect.top + elementRect.height / 2 - tooltipRect.height / 2;
        break;
      case "right":
        position.x = elementRect.right + gap;
        position.y = elementRect.top + elementRect.height / 2 - tooltipRect.height / 2;
        break;
      default:
        // Try bottom first (most common), then top, then right, then left
        if (elementRect.bottom + gap + tooltipRect.height < viewportHeight) {
          position.placement = "bottom";
          position.x = elementRect.left + elementRect.width / 2 - tooltipRect.width / 2;
          position.y = elementRect.bottom + gap;
        } else if (elementRect.top - gap - tooltipRect.height > 0) {
          position.placement = "top";
          position.x = elementRect.left + elementRect.width / 2 - tooltipRect.width / 2;
          position.y = elementRect.top - tooltipRect.height - gap;
        } else if (elementRect.right + gap + tooltipRect.width < viewportWidth) {
          position.placement = "right";
          position.x = elementRect.right + gap;
          position.y = elementRect.top + elementRect.height / 2 - tooltipRect.height / 2;
        } else {
          position.placement = "left";
          position.x = elementRect.left - tooltipRect.width - gap;
          position.y = elementRect.top + elementRect.height / 2 - tooltipRect.height / 2;
        }
        break;
    }

    // Ensure tooltip stays within viewport bounds
    position.x = Math.max(4, Math.min(position.x, viewportWidth - tooltipRect.width - 4));
    position.y = Math.max(4, Math.min(position.y, viewportHeight - tooltipRect.height - 4));

    return position;
  }

  /**
   * Handle mouse enter event
   */
  private handleMouseEnter(element: HTMLElement): void {
    const options = this.attachedElements.get(element);
    if (!options) return;

    const delay = options.delay ?? 500;

    // Clear any existing timer
    if (this.showTimer !== null) {
      window.clearTimeout(this.showTimer);
    }

    // Set timer to show tooltip after delay
    this.showTimer = window.setTimeout(() => {
      this.show(element, options);
      this.showTimer = null;
    }, delay);
  }

  /**
   * Handle mouse leave event
   */
  private handleMouseLeave(): void {
    this.hide();
  }

  /**
   * Handle focus event (keyboard navigation)
   */
  private handleFocus(element: HTMLElement): void {
    const options = this.attachedElements.get(element);
    if (!options) return;

    // Show immediately on keyboard focus
    this.show(element, options);
  }

  /**
   * Handle blur event
   */
  private handleBlur(): void {
    this.hide();
  }
}

// Singleton instance
let tooltipManagerInstance: TooltipManager | null = null;

export function getTooltipManager(): TooltipManager {
  if (!tooltipManagerInstance) {
    tooltipManagerInstance = new TooltipManager();
  }
  return tooltipManagerInstance;
}
