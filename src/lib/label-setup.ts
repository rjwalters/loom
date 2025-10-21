/**
 * GitHub Label Setup Utility
 *
 * Configures standard Loom workflow labels for a GitHub repository.
 * Labels coordinate work between different AI agent roles (Architect, Curator, Worker, Reviewer).
 *
 * See WORKFLOWS.md for complete workflow documentation.
 */

export interface LabelDefinition {
  name: string;
  description: string;
  color: string; // 6-character hex color (without #)
}

/**
 * Standard Loom workflow labels
 *
 * See WORKFLOWS.md for complete documentation.
 *
 * Color semantics:
 * - Blue: Human action needed
 * - Green: Loom bot action needed
 * - Amber: Work in progress
 * - Red: Blocked/needs help
 *
 * Labels are separated into Issue labels and PR labels:
 * - Issue labels: loom:architect, loom:hermit, loom:curated, loom:issue, loom:in-progress, loom:blocked, loom:urgent
 * - PR labels: loom:review-requested, loom:changes-requested, loom:pr
 */
export const LOOM_LABELS: LabelDefinition[] = [
  // Issue Labels
  {
    name: "loom:architect",
    description: "Architect suggestion awaiting user approval",
    color: "3B82F6", // Blue - human action needed
  },
  {
    name: "loom:hermit",
    description: "Hermit removal/simplification proposal awaiting user approval",
    color: "3B82F6", // Blue - human action needed
  },
  {
    name: "loom:curated",
    description: "Curator enhanced, awaiting human approval for work",
    color: "10B981", // Green - Loom bot action needed
  },
  {
    name: "loom:issue",
    description: "Human approved, ready for Worker to claim and implement",
    color: "10B981", // Green - ready for work
  },
  {
    name: "loom:in-progress",
    description: "Worker actively implementing this issue",
    color: "F59E0B", // Amber - work in progress
  },
  {
    name: "loom:blocked",
    description: "Implementation blocked, needs help or clarification",
    color: "EF4444", // Red - attention needed
  },
  {
    name: "loom:urgent",
    description: "High priority issue requiring immediate attention",
    color: "DC2626", // Dark red - urgent
  },
  // PR Labels
  {
    name: "loom:review-requested",
    description: "PR ready for Reviewer to review",
    color: "10B981", // Green - Loom bot action needed
  },
  {
    name: "loom:changes-requested",
    description: "PR needs fixes from Fixer",
    color: "F59E0B", // Amber - work in progress
  },
  {
    name: "loom:pr",
    description: "PR approved by Reviewer, ready for human to merge",
    color: "3B82F6", // Blue - human action needed
  },
];

/**
 * Result of label setup operation
 */
export interface LabelSetupResult {
  created: string[];
  updated: string[];
  skipped: string[];
  errors: Array<{ label: string; error: string }>;
}

/**
 * Setup all Loom workflow labels in the current repository.
 *
 * - Creates labels that don't exist
 * - Updates labels if force=true
 * - Skips existing labels if force=false
 * - Continues on errors to process all labels
 *
 * @param force - If true, update existing labels with new description/color
 * @returns Result summary with created/updated/skipped/errors
 */
export async function setupLoomLabels(force = false): Promise<LabelSetupResult> {
  const result: LabelSetupResult = {
    created: [],
    updated: [],
    skipped: [],
    errors: [],
  };

  // Check if we're in a git repository with a GitHub remote
  try {
    const { invoke } = await import("@tauri-apps/api/tauri");
    const hasGitHub = await invoke<boolean>("check_github_remote");
    if (!hasGitHub) {
      throw new Error(
        "Not in a GitHub repository. Please select a workspace with a GitHub remote."
      );
    }
  } catch (error) {
    result.errors.push({
      label: "all",
      error: error instanceof Error ? error.message : String(error),
    });
    return result;
  }

  // Process each label
  for (const label of LOOM_LABELS) {
    try {
      const created = await createOrUpdateLabel(label, force);
      if (created === "created") {
        result.created.push(label.name);
      } else if (created === "updated") {
        result.updated.push(label.name);
      } else {
        result.skipped.push(label.name);
      }
    } catch (error) {
      result.errors.push({
        label: label.name,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }

  return result;
}

/**
 * Create or update a single label.
 *
 * @param label - Label definition
 * @param force - If true, update existing label
 * @returns 'created', 'updated', or 'skipped'
 */
async function createOrUpdateLabel(
  label: LabelDefinition,
  force: boolean
): Promise<"created" | "updated" | "skipped"> {
  const { invoke } = await import("@tauri-apps/api/tauri");

  // Check if label exists
  const exists = await invoke<boolean>("check_label_exists", {
    name: label.name,
  });

  if (exists && !force) {
    return "skipped";
  }

  // Create or update label
  if (force && exists) {
    await invoke("update_github_label", {
      name: label.name,
      description: label.description,
      color: label.color,
    });
    return "updated";
  }

  await invoke("create_github_label", {
    name: label.name,
    description: label.description,
    color: label.color,
  });
  return "created";
}

/**
 * Get a formatted summary of the setup result for display to user.
 *
 * @param result - Setup result
 * @returns Human-readable summary string
 */
export function formatSetupResult(result: LabelSetupResult): string {
  const lines: string[] = [];

  if (result.created.length > 0) {
    lines.push(`✅ Created ${result.created.length} labels:`);
    for (const name of result.created) {
      lines.push(`   - ${name}`);
    }
  }

  if (result.updated.length > 0) {
    lines.push(`🔄 Updated ${result.updated.length} labels:`);
    for (const name of result.updated) {
      lines.push(`   - ${name}`);
    }
  }

  if (result.skipped.length > 0) {
    lines.push(`⏭️  Skipped ${result.skipped.length} existing labels:`);
    for (const name of result.skipped) {
      lines.push(`   - ${name}`);
    }
  }

  if (result.errors.length > 0) {
    lines.push(`❌ Failed ${result.errors.length} labels:`);
    for (const { label, error } of result.errors) {
      lines.push(`   - ${label}: ${error}`);
    }
  }

  return lines.join("\n");
}
