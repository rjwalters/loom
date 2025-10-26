/**
 * Tarot Card Mapping for Role-Based Terminal UI
 *
 * Maps terminal roles to their corresponding tarot card SVG assets.
 * Used for visual effects during drag-and-drop operations.
 */

export type TarotRole =
  | "builder"
  | "curator"
  | "champion"
  | "architect"
  | "judge"
  | "hermit"
  | "doctor"
  | "guide"
  | "driver"
  | "default";

/**
 * Maps a role string to the corresponding tarot card SVG path
 * @param role - The terminal role (e.g., "builder", "curator", "judge", etc.)
 * @returns Path to the tarot card SVG file, or default card if role not recognized
 */
export function getTarotCardPath(role: string | undefined): string {
  // Extract base role from role identifiers like "claude-code-worker"
  const normalizedRole = normalizeRole(role);

  const roleToCard: Record<TarotRole, string> = {
    builder: "assets/tarot-cards/builder.svg",
    curator: "assets/tarot-cards/curator.svg",
    champion: "assets/tarot-cards/champion.svg",
    architect: "assets/tarot-cards/architect.svg",
    judge: "assets/tarot-cards/judge.svg",
    hermit: "assets/tarot-cards/hermit.svg",
    doctor: "assets/tarot-cards/doctor.svg",
    guide: "assets/tarot-cards/guide.svg",
    driver: "assets/tarot-cards/driver.svg",
    default: "assets/tarot-cards/builder.svg", // Fallback to builder card
  };

  return roleToCard[normalizedRole] || roleToCard.default;
}

/**
 * Normalizes a role identifier to the base tarot role
 * @param role - Role identifier (e.g., "builder", "curator", "judge")
 * @returns Normalized tarot role
 */
function normalizeRole(role: string | undefined): TarotRole {
  if (!role) {
    return "default";
  }

  const lowerRole = role.toLowerCase();

  // Map various role formats to tarot roles
  if (lowerRole.includes("builder")) {
    return "builder";
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
  if (lowerRole.includes("judge")) {
    return "judge";
  }
  if (lowerRole.includes("hermit")) {
    return "hermit";
  }
  if (lowerRole.includes("doctor")) {
    return "doctor";
  }
  if (lowerRole.includes("guide")) {
    return "guide";
  }
  if (lowerRole.includes("driver")) {
    return "driver";
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
    builder: "The Magician",
    curator: "The High Priestess",
    champion: "Strength",
    architect: "The Emperor",
    judge: "Justice",
    hermit: "The Hermit",
    doctor: "The Hanged Man",
    guide: "The Hierophant",
    driver: "The Chariot",
    default: "The Fool",
  };

  return roleToArchetype[normalizedRole] || roleToArchetype.default;
}
