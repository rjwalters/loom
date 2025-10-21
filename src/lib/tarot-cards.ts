/**
 * Tarot Card Mapping for Role-Based Terminal UI
 *
 * Maps terminal roles to their corresponding tarot card SVG assets.
 * Used for visual effects during drag-and-drop operations.
 */

export type TarotRole =
  | "worker"
  | "curator"
  | "champion"
  | "architect"
  | "reviewer"
  | "critic"
  | "fixer"
  | "default";

/**
 * Maps a role string to the corresponding tarot card SVG path
 * @param role - The terminal role (e.g., "claude-code-worker", "curator", etc.)
 * @returns Path to the tarot card SVG file, or default card if role not recognized
 */
export function getTarotCardPath(role: string | undefined): string {
  // Extract base role from role identifiers like "claude-code-worker"
  const normalizedRole = normalizeRole(role);

  const roleToCard: Record<TarotRole, string> = {
    worker: "assets/tarot-cards/worker.svg",
    curator: "assets/tarot-cards/curator.svg",
    champion: "assets/tarot-cards/champion.svg",
    architect: "assets/tarot-cards/architect.svg",
    reviewer: "assets/tarot-cards/reviewer.svg",
    critic: "assets/tarot-cards/critic.svg",
    fixer: "assets/tarot-cards/fixer.svg",
    default: "assets/tarot-cards/worker.svg", // Fallback to worker card
  };

  return roleToCard[normalizedRole] || roleToCard.default;
}

/**
 * Normalizes a role identifier to the base tarot role
 * @param role - Role identifier (e.g., "claude-code-worker", "worker", "curator")
 * @returns Normalized tarot role
 */
function normalizeRole(role: string | undefined): TarotRole {
  if (!role) {
    return "default";
  }

  const lowerRole = role.toLowerCase();

  // Map various role formats to tarot roles
  if (lowerRole.includes("worker")) {
    return "worker";
  }
  if (lowerRole.includes("curator")) {
    return "curator";
  }
  if (lowerRole.includes("champion")) {
    return "champion";
  }
  if (lowerRole.includes("architect")) {
    return "architect";
  }
  if (lowerRole.includes("reviewer")) {
    return "reviewer";
  }
  if (lowerRole.includes("critic")) {
    return "critic";
  }
  if (lowerRole.includes("fixer")) {
    return "fixer";
  }

  return "default";
}

/**
 * Gets the tarot archetype name for a role
 * @param role - The terminal role
 * @returns The tarot archetype name (e.g., "The Magician")
 */
export function getTarotArchetype(role: string | undefined): string {
  const normalizedRole = normalizeRole(role);

  const roleToArchetype: Record<TarotRole, string> = {
    worker: "The Magician",
    curator: "The High Priestess",
    champion: "Strength",
    architect: "The Emperor",
    reviewer: "Justice",
    critic: "The Hermit",
    fixer: "The Hanged Man",
    default: "The Fool",
  };

  return roleToArchetype[normalizedRole] || roleToArchetype.default;
}
